use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct OverheadProfileConfig {
    pub path: String,
    pub interval: Duration,
    pub alloc_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadClockSample {
    wall_ns: u64,
    cpu_ns: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlannerMemorySample {
    pub estimated_bytes: u64,
    pub threads_len: usize,
    pub pending_tids_len: usize,
    pub allowed_cpus_entry_count: usize,
    pub allowed_cpus_total_members: usize,
    pub allowed_domains_entry_count: usize,
    pub allowed_domains_total_members: usize,
    pub previous_plan_entries_len: usize,
    pub vmrss_kib: u64,
    pub vmhwm_kib: u64,
}

#[derive(Debug, Default)]
struct ScopeStats {
    count: u64,
    error_count: u64,
    wall_ns_total: u128,
    cpu_ns_total: u128,
    wall_ns_max: u64,
    cpu_ns_max: u64,
    wall_samples: Vec<u64>,
    cpu_samples: Vec<u64>,
}

#[derive(Debug, Default)]
struct MemoryStats {
    count: u64,
    estimated_bytes_total: u128,
    estimated_bytes_max: u64,
    vmrss_kib_max: u64,
    vmhwm_kib_max: u64,
    last: PlannerMemorySample,
}

#[derive(Debug)]
struct OverheadProfileState {
    writer: BufWriter<File>,
    interval: Duration,
    alloc_enabled: bool,
    last_flush: Instant,
    scopes: BTreeMap<&'static str, ScopeStats>,
    counters: BTreeMap<&'static str, u64>,
    planner_memory: MemoryStats,
}

#[derive(Debug)]
pub struct OverheadProfiler {
    state: Mutex<OverheadProfileState>,
}

static GLOBAL_PROFILER: OnceLock<Mutex<Option<Arc<OverheadProfiler>>>> = OnceLock::new();

pub fn init_global(config: Option<OverheadProfileConfig>) -> Result<()> {
    let global = GLOBAL_PROFILER.get_or_init(|| Mutex::new(None));
    let mut guard = global.lock().unwrap();
    *guard = match config {
        Some(config) => Some(Arc::new(OverheadProfiler::new(config)?)),
        None => None,
    };
    Ok(())
}

pub fn global() -> Option<Arc<OverheadProfiler>> {
    GLOBAL_PROFILER
        .get()
        .and_then(|state| state.lock().ok().and_then(|guard| guard.as_ref().cloned()))
}

pub fn flush_global(force: bool) {
    if let Some(profiler) = global() {
        profiler.flush(force);
    }
}

pub fn shutdown_global() {
    flush_global(true);
    if let Some(state) = GLOBAL_PROFILER.get() {
        let mut guard = state.lock().unwrap();
        *guard = None;
    }
}

impl ThreadClockSample {
    pub fn capture() -> Self {
        Self {
            wall_ns: clock_ns(libc::CLOCK_MONOTONIC_RAW),
            cpu_ns: clock_ns(libc::CLOCK_THREAD_CPUTIME_ID),
        }
    }
}

impl OverheadProfiler {
    fn new(config: OverheadProfileConfig) -> Result<Self> {
        let writer = BufWriter::new(File::create(Path::new(&config.path)).with_context(|| {
            format!("failed to create overhead profile log at {}", config.path)
        })?);
        Ok(Self {
            state: Mutex::new(OverheadProfileState {
                writer,
                interval: config.interval,
                alloc_enabled: config.alloc_enabled,
                last_flush: Instant::now(),
                scopes: BTreeMap::new(),
                counters: BTreeMap::new(),
                planner_memory: MemoryStats::default(),
            }),
        })
    }

    pub fn alloc_enabled(&self) -> bool {
        self.state.lock().unwrap().alloc_enabled
    }

    pub fn record_scope(&self, scope: &'static str, start: ThreadClockSample, success: bool) {
        let end = ThreadClockSample::capture();
        let wall_ns = end.wall_ns.saturating_sub(start.wall_ns);
        let cpu_ns = end.cpu_ns.saturating_sub(start.cpu_ns);
        self.record_duration(scope, wall_ns, cpu_ns, success);
    }

    pub fn record_duration(&self, scope: &'static str, wall_ns: u64, cpu_ns: u64, success: bool) {
        let mut state = self.state.lock().unwrap();
        let stats = state.scopes.entry(scope).or_default();
        stats.count = stats.count.saturating_add(1);
        if !success {
            stats.error_count = stats.error_count.saturating_add(1);
        }
        stats.wall_ns_total = stats.wall_ns_total.saturating_add(wall_ns as u128);
        stats.cpu_ns_total = stats.cpu_ns_total.saturating_add(cpu_ns as u128);
        stats.wall_ns_max = stats.wall_ns_max.max(wall_ns);
        stats.cpu_ns_max = stats.cpu_ns_max.max(cpu_ns);
        stats.wall_samples.push(wall_ns);
        stats.cpu_samples.push(cpu_ns);
        self.flush_locked(&mut state, false);
    }

    pub fn increment_counter(&self, scope: &'static str) {
        self.add_counter(scope, 1);
    }

    pub fn add_counter(&self, scope: &'static str, value: u64) {
        let mut state = self.state.lock().unwrap();
        let counter = state.counters.entry(scope).or_default();
        *counter = counter.saturating_add(value);
        self.flush_locked(&mut state, false);
    }

    pub fn record_planner_memory(&self, sample: PlannerMemorySample) {
        let mut state = self.state.lock().unwrap();
        if !state.alloc_enabled {
            return;
        }
        let stats = &mut state.planner_memory;
        stats.count = stats.count.saturating_add(1);
        stats.estimated_bytes_total = stats
            .estimated_bytes_total
            .saturating_add(sample.estimated_bytes as u128);
        stats.estimated_bytes_max = stats.estimated_bytes_max.max(sample.estimated_bytes);
        stats.vmrss_kib_max = stats.vmrss_kib_max.max(sample.vmrss_kib);
        stats.vmhwm_kib_max = stats.vmhwm_kib_max.max(sample.vmhwm_kib);
        stats.last = sample;
        self.flush_locked(&mut state, false);
    }

    pub fn flush(&self, force: bool) {
        let mut state = self.state.lock().unwrap();
        self.flush_locked(&mut state, force);
    }

    fn flush_locked(&self, state: &mut OverheadProfileState, force: bool) {
        if !force && state.last_flush.elapsed() < state.interval {
            return;
        }

        let ts_ns = clock_ns(libc::CLOCK_MONOTONIC_RAW);
        for (scope, stats) in &state.scopes {
            let wall = quantiles(&stats.wall_samples);
            let cpu = quantiles(&stats.cpu_samples);
            let wall_avg = avg(stats.wall_ns_total, stats.count);
            let cpu_avg = avg(stats.cpu_ns_total, stats.count);
            let _ = writeln!(
                state.writer,
                "ts_ns={} kind=timing scope={} count={} error_count={} wall_ns_total={} wall_ns_avg={} wall_ns_max={} wall_p50_ns={} wall_p95_ns={} wall_p99_ns={} cpu_ns_total={} cpu_ns_avg={} cpu_ns_max={} cpu_p50_ns={} cpu_p95_ns={} cpu_p99_ns={}",
                ts_ns,
                scope,
                stats.count,
                stats.error_count,
                stats.wall_ns_total,
                wall_avg,
                stats.wall_ns_max,
                wall.0,
                wall.1,
                wall.2,
                stats.cpu_ns_total,
                cpu_avg,
                stats.cpu_ns_max,
                cpu.0,
                cpu.1,
                cpu.2,
            );
        }

        for (scope, count) in &state.counters {
            let _ = writeln!(
                state.writer,
                "ts_ns={} kind=counter scope={} count={}",
                ts_ns, scope, count
            );
        }

        if state.alloc_enabled && state.planner_memory.count > 0 {
            let stats = &state.planner_memory;
            let estimated_avg = avg(stats.estimated_bytes_total, stats.count);
            let last = stats.last;
            let _ = writeln!(
                state.writer,
                "ts_ns={} kind=memory scope=planner_input_mem count={} estimated_bytes_last={} estimated_bytes_avg={} estimated_bytes_max={} threads_len_last={} pending_tids_len_last={} allowed_cpus_entry_count_last={} allowed_cpus_total_members_last={} allowed_domains_entry_count_last={} allowed_domains_total_members_last={} previous_plan_entries_len_last={} vmrss_kib_last={} vmrss_kib_max={} vmhwm_kib_last={} vmhwm_kib_max={}",
                ts_ns,
                stats.count,
                last.estimated_bytes,
                estimated_avg,
                stats.estimated_bytes_max,
                last.threads_len,
                last.pending_tids_len,
                last.allowed_cpus_entry_count,
                last.allowed_cpus_total_members,
                last.allowed_domains_entry_count,
                last.allowed_domains_total_members,
                last.previous_plan_entries_len,
                last.vmrss_kib,
                stats.vmrss_kib_max,
                last.vmhwm_kib,
                stats.vmhwm_kib_max,
            );
        }

        let _ = state.writer.flush();
        state.last_flush = Instant::now();
    }
}

pub fn process_memory_kib() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut vmrss = None;
    let mut vmhwm = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            vmrss = parse_status_kib(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            vmhwm = parse_status_kib(rest);
        }
    }
    Some((vmrss.unwrap_or(0), vmhwm.unwrap_or(0)))
}

fn parse_status_kib(raw: &str) -> Option<u64> {
    raw.split_whitespace().next()?.parse().ok()
}

fn clock_ns(clock_id: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(clock_id, &mut ts) };
    if rc != 0 || ts.tv_sec < 0 || ts.tv_nsec < 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

fn avg(total: u128, count: u64) -> u128 {
    if count == 0 {
        0
    } else {
        total / count as u128
    }
}

fn quantiles(values: &[u64]) -> (u64, u64, u64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    (
        percentile(&sorted, 50),
        percentile(&sorted, 95),
        percentile(&sorted, 99),
    )
}

fn percentile(sorted: &[u64], pct: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let len = sorted.len();
    let rank = ((len.saturating_sub(1)) as u128 * pct as u128) / 100;
    sorted[rank as usize]
}
