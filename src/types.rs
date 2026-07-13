use plain::Plain;
use std::collections::BTreeSet;

pub const MAX_DOMAINS: usize = 16;
pub const MAX_CPU_IDS: usize = 128;
pub const COMM_LEN: usize = 16;
pub const MEM_SOURCE_COUNT: usize = 3;
pub const MEM_SOURCE_LOCAL_CCX: usize = 0;
pub const MEM_SOURCE_NEAR_CACHE: usize = 1;
pub const MEM_SOURCE_DRAM_NEAR: usize = 2;

pub const LLC_STALE_MS_DEFAULT: u64 = 40;
pub const DF_STALE_MS_DEFAULT: u64 = 120;
pub const DEFAULT_DF_CAPACITY_MIB_S_X100: u32 = 2_000_000;
pub const DEFAULT_SLICE_US: u64 = 4_000;
pub const MIN_VALID_RUN_NS: u64 = 200_000;
pub const SIGNATURE_STABLE_SAMPLES: u32 = 2;
pub const SIGNATURE_CONFIDENCE_THRESHOLD_X100: u32 = 4_000;
pub const VILLAIN_RESLICE_DIVISOR: u64 = 4;
pub const VILLAIN_RESLICE_MIN_NS: u64 = 250_000;

pub const fn villain_reslice_slice_ns() -> u64 {
    let slice_ns = DEFAULT_SLICE_US * 1_000 / VILLAIN_RESLICE_DIVISOR;
    if slice_ns < VILLAIN_RESLICE_MIN_NS {
        VILLAIN_RESLICE_MIN_NS
    } else {
        slice_ns
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThreadClass {
    #[default]
    Cold = 0,
    LinkNeed = 1,
    Congested = 2,
}

impl ThreadClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "Cold",
            Self::LinkNeed => "LinkNeed",
            Self::Congested => "Congested",
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HotCoolState {
    #[default]
    Cool = 0,
    Hot = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QueueTrigger {
    #[default]
    Enqueue = 0,
    Tick = 1,
    VillainReslice = 2,
    Helper = 3,
}

impl QueueTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enqueue => "enqueue",
            Self::Tick => "tick",
            Self::VillainReslice => "villain_reslice",
            Self::Helper => "helper",
        }
    }

    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Tick,
            2 => Self::VillainReslice,
            3 => Self::Helper,
            _ => Self::Enqueue,
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TickDecision {
    #[default]
    None = 0,
    Stay = 1,
    Move = 2,
    Defer = 3,
    Reslice = 4,
    FastpathBypass = 5,
}

impl TickDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Stay => "stay",
            Self::Move => "move",
            Self::Defer => "defer",
            Self::Reslice => "reslice",
            Self::FastpathBypass => "fastpath_bypass",
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LlcStateValue {
    pub sample_ts_ns: u64,
    pub raw_l3_to_l3_ns_x100: u32,
    pub raw_dram_lat_ns_x100: u32,
    pub raw_l3_bw_mib_s_x100: u32,
    pub raw_miss_ratio_ppm: u32,
    pub raw_l3_req_per_s: u64,
    pub raw_l3_miss_per_s: u64,
    pub raw_pressure_pct_x100: u32,
    pub ewma_pressure_pct_x100: u32,
    pub state: u32,
    pub hot_count: u32,
    pub cool_count: u32,
    pub valid: u32,
}

unsafe impl Plain for LlcStateValue {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CcmDfStateValue {
    pub sample_ts_ns: u64,
    pub ccx_id: u32,
    pub ccm_id: u32,
    pub raw_read_bw_mib_s_x100: u32,
    pub raw_write_bw_mib_s_x100: u32,
    pub raw_pressure_pct_x100: u32,
    pub ewma_pressure_pct_x100: u32,
    pub state: u32,
    pub hot_count: u32,
    pub cool_count: u32,
    pub valid: u32,
}

unsafe impl Plain for CcmDfStateValue {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CpuStateValue {
    pub domain_id: u32,
    pub idle: u32,
    pub cpu_dsq_depth: u32,
    pub current_tid: u32,
    pub last_update_ns: u64,
}

unsafe impl Plain for CpuStateValue {}

#[derive(Clone, Debug)]
pub struct DomainInfo {
    pub domain_id: u32,
    pub kernel_l3_id: u32,
    pub rep_cpu: u32,
    pub cpus: Vec<u32>,
    pub l3_size_mb: f64,
}

#[derive(Clone, Debug)]
pub struct TopologyLayout {
    pub nr_cpu_ids: usize,
    pub domains: Vec<DomainInfo>,
    pub cpu_to_domain: Vec<Option<u32>>,
}

#[derive(Clone, Debug)]
pub struct MappingInfo {
    pub domain_to_ccx: Vec<u32>,
    pub domain_to_ccm: Vec<Option<u32>>,
    pub domain_to_df_capacity_mib_s_x100: Vec<Option<u32>>,
    pub cs_link_capacity_mib_s_x100: Vec<Option<u32>>,
    pub eligible_domains: BTreeSet<u32>,
    pub excluded_domains: BTreeSet<u32>,
    pub eligible_cpus: BTreeSet<u32>,
}

impl MappingInfo {
    pub fn link_count(&self) -> usize {
        self.domain_to_ccm
            .iter()
            .copied()
            .flatten()
            .max()
            .map(|link_id| link_id as usize + 1)
            .unwrap_or(0)
    }

    pub fn cs_link_count(&self) -> usize {
        self.domain_to_ccx
            .len()
            .max(self.cs_link_capacity_mib_s_x100.len())
            .min(MAX_DOMAINS)
    }

    pub fn link_for_domain(&self, domain_id: u32) -> Option<u32> {
        self.domain_to_ccm
            .get(domain_id as usize)
            .copied()
            .flatten()
    }

    pub fn df_capacity_mib_s_x100(&self, domain_id: u32) -> u32 {
        self.domain_to_df_capacity_mib_s_x100
            .get(domain_id as usize)
            .copied()
            .flatten()
            .unwrap_or(DEFAULT_DF_CAPACITY_MIB_S_X100)
    }

    pub fn max_df_capacity_mib_s_x100(&self) -> u32 {
        self.domain_to_df_capacity_mib_s_x100
            .iter()
            .copied()
            .flatten()
            .max()
            .unwrap_or(DEFAULT_DF_CAPACITY_MIB_S_X100)
    }

    pub fn cs_capacity_mib_s_x100(&self, link_id: u32) -> u32 {
        self.cs_link_capacity_mib_s_x100
            .get(link_id as usize)
            .copied()
            .flatten()
            .unwrap_or(DEFAULT_DF_CAPACITY_MIB_S_X100)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FilteredMetric {
    pub raw_x100: u32,
    pub ewma_x100: u32,
    pub state: HotCoolState,
    pub hot_count: u32,
    pub cool_count: u32,
    pub sample_ts_ns: u64,
    pub valid: bool,
}

impl Default for FilteredMetric {
    fn default() -> Self {
        Self {
            raw_x100: 0,
            ewma_x100: 0,
            state: HotCoolState::Cool,
            hot_count: 0,
            cool_count: 0,
            sample_ts_ns: 0,
            valid: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LlcSampleUpdate {
    pub domain_id: u32,
    pub raw_l3_to_l3_ns_x100: u32,
    pub raw_dram_lat_ns_x100: u32,
    pub raw_l3_bw_mib_s_x100: u32,
    pub raw_miss_ratio_ppm: u32,
    pub raw_l3_req_per_s: u64,
    pub raw_l3_miss_per_s: u64,
    pub metric: FilteredMetric,
}

#[derive(Clone, Copy, Debug)]
pub struct DfSampleUpdate {
    pub domain_id: u32,
    pub ccx_id: u32,
    pub ccm_id: u32,
    pub raw_read_bw_mib_s_x100: u32,
    pub raw_write_bw_mib_s_x100: u32,
    pub metric: FilteredMetric,
}

#[derive(Clone, Copy, Debug)]
pub struct PolicyConfig {
    pub l2_need_mib_s_x100: u32,
    pub migrate_margin_x100: u32,
    pub cpu_high_util_x100: u32,
    pub cpu_rebalance_job_delta: u32,
    pub stall_victim_min_pct_x100: u32,
    pub stall_victim_delta_pct_x100: u32,
    pub llc_stale_ms: u64,
    pub df_stale_ms: u64,
    pub migrate_settle_ms: u64,
    pub signature_snapshots: bool,
    pub cs_villain_throttle: bool,
    pub tick_reeval_every: u32,
    pub tick_defer_max: u32,
    pub cs_villain_reslice_ns: u64,
    pub cs_villain_refill_divisor: u64,
    pub cs_villain_settle_ns: u64,
    pub cs_villain_release_samples: u32,
    pub tick_move_phase_mod: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DispatchSnapshot {
    pub captured_at_ns: u64,
    pub selected_domain: Option<u32>,
    pub selected_cpu: Option<u32>,
    pub df_raw_bw_mib_s_x100: [u32; MAX_DOMAINS],
    pub valid_domains: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ThreadSignature {
    pub valid: bool,
    pub stable: bool,
    pub sample_count: u32,
    pub last_update_ns: u64,
    pub confidence_x100: u32,
    pub fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    pub fill_share_x100: [u32; MEM_SOURCE_COUNT],
    pub projected_df_pressure_x100: u32,
    pub projected_llc_pressure_x100: u32,
    pub df_domain_delta_x100: [u32; MAX_DOMAINS],
    pub raw_df_domain_delta_x100: [u32; MAX_DOMAINS],
}

#[derive(Clone, Debug, Default)]
pub struct DomainSignatureAggregate {
    pub df_pressure_x100: u32,
    pub llc_pressure_x100: u32,
    pub fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    pub contributor_count: u32,
    pub stable_count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PlannedDomainState {
    pub df_pressure_x100: u32,
    pub llc_pressure_x100: u32,
    pub fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    pub contributor_count: u32,
    pub stable_count: u32,
    pub task_count: u32,
    pub incoming_migrations: u32,
    pub outgoing_migrations: u32,
    pub migration_budget: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PlannedCpuState {
    pub jobs: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct ManagedThreadState {
    pub tid: u32,
    pub tgid: u32,
    pub comm: String,
    pub last_seen_ns: u64,
    pub last_class: ThreadClass,
    pub last_sync_qualified: bool,
    pub last_selected_domain: Option<u32>,
    pub last_selected_cpu: Option<u32>,
    pub last_observed_domain: Option<u32>,
    pub last_observed_cpu: Option<u32>,
    pub settle_until_ns: u64,
    pub last_migration_from_domain: Option<u32>,
    pub last_migration_to_domain: Option<u32>,
    pub last_migration_at_ns: u64,
    pub reverse_protect_until_ns: u64,
    pub dispatch_snapshot: Option<DispatchSnapshot>,
    pub signature: ThreadSignature,
    pub villain_dispatch_snapshot: Option<DispatchSnapshot>,
    pub io_cs_signature: ThreadSignature,
    pub last_l2_bw_mib_s_x100: u32,
    pub ewma_l2_bw_mib_s_x100: u32,
    pub last_fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    pub ewma_fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    pub last_ipc_x1000: u32,
    pub ewma_ipc_x1000: u32,
    pub last_stall_pct_x100: u32,
    pub ewma_stall_pct_x100: u32,
    pub last_stall_delta_pct_x100: u32,
    pub last_tick_seen_ns: u64,
    pub tick_stay_backoff_exp: u32,
    pub throttle_link_id: Option<u32>,
    pub throttle_tokens_ns: u64,
    pub throttle_last_refill_ns: u64,
    pub throttle_defer_started_ns: u64,
    pub consecutive_tick_defer: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionReason {
    IdleSpread,
    StayCurrentDomain,
    MoveCoolerDomain,
    ExcludedDomainEscape,
    NoCandidate,
    MarginNotMet,
    MigrationBudget,
    MovePhaseGate,
    ReverseHysteresis,
    DestinationGuard,
}

impl DecisionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdleSpread => "idle_spread",
            Self::StayCurrentDomain => "stay_current_domain",
            Self::MoveCoolerDomain => "move_cooler_domain",
            Self::ExcludedDomainEscape => "excluded_domain_escape",
            Self::NoCandidate => "no_candidate",
            Self::MarginNotMet => "margin_not_met",
            Self::MigrationBudget => "migration_budget",
            Self::MovePhaseGate => "move_phase_gate",
            Self::ReverseHysteresis => "reverse_hysteresis",
            Self::DestinationGuard => "destination_guard",
        }
    }
}

#[cfg(feature = "diagnostics")]
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    pub ts_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    pub comm: String,
    pub source_cpu: Option<u32>,
    pub source_domain: Option<u32>,
    pub selected_cpu: Option<u32>,
    pub selected_domain: Option<u32>,
    pub class: ThreadClass,
    pub current_source_df_x100: u32,
    pub predicted_source_df_x100: u32,
    pub current_source_llc_x100: u32,
    pub predicted_source_llc_x100: u32,
    pub current_destination_df_x100: u32,
    pub predicted_destination_df_x100: u32,
    pub current_destination_llc_x100: u32,
    pub predicted_destination_llc_x100: u32,
    pub trigger: QueueTrigger,
    pub tick_seq: u64,
    pub tick_decision: TickDecision,
    pub fill_total_bw_mib_s_x100: u32,
    pub last_ipc_x1000: u32,
    pub ewma_ipc_x1000: u32,
    pub last_stall_pct_x100: u32,
    pub ewma_stall_pct_x100: u32,
    pub stall_delta_pct_x100: u32,
    pub slice_ns: u64,
    pub villain_score: u32,
    pub signature_valid: bool,
    pub signature_sample_count: u32,
    pub signature_confidence_x100: u32,
    pub signature_stable: bool,
    pub io_cs_signature_valid: bool,
    pub io_cs_signature_sample_count: u32,
    pub io_cs_signature_confidence_x100: u32,
    pub io_cs_signature_stable: bool,
    pub io_cs_top_link: Option<u32>,
    pub io_cs_top_contrib_x100: u32,
    pub io_cs_raw_top_link: Option<u32>,
    pub io_cs_raw_top_contrib_x100: u32,
    pub live_source_df_x100: u32,
    pub observed_candidate_domain: Option<u32>,
    pub observed_candidate_cpu: Option<u32>,
    pub observed_destination_live_df_x100: u32,
    pub baseline_total_overload_x100: u32,
    pub candidate_total_overload_x100: u32,
    pub baseline_max_overload_x100: u32,
    pub candidate_max_overload_x100: u32,
    pub baseline_pair_penalty: u64,
    pub candidate_pair_penalty: u64,
    pub baseline_combined_cost: u64,
    pub candidate_combined_cost: u64,
    pub considered_candidates: u32,
    pub skipped_missing_df: u32,
    pub skipped_missing_llc: u32,
    pub skipped_no_cpu: u32,
    pub skipped_same_domain: u32,
    pub skipped_budget: u32,
    pub blocked_destination: u32,
    pub blocked_phase: u32,
    pub blocked_reverse: u32,
    pub planner_epoch: u64,
    pub plan_revision: u64,
    pub plan_used: bool,
    pub fallback_reason: String,
    pub planned_domain: Option<u32>,
    pub planned_cpu: Option<u32>,
    pub sync_group_id: Option<u32>,
    pub sync_anchor_domain: Option<u32>,
    pub sync_override: bool,
    pub control_plane_cpu_policy: String,
    pub control_plane_cpu: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub select_cpu_in_domain_debug_enabled: bool,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub selected_cpu_planned_jobs: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub selected_cpu_idle: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub selected_cpu_dsq_depth: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub selected_cpu_util_x100: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub selected_cpu_current_tid: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub best_idle_cpu_in_domain: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub best_idle_cpu_planned_jobs: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub min_planned_jobs_in_domain: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub target_cpu_prepass: Option<u32>,
    #[cfg(feature = "select-cpu-in-domain-debug")]
    pub reserved_cpu_override_used: bool,
    pub reason: DecisionReason,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TickStatsSnapshot {
    pub tick_events: u64,
    pub tick_to_userspace: u64,
    pub tick_backoff_skip: u64,
    pub tick_fastpath_stay: u64,
    pub tick_reslice: u64,
    pub reenqueue_local_fail: u64,
    pub villain_reslices: u64,
    pub user_dispatches: u64,
    pub kernel_dispatches: u64,
    pub cancel_dispatches: u64,
    pub bounce_dispatches: u64,
    pub failed_dispatches: u64,
    pub sched_congested: u64,
    pub dispatched_ringbuf_drains: u64,
    pub dispatch_task_missing: u64,
    pub dispatch_cpu_inserts: u64,
    pub dispatch_shared_inserts: u64,
    pub dispatch_cpu_kicks: u64,
    pub dispatch_cpu_consumes: u64,
    pub dispatch_shared_consumes: u64,
    pub dispatch_sched_consumes: u64,
    pub stale_dispatch_rescues: u64,
}

pub fn comm_to_string(raw: &[i8; COMM_LEN]) -> String {
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(COMM_LEN);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 || ts.tv_sec < 0 || ts.tv_nsec < 0 {
        return 0;
    }

    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

pub fn pct_to_x100(v: u32) -> u32 {
    v.saturating_mul(100)
}

pub fn cpus_to_cpulist(cpus: &BTreeSet<u32>) -> String {
    let mut out = Vec::new();
    let mut iter = cpus.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while let Some(next) = iter.peek().copied() {
            if next == end + 1 {
                end = next;
                iter.next();
            } else {
                break;
            }
        }
        if start == end {
            out.push(start.to_string());
        } else {
            out.push(format!("{start}-{end}"));
        }
    }
    out.join(",")
}
