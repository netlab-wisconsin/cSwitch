use super::*;
use crate::types::{
    CcmDfStateValue, DomainInfo, HotCoolState, LlcStateValue, ManagedThreadState, MappingInfo,
    PolicyConfig, ThreadSignature, TopologyLayout,
};

fn policy() -> PolicyConfig {
    PolicyConfig {
        l2_need_mib_s_x100: 512 * 100,
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

fn planner_config() -> PlannerConfig {
    PlannerConfig {
        disable_auto_sync_hints: false,
        sync_tgid_overrides: BTreeSet::new(),
        incremental_item_limit: 16,
        incremental_domain_limit: 8,
        max_passes: 16,
        swap_pass_items: 8,
        debounce: Duration::ZERO,
        move_trace_path: None,
        plan_debug_path: None,
    }
}

fn ccd_topology() -> TopologyLayout {
    let domains = vec![
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
            rep_cpu: 7,
            cpus: vec![7, 8],
            l3_size_mb: 32.0,
        },
        DomainInfo {
            domain_id: 2,
            kernel_l3_id: 3,
            rep_cpu: 21,
            cpus: vec![21, 22],
            l3_size_mb: 32.0,
        },
        DomainInfo {
            domain_id: 3,
            kernel_l3_id: 4,
            rep_cpu: 28,
            cpus: vec![28, 29],
            l3_size_mb: 32.0,
        },
    ];
    let mut cpu_to_domain = vec![None; 30];
    for domain in &domains {
        for &cpu in &domain.cpus {
            cpu_to_domain[cpu as usize] = Some(domain.domain_id);
        }
    }
    TopologyLayout {
        nr_cpu_ids: 30,
        domains,
        cpu_to_domain,
    }
}

fn ccd_mapping() -> MappingInfo {
    MappingInfo {
        domain_to_ccx: vec![0, 1, 3, 4],
        domain_to_ccm: vec![Some(0), Some(1), Some(3), Some(4)],
        domain_to_df_capacity_mib_s_x100: vec![
            Some(2_000_000),
            Some(2_000_000),
            Some(2_000_000),
            Some(2_000_000),
        ],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: BTreeSet::from([0, 1, 2, 3]),
        excluded_domains: BTreeSet::new(),
        eligible_cpus: BTreeSet::from([0, 1, 7, 8, 21, 22, 28, 29]),
    }
}

fn llc_state(pressure_pct_x100: u32, state: HotCoolState) -> Option<LlcStateValue> {
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

fn df_state(read_bw_mib_s_x100: u32) -> Option<CcmDfStateValue> {
    Some(CcmDfStateValue {
        sample_ts_ns: 1_000_000,
        raw_read_bw_mib_s_x100: read_bw_mib_s_x100,
        raw_write_bw_mib_s_x100: 0,
        valid: 1,
        ..CcmDfStateValue::default()
    })
}

fn traffic_thread(
    tid: u32,
    tgid: u32,
    current_domain: u32,
    current_cpu: u32,
    df_mib_s_x100: u32,
) -> ManagedThreadState {
    ManagedThreadState {
        tid,
        tgid,
        last_seen_ns: 2_000_000,
        last_observed_domain: Some(current_domain),
        last_selected_domain: Some(current_domain),
        last_observed_cpu: Some(current_cpu),
        last_selected_cpu: Some(current_cpu),
        signature: ThreadSignature {
            valid: true,
            stable: true,
            sample_count: 2,
            confidence_x100: 90_00,
            projected_df_pressure_x100: df_mib_s_x100,
            projected_llc_pressure_x100: 10_00,
            fill_bw_mib_s_x100: [0, df_mib_s_x100, 0],
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    }
}

fn plan_for(
    threads: BTreeMap<u32, ManagedThreadState>,
    allowed_cpus: BTreeMap<u32, BTreeSet<u32>>,
    allowed_domains: BTreeMap<u32, BTreeSet<u32>>,
    df_states: Vec<Option<CcmDfStateValue>>,
    config: PlannerConfig,
) -> PlannerOutput {
    plan_for_with_mapping(
        threads,
        allowed_cpus,
        allowed_domains,
        df_states,
        ccd_mapping(),
        config,
    )
}

fn plan_for_with_mapping(
    threads: BTreeMap<u32, ManagedThreadState>,
    allowed_cpus: BTreeMap<u32, BTreeSet<u32>>,
    allowed_domains: BTreeMap<u32, BTreeSet<u32>>,
    df_states: Vec<Option<CcmDfStateValue>>,
    mapping: MappingInfo,
    config: PlannerConfig,
) -> PlannerOutput {
    let now_ns = 10_000_000;
    let latest_df_sweep_epoch = 11;
    let pending_tids = threads.keys().copied().collect::<BTreeSet<_>>();
    let snapshot = PlannerInput {
        topo: ccd_topology(),
        mapping,
        policy: policy(),
        now_ns,
        latest_df_sweep_epoch,
        pending_tids,
        threads,
        allowed_cpus,
        allowed_domains,
        llc_states: vec![
            llc_state(10_00, HotCoolState::Cool),
            llc_state(10_00, HotCoolState::Cool),
            llc_state(10_00, HotCoolState::Cool),
            llc_state(10_00, HotCoolState::Cool),
        ],
        df_states,
        previous_plan: None,
    };
    let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
    compute_plan(
        &config,
        snapshot,
        &[PlannerTrigger::SweepComplete(latest_df_sweep_epoch)],
        &mut move_trace,
    )
}

#[test]
fn paper_bandwidth_matching_can_refill_relieved_source_when_it_is_best_path() {
    let high_tid = 101;
    let low_tid = 102;
    // Verifies cSwitch §4.4's bandwidth-aware matching at planner granularity.
    // Expected: the highest-demand flow is assigned to the highest residual
    // bandwidth path. Once that simulated move relieves the source, the
    // lower-demand flow may stay there if it is again the best legal path.
    let threads = BTreeMap::from([
        (
            high_tid,
            traffic_thread(high_tid, high_tid, 0, 0, 15_000_00),
        ),
        (low_tid, traffic_thread(low_tid, low_tid, 0, 1, 5_000_00)),
    ]);
    let allowed_cpus = BTreeMap::from([
        (high_tid, BTreeSet::from([0, 1, 7, 8, 21, 22])),
        (low_tid, BTreeSet::from([0, 1, 7, 8, 21, 22])),
    ]);
    let allowed_domains = BTreeMap::from([
        (high_tid, BTreeSet::from([0, 1, 2])),
        (low_tid, BTreeSet::from([0, 1, 2])),
    ]);
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![
            df_state(25_000_00),
            df_state(0),
            df_state(13_000_00),
            df_state(25_000_00),
        ],
        planner_config(),
    );

    assert_eq!(output.plan.entries[&high_tid].target_domain, 1);
    assert_eq!(output.plan.entries[&low_tid].target_domain, 0);
}

#[test]
fn paper_under_subscribed_flow_stays_without_pressure_to_move() {
    let tid = 101;
    // Verifies cSwitch §4.4's uncongested-channel deferral at planner
    // granularity. Expected: both source and destination paths have bandwidth
    // headroom, so the paper-greedy seed should not move the flow only to smooth
    // a signature imbalance.
    let threads = BTreeMap::from([(tid, traffic_thread(tid, tid, 0, 0, 9_000_00))]);
    let allowed_cpus = BTreeMap::from([(tid, BTreeSet::from([0, 1, 7, 8]))]);
    let allowed_domains = BTreeMap::from([(tid, BTreeSet::from([0, 1]))]);
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![
            df_state(18_000_00),
            df_state(1_000_00),
            df_state(0),
            df_state(0),
        ],
        planner_config(),
    );

    assert_eq!(output.plan.entries[&tid].target_domain, 0);
}

#[test]
fn paper_under_subscribed_gate_uses_mapping_df_capacity() {
    let tid = 101;
    let threads = BTreeMap::from([(tid, traffic_thread(tid, tid, 0, 0, 9_000_00))]);
    let allowed_cpus = BTreeMap::from([(tid, BTreeSet::from([0, 1, 7, 8]))]);
    let allowed_domains = BTreeMap::from([(tid, BTreeSet::from([0, 1]))]);
    let mut mapping = ccd_mapping();
    mapping.domain_to_df_capacity_mib_s_x100[0] = Some(15_000_00);
    let output = plan_for_with_mapping(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![
            df_state(18_000_00),
            df_state(1_000_00),
            df_state(0),
            df_state(0),
        ],
        mapping,
        planner_config(),
    );

    assert_eq!(output.plan.entries[&tid].target_domain, 1);
}

#[test]
fn paper_greedy_does_not_force_make_room_move_when_source_is_legal() {
    let under_tid = 101;
    let hot_tid = 201;
    // Verifies that paper-greedy evaluates the source alongside every other
    // legal path instead of hard-excluding it. Expected: the congested flow does
    // not displace the smaller under-subscribed flow just to make room on a
    // single legal CPU slot, and the under-subscribed flow keeps its source.
    let threads = BTreeMap::from([
        (
            under_tid,
            traffic_thread(under_tid, under_tid, 0, 0, 4_000_00),
        ),
        (hot_tid, traffic_thread(hot_tid, hot_tid, 1, 7, 15_000_00)),
    ]);
    let allowed_cpus = BTreeMap::from([
        (under_tid, BTreeSet::from([0, 21])),
        (hot_tid, BTreeSet::from([0, 7])),
    ]);
    let allowed_domains = BTreeMap::from([
        (under_tid, BTreeSet::from([0, 2])),
        (hot_tid, BTreeSet::from([0, 1])),
    ]);
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![
            df_state(4_000_00),
            df_state(25_000_00),
            df_state(0),
            df_state(0),
        ],
        planner_config(),
    );

    assert_eq!(output.plan.entries[&hot_tid].target_domain, 1);
    assert_eq!(output.plan.entries[&under_tid].target_domain, 0);
}

#[test]
fn paper_greedy_does_not_relax_hard_affinity_for_congested_flow() {
    let tid = 101;
    // Verifies that paper-greedy only relaxes soft home/current-domain
    // stickiness. Expected: even with a congested source and cooler domains
    // available in the machine, hard affinity leaves no legal destination, so
    // the planner keeps the flow on its only allowed domain.
    let threads = BTreeMap::from([(tid, traffic_thread(tid, tid, 0, 0, 15_000_00))]);
    let allowed_cpus = BTreeMap::from([(tid, BTreeSet::from([0]))]);
    let allowed_domains = BTreeMap::from([(tid, BTreeSet::from([0]))]);
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![df_state(25_000_00), df_state(0), df_state(0), df_state(0)],
        planner_config(),
    );

    assert_eq!(output.plan.entries[&tid].target_domain, 0);
}

#[test]
fn paper_megacflow_under_capacity_stays_colocated_as_single_unit() {
    let tgid = 9_000;
    let tids = [901u32, 902, 903, 904];
    let allowed_cpus_for_group = BTreeSet::from([0, 1, 7, 8]);
    let allowed_domains_for_group = BTreeSet::from([0, 1]);
    let mut threads = BTreeMap::new();
    let mut allowed_cpus = BTreeMap::new();
    let mut allowed_domains = BTreeMap::new();
    for (tid, cpu) in tids.into_iter().zip([0u32, 1, 0, 1]) {
        threads.insert(tid, traffic_thread(tid, tgid, 0, cpu, 2_000_00));
        allowed_cpus.insert(tid, allowed_cpus_for_group.clone());
        allowed_domains.insert(tid, allowed_domains_for_group.clone());
    }
    let mut config = planner_config();
    config.sync_tgid_overrides = BTreeSet::from([tgid]);
    // Verifies cSwitch §4.5's under-capacity MegaCFlow behavior.
    // Expected: if aggregate demand fits the dominant path capacity, the
    // synchronized group stays colocated as one scheduling unit.
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![df_state(0), df_state(0), df_state(0), df_state(0)],
        config,
    );
    let planned_domains = output
        .plan
        .entries
        .values()
        .map(|entry| entry.target_domain)
        .collect::<BTreeSet<_>>();

    assert_eq!(planned_domains, BTreeSet::from([0]));
}

#[test]
fn paper_megacflow_over_capacity_decomposes_across_less_contended_paths() {
    let tgid = 9_000;
    let tids = [901u32, 902, 903, 904];
    let allowed_cpus_for_group = BTreeSet::from([0, 1, 7, 8]);
    let allowed_domains_for_group = BTreeSet::from([0, 1]);
    let mut threads = BTreeMap::new();
    let mut allowed_cpus = BTreeMap::new();
    let mut allowed_domains = BTreeMap::new();
    for (tid, cpu) in tids.into_iter().zip([0u32, 1, 0, 1]) {
        threads.insert(tid, traffic_thread(tid, tgid, 0, cpu, 7_000_00));
        allowed_cpus.insert(tid, allowed_cpus_for_group.clone());
        allowed_domains.insert(tid, allowed_domains_for_group.clone());
    }
    let mut config = planner_config();
    config.sync_tgid_overrides = BTreeSet::from([tgid]);
    // Verifies cSwitch §4.5's splittable MegaCFlow behavior.
    // Expected: if aggregate demand exceeds one channel's capacity, the group
    // decomposes and packs sub-cFlows across less-contended paths.
    let output = plan_for(
        threads,
        allowed_cpus,
        allowed_domains,
        vec![df_state(0), df_state(0), df_state(0), df_state(0)],
        config,
    );
    let mut domain_counts = BTreeMap::<u32, usize>::new();
    for entry in output.plan.entries.values() {
        *domain_counts.entry(entry.target_domain).or_default() += 1;
    }

    assert_eq!(domain_counts.get(&0).copied().unwrap_or(0), 2);
    assert_eq!(domain_counts.get(&1).copied().unwrap_or(0), 2);
}
