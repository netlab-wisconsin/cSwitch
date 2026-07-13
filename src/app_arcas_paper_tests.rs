use super::*;
use crate::types::{
    DomainInfo, LlcStateValue, MappingInfo, PlannedCpuState, PlannedDomainState, QueueTrigger,
    COMM_LEN,
};

fn test_topo() -> TopologyLayout {
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

fn test_mapping() -> MappingInfo {
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

fn planned_states() -> Vec<PlannedDomainState> {
    (0..4)
        .map(|_| PlannedDomainState {
            migration_budget: 4,
            ..PlannedDomainState::default()
        })
        .collect()
}

fn idle_cpu_states() -> Vec<CpuStateValue> {
    (0..8)
        .map(|cpu| CpuStateValue {
            domain_id: (cpu / 2) as u32,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        })
        .collect()
}

fn queued_task(
    tid: u32,
    current_cpu: i32,
    current_domain: i32,
    near_fill_mib_s_x100: u32,
) -> QueuedTask {
    let mut task = QueuedTask {
        tid: tid as i32,
        tgid: tid as i32,
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
        last_ipc_x1000: 4_000,
        ewma_ipc_x1000: 4_000,
        last_stall_pct_x100: 0,
        ewma_stall_pct_x100: 0,
        trigger: QueueTrigger::Enqueue as u32,
        tick_seq: 0,
        last_update_ns: 0,
        comm: [0; COMM_LEN],
    };
    task.last_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE] = near_fill_mib_s_x100;
    task
}

fn queued_task_with_comm(
    tid: u32,
    current_cpu: i32,
    current_domain: i32,
    near_fill_mib_s_x100: u32,
    comm: &str,
) -> QueuedTask {
    let mut task = queued_task(tid, current_cpu, current_domain, near_fill_mib_s_x100);
    for (idx, byte) in comm.as_bytes().iter().take(task.comm.len() - 1).enumerate() {
        task.comm[idx] = *byte as i8;
    }
    task
}

fn controller_with_targets(targets: &[(u32, u32, u32)]) -> ArcasControllerState {
    let mut controller = ArcasControllerState {
        spread_domains: 4,
        active_domains: vec![0, 1, 2, 3],
        ..ArcasControllerState::default()
    };
    for &(tid, domain, cpu) in targets {
        controller.target_domain_by_tid.insert(tid, domain);
        controller.target_cpu_by_tid.insert(tid, cpu);
    }
    controller
}

#[test]
fn direct_fill_totals_use_max_of_last_and_ewma_per_source() {
    let mut thread = ManagedThreadState::default();
    thread.last_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE] = 100;
    thread.ewma_fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE] = 250;
    thread.last_fill_bw_mib_s_x100[0] = 900;
    thread.ewma_fill_bw_mib_s_x100[0] = 300;
    thread.last_fill_bw_mib_s_x100[2] = 40;
    thread.ewma_fill_bw_mib_s_x100[2] = 80;

    // Verifies ArCAS controller input accounting.
    // Expected: each memory-source bucket contributes max(last, EWMA), and the
    // near-cache value is reported from the same max(last, EWMA) rule.
    let (near_fill_mib_s_x100, total_fill_mib_s_x100) = direct_thread_fill_totals(&thread);

    assert_eq!(near_fill_mib_s_x100, 250);
    assert_eq!(total_fill_mib_s_x100, 900 + 250 + 80);
}

#[test]
fn active_domain_selection_uses_capacity_fallback_when_even_sample_undercovers() {
    let topo = test_topo();
    let allowed_cpus_by_tid = BTreeMap::from([
        (10, BTreeSet::from([0, 2, 3, 4, 6, 7])),
        (11, BTreeSet::from([0, 2, 3, 4, 6, 7])),
        (12, BTreeSet::from([0, 2, 3, 4, 6, 7])),
    ]);
    let spread_domain_count = 2;
    // Verifies the active-domain picker's capacity fallback.
    // Expected: if even sampling selects too little allowed CPU capacity for
    // the runnable set, ArCAS chooses the highest-capacity eligible domains.
    let selected_domains = select_active_domains(
        &topo,
        &[0, 1, 2, 3],
        &allowed_cpus_by_tid,
        spread_domain_count,
    );

    assert_eq!(selected_domains, vec![1, 3]);
}

#[test]
fn select_next_pending_prioritizes_highest_near_fill_cross_domain_move() {
    let pending = VecDeque::from([queued_task(10, 0, 0, 100), queued_task(11, 0, 0, 900)]);
    let allowed_cpus_by_tid = BTreeMap::from([
        (10, BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])),
        (11, BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])),
    ]);
    let controller = controller_with_targets(&[(10, 1, 2), (11, 2, 4)]);
    let now_ns = 10_000_000;
    // Verifies ArCAS pending selection policy for cross-domain moves.
    // Expected: among runnable cross-domain target moves, the task with the
    // highest near-cache fill is selected before an earlier lower-fill task.
    let selection = select_next_pending(
        &pending,
        &BTreeMap::new(),
        &test_topo(),
        &test_mapping(),
        &planned_states(),
        &vec![PlannedCpuState::default(); 8],
        &vec![None::<LlcStateValue>; 4],
        &idle_cpu_states(),
        &[0; 8],
        &allowed_cpus_by_tid,
        &controller,
        now_ns,
        policy_cfg(),
    )
    .unwrap()
    .unwrap();

    match selection {
        PendingSelection::Runnable { index, decision } => {
            assert_eq!(index, 1);
            assert_eq!(decision.selected_domain, Some(2));
            assert_eq!(decision.selected_cpu, Some(4));
        }
        PendingSelection::Drop { .. } => panic!("expected runnable selection"),
    }
}

#[test]
fn select_next_pending_does_not_let_helper_beat_targeted_cross_domain_work() {
    let pending = VecDeque::from([
        queued_task_with_comm(10, 0, 0, 0, "numactl"),
        queued_task_with_comm(11, 0, 0, 500, "rocksdb:high0"),
    ]);
    let allowed_cpus_by_tid = BTreeMap::from([
        (10, BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])),
        (11, BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])),
    ]);
    let controller = controller_with_targets(&[(11, 2, 4)]);
    let now_ns = 10_000_000;
    // Verifies helper/sidecar ordering at the private selector seam.
    // Expected: normal targeted runnable work remains selectable ahead of a
    // launch helper when this selector is asked to choose policy work.
    let selection = select_next_pending(
        &pending,
        &BTreeMap::new(),
        &test_topo(),
        &test_mapping(),
        &planned_states(),
        &vec![PlannedCpuState::default(); 8],
        &vec![None::<LlcStateValue>; 4],
        &idle_cpu_states(),
        &[0; 8],
        &allowed_cpus_by_tid,
        &controller,
        now_ns,
        policy_cfg(),
    )
    .unwrap()
    .unwrap();

    match selection {
        PendingSelection::Runnable { index, decision } => {
            assert_eq!(index, 1);
            assert_eq!(decision.selected_domain, Some(2));
        }
        PendingSelection::Drop { .. } => panic!("expected runnable selection"),
    }
}
