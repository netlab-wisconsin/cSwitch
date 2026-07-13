use crate::bpf::{BpfScheduler, DispatchedTask, QueuedTask, RL_CPU_ANY};
use crate::cli::{ControlPlaneCpuPolicy, Opts};
use crate::cpu_util::CpuUtilTracker;
use crate::df_ccm_sampler::{start as start_df_sampler, DfSamplerConfig, DfSamplerEvent};
use crate::df_cs_sampler::{start as start_df_cs_sampler, DfSamplerConfig as DfCsSamplerConfig};
use crate::filter::{is_stale, FilterConfig};
use crate::host_map::{
    apply_host_cpu_filters, build_mapping_from_host_mapping, eligible_cpulist, parse_host_mapping,
};
use crate::llc_sampler::{start as start_llc_sampler, LlcSamplerConfig};
use crate::overhead_profile::{
    self, OverheadProfileConfig, PlannerMemorySample, ThreadClockSample,
};
use crate::planner::{
    maybe_dedicated_control_plane_cpu, start_worker as start_planner_worker,
    sync_qualified_for_planner, PlacementPlan, PlacementPlanEntry, PlannerConfig, PlannerInput,
    PlannerRequest, PlannerTrigger,
};
#[cfg(all(feature = "diagnostics", feature = "select-cpu-in-domain-debug"))]
use crate::policy::planned_jobs_len;
use crate::policy::{
    choose_cpu_in_domain, choose_idle_cpu_in_domain, choose_placement_with_planned_cpus,
    classify_task, current_acceptable_cpu_in_domain, current_cpu_in_domain,
    current_idle_cpu_in_domain, planned_cpu_available_for_task, preferred_cpu_in_domain,
    should_rebalance_to_idle_sibling, task_signature_effect, PlacementDebugInfo, PlacementDecision,
};
use crate::runtime_log;
use crate::topology;
use crate::types::{
    now_ns, pct_to_x100, CcmDfStateValue, CpuStateValue, DecisionReason, DispatchSnapshot,
    LlcStateValue, ManagedThreadState, MappingInfo, PlannedCpuState, PlannedDomainState,
    PolicyConfig, QueueTrigger, ThreadClass, ThreadSignature, TickDecision, TopologyLayout,
    DEFAULT_SLICE_US, MAX_DOMAINS, MEM_SOURCE_COUNT, MEM_SOURCE_DRAM_NEAR, MEM_SOURCE_NEAR_CACHE,
    MIN_VALID_RUN_NS, SIGNATURE_CONFIDENCE_THRESHOLD_X100, SIGNATURE_STABLE_SAMPLES,
};
#[cfg(test)]
use crate::villain_control::{
    apply_villain_token_throttle, choose_link_villains, maybe_apply_villain_token_throttle,
    reset_villain_throttle, VILLAIN_DEFER_UPPER_BOUND_NS,
};
use crate::villain_control::{
    choose_link_villains_with_fallbacks, live_df_overload_x100_with_capacity,
    CsVillainLatchController, CsVillainLatchEvent, CsVillainPressurePolicy, LinkContender,
    TokenBucketPressurePolicy, VillainThrottleAction,
};
#[cfg(feature = "light-compete")]
use crate::workload::move_pid_to_cgroup;
#[cfg(feature = "light-compete")]
use crate::workload::set_sched_ext;
use crate::workload::{
    ensure_cgroup_dir, ensure_current_thread_sched_other, launch_shell_workload, launch_workload,
    restrict_cgroup_cpus, shutdown_child_gracefully, CgroupGuard, WorkloadAdopter,
};
use anyhow::{Context, Result};
use clap::Parser;
use crossbeam_channel::{unbounded, Receiver};
use libbpf_rs::{MapCore as _, MapFlags, OpenObject};
use scx_utils::{init_libbpf_logging, perf, try_set_rlimit_infinity};
use simplelog::{ColorChoice, ConfigBuilder, LevelFilter, TermLogger, TerminalMode};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem::{size_of, size_of_val, MaybeUninit};
#[cfg(any(feature = "light-compete", feature = "stall-filler-spinner"))]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(any(feature = "light-compete", feature = "stall-filler-spinner"))]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "diagnostics")]
use crate::decision_log::DecisionLogger;
#[cfg(feature = "diagnostics")]
use crate::monitor;
#[cfg(feature = "diagnostics")]
use crate::policy::build_record;
#[cfg(feature = "diagnostics")]
use crate::types::{DecisionRecord, TickStatsSnapshot};

pub(crate) fn setup_logging(verbose: bool) -> Result<()> {
    let mut cfg = ConfigBuilder::new();
    let _ = cfg.set_time_offset_to_local();
    cfg.set_time_level(LevelFilter::Error)
        .set_location_level(LevelFilter::Off)
        .set_target_level(LevelFilter::Off)
        .set_thread_level(LevelFilter::Off);
    TermLogger::init(
        if verbose {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        },
        cfg.build(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    )?;
    init_libbpf_logging(None);
    Ok(())
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast(), size_of::<T>()) }
}

fn key_bytes(key: &u32) -> &[u8] {
    as_bytes(key)
}

const ROUND_MIGRATION_BUDGET_MAX: u32 = 4;
const REVERSE_HYSTERESIS_MULTIPLIER: u64 = 1;

fn round_migration_budget(task_count: u32) -> u32 {
    task_count
        .saturating_add(3)
        .div_ceil(4)
        .clamp(1, ROUND_MIGRATION_BUDGET_MAX)
}

fn thread_domain(thread: &ManagedThreadState) -> Option<u32> {
    thread.last_selected_domain.or(thread.last_observed_domain)
}

fn thread_cpu(thread: &ManagedThreadState) -> Option<u32> {
    thread.last_selected_cpu.or(thread.last_observed_cpu)
}

fn task_current_domain(task: &QueuedTask) -> Option<u32> {
    u32::try_from(task.current_domain).ok()
}

fn task_current_cpu(task: &QueuedTask) -> Option<u32> {
    u32::try_from(task.current_cpu).ok()
}

fn thread_signature_valid(thread: &ManagedThreadState) -> bool {
    thread.signature.valid
}

fn thread_signature_ccm_bw(thread: &ManagedThreadState) -> u32 {
    if !thread_signature_valid(thread) {
        return 0;
    }
    let fill_sum = thread.signature.fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE]
        .saturating_add(thread.signature.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR]);
    if fill_sum > 0 {
        fill_sum
    } else {
        thread.signature.projected_df_pressure_x100
    }
}

fn thread_signature_llc_pressure(thread: &ManagedThreadState) -> u32 {
    if thread_signature_valid(thread) {
        thread.signature.projected_llc_pressure_x100
    } else {
        0
    }
}

fn task_stall_delta_pct_x100(task: &QueuedTask) -> u32 {
    task.last_stall_pct_x100
        .saturating_sub(task.ewma_stall_pct_x100)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StallSignalKind {
    Delta,
    Absolute,
}

impl StallSignalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delta => "delta",
            Self::Absolute => "absolute",
        }
    }

    fn allows_fallback_villain(self) -> bool {
        matches!(self, Self::Delta)
    }
}

#[derive(Clone, Copy, Debug)]
struct StallSignal {
    kind: StallSignalKind,
    live_overload_x100: u32,
}

fn fmt_x100(value: u32) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

fn log_stall_signal(task: &QueuedTask, link_id: u32, overload_x100: u32, kind: StallSignalKind) {
    let comm = crate::types::comm_to_string(&task.comm);
    runtime_log::line(format!(
        "stall_signal tid={} tgid={} comm={} link={} src_domain={} src_cpu={} trigger={} signal={} live_overload_mib_s={} last_stall_pct={} ewma_stall_pct={} stall_delta_pct={}",
        task.tid,
        task.tgid,
        comm,
        link_id,
        task.current_domain,
        task.current_cpu,
        QueueTrigger::from_u32(task.trigger).as_str(),
        kind.as_str(),
        fmt_x100(overload_x100),
        fmt_x100(task.last_stall_pct_x100),
        fmt_x100(task.ewma_stall_pct_x100),
        fmt_x100(task_stall_delta_pct_x100(task)),
    ));
}

fn log_stall_control(
    task: &QueuedTask,
    link_id: u32,
    score: u32,
    action: &'static str,
    slice_ns: u64,
    defer_count: u32,
    tokens_ns: u64,
    token_capacity_ns: u64,
) {
    let comm = crate::types::comm_to_string(&task.comm);
    runtime_log::line(format!(
        "stall_control tid={} tgid={} comm={} link={} src_domain={} action={} trigger={} score={} slice_ns={} consecutive_tick_defer={} tokens_ns={} token_capacity_ns={} src_cpu={}",
        task.tid,
        task.tgid,
        comm,
        link_id,
        task.current_domain,
        action,
        QueueTrigger::from_u32(task.trigger).as_str(),
        score,
        slice_ns,
        defer_count,
        tokens_ns,
        token_capacity_ns,
        task.current_cpu,
    ));
}

fn log_stall_latch(event: CsVillainLatchEvent) {
    runtime_log::line(format!(
        "stall_latch action={} link={} tid={} score={} live_overload_mib_s={} sample_ts_ns={} hold_until_ns={} clear_samples={} pressure_epoch={}",
        event.action.as_str(),
        event.link_id,
        event.tid,
        event.score,
        fmt_x100(event.overload_x100),
        event.sample_ts_ns,
        event.hold_until_ns,
        event.clear_samples,
        event.pressure_epoch,
    ));
}

#[cfg(test)]
fn is_suspicious_stall_victim(
    task: &QueuedTask,
    link_id: u32,
    mapping: &MappingInfo,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    cfg: PolicyConfig,
) -> bool {
    stall_signal_for_link(task, link_id, mapping, df_cs_states, now_ns, cfg).is_some()
}

fn stall_signal_for_link(
    task: &QueuedTask,
    link_id: u32,
    mapping: &MappingInfo,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    cfg: PolicyConfig,
) -> Option<StallSignal> {
    let live_overload_x100 = live_df_overload_x100_with_capacity(
        df_cs_states,
        link_id as usize,
        mapping.cs_capacity_mib_s_x100(link_id),
        now_ns,
        cfg,
    );
    if live_overload_x100 == 0 || task.last_update_ns == 0 {
        return None;
    }
    if task_stall_delta_pct_x100(task) >= cfg.stall_victim_delta_pct_x100 {
        return Some(StallSignal {
            kind: StallSignalKind::Delta,
            live_overload_x100,
        });
    }
    if task.last_stall_pct_x100 >= cfg.stall_victim_min_pct_x100 {
        return Some(StallSignal {
            kind: StallSignalKind::Absolute,
            live_overload_x100,
        });
    }
    None
}

#[cfg(test)]
fn suspicious_stall_victim_links(
    task: &QueuedTask,
    mapping: &MappingInfo,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    cfg: PolicyConfig,
) -> BTreeSet<u32> {
    let mut links = BTreeSet::new();
    for link_id in 0..df_cs_states.len().min(MAX_DOMAINS) {
        let link_id = link_id as u32;
        if stall_signal_for_link(task, link_id, mapping, df_cs_states, now_ns, cfg).is_some() {
            links.insert(link_id);
        }
    }
    links
}

pub(crate) fn build_planned_thread_domains(
    threads: &BTreeMap<u32, ManagedThreadState>,
    pending: &VecDeque<QueuedTask>,
) -> BTreeMap<u32, u32> {
    let mut planned_domains = BTreeMap::new();
    for (tid, thread) in threads {
        if let Some(domain) = thread_domain(thread) {
            planned_domains.insert(*tid, domain);
        }
    }
    for task in pending {
        if let Ok(domain) = u32::try_from(task.current_domain) {
            planned_domains.insert(task.tid as u32, domain);
        }
    }
    planned_domains
}

pub(crate) fn build_planned_thread_cpus(
    threads: &BTreeMap<u32, ManagedThreadState>,
    pending: &VecDeque<QueuedTask>,
) -> BTreeMap<u32, u32> {
    let mut planned_cpus = BTreeMap::new();
    for (tid, thread) in threads {
        if let Some(cpu) = thread_cpu(thread) {
            planned_cpus.insert(*tid, cpu);
        }
    }
    for task in pending {
        let tid = task.tid as u32;
        if planned_cpus.contains_key(&tid) {
            continue;
        }
        if let Ok(cpu) = u32::try_from(task.current_cpu) {
            planned_cpus.insert(tid, cpu);
        }
    }
    planned_cpus
}

// Mutable reservation ledger for one dispatch burst.
//
// This is intentionally not a simultaneous global plan. The default dispatcher
// makes greedy decisions in pending-queue order and commits each successful
// decision here so later tasks see the domain/CPU slots already reserved in the
// same burst.
pub(crate) struct DispatchBatchReservations {
    domain_by_tid: BTreeMap<u32, u32>,
    cpu_by_tid: BTreeMap<u32, u32>,
    domain_states: Vec<PlannedDomainState>,
    cpu_states: Vec<PlannedCpuState>,
}

impl DispatchBatchReservations {
    pub(crate) fn from_snapshot(
        topo: &TopologyLayout,
        threads: &BTreeMap<u32, ManagedThreadState>,
        pending: &VecDeque<QueuedTask>,
        initial_domain_targets: &BTreeMap<u32, u32>,
        initial_cpu_targets: &BTreeMap<u32, u32>,
    ) -> Self {
        let mut domain_by_tid = build_planned_thread_domains(threads, pending);
        for (&tid, &target_domain) in initial_domain_targets {
            domain_by_tid.insert(tid, target_domain);
        }
        let mut cpu_by_tid = build_planned_thread_cpus(threads, pending);
        for (&tid, &target_cpu) in initial_cpu_targets {
            cpu_by_tid.insert(tid, target_cpu);
        }
        let domain_states = build_planned_domain_states(topo, threads, pending, &domain_by_tid);
        let cpu_states = build_planned_cpu_states(topo.nr_cpu_ids, &cpu_by_tid);
        Self {
            domain_by_tid,
            cpu_by_tid,
            domain_states,
            cpu_states,
        }
    }

    pub(crate) fn domain_states(&self) -> &[PlannedDomainState] {
        &self.domain_states
    }

    pub(crate) fn cpu_states(&self) -> &[PlannedCpuState] {
        &self.cpu_states
    }

    fn reserve_burst_target_cpus(
        &mut self,
        pending: &VecDeque<QueuedTask>,
        topo: &TopologyLayout,
        mapping: &MappingInfo,
        cpu_states: &[CpuStateValue],
        cpu_utils_x100: &[u32],
        allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
        fixed_cpu_targets: &BTreeMap<u32, u32>,
        cfg: PolicyConfig,
    ) -> BTreeMap<u32, u32> {
        build_burst_target_cpu_plan(
            pending,
            &self.domain_by_tid,
            topo,
            mapping,
            cpu_states,
            cpu_utils_x100,
            &mut self.cpu_states,
            &mut self.cpu_by_tid,
            allowed_cpus_by_tid,
            fixed_cpu_targets,
            cfg,
        )
    }

    pub(crate) fn commit_domain_decision(
        &mut self,
        task: &QueuedTask,
        meta: Option<&ManagedThreadState>,
        mapping: &MappingInfo,
        selected_domain: u32,
    ) {
        apply_planned_migration(
            &mut self.domain_states,
            &mut self.domain_by_tid,
            task,
            meta,
            mapping,
            selected_domain,
        );
    }

    pub(crate) fn commit_cpu_decision(&mut self, task: &QueuedTask, selected_cpu: u32) {
        apply_planned_cpu_assignment(
            &mut self.cpu_states,
            &mut self.cpu_by_tid,
            task,
            selected_cpu,
        );
    }
}

fn collect_allowed_cpus_for_pending(
    pending: &VecDeque<QueuedTask>,
    affinity: &mut AffinityCache,
    now: u64,
) -> BTreeMap<u32, BTreeSet<u32>> {
    let mut allowed_cpus_by_tid = BTreeMap::new();
    for task in pending {
        let tid = task.tid as u32;
        if allowed_cpus_by_tid.contains_key(&tid) {
            continue;
        }
        if let Ok(cpus) = affinity.allowed_cpus(tid, now) {
            if !cpus.is_empty() {
                allowed_cpus_by_tid.insert(tid, cpus);
            }
        }
    }
    allowed_cpus_by_tid
}

pub(crate) fn build_planned_domain_states(
    topo: &TopologyLayout,
    threads: &BTreeMap<u32, ManagedThreadState>,
    pending: &VecDeque<QueuedTask>,
    planned_domains: &BTreeMap<u32, u32>,
) -> Vec<PlannedDomainState> {
    let mut states = vec![PlannedDomainState::default(); topo.domains.len()];
    for (tid, thread) in threads {
        let Some(domain) = planned_domains
            .get(tid)
            .copied()
            .or_else(|| thread_domain(thread))
        else {
            continue;
        };
        let Some(state) = states.get_mut(domain as usize) else {
            continue;
        };
        state.task_count = state.task_count.saturating_add(1);
        if !thread_signature_valid(thread) {
            continue;
        }
        state.df_pressure_x100 = state
            .df_pressure_x100
            .saturating_add(thread_signature_ccm_bw(thread));
        state.llc_pressure_x100 = state
            .llc_pressure_x100
            .saturating_add(thread_signature_llc_pressure(thread));
        for idx in 0..MEM_SOURCE_COUNT {
            state.fill_bw_mib_s_x100[idx] = state.fill_bw_mib_s_x100[idx]
                .saturating_add(thread.signature.fill_bw_mib_s_x100[idx]);
        }
        state.contributor_count = state.contributor_count.saturating_add(1);
        if thread.signature.stable {
            state.stable_count = state.stable_count.saturating_add(1);
        }
    }
    let known_threads = threads.keys().copied().collect::<BTreeSet<_>>();
    for task in pending {
        let tid = task.tid as u32;
        if known_threads.contains(&tid) {
            continue;
        }
        let Ok(domain) = u32::try_from(task.current_domain) else {
            continue;
        };
        if let Some(state) = states.get_mut(domain as usize) {
            state.task_count = state.task_count.saturating_add(1);
        }
    }
    for state in &mut states {
        state.migration_budget = round_migration_budget(state.task_count.max(1));
    }
    states
}

fn candidate_cpus_for_target_domain(
    allowed_cpus: &BTreeSet<u32>,
    target_domain: u32,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
) -> BTreeSet<u32> {
    let eligible = allowed_eligible_cpus(allowed_cpus, mapping)
        .into_iter()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(target_domain)
        })
        .collect::<BTreeSet<_>>();
    if !eligible.is_empty() {
        return eligible;
    }
    allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(target_domain)
        })
        .collect()
}

pub(crate) fn build_planned_cpu_states(
    nr_cpu_ids: usize,
    planned_cpus: &BTreeMap<u32, u32>,
) -> Vec<PlannedCpuState> {
    let mut states = vec![PlannedCpuState::default(); nr_cpu_ids];
    for (&tid, &cpu) in planned_cpus {
        let Some(state) = states.get_mut(cpu as usize) else {
            continue;
        };
        state.jobs.push(tid);
    }
    states
}

fn build_burst_target_cpu_plan(
    pending: &VecDeque<QueuedTask>,
    planned_domains: &BTreeMap<u32, u32>,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &mut [PlannedCpuState],
    planned_cpus: &mut BTreeMap<u32, u32>,
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    fixed_cpu_targets: &BTreeMap<u32, u32>,
    cfg: PolicyConfig,
) -> BTreeMap<u32, u32> {
    let mut candidates = Vec::new();
    let mut seen_tids = BTreeSet::new();
    for (index, task) in pending.iter().enumerate() {
        let tid = task.tid as u32;
        if !seen_tids.insert(tid) {
            continue;
        }
        let Some(allowed_cpus) = allowed_cpus_by_tid.get(&tid) else {
            continue;
        };
        let Some(target_domain) = planned_domains
            .get(&tid)
            .copied()
            .or_else(|| u32::try_from(task.current_domain).ok())
        else {
            continue;
        };
        let current_domain = u32::try_from(task.current_domain).ok();
        let legal_domains = allowed_domains_from_cpus(allowed_cpus, topo, mapping);
        if !legal_domains.contains(&target_domain) && current_domain != Some(target_domain) {
            continue;
        }
        let candidate_count =
            candidate_cpus_for_target_domain(allowed_cpus, target_domain, topo, mapping).len();
        if candidate_count == 0 {
            continue;
        }
        candidates.push((
            candidate_count,
            allowed_cpus.len(),
            index,
            tid,
            target_domain,
        ));
    }
    candidates.sort_by_key(|(candidate_count, allowed_count, index, tid, _)| {
        (*candidate_count, *allowed_count, *index, *tid)
    });

    let mut target_cpus = BTreeMap::new();
    for (_, _, index, tid, target_domain) in candidates {
        let Some(task) = pending.get(index) else {
            continue;
        };
        let Some(allowed_cpus) = allowed_cpus_by_tid.get(&tid) else {
            continue;
        };
        let candidate_cpus =
            candidate_cpus_for_target_domain(allowed_cpus, target_domain, topo, mapping);
        if let Some(fixed_cpu) = fixed_cpu_targets
            .get(&tid)
            .copied()
            .filter(|cpu| candidate_cpus.contains(cpu))
        {
            apply_planned_cpu_assignment(planned_cpu_states, planned_cpus, task, fixed_cpu);
            target_cpus.insert(tid, fixed_cpu);
            continue;
        }
        let target_cpu = planned_cpus
            .get(&tid)
            .copied()
            .and_then(|cpu| {
                preferred_reserved_cpu_for_selected_domain(
                    Some(cpu),
                    Some(target_domain),
                    topo,
                    mapping,
                    cpu_states,
                    cpu_utils_x100,
                    planned_cpu_states,
                    allowed_cpus,
                    cfg,
                    tid,
                )
            })
            .or_else(|| {
                choose_cpu_for_planned_domain(
                    task,
                    target_domain,
                    topo,
                    mapping,
                    cpu_states,
                    cpu_utils_x100,
                    planned_cpu_states,
                    allowed_cpus,
                    cfg,
                )
            });
        let Some(target_cpu) = target_cpu else {
            continue;
        };
        apply_planned_cpu_assignment(planned_cpu_states, planned_cpus, task, target_cpu);
        target_cpus.insert(tid, target_cpu);
    }
    target_cpus
}

pub(crate) fn refresh_thread_from_task(
    entry: &mut ManagedThreadState,
    task: &QueuedTask,
    now: u64,
) {
    entry.tid = task.tid as u32;
    entry.tgid = task.tgid as u32;
    entry.comm = crate::types::comm_to_string(&task.comm);
    entry.last_seen_ns = now;
    entry.last_observed_domain = u32::try_from(task.current_domain).ok();
    entry.last_observed_cpu = u32::try_from(task.current_cpu).ok();
    entry.last_l2_bw_mib_s_x100 = task.last_l2_bw_mib_s_x100;
    entry.ewma_l2_bw_mib_s_x100 = task.ewma_l2_bw_mib_s_x100;
    entry.last_fill_bw_mib_s_x100 = task.last_fill_bw_mib_s_x100;
    entry.ewma_fill_bw_mib_s_x100 = task.ewma_fill_bw_mib_s_x100;
    entry.last_ipc_x1000 = task.last_ipc_x1000;
    entry.ewma_ipc_x1000 = task.ewma_ipc_x1000;
    entry.last_stall_pct_x100 = task.last_stall_pct_x100;
    entry.ewma_stall_pct_x100 = task.ewma_stall_pct_x100;
    entry.last_stall_delta_pct_x100 = task_stall_delta_pct_x100(task);
}

pub(crate) fn apply_planned_migration(
    planned_states: &mut [PlannedDomainState],
    planned_domains: &mut BTreeMap<u32, u32>,
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    mapping: &MappingInfo,
    target_domain: u32,
) {
    let tid = task.tid as u32;
    let source_domain = u32::try_from(task.current_domain)
        .ok()
        .or_else(|| planned_domains.get(&tid).copied());
    if source_domain == Some(target_domain) {
        planned_domains.insert(tid, target_domain);
        return;
    }

    if let Some(source_domain) = source_domain {
        if let Some(state) = planned_states.get_mut(source_domain as usize) {
            state.task_count = state.task_count.saturating_sub(1);
            state.outgoing_migrations = state.outgoing_migrations.saturating_add(1);
            let source_effect =
                task_signature_effect(task, meta, mapping.df_capacity_mib_s_x100(source_domain));
            if source_effect.signature_valid {
                state.df_pressure_x100 =
                    state.df_pressure_x100.saturating_sub(source_effect.df_x100);
                state.llc_pressure_x100 = state
                    .llc_pressure_x100
                    .saturating_sub(source_effect.llc_x100);
                state.contributor_count = state.contributor_count.saturating_sub(1);
                if meta.map(|thread| thread.signature.stable).unwrap_or(false) {
                    state.stable_count = state.stable_count.saturating_sub(1);
                }
            }
        }
    }

    if let Some(state) = planned_states.get_mut(target_domain as usize) {
        state.task_count = state.task_count.saturating_add(1);
        state.incoming_migrations = state.incoming_migrations.saturating_add(1);
        let destination_effect =
            task_signature_effect(task, meta, mapping.df_capacity_mib_s_x100(target_domain));
        if destination_effect.signature_valid {
            state.df_pressure_x100 = state
                .df_pressure_x100
                .saturating_add(destination_effect.df_x100);
            state.llc_pressure_x100 = state
                .llc_pressure_x100
                .saturating_add(destination_effect.llc_x100);
            state.contributor_count = state.contributor_count.saturating_add(1);
            if meta.map(|thread| thread.signature.stable).unwrap_or(false) {
                state.stable_count = state.stable_count.saturating_add(1);
            }
        }
    }

    planned_domains.insert(tid, target_domain);
}

fn remove_planned_cpu_job(planned_cpu_states: &mut [PlannedCpuState], cpu: u32, tid: u32) {
    let Some(state) = planned_cpu_states.get_mut(cpu as usize) else {
        return;
    };
    if let Some(index) = state.jobs.iter().position(|candidate| *candidate == tid) {
        state.jobs.swap_remove(index);
    }
}

fn add_planned_cpu_job(planned_cpu_states: &mut [PlannedCpuState], cpu: u32, tid: u32) {
    let Some(state) = planned_cpu_states.get_mut(cpu as usize) else {
        return;
    };
    if !state.jobs.contains(&tid) {
        state.jobs.push(tid);
    }
}

pub(crate) fn apply_planned_cpu_assignment(
    planned_cpu_states: &mut [PlannedCpuState],
    planned_cpus: &mut BTreeMap<u32, u32>,
    task: &QueuedTask,
    target_cpu: u32,
) {
    let tid = task.tid as u32;
    let source_cpu = planned_cpus
        .get(&tid)
        .copied()
        .or_else(|| u32::try_from(task.current_cpu).ok());
    if let Some(source_cpu) = source_cpu {
        remove_planned_cpu_job(planned_cpu_states, source_cpu, tid);
    }
    add_planned_cpu_job(planned_cpu_states, target_cpu, tid);
    planned_cpus.insert(tid, target_cpu);
}

fn is_cross_domain_move(task: &QueuedTask, decision: &PlacementDecision) -> bool {
    decision
        .selected_domain
        .is_some_and(|domain| Some(domain) != task_current_domain(task))
        && matches!(
            decision.reason,
            crate::types::DecisionReason::MoveCoolerDomain
                | crate::types::DecisionReason::ExcludedDomainEscape
        )
        && !decision.defer_dispatch
}

fn ewma_u32(previous: u32, sample: u32) -> u32 {
    if previous == 0 {
        sample
    } else {
        ((u64::from(previous) * 3 + u64::from(sample)) / 4) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignatureUpdateKind {
    Placement,
    IoCs,
}

const IO_CS_SIGNATURE_FILL_CAP_NUMERATOR: u64 = 5;
const IO_CS_SIGNATURE_FILL_CAP_DENOMINATOR: u64 = 4;
const IO_CS_SIGNATURE_FILL_CAP_HEADROOM_X100: u32 = 500_00;
const IO_CS_SIGNATURE_STARTUP_CAP_DIVISOR: u32 = 4;
const IO_CS_SIGNATURE_STARTUP_CAP_FLOOR_X100: u32 = 5_000_00;
const IO_CS_SIGNATURE_HISTORY_GROWTH_NUMERATOR: u64 = 2;
const IO_CS_SIGNATURE_HISTORY_GROWTH_DENOMINATOR: u64 = 1;
const PLAN_CS_ATTRIBUTION_MAX_SWEEPS: u64 = 2;
const PLAN_CS_NEGATIVE_DECAY_DIVISOR: u32 = 16;
const PLAN_CS_EVIDENCE_CURRENT_CAP_NUMERATOR: u64 = 5;
const PLAN_CS_EVIDENCE_CURRENT_CAP_DENOMINATOR: u64 = 4;
const PLAN_CS_INGRESS_RELATIVE_WEIGHT_FLOOR_X100: u32 = 1_000_00;
const PLAN_CS_INGRESS_RELATIVE_WEIGHT_SCALE: u64 = 10_000;
const PLAN_CS_INGRESS_RELATIVE_WEIGHT_MIN_X10000: u32 = 150;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum PlanCsIngressSource {
    DramNearFill,
    TotalFill,
    L2,
}

impl PlanCsIngressSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::DramNearFill => "dram_near_fill",
            Self::TotalFill => "total_fill",
            Self::L2 => "l2",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PlanCsLinkBaseline {
    bw_x100: u32,
    sample_ts_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlanCsEvidenceDecay {
    contributors: u32,
    before_sum_x100: u32,
    limit_x100: u32,
    after_sum_x100: u32,
}

#[derive(Clone, Debug)]
struct PlanCsCandidateBaseline {
    fill_bw_mib_s_x100: [u32; MEM_SOURCE_COUNT],
    l2_bw_mib_s_x100: u32,
}

#[derive(Clone, Copy, Debug)]
struct PlanCsIngressWeight {
    tid: u32,
    weight: u32,
    ingress_delta_x100: i64,
    baseline_x100: u32,
    current_x100: u32,
    relative_x10000: u32,
}

#[derive(Clone, Debug)]
struct PendingPlanCsAttribution {
    planner_epoch: u64,
    plan_revision: u64,
    armed_at_ns: u64,
    link_count: usize,
    link_baselines: [Option<PlanCsLinkBaseline>; MAX_DOMAINS],
    applied_links: [bool; MAX_DOMAINS],
    candidates: BTreeMap<u32, PlanCsCandidateBaseline>,
}

impl PendingPlanCsAttribution {
    fn pending_link_count(&self) -> usize {
        self.link_baselines
            .iter()
            .take(self.link_count)
            .enumerate()
            .filter(|(idx, baseline)| baseline.is_some() && !self.applied_links[*idx])
            .count()
    }

    fn complete(&self) -> bool {
        self.pending_link_count() == 0
    }
}

#[derive(Default)]
struct PlanCsAttributionState {
    pending: Option<PendingPlanCsAttribution>,
    last_armed_plan: Option<(u64, u64)>,
}

fn saturating_ratio_u32(value: u32, numerator: u64, denominator: u64) -> u32 {
    if denominator == 0 {
        return u32::MAX;
    }
    ((u64::from(value) * numerator) / denominator).min(u64::from(u32::MAX)) as u32
}

fn task_total_fill_bw_x100(task: &QueuedTask) -> u32 {
    task.last_fill_bw_mib_s_x100
        .iter()
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value))
}

fn io_cs_task_fill_cap_x100(task: &QueuedTask) -> u32 {
    let total_fill = task_total_fill_bw_x100(task);
    if total_fill == 0 {
        return 0;
    }
    saturating_ratio_u32(
        total_fill,
        IO_CS_SIGNATURE_FILL_CAP_NUMERATOR,
        IO_CS_SIGNATURE_FILL_CAP_DENOMINATOR,
    )
    .saturating_add(IO_CS_SIGNATURE_FILL_CAP_HEADROOM_X100)
}

fn io_cs_startup_link_cap_x100(mapping: &MappingInfo, link_id: usize) -> u32 {
    mapping
        .cs_capacity_mib_s_x100(link_id as u32)
        .checked_div(IO_CS_SIGNATURE_STARTUP_CAP_DIVISOR)
        .unwrap_or(0)
        .max(IO_CS_SIGNATURE_STARTUP_CAP_FLOOR_X100)
}

fn io_cs_history_link_cap_x100(
    previous_signature: &ThreadSignature,
    mapping: &MappingInfo,
    link_id: usize,
) -> u32 {
    let startup_cap = io_cs_startup_link_cap_x100(mapping, link_id);
    if !previous_signature.valid {
        return startup_cap;
    }
    let previous = previous_signature
        .df_domain_delta_x100
        .get(link_id)
        .copied()
        .unwrap_or(0);
    if previous == 0 {
        return startup_cap;
    }
    saturating_ratio_u32(
        previous,
        IO_CS_SIGNATURE_HISTORY_GROWTH_NUMERATOR,
        IO_CS_SIGNATURE_HISTORY_GROWTH_DENOMINATOR,
    )
    .saturating_add(IO_CS_SIGNATURE_FILL_CAP_HEADROOM_X100)
    .max(startup_cap)
}

fn clamp_io_cs_signature_deltas(
    df_domain_delta_x100: &mut [u32; MAX_DOMAINS],
    previous_signature: &ThreadSignature,
    task: &QueuedTask,
    mapping: &MappingInfo,
    valid_domains: usize,
) {
    let fill_cap = io_cs_task_fill_cap_x100(task);
    for (idx, delta) in df_domain_delta_x100
        .iter_mut()
        .enumerate()
        .take(valid_domains.min(MAX_DOMAINS))
    {
        if *delta == 0 {
            continue;
        }
        if fill_cap == 0 {
            *delta = 0;
            continue;
        }
        let history_cap = io_cs_history_link_cap_x100(previous_signature, mapping, idx);
        *delta = (*delta).min(fill_cap).min(history_cap);
    }
}

fn signature_confidence_x100(
    previous: &crate::types::ThreadSignature,
    sample_df_x100: u32,
    sample_llc_x100: u32,
) -> u32 {
    if !previous.valid {
        return 7_000;
    }

    let previous_df = previous.projected_df_pressure_x100.max(1);
    let previous_llc = previous.projected_llc_pressure_x100.max(1);
    let df_delta = previous_df.abs_diff(sample_df_x100);
    let llc_delta = previous_llc.abs_diff(sample_llc_x100);
    let df_pct = ((u64::from(df_delta) * 10_000) / u64::from(previous_df)) as u32;
    let llc_pct = ((u64::from(llc_delta) * 10_000) / u64::from(previous_llc)) as u32;
    let delta_pct = df_pct.max(llc_pct);

    if delta_pct <= 2_500 {
        previous.confidence_x100.saturating_add(1_000).min(10_000)
    } else if delta_pct <= 5_000 {
        previous.confidence_x100.saturating_add(300).min(10_000)
    } else if delta_pct <= 10_000 {
        previous.confidence_x100.saturating_sub(1_500).max(1_000)
    } else {
        previous.confidence_x100.saturating_sub(2_500).max(500)
    }
}

fn fill_share_x100(fill_bw: &[u32; MEM_SOURCE_COUNT]) -> [u32; MEM_SOURCE_COUNT] {
    let mut shares = [0u32; MEM_SOURCE_COUNT];
    let total = fill_bw
        .iter()
        .fold(0u64, |acc, value| acc.saturating_add(u64::from(*value)));
    if total == 0 {
        return shares;
    }
    for (idx, value) in fill_bw.iter().enumerate() {
        shares[idx] = ((u64::from(*value) * 10_000) / total).min(u64::from(u32::MAX)) as u32;
    }
    shares
}

fn raw_df_snapshot(
    df_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    stale_ms: u64,
) -> Option<([u32; MAX_DOMAINS], usize)> {
    let mut snapshot = [0u32; MAX_DOMAINS];
    let valid_domains = df_states.len().min(MAX_DOMAINS);
    if valid_domains == 0 {
        return None;
    }
    for (idx, state) in df_states.iter().enumerate().take(valid_domains) {
        let Some(state) = state.as_ref() else {
            continue;
        };
        if state.valid == 0 || is_stale(state.sample_ts_ns, now_ns, stale_ms) {
            continue;
        }
        snapshot[idx] = state
            .raw_read_bw_mib_s_x100
            .saturating_add(state.raw_write_bw_mib_s_x100);
    }
    Some((snapshot, valid_domains))
}

fn plan_cs_stale_ms(policy: PolicyConfig, link_count: usize) -> u64 {
    policy
        .df_stale_ms
        .max(1)
        .saturating_mul(link_count.max(1) as u64)
}

fn plan_cs_pending_max_age_ns(policy: PolicyConfig, link_count: usize) -> u64 {
    plan_cs_stale_ms(policy, link_count)
        .saturating_mul(PLAN_CS_ATTRIBUTION_MAX_SWEEPS)
        .saturating_mul(1_000_000)
}

fn cs_link_bw_x100(state: CcmDfStateValue) -> u32 {
    state
        .raw_read_bw_mib_s_x100
        .saturating_add(state.raw_write_bw_mib_s_x100)
}

fn capture_plan_cs_link_baselines(
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    policy: PolicyConfig,
) -> ([Option<PlanCsLinkBaseline>; MAX_DOMAINS], usize, usize) {
    let link_count = df_cs_states.len().min(MAX_DOMAINS);
    let stale_ms = plan_cs_stale_ms(policy, link_count);
    let mut baselines = [None; MAX_DOMAINS];
    let mut valid_links = 0usize;
    for (idx, state) in df_cs_states.iter().enumerate().take(link_count) {
        let Some(state) = state.as_ref().copied() else {
            continue;
        };
        if state.valid == 0 || is_stale(state.sample_ts_ns, now_ns, stale_ms) {
            continue;
        }
        baselines[idx] = Some(PlanCsLinkBaseline {
            bw_x100: cs_link_bw_x100(state),
            sample_ts_ns: state.sample_ts_ns,
        });
        valid_links += 1;
    }
    (baselines, link_count, valid_links)
}

fn collect_plan_cs_candidates(
    plan: &PlacementPlan,
    threads: &BTreeMap<u32, ManagedThreadState>,
    active_cutoff_ns: u64,
) -> BTreeMap<u32, PlanCsCandidateBaseline> {
    plan.entries
        .keys()
        .filter_map(|tid| {
            let thread = threads.get(tid)?;
            if thread.last_seen_ns < active_cutoff_ns {
                return None;
            }
            Some((
                *tid,
                PlanCsCandidateBaseline {
                    fill_bw_mib_s_x100: thread.last_fill_bw_mib_s_x100,
                    l2_bw_mib_s_x100: thread.last_l2_bw_mib_s_x100,
                },
            ))
        })
        .collect()
}

fn signed_delta_u32(current: u32, baseline: u32) -> i64 {
    i64::from(current) - i64::from(baseline)
}

fn same_delta_direction(value: i64, reference: i64) -> bool {
    (reference > 0 && value > 0) || (reference < 0 && value < 0)
}

fn total_fill_bw_x100(fill_bw: &[u32; MEM_SOURCE_COUNT]) -> u32 {
    fill_bw
        .iter()
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value))
}

fn plan_cs_ingress_values(
    baseline: &PlanCsCandidateBaseline,
    thread: &ManagedThreadState,
    source: PlanCsIngressSource,
) -> (u32, u32) {
    match source {
        PlanCsIngressSource::DramNearFill => (
            baseline.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR],
            thread.last_fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR],
        ),
        PlanCsIngressSource::TotalFill => (
            total_fill_bw_x100(&baseline.fill_bw_mib_s_x100),
            total_fill_bw_x100(&thread.last_fill_bw_mib_s_x100),
        ),
        PlanCsIngressSource::L2 => (baseline.l2_bw_mib_s_x100, thread.last_l2_bw_mib_s_x100),
    }
}

fn plan_cs_ingress_delta(
    baseline: &PlanCsCandidateBaseline,
    thread: &ManagedThreadState,
    source: PlanCsIngressSource,
) -> i64 {
    let (baseline_x100, current_x100) = plan_cs_ingress_values(baseline, thread, source);
    signed_delta_u32(current_x100, baseline_x100)
}

fn plan_cs_relative_ingress_weight(
    ingress_delta_x100: i64,
    baseline_x100: u32,
) -> Option<(u32, u32)> {
    let delta_abs_x100 = ingress_delta_x100.unsigned_abs();
    if delta_abs_x100 == 0 {
        return None;
    }
    let denominator = u64::from(baseline_x100.max(PLAN_CS_INGRESS_RELATIVE_WEIGHT_FLOOR_X100));
    let relative_x10000 = delta_abs_x100
        .saturating_mul(PLAN_CS_INGRESS_RELATIVE_WEIGHT_SCALE)
        .checked_div(denominator)
        .unwrap_or(u64::from(u32::MAX))
        .clamp(1, u64::from(u32::MAX)) as u32;
    if relative_x10000 < PLAN_CS_INGRESS_RELATIVE_WEIGHT_MIN_X10000 {
        return None;
    }
    Some((relative_x10000, relative_x10000))
}

fn plan_cs_ingress_weights(
    pending: &PendingPlanCsAttribution,
    threads: &BTreeMap<u32, ManagedThreadState>,
    link_delta_x100: i64,
    active_cutoff_ns: u64,
) -> Option<(PlanCsIngressSource, Vec<PlanCsIngressWeight>)> {
    const ORDER: [PlanCsIngressSource; 3] = [
        PlanCsIngressSource::DramNearFill,
        PlanCsIngressSource::TotalFill,
        PlanCsIngressSource::L2,
    ];

    for source in ORDER {
        let mut weights = Vec::new();
        for (&tid, baseline) in &pending.candidates {
            let Some(thread) = threads.get(&tid) else {
                continue;
            };
            if thread.last_seen_ns < active_cutoff_ns {
                continue;
            }
            let (baseline_x100, current_x100) = plan_cs_ingress_values(baseline, thread, source);
            let ingress_delta = plan_cs_ingress_delta(baseline, thread, source);
            if !same_delta_direction(ingress_delta, link_delta_x100) {
                continue;
            }
            if let Some((weight, relative_x10000)) =
                plan_cs_relative_ingress_weight(ingress_delta, baseline_x100)
            {
                weights.push(PlanCsIngressWeight {
                    tid,
                    weight,
                    ingress_delta_x100: ingress_delta,
                    baseline_x100,
                    current_x100,
                    relative_x10000,
                });
            }
        }
        if !weights.is_empty() {
            return Some((source, weights));
        }
    }

    None
}

fn update_plan_cs_thread_signature(
    thread: &mut ManagedThreadState,
    link_id: usize,
    link_count: usize,
    raw_share_x100: u32,
    link_delta_x100: i64,
    now_ns: u64,
) {
    if raw_share_x100 == 0 || link_id >= MAX_DOMAINS {
        return;
    }

    let current_fill = thread.last_fill_bw_mib_s_x100;
    let current_l2 = thread.last_l2_bw_mib_s_x100;
    let fill_share = fill_share_x100(&current_fill);
    let signature = &mut thread.io_cs_signature;
    let previous_valid = signature.valid;

    signature.valid = true;
    signature.sample_count = signature.sample_count.saturating_add(1);
    signature.last_update_ns = now_ns;
    signature.confidence_x100 = if previous_valid {
        signature.confidence_x100.saturating_add(500).min(10_000)
    } else {
        7_000
    };
    for idx in 0..MEM_SOURCE_COUNT {
        signature.fill_bw_mib_s_x100[idx] =
            ewma_u32(signature.fill_bw_mib_s_x100[idx], current_fill[idx]);
        signature.fill_share_x100[idx] = ewma_u32(signature.fill_share_x100[idx], fill_share[idx]);
    }
    signature.projected_llc_pressure_x100 =
        ewma_u32(signature.projected_llc_pressure_x100, current_l2);

    let previous_link = signature.df_domain_delta_x100[link_id];
    let filtered_link = if link_delta_x100 > 0 {
        signature.raw_df_domain_delta_x100[link_id] = raw_share_x100;
        ewma_u32(previous_link, raw_share_x100)
    } else {
        signature.raw_df_domain_delta_x100[link_id] = 0;
        previous_link.saturating_sub(raw_share_x100.div_ceil(PLAN_CS_NEGATIVE_DECAY_DIVISOR))
    };
    signature.df_domain_delta_x100[link_id] = filtered_link;
    signature.projected_df_pressure_x100 = signature
        .df_domain_delta_x100
        .iter()
        .take(link_count.min(MAX_DOMAINS))
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value));
    signature.stable = signature.sample_count >= SIGNATURE_STABLE_SAMPLES
        && signature.confidence_x100 >= SIGNATURE_CONFIDENCE_THRESHOLD_X100;
}

fn recompute_io_cs_projected_df_pressure(signature: &mut ThreadSignature, link_count: usize) {
    signature.projected_df_pressure_x100 = signature
        .df_domain_delta_x100
        .iter()
        .take(link_count.min(MAX_DOMAINS))
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value));
}

fn plan_cs_evidence_current_limit_x100(current_bw_x100: u32) -> u32 {
    ((u64::from(current_bw_x100) * PLAN_CS_EVIDENCE_CURRENT_CAP_NUMERATOR)
        / PLAN_CS_EVIDENCE_CURRENT_CAP_DENOMINATOR)
        .min(u64::from(u32::MAX)) as u32
}

fn decay_io_cs_link_evidence_to_current(
    threads: &mut BTreeMap<u32, ManagedThreadState>,
    link_id: usize,
    link_count: usize,
    current_bw_x100: u32,
    now_ns: u64,
) -> Option<PlanCsEvidenceDecay> {
    if link_id >= MAX_DOMAINS {
        return None;
    }

    let contributors = threads
        .iter()
        .filter_map(|(&tid, thread)| {
            if !thread.io_cs_signature.valid {
                return None;
            }
            let contribution = thread.io_cs_signature.df_domain_delta_x100[link_id];
            (contribution > 0).then_some((tid, contribution))
        })
        .collect::<Vec<_>>();
    if contributors.is_empty() {
        return None;
    }

    let before_sum_x100 = contributors.iter().fold(0u32, |acc, (_, contribution)| {
        acc.saturating_add(*contribution)
    });
    let limit_x100 = plan_cs_evidence_current_limit_x100(current_bw_x100);
    if before_sum_x100 <= limit_x100 {
        return None;
    }

    let mut remaining_limit = limit_x100;
    let mut changed = 0u32;
    for (idx, (tid, contribution)) in contributors.iter().copied().enumerate() {
        let new_contribution = if idx + 1 == contributors.len() {
            remaining_limit.min(contribution)
        } else {
            let scaled = ((u64::from(contribution) * u64::from(limit_x100))
                / u64::from(before_sum_x100))
            .min(u64::from(contribution)) as u32;
            remaining_limit = remaining_limit.saturating_sub(scaled);
            scaled
        };

        if let Some(thread) = threads.get_mut(&tid) {
            let signature = &mut thread.io_cs_signature;
            if signature.df_domain_delta_x100[link_id] != new_contribution {
                changed = changed.saturating_add(1);
            }
            signature.df_domain_delta_x100[link_id] = new_contribution;
            signature.raw_df_domain_delta_x100[link_id] = new_contribution;
            signature.last_update_ns = now_ns;
            signature.sample_count = signature.sample_count.saturating_add(1);
            recompute_io_cs_projected_df_pressure(signature, link_count);
            signature.stable = signature.sample_count >= SIGNATURE_STABLE_SAMPLES
                && signature.confidence_x100 >= SIGNATURE_CONFIDENCE_THRESHOLD_X100;
        }
    }

    if changed == 0 {
        return None;
    }

    let after_sum_x100 = threads.values().fold(0u32, |acc, thread| {
        if !thread.io_cs_signature.valid {
            return acc;
        }
        acc.saturating_add(thread.io_cs_signature.df_domain_delta_x100[link_id])
    });

    Some(PlanCsEvidenceDecay {
        contributors: changed,
        before_sum_x100,
        limit_x100,
        after_sum_x100,
    })
}

fn decay_io_cs_evidence_to_current_samples(
    threads: &mut BTreeMap<u32, ManagedThreadState>,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    policy: PolicyConfig,
) {
    let link_count = df_cs_states.len().min(MAX_DOMAINS);
    if link_count == 0 {
        return;
    }

    let stale_ms = plan_cs_stale_ms(policy, link_count);
    for (link_id, state) in df_cs_states.iter().enumerate().take(link_count) {
        let Some(state) = state.as_ref().copied() else {
            continue;
        };
        if state.valid == 0 || is_stale(state.sample_ts_ns, now_ns, stale_ms) {
            continue;
        }
        let current_bw_x100 = cs_link_bw_x100(state);
        if let Some(decay) = decay_io_cs_link_evidence_to_current(
            threads,
            link_id,
            link_count,
            current_bw_x100,
            now_ns,
        ) {
            log_plan_cs_delta(format!(
                "action=decay reason=current_sample_cap link={} sample_ts_ns={} current_bw={} decayed_contributors={} evidence_before_mib_s_x100={} evidence_limit_mib_s_x100={} evidence_after_mib_s_x100={}",
                link_id,
                state.sample_ts_ns,
                current_bw_x100,
                decay.contributors,
                decay.before_sum_x100,
                decay.limit_x100,
                decay.after_sum_x100,
            ));
        }
    }
}

fn log_plan_cs_delta(message: String) {
    runtime_log::line(format!("plan_cs_delta {message}"));
}

fn arm_plan_cs_attribution(
    state: &mut PlanCsAttributionState,
    plan: &PlacementPlan,
    threads: &BTreeMap<u32, ManagedThreadState>,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    policy: PolicyConfig,
) {
    let plan_key = (plan.built_from_sweep_epoch, plan.plan_revision);
    if state.last_armed_plan == Some(plan_key) {
        return;
    }

    let active_cutoff_ns = now_ns
        .saturating_sub(plan_cs_stale_ms(policy, df_cs_states.len()).saturating_mul(1_000_000));
    let candidates = collect_plan_cs_candidates(plan, threads, active_cutoff_ns);
    if candidates.is_empty() {
        log_plan_cs_delta(format!(
            "action=skip reason=no_candidates planner_epoch={} plan_revision={} plan_entries={}",
            plan.built_from_sweep_epoch,
            plan.plan_revision,
            plan.entries.len()
        ));
        return;
    }

    let (link_baselines, link_count, valid_links) =
        capture_plan_cs_link_baselines(df_cs_states, now_ns, policy);
    if valid_links == 0 {
        log_plan_cs_delta(format!(
            "action=skip reason=no_cs_baseline planner_epoch={} plan_revision={} candidates={}",
            plan.built_from_sweep_epoch,
            plan.plan_revision,
            candidates.len()
        ));
        return;
    }

    let replaced = state
        .pending
        .as_ref()
        .map(|pending| (pending.candidates.len(), pending.pending_link_count()));
    state.pending = Some(PendingPlanCsAttribution {
        planner_epoch: plan.built_from_sweep_epoch,
        plan_revision: plan.plan_revision,
        armed_at_ns: now_ns,
        link_count,
        link_baselines,
        applied_links: [false; MAX_DOMAINS],
        candidates,
    });
    state.last_armed_plan = Some(plan_key);
    let candidates = state
        .pending
        .as_ref()
        .map(|pending| pending.candidates.len())
        .unwrap_or(0);
    if let Some((replaced_candidates, replaced_pending_links)) = replaced {
        log_plan_cs_delta(format!(
            "action=arm pending=replace planner_epoch={} plan_revision={} candidates={} baseline_links={} link_count={} replaced_candidates={} replaced_pending_links={}",
            plan.built_from_sweep_epoch,
            plan.plan_revision,
            candidates,
            valid_links,
            link_count,
            replaced_candidates,
            replaced_pending_links
        ));
    } else {
        log_plan_cs_delta(format!(
            "action=arm pending=new planner_epoch={} plan_revision={} candidates={} baseline_links={} link_count={}",
            plan.built_from_sweep_epoch,
            plan.plan_revision,
            candidates,
            valid_links,
            link_count
        ));
    }
}

fn apply_plan_cs_attribution(
    state: &mut PlanCsAttributionState,
    threads: &mut BTreeMap<u32, ManagedThreadState>,
    df_cs_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    policy: PolicyConfig,
) {
    let Some(mut pending) = state.pending.take() else {
        return;
    };

    let link_count = pending.link_count.min(df_cs_states.len()).min(MAX_DOMAINS);
    let stale_ms = plan_cs_stale_ms(policy, link_count);
    let mut fresh_links = Vec::new();
    for link_id in 0..link_count {
        if pending.applied_links[link_id] {
            continue;
        }
        let Some(baseline) = pending.link_baselines[link_id] else {
            continue;
        };
        let Some(current) = df_cs_states[link_id].as_ref().copied() else {
            continue;
        };
        if current.valid == 0 || is_stale(current.sample_ts_ns, now_ns, stale_ms) {
            continue;
        }
        if current.sample_ts_ns > baseline.sample_ts_ns {
            fresh_links.push((link_id, baseline, current));
        }
    }

    if fresh_links.is_empty() {
        if now_ns.saturating_sub(pending.armed_at_ns)
            >= plan_cs_pending_max_age_ns(policy, pending.link_count)
        {
            log_plan_cs_delta(format!(
                "action=expire reason=stale_pending planner_epoch={} plan_revision={} candidates={} pending_links={} age_ns={}",
                pending.planner_epoch,
                pending.plan_revision,
                pending.candidates.len(),
                pending.pending_link_count(),
                now_ns.saturating_sub(pending.armed_at_ns)
            ));
        } else {
            state.pending = Some(pending);
        }
        return;
    }

    let active_cutoff_ns = now_ns.saturating_sub(stale_ms.saturating_mul(1_000_000));
    for (link_id, baseline, current) in fresh_links {
        let current_bw_x100 = cs_link_bw_x100(current);
        let link_delta_x100 = signed_delta_u32(current_bw_x100, baseline.bw_x100);
        pending.applied_links[link_id] = true;
        if link_delta_x100 == 0 {
            log_plan_cs_delta(format!(
                "action=skip reason=zero_cs_delta planner_epoch={} plan_revision={} link={} baseline_bw={} current_bw={} candidates={}",
                pending.planner_epoch,
                pending.plan_revision,
                link_id,
                baseline.bw_x100,
                current_bw_x100,
                pending.candidates.len()
            ));
            continue;
        }

        let Some((source, weights)) =
            plan_cs_ingress_weights(&pending, threads, link_delta_x100, active_cutoff_ns)
        else {
            if link_delta_x100 < 0 {
                if let Some(decay) = decay_io_cs_link_evidence_to_current(
                    threads,
                    link_id,
                    link_count,
                    current_bw_x100,
                    now_ns,
                ) {
                    log_plan_cs_delta(format!(
                        "action=decay reason=no_ingress_delta planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} baseline_bw={} current_bw={} candidates={} decayed_contributors={} evidence_before_mib_s_x100={} evidence_limit_mib_s_x100={} evidence_after_mib_s_x100={}",
                        pending.planner_epoch,
                        pending.plan_revision,
                        link_id,
                        link_delta_x100,
                        baseline.bw_x100,
                        current_bw_x100,
                        pending.candidates.len(),
                        decay.contributors,
                        decay.before_sum_x100,
                        decay.limit_x100,
                        decay.after_sum_x100,
                    ));
                    continue;
                }
            }
            log_plan_cs_delta(format!(
                "action=skip reason=no_ingress_delta planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} baseline_bw={} current_bw={} candidates={}",
                pending.planner_epoch,
                pending.plan_revision,
                link_id,
                link_delta_x100,
                baseline.bw_x100,
                current_bw_x100,
                pending.candidates.len()
            ));
            continue;
        };

        let delta_abs_x100 = link_delta_x100.unsigned_abs().min(u64::from(u32::MAX)) as u32;
        let total_weight = weights.iter().fold(0u64, |acc, weight| {
            acc.saturating_add(u64::from(weight.weight))
        });
        if total_weight == 0 {
            if link_delta_x100 < 0 {
                if let Some(decay) = decay_io_cs_link_evidence_to_current(
                    threads,
                    link_id,
                    link_count,
                    current_bw_x100,
                    now_ns,
                ) {
                    log_plan_cs_delta(format!(
                        "action=decay reason=no_ingress_weight planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} baseline_bw={} current_bw={} candidates={} decayed_contributors={} evidence_before_mib_s_x100={} evidence_limit_mib_s_x100={} evidence_after_mib_s_x100={}",
                        pending.planner_epoch,
                        pending.plan_revision,
                        link_id,
                        link_delta_x100,
                        baseline.bw_x100,
                        current_bw_x100,
                        pending.candidates.len(),
                        decay.contributors,
                        decay.before_sum_x100,
                        decay.limit_x100,
                        decay.after_sum_x100,
                    ));
                    continue;
                }
            }
            log_plan_cs_delta(format!(
                "action=skip reason=no_ingress_delta planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} candidates={}",
                pending.planner_epoch,
                pending.plan_revision,
                link_id,
                link_delta_x100,
                pending.candidates.len()
            ));
            continue;
        }

        let mut remaining_delta = u64::from(delta_abs_x100);
        let mut remaining_weight = total_weight;
        let mut attributed = 0u32;
        let mut updated = 0u32;
        for (idx, weight) in weights.iter().enumerate() {
            let share = if idx + 1 == weights.len() || remaining_weight <= u64::from(weight.weight)
            {
                remaining_delta
            } else {
                (u64::from(delta_abs_x100) * u64::from(weight.weight)) / total_weight
            }
            .min(u64::from(u32::MAX)) as u32;
            remaining_delta = remaining_delta.saturating_sub(u64::from(share));
            remaining_weight = remaining_weight.saturating_sub(u64::from(weight.weight));
            if share == 0 {
                continue;
            }
            if let Some(thread) = threads.get_mut(&weight.tid) {
                update_plan_cs_thread_signature(
                    thread,
                    link_id,
                    link_count,
                    share,
                    link_delta_x100,
                    now_ns,
                );
                attributed = attributed.saturating_add(share);
                updated = updated.saturating_add(1);
                log_plan_cs_delta(format!(
                    "action=apply_candidate planner_epoch={} plan_revision={} link={} tid={} share_mib_s_x100={} weight={} weight_mode=relative_ingress_delta weight_source={} ingress_delta_mib_s_x100={} ingress_baseline={} ingress_current={} ingress_relative_x10000={}",
                    pending.planner_epoch,
                    pending.plan_revision,
                    link_id,
                    weight.tid,
                    share,
                    weight.weight,
                    source.as_str(),
                    weight.ingress_delta_x100,
                    weight.baseline_x100,
                    weight.current_x100,
                    weight.relative_x10000,
                ));
            }
        }

        let raw_pos = if link_delta_x100 > 0 {
            delta_abs_x100
        } else {
            0
        };
        let raw_neg = if link_delta_x100 < 0 {
            delta_abs_x100
        } else {
            0
        };
        log_plan_cs_delta(format!(
            "action=apply planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} raw_pos_mib_s_x100={} raw_neg_mib_s_x100={} baseline_bw={} current_bw={} weight_mode=relative_ingress_delta weight_source={} weight_sources={} candidates={} updated_candidates={} total_weight={} attributed_mib_s_x100={} pending_links={}",
            pending.planner_epoch,
            pending.plan_revision,
            link_id,
            link_delta_x100,
            raw_pos,
            raw_neg,
            baseline.bw_x100,
            current_bw_x100,
            source.as_str(),
            source.as_str(),
            pending.candidates.len(),
            updated,
            total_weight,
            attributed,
            pending.pending_link_count(),
        ));
        if link_delta_x100 < 0 {
            if let Some(decay) = decay_io_cs_link_evidence_to_current(
                threads,
                link_id,
                link_count,
                current_bw_x100,
                now_ns,
            ) {
                log_plan_cs_delta(format!(
                    "action=decay reason=current_link_cap planner_epoch={} plan_revision={} link={} cs_delta_mib_s_x100={} baseline_bw={} current_bw={} candidates={} decayed_contributors={} evidence_before_mib_s_x100={} evidence_limit_mib_s_x100={} evidence_after_mib_s_x100={}",
                    pending.planner_epoch,
                    pending.plan_revision,
                    link_id,
                    link_delta_x100,
                    baseline.bw_x100,
                    current_bw_x100,
                    pending.candidates.len(),
                    decay.contributors,
                    decay.before_sum_x100,
                    decay.limit_x100,
                    decay.after_sum_x100,
                ));
            }
        }
    }

    if !pending.complete() {
        state.pending = Some(pending);
    }
}

#[cfg(test)]
mod signature_snapshot_tests {
    use super::*;
    use crate::bpf::QueuedTask;

    fn df_state_at(raw_bw_x100: u32, sample_ts_ns: u64) -> Option<CcmDfStateValue> {
        Some(CcmDfStateValue {
            sample_ts_ns,
            raw_read_bw_mib_s_x100: raw_bw_x100,
            raw_write_bw_mib_s_x100: 0,
            valid: 1,
            ..CcmDfStateValue::default()
        })
    }

    fn df_state(raw_bw_x100: u32) -> Option<CcmDfStateValue> {
        df_state_at(raw_bw_x100, 1_000_000)
    }

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1, 2],
            domain_to_ccm: vec![Some(0), Some(1), Some(2)],
            domain_to_df_capacity_mib_s_x100: vec![
                Some(20_000_00),
                Some(20_000_00),
                Some(20_000_00),
            ],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1, 2]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2]),
        }
    }

    fn task_with_fill(start_ts: u64, stop_ts: u64, fill_bw_x100: u32) -> QueuedTask {
        QueuedTask {
            tid: 11,
            tgid: 11,
            current_cpu: 0,
            current_domain: 0,
            nr_cpus_allowed: 2,
            flags: 0,
            start_ts,
            stop_ts,
            exec_runtime: stop_ts.saturating_sub(start_ts),
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: fill_bw_x100,
            ewma_l2_bw_mib_s_x100: fill_bw_x100,
            last_fill_bw_mib_s_x100: [0, fill_bw_x100, 0],
            ewma_fill_bw_mib_s_x100: [0, fill_bw_x100, 0],
            last_ipc_x1000: STALL_IPC_CAP_X1000,
            ewma_ipc_x1000: STALL_IPC_CAP_X1000,
            last_stall_pct_x100: 0,
            ewma_stall_pct_x100: 0,
            trigger: QueueTrigger::Enqueue as u32,
            tick_seq: 0,
            last_update_ns: stop_ts,
            comm: [0; crate::types::COMM_LEN],
        }
    }

    fn cfg() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 85_00,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 50_00,
            stall_victim_delta_pct_x100: 15_00,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: true,
            cs_villain_throttle: true,
            tick_reeval_every: 1,
            tick_defer_max: 1,
            cs_villain_reslice_ns: 1_000_000,
            cs_villain_refill_divisor: 4,
            cs_villain_settle_ns: 20_000_000,
            cs_villain_release_samples: 2,
            tick_move_phase_mod: 1,
        }
    }

    fn plan_with_tids(tids: &[u32]) -> PlacementPlan {
        let mut entries = BTreeMap::new();
        for &tid in tids {
            entries.insert(
                tid,
                PlacementPlanEntry {
                    target_domain: 0,
                    built_from_sweep_epoch: 7,
                    plan_revision: 11,
                    ..PlacementPlanEntry::default()
                },
            );
        }
        PlacementPlan {
            built_from_sweep_epoch: 7,
            plan_revision: 11,
            planned_at_ns: 1_500_000,
            entries,
        }
    }

    fn thread_with_ingress(
        tid: u32,
        seen_ns: u64,
        dram_near_x100: u32,
        near_cache_x100: u32,
        l2_x100: u32,
    ) -> ManagedThreadState {
        let mut fill = [0; MEM_SOURCE_COUNT];
        fill[MEM_SOURCE_DRAM_NEAR] = dram_near_x100;
        fill[MEM_SOURCE_NEAR_CACHE] = near_cache_x100;
        ManagedThreadState {
            tid,
            tgid: tid,
            last_seen_ns: seen_ns,
            last_fill_bw_mib_s_x100: fill,
            last_l2_bw_mib_s_x100: l2_x100,
            ..ManagedThreadState::default()
        }
    }

    fn refresh_test_thread_ingress(
        thread: &mut ManagedThreadState,
        seen_ns: u64,
        dram_near_x100: u32,
        near_cache_x100: u32,
        l2_x100: u32,
    ) {
        thread.last_seen_ns = seen_ns;
        thread.last_fill_bw_mib_s_x100 = [0; MEM_SOURCE_COUNT];
        thread.last_fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR] = dram_near_x100;
        thread.last_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE] = near_cache_x100;
        thread.last_l2_bw_mib_s_x100 = l2_x100;
    }

    #[test]
    fn raw_df_snapshot_tolerates_sparse_domains() {
        let snapshot = raw_df_snapshot(&[df_state(100), None, df_state(300)], 2_000_000, 100)
            .expect("snapshot");
        assert_eq!(snapshot.1, 3);
        assert_eq!(snapshot.0[0], 100);
        assert_eq!(snapshot.0[1], 0);
        assert_eq!(snapshot.0[2], 300);
    }

    #[test]
    fn update_thread_signature_works_with_sparse_domains() {
        let now_ns = 2_000_000;
        let before = [df_state(1_000), None, df_state(2_000)];
        let after = [df_state(1_400), None, df_state(2_500)];
        let mut thread = ManagedThreadState::default();
        thread.dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns, 100, Some(0), Some(0));

        update_thread_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 200_000),
            &mapping(),
            &after,
            now_ns + 1_000,
            100,
        );

        assert!(thread.signature.valid);
        assert_eq!(thread.signature.sample_count, 1);
        assert_eq!(thread.signature.confidence_x100, 7_000);
        assert_eq!(thread.signature.projected_df_pressure_x100, 900);
        assert_eq!(thread.signature.df_domain_delta_x100[0], 400);
        assert_eq!(thread.signature.df_domain_delta_x100[1], 0);
        assert_eq!(thread.signature.df_domain_delta_x100[2], 500);
    }

    #[test]
    fn update_thread_signature_becomes_stable_after_second_consistent_sample() {
        let now_ns = 2_000_000;
        let before = [df_state(1_000), None, df_state(2_000)];
        let after = [df_state(1_400), None, df_state(2_500)];
        let mut thread = ManagedThreadState::default();
        thread.dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns, 100, Some(0), Some(0));

        update_thread_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 200_000),
            &mapping(),
            &after,
            now_ns + 1_000,
            100,
        );

        thread.dispatch_snapshot =
            capture_dispatch_snapshot(&after, now_ns + 2_000, 100, Some(0), Some(0));
        let after2 = [df_state(1_800), None, df_state(3_000)];
        update_thread_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 200_000),
            &mapping(),
            &after2,
            now_ns + 3_000,
            100,
        );

        assert!(thread.signature.valid);
        assert!(thread.signature.stable);
        assert_eq!(thread.signature.sample_count, 2);
        assert!(thread.signature.confidence_x100 >= SIGNATURE_CONFIDENCE_THRESHOLD_X100);
    }

    #[test]
    fn update_thread_io_cs_signature_clamps_first_contaminated_delta() {
        let now_ns = 2_000_000;
        let before = [df_state(0), None, df_state(0)];
        let after = [df_state(30_000_00), None, df_state(0)];
        let mut thread = ManagedThreadState::default();
        thread.villain_dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns, 100, Some(0), Some(0));

        update_thread_io_cs_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 6_000_00),
            &mapping(),
            &after,
            now_ns + 1_000,
            100,
        );

        assert!(thread.io_cs_signature.valid);
        assert_eq!(thread.io_cs_signature.sample_count, 1);
        assert_eq!(
            thread.io_cs_signature.raw_df_domain_delta_x100[0],
            30_000_00
        );
        assert_eq!(thread.io_cs_signature.df_domain_delta_x100[0], 5_000_00);
        assert_eq!(thread.io_cs_signature.projected_df_pressure_x100, 5_000_00);
    }

    #[test]
    fn update_thread_io_cs_signature_grows_with_confirming_fill_signal() {
        let now_ns = 2_000_000;
        let before = [df_state(0), None, df_state(0)];
        let after = [df_state(30_000_00), None, df_state(0)];
        let mut thread = ManagedThreadState::default();
        thread.villain_dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns, 100, Some(0), Some(0));
        update_thread_io_cs_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 6_000_00),
            &mapping(),
            &after,
            now_ns + 1_000,
            100,
        );

        thread.villain_dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns + 2_000, 100, Some(0), Some(0));
        update_thread_io_cs_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 6_000_00),
            &mapping(),
            &after,
            now_ns + 3_000,
            100,
        );

        assert!(thread.io_cs_signature.valid);
        assert_eq!(thread.io_cs_signature.sample_count, 2);
        assert_eq!(
            thread.io_cs_signature.raw_df_domain_delta_x100[0],
            30_000_00
        );
        // First sample is capped at link capacity / 4 (5,000 MiB/s). Once the
        // thread already has that signature, the next high sample can grow up
        // to the task-fill cap (6,000 * 1.25 + 500 = 8,000 MiB/s), then enters
        // the existing EWMA: (5,000 * 3 + 8,000) / 4 = 5,750 MiB/s.
        assert_eq!(thread.io_cs_signature.df_domain_delta_x100[0], 5_750_00);
        assert_eq!(thread.io_cs_signature.projected_df_pressure_x100, 5_750_00);
    }

    #[test]
    fn update_thread_signature_keeps_raw_ccm_delta_without_io_cs_clamp() {
        let now_ns = 2_000_000;
        let before = [df_state(0), None, df_state(0)];
        let after = [df_state(30_000_00), None, df_state(0)];
        let mut thread = ManagedThreadState::default();
        thread.dispatch_snapshot =
            capture_dispatch_snapshot(&before, now_ns, 100, Some(0), Some(0));

        update_thread_signature(
            &mut thread,
            &task_with_fill(0, MIN_VALID_RUN_NS + 1, 1_000_00),
            &mapping(),
            &after,
            now_ns + 1_000,
            100,
        );

        assert!(thread.signature.valid);
        assert_eq!(thread.signature.df_domain_delta_x100[0], 30_000_00);
        assert_eq!(thread.signature.projected_df_pressure_x100, 30_000_00);
    }

    #[test]
    fn plan_cs_attribution_assigns_cs_delta_to_matching_dram_near_ingress_delta() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11, 22]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 1_000_00, 0, 1_000_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_000_000, 2_000_00, 0, 2_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 1_800_00, 0, 1_800_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_100_000, 2_000_00, 0, 2_000_00),
        );
        let after = [df_state_at(6_200_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        let producer = threads.get(&11).expect("producer");
        let peer = threads.get(&22).expect("peer");
        assert!(producer.io_cs_signature.valid);
        assert_eq!(
            producer.io_cs_signature.raw_df_domain_delta_x100[0],
            1_200_00
        );
        assert_eq!(producer.io_cs_signature.df_domain_delta_x100[0], 1_200_00);
        assert!(!peer.io_cs_signature.valid);
        assert!(state.pending.is_none());
    }

    #[test]
    fn plan_cs_attribution_weights_ingress_delta_relative_to_baseline() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11, 22]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 1_000_00, 0, 1_000_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_000_000, 10_000_00, 0, 10_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 1_500_00, 0, 1_500_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_100_000, 10_750_00, 0, 10_750_00),
        );
        let after = [df_state_at(6_000_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        let probe_share = threads
            .get(&11)
            .unwrap()
            .io_cs_signature
            .raw_df_domain_delta_x100[0];
        let memory_share = threads
            .get(&22)
            .unwrap()
            .io_cs_signature
            .raw_df_domain_delta_x100[0];
        assert!(probe_share > memory_share);
        assert!(probe_share >= 86_000);
        assert!(memory_share <= 14_000);
        assert_eq!(probe_share.saturating_add(memory_share), 1_000_00);
    }

    #[test]
    fn plan_cs_attribution_replaces_pending_to_align_link_and_ingress_baselines() {
        let mut state = PlanCsAttributionState::default();
        let old_plan = plan_with_tids(&[11]);
        let mut new_plan = plan_with_tids(&[22]);
        new_plan.plan_revision = 12;
        for entry in new_plan.entries.values_mut() {
            entry.plan_revision = 12;
        }

        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 4_000_00, 0, 4_000_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_000_000, 1_000_00, 0, 1_000_00),
        );

        let old_baseline = [
            df_state_at(1_000_00, 1_000_000),
            None,
            None,
            df_state_at(1_000_00, 1_000_000),
        ];
        arm_plan_cs_attribution(
            &mut state,
            &old_plan,
            &threads,
            &old_baseline,
            2_000_000,
            cfg(),
        );

        let new_baseline = [
            df_state_at(5_000_00, 2_050_000),
            None,
            None,
            df_state_at(5_000_00, 2_050_000),
        ];
        arm_plan_cs_attribution(
            &mut state,
            &new_plan,
            &threads,
            &new_baseline,
            2_100_000,
            cfg(),
        );

        refresh_test_thread_ingress(
            threads.get_mut(&22).unwrap(),
            2_200_000,
            2_000_00,
            0,
            2_000_00,
        );
        let after = [
            df_state_at(5_000_00, 2_150_000),
            None,
            None,
            df_state_at(7_000_00, 2_150_000),
        ];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_200_000, cfg());

        assert!(!threads.get(&11).unwrap().io_cs_signature.valid);
        assert_eq!(
            threads
                .get(&22)
                .unwrap()
                .io_cs_signature
                .raw_df_domain_delta_x100[3],
            2_000_00
        );
        assert_eq!(
            threads
                .get(&22)
                .unwrap()
                .io_cs_signature
                .df_domain_delta_x100[3],
            2_000_00
        );
    }

    #[test]
    fn plan_cs_attribution_skips_subthreshold_relative_ingress_jitter() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 1_000_00, 0, 1_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 1_010_00, 0, 1_010_00),
        );
        let after = [df_state_at(6_000_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        assert!(!threads.get(&11).unwrap().io_cs_signature.valid);
    }

    #[test]
    fn plan_cs_attribution_skips_cs_delta_without_same_direction_ingress_delta() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 2_000_00, 0, 2_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 1_000_00, 0, 1_000_00),
        );
        let after = [df_state_at(6_200_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        assert!(!threads.get(&11).unwrap().io_cs_signature.valid);
        assert!(state.pending.is_none());
    }

    #[test]
    fn plan_cs_attribution_falls_back_to_total_fill_delta() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11, 22]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 0, 1_000_00, 1_000_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_000_000, 0, 2_000_00, 2_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 0, 1_900_00, 1_900_00),
        );
        threads.insert(
            22,
            thread_with_ingress(22, 2_100_000, 0, 2_000_00, 2_000_00),
        );
        let after = [df_state_at(5_900_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        assert_eq!(
            threads
                .get(&11)
                .unwrap()
                .io_cs_signature
                .raw_df_domain_delta_x100[0],
            900_00
        );
        assert!(!threads.get(&22).unwrap().io_cs_signature.valid);
    }

    #[test]
    fn plan_cs_attribution_falls_back_to_l2_delta() {
        let mut state = PlanCsAttributionState::default();
        let plan = plan_with_tids(&[11]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 0, 1_000_00, 1_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        threads.insert(
            11,
            thread_with_ingress(11, 2_100_000, 0, 1_000_00, 1_700_00),
        );
        let after = [df_state_at(5_700_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after, 2_100_000, cfg());

        assert_eq!(
            threads
                .get(&11)
                .unwrap()
                .io_cs_signature
                .raw_df_domain_delta_x100[0],
            700_00
        );
    }

    #[test]
    fn plan_cs_negative_delta_decays_existing_evidence_slowly() {
        let mut state = PlanCsAttributionState::default();
        let mut plan = plan_with_tids(&[11]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 1_000_00, 0, 1_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        refresh_test_thread_ingress(
            threads.get_mut(&11).unwrap(),
            2_100_000,
            3_000_00,
            0,
            3_000_00,
        );
        let after_positive = [df_state_at(7_000_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after_positive, 2_100_000, cfg());
        assert_eq!(
            threads
                .get(&11)
                .unwrap()
                .io_cs_signature
                .df_domain_delta_x100[0],
            2_000_00
        );

        plan.plan_revision = 12;
        if let Some(entry) = plan.entries.get_mut(&11) {
            entry.plan_revision = 12;
        }
        arm_plan_cs_attribution(
            &mut state,
            &plan,
            &threads,
            &after_positive,
            2_200_000,
            cfg(),
        );

        refresh_test_thread_ingress(
            threads.get_mut(&11).unwrap(),
            2_300_000,
            1_400_00,
            0,
            1_400_00,
        );
        let after_negative = [df_state_at(5_400_00, 2_250_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after_negative, 2_300_000, cfg());

        assert_eq!(
            threads
                .get(&11)
                .unwrap()
                .io_cs_signature
                .df_domain_delta_x100[0],
            1_900_00
        );
    }

    #[test]
    fn plan_cs_negative_delta_without_ingress_delta_decays_to_current_link() {
        let mut state = PlanCsAttributionState::default();
        let mut plan = plan_with_tids(&[11]);
        let mut threads = BTreeMap::new();
        threads.insert(
            11,
            thread_with_ingress(11, 2_000_000, 1_000_00, 0, 1_000_00),
        );
        let before = [df_state_at(5_000_00, 1_000_000), None, None];

        arm_plan_cs_attribution(&mut state, &plan, &threads, &before, 2_000_000, cfg());

        refresh_test_thread_ingress(
            threads.get_mut(&11).unwrap(),
            2_100_000,
            3_000_00,
            0,
            3_000_00,
        );
        let after_positive = [df_state_at(7_000_00, 2_050_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after_positive, 2_100_000, cfg());
        assert_eq!(
            threads
                .get(&11)
                .unwrap()
                .io_cs_signature
                .df_domain_delta_x100[0],
            2_000_00
        );

        plan.plan_revision = 12;
        if let Some(entry) = plan.entries.get_mut(&11) {
            entry.plan_revision = 12;
        }
        arm_plan_cs_attribution(
            &mut state,
            &plan,
            &threads,
            &after_positive,
            2_200_000,
            cfg(),
        );

        refresh_test_thread_ingress(
            threads.get_mut(&11).unwrap(),
            2_300_000,
            3_000_00,
            0,
            3_000_00,
        );
        let after_cool = [df_state_at(10_00, 2_250_000), None, None];
        apply_plan_cs_attribution(&mut state, &mut threads, &after_cool, 2_300_000, cfg());

        let signature = &threads.get(&11).unwrap().io_cs_signature;
        assert_eq!(signature.df_domain_delta_x100[0], 12_50);
        assert_eq!(signature.raw_df_domain_delta_x100[0], 12_50);
        assert_eq!(signature.projected_df_pressure_x100, 12_50);
        assert!(state.pending.is_none());
    }

    #[test]
    fn current_sample_cap_decays_evidence_without_plan_delta() {
        let mut thread = thread_with_ingress(11, 2_000_000, 3_000_00, 0, 3_000_00);
        thread.io_cs_signature.valid = true;
        thread.io_cs_signature.sample_count = 2;
        thread.io_cs_signature.confidence_x100 = 8_000;
        thread.io_cs_signature.df_domain_delta_x100[0] = 730_57;
        thread.io_cs_signature.raw_df_domain_delta_x100[0] = 730_57;
        thread.io_cs_signature.projected_df_pressure_x100 = 730_57;
        thread.io_cs_signature.stable = true;

        let mut threads = BTreeMap::from([(11, thread)]);
        let df_cs_states = [df_state_at(10_00, 2_050_000), None, None];

        decay_io_cs_evidence_to_current_samples(&mut threads, &df_cs_states, 2_100_000, cfg());

        let signature = &threads.get(&11).unwrap().io_cs_signature;
        assert_eq!(signature.df_domain_delta_x100[0], 12_50);
        assert_eq!(signature.raw_df_domain_delta_x100[0], 12_50);
        assert_eq!(signature.projected_df_pressure_x100, 12_50);
        assert_eq!(signature.sample_count, 3);
    }
}

#[cfg(test)]
mod villain_selection_tests {
    use super::*;
    use crate::types::{
        villain_reslice_slice_ns, MappingInfo, PolicyConfig, ThreadSignature,
        VILLAIN_RESLICE_DIVISOR,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(20_000_00), Some(20_000_00)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
        }
    }

    fn cfg() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 85_00,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 50_00,
            stall_victim_delta_pct_x100: 15_00,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: true,
            cs_villain_throttle: true,
            tick_reeval_every: 1,
            tick_defer_max: 1,
            cs_villain_reslice_ns: 1_000_000,
            cs_villain_refill_divisor: 4,
            cs_villain_settle_ns: 20_000_000,
            cs_villain_release_samples: 2,
            tick_move_phase_mod: 1,
        }
    }

    fn df_state(raw_total_centi_mib_per_s: u32) -> Option<CcmDfStateValue> {
        df_state_at(raw_total_centi_mib_per_s, 1_000_000)
    }

    fn contender(
        tid: u32,
        dram_near_centi_mib_per_s: u32,
        near_cache_centi_mib_per_s: u32,
    ) -> ManagedThreadState {
        let mut thread = ManagedThreadState {
            tid,
            ..ManagedThreadState::default()
        };
        thread.io_cs_signature = ThreadSignature {
            valid: true,
            stable: true,
            fill_bw_mib_s_x100: [0, near_cache_centi_mib_per_s, dram_near_centi_mib_per_s],
            projected_df_pressure_x100: dram_near_centi_mib_per_s,
            df_domain_delta_x100: [
                dram_near_centi_mib_per_s,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
            ..ThreadSignature::default()
        };
        thread
    }

    fn contender_on_link(
        tid: u32,
        current_domain: u32,
        link_id: u32,
        centi_mib_per_s: u32,
    ) -> ManagedThreadState {
        contender_on_links(tid, current_domain, &[(link_id, centi_mib_per_s)])
    }

    fn contender_on_links(
        tid: u32,
        current_domain: u32,
        link_scores: &[(u32, u32)],
    ) -> ManagedThreadState {
        let mut thread = ManagedThreadState {
            tid,
            last_selected_domain: Some(current_domain),
            ..ManagedThreadState::default()
        };
        let mut df_domain_delta_x100 = [0; MAX_DOMAINS];
        let mut projected_df_pressure_x100 = 0u32;
        for &(link_id, centi_mib_per_s) in link_scores {
            df_domain_delta_x100[link_id as usize] = centi_mib_per_s;
            projected_df_pressure_x100 = projected_df_pressure_x100.saturating_add(centi_mib_per_s);
        }
        thread.io_cs_signature = ThreadSignature {
            valid: true,
            stable: true,
            projected_df_pressure_x100,
            df_domain_delta_x100,
            ..ThreadSignature::default()
        };
        thread
    }

    fn contender_with_aggregate_only(
        tid: u32,
        current_domain: u32,
        centi_mib_per_s: u32,
    ) -> ManagedThreadState {
        let mut thread = ManagedThreadState {
            tid,
            last_selected_domain: Some(current_domain),
            ..ManagedThreadState::default()
        };
        thread.io_cs_signature = ThreadSignature {
            valid: true,
            stable: true,
            projected_df_pressure_x100: centi_mib_per_s,
            ..ThreadSignature::default()
        };
        thread
    }

    fn assert_only_link_villain(
        villains: &[Option<LinkContender>],
        victim_link: u32,
        expected_tid: u32,
    ) {
        assert_eq!(
            villains[victim_link as usize].map(|winner| winner.tid),
            Some(expected_tid)
        );
        assert!(villains
            .iter()
            .enumerate()
            .all(|(link_id, winner)| link_id == victim_link as usize || winner.is_none()));
    }

    fn assert_no_link_villains(villains: &[Option<LinkContender>]) {
        assert!(villains.iter().all(Option::is_none));
    }

    #[test]
    fn villain_token_bucket_reslices_then_defers_until_refill() {
        let mut entry = ManagedThreadState::default();
        let link_id = 7;
        let now_ns = 1_000_000;
        let slice_ns = villain_reslice_slice_ns();

        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns, cfg()),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 1_000_000, cfg()),
            VillainThrottleAction::Defer {
                tokens_ns: slice_ns / VILLAIN_RESLICE_DIVISOR,
            }
        );
        assert_eq!(entry.consecutive_tick_defer, 1);
        assert_eq!(entry.throttle_defer_started_ns, now_ns + 1_000_000);
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 4_000_000, cfg()),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(entry.consecutive_tick_defer, 0);
        assert_eq!(entry.throttle_defer_started_ns, 0);
    }

    #[test]
    fn villain_token_bucket_refill_scales_with_tick_reeval_every() {
        let mut entry = ManagedThreadState::default();
        let mut policy = cfg();
        policy.tick_reeval_every = 4;
        let link_id = 7;
        let now_ns = 1_000_000;
        let slice_ns = villain_reslice_slice_ns();

        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns, policy),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 4_000_000, policy),
            VillainThrottleAction::Defer {
                tokens_ns: slice_ns / 4,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 16_000_000, policy),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
    }

    #[test]
    fn villain_token_bucket_honors_custom_reslice_and_refill_divisor() {
        let mut entry = ManagedThreadState::default();
        let mut policy = cfg();
        policy.cs_villain_reslice_ns = 500_000;
        policy.cs_villain_refill_divisor = 8;
        let link_id = 7;
        let now_ns = 1_000_000;

        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns, policy),
            VillainThrottleAction::Reslice {
                slice_ns: 500_000,
                tokens_ns: 0,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 2_000_000, policy),
            VillainThrottleAction::Defer { tokens_ns: 250_000 }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns + 4_000_000, policy),
            VillainThrottleAction::Reslice {
                slice_ns: 500_000,
                tokens_ns: 0,
            }
        );
    }

    #[test]
    fn villain_token_bucket_caps_burst_with_tick_defer_max() {
        let mut entry = ManagedThreadState::default();
        let mut policy = cfg();
        policy.tick_defer_max = 2;
        let link_id = 7;
        let slice_ns = villain_reslice_slice_ns();

        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, 1_000_000, policy),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: slice_ns,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, 1_000_100, policy),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 25,
            }
        );
    }

    #[test]
    fn villain_token_bucket_resets_when_link_changes_or_villain_clears() {
        let mut entry = ManagedThreadState::default();
        let slice_ns = villain_reslice_slice_ns();

        assert_eq!(
            apply_villain_token_throttle(&mut entry, 7, 1_000_000, cfg()),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(
            apply_villain_token_throttle(&mut entry, 8, 1_000_100, cfg()),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(entry.throttle_link_id, Some(8));

        reset_villain_throttle(&mut entry);
        assert_eq!(entry.throttle_link_id, None);
        assert_eq!(entry.throttle_tokens_ns, 0);
        assert_eq!(entry.throttle_last_refill_ns, 0);
        assert_eq!(entry.throttle_defer_started_ns, 0);
        assert_eq!(entry.consecutive_tick_defer, 0);
    }

    #[test]
    fn villain_token_bucket_bounds_defer_wait_before_watchdog() {
        let link_id = 7;
        let now_ns = 2_000_000_000;
        let slice_ns = villain_reslice_slice_ns();
        let mut entry = ManagedThreadState {
            throttle_link_id: Some(link_id),
            throttle_tokens_ns: 0,
            throttle_last_refill_ns: now_ns,
            throttle_defer_started_ns: now_ns - VILLAIN_DEFER_UPPER_BOUND_NS,
            consecutive_tick_defer: 99,
            ..ManagedThreadState::default()
        };

        assert_eq!(
            apply_villain_token_throttle(&mut entry, link_id, now_ns, cfg()),
            VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: 0,
            }
        );
        assert_eq!(entry.consecutive_tick_defer, 0);
        assert_eq!(entry.throttle_defer_started_ns, 0);
    }

    #[test]
    fn villain_token_bucket_allows_process_leader_when_selected_villain() {
        let leader_tid = 3773096;
        let now_ns = 2_000_000;
        let mut entry = ManagedThreadState {
            tid: leader_tid,
            tgid: leader_tid,
            comm: "mc_stride_probe".to_string(),
            throttle_link_id: Some(0),
            throttle_tokens_ns: 0,
            throttle_last_refill_ns: now_ns,
            ..ManagedThreadState::default()
        };
        let villain = LinkContender {
            tid: leader_tid,
            score: 1_850_000,
            contender_count: 4,
        };

        // A process leader can be the actual memory-pressure producer. Do not
        // exclude it merely because tid == tgid or because the comm looks like
        // a probe/launcher process.
        let control = maybe_apply_villain_token_throttle(
            &mut entry,
            Some((0, villain)),
            true,
            now_ns + 1_000,
            cfg(),
        );

        assert!(matches!(
            control.map(|control| control.action),
            Some(VillainThrottleAction::Defer { .. })
        ));
    }

    #[test]
    fn disabled_cs_villain_throttle_keeps_detection_but_skips_token_control() {
        let mut entry = ManagedThreadState {
            throttle_link_id: Some(7),
            throttle_tokens_ns: 123,
            throttle_last_refill_ns: 456,
            consecutive_tick_defer: 2,
            ..ManagedThreadState::default()
        };
        let mut policy = cfg();
        policy.cs_villain_throttle = false;
        let villain = LinkContender {
            tid: 42,
            score: 9_000,
            contender_count: 2,
        };

        assert!(maybe_apply_villain_token_throttle(
            &mut entry,
            Some((7, villain)),
            true,
            1_000_000,
            policy,
        )
        .is_none());
        assert_eq!(entry.throttle_link_id, None);
        assert_eq!(entry.throttle_tokens_ns, 0);
        assert_eq!(entry.throttle_last_refill_ns, 0);
        assert_eq!(entry.throttle_defer_started_ns, 0);
        assert_eq!(entry.consecutive_tick_defer, 0);
    }

    fn df_state_at(raw_total_centi_mib_per_s: u32, sample_ts_ns: u64) -> Option<CcmDfStateValue> {
        Some(CcmDfStateValue {
            sample_ts_ns,
            raw_read_bw_mib_s_x100: raw_total_centi_mib_per_s,
            raw_write_bw_mib_s_x100: 0,
            valid: 1,
            ..CcmDfStateValue::default()
        })
    }

    fn stale_df_state(raw_total_centi_mib_per_s: u32) -> Option<CcmDfStateValue> {
        df_state_at(raw_total_centi_mib_per_s, 1)
    }

    fn fresh_df_state(raw_total_centi_mib_per_s: u32, now_ns: u64) -> Option<CcmDfStateValue> {
        df_state_at(raw_total_centi_mib_per_s, now_ns.saturating_sub(1_000_000))
    }

    #[derive(Clone, Debug)]
    struct MockIoDomain {
        // Human-readable chiplet/CCD label used by tests, not scheduler input.
        label: &'static str,
        // Linux CPU ids that belong to this mock LLC/CCD domain.
        cpus: Vec<u32>,
        // Live chip-select Data Fabric read bandwidth in centi-MiB/s.
        cs_read_centi_mib_per_s: u32,
    }

    #[derive(Clone, Debug)]
    struct MockIoLoadMap {
        domains: Vec<MockIoDomain>,
    }

    impl MockIoLoadMap {
        // Builds the DIMM-stall CS overload scenario. ccd0 maps to the live
        // overloaded chip-select link, while the other CCDs remain lower-pressure
        // alternatives. Expected behavior in tests: the victim signal fires only
        // on the overloaded link, and villain selection picks the thread with the
        // strongest DRAM-side signature.
        fn dimm_stall() -> Self {
            Self {
                domains: vec![
                    // Raw mock data: CS read = 25,000 MiB/s, write = 0 MiB/s.
                    // This is 100% of the assumed fully-stressed DF read level
                    // and is the overloaded victim/villain domain.
                    MockIoDomain {
                        label: "ccd0",
                        cpus: vec![0, 1],
                        cs_read_centi_mib_per_s: 25_000_00,
                    },
                    // Raw mock data: CS read = 12,000 MiB/s, write = 0 MiB/s.
                    // This is below the full-stress read level.
                    MockIoDomain {
                        label: "ccd1",
                        cpus: vec![7, 8],
                        cs_read_centi_mib_per_s: 12_000_00,
                    },
                    // Raw mock data: CS read = 9,000 MiB/s, write = 0 MiB/s.
                    // This is about 36% of the full-stress read level.
                    MockIoDomain {
                        label: "ccd3",
                        cpus: vec![21, 22],
                        cs_read_centi_mib_per_s: 9_000_00,
                    },
                    // Raw mock data: CS read = 7,000 MiB/s, write = 0 MiB/s.
                    // This is about 28% of the full-stress read level.
                    MockIoDomain {
                        label: "ccd4",
                        cpus: vec![28, 29],
                        cs_read_centi_mib_per_s: 7_000_00,
                    },
                ],
            }
        }

        fn domain_id(&self, label: &str) -> u32 {
            self.domains
                .iter()
                .position(|domain| domain.label == label)
                .unwrap_or_else(|| panic!("unknown mock IO domain label: {label}"))
                as u32
        }

        fn mapping(&self) -> MappingInfo {
            let mut eligible_cpus = BTreeSet::new();
            for domain in &self.domains {
                eligible_cpus.extend(domain.cpus.iter().copied());
            }
            let domain_count = self.domains.len();
            MappingInfo {
                domain_to_ccx: (0..domain_count as u32).collect(),
                domain_to_ccm: (0..domain_count as u32).map(Some).collect(),
                domain_to_df_capacity_mib_s_x100: vec![Some(20_000_00); domain_count],
                cs_link_capacity_mib_s_x100: vec![],
                eligible_domains: (0..domain_count as u32).collect(),
                excluded_domains: BTreeSet::new(),
                eligible_cpus,
            }
        }

        fn df_states(&self) -> Vec<Option<CcmDfStateValue>> {
            self.domains
                .iter()
                .enumerate()
                .map(|(domain_idx, domain)| {
                    Some(CcmDfStateValue {
                        sample_ts_ns: 1_000_000,
                        ccx_id: domain_idx as u32,
                        ccm_id: domain_idx as u32,
                        raw_read_bw_mib_s_x100: domain.cs_read_centi_mib_per_s,
                        raw_write_bw_mib_s_x100: 0,
                        raw_pressure_pct_x100: domain.cs_read_centi_mib_per_s,
                        ewma_pressure_pct_x100: domain.cs_read_centi_mib_per_s,
                        valid: 1,
                        ..CcmDfStateValue::default()
                    })
                })
                .collect()
        }
    }

    fn stalled_tick_task(tid: u32, current_cpu: i32, current_domain: i32) -> QueuedTask {
        QueuedTask {
            tid: tid as i32,
            tgid: tid as i32,
            current_cpu,
            current_domain,
            nr_cpus_allowed: 2,
            flags: 0,
            start_ts: 0,
            stop_ts: MIN_VALID_RUN_NS + 1,
            exec_runtime: MIN_VALID_RUN_NS + 1,
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: 0,
            ewma_l2_bw_mib_s_x100: 0,
            last_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            ewma_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            last_ipc_x1000: 1_000,
            ewma_ipc_x1000: STALL_IPC_CAP_X1000,
            // Raw mock stall data: current stall = 60%, EWMA stall = 40%.
            last_stall_pct_x100: 60_00,
            ewma_stall_pct_x100: 40_00,
            trigger: QueueTrigger::Tick as u32,
            tick_seq: 1,
            last_update_ns: 1_000_000,
            comm: [0; crate::types::COMM_LEN],
        }
    }

    // Verifies the DIMM-stall victim signal against a mock chip-select IO map.
    // Expected: a task opens a victim-link signal when live CS overload is
    // paired with either a stall-delta spike or high absolute stall.
    #[test]
    fn eval_policy_io_load_map_stall_signal_uses_cs_overload_and_stall() {
        let map = MockIoLoadMap::dimm_stall();
        let overloaded_domain = map.domain_id("ccd0");
        let victim_tid = 41;
        let victim_current_cpu = 0;
        let victim_current_domain = overloaded_domain as i32;
        // Mock scheduler timestamp: the DF sample was taken at 1,000,000 ns,
        // so 2,000,000 ns keeps it fresh under the 100 ms stale window.
        let scheduler_now_ns = 2_000_000;
        let policy_cfg = cfg();
        let task = stalled_tick_task(victim_tid, victim_current_cpu, victim_current_domain);
        let mapping = map.mapping();
        let df_states = map.df_states();

        assert!(is_suspicious_stall_victim(
            &task,
            overloaded_domain,
            &mapping,
            &df_states,
            scheduler_now_ns,
            policy_cfg,
        ));
        assert!(is_suspicious_stall_victim(
            &QueuedTask {
                // Raw mock absolute-stall case: current stall is still 60%,
                // but EWMA stall = 59% leaves only a 1% delta.
                ewma_stall_pct_x100: 59_00,
                ..task.clone()
            },
            overloaded_domain,
            &mapping,
            &df_states,
            scheduler_now_ns,
            policy_cfg,
        ));
        assert!(!is_suspicious_stall_victim(
            &QueuedTask {
                // Raw mock negative case: current stall = 45% is below the
                // absolute threshold and EWMA stall = 44% leaves only 1% delta.
                last_stall_pct_x100: 45_00,
                ewma_stall_pct_x100: 44_00,
                ..task.clone()
            },
            overloaded_domain,
            &mapping,
            &df_states,
            scheduler_now_ns,
            policy_cfg,
        ));
    }

    #[test]
    fn stall_victim_links_scan_all_cs_links_not_current_domain_link() {
        let mapping = mapping();
        let task = stalled_tick_task(41, 0, 0);
        let mut df_states = vec![None; 12];
        df_states[10] = df_state(25_000_00);

        let links = suspicious_stall_victim_links(&task, &mapping, &df_states, 2_000_000, cfg());

        assert_eq!(links, BTreeSet::from([10]));
    }

    #[test]
    fn single_overloaded_cs_link_selects_only_that_links_villain() {
        let mapping = mapping();
        let victim_link = 7;
        let non_overloaded_link = 3;
        let mut df_states = vec![Some(CcmDfStateValue::default()); 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        df_states[non_overloaded_link as usize] = df_state(19_000_00);

        let victim_links = suspicious_stall_victim_links(
            &stalled_tick_task(41, 0, 0),
            &mapping,
            &df_states,
            2_000_000,
            cfg(),
        );
        assert_eq!(victim_links, BTreeSet::from([victim_link]));

        let mut threads = BTreeMap::new();
        threads.insert(71, contender_on_link(71, 0, victim_link, 9_000_00));
        threads.insert(72, contender_on_link(72, 1, victim_link, 6_000_00));
        threads.insert(73, contender_on_link(73, 0, non_overloaded_link, 20_000_00));

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 71);
    }

    #[test]
    fn link_villain_selection_allows_process_leader_when_it_has_top_cs_signature() {
        let mapping = mapping();
        let victim_link = 0;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);

        let mut leader = contender_on_link(1000, 0, victim_link, 30_000_00);
        leader.tgid = 1000;
        leader.comm = "mc_stride_probe".to_string();
        let mut worker = contender_on_link(1001, 0, victim_link, 12_000_00);
        worker.tgid = 1000;
        worker.comm = "mc_stride_probe".to_string();
        let peer = contender_on_link(2001, 0, victim_link, 5_000_00);
        let mut threads = BTreeMap::new();
        threads.insert(leader.tid, leader);
        threads.insert(worker.tid, worker);
        threads.insert(peer.tid, peer);

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &BTreeSet::from([victim_link]),
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 1000);
    }

    #[test]
    fn absolute_stall_signal_selects_signature_villain_without_fallback() {
        let mapping = mapping();
        let victim_link = 0;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        let task = QueuedTask {
            // Raw mock persistent-stall case: current stall is 83%, but EWMA is
            // already 82%, so delta alone would not open a victim-link signal.
            last_stall_pct_x100: 83_00,
            ewma_stall_pct_x100: 82_00,
            ..stalled_tick_task(41, 0, 0)
        };
        let signal =
            stall_signal_for_link(&task, victim_link, &mapping, &df_states, 2_000_000, cfg())
                .expect("absolute stall should open a CS victim-link signal");

        assert_eq!(signal.kind, StallSignalKind::Absolute);
        assert!(!signal.kind.allows_fallback_villain());

        let mut threads = BTreeMap::new();
        threads.insert(41, contender_with_aggregate_only(41, 0, 30_000_00));
        threads.insert(42, contender_on_link(42, 0, victim_link, 8_000_00));
        threads.insert(43, contender_on_link(43, 0, victim_link, 12_000_00));
        let fallbacks = BTreeMap::new();
        let villains = choose_link_villains_with_fallbacks(
            &mapping,
            &threads,
            &BTreeSet::from([victim_link]),
            &df_states,
            &fallbacks,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 43);

        let mut no_signature_threads = BTreeMap::new();
        no_signature_threads.insert(41, contender_with_aggregate_only(41, 0, 30_000_00));
        let no_signature_villains = choose_link_villains_with_fallbacks(
            &mapping,
            &no_signature_threads,
            &BTreeSet::from([victim_link]),
            &df_states,
            &fallbacks,
            2_000_000,
            cfg(),
        );
        assert_no_link_villains(&no_signature_villains);
    }

    #[test]
    fn single_overloaded_cs_link_ignores_aggregate_only_signatures() {
        let mapping = mapping();
        let victim_link = 7;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        let victim_links = suspicious_stall_victim_links(
            &stalled_tick_task(41, 0, 0),
            &mapping,
            &df_states,
            2_000_000,
            cfg(),
        );
        assert_eq!(victim_links, BTreeSet::from([victim_link]));

        let mut threads = BTreeMap::new();
        threads.insert(81, contender_on_link(81, 0, victim_link, 9_000_00));
        threads.insert(82, contender_with_aggregate_only(82, 1, 30_000_00));

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_no_link_villains(&villains);
    }

    #[test]
    fn link_villain_falls_back_to_current_stall_victim_when_signature_is_empty() {
        let mapping = mapping();
        let victim_link = 0;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        let victim_links = BTreeSet::from([victim_link]);

        let mut threads = BTreeMap::new();
        threads.insert(91, contender_with_aggregate_only(91, 0, 30_000_00));
        let mut fallbacks = BTreeMap::new();
        fallbacks.insert(
            victim_link,
            LinkContender {
                tid: 91,
                score: 10_000_00,
                contender_count: 1,
            },
        );

        let villains = choose_link_villains_with_fallbacks(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            &fallbacks,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 91);
    }

    #[test]
    fn single_overloaded_cs_link_scores_victim_delta_not_other_link_delta() {
        let mapping = mapping();
        let victim_link = 7;
        let non_overloaded_link = 3;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        df_states[non_overloaded_link as usize] = df_state(19_000_00);
        let victim_links = suspicious_stall_victim_links(
            &stalled_tick_task(41, 0, 0),
            &mapping,
            &df_states,
            2_000_000,
            cfg(),
        );
        assert_eq!(victim_links, BTreeSet::from([victim_link]));

        let mut threads = BTreeMap::new();
        threads.insert(
            91,
            contender_on_links(
                91,
                0,
                &[(victim_link, 4_000_00), (non_overloaded_link, 20_000_00)],
            ),
        );
        threads.insert(92, contender_on_link(92, 1, victim_link, 6_000_00));

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 92);
    }

    #[test]
    fn single_overloaded_unmapped_cs_link_ignores_mapped_off_link_pressure() {
        let mapping = mapping();
        let victim_link = 10;
        let mapped_non_overloaded_link = 0;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);
        df_states[mapped_non_overloaded_link as usize] = df_state(19_000_00);
        let victim_links = suspicious_stall_victim_links(
            &stalled_tick_task(41, 0, 0),
            &mapping,
            &df_states,
            2_000_000,
            cfg(),
        );
        assert_eq!(victim_links, BTreeSet::from([victim_link]));

        let mut threads = BTreeMap::new();
        threads.insert(101, contender_on_link(101, 0, victim_link, 9_000_00));
        threads.insert(102, contender_on_link(102, 1, victim_link, 6_000_00));
        threads.insert(
            103,
            contender_on_link(103, 0, mapped_non_overloaded_link, 30_000_00),
        );

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_only_link_villain(&villains, victim_link, 101);
    }

    #[test]
    fn single_overloaded_cs_link_ignores_stale_off_link_overload() {
        let mapping = mapping();
        let victim_link = 7;
        let stale_link = 3;
        let now_ns = 200_000_000;
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = fresh_df_state(25_000_00, now_ns);
        df_states[stale_link as usize] = stale_df_state(40_000_00);
        let victim_links = suspicious_stall_victim_links(
            &stalled_tick_task(41, 0, 0),
            &mapping,
            &df_states,
            now_ns,
            cfg(),
        );
        assert_eq!(victim_links, BTreeSet::from([victim_link]));

        let mut threads = BTreeMap::new();
        threads.insert(111, contender_on_link(111, 0, victim_link, 9_000_00));
        threads.insert(112, contender_on_link(112, 1, victim_link, 6_000_00));
        threads.insert(113, contender_on_link(113, 0, stale_link, 30_000_00));

        let villains =
            choose_link_villains(&mapping, &threads, &victim_links, &df_states, now_ns, cfg());

        assert_only_link_villain(&villains, victim_link, 111);
    }

    // Verifies villain selection on the overloaded victim link using per-thread
    // DRAM-side IO signatures. Expected: the thread with the larger DRAM-near
    // signature is selected as the villain.
    #[test]
    fn eval_policy_io_load_map_villain_prefers_dram_side_io_signature() {
        let map = MockIoLoadMap::dimm_stall();
        let victim_link = map.domain_id("ccd0");
        let cache_side_tid = 21;
        // Raw mock signature: cache-side thread has 2,500 MiB/s DRAM-near
        // bandwidth and 9,500 MiB/s near-cache bandwidth.
        let cache_side_dram_near_centi_mib_per_s = 2_500_00;
        let cache_side_near_cache_centi_mib_per_s = 9_500_00;
        let dram_side_tid = 22;
        // Raw mock signature: DRAM-side thread has 8,000 MiB/s DRAM-near
        // bandwidth and 1,000 MiB/s near-cache bandwidth.
        let dram_side_dram_near_centi_mib_per_s = 8_000_00;
        let dram_side_near_cache_centi_mib_per_s = 1_000_00;
        // Mock scheduler timestamp: the DF sample was taken at 1,000,000 ns,
        // so 2,000,000 ns keeps it fresh under the 100 ms stale window.
        let scheduler_now_ns = 2_000_000;
        let expected_villain_tid = dram_side_tid;
        let policy_cfg = cfg();
        let mut threads = BTreeMap::new();
        threads.insert(
            cache_side_tid,
            contender(
                cache_side_tid,
                cache_side_dram_near_centi_mib_per_s,
                cache_side_near_cache_centi_mib_per_s,
            ),
        );
        threads.insert(
            dram_side_tid,
            contender(
                dram_side_tid,
                dram_side_dram_near_centi_mib_per_s,
                dram_side_near_cache_centi_mib_per_s,
            ),
        );
        let mapping = map.mapping();
        let df_states = map.df_states();
        let victim_links = BTreeSet::from([victim_link]);

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &df_states,
            scheduler_now_ns,
            policy_cfg,
        );

        assert_eq!(
            villains[victim_link as usize].map(|winner| winner.tid),
            Some(expected_villain_tid)
        );
    }

    #[test]
    fn villain_selection_requires_live_df_overload() {
        let mapping = mapping();
        let mut threads = BTreeMap::new();
        // Raw mock signatures: contender 11 has 6,000 MiB/s DRAM-near and
        // 1,000 MiB/s near-cache bandwidth; contender 12 has 4,000 MiB/s
        // DRAM-near and 9,000 MiB/s near-cache bandwidth.
        threads.insert(11, contender(11, 6_000_00, 1_000_00));
        threads.insert(12, contender(12, 4_000_00, 9_000_00));
        let victim_links = BTreeSet::from([0]);
        let scheduler_now_ns = 2_000_000;
        // Raw mock live CS data: 15,000 MiB/s is 60% of the 25,000 MiB/s
        // full-stress level and below scheduler overload capacity, so no
        // villain should be selected.
        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &[df_state(15_000_00), None],
            scheduler_now_ns,
            cfg(),
        );
        assert!(villains[0].is_none());
    }

    #[test]
    fn villain_selection_uses_cs_capacity_not_ccm_capacity() {
        let mut mapping = mapping();
        mapping.domain_to_df_capacity_mib_s_x100[0] = Some(12_000_00);
        let mut threads = BTreeMap::new();
        threads.insert(11, contender(11, 6_000_00, 1_000_00));
        threads.insert(12, contender(12, 4_000_00, 9_000_00));
        let victim_links = BTreeSet::from([0]);
        let scheduler_now_ns = 2_000_000;

        let villains_with_low_ccm_capacity = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &[df_state(15_000_00), None],
            scheduler_now_ns,
            cfg(),
        );
        assert!(villains_with_low_ccm_capacity[0].is_none());

        mapping.cs_link_capacity_mib_s_x100 = vec![Some(12_000_00), None];
        let villains_with_low_cs_capacity = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &[df_state(15_000_00), None],
            scheduler_now_ns,
            cfg(),
        );
        assert_eq!(
            villains_with_low_cs_capacity[0].map(|winner| winner.tid),
            Some(11)
        );
    }

    #[test]
    fn villain_selection_prefers_dram_side_pressure() {
        let mapping = mapping();
        let mut threads = BTreeMap::new();
        // Raw mock signatures: contender 21 is mostly near-cache
        // (2,500 MiB/s DRAM-near, 9,500 MiB/s near-cache), while contender 22
        // is mostly DRAM-near (8,000 MiB/s DRAM-near, 1,000 MiB/s near-cache).
        threads.insert(21, contender(21, 2_500_00, 9_500_00));
        threads.insert(22, contender(22, 8_000_00, 1_000_00));
        let victim_links = BTreeSet::from([0]);
        let scheduler_now_ns = 2_000_000;
        // Raw mock live CS data: 25,000 MiB/s is the fully-stressed read level
        // and exceeds scheduler overload capacity, so the DRAM-side contender
        // should win.
        let villains = choose_link_villains(
            &mapping,
            &threads,
            &victim_links,
            &[df_state(25_000_00), None],
            scheduler_now_ns,
            cfg(),
        );
        assert_eq!(villains[0].map(|winner| winner.tid), Some(22));
    }

    #[test]
    fn villain_selection_is_link_based_not_current_domain_based() {
        let mapping = mapping();
        let victim_link = 1;
        let mut threads = BTreeMap::new();
        // Raw mock signatures: both contenders are currently placed on domain0,
        // but their CS signature projects onto link1. The larger link1
        // contributor should be throttled even though its CPU domain is not 1.
        threads.insert(41, contender_on_link(41, 0, victim_link, 9_000_00));
        threads.insert(42, contender_on_link(42, 0, victim_link, 6_000_00));

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &BTreeSet::from([victim_link]),
            &[None, df_state(25_000_00)],
            2_000_000,
            cfg(),
        );

        assert_eq!(
            villains[victim_link as usize].map(|winner| winner.tid),
            Some(41)
        );
    }

    #[test]
    fn villain_selection_supports_cs_links_beyond_mapped_ccm_count() {
        let mapping = mapping();
        let victim_link = 10;
        let mut threads = BTreeMap::new();
        threads.insert(51, contender_on_link(51, 0, victim_link, 9_000_00));
        threads.insert(52, contender_on_link(52, 1, victim_link, 6_000_00));
        let mut df_states = vec![None; 12];
        df_states[victim_link as usize] = df_state(25_000_00);

        let villains = choose_link_villains(
            &mapping,
            &threads,
            &BTreeSet::from([victim_link]),
            &df_states,
            2_000_000,
            cfg(),
        );

        assert_eq!(
            villains[victim_link as usize].map(|winner| winner.tid),
            Some(51)
        );
    }

    #[test]
    fn villain_selection_requires_victim_domain_signal() {
        let mapping = mapping();
        let mut threads = BTreeMap::new();
        // Raw mock signatures: both contenders are DRAM-near heavy, but this
        // test intentionally provides no victim-link signal.
        threads.insert(31, contender(31, 7_000_00, 1_000_00));
        threads.insert(32, contender(32, 6_500_00, 500_00));
        let scheduler_now_ns = 2_000_000;
        // Raw mock live CS data is fully stressed at 25,000 MiB/s, but the
        // empty victim set means no villain should be selected.
        let villains = choose_link_villains(
            &mapping,
            &threads,
            &BTreeSet::new(),
            &[df_state(25_000_00), None],
            scheduler_now_ns,
            cfg(),
        );
        assert!(villains[0].is_none());
    }

    #[test]
    fn suspicious_stall_victim_needs_overload_and_delta_or_absolute_stall() {
        let task = QueuedTask {
            tid: 41,
            tgid: 41,
            current_cpu: 0,
            current_domain: 0,
            nr_cpus_allowed: 2,
            flags: 0,
            start_ts: 0,
            stop_ts: MIN_VALID_RUN_NS + 1,
            exec_runtime: MIN_VALID_RUN_NS + 1,
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: 0,
            ewma_l2_bw_mib_s_x100: 0,
            last_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            ewma_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            last_ipc_x1000: 1_000,
            ewma_ipc_x1000: STALL_IPC_CAP_X1000,
            // Raw mock stall data: current stall = 60%, EWMA stall = 40%.
            last_stall_pct_x100: 60_00,
            ewma_stall_pct_x100: 40_00,
            trigger: QueueTrigger::Tick as u32,
            tick_seq: 1,
            last_update_ns: 1_000_000,
            comm: [0; crate::types::COMM_LEN],
        };
        let scheduler_now_ns = 2_000_000;
        let mapping = mapping();
        // Raw mock live CS data: 25,000 MiB/s is the fully-stressed read level
        // and exceeds scheduler overload capacity; the stall delta is 20%.
        assert!(is_suspicious_stall_victim(
            &task,
            0,
            &mapping,
            &[df_state(25_000_00), None],
            scheduler_now_ns,
            cfg(),
        ));
        // Raw mock absolute-stall case: EWMA stall = 59% leaves only 1% delta,
        // but current stall = 60% remains above the absolute threshold.
        assert!(is_suspicious_stall_victim(
            &QueuedTask {
                ewma_stall_pct_x100: 59_00,
                ..task.clone()
            },
            0,
            &mapping,
            &[df_state(25_000_00), None],
            scheduler_now_ns,
            cfg(),
        ));
        // Raw mock negative case: current stall = 45% is below the absolute
        // threshold and EWMA stall = 44% leaves only 1% delta.
        assert!(!is_suspicious_stall_victim(
            &QueuedTask {
                last_stall_pct_x100: 45_00,
                ewma_stall_pct_x100: 44_00,
                ..task.clone()
            },
            0,
            &mapping,
            &[df_state(25_000_00), None],
            scheduler_now_ns,
            cfg(),
        ));
        // Raw mock negative case: 15,000 MiB/s is only 60% of the 25,000 MiB/s
        // full-stress level and below scheduler overload capacity, even though
        // the task stall delta is high.
        assert!(!is_suspicious_stall_victim(
            &task,
            0,
            &mapping,
            &[df_state(15_000_00), None],
            scheduler_now_ns,
            cfg(),
        ));
    }
}

pub(crate) fn capture_dispatch_snapshot(
    df_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    stale_ms: u64,
    selected_domain: Option<u32>,
    selected_cpu: Option<u32>,
) -> Option<DispatchSnapshot> {
    let (df_raw_pressure_x100, valid_domains) = raw_df_snapshot(df_states, now_ns, stale_ms)?;
    Some(DispatchSnapshot {
        captured_at_ns: now_ns,
        selected_domain,
        selected_cpu,
        df_raw_bw_mib_s_x100: df_raw_pressure_x100,
        valid_domains,
    })
}

fn update_signature_state(
    signature: &mut ThreadSignature,
    snapshot_slot: &mut Option<DispatchSnapshot>,
    task: &QueuedTask,
    mapping: &MappingInfo,
    df_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    stale_ms: u64,
    kind: SignatureUpdateKind,
) {
    let Some(snapshot) = snapshot_slot.take() else {
        return;
    };
    if task.stop_ts <= task.start_ts {
        return;
    }
    if task.stop_ts.saturating_sub(task.start_ts) < MIN_VALID_RUN_NS {
        return;
    }
    let Some((current_df, valid_domains)) = raw_df_snapshot(df_states, now_ns, stale_ms) else {
        return;
    };
    if valid_domains != snapshot.valid_domains {
        return;
    }

    let previous_signature = signature.clone();
    let fill_share = fill_share_x100(&task.last_fill_bw_mib_s_x100);
    let mut df_domain_delta_x100 = [0u32; MAX_DOMAINS];
    for idx in 0..valid_domains {
        let delta = current_df[idx].saturating_sub(snapshot.df_raw_bw_mib_s_x100[idx]);
        df_domain_delta_x100[idx] = delta;
    }
    let raw_df_domain_delta_x100 = df_domain_delta_x100;
    if kind == SignatureUpdateKind::IoCs {
        clamp_io_cs_signature_deltas(
            &mut df_domain_delta_x100,
            &previous_signature,
            task,
            mapping,
            valid_domains,
        );
    }
    let projected_df_pressure_x100 = df_domain_delta_x100
        .iter()
        .take(valid_domains)
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value));
    let capacity_mib_s_x100 = snapshot
        .selected_domain
        .map(|domain| mapping.df_capacity_mib_s_x100(domain))
        .unwrap_or_else(|| mapping.max_df_capacity_mib_s_x100());
    let projected_llc_pressure_x100 =
        crate::policy::l2_bw_to_pressure_x100(task_total_fill_bw_x100(task), capacity_mib_s_x100);

    let has_signal = projected_df_pressure_x100 > 0
        || task
            .last_fill_bw_mib_s_x100
            .iter()
            .copied()
            .any(|value| value > 0);
    if !has_signal {
        return;
    }

    let confidence_x100 = signature_confidence_x100(
        &previous_signature,
        projected_df_pressure_x100,
        projected_llc_pressure_x100,
    );
    signature.valid = true;
    signature.sample_count = previous_signature.sample_count.saturating_add(1);
    signature.last_update_ns = now_ns;
    signature.confidence_x100 = confidence_x100;
    signature.projected_df_pressure_x100 = ewma_u32(
        signature.projected_df_pressure_x100,
        projected_df_pressure_x100,
    );
    signature.projected_llc_pressure_x100 = ewma_u32(
        signature.projected_llc_pressure_x100,
        projected_llc_pressure_x100,
    );
    for idx in 0..MEM_SOURCE_COUNT {
        signature.fill_bw_mib_s_x100[idx] = ewma_u32(
            signature.fill_bw_mib_s_x100[idx],
            task.last_fill_bw_mib_s_x100[idx],
        );
        signature.fill_share_x100[idx] = ewma_u32(signature.fill_share_x100[idx], fill_share[idx]);
    }
    for idx in 0..valid_domains {
        signature.raw_df_domain_delta_x100[idx] = raw_df_domain_delta_x100[idx];
        signature.df_domain_delta_x100[idx] = ewma_u32(
            signature.df_domain_delta_x100[idx],
            df_domain_delta_x100[idx],
        );
    }
    signature.stable = signature.sample_count >= SIGNATURE_STABLE_SAMPLES
        && signature.confidence_x100 >= SIGNATURE_CONFIDENCE_THRESHOLD_X100;
}

pub(crate) fn update_thread_signature(
    thread: &mut ManagedThreadState,
    task: &QueuedTask,
    mapping: &MappingInfo,
    df_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    stale_ms: u64,
) {
    update_signature_state(
        &mut thread.signature,
        &mut thread.dispatch_snapshot,
        task,
        mapping,
        df_states,
        now_ns,
        stale_ms,
        SignatureUpdateKind::Placement,
    );
}

fn update_thread_io_cs_signature(
    thread: &mut ManagedThreadState,
    task: &QueuedTask,
    mapping: &MappingInfo,
    df_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    stale_ms: u64,
) {
    update_signature_state(
        &mut thread.io_cs_signature,
        &mut thread.villain_dispatch_snapshot,
        task,
        mapping,
        df_states,
        now_ns,
        stale_ms,
        SignatureUpdateKind::IoCs,
    );
}

fn lookup_value<M, T>(map: &M, key: &u32) -> Result<Option<T>>
where
    M: libbpf_rs::MapCore,
    T: Default + plain::Plain,
{
    let Some(bytes) = map.lookup(key_bytes(key), MapFlags::ANY)? else {
        return Ok(None);
    };
    let mut value = T::default();
    plain::copy_from_bytes(&mut value, &bytes)
        .map_err(|err| anyhow::anyhow!("failed to decode map value: {err:?}"))?;
    Ok(Some(value))
}

struct AffinityCacheEntry {
    cpus: BTreeSet<u32>,
    sampled_at_ns: u64,
}

pub(crate) struct AffinityCache {
    ttl_ns: u64,
    entries: BTreeMap<u32, AffinityCacheEntry>,
}

impl AffinityCache {
    pub(crate) fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ns: ttl_ms.saturating_mul(1_000_000),
            entries: BTreeMap::new(),
        }
    }

    pub(crate) fn allowed_cpus(&mut self, tid: u32, now_ns: u64) -> Result<BTreeSet<u32>> {
        if let Some(entry) = self.entries.get(&tid) {
            if now_ns.saturating_sub(entry.sampled_at_ns) <= self.ttl_ns {
                return Ok(entry.cpus.clone());
            }
        }
        let path = format!("/proc/{tid}/status");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
        let mut cpus = BTreeSet::new();
        for line in text.lines() {
            if let Some(list) = line.strip_prefix("Cpus_allowed_list:\t") {
                for part in list.trim().split(',') {
                    if let Some((start, end)) = part.split_once('-') {
                        let start: u32 = start.parse()?;
                        let end: u32 = end.parse()?;
                        for cpu in start..=end {
                            cpus.insert(cpu);
                        }
                    } else if !part.is_empty() {
                        cpus.insert(part.parse()?);
                    }
                }
                break;
            }
        }
        self.entries.insert(
            tid,
            AffinityCacheEntry {
                cpus: cpus.clone(),
                sampled_at_ns: now_ns,
            },
        );
        Ok(cpus)
    }

    pub(crate) fn invalidate(&mut self, tid: u32) {
        self.entries.remove(&tid);
    }
}

struct WorkloadProcess {
    label: String,
    child: Child,
    exited_at: Option<Instant>,
}

#[cfg(feature = "stall-filler-spinner")]
struct StallFillerSpinner {
    child: Child,
    held_deferred: VecDeque<QueuedTask>,
    slice_ns: u64,
    dispatch_count: u64,
}

#[cfg(feature = "stall-filler-spinner")]
impl StallFillerSpinner {
    fn spawn(mapping: &MappingInfo, slice_ns: u64, verbose: bool) -> Result<Self> {
        let current = std::env::current_exe().context("failed to resolve scheduler executable")?;
        let mut command = Command::new(&current);
        command.arg(crate::STALL_FILLER_SPINNER_ARG);
        if !verbose {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        unsafe {
            command.pre_exec(stall_filler_pre_exec);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to spawn stall filler spinner using {}",
                current.display()
            )
        })?;
        let pid = child.id();
        if let Err(err) = wait_for_stall_filler_ready(pid) {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = child.wait();
            return Err(err);
        }
        if let Err(err) = set_pid_affinity(pid, &mapping.eligible_cpus) {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = child.wait();
            return Err(err);
        }
        let _ = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
        runtime_log::line(format!(
            "stall_filler_start pid={} slice_ns={} affinity_cpus={} binary={}",
            pid,
            slice_ns,
            eligible_cpulist(mapping),
            current.display()
        ));
        Ok(Self {
            child,
            held_deferred: VecDeque::new(),
            slice_ns,
            dispatch_count: 0,
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn maybe_hold_deferred_with_spinner(
        &mut self,
        deferred: &mut VecDeque<QueuedTask>,
    ) -> Option<Duration> {
        if deferred.is_empty() {
            return None;
        }
        let pid = self.pid();
        let ret = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
        if ret != 0 {
            runtime_log::line(format!(
                "stall_filler_unavailable pid={} held_deferred={} reason=sigcont_error error={}",
                pid,
                deferred.len(),
                std::io::Error::last_os_error()
            ));
            return None;
        }
        self.dispatch_count = self.dispatch_count.saturating_add(1);
        let held_count = deferred.len();
        self.held_deferred.extend(deferred.drain(..));
        runtime_log::line(format!(
            "stall_filler_run pid={} held_deferred={} slice_ns={} dispatch_count={}",
            pid, held_count, self.slice_ns, self.dispatch_count
        ));
        Some(Duration::from_nanos(self.slice_ns))
    }

    fn release_held_deferred(&mut self) -> usize {
        let pid = self.pid();
        let ret = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
        if ret != 0 {
            runtime_log::line(format!(
                "stall_filler_stop_error pid={} error={}",
                pid,
                std::io::Error::last_os_error()
            ));
        }
        let released = self.held_deferred.len();
        if released > 0 {
            runtime_log::line(format!(
                "stall_filler_release pid={} released_deferred={} dispatch_count={}",
                self.pid(),
                released,
                self.dispatch_count
            ));
        }
        released
    }

    fn drain_held_deferred(&mut self) -> impl Iterator<Item = QueuedTask> + '_ {
        self.held_deferred.drain(..)
    }

    fn shutdown(&mut self, verbose: bool) {
        let pid = self.pid();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
        }
        let _ = self.child.wait();
        if verbose {
            crate::diag_line!(
                "stall_filler_stop pid={} dispatch_count={}",
                pid,
                self.dispatch_count
            );
        }
    }
}

#[cfg(feature = "stall-filler-spinner")]
impl Drop for StallFillerSpinner {
    fn drop(&mut self) {
        let pid = self.child.id();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
        }
        let _ = self.child.wait();
    }
}

#[cfg(feature = "stall-filler-spinner")]
fn wait_for_stall_filler_ready(pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .with_context(|| format!("failed to read /proc/{pid}/comm for stall filler"))?;
        if comm.trim() == "rustland_filler" {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "stall filler pid={} did not enter hidden spinner mode before deadline; last_comm={}",
                pid,
                comm.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(feature = "stall-filler-spinner")]
fn set_pid_affinity(pid: u32, cpus: &BTreeSet<u32>) -> Result<()> {
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        for cpu in cpus {
            libc::CPU_SET(*cpu as usize, &mut cpuset);
        }
    }
    let ret = unsafe {
        libc::sched_setaffinity(pid as libc::pid_t, size_of::<libc::cpu_set_t>(), &cpuset)
    };
    if ret != 0 {
        anyhow::bail!(
            "sched_setaffinity(stall filler pid={pid}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(feature = "stall-filler-spinner")]
fn stall_filler_pre_exec() -> std::io::Result<()> {
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(feature = "light-compete")]
pub(crate) struct LightHelper {
    cpu: u32,
    child: Child,
}

#[cfg(feature = "light-compete")]
fn synth_load_path() -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let dir = current
        .parent()
        .context("scheduler executable does not have a parent directory")?;
    let candidate = dir.join("synth_load");
    if candidate.exists() {
        return Ok(candidate);
    }
    anyhow::bail!(
        "failed to find synth_load helper next to scheduler binary: {}",
        candidate.display()
    );
}

#[cfg(feature = "light-compete")]
fn helper_pre_exec(cpu: u32) -> std::io::Result<()> {
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu as usize, &mut cpuset);
    }
    let ret = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(feature = "light-compete")]
pub(crate) fn spawn_light_helpers(
    bpf: &mut BpfScheduler<'_>,
    cgroup_path: &Path,
    mapping: &crate::types::MappingInfo,
    verbose: bool,
) -> Result<Vec<LightHelper>> {
    let synth_bin = synth_load_path()?;
    let mut helpers: Vec<LightHelper> = Vec::new();
    for cpu in &mapping.eligible_cpus {
        let mut command = Command::new(&synth_bin);
        command
            .arg("spin")
            .arg("--threads")
            .arg("1")
            .arg("--duration-ms")
            .arg("0")
            .arg("--yield-every")
            .arg("4096");
        if !verbose {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let helper_cpu = *cpu;
        unsafe {
            command.pre_exec(move || helper_pre_exec(helper_cpu));
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to spawn light-compete helper for cpu {} using {}",
                cpu,
                synth_bin.display()
            )
        })?;
        let pid = child.id();
        let _ = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
        if let Err(err) = move_pid_to_cgroup(cgroup_path, pid)
            .and_then(|_| set_sched_ext(pid))
            .and_then(|_| set_internal_tgid(bpf, pid, true))
        {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = child.wait();
            for helper in helpers.iter_mut() {
                let helper_pid = helper.child.id();
                let _ = set_internal_tgid(bpf, helper_pid, false);
                let _ = unsafe { libc::kill(helper_pid as i32, libc::SIGKILL) };
                let _ = helper.child.wait();
            }
            return Err(err).with_context(|| {
                format!(
                    "failed to initialize light-compete helper pid={} cpu={}",
                    pid, cpu
                )
            });
        }
        let _ = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
        if verbose {
            crate::diag_line!(
                "light_helper_start pid={} cpu={} binary={}",
                pid,
                cpu,
                synth_bin.display(),
            );
        }
        helpers.push(LightHelper { cpu: *cpu, child });
    }
    Ok(helpers)
}

#[cfg(feature = "light-compete")]
pub(crate) fn shutdown_light_helpers(
    bpf: &mut BpfScheduler<'_>,
    helpers: &mut Vec<LightHelper>,
    verbose: bool,
) {
    for helper in helpers.iter_mut() {
        let pid = helper.child.id();
        let _ = set_internal_tgid(bpf, pid, false);
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let _ = helper.child.wait();
        if verbose {
            crate::diag_line!("light_helper_stop pid={} cpu={}", pid, helper.cpu);
        }
    }
    helpers.clear();
}

pub(crate) struct PerfEventFds {
    fds: Vec<i32>,
}

const PMCX165_EVENT: u16 = 0x165;
const PMCX165_UMASK_LOCAL_CCX: u8 = 0x02;
const PMCX165_UMASK_NEAR_CACHE: u8 = 0x04;
const PMCX165_UMASK_DRAM_NEAR: u8 = 0x08;
#[cfg(test)]
const STALL_IPC_CAP_X1000: u32 = 4_000;
const MAX_TICK_STAY_BACKOFF_EXP: u32 = 3;

fn amd_raw_event_config(event: u16, umask: u8) -> u64 {
    let event_low = u64::from(event & 0x00ff);
    let event_high = u64::from((event >> 8) & 0x000f);
    event_low | (u64::from(umask) << 8) | (event_high << 32)
}

impl PerfEventFds {
    fn attach_perf_event_map<M: libbpf_rs::MapCore>(
        map: &M,
        fds: &mut Vec<i32>,
        nr_cpu_ids: usize,
        type_: u32,
        config: u64,
    ) -> Result<()> {
        for cpu in 0..nr_cpu_ids.min(crate::types::MAX_CPU_IDS) {
            let mut attr = perf::bindings::perf_event_attr {
                type_,
                size: size_of::<perf::bindings::perf_event_attr>() as u32,
                config,
                ..unsafe { std::mem::zeroed() }
            };
            attr.set_exclude_hv(1);
            attr.set_exclude_guest(1);

            let fd = unsafe { perf::perf_event_open(&mut attr, -1, cpu as i32, -1, 0) };
            if fd < 0 {
                continue;
            }
            unsafe {
                perf::ioctls::enable(fd, 0);
            }
            let key = cpu as u32;
            map.update(key_bytes(&key), as_bytes(&fd), MapFlags::ANY)?;
            fds.push(fd);
        }
        Ok(())
    }

    pub(crate) fn attach(bpf: &mut BpfScheduler<'_>, nr_cpu_ids: usize) -> Result<Self> {
        let mut fds = Vec::new();
        Self::attach_perf_event_map(
            &bpf.skel.maps.perf_events_local_ccx,
            &mut fds,
            nr_cpu_ids,
            perf::bindings::PERF_TYPE_RAW,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_LOCAL_CCX),
        )?;
        Self::attach_perf_event_map(
            &bpf.skel.maps.perf_events_near_cache,
            &mut fds,
            nr_cpu_ids,
            perf::bindings::PERF_TYPE_RAW,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_NEAR_CACHE),
        )?;
        Self::attach_perf_event_map(
            &bpf.skel.maps.perf_events_dram_near,
            &mut fds,
            nr_cpu_ids,
            perf::bindings::PERF_TYPE_RAW,
            amd_raw_event_config(PMCX165_EVENT, PMCX165_UMASK_DRAM_NEAR),
        )?;
        Self::attach_perf_event_map(
            &bpf.skel.maps.perf_events_instructions,
            &mut fds,
            nr_cpu_ids,
            perf::bindings::PERF_TYPE_HARDWARE,
            perf::bindings::PERF_COUNT_HW_INSTRUCTIONS as u64,
        )?;
        Self::attach_perf_event_map(
            &bpf.skel.maps.perf_events_cpu_cycles,
            &mut fds,
            nr_cpu_ids,
            perf::bindings::PERF_TYPE_HARDWARE,
            perf::bindings::PERF_COUNT_HW_CPU_CYCLES as u64,
        )?;
        Ok(Self { fds })
    }
}

impl Drop for PerfEventFds {
    fn drop(&mut self) {
        for fd in &self.fds {
            unsafe {
                libc::close(*fd);
            }
        }
    }
}

pub(crate) fn update_cpu_domain_map(
    bpf: &mut BpfScheduler<'_>,
    topo: &TopologyLayout,
) -> Result<()> {
    for cpu in 0..topo.nr_cpu_ids {
        let key = cpu as u32;
        let domain = topo.cpu_to_domain[cpu].unwrap_or(u32::MAX);
        bpf.skel
            .maps
            .cpu_domain_map
            .update(key_bytes(&key), as_bytes(&domain), MapFlags::ANY)?;
        let state = CpuStateValue {
            domain_id: domain,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        };
        bpf.skel
            .maps
            .cpu_state_map
            .update(key_bytes(&key), as_bytes(&state), MapFlags::ANY)?;
    }
    Ok(())
}

pub(crate) fn read_cpu_states(
    bpf: &mut BpfScheduler<'_>,
    nr_cpu_ids: usize,
) -> Result<Vec<CpuStateValue>> {
    let mut out = Vec::with_capacity(nr_cpu_ids);
    for cpu in 0..nr_cpu_ids {
        out.push(lookup_value(&bpf.skel.maps.cpu_state_map, &(cpu as u32))?.unwrap_or_default());
    }
    Ok(out)
}

pub(crate) fn update_domain_state_maps(
    bpf: &mut BpfScheduler<'_>,
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
) -> Result<()> {
    for domain in 0..MAX_DOMAINS {
        let key = domain as u32;
        let llc = llc_states
            .get(domain)
            .copied()
            .flatten()
            .unwrap_or_default();
        let df = df_states.get(domain).copied().flatten().unwrap_or_default();
        bpf.skel
            .maps
            .llc_state_map
            .update(key_bytes(&key), as_bytes(&llc), MapFlags::ANY)?;
        bpf.skel
            .maps
            .df_state_map
            .update(key_bytes(&key), as_bytes(&df), MapFlags::ANY)?;
    }
    Ok(())
}

#[cfg(feature = "diagnostics")]
pub(crate) fn tick_stats_snapshot(bpf: &mut BpfScheduler<'_>) -> TickStatsSnapshot {
    let bss = bpf.skel.maps.bss_data.as_mut().unwrap();
    TickStatsSnapshot {
        tick_events: bss.nr_tick_events,
        tick_to_userspace: bss.nr_tick_to_userspace,
        tick_backoff_skip: bss.nr_tick_backoff_skip,
        tick_fastpath_stay: bss.nr_tick_fastpath_stay,
        tick_reslice: bss.nr_tick_reslice,
        reenqueue_local_fail: bss.nr_reenqueue_local_fail,
        villain_reslices: bss.nr_tick_reslice,
        user_dispatches: bss.nr_user_dispatches,
        kernel_dispatches: bss.nr_kernel_dispatches,
        cancel_dispatches: bss.nr_cancel_dispatches,
        bounce_dispatches: bss.nr_bounce_dispatches,
        failed_dispatches: bss.nr_failed_dispatches,
        sched_congested: bss.nr_sched_congested,
        dispatched_ringbuf_drains: bss.nr_dispatched_ringbuf_drains,
        dispatch_task_missing: bss.nr_dispatch_task_missing,
        dispatch_cpu_inserts: bss.nr_dispatch_cpu_inserts,
        dispatch_shared_inserts: bss.nr_dispatch_shared_inserts,
        dispatch_cpu_kicks: bss.nr_dispatch_cpu_kicks,
        dispatch_cpu_consumes: bss.nr_dispatch_cpu_consumes,
        dispatch_shared_consumes: bss.nr_dispatch_shared_consumes,
        dispatch_sched_consumes: bss.nr_dispatch_sched_consumes,
        stale_dispatch_rescues: bss.nr_stale_dispatch_rescues,
    }
}

pub(crate) fn next_tick_holdoff_ticks(
    entry: &mut ManagedThreadState,
    task: &QueuedTask,
    decision: &PlacementDecision,
) -> u32 {
    let is_tick_stay = matches!(decision.trigger, QueueTrigger::Tick)
        && matches!(decision.tick_decision, TickDecision::Stay)
        && !decision.defer_dispatch
        && decision.selected_domain == task_current_domain(task)
        && decision.selected_cpu == task_current_cpu(task)
        && matches!(
            decision.reason,
            DecisionReason::StayCurrentDomain
                | DecisionReason::NoCandidate
                | DecisionReason::MarginNotMet
                | DecisionReason::MigrationBudget
                | DecisionReason::ReverseHysteresis
                | DecisionReason::DestinationGuard
        );

    if !is_tick_stay {
        entry.tick_stay_backoff_exp = 0;
        return 0;
    }

    let exp = entry.tick_stay_backoff_exp.min(MAX_TICK_STAY_BACKOFF_EXP);
    entry.tick_stay_backoff_exp = exp.saturating_add(1).min(MAX_TICK_STAY_BACKOFF_EXP);
    1u32 << exp
}

#[derive(Default)]
pub(crate) struct SampleUpdateSummary {
    pub latest_df_sweep_epoch: Option<u64>,
    pub domain_state_flips: BTreeSet<u32>,
}

fn control_plane_cpu_policy_str(policy: ControlPlaneCpuPolicy) -> &'static str {
    match policy {
        ControlPlaneCpuPolicy::Mixed => "mixed",
        ControlPlaneCpuPolicy::AutoDedicated => "auto-dedicated",
        ControlPlaneCpuPolicy::Dedicated => "dedicated",
    }
}

#[cfg(feature = "diagnostics")]
fn fill_io_cs_signature_debug(record: &mut DecisionRecord, entry: Option<&ManagedThreadState>) {
    let Some(entry) = entry else {
        return;
    };
    let signature = &entry.io_cs_signature;
    record.io_cs_signature_valid = signature.valid;
    record.io_cs_signature_sample_count = signature.sample_count;
    record.io_cs_signature_confidence_x100 = signature.confidence_x100;
    record.io_cs_signature_stable = signature.stable;
    if !signature.valid {
        return;
    }
    if let Some((link_id, score)) = signature
        .df_domain_delta_x100
        .iter()
        .copied()
        .enumerate()
        .take(MAX_DOMAINS)
        .max_by_key(|(_, score)| *score)
        .filter(|(_, score)| *score > 0)
    {
        record.io_cs_top_link = Some(link_id as u32);
        record.io_cs_top_contrib_x100 = score;
    }
    if let Some((link_id, score)) = signature
        .raw_df_domain_delta_x100
        .iter()
        .copied()
        .enumerate()
        .take(MAX_DOMAINS)
        .max_by_key(|(_, score)| *score)
        .filter(|(_, score)| *score > 0)
    {
        record.io_cs_raw_top_link = Some(link_id as u32);
        record.io_cs_raw_top_contrib_x100 = score;
    }
}

fn pin_current_thread_to_cpu(cpu: u32) -> Result<()> {
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu as usize, &mut cpuset);
    }
    let ret = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        anyhow::bail!(
            "sched_setaffinity(control plane cpu={cpu}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn plan_is_current(plan: &PlacementPlan, latest_df_sweep_epoch: u64) -> bool {
    latest_df_sweep_epoch.saturating_sub(plan.built_from_sweep_epoch) <= 1
}

fn allowed_domains_from_cpus(
    cpus: &BTreeSet<u32>,
    topo: &TopologyLayout,
    mapping: &crate::types::MappingInfo,
) -> BTreeSet<u32> {
    cpus.iter()
        .copied()
        .filter_map(|cpu| topo.cpu_to_domain.get(cpu as usize).copied().flatten())
        .filter(|domain| mapping.eligible_domains.contains(domain))
        .collect()
}

fn allowed_eligible_cpus(
    allowed_cpus: &BTreeSet<u32>,
    mapping: &crate::types::MappingInfo,
) -> BTreeSet<u32> {
    allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| mapping.eligible_cpus.contains(cpu))
        .collect()
}

fn preferred_reserved_cpu_for_selected_domain(
    preferred_cpu: Option<u32>,
    selected_domain: Option<u32>,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
    task_tid: u32,
) -> Option<u32> {
    let preferred_cpu = preferred_cpu.and_then(|cpu| i32::try_from(cpu).ok())?;
    let selected_domain = selected_domain?;
    let allowed_eligible = allowed_eligible_cpus(allowed_cpus, mapping);
    let allowed_eligible_in_domain = allowed_eligible
        .iter()
        .copied()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(selected_domain)
        })
        .collect::<BTreeSet<_>>();
    let allowed_in_domain = allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(selected_domain)
        })
        .collect::<BTreeSet<_>>();
    preferred_cpu_in_domain(
        preferred_cpu,
        selected_domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        &allowed_eligible,
        cfg.cpu_high_util_x100,
        task_tid,
    )
    .filter(|cpu| {
        planned_cpu_available_for_task(
            planned_cpu_states,
            *cpu,
            task_tid,
            &allowed_eligible_in_domain,
            cpu_states,
            cpu_utils_x100,
            cfg.cpu_high_util_x100,
        )
    })
    .or_else(|| {
        preferred_cpu_in_domain(
            preferred_cpu,
            selected_domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            allowed_cpus,
            cfg.cpu_high_util_x100,
            task_tid,
        )
        .filter(|cpu| {
            planned_cpu_available_for_task(
                planned_cpu_states,
                *cpu,
                task_tid,
                &allowed_in_domain,
                cpu_states,
                cpu_utils_x100,
                cfg.cpu_high_util_x100,
            )
        })
    })
}

#[cfg(all(feature = "diagnostics", feature = "select-cpu-in-domain-debug"))]
fn fill_select_cpu_in_domain_debug(
    record: &mut DecisionRecord,
    decision: &PlacementDecision,
    target_cpu_prepass: Option<u32>,
    reserved_cpu_override_used: bool,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
) {
    record.select_cpu_in_domain_debug_enabled = true;
    record.target_cpu_prepass = target_cpu_prepass;
    record.reserved_cpu_override_used = reserved_cpu_override_used;

    if let Some(selected_cpu) = decision.selected_cpu {
        let state = cpu_states
            .get(selected_cpu as usize)
            .copied()
            .unwrap_or_default();
        record.selected_cpu_planned_jobs =
            Some(planned_jobs_len(planned_cpu_states, selected_cpu) as u32);
        record.selected_cpu_idle = Some(state.idle);
        record.selected_cpu_dsq_depth = Some(state.cpu_dsq_depth);
        record.selected_cpu_util_x100 = Some(
            cpu_utils_x100
                .get(selected_cpu as usize)
                .copied()
                .unwrap_or(0),
        );
        record.selected_cpu_current_tid = Some(state.current_tid);
    }

    let Some(selected_domain) = decision.selected_domain else {
        return;
    };
    let candidate_cpus =
        candidate_cpus_for_target_domain(allowed_cpus, selected_domain, topo, mapping);
    if candidate_cpus.is_empty() {
        return;
    }

    record.min_planned_jobs_in_domain = candidate_cpus
        .iter()
        .copied()
        .map(|cpu| planned_jobs_len(planned_cpu_states, cpu) as u32)
        .min();
    record.best_idle_cpu_in_domain = choose_idle_cpu_in_domain(
        selected_domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        &candidate_cpus,
        cfg.cpu_high_util_x100,
    );
    record.best_idle_cpu_planned_jobs = record
        .best_idle_cpu_in_domain
        .map(|cpu| planned_jobs_len(planned_cpu_states, cpu) as u32);
}

fn build_planner_input(
    topo: &TopologyLayout,
    mapping: &crate::types::MappingInfo,
    policy: PolicyConfig,
    pending: &VecDeque<QueuedTask>,
    threads: &BTreeMap<u32, ManagedThreadState>,
    affinity: &mut AffinityCache,
    now: u64,
    latest_df_sweep_epoch: u64,
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
    active_plan: Option<&PlacementPlan>,
) -> PlannerInput {
    let profiler = overhead_profile::global();
    let profile_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let mut allowed_cpus = BTreeMap::<u32, BTreeSet<u32>>::new();
    let mut allowed_domains = BTreeMap::<u32, BTreeSet<u32>>::new();
    for tid in threads.keys().copied() {
        let cpus = affinity.allowed_cpus(tid, now).unwrap_or_default();
        let domains = allowed_domains_from_cpus(&cpus, topo, mapping);
        allowed_cpus.insert(tid, cpus);
        allowed_domains.insert(tid, domains);
    }
    let snapshot = PlannerInput {
        topo: topo.clone(),
        mapping: mapping.clone(),
        policy,
        now_ns: now,
        latest_df_sweep_epoch,
        pending_tids: pending.iter().map(|task| task.tid as u32).collect(),
        threads: threads.clone(),
        allowed_cpus,
        allowed_domains,
        llc_states: llc_states.to_vec(),
        df_states: df_states.to_vec(),
        previous_plan: active_plan.cloned(),
    };
    if let (Some(profiler), Some(start)) = (profiler, profile_start) {
        if profiler.alloc_enabled() {
            let (vmrss_kib, vmhwm_kib) = overhead_profile::process_memory_kib().unwrap_or((0, 0));
            profiler.record_planner_memory(planner_input_memory_sample(
                &snapshot, vmrss_kib, vmhwm_kib,
            ));
        }
        profiler.record_scope("planner_build_input", start, true);
    }
    snapshot
}

fn planner_input_memory_sample(
    snapshot: &PlannerInput,
    vmrss_kib: u64,
    vmhwm_kib: u64,
) -> PlannerMemorySample {
    let topo_bytes = size_of::<TopologyLayout>()
        + snapshot.topo.cpu_to_domain.len() * size_of::<Option<u32>>()
        + snapshot
            .topo
            .domains
            .iter()
            .map(|domain| size_of_val(domain) + domain.cpus.len() * size_of::<u32>())
            .sum::<usize>();
    let mapping_bytes = size_of::<MappingInfo>()
        + snapshot.mapping.domain_to_ccx.len() * size_of::<u32>()
        + snapshot.mapping.domain_to_ccm.len() * size_of::<Option<u32>>()
        + snapshot.mapping.eligible_domains.len() * size_of::<u32>()
        + snapshot.mapping.excluded_domains.len() * size_of::<u32>()
        + snapshot.mapping.eligible_cpus.len() * size_of::<u32>();
    let thread_bytes = snapshot
        .threads
        .values()
        .map(|thread| size_of::<ManagedThreadState>() + thread.comm.len())
        .sum::<usize>();
    let allowed_cpus_total_members = snapshot
        .allowed_cpus
        .values()
        .map(|cpus| cpus.len())
        .sum::<usize>();
    let allowed_domains_total_members = snapshot
        .allowed_domains
        .values()
        .map(|domains| domains.len())
        .sum::<usize>();
    let allowed_cpus_bytes = snapshot.allowed_cpus.len()
        * (size_of::<u32>() + size_of::<BTreeSet<u32>>())
        + allowed_cpus_total_members * size_of::<u32>();
    let allowed_domains_bytes = snapshot.allowed_domains.len()
        * (size_of::<u32>() + size_of::<BTreeSet<u32>>())
        + allowed_domains_total_members * size_of::<u32>();
    let llc_bytes = snapshot.llc_states.len() * size_of::<Option<LlcStateValue>>();
    let df_bytes = snapshot.df_states.len() * size_of::<Option<CcmDfStateValue>>();
    let pending_bytes = snapshot.pending_tids.len() * size_of::<u32>();
    let previous_plan_entries_len = snapshot
        .previous_plan
        .as_ref()
        .map(|plan| plan.entries.len())
        .unwrap_or(0);
    let previous_plan_bytes = snapshot.previous_plan.as_ref().map_or(0usize, |plan| {
        size_of::<PlacementPlan>()
            + plan.entries.len() * (size_of::<u32>() + size_of::<PlacementPlanEntry>())
    });
    PlannerMemorySample {
        estimated_bytes: (size_of::<PlannerInput>()
            + topo_bytes
            + mapping_bytes
            + thread_bytes
            + allowed_cpus_bytes
            + allowed_domains_bytes
            + llc_bytes
            + df_bytes
            + pending_bytes
            + previous_plan_bytes) as u64,
        threads_len: snapshot.threads.len(),
        pending_tids_len: snapshot.pending_tids.len(),
        allowed_cpus_entry_count: snapshot.allowed_cpus.len(),
        allowed_cpus_total_members,
        allowed_domains_entry_count: snapshot.allowed_domains.len(),
        allowed_domains_total_members,
        previous_plan_entries_len,
        vmrss_kib,
        vmhwm_kib,
    }
}

fn choose_cpu_for_planned_domain(
    task: &QueuedTask,
    target_domain: u32,
    topo: &TopologyLayout,
    mapping: &crate::types::MappingInfo,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
) -> Option<u32> {
    let allowed_eligible = allowed_eligible_cpus(allowed_cpus, mapping);
    let allowed_eligible_in_domain = allowed_eligible
        .iter()
        .copied()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(target_domain)
        })
        .collect::<BTreeSet<_>>();
    let allowed_in_domain = allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(target_domain)
        })
        .collect::<BTreeSet<_>>();
    let current_cpu_u32 = u32::try_from(task.current_cpu).ok();
    let task_tid = task.tid as u32;
    let current_domain = u32::try_from(task.current_domain).ok();
    let tick_current_cpu = if current_domain == Some(target_domain)
        && !mapping.excluded_domains.contains(&target_domain)
        && matches!(
            QueueTrigger::from_u32(task.trigger),
            QueueTrigger::Tick | QueueTrigger::VillainReslice
        ) {
        current_cpu_in_domain(task.current_cpu, target_domain, topo, &allowed_eligible)
            .or_else(|| current_cpu_in_domain(task.current_cpu, target_domain, topo, allowed_cpus))
    } else {
        None
    };
    let current_idle_cpu = current_idle_cpu_in_domain(
        task.current_cpu,
        target_domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        &allowed_eligible,
        cfg.cpu_high_util_x100,
    )
    .filter(|cpu| {
        planned_cpu_available_for_task(
            planned_cpu_states,
            *cpu,
            task_tid,
            &allowed_eligible_in_domain,
            cpu_states,
            cpu_utils_x100,
            cfg.cpu_high_util_x100,
        )
    })
    .or_else(|| {
        current_idle_cpu_in_domain(
            task.current_cpu,
            target_domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            allowed_cpus,
            cfg.cpu_high_util_x100,
        )
        .filter(|cpu| {
            planned_cpu_available_for_task(
                planned_cpu_states,
                *cpu,
                task_tid,
                &allowed_in_domain,
                cpu_states,
                cpu_utils_x100,
                cfg.cpu_high_util_x100,
            )
        })
    });
    let best_idle_cpu = choose_idle_cpu_in_domain(
        target_domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        &allowed_eligible,
        cfg.cpu_high_util_x100,
    )
    .or_else(|| {
        choose_idle_cpu_in_domain(
            target_domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            allowed_cpus,
            cfg.cpu_high_util_x100,
        )
    });
    let rebalance_idle_cpu = best_idle_cpu.filter(|candidate| {
        should_rebalance_to_idle_sibling(current_cpu_u32, task.tid as u32, cpu_states, *candidate)
    });
    tick_current_cpu
        .or(current_idle_cpu)
        .or(rebalance_idle_cpu)
        .or_else(|| {
            current_acceptable_cpu_in_domain(
                task.current_cpu,
                target_domain,
                topo,
                cpu_states,
                &allowed_eligible,
                task.tid as u32,
            )
            .filter(|cpu| {
                planned_cpu_available_for_task(
                    planned_cpu_states,
                    *cpu,
                    task_tid,
                    &allowed_eligible_in_domain,
                    cpu_states,
                    cpu_utils_x100,
                    cfg.cpu_high_util_x100,
                )
            })
        })
        .or_else(|| {
            current_acceptable_cpu_in_domain(
                task.current_cpu,
                target_domain,
                topo,
                cpu_states,
                allowed_cpus,
                task.tid as u32,
            )
            .filter(|cpu| {
                planned_cpu_available_for_task(
                    planned_cpu_states,
                    *cpu,
                    task_tid,
                    &allowed_in_domain,
                    cpu_states,
                    cpu_utils_x100,
                    cfg.cpu_high_util_x100,
                )
            })
        })
        .or_else(|| {
            choose_cpu_in_domain(
                target_domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                planned_cpu_states,
                &allowed_eligible,
                cfg.cpu_high_util_x100,
                task.tid as u32,
            )
        })
        .or_else(|| {
            choose_cpu_in_domain(
                target_domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                planned_cpu_states,
                allowed_cpus,
                cfg.cpu_high_util_x100,
                task.tid as u32,
            )
        })
}

#[cfg(test)]
mod planned_cpu_selection_tests {
    use super::{
        build_burst_target_cpu_plan, build_planned_cpu_states, choose_cpu_for_planned_domain,
        preferred_reserved_cpu_for_selected_domain,
    };
    use crate::bpf::QueuedTask;
    use crate::types::{
        CpuStateValue, DomainInfo, MappingInfo, PlannedCpuState, PolicyConfig, QueueTrigger,
        TopologyLayout,
    };
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    fn topo() -> TopologyLayout {
        TopologyLayout {
            nr_cpu_ids: 2,
            domains: vec![DomainInfo {
                domain_id: 0,
                kernel_l3_id: 0,
                rep_cpu: 0,
                cpus: vec![0, 1],
                l3_size_mb: 32.0,
            }],
            cpu_to_domain: vec![Some(0), Some(0)],
        }
    }

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0],
            domain_to_ccm: vec![Some(0)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1]),
        }
    }

    fn cfg() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 8_500,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 5_000,
            stall_victim_delta_pct_x100: 1_500,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: true,
            cs_villain_throttle: true,
            tick_reeval_every: 1,
            tick_defer_max: 1,
            cs_villain_reslice_ns: 1_000_000,
            cs_villain_refill_divisor: 4,
            cs_villain_settle_ns: 20_000_000,
            cs_villain_release_samples: 2,
            tick_move_phase_mod: 1,
        }
    }

    fn task_with_tid(tid: u32, current_cpu: i32) -> QueuedTask {
        QueuedTask {
            tid: tid as i32,
            tgid: 100,
            current_cpu,
            current_domain: 0,
            nr_cpus_allowed: 2,
            flags: 0,
            start_ts: 0,
            stop_ts: 0,
            exec_runtime: 0,
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: 0,
            ewma_l2_bw_mib_s_x100: 0,
            last_fill_bw_mib_s_x100: [0; crate::types::MEM_SOURCE_COUNT],
            ewma_fill_bw_mib_s_x100: [0; crate::types::MEM_SOURCE_COUNT],
            last_ipc_x1000: 0,
            ewma_ipc_x1000: 0,
            last_stall_pct_x100: 0,
            ewma_stall_pct_x100: 0,
            trigger: 0,
            tick_seq: 0,
            last_update_ns: 0,
            comm: [0; crate::types::COMM_LEN],
        }
    }

    fn task(current_cpu: i32) -> QueuedTask {
        task_with_tid(101, current_cpu)
    }

    fn next_seeded_u32(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 32) as u32
    }

    #[test]
    fn planned_cpu_selection_prefers_idle_current_cpu() {
        let selected = choose_cpu_for_planned_domain(
            &task(0),
            0,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &vec![PlannedCpuState::default(); 2],
            &BTreeSet::from([0, 1]),
            cfg(),
        );

        assert_eq!(selected, Some(0));
    }

    #[test]
    fn planned_enqueue_selection_replaces_busy_current_cpu() {
        let selected = choose_cpu_for_planned_domain(
            &task(0),
            0,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 1,
                    current_tid: 202,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &vec![PlannedCpuState::default(); 2],
            &BTreeSet::from([0, 1]),
            cfg(),
        );

        assert_eq!(selected, Some(1));
    }

    #[test]
    fn planned_tick_same_domain_stay_keeps_legal_current_cpu_for_seeded_cases() {
        let mut seed = 0x5eed_c0de_51ab_cafe_u64;

        for case_idx in 0..256_u32 {
            let current_cpu = (next_seeded_u32(&mut seed) % 2) as i32;
            let current_cpu_u32 = current_cpu as u32;
            let mut task = task_with_tid(1_001 + case_idx, current_cpu);
            task.trigger = if next_seeded_u32(&mut seed) % 2 == 0 {
                QueueTrigger::Tick as u32
            } else {
                QueueTrigger::VillainReslice as u32
            };
            task.tick_seq = u64::from(next_seeded_u32(&mut seed));

            let cpu_states = (0..2)
                .map(|cpu| {
                    let owner_roll = next_seeded_u32(&mut seed) % 4;
                    CpuStateValue {
                        domain_id: 0,
                        idle: next_seeded_u32(&mut seed) % 2,
                        cpu_dsq_depth: next_seeded_u32(&mut seed) % 4,
                        current_tid: match owner_roll {
                            0 => 0,
                            1 => task.tid as u32,
                            _ => 20_000 + case_idx * 8 + cpu,
                        },
                        last_update_ns: 0,
                    }
                })
                .collect::<Vec<_>>();
            let cpu_utils_x100 = (0..2)
                .map(|_| next_seeded_u32(&mut seed) % 10_000)
                .collect::<Vec<_>>();
            let mut planned_cpu_states = vec![PlannedCpuState::default(); 2];
            for cpu in 0..2 {
                let job_count = next_seeded_u32(&mut seed) % 3;
                planned_cpu_states[cpu as usize].jobs = (0..job_count)
                    .map(|idx| 30_000 + case_idx * 8 + cpu * 3 + idx)
                    .collect();
                if next_seeded_u32(&mut seed) % 5 == 0 {
                    planned_cpu_states[cpu as usize].jobs.push(task.tid as u32);
                }
            }
            let allowed_cpus = if next_seeded_u32(&mut seed) % 3 == 0 {
                BTreeSet::from([current_cpu_u32])
            } else {
                BTreeSet::from([0, 1])
            };

            let selected = choose_cpu_for_planned_domain(
                &task,
                0,
                &topo(),
                &mapping(),
                &cpu_states,
                &cpu_utils_x100,
                &planned_cpu_states,
                &allowed_cpus,
                cfg(),
            );

            assert_eq!(
                selected,
                Some(current_cpu_u32),
                "case {case_idx} seed={seed:#x} task={task:?} cpu_states={cpu_states:?} cpu_utils={cpu_utils_x100:?} planned_cpu_states={planned_cpu_states:?} allowed={allowed_cpus:?}"
            );
        }
    }

    #[test]
    fn burst_target_cpu_plan_reserves_scarce_cpu_first() {
        let pending = VecDeque::from([task_with_tid(101, 0), task_with_tid(102, 0)]);
        let planned_domains = BTreeMap::from([(101, 0), (102, 0)]);
        let allowed_cpus_by_tid =
            BTreeMap::from([(101, BTreeSet::from([0])), (102, BTreeSet::from([0, 1]))]);
        let mut planned_cpus = BTreeMap::from([(101, 0), (102, 0)]);
        let mut planned_cpu_states = build_planned_cpu_states(2, &planned_cpus);
        let target_cpus = build_burst_target_cpu_plan(
            &pending,
            &planned_domains,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 1,
                    current_tid: 303,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &mut planned_cpu_states,
            &mut planned_cpus,
            &allowed_cpus_by_tid,
            &BTreeMap::new(),
            cfg(),
        );

        assert_eq!(target_cpus.get(&101), Some(&0));
        assert_eq!(target_cpus.get(&102), Some(&1));
        assert_eq!(planned_cpus.get(&101), Some(&0));
        assert_eq!(planned_cpus.get(&102), Some(&1));
    }

    #[test]
    fn burst_target_cpu_plan_moves_unpinned_off_reserved_pin_cpu() {
        let pending = VecDeque::from([task_with_tid(102, 1), task_with_tid(101, 1)]);
        let planned_domains = BTreeMap::from([(101, 0), (102, 0)]);
        let allowed_cpus_by_tid =
            BTreeMap::from([(101, BTreeSet::from([1])), (102, BTreeSet::from([0, 1]))]);
        let mut planned_cpus = BTreeMap::from([(101, 1), (102, 1)]);
        let mut planned_cpu_states = build_planned_cpu_states(2, &planned_cpus);
        let target_cpus = build_burst_target_cpu_plan(
            &pending,
            &planned_domains,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &mut planned_cpu_states,
            &mut planned_cpus,
            &allowed_cpus_by_tid,
            &BTreeMap::new(),
            cfg(),
        );

        assert_eq!(target_cpus.get(&101), Some(&1));
        assert_eq!(target_cpus.get(&102), Some(&0));
        assert_eq!(planned_cpus.get(&101), Some(&1));
        assert_eq!(planned_cpus.get(&102), Some(&0));
    }

    #[test]
    fn burst_target_cpu_plan_keeps_existing_plan_cpu_when_legal() {
        let pending = VecDeque::from([task_with_tid(101, 0)]);
        let planned_domains = BTreeMap::from([(101, 0)]);
        let allowed_cpus_by_tid = BTreeMap::from([(101, BTreeSet::from([0, 1]))]);
        let mut planned_cpus = BTreeMap::from([(101, 1)]);
        let mut planned_cpu_states = build_planned_cpu_states(2, &planned_cpus);
        let target_cpus = build_burst_target_cpu_plan(
            &pending,
            &planned_domains,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &mut planned_cpu_states,
            &mut planned_cpus,
            &allowed_cpus_by_tid,
            &BTreeMap::new(),
            cfg(),
        );

        assert_eq!(target_cpus.get(&101), Some(&1));
        assert_eq!(planned_cpus.get(&101), Some(&1));
    }

    #[test]
    fn burst_target_cpu_plan_keeps_fixed_planner_cpu_when_legal() {
        let pending = VecDeque::from([task_with_tid(101, 0), task_with_tid(102, 0)]);
        let planned_domains = BTreeMap::from([(101, 0), (102, 0)]);
        let allowed_cpus_by_tid =
            BTreeMap::from([(101, BTreeSet::from([0, 1])), (102, BTreeSet::from([0, 1]))]);
        let fixed_cpu_targets = BTreeMap::from([(101, 1), (102, 1)]);
        let mut planned_cpus = fixed_cpu_targets.clone();
        let mut planned_cpu_states = build_planned_cpu_states(2, &planned_cpus);
        let target_cpus = build_burst_target_cpu_plan(
            &pending,
            &planned_domains,
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &mut planned_cpu_states,
            &mut planned_cpus,
            &allowed_cpus_by_tid,
            &fixed_cpu_targets,
            cfg(),
        );

        assert_eq!(target_cpus.get(&101), Some(&1));
        assert_eq!(target_cpus.get(&102), Some(&1));
        assert_eq!(planned_cpus.get(&101), Some(&1));
        assert_eq!(planned_cpus.get(&102), Some(&1));
    }

    #[test]
    fn reserved_cpu_override_uses_prepass_target_when_still_legal() {
        let preferred = preferred_reserved_cpu_for_selected_domain(
            Some(1),
            Some(0),
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &vec![PlannedCpuState::default(); 2],
            &BTreeSet::from([0, 1]),
            cfg(),
            102,
        );

        assert_eq!(preferred, Some(1));
    }

    #[test]
    fn reserved_cpu_override_rejects_stacked_target_when_idle_peer_exists() {
        let mut planned_cpu_states = vec![PlannedCpuState::default(); 2];
        planned_cpu_states[1].jobs.extend([101, 102]);
        let preferred = preferred_reserved_cpu_for_selected_domain(
            Some(1),
            Some(0),
            &topo(),
            &mapping(),
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
            ],
            &[1_000, 1_000],
            &planned_cpu_states,
            &BTreeSet::from([0, 1]),
            cfg(),
            102,
        );

        assert_eq!(preferred, None);
    }
}

#[cfg(test)]
mod dispatch_batch_reservation_tests {
    use super::DispatchBatchReservations;
    use crate::bpf::QueuedTask;
    use crate::policy::choose_placement_with_planned_cpus;
    use crate::types::{
        CcmDfStateValue, CpuStateValue, DecisionReason, DomainInfo, HotCoolState, LlcStateValue,
        ManagedThreadState, MappingInfo, PolicyConfig, QueueTrigger, ThreadSignature,
        TopologyLayout,
    };
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    fn topo() -> TopologyLayout {
        let domains = vec![
            DomainInfo {
                domain_id: 0,
                kernel_l3_id: 0,
                rep_cpu: 0,
                cpus: vec![0, 1, 2, 3],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 1,
                kernel_l3_id: 1,
                rep_cpu: 4,
                cpus: vec![4, 5, 6, 7, 8, 9, 10],
                l3_size_mb: 32.0,
            },
        ];
        let mut cpu_to_domain = vec![None; 11];
        for domain in &domains {
            for &cpu in &domain.cpus {
                cpu_to_domain[cpu as usize] = Some(domain.domain_id);
            }
        }
        TopologyLayout {
            nr_cpu_ids: 11,
            domains,
            cpu_to_domain,
        }
    }

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: (0..=10).collect(),
        }
    }

    fn cfg() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 85_00,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 50_00,
            stall_victim_delta_pct_x100: 15_00,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: true,
            cs_villain_throttle: true,
            tick_reeval_every: 1,
            tick_defer_max: 1,
            cs_villain_reslice_ns: 1_000_000,
            cs_villain_refill_divisor: 4,
            cs_villain_settle_ns: 20_000_000,
            cs_villain_release_samples: 2,
            tick_move_phase_mod: 1,
        }
    }

    fn llc(pressure_basis_points: u32, state: HotCoolState) -> Option<LlcStateValue> {
        Some(LlcStateValue {
            sample_ts_ns: 1_000_000,
            raw_l3_bw_mib_s_x100: pressure_basis_points,
            raw_pressure_pct_x100: pressure_basis_points,
            ewma_pressure_pct_x100: pressure_basis_points,
            state: state as u32,
            valid: 1,
            ..LlcStateValue::default()
        })
    }

    fn df(read_centi_mib_per_s: u32) -> Option<CcmDfStateValue> {
        Some(CcmDfStateValue {
            sample_ts_ns: 1_000_000,
            raw_read_bw_mib_s_x100: read_centi_mib_per_s,
            raw_write_bw_mib_s_x100: 0,
            valid: 1,
            ..CcmDfStateValue::default()
        })
    }

    fn task(tid: u32, cpu: u32, domain: u32, df_centi_mib_per_s: u32) -> QueuedTask {
        QueuedTask {
            tid: tid as i32,
            tgid: tid as i32,
            current_cpu: cpu as i32,
            current_domain: domain as i32,
            nr_cpus_allowed: 11,
            flags: 0,
            start_ts: 0,
            stop_ts: 0,
            exec_runtime: 0,
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: df_centi_mib_per_s,
            ewma_l2_bw_mib_s_x100: df_centi_mib_per_s,
            last_fill_bw_mib_s_x100: [0; crate::types::MEM_SOURCE_COUNT],
            ewma_fill_bw_mib_s_x100: [0; crate::types::MEM_SOURCE_COUNT],
            last_ipc_x1000: 0,
            ewma_ipc_x1000: 0,
            last_stall_pct_x100: 0,
            ewma_stall_pct_x100: 0,
            trigger: QueueTrigger::Enqueue as u32,
            tick_seq: 0,
            last_update_ns: 1_000_000,
            comm: [0; crate::types::COMM_LEN],
        }
    }

    fn thread(tid: u32, cpu: u32, domain: u32, df_centi_mib_per_s: u32) -> ManagedThreadState {
        ManagedThreadState {
            tid,
            tgid: tid,
            last_seen_ns: 1_000_000,
            last_observed_domain: Some(domain),
            last_observed_cpu: Some(cpu),
            signature: ThreadSignature {
                valid: true,
                stable: true,
                sample_count: 2,
                confidence_x100: 90_00,
                projected_df_pressure_x100: df_centi_mib_per_s,
                projected_llc_pressure_x100: 10_00,
                ..ThreadSignature::default()
            },
            ..ManagedThreadState::default()
        }
    }

    fn cpu_states() -> Vec<CpuStateValue> {
        let mut states = vec![CpuStateValue::default(); 11];
        for cpu in 0..=3 {
            states[cpu] = CpuStateValue {
                domain_id: 0,
                idle: 0,
                cpu_dsq_depth: 1,
                current_tid: 101 + cpu as u32,
                last_update_ns: 1_000_000,
            };
        }
        for cpu in 4..=7 {
            states[cpu] = CpuStateValue {
                domain_id: 1,
                idle: 0,
                cpu_dsq_depth: 1,
                current_tid: 201 + (cpu as u32 - 4),
                last_update_ns: 1_000_000,
            };
        }
        for cpu in 8..=10 {
            states[cpu] = CpuStateValue {
                domain_id: 1,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 1_000_000,
            };
        }
        states
    }

    // Verifies the production dispatch semantics at the helper boundary.
    // Expected: after the first greedy decision is committed to the mutable
    // batch reservation ledger, the next policy decision sees the updated
    // domain pressure and CPU reservation instead of the original batch state.
    #[test]
    fn dispatch_batch_reservations_commit_updates_follow_on_policy_view() {
        let topo = topo();
        let mapping = mapping();
        let mut threads = BTreeMap::new();
        for (tid, cpu) in [(101, 0), (102, 1), (103, 2), (104, 3)] {
            threads.insert(tid, thread(tid, cpu, 0, 1_000_000));
        }
        for (tid, cpu) in [(201, 4), (202, 5), (203, 6), (204, 7)] {
            threads.insert(tid, thread(tid, cpu, 1, 100_000));
        }
        let first_task = task(101, 0, 0, 1_000_000);
        let second_task = task(102, 1, 0, 1_000_000);
        let pending = VecDeque::from([first_task.clone(), second_task.clone()]);
        let mut reservations = DispatchBatchReservations::from_snapshot(
            &topo,
            &threads,
            &pending,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        let llc_states = vec![
            llc(90_00, HotCoolState::Hot),
            llc(10_00, HotCoolState::Cool),
        ];
        let df_states = vec![df(7_000_000), df(0)];
        let cpu_states = cpu_states();
        let cpu_utils = vec![10_00; 11];
        let allowed_cpus = BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let scheduler_now_ns = 2_000_000;
        let policy_cfg = cfg();

        let first_decision = choose_placement_with_planned_cpus(
            &first_task,
            threads.get(&101),
            &topo,
            &mapping,
            reservations.domain_states(),
            &llc_states,
            &df_states,
            &cpu_states,
            &cpu_utils,
            reservations.cpu_states(),
            &allowed_cpus,
            scheduler_now_ns,
            policy_cfg,
        );

        assert_eq!(first_decision.reason, DecisionReason::MoveCoolerDomain);
        assert_eq!(first_decision.selected_domain, Some(1));
        assert_eq!(first_decision.selected_cpu, Some(8));
        reservations.commit_domain_decision(&first_task, threads.get(&101), &mapping, 1);
        reservations.commit_cpu_decision(&first_task, 8);

        assert_eq!(reservations.domain_states()[0].task_count, 3);
        assert_eq!(reservations.domain_states()[1].task_count, 5);
        assert!(reservations.cpu_states()[8].jobs.contains(&101));

        let second_decision = choose_placement_with_planned_cpus(
            &second_task,
            threads.get(&102),
            &topo,
            &mapping,
            reservations.domain_states(),
            &llc_states,
            &df_states,
            &cpu_states,
            &cpu_utils,
            reservations.cpu_states(),
            &allowed_cpus,
            scheduler_now_ns,
            policy_cfg,
        );

        assert_eq!(second_decision.reason, DecisionReason::MoveCoolerDomain);
        assert_eq!(second_decision.selected_domain, Some(1));
        assert_eq!(second_decision.selected_cpu, Some(9));
    }
}

fn planned_decision(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    plan: &PlacementPlan,
    topo: &TopologyLayout,
    mapping: &crate::types::MappingInfo,
    planned_states: &[PlannedDomainState],
    planned_cpu_states: &[PlannedCpuState],
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus: &BTreeSet<u32>,
    now_ns: u64,
    cfg: PolicyConfig,
    latest_df_sweep_epoch: u64,
) -> std::result::Result<PlacementDecision, &'static str> {
    if !plan_is_current(plan, latest_df_sweep_epoch) {
        return Err("stale_plan");
    }
    let Some(entry) = plan.entries.get(&(task.tid as u32)) else {
        return Err("no_plan");
    };
    let target_domain = entry.target_domain;
    let current_domain = u32::try_from(task.current_domain).ok();
    let current_excluded = current_domain
        .map(|domain| mapping.excluded_domains.contains(&domain))
        .unwrap_or(true);
    let legal_domains = allowed_domains_from_cpus(allowed_cpus, topo, mapping);
    if !legal_domains.contains(&target_domain) && current_domain != Some(target_domain) {
        return Err("illegal_domain");
    }
    if let Some(thread) = meta {
        if !current_excluded && current_domain != Some(target_domain) {
            if now_ns < thread.settle_until_ns {
                return Err("settle_window");
            }
            if now_ns < thread.reverse_protect_until_ns
                && thread.last_migration_to_domain == current_domain
                && thread.last_migration_from_domain == Some(target_domain)
            {
                return Err("reverse_protect");
            }
        }
    }
    if !current_excluded && current_domain != Some(target_domain) {
        if let Some(source_domain) = current_domain {
            if planned_states
                .get(source_domain as usize)
                .map(|state| state.outgoing_migrations >= state.migration_budget)
                .unwrap_or(false)
            {
                return Err("source_migration_budget");
            }
        }
    }
    if current_domain != Some(target_domain)
        && planned_states
            .get(target_domain as usize)
            .map(|state| state.incoming_migrations >= state.migration_budget)
            .unwrap_or(false)
    {
        return Err("destination_migration_budget");
    }

    let target_cpu = entry.target_cpu.filter(|cpu| {
        candidate_cpus_for_target_domain(allowed_cpus, target_domain, topo, mapping).contains(cpu)
    });
    let Some(selected_cpu) = target_cpu.or_else(|| {
        choose_cpu_for_planned_domain(
            task,
            target_domain,
            topo,
            mapping,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            allowed_cpus,
            cfg,
        )
    }) else {
        return Err("no_cpu");
    };

    let llc_state =
        current_domain.and_then(|domain| llc_states.get(domain as usize).copied().flatten());
    let class = classify_task(task, llc_state, now_ns, cfg);
    let reason = if current_domain == Some(target_domain) {
        DecisionReason::StayCurrentDomain
    } else if current_excluded {
        DecisionReason::ExcludedDomainEscape
    } else {
        DecisionReason::MoveCoolerDomain
    };
    let trigger = QueueTrigger::from_u32(task.trigger);
    let tick_decision = if matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice) {
        if current_domain == Some(target_domain) {
            TickDecision::Stay
        } else {
            TickDecision::Move
        }
    } else {
        TickDecision::None
    };

    let current_source_df_x100 = current_domain
        .and_then(|domain| planned_states.get(domain as usize))
        .map(|state| state.df_pressure_x100)
        .unwrap_or(0);
    let current_source_llc_x100 = current_domain
        .and_then(|domain| planned_states.get(domain as usize))
        .map(|state| state.llc_pressure_x100)
        .unwrap_or(0);
    let current_destination_df_x100 = planned_states
        .get(target_domain as usize)
        .map(|state| state.df_pressure_x100)
        .unwrap_or(0);
    let current_destination_llc_x100 = planned_states
        .get(target_domain as usize)
        .map(|state| state.llc_pressure_x100)
        .unwrap_or(0);
    let source_capacity_mib_s_x100 = current_domain
        .map(|domain| mapping.df_capacity_mib_s_x100(domain))
        .unwrap_or_else(|| mapping.max_df_capacity_mib_s_x100());
    let source_effect = task_signature_effect(task, meta, source_capacity_mib_s_x100);
    let destination_effect =
        task_signature_effect(task, meta, mapping.df_capacity_mib_s_x100(target_domain));
    let current_source_live_df_x100 = current_domain
        .and_then(|domain| df_states.get(domain as usize).copied().flatten())
        .map(|value| {
            value
                .raw_read_bw_mib_s_x100
                .saturating_add(value.raw_write_bw_mib_s_x100)
        })
        .unwrap_or(0);

    Ok(PlacementDecision {
        class,
        selected_domain: Some(target_domain),
        selected_cpu: Some(selected_cpu),
        reason,
        trigger,
        tick_decision,
        signature_valid: meta.map(|thread| thread.signature.valid).unwrap_or(false),
        villain_score: 0,
        defer_dispatch: false,
        score_gain: 0,
        slice_ns: DEFAULT_SLICE_US * 1_000,
        current_source_df_x100,
        predicted_source_df_x100: current_source_df_x100.saturating_sub(source_effect.df_x100),
        current_source_llc_x100,
        predicted_source_llc_x100: current_source_llc_x100.saturating_sub(source_effect.llc_x100),
        current_destination_df_x100,
        predicted_destination_df_x100: current_destination_df_x100
            .saturating_add(destination_effect.df_x100),
        current_destination_llc_x100,
        predicted_destination_llc_x100: current_destination_llc_x100
            .saturating_add(destination_effect.llc_x100)
            .min(10_000),
        signature_sample_count: meta
            .map(|thread| thread.signature.sample_count)
            .unwrap_or(0),
        signature_confidence_x100: meta
            .map(|thread| thread.signature.confidence_x100)
            .unwrap_or(0),
        signature_stable: meta.map(|thread| thread.signature.stable).unwrap_or(false),
        planner_epoch: entry.built_from_sweep_epoch,
        plan_revision: entry.plan_revision,
        plan_used: true,
        fallback_reason: "",
        planned_domain: Some(target_domain),
        planned_cpu: entry.target_cpu,
        sync_group_id: entry.sync_group_id,
        sync_anchor_domain: entry.sync_anchor_domain,
        sync_override: entry.sync_override,
        debug: crate::policy::PlacementDebugInfo {
            live_source_df_x100: current_source_live_df_x100,
            ..crate::policy::PlacementDebugInfo::default()
        },
    })
}

#[cfg(feature = "light-compete")]
fn set_internal_tgid(bpf: &mut BpfScheduler<'_>, tgid: u32, present: bool) -> Result<()> {
    if present {
        let value = 1u8;
        bpf.skel.maps.internal_tgids.update(
            key_bytes(&tgid),
            std::slice::from_ref(&value),
            MapFlags::ANY,
        )?;
    } else {
        let _ = bpf.skel.maps.internal_tgids.delete(key_bytes(&tgid));
    }
    Ok(())
}

pub(crate) fn apply_sample_updates(
    llc_rx: &Receiver<crate::types::LlcSampleUpdate>,
    df_rx: &Receiver<DfSamplerEvent>,
    df_cs_rx: &Receiver<crate::types::DfSampleUpdate>,
    llc_states: &mut [Option<LlcStateValue>],
    df_states: &mut [Option<CcmDfStateValue>],
    df_cs_states: &mut [Option<CcmDfStateValue>],
) -> SampleUpdateSummary {
    let profiler = overhead_profile::global();
    let profile_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let mut summary = SampleUpdateSummary::default();
    while let Ok(update) = llc_rx.try_recv() {
        let previous_state = llc_states
            .get(update.domain_id as usize)
            .copied()
            .flatten()
            .map(|value| value.state);
        if let Some(slot) = llc_states.get_mut(update.domain_id as usize) {
            *slot = Some(LlcStateValue {
                sample_ts_ns: update.metric.sample_ts_ns,
                raw_l3_to_l3_ns_x100: update.raw_l3_to_l3_ns_x100,
                raw_dram_lat_ns_x100: update.raw_dram_lat_ns_x100,
                raw_l3_bw_mib_s_x100: update.raw_l3_bw_mib_s_x100,
                raw_miss_ratio_ppm: update.raw_miss_ratio_ppm,
                raw_l3_req_per_s: update.raw_l3_req_per_s,
                raw_l3_miss_per_s: update.raw_l3_miss_per_s,
                raw_pressure_pct_x100: update.metric.raw_x100,
                ewma_pressure_pct_x100: update.metric.ewma_x100,
                state: update.metric.state as u32,
                hot_count: update.metric.hot_count,
                cool_count: update.metric.cool_count,
                valid: u32::from(update.metric.valid),
            });
            if previous_state != Some(update.metric.state as u32) {
                summary.domain_state_flips.insert(update.domain_id);
            }
        }
    }

    while let Ok(event) = df_rx.try_recv() {
        match event {
            DfSamplerEvent::Sample(update) => {
                let previous_state = df_states
                    .get(update.domain_id as usize)
                    .copied()
                    .flatten()
                    .map(|value| value.state);
                if let Some(slot) = df_states.get_mut(update.domain_id as usize) {
                    *slot = Some(CcmDfStateValue {
                        sample_ts_ns: update.metric.sample_ts_ns,
                        ccx_id: update.ccx_id,
                        ccm_id: update.ccm_id,
                        raw_read_bw_mib_s_x100: update.raw_read_bw_mib_s_x100,
                        raw_write_bw_mib_s_x100: update.raw_write_bw_mib_s_x100,
                        raw_pressure_pct_x100: update.metric.raw_x100,
                        ewma_pressure_pct_x100: update.metric.ewma_x100,
                        state: update.metric.state as u32,
                        hot_count: update.metric.hot_count,
                        cool_count: update.metric.cool_count,
                        valid: u32::from(update.metric.valid),
                    });
                    if previous_state != Some(update.metric.state as u32) {
                        summary.domain_state_flips.insert(update.domain_id);
                    }
                }
            }
            DfSamplerEvent::SweepComplete { epoch, .. } => {
                summary.latest_df_sweep_epoch = Some(epoch);
            }
        }
    }
    while let Ok(update) = df_cs_rx.try_recv() {
        if let Some(slot) = df_cs_states.get_mut(update.domain_id as usize) {
            *slot = Some(CcmDfStateValue {
                sample_ts_ns: update.metric.sample_ts_ns,
                ccx_id: update.ccx_id,
                ccm_id: update.ccm_id,
                raw_read_bw_mib_s_x100: update.raw_read_bw_mib_s_x100,
                raw_write_bw_mib_s_x100: update.raw_write_bw_mib_s_x100,
                raw_pressure_pct_x100: update.metric.raw_x100,
                ewma_pressure_pct_x100: update.metric.ewma_x100,
                state: update.metric.state as u32,
                hot_count: update.metric.hot_count,
                cool_count: update.metric.cool_count,
                valid: u32::from(update.metric.valid),
            });
        }
    }
    if let (Some(profiler), Some(start)) = (profiler, profile_start) {
        profiler.record_scope("main_apply_sample_updates", start, true);
    }
    summary
}

pub(crate) fn prune_dead_threads(
    threads: &mut BTreeMap<u32, ManagedThreadState>,
    affinity: &mut AffinityCache,
) {
    let stale: Vec<u32> = threads
        .keys()
        .copied()
        .filter(|tid| !Path::new(&format!("/proc/{tid}")).exists())
        .collect();
    for tid in stale {
        threads.remove(&tid);
        affinity.invalidate(tid);
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmptyRuntimeMode {
    Minimal,
    Observed,
    ObservedSharedSixth,
}

impl EmptyRuntimeMode {
    fn observed(self) -> bool {
        matches!(self, Self::Observed | Self::ObservedSharedSixth)
    }

    fn shared_dsq_interval(self) -> u64 {
        match self {
            Self::ObservedSharedSixth => 6,
            Self::Minimal | Self::Observed => 1,
        }
    }

    fn scheduler_name(self) -> &'static str {
        match self {
            Self::Minimal => "rustland_empty_min",
            Self::Observed => "rustland_empty_obs",
            Self::ObservedSharedSixth => "rustland_empty_1in6",
        }
    }

    fn log_name(self) -> &'static str {
        match self {
            Self::Minimal => "empty-minimal",
            Self::Observed => "empty-observed",
            Self::ObservedSharedSixth => "empty-observed-shared-sixth",
        }
    }
}

fn empty_policy_decision(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    selected_domain: Option<u32>,
    selected_cpu: Option<u32>,
) -> PlacementDecision {
    let trigger = QueueTrigger::from_u32(task.trigger);
    let tick_decision = if matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice) {
        TickDecision::Stay
    } else {
        TickDecision::None
    };
    PlacementDecision {
        class: meta
            .map(|thread| thread.last_class)
            .unwrap_or(ThreadClass::Cold),
        selected_domain,
        selected_cpu,
        reason: DecisionReason::StayCurrentDomain,
        trigger,
        tick_decision,
        signature_valid: meta.map(|thread| thread.signature.valid).unwrap_or(false),
        villain_score: 0,
        defer_dispatch: false,
        score_gain: 0,
        slice_ns: DEFAULT_SLICE_US * 1_000,
        current_source_df_x100: 0,
        predicted_source_df_x100: 0,
        current_source_llc_x100: 0,
        predicted_source_llc_x100: 0,
        current_destination_df_x100: 0,
        predicted_destination_df_x100: 0,
        current_destination_llc_x100: 0,
        predicted_destination_llc_x100: 0,
        signature_sample_count: meta
            .map(|thread| thread.signature.sample_count)
            .unwrap_or(0),
        signature_confidence_x100: meta
            .map(|thread| thread.signature.confidence_x100)
            .unwrap_or(0),
        signature_stable: meta.map(|thread| thread.signature.stable).unwrap_or(false),
        planner_epoch: 0,
        plan_revision: 0,
        plan_used: false,
        fallback_reason: "empty_policy",
        planned_domain: None,
        planned_cpu: None,
        sync_group_id: None,
        sync_anchor_domain: None,
        sync_override: false,
        debug: PlacementDebugInfo::default(),
    }
}

fn empty_dispatch_cpu(task: &QueuedTask, dispatch_seq: u64, shared_dsq_interval: u64) -> i32 {
    if shared_dsq_interval <= 1 || dispatch_seq % shared_dsq_interval == 0 {
        RL_CPU_ANY
    } else if task.current_cpu >= 0 {
        task.current_cpu
    } else {
        RL_CPU_ANY
    }
}

#[allow(dead_code)]
fn run_empty(mode: EmptyRuntimeMode) -> Result<()> {
    let opts = Opts::parse();
    setup_logging(opts.verbose)?;
    #[cfg(feature = "diagnostics")]
    let runtime_log_path = opts.runtime_log_path.as_deref();
    #[cfg(not(feature = "diagnostics"))]
    let runtime_log_path = None;
    runtime_log::init(runtime_log_path, opts.verbose)?;
    overhead_profile::init_global(opts.overhead_profile_path.as_ref().map(|path| {
        OverheadProfileConfig {
            path: path.clone(),
            interval: Duration::from_millis(opts.overhead_profile_interval_ms.max(1)),
            alloc_enabled: opts.overhead_profile_alloc,
        }
    }))?;
    runtime_log::line(format!("empty_scheduler mode={}", mode.log_name()));
    try_set_rlimit_infinity();
    ensure_current_thread_sched_other()?;
    let usersched_pid = std::process::id();

    let topo = topology::discover()?;
    let host_mapping = parse_host_mapping(Path::new(&opts.ccm_mapping_path))?;
    let mut mapping = build_mapping_from_host_mapping(&topo, &host_mapping)?;
    apply_host_cpu_filters(
        &topo,
        &mut mapping,
        opts.managed_cpu_max,
        opts.primary_smt_only,
    )?;

    let cgroup_path = PathBuf::from(&opts.cgroup_path);
    ensure_cgroup_dir(&cgroup_path)?;
    if opts.restrict_mapped_cpus {
        restrict_cgroup_cpus(&cgroup_path, &eligible_cpulist(&mapping))?;
    }
    let mut cgroup_guard = CgroupGuard::new(cgroup_path.clone(), opts.verbose);
    if opts.guard_clean_cgroup {
        cgroup_guard.assert_clean_start()?;
    }
    let control_plane_cpu =
        maybe_dedicated_control_plane_cpu(opts.control_plane_cpu_policy, &topo, &mapping)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (llc_tx, llc_rx) = unbounded();
    let (df_tx, df_rx) = unbounded();
    let (df_cs_tx, df_cs_rx) = unbounded();
    let mut llc_handles = Vec::new();
    let mut df_handle = None;
    let mut df_cs_handle = None;
    if mode.observed() {
        llc_handles = start_llc_sampler(
            topo.clone(),
            LlcSamplerConfig {
                window_ms: opts.llc_window_ms,
                period_ms: opts.llc_period_ms,
                dram_hot_ns_x100: pct_to_x100(opts.dram_hot_ns),
                miss_ppm_hot: opts.miss_ppm_hot,
                filter: FilterConfig {
                    alpha_pct: opts.llc_ewma_alpha_pct,
                    hot_pct_x100: pct_to_x100(opts.llc_hot_pct),
                    cool_pct_x100: pct_to_x100(opts.llc_cool_pct),
                    hot_persist: opts.llc_hot_persist,
                    cool_persist: opts.llc_cool_persist,
                },
            },
            Arc::clone(&stop),
            llc_tx,
        );
        df_handle = Some(start_df_sampler(
            mapping.clone(),
            DfSamplerConfig {
                window_ms: opts.df_window_ms,
                filter: FilterConfig {
                    alpha_pct: opts.df_ewma_alpha_pct,
                    hot_pct_x100: pct_to_x100(opts.df_hot_pct),
                    cool_pct_x100: pct_to_x100(opts.df_cool_pct),
                    hot_persist: opts.df_hot_persist,
                    cool_persist: opts.df_cool_persist,
                },
            },
            Arc::clone(&stop),
            df_tx,
            control_plane_cpu,
        ));
        df_cs_handle = Some(start_df_cs_sampler(
            topo.clone(),
            mapping.clone(),
            DfCsSamplerConfig {
                window_ms: opts.df_window_ms,
                filter: FilterConfig {
                    alpha_pct: opts.df_ewma_alpha_pct,
                    hot_pct_x100: pct_to_x100(opts.df_hot_pct),
                    cool_pct_x100: pct_to_x100(opts.df_cool_pct),
                    hot_persist: opts.df_hot_persist,
                    cool_persist: opts.df_cool_persist,
                },
            },
            Arc::clone(&stop),
            df_cs_tx,
            control_plane_cpu,
        ));
    }

    let mut open_object = MaybeUninit::<OpenObject>::uninit();
    let mut bpf = BpfScheduler::init(
        &mut open_object,
        None,
        0,
        true,
        opts.verbose,
        true,
        DEFAULT_SLICE_US * 1_000,
        opts.tick_reeval_every.max(1),
        opts.l2_need_mib_s.saturating_mul(100),
        opts.tick_defer_max.max(1),
        opts.llc_stale_ms
            .max(opts.llc_period_ms.saturating_mul(2))
            .saturating_mul(1_000_000),
        opts.df_stale_ms
            .max(opts.df_window_ms.saturating_mul(2))
            .saturating_mul(1_000_000),
        mode.scheduler_name(),
    )?;
    update_cpu_domain_map(&mut bpf, &topo)?;
    let _perf_events = PerfEventFds::attach(&mut bpf, topo.nr_cpu_ids)?;

    let mut cpu_util = mode
        .observed()
        .then(|| CpuUtilTracker::new(topo.nr_cpu_ids));
    let cpu_util_every = Duration::from_millis(opts.cpu_util_sample_ms.max(10));
    if let Some(cpu_util) = cpu_util.as_mut() {
        let _ = cpu_util.sample(topo.nr_cpu_ids);
    }
    let has_managed_workload = !opts.command.is_empty() || !opts.spawn_shell.is_empty();
    if mode.observed() && has_managed_workload {
        std::thread::sleep(cpu_util_every);
        if let Some(cpu_util) = cpu_util.as_mut() {
            let _ = cpu_util.sample(topo.nr_cpu_ids);
        }
    }

    let mut workloads = Vec::<WorkloadProcess>::new();
    if !opts.command.is_empty() {
        let child = launch_workload(&opts.command, &cgroup_path, opts.verbose)?;
        cgroup_guard.allow_root(child.id());
        workloads.push(WorkloadProcess {
            label: opts.command.join(" "),
            child,
            exited_at: None,
        });
    }
    for command_text in &opts.spawn_shell {
        let child = launch_shell_workload(command_text, &cgroup_path, opts.verbose)?;
        cgroup_guard.allow_root(child.id());
        workloads.push(WorkloadProcess {
            label: command_text.clone(),
            child,
            exited_at: None,
        });
        if opts.spawn_stagger_ms > 0 {
            std::thread::sleep(Duration::from_millis(opts.spawn_stagger_ms));
        }
    }
    BpfScheduler::enter_sched_ext_current_thread()?;
    if opts.verbose {
        for workload in &workloads {
            crate::diag_line!(
                "startup_diag usersched_pid={} workload_pid={} workload_label={}",
                usersched_pid,
                workload.child.id(),
                workload.label,
            );
        }
    }
    let mut adopters = workloads
        .iter()
        .map(|workload| {
            WorkloadAdopter::new(workload.child.id(), cgroup_path.clone(), opts.verbose)
        })
        .collect::<Vec<_>>();

    let mut llc_states = vec![None; topo.domains.len()];
    let mut df_states = vec![None; topo.domains.len()];
    let mut df_cs_states = vec![None; mapping.cs_link_count()];
    let mut pending = VecDeque::<QueuedTask>::new();
    let mut threads = BTreeMap::<u32, ManagedThreadState>::new();
    #[cfg(feature = "diagnostics")]
    let mut recent = VecDeque::with_capacity(64);
    #[cfg(feature = "diagnostics")]
    let mut logger = if mode.observed() {
        Some(DecisionLogger::new(
            opts.decision_log_path.as_deref(),
            opts.verbose,
        )?)
    } else {
        None
    };
    let mut affinity = AffinityCache::new(100);
    let mut last_cpu_util = Instant::now();
    #[cfg(feature = "diagnostics")]
    let mut last_monitor = Instant::now();
    #[cfg(feature = "diagnostics")]
    let monitor_every = if mode.observed() {
        opts.monitor.map(Duration::from_secs_f64)
    } else {
        None
    };
    let mut last_guard_check = Instant::now() - Duration::from_secs(1);
    let guard_every = Duration::from_millis(250);
    let df_stale_ms = opts.df_stale_ms.max(opts.df_window_ms.saturating_mul(2));
    let overhead_profiler = overhead_profile::global();
    let shared_dsq_interval = mode.shared_dsq_interval();
    let mut empty_dispatch_seq = 0_u64;

    if let Some(cpu) = control_plane_cpu {
        pin_current_thread_to_cpu(cpu)?;
        if opts.verbose {
            crate::diag_line!(
                "control_plane_cpu policy={} cpu={}",
                control_plane_cpu_policy_str(opts.control_plane_cpu_policy),
                cpu
            );
        }
    }

    while !bpf.exited() {
        let loop_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        for adopter in adopters.iter_mut() {
            adopter.maybe_sync();
        }
        if opts.guard_clean_cgroup && last_guard_check.elapsed() >= guard_every {
            let mut tracked = BTreeSet::new();
            for adopter in &adopters {
                tracked.extend(adopter.tracked_pids());
            }
            cgroup_guard.set_allowed_anchors(tracked);
            cgroup_guard.assert_runtime_membership()?;
            last_guard_check = Instant::now();
        }
        if mode.observed() {
            apply_sample_updates(
                &llc_rx,
                &df_rx,
                &df_cs_rx,
                &mut llc_states,
                &mut df_states,
                &mut df_cs_states,
            );
            update_domain_state_maps(&mut bpf, &llc_states, &df_states)?;
            prune_dead_threads(&mut threads, &mut affinity);

            if last_cpu_util.elapsed() >= cpu_util_every {
                if let Some(cpu_util) = cpu_util.as_mut() {
                    let _ = cpu_util.sample(topo.nr_cpu_ids);
                }
                last_cpu_util = Instant::now();
            }
        }

        let dequeue_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        loop {
            match bpf.dequeue_task() {
                Ok(Some(task)) => pending.push_back(task),
                Ok(None) => break,
                Err(err) => {
                    log::warn!("ringbuf dequeue error: {err}");
                    break;
                }
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, dequeue_start) {
            profiler.record_scope("main_ringbuf_drain", start, true);
        }
        bpf.publish_scheduled(pending.len() as u64);

        let dispatch_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        while let Some(task) = pending.pop_front() {
            let observed_task = mode.observed() && task.tgid as u32 != usersched_pid;
            let now = now_ns();
            let tid = task.tid as u32;
            empty_dispatch_seq = empty_dispatch_seq.wrapping_add(1);
            let dispatch_cpu = empty_dispatch_cpu(&task, empty_dispatch_seq, shared_dsq_interval);
            let selected_cpu = u32::try_from(dispatch_cpu).ok();
            let selected_domain = selected_cpu
                .and_then(|cpu| topo.cpu_to_domain.get(cpu as usize).copied().flatten());
            if observed_task {
                let entry = threads.entry(tid).or_default();
                refresh_thread_from_task(entry, &task, now);
                if opts.signature_snapshots {
                    update_thread_signature(entry, &task, &mapping, &df_states, now, df_stale_ms);
                    update_thread_io_cs_signature(
                        entry,
                        &task,
                        &mapping,
                        &df_cs_states,
                        now,
                        df_stale_ms,
                    );
                    if matches!(
                        QueueTrigger::from_u32(task.trigger),
                        QueueTrigger::Tick | QueueTrigger::VillainReslice
                    ) {
                        entry.last_tick_seen_ns = now;
                    }
                }
            }

            let decision = if observed_task {
                empty_policy_decision(&task, threads.get(&tid), selected_domain, selected_cpu)
            } else {
                empty_policy_decision(&task, None, selected_domain, selected_cpu)
            };
            let mut dispatched = DispatchedTask::new(&task);
            dispatched.cpu = dispatch_cpu;
            dispatched.slice_ns = decision.slice_ns;
            if bpf.dispatch_task(&dispatched).is_err() {
                pending.push_front(task);
                break;
            }

            if observed_task {
                let comm = crate::types::comm_to_string(&task.comm);
                #[cfg(feature = "diagnostics")]
                if let Some(logger) = logger.as_mut() {
                    let mut record = build_record(&task, &decision, comm.clone());
                    record.control_plane_cpu_policy =
                        control_plane_cpu_policy_str(opts.control_plane_cpu_policy).to_string();
                    record.control_plane_cpu = control_plane_cpu;
                    fill_io_cs_signature_debug(&mut record, threads.get(&tid));
                    logger.write(&record)?;
                    if recent.len() >= 64 {
                        recent.pop_front();
                    }
                    recent.push_back(record);
                }

                let selected_domain = decision
                    .selected_domain
                    .or_else(|| task_current_domain(&task));
                let selected_cpu = decision.selected_cpu.or_else(|| task_current_cpu(&task));
                let entry = threads.entry(tid).or_default();
                entry.last_class = decision.class;
                entry.last_selected_domain = selected_domain;
                entry.last_selected_cpu = selected_cpu;
                if opts.signature_snapshots {
                    entry.dispatch_snapshot = capture_dispatch_snapshot(
                        &df_states,
                        now,
                        df_stale_ms,
                        selected_domain,
                        selected_cpu,
                    );
                    entry.villain_dispatch_snapshot = capture_dispatch_snapshot(
                        &df_cs_states,
                        now,
                        df_stale_ms,
                        selected_domain,
                        selected_cpu,
                    );
                }
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, dispatch_start) {
            profiler.record_scope("main_dispatch_loop", start, true);
        }

        let notify_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        bpf.notify_complete(pending.len() as u64);
        if let (Some(profiler), Some(start)) = (&overhead_profiler, notify_start) {
            profiler.record_scope("main_notify_complete", start, true);
        }
        overhead_profile::flush_global(false);

        #[cfg(feature = "diagnostics")]
        if let Some(interval) = monitor_every {
            if last_monitor.elapsed() >= interval {
                let tick_stats = tick_stats_snapshot(&mut bpf);
                runtime_log::line(monitor::render(
                    pending.len(),
                    &topo,
                    &mapping,
                    &llc_states,
                    &df_states,
                    Some(&df_cs_states),
                    &threads,
                    tick_stats,
                    &recent,
                ));
                last_monitor = Instant::now();
            }
        }

        let workload_wait_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        for workload in workloads.iter_mut() {
            if workload.child.try_wait()?.is_some() && workload.exited_at.is_none() {
                workload.exited_at = Some(Instant::now());
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, workload_wait_start) {
            profiler.record_scope("main_workload_try_wait", start, true);
        }
        let all_workloads_done = !workloads.is_empty()
            && workloads.iter().all(|workload| {
                workload
                    .exited_at
                    .map(|ts| ts.elapsed() >= Duration::from_millis(250))
                    .unwrap_or(false)
            });
        if all_workloads_done && pending.is_empty() {
            break;
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, loop_start) {
            profiler.record_scope("main_loop_total", start, true);
        }
    }

    stop.store(true, Ordering::Relaxed);
    for handle in llc_handles {
        let _ = handle.join();
    }
    if let Some(handle) = df_handle {
        let _ = handle.join();
    }
    if let Some(handle) = df_cs_handle {
        let _ = handle.join();
    }

    for workload in workloads.iter_mut() {
        shutdown_child_gracefully(&mut workload.child);
    }

    overhead_profile::shutdown_global();
    let _ = bpf.shutdown_and_report()?;
    Ok(())
}

pub fn run() -> Result<()> {
    let empty_runner: Option<fn() -> Result<()>> = {
        #[cfg(feature = "scheduler-empty-minimal")]
        {
            Some(|| run_empty(EmptyRuntimeMode::Minimal))
        }
        #[cfg(feature = "scheduler-empty-observed")]
        {
            Some(|| run_empty(EmptyRuntimeMode::Observed))
        }
        #[cfg(feature = "scheduler-empty-observed-shared-sixth")]
        {
            Some(|| run_empty(EmptyRuntimeMode::ObservedSharedSixth))
        }
        #[cfg(not(any(
            feature = "scheduler-empty-minimal",
            feature = "scheduler-empty-observed",
            feature = "scheduler-empty-observed-shared-sixth"
        )))]
        {
            None
        }
    };
    if let Some(run_empty_variant) = empty_runner {
        return run_empty_variant();
    }

    let opts = Opts::parse();
    setup_logging(opts.verbose)?;
    #[cfg(feature = "diagnostics")]
    let runtime_log_path = opts.runtime_log_path.as_deref();
    #[cfg(not(feature = "diagnostics"))]
    let runtime_log_path = None;
    runtime_log::init(runtime_log_path, opts.verbose)?;
    overhead_profile::init_global(opts.overhead_profile_path.as_ref().map(|path| {
        OverheadProfileConfig {
            path: path.clone(),
            interval: Duration::from_millis(opts.overhead_profile_interval_ms.max(1)),
            alloc_enabled: opts.overhead_profile_alloc,
        }
    }))?;
    try_set_rlimit_infinity();
    ensure_current_thread_sched_other()?;
    let usersched_pid = std::process::id();

    let topo = topology::discover()?;
    let host_mapping = parse_host_mapping(Path::new(&opts.ccm_mapping_path))?;
    let mut mapping = build_mapping_from_host_mapping(&topo, &host_mapping)?;
    apply_host_cpu_filters(
        &topo,
        &mut mapping,
        opts.managed_cpu_max,
        opts.primary_smt_only,
    )?;

    let cgroup_path = PathBuf::from(&opts.cgroup_path);
    ensure_cgroup_dir(&cgroup_path)?;
    if opts.restrict_mapped_cpus {
        restrict_cgroup_cpus(&cgroup_path, &eligible_cpulist(&mapping))?;
    }
    let mut cgroup_guard = CgroupGuard::new(cgroup_path.clone(), opts.verbose);
    if opts.guard_clean_cgroup {
        cgroup_guard.assert_clean_start()?;
    }
    let control_plane_cpu =
        maybe_dedicated_control_plane_cpu(opts.control_plane_cpu_policy, &topo, &mapping)?;
    #[cfg(feature = "stall-filler-spinner")]
    let stall_filler_slice_ns = opts.stall_filler_slice_ms.max(1).saturating_mul(1_000_000);
    #[cfg(feature = "stall-filler-spinner")]
    let mut stall_filler =
        StallFillerSpinner::spawn(&mapping, stall_filler_slice_ns, opts.verbose)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (llc_tx, llc_rx) = unbounded();
    let (df_tx, df_rx) = unbounded();
    let (df_cs_tx, df_cs_rx) = unbounded();
    let llc_handles = start_llc_sampler(
        topo.clone(),
        LlcSamplerConfig {
            window_ms: opts.llc_window_ms,
            period_ms: opts.llc_period_ms,
            dram_hot_ns_x100: pct_to_x100(opts.dram_hot_ns),
            miss_ppm_hot: opts.miss_ppm_hot,
            filter: FilterConfig {
                alpha_pct: opts.llc_ewma_alpha_pct,
                hot_pct_x100: pct_to_x100(opts.llc_hot_pct),
                cool_pct_x100: pct_to_x100(opts.llc_cool_pct),
                hot_persist: opts.llc_hot_persist,
                cool_persist: opts.llc_cool_persist,
            },
        },
        Arc::clone(&stop),
        llc_tx,
    );
    let df_handle = start_df_sampler(
        mapping.clone(),
        DfSamplerConfig {
            window_ms: opts.df_window_ms,
            filter: FilterConfig {
                alpha_pct: opts.df_ewma_alpha_pct,
                hot_pct_x100: pct_to_x100(opts.df_hot_pct),
                cool_pct_x100: pct_to_x100(opts.df_cool_pct),
                hot_persist: opts.df_hot_persist,
                cool_persist: opts.df_cool_persist,
            },
        },
        Arc::clone(&stop),
        df_tx,
        control_plane_cpu,
    );
    let df_cs_handle = start_df_cs_sampler(
        topo.clone(),
        mapping.clone(),
        DfCsSamplerConfig {
            window_ms: opts.df_window_ms,
            filter: FilterConfig {
                alpha_pct: opts.df_ewma_alpha_pct,
                hot_pct_x100: pct_to_x100(opts.df_hot_pct),
                cool_pct_x100: pct_to_x100(opts.df_cool_pct),
                hot_persist: opts.df_hot_persist,
                cool_persist: opts.df_cool_persist,
            },
        },
        Arc::clone(&stop),
        df_cs_tx,
        control_plane_cpu,
    );

    let mut open_object = MaybeUninit::<OpenObject>::uninit();
    let mut bpf = BpfScheduler::init(
        &mut open_object,
        None,
        0,
        true,
        opts.verbose,
        true,
        DEFAULT_SLICE_US * 1_000,
        opts.tick_reeval_every.max(1),
        opts.l2_need_mib_s.saturating_mul(100),
        opts.tick_defer_max.max(1),
        opts.llc_stale_ms
            .max(opts.llc_period_ms.saturating_mul(2))
            .saturating_mul(1_000_000),
        opts.df_stale_ms
            .max(opts.df_window_ms.saturating_mul(2))
            .saturating_mul(1_000_000),
        "rustland_la",
    )?;
    update_cpu_domain_map(&mut bpf, &topo)?;
    let _perf_events = PerfEventFds::attach(&mut bpf, topo.nr_cpu_ids)?;
    #[cfg(feature = "light-compete")]
    let mut light_helpers = spawn_light_helpers(&mut bpf, &cgroup_path, &mapping, opts.verbose)?;

    let mut cpu_util = CpuUtilTracker::new(topo.nr_cpu_ids);
    let _ = cpu_util.sample(topo.nr_cpu_ids);
    let cpu_util_every = Duration::from_millis(opts.cpu_util_sample_ms.max(10));
    let has_managed_workload = !opts.command.is_empty() || !opts.spawn_shell.is_empty();
    if has_managed_workload {
        std::thread::sleep(cpu_util_every);
        let _ = cpu_util.sample(topo.nr_cpu_ids);
    }
    let mut workloads = Vec::<WorkloadProcess>::new();
    if !opts.command.is_empty() {
        let child = launch_workload(&opts.command, &cgroup_path, opts.verbose)?;
        cgroup_guard.allow_root(child.id());
        workloads.push(WorkloadProcess {
            label: opts.command.join(" "),
            child,
            exited_at: None,
        });
    }
    for command_text in &opts.spawn_shell {
        let child = launch_shell_workload(command_text, &cgroup_path, opts.verbose)?;
        cgroup_guard.allow_root(child.id());
        workloads.push(WorkloadProcess {
            label: command_text.clone(),
            child,
            exited_at: None,
        });
        if opts.spawn_stagger_ms > 0 {
            std::thread::sleep(Duration::from_millis(opts.spawn_stagger_ms));
        }
    }
    BpfScheduler::enter_sched_ext_current_thread()?;
    if opts.verbose {
        for workload in &workloads {
            crate::diag_line!(
                "startup_diag usersched_pid={} workload_pid={} workload_label={}",
                usersched_pid,
                workload.child.id(),
                workload.label,
            );
        }
    }
    let mut adopters = workloads
        .iter()
        .map(|workload| {
            WorkloadAdopter::new(workload.child.id(), cgroup_path.clone(), opts.verbose)
        })
        .collect::<Vec<_>>();

    let mut llc_states = vec![None; topo.domains.len()];
    let mut df_states = vec![None; topo.domains.len()];
    let mut df_cs_states = vec![None; mapping.cs_link_count()];
    let mut pending = VecDeque::<QueuedTask>::new();
    let mut threads = BTreeMap::<u32, ManagedThreadState>::new();
    #[cfg(feature = "diagnostics")]
    let mut recent = VecDeque::with_capacity(64);
    #[cfg(feature = "diagnostics")]
    let mut logger = DecisionLogger::new(opts.decision_log_path.as_deref(), opts.verbose)?;
    let mut affinity = AffinityCache::new(100);
    let mut last_cpu_util = Instant::now();
    #[cfg(feature = "diagnostics")]
    let mut last_monitor = Instant::now();
    #[cfg(feature = "diagnostics")]
    let monitor_every = opts.monitor.map(Duration::from_secs_f64);
    let cs_villain_settle_ms = opts
        .cs_villain_settle_ms
        .unwrap_or_else(|| opts.df_window_ms.saturating_mul(4).max(20))
        .max(1);
    let cs_villain_release_samples = opts
        .cs_villain_release_samples
        .unwrap_or(opts.df_cool_persist)
        .max(1);
    let policy = PolicyConfig {
        l2_need_mib_s_x100: opts.l2_need_mib_s.saturating_mul(100),
        migrate_margin_x100: pct_to_x100(opts.migrate_margin),
        cpu_high_util_x100: pct_to_x100(opts.cpu_high_util_pct),
        cpu_rebalance_job_delta: opts.cpu_rebalance_job_delta,
        stall_victim_min_pct_x100: pct_to_x100(opts.stall_victim_min_pct),
        stall_victim_delta_pct_x100: pct_to_x100(opts.stall_victim_delta_pct),
        llc_stale_ms: opts.llc_stale_ms.max(opts.llc_period_ms.saturating_mul(2)),
        df_stale_ms: opts.df_stale_ms.max(opts.df_window_ms.saturating_mul(2)),
        migrate_settle_ms: opts.migrate_settle_ms,
        signature_snapshots: opts.signature_snapshots,
        cs_villain_throttle: opts.cs_villain_throttle,
        tick_reeval_every: opts.tick_reeval_every.max(1),
        tick_defer_max: opts.tick_defer_max.max(1),
        cs_villain_reslice_ns: opts.cs_villain_reslice_us.max(1).saturating_mul(1_000),
        cs_villain_refill_divisor: opts.cs_villain_refill_divisor.max(1),
        cs_villain_settle_ns: cs_villain_settle_ms.saturating_mul(1_000_000),
        cs_villain_release_samples,
        tick_move_phase_mod: opts.tick_move_phase_mod.max(1),
    };
    let mut cs_villain_pressure = TokenBucketPressurePolicy::default();
    let mut cs_villain_latch = CsVillainLatchController::default();
    let mut last_guard_check = Instant::now() - Duration::from_secs(1);
    let guard_every = Duration::from_millis(250);
    let sync_tgid_overrides = opts.sync_tgid.iter().copied().collect::<BTreeSet<_>>();
    let planner_config = PlannerConfig {
        disable_auto_sync_hints: opts.disable_auto_sync_hints,
        sync_tgid_overrides: sync_tgid_overrides.clone(),
        incremental_item_limit: 64,
        incremental_domain_limit: 2,
        max_passes: 4,
        swap_pass_items: 16,
        debounce: Duration::from_millis(2),
        #[cfg(feature = "diagnostics")]
        move_trace_path: opts.planner_move_trace_path.clone(),
        #[cfg(not(feature = "diagnostics"))]
        move_trace_path: None,
        #[cfg(feature = "diagnostics")]
        plan_debug_path: opts.planner_plan_debug_path.clone(),
        #[cfg(not(feature = "diagnostics"))]
        plan_debug_path: None,
    };
    let (planner_tx, planner_rx, planner_handle) =
        start_planner_worker(planner_config, Arc::clone(&stop), control_plane_cpu)?;
    let mut latest_df_sweep_epoch = 0u64;
    let mut active_plan: Option<PlacementPlan> = None;
    let mut plan_cs_attribution = PlanCsAttributionState::default();
    let overhead_profiler = overhead_profile::global();

    if let Some(cpu) = control_plane_cpu {
        pin_current_thread_to_cpu(cpu)?;
        if opts.verbose {
            crate::diag_line!(
                "control_plane_cpu policy={} cpu={}",
                control_plane_cpu_policy_str(opts.control_plane_cpu_policy),
                cpu
            );
        }
    }

    while !bpf.exited() {
        let loop_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        for adopter in adopters.iter_mut() {
            adopter.maybe_sync();
        }
        if opts.guard_clean_cgroup && last_guard_check.elapsed() >= guard_every {
            let mut tracked = BTreeSet::new();
            for adopter in &adopters {
                tracked.extend(adopter.tracked_pids());
            }
            cgroup_guard.set_allowed_anchors(tracked);
            cgroup_guard.assert_runtime_membership()?;
            last_guard_check = Instant::now();
        }
        let sample_summary = apply_sample_updates(
            &llc_rx,
            &df_rx,
            &df_cs_rx,
            &mut llc_states,
            &mut df_states,
            &mut df_cs_states,
        );
        if let Some(epoch) = sample_summary.latest_df_sweep_epoch {
            latest_df_sweep_epoch = latest_df_sweep_epoch.max(epoch);
        }
        while let Ok(output) = planner_rx.try_recv() {
            active_plan = Some(output.plan);
        }
        update_domain_state_maps(&mut bpf, &llc_states, &df_states)?;
        prune_dead_threads(&mut threads, &mut affinity);

        if last_cpu_util.elapsed() >= cpu_util_every {
            let _ = cpu_util.sample(topo.nr_cpu_ids);
            last_cpu_util = Instant::now();
        }

        let dequeue_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        loop {
            match bpf.dequeue_task() {
                Ok(Some(task)) => pending.push_back(task),
                Ok(None) => break,
                Err(err) => {
                    log::warn!("ringbuf dequeue error: {err}");
                    break;
                }
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, dequeue_start) {
            profiler.record_scope("main_ringbuf_drain", start, true);
        }
        bpf.publish_scheduled(pending.len() as u64);

        let mut cpu_states = read_cpu_states(&mut bpf, topo.nr_cpu_ids)?;
        let mut deferred = VecDeque::<QueuedTask>::new();
        let mut planner_triggers = Vec::<PlannerTrigger>::new();
        if let Some(epoch) = sample_summary.latest_df_sweep_epoch {
            planner_triggers.push(PlannerTrigger::SweepComplete(epoch));
        }
        for domain in sample_summary.domain_state_flips {
            planner_triggers.push(PlannerTrigger::DomainStateFlip(domain));
        }

        let mut victim_links = BTreeSet::new();
        let mut victim_fallbacks = BTreeMap::<u32, LinkContender>::new();
        let prepass_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        if !pending.is_empty() {
            let prepass_now = now_ns();
            let active_cutoff_ns = prepass_now.saturating_sub(
                policy
                    .df_stale_ms
                    .max(policy.llc_stale_ms)
                    .saturating_mul(1_000_000),
            );
            for task in pending.iter() {
                if task.tgid as u32 == usersched_pid {
                    continue;
                }
                let tid = task.tid as u32;
                let entry = threads.entry(tid).or_default();
                let was_active = entry.last_seen_ns >= active_cutoff_ns;
                let previous_class = entry.last_class;
                let previous_sync = entry.last_sync_qualified;
                refresh_thread_from_task(entry, task, prepass_now);
                if policy.signature_snapshots {
                    update_thread_signature(
                        entry,
                        task,
                        &mapping,
                        &df_states,
                        prepass_now,
                        policy.df_stale_ms,
                    );
                    let task_trigger = QueueTrigger::from_u32(task.trigger);
                    if matches!(
                        task_trigger,
                        QueueTrigger::Tick | QueueTrigger::VillainReslice
                    ) {
                        entry.last_tick_seen_ns = prepass_now;
                    }
                }
                let current_llc = u32::try_from(task.current_domain)
                    .ok()
                    .and_then(|domain| llc_states.get(domain as usize).copied().flatten());
                let next_class = classify_task(task, current_llc, prepass_now, policy);
                entry.last_class = next_class;
                let next_sync = sync_qualified_for_planner(
                    entry,
                    opts.disable_auto_sync_hints,
                    &sync_tgid_overrides,
                );
                entry.last_sync_qualified = next_sync;
                if !was_active {
                    planner_triggers.push(PlannerTrigger::RunnableDelta(tid));
                }
                if previous_class != next_class || previous_sync != next_sync {
                    planner_triggers.push(PlannerTrigger::SignatureChange(tid));
                }
                for link_id in 0..df_cs_states.len().min(MAX_DOMAINS) {
                    let link_id = link_id as u32;
                    let Some(signal) = stall_signal_for_link(
                        task,
                        link_id,
                        &mapping,
                        &df_cs_states,
                        prepass_now,
                        policy,
                    ) else {
                        continue;
                    };
                    log_stall_signal(task, link_id, signal.live_overload_x100, signal.kind);
                    victim_links.insert(link_id);
                    if signal.kind.allows_fallback_villain() {
                        let fallback_score = signal
                            .live_overload_x100
                            .saturating_add(task.last_stall_pct_x100)
                            .max(1);
                        let fallback = LinkContender {
                            tid,
                            score: fallback_score,
                            contender_count: 1,
                        };
                        victim_fallbacks
                            .entry(link_id)
                            .and_modify(|current| {
                                if fallback.score > current.score {
                                    *current = fallback;
                                }
                            })
                            .or_insert(fallback);
                    }
                }
            }
        }
        if !victim_links.is_empty() {
            for domain in &mapping.eligible_domains {
                planner_triggers.push(PlannerTrigger::TickPressure(*domain));
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, prepass_start) {
            profiler.record_scope("main_pending_prepass", start, true);
        }
        if policy.signature_snapshots {
            let plan_cs_now = now_ns();
            apply_plan_cs_attribution(
                &mut plan_cs_attribution,
                &mut threads,
                &df_cs_states,
                plan_cs_now,
                policy,
            );
            decay_io_cs_evidence_to_current_samples(
                &mut threads,
                &df_cs_states,
                plan_cs_now,
                policy,
            );
        }
        if latest_df_sweep_epoch > 0 && !planner_triggers.is_empty() {
            let planner_request_start = overhead_profiler
                .as_ref()
                .map(|_| ThreadClockSample::capture());
            let snapshot = build_planner_input(
                &topo,
                &mapping,
                policy,
                &pending,
                &threads,
                &mut affinity,
                now_ns(),
                latest_df_sweep_epoch,
                &llc_states,
                &df_states,
                active_plan.as_ref(),
            );
            let _ = planner_tx.try_send(PlannerRequest {
                snapshot,
                triggers: planner_triggers,
            });
            if let (Some(profiler), Some(start)) = (&overhead_profiler, planner_request_start) {
                profiler.record_scope("main_planner_request", start, true);
            }
        }
        while let Ok(output) = planner_rx.try_recv() {
            active_plan = Some(output.plan);
        }
        if policy.signature_snapshots {
            if let Some(plan) = active_plan
                .as_ref()
                .filter(|plan| plan_is_current(plan, latest_df_sweep_epoch))
            {
                arm_plan_cs_attribution(
                    &mut plan_cs_attribution,
                    plan,
                    &threads,
                    &df_cs_states,
                    now_ns(),
                    policy,
                );
            }
        }

        let plan_build_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        let mut initial_domain_targets = BTreeMap::new();
        let mut initial_cpu_targets = BTreeMap::new();
        if let Some(plan) = active_plan
            .as_ref()
            .filter(|plan| plan_is_current(plan, latest_df_sweep_epoch))
        {
            for (&tid, entry) in &plan.entries {
                initial_domain_targets.insert(tid, entry.target_domain);
                if let Some(target_cpu) = entry.target_cpu {
                    initial_cpu_targets.insert(tid, target_cpu);
                }
            }
        }
        let allowed_cpus_by_tid =
            collect_allowed_cpus_for_pending(&pending, &mut affinity, now_ns());
        let mut reservations = DispatchBatchReservations::from_snapshot(
            &topo,
            &threads,
            &pending,
            &initial_domain_targets,
            &initial_cpu_targets,
        );
        let target_cpus = reservations.reserve_burst_target_cpus(
            &pending,
            &topo,
            &mapping,
            &cpu_states,
            cpu_util.values(),
            &allowed_cpus_by_tid,
            &initial_cpu_targets,
            policy,
        );
        if let Some(plan) = active_plan
            .as_mut()
            .filter(|plan| plan_is_current(plan, latest_df_sweep_epoch))
        {
            for (&tid, &target_cpu) in &target_cpus {
                if let Some(entry) = plan.entries.get_mut(&tid) {
                    entry.target_cpu = Some(target_cpu);
                }
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, plan_build_start) {
            profiler.record_scope("main_target_cpu_build", start, true);
        }

        let villain_now = now_ns();
        let fresh_link_villains = choose_link_villains_with_fallbacks(
            &mapping,
            &threads,
            &victim_links,
            &df_cs_states,
            &victim_fallbacks,
            villain_now,
            policy,
        );
        let villain_active_cutoff_ns = villain_now.saturating_sub(
            policy
                .df_stale_ms
                .max(policy.llc_stale_ms)
                .saturating_mul(1_000_000),
        );
        let latch_output = cs_villain_latch.effective_villains(
            &mapping,
            &fresh_link_villains,
            &df_cs_states,
            &threads,
            villain_active_cutoff_ns,
            villain_now,
            policy,
        );
        for event in latch_output.events {
            log_stall_latch(event);
        }
        let effective_link_villains = latch_output.effective_villains;

        let dispatch_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        while let Some(task) = pending.pop_front() {
            if task.tgid as u32 == usersched_pid {
                let mut dispatched = DispatchedTask::new(&task);
                dispatched.cpu = RL_CPU_ANY;
                dispatched.slice_ns = DEFAULT_SLICE_US * 1_000;
                if bpf.dispatch_task(&dispatched).is_err() {
                    pending.push_front(task);
                    break;
                }
                continue;
            }
            let now = now_ns();
            let tid = task.tid as u32;
            let Some(allowed_cpus) = allowed_cpus_by_tid.get(&tid) else {
                continue;
            };
            let mut fallback_reason = "no_plan";
            let mut decision = if let Some(plan) = active_plan.as_ref() {
                match planned_decision(
                    &task,
                    threads.get(&tid),
                    plan,
                    &topo,
                    &mapping,
                    reservations.domain_states(),
                    reservations.cpu_states(),
                    &llc_states,
                    &df_states,
                    &cpu_states,
                    cpu_util.values(),
                    &allowed_cpus,
                    now,
                    policy,
                    latest_df_sweep_epoch,
                ) {
                    Ok(decision) => decision,
                    Err(reason) => {
                        fallback_reason = reason;
                        choose_placement_with_planned_cpus(
                            &task,
                            threads.get(&tid),
                            &topo,
                            &mapping,
                            reservations.domain_states(),
                            &llc_states,
                            &df_states,
                            &cpu_states,
                            cpu_util.values(),
                            reservations.cpu_states(),
                            &allowed_cpus,
                            now,
                            policy,
                        )
                    }
                }
            } else {
                choose_placement_with_planned_cpus(
                    &task,
                    threads.get(&tid),
                    &topo,
                    &mapping,
                    reservations.domain_states(),
                    &llc_states,
                    &df_states,
                    &cpu_states,
                    cpu_util.values(),
                    reservations.cpu_states(),
                    &allowed_cpus,
                    now,
                    policy,
                )
            };
            if !decision.plan_used {
                decision.fallback_reason = fallback_reason;
                if let Some(plan) = active_plan.as_ref() {
                    decision.planner_epoch = plan.built_from_sweep_epoch;
                    decision.plan_revision = plan.plan_revision;
                    if let Some(entry) = plan.entries.get(&tid) {
                        decision.planned_domain = Some(entry.target_domain);
                        decision.planned_cpu = entry.target_cpu;
                        decision.sync_group_id = entry.sync_group_id;
                        decision.sync_anchor_domain = entry.sync_anchor_domain;
                        decision.sync_override = entry.sync_override;
                    } else {
                        decision.planned_domain = None;
                        decision.planned_cpu = None;
                    }
                } else {
                    decision.planned_domain = None;
                    decision.planned_cpu = None;
                }
            }
            let task_trigger = QueueTrigger::from_u32(task.trigger);
            let is_tick_trigger = matches!(
                task_trigger,
                QueueTrigger::Tick | QueueTrigger::VillainReslice
            );
            {
                let current_villain = effective_link_villains
                    .iter()
                    .enumerate()
                    .filter_map(|(link_id, contender)| {
                        contender.map(|contender| (link_id as u32, contender))
                    })
                    .find(|(_, contender)| contender.tid == tid);
                let entry = threads.entry(tid).or_default();
                if let Some(control) =
                    cs_villain_pressure.apply(entry, current_villain, is_tick_trigger, now, policy)
                {
                    decision.trigger = QueueTrigger::VillainReslice;
                    decision.villain_score = control.score;
                    match control.action {
                        VillainThrottleAction::Defer { tokens_ns } => {
                            decision.defer_dispatch = true;
                            decision.tick_decision = TickDecision::Defer;
                            decision.selected_domain = task_current_domain(&task);
                            decision.selected_cpu = task_current_cpu(&task);
                            log_stall_control(
                                &task,
                                control.link_id,
                                control.score,
                                "defer_token",
                                decision.slice_ns,
                                entry.consecutive_tick_defer,
                                tokens_ns,
                                control.token_capacity_ns,
                            );
                        }
                        VillainThrottleAction::Reslice {
                            slice_ns,
                            tokens_ns,
                        } => {
                            decision.tick_decision = TickDecision::Reslice;
                            decision.slice_ns = slice_ns;
                            let bss = bpf.skel.maps.bss_data.as_mut().unwrap();
                            bss.nr_tick_reslice = bss.nr_tick_reslice.saturating_add(1);
                            log_stall_control(
                                &task,
                                control.link_id,
                                control.score,
                                "reslice_token",
                                decision.slice_ns,
                                entry.consecutive_tick_defer,
                                tokens_ns,
                                control.token_capacity_ns,
                            );
                        }
                    }
                }
            }
            if !decision.plan_used && is_cross_domain_move(&task, &decision) {
                let revalidated = choose_placement_with_planned_cpus(
                    &task,
                    threads.get(&tid),
                    &topo,
                    &mapping,
                    reservations.domain_states(),
                    &llc_states,
                    &df_states,
                    &cpu_states,
                    cpu_util.values(),
                    reservations.cpu_states(),
                    &allowed_cpus,
                    now,
                    policy,
                );
                if !is_cross_domain_move(&task, &revalidated)
                    || revalidated.selected_domain != decision.selected_domain
                    || revalidated.selected_cpu != decision.selected_cpu
                {
                    decision = revalidated;
                }
            }
            let target_cpu_prepass = active_plan
                .as_ref()
                .and_then(|plan| plan.entries.get(&tid))
                .and_then(|entry| entry.target_cpu)
                .or_else(|| target_cpus.get(&tid).copied());
            let mut reserved_cpu_override_used = false;
            if !decision.defer_dispatch {
                if let Some(reserved_cpu) = preferred_reserved_cpu_for_selected_domain(
                    target_cpu_prepass,
                    decision.selected_domain,
                    &topo,
                    &mapping,
                    &cpu_states,
                    cpu_util.values(),
                    reservations.cpu_states(),
                    allowed_cpus,
                    policy,
                    tid,
                ) {
                    reserved_cpu_override_used = true;
                    decision.selected_cpu = Some(reserved_cpu);
                }
            }
            #[cfg(not(all(feature = "diagnostics", feature = "select-cpu-in-domain-debug")))]
            let _ = reserved_cpu_override_used;
            let mut dispatched = DispatchedTask::new(&task);
            dispatched.cpu = decision
                .selected_cpu
                .and_then(|cpu| i32::try_from(cpu).ok())
                .unwrap_or(RL_CPU_ANY);
            dispatched.slice_ns = decision.slice_ns;
            {
                let entry = threads.entry(tid).or_default();
                dispatched.tick_holdoff = next_tick_holdoff_ticks(entry, &task, &decision);
            }
            if !decision.defer_dispatch && bpf.dispatch_task(&dispatched).is_err() {
                pending.push_front(task);
                break;
            }

            let comm = crate::types::comm_to_string(&task.comm);
            #[cfg(feature = "diagnostics")]
            {
                let mut record = build_record(&task, &decision, comm.clone());
                record.control_plane_cpu_policy =
                    control_plane_cpu_policy_str(opts.control_plane_cpu_policy).to_string();
                record.control_plane_cpu = control_plane_cpu;
                fill_io_cs_signature_debug(&mut record, threads.get(&tid));
                #[cfg(feature = "select-cpu-in-domain-debug")]
                if opts.select_cpu_in_domain {
                    fill_select_cpu_in_domain_debug(
                        &mut record,
                        &decision,
                        target_cpu_prepass,
                        reserved_cpu_override_used,
                        &topo,
                        &cpu_states,
                        cpu_util.values(),
                        reservations.cpu_states(),
                        allowed_cpus,
                        policy,
                    );
                }
                logger.write(&record)?;
                if recent.len() >= 64 {
                    recent.pop_front();
                }
                recent.push_back(record);
            }

            let selected_domain = decision
                .selected_domain
                .or_else(|| task_current_domain(&task));
            let selected_cpu = decision.selected_cpu.or_else(|| task_current_cpu(&task));
            let entry = threads.entry(tid).or_default();
            entry.tid = tid;
            entry.tgid = task.tgid as u32;
            entry.comm = comm;
            entry.last_seen_ns = now;
            entry.last_class = decision.class;
            entry.last_selected_domain = selected_domain;
            entry.last_selected_cpu = selected_cpu;
            entry.last_observed_domain = u32::try_from(task.current_domain).ok();
            entry.last_observed_cpu = u32::try_from(task.current_cpu).ok();
            if task_current_domain(&task).is_some()
                && decision.selected_domain.is_some()
                && task_current_domain(&task) != decision.selected_domain
                && !decision.defer_dispatch
            {
                entry.settle_until_ns = now + policy.migrate_settle_ms.saturating_mul(1_000_000);
                entry.last_migration_from_domain = u32::try_from(task.current_domain).ok();
                entry.last_migration_to_domain = selected_domain;
                entry.last_migration_at_ns = now;
                entry.reverse_protect_until_ns = now
                    + policy
                        .migrate_settle_ms
                        .saturating_mul(REVERSE_HYSTERESIS_MULTIPLIER)
                        .saturating_mul(1_000_000);
            }

            if policy.signature_snapshots && !decision.defer_dispatch {
                entry.dispatch_snapshot = capture_dispatch_snapshot(
                    &df_states,
                    now,
                    policy.df_stale_ms,
                    selected_domain,
                    selected_cpu,
                );
            }

            if decision.defer_dispatch {
                deferred.push_back(task);
                continue;
            }

            if let Some(domain) = selected_domain.filter(|_| is_cross_domain_move(&task, &decision))
            {
                let meta = threads.get(&tid);
                reservations.commit_domain_decision(&task, meta, &mapping, domain);
            }

            if let Some(cpu) = selected_cpu {
                reservations.commit_cpu_decision(&task, cpu);
            }

            if let Some(cpu) = selected_cpu {
                if let Some(state) = cpu_states.get_mut(cpu as usize) {
                    state.idle = 0;
                    state.cpu_dsq_depth = state.cpu_dsq_depth.saturating_add(1);
                    state.current_tid = tid;
                    state.last_update_ns = now;
                }
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, dispatch_start) {
            profiler.record_scope("main_dispatch_loop", start, true);
        }

        #[cfg(feature = "stall-filler-spinner")]
        let stall_filler_sleep = if pending.is_empty() {
            stall_filler.maybe_hold_deferred_with_spinner(&mut deferred)
        } else {
            None
        };

        pending.extend(deferred);

        let notify_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        bpf.notify_complete(pending.len() as u64);
        if let (Some(profiler), Some(start)) = (&overhead_profiler, notify_start) {
            profiler.record_scope("main_notify_complete", start, true);
        }
        overhead_profile::flush_global(false);

        #[cfg(feature = "stall-filler-spinner")]
        if let Some(sleep_for) = stall_filler_sleep {
            std::thread::sleep(sleep_for);
            let released = stall_filler.release_held_deferred();
            pending.extend(stall_filler.drain_held_deferred());
            if released > 0 {
                bpf.publish_scheduled(pending.len() as u64);
            }
        }

        #[cfg(feature = "diagnostics")]
        if let Some(interval) = monitor_every {
            if last_monitor.elapsed() >= interval {
                let tick_stats = tick_stats_snapshot(&mut bpf);
                runtime_log::line(monitor::render(
                    pending.len(),
                    &topo,
                    &mapping,
                    &llc_states,
                    &df_states,
                    Some(&df_cs_states),
                    &threads,
                    tick_stats,
                    &recent,
                ));
                last_monitor = Instant::now();
            }
        }

        let workload_wait_start = overhead_profiler
            .as_ref()
            .map(|_| ThreadClockSample::capture());
        for workload in workloads.iter_mut() {
            if workload.child.try_wait()?.is_some() && workload.exited_at.is_none() {
                workload.exited_at = Some(Instant::now());
            }
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, workload_wait_start) {
            profiler.record_scope("main_workload_try_wait", start, true);
        }
        let all_workloads_done = !workloads.is_empty()
            && workloads.iter().all(|workload| {
                workload
                    .exited_at
                    .map(|ts| ts.elapsed() >= Duration::from_millis(250))
                    .unwrap_or(false)
            });
        if all_workloads_done && pending.is_empty() {
            break;
        }
        if let (Some(profiler), Some(start)) = (&overhead_profiler, loop_start) {
            profiler.record_scope("main_loop_total", start, true);
        }
    }

    stop.store(true, Ordering::Relaxed);
    for handle in llc_handles {
        let _ = handle.join();
    }
    let _ = df_handle.join();
    let _ = df_cs_handle.join();
    let _ = planner_handle.join();

    for workload in workloads.iter_mut() {
        shutdown_child_gracefully(&mut workload.child);
    }
    #[cfg(feature = "stall-filler-spinner")]
    stall_filler.shutdown(opts.verbose);
    #[cfg(feature = "light-compete")]
    shutdown_light_helpers(&mut bpf, &mut light_helpers, opts.verbose);

    overhead_profile::shutdown_global();
    let _ = bpf.shutdown_and_report()?;
    Ok(())
}
