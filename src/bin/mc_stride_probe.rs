use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::hint::black_box;
use std::io::{self, BufWriter, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

static SIGNAL_REQUESTED: AtomicBool = AtomicBool::new(false);

const CACHE_LINE_BYTES: usize = 64;
const WORD_BYTES: usize = std::mem::size_of::<usize>();
const PAGE_BYTES: usize = 4096;
const HUGE_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MASK_48: u64 = (1u64 << 48) - 1;

const MSR_DF_PERF_CTL_0: u64 = 0xC0010240;
const MSR_DF_PERF_CTR_0: u64 = 0xC0010241;
const MSR_DF_PERF_CTL_1: u64 = 0xC0010242;
const MSR_DF_PERF_CTR_1: u64 = 0xC0010243;

const MSR_CHL3_PMC_CFG_0: u64 = 0xC0010230;
const MSR_CHL3_PMC_CTR_0: u64 = 0xC0010231;
const MSR_CHL3_PMC_STRIDE: u64 = 0x2;
const L3_EVT_LOOKUP_STATE: u64 = 0x04;
const UMASK_L3_LOOKUP_ALL: u64 = 0xFF;
const UMASK_L3_LOOKUP_MISS: u64 = 0x01;
const L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES: u64 = 0x303C00000400000;
const L3_CFG_CHECK_MASK: u64 = L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES | (0xFFu64 << 8) | 0xFFu64;

const PMCX165_EVENT: u16 = 0x165;
const PMCX165_UMASK_LOCAL_CCX: u8 = 0x02;
const PMCX165_UMASK_NEAR_CACHE: u8 = 0x04;
const PMCX165_UMASK_DRAM_NEAR: u8 = 0x08;
const FILL_MIB_PER_SEC_X100_SCALE: u64 = 6_103_516;
const OBSERVED_EPYC_CS_LINKS: [u32; 8] = [0, 3, 4, 5, 6, 9, 10, 11];
const OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT: u32 = 8;
const OBSERVED_EPYC_NODE0_V1_NIBBLE_LUT: [u8; 16] =
    [5, 5, 0, 1, 4, 2, 7, 6, 1, 1, 4, 5, 0, 2, 7, 6];

#[derive(Debug, Parser)]
#[command(name = "mc_stride_probe")]
#[command(about = "Standalone memory-controller stride aliasing probe")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run(RunArgs),
    Scan(ScanArgs),
}

#[derive(Clone, Debug, Parser)]
struct RunArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value = "64", value_parser = parse_byte_size_arg)]
    stride: usize,
    #[arg(long, default_value = "0", value_parser = parse_byte_size_arg)]
    base_offset: usize,
}

#[derive(Clone, Debug, Parser)]
struct ScanArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value = "64")]
    strides: String,
    #[arg(long, default_value = "0")]
    base_offsets: String,
}

#[derive(Clone, Debug, Parser)]
struct CommonArgs {
    #[arg(long, default_value_t = 1)]
    threads: usize,
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    inline_worker: bool,
    #[arg(long)]
    cpus: Option<String>,
    #[arg(long, default_value_t = 1_000)]
    duration_ms: u64,
    #[arg(long, default_value_t = 0)]
    start_cooldown_ms: u64,
    #[arg(long, default_value_t = 0)]
    traffic_active_ms: u64,
    #[arg(long, default_value_t = 0)]
    traffic_cool_ms: u64,
    #[arg(long, default_value_t = 1024)]
    size_mib: usize,
    #[arg(long, value_enum, default_value_t = Pattern::Stream)]
    pattern: Pattern,
    #[arg(long, value_enum, default_value_t = AddressMode::Linear)]
    address_mode: AddressMode,
    #[arg(long, value_enum, default_value_t = AccessKind::Read)]
    access_kind: AccessKind,
    #[arg(long, default_value_t = 4)]
    avx512_unroll: usize,
    #[arg(long, default_value_t = 0)]
    prefetch_distance: usize,
    #[arg(long, default_value_t = 1)]
    residue_period: usize,
    #[arg(long, default_value_t = 0)]
    residue_id: usize,
    #[arg(long, default_value = "0", value_parser = parse_u64_arg)]
    xor_mask: u64,
    #[arg(long, value_enum, default_value_t = NumaPolicy::Default)]
    numa_policy: NumaPolicy,
    #[arg(long)]
    numa_nodes: Option<String>,
    #[arg(long, value_enum, default_value_t = PageMode::Base)]
    page_mode: PageMode,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    measure_cs: bool,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    measure_l3: bool,
    #[arg(long)]
    l3_cpus: Option<String>,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    measure_fill: bool,
    #[arg(long)]
    fill_cpus: Option<String>,
    #[arg(long, default_value = "0-11")]
    links: String,
    #[arg(long, default_value_t = 0)]
    msr_cpu: u32,
    #[arg(long, default_value_t = 0)]
    sample_window_ms: u64,
    #[arg(long, default_value_t = 100.0)]
    active_link_floor_mib_s: f64,
    #[arg(long, default_value_t = 4096)]
    batch_ops: u64,
    #[arg(long, default_value_t = 1)]
    lines_per_step: usize,
    #[arg(long, value_enum, default_value_t = SelectedOrder::RoundRobin)]
    selected_order: SelectedOrder,
    #[arg(long, default_value = "0", value_parser = parse_u64_arg)]
    pfn_mask: u64,
    #[arg(long, default_value = "0", value_parser = parse_u64_arg)]
    pfn_value: u64,
    #[arg(long, default_value_t = 0)]
    pfn_max_pages: usize,
    #[arg(long, default_value_t = 0)]
    predicted_cs_select: usize,
    #[arg(long, default_value_t = 0)]
    predicted_cs_link: u32,
    #[arg(long, default_value = "0:4032:64")]
    predicted_cs_line_offsets: String,
    #[arg(long, default_value_t = 0)]
    predicted_cs_max_pages: usize,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    predicted_cs_autotune: bool,
    #[arg(long, default_value_t = 0)]
    predicted_cs_segment_pages: usize,
    #[arg(long, default_value_t = 0)]
    predicted_cs_phys_segment_pages: usize,
    #[arg(long)]
    predicted_cs_pfn_allowlist: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = PredictedCsPfnFormula::None)]
    predicted_cs_pfn_formula: PredictedCsPfnFormula,
    #[arg(long, default_value_t = 12)]
    predicted_cs_pfn_bucket_shift: u32,
    #[arg(long, default_value_t = 0.95)]
    predicted_cs_pfn_min_target_share: f64,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    predicted_cs_pfn_require_top: bool,
    #[arg(long)]
    predicted_cs_pfn_bucket_score_out: Option<PathBuf>,
    #[arg(long)]
    predicted_cs_pfn_bucket_score_seed: Option<PathBuf>,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    predicted_cs_pfn_bucket_score_all_links: bool,
    #[arg(long, default_value_t = 1024)]
    predicted_cs_pfn_bucket_sample_offsets: usize,
    #[arg(long, default_value_t = 0)]
    hot_cs_select: usize,
    #[arg(long)]
    hot_cs_link: Option<u32>,
    #[arg(long, default_value_t = 4096)]
    hot_cs_candidates: usize,
    #[arg(long, default_value_t = 256)]
    hot_cs_group_pages: usize,
    #[arg(long, default_value_t = 5)]
    hot_cs_window_ms: u64,
    #[arg(long, value_enum, default_value_t = HotCsScoreUnit::Group)]
    hot_cs_score_unit: HotCsScoreUnit,
    #[arg(long, default_value = "0")]
    hot_cs_line_offsets: String,
    #[arg(long)]
    hot_cs_score_out: Option<PathBuf>,
    #[arg(long)]
    verified_lines_in: Option<PathBuf>,
    #[arg(long)]
    verified_lines_out: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = VerifiedLineMatch::Auto)]
    verified_lines_match: VerifiedLineMatch,
    #[arg(long, default_value = "0xff", value_parser = parse_u64_arg)]
    verified_lines_color_mask: u64,
    #[arg(long, default_value_t = 1)]
    verified_lines_alloc_retries: usize,
    #[arg(long, default_value_t = 0)]
    verified_lines_min_offsets: usize,
    #[arg(long, default_value_t = 0)]
    cache_evict_mib: usize,
    #[arg(long, default_value = "64", value_parser = parse_byte_size_arg)]
    cache_evict_stride: usize,
    #[arg(long, default_value_t = 0)]
    side_stream_threads: usize,
    #[arg(long)]
    side_stream_cpus: Option<String>,
    #[arg(long, default_value_t = 1024)]
    side_stream_size_mib: usize,
    #[arg(long, default_value = "64", value_parser = parse_byte_size_arg)]
    side_stream_stride: usize,
    #[arg(long, default_value_t = 0)]
    flush_threads: usize,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    self_flush: bool,
    #[arg(long)]
    flush_cpus: Option<String>,
    #[arg(long, value_enum, default_value_t = FlushInstruction::Clflush)]
    flush_instruction: FlushInstruction,
    #[arg(long, value_enum, default_value_t = FlushTarget::Chase)]
    flush_target: FlushTarget,
    #[arg(long, default_value_t = 0)]
    flush_pause_ns: u64,
    #[arg(long, default_value_t = 0)]
    flush_max_lines: usize,
    #[arg(long, default_value_t = 1_048_576)]
    chase_nodes: usize,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum Pattern {
    Stream,
    Chase,
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stream => "stream",
            Self::Chase => "chase",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum AddressMode {
    Linear,
    Residue,
    Xor,
    PageOffset,
}

impl fmt::Display for AddressMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Linear => "linear",
            Self::Residue => "residue",
            Self::Xor => "xor",
            Self::PageOffset => "page-offset",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum AccessKind {
    Read,
    Write,
    Rmw,
    Avx512Read,
}

impl fmt::Display for AccessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Rmw => "rmw",
            Self::Avx512Read => "avx512-read",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum NumaPolicy {
    Default,
    Bind,
    Interleave,
    Preferred,
}

impl fmt::Display for NumaPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Bind => "bind",
            Self::Interleave => "interleave",
            Self::Preferred => "preferred",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PageMode {
    Base,
    Thp,
    Hugetlb,
}

impl fmt::Display for PageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Base => "base",
            Self::Thp => "thp",
            Self::Hugetlb => "hugetlb",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum FlushTarget {
    Selected,
    Chase,
    Payload,
    All,
}

impl fmt::Display for FlushTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Selected => "selected",
            Self::Chase => "chase",
            Self::Payload => "payload",
            Self::All => "all",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum FlushInstruction {
    Clflush,
    Clflushopt,
}

impl fmt::Display for FlushInstruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clflush => "clflush",
            Self::Clflushopt => "clflushopt",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum SelectedOrder {
    RoundRobin,
    VirtualSorted,
    BucketGrouped,
}

impl fmt::Display for SelectedOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RoundRobin => "round-robin",
            Self::VirtualSorted => "virtual-sorted",
            Self::BucketGrouped => "bucket-grouped",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum HotCsScoreUnit {
    Group,
    Page,
    Line,
}

impl fmt::Display for HotCsScoreUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Group => "group",
            Self::Page => "page",
            Self::Line => "line",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum VerifiedLineMatch {
    Auto,
    ExactPfn,
    Color,
    Offset,
}

impl fmt::Display for VerifiedLineMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::ExactPfn => "exact-pfn",
            Self::Color => "color",
            Self::Offset => "offset",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PredictedCsPfnFormula {
    None,
    ObservedEpycNode0V1,
}

impl fmt::Display for PredictedCsPfnFormula {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::ObservedEpycNode0V1 => "observed-epyc-node0-v1",
        })
    }
}

#[derive(Clone, Debug)]
struct RunConfig {
    pattern: Pattern,
    address_mode: AddressMode,
    access_kind: AccessKind,
    avx512_unroll: usize,
    prefetch_distance: usize,
    threads: usize,
    inline_worker: bool,
    cpus: Vec<usize>,
    cpus_label: String,
    duration_ms: u64,
    start_cooldown_ms: u64,
    traffic_active_ms: u64,
    traffic_cool_ms: u64,
    size_bytes: usize,
    stride_bytes: usize,
    base_offset_bytes: usize,
    residue_period: usize,
    residue_id: usize,
    xor_mask: u64,
    numa_policy: NumaPolicy,
    numa_nodes: Vec<usize>,
    numa_nodes_label: String,
    page_mode: PageMode,
    measure_cs: bool,
    measure_l3: bool,
    l3_cpus: Vec<usize>,
    l3_cpus_label: String,
    measure_fill: bool,
    fill_cpus: Vec<usize>,
    fill_cpus_label: String,
    links: Vec<u32>,
    msr_cpu: u32,
    sample_window_ms: u64,
    active_link_floor_mib_s: f64,
    batch_ops: u64,
    lines_per_step: usize,
    selected_order: SelectedOrder,
    pfn_mask: u64,
    pfn_value: u64,
    pfn_max_pages: usize,
    predicted_cs_select: usize,
    predicted_cs_link: u32,
    predicted_cs_line_offsets: Vec<usize>,
    predicted_cs_line_offsets_label: String,
    predicted_cs_max_pages: usize,
    predicted_cs_autotune: bool,
    predicted_cs_segment_pages: usize,
    predicted_cs_phys_segment_pages: usize,
    predicted_cs_pfn_allowlist: Option<PathBuf>,
    predicted_cs_pfn_formula: PredictedCsPfnFormula,
    predicted_cs_pfn_bucket_shift: u32,
    predicted_cs_pfn_min_target_share: f64,
    predicted_cs_pfn_require_top: bool,
    predicted_cs_pfn_bucket_score_out: Option<PathBuf>,
    predicted_cs_pfn_bucket_score_seed: Option<PathBuf>,
    predicted_cs_pfn_bucket_score_all_links: bool,
    predicted_cs_pfn_bucket_sample_offsets: usize,
    hot_cs_select: usize,
    hot_cs_link: Option<u32>,
    hot_cs_candidates: usize,
    hot_cs_group_pages: usize,
    hot_cs_window_ms: u64,
    hot_cs_score_unit: HotCsScoreUnit,
    hot_cs_line_offsets: Vec<usize>,
    hot_cs_line_offsets_label: String,
    hot_cs_score_out: Option<PathBuf>,
    verified_lines_in: Option<PathBuf>,
    verified_lines_out: Option<PathBuf>,
    verified_lines_match: VerifiedLineMatch,
    verified_lines_color_mask: u64,
    verified_lines_alloc_retries: usize,
    verified_lines_min_offsets: usize,
    cache_evict_bytes: usize,
    cache_evict_stride_bytes: usize,
    side_stream_threads: usize,
    side_stream_cpus: Vec<usize>,
    side_stream_cpus_label: String,
    side_stream_size_bytes: usize,
    side_stream_stride_bytes: usize,
    flush_threads: usize,
    self_flush: bool,
    flush_cpus: Vec<usize>,
    flush_cpus_label: String,
    flush_instruction: FlushInstruction,
    flush_target: FlushTarget,
    flush_pause_ns: u64,
    flush_max_lines: usize,
    chase_nodes: usize,
}

#[derive(Clone, Debug, Default)]
struct WorkerStats {
    ops: u64,
    touched_bytes: u64,
    evict_touched_bytes: u64,
    batch_ns_per_access: Vec<f64>,
    cpu_time_ms: f64,
}

impl WorkerStats {
    fn merge(&mut self, mut other: WorkerStats) {
        self.ops = self.ops.saturating_add(other.ops);
        self.touched_bytes = self.touched_bytes.saturating_add(other.touched_bytes);
        self.evict_touched_bytes = self
            .evict_touched_bytes
            .saturating_add(other.evict_touched_bytes);
        self.cpu_time_ms += other.cpu_time_ms;
        self.batch_ns_per_access
            .append(&mut other.batch_ns_per_access);
    }
}

#[derive(Clone, Debug, Default)]
struct SideStreamStats {
    touched_bytes: u64,
    cpu_time_ms: f64,
}

#[derive(Clone, Debug, Default)]
struct FlushStats {
    ops: u64,
    cpu_time_ms: f64,
}

#[derive(Clone, Debug, Default)]
struct CsLinkBw {
    link_id: u32,
    read_mib_s: f64,
    write_mib_s: f64,
}

#[derive(Clone, Debug)]
struct RunResult {
    cfg: RunConfig,
    interrupted: bool,
    selected_offsets: usize,
    calibrated_hot_link: Option<u32>,
    flush_offsets: usize,
    elapsed_ms: f64,
    worker_cpu_ms: f64,
    side_stream_cpu_ms: f64,
    flush_cpu_ms: f64,
    total_thread_cpu_ms: f64,
    operations: u64,
    touched_bytes: u64,
    evict_touched_bytes: u64,
    flush_ops: u64,
    side_touched_bytes: u64,
    mib_s: f64,
    worker_cpu_mib_s: f64,
    evict_mib_s: f64,
    flush_mops_s: f64,
    side_mib_s: f64,
    ns_access_p50: f64,
    ns_access_p99: f64,
    l3_summary: L3Summary,
    fill_summary: FillSummary,
    cs_links: Vec<CsLinkBw>,
    cs_summary: CsSummary,
}

#[derive(Clone, Debug, Default)]
struct CsSummary {
    total_read_mib_s: f64,
    total_write_mib_s: f64,
    active_links: usize,
    top_link: i32,
    top_link_share: f64,
    max_mean_skew: f64,
    gini_skew: f64,
}

#[derive(Clone, Debug, Default)]
struct L3Summary {
    sampled_domains: usize,
    lookup_all_per_s: f64,
    lookup_miss_per_s: f64,
    hit_ratio: f64,
    miss_ratio: f64,
    miss_mib_s: f64,
    raw_lookup_all: u64,
    raw_lookup_miss: u64,
}

#[derive(Clone, Debug, Default)]
struct FillSummary {
    sampled_cpus: usize,
    local_ccx_mib_s: f64,
    near_cache_mib_s: f64,
    dram_near_mib_s: f64,
    total_mib_s: f64,
    local_ccx_ratio: f64,
    near_cache_ratio: f64,
    dram_near_ratio: f64,
    raw_local_ccx: u64,
    raw_near_cache: u64,
    raw_dram_near: u64,
}

struct MmapAllocation {
    ptr: *mut u8,
    len: usize,
}

impl MmapAllocation {
    fn new(requested_len: usize, page_mode: PageMode) -> Result<Self> {
        let len = match page_mode {
            PageMode::Hugetlb => round_up(requested_len.max(1), HUGE_PAGE_BYTES),
            PageMode::Base | PageMode::Thp => requested_len.max(PAGE_BYTES),
        };
        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if page_mode == PageMode::Hugetlb {
            flags |= libc::MAP_HUGETLB;
        }
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!("mmap failed for {len} bytes with page mode {page_mode}")
            });
        }
        let alloc = Self {
            ptr: ptr.cast::<u8>(),
            len,
        };
        match page_mode {
            PageMode::Thp => alloc.madvise(libc::MADV_HUGEPAGE, "MADV_HUGEPAGE")?,
            PageMode::Base => {
                let _ = alloc.madvise(libc::MADV_NOHUGEPAGE, "MADV_NOHUGEPAGE");
            }
            PageMode::Hugetlb => {}
        }
        Ok(alloc)
    }

    fn madvise(&self, advice: i32, label: &str) -> Result<()> {
        let ret = unsafe { libc::madvise(self.ptr.cast(), self.len, advice) };
        if ret != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("{label} failed"));
        }
        Ok(())
    }
}

impl Drop for MmapAllocation {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler()?;
    SIGNAL_REQUESTED.store(false, Ordering::Relaxed);
    match cli.command {
        Command::Run(args) => {
            let out = args.common.out.clone();
            let cfg = config_from_common(args.common, args.stride, args.base_offset)?;
            stop.store(false, Ordering::Relaxed);
            let result = execute_one(cfg, &stop)?;
            let was_interrupted = result.interrupted;
            write_results(out, &[result.clone()])?;
            if was_interrupted {
                write_interrupted_summary_stdout(&result)?;
            }
        }
        Command::Scan(args) => {
            let strides = parse_byte_sweep(&args.strides)?;
            let base_offsets = parse_byte_sweep(&args.base_offsets)?;
            let out = args.common.out.clone();
            let mut results = Vec::with_capacity(strides.len().saturating_mul(base_offsets.len()));
            for stride in strides {
                for base_offset in &base_offsets {
                    if signal_requested() {
                        break;
                    }
                    stop.store(false, Ordering::Relaxed);
                    let cfg = config_from_common(args.common.clone(), stride, *base_offset)?;
                    let result = execute_one(cfg, &stop)?;
                    let was_interrupted = result.interrupted;
                    results.push(result);
                    if was_interrupted {
                        break;
                    }
                }
                if signal_requested() {
                    break;
                }
            }
            write_results(out, &results)?;
            if let Some(result) = results.iter().find(|result| result.interrupted) {
                write_interrupted_summary_stdout(result)?;
            }
        }
    }
    Ok(())
}

extern "C" fn handle_signal(_: libc::c_int) {
    SIGNAL_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handler() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("failed to install signal handler for {signal}"));
            }
        }
    }
    Ok(())
}

fn signal_requested() -> bool {
    SIGNAL_REQUESTED.load(Ordering::Relaxed)
}

fn stop_requested(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed) || signal_requested()
}

fn optional_stop_requested(stop: Option<&AtomicBool>) -> bool {
    stop.map(stop_requested).unwrap_or_else(signal_requested)
}

fn config_from_common(common: CommonArgs, stride: usize, base_offset: usize) -> Result<RunConfig> {
    let cpus = parse_cpulist(common.cpus.as_deref())?;
    let cpus_label = common.cpus.unwrap_or_else(|| "unbound".to_string());
    let l3_cpus = if let Some(spec) = common.l3_cpus.as_deref() {
        parse_cpulist(Some(spec))?
    } else {
        default_l3_sample_cpus(&cpus, common.msr_cpu as usize)
    };
    let l3_cpus_label = l3_cpus
        .iter()
        .map(|cpu| cpu.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let fill_cpus = if let Some(spec) = common.fill_cpus.as_deref() {
        parse_cpulist(Some(spec))?
    } else if cpus.is_empty() {
        vec![common.msr_cpu as usize]
    } else {
        cpus.clone()
    };
    let fill_cpus_label = fill_cpus
        .iter()
        .map(|cpu| cpu.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let side_stream_cpus = parse_cpulist(common.side_stream_cpus.as_deref())?;
    let side_stream_cpus_label = common
        .side_stream_cpus
        .unwrap_or_else(|| "unbound".to_string());
    let flush_cpus = parse_cpulist(common.flush_cpus.as_deref())?;
    let flush_cpus_label = common.flush_cpus.unwrap_or_else(|| "unbound".to_string());
    let numa_nodes = parse_usize_list(common.numa_nodes.as_deref())?;
    let numa_nodes_label = if numa_nodes.is_empty() {
        "none".to_string()
    } else {
        numa_nodes
            .iter()
            .map(|node| node.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    if common.batch_ops == 0 {
        bail!("--batch-ops must be greater than zero");
    }
    if common.lines_per_step == 0 {
        bail!("--lines-per-step must be greater than zero");
    }
    if (common.traffic_active_ms == 0) != (common.traffic_cool_ms == 0) {
        bail!("--traffic-active-ms and --traffic-cool-ms must be set together");
    }
    if common.inline_worker {
        if common.threads != 1 {
            bail!("--inline-worker requires --threads 1");
        }
        if cpus.len() > 1 {
            bail!("--inline-worker accepts at most one --cpus entry");
        }
        if common.side_stream_threads > 0 {
            bail!("--inline-worker is mutually exclusive with --side-stream-threads");
        }
        if common.flush_threads > 0 {
            bail!("--inline-worker is mutually exclusive with --flush-threads");
        }
        if common.hot_cs_select > 0 {
            bail!("--inline-worker does not support hot-CS calibration");
        }
        if common.predicted_cs_autotune {
            bail!("--inline-worker does not support predicted-CS autotune");
        }
        if common.predicted_cs_pfn_bucket_score_out.is_some() {
            bail!("--inline-worker does not support PFN bucket scoring");
        }
    }
    if common.access_kind == AccessKind::Avx512Read && !avx512f_available() {
        bail!("--access-kind avx512-read requires x86_64 AVX-512F support");
    }
    if common.avx512_unroll == 0 {
        bail!("--avx512-unroll must be greater than zero");
    }
    if !matches!(common.avx512_unroll, 1 | 4 | 8 | 16) {
        bail!("--avx512-unroll must be one of 1, 4, 8, or 16");
    }
    if common.avx512_unroll != 4 && common.access_kind != AccessKind::Avx512Read {
        bail!("--avx512-unroll other than 4 requires --access-kind avx512-read");
    }
    if common.flush_instruction == FlushInstruction::Clflushopt && !clflushopt_available() {
        bail!("--flush-instruction clflushopt requires x86_64 CLFLUSHOPT support");
    }
    if common.cache_evict_mib > 0 && common.cache_evict_stride == 0 {
        bail!("--cache-evict-stride must be greater than zero");
    }
    if common.side_stream_threads > 0 && common.side_stream_stride == 0 {
        bail!("--side-stream-stride must be greater than zero");
    }
    if common.pfn_mask == 0 && common.pfn_value != 0 {
        bail!("--pfn-value requires a non-zero --pfn-mask");
    }
    if common.verified_lines_in.is_some() && common.pfn_mask != 0 {
        bail!("--verified-lines-in is mutually exclusive with --pfn-mask");
    }
    if common.verified_lines_in.is_some() && common.predicted_cs_select > 0 {
        bail!("--verified-lines-in is mutually exclusive with --predicted-cs-select");
    }
    if common.verified_lines_alloc_retries == 0 {
        bail!("--verified-lines-alloc-retries must be greater than zero");
    }
    if common.verified_lines_alloc_retries > 1 && common.verified_lines_in.is_none() {
        bail!("--verified-lines-alloc-retries greater than one requires --verified-lines-in");
    }
    if common.verified_lines_min_offsets > 0 && common.verified_lines_in.is_none() {
        bail!("--verified-lines-min-offsets requires --verified-lines-in");
    }
    if common.verified_lines_out.is_some() && common.hot_cs_select == 0 {
        bail!(
            "--verified-lines-out requires --hot-cs-select so saved lines have measured CS scores"
        );
    }
    if common.verified_lines_match == VerifiedLineMatch::Color
        && common.verified_lines_color_mask == 0
    {
        bail!("--verified-lines-match color requires a non-zero --verified-lines-color-mask");
    }
    if common.predicted_cs_select > 0 && common.pfn_mask != 0 {
        bail!("--predicted-cs-select is mutually exclusive with --pfn-mask");
    }
    if common.predicted_cs_select > 0 && common.hot_cs_select > 0 {
        bail!("--predicted-cs-select is mutually exclusive with --hot-cs-select");
    }
    if common.predicted_cs_autotune && common.predicted_cs_select == 0 {
        bail!("--predicted-cs-autotune requires --predicted-cs-select");
    }
    if common.predicted_cs_autotune && !common.measure_cs {
        bail!("--predicted-cs-autotune requires --measure-cs=true");
    }
    if common.predicted_cs_segment_pages > 0 && !common.predicted_cs_autotune {
        bail!("--predicted-cs-segment-pages requires --predicted-cs-autotune=true");
    }
    if common.predicted_cs_phys_segment_pages > 0 && !common.predicted_cs_autotune {
        bail!("--predicted-cs-phys-segment-pages requires --predicted-cs-autotune=true");
    }
    if common.predicted_cs_segment_pages > 0 && common.predicted_cs_phys_segment_pages > 0 {
        bail!("--predicted-cs-segment-pages and --predicted-cs-phys-segment-pages are mutually exclusive");
    }
    if common.predicted_cs_pfn_allowlist.is_some() && common.predicted_cs_select == 0 {
        bail!("--predicted-cs-pfn-allowlist requires --predicted-cs-select");
    }
    if common.predicted_cs_pfn_formula != PredictedCsPfnFormula::None
        && common.predicted_cs_select == 0
    {
        bail!("--predicted-cs-pfn-formula requires --predicted-cs-select");
    }
    if common.predicted_cs_pfn_bucket_score_out.is_some() && !common.measure_cs {
        bail!("--predicted-cs-pfn-bucket-score-out requires --measure-cs=true");
    }
    if common.predicted_cs_pfn_bucket_score_out.is_some()
        && predicted_cs_link_to_code(common.predicted_cs_link).is_none()
    {
        bail!(
            "--predicted-cs-link {} is unsupported by the observed EPYC line hash",
            common.predicted_cs_link
        );
    }
    if common.predicted_cs_pfn_bucket_score_out.is_some()
        && common.predicted_cs_pfn_bucket_sample_offsets < 2
    {
        bail!("--predicted-cs-pfn-bucket-sample-offsets must be at least two");
    }
    if common.predicted_cs_pfn_allowlist.is_some() && common.predicted_cs_autotune {
        bail!("--predicted-cs-pfn-allowlist is mutually exclusive with --predicted-cs-autotune");
    }
    if common.predicted_cs_pfn_formula != PredictedCsPfnFormula::None
        && common.predicted_cs_pfn_allowlist.is_some()
    {
        bail!("--predicted-cs-pfn-formula is mutually exclusive with --predicted-cs-pfn-allowlist");
    }
    if common.predicted_cs_pfn_formula != PredictedCsPfnFormula::None
        && common.predicted_cs_autotune
    {
        bail!("--predicted-cs-pfn-formula is mutually exclusive with --predicted-cs-autotune");
    }
    if common.predicted_cs_pfn_allowlist.is_some()
        && (common.predicted_cs_segment_pages > 0 || common.predicted_cs_phys_segment_pages > 0)
    {
        bail!("--predicted-cs-pfn-allowlist is mutually exclusive with predicted-CS segmented autotune");
    }
    if common.predicted_cs_pfn_formula != PredictedCsPfnFormula::None
        && (common.predicted_cs_segment_pages > 0 || common.predicted_cs_phys_segment_pages > 0)
    {
        bail!(
            "--predicted-cs-pfn-formula is mutually exclusive with predicted-CS segmented autotune"
        );
    }
    if common.predicted_cs_pfn_bucket_shift > 52 {
        bail!("--predicted-cs-pfn-bucket-shift must be <= 52");
    }
    if !(0.0..=1.0).contains(&common.predicted_cs_pfn_min_target_share) {
        bail!("--predicted-cs-pfn-min-target-share must be between 0 and 1");
    }
    if common.predicted_cs_select > 0
        && predicted_cs_link_to_code(common.predicted_cs_link).is_none()
    {
        bail!(
            "--predicted-cs-link {} is unsupported by the observed EPYC line hash",
            common.predicted_cs_link
        );
    }
    if common.hot_cs_group_pages == 0 {
        bail!("--hot-cs-group-pages must be greater than zero");
    }
    if common.hot_cs_select > 0 && !common.measure_cs {
        bail!("--hot-cs-select requires --measure-cs=true");
    }
    if common.measure_l3 && l3_cpus.is_empty() {
        bail!("--measure-l3 requires at least one --l3-cpus entry or one worker CPU");
    }
    if common.measure_fill && fill_cpus.is_empty() {
        bail!("--measure-fill requires at least one --fill-cpus entry or one worker CPU");
    }
    let predicted_cs_line_offsets = parse_byte_sweep(&common.predicted_cs_line_offsets)?;
    if common.predicted_cs_select > 0 && predicted_cs_line_offsets.is_empty() {
        bail!("--predicted-cs-line-offsets must select at least one offset");
    }
    if predicted_cs_line_offsets
        .iter()
        .any(|offset| *offset >= PAGE_BYTES || *offset % CACHE_LINE_BYTES != 0)
    {
        bail!("--predicted-cs-line-offsets entries must be 64-byte aligned and smaller than 4096");
    }
    let hot_cs_line_offsets = parse_byte_sweep(&common.hot_cs_line_offsets)?;
    if common.hot_cs_score_unit == HotCsScoreUnit::Line && hot_cs_line_offsets.is_empty() {
        bail!("--hot-cs-score-unit line requires non-empty --hot-cs-line-offsets");
    }
    if hot_cs_line_offsets
        .iter()
        .any(|offset| *offset >= PAGE_BYTES)
    {
        bail!("--hot-cs-line-offsets entries must be smaller than 4096 bytes");
    }
    let size_bytes = common
        .size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("--size-mib is too large"))?;
    let cache_evict_bytes = common
        .cache_evict_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("--cache-evict-mib is too large"))?;
    let side_stream_size_bytes = common
        .side_stream_size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("--side-stream-size-mib is too large"))?;
    if common.side_stream_threads > 0 && side_stream_size_bytes == 0 {
        bail!("--side-stream-size-mib must be greater than zero when side streams are enabled");
    }
    let links = parse_link_list(&common.links)?;
    if links.is_empty() {
        bail!("--links must select at least one CS link");
    }
    if common.numa_policy != NumaPolicy::Default && numa_nodes.is_empty() {
        bail!(
            "--numa-policy {policy} requires --numa-nodes",
            policy = common.numa_policy
        );
    }
    if common.numa_policy == NumaPolicy::Preferred && numa_nodes.len() != 1 {
        bail!("--numa-policy preferred requires exactly one --numa-nodes entry");
    }
    let predicted_cs_pfn_bucket_shift =
        if common.predicted_cs_pfn_formula == PredictedCsPfnFormula::ObservedEpycNode0V1 {
            OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT
        } else {
            common.predicted_cs_pfn_bucket_shift
        };

    Ok(RunConfig {
        pattern: common.pattern,
        address_mode: common.address_mode,
        access_kind: common.access_kind,
        avx512_unroll: common.avx512_unroll,
        prefetch_distance: common.prefetch_distance,
        threads: common.threads,
        inline_worker: common.inline_worker,
        cpus,
        cpus_label,
        duration_ms: common.duration_ms,
        start_cooldown_ms: common.start_cooldown_ms,
        traffic_active_ms: common.traffic_active_ms,
        traffic_cool_ms: common.traffic_cool_ms,
        size_bytes,
        stride_bytes: stride.max(1),
        base_offset_bytes: base_offset,
        residue_period: common.residue_period.max(1),
        residue_id: common.residue_id,
        xor_mask: common.xor_mask,
        numa_policy: common.numa_policy,
        numa_nodes,
        numa_nodes_label,
        page_mode: common.page_mode,
        measure_cs: common.measure_cs,
        measure_l3: common.measure_l3,
        l3_cpus,
        l3_cpus_label,
        measure_fill: common.measure_fill,
        fill_cpus,
        fill_cpus_label,
        links,
        msr_cpu: common.msr_cpu,
        sample_window_ms: common.sample_window_ms,
        active_link_floor_mib_s: common.active_link_floor_mib_s,
        batch_ops: common.batch_ops,
        lines_per_step: common.lines_per_step,
        selected_order: common.selected_order,
        pfn_mask: common.pfn_mask,
        pfn_value: common.pfn_value,
        pfn_max_pages: common.pfn_max_pages,
        predicted_cs_select: common.predicted_cs_select,
        predicted_cs_link: common.predicted_cs_link,
        predicted_cs_line_offsets,
        predicted_cs_line_offsets_label: common.predicted_cs_line_offsets,
        predicted_cs_max_pages: common.predicted_cs_max_pages,
        predicted_cs_autotune: common.predicted_cs_autotune,
        predicted_cs_segment_pages: common.predicted_cs_segment_pages,
        predicted_cs_phys_segment_pages: common.predicted_cs_phys_segment_pages,
        predicted_cs_pfn_allowlist: common.predicted_cs_pfn_allowlist,
        predicted_cs_pfn_formula: common.predicted_cs_pfn_formula,
        predicted_cs_pfn_bucket_shift,
        predicted_cs_pfn_min_target_share: common.predicted_cs_pfn_min_target_share,
        predicted_cs_pfn_require_top: common.predicted_cs_pfn_require_top,
        predicted_cs_pfn_bucket_score_out: common.predicted_cs_pfn_bucket_score_out,
        predicted_cs_pfn_bucket_score_seed: common.predicted_cs_pfn_bucket_score_seed,
        predicted_cs_pfn_bucket_score_all_links: common.predicted_cs_pfn_bucket_score_all_links,
        predicted_cs_pfn_bucket_sample_offsets: common.predicted_cs_pfn_bucket_sample_offsets,
        hot_cs_select: common.hot_cs_select,
        hot_cs_link: common.hot_cs_link,
        hot_cs_candidates: common.hot_cs_candidates,
        hot_cs_group_pages: common.hot_cs_group_pages,
        hot_cs_window_ms: common.hot_cs_window_ms,
        hot_cs_score_unit: common.hot_cs_score_unit,
        hot_cs_line_offsets,
        hot_cs_line_offsets_label: common.hot_cs_line_offsets,
        hot_cs_score_out: common.hot_cs_score_out,
        verified_lines_in: common.verified_lines_in,
        verified_lines_out: common.verified_lines_out,
        verified_lines_match: common.verified_lines_match,
        verified_lines_color_mask: common.verified_lines_color_mask,
        verified_lines_alloc_retries: common.verified_lines_alloc_retries,
        verified_lines_min_offsets: common.verified_lines_min_offsets,
        cache_evict_bytes,
        cache_evict_stride_bytes: common.cache_evict_stride.max(1),
        side_stream_threads: common.side_stream_threads,
        side_stream_cpus,
        side_stream_cpus_label,
        side_stream_size_bytes,
        side_stream_stride_bytes: common.side_stream_stride.max(1),
        flush_threads: common.flush_threads,
        self_flush: common.self_flush,
        flush_cpus,
        flush_cpus_label,
        flush_instruction: common.flush_instruction,
        flush_target: common.flush_target,
        flush_pause_ns: common.flush_pause_ns,
        flush_max_lines: common.flush_max_lines,
        chase_nodes: common.chase_nodes.max(2),
    })
}

fn allocate_workload_buffer(
    alloc_len: usize,
    cfg: &RunConfig,
) -> Result<(MmapAllocation, Option<Vec<usize>>)> {
    let attempts = if cfg.verified_lines_in.is_some() {
        cfg.verified_lines_alloc_retries.max(1)
    } else {
        1
    };
    let required_offsets = cfg.verified_lines_min_offsets.max(2);
    let retry_match_override =
        if attempts > 1 && cfg.verified_lines_match == VerifiedLineMatch::Auto {
            Some(VerifiedLineMatch::ExactPfn)
        } else {
            None
        };

    let mut last_error = None;
    for attempt in 1..=attempts {
        let alloc = MmapAllocation::new(alloc_len, cfg.page_mode)?;
        apply_numa_policy(&alloc, cfg.numa_policy, &cfg.numa_nodes)?;
        first_touch_pages(&alloc);

        let Some(path) = cfg.verified_lines_in.as_ref() else {
            return Ok((alloc, None));
        };
        match load_verified_line_offsets(&alloc, cfg, path, required_offsets, retry_match_override)
        {
            Ok(offsets) => {
                if attempts > 1 {
                    eprintln!(
                        "verified-lines allocation attempt {attempt}/{attempts} satisfied {} required offsets",
                        required_offsets
                    );
                }
                return Ok((alloc, Some(offsets)));
            }
            Err(err) => {
                if attempt == attempts {
                    return Err(err).with_context(|| {
                        format!(
                            "verified-lines allocation retries exhausted after {attempts} attempts"
                        )
                    });
                }
                eprintln!(
                    "verified-lines allocation attempt {attempt}/{attempts} did not satisfy catalog: {err:#}"
                );
                last_error = Some(err);
            }
        }
    }

    if let Some(err) = last_error {
        Err(err)
    } else {
        bail!("failed to allocate workload buffer")
    }
}

fn execute_one(cfg: RunConfig, stop: &Arc<AtomicBool>) -> Result<RunResult> {
    let alloc_len = cfg
        .size_bytes
        .checked_add(cfg.base_offset_bytes)
        .and_then(|len| len.checked_add(PAGE_BYTES))
        .ok_or_else(|| anyhow!("allocation size overflow"))?;
    let (alloc, verified_offset_pool) = allocate_workload_buffer(alloc_len, &cfg)?;
    let cache_evict_alloc = if cfg.cache_evict_bytes > 0 {
        let evict = MmapAllocation::new(cfg.cache_evict_bytes, cfg.page_mode)?;
        apply_numa_policy(&evict, cfg.numa_policy, &cfg.numa_nodes)?;
        first_touch_pages(&evict);
        Some(evict)
    } else {
        None
    };
    let side_stream_alloc = if cfg.side_stream_threads > 0 {
        let side = MmapAllocation::new(cfg.side_stream_size_bytes, cfg.page_mode)?;
        apply_numa_policy(&side, cfg.numa_policy, &cfg.numa_nodes)?;
        first_touch_pages(&side);
        Some(side)
    } else {
        None
    };

    if let Some(path) = cfg.predicted_cs_pfn_bucket_score_out.as_ref() {
        write_predicted_cs_pfn_bucket_score(&alloc, &cfg, path)?;
    }

    let mut calibrated_hot_link = None;
    let mut offset_pool = if let Some(offsets) = verified_offset_pool {
        offsets
    } else if cfg.pfn_mask != 0 {
        collect_pfn_colored_offsets(&alloc, &cfg)?
    } else if cfg.predicted_cs_select > 0 {
        if cfg.predicted_cs_pfn_allowlist.is_some() {
            collect_bucket_allowlisted_predicted_cs_offsets(&alloc, &cfg)?
        } else if cfg.predicted_cs_pfn_formula != PredictedCsPfnFormula::None {
            collect_formula_predicted_cs_offsets(&alloc, &cfg)?
        } else if cfg.predicted_cs_autotune {
            if cfg.predicted_cs_phys_segment_pages > 0 {
                collect_phys_segmented_autotuned_predicted_cs_offsets(&alloc, &cfg)?
            } else if cfg.predicted_cs_segment_pages > 0 {
                collect_segmented_autotuned_predicted_cs_offsets(&alloc, &cfg)?
            } else {
                collect_autotuned_predicted_cs_offsets(&alloc, &cfg)?
            }
        } else {
            collect_predicted_cs_offsets(&alloc, &cfg, cfg.predicted_cs_link)?
        }
    } else {
        Vec::new()
    };
    if cfg.hot_cs_select > 0 {
        let candidates = if offset_pool.is_empty() {
            collect_generated_candidate_offsets(&cfg, alloc.len, cfg.hot_cs_candidates)
        } else {
            offset_pool
                .iter()
                .copied()
                .take(cfg.hot_cs_candidates.max(1))
                .collect()
        };
        let calibrated = calibrate_hot_cs_offsets(alloc.ptr as usize, alloc.len, &cfg, candidates)?;
        if let Some(path) = cfg.verified_lines_out.as_ref() {
            write_verified_line_catalog(path, alloc.ptr as usize, &calibrated, &cfg)?;
        }
        offset_pool = calibrated.offsets;
        calibrated_hot_link = Some(calibrated.target_link);
    }
    let selected_offsets = offset_pool.len();
    let offset_pool = Arc::new(offset_pool);

    let chase_ring = if cfg.pattern == Pattern::Chase {
        Some(build_chase_ring(
            alloc.ptr as usize,
            alloc.len,
            &cfg,
            offset_pool.as_slice(),
        )?)
    } else {
        None
    };
    let flush_offsets = if cfg.flush_threads > 0 {
        build_flush_offsets(
            alloc.len,
            &cfg,
            offset_pool.as_slice(),
            chase_ring.as_deref(),
        )?
    } else {
        Vec::new()
    };
    let flush_offsets_count = flush_offsets.len();
    let flush_offsets = Arc::new(flush_offsets);
    let cache_evict_base = cache_evict_alloc
        .as_ref()
        .map(|alloc| alloc.ptr as usize)
        .unwrap_or(0);
    let cache_evict_len = cache_evict_alloc
        .as_ref()
        .map(|alloc| alloc.len)
        .unwrap_or(0);

    if cfg.inline_worker {
        return execute_one_inline(
            cfg,
            alloc.ptr as usize,
            alloc_len,
            selected_offsets,
            calibrated_hot_link,
            flush_offsets_count,
            offset_pool.as_slice(),
            chase_ring.as_deref(),
            cache_evict_base,
            cache_evict_len,
            stop,
        );
    }

    let barrier = Arc::new(Barrier::new(
        cfg.threads + cfg.side_stream_threads + cfg.flush_threads + 1,
    ));
    let mut handles = Vec::with_capacity(cfg.threads);
    for thread_idx in 0..cfg.threads {
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        let offset_pool = Arc::clone(&offset_pool);
        let cfg_for_thread = cfg.clone();
        let start_offset = chase_ring
            .as_ref()
            .and_then(|ring| ring.get(thread_idx % ring.len()).copied())
            .unwrap_or(0);
        let base_addr = alloc.ptr as usize;
        let assigned_cpu = cfg.cpus.get(thread_idx % cfg.cpus.len().max(1)).copied();
        handles.push(thread::spawn(move || -> Result<WorkerStats> {
            if let Some(cpu) = assigned_cpu {
                pin_current_thread(cpu)
                    .with_context(|| format!("failed to pin worker {thread_idx} to CPU {cpu}"))?;
            }
            barrier.wait();
            Ok(run_worker(
                base_addr,
                alloc_len,
                thread_idx,
                start_offset,
                &cfg_for_thread,
                offset_pool.as_slice(),
                cache_evict_base,
                cache_evict_len,
                &stop,
                None,
            ))
        }));
    }
    let side_stream_base = side_stream_alloc
        .as_ref()
        .map(|alloc| alloc.ptr as usize)
        .unwrap_or(0);
    let side_stream_len = side_stream_alloc
        .as_ref()
        .map(|alloc| alloc.len)
        .unwrap_or(0);
    let mut side_handles = Vec::with_capacity(cfg.side_stream_threads);
    for side_idx in 0..cfg.side_stream_threads {
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        let cfg_for_thread = cfg.clone();
        let assigned_cpu = cfg_for_thread
            .side_stream_cpus
            .get(side_idx % cfg_for_thread.side_stream_cpus.len().max(1))
            .copied();
        side_handles.push(thread::spawn(move || -> Result<SideStreamStats> {
            if let Some(cpu) = assigned_cpu {
                pin_current_thread(cpu).with_context(|| {
                    format!("failed to pin side stream {side_idx} to CPU {cpu}")
                })?;
            }
            barrier.wait();
            Ok(run_side_stream_worker(
                side_stream_base,
                side_stream_len,
                cfg_for_thread.side_stream_stride_bytes,
                &stop,
            ))
        }));
    }
    let mut flush_handles = Vec::with_capacity(cfg.flush_threads);
    for flush_idx in 0..cfg.flush_threads {
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        let cfg_for_thread = cfg.clone();
        let flush_offsets = Arc::clone(&flush_offsets);
        let base_addr = alloc.ptr as usize;
        let assigned_cpu = cfg_for_thread
            .flush_cpus
            .get(flush_idx % cfg_for_thread.flush_cpus.len().max(1))
            .copied();
        flush_handles.push(thread::spawn(move || -> Result<FlushStats> {
            if let Some(cpu) = assigned_cpu {
                pin_current_thread(cpu).with_context(|| {
                    format!("failed to pin flush worker {flush_idx} to CPU {cpu}")
                })?;
            }
            barrier.wait();
            Ok(run_flush_worker(
                base_addr,
                flush_offsets.as_slice(),
                flush_idx,
                cfg_for_thread.flush_threads,
                cfg_for_thread.flush_instruction,
                cfg_for_thread.flush_pause_ns,
                &stop,
            ))
        }));
    }
    sleep_start_cooldown(&cfg, Some(stop.as_ref()));

    let l3_sampler = if cfg.measure_l3 {
        Some(start_l3_sampling(&cfg)?)
    } else {
        None
    };
    let fill_sampler = if cfg.measure_fill {
        Some(start_fill_sampling(&cfg)?)
    } else {
        None
    };

    barrier.wait();
    let started = Instant::now();
    let cs_links = if cfg.measure_cs {
        sample_cs_links(&cfg, Some(stop.as_ref()))?
    } else {
        sleep_until_stop(Duration::from_millis(cfg.duration_ms), Some(stop.as_ref()));
        cfg.links
            .iter()
            .map(|link_id| CsLinkBw {
                link_id: *link_id,
                ..Default::default()
            })
            .collect()
    };
    stop.store(true, Ordering::Relaxed);

    let mut worker_stats = Vec::with_capacity(handles.len());
    for handle in handles {
        worker_stats.push(
            handle
                .join()
                .map_err(|_| anyhow!("worker thread panicked"))??,
        );
    }
    let mut side_touched_bytes = 0u64;
    let mut side_stream_cpu_ms = 0.0;
    for handle in side_handles {
        let stats = handle
            .join()
            .map_err(|_| anyhow!("side stream thread panicked"))??;
        side_touched_bytes = side_touched_bytes.saturating_add(stats.touched_bytes);
        side_stream_cpu_ms += stats.cpu_time_ms;
    }
    let mut flush_ops = 0u64;
    let mut flush_cpu_ms = 0.0;
    for handle in flush_handles {
        let stats = handle
            .join()
            .map_err(|_| anyhow!("flush worker thread panicked"))??;
        flush_ops = flush_ops.saturating_add(stats.ops);
        flush_cpu_ms += stats.cpu_time_ms;
    }
    finish_execution(
        cfg,
        selected_offsets,
        calibrated_hot_link,
        flush_offsets_count,
        started,
        worker_stats,
        side_touched_bytes,
        side_stream_cpu_ms,
        flush_ops,
        flush_cpu_ms,
        cs_links,
        l3_sampler,
        fill_sampler,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_one_inline(
    cfg: RunConfig,
    base_addr: usize,
    alloc_len: usize,
    selected_offsets: usize,
    calibrated_hot_link: Option<u32>,
    flush_offsets_count: usize,
    offset_pool: &[usize],
    chase_ring: Option<&[usize]>,
    cache_evict_base: usize,
    cache_evict_len: usize,
    stop: &AtomicBool,
) -> Result<RunResult> {
    if let Some(cpu) = cfg.cpus.first().copied() {
        pin_current_thread(cpu)
            .with_context(|| format!("failed to pin inline worker to CPU {cpu}"))?;
    }

    sleep_start_cooldown(&cfg, Some(stop));

    let l3_sampler = if cfg.measure_l3 {
        Some(start_l3_sampling(&cfg)?)
    } else {
        None
    };
    let fill_sampler = if cfg.measure_fill {
        Some(start_fill_sampling(&cfg)?)
    } else {
        None
    };

    let started = Instant::now();
    let start_offset = chase_ring
        .and_then(|ring| ring.first().copied())
        .unwrap_or(0);
    let mut worker_stats = WorkerStats::default();
    let cs_links = if cfg.measure_cs {
        sample_cs_links_inline_worker(
            &cfg,
            base_addr,
            alloc_len,
            start_offset,
            offset_pool,
            cache_evict_base,
            cache_evict_len,
            stop,
            &mut worker_stats,
        )?
    } else {
        let deadline = Instant::now() + Duration::from_millis(cfg.duration_ms);
        worker_stats.merge(run_worker(
            base_addr,
            alloc_len,
            0,
            start_offset,
            &cfg,
            offset_pool,
            cache_evict_base,
            cache_evict_len,
            stop,
            Some(deadline),
        ));
        cfg.links
            .iter()
            .map(|link_id| CsLinkBw {
                link_id: *link_id,
                ..Default::default()
            })
            .collect()
    };
    stop.store(true, Ordering::Relaxed);

    finish_execution(
        cfg,
        selected_offsets,
        calibrated_hot_link,
        flush_offsets_count,
        started,
        vec![worker_stats],
        0,
        0.0,
        0,
        0.0,
        cs_links,
        l3_sampler,
        fill_sampler,
    )
}

#[allow(clippy::too_many_arguments)]
fn sample_cs_links_inline_worker(
    cfg: &RunConfig,
    base_addr: usize,
    alloc_len: usize,
    start_offset: usize,
    offset_pool: &[usize],
    cache_evict_base: usize,
    cache_evict_len: usize,
    stop: &AtomicBool,
    worker_stats: &mut WorkerStats,
) -> Result<Vec<CsLinkBw>> {
    let path = format!("/dev/cpu/{}/msr", cfg.msr_cpu);
    let file = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {path}; root and msr support are required"))?;
    let fd = file.as_raw_fd();
    let window_ms = if cfg.sample_window_ms > 0 {
        cfg.sample_window_ms
    } else {
        (cfg.duration_ms / cfg.links.len().max(1) as u64).max(1)
    };
    let mut samples = Vec::with_capacity(cfg.links.len());
    for link_id in &cfg.links {
        if stop_requested(stop) {
            samples.push(CsLinkBw {
                link_id: *link_id,
                ..Default::default()
            });
            continue;
        }
        let started = start_cs_link_counter(fd, *link_id)?;
        let deadline = Instant::now() + Duration::from_millis(window_ms.max(1));
        worker_stats.merge(run_worker(
            base_addr,
            alloc_len,
            0,
            start_offset,
            cfg,
            offset_pool,
            cache_evict_base,
            cache_evict_len,
            stop,
            Some(deadline),
        ));
        samples.push(finish_cs_link_counter(fd, *link_id, started)?);
    }
    disable_counters(fd);
    Ok(samples)
}

fn finish_execution(
    cfg: RunConfig,
    selected_offsets: usize,
    calibrated_hot_link: Option<u32>,
    flush_offsets_count: usize,
    started: Instant,
    worker_stats: Vec<WorkerStats>,
    side_touched_bytes: u64,
    side_stream_cpu_ms: f64,
    flush_ops: u64,
    flush_cpu_ms: f64,
    cs_links: Vec<CsLinkBw>,
    l3_sampler: Option<L3Sampler>,
    fill_sampler: Option<FillSampler>,
) -> Result<RunResult> {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let operations = worker_stats.iter().map(|stats| stats.ops).sum::<u64>();
    let touched_bytes = worker_stats
        .iter()
        .map(|stats| stats.touched_bytes)
        .sum::<u64>();
    let evict_touched_bytes = worker_stats
        .iter()
        .map(|stats| stats.evict_touched_bytes)
        .sum::<u64>();
    let worker_cpu_ms = worker_stats
        .iter()
        .map(|stats| stats.cpu_time_ms)
        .sum::<f64>();
    let total_thread_cpu_ms = worker_cpu_ms + side_stream_cpu_ms + flush_cpu_ms;
    let mib_s = if elapsed_ms > 0.0 {
        touched_bytes as f64 / (1024.0 * 1024.0) / (elapsed_ms / 1_000.0)
    } else {
        0.0
    };
    let worker_cpu_mib_s = if worker_cpu_ms > 0.0 {
        touched_bytes as f64 / (1024.0 * 1024.0) / (worker_cpu_ms / 1_000.0)
    } else {
        0.0
    };
    let evict_mib_s = if elapsed_ms > 0.0 {
        evict_touched_bytes as f64 / (1024.0 * 1024.0) / (elapsed_ms / 1_000.0)
    } else {
        0.0
    };
    let side_mib_s = if elapsed_ms > 0.0 {
        side_touched_bytes as f64 / (1024.0 * 1024.0) / (elapsed_ms / 1_000.0)
    } else {
        0.0
    };
    let flush_mops_s = if elapsed_ms > 0.0 {
        flush_ops as f64 / 1_000_000.0 / (elapsed_ms / 1_000.0)
    } else {
        0.0
    };
    let mut batch_samples = worker_stats
        .into_iter()
        .flat_map(|stats| stats.batch_ns_per_access)
        .collect::<Vec<_>>();
    let ns_access_p50 = percentile(&mut batch_samples, 0.50);
    let ns_access_p99 = percentile(&mut batch_samples, 0.99);
    let l3_summary = if let Some(sampler) = l3_sampler {
        finish_l3_sampling(sampler)?
    } else {
        L3Summary::default()
    };
    let fill_summary = if let Some(sampler) = fill_sampler {
        finish_fill_sampling(sampler)?
    } else {
        FillSummary::default()
    };
    let cs_summary = summarize_cs(&cs_links, cfg.active_link_floor_mib_s);

    Ok(RunResult {
        cfg,
        interrupted: signal_requested(),
        selected_offsets,
        calibrated_hot_link,
        flush_offsets: flush_offsets_count,
        elapsed_ms,
        worker_cpu_ms,
        side_stream_cpu_ms,
        flush_cpu_ms,
        total_thread_cpu_ms,
        operations,
        touched_bytes,
        evict_touched_bytes,
        flush_ops,
        side_touched_bytes,
        mib_s,
        worker_cpu_mib_s,
        evict_mib_s,
        flush_mops_s,
        side_mib_s,
        ns_access_p50,
        ns_access_p99,
        l3_summary,
        fill_summary,
        cs_links,
        cs_summary,
    })
}

fn run_worker(
    base_addr: usize,
    alloc_len: usize,
    thread_idx: usize,
    start_offset: usize,
    cfg: &RunConfig,
    offset_pool: &[usize],
    cache_evict_base: usize,
    cache_evict_len: usize,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> WorkerStats {
    let cpu_started_ms = thread_cpu_time_ms();
    let mut stats = WorkerStats::default();
    let mut logical = thread_idx as u64;
    let mut chase_offset = start_offset;
    let thread_stride = cfg.threads.max(1) as u64;
    let traffic_started = Instant::now();

    while !worker_should_stop(stop, deadline) {
        if wait_for_traffic_active_phase(cfg, traffic_started, stop, deadline) {
            break;
        }
        if cache_evict_len > 0 {
            stats.evict_touched_bytes = stats.evict_touched_bytes.saturating_add(scan_read_buffer(
                cache_evict_base,
                cache_evict_len,
                cfg.cache_evict_stride_bytes,
                stop,
            ));
            if worker_should_stop(stop, deadline) {
                break;
            }
        }
        let batch_start = Instant::now();
        let mut batch_remaining = cfg.batch_ops;
        let mut batch_done = 0u64;
        while batch_remaining > 0 && !stop_requested(stop) {
            unsafe {
                if let Some(unroll) = stream_pool_avx512_unroll(cfg, batch_remaining) {
                    if unroll == 4 && cfg.prefetch_distance == 0 {
                        let offset0 =
                            pooled_or_generated_offset(cfg, offset_pool, logical, alloc_len);
                        let offset1 = pooled_or_generated_offset(
                            cfg,
                            offset_pool,
                            logical.wrapping_add(thread_stride),
                            alloc_len,
                        );
                        let offset2 = pooled_or_generated_offset(
                            cfg,
                            offset_pool,
                            logical.wrapping_add(thread_stride.saturating_mul(2)),
                            alloc_len,
                        );
                        let offset3 = pooled_or_generated_offset(
                            cfg,
                            offset_pool,
                            logical.wrapping_add(thread_stride.saturating_mul(3)),
                            alloc_len,
                        );
                        touch_avx512_read_4x(
                            (base_addr + offset0) as *const u8,
                            (base_addr + offset1) as *const u8,
                            (base_addr + offset2) as *const u8,
                            (base_addr + offset3) as *const u8,
                        );
                    } else {
                        touch_stream_pool_avx512(
                            base_addr,
                            cfg,
                            offset_pool,
                            logical,
                            thread_stride,
                            alloc_len,
                            unroll,
                        );
                    }
                    logical = logical.wrapping_add(thread_stride.saturating_mul(unroll as u64));
                    batch_remaining = batch_remaining.saturating_sub(unroll as u64);
                    batch_done = batch_done.saturating_add(unroll as u64);
                    continue;
                }
                if let Some(unroll) = stream_pool_clflushopt_avx512_unroll(cfg, batch_remaining) {
                    let offset0 = pooled_or_generated_offset(cfg, offset_pool, logical, alloc_len);
                    let offset1 = pooled_or_generated_offset(
                        cfg,
                        offset_pool,
                        logical.wrapping_add(thread_stride),
                        alloc_len,
                    );
                    let offset2 = pooled_or_generated_offset(
                        cfg,
                        offset_pool,
                        logical.wrapping_add(thread_stride.saturating_mul(2)),
                        alloc_len,
                    );
                    let offset3 = pooled_or_generated_offset(
                        cfg,
                        offset_pool,
                        logical.wrapping_add(thread_stride.saturating_mul(3)),
                        alloc_len,
                    );
                    flush_cache_line((base_addr + offset0) as *const u8, cfg.flush_instruction);
                    flush_cache_line((base_addr + offset1) as *const u8, cfg.flush_instruction);
                    flush_cache_line((base_addr + offset2) as *const u8, cfg.flush_instruction);
                    flush_cache_line((base_addr + offset3) as *const u8, cfg.flush_instruction);
                    flush_cache_fence(cfg.flush_instruction);
                    touch_avx512_read_4x(
                        (base_addr + offset0) as *const u8,
                        (base_addr + offset1) as *const u8,
                        (base_addr + offset2) as *const u8,
                        (base_addr + offset3) as *const u8,
                    );
                    logical = logical.wrapping_add(thread_stride.saturating_mul(unroll as u64));
                    batch_remaining = batch_remaining.saturating_sub(unroll as u64);
                    batch_done = batch_done.saturating_add(unroll as u64);
                    continue;
                }

                match cfg.pattern {
                    Pattern::Stream => {
                        let anchor =
                            pooled_or_generated_offset(cfg, offset_pool, logical, alloc_len);
                        touch_step_lines(base_addr, anchor, alloc_len, cfg, cfg.self_flush);
                    }
                    Pattern::Chase => {
                        self_flush_line_if_enabled(
                            base_addr,
                            chase_offset,
                            cfg.self_flush,
                            cfg.flush_instruction,
                        );
                        let next = ptr::read_volatile((base_addr + chase_offset) as *const usize);
                        let payload_anchor = (next + WORD_BYTES).min(alloc_len.saturating_sub(1));
                        touch_step_lines(base_addr, payload_anchor, alloc_len, cfg, false);
                        chase_offset = next;
                    }
                }
            }
            logical = logical.wrapping_add(thread_stride);
            batch_remaining = batch_remaining.saturating_sub(1);
            batch_done = batch_done.saturating_add(1);
        }
        if batch_done == 0 {
            break;
        }
        let batch_ns = batch_start.elapsed().as_secs_f64() * 1_000_000_000.0;
        let batch_accesses = batch_done.saturating_mul(cfg.lines_per_step as u64);
        stats
            .batch_ns_per_access
            .push(batch_ns / batch_accesses.max(1) as f64);
        stats.ops = stats.ops.saturating_add(batch_accesses);
        stats.touched_bytes = stats
            .touched_bytes
            .saturating_add(batch_accesses.saturating_mul(CACHE_LINE_BYTES as u64));
    }
    stats.cpu_time_ms = (thread_cpu_time_ms() - cpu_started_ms).max(0.0);
    stats
}

fn run_side_stream_worker(
    base_addr: usize,
    len: usize,
    stride_bytes: usize,
    stop: &AtomicBool,
) -> SideStreamStats {
    let cpu_started_ms = thread_cpu_time_ms();
    if len == 0 {
        return SideStreamStats {
            cpu_time_ms: (thread_cpu_time_ms() - cpu_started_ms).max(0.0),
            ..Default::default()
        };
    }
    let mut touched_bytes = 0u64;
    while !stop_requested(stop) {
        touched_bytes =
            touched_bytes.saturating_add(scan_read_buffer(base_addr, len, stride_bytes, stop));
    }
    SideStreamStats {
        touched_bytes,
        cpu_time_ms: (thread_cpu_time_ms() - cpu_started_ms).max(0.0),
    }
}

fn run_flush_worker(
    base_addr: usize,
    offsets: &[usize],
    worker_idx: usize,
    workers: usize,
    flush_instruction: FlushInstruction,
    pause_ns: u64,
    stop: &AtomicBool,
) -> FlushStats {
    let cpu_started_ms = thread_cpu_time_ms();
    if offsets.is_empty() {
        return FlushStats {
            cpu_time_ms: (thread_cpu_time_ms() - cpu_started_ms).max(0.0),
            ..Default::default()
        };
    }
    let mut ops = 0u64;
    let workers = workers.max(1);
    while !stop_requested(stop) {
        for idx in (worker_idx..offsets.len()).step_by(workers) {
            if stop_requested(stop) {
                break;
            }
            unsafe {
                flush_cache_line((base_addr + offsets[idx]) as *const u8, flush_instruction);
            }
            ops = ops.saturating_add(1);
        }
        flush_cache_fence(flush_instruction);
        if pause_ns > 0 && !stop_requested(stop) {
            thread::sleep(Duration::from_nanos(pause_ns));
        }
    }
    FlushStats {
        ops,
        cpu_time_ms: (thread_cpu_time_ms() - cpu_started_ms).max(0.0),
    }
}

fn scan_read_buffer(base_addr: usize, len: usize, stride_bytes: usize, stop: &AtomicBool) -> u64 {
    let stride = stride_bytes.max(1);
    let mut touched_bytes = 0u64;
    for offset in (0..len).step_by(stride) {
        if stop_requested(stop) {
            break;
        }
        unsafe {
            touch_byte(base_addr, offset, AccessKind::Read);
        }
        touched_bytes = touched_bytes.saturating_add(CACHE_LINE_BYTES as u64);
    }
    touched_bytes
}

fn thread_cpu_time_ms() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if ret == 0 {
        ts.tv_sec as f64 * 1_000.0 + ts.tv_nsec as f64 / 1_000_000.0
    } else {
        0.0
    }
}

fn sleep_until_stop(duration: Duration, stop: Option<&AtomicBool>) {
    let started = Instant::now();
    while started.elapsed() < duration {
        if optional_stop_requested(stop) {
            break;
        }
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn sleep_start_cooldown(cfg: &RunConfig, stop: Option<&AtomicBool>) {
    if cfg.start_cooldown_ms == 0 {
        return;
    }
    sleep_until_stop(Duration::from_millis(cfg.start_cooldown_ms), stop);
}

fn traffic_phases_enabled(cfg: &RunConfig) -> bool {
    cfg.traffic_active_ms > 0 && cfg.traffic_cool_ms > 0
}

fn wait_for_traffic_active_phase(
    cfg: &RunConfig,
    traffic_started: Instant,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> bool {
    if !traffic_phases_enabled(cfg) {
        return worker_should_stop(stop, deadline);
    }

    let active_ms = u128::from(cfg.traffic_active_ms);
    let cycle_ms = active_ms + u128::from(cfg.traffic_cool_ms);
    loop {
        if worker_should_stop(stop, deadline) {
            return true;
        }
        let elapsed_ms = Instant::now()
            .saturating_duration_since(traffic_started)
            .as_millis();
        let phase_ms = elapsed_ms % cycle_ms;
        if phase_ms < active_ms {
            return false;
        }

        let remaining_ms = (cycle_ms - phase_ms).clamp(1, 10) as u64;
        let mut sleep_for = Duration::from_millis(remaining_ms);
        if let Some(deadline) = deadline {
            let until_deadline = deadline.saturating_duration_since(Instant::now());
            if until_deadline.is_zero() {
                return true;
            }
            sleep_for = sleep_for.min(until_deadline);
        }
        thread::sleep(sleep_for);
    }
}

fn worker_should_stop(stop: &AtomicBool, deadline: Option<Instant>) -> bool {
    stop_requested(stop)
        || deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false)
}

#[inline(always)]
fn stream_pool_avx512_unroll(cfg: &RunConfig, batch_remaining: u64) -> Option<usize> {
    if cfg.pattern == Pattern::Stream
        && cfg.access_kind == AccessKind::Avx512Read
        && !cfg.self_flush
        && cfg.lines_per_step == 1
        && batch_remaining >= cfg.avx512_unroll as u64
    {
        Some(cfg.avx512_unroll)
    } else {
        None
    }
}

#[inline(always)]
fn stream_pool_clflushopt_avx512_unroll(cfg: &RunConfig, batch_remaining: u64) -> Option<usize> {
    if cfg.pattern == Pattern::Stream
        && cfg.access_kind == AccessKind::Avx512Read
        && cfg.self_flush
        && cfg.flush_instruction == FlushInstruction::Clflushopt
        && cfg.lines_per_step == 1
        && batch_remaining >= 4
    {
        Some(4)
    } else {
        None
    }
}

#[inline(always)]
unsafe fn touch_stream_pool_avx512(
    base_addr: usize,
    cfg: &RunConfig,
    offset_pool: &[usize],
    logical: u64,
    thread_stride: u64,
    alloc_len: usize,
    unroll: usize,
) {
    let mut offsets = [0usize; 16];
    for idx in 0..unroll {
        offsets[idx] = pooled_or_generated_offset(
            cfg,
            offset_pool,
            logical.wrapping_add(thread_stride.saturating_mul(idx as u64)),
            alloc_len,
        );
    }
    if cfg.prefetch_distance > 0 {
        for idx in 0..unroll {
            let prefetch_logical = logical.wrapping_add(
                thread_stride.saturating_mul(cfg.prefetch_distance.saturating_add(idx) as u64),
            );
            let offset = pooled_or_generated_offset(cfg, offset_pool, prefetch_logical, alloc_len);
            prefetch_read_t0((base_addr + offset) as *const u8);
        }
    }
    touch_avx512_read_offsets(base_addr, &offsets[..unroll]);
}

#[inline(always)]
unsafe fn touch_avx512_read_offsets(base_addr: usize, offsets: &[usize]) {
    match offsets.len() {
        1 => touch_avx512_read_line((base_addr + offsets[0]) as *const u8),
        4 => touch_avx512_read_4x(
            (base_addr + offsets[0]) as *const u8,
            (base_addr + offsets[1]) as *const u8,
            (base_addr + offsets[2]) as *const u8,
            (base_addr + offsets[3]) as *const u8,
        ),
        8 => touch_avx512_read_8x(
            (base_addr + offsets[0]) as *const u8,
            (base_addr + offsets[1]) as *const u8,
            (base_addr + offsets[2]) as *const u8,
            (base_addr + offsets[3]) as *const u8,
            (base_addr + offsets[4]) as *const u8,
            (base_addr + offsets[5]) as *const u8,
            (base_addr + offsets[6]) as *const u8,
            (base_addr + offsets[7]) as *const u8,
        ),
        16 => touch_avx512_read_16x(
            (base_addr + offsets[0]) as *const u8,
            (base_addr + offsets[1]) as *const u8,
            (base_addr + offsets[2]) as *const u8,
            (base_addr + offsets[3]) as *const u8,
            (base_addr + offsets[4]) as *const u8,
            (base_addr + offsets[5]) as *const u8,
            (base_addr + offsets[6]) as *const u8,
            (base_addr + offsets[7]) as *const u8,
            (base_addr + offsets[8]) as *const u8,
            (base_addr + offsets[9]) as *const u8,
            (base_addr + offsets[10]) as *const u8,
            (base_addr + offsets[11]) as *const u8,
            (base_addr + offsets[12]) as *const u8,
            (base_addr + offsets[13]) as *const u8,
            (base_addr + offsets[14]) as *const u8,
            (base_addr + offsets[15]) as *const u8,
        ),
        _ => unreachable!("validated --avx512-unroll allows only 1, 4, 8, or 16"),
    }
}

unsafe fn touch_byte(base_addr: usize, offset: usize, kind: AccessKind) {
    let ptr = (base_addr + offset) as *mut u8;
    match kind {
        AccessKind::Read => {
            black_box(ptr::read_volatile(ptr.cast_const()));
        }
        AccessKind::Write => {
            ptr::write_volatile(ptr, 0x5a);
        }
        AccessKind::Rmw => {
            let value = ptr::read_volatile(ptr.cast_const());
            ptr::write_volatile(ptr, value.wrapping_add(1));
        }
        AccessKind::Avx512Read => {
            touch_avx512_read_line(ptr.cast_const());
        }
    }
}

unsafe fn touch_step_lines(
    base_addr: usize,
    anchor_offset: usize,
    alloc_len: usize,
    cfg: &RunConfig,
    flush_each: bool,
) {
    if cfg.access_kind == AccessKind::Avx512Read && !flush_each {
        let mut line_idx = 0usize;
        while line_idx + 4 <= cfg.lines_per_step {
            let offset0 = offset_for_step_line(cfg, anchor_offset, line_idx, alloc_len);
            let offset1 = offset_for_step_line(cfg, anchor_offset, line_idx + 1, alloc_len);
            let offset2 = offset_for_step_line(cfg, anchor_offset, line_idx + 2, alloc_len);
            let offset3 = offset_for_step_line(cfg, anchor_offset, line_idx + 3, alloc_len);
            touch_avx512_read_4x(
                (base_addr + offset0) as *const u8,
                (base_addr + offset1) as *const u8,
                (base_addr + offset2) as *const u8,
                (base_addr + offset3) as *const u8,
            );
            line_idx += 4;
        }
        while line_idx < cfg.lines_per_step {
            let offset = offset_for_step_line(cfg, anchor_offset, line_idx, alloc_len);
            touch_avx512_read_line((base_addr + offset) as *const u8);
            line_idx += 1;
        }
        return;
    }

    for line_idx in 0..cfg.lines_per_step {
        let offset = offset_for_step_line(cfg, anchor_offset, line_idx, alloc_len);
        self_flush_line_if_enabled(base_addr, offset, flush_each, cfg.flush_instruction);
        touch_byte(base_addr, offset, cfg.access_kind);
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn touch_avx512_read_line(ptr: *const u8) {
    core::arch::asm!(
        "vmovdqu64 zmm0, zmmword ptr [{ptr}]",
        ptr = in(reg) ptr,
        out("zmm0") _,
        options(nostack, readonly, preserves_flags),
    );
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn touch_avx512_read_line(ptr: *const u8) {
    black_box(ptr::read_volatile(ptr));
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn touch_avx512_read_4x(ptr0: *const u8, ptr1: *const u8, ptr2: *const u8, ptr3: *const u8) {
    core::arch::asm!(
        "vmovdqu64 zmm0, zmmword ptr [{ptr0}]",
        "vmovdqu64 zmm1, zmmword ptr [{ptr1}]",
        "vmovdqu64 zmm2, zmmword ptr [{ptr2}]",
        "vmovdqu64 zmm3, zmmword ptr [{ptr3}]",
        ptr0 = in(reg) ptr0,
        ptr1 = in(reg) ptr1,
        ptr2 = in(reg) ptr2,
        ptr3 = in(reg) ptr3,
        out("zmm0") _,
        out("zmm1") _,
        out("zmm2") _,
        out("zmm3") _,
        options(nostack, readonly, preserves_flags),
    );
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn touch_avx512_read_4x(ptr0: *const u8, ptr1: *const u8, ptr2: *const u8, ptr3: *const u8) {
    black_box(ptr::read_volatile(ptr0));
    black_box(ptr::read_volatile(ptr1));
    black_box(ptr::read_volatile(ptr2));
    black_box(ptr::read_volatile(ptr3));
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn touch_avx512_read_8x(
    ptr0: *const u8,
    ptr1: *const u8,
    ptr2: *const u8,
    ptr3: *const u8,
    ptr4: *const u8,
    ptr5: *const u8,
    ptr6: *const u8,
    ptr7: *const u8,
) {
    core::arch::asm!(
        "vmovdqu64 zmm0, zmmword ptr [{ptr0}]",
        "vmovdqu64 zmm1, zmmword ptr [{ptr1}]",
        "vmovdqu64 zmm2, zmmword ptr [{ptr2}]",
        "vmovdqu64 zmm3, zmmword ptr [{ptr3}]",
        "vmovdqu64 zmm4, zmmword ptr [{ptr4}]",
        "vmovdqu64 zmm5, zmmword ptr [{ptr5}]",
        "vmovdqu64 zmm6, zmmword ptr [{ptr6}]",
        "vmovdqu64 zmm7, zmmword ptr [{ptr7}]",
        ptr0 = in(reg) ptr0,
        ptr1 = in(reg) ptr1,
        ptr2 = in(reg) ptr2,
        ptr3 = in(reg) ptr3,
        ptr4 = in(reg) ptr4,
        ptr5 = in(reg) ptr5,
        ptr6 = in(reg) ptr6,
        ptr7 = in(reg) ptr7,
        out("zmm0") _,
        out("zmm1") _,
        out("zmm2") _,
        out("zmm3") _,
        out("zmm4") _,
        out("zmm5") _,
        out("zmm6") _,
        out("zmm7") _,
        options(nostack, readonly, preserves_flags),
    );
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn touch_avx512_read_8x(
    ptr0: *const u8,
    ptr1: *const u8,
    ptr2: *const u8,
    ptr3: *const u8,
    ptr4: *const u8,
    ptr5: *const u8,
    ptr6: *const u8,
    ptr7: *const u8,
) {
    touch_avx512_read_4x(ptr0, ptr1, ptr2, ptr3);
    touch_avx512_read_4x(ptr4, ptr5, ptr6, ptr7);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn touch_avx512_read_16x(
    ptr0: *const u8,
    ptr1: *const u8,
    ptr2: *const u8,
    ptr3: *const u8,
    ptr4: *const u8,
    ptr5: *const u8,
    ptr6: *const u8,
    ptr7: *const u8,
    ptr8: *const u8,
    ptr9: *const u8,
    ptr10: *const u8,
    ptr11: *const u8,
    ptr12: *const u8,
    ptr13: *const u8,
    ptr14: *const u8,
    ptr15: *const u8,
) {
    touch_avx512_read_8x(ptr0, ptr1, ptr2, ptr3, ptr4, ptr5, ptr6, ptr7);
    touch_avx512_read_8x(ptr8, ptr9, ptr10, ptr11, ptr12, ptr13, ptr14, ptr15);
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn touch_avx512_read_16x(
    ptr0: *const u8,
    ptr1: *const u8,
    ptr2: *const u8,
    ptr3: *const u8,
    ptr4: *const u8,
    ptr5: *const u8,
    ptr6: *const u8,
    ptr7: *const u8,
    ptr8: *const u8,
    ptr9: *const u8,
    ptr10: *const u8,
    ptr11: *const u8,
    ptr12: *const u8,
    ptr13: *const u8,
    ptr14: *const u8,
    ptr15: *const u8,
) {
    touch_avx512_read_8x(ptr0, ptr1, ptr2, ptr3, ptr4, ptr5, ptr6, ptr7);
    touch_avx512_read_8x(ptr8, ptr9, ptr10, ptr11, ptr12, ptr13, ptr14, ptr15);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn prefetch_read_t0(ptr: *const u8) {
    core::arch::asm!(
        "prefetcht0 byte ptr [{ptr}]",
        ptr = in(reg) ptr,
        options(nostack, preserves_flags),
    );
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn prefetch_read_t0(ptr: *const u8) {
    black_box(ptr);
}

fn avx512f_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx512f")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn clflushopt_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let leaf7 = unsafe { std::arch::x86_64::__cpuid_count(7, 0) };
        (leaf7.ebx & (1 << 23)) != 0
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn address_offset(cfg: &RunConfig, logical: u64, alloc_len: usize) -> usize {
    let base = cfg.base_offset_bytes.min(alloc_len.saturating_sub(1));
    let usable = cfg.size_bytes.min(alloc_len.saturating_sub(base)).max(1);
    let inner = match cfg.address_mode {
        AddressMode::Linear => (logical as usize).wrapping_mul(cfg.stride_bytes) % usable,
        AddressMode::Residue => {
            let period = cfg.residue_period.max(1);
            let residue = cfg.residue_id % period;
            (residue
                .wrapping_add((logical as usize).wrapping_mul(period))
                .wrapping_mul(cfg.stride_bytes))
                % usable
        }
        AddressMode::Xor => {
            ((logical as usize).wrapping_mul(cfg.stride_bytes) ^ cfg.xor_mask as usize) % usable
        }
        AddressMode::PageOffset => {
            let page_stride = cfg.stride_bytes.max(PAGE_BYTES);
            let page_inner = cfg.base_offset_bytes % page_stride.min(PAGE_BYTES);
            let page_count = usable.saturating_sub(page_inner).max(1) / page_stride.max(1);
            let page = if page_count == 0 {
                0
            } else {
                (logical as usize % page_count) * page_stride
            };
            (page + page_inner).min(usable.saturating_sub(1))
        }
    };
    (base + inner).min(alloc_len.saturating_sub(1))
}

#[inline(always)]
fn pooled_or_generated_offset(
    cfg: &RunConfig,
    offset_pool: &[usize],
    logical: u64,
    alloc_len: usize,
) -> usize {
    if offset_pool.is_empty() {
        address_offset(cfg, logical, alloc_len)
    } else {
        offset_pool[logical as usize % offset_pool.len()].min(alloc_len.saturating_sub(1))
    }
}

fn offset_for_step_line(
    cfg: &RunConfig,
    anchor_offset: usize,
    line_idx: usize,
    alloc_len: usize,
) -> usize {
    let base = cfg.base_offset_bytes.min(alloc_len.saturating_sub(1));
    let usable = cfg.size_bytes.min(alloc_len.saturating_sub(base)).max(1);
    let relative = anchor_offset.saturating_sub(base) % usable;
    let line_relative = relative.wrapping_add(line_idx.wrapping_mul(CACHE_LINE_BYTES)) % usable;
    (base + line_relative).min(alloc_len.saturating_sub(1))
}

fn build_chase_ring(
    base_addr: usize,
    alloc_len: usize,
    cfg: &RunConfig,
    offset_pool: &[usize],
) -> Result<Vec<usize>> {
    let target_nodes = cfg.chase_nodes.min(cfg.size_bytes / WORD_BYTES).max(2);
    let mut seen = BTreeSet::new();
    let mut offsets = Vec::with_capacity(target_nodes);
    if offset_pool.is_empty() {
        let max_attempts = target_nodes.saturating_mul(32).max(1024);
        for logical in 0..max_attempts as u64 {
            let raw = address_offset(cfg, logical, alloc_len.saturating_sub(WORD_BYTES));
            let aligned = raw - (raw % WORD_BYTES);
            if aligned + WORD_BYTES <= alloc_len && seen.insert(aligned) {
                offsets.push(aligned);
                if offsets.len() == target_nodes {
                    break;
                }
            }
        }
    } else {
        for raw in offset_pool.iter().copied() {
            let aligned = raw - (raw % WORD_BYTES);
            if aligned + WORD_BYTES <= alloc_len && seen.insert(aligned) {
                offsets.push(aligned);
                if offsets.len() == target_nodes {
                    break;
                }
            }
        }
    }
    if offsets.len() < 2 {
        bail!("address pattern produced fewer than two unique pointer-chase nodes");
    }
    for idx in 0..offsets.len() {
        let current = offsets[idx];
        let next = offsets[(idx + 1) % offsets.len()];
        unsafe {
            ptr::write_volatile((base_addr + current) as *mut usize, next);
        }
    }
    Ok(offsets)
}

fn build_flush_offsets(
    alloc_len: usize,
    cfg: &RunConfig,
    offset_pool: &[usize],
    chase_ring: Option<&[usize]>,
) -> Result<Vec<usize>> {
    let anchors = flush_anchor_offsets(alloc_len, cfg, offset_pool, chase_ring);
    let mut lines = BTreeSet::new();
    match cfg.flush_target {
        FlushTarget::Selected => add_anchor_flush_offsets(&mut lines, &anchors, alloc_len),
        FlushTarget::Chase => {
            if let Some(ring) = chase_ring {
                add_anchor_flush_offsets(&mut lines, ring, alloc_len);
            } else {
                add_anchor_flush_offsets(&mut lines, &anchors, alloc_len);
            }
        }
        FlushTarget::Payload => {
            add_payload_flush_offsets(&mut lines, cfg, &anchors, alloc_len);
        }
        FlushTarget::All => {
            if let Some(ring) = chase_ring {
                add_anchor_flush_offsets(&mut lines, ring, alloc_len);
            } else {
                add_anchor_flush_offsets(&mut lines, &anchors, alloc_len);
            }
            add_payload_flush_offsets(&mut lines, cfg, &anchors, alloc_len);
        }
    }
    let mut offsets = lines.into_iter().collect::<Vec<_>>();
    if cfg.flush_max_lines > 0 && offsets.len() > cfg.flush_max_lines {
        offsets.truncate(cfg.flush_max_lines);
    }
    if offsets.is_empty() {
        bail!("--flush-threads selected no cache lines to flush");
    }
    eprintln!(
        "clflush selected {} lines target={} threads={} pause_ns={}",
        offsets.len(),
        cfg.flush_target,
        cfg.flush_threads,
        cfg.flush_pause_ns
    );
    Ok(offsets)
}

fn flush_anchor_offsets(
    alloc_len: usize,
    cfg: &RunConfig,
    offset_pool: &[usize],
    chase_ring: Option<&[usize]>,
) -> Vec<usize> {
    if !offset_pool.is_empty() {
        return offset_pool.to_vec();
    }
    if let Some(ring) = chase_ring {
        return ring.to_vec();
    }
    collect_generated_offsets(
        cfg,
        alloc_len,
        cfg.flush_max_lines.max(cfg.chase_nodes).max(2),
    )
}

fn collect_generated_offsets(cfg: &RunConfig, alloc_len: usize, limit: usize) -> Vec<usize> {
    let target = limit.max(1);
    let mut seen = BTreeSet::new();
    let mut offsets = Vec::with_capacity(target);
    let max_attempts = target.saturating_mul(128).max(4096);
    for logical in 0..max_attempts as u64 {
        let offset = address_offset(cfg, logical, alloc_len);
        if seen.insert(cache_line_offset(offset)) {
            offsets.push(offset);
            if offsets.len() >= target {
                break;
            }
        }
    }
    offsets
}

fn add_anchor_flush_offsets(lines: &mut BTreeSet<usize>, anchors: &[usize], alloc_len: usize) {
    for offset in anchors {
        if *offset < alloc_len {
            lines.insert(cache_line_offset(*offset));
        }
    }
}

fn add_payload_flush_offsets(
    lines: &mut BTreeSet<usize>,
    cfg: &RunConfig,
    anchors: &[usize],
    alloc_len: usize,
) {
    for anchor in anchors {
        let payload_anchor = anchor
            .saturating_add(WORD_BYTES)
            .min(alloc_len.saturating_sub(1));
        for line_idx in 0..cfg.lines_per_step {
            let offset = offset_for_step_line(cfg, payload_anchor, line_idx, alloc_len);
            lines.insert(cache_line_offset(offset));
        }
    }
}

fn cache_line_offset(offset: usize) -> usize {
    offset - offset % CACHE_LINE_BYTES
}

fn self_flush_line_if_enabled(
    base_addr: usize,
    offset: usize,
    enabled: bool,
    flush_instruction: FlushInstruction,
) {
    if enabled {
        unsafe {
            flush_cache_line(
                (base_addr + cache_line_offset(offset)) as *const u8,
                flush_instruction,
            );
        }
        flush_cache_fence(flush_instruction);
    }
}

fn first_touch_pages(alloc: &MmapAllocation) {
    for offset in (0..alloc.len).step_by(PAGE_BYTES) {
        unsafe {
            ptr::write_volatile(alloc.ptr.add(offset), 0);
        }
    }
}

fn collect_pfn_colored_offsets(alloc: &MmapAllocation, cfg: &RunConfig) -> Result<Vec<usize>> {
    let pagemap = File::open("/proc/self/pagemap").context("failed to open /proc/self/pagemap")?;
    let page_intra = cfg.base_offset_bytes % PAGE_BYTES;
    let region_start = cfg.base_offset_bytes.min(alloc.len.saturating_sub(1));
    let region_end = region_start
        .saturating_add(cfg.size_bytes)
        .min(alloc.len.saturating_sub(1));
    let first_page = region_start / PAGE_BYTES;
    let last_page = region_end / PAGE_BYTES;
    let mut offsets = Vec::new();
    let mut saw_present = false;
    let mut saw_nonzero_pfn = false;
    for page_idx in first_page..=last_page {
        let virt = alloc.ptr as usize + page_idx * PAGE_BYTES;
        let entry = read_pagemap_entry(&pagemap, virt)?;
        if !pagemap_present(entry) {
            continue;
        }
        saw_present = true;
        let pfn = pagemap_pfn(entry);
        saw_nonzero_pfn |= pfn != 0;
        if pfn_color_matches(pfn, cfg.pfn_mask, cfg.pfn_value) {
            let offset = page_idx
                .saturating_mul(PAGE_BYTES)
                .saturating_add(page_intra);
            if offset < region_end {
                offsets.push(offset);
                if cfg.pfn_max_pages > 0 && offsets.len() >= cfg.pfn_max_pages {
                    break;
                }
            }
        }
    }
    if !saw_present {
        bail!("pagemap had no present pages for the selected allocation");
    }
    if !saw_nonzero_pfn {
        bail!("pagemap PFNs are hidden; run with sufficient privilege for page coloring");
    }
    if offsets.len() < 2 {
        bail!(
            "PFN color mask=0x{:x} value=0x{:x} selected fewer than two pages",
            cfg.pfn_mask,
            cfg.pfn_value
        );
    }
    eprintln!(
        "pfn-color selected {} offsets with mask=0x{:x} value=0x{:x}",
        offsets.len(),
        cfg.pfn_mask,
        cfg.pfn_value
    );
    Ok(offsets)
}

fn read_pagemap_entry(pagemap: &File, virt_addr: usize) -> Result<u64> {
    let page_idx = virt_addr / PAGE_BYTES;
    let mut bytes = [0u8; 8];
    pagemap
        .read_exact_at(&mut bytes, (page_idx * 8) as u64)
        .context("failed to read pagemap entry")?;
    Ok(u64::from_le_bytes(bytes))
}

fn pagemap_present(entry: u64) -> bool {
    (entry & (1u64 << 63)) != 0
}

fn pagemap_pfn(entry: u64) -> u64 {
    entry & ((1u64 << 55) - 1)
}

fn pfn_color_matches(pfn: u64, mask: u64, value: u64) -> bool {
    mask == 0 || (pfn & mask) == (value & mask)
}

fn configured_page_range(alloc: &MmapAllocation, cfg: &RunConfig) -> (usize, usize, usize, usize) {
    let region_start = cfg.base_offset_bytes.min(alloc.len.saturating_sub(1));
    let region_end = region_start
        .saturating_add(cfg.size_bytes)
        .min(alloc.len.saturating_sub(1));
    let first_page = region_start / PAGE_BYTES;
    let last_page = region_end / PAGE_BYTES;
    (region_start, region_end, first_page, last_page)
}

fn collect_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    predicted_link: u32,
) -> Result<Vec<usize>> {
    let pagemap = File::open("/proc/self/pagemap").context("failed to open /proc/self/pagemap")?;
    let (_region_start, _region_end, first_page, last_page) = configured_page_range(alloc, cfg);
    let max_pages = cfg
        .predicted_cs_max_pages
        .max(1)
        .min(last_page.saturating_sub(first_page).saturating_add(1));
    let last_page = if cfg.predicted_cs_max_pages > 0 {
        first_page.saturating_add(max_pages.saturating_sub(1))
    } else {
        last_page
    };
    let collected = collect_predicted_cs_offsets_from_pages(
        alloc,
        cfg,
        &pagemap,
        predicted_link,
        first_page,
        last_page,
        cfg.predicted_cs_select,
    )?;
    validate_predicted_collect_result(&collected, "predicted CS selection")?;
    let offsets = collected.offsets;
    if offsets.len() < 2 {
        bail!(
            "predicted CS{} selection produced fewer than two offsets",
            predicted_link
        );
    }
    eprintln!(
        "predicted-cs selected {} offsets targeting CS{} from {} pages using observed-epyc-line-v1",
        offsets.len(),
        predicted_link,
        collected.pages_seen
    );
    Ok(offsets)
}

#[derive(Clone, Debug)]
struct PfnBucketAllowlist {
    buckets: BTreeSet<u64>,
    bucket_links: BTreeMap<u64, BTreeSet<u32>>,
    accepted_rows: usize,
    accepted_pfns: usize,
    total_rows: usize,
}

struct BucketPredictedSource<'a> {
    bucket: u64,
    predicted_link: u32,
    pages: &'a [PagePfn],
}

#[derive(Default)]
struct BucketRoundRobinStats {
    candidate_offsets: usize,
    quota_per_source: usize,
    source_count: usize,
    used_sources: usize,
    used_buckets: usize,
}

fn collect_bucket_allowlisted_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<usize>> {
    let path = cfg
        .predicted_cs_pfn_allowlist
        .as_ref()
        .ok_or_else(|| anyhow!("missing --predicted-cs-pfn-allowlist"))?;
    let allowlist = load_pfn_bucket_allowlist(path, cfg, cfg.predicted_cs_link)?;
    let pages = collect_present_page_pfns(alloc, cfg)?;
    let mut matched_buckets = BTreeMap::<u64, Vec<PagePfn>>::new();
    for page in pages {
        let bucket = page.pfn >> cfg.predicted_cs_pfn_bucket_shift;
        if allowlist.buckets.contains(&bucket) {
            matched_buckets.entry(bucket).or_default().push(page);
        }
    }
    let matched_page_count = matched_buckets
        .values()
        .map(|pages| pages.len())
        .sum::<usize>();
    if matched_page_count == 0 {
        bail!(
            "predicted-CS PFN allowlist matched no pages from current allocation; buckets={} shift={}",
            allowlist.buckets.len(),
            cfg.predicted_cs_pfn_bucket_shift
        );
    }
    let mut sources = Vec::new();
    for (bucket, pages) in &matched_buckets {
        let links = allowlist
            .bucket_links
            .get(bucket)
            .ok_or_else(|| anyhow!("allowlist bucket {bucket} has no predicted-link entries"))?;
        for predicted_link in links {
            sources.push(BucketPredictedSource {
                bucket: *bucket,
                predicted_link: *predicted_link,
                pages,
            });
        }
    }
    let (offsets, stats) =
        collect_bucket_sources_round_robin(alloc, cfg, &sources, cfg.predicted_cs_select)?;
    if offsets.len() < 2 {
        bail!(
            "predicted-CS PFN allowlist produced fewer than two offsets from {} matched pages",
            matched_page_count
        );
    }
    let bucket_pages = 1u64 << cfg.predicted_cs_pfn_bucket_shift;
    eprintln!(
        "predicted-cs pfn-allowlist selected {} offsets targeting CS{} from {} matched pages; buckets={} accepted_rows={} accepted_pfns={} total_rows={} bucket_pages={} bucket_mib={:.3}",
        offsets.len(),
        cfg.predicted_cs_link,
        matched_page_count,
        allowlist.buckets.len(),
        allowlist.accepted_rows,
        allowlist.accepted_pfns,
        allowlist.total_rows,
        bucket_pages,
        bucket_pages as f64 * PAGE_BYTES as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "predicted-cs pfn-allowlist {} used {} / {} bucket-link sources across {} / {} matched buckets; candidate_offsets={} quota_per_source={}",
        cfg.selected_order,
        stats.used_sources,
        stats.source_count,
        stats.used_buckets,
        matched_buckets.len(),
        stats.candidate_offsets,
        stats.quota_per_source
    );
    Ok(offsets)
}

fn collect_formula_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<usize>> {
    let pages = collect_present_page_pfns(alloc, cfg)?;
    let mut matched_sources = BTreeMap::<(u64, u32), Vec<PagePfn>>::new();
    for page in pages {
        let (bucket, predicted_link) = formula_bucket_and_predicted_link(
            page.pfn,
            cfg.predicted_cs_link,
            cfg.predicted_cs_pfn_formula,
        )
        .with_context(|| {
            format!(
                "PFN formula {} cannot target CS{}",
                cfg.predicted_cs_pfn_formula, cfg.predicted_cs_link
            )
        })?;
        matched_sources
            .entry((bucket, predicted_link))
            .or_default()
            .push(page);
    }
    let matched_page_count = matched_sources
        .values()
        .map(|pages| pages.len())
        .sum::<usize>();
    if matched_page_count == 0 {
        bail!(
            "predicted-CS PFN formula {} matched no pages from current allocation",
            cfg.predicted_cs_pfn_formula
        );
    }

    let mut sources = Vec::with_capacity(matched_sources.len());
    for ((bucket, predicted_link), pages) in &matched_sources {
        sources.push(BucketPredictedSource {
            bucket: *bucket,
            predicted_link: *predicted_link,
            pages,
        });
    }
    let (offsets, stats) =
        collect_bucket_sources_round_robin(alloc, cfg, &sources, cfg.predicted_cs_select)?;
    if offsets.len() < 2 {
        bail!(
            "predicted-CS PFN formula {} produced fewer than two offsets from {} matched pages",
            cfg.predicted_cs_pfn_formula,
            matched_page_count
        );
    }
    let bucket_pages = 1u64 << cfg.predicted_cs_pfn_bucket_shift;
    eprintln!(
        "predicted-cs pfn-formula {} selected {} offsets targeting CS{} from {} matched pages; bucket_sources={} bucket_pages={} bucket_mib={:.3}",
        cfg.predicted_cs_pfn_formula,
        offsets.len(),
        cfg.predicted_cs_link,
        matched_page_count,
        matched_sources.len(),
        bucket_pages,
        bucket_pages as f64 * PAGE_BYTES as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "predicted-cs pfn-formula {} used {} / {} bucket-link sources across {} / {} matched buckets; candidate_offsets={} quota_per_source={}",
        cfg.selected_order,
        stats.used_sources,
        stats.source_count,
        stats.used_buckets,
        matched_sources.len(),
        stats.candidate_offsets,
        stats.quota_per_source
    );
    Ok(offsets)
}

fn collect_bucket_sources_round_robin(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    sources: &[BucketPredictedSource<'_>],
    target: usize,
) -> Result<(Vec<usize>, BucketRoundRobinStats)> {
    if sources.is_empty() {
        bail!("PFN bucket allowlist has no bucket-link sources for the current allocation");
    }
    let mut quota = target.div_ceil(sources.len()).max(2);
    let mut best_offsets = Vec::new();
    let mut best_stats = BucketRoundRobinStats::default();
    let mut last_len = 0usize;

    loop {
        let mut source_offsets = Vec::with_capacity(sources.len());
        let mut candidate_offsets = 0usize;
        for source in sources {
            let offsets = predicted_offsets_from_page_pfns(
                alloc,
                cfg,
                source.pages,
                source.predicted_link,
                quota,
            );
            candidate_offsets = candidate_offsets.saturating_add(offsets.len());
            source_offsets.push(offsets);
        }

        let (offsets, mut stats) =
            order_bucket_offsets(cfg.selected_order, sources, &source_offsets, target);
        stats.candidate_offsets = candidate_offsets;
        stats.quota_per_source = quota;
        stats.source_count = sources.len();

        let complete = offsets.len() >= target;
        let saturated = offsets.len() == last_len || quota >= target;
        if offsets.len() > best_offsets.len() {
            best_offsets = offsets;
            best_stats = stats;
        }
        if complete || saturated {
            break;
        }

        last_len = best_offsets.len();
        quota = quota.saturating_mul(2).min(target);
    }

    Ok((best_offsets, best_stats))
}

fn order_bucket_offsets(
    order: SelectedOrder,
    sources: &[BucketPredictedSource<'_>],
    source_offsets: &[Vec<usize>],
    target: usize,
) -> (Vec<usize>, BucketRoundRobinStats) {
    match order {
        SelectedOrder::RoundRobin => round_robin_bucket_offsets(sources, source_offsets, target),
        SelectedOrder::VirtualSorted => {
            virtual_sorted_bucket_offsets(sources, source_offsets, target)
        }
        SelectedOrder::BucketGrouped => bucket_grouped_offsets(sources, source_offsets, target),
    }
}

fn round_robin_bucket_offsets(
    sources: &[BucketPredictedSource<'_>],
    source_offsets: &[Vec<usize>],
    target: usize,
) -> (Vec<usize>, BucketRoundRobinStats) {
    let max_len = source_offsets
        .iter()
        .map(|offsets| offsets.len())
        .max()
        .unwrap_or(0);
    let mut offsets = Vec::with_capacity(target);
    let mut used_source = vec![false; sources.len()];
    let mut used_buckets = BTreeSet::new();

    'passes: for idx in 0..max_len {
        for (source_idx, source) in sources.iter().enumerate() {
            let Some(offset) = source_offsets
                .get(source_idx)
                .and_then(|offsets| offsets.get(idx))
                .copied()
            else {
                continue;
            };
            offsets.push(offset);
            used_source[source_idx] = true;
            used_buckets.insert(source.bucket);
            if offsets.len() >= target {
                break 'passes;
            }
        }
    }

    let stats = BucketRoundRobinStats {
        used_sources: used_source.into_iter().filter(|used| *used).count(),
        used_buckets: used_buckets.len(),
        ..BucketRoundRobinStats::default()
    };
    (offsets, stats)
}

fn virtual_sorted_bucket_offsets(
    sources: &[BucketPredictedSource<'_>],
    source_offsets: &[Vec<usize>],
    target: usize,
) -> (Vec<usize>, BucketRoundRobinStats) {
    let mut candidates = Vec::new();
    for (source_idx, offsets) in source_offsets.iter().enumerate() {
        candidates.extend(offsets.iter().copied().map(|offset| (offset, source_idx)));
    }
    candidates.sort_unstable_by_key(|(offset, source_idx)| (*offset, *source_idx));
    select_candidate_offsets(sources, candidates.into_iter(), target)
}

fn bucket_grouped_offsets(
    sources: &[BucketPredictedSource<'_>],
    source_offsets: &[Vec<usize>],
    target: usize,
) -> (Vec<usize>, BucketRoundRobinStats) {
    let candidates = source_offsets
        .iter()
        .enumerate()
        .flat_map(|(source_idx, offsets)| {
            offsets
                .iter()
                .copied()
                .map(move |offset| (offset, source_idx))
        });
    select_candidate_offsets(sources, candidates, target)
}

fn select_candidate_offsets<I>(
    sources: &[BucketPredictedSource<'_>],
    candidates: I,
    target: usize,
) -> (Vec<usize>, BucketRoundRobinStats)
where
    I: IntoIterator<Item = (usize, usize)>,
{
    let mut offsets = Vec::with_capacity(target);
    let mut used_source = vec![false; sources.len()];
    let mut used_buckets = BTreeSet::new();
    for (offset, source_idx) in candidates {
        let Some(source) = sources.get(source_idx) else {
            continue;
        };
        offsets.push(offset);
        used_source[source_idx] = true;
        used_buckets.insert(source.bucket);
        if offsets.len() >= target {
            break;
        }
    }
    let stats = BucketRoundRobinStats {
        used_sources: used_source.into_iter().filter(|used| *used).count(),
        used_buckets: used_buckets.len(),
        ..BucketRoundRobinStats::default()
    };
    (offsets, stats)
}

fn write_predicted_cs_pfn_bucket_score(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    path: &PathBuf,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let pages = collect_present_page_pfns(alloc, cfg)?;
    let mut buckets = BTreeMap::<u64, Vec<PagePfn>>::new();
    for page in pages {
        buckets
            .entry(page.pfn >> cfg.predicted_cs_pfn_bucket_shift)
            .or_default()
            .push(page);
    }
    let seed_bucket_count = if let Some(seed_path) = cfg.predicted_cs_pfn_bucket_score_seed.as_ref()
    {
        let seed = load_pfn_bucket_allowlist(seed_path, cfg, cfg.predicted_cs_link)?;
        buckets.retain(|bucket, _| seed.buckets.contains(bucket));
        Some(seed.buckets.len())
    } else {
        None
    };

    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut columns = vec![
        "bucket_shift",
        "bucket",
        "pfn",
        "first_pfn",
        "last_pfn",
        "page_count",
        "sampled_offsets",
        "target_link",
        "top_link",
        "top_link_share",
        "target_link_share",
        "target_mib_s",
        "total_mib_s",
        "max_mean_skew",
        "gini_skew",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    for link in &cfg.links {
        columns.push(format!("cs{link}_read_mib_s"));
        columns.push(format!("cs{link}_write_mib_s"));
    }
    writeln!(writer, "{}", columns.join("\t"))?;

    let score_links = if cfg.predicted_cs_pfn_bucket_score_all_links {
        OBSERVED_EPYC_CS_LINKS.to_vec()
    } else {
        vec![cfg.predicted_cs_link]
    };
    let mut scored = 0usize;
    let mut buckets_scored = 0usize;
    for (bucket, pages) in buckets {
        let mut bucket_rows = 0usize;
        let first_pfn = pages.iter().map(|page| page.pfn).min().unwrap_or(0);
        let last_pfn = pages.iter().map(|page| page.pfn).max().unwrap_or(0);
        for predicted_link in &score_links {
            let offsets = predicted_offsets_from_page_pfns(
                alloc,
                cfg,
                &pages,
                *predicted_link,
                cfg.predicted_cs_pfn_bucket_sample_offsets,
            );
            if offsets.len() < 2 {
                continue;
            }
            let links =
                measure_offset_group_links(alloc.ptr as usize, alloc.len, cfg, offsets.clone())?;
            let summary = summarize_cs(&links, cfg.active_link_floor_mib_s);
            let total_mib_s = links
                .iter()
                .map(|link| link.read_mib_s + link.write_mib_s)
                .sum::<f64>();
            let target_mib_s = links
                .iter()
                .find(|link| link.link_id == *predicted_link)
                .map(|link| link.read_mib_s + link.write_mib_s)
                .unwrap_or(0.0);
            let target_link_share = ratio_or_zero(target_mib_s, total_mib_s);
            let mut row = vec![
                cfg.predicted_cs_pfn_bucket_shift.to_string(),
                bucket.to_string(),
                (bucket << cfg.predicted_cs_pfn_bucket_shift).to_string(),
                first_pfn.to_string(),
                last_pfn.to_string(),
                pages.len().to_string(),
                offsets.len().to_string(),
                predicted_link.to_string(),
                summary.top_link.to_string(),
                format!("{:.6}", summary.top_link_share),
                format!("{:.6}", target_link_share),
                format!("{:.3}", target_mib_s),
                format!("{:.3}", total_mib_s),
                format!("{:.6}", summary.max_mean_skew),
                format!("{:.6}", summary.gini_skew),
            ];
            for link_id in &cfg.links {
                let link = links.iter().find(|link| link.link_id == *link_id);
                row.push(format!(
                    "{:.3}",
                    link.map(|link| link.read_mib_s).unwrap_or(0.0)
                ));
                row.push(format!(
                    "{:.3}",
                    link.map(|link| link.write_mib_s).unwrap_or(0.0)
                ));
            }
            writeln!(writer, "{}", row.join("\t"))?;
            scored = scored.saturating_add(1);
            bucket_rows = bucket_rows.saturating_add(1);
        }
        if bucket_rows > 0 {
            buckets_scored = buckets_scored.saturating_add(1);
        }
    }
    eprintln!(
        "predicted-cs pfn-bucket-score wrote {} rows to {}; buckets_scored={} seed_buckets={}",
        scored,
        path.display(),
        buckets_scored,
        seed_bucket_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    Ok(())
}

struct PredictedCollectResult {
    offsets: Vec<usize>,
    pages_seen: usize,
    saw_present: bool,
    saw_nonzero_pfn: bool,
}

#[derive(Clone, Copy, Debug)]
struct PagePfn {
    page_idx: usize,
    pfn: u64,
}

fn collect_predicted_cs_offsets_from_pages(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    pagemap: &File,
    predicted_link: u32,
    first_page: usize,
    last_page: usize,
    limit: usize,
) -> Result<PredictedCollectResult> {
    let (region_start, region_end, _, _) = configured_page_range(alloc, cfg);
    let mut offsets = Vec::new();
    let mut pages_seen = 0usize;
    let mut saw_present = false;
    let mut saw_nonzero_pfn = false;
    let mut seen_offsets = BTreeSet::new();

    'pages: for page_idx in first_page..=last_page {
        pages_seen = pages_seen.saturating_add(1);
        let virt = alloc.ptr as usize + page_idx * PAGE_BYTES;
        let entry = read_pagemap_entry(pagemap, virt)?;
        if !pagemap_present(entry) {
            continue;
        }
        saw_present = true;
        let pfn = pagemap_pfn(entry);
        saw_nonzero_pfn |= pfn != 0;
        for line_offset in &cfg.predicted_cs_line_offsets {
            let offset = page_idx
                .saturating_mul(PAGE_BYTES)
                .saturating_add(*line_offset);
            if offset < region_start || offset >= region_end {
                continue;
            }
            if observed_epyc_line_hash_cs(pfn, *line_offset) != Some(predicted_link) {
                continue;
            }
            let aligned = cache_line_offset(offset);
            if seen_offsets.insert(aligned) {
                offsets.push(aligned);
                if offsets.len() >= limit {
                    break 'pages;
                }
            }
        }
    }
    Ok(PredictedCollectResult {
        offsets,
        pages_seen,
        saw_present,
        saw_nonzero_pfn,
    })
}

fn collect_present_page_pfns(alloc: &MmapAllocation, cfg: &RunConfig) -> Result<Vec<PagePfn>> {
    let pagemap = File::open("/proc/self/pagemap").context("failed to open /proc/self/pagemap")?;
    let (_, _, first_page, last_page) = configured_page_range(alloc, cfg);
    let max_pages = cfg
        .predicted_cs_max_pages
        .max(1)
        .min(last_page.saturating_sub(first_page).saturating_add(1));
    let scan_last_page = if cfg.predicted_cs_max_pages > 0 {
        first_page.saturating_add(max_pages.saturating_sub(1))
    } else {
        last_page
    };
    let mut pages = Vec::new();
    let mut saw_present = false;
    let mut saw_nonzero_pfn = false;
    for page_idx in first_page..=scan_last_page {
        let virt = alloc.ptr as usize + page_idx * PAGE_BYTES;
        let entry = read_pagemap_entry(&pagemap, virt)?;
        if !pagemap_present(entry) {
            continue;
        }
        saw_present = true;
        let pfn = pagemap_pfn(entry);
        saw_nonzero_pfn |= pfn != 0;
        if pfn != 0 {
            pages.push(PagePfn { page_idx, pfn });
        }
    }
    if !saw_present {
        bail!("pagemap had no present pages for the selected allocation");
    }
    if !saw_nonzero_pfn {
        bail!("pagemap PFNs are hidden; run with sufficient privilege");
    }
    Ok(pages)
}

fn predicted_offsets_from_page_pfns(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    pages: &[PagePfn],
    predicted_link: u32,
    limit: usize,
) -> Vec<usize> {
    let (region_start, region_end, _, _) = configured_page_range(alloc, cfg);
    let mut offsets = Vec::new();
    'pages: for page in pages {
        for line_offset in &cfg.predicted_cs_line_offsets {
            let offset = page
                .page_idx
                .saturating_mul(PAGE_BYTES)
                .saturating_add(*line_offset);
            if offset < region_start || offset >= region_end {
                continue;
            }
            if observed_epyc_line_hash_cs(page.pfn, *line_offset) != Some(predicted_link) {
                continue;
            }
            let aligned = cache_line_offset(offset);
            offsets.push(aligned);
            if offsets.len() >= limit {
                break 'pages;
            }
        }
    }
    offsets
}

fn validate_predicted_collect_result(result: &PredictedCollectResult, label: &str) -> Result<()> {
    if !result.saw_present {
        bail!("{label}: pagemap had no present pages for the selected allocation");
    }
    if !result.saw_nonzero_pfn {
        bail!("{label}: pagemap PFNs are hidden; run with sufficient privilege");
    }
    Ok(())
}

fn collect_autotuned_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<usize>> {
    let mut best_offsets = Vec::new();
    let mut best_predicted_link = OBSERVED_EPYC_CS_LINKS[0];
    let mut best_summary = CsSummary::default();
    let mut best_score = f64::NEG_INFINITY;

    for predicted_link in OBSERVED_EPYC_CS_LINKS {
        let offsets = collect_predicted_cs_offsets(alloc, cfg, predicted_link)
            .with_context(|| format!("failed to collect predicted CS color {predicted_link}"))?;
        let links =
            measure_offset_group_links(alloc.ptr as usize, alloc.len, cfg, offsets.clone())?;
        let summary = summarize_cs(&links, cfg.active_link_floor_mib_s);
        let group = CalibrationGroup {
            candidate_idx: 0,
            offsets: offsets.clone(),
            links,
            summary: summary.clone(),
        };
        let score = calibration_group_score(&group, cfg.predicted_cs_link);
        if score > best_score {
            best_score = score;
            best_offsets = offsets;
            best_predicted_link = predicted_link;
            best_summary = summary;
        }
    }

    if best_offsets.len() < 2 {
        bail!("predicted-CS autotune selected fewer than two offsets");
    }
    eprintln!(
        "predicted-cs autotune selected hash color CS{} for target CS{}; measured_top={} share={:.3} gini={:.3}",
        best_predicted_link,
        cfg.predicted_cs_link,
        best_summary.top_link,
        best_summary.top_link_share,
        best_summary.gini_skew
    );
    Ok(best_offsets)
}

fn collect_segmented_autotuned_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<usize>> {
    let pagemap = File::open("/proc/self/pagemap").context("failed to open /proc/self/pagemap")?;
    let (_, _, first_page, last_page) = configured_page_range(alloc, cfg);
    let segment_pages = cfg.predicted_cs_segment_pages.max(1);
    let max_pages = cfg
        .predicted_cs_max_pages
        .max(1)
        .min(last_page.saturating_sub(first_page).saturating_add(1));
    let scan_last_page = if cfg.predicted_cs_max_pages > 0 {
        first_page.saturating_add(max_pages.saturating_sub(1))
    } else {
        last_page
    };

    let mut selected = Vec::with_capacity(cfg.predicted_cs_select);
    let mut segment_start = first_page;
    let mut segment_idx = 0usize;
    while segment_start <= scan_last_page && selected.len() < cfg.predicted_cs_select {
        let segment_end = segment_start
            .saturating_add(segment_pages.saturating_sub(1))
            .min(scan_last_page);
        let remaining = cfg.predicted_cs_select - selected.len();
        let mut best_offsets = Vec::new();
        let mut best_predicted_link = OBSERVED_EPYC_CS_LINKS[0];
        let mut best_summary = CsSummary::default();
        let mut best_score = f64::NEG_INFINITY;
        let mut any_present = false;
        let mut any_nonzero_pfn = false;

        for predicted_link in OBSERVED_EPYC_CS_LINKS {
            let collected = collect_predicted_cs_offsets_from_pages(
                alloc,
                cfg,
                &pagemap,
                predicted_link,
                segment_start,
                segment_end,
                remaining.max(2),
            )?;
            any_present |= collected.saw_present;
            any_nonzero_pfn |= collected.saw_nonzero_pfn;
            if collected.offsets.len() < 2 {
                continue;
            }
            let links = measure_offset_group_links(
                alloc.ptr as usize,
                alloc.len,
                cfg,
                collected.offsets.clone(),
            )?;
            let summary = summarize_cs(&links, cfg.active_link_floor_mib_s);
            let group = CalibrationGroup {
                candidate_idx: segment_idx,
                offsets: collected.offsets.clone(),
                links,
                summary: summary.clone(),
            };
            let score = calibration_group_score(&group, cfg.predicted_cs_link);
            if score > best_score {
                best_score = score;
                best_offsets = collected.offsets;
                best_predicted_link = predicted_link;
                best_summary = summary;
            }
        }

        let segment_pages_seen = segment_end.saturating_sub(segment_start).saturating_add(1);
        if !any_present || !any_nonzero_pfn {
            bail!(
                "predicted-CS segmented autotune failed in segment {segment_idx}: pagemap missing PFNs"
            );
        }
        if best_offsets.len() >= 2 {
            let take = best_offsets.len().min(remaining);
            selected.extend(best_offsets.into_iter().take(take));
            eprintln!(
                "predicted-cs segment {} pages={} selected {} offsets via hash color CS{} for target CS{}; measured_top={} share={:.3}",
                segment_idx,
                segment_pages_seen,
                take,
                best_predicted_link,
                cfg.predicted_cs_link,
                best_summary.top_link,
                best_summary.top_link_share
            );
        } else {
            eprintln!(
                "predicted-cs segment {} pages={} selected 0 offsets for target CS{}",
                segment_idx, segment_pages_seen, cfg.predicted_cs_link
            );
        }

        segment_idx = segment_idx.saturating_add(1);
        segment_start = segment_end.saturating_add(1);
    }

    if selected.len() < 2 {
        bail!("predicted-CS segmented autotune selected fewer than two offsets");
    }
    eprintln!(
        "predicted-cs segmented autotune selected {} offsets across {} segments for target CS{}",
        selected.len(),
        segment_idx,
        cfg.predicted_cs_link
    );
    Ok(selected)
}

fn collect_phys_segmented_autotuned_predicted_cs_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<usize>> {
    let pages = collect_present_page_pfns(alloc, cfg)?;
    let bucket_pages = cfg.predicted_cs_phys_segment_pages.max(1) as u64;
    let mut buckets = BTreeMap::<u64, Vec<PagePfn>>::new();
    for page in pages {
        buckets
            .entry(page.pfn / bucket_pages)
            .or_default()
            .push(page);
    }

    let mut selected = Vec::with_capacity(cfg.predicted_cs_select);
    let mut bucket_count = 0usize;
    for (bucket, pages) in buckets {
        if selected.len() >= cfg.predicted_cs_select {
            break;
        }
        let remaining = cfg.predicted_cs_select - selected.len();
        let mut best_offsets = Vec::new();
        let mut best_predicted_link = OBSERVED_EPYC_CS_LINKS[0];
        let mut best_summary = CsSummary::default();
        let mut best_score = f64::NEG_INFINITY;

        for predicted_link in OBSERVED_EPYC_CS_LINKS {
            let offsets = predicted_offsets_from_page_pfns(
                alloc,
                cfg,
                &pages,
                predicted_link,
                remaining.max(2),
            );
            if offsets.len() < 2 {
                continue;
            }
            let links =
                measure_offset_group_links(alloc.ptr as usize, alloc.len, cfg, offsets.clone())?;
            let summary = summarize_cs(&links, cfg.active_link_floor_mib_s);
            let group = CalibrationGroup {
                candidate_idx: bucket_count,
                offsets: offsets.clone(),
                links,
                summary: summary.clone(),
            };
            let score = calibration_group_score(&group, cfg.predicted_cs_link);
            if score > best_score {
                best_score = score;
                best_offsets = offsets;
                best_predicted_link = predicted_link;
                best_summary = summary;
            }
        }

        if best_offsets.len() >= 2 {
            let take = best_offsets.len().min(remaining);
            selected.extend(best_offsets.into_iter().take(take));
            eprintln!(
                "predicted-cs phys-bucket {} pages={} selected {} offsets via hash color CS{} for target CS{}; measured_top={} share={:.3}",
                bucket,
                pages.len(),
                take,
                best_predicted_link,
                cfg.predicted_cs_link,
                best_summary.top_link,
                best_summary.top_link_share
            );
        }
        bucket_count = bucket_count.saturating_add(1);
    }

    if selected.len() < 2 {
        bail!("predicted-CS physical segmented autotune selected fewer than two offsets");
    }
    eprintln!(
        "predicted-cs physical segmented autotune selected {} offsets across {} PFN buckets for target CS{}",
        selected.len(),
        bucket_count,
        cfg.predicted_cs_link
    );
    Ok(selected)
}

fn observed_epyc_line_hash_cs(pfn: u64, page_offset: usize) -> Option<u32> {
    if page_offset >= PAGE_BYTES || page_offset % CACHE_LINE_BYTES != 0 {
        return None;
    }
    let line_idx = (page_offset / CACHE_LINE_BYTES) as u64;
    let phys_line = (pfn << 6) | line_idx;
    let b0 = 1
        ^ bit(phys_line, 6)
        ^ bit(phys_line, 11)
        ^ bit(phys_line, 14)
        ^ bit(phys_line, 20)
        ^ bit(phys_line, 25);
    let b1 = bit(phys_line, 7) ^ bit(phys_line, 13) ^ bit(phys_line, 18) ^ bit(phys_line, 23);
    let b2 = 1
        ^ bit(phys_line, 2)
        ^ bit(phys_line, 8)
        ^ bit(phys_line, 10)
        ^ bit(phys_line, 22)
        ^ bit(phys_line, 30);
    let code = b0 | (b1 << 1) | (b2 << 2);
    Some(match code {
        0b000 => 0,
        0b100 => 3,
        0b001 => 4,
        0b101 => 5,
        0b010 => 6,
        0b110 => 9,
        0b011 => 10,
        0b111 => 11,
        _ => unreachable!(),
    })
}

fn predicted_cs_link_to_code(link: u32) -> Option<u8> {
    Some(match link {
        0 => 0b000,
        3 => 0b100,
        4 => 0b001,
        5 => 0b101,
        6 => 0b010,
        9 => 0b110,
        10 => 0b011,
        11 => 0b111,
        _ => return None,
    })
}

fn observed_epyc_link_index(link: u32) -> Option<u8> {
    OBSERVED_EPYC_CS_LINKS
        .iter()
        .position(|candidate| *candidate == link)
        .and_then(|idx| u8::try_from(idx).ok())
}

fn observed_epyc_node0_v1_bucket_class(bucket: u64) -> u8 {
    let mut class = OBSERVED_EPYC_NODE0_V1_NIBBLE_LUT[((bucket >> 8) & 0xf) as usize];
    for (bucket_bit, class_xor) in [
        (0, 2),
        (1, 1),
        (2, 2),
        (3, 4),
        (4, 4),
        (6, 2),
        (12, 4),
        (16, 1),
    ] {
        if bit(bucket, bucket_bit) != 0 {
            class ^= class_xor;
        }
    }
    class
}

fn observed_epyc_node0_v1_predicted_link_for_target(pfn: u64, target_link: u32) -> Option<u32> {
    let target_index = observed_epyc_link_index(target_link)?;
    let bucket = pfn >> OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT;
    let predicted_index = target_index ^ observed_epyc_node0_v1_bucket_class(bucket);
    OBSERVED_EPYC_CS_LINKS
        .get(predicted_index as usize)
        .copied()
}

fn formula_bucket_and_predicted_link(
    pfn: u64,
    target_link: u32,
    formula: PredictedCsPfnFormula,
) -> Option<(u64, u32)> {
    match formula {
        PredictedCsPfnFormula::None => None,
        PredictedCsPfnFormula::ObservedEpycNode0V1 => {
            let bucket = pfn >> OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT;
            observed_epyc_node0_v1_predicted_link_for_target(pfn, target_link)
                .map(|predicted_link| (bucket, predicted_link))
        }
    }
}

fn bit(value: u64, bit: u8) -> u8 {
    ((value >> bit) & 1) as u8
}

fn collect_generated_candidate_offsets(
    cfg: &RunConfig,
    alloc_len: usize,
    limit: usize,
) -> Vec<usize> {
    let target = limit.max(1);
    let mut seen_pages = BTreeSet::new();
    let mut offsets = Vec::with_capacity(target);
    let max_attempts = target.saturating_mul(128).max(4096);
    for logical in 0..max_attempts as u64 {
        let offset = address_offset(cfg, logical, alloc_len);
        let page = offset / PAGE_BYTES;
        if seen_pages.insert(page) {
            offsets.push(offset);
            if offsets.len() >= target {
                break;
            }
        }
    }
    offsets
}

#[derive(Clone, Debug)]
struct CalibratedOffsets {
    offsets: Vec<usize>,
    target_link: u32,
    selected_groups: Vec<SelectedCalibrationGroup>,
}

#[derive(Clone, Debug)]
struct SelectedCalibrationGroup {
    group: CalibrationGroup,
    selected_offsets: Vec<usize>,
}

#[derive(Clone, Debug)]
struct CalibrationGroup {
    candidate_idx: usize,
    offsets: Vec<usize>,
    links: Vec<CsLinkBw>,
    summary: CsSummary,
}

fn calibrate_hot_cs_offsets(
    base_addr: usize,
    alloc_len: usize,
    cfg: &RunConfig,
    candidates: Vec<usize>,
) -> Result<CalibratedOffsets> {
    if candidates.len() < 2 {
        bail!("hot-CS calibration needs at least two candidate offsets");
    }
    let candidates = match cfg.hot_cs_score_unit {
        HotCsScoreUnit::Group | HotCsScoreUnit::Page => candidates,
        HotCsScoreUnit::Line => expand_line_candidate_offsets(&candidates, cfg, alloc_len)?,
    };
    if candidates.len() < 2 {
        bail!("hot-CS line expansion produced fewer than two candidate offsets");
    }
    let group_pages = match cfg.hot_cs_score_unit {
        HotCsScoreUnit::Group => cfg.hot_cs_group_pages.max(1),
        HotCsScoreUnit::Page | HotCsScoreUnit::Line => 1,
    };
    let mut groups = Vec::new();
    for (candidate_idx, chunk) in candidates.chunks(group_pages).enumerate() {
        let offsets = chunk.to_vec();
        let links = measure_offset_group_links(base_addr, alloc_len, cfg, offsets.clone())?;
        let summary = summarize_cs(&links, cfg.active_link_floor_mib_s);
        groups.push(CalibrationGroup {
            candidate_idx,
            offsets,
            links,
            summary,
        });
    }
    if groups.is_empty() {
        bail!("hot-CS calibration produced no candidate groups");
    }
    let target_link = cfg.hot_cs_link.unwrap_or_else(|| {
        groups
            .iter()
            .max_by(|left, right| {
                left.summary
                    .top_link_share
                    .partial_cmp(&right.summary.top_link_share)
                    .unwrap_or(CmpOrdering::Equal)
            })
            .and_then(|group| {
                (group.summary.top_link >= 0).then_some(group.summary.top_link as u32)
            })
            .unwrap_or(cfg.links[0])
    });
    if let Some(path) = cfg.hot_cs_score_out.as_ref() {
        write_hot_cs_score_dump(path, base_addr, &groups, target_link, cfg)?;
    }
    groups.sort_by(|left, right| {
        calibration_group_score(right, target_link)
            .partial_cmp(&calibration_group_score(left, target_link))
            .unwrap_or(CmpOrdering::Equal)
    });
    let mut selected = Vec::new();
    let mut selected_groups = Vec::new();
    for group in &groups {
        let remaining = cfg.hot_cs_select.saturating_sub(selected.len());
        if remaining == 0 {
            break;
        }
        let group_offsets = group
            .offsets
            .iter()
            .copied()
            .take(remaining)
            .collect::<Vec<_>>();
        selected.extend(group_offsets.iter().copied());
        selected_groups.push(SelectedCalibrationGroup {
            group: group.clone(),
            selected_offsets: group_offsets,
        });
        if selected.len() >= cfg.hot_cs_select {
            selected.truncate(cfg.hot_cs_select);
            break;
        }
    }
    if selected.len() < 2 {
        bail!("hot-CS calibration selected fewer than two offsets");
    }
    let best = &groups[0];
    eprintln!(
        "hot-cs selected {} offsets targeting CS{} unit={}; best_group_top={} share={:.3} gini={:.3}",
        selected.len(),
        target_link,
        cfg.hot_cs_score_unit,
        best.summary.top_link,
        best.summary.top_link_share,
        best.summary.gini_skew
    );
    Ok(CalibratedOffsets {
        offsets: selected,
        target_link,
        selected_groups,
    })
}

fn expand_line_candidate_offsets(
    candidates: &[usize],
    cfg: &RunConfig,
    alloc_len: usize,
) -> Result<Vec<usize>> {
    let mut seen = BTreeSet::new();
    let mut expanded = Vec::new();
    for candidate in candidates {
        let page_base = candidate / PAGE_BYTES * PAGE_BYTES;
        for line_offset in &cfg.hot_cs_line_offsets {
            let Some(offset) = page_base.checked_add(*line_offset) else {
                continue;
            };
            if offset >= alloc_len || !offset_in_config_region(cfg, offset, alloc_len) {
                continue;
            }
            let aligned = cache_line_offset(offset);
            if seen.insert(aligned) {
                expanded.push(aligned);
            }
        }
    }
    Ok(expanded)
}

fn offset_in_config_region(cfg: &RunConfig, offset: usize, alloc_len: usize) -> bool {
    let start = cfg.base_offset_bytes.min(alloc_len.saturating_sub(1));
    let end = start.saturating_add(cfg.size_bytes).min(alloc_len);
    offset >= start && offset < end
}

fn calibration_group_score(group: &CalibrationGroup, target_link: u32) -> f64 {
    let target_bw = group
        .links
        .iter()
        .find(|link| link.link_id == target_link)
        .map(|link| link.read_mib_s + link.write_mib_s)
        .unwrap_or(0.0);
    let total = group
        .links
        .iter()
        .map(|link| link.read_mib_s + link.write_mib_s)
        .sum::<f64>();
    let share = if total > 0.0 { target_bw / total } else { 0.0 };
    share * 1_000_000.0 + target_bw
}

fn write_hot_cs_score_dump(
    path: &PathBuf,
    base_addr: usize,
    groups: &[CalibrationGroup],
    target_link: u32,
    cfg: &RunConfig,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let pagemap = File::open("/proc/self/pagemap").ok();

    let mut columns = vec![
        "score_unit",
        "candidate_idx",
        "offset_count",
        "first_offset",
        "page_index",
        "page_offset",
        "pfn",
        "pfn_mod16",
        "pfn_mod64",
        "pfn_mod256",
        "target_link",
        "top_link",
        "top_link_share",
        "target_link_share",
        "target_mib_s",
        "total_mib_s",
        "max_mean_skew",
        "gini_skew",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    for link in &cfg.links {
        columns.push(format!("cs{link}_read_mib_s"));
        columns.push(format!("cs{link}_write_mib_s"));
    }
    writeln!(writer, "{}", columns.join("\t"))?;

    for group in groups {
        let first_offset = group.offsets.first().copied().unwrap_or(0);
        let pfn = pagemap
            .as_ref()
            .and_then(|file| pfn_for_offset(file, base_addr, first_offset));
        let total_mib_s = group
            .links
            .iter()
            .map(|link| link.read_mib_s + link.write_mib_s)
            .sum::<f64>();
        let target_mib_s = group
            .links
            .iter()
            .find(|link| link.link_id == target_link)
            .map(|link| link.read_mib_s + link.write_mib_s)
            .unwrap_or(0.0);
        let target_link_share = ratio_or_zero(target_mib_s, total_mib_s);
        let pfn_value = pfn
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-1".to_string());
        let pfn_mod = |modulo: u64| {
            pfn.map(|value| (value % modulo).to_string())
                .unwrap_or_else(|| "-1".to_string())
        };

        let mut row = vec![
            cfg.hot_cs_score_unit.to_string(),
            group.candidate_idx.to_string(),
            group.offsets.len().to_string(),
            first_offset.to_string(),
            (first_offset / PAGE_BYTES).to_string(),
            (first_offset % PAGE_BYTES).to_string(),
            pfn_value,
            pfn_mod(16),
            pfn_mod(64),
            pfn_mod(256),
            target_link.to_string(),
            group.summary.top_link.to_string(),
            format!("{:.6}", group.summary.top_link_share),
            format!("{:.6}", target_link_share),
            format!("{:.3}", target_mib_s),
            format!("{:.3}", total_mib_s),
            format!("{:.6}", group.summary.max_mean_skew),
            format!("{:.6}", group.summary.gini_skew),
        ];
        for link_id in &cfg.links {
            let link = group.links.iter().find(|link| link.link_id == *link_id);
            row.push(format!(
                "{:.3}",
                link.map(|link| link.read_mib_s).unwrap_or(0.0)
            ));
            row.push(format!(
                "{:.3}",
                link.map(|link| link.write_mib_s).unwrap_or(0.0)
            ));
        }
        writeln!(writer, "{}", row.join("\t"))?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct VerifiedLineEntry {
    offset: Option<usize>,
    pfn: Option<u64>,
    page_offset: usize,
}

fn write_verified_line_catalog(
    path: &PathBuf,
    base_addr: usize,
    calibrated: &CalibratedOffsets,
    cfg: &RunConfig,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let pagemap = File::open("/proc/self/pagemap").ok();

    let mut columns = vec![
        "catalog_version",
        "selected_idx",
        "score_unit",
        "source_candidate_idx",
        "source_offset_count",
        "offset",
        "page_index",
        "page_offset",
        "pfn",
        "pfn_mod16",
        "pfn_mod64",
        "pfn_mod256",
        "target_link",
        "top_link",
        "top_link_share",
        "target_link_share",
        "target_mib_s",
        "total_mib_s",
        "max_mean_skew",
        "gini_skew",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    for link in &cfg.links {
        columns.push(format!("cs{link}_read_mib_s"));
        columns.push(format!("cs{link}_write_mib_s"));
    }
    writeln!(writer, "{}", columns.join("\t"))?;

    let mut selected_idx = 0usize;
    for selected_group in &calibrated.selected_groups {
        let group = &selected_group.group;
        let total_mib_s = group
            .links
            .iter()
            .map(|link| link.read_mib_s + link.write_mib_s)
            .sum::<f64>();
        let target_mib_s = group
            .links
            .iter()
            .find(|link| link.link_id == calibrated.target_link)
            .map(|link| link.read_mib_s + link.write_mib_s)
            .unwrap_or(0.0);
        let target_link_share = ratio_or_zero(target_mib_s, total_mib_s);

        for offset in &selected_group.selected_offsets {
            let pfn = pagemap
                .as_ref()
                .and_then(|file| pfn_for_offset(file, base_addr, *offset));
            let pfn_value = pfn
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-1".to_string());
            let pfn_mod = |modulo: u64| {
                pfn.map(|value| (value % modulo).to_string())
                    .unwrap_or_else(|| "-1".to_string())
            };
            let mut row = vec![
                "1".to_string(),
                selected_idx.to_string(),
                cfg.hot_cs_score_unit.to_string(),
                group.candidate_idx.to_string(),
                group.offsets.len().to_string(),
                offset.to_string(),
                (offset / PAGE_BYTES).to_string(),
                (offset % PAGE_BYTES).to_string(),
                pfn_value,
                pfn_mod(16),
                pfn_mod(64),
                pfn_mod(256),
                calibrated.target_link.to_string(),
                group.summary.top_link.to_string(),
                format!("{:.6}", group.summary.top_link_share),
                format!("{:.6}", target_link_share),
                format!("{:.3}", target_mib_s),
                format!("{:.3}", total_mib_s),
                format!("{:.6}", group.summary.max_mean_skew),
                format!("{:.6}", group.summary.gini_skew),
            ];
            for link_id in &cfg.links {
                let link = group.links.iter().find(|link| link.link_id == *link_id);
                row.push(format!(
                    "{:.3}",
                    link.map(|link| link.read_mib_s).unwrap_or(0.0)
                ));
                row.push(format!(
                    "{:.3}",
                    link.map(|link| link.write_mib_s).unwrap_or(0.0)
                ));
            }
            writeln!(writer, "{}", row.join("\t"))?;
            selected_idx = selected_idx.saturating_add(1);
        }
    }
    eprintln!(
        "verified-lines wrote {} selected offsets to {}",
        selected_idx,
        path.display()
    );
    Ok(())
}

fn load_verified_line_offsets(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    path: &PathBuf,
    min_offsets: usize,
    match_override: Option<VerifiedLineMatch>,
) -> Result<Vec<usize>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let entries = parse_verified_line_entries(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let min_offsets = min_offsets.max(2);

    let selected_match = match_override.unwrap_or(cfg.verified_lines_match);
    let modes = match selected_match {
        VerifiedLineMatch::Auto => vec![
            VerifiedLineMatch::ExactPfn,
            VerifiedLineMatch::Color,
            VerifiedLineMatch::Offset,
        ],
        other => vec![other],
    };
    let mut last_error = None;
    for mode in modes {
        let matched = match mode {
            VerifiedLineMatch::Auto => unreachable!(),
            VerifiedLineMatch::ExactPfn => match_verified_lines_by_pfn(alloc, cfg, &entries, None),
            VerifiedLineMatch::Color => match_verified_lines_by_pfn(
                alloc,
                cfg,
                &entries,
                Some(cfg.verified_lines_color_mask),
            ),
            VerifiedLineMatch::Offset => Ok(match_verified_lines_by_offset(alloc, cfg, &entries)),
        };
        match matched {
            Ok(offsets) if offsets.len() >= min_offsets => {
                eprintln!(
                    "verified-lines loaded {} offsets from {} using {} match",
                    offsets.len(),
                    path.display(),
                    mode
                );
                return Ok(offsets);
            }
            Ok(offsets) => {
                if selected_match != VerifiedLineMatch::Auto {
                    bail!(
                        "verified-lines {} match selected {} offsets from {}, below required {}",
                        mode,
                        offsets.len(),
                        path.display(),
                        min_offsets
                    );
                }
                eprintln!(
                    "verified-lines {} match found {} offsets, below required {}; trying next match mode",
                    mode,
                    offsets.len(),
                    min_offsets
                );
            }
            Err(err) => {
                if selected_match != VerifiedLineMatch::Auto {
                    return Err(err);
                }
                eprintln!("verified-lines {} match failed: {err:#}", mode);
                last_error = Some(err);
            }
        }
    }
    if let Some(err) = last_error {
        Err(err).with_context(|| {
            format!(
                "verified-lines auto match could not select enough offsets from {}",
                path.display()
            )
        })
    } else {
        bail!(
            "verified-lines auto match selected fewer than {} offsets from {}",
            min_offsets,
            path.display()
        )
    }
}

fn parse_verified_line_entries(contents: &str) -> Result<Vec<VerifiedLineEntry>> {
    let data_lines = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let Some(first) = data_lines.first() else {
        bail!("empty verified-lines catalog");
    };

    let first_columns = split_tsv_line(first);
    let has_header = first_columns
        .iter()
        .any(|column| matches!(*column, "offset" | "first_offset" | "page_offset" | "pfn"));
    if !has_header {
        return data_lines
            .iter()
            .map(|line| {
                let offset = parse_usize_field(
                    split_tsv_line(line)
                        .first()
                        .copied()
                        .ok_or_else(|| anyhow!("missing offset field"))?,
                )?
                .ok_or_else(|| anyhow!("missing offset field"))?;
                Ok(VerifiedLineEntry {
                    offset: Some(cache_line_offset(offset)),
                    pfn: None,
                    page_offset: cache_line_offset(offset % PAGE_BYTES),
                })
            })
            .collect();
    }

    let header = first_columns;
    let offset_idx =
        column_index(&header, "offset").or_else(|| column_index(&header, "first_offset"));
    let page_offset_idx = column_index(&header, "page_offset");
    let pfn_idx = column_index(&header, "pfn");
    if offset_idx.is_none() && page_offset_idx.is_none() {
        bail!("verified-lines TSV needs an offset or page_offset column");
    }

    let mut entries = Vec::new();
    for (line_no, line) in data_lines.iter().enumerate().skip(1) {
        let columns = split_tsv_line(line);
        let offset = offset_idx
            .and_then(|idx| columns.get(idx))
            .map(|value| parse_usize_field(value))
            .transpose()
            .with_context(|| format!("invalid offset at data line {}", line_no + 1))?
            .flatten()
            .map(cache_line_offset);
        let page_offset = page_offset_idx
            .and_then(|idx| columns.get(idx))
            .map(|value| parse_usize_field(value))
            .transpose()
            .with_context(|| format!("invalid page_offset at data line {}", line_no + 1))?
            .flatten()
            .or_else(|| offset.map(|offset| offset % PAGE_BYTES))
            .ok_or_else(|| anyhow!("missing page_offset at data line {}", line_no + 1))?;
        let page_offset = cache_line_offset(page_offset);
        if page_offset >= PAGE_BYTES {
            bail!("page_offset {} is outside a 4KiB page", page_offset);
        }
        let pfn = pfn_idx
            .and_then(|idx| columns.get(idx))
            .map(|value| parse_u64_field(value))
            .transpose()
            .with_context(|| format!("invalid pfn at data line {}", line_no + 1))?
            .flatten();
        entries.push(VerifiedLineEntry {
            offset,
            pfn,
            page_offset,
        });
    }
    Ok(entries)
}

fn split_tsv_line(line: &str) -> Vec<&str> {
    line.split('\t').map(str::trim).collect()
}

fn column_index(header: &[&str], name: &str) -> Option<usize> {
    header.iter().position(|column| *column == name)
}

fn parse_usize_field(value: &str) -> Result<Option<usize>> {
    let value = value.trim();
    if value.is_empty() || value == "-1" {
        return Ok(None);
    }
    value
        .parse::<usize>()
        .map(Some)
        .with_context(|| format!("invalid usize field {value:?}"))
}

fn parse_u64_field(value: &str) -> Result<Option<u64>> {
    let value = value.trim();
    if value.is_empty() || value == "-1" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("invalid u64 field {value:?}"))
}

fn parse_f64_field(value: &str) -> Result<Option<f64>> {
    let value = value.trim();
    if value.is_empty() || value == "-1" {
        return Ok(None);
    }
    value
        .parse::<f64>()
        .map(Some)
        .with_context(|| format!("invalid f64 field {value:?}"))
}

fn load_pfn_bucket_allowlist(
    path: &PathBuf,
    cfg: &RunConfig,
    target_link: u32,
) -> Result<PfnBucketAllowlist> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_pfn_bucket_allowlist(&contents, cfg, target_link)
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn parse_pfn_bucket_allowlist(
    contents: &str,
    cfg: &RunConfig,
    target_link: u32,
) -> Result<PfnBucketAllowlist> {
    let data_lines = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let Some(header_line) = data_lines.first() else {
        bail!("empty PFN bucket allowlist catalog");
    };
    let header = split_tsv_line(header_line);
    let Some(pfn_idx) = column_index(&header, "pfn") else {
        bail!("PFN bucket allowlist TSV needs a pfn column");
    };
    let target_idx = column_index(&header, "target_link");
    let top_idx = column_index(&header, "top_link");
    let share_idx = column_index(&header, "target_link_share");
    let top_share_idx = column_index(&header, "top_link_share");
    let bucket_shift_idx = column_index(&header, "bucket_shift");
    let bucket_idx = column_index(&header, "bucket");
    if cfg.predicted_cs_pfn_require_top && top_idx.is_none() {
        bail!("--predicted-cs-pfn-require-top requires top_link in the allowlist TSV");
    }

    let mut buckets = BTreeSet::new();
    let mut bucket_links = BTreeMap::<u64, BTreeSet<u32>>::new();
    let mut accepted_pfn_set = BTreeSet::new();
    let mut accepted_rows = 0usize;
    let mut total_rows = 0usize;
    for (line_no, line) in data_lines.iter().enumerate().skip(1) {
        total_rows = total_rows.saturating_add(1);
        let columns = split_tsv_line(line);
        let Some(pfn) = columns
            .get(pfn_idx)
            .map(|value| parse_u64_field(value))
            .transpose()
            .with_context(|| format!("invalid pfn at data line {}", line_no + 1))?
            .flatten()
        else {
            continue;
        };
        let predicted_link = target_idx
            .and_then(|idx| columns.get(idx))
            .map(|value| parse_u64_field(value))
            .transpose()
            .with_context(|| format!("invalid target_link at data line {}", line_no + 1))?
            .flatten()
            .unwrap_or(target_link as u64);
        if cfg.predicted_cs_pfn_require_top {
            let top = top_idx
                .and_then(|idx| columns.get(idx))
                .map(|value| parse_u64_field(value))
                .transpose()
                .with_context(|| format!("invalid top_link at data line {}", line_no + 1))?
                .flatten();
            if top != Some(target_link as u64) {
                continue;
            }
        }
        let share_column = top_share_idx.or(share_idx);
        let share = share_column
            .and_then(|idx| columns.get(idx))
            .map(|value| parse_f64_field(value))
            .transpose()
            .with_context(|| format!("invalid target_link_share at data line {}", line_no + 1))?
            .flatten()
            .unwrap_or(1.0);
        if share < cfg.predicted_cs_pfn_min_target_share {
            continue;
        }
        let bucket =
            if let (Some(bucket_shift_idx), Some(bucket_idx)) = (bucket_shift_idx, bucket_idx) {
                let row_shift = columns
                    .get(bucket_shift_idx)
                    .map(|value| parse_u64_field(value))
                    .transpose()
                    .with_context(|| format!("invalid bucket_shift at data line {}", line_no + 1))?
                    .flatten()
                    .unwrap_or(cfg.predicted_cs_pfn_bucket_shift as u64);
                let row_bucket = columns
                    .get(bucket_idx)
                    .map(|value| parse_u64_field(value))
                    .transpose()
                    .with_context(|| format!("invalid bucket at data line {}", line_no + 1))?
                    .flatten();
                if row_shift == cfg.predicted_cs_pfn_bucket_shift as u64 {
                    row_bucket.unwrap_or(pfn >> cfg.predicted_cs_pfn_bucket_shift)
                } else {
                    pfn >> cfg.predicted_cs_pfn_bucket_shift
                }
            } else {
                pfn >> cfg.predicted_cs_pfn_bucket_shift
            };
        buckets.insert(bucket);
        bucket_links
            .entry(bucket)
            .or_default()
            .insert(u32::try_from(predicted_link).context("target_link does not fit in u32")?);
        accepted_pfn_set.insert(pfn);
        accepted_rows = accepted_rows.saturating_add(1);
    }
    if buckets.is_empty() {
        bail!(
            "PFN bucket allowlist selected no buckets for target CS{} at min_share={:.3}",
            target_link,
            cfg.predicted_cs_pfn_min_target_share
        );
    }
    Ok(PfnBucketAllowlist {
        buckets,
        bucket_links,
        accepted_rows,
        accepted_pfns: accepted_pfn_set.len(),
        total_rows,
    })
}

fn match_verified_lines_by_offset(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    entries: &[VerifiedLineEntry],
) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut offsets = Vec::new();
    for entry in entries {
        let Some(offset) = entry.offset.map(cache_line_offset) else {
            continue;
        };
        if offset < alloc.len
            && offset_in_config_region(cfg, offset, alloc.len)
            && seen.insert(offset)
        {
            offsets.push(offset);
        }
    }
    offsets
}

fn match_verified_lines_by_pfn(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
    entries: &[VerifiedLineEntry],
    color_mask: Option<u64>,
) -> Result<Vec<usize>> {
    let mut wanted = BTreeMap::<u64, BTreeSet<usize>>::new();
    for entry in entries {
        let Some(pfn) = entry.pfn else {
            continue;
        };
        let key = if let Some(mask) = color_mask {
            pfn & mask
        } else {
            pfn
        };
        wanted.entry(key).or_default().insert(entry.page_offset);
    }
    if wanted.is_empty() {
        return Ok(Vec::new());
    }

    let pages = collect_present_page_pfns_for_region(alloc, cfg)?;
    let mut seen = BTreeSet::new();
    let mut offsets = Vec::new();
    for page in pages {
        let key = if let Some(mask) = color_mask {
            page.pfn & mask
        } else {
            page.pfn
        };
        let Some(page_offsets) = wanted.get(&key) else {
            continue;
        };
        for page_offset in page_offsets {
            let offset = page
                .page_idx
                .saturating_mul(PAGE_BYTES)
                .saturating_add(*page_offset);
            if offset < alloc.len
                && offset_in_config_region(cfg, offset, alloc.len)
                && seen.insert(offset)
            {
                offsets.push(offset);
            }
        }
    }
    Ok(offsets)
}

fn collect_present_page_pfns_for_region(
    alloc: &MmapAllocation,
    cfg: &RunConfig,
) -> Result<Vec<PagePfn>> {
    let pagemap = File::open("/proc/self/pagemap").context("failed to open /proc/self/pagemap")?;
    let (_, _, first_page, last_page) = configured_page_range(alloc, cfg);
    let mut pages = Vec::new();
    let mut saw_present = false;
    let mut saw_nonzero_pfn = false;
    for page_idx in first_page..=last_page {
        let virt = alloc.ptr as usize + page_idx * PAGE_BYTES;
        let entry = read_pagemap_entry(&pagemap, virt)?;
        if !pagemap_present(entry) {
            continue;
        }
        saw_present = true;
        let pfn = pagemap_pfn(entry);
        saw_nonzero_pfn |= pfn != 0;
        if pfn != 0 {
            pages.push(PagePfn { page_idx, pfn });
        }
    }
    if !saw_present {
        bail!("pagemap had no present pages for the selected allocation");
    }
    if !saw_nonzero_pfn {
        bail!("pagemap PFNs are hidden; run with sufficient privilege");
    }
    Ok(pages)
}

fn pfn_for_offset(pagemap: &File, base_addr: usize, offset: usize) -> Option<u64> {
    let entry = read_pagemap_entry(pagemap, base_addr.checked_add(offset)?).ok()?;
    if !pagemap_present(entry) {
        return None;
    }
    let pfn = pagemap_pfn(entry);
    (pfn != 0).then_some(pfn)
}

fn measure_offset_group_links(
    base_addr: usize,
    alloc_len: usize,
    cfg: &RunConfig,
    offsets: Vec<usize>,
) -> Result<Vec<CsLinkBw>> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_cfg = cfg.clone();
    let assigned_cpu = cfg.cpus.first().copied();
    let handle = thread::spawn(move || {
        if let Some(cpu) = assigned_cpu {
            let _ = pin_current_thread(cpu);
        }
        while !stop_requested(&worker_stop) {
            for anchor in &offsets {
                if stop_requested(&worker_stop) {
                    break;
                }
                for line_idx in 0..worker_cfg.lines_per_step {
                    let offset = offset_for_step_line(&worker_cfg, *anchor, line_idx, alloc_len);
                    unsafe {
                        touch_byte(base_addr, offset, AccessKind::Read);
                        flush_cache_line(
                            (base_addr + offset) as *const u8,
                            FlushInstruction::Clflush,
                        );
                    }
                }
            }
        }
    });
    let mut sample_cfg = cfg.clone();
    sample_cfg.sample_window_ms = cfg.hot_cs_window_ms.max(1);
    let links = sample_cs_links(&sample_cfg, None);
    stop.store(true, Ordering::Relaxed);
    handle
        .join()
        .map_err(|_| anyhow!("hot-CS calibration worker panicked"))?;
    links
}

unsafe fn flush_cache_line(ptr: *const u8, instruction: FlushInstruction) {
    match instruction {
        FlushInstruction::Clflush => flush_cache_line_clflush(ptr),
        FlushInstruction::Clflushopt => flush_cache_line_clflushopt(ptr),
    }
}

unsafe fn flush_cache_line_clflush(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    {
        core::arch::x86_64::_mm_clflush(ptr.cast());
    }
    #[cfg(target_arch = "x86")]
    {
        core::arch::x86::_mm_clflush(ptr.cast());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        let _ = ptr;
    }
}

unsafe fn flush_cache_line_clflushopt(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    {
        core::arch::asm!(
            "clflushopt byte ptr [{ptr}]",
            ptr = in(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
    #[cfg(target_arch = "x86")]
    {
        core::arch::asm!(
            "clflushopt byte ptr [{ptr}]",
            ptr = in(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        let _ = ptr;
    }
}

fn flush_cache_fence(instruction: FlushInstruction) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        match instruction {
            FlushInstruction::Clflush => core::arch::x86_64::_mm_mfence(),
            FlushInstruction::Clflushopt => core::arch::x86_64::_mm_sfence(),
        }
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        match instruction {
            FlushInstruction::Clflush => core::arch::x86::_mm_mfence(),
            FlushInstruction::Clflushopt => core::arch::x86::_mm_sfence(),
        }
    }
}

fn apply_numa_policy(alloc: &MmapAllocation, policy: NumaPolicy, nodes: &[usize]) -> Result<()> {
    if policy == NumaPolicy::Default {
        return Ok(());
    }
    let nodemask = build_nodemask(nodes)?;
    let maxnode = nodes.iter().copied().max().unwrap_or(0) + 1;
    let mode = match policy {
        NumaPolicy::Default => 0,
        NumaPolicy::Bind => libc::MPOL_BIND,
        NumaPolicy::Interleave => libc::MPOL_INTERLEAVE,
        NumaPolicy::Preferred => libc::MPOL_PREFERRED,
    };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mbind,
            alloc.ptr as libc::c_ulong,
            alloc.len as libc::c_ulong,
            mode,
            nodemask.as_ptr(),
            maxnode as libc::c_ulong,
            0u32,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "mbind failed for policy {policy} nodes {}",
                nodes
                    .iter()
                    .map(|node| node.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
    }
    Ok(())
}

fn build_nodemask(nodes: &[usize]) -> Result<Vec<libc::c_ulong>> {
    let Some(max_node) = nodes.iter().copied().max() else {
        return Ok(Vec::new());
    };
    let bits_per_word = usize::BITS as usize;
    let mut words = vec![0 as libc::c_ulong; max_node / bits_per_word + 1];
    for node in nodes {
        let word = node / bits_per_word;
        let bit = node % bits_per_word;
        words[word] |= (1 as libc::c_ulong) << bit;
    }
    Ok(words)
}

fn sample_cs_links(cfg: &RunConfig, stop: Option<&AtomicBool>) -> Result<Vec<CsLinkBw>> {
    let path = format!("/dev/cpu/{}/msr", cfg.msr_cpu);
    let file = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {path}; root and msr support are required"))?;
    let fd = file.as_raw_fd();
    let window_ms = if cfg.sample_window_ms > 0 {
        cfg.sample_window_ms
    } else {
        (cfg.duration_ms / cfg.links.len().max(1) as u64).max(1)
    };
    let mut samples = Vec::with_capacity(cfg.links.len());
    for link_id in &cfg.links {
        if optional_stop_requested(stop) {
            samples.push(CsLinkBw {
                link_id: *link_id,
                ..Default::default()
            });
        } else {
            samples.push(sample_cs_link(fd, *link_id, window_ms, stop)?);
        }
    }
    disable_counters(fd);
    Ok(samples)
}

fn sample_cs_link(
    fd: RawFd,
    link_id: u32,
    window_ms: u64,
    stop: Option<&AtomicBool>,
) -> Result<CsLinkBw> {
    let start = start_cs_link_counter(fd, link_id)?;
    sleep_until_stop(Duration::from_millis(window_ms.max(1)), stop);
    finish_cs_link_counter(fd, link_id, start)
}

fn start_cs_link_counter(fd: RawFd, link_id: u32) -> Result<Instant> {
    let event_sel = (link_id << 6) | 0x1f;
    let read_cfg = pack_df_perf_ctl(event_sel, 0xffe, 1);
    let write_cfg = pack_df_perf_ctl(event_sel, 0xfff, 1);

    wrmsr(fd, MSR_DF_PERF_CTL_0, read_cfg)?;
    wrmsr(fd, MSR_DF_PERF_CTL_1, write_cfg)?;
    wrmsr(fd, MSR_DF_PERF_CTR_0, 0)?;
    wrmsr(fd, MSR_DF_PERF_CTR_1, 0)?;
    Ok(Instant::now())
}

fn finish_cs_link_counter(fd: RawFd, link_id: u32, start: Instant) -> Result<CsLinkBw> {
    let read_beats = rdmsr(fd, MSR_DF_PERF_CTR_0)? & MASK_48;
    let write_beats = rdmsr(fd, MSR_DF_PERF_CTR_1)? & MASK_48;
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    disable_counters(fd);
    Ok(CsLinkBw {
        link_id,
        read_mib_s: beats_to_mib_s(read_beats, 64, elapsed),
        write_mib_s: beats_to_mib_s(write_beats, 64, elapsed),
    })
}

fn pack_df_perf_ctl(event_sel: u32, unit_mask: u32, en: u32) -> u64 {
    ((((event_sel >> 12) & 0x3) as u64) << 36)
        | ((((event_sel >> 8) & 0xF) as u64) << 32)
        | ((((unit_mask >> 8) & 0xF) as u64) << 24)
        | (((en & 0x1) as u64) << 22)
        | (((unit_mask & 0xFF) as u64) << 8)
        | ((event_sel & 0xFF) as u64)
}

fn wrmsr(fd: RawFd, msr: u64, value: u64) -> io::Result<()> {
    unsafe {
        if libc::lseek(fd, msr as libc::off_t, libc::SEEK_SET) < 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes = value.to_ne_bytes();
        let wrote = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        if wrote != bytes.len() as isize {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn rdmsr(fd: RawFd, msr: u64) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    unsafe {
        if libc::lseek(fd, msr as libc::off_t, libc::SEEK_SET) < 0 {
            return Err(io::Error::last_os_error());
        }
        let read = libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len());
        if read != bytes.len() as isize {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(u64::from_ne_bytes(bytes))
}

fn disable_counters(fd: RawFd) {
    let _ = wrmsr(fd, MSR_DF_PERF_CTL_0, 0);
    let _ = wrmsr(fd, MSR_DF_PERF_CTL_1, 0);
}

fn beats_to_mib_s(beats: u64, bytes_per_beat: u32, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 {
        0.0
    } else {
        beats as f64 * bytes_per_beat as f64 / (1024.0 * 1024.0) / elapsed_secs
    }
}

fn summarize_cs(links: &[CsLinkBw], active_floor_mib_s: f64) -> CsSummary {
    let totals = links
        .iter()
        .map(|link| link.read_mib_s + link.write_mib_s)
        .collect::<Vec<_>>();
    let total = totals.iter().sum::<f64>();
    let total_read_mib_s = links.iter().map(|link| link.read_mib_s).sum::<f64>();
    let total_write_mib_s = links.iter().map(|link| link.write_mib_s).sum::<f64>();
    let active_links = totals
        .iter()
        .filter(|value| **value >= active_floor_mib_s)
        .count();
    let (top_link, top_bw) = if total > 0.0 {
        links
            .iter()
            .zip(totals.iter())
            .max_by(|(_, left), (_, right)| left.partial_cmp(right).unwrap_or(CmpOrdering::Equal))
            .map(|(link, bw)| (link.link_id as i32, *bw))
            .unwrap_or((-1, 0.0))
    } else {
        (-1, 0.0)
    };
    let mean = if totals.is_empty() {
        0.0
    } else {
        total / totals.len() as f64
    };
    CsSummary {
        total_read_mib_s,
        total_write_mib_s,
        active_links,
        top_link,
        top_link_share: if total > 0.0 { top_bw / total } else { 0.0 },
        max_mean_skew: if mean > 0.0 { top_bw / mean } else { 0.0 },
        gini_skew: gini(&totals),
    }
}

struct L3CounterSet {
    cpu: usize,
    cache_id: String,
    file: File,
}

struct L3Sampler {
    counters: Vec<L3CounterSet>,
    start: Instant,
}

fn start_l3_sampling(cfg: &RunConfig) -> Result<L3Sampler> {
    let mut counters = Vec::with_capacity(cfg.l3_cpus.len());
    for cpu in &cfg.l3_cpus {
        let path = format!("/dev/cpu/{cpu}/msr");
        let file = File::options()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {path}; root and msr support are required"))?;
        let fd = file.as_raw_fd();
        disable_l3_counters(fd, 2);
        let cfgs = [
            pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_ALL),
            pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_MISS),
        ];
        for (idx, value) in cfgs.into_iter().enumerate() {
            let cfg_msr = MSR_CHL3_PMC_CFG_0 + idx as u64 * MSR_CHL3_PMC_STRIDE;
            let ctr_msr = MSR_CHL3_PMC_CTR_0 + idx as u64 * MSR_CHL3_PMC_STRIDE;
            wrmsr(fd, cfg_msr, value)?;
            wrmsr(fd, ctr_msr, 0)?;
            let programmed = rdmsr(fd, cfg_msr)?;
            if (programmed & L3_CFG_CHECK_MASK) != (value & L3_CFG_CHECK_MASK) {
                bail!(
                    "L3 PMC cfg verify failed cpu={} idx={} expected=0x{:x} got=0x{:x}",
                    cpu,
                    idx,
                    value & L3_CFG_CHECK_MASK,
                    programmed & L3_CFG_CHECK_MASK
                );
            }
        }
        counters.push(L3CounterSet {
            cpu: *cpu,
            cache_id: read_l3_cache_id(*cpu).unwrap_or_else(|| format!("cpu{cpu}")),
            file,
        });
    }
    Ok(L3Sampler {
        counters,
        start: Instant::now(),
    })
}

fn finish_l3_sampling(sampler: L3Sampler) -> Result<L3Summary> {
    let elapsed_s = sampler.start.elapsed().as_secs_f64().max(1e-9);
    let expected = [
        pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_ALL),
        pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_MISS),
    ];
    let mut raw_lookup_all = 0u64;
    let mut raw_lookup_miss = 0u64;
    for counter in &sampler.counters {
        let fd = counter.file.as_raw_fd();
        for (idx, expected_cfg) in expected.into_iter().enumerate() {
            let cfg_msr = MSR_CHL3_PMC_CFG_0 + idx as u64 * MSR_CHL3_PMC_STRIDE;
            let current = rdmsr(fd, cfg_msr)?;
            if (current & L3_CFG_CHECK_MASK) != (expected_cfg & L3_CFG_CHECK_MASK) {
                disable_l3_counters(fd, 2);
                bail!(
                    "L3 PMC cfg clobbered cpu={} l3={} idx={} expected=0x{:x} got=0x{:x}",
                    counter.cpu,
                    counter.cache_id,
                    idx,
                    expected_cfg & L3_CFG_CHECK_MASK,
                    current & L3_CFG_CHECK_MASK
                );
            }
        }
        raw_lookup_all = raw_lookup_all.saturating_add(rdmsr(fd, MSR_CHL3_PMC_CTR_0)? & MASK_48);
        raw_lookup_miss = raw_lookup_miss
            .saturating_add(rdmsr(fd, MSR_CHL3_PMC_CTR_0 + MSR_CHL3_PMC_STRIDE)? & MASK_48);
        disable_l3_counters(fd, 2);
    }
    let lookup_all_per_s = raw_lookup_all as f64 / elapsed_s;
    let lookup_miss_per_s = raw_lookup_miss as f64 / elapsed_s;
    let miss_ratio = if raw_lookup_all > 0 {
        (raw_lookup_miss as f64 / raw_lookup_all as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Ok(L3Summary {
        sampled_domains: sampler.counters.len(),
        lookup_all_per_s,
        lookup_miss_per_s,
        hit_ratio: 1.0 - miss_ratio,
        miss_ratio,
        miss_mib_s: lookup_miss_per_s * CACHE_LINE_BYTES as f64 / (1024.0 * 1024.0),
        raw_lookup_all,
        raw_lookup_miss,
    })
}

fn disable_l3_counters(fd: RawFd, counter_count: usize) {
    for idx in 0..counter_count {
        let _ = wrmsr(fd, MSR_CHL3_PMC_CFG_0 + idx as u64 * MSR_CHL3_PMC_STRIDE, 0);
    }
}

fn pack_l3_pmc_ctl(event_code: u64, umask: u64) -> u64 {
    L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES | ((umask & 0xFF) << 8) | (event_code & 0xFF)
}

struct FillCounterSet {
    cpu: usize,
    local_fd: i32,
    near_fd: i32,
    dram_fd: i32,
}

impl Drop for FillCounterSet {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.local_fd);
            libc::close(self.near_fd);
            libc::close(self.dram_fd);
        }
    }
}

struct FillSampler {
    counters: Vec<FillCounterSet>,
    start: Instant,
}

fn start_fill_sampling(cfg: &RunConfig) -> Result<FillSampler> {
    let mut counters = Vec::with_capacity(cfg.fill_cpus.len());
    for cpu in &cfg.fill_cpus {
        let local_fd = open_raw_perf_counter(
            *cpu,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_LOCAL_CCX),
        )
        .with_context(|| format!("failed to open local-CCX fill perf counter on CPU {cpu}"))?;
        let near_fd = open_raw_perf_counter(
            *cpu,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_NEAR_CACHE),
        )
        .with_context(|| format!("failed to open near-cache fill perf counter on CPU {cpu}"))?;
        let dram_fd = open_raw_perf_counter(
            *cpu,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_DRAM_NEAR),
        )
        .with_context(|| format!("failed to open DRAM-near fill perf counter on CPU {cpu}"))?;
        counters.push(FillCounterSet {
            cpu: *cpu,
            local_fd,
            near_fd,
            dram_fd,
        });
    }
    Ok(FillSampler {
        counters,
        start: Instant::now(),
    })
}

fn finish_fill_sampling(sampler: FillSampler) -> Result<FillSummary> {
    let elapsed_ns = sampler.start.elapsed().as_nanos().max(1) as u64;
    let mut raw_local_ccx = 0u64;
    let mut raw_near_cache = 0u64;
    let mut raw_dram_near = 0u64;
    for counter in &sampler.counters {
        raw_local_ccx = raw_local_ccx.saturating_add(
            read_perf_counter(counter.local_fd).with_context(|| {
                format!(
                    "failed to read local-CCX fill counter on CPU {}",
                    counter.cpu
                )
            })?,
        );
        raw_near_cache = raw_near_cache.saturating_add(
            read_perf_counter(counter.near_fd).with_context(|| {
                format!(
                    "failed to read near-cache fill counter on CPU {}",
                    counter.cpu
                )
            })?,
        );
        raw_dram_near = raw_dram_near.saturating_add(
            read_perf_counter(counter.dram_fd).with_context(|| {
                format!(
                    "failed to read DRAM-near fill counter on CPU {}",
                    counter.cpu
                )
            })?,
        );
    }
    let local_ccx_mib_s = fill_count_to_mib_s(raw_local_ccx, elapsed_ns);
    let near_cache_mib_s = fill_count_to_mib_s(raw_near_cache, elapsed_ns);
    let dram_near_mib_s = fill_count_to_mib_s(raw_dram_near, elapsed_ns);
    let total_mib_s = local_ccx_mib_s + near_cache_mib_s + dram_near_mib_s;
    Ok(FillSummary {
        sampled_cpus: sampler.counters.len(),
        local_ccx_mib_s,
        near_cache_mib_s,
        dram_near_mib_s,
        total_mib_s,
        local_ccx_ratio: ratio_or_zero(local_ccx_mib_s, total_mib_s),
        near_cache_ratio: ratio_or_zero(near_cache_mib_s, total_mib_s),
        dram_near_ratio: ratio_or_zero(dram_near_mib_s, total_mib_s),
        raw_local_ccx,
        raw_near_cache,
        raw_dram_near,
    })
}

fn open_raw_perf_counter(cpu: usize, config: u64) -> Result<i32> {
    let mut attr = scx_utils::perf::bindings::perf_event_attr {
        type_: scx_utils::perf::bindings::PERF_TYPE_RAW,
        size: size_of::<scx_utils::perf::bindings::perf_event_attr>() as u32,
        config,
        ..unsafe { std::mem::zeroed() }
    };
    attr.set_exclude_hv(1);
    attr.set_exclude_guest(1);
    let fd = unsafe { scx_utils::perf::perf_event_open(&mut attr, -1, cpu as i32, -1, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("perf_event_open failed");
    }
    unsafe {
        scx_utils::perf::ioctls::reset(fd, 0);
        scx_utils::perf::ioctls::enable(fd, 0);
    }
    Ok(fd)
}

fn read_perf_counter(fd: i32) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if read != bytes.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from_ne_bytes(bytes))
}

fn amd_raw_event_config(event: u16, umask: u8) -> u64 {
    let event_low = u64::from(event & 0x00ff);
    let event_high = u64::from((event >> 8) & 0x000f);
    event_low | (u64::from(umask) << 8) | (event_high << 32)
}

fn fill_count_to_mib_s(count: u64, elapsed_ns: u64) -> f64 {
    let mib_s_x100 = count.saturating_mul(FILL_MIB_PER_SEC_X100_SCALE) as f64 / elapsed_ns as f64;
    mib_s_x100 / 100.0
}

fn ratio_or_zero(value: f64, total: f64) -> f64 {
    if total > 0.0 {
        value / total
    } else {
        0.0
    }
}

fn gini(values: &[f64]) -> f64 {
    let mut sorted = values
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .collect::<Vec<_>>();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(CmpOrdering::Equal));
    let sum = sorted.iter().sum::<f64>();
    if sum <= 0.0 {
        return 0.0;
    }
    let n = sorted.len() as f64;
    let weighted = sorted
        .iter()
        .enumerate()
        .map(|(idx, value)| (idx as f64 + 1.0) * value)
        .sum::<f64>();
    (2.0 * weighted) / (n * sum) - (n + 1.0) / n
}

fn write_results(out: Option<PathBuf>, results: &[RunResult]) -> Result<()> {
    let links = results
        .first()
        .map(|result| result.cfg.links.clone())
        .unwrap_or_default();
    if let Some(path) = out {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        write_tsv(&mut writer, results, &links)?;
    } else {
        let stdout = io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        write_tsv(&mut writer, results, &links)?;
    }
    Ok(())
}

fn write_interrupted_summary_stdout(result: &RunResult) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let cs = &result.cs_summary;
    writeln!(stdout, "# mc_stride_probe interrupted summary")?;
    writeln!(stdout, "# wall_ms\t{:.3}", result.elapsed_ms)?;
    writeln!(stdout, "# worker_cpu_ms\t{:.3}", result.worker_cpu_ms)?;
    writeln!(
        stdout,
        "# total_thread_cpu_ms\t{:.3}",
        result.total_thread_cpu_ms
    )?;
    writeln!(stdout, "# operations\t{}", result.operations)?;
    writeln!(stdout, "# touched_bytes\t{}", result.touched_bytes)?;
    writeln!(
        stdout,
        "# touched_mib\t{:.3}",
        bytes_to_mib(result.touched_bytes)
    )?;
    writeln!(stdout, "# wall_mib_s\t{:.3}", result.mib_s)?;
    writeln!(stdout, "# worker_cpu_mib_s\t{:.3}", result.worker_cpu_mib_s)?;
    writeln!(
        stdout,
        "# evict_touched_bytes\t{}",
        result.evict_touched_bytes
    )?;
    writeln!(
        stdout,
        "# side_touched_bytes\t{}",
        result.side_touched_bytes
    )?;
    writeln!(stdout, "# flush_ops\t{}", result.flush_ops)?;
    writeln!(stdout, "# total_cs_read_mib_s\t{:.3}", cs.total_read_mib_s)?;
    writeln!(
        stdout,
        "# total_cs_write_mib_s\t{:.3}",
        cs.total_write_mib_s
    )?;
    writeln!(stdout, "# top_cs_link\t{}", cs.top_link)?;
    writeln!(stdout, "# top_cs_share\t{:.6}", cs.top_link_share)?;
    stdout.flush()?;
    Ok(())
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn write_tsv<W: Write>(writer: &mut W, results: &[RunResult], links: &[u32]) -> Result<()> {
    writeln!(writer, "{}", tsv_header(links))?;
    for result in results {
        writeln!(writer, "{}", tsv_row(result, links))?;
    }
    Ok(())
}

fn tsv_header(links: &[u32]) -> String {
    let mut columns = vec![
        "pattern",
        "address_mode",
        "access_kind",
        "avx512_unroll",
        "prefetch_distance",
        "threads",
        "inline_worker",
        "cpus",
        "start_cooldown_ms",
        "traffic_active_ms",
        "traffic_cool_ms",
        "size_bytes",
        "stride_bytes",
        "base_offset_bytes",
        "lines_per_step",
        "selected_order",
        "pfn_mask",
        "pfn_value",
        "predicted_cs_select",
        "predicted_cs_link",
        "predicted_cs_line_offsets",
        "predicted_cs_max_pages",
        "predicted_cs_autotune",
        "predicted_cs_segment_pages",
        "predicted_cs_phys_segment_pages",
        "predicted_cs_pfn_allowlist",
        "predicted_cs_pfn_formula",
        "predicted_cs_pfn_bucket_shift",
        "predicted_cs_pfn_min_target_share",
        "predicted_cs_pfn_require_top",
        "predicted_cs_pfn_bucket_score_out",
        "predicted_cs_pfn_bucket_score_seed",
        "predicted_cs_pfn_bucket_score_all_links",
        "predicted_cs_pfn_bucket_sample_offsets",
        "selected_offsets",
        "calibrated_hot_link",
        "hot_cs_score_unit",
        "hot_cs_line_offsets",
        "hot_cs_score_out",
        "verified_lines_in",
        "verified_lines_out",
        "verified_lines_match",
        "verified_lines_color_mask",
        "verified_lines_alloc_retries",
        "verified_lines_min_offsets",
        "cache_evict_bytes",
        "cache_evict_stride_bytes",
        "side_stream_threads",
        "side_stream_cpus",
        "side_stream_size_bytes",
        "side_stream_stride_bytes",
        "flush_threads",
        "self_flush",
        "flush_cpus",
        "flush_instruction",
        "flush_target",
        "flush_pause_ns",
        "flush_max_lines",
        "flush_offsets",
        "residue_period",
        "residue_id",
        "xor_mask",
        "numa_policy",
        "numa_nodes",
        "page_mode",
        "measure_l3",
        "l3_cpus",
        "l3_sampled_domains",
        "measure_fill",
        "fill_cpus",
        "fill_sampled_cpus",
        "interrupted",
        "elapsed_ms",
        "worker_cpu_ms",
        "side_stream_cpu_ms",
        "flush_cpu_ms",
        "total_thread_cpu_ms",
        "operations",
        "touched_bytes",
        "evict_touched_bytes",
        "flush_ops",
        "side_touched_bytes",
        "mib_s",
        "worker_cpu_mib_s",
        "evict_mib_s",
        "flush_mops_s",
        "side_mib_s",
        "ns_access_p50",
        "ns_access_p99",
        "l3_lookup_all_per_s",
        "l3_lookup_miss_per_s",
        "l3_hit_ratio",
        "l3_miss_ratio",
        "l3_miss_mib_s",
        "l3_raw_lookup_all",
        "l3_raw_lookup_miss",
        "fill_local_ccx_mib_s",
        "fill_near_cache_mib_s",
        "fill_dram_near_mib_s",
        "fill_total_mib_s",
        "fill_local_ccx_ratio",
        "fill_near_cache_ratio",
        "fill_dram_near_ratio",
        "fill_raw_local_ccx",
        "fill_raw_near_cache",
        "fill_raw_dram_near",
        "total_cs_read_mib_s",
        "total_cs_write_mib_s",
        "active_cs_links",
        "top_cs_link",
        "top_cs_share",
        "max_mean_skew",
        "gini_skew",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    for link in links {
        columns.push(format!("cs{link}_read_mib_s"));
        columns.push(format!("cs{link}_write_mib_s"));
    }
    columns.join("\t")
}

fn tsv_row(result: &RunResult, links: &[u32]) -> String {
    let cfg = &result.cfg;
    let mut columns = vec![
        cfg.pattern.to_string(),
        cfg.address_mode.to_string(),
        cfg.access_kind.to_string(),
        cfg.avx512_unroll.to_string(),
        cfg.prefetch_distance.to_string(),
        cfg.threads.to_string(),
        cfg.inline_worker.to_string(),
        cfg.cpus_label.clone(),
        cfg.start_cooldown_ms.to_string(),
        cfg.traffic_active_ms.to_string(),
        cfg.traffic_cool_ms.to_string(),
        cfg.size_bytes.to_string(),
        cfg.stride_bytes.to_string(),
        cfg.base_offset_bytes.to_string(),
        cfg.lines_per_step.to_string(),
        cfg.selected_order.to_string(),
        cfg.pfn_mask.to_string(),
        cfg.pfn_value.to_string(),
        cfg.predicted_cs_select.to_string(),
        cfg.predicted_cs_link.to_string(),
        cfg.predicted_cs_line_offsets_label.clone(),
        cfg.predicted_cs_max_pages.to_string(),
        cfg.predicted_cs_autotune.to_string(),
        cfg.predicted_cs_segment_pages.to_string(),
        cfg.predicted_cs_phys_segment_pages.to_string(),
        cfg.predicted_cs_pfn_allowlist
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.predicted_cs_pfn_formula.to_string(),
        cfg.predicted_cs_pfn_bucket_shift.to_string(),
        format!("{:.6}", cfg.predicted_cs_pfn_min_target_share),
        cfg.predicted_cs_pfn_require_top.to_string(),
        cfg.predicted_cs_pfn_bucket_score_out
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.predicted_cs_pfn_bucket_score_seed
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.predicted_cs_pfn_bucket_score_all_links.to_string(),
        cfg.predicted_cs_pfn_bucket_sample_offsets.to_string(),
        result.selected_offsets.to_string(),
        result
            .calibrated_hot_link
            .map(|link| link.to_string())
            .unwrap_or_else(|| "-1".to_string()),
        cfg.hot_cs_score_unit.to_string(),
        cfg.hot_cs_line_offsets_label.clone(),
        cfg.hot_cs_score_out
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.verified_lines_in
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.verified_lines_out
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.verified_lines_match.to_string(),
        cfg.verified_lines_color_mask.to_string(),
        cfg.verified_lines_alloc_retries.to_string(),
        cfg.verified_lines_min_offsets.to_string(),
        cfg.cache_evict_bytes.to_string(),
        cfg.cache_evict_stride_bytes.to_string(),
        cfg.side_stream_threads.to_string(),
        cfg.side_stream_cpus_label.clone(),
        cfg.side_stream_size_bytes.to_string(),
        cfg.side_stream_stride_bytes.to_string(),
        cfg.flush_threads.to_string(),
        cfg.self_flush.to_string(),
        cfg.flush_cpus_label.clone(),
        cfg.flush_instruction.to_string(),
        cfg.flush_target.to_string(),
        cfg.flush_pause_ns.to_string(),
        cfg.flush_max_lines.to_string(),
        result.flush_offsets.to_string(),
        cfg.residue_period.to_string(),
        cfg.residue_id.to_string(),
        cfg.xor_mask.to_string(),
        cfg.numa_policy.to_string(),
        cfg.numa_nodes_label.clone(),
        cfg.page_mode.to_string(),
        cfg.measure_l3.to_string(),
        cfg.l3_cpus_label.clone(),
        result.l3_summary.sampled_domains.to_string(),
        cfg.measure_fill.to_string(),
        cfg.fill_cpus_label.clone(),
        result.fill_summary.sampled_cpus.to_string(),
        result.interrupted.to_string(),
        format!("{:.3}", result.elapsed_ms),
        format!("{:.3}", result.worker_cpu_ms),
        format!("{:.3}", result.side_stream_cpu_ms),
        format!("{:.3}", result.flush_cpu_ms),
        format!("{:.3}", result.total_thread_cpu_ms),
        result.operations.to_string(),
        result.touched_bytes.to_string(),
        result.evict_touched_bytes.to_string(),
        result.flush_ops.to_string(),
        result.side_touched_bytes.to_string(),
        format!("{:.3}", result.mib_s),
        format!("{:.3}", result.worker_cpu_mib_s),
        format!("{:.3}", result.evict_mib_s),
        format!("{:.3}", result.flush_mops_s),
        format!("{:.3}", result.side_mib_s),
        format!("{:.3}", result.ns_access_p50),
        format!("{:.3}", result.ns_access_p99),
        format!("{:.3}", result.l3_summary.lookup_all_per_s),
        format!("{:.3}", result.l3_summary.lookup_miss_per_s),
        format!("{:.6}", result.l3_summary.hit_ratio),
        format!("{:.6}", result.l3_summary.miss_ratio),
        format!("{:.3}", result.l3_summary.miss_mib_s),
        result.l3_summary.raw_lookup_all.to_string(),
        result.l3_summary.raw_lookup_miss.to_string(),
        format!("{:.3}", result.fill_summary.local_ccx_mib_s),
        format!("{:.3}", result.fill_summary.near_cache_mib_s),
        format!("{:.3}", result.fill_summary.dram_near_mib_s),
        format!("{:.3}", result.fill_summary.total_mib_s),
        format!("{:.6}", result.fill_summary.local_ccx_ratio),
        format!("{:.6}", result.fill_summary.near_cache_ratio),
        format!("{:.6}", result.fill_summary.dram_near_ratio),
        result.fill_summary.raw_local_ccx.to_string(),
        result.fill_summary.raw_near_cache.to_string(),
        result.fill_summary.raw_dram_near.to_string(),
        format!("{:.3}", result.cs_summary.total_read_mib_s),
        format!("{:.3}", result.cs_summary.total_write_mib_s),
        result.cs_summary.active_links.to_string(),
        result.cs_summary.top_link.to_string(),
        format!("{:.6}", result.cs_summary.top_link_share),
        format!("{:.6}", result.cs_summary.max_mean_skew),
        format!("{:.6}", result.cs_summary.gini_skew),
    ];
    for link_id in links {
        let link = result.cs_links.iter().find(|link| link.link_id == *link_id);
        columns.push(format!(
            "{:.3}",
            link.map(|link| link.read_mib_s).unwrap_or(0.0)
        ));
        columns.push(format!(
            "{:.3}",
            link.map(|link| link.write_mib_s).unwrap_or(0.0)
        ));
    }
    columns.join("\t")
}

fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(CmpOrdering::Equal));
    let idx = ((values.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    values[idx]
}

fn parse_cpulist(spec: Option<&str>) -> Result<Vec<usize>> {
    let Some(spec) = spec.filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    parse_range_list(spec)
}

fn parse_link_list(spec: &str) -> Result<Vec<u32>> {
    parse_range_list(spec)?
        .into_iter()
        .map(|value| u32::try_from(value).context("link id does not fit in u32"))
        .collect()
}

fn parse_usize_list(spec: Option<&str>) -> Result<Vec<usize>> {
    let Some(spec) = spec.filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    spec.split(',')
        .map(|part| {
            part.trim()
                .parse::<usize>()
                .with_context(|| format!("invalid list item {part:?}"))
        })
        .collect()
}

fn parse_range_list(spec: &str) -> Result<Vec<usize>> {
    let mut values = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start.trim().parse()?;
            let end: usize = end.trim().parse()?;
            if end < start {
                bail!("invalid descending range {part:?}");
            }
            values.extend(start..=end);
        } else {
            values.push(part.parse()?);
        }
    }
    Ok(values)
}

fn parse_byte_sweep(spec: &str) -> Result<Vec<usize>> {
    let spec = spec.trim();
    if spec.contains(':') {
        let parts = spec.split(':').collect::<Vec<_>>();
        if parts.len() != 3 {
            bail!("byte range must use start:end:step");
        }
        let start = parse_byte_size(parts[0])?;
        let end = parse_byte_size(parts[1])?;
        let step = parse_byte_size(parts[2])?;
        if step == 0 {
            bail!("byte range step must be greater than zero");
        }
        if end < start {
            bail!("byte range end must be >= start");
        }
        let mut values = Vec::new();
        let mut current = start;
        while current <= end {
            values.push(current);
            current = current.saturating_add(step);
            if current == usize::MAX {
                break;
            }
        }
        Ok(values)
    } else {
        spec.split(',')
            .filter(|part| !part.trim().is_empty())
            .map(parse_byte_size)
            .collect()
    }
}

fn parse_byte_size_arg(value: &str) -> std::result::Result<usize, String> {
    parse_byte_size(value).map_err(|err| err.to_string())
}

fn parse_byte_size(value: &str) -> Result<usize> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty byte size");
    }
    let split_idx = value
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '_'))
        .unwrap_or(value.len());
    let number = value[..split_idx].replace('_', "");
    let suffix = value[split_idx..].trim().to_ascii_lowercase();
    let base: usize = number
        .parse()
        .with_context(|| format!("invalid byte size number {number:?}"))?;
    let multiplier = match suffix.as_str() {
        "" | "b" => 1usize,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => bail!("unsupported byte size suffix {other:?}"),
    };
    base.checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size overflow"))
}

fn parse_u64_arg(value: &str) -> std::result::Result<u64, String> {
    let value = value.trim().replace('_', "");
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|err| err.to_string())
    } else {
        value.parse::<u64>().map_err(|err| err.to_string())
    }
}

fn default_l3_sample_cpus(worker_cpus: &[usize], fallback_cpu: usize) -> Vec<usize> {
    let source = if worker_cpus.is_empty() {
        vec![fallback_cpu]
    } else {
        worker_cpus.to_vec()
    };
    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for cpu in source {
        let key = read_l3_cache_id(cpu).unwrap_or_else(|| format!("cpu{cpu}"));
        if seen.insert(key) {
            selected.push(cpu);
        }
    }
    selected
}

fn read_l3_cache_id(cpu: usize) -> Option<String> {
    for idx in 0..8 {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{idx}");
        let level = std::fs::read_to_string(format!("{base}/level")).ok()?;
        if level.trim() != "3" {
            continue;
        }
        let cache_type = std::fs::read_to_string(format!("{base}/type")).ok()?;
        if cache_type.trim() != "Unified" {
            continue;
        }
        return std::fs::read_to_string(format!("{base}/id"))
            .ok()
            .map(|value| value.trim().to_string());
    }
    None
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu, &mut cpuset);
    }
    let ret =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn round_up(value: usize, align: usize) -> usize {
    if align == 0 {
        return value;
    }
    value
        .checked_add(align - 1)
        .map(|value| value / align * align)
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_for_test(mode: AddressMode) -> RunConfig {
        RunConfig {
            pattern: Pattern::Stream,
            address_mode: mode,
            access_kind: AccessKind::Read,
            avx512_unroll: 4,
            prefetch_distance: 0,
            threads: 1,
            inline_worker: false,
            cpus: Vec::new(),
            cpus_label: "unbound".to_string(),
            duration_ms: 1,
            start_cooldown_ms: 0,
            traffic_active_ms: 0,
            traffic_cool_ms: 0,
            size_bytes: 4096,
            stride_bytes: 64,
            base_offset_bytes: 128,
            residue_period: 4,
            residue_id: 2,
            xor_mask: 0x180,
            numa_policy: NumaPolicy::Default,
            numa_nodes: Vec::new(),
            numa_nodes_label: "none".to_string(),
            page_mode: PageMode::Base,
            measure_cs: false,
            measure_l3: false,
            l3_cpus: vec![0],
            l3_cpus_label: "0".to_string(),
            measure_fill: false,
            fill_cpus: vec![0],
            fill_cpus_label: "0".to_string(),
            links: vec![0, 1, 2, 3],
            msr_cpu: 0,
            sample_window_ms: 0,
            active_link_floor_mib_s: 100.0,
            batch_ops: 16,
            lines_per_step: 1,
            selected_order: SelectedOrder::RoundRobin,
            pfn_mask: 0,
            pfn_value: 0,
            pfn_max_pages: 0,
            predicted_cs_select: 0,
            predicted_cs_link: 0,
            predicted_cs_line_offsets: (0..PAGE_BYTES).step_by(CACHE_LINE_BYTES).collect(),
            predicted_cs_line_offsets_label: "0:4032:64".to_string(),
            predicted_cs_max_pages: 0,
            predicted_cs_autotune: false,
            predicted_cs_segment_pages: 0,
            predicted_cs_phys_segment_pages: 0,
            predicted_cs_pfn_allowlist: None,
            predicted_cs_pfn_formula: PredictedCsPfnFormula::None,
            predicted_cs_pfn_bucket_shift: 12,
            predicted_cs_pfn_min_target_share: 0.95,
            predicted_cs_pfn_require_top: true,
            predicted_cs_pfn_bucket_score_out: None,
            predicted_cs_pfn_bucket_score_seed: None,
            predicted_cs_pfn_bucket_score_all_links: false,
            predicted_cs_pfn_bucket_sample_offsets: 1024,
            hot_cs_select: 0,
            hot_cs_link: None,
            hot_cs_candidates: 64,
            hot_cs_group_pages: 8,
            hot_cs_window_ms: 1,
            hot_cs_score_unit: HotCsScoreUnit::Group,
            hot_cs_line_offsets: vec![0],
            hot_cs_line_offsets_label: "0".to_string(),
            hot_cs_score_out: None,
            verified_lines_in: None,
            verified_lines_out: None,
            verified_lines_match: VerifiedLineMatch::Auto,
            verified_lines_color_mask: 0xff,
            verified_lines_alloc_retries: 1,
            verified_lines_min_offsets: 0,
            cache_evict_bytes: 0,
            cache_evict_stride_bytes: 64,
            side_stream_threads: 0,
            side_stream_cpus: Vec::new(),
            side_stream_cpus_label: "unbound".to_string(),
            side_stream_size_bytes: 0,
            side_stream_stride_bytes: 64,
            flush_threads: 0,
            self_flush: false,
            flush_cpus: Vec::new(),
            flush_cpus_label: "unbound".to_string(),
            flush_instruction: FlushInstruction::Clflush,
            flush_target: FlushTarget::Chase,
            flush_pause_ns: 0,
            flush_max_lines: 0,
            chase_nodes: 64,
        }
    }

    #[test]
    fn parses_cpu_and_link_lists() {
        assert_eq!(
            parse_cpulist(Some("0-2,7,9-10")).unwrap(),
            vec![0, 1, 2, 7, 9, 10]
        );
        assert_eq!(parse_link_list("0-1,11").unwrap(), vec![0, 1, 11]);
        assert!(parse_cpulist(Some("4-2")).is_err());
    }

    #[test]
    fn inline_worker_validation_is_single_thread_only() {
        let Command::Run(valid) = Cli::parse_from([
            "mc_stride_probe",
            "run",
            "--inline-worker",
            "--start-cooldown-ms",
            "250",
            "--traffic-active-ms",
            "100",
            "--traffic-cool-ms",
            "200",
            "--measure-cs=false",
        ])
        .command
        else {
            panic!("expected run command");
        };
        let cfg = config_from_common(valid.common, valid.stride, valid.base_offset).unwrap();
        assert!(cfg.inline_worker);
        assert_eq!(cfg.start_cooldown_ms, 250);
        assert_eq!(cfg.traffic_active_ms, 100);
        assert_eq!(cfg.traffic_cool_ms, 200);

        let Command::Run(invalid) = Cli::parse_from([
            "mc_stride_probe",
            "run",
            "--inline-worker",
            "--threads",
            "2",
            "--measure-cs=false",
        ])
        .command
        else {
            panic!("expected run command");
        };
        assert!(config_from_common(invalid.common, invalid.stride, invalid.base_offset).is_err());

        let Command::Run(invalid_phase) = Cli::parse_from([
            "mc_stride_probe",
            "run",
            "--traffic-active-ms",
            "100",
            "--measure-cs=false",
        ])
        .command
        else {
            panic!("expected run command");
        };
        assert!(config_from_common(
            invalid_phase.common,
            invalid_phase.stride,
            invalid_phase.base_offset
        )
        .is_err());
    }

    #[test]
    fn parses_byte_sizes_and_sweeps() {
        assert_eq!(parse_byte_size("64").unwrap(), 64);
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_byte_size("1_gib").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_byte_sweep("0:256:128").unwrap(), vec![0, 128, 256]);
        assert_eq!(parse_byte_sweep("64,1KiB").unwrap(), vec![64, 1024]);
    }

    #[test]
    fn builds_nodemask_words() {
        let mask = build_nodemask(&[0, 2, 65]).unwrap();
        assert_eq!(mask[0] & 0b101, 0b101);
        assert_ne!(mask[1] & 0b10, 0);
    }

    #[test]
    fn address_generators_stay_in_allocation() {
        for mode in [
            AddressMode::Linear,
            AddressMode::Residue,
            AddressMode::Xor,
            AddressMode::PageOffset,
        ] {
            let cfg = cfg_for_test(mode);
            for logical in 0..1024 {
                let offset = address_offset(&cfg, logical, 8192);
                assert!(offset < 8192);
                assert!(offset >= cfg.base_offset_bytes);
            }
        }
    }

    #[test]
    fn step_lines_wrap_inside_configured_region() {
        let mut cfg = cfg_for_test(AddressMode::Linear);
        cfg.size_bytes = 256;
        cfg.base_offset_bytes = 128;
        cfg.lines_per_step = 8;
        let anchor = 128 + 192;
        let offsets = (0..cfg.lines_per_step)
            .map(|line_idx| offset_for_step_line(&cfg, anchor, line_idx, 1024))
            .collect::<Vec<_>>();
        assert_eq!(offsets, vec![320, 128, 192, 256, 320, 128, 192, 256]);
    }

    #[test]
    fn pointer_chase_ring_is_permutation() {
        let cfg = cfg_for_test(AddressMode::Linear);
        let alloc = MmapAllocation::new(8192, PageMode::Base).unwrap();
        let ring = build_chase_ring(alloc.ptr as usize, alloc.len, &cfg, &[]).unwrap();
        let set = ring.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(set.len(), ring.len());
        let mut seen = BTreeSet::new();
        let mut current = ring[0];
        for _ in 0..ring.len() {
            assert!(seen.insert(current));
            current = unsafe { ptr::read_volatile(alloc.ptr.add(current) as *const usize) };
        }
        assert_eq!(current, ring[0]);
        assert_eq!(seen.len(), ring.len());
    }

    #[test]
    fn flush_offsets_cover_chase_and_payload_lines() {
        let mut cfg = cfg_for_test(AddressMode::Linear);
        cfg.flush_threads = 1;
        cfg.flush_target = FlushTarget::All;
        cfg.flush_max_lines = 0;
        cfg.lines_per_step = 2;
        let ring = vec![128, 4096];
        let offsets = build_flush_offsets(8192, &cfg, &[], Some(&ring)).unwrap();
        assert!(offsets.contains(&128));
        assert!(offsets.contains(&4096));
        assert!(offsets.contains(&192));
        assert!(offsets.contains(&4160));
    }

    #[test]
    fn bucket_round_robin_spreads_sources() {
        let empty_pages: [PagePfn; 0] = [];
        let sources = vec![
            BucketPredictedSource {
                bucket: 10,
                predicted_link: 0,
                pages: &empty_pages,
            },
            BucketPredictedSource {
                bucket: 11,
                predicted_link: 3,
                pages: &empty_pages,
            },
            BucketPredictedSource {
                bucket: 12,
                predicted_link: 4,
                pages: &empty_pages,
            },
        ];
        let source_offsets = vec![vec![100, 101], vec![200], vec![300, 301]];
        let (offsets, stats) = round_robin_bucket_offsets(&sources, &source_offsets, 4);

        assert_eq!(offsets, vec![100, 200, 300, 101]);
        assert_eq!(stats.used_sources, 3);
        assert_eq!(stats.used_buckets, 3);
    }

    #[test]
    fn bucket_ordering_modes_shape_selected_offsets() {
        let empty_pages: [PagePfn; 0] = [];
        let sources = vec![
            BucketPredictedSource {
                bucket: 10,
                predicted_link: 0,
                pages: &empty_pages,
            },
            BucketPredictedSource {
                bucket: 11,
                predicted_link: 3,
                pages: &empty_pages,
            },
            BucketPredictedSource {
                bucket: 12,
                predicted_link: 4,
                pages: &empty_pages,
            },
        ];
        let source_offsets = vec![vec![300, 301], vec![100], vec![200, 201]];

        let (round_robin, rr_stats) =
            order_bucket_offsets(SelectedOrder::RoundRobin, &sources, &source_offsets, 4);
        assert_eq!(round_robin, vec![300, 100, 200, 301]);
        assert_eq!(rr_stats.used_sources, 3);

        let (virtual_sorted, sorted_stats) =
            order_bucket_offsets(SelectedOrder::VirtualSorted, &sources, &source_offsets, 4);
        assert_eq!(virtual_sorted, vec![100, 200, 201, 300]);
        assert_eq!(sorted_stats.used_sources, 3);

        let (bucket_grouped, grouped_stats) =
            order_bucket_offsets(SelectedOrder::BucketGrouped, &sources, &source_offsets, 4);
        assert_eq!(bucket_grouped, vec![300, 301, 100, 200]);
        assert_eq!(grouped_stats.used_sources, 3);
    }

    #[test]
    fn avx512_stream_unroll_requires_colored_single_line_steps() {
        let mut cfg = cfg_for_test(AddressMode::Linear);
        cfg.pattern = Pattern::Stream;
        cfg.access_kind = AccessKind::Avx512Read;
        cfg.lines_per_step = 1;
        cfg.self_flush = false;
        cfg.flush_instruction = FlushInstruction::Clflush;
        assert_eq!(stream_pool_avx512_unroll(&cfg, 4), Some(4));
        assert_eq!(stream_pool_clflushopt_avx512_unroll(&cfg, 4), None);

        cfg.avx512_unroll = 8;
        assert_eq!(stream_pool_avx512_unroll(&cfg, 7), None);
        assert_eq!(stream_pool_avx512_unroll(&cfg, 8), Some(8));

        cfg.avx512_unroll = 4;
        cfg.lines_per_step = 4;
        assert_eq!(stream_pool_avx512_unroll(&cfg, 4), None);

        cfg.lines_per_step = 1;
        cfg.self_flush = true;
        assert_eq!(stream_pool_avx512_unroll(&cfg, 4), None);
        assert_eq!(stream_pool_clflushopt_avx512_unroll(&cfg, 4), None);

        cfg.flush_instruction = FlushInstruction::Clflushopt;
        cfg.avx512_unroll = 16;
        assert_eq!(stream_pool_clflushopt_avx512_unroll(&cfg, 4), Some(4));
    }

    #[test]
    fn line_candidate_expansion_uses_page_offsets() {
        let mut cfg = cfg_for_test(AddressMode::Linear);
        cfg.size_bytes = 8192;
        cfg.base_offset_bytes = 0;
        cfg.hot_cs_score_unit = HotCsScoreUnit::Line;
        cfg.hot_cs_line_offsets = vec![0, 64, 512];
        let expanded = expand_line_candidate_offsets(&[4096], &cfg, 8192).unwrap();
        assert_eq!(expanded, vec![4096, 4160, 4608]);
    }

    #[test]
    fn verified_line_catalog_parses_and_matches_offsets() {
        let catalog = "\
catalog_version\tselected_idx\toffset\tpage_offset\tpfn\ttarget_link\ttop_link\ttop_link_share
1\t0\t128\t128\t-1\t0\t0\t0.99
1\t1\t4096\t0\t-1\t0\t0\t0.98
";
        let entries = parse_verified_line_entries(catalog).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].offset, Some(128));
        assert_eq!(entries[0].page_offset, 128);
        assert_eq!(entries[0].pfn, None);

        let cfg = cfg_for_test(AddressMode::Linear);
        let alloc = MmapAllocation::new(8192, PageMode::Base).unwrap();
        let offsets = match_verified_lines_by_offset(&alloc, &cfg, &entries);
        assert_eq!(offsets, vec![128, 4096]);

        let plain = parse_verified_line_entries("128\n4096\n").unwrap();
        assert_eq!(plain[0].offset, Some(128));
        assert_eq!(plain[1].page_offset, 0);
    }

    #[test]
    fn pfn_bucket_allowlist_filters_by_target_share() {
        let mut cfg = cfg_for_test(AddressMode::Linear);
        cfg.predicted_cs_pfn_bucket_shift = 8;
        cfg.predicted_cs_pfn_min_target_share = 0.95;
        cfg.predicted_cs_pfn_require_top = true;
        let catalog = "\
catalog_version\tpfn\tpage_offset\ttarget_link\ttop_link\ttarget_link_share
1\t4096\t0\t0\t0\t0.99
1\t4097\t64\t0\t3\t0.98
1\t8192\t128\t0\t0\t0.90
1\t12288\t192\t3\t3\t0.99
1\t16384\t256\t0\t0\t0.96
";
        let allowlist = parse_pfn_bucket_allowlist(catalog, &cfg, 0).unwrap();
        assert_eq!(allowlist.accepted_rows, 2);
        assert_eq!(allowlist.accepted_pfns, 2);
        assert!(allowlist.buckets.contains(&(4096 >> 8)));
        assert!(allowlist.buckets.contains(&(16384 >> 8)));
        assert!(!allowlist.buckets.contains(&(8192 >> 8)));
    }

    #[test]
    fn observed_epyc_line_hash_matches_scored_examples() {
        let base_pfn = 1_911_808;
        assert_eq!(observed_epyc_line_hash_cs(base_pfn, 0), Some(0));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn, 256), Some(3));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 1, 0), Some(4));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 1, 256), Some(5));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 2, 0), Some(6));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 2, 256), Some(9));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 3, 0), Some(10));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn + 3, 256), Some(11));
        assert_eq!(observed_epyc_line_hash_cs(base_pfn, 1), None);
        assert!(predicted_cs_link_to_code(1).is_none());
    }

    #[test]
    fn observed_epyc_node0_v1_formula_matches_scored_buckets() {
        assert_eq!(observed_epyc_link_index(0), Some(0));
        assert_eq!(observed_epyc_link_index(3), Some(1));
        assert_eq!(observed_epyc_link_index(11), Some(7));
        assert_eq!(observed_epyc_link_index(1), None);

        assert_eq!(observed_epyc_node0_v1_bucket_class(5309), 0);
        assert_eq!(observed_epyc_node0_v1_bucket_class(5457), 2);
        assert_eq!(observed_epyc_node0_v1_bucket_class(5487), 1);

        let pfn_5309 = 5309u64 << OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT;
        let pfn_5457 = 5457u64 << OBSERVED_EPYC_NODE0_V1_BUCKET_SHIFT;
        assert_eq!(
            observed_epyc_node0_v1_predicted_link_for_target(pfn_5309, 0),
            Some(0)
        );
        assert_eq!(
            observed_epyc_node0_v1_predicted_link_for_target(pfn_5457, 0),
            Some(4)
        );
        assert_eq!(
            observed_epyc_node0_v1_predicted_link_for_target(pfn_5457, 3),
            Some(5)
        );
        assert_eq!(
            formula_bucket_and_predicted_link(
                pfn_5457,
                0,
                PredictedCsPfnFormula::ObservedEpycNode0V1
            ),
            Some((5457, 4))
        );
        assert_eq!(
            formula_bucket_and_predicted_link(
                pfn_5457,
                1,
                PredictedCsPfnFormula::ObservedEpycNode0V1
            ),
            None
        );
    }

    #[test]
    fn summarizes_skew_metrics() {
        let balanced = vec![
            CsLinkBw {
                link_id: 0,
                read_mib_s: 10.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 1,
                read_mib_s: 10.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 2,
                read_mib_s: 10.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 3,
                read_mib_s: 10.0,
                write_mib_s: 0.0,
            },
        ];
        let summary = summarize_cs(&balanced, 1.0);
        assert_eq!(summary.active_links, 4);
        assert!((summary.max_mean_skew - 1.0).abs() < 1e-9);
        assert!(summary.gini_skew.abs() < 1e-9);

        let single_hot = vec![
            CsLinkBw {
                link_id: 0,
                read_mib_s: 40.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 1,
                read_mib_s: 0.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 2,
                read_mib_s: 0.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 3,
                read_mib_s: 0.0,
                write_mib_s: 0.0,
            },
        ];
        let summary = summarize_cs(&single_hot, 1.0);
        assert_eq!(summary.top_link, 0);
        assert!((summary.top_link_share - 1.0).abs() < 1e-9);
        assert!((summary.max_mean_skew - 4.0).abs() < 1e-9);
        assert!(summary.gini_skew > 0.7);

        let partial = vec![
            CsLinkBw {
                link_id: 0,
                read_mib_s: 30.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 1,
                read_mib_s: 10.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 2,
                read_mib_s: 0.0,
                write_mib_s: 0.0,
            },
            CsLinkBw {
                link_id: 3,
                read_mib_s: 0.0,
                write_mib_s: 0.0,
            },
        ];
        let summary = summarize_cs(&partial, 1.0);
        assert_eq!(summary.active_links, 2);
        assert!(summary.gini_skew > 0.5);
    }

    #[test]
    fn tsv_header_and_row_match_width() {
        let cfg = cfg_for_test(AddressMode::Linear);
        let links = cfg.links.clone();
        let result = RunResult {
            cfg,
            interrupted: false,
            selected_offsets: 0,
            calibrated_hot_link: None,
            flush_offsets: 0,
            elapsed_ms: 10.0,
            worker_cpu_ms: 9.0,
            side_stream_cpu_ms: 0.0,
            flush_cpu_ms: 0.0,
            total_thread_cpu_ms: 9.0,
            operations: 100,
            touched_bytes: 6400,
            evict_touched_bytes: 0,
            flush_ops: 0,
            side_touched_bytes: 0,
            mib_s: 625.0,
            worker_cpu_mib_s: 694.0,
            evict_mib_s: 0.0,
            flush_mops_s: 0.0,
            side_mib_s: 0.0,
            ns_access_p50: 1.0,
            ns_access_p99: 2.0,
            l3_summary: L3Summary::default(),
            fill_summary: FillSummary::default(),
            cs_links: links
                .iter()
                .map(|link_id| CsLinkBw {
                    link_id: *link_id,
                    read_mib_s: *link_id as f64,
                    write_mib_s: 0.0,
                })
                .collect(),
            cs_summary: CsSummary::default(),
        };
        let header = tsv_header(&links);
        let row = tsv_row(&result, &links);
        assert_eq!(header.split('\t').count(), row.split('\t').count());
        assert!(header.contains("cs3_write_mib_s"));
    }
}
