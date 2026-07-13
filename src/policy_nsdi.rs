use crate::bpf::QueuedTask;
use crate::policy::{
    classify_task, current_cpu_available_for_task, effectively_idle_cpu, PlacementDebugInfo,
    PlacementDecision,
};
use crate::types::{
    CpuStateValue, DecisionReason, LlcStateValue, ManagedThreadState, PlannedCpuState,
    PolicyConfig, QueueTrigger, TickDecision, TopologyLayout, DEFAULT_SLICE_US,
};
use std::collections::BTreeSet;

fn planned_jobs_len(planned_cpu_states: &[PlannedCpuState], cpu: u32) -> usize {
    planned_cpu_states
        .get(cpu as usize)
        .map(|state| state.jobs.len())
        .unwrap_or(0)
}

fn domain_for_cpu(topo: &TopologyLayout, cpu: u32, fallback: i32) -> i32 {
    topo.cpu_to_domain
        .get(cpu as usize)
        .copied()
        .flatten()
        .map(|domain| domain as i32)
        .unwrap_or(fallback)
}

fn current_cpu_usable(
    task: &QueuedTask,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    legal_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
) -> Option<u32> {
    let cpu = u32::try_from(task.current_cpu).ok()?;
    if !legal_cpus.contains(&cpu) {
        return None;
    }
    let util = cpu_utils_x100.get(cpu as usize).copied().unwrap_or(0);
    if util >= cfg.cpu_high_util_x100 {
        return None;
    }
    let state = cpu_states.get(cpu as usize).copied().unwrap_or_default();
    let task_tid = task.tid as u32;
    if !current_cpu_available_for_task(
        planned_cpu_states,
        cpu,
        task_tid,
        legal_cpus,
        cpu_states,
        cpu_utils_x100,
        cfg.cpu_high_util_x100,
    ) {
        return None;
    }
    if state.current_tid == task_tid {
        return Some(cpu);
    }
    if state.current_tid != 0 {
        return None;
    }
    effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100).then_some(cpu)
}

fn choose_idle_owned_cpu(
    legal_cpus: &BTreeSet<u32>,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    cfg: PolicyConfig,
) -> Option<u32> {
    legal_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            effectively_idle_cpu(*cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100)
        })
        .min_by_key(|cpu| {
            let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
            let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
            (
                planned_jobs_len(planned_cpu_states, *cpu),
                util,
                state.cpu_dsq_depth,
                *cpu,
            )
        })
}

fn choose_spread_cpu(
    legal_cpus: &BTreeSet<u32>,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    task_tid: u32,
) -> Option<u32> {
    legal_cpus.iter().copied().min_by_key(|cpu| {
        let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
        let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
        let occupied_by_other = state.current_tid != 0 && state.current_tid != task_tid;
        (
            occupied_by_other,
            planned_jobs_len(planned_cpu_states, *cpu),
            util,
            state.cpu_dsq_depth,
            *cpu,
        )
    })
}

fn tick_decision_for(trigger: QueueTrigger, moved: bool) -> TickDecision {
    if !matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice) {
        return TickDecision::None;
    }
    if moved {
        TickDecision::Move
    } else {
        TickDecision::Stay
    }
}

pub fn choose_placement_with_owned_cpus(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    topo: &TopologyLayout,
    llc_states: &[Option<LlcStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    owned_cpus: &BTreeSet<u32>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> Option<PlacementDecision> {
    let legal_cpus = allowed_cpus
        .intersection(owned_cpus)
        .copied()
        .collect::<BTreeSet<_>>();
    if legal_cpus.is_empty() {
        return None;
    }

    let current_domain = u32::try_from(task.current_domain).ok();
    let current_llc =
        current_domain.and_then(|idx| llc_states.get(idx as usize).copied().flatten());
    let class = classify_task(task, current_llc, now_ns, cfg);
    let selected_cpu = current_cpu_usable(
        task,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        &legal_cpus,
        cfg,
    )
    .or_else(|| {
        choose_idle_owned_cpu(
            &legal_cpus,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            cfg,
        )
    })
    .or_else(|| {
        choose_spread_cpu(
            &legal_cpus,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            task.tid as u32,
        )
    })?;

    let selected_domain = domain_for_cpu(topo, selected_cpu, task.current_domain.max(0));
    let selected_domain_u32 = u32::try_from(selected_domain).ok();
    let trigger = QueueTrigger::from_u32(task.trigger);
    let moved = Some(selected_cpu) != u32::try_from(task.current_cpu).ok()
        || selected_domain_u32 != u32::try_from(task.current_domain).ok();
    let reason = if !moved {
        DecisionReason::StayCurrentDomain
    } else {
        DecisionReason::IdleSpread
    };
    let signature = meta.map(|value| &value.signature);

    Some(PlacementDecision {
        class,
        selected_domain: selected_domain_u32,
        selected_cpu: Some(selected_cpu),
        reason,
        trigger,
        tick_decision: tick_decision_for(trigger, moved),
        signature_valid: signature.map(|value| value.valid).unwrap_or(false),
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
        signature_sample_count: signature.map(|value| value.sample_count).unwrap_or(0),
        signature_confidence_x100: signature.map(|value| value.confidence_x100).unwrap_or(0),
        signature_stable: signature.map(|value| value.stable).unwrap_or(false),
        planner_epoch: 0,
        plan_revision: 0,
        plan_used: false,
        fallback_reason: "",
        planned_domain: selected_domain_u32,
        planned_cpu: Some(selected_cpu),
        sync_group_id: None,
        sync_anchor_domain: None,
        sync_override: false,
        debug: PlacementDebugInfo::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DomainInfo, PolicyConfig, TopologyLayout, COMM_LEN};

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

    fn task(current_cpu: i32) -> QueuedTask {
        QueuedTask {
            tid: 11,
            tgid: 11,
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

    #[test]
    fn selector_respects_owned_and_allowed_cpus() {
        let decision = choose_placement_with_owned_cpus(
            &task(0),
            None,
            &topo(),
            &[],
            &[
                CpuStateValue::default(),
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
            &[1_000, 500, 0, 0],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([0, 1]),
            &BTreeSet::from([1, 2]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_cpu, Some(1));
        assert_eq!(decision.selected_domain, Some(0));
    }

    #[test]
    fn selector_returns_none_without_owned_affinity_intersection() {
        let decision = choose_placement_with_owned_cpus(
            &task(0),
            None,
            &topo(),
            &[],
            &vec![CpuStateValue::default(); 4],
            &[0; 4],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([0, 1]),
            &BTreeSet::from([2, 3]),
            0,
            cfg(),
        );

        assert!(decision.is_none());
    }

    #[test]
    fn selector_keeps_current_cpu_when_usable() {
        let decision = choose_placement_with_owned_cpus(
            &task(1),
            None,
            &topo(),
            &[],
            &[
                CpuStateValue::default(),
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 1,
                    current_tid: 11,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 1,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue::default(),
            ],
            &[100, 200, 0, 0],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([1, 2]),
            &BTreeSet::from([1, 2]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_cpu, Some(1));
        assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
    }

    #[test]
    fn selector_skips_busy_current_cpu_for_idle_owned_sibling() {
        let decision = choose_placement_with_owned_cpus(
            &task(1),
            None,
            &topo(),
            &[],
            &[
                CpuStateValue::default(),
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 2,
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
                CpuStateValue::default(),
            ],
            &[100, 200, 0, 0],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([1, 2]),
            &BTreeSet::from([1, 2]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_cpu, Some(2));
        assert_eq!(decision.selected_domain, Some(1));
    }

    #[test]
    fn selector_prefers_unoccupied_cpu_over_other_thread_in_spread_fallback() {
        let decision = choose_placement_with_owned_cpus(
            &task(-1),
            None,
            &topo(),
            &[],
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 0,
                    current_tid: 99,
                    last_update_ns: 0,
                },
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 1,
                    current_tid: 0,
                    last_update_ns: 0,
                },
                CpuStateValue::default(),
                CpuStateValue::default(),
            ],
            &[100, 500, 0, 0],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([0, 1]),
            &BTreeSet::from([0, 1]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_cpu, Some(1));
    }

    #[test]
    fn selector_honors_single_allowed_cpu_when_owned() {
        let mut pinned = task(0);
        pinned.nr_cpus_allowed = 1;
        let allowed = BTreeSet::from([0]);
        let decision = choose_placement_with_owned_cpus(
            &pinned,
            None,
            &topo(),
            &[],
            &[
                CpuStateValue {
                    domain_id: 0,
                    idle: 0,
                    cpu_dsq_depth: 2,
                    current_tid: 99,
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
                CpuStateValue::default(),
            ],
            &[9_000, 1_000, 1_000, 1_000],
            &vec![PlannedCpuState::default(); 4],
            &allowed,
            &BTreeSet::from([0, 1, 2]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_domain, Some(0));
        assert_eq!(decision.selected_cpu, Some(0));
        assert!(decision
            .selected_cpu
            .is_some_and(|cpu| allowed.contains(&cpu)));
    }

    #[test]
    fn selector_does_not_place_pinned_task_when_owned_set_excludes_pin() {
        let mut pinned = task(0);
        pinned.nr_cpus_allowed = 1;
        let decision = choose_placement_with_owned_cpus(
            &pinned,
            None,
            &topo(),
            &[],
            &vec![CpuStateValue::default(); 4],
            &[0; 4],
            &vec![PlannedCpuState::default(); 4],
            &BTreeSet::from([0]),
            &BTreeSet::from([1, 2]),
            0,
            cfg(),
        );

        assert!(decision.is_none());
    }

    #[test]
    fn selector_avoids_cpu_reserved_for_pinned_peer() {
        let mut unpinned = task(1);
        unpinned.tid = 12;
        unpinned.nr_cpus_allowed = 2;
        let mut planned_cpu_states = vec![PlannedCpuState::default(); 4];
        planned_cpu_states[1].jobs.push(11);

        let decision = choose_placement_with_owned_cpus(
            &unpinned,
            None,
            &topo(),
            &[],
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
            &BTreeSet::from([0, 1]),
            0,
            cfg(),
        )
        .expect("decision");

        assert_eq!(decision.selected_domain, Some(0));
        assert_eq!(decision.selected_cpu, Some(0));
    }
}
