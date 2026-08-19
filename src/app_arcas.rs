#[cfg(feature = "diagnostics")]
use crate::app::tick_stats_snapshot;
use crate::app::{
    apply_planned_cpu_assignment, apply_planned_migration, apply_sample_updates,
    build_planned_cpu_states, build_planned_domain_states, build_planned_thread_cpus,
    build_planned_thread_domains, capture_dispatch_snapshot, next_tick_holdoff_ticks,
    prune_dead_threads, read_cpu_states, refresh_thread_from_task, setup_logging,
    update_cpu_domain_map, update_domain_state_maps, update_thread_signature, AffinityCache,
    PerfEventFds,
};
use crate::bpf::{BpfScheduler, DispatchedTask, QueuedTask, RL_CPU_ANY};
use crate::cli::Opts;
use crate::cpu_util::CpuUtilTracker;
#[cfg(feature = "diagnostics")]
use crate::decision_log::DecisionLogger;
use crate::df_ccm_sampler::{start as start_df_sampler, DfSamplerConfig};
use crate::filter::FilterConfig;
use crate::host_map::{
    apply_host_cpu_filters, build_mapping_from_host_mapping, eligible_cpulist, parse_host_mapping,
};
use crate::llc_sampler::{start as start_llc_sampler, LlcSamplerConfig};
#[cfg(feature = "diagnostics")]
use crate::monitor;
#[cfg(feature = "diagnostics")]
use crate::policy::build_record;
use crate::policy_arcas::choose_placement_with_targets;
use crate::runtime_log;
use crate::topology;
use crate::types::{
    now_ns, pct_to_x100, CpuStateValue, LlcStateValue, ManagedThreadState, PolicyConfig,
    TopologyLayout, DEFAULT_SLICE_US, MEM_SOURCE_COUNT, MEM_SOURCE_NEAR_CACHE,
};
use crate::workload::{
    ensure_cgroup_dir, ensure_current_thread_sched_other, launch_shell_workload, launch_workload,
    restrict_cgroup_cpus, shutdown_child_gracefully, CgroupGuard, WorkloadAdopter,
};
use anyhow::Result;
use clap::Parser;
use crossbeam_channel::unbounded;
use libbpf_rs::OpenObject;
use scx_utils::try_set_rlimit_infinity;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "light-compete")]
use crate::app::{shutdown_light_helpers, spawn_light_helpers};
#[cfg(feature = "diagnostics")]
use crate::types::TickStatsSnapshot;

#[derive(Clone, Copy, Debug)]
struct ArcasConfig {
    interval_ms: u64,
    remote_fill_threshold_mib_s_x100: u32,
    min_domains: usize,
    max_domains: usize,
    step: usize,
}

#[derive(Clone, Debug, Default)]
struct ArcasControllerState {
    spread_domains: usize,
    last_adjust_ns: u64,
    active_domains: Vec<u32>,
    target_domain_by_tid: BTreeMap<u32, u32>,
    target_cpu_by_tid: BTreeMap<u32, u32>,
    rank_by_tid: BTreeMap<u32, usize>,
    next_rank: usize,
    last_remote_fill_mib_s_x100: u64,
    last_sampled_thread_count: usize,
}

struct WorkloadProcess {
    label: String,
    child: Child,
    exited_at: Option<Instant>,
}

enum PendingSelection {
    Runnable {
        index: usize,
        decision: crate::policy::PlacementDecision,
    },
    Drop {
        index: usize,
    },
}

fn eligible_domain_list(mapping: &crate::types::MappingInfo) -> Vec<u32> {
    mapping.eligible_domains.iter().copied().collect()
}

fn effective_max_domains(cfg: ArcasConfig, eligible_len: usize) -> usize {
    let raw_max = if cfg.max_domains == 0 {
        eligible_len
    } else {
        cfg.max_domains
    };
    raw_max.max(1).min(eligible_len.max(1))
}

fn clamp_spread_domains(spread: usize, cfg: ArcasConfig, eligible_len: usize) -> usize {
    if eligible_len == 0 {
        return 0;
    }
    let max_domains = effective_max_domains(cfg, eligible_len);
    let min_domains = cfg.min_domains.max(1).min(max_domains);
    spread.clamp(min_domains, max_domains)
}

fn direct_thread_fill_totals(thread: &ManagedThreadState) -> (u32, u32) {
    let near_fill_x100 = thread.ewma_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE]
        .max(thread.last_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE]);
    let total_fill_x100 = (0..MEM_SOURCE_COUNT).fold(0u32, |acc, idx| {
        acc.saturating_add(
            thread.ewma_fill_bw_mib_s_x100[idx].max(thread.last_fill_bw_mib_s_x100[idx]),
        )
    });
    (near_fill_x100, total_fill_x100)
}

fn union_allowed_cpus(allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>) -> BTreeSet<u32> {
    let mut union = BTreeSet::new();
    for cpus in allowed_cpus_by_tid.values() {
        union.extend(cpus.iter().copied());
    }
    union
}

fn domain_slot_cpus(
    topo: &TopologyLayout,
    domain_id: u32,
    allowed_union: &BTreeSet<u32>,
) -> Vec<u32> {
    topo.domains
        .get(domain_id as usize)
        .map(|domain| {
            domain
                .cpus
                .iter()
                .copied()
                .filter(|cpu| allowed_union.is_empty() || allowed_union.contains(cpu))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn min_domains_for_runnable_load(
    topo: &TopologyLayout,
    eligible_domains: &[u32],
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
) -> usize {
    let runnable_threads = allowed_cpus_by_tid.len();
    if runnable_threads == 0 {
        return 0;
    }

    let allowed_union = union_allowed_cpus(allowed_cpus_by_tid);
    let mut capacities = eligible_domains
        .iter()
        .copied()
        .map(|domain| domain_slot_cpus(topo, domain, &allowed_union).len())
        .filter(|capacity| *capacity > 0)
        .collect::<Vec<_>>();
    capacities.sort_unstable_by(|a, b| b.cmp(a));

    let mut covered = 0usize;
    for (index, capacity) in capacities.into_iter().enumerate() {
        covered = covered.saturating_add(capacity);
        if covered >= runnable_threads {
            return index + 1;
        }
    }
    eligible_domains.len()
}

fn select_active_domains(
    topo: &TopologyLayout,
    eligible_domains: &[u32],
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    spread_domains: usize,
) -> Vec<u32> {
    if eligible_domains.is_empty() || spread_domains == 0 {
        return Vec::new();
    }

    let allowed_union = union_allowed_cpus(allowed_cpus_by_tid);
    let mut candidates = eligible_domains
        .iter()
        .copied()
        .filter(|domain| {
            allowed_union.is_empty() || !domain_slot_cpus(topo, *domain, &allowed_union).is_empty()
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        candidates = eligible_domains.to_vec();
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let desired = spread_domains.min(candidates.len());
    if desired == candidates.len() {
        return candidates;
    }

    let mut selected = Vec::with_capacity(desired);
    for index in 0..desired {
        let candidate_index = (index * candidates.len()) / desired;
        let domain = candidates[candidate_index];
        if !selected.contains(&domain) {
            selected.push(domain);
        }
    }

    let runnable_threads = allowed_cpus_by_tid.len();
    let selected_capacity = selected.iter().fold(0usize, |acc, domain| {
        acc.saturating_add(domain_slot_cpus(topo, *domain, &allowed_union).len())
    });
    if selected.len() == desired && (runnable_threads == 0 || selected_capacity >= runnable_threads)
    {
        return selected;
    }

    candidates.sort_by_key(|domain| {
        let capacity = domain_slot_cpus(topo, *domain, &allowed_union).len();
        (Reverse(capacity), *domain)
    });
    candidates.truncate(desired);
    candidates
}

fn ranked_tids_for_plan(
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    rank_by_tid: &BTreeMap<u32, usize>,
) -> Vec<u32> {
    let mut tids = allowed_cpus_by_tid.keys().copied().collect::<Vec<_>>();
    tids.sort_by_key(|tid| (rank_by_tid.get(tid).copied().unwrap_or(usize::MAX), *tid));
    tids
}

fn build_target_plan(
    topo: &TopologyLayout,
    active_domains: &[u32],
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    rank_by_tid: &BTreeMap<u32, usize>,
) -> (BTreeMap<u32, u32>, BTreeMap<u32, u32>) {
    if active_domains.is_empty() || allowed_cpus_by_tid.is_empty() {
        return (BTreeMap::new(), BTreeMap::new());
    }

    let allowed_union = union_allowed_cpus(allowed_cpus_by_tid);
    let domain_cpus = active_domains
        .iter()
        .copied()
        .map(|domain| (domain, domain_slot_cpus(topo, domain, &allowed_union)))
        .collect::<BTreeMap<_, _>>();

    let mut ranked_tids = ranked_tids_for_plan(allowed_cpus_by_tid, rank_by_tid);
    ranked_tids.sort_by_key(|tid| {
        (
            allowed_cpus_by_tid
                .get(tid)
                .map(|cpus| cpus.len())
                .unwrap_or(usize::MAX),
            rank_by_tid.get(tid).copied().unwrap_or(usize::MAX),
            *tid,
        )
    });

    let mut target_domain_by_tid = BTreeMap::new();
    let mut target_cpu_by_tid = BTreeMap::new();
    let mut slot_cursor_by_domain = BTreeMap::<u32, usize>::new();
    let mut used_cpus = BTreeSet::<u32>::new();
    for tid in ranked_tids {
        let Some(allowed_cpus) = allowed_cpus_by_tid.get(&tid) else {
            continue;
        };

        let rank = rank_by_tid.get(&tid).copied().unwrap_or(usize::MAX);
        let start_index = rank % active_domains.len();
        let mut chosen = None;
        for offset in 0..active_domains.len() {
            let domain = active_domains[(start_index + offset) % active_domains.len()];
            let Some(cpus) = domain_cpus.get(&domain) else {
                continue;
            };
            if cpus.is_empty() {
                continue;
            }
            let cursor = slot_cursor_by_domain.get(&domain).copied().unwrap_or(0);
            for allow_reserved in [false, true] {
                for slot_offset in 0..cpus.len() {
                    let cpu = cpus[(cursor + slot_offset) % cpus.len()];
                    if allowed_cpus.contains(&cpu) && (allow_reserved || !used_cpus.contains(&cpu))
                    {
                        chosen = Some((domain, cpu));
                        slot_cursor_by_domain.insert(domain, cursor.saturating_add(1));
                        break;
                    }
                }
                if chosen.is_some() {
                    break;
                }
            }
            if chosen.is_some() {
                break;
            }
        }

        if chosen.is_none() {
            for allow_reserved in [false, true] {
                chosen = allowed_cpus.iter().copied().find_map(|cpu| {
                    if !allow_reserved && used_cpus.contains(&cpu) {
                        return None;
                    }
                    let domain = topo.cpu_to_domain.get(cpu as usize).copied().flatten()?;
                    active_domains.contains(&domain).then_some((domain, cpu))
                });
                if chosen.is_some() {
                    break;
                }
            }
        }

        if let Some((domain, cpu)) = chosen {
            target_domain_by_tid.insert(tid, domain);
            target_cpu_by_tid.insert(tid, cpu);
            used_cpus.insert(cpu);
        }
    }

    (target_domain_by_tid, target_cpu_by_tid)
}

fn direct_remote_fill_signal(
    threads: &BTreeMap<u32, ManagedThreadState>,
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
) -> (u64, usize) {
    let mut total = 0u64;
    for tid in allowed_cpus_by_tid.keys().copied() {
        let Some(thread) = threads.get(&tid) else {
            continue;
        };
        let (near_fill_x100, _) = direct_thread_fill_totals(thread);
        total = total.saturating_add(u64::from(near_fill_x100));
    }
    (total, allowed_cpus_by_tid.len())
}

impl ArcasControllerState {
    fn new(mapping: &crate::types::MappingInfo, cfg: ArcasConfig) -> Self {
        let eligible = eligible_domain_list(mapping);
        let spread_domains = clamp_spread_domains(cfg.min_domains.max(1), cfg, eligible.len());
        let active_domains = eligible.into_iter().take(spread_domains).collect();
        Self {
            spread_domains,
            active_domains,
            ..Self::default()
        }
    }

    fn maybe_rebalance(
        &mut self,
        threads: &BTreeMap<u32, ManagedThreadState>,
        topo: &TopologyLayout,
        mapping: &crate::types::MappingInfo,
        allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
        now_ns: u64,
        cfg: ArcasConfig,
    ) -> bool {
        let eligible = eligible_domain_list(mapping);
        if eligible.is_empty() {
            self.spread_domains = 0;
            self.active_domains.clear();
            self.target_domain_by_tid.clear();
            self.target_cpu_by_tid.clear();
            self.rank_by_tid.clear();
            return false;
        }

        let current_tids = allowed_cpus_by_tid.keys().copied().collect::<BTreeSet<_>>();
        let rank_len_before = self.rank_by_tid.len();
        self.rank_by_tid.retain(|tid, _| current_tids.contains(tid));
        let target_domain_len_before = self.target_domain_by_tid.len();
        self.target_domain_by_tid
            .retain(|tid, _| current_tids.contains(tid));
        let target_cpu_len_before = self.target_cpu_by_tid.len();
        self.target_cpu_by_tid
            .retain(|tid, _| current_tids.contains(tid));
        let pruned_controller_state = rank_len_before != self.rank_by_tid.len()
            || target_domain_len_before != self.target_domain_by_tid.len()
            || target_cpu_len_before != self.target_cpu_by_tid.len();

        for tid in allowed_cpus_by_tid.keys().copied() {
            if self.rank_by_tid.contains_key(&tid) {
                continue;
            }
            self.rank_by_tid.insert(tid, self.next_rank);
            self.next_rank = self.next_rank.saturating_add(1);
        }

        let capacity_floor = min_domains_for_runnable_load(topo, &eligible, allowed_cpus_by_tid);
        let mut refresh_targets = self.active_domains.is_empty()
            || self.target_domain_by_tid.is_empty()
            || self.target_cpu_by_tid.is_empty()
            || pruned_controller_state;
        if self.spread_domains == 0 {
            self.spread_domains = clamp_spread_domains(
                cfg.min_domains.max(capacity_floor).max(1),
                cfg,
                eligible.len(),
            );
            refresh_targets = true;
        }

        let interval_ns = cfg.interval_ms.saturating_mul(1_000_000);
        let ready =
            self.last_adjust_ns == 0 || now_ns.saturating_sub(self.last_adjust_ns) >= interval_ns;

        let mut adjusted = false;
        let required_spread = clamp_spread_domains(
            self.spread_domains
                .max(capacity_floor)
                .max(cfg.min_domains.max(1)),
            cfg,
            eligible.len(),
        );
        if required_spread != self.spread_domains {
            self.spread_domains = required_spread;
            adjusted = true;
            refresh_targets = true;
        }

        if ready {
            let (remote_fill, sampled_count) =
                direct_remote_fill_signal(threads, allowed_cpus_by_tid);
            self.last_remote_fill_mib_s_x100 = remote_fill;
            self.last_sampled_thread_count = sampled_count;
            if sampled_count > 0 {
                let mut next = self.spread_domains;
                if remote_fill >= u64::from(cfg.remote_fill_threshold_mib_s_x100) {
                    next = next.saturating_add(cfg.step);
                } else {
                    next = next.saturating_sub(cfg.step);
                }
                next = clamp_spread_domains(next.max(capacity_floor), cfg, eligible.len());
                adjusted |= next != self.spread_domains;
                self.spread_domains = next;
            }
            self.last_adjust_ns = now_ns;
            refresh_targets = true;
        }

        if refresh_targets {
            let spread = clamp_spread_domains(self.spread_domains, cfg, eligible.len());
            self.active_domains =
                select_active_domains(topo, &eligible, allowed_cpus_by_tid, spread);
            (self.target_domain_by_tid, self.target_cpu_by_tid) = build_target_plan(
                topo,
                &self.active_domains,
                allowed_cpus_by_tid,
                &self.rank_by_tid,
            );
        }
        adjusted
    }
}

fn collect_allowed_cpus_for_pending(
    pending: &VecDeque<QueuedTask>,
    affinity: &mut AffinityCache,
    now_ns: u64,
    usersched_pid: u32,
) -> BTreeMap<u32, BTreeSet<u32>> {
    collect_allowed_cpus_for_pending_with(pending, now_ns, usersched_pid, |tid, now_ns| {
        affinity.allowed_cpus(tid, now_ns)
    })
}

fn collect_allowed_cpus_for_pending_with<F>(
    pending: &VecDeque<QueuedTask>,
    now_ns: u64,
    usersched_pid: u32,
    mut allowed_cpus_for_tid: F,
) -> BTreeMap<u32, BTreeSet<u32>>
where
    F: FnMut(u32, u64) -> Result<BTreeSet<u32>>,
{
    let mut out = BTreeMap::new();
    for task in pending {
        if task.tgid as u32 == usersched_pid {
            continue;
        }
        let tid = task.tid as u32;
        if out.contains_key(&tid) {
            continue;
        }
        if let Ok(cpus) = allowed_cpus_for_tid(tid, now_ns) {
            if !cpus.is_empty() {
                out.insert(tid, cpus);
            }
        }
    }
    out
}

fn is_cross_domain_move(task: &QueuedTask, decision: &crate::policy::PlacementDecision) -> bool {
    decision
        .selected_domain
        .is_some_and(|domain| Some(domain) != u32::try_from(task.current_domain).ok())
        && !decision.defer_dispatch
}

fn is_launch_helper_task(task: &QueuedTask) -> bool {
    matches!(
        crate::types::comm_to_string(&task.comm).as_str(),
        "bash"
            | "cut"
            | "grep"
            | "head"
            | "lscpu"
            | "memory_benchmar"
            | "numactl"
            | "perf"
            | "perf-exec"
            | "ps"
            | "python3"
            | "sh"
            | "sleep"
            | "sudo"
            | "tail"
            | "taskset"
            | "tee"
            | "time"
            | "tr"
    )
}

fn launch_helper_cpu(_task: &QueuedTask, allowed_cpus: Option<&BTreeSet<u32>>) -> i32 {
    allowed_cpus
        .filter(|cpus| cpus.len() == 1)
        .and_then(|cpus| cpus.iter().next().copied())
        .and_then(|cpu| i32::try_from(cpu).ok())
        .unwrap_or(RL_CPU_ANY)
}

fn select_next_pending(
    pending: &VecDeque<QueuedTask>,
    threads: &BTreeMap<u32, ManagedThreadState>,
    topo: &crate::types::TopologyLayout,
    mapping: &crate::types::MappingInfo,
    planned_states: &[crate::types::PlannedDomainState],
    planned_cpu_states: &[crate::types::PlannedCpuState],
    llc_states: &[Option<LlcStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    controller: &ArcasControllerState,
    now_ns: u64,
    policy: PolicyConfig,
) -> Result<Option<PendingSelection>> {
    let mut best_move: Option<(u32, usize, crate::policy::PlacementDecision)> = None;
    let mut first_runnable: Option<(usize, crate::policy::PlacementDecision)> = None;
    let mut drop_index: Option<usize> = None;

    for (index, task) in pending.iter().enumerate() {
        let tid = task.tid as u32;
        let Some(allowed_cpus) = allowed_cpus_by_tid.get(&tid) else {
            drop_index.get_or_insert(index);
            continue;
        };
        let target_domain = controller.target_domain_by_tid.get(&tid).copied();
        let target_cpu = controller.target_cpu_by_tid.get(&tid).copied();
        let decision = choose_placement_with_targets(
            task,
            threads.get(&tid),
            topo,
            mapping,
            planned_states,
            llc_states,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            allowed_cpus,
            &controller.active_domains,
            target_domain,
            target_cpu,
            now_ns,
            policy,
        );

        let target_mismatch = target_domain
            .zip(u32::try_from(task.current_domain).ok())
            .map(|(target, current)| target != current)
            .unwrap_or(false);
        if target_mismatch && is_cross_domain_move(task, &decision) {
            let near_fill_x100 = task.last_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE];
            match &best_move {
                None => best_move = Some((near_fill_x100, index, decision)),
                Some((best_near_fill_x100, best_index, _))
                    if near_fill_x100 > *best_near_fill_x100
                        || (near_fill_x100 == *best_near_fill_x100 && index < *best_index) =>
                {
                    best_move = Some((near_fill_x100, index, decision));
                }
                _ => {}
            }
        } else if first_runnable.is_none() {
            first_runnable = Some((index, decision));
        }
    }

    if let Some((_, index, decision)) = best_move {
        return Ok(Some(PendingSelection::Runnable { index, decision }));
    }
    if let Some((index, decision)) = first_runnable {
        return Ok(Some(PendingSelection::Runnable { index, decision }));
    }
    Ok(drop_index.map(|index| PendingSelection::Drop { index }))
}

pub fn run() -> Result<()> {
    let opts = Opts::parse();
    setup_logging(opts.verbose)?;
    #[cfg(feature = "diagnostics")]
    let runtime_log_path = opts.runtime_log_path.as_deref();
    #[cfg(not(feature = "diagnostics"))]
    let runtime_log_path = None;
    runtime_log::init(runtime_log_path, opts.verbose)?;
    try_set_rlimit_infinity();
    ensure_current_thread_sched_other()?;
    let usersched_pid = std::process::id();

    let topo = topology::discover()?;
    let host_mapping = parse_host_mapping(PathBuf::from(&opts.ccm_mapping_path).as_path())?;
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

    let stop = Arc::new(AtomicBool::new(false));
    let (llc_tx, llc_rx) = unbounded();
    let (df_tx, df_rx) = unbounded();
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
        None,
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
        "rustland_la_arcas",
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
    let mut adopters = if opts.adopt_workload_descendants {
        workloads
            .iter()
            .map(|workload| {
                WorkloadAdopter::new(workload.child.id(), cgroup_path.clone(), opts.verbose)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let mut llc_states = vec![None; topo.domains.len()];
    let mut df_states = vec![None; topo.domains.len()];
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
        cs_villain_settle_ns: opts
            .cs_villain_settle_ms
            .unwrap_or_else(|| opts.df_window_ms.saturating_mul(4).max(20))
            .max(1)
            .saturating_mul(1_000_000),
        cs_villain_release_samples: opts
            .cs_villain_release_samples
            .unwrap_or(opts.df_cool_persist)
            .max(1),
        tick_move_phase_mod: opts.tick_move_phase_mod.max(1),
    };
    let arcas_cfg = ArcasConfig {
        interval_ms: opts.arcas_interval_ms.max(1),
        remote_fill_threshold_mib_s_x100: opts
            .arcas_remote_fill_threshold_mib_s
            .saturating_mul(100),
        min_domains: usize::try_from(opts.arcas_min_domains).unwrap_or(usize::MAX),
        max_domains: usize::try_from(opts.arcas_max_domains).unwrap_or(usize::MAX),
        step: usize::try_from(opts.arcas_step.max(1)).unwrap_or(1),
    };
    let mut controller = ArcasControllerState::new(&mapping, arcas_cfg);
    let mut last_guard_check = Instant::now() - Duration::from_secs(1);
    let guard_every = Duration::from_millis(250);

    while !bpf.exited() {
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
        apply_sample_updates(
            &llc_rx,
            &df_rx,
            &crossbeam_channel::never(),
            &mut llc_states,
            &mut df_states,
            &mut [],
        );
        update_domain_state_maps(&mut bpf, &llc_states, &df_states)?;
        prune_dead_threads(&mut threads, &mut affinity);

        if last_cpu_util.elapsed() >= cpu_util_every {
            let _ = cpu_util.sample(topo.nr_cpu_ids);
            last_cpu_util = Instant::now();
        }

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
        bpf.publish_scheduled(pending.len() as u64);

        let mut cpu_states = read_cpu_states(&mut bpf, topo.nr_cpu_ids)?;

        if !pending.is_empty() {
            let prepass_now = now_ns();
            for task in pending.iter() {
                if task.tgid as u32 == usersched_pid {
                    continue;
                }
                let tid = task.tid as u32;
                let entry = threads.entry(tid).or_default();
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
                }
            }
        }

        let allowed_cpus_by_tid =
            collect_allowed_cpus_for_pending(&pending, &mut affinity, now_ns(), usersched_pid);
        controller.maybe_rebalance(
            &threads,
            &topo,
            &mapping,
            &allowed_cpus_by_tid,
            now_ns(),
            arcas_cfg,
        );

        let mut planned_domains = build_planned_thread_domains(&threads, &pending);
        let mut planned_cpus = build_planned_thread_cpus(&threads, &pending);
        let mut planned_states =
            build_planned_domain_states(&topo, &threads, &pending, &planned_domains);
        let mut planned_cpu_states = build_planned_cpu_states(topo.nr_cpu_ids, &planned_cpus);

        while !pending.is_empty() {
            if let Some(index) = pending
                .iter()
                .position(|task| task.tgid as u32 == usersched_pid)
            {
                let task = pending
                    .remove(index)
                    .expect("usersched task disappeared from pending queue");
                let mut dispatched = DispatchedTask::new(&task);
                dispatched.cpu = RL_CPU_ANY;
                dispatched.slice_ns = DEFAULT_SLICE_US * 1_000;
                if bpf.dispatch_task(&dispatched).is_err() {
                    pending.push_front(task);
                    break;
                }
                continue;
            }

            if let Some(index) = pending.iter().position(is_launch_helper_task) {
                let task = pending
                    .remove(index)
                    .expect("launch helper task disappeared from pending queue");
                let tid = task.tid as u32;
                let mut dispatched = DispatchedTask::new(&task);
                dispatched.cpu = launch_helper_cpu(&task, allowed_cpus_by_tid.get(&tid));
                dispatched.slice_ns = DEFAULT_SLICE_US * 1_000;
                if bpf.dispatch_task(&dispatched).is_err() {
                    pending.push_front(task);
                    break;
                }
                continue;
            }

            let now = now_ns();
            let selection = select_next_pending(
                &pending,
                &threads,
                &topo,
                &mapping,
                &planned_states,
                &planned_cpu_states,
                &llc_states,
                &cpu_states,
                cpu_util.values(),
                &allowed_cpus_by_tid,
                &controller,
                now,
                policy,
            )?;
            let Some(selection) = selection else {
                break;
            };
            let (index, decision) = match selection {
                PendingSelection::Runnable { index, decision } => (index, decision),
                PendingSelection::Drop { index } => {
                    let _ = pending.remove(index);
                    continue;
                }
            };
            let task = pending
                .remove(index)
                .expect("selected task disappeared from pending queue");

            let mut dispatched = DispatchedTask::new(&task);
            dispatched.cpu = decision
                .selected_cpu
                .and_then(|cpu| i32::try_from(cpu).ok())
                .unwrap_or(RL_CPU_ANY);
            dispatched.slice_ns = decision.slice_ns;
            {
                let entry = threads.entry(task.tid as u32).or_default();
                dispatched.tick_holdoff = next_tick_holdoff_ticks(entry, &task, &decision);
            }
            if bpf.dispatch_task(&dispatched).is_err() {
                pending.push_front(task);
                break;
            }

            let comm = crate::types::comm_to_string(&task.comm);
            #[cfg(feature = "diagnostics")]
            {
                let record = build_record(&task, &decision, comm.clone());
                logger.write(&record)?;
                if recent.len() >= 64 {
                    recent.pop_front();
                }
                recent.push_back(record);
            }

            let selected_domain = decision
                .selected_domain
                .or_else(|| u32::try_from(task.current_domain).ok());
            let selected_cpu = decision
                .selected_cpu
                .or_else(|| u32::try_from(task.current_cpu).ok());
            let entry = threads.entry(task.tid as u32).or_default();
            entry.tid = task.tid as u32;
            entry.tgid = task.tgid as u32;
            entry.comm = comm;
            entry.last_seen_ns = now;
            entry.last_class = decision.class;
            entry.last_selected_domain = selected_domain;
            entry.last_selected_cpu = selected_cpu;
            entry.last_observed_domain = u32::try_from(task.current_domain).ok();
            entry.last_observed_cpu = u32::try_from(task.current_cpu).ok();
            if u32::try_from(task.current_domain).ok().is_some()
                && decision.selected_domain.is_some()
                && u32::try_from(task.current_domain).ok() != decision.selected_domain
            {
                entry.settle_until_ns = now + policy.migrate_settle_ms.saturating_mul(1_000_000);
                entry.last_migration_from_domain = u32::try_from(task.current_domain).ok();
                entry.last_migration_to_domain = selected_domain;
                entry.last_migration_at_ns = now;
            }
            if policy.signature_snapshots {
                entry.dispatch_snapshot = capture_dispatch_snapshot(
                    &df_states,
                    now,
                    policy.df_stale_ms,
                    selected_domain,
                    selected_cpu,
                );
            }

            if let Some(domain) = selected_domain.filter(|_| is_cross_domain_move(&task, &decision))
            {
                let meta = threads.get(&(task.tid as u32));
                apply_planned_migration(
                    &mut planned_states,
                    &mut planned_domains,
                    &task,
                    meta,
                    &mapping,
                    domain,
                );
            }
            if let Some(cpu) = selected_cpu {
                apply_planned_cpu_assignment(
                    &mut planned_cpu_states,
                    &mut planned_cpus,
                    &task,
                    cpu,
                );
                if let Some(state) = cpu_states.get_mut(cpu as usize) {
                    state.idle = 0;
                    state.cpu_dsq_depth = state.cpu_dsq_depth.saturating_add(1);
                    state.current_tid = task.tid as u32;
                    state.last_update_ns = now;
                }
            }
        }

        bpf.notify_complete(pending.len() as u64);

        #[cfg(feature = "diagnostics")]
        if let Some(interval) = monitor_every {
            if last_monitor.elapsed() >= interval {
                let tick_stats: TickStatsSnapshot = tick_stats_snapshot(&mut bpf);
                runtime_log::line(monitor::render(
                    pending.len(),
                    &topo,
                    &mapping,
                    &llc_states,
                    &df_states,
                    None,
                    &threads,
                    tick_stats,
                    &recent,
                ));
                last_monitor = Instant::now();
            }
        }

        for workload in workloads.iter_mut() {
            if workload.child.try_wait()?.is_some() && workload.exited_at.is_none() {
                workload.exited_at = Some(Instant::now());
            }
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
    }

    stop.store(true, Ordering::Relaxed);
    for handle in llc_handles {
        let _ = handle.join();
    }
    let _ = df_handle.join();

    for workload in workloads.iter_mut() {
        shutdown_child_gracefully(&mut workload.child);
    }
    #[cfg(feature = "light-compete")]
    shutdown_light_helpers(&mut bpf, &mut light_helpers, opts.verbose);

    let _ = bpf.shutdown_and_report()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DomainInfo, MappingInfo, ThreadSignature, MEM_SOURCE_COUNT};

    fn topo() -> TopologyLayout {
        TopologyLayout {
            nr_cpu_ids: 8,
            domains: vec![
                DomainInfo {
                    domain_id: 0,
                    kernel_l3_id: 0,
                    rep_cpu: 0,
                    cpus: vec![0, 1],
                    l3_size_mb: 32.0,
                },
                DomainInfo {
                    domain_id: 1,
                    kernel_l3_id: 1,
                    rep_cpu: 2,
                    cpus: vec![2, 3],
                    l3_size_mb: 32.0,
                },
                DomainInfo {
                    domain_id: 2,
                    kernel_l3_id: 2,
                    rep_cpu: 4,
                    cpus: vec![4, 5],
                    l3_size_mb: 32.0,
                },
                DomainInfo {
                    domain_id: 3,
                    kernel_l3_id: 3,
                    rep_cpu: 6,
                    cpus: vec![6, 7],
                    l3_size_mb: 32.0,
                },
            ],
            cpu_to_domain: vec![
                Some(0),
                Some(0),
                Some(1),
                Some(1),
                Some(2),
                Some(2),
                Some(3),
                Some(3),
            ],
        }
    }

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1, 2, 3],
            domain_to_ccm: vec![Some(0), Some(1), Some(2), Some(3)],
            domain_to_df_capacity_mib_s_x100: vec![
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
            ],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1, 2, 3]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7]),
        }
    }

    fn thread(
        tid: u32,
        valid: bool,
        near_fill_x100: u32,
        total_fill_x100: u32,
    ) -> ManagedThreadState {
        let mut fill_bw = [0u32; MEM_SOURCE_COUNT];
        fill_bw[MEM_SOURCE_NEAR_CACHE] = near_fill_x100;
        let remaining = total_fill_x100.saturating_sub(near_fill_x100);
        fill_bw[0] = remaining;
        ManagedThreadState {
            tid,
            last_fill_bw_mib_s_x100: fill_bw,
            ewma_fill_bw_mib_s_x100: fill_bw,
            signature: ThreadSignature {
                valid,
                fill_bw_mib_s_x100: fill_bw,
                ..ThreadSignature::default()
            },
            ..ManagedThreadState::default()
        }
    }

    fn allowed_map(tids: &[u32], cpus: &[u32]) -> BTreeMap<u32, BTreeSet<u32>> {
        let allowed = cpus.iter().copied().collect::<BTreeSet<_>>();
        tids.iter()
            .copied()
            .map(|tid| (tid, allowed.clone()))
            .collect()
    }

    fn queued_task(tid: u32, tgid: u32, current_cpu: i32, current_domain: i32) -> QueuedTask {
        QueuedTask {
            tid: tid as i32,
            tgid: tgid as i32,
            current_cpu,
            current_domain,
            nr_cpus_allowed: 8,
            flags: 0,
            start_ts: 0,
            stop_ts: 0,
            exec_runtime: 0,
            weight: 100,
            vtime: 0,
            enq_cnt: 1,
            last_l2_bw_mib_s_x100: 0,
            ewma_l2_bw_mib_s_x100: 0,
            last_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            ewma_fill_bw_mib_s_x100: [0; MEM_SOURCE_COUNT],
            last_ipc_x1000: 0,
            ewma_ipc_x1000: 0,
            last_stall_pct_x100: 0,
            ewma_stall_pct_x100: 0,
            trigger: 0,
            tick_seq: 0,
            last_update_ns: 0,
            comm: [0; 16],
        }
    }

    fn queued_task_with_comm(
        tid: u32,
        tgid: u32,
        current_cpu: i32,
        current_domain: i32,
        comm: &str,
    ) -> QueuedTask {
        let mut task = queued_task(tid, tgid, current_cpu, current_domain);
        for (idx, byte) in comm.as_bytes().iter().take(task.comm.len() - 1).enumerate() {
            task.comm[idx] = *byte as i8;
        }
        task
    }

    fn cfg() -> ArcasConfig {
        ArcasConfig {
            interval_ms: 100,
            remote_fill_threshold_mib_s_x100: 300_000,
            min_domains: 1,
            max_domains: 0,
            step: 1,
        }
    }

    #[test]
    fn rebalance_waits_for_interval_before_adjusting_spread() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let threads = BTreeMap::from([(1, thread(1, true, 400_000, 500_000))]);
        let allowed = allowed_map(&[1], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let changed = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg());
        assert!(changed);
        let spread_after_first = state.spread_domains;

        let changed = state.maybe_rebalance(
            &threads,
            &topo,
            &mapping,
            &allowed,
            10_000_000 + 50_000_000,
            cfg(),
        );
        assert!(!changed);
        assert_eq!(state.spread_domains, spread_after_first);
    }

    #[test]
    fn rebalance_uses_direct_remote_fill_without_valid_signatures() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let threads = BTreeMap::from([(1, thread(1, false, 450_000, 450_000))]);
        let allowed = allowed_map(&[1], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let changed = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg());
        assert!(changed);
        assert_eq!(state.spread_domains, 2);
        assert_eq!(state.last_sampled_thread_count, 1);
    }

    #[test]
    fn target_assignments_follow_stable_rank_when_fill_order_changes() {
        let topo = topo();
        let mapping = mapping();
        let cfg = ArcasConfig {
            min_domains: 2,
            max_domains: 2,
            ..cfg()
        };
        let mut state = ArcasControllerState::new(&mapping, cfg);
        let allowed = allowed_map(&[10, 11], &[0, 1, 2, 3, 4, 5, 6, 7]);
        let hot_first = BTreeMap::from([
            (10, thread(10, true, 500_000, 600_000)),
            (11, thread(11, true, 100_000, 200_000)),
        ]);
        let hot_second = BTreeMap::from([
            (10, thread(10, true, 100_000, 200_000)),
            (11, thread(11, true, 500_000, 600_000)),
        ]);

        let _ = state.maybe_rebalance(&hot_first, &topo, &mapping, &allowed, 10_000_000, cfg);
        let initial_targets = state.target_domain_by_tid.clone();
        assert_eq!(initial_targets.get(&10), Some(&0));
        assert_eq!(initial_targets.get(&11), Some(&2));

        let _ = state.maybe_rebalance(
            &hot_second,
            &topo,
            &mapping,
            &allowed,
            10_000_000 + 200_000_000,
            cfg,
        );
        assert_eq!(state.target_domain_by_tid, initial_targets);
    }

    #[test]
    fn target_assignments_follow_rank_slot_cpu_order() {
        let allowed = allowed_map(&[10, 11, 12], &[0, 1, 4, 5]);
        let rank_by_tid = BTreeMap::from([(10, 0usize), (11, 1usize), (12, 2usize)]);

        let (domains, cpus) = build_target_plan(&topo(), &[0, 2], &allowed, &rank_by_tid);
        assert_eq!(domains.get(&10), Some(&0));
        assert_eq!(cpus.get(&10), Some(&0));
        assert_eq!(domains.get(&11), Some(&2));
        assert_eq!(cpus.get(&11), Some(&4));
        assert_eq!(domains.get(&12), Some(&0));
        assert_eq!(cpus.get(&12), Some(&1));
    }

    #[test]
    fn target_plan_reserves_single_cpu_affinity_before_unpinned_tasks() {
        let allowed = BTreeMap::from([(10, BTreeSet::from([0, 1])), (11, BTreeSet::from([1]))]);
        let rank_by_tid = BTreeMap::from([(11, 0usize), (10, 1usize)]);

        let (domains, cpus) = build_target_plan(&topo(), &[0], &allowed, &rank_by_tid);

        assert_eq!(domains.get(&11), Some(&0));
        assert_eq!(cpus.get(&11), Some(&1));
        assert_eq!(domains.get(&10), Some(&0));
        assert_eq!(cpus.get(&10), Some(&0));
    }

    #[test]
    fn spread_domains_respect_min_max_clamp() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(
            &mapping,
            ArcasConfig {
                max_domains: 2,
                ..cfg()
            },
        );
        let threads = BTreeMap::from([(1, thread(1, true, 900_000, 900_000))]);
        let allowed = allowed_map(&[1], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let _ = state.maybe_rebalance(
            &threads,
            &topo,
            &mapping,
            &allowed,
            10_000_000,
            ArcasConfig {
                max_domains: 2,
                ..cfg()
            },
        );
        assert_eq!(state.spread_domains, 2);

        let _ = state.maybe_rebalance(
            &threads,
            &topo,
            &mapping,
            &allowed,
            10_000_000 + 200_000_000,
            ArcasConfig {
                min_domains: 2,
                max_domains: 2,
                ..cfg()
            },
        );
        assert_eq!(state.spread_domains, 2);
    }

    #[test]
    fn spread_domains_contract_when_remote_fill_drops() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let hot_threads = BTreeMap::from([(1, thread(1, true, 900_000, 900_000))]);
        let cool_threads = BTreeMap::from([(1, thread(1, true, 50_000, 50_000))]);
        let allowed = allowed_map(&[1], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let _ = state.maybe_rebalance(&hot_threads, &topo, &mapping, &allowed, 10_000_000, cfg());
        assert_eq!(state.spread_domains, 2);

        let _ = state.maybe_rebalance(
            &cool_threads,
            &topo,
            &mapping,
            &allowed,
            10_000_000 + 200_000_000,
            cfg(),
        );
        assert_eq!(state.spread_domains, 1);
    }

    #[test]
    fn spread_domains_respect_runnable_capacity_floor() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let threads = (0..7)
            .map(|tid| (tid, thread(tid, true, 10_000, 10_000)))
            .collect::<BTreeMap<_, _>>();
        let allowed = allowed_map(&[0, 1, 2, 3, 4, 5, 6], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let _ = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg());
        assert_eq!(state.spread_domains, 4);
    }

    #[test]
    fn rebalance_reports_changed_when_capacity_floor_forces_spread() {
        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let threads = (0..7)
            .map(|tid| (tid, thread(tid, true, 10_000, 10_000)))
            .collect::<BTreeMap<_, _>>();
        let allowed = allowed_map(&[0, 1, 2, 3, 4, 5, 6], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let changed = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg());

        assert!(changed);
        assert_eq!(state.spread_domains, 4);
    }

    #[test]
    fn rebalance_prunes_rank_state_for_absent_threads() {
        let topo = topo();
        let mapping = mapping();
        let cfg = ArcasConfig {
            min_domains: 2,
            max_domains: 2,
            ..cfg()
        };
        let mut state = ArcasControllerState::new(&mapping, cfg);
        let all_cpus = &[0, 1, 2, 3, 4, 5, 6, 7];
        let initial_threads = BTreeMap::from([
            (10, thread(10, true, 500_000, 600_000)),
            (11, thread(11, true, 100_000, 200_000)),
        ]);
        let initial_allowed = allowed_map(&[10, 11], all_cpus);

        let _ = state.maybe_rebalance(
            &initial_threads,
            &topo,
            &mapping,
            &initial_allowed,
            10_000_000,
            cfg,
        );
        assert_eq!(
            state.rank_by_tid.keys().copied().collect::<Vec<_>>(),
            vec![10, 11]
        );

        let remaining_threads = BTreeMap::from([(10, thread(10, true, 100_000, 200_000))]);
        let remaining_allowed = allowed_map(&[10], all_cpus);
        let _ = state.maybe_rebalance(
            &remaining_threads,
            &topo,
            &mapping,
            &remaining_allowed,
            210_000_000,
            cfg,
        );

        assert_eq!(
            state.rank_by_tid.keys().copied().collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(
            state
                .target_domain_by_tid
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(
            state.target_cpu_by_tid.keys().copied().collect::<Vec<_>>(),
            vec![10]
        );
    }

    #[test]
    fn controller_inputs_ignore_usersched_tasks() {
        let usersched_pid = 42;
        let pending = VecDeque::from([
            queued_task(90, usersched_pid, 0, 0),
            queued_task(91, usersched_pid, 2, 1),
            queued_task(92, usersched_pid, 4, 2),
            queued_task(10, 1000, 0, 0),
        ]);
        let allowed =
            collect_allowed_cpus_for_pending_with(&pending, 10_000_000, usersched_pid, |tid, _| {
                Ok(if tid == 10 {
                    BTreeSet::from([0, 1])
                } else {
                    BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])
                })
            });

        assert_eq!(allowed.keys().copied().collect::<Vec<_>>(), vec![10]);

        let topo = topo();
        let mapping = mapping();
        let mut state = ArcasControllerState::new(&mapping, cfg());
        let threads = BTreeMap::from([
            (10, thread(10, true, 10_000, 10_000)),
            (90, thread(90, true, 900_000, 900_000)),
            (91, thread(91, true, 900_000, 900_000)),
            (92, thread(92, true, 900_000, 900_000)),
        ]);

        let _ = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg());

        assert_eq!(state.spread_domains, 1);
        assert_eq!(state.last_sampled_thread_count, 1);
        assert_eq!(
            state.rank_by_tid.keys().copied().collect::<Vec<_>>(),
            vec![10]
        );
    }

    #[test]
    fn launch_helper_bypass_prefers_legal_current_cpu() {
        let helper = queued_task_with_comm(10, 10, 3, 1, "numactl");
        let worker = queued_task_with_comm(11, 11, 4, 2, "rocksdb:high0");
        let allowed = BTreeSet::from([1, 3, 5]);

        assert!(is_launch_helper_task(&helper));
        assert!(!is_launch_helper_task(&worker));
        assert_eq!(launch_helper_cpu(&helper, Some(&allowed)), RL_CPU_ANY);

        let moved_helper = queued_task_with_comm(12, 12, 7, 3, "taskset");
        assert_eq!(
            launch_helper_cpu(&moved_helper, Some(&BTreeSet::from([5]))),
            5
        );
        assert_eq!(launch_helper_cpu(&moved_helper, Some(&allowed)), RL_CPU_ANY);
        assert_eq!(launch_helper_cpu(&moved_helper, None), RL_CPU_ANY);

        let noise = queued_task_with_comm(13, 13, 6, 0, "memory_benchmar");
        assert!(is_launch_helper_task(&noise));
    }

    #[test]
    fn active_domains_use_stable_even_sampling_instead_of_first_domains() {
        let topo = topo();
        let mapping = mapping();
        let cfg = ArcasConfig {
            min_domains: 2,
            max_domains: 2,
            ..cfg()
        };
        let mut state = ArcasControllerState::new(&mapping, cfg);
        let threads = BTreeMap::from([(1, thread(1, true, 50_000, 50_000))]);
        let allowed = allowed_map(&[1], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let _ = state.maybe_rebalance(&threads, &topo, &mapping, &allowed, 10_000_000, cfg);
        assert_eq!(state.active_domains, vec![0, 2]);
    }
}

#[cfg(test)]
#[path = "app_arcas_paper_tests.rs"]
mod paper_tests;
