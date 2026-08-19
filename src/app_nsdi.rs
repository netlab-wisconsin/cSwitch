#[cfg(feature = "diagnostics")]
use crate::app::tick_stats_snapshot;
use crate::app::{
    apply_planned_cpu_assignment, apply_sample_updates, build_planned_cpu_states,
    build_planned_thread_cpus, build_planned_thread_domains, next_tick_holdoff_ticks,
    prune_dead_threads, read_cpu_states, refresh_thread_from_task, setup_logging,
    update_cpu_domain_map, update_domain_state_maps, AffinityCache, PerfEventFds,
};
use crate::bpf::{BpfScheduler, DispatchedTask, QueuedTask, RL_CPU_ANY};
use crate::cli::{NsdiPolicyKind, Opts};
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
use crate::overhead_profile::{self, OverheadProfileConfig, ThreadClockSample};
#[cfg(feature = "diagnostics")]
use crate::policy::build_record;
use crate::policy::PlacementDecision;
use crate::policy_nsdi::choose_placement_with_owned_cpus;
use crate::runtime_log;
use crate::topology;
#[cfg(feature = "diagnostics")]
use crate::types::TickStatsSnapshot;
use crate::types::{
    now_ns, pct_to_x100, CpuStateValue, ManagedThreadState, PlannedCpuState, PolicyConfig,
    DEFAULT_SLICE_US,
};
use crate::workload::{
    ensure_cgroup_dir, ensure_current_thread_sched_other, launch_shell_workload, launch_workload,
    restrict_cgroup_cpus, shutdown_child_gracefully, CgroupGuard, WorkloadAdopter,
};
use anyhow::Result;
use clap::Parser;
use crossbeam_channel::{never, unbounded};
use libbpf_rs::OpenObject;
use scx_utils::try_set_rlimit_infinity;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "light-compete")]
use crate::app::{shutdown_light_helpers, spawn_light_helpers};

#[derive(Clone, Debug)]
struct PendingTask {
    task: QueuedTask,
    queued_at_ns: u64,
}

#[derive(Clone, Debug)]
struct NsdiAppState {
    id: usize,
    root_pid: u32,
    label: String,
    owned_cpus: BTreeSet<u32>,
    pending_tasks: u32,
    pending_delay_ewma_us: u64,
    last_scale_at_ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct NsdiConfig {
    policy: NsdiPolicyKind,
    control_interval_ns: u64,
    delay_low_us: u64,
    delay_high_us: u64,
}

struct WorkloadProcess {
    label: String,
    child: Child,
    exited_at: Option<Instant>,
}

enum PendingSelection {
    Runnable {
        index: usize,
        decision: PlacementDecision,
    },
    Drop {
        index: usize,
    },
}

fn format_cpus(cpus: &BTreeSet<u32>) -> String {
    if cpus.is_empty() {
        return "-".to_string();
    }
    cpus.iter()
        .map(|cpu| cpu.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("PPid:\t") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn resolve_app_id_with_lookup(
    pid: u32,
    pid_to_app: &mut BTreeMap<u32, usize>,
    root_to_app: &BTreeMap<u32, usize>,
    parent_lookup: &impl Fn(u32) -> Option<u32>,
) -> Option<usize> {
    if let Some(app_id) = pid_to_app.get(&pid).copied() {
        return Some(app_id);
    }
    let mut lineage = Vec::new();
    let mut current = Some(pid);
    let mut depth = 0usize;
    while let Some(value) = current {
        if let Some(app_id) = pid_to_app
            .get(&value)
            .copied()
            .or_else(|| root_to_app.get(&value).copied())
        {
            for visited in lineage {
                pid_to_app.insert(visited, app_id);
            }
            pid_to_app.insert(pid, app_id);
            return Some(app_id);
        }
        lineage.push(value);
        current = parent_lookup(value);
        depth += 1;
        if depth >= 256 {
            break;
        }
    }
    None
}

fn resolve_app_id(
    pid: u32,
    pid_to_app: &mut BTreeMap<u32, usize>,
    root_to_app: &BTreeMap<u32, usize>,
) -> Option<usize> {
    resolve_app_id_with_lookup(pid, pid_to_app, root_to_app, &proc_ppid)
}

fn build_pid_to_app_map(adopters: &[WorkloadAdopter]) -> BTreeMap<u32, usize> {
    let mut out = BTreeMap::new();
    for (app_id, adopter) in adopters.iter().enumerate() {
        for pid in adopter.tracked_pids() {
            out.insert(pid, app_id);
        }
    }
    out
}

fn pending_snapshot(pending: &VecDeque<PendingTask>, usersched_pid: u32) -> VecDeque<QueuedTask> {
    pending
        .iter()
        .filter(|entry| entry.task.tgid as u32 != usersched_pid)
        .filter(|entry| !is_launch_helper_task(&entry.task))
        .map(|entry| entry.task.clone())
        .collect::<VecDeque<_>>()
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

fn reset_app_metrics(app_states: &mut [NsdiAppState]) {
    for app in app_states {
        app.pending_tasks = 0;
    }
}

fn refresh_app_pending_metrics(
    app_states: &mut [NsdiAppState],
    pending: &VecDeque<PendingTask>,
    pid_to_app: &mut BTreeMap<u32, usize>,
    root_to_app: &BTreeMap<u32, usize>,
    usersched_pid: u32,
    now_ns: u64,
) -> bool {
    reset_app_metrics(app_states);
    let mut pending_delay_sum_us = vec![0u64; app_states.len()];
    for entry in pending {
        let task = &entry.task;
        if task.tgid as u32 == usersched_pid || is_launch_helper_task(task) {
            continue;
        }
        let Some(app_id) = resolve_app_id(task.tgid as u32, pid_to_app, root_to_app) else {
            continue;
        };
        let delay_us = now_ns.saturating_sub(entry.queued_at_ns) / 1_000;
        app_states[app_id].pending_tasks = app_states[app_id].pending_tasks.saturating_add(1);
        pending_delay_sum_us[app_id] = pending_delay_sum_us[app_id].saturating_add(delay_us);
    }

    let mut zero_owned_pending = false;
    for app in app_states {
        if app.pending_tasks == 0 {
            app.pending_delay_ewma_us = 0;
            continue;
        }
        let sample = pending_delay_sum_us[app.id] / u64::from(app.pending_tasks);
        app.pending_delay_ewma_us = if app.pending_delay_ewma_us == 0 {
            sample
        } else {
            (app.pending_delay_ewma_us
                .saturating_mul(3)
                .saturating_add(sample))
                / 4
        };
        zero_owned_pending |= app.owned_cpus.is_empty();
    }
    zero_owned_pending
}

fn initialize_static_cpu_partition(app_states: &mut [NsdiAppState], eligible_cpus: &BTreeSet<u32>) {
    let sorted = eligible_cpus.iter().copied().collect::<Vec<_>>();
    let app_count = app_states.len();
    if app_count == 0 {
        return;
    }
    let base = sorted.len() / app_count;
    let remainder = sorted.len() % app_count;
    let mut cursor = 0usize;

    for app in app_states {
        let share = base + usize::from(app.id < remainder);
        app.owned_cpus = sorted[cursor..cursor + share].iter().copied().collect();
        cursor += share;
        runtime_log::line(format!(
            "nsdi_ownership_init app={} root_pid={} label={} mode=static owned_cpus={}",
            app.id,
            app.root_pid,
            app.label,
            format_cpus(&app.owned_cpus),
        ));
    }
}

fn planned_jobs_len(planned_cpu_states: &[PlannedCpuState], cpu: u32) -> usize {
    planned_cpu_states
        .get(cpu as usize)
        .map(|state| state.jobs.len())
        .unwrap_or(0)
}

fn all_owned_cpus(app_states: &[NsdiAppState]) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for app in app_states {
        out.extend(app.owned_cpus.iter().copied());
    }
    out
}

fn free_cpus(app_states: &[NsdiAppState], eligible_cpus: &BTreeSet<u32>) -> BTreeSet<u32> {
    let owned = all_owned_cpus(app_states);
    eligible_cpus.difference(&owned).copied().collect()
}

fn choose_revocable_cpu(
    app: &NsdiAppState,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    policy: PolicyConfig,
) -> Option<u32> {
    app.owned_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
            let planned_jobs = planned_jobs_len(planned_cpu_states, *cpu);
            let effectively_idle = crate::policy::effectively_idle_cpu(
                *cpu,
                cpu_states,
                cpu_utils_x100,
                policy.cpu_high_util_x100,
            );
            effectively_idle || planned_jobs <= 1 || state.current_tid == 0
        })
        .min_by_key(|cpu| {
            let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
            let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
            let effectively_idle = crate::policy::effectively_idle_cpu(
                *cpu,
                cpu_states,
                cpu_utils_x100,
                policy.cpu_high_util_x100,
            );
            (
                !effectively_idle,
                planned_jobs_len(planned_cpu_states, *cpu),
                util,
                state.cpu_dsq_depth,
                *cpu,
            )
        })
}

fn log_scale_event(app: &NsdiAppState, action: &str, cpu: u32, detail: &str) {
    runtime_log::line(format!(
        "nsdi_scale action={} app={} root_pid={} label={} cpu={} detail={} owned_cpus={} pending={} delay_ewma_us={}",
        action,
        app.id,
        app.root_pid,
        app.label,
        cpu,
        detail,
        format_cpus(&app.owned_cpus),
        app.pending_tasks,
        app.pending_delay_ewma_us,
    ));
}

fn maybe_run_delay_range_controller(
    app_states: &mut [NsdiAppState],
    eligible_cpus: &BTreeSet<u32>,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    policy: PolicyConfig,
    cfg: NsdiConfig,
    now_ns: u64,
) -> bool {
    if !matches!(cfg.policy, NsdiPolicyKind::DelayRange) {
        return false;
    }

    let mut changed_apps = BTreeSet::new();
    let mut free_pool = free_cpus(app_states, eligible_cpus);

    for app_id in 0..app_states.len() {
        let should_release = {
            let app = &app_states[app_id];
            app.pending_delay_ewma_us < cfg.delay_low_us
                && !app.owned_cpus.is_empty()
                && !(app.pending_tasks > 0 && app.owned_cpus.len() <= 1)
        };
        if !should_release {
            continue;
        }
        let Some(cpu) = choose_revocable_cpu(
            &app_states[app_id],
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            policy,
        ) else {
            continue;
        };
        {
            let app = &mut app_states[app_id];
            app.owned_cpus.remove(&cpu);
            app.last_scale_at_ns = now_ns;
            log_scale_event(app, "revoke", cpu, "below_low");
        }
        free_pool.insert(cpu);
        changed_apps.insert(app_id);
    }

    let mut recipients = app_states
        .iter()
        .filter(|app| {
            app.pending_tasks > 0
                && (app.owned_cpus.is_empty() || app.pending_delay_ewma_us > cfg.delay_high_us)
        })
        .map(|app| (app.owned_cpus.is_empty(), app.pending_delay_ewma_us, app.id))
        .collect::<Vec<_>>();
    recipients.sort_by_key(|(owned_empty, delay, app_id)| (!owned_empty, Reverse(*delay), *app_id));

    for (_, _, app_id) in recipients {
        if changed_apps.contains(&app_id) {
            continue;
        }

        let granted_cpu = if let Some(cpu) = free_pool.pop_first() {
            Some((None, cpu))
        } else {
            let donor = app_states
                .iter()
                .filter(|app| {
                    app.id != app_id
                        && !changed_apps.contains(&app.id)
                        && app.pending_delay_ewma_us < cfg.delay_low_us
                        && !app.owned_cpus.is_empty()
                        && !(app.pending_tasks > 0 && app.owned_cpus.len() <= 1)
                })
                .filter_map(|app| {
                    choose_revocable_cpu(
                        app,
                        cpu_states,
                        cpu_utils_x100,
                        planned_cpu_states,
                        policy,
                    )
                    .map(|cpu| (app.id, cpu))
                })
                .min_by_key(|(_, cpu)| {
                    let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
                    let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
                    (
                        planned_jobs_len(planned_cpu_states, *cpu),
                        util,
                        state.cpu_dsq_depth,
                        *cpu,
                    )
                });

            donor.map(|(donor_id, cpu)| (Some(donor_id), cpu))
        };

        let Some((donor_id, cpu)) = granted_cpu else {
            continue;
        };

        if let Some(donor_id) = donor_id {
            let donor = &mut app_states[donor_id];
            donor.owned_cpus.remove(&cpu);
            donor.last_scale_at_ns = now_ns;
            log_scale_event(donor, "revoke", cpu, &format!("transfer_to_app_{app_id}"));
            changed_apps.insert(donor_id);
        }

        let recipient = &mut app_states[app_id];
        recipient.owned_cpus.insert(cpu);
        recipient.last_scale_at_ns = now_ns;
        let detail = donor_id
            .map(|id| format!("grant_from_app_{id}"))
            .unwrap_or_else(|| "grant_from_free".to_string());
        log_scale_event(recipient, "grant", cpu, &detail);
        changed_apps.insert(app_id);
    }

    !changed_apps.is_empty()
}

fn select_next_pending(
    pending: &VecDeque<PendingTask>,
    threads: &BTreeMap<u32, ManagedThreadState>,
    topo: &crate::types::TopologyLayout,
    llc_states: &[Option<crate::types::LlcStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    affinity: &mut AffinityCache,
    app_states: &[NsdiAppState],
    pid_to_app: &mut BTreeMap<u32, usize>,
    root_to_app: &BTreeMap<u32, usize>,
    now_ns: u64,
    policy: PolicyConfig,
) -> Result<Option<PendingSelection>> {
    let mut first_runnable: Option<(usize, PlacementDecision)> = None;
    let mut drop_index: Option<usize> = None;

    for (index, pending_task) in pending.iter().enumerate() {
        let task = &pending_task.task;
        let tid = task.tid as u32;
        let allowed_cpus = match affinity.allowed_cpus(tid, now_ns) {
            Ok(cpus) if !cpus.is_empty() => cpus,
            _ => {
                drop_index.get_or_insert(index);
                continue;
            }
        };
        let Some(app_id) = resolve_app_id(task.tgid as u32, pid_to_app, root_to_app) else {
            continue;
        };
        let Some(decision) = choose_placement_with_owned_cpus(
            task,
            threads.get(&tid),
            topo,
            llc_states,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            &allowed_cpus,
            &app_states[app_id].owned_cpus,
            now_ns,
            policy,
        ) else {
            continue;
        };
        if first_runnable.is_none() {
            first_runnable = Some((index, decision));
        }
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
        "rustland_la_nsdi",
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

    let mut app_states = workloads
        .iter()
        .enumerate()
        .map(|(id, workload)| NsdiAppState {
            id,
            root_pid: workload.child.id(),
            label: workload.label.clone(),
            owned_cpus: BTreeSet::new(),
            pending_tasks: 0,
            pending_delay_ewma_us: 0,
            last_scale_at_ns: 0,
        })
        .collect::<Vec<_>>();
    if matches!(opts.nsdi_policy, NsdiPolicyKind::Static) {
        initialize_static_cpu_partition(&mut app_states, &mapping.eligible_cpus);
    } else {
        for app in &app_states {
            runtime_log::line(format!(
                "nsdi_ownership_init app={} root_pid={} label={} mode=delay-range owned_cpus={}",
                app.id,
                app.root_pid,
                app.label,
                format_cpus(&app.owned_cpus),
            ));
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
    let mut pending = VecDeque::<PendingTask>::new();
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
        signature_snapshots: false,
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
    let nsdi_cfg = NsdiConfig {
        policy: opts.nsdi_policy,
        control_interval_ns: opts.nsdi_control_ms.max(1).saturating_mul(1_000_000),
        delay_low_us: opts.nsdi_delay_low_us,
        delay_high_us: opts.nsdi_delay_high_us.max(opts.nsdi_delay_low_us),
    };
    let overhead_profiler = overhead_profile::global();
    macro_rules! profile_start {
        () => {
            overhead_profiler
                .as_ref()
                .map(|_| ThreadClockSample::capture())
        };
    }
    macro_rules! profile_record {
        ($scope:literal, $start:expr, $success:expr) => {
            if let (Some(profiler), Some(start)) = (&overhead_profiler, $start) {
                profiler.record_scope($scope, start, $success);
            }
        };
    }
    let mut last_control_ns = 0u64;
    let mut last_guard_check = Instant::now() - Duration::from_secs(1);
    let guard_every = Duration::from_millis(250);

    while !bpf.exited() {
        let loop_start = profile_start!();
        for adopter in adopters.iter_mut() {
            adopter.maybe_sync();
        }
        let guard_start = profile_start!();
        if opts.guard_clean_cgroup && last_guard_check.elapsed() >= guard_every {
            let mut tracked = BTreeSet::new();
            for adopter in &adopters {
                tracked.extend(adopter.tracked_pids());
            }
            cgroup_guard.set_allowed_anchors(tracked);
            cgroup_guard.assert_runtime_membership()?;
            last_guard_check = Instant::now();
        }
        profile_record!("nsdi_guard_check", guard_start, true);
        let sample_start = profile_start!();
        apply_sample_updates(
            &llc_rx,
            &df_rx,
            &never(),
            &mut llc_states,
            &mut df_states,
            &mut [],
        );
        update_domain_state_maps(&mut bpf, &llc_states, &df_states)?;
        profile_record!("nsdi_apply_samples", sample_start, true);
        let prune_start = profile_start!();
        prune_dead_threads(&mut threads, &mut affinity);
        profile_record!("nsdi_prune_dead_threads", prune_start, true);

        if last_cpu_util.elapsed() >= cpu_util_every {
            let _ = cpu_util.sample(topo.nr_cpu_ids);
            last_cpu_util = Instant::now();
        }

        let dequeue_start = profile_start!();
        loop {
            match bpf.dequeue_task() {
                Ok(Some(task)) => pending.push_back(PendingTask {
                    task,
                    queued_at_ns: now_ns(),
                }),
                Ok(None) => break,
                Err(err) => {
                    log::warn!("ringbuf dequeue error: {err}");
                    break;
                }
            }
        }
        profile_record!("nsdi_ringbuf_drain", dequeue_start, true);
        bpf.publish_scheduled(pending.len() as u64);

        let cpu_states_start = profile_start!();
        let mut cpu_states = read_cpu_states(&mut bpf, topo.nr_cpu_ids)?;
        profile_record!("nsdi_read_cpu_states", cpu_states_start, true);

        let prepass_start = profile_start!();
        if !pending.is_empty() {
            let prepass_now = now_ns();
            for pending_task in &pending {
                let task = &pending_task.task;
                if task.tgid as u32 == usersched_pid || is_launch_helper_task(task) {
                    continue;
                }
                let tid = task.tid as u32;
                let entry = threads.entry(tid).or_default();
                refresh_thread_from_task(entry, task, prepass_now);
            }
        }
        profile_record!("nsdi_pending_prepass", prepass_start, true);

        let metrics_start = profile_start!();
        let mut pid_to_app = build_pid_to_app_map(&adopters);
        let root_to_app = app_states
            .iter()
            .map(|app| (app.root_pid, app.id))
            .collect::<BTreeMap<_, _>>();
        let metric_now = now_ns();
        let zero_owned_pending = refresh_app_pending_metrics(
            &mut app_states,
            &pending,
            &mut pid_to_app,
            &root_to_app,
            usersched_pid,
            metric_now,
        );
        profile_record!("nsdi_pending_metrics", metrics_start, true);

        let plan_start = profile_start!();
        let pending_tasks = pending_snapshot(&pending, usersched_pid);
        let mut planned_domains = build_planned_thread_domains(&threads, &pending_tasks);
        let mut planned_cpus = build_planned_thread_cpus(&threads, &pending_tasks);
        let mut planned_cpu_states = build_planned_cpu_states(topo.nr_cpu_ids, &planned_cpus);
        profile_record!("nsdi_plan_build", plan_start, true);

        if matches!(nsdi_cfg.policy, NsdiPolicyKind::DelayRange)
            && ((last_control_ns == 0
                || metric_now.saturating_sub(last_control_ns) >= nsdi_cfg.control_interval_ns)
                || zero_owned_pending)
        {
            let controller_start = profile_start!();
            if maybe_run_delay_range_controller(
                &mut app_states,
                &mapping.eligible_cpus,
                &cpu_states,
                cpu_util.values(),
                &planned_cpu_states,
                policy,
                nsdi_cfg,
                metric_now,
            ) {
                last_control_ns = metric_now;
            } else if last_control_ns == 0
                || metric_now.saturating_sub(last_control_ns) >= nsdi_cfg.control_interval_ns
            {
                last_control_ns = metric_now;
            }
            profile_record!("nsdi_delay_range_controller", controller_start, true);
        }

        let dispatch_loop_start = profile_start!();
        while !pending.is_empty() {
            if let Some(index) = pending
                .iter()
                .position(|task| task.task.tgid as u32 == usersched_pid)
            {
                let pending_task = pending
                    .remove(index)
                    .expect("usersched task disappeared from pending queue");
                let mut dispatched = DispatchedTask::new(&pending_task.task);
                dispatched.cpu = RL_CPU_ANY;
                dispatched.slice_ns = DEFAULT_SLICE_US * 1_000;
                if bpf.dispatch_task(&dispatched).is_err() {
                    pending.push_front(pending_task);
                    break;
                }
                continue;
            }

            if let Some(index) = pending
                .iter()
                .position(|task| is_launch_helper_task(&task.task))
            {
                let pending_task = pending
                    .remove(index)
                    .expect("launch helper task disappeared from pending queue");
                let tid = pending_task.task.tid as u32;
                let allowed_cpus = affinity.allowed_cpus(tid, now_ns()).ok();
                let mut dispatched = DispatchedTask::new(&pending_task.task);
                dispatched.cpu = launch_helper_cpu(&pending_task.task, allowed_cpus.as_ref());
                dispatched.slice_ns = DEFAULT_SLICE_US * 1_000;
                if bpf.dispatch_task(&dispatched).is_err() {
                    pending.push_front(pending_task);
                    break;
                }
                continue;
            }

            let now = now_ns();
            let select_start = profile_start!();
            let selection = select_next_pending(
                &pending,
                &threads,
                &topo,
                &llc_states,
                &cpu_states,
                cpu_util.values(),
                &planned_cpu_states,
                &mut affinity,
                &app_states,
                &mut pid_to_app,
                &root_to_app,
                now,
                policy,
            )?;
            profile_record!("nsdi_select_next_pending", select_start, true);
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
            let pending_task = pending
                .remove(index)
                .expect("selected task disappeared from pending queue");
            let task = pending_task.task;

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
            let dispatch_start = profile_start!();
            if bpf.dispatch_task(&dispatched).is_err() {
                pending.push_front(PendingTask {
                    task,
                    queued_at_ns: pending_task.queued_at_ns,
                });
                break;
            }
            profile_record!("nsdi_dispatch_task", dispatch_start, true);

            let comm = crate::types::comm_to_string(&task.comm);
            #[cfg(feature = "diagnostics")]
            {
                let log_start = profile_start!();
                let record = build_record(&task, &decision, comm.clone());
                logger.write(&record)?;
                if recent.len() >= 64 {
                    recent.pop_front();
                }
                recent.push_back(record);
                profile_record!("nsdi_decision_log_write", log_start, true);
            }

            let selected_cpu = decision
                .selected_cpu
                .or_else(|| u32::try_from(task.current_cpu).ok());
            let selected_domain = decision
                .selected_domain
                .or_else(|| u32::try_from(task.current_domain).ok());
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

            let _ = &mut planned_domains;
        }
        profile_record!("nsdi_dispatch_loop", dispatch_loop_start, true);

        let notify_start = profile_start!();
        bpf.notify_complete(pending.len() as u64);
        profile_record!("nsdi_notify_complete", notify_start, true);
        overhead_profile::flush_global(false);

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

        let workload_wait_start = profile_start!();
        for workload in workloads.iter_mut() {
            if workload.child.try_wait()?.is_some() && workload.exited_at.is_none() {
                workload.exited_at = Some(Instant::now());
            }
        }
        profile_record!("nsdi_workload_try_wait", workload_wait_start, true);
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
        profile_record!("nsdi_loop_total", loop_start, true);
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
    overhead_profile::shutdown_global();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{QueueTrigger, COMM_LEN};

    fn make_app(id: usize, owned: &[u32]) -> NsdiAppState {
        NsdiAppState {
            id,
            root_pid: (id + 1) as u32,
            label: format!("app-{id}"),
            owned_cpus: owned.iter().copied().collect(),
            pending_tasks: 0,
            pending_delay_ewma_us: 0,
            last_scale_at_ns: 0,
        }
    }

    fn cfg() -> NsdiConfig {
        NsdiConfig {
            policy: NsdiPolicyKind::DelayRange,
            control_interval_ns: 1_000_000,
            delay_low_us: 500,
            delay_high_us: 1_000,
        }
    }

    fn policy_cfg() -> PolicyConfig {
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
            signature_snapshots: false,
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

    fn queued_task(tid: u32, tgid: u32, current_cpu: i32) -> QueuedTask {
        QueuedTask {
            tid: tid as i32,
            tgid: tgid as i32,
            current_cpu,
            current_domain: if current_cpu >= 2 { 1 } else { 0 },
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
            last_fill_bw_mib_s_x100: [0; 3],
            ewma_fill_bw_mib_s_x100: [0; 3],
            last_ipc_x1000: 4_000,
            ewma_ipc_x1000: 4_000,
            last_stall_pct_x100: 0,
            ewma_stall_pct_x100: 0,
            trigger: QueueTrigger::Enqueue as u32,
            tick_seq: 0,
            last_update_ns: 0,
            comm: [0; COMM_LEN],
        }
    }

    fn queued_task_with_comm(tid: u32, tgid: u32, current_cpu: i32, comm: &str) -> QueuedTask {
        let mut task = queued_task(tid, tgid, current_cpu);
        for (idx, byte) in comm.as_bytes().iter().take(task.comm.len() - 1).enumerate() {
            task.comm[idx] = *byte as i8;
        }
        task
    }

    #[test]
    fn resolve_app_id_follows_root_lineage() {
        let mut pid_to_app = BTreeMap::from([(101u32, 0usize)]);
        let roots = BTreeMap::from([(100u32, 0usize), (200u32, 1usize)]);
        let parents = BTreeMap::from([(102u32, Some(101u32)), (101u32, Some(100u32))]);
        let app_id = resolve_app_id_with_lookup(102, &mut pid_to_app, &roots, &|pid| {
            parents.get(&pid).copied().flatten()
        });

        assert_eq!(app_id, Some(0));
        assert_eq!(pid_to_app.get(&102).copied(), Some(0));
    }

    #[test]
    fn static_partition_is_even_and_deterministic() {
        let mut apps = vec![make_app(0, &[]), make_app(1, &[]), make_app(2, &[])];
        initialize_static_cpu_partition(&mut apps, &BTreeSet::from([0, 1, 2, 3, 4]));

        assert_eq!(apps[0].owned_cpus, BTreeSet::from([0, 1]));
        assert_eq!(apps[1].owned_cpus, BTreeSet::from([2, 3]));
        assert_eq!(apps[2].owned_cpus, BTreeSet::from([4]));
    }

    #[test]
    fn planned_inputs_ignore_usersched_pending_tasks() {
        let pending = VecDeque::from([
            PendingTask {
                task: queued_task(10, 100, 0),
                queued_at_ns: 0,
            },
            PendingTask {
                task: queued_task(20, 999, 1),
                queued_at_ns: 0,
            },
        ]);
        let planned = build_planned_thread_cpus(&BTreeMap::new(), &pending_snapshot(&pending, 999));

        assert_eq!(planned.get(&10), Some(&0));
        assert!(!planned.contains_key(&20));
    }

    #[test]
    fn planned_inputs_ignore_launch_helper_tasks() {
        let pending = VecDeque::from([
            PendingTask {
                task: queued_task_with_comm(10, 100, 0, "rocksdb:high0"),
                queued_at_ns: 0,
            },
            PendingTask {
                task: queued_task_with_comm(20, 100, 1, "memory_benchmar"),
                queued_at_ns: 0,
            },
        ]);
        let planned = build_planned_thread_cpus(&BTreeMap::new(), &pending_snapshot(&pending, 999));

        assert_eq!(planned.get(&10), Some(&0));
        assert!(!planned.contains_key(&20));
    }

    #[test]
    fn pending_metrics_ignore_usersched_tasks() {
        let mut apps = vec![make_app(0, &[])];
        let pending = VecDeque::from([
            PendingTask {
                task: queued_task(10, 100, 0),
                queued_at_ns: 0,
            },
            PendingTask {
                task: queued_task(20, 999, 1),
                queued_at_ns: 0,
            },
        ]);
        let mut pid_to_app = BTreeMap::from([(100, 0)]);
        let root_to_app = BTreeMap::from([(100, 0)]);

        let zero_owned_pending = refresh_app_pending_metrics(
            &mut apps,
            &pending,
            &mut pid_to_app,
            &root_to_app,
            999,
            2_000_000,
        );

        assert!(zero_owned_pending);
        assert_eq!(apps[0].pending_tasks, 1);
        assert_eq!(apps[0].pending_delay_ewma_us, 2_000);
    }

    #[test]
    fn pending_metrics_ignore_launch_helper_tasks() {
        let mut apps = vec![make_app(0, &[])];
        let pending = VecDeque::from([
            PendingTask {
                task: queued_task_with_comm(10, 100, 0, "rocksdb:high0"),
                queued_at_ns: 0,
            },
            PendingTask {
                task: queued_task_with_comm(20, 100, 1, "memory_benchmar"),
                queued_at_ns: 0,
            },
        ]);
        let mut pid_to_app = BTreeMap::from([(100, 0)]);
        let root_to_app = BTreeMap::from([(100, 0)]);

        let zero_owned_pending = refresh_app_pending_metrics(
            &mut apps,
            &pending,
            &mut pid_to_app,
            &root_to_app,
            999,
            2_000_000,
        );

        assert!(zero_owned_pending);
        assert_eq!(apps[0].pending_tasks, 1);
        assert_eq!(apps[0].pending_delay_ewma_us, 2_000);
    }

    #[test]
    fn launch_helper_bypass_uses_shared_or_single_allowed_cpu() {
        let helper = queued_task_with_comm(10, 100, 5, "memory_benchmar");
        let worker = queued_task_with_comm(11, 100, 6, "rocksdb:high0");
        let allowed = BTreeSet::from([5, 6, 12]);

        assert!(is_launch_helper_task(&helper));
        assert!(!is_launch_helper_task(&worker));
        assert_eq!(launch_helper_cpu(&helper, Some(&allowed)), RL_CPU_ANY);
        assert_eq!(launch_helper_cpu(&helper, Some(&BTreeSet::from([12]))), 12);
        assert_eq!(launch_helper_cpu(&helper, None), RL_CPU_ANY);
    }

    #[test]
    fn delay_range_grants_free_cpu_to_zero_owned_app() {
        let mut apps = vec![make_app(0, &[]), make_app(1, &[1])];
        apps[0].pending_tasks = 1;
        apps[0].pending_delay_ewma_us = 1_500;
        apps[1].pending_tasks = 0;
        apps[1].pending_delay_ewma_us = 0;

        let changed = maybe_run_delay_range_controller(
            &mut apps,
            &BTreeSet::from([0, 1]),
            &vec![CpuStateValue::default(); 2],
            &[0, 0],
            &vec![PlannedCpuState::default(); 2],
            policy_cfg(),
            cfg(),
            1_000,
        );

        assert!(changed);
        assert_eq!(apps[0].owned_cpus, BTreeSet::from([0]));
    }

    #[test]
    fn delay_range_revokes_from_low_delay_donor() {
        let mut apps = vec![make_app(0, &[]), make_app(1, &[0])];
        apps[0].pending_tasks = 1;
        apps[0].pending_delay_ewma_us = 2_000;
        apps[1].pending_tasks = 0;
        apps[1].pending_delay_ewma_us = 100;

        let changed = maybe_run_delay_range_controller(
            &mut apps,
            &BTreeSet::from([0]),
            &[CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            }],
            &[0],
            &vec![PlannedCpuState::default(); 1],
            policy_cfg(),
            cfg(),
            2_000,
        );

        assert!(changed);
        assert_eq!(apps[0].owned_cpus, BTreeSet::from([0]));
        assert!(apps[1].owned_cpus.is_empty());
    }

    #[test]
    fn delay_range_keeps_last_cpu_for_pending_low_delay_app() {
        let mut apps = vec![make_app(0, &[0])];
        apps[0].pending_tasks = 1;
        apps[0].pending_delay_ewma_us = 100;

        let changed = maybe_run_delay_range_controller(
            &mut apps,
            &BTreeSet::from([0]),
            &[CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            }],
            &[0],
            &vec![PlannedCpuState::default(); 1],
            policy_cfg(),
            cfg(),
            3_000,
        );

        assert!(!changed);
        assert_eq!(apps[0].owned_cpus, BTreeSet::from([0]));
    }

    #[test]
    fn delay_range_does_not_transfer_last_cpu_from_pending_low_delay_donor() {
        let mut apps = vec![make_app(0, &[]), make_app(1, &[0])];
        apps[0].pending_tasks = 1;
        apps[0].pending_delay_ewma_us = 2_000;
        apps[1].pending_tasks = 1;
        apps[1].pending_delay_ewma_us = 100;

        let changed = maybe_run_delay_range_controller(
            &mut apps,
            &BTreeSet::from([0]),
            &[CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            }],
            &[0],
            &vec![PlannedCpuState::default(); 1],
            policy_cfg(),
            cfg(),
            4_000,
        );

        assert!(!changed);
        assert!(apps[0].owned_cpus.is_empty());
        assert_eq!(apps[1].owned_cpus, BTreeSet::from([0]));
    }
}

#[cfg(test)]
#[path = "app_nsdi_paper_tests.rs"]
mod paper_tests;
