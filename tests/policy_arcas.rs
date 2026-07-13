#![cfg(feature = "scheduler-arcas")]

use scx_rustland_la::bpf::QueuedTask;
use scx_rustland_la::policy_arcas::choose_placement_with_targets;
use scx_rustland_la::types::{
    pct_to_x100, CpuStateValue, DomainInfo, HotCoolState, LlcStateValue, ManagedThreadState,
    MappingInfo, PlannedCpuState, PlannedDomainState, PolicyConfig, ThreadSignature,
    TopologyLayout,
};
use std::collections::BTreeSet;

fn topo() -> TopologyLayout {
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

fn topo_three_domains() -> TopologyLayout {
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

fn mapping() -> MappingInfo {
    MappingInfo {
        domain_to_ccx: vec![0, 1],
        domain_to_ccm: vec![Some(0), Some(1)],
        domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: BTreeSet::from([0, 1]),
        excluded_domains: BTreeSet::new(),
        eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
    }
}

fn mapping_three_domains() -> MappingInfo {
    MappingInfo {
        domain_to_ccx: vec![0, 1, 2],
        domain_to_ccm: vec![Some(0), Some(1), Some(2)],
        domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000), Some(2_000_000)],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: BTreeSet::from([0, 1, 2]),
        excluded_domains: BTreeSet::new(),
        eligible_cpus: BTreeSet::from([0, 1, 2, 3, 4, 5]),
    }
}

fn mapping_only_domain0() -> MappingInfo {
    MappingInfo {
        domain_to_ccx: vec![0, 1],
        domain_to_ccm: vec![Some(0), Some(1)],
        domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: BTreeSet::from([0]),
        excluded_domains: BTreeSet::from([1]),
        eligible_cpus: BTreeSet::from([0, 1]),
    }
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

fn llc(ewma: u32, state: HotCoolState) -> Option<LlcStateValue> {
    Some(LlcStateValue {
        sample_ts_ns: 1_000_000,
        raw_l3_bw_mib_s_x100: ewma,
        raw_pressure_pct_x100: ewma,
        ewma_pressure_pct_x100: ewma,
        state: state as u32,
        valid: 1,
        ..LlcStateValue::default()
    })
}

fn task(current_cpu: i32, current_domain: i32) -> QueuedTask {
    QueuedTask {
        tid: 1001,
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
        last_l2_bw_mib_s_x100: 900_000,
        ewma_l2_bw_mib_s_x100: 900_000,
        last_fill_bw_mib_s_x100: [0, 400_000, 50_000],
        ewma_fill_bw_mib_s_x100: [0, 400_000, 50_000],
        last_ipc_x1000: 4_000,
        ewma_ipc_x1000: 4_000,
        last_stall_pct_x100: 0,
        ewma_stall_pct_x100: 0,
        trigger: 0,
        tick_seq: 0,
        last_update_ns: 1_000_000,
        comm: [0; 16],
    }
}

fn meta() -> ManagedThreadState {
    let mut state = ManagedThreadState::default();
    state.signature = ThreadSignature {
        valid: true,
        stable: true,
        projected_df_pressure_x100: 450_000,
        projected_llc_pressure_x100: 1_500,
        fill_bw_mib_s_x100: [0, 400_000, 50_000],
        ..ThreadSignature::default()
    };
    state
}

fn cpu_states() -> Vec<CpuStateValue> {
    vec![
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
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_three_domains() -> Vec<CpuStateValue> {
    vec![
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
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 2,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 2,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn planned_states() -> Vec<PlannedDomainState> {
    vec![
        PlannedDomainState {
            df_pressure_x100: 800_000,
            llc_pressure_x100: 1_500,
            contributor_count: 1,
            task_count: 1,
            ..PlannedDomainState::default()
        },
        PlannedDomainState {
            df_pressure_x100: 100_000,
            llc_pressure_x100: 300,
            contributor_count: 1,
            task_count: 1,
            ..PlannedDomainState::default()
        },
    ]
}

fn planned_states_three_domains() -> Vec<PlannedDomainState> {
    vec![
        PlannedDomainState {
            df_pressure_x100: 800_000,
            llc_pressure_x100: 1_500,
            contributor_count: 1,
            task_count: 1,
            ..PlannedDomainState::default()
        },
        PlannedDomainState {
            df_pressure_x100: 600_000,
            llc_pressure_x100: 1_200,
            contributor_count: 1,
            task_count: 1,
            ..PlannedDomainState::default()
        },
        PlannedDomainState {
            df_pressure_x100: 100_000,
            llc_pressure_x100: 300,
            contributor_count: 1,
            task_count: 1,
            ..PlannedDomainState::default()
        },
    ]
}

fn llc_three_domains() -> Vec<Option<LlcStateValue>> {
    vec![
        llc(8_000, HotCoolState::Hot),
        llc(7_000, HotCoolState::Hot),
        llc(1_000, HotCoolState::Cool),
    ]
}

#[test]
fn falls_back_to_next_legal_active_domain_when_target_domain_is_illegal() {
    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states(),
        &[1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &BTreeSet::from([0, 1]),
        &[1, 0],
        Some(1),
        None,
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(
        decision.reason,
        scx_rustland_la::types::DecisionReason::StayCurrentDomain
    );
}

#[test]
fn uses_later_active_domain_when_target_and_current_are_unusable() {
    let allowed = BTreeSet::from([4, 5]);
    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo_three_domains(),
        &mapping_three_domains(),
        &planned_states_three_domains(),
        &llc_three_domains(),
        &cpu_states_three_domains(),
        &[1_000, 1_000, 1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 6],
        &allowed,
        &[1, 0, 2],
        Some(1),
        None,
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(2));
    assert!(decision
        .selected_cpu
        .is_some_and(|cpu| allowed.contains(&cpu)));
    assert_eq!(
        decision.reason,
        scx_rustland_la::types::DecisionReason::MoveCoolerDomain
    );
}

#[test]
fn does_not_place_on_excluded_target_domain() {
    let mapping = mapping_only_domain0();
    let decision = choose_placement_with_targets(
        &task(-1, -1),
        Some(&meta()),
        &topo(),
        &mapping,
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states(),
        &[1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &BTreeSet::from([0, 1, 2, 3]),
        &[0],
        Some(1),
        Some(2),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert!(decision
        .selected_cpu
        .is_some_and(|cpu| mapping.eligible_cpus.contains(&cpu)));
}

#[test]
fn skips_busy_target_cpu_for_idle_sibling() {
    let mut cpu_states = cpu_states();
    cpu_states[2] = CpuStateValue {
        domain_id: 1,
        idle: 0,
        cpu_dsq_depth: 2,
        current_tid: 0,
        last_update_ns: 0,
    };
    cpu_states[3] = CpuStateValue {
        domain_id: 1,
        idle: 1,
        cpu_dsq_depth: 0,
        current_tid: 0,
        last_update_ns: 0,
    };

    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states,
        &[1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &BTreeSet::from([2, 3]),
        &[1, 0],
        Some(1),
        Some(2),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(3));
}

#[test]
fn pinned_thread_never_escapes_single_allowed_cpu() {
    let mut pinned = task(0, 0);
    pinned.nr_cpus_allowed = 1;
    let allowed = BTreeSet::from([0]);

    let decision = choose_placement_with_targets(
        &pinned,
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states(),
        &[9_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &allowed,
        &[1, 0],
        Some(1),
        Some(2),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert!(decision
        .selected_cpu
        .is_some_and(|cpu| allowed.contains(&cpu)));
}

#[test]
fn unpinned_thread_avoids_target_cpu_reserved_for_pinned_peer() {
    let mut unpinned = task(1, 0);
    unpinned.tid = 1002;
    unpinned.nr_cpus_allowed = 2;
    let mut planned_cpu_states = vec![PlannedCpuState::default(); 4];
    planned_cpu_states[1].jobs.push(1001);

    let decision = choose_placement_with_targets(
        &unpinned,
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
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
            CpuStateValue::default(),
            CpuStateValue::default(),
        ],
        &[1_000, 1_000, 1_000, 1_000],
        &planned_cpu_states,
        &BTreeSet::from([0, 1]),
        &[0],
        Some(0),
        Some(1),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
}

#[test]
fn nocandidate_never_returns_cpu_outside_allowed_mask() {
    let allowed = BTreeSet::from([4, 5]);
    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo_three_domains(),
        &mapping_three_domains(),
        &planned_states_three_domains(),
        &llc_three_domains(),
        &cpu_states_three_domains(),
        &[1_000, 1_000, 1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 6],
        &allowed,
        &[1],
        Some(1),
        None,
        10_000_000,
        policy(),
    );

    assert!(decision
        .selected_cpu
        .map(|cpu| allowed.contains(&cpu))
        .unwrap_or(true));
}

#[test]
fn falls_back_to_current_domain_when_no_active_domain_is_legal() {
    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states(),
        &[1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &BTreeSet::from([0, 1]),
        &[1],
        Some(1),
        None,
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(
        decision.reason,
        scx_rustland_la::types::DecisionReason::NoCandidate
    );
}

#[test]
fn honors_rank_slot_target_cpu_when_it_is_legal_and_idle() {
    let decision = choose_placement_with_targets(
        &task(0, 0),
        Some(&meta()),
        &topo(),
        &mapping(),
        &planned_states(),
        &[
            llc(8_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &cpu_states(),
        &[1_000, 1_000, 1_000, 1_000],
        &vec![PlannedCpuState::default(); 4],
        &BTreeSet::from([2, 3]),
        &[1, 0],
        Some(1),
        Some(3),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(3));
    assert_eq!(
        decision.reason,
        scx_rustland_la::types::DecisionReason::MoveCoolerDomain
    );
}
