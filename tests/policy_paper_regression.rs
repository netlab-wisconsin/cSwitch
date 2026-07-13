use scx_rustland_la::bpf::QueuedTask;
use scx_rustland_la::policy::choose_placement_with_planned_cpus;
use scx_rustland_la::types::{
    pct_to_x100, CcmDfStateValue, CpuStateValue, DecisionReason, DomainInfo,
    DomainSignatureAggregate, HotCoolState, LlcStateValue, ManagedThreadState, MappingInfo,
    PlannedCpuState, PlannedDomainState, PolicyConfig, QueueTrigger, ThreadSignature, TickDecision,
    TopologyLayout, DEFAULT_SLICE_US,
};
use std::collections::BTreeSet;

fn two_domain_topo() -> TopologyLayout {
    TopologyLayout {
        nr_cpu_ids: 4,
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
        ],
        cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1)],
    }
}

fn three_domain_topo() -> TopologyLayout {
    TopologyLayout {
        nr_cpu_ids: 6,
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
        ],
        cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1), Some(2), Some(2)],
    }
}

fn mapping(domain_count: u32, cpu_count: u32) -> MappingInfo {
    MappingInfo {
        domain_to_ccx: (0..domain_count).collect(),
        domain_to_ccm: (0..domain_count).map(Some).collect(),
        domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000); domain_count as usize],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: (0..domain_count).collect(),
        excluded_domains: BTreeSet::new(),
        eligible_cpus: (0..cpu_count).collect(),
    }
}

fn llc(pressure_pct_x100: u32, state: HotCoolState) -> Option<LlcStateValue> {
    Some(LlcStateValue {
        sample_ts_ns: 1_000_000,
        raw_l3_bw_mib_s_x100: pressure_pct_x100,
        raw_pressure_pct_x100: pressure_pct_x100,
        ewma_pressure_pct_x100: pressure_pct_x100,
        state: state as u32,
        valid: 1,
        ..LlcStateValue::default()
    })
}

fn df(read_bw_mib_s_x100: u32) -> Option<CcmDfStateValue> {
    Some(CcmDfStateValue {
        sample_ts_ns: 1_000_000,
        raw_read_bw_mib_s_x100: read_bw_mib_s_x100,
        raw_write_bw_mib_s_x100: 0,
        raw_pressure_pct_x100: read_bw_mib_s_x100,
        ewma_pressure_pct_x100: read_bw_mib_s_x100,
        state: HotCoolState::Cool as u32,
        valid: 1,
        ..CcmDfStateValue::default()
    })
}

fn task_with_tid(
    tid: u32,
    current_cpu: i32,
    current_domain: i32,
    l2_bw_mib_s_x100: u32,
) -> QueuedTask {
    QueuedTask {
        tid: tid as i32,
        tgid: 1000,
        current_cpu,
        current_domain,
        nr_cpus_allowed: 4,
        flags: 0,
        start_ts: 0,
        stop_ts: 0,
        exec_runtime: 0,
        weight: 100,
        vtime: 0,
        enq_cnt: 6,
        last_l2_bw_mib_s_x100: l2_bw_mib_s_x100,
        ewma_l2_bw_mib_s_x100: l2_bw_mib_s_x100,
        last_fill_bw_mib_s_x100: [0; 3],
        ewma_fill_bw_mib_s_x100: [0; 3],
        last_ipc_x1000: 4_000,
        ewma_ipc_x1000: 4_000,
        last_stall_pct_x100: 0,
        ewma_stall_pct_x100: 0,
        trigger: QueueTrigger::Enqueue as u32,
        tick_seq: 0,
        last_update_ns: 1_000_000,
        comm: [0; 16],
    }
}

fn tick_task(tid: u32, current_cpu: i32, current_domain: i32, l2_bw_mib_s_x100: u32) -> QueuedTask {
    let mut task = task_with_tid(tid, current_cpu, current_domain, l2_bw_mib_s_x100);
    task.trigger = QueueTrigger::Tick as u32;
    task.tick_seq = 7;
    task
}

fn villain_reslice_task(
    tid: u32,
    current_cpu: i32,
    current_domain: i32,
    l2_bw_mib_s_x100: u32,
) -> QueuedTask {
    let mut task = tick_task(tid, current_cpu, current_domain, l2_bw_mib_s_x100);
    task.trigger = QueueTrigger::VillainReslice as u32;
    task
}

fn cpu_states(entries: &[(u32, bool, u32, u32)]) -> Vec<CpuStateValue> {
    entries
        .iter()
        .map(
            |(domain_id, is_idle, dsq_depth, current_tid)| CpuStateValue {
                domain_id: *domain_id,
                idle: u32::from(*is_idle),
                cpu_dsq_depth: *dsq_depth,
                current_tid: *current_tid,
                last_update_ns: 0,
            },
        )
        .collect()
}

fn agg(df_mib_s_x100: u32, llc_pressure_pct_x100: u32) -> DomainSignatureAggregate {
    DomainSignatureAggregate {
        df_pressure_x100: df_mib_s_x100,
        llc_pressure_x100: llc_pressure_pct_x100,
        fill_bw_mib_s_x100: [0; 3],
        contributor_count: 1,
        stable_count: 1,
    }
}

fn planned_states(
    counts: &[u32],
    aggregates: &[DomainSignatureAggregate],
) -> Vec<PlannedDomainState> {
    counts
        .iter()
        .enumerate()
        .map(|(idx, task_count)| {
            let aggregate = aggregates.get(idx).cloned().unwrap_or_default();
            PlannedDomainState {
                df_pressure_x100: aggregate.df_pressure_x100,
                llc_pressure_x100: aggregate.llc_pressure_x100,
                fill_bw_mib_s_x100: aggregate.fill_bw_mib_s_x100,
                contributor_count: aggregate.contributor_count,
                stable_count: aggregate.stable_count,
                task_count: *task_count,
                incoming_migrations: 0,
                outgoing_migrations: 0,
                migration_budget: 4,
            }
        })
        .collect()
}

fn planned_cpu_states(cpu_count: usize) -> Vec<PlannedCpuState> {
    vec![PlannedCpuState::default(); cpu_count]
}

fn stable_meta_with_domain_df(
    domain_df_deltas_mib_s_x100: &[u32],
    llc_pressure_pct_x100: u32,
) -> ManagedThreadState {
    let mut meta = ManagedThreadState {
        signature: ThreadSignature {
            valid: true,
            stable: true,
            projected_df_pressure_x100: domain_df_deltas_mib_s_x100.first().copied().unwrap_or(0),
            projected_llc_pressure_x100: llc_pressure_pct_x100,
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    };
    for (idx, delta_mib_s_x100) in domain_df_deltas_mib_s_x100.iter().copied().enumerate() {
        meta.signature.df_domain_delta_x100[idx] = delta_mib_s_x100;
    }
    meta
}

fn policy() -> PolicyConfig {
    PolicyConfig {
        l2_need_mib_s_x100: 512 * 100,
        migrate_margin_x100: 0,
        cpu_high_util_x100: pct_to_x100(85),
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

#[test]
fn paper_under_subscribed_single_flow_defers_to_current_domain() {
    let topo = two_domain_topo();
    let map = mapping(2, 4);
    let meta = stable_meta_with_domain_df(&[9_000_00, 0], 15_00);
    let task = task_with_tid(1001, 0, 0, 9_000_00);
    let now_ns = 10_000_000;
    // Verifies cSwitch §4.4's uncongested-channel rule for one flow.
    // Expected: both live paths are below capacity, so placement defers to
    // default/current-domain behavior instead of migrating for signature balance.
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &map,
        &planned_states(&[2, 0], &[agg(18_000_00, 30_00), agg(1_000_00, 5_00)]),
        &[
            llc(30_00, HotCoolState::Cool),
            llc(5_00, HotCoolState::Cool),
        ],
        &[df(18_000_00), df(1_000_00)],
        &cpu_states(&[
            (0, false, 0, 1001),
            (0, true, 0, 0),
            (1, true, 0, 0),
            (1, true, 0, 0),
        ]),
        &[10_00, 5_00, 1_00, 1_00],
        &planned_cpu_states(4),
        &BTreeSet::from([0, 1, 2, 3]),
        now_ns,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn paper_under_subscribed_multi_flow_defers_each_flow_to_current_domain() {
    let topo = two_domain_topo();
    let map = mapping(2, 4);
    let planned_domain_states =
        planned_states(&[2, 0], &[agg(18_000_00, 30_00), agg(1_000_00, 5_00)]);
    let llc_states = [
        llc(30_00, HotCoolState::Cool),
        llc(5_00, HotCoolState::Cool),
    ];
    let df_states = [df(18_000_00), df(1_000_00)];
    let cpu_states = cpu_states(&[
        (0, false, 0, 1001),
        (0, false, 0, 1002),
        (1, true, 0, 0),
        (1, true, 0, 0),
    ]);
    let allowed_cpus = BTreeSet::from([0, 1, 2, 3]);
    let meta = stable_meta_with_domain_df(&[9_000_00, 0], 15_00);
    let now_ns = 10_000_000;

    // Verifies the same uncongested-channel rule across multiple runnable
    // flows. Expected: every flow remains on its current domain while the
    // source and destination paths are under-subscribed.
    for (tid, cpu) in [(1001, 0), (1002, 1)] {
        let task = task_with_tid(tid, cpu, 0, 9_000_00);
        let decision = choose_placement_with_planned_cpus(
            &task,
            Some(&meta),
            &topo,
            &map,
            &planned_domain_states,
            &llc_states,
            &df_states,
            &cpu_states,
            &[10_00, 10_00, 1_00, 1_00],
            &planned_cpu_states(4),
            &allowed_cpus,
            now_ns,
            policy(),
        );

        assert_eq!(decision.selected_domain, Some(0));
        assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
    }
}

#[test]
fn paper_congested_egress_without_legal_destination_uses_rate_control() {
    let topo = two_domain_topo();
    let map = mapping(2, 4);
    let meta = stable_meta_with_domain_df(&[9_000_00, 0], 90_00);
    let task = villain_reslice_task(1001, 0, 0, 9_000_00);
    let now_ns = 10_000_000;
    // Verifies cSwitch §4.4's egress-driven rate-control fallback.
    // Expected: if the current egress path is congested but affinity leaves no
    // legal cooler destination, a selected villain flow is resliced/throttled.
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &map,
        &planned_states(&[2, 0], &[agg(25_000_00, 90_00), agg(1_000_00, 5_00)]),
        &[llc(90_00, HotCoolState::Hot), llc(5_00, HotCoolState::Cool)],
        &[df(25_000_00), df(1_000_00)],
        &cpu_states(&[
            (0, false, 0, 1001),
            (0, true, 0, 0),
            (1, true, 0, 0),
            (1, true, 0, 0),
        ]),
        &[10_00, 5_00, 1_00, 1_00],
        &planned_cpu_states(4),
        &BTreeSet::from([0, 1]),
        now_ns,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.tick_decision, TickDecision::Reslice);
    assert!(decision.slice_ns < DEFAULT_SLICE_US * 1_000);
}

#[test]
fn paper_highest_demand_flow_uses_highest_residual_bandwidth_path() {
    let topo = three_domain_topo();
    let map = mapping(3, 6);
    let high_demand_task = task_with_tid(1001, 0, 0, 15_000_00);
    let high_demand_meta = stable_meta_with_domain_df(&[15_000_00, 15_000_00, 15_000_00], 10_00);
    let low_demand_task = task_with_tid(1002, 1, 0, 5_000_00);
    let low_demand_meta = stable_meta_with_domain_df(&[5_000_00, 5_000_00, 5_000_00], 10_00);
    let base_cpu_states = cpu_states(&[
        (0, false, 0, 1001),
        (0, false, 0, 1002),
        (1, true, 0, 0),
        (1, true, 0, 0),
        (2, true, 0, 0),
        (2, true, 0, 0),
    ]);
    let allowed_cpus = BTreeSet::from([0, 1, 2, 3, 4, 5]);
    let llc_states = [
        llc(80_00, HotCoolState::Hot),
        llc(5_00, HotCoolState::Cool),
        llc(5_00, HotCoolState::Cool),
    ];
    let df_states = [df(25_000_00), df(0), df(13_000_00)];
    let now_ns = 10_000_000;

    // Verifies cSwitch §4.4's bandwidth-aware matching rule.
    // Expected: the highest-demand flow gets the highest-residual path first;
    // the lower-demand flow then uses the next best residual path.
    let high_decision = choose_placement_with_planned_cpus(
        &high_demand_task,
        Some(&high_demand_meta),
        &topo,
        &map,
        &planned_states(
            &[2, 0, 0],
            &[agg(20_000_00, 80_00), agg(0, 5_00), agg(13_000_00, 5_00)],
        ),
        &llc_states,
        &df_states,
        &base_cpu_states,
        &[50_00, 50_00, 1_00, 1_00, 1_00, 1_00],
        &planned_cpu_states(6),
        &allowed_cpus,
        now_ns,
        policy(),
    );
    assert_eq!(high_decision.selected_domain, Some(1));

    let low_decision = choose_placement_with_planned_cpus(
        &low_demand_task,
        Some(&low_demand_meta),
        &topo,
        &map,
        &planned_states(
            &[1, 1, 0],
            &[
                agg(5_000_00, 80_00),
                agg(15_000_00, 5_00),
                agg(13_000_00, 5_00),
            ],
        ),
        &llc_states,
        &df_states,
        &base_cpu_states,
        &[50_00, 50_00, 1_00, 1_00, 1_00, 1_00],
        &planned_cpu_states(6),
        &allowed_cpus,
        now_ns,
        policy(),
    );
    assert_eq!(low_decision.selected_domain, Some(2));
}
