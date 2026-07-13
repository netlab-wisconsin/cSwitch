use super::*;
use crate::types::{DomainInfo, LlcStateValue, QueueTrigger, TopologyLayout, COMM_LEN};

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

fn idle_cpu_states(cpu_count: usize) -> Vec<CpuStateValue> {
    (0..cpu_count)
        .map(|_| CpuStateValue {
            domain_id: 0,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        })
        .collect()
}

fn queued_task(tid: u32, tgid: u32, current_cpu: i32, current_domain: i32) -> QueuedTask {
    QueuedTask {
        tid: tid as i32,
        tgid: tgid as i32,
        current_cpu,
        current_domain,
        nr_cpus_allowed: 1,
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

#[test]
fn delay_range_recipients_prefer_zero_owned_then_highest_delay_then_app_id() {
    let mut apps = vec![
        make_app(0, &[]),
        make_app(1, &[]),
        make_app(2, &[]),
        make_app(3, &[3]),
    ];
    apps[0].pending_tasks = 1;
    apps[0].pending_delay_ewma_us = 2_000;
    apps[1].pending_tasks = 1;
    apps[1].pending_delay_ewma_us = 2_000;
    apps[2].pending_tasks = 1;
    apps[2].pending_delay_ewma_us = 3_000;
    apps[3].pending_tasks = 1;
    apps[3].pending_delay_ewma_us = 5_000;

    let now_ns = 10_000;
    // Verifies NSDI delay-range recipient ordering.
    // Expected: zero-owned pending apps receive free CPUs first; within that
    // group, higher pending-delay EWMA wins, then lower app id breaks ties.
    let changed = maybe_run_delay_range_controller(
        &mut apps,
        &BTreeSet::from([0, 1, 2, 3]),
        &idle_cpu_states(4),
        &[0; 4],
        &vec![PlannedCpuState::default(); 4],
        policy_cfg(),
        cfg(),
        now_ns,
    );

    assert!(changed);
    assert_eq!(apps[2].owned_cpus, BTreeSet::from([0]));
    assert_eq!(apps[0].owned_cpus, BTreeSet::from([1]));
    assert_eq!(apps[1].owned_cpus, BTreeSet::from([2]));
    assert_eq!(apps[3].owned_cpus, BTreeSet::from([3]));
}

#[test]
fn revocable_cpu_choice_prefers_planned_jobs_then_util_then_dsq_depth() {
    let app = make_app(0, &[0, 1, 2, 3]);
    let mut planned_cpu_states = vec![PlannedCpuState::default(); 4];
    planned_cpu_states[0].jobs = vec![10];
    let mut cpu_states = idle_cpu_states(4);
    cpu_states[3].cpu_dsq_depth = 5;
    let cpu_utils_x100 = [100, 9_000, 100, 100];
    // Verifies donor CPU choice for delay-range revocation.
    // Expected: among revocable CPUs, choose by fewer planned jobs, then lower
    // sampled CPU util, lower DSQ depth, and finally CPU id.
    let chosen_cpu = choose_revocable_cpu(
        &app,
        &cpu_states,
        &cpu_utils_x100,
        &planned_cpu_states,
        policy_cfg(),
    );

    assert_eq!(chosen_cpu, Some(2));
}

#[test]
fn pending_delay_ewma_smooths_successive_samples() {
    let mut apps = vec![make_app(0, &[])];
    let pending = VecDeque::from([PendingTask {
        task: queued_task(10, 100, 0, 0),
        queued_at_ns: 0,
    }]);
    let root_to_app = BTreeMap::from([(100, 0)]);
    let mut pid_to_app = BTreeMap::from([(100, 0)]);

    let first_now_ns = 2_000_000;
    let second_now_ns = 6_000_000;
    // Verifies pending-delay EWMA smoothing.
    // Expected: first sample initializes the EWMA; second sample applies the
    // current 3/4 old + 1/4 new update, producing 3000 us from 2000/6000 us.
    let first_zero_owned = refresh_app_pending_metrics(
        &mut apps,
        &pending,
        &mut pid_to_app,
        &root_to_app,
        999,
        first_now_ns,
    );
    assert!(first_zero_owned);
    assert_eq!(apps[0].pending_delay_ewma_us, 2_000);

    let second_zero_owned = refresh_app_pending_metrics(
        &mut apps,
        &pending,
        &mut pid_to_app,
        &root_to_app,
        999,
        second_now_ns,
    );
    assert!(second_zero_owned);
    assert_eq!(apps[0].pending_delay_ewma_us, 3_000);
}

#[test]
fn delay_range_makes_no_change_without_free_cpu_or_revocable_donor() {
    let mut apps = vec![make_app(0, &[]), make_app(1, &[0])];
    apps[0].pending_tasks = 1;
    apps[0].pending_delay_ewma_us = 2_000;
    apps[1].pending_tasks = 0;
    apps[1].pending_delay_ewma_us = 2_000;

    let now_ns = 20_000;
    // Verifies the no-op path when scale-up has nowhere to get a CPU.
    // Expected: a high-delay zero-owned app remains unchanged if there is no
    // free CPU and no below-low donor that can safely give one up.
    let changed = maybe_run_delay_range_controller(
        &mut apps,
        &BTreeSet::from([0]),
        &idle_cpu_states(1),
        &[0],
        &vec![PlannedCpuState::default(); 1],
        policy_cfg(),
        cfg(),
        now_ns,
    );

    assert!(!changed);
    assert!(apps[0].owned_cpus.is_empty());
    assert_eq!(apps[1].owned_cpus, BTreeSet::from([0]));
}

#[test]
fn select_next_pending_skips_unresolved_and_empty_owned_apps_for_later_runnable() {
    let now_ns = 10_000_000;
    let tid = std::process::id();
    let mut affinity = AffinityCache::new(1_000);
    let allowed_cpus = affinity.allowed_cpus(tid, now_ns).unwrap();
    let cpu = *allowed_cpus
        .iter()
        .next()
        .expect("current process has no CPU affinity");
    let cpu_count = cpu as usize + 1;
    let mut cpu_to_domain = vec![None; cpu_count];
    cpu_to_domain[cpu as usize] = Some(0);
    let topo = TopologyLayout {
        nr_cpu_ids: cpu_count,
        domains: vec![DomainInfo {
            domain_id: 0,
            kernel_l3_id: 0,
            rep_cpu: cpu,
            cpus: vec![cpu],
            l3_size_mb: 32.0,
        }],
        cpu_to_domain,
    };
    let pending = VecDeque::from([
        PendingTask {
            task: queued_task(tid, 999, cpu as i32, 0),
            queued_at_ns: 0,
        },
        PendingTask {
            task: queued_task(tid, 100, cpu as i32, 0),
            queued_at_ns: 0,
        },
        PendingTask {
            task: queued_task(tid, 200, cpu as i32, 0),
            queued_at_ns: 0,
        },
    ]);
    let apps = vec![make_app(0, &[]), make_app(1, &[cpu])];
    let mut pid_to_app = BTreeMap::new();
    let root_to_app = BTreeMap::from([(100, 0), (200, 1)]);
    let mut cpu_states = vec![CpuStateValue::default(); cpu_count];
    cpu_states[cpu as usize] = CpuStateValue {
        domain_id: 0,
        idle: 1,
        cpu_dsq_depth: 0,
        current_tid: 0,
        last_update_ns: 0,
    };
    // Verifies NSDI pending selection skip behavior.
    // Expected: unresolved app ids and apps with an empty owned-CPU set are
    // skipped so a later task with legal ownership can still dispatch.
    let selection = select_next_pending(
        &pending,
        &BTreeMap::new(),
        &topo,
        &vec![None::<LlcStateValue>; 1],
        &cpu_states,
        &vec![0; cpu_count],
        &vec![PlannedCpuState::default(); cpu_count],
        &mut affinity,
        &apps,
        &mut pid_to_app,
        &root_to_app,
        now_ns,
        policy_cfg(),
    )
    .unwrap()
    .unwrap();

    match selection {
        PendingSelection::Runnable { index, decision } => {
            assert_eq!(index, 2);
            assert_eq!(decision.selected_cpu, Some(cpu));
        }
        PendingSelection::Drop { .. } => panic!("expected later runnable task"),
    }
}
