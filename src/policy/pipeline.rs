use crate::bpf::QueuedTask;
use crate::filter::is_stale;
use crate::types::{
    villain_reslice_slice_ns, CcmDfStateValue, CpuStateValue, DecisionReason, LlcStateValue,
    ManagedThreadState, MappingInfo, PlannedCpuState, PlannedDomainState, PolicyConfig,
    QueueTrigger, ThreadClass, TickDecision, TopologyLayout, DEFAULT_SLICE_US,
    SIGNATURE_STABLE_SAMPLES,
};
use std::collections::BTreeSet;

#[cfg(feature = "diagnostics")]
use crate::types::DecisionRecord;

use super::{
    candidate::{
        CandidateEval, CandidateScan, CostBreakdown, SlotCapacityConstraint,
        NON_IDLE_CPU_SELECTION_SOFT_COST,
    },
    cpu_select::{
        choose_cpu_in_domain, choose_idle_cpu_in_domain, current_acceptable_cpu_in_domain,
        current_cpu_available_for_task, current_cpu_in_domain, current_idle_cpu_in_domain,
        effectively_idle_cpu, planned_jobs_len, should_rebalance_to_idle_sibling,
    },
    task_signature_effect, DecisionBuilder, PlacementDebugInfo, PlacementDecision,
};

pub(crate) fn usable_llc(
    state: Option<LlcStateValue>,
    now_ns: u64,
    stale_ms: u64,
) -> Option<LlcStateValue> {
    match state {
        Some(state) if state.valid != 0 && !is_stale(state.sample_ts_ns, now_ns, stale_ms) => {
            Some(state)
        }
        _ => None,
    }
}

fn usable_df(
    state: Option<CcmDfStateValue>,
    now_ns: u64,
    stale_ms: u64,
) -> Option<CcmDfStateValue> {
    match state {
        Some(state) if state.valid != 0 && !is_stale(state.sample_ts_ns, now_ns, stale_ms) => {
            Some(state)
        }
        _ => None,
    }
}

pub(crate) fn classify_task(
    task: &QueuedTask,
    llc_state: Option<LlcStateValue>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> ThreadClass {
    if task.ewma_l2_bw_mib_s_x100 < cfg.l2_need_mib_s_x100 {
        return ThreadClass::Cold;
    }
    let Some(llc_state) = usable_llc(llc_state, now_ns, cfg.llc_stale_ms) else {
        return ThreadClass::LinkNeed;
    };
    if llc_state.state == crate::types::HotCoolState::Hot as u32 {
        ThreadClass::Congested
    } else {
        ThreadClass::LinkNeed
    }
}

const SIGNATURE_BALANCE_DEADBAND_MIB_S_X100: u32 = 250_000;

pub(crate) fn overload_x100_with_capacity(df_x100: u32, capacity_mib_s_x100: u32) -> u32 {
    df_x100.saturating_sub(capacity_mib_s_x100)
}

fn tick_rate_control_fallback(
    trigger: QueueTrigger,
    live_source_df_x100: u32,
    source_capacity_mib_s_x100: u32,
) -> Option<(TickDecision, u64)> {
    if matches!(trigger, QueueTrigger::VillainReslice)
        && overload_x100_with_capacity(live_source_df_x100, source_capacity_mib_s_x100) > 0
    {
        Some((TickDecision::Reslice, villain_reslice_slice_ns()))
    } else {
        None
    }
}

fn live_balance_weight_x100(current_source_live_df_x100: u32, capacity_mib_s_x100: u32) -> u32 {
    let capacity = capacity_mib_s_x100.max(1);
    let capped = current_source_live_df_x100.min(capacity);
    ((u64::from(capped) * 10_000) / u64::from(capacity)).min(u64::from(u32::MAX)) as u32
}

fn domain_signature_penalty_x100(signature_sum_x100: u32) -> u32 {
    signature_sum_x100.saturating_sub(SIGNATURE_BALANCE_DEADBAND_MIB_S_X100)
}

pub(crate) fn domain_signature_balance_penalty(signature_sum_x100: u32) -> u64 {
    let balance = u64::from(domain_signature_penalty_x100(signature_sum_x100));
    balance.saturating_mul(balance)
}

fn cpu_selection_soft_cost(selected_cpu_is_idle: bool) -> u64 {
    if selected_cpu_is_idle {
        0
    } else {
        NON_IDLE_CPU_SELECTION_SOFT_COST
    }
}

fn pair_signature_balance_penalty(
    source_signature_sum_x100: u32,
    destination_signature_sum_x100: u32,
) -> u64 {
    domain_signature_balance_penalty(source_signature_sum_x100).saturating_add(
        domain_signature_balance_penalty(destination_signature_sum_x100),
    )
}

fn predicted_destination_pressure(current_x100: u32, effect_x100: u32) -> u32 {
    current_x100.saturating_add(effect_x100)
}

pub(crate) fn task_slot_constraint(task_count: u32, cpu_capacity: u32) -> SlotCapacityConstraint {
    SlotCapacityConstraint::from_task_count(task_count, cpu_capacity)
}

pub(crate) fn domain_allowed_cpu_capacity(
    topo: &TopologyLayout,
    domain_id: u32,
    allowed_cpus: &BTreeSet<u32>,
) -> u32 {
    topo.domains
        .get(domain_id as usize)
        .map(|domain| {
            domain
                .cpus
                .iter()
                .filter(|cpu| allowed_cpus.contains(cpu))
                .count() as u32
        })
        .unwrap_or(0)
}

pub(crate) fn domain_task_slot_constraint_with_allowed_cpus(
    topo: &TopologyLayout,
    domain_id: u32,
    task_count: u32,
    allowed_cpus: &BTreeSet<u32>,
) -> SlotCapacityConstraint {
    task_slot_constraint(
        task_count,
        domain_allowed_cpu_capacity(topo, domain_id, allowed_cpus),
    )
}

pub(crate) fn overload_cost(total_overload_x100: u32, max_overload_x100: u32) -> u64 {
    let total = u64::from(total_overload_x100);
    let max = u64::from(max_overload_x100);
    total
        .saturating_mul(total)
        .saturating_add(max.saturating_mul(max))
}

pub(crate) fn move_pressure_cost(
    total_overload_x100: u32,
    max_overload_x100: u32,
    pair_penalty: u64,
    current_source_live_df_x100: u32,
    source_capacity_mib_s_x100: u32,
) -> u64 {
    let balance_weight_x100 = u64::from(live_balance_weight_x100(
        current_source_live_df_x100,
        source_capacity_mib_s_x100,
    ));
    overload_cost(total_overload_x100, max_overload_x100)
        .saturating_add(pair_penalty.saturating_mul(balance_weight_x100) / 10_000)
}

fn under_subscribed_soft_only_move_blocked(
    baseline_cost: CostBreakdown,
    candidate_cost: CostBreakdown,
    baseline_total_overload_x100: u32,
    candidate_total_overload_x100: u32,
    cpu_improvement_requires_cross_domain_move: bool,
) -> bool {
    let cpu_selection_improves =
        candidate_cost.cpu_selection_cost < baseline_cost.cpu_selection_cost;
    baseline_total_overload_x100 == 0
        && candidate_total_overload_x100 == 0
        && candidate_cost.hard_constraint() >= baseline_cost.hard_constraint()
        && (!cpu_selection_improves || !cpu_improvement_requires_cross_domain_move)
}

fn overloaded_cpu_only_cross_domain_move_blocked(
    baseline_cost: CostBreakdown,
    candidate_cost: CostBreakdown,
) -> bool {
    let overloaded =
        baseline_cost.total_overload_x100 > 0 || candidate_cost.total_overload_x100 > 0;
    let cpu_selection_improves =
        candidate_cost.cpu_selection_cost < baseline_cost.cpu_selection_cost;
    let hard_constraint_improves =
        candidate_cost.hard_constraint() < baseline_cost.hard_constraint();
    let pressure_improves = candidate_cost.total_overload_x100 < baseline_cost.total_overload_x100
        || candidate_cost.max_overload_x100 < baseline_cost.max_overload_x100
        || candidate_cost.pair_penalty < baseline_cost.pair_penalty
        || candidate_cost.pressure_cost < baseline_cost.pressure_cost;

    overloaded && cpu_selection_improves && !hard_constraint_improves && !pressure_improves
}

fn reverse_hysteresis_can_yield(
    current_source_live_df_x100: u32,
    predicted_source_live_df_x100: u32,
    baseline_total_overload_x100: u32,
    predicted_total_overload_x100: u32,
    source_capacity_mib_s_x100: u32,
) -> bool {
    if current_source_live_df_x100 <= source_capacity_mib_s_x100 {
        return false;
    }
    if predicted_source_live_df_x100 <= source_capacity_mib_s_x100 {
        return true;
    }
    predicted_total_overload_x100 < baseline_total_overload_x100
}

fn reverse_move_blocked(
    meta: Option<&ManagedThreadState>,
    current_domain: Option<u32>,
    candidate_domain: u32,
    now_ns: u64,
) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    if now_ns >= meta.reverse_protect_until_ns {
        return false;
    }
    current_domain == meta.last_migration_to_domain
        && Some(candidate_domain) == meta.last_migration_from_domain
}

fn tick_move_phase_allows(
    task: &QueuedTask,
    current_domain: Option<u32>,
    candidate_domain: u32,
    current_source_live_df_x100: u32,
    source_capacity_mib_s_x100: u32,
    cfg: PolicyConfig,
) -> bool {
    if overload_x100_with_capacity(current_source_live_df_x100, source_capacity_mib_s_x100) > 0 {
        return true;
    }
    if cfg.tick_move_phase_mod <= 1 {
        return true;
    }

    if !matches!(
        QueueTrigger::from_u32(task.trigger),
        QueueTrigger::Tick | QueueTrigger::VillainReslice
    ) {
        return true;
    }

    let source_domain = current_domain.unwrap_or_default();
    let phase = (task.tick_seq as u32)
        .wrapping_add(task.tid as u32)
        .wrapping_add(source_domain.wrapping_mul(3))
        .wrapping_add(candidate_domain.wrapping_mul(5));
    phase % cfg.tick_move_phase_mod == 0
}

pub(crate) fn effective_migrate_margin_x100(
    current_source_live_df_x100: u32,
    margin_x100: u32,
    source_capacity_mib_s_x100: u32,
) -> u32 {
    if margin_x100 == 0 {
        return 0;
    }
    let capacity = source_capacity_mib_s_x100.max(1);
    let remaining = capacity.saturating_sub(current_source_live_df_x100.min(capacity));
    ((u64::from(margin_x100) * u64::from(remaining)) / u64::from(capacity)).min(u64::from(u32::MAX))
        as u32
}

fn placement_decision(
    class: ThreadClass,
    selected_domain: Option<u32>,
    selected_cpu: Option<u32>,
    reason: DecisionReason,
    trigger: QueueTrigger,
    tick_decision: TickDecision,
    signature_valid: bool,
    villain_score: u32,
    defer_dispatch: bool,
    score_gain: u64,
    slice_ns: u64,
    current_source_df_x100: u32,
    predicted_source_df_x100: u32,
    current_source_llc_x100: u32,
    predicted_source_llc_x100: u32,
    current_destination_df_x100: u32,
    current_destination_llc_x100: u32,
    destination_df_effect_x100: u32,
    destination_llc_effect_x100: u32,
    debug: PlacementDebugInfo,
    meta: Option<&ManagedThreadState>,
) -> PlacementDecision {
    let signature_sample_count = meta.map(|meta| meta.signature.sample_count).unwrap_or(0);
    let signature_confidence_x100 = meta.map(|meta| meta.signature.confidence_x100).unwrap_or(0);
    let signature_stable = meta.map(|meta| meta.signature.stable).unwrap_or(false);
    DecisionBuilder::new(class, reason, trigger)
        .target(selected_domain, selected_cpu)
        .tick(tick_decision)
        .signature(
            signature_valid,
            signature_sample_count,
            signature_confidence_x100,
            signature_stable,
        )
        .villain_score(villain_score)
        .defer_dispatch(defer_dispatch)
        .score(score_gain)
        .slice(slice_ns)
        .source(
            current_source_df_x100,
            predicted_source_df_x100,
            current_source_llc_x100,
            predicted_source_llc_x100,
        )
        .destination(
            current_destination_df_x100,
            predicted_destination_pressure(current_destination_df_x100, destination_df_effect_x100),
            current_destination_llc_x100,
            predicted_destination_pressure(
                current_destination_llc_x100,
                destination_llc_effect_x100,
            ),
        )
        .debug(debug)
        .build()
}

const INITIAL_SPREAD_ENQ_LIMIT: u64 = 4;

fn young_task_needs_idle_spread(task: &QueuedTask, meta: Option<&ManagedThreadState>) -> bool {
    if !matches!(
        QueueTrigger::from_u32(task.trigger),
        QueueTrigger::Enqueue | QueueTrigger::Helper
    ) {
        return false;
    }

    if task.enq_cnt > INITIAL_SPREAD_ENQ_LIMIT {
        return false;
    }

    meta.map(|thread| thread.signature.sample_count < SIGNATURE_STABLE_SAMPLES)
        .unwrap_or(true)
}

fn choose_idle_spread_target(
    task: &QueuedTask,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    planned_states: &[PlannedDomainState],
    planned_cpu_states: &[PlannedCpuState],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus: &BTreeSet<u32>,
    cpu_high_util_x100: u32,
) -> Option<(u32, u32)> {
    let allowed_eligible = intersection_with_eligible(allowed_cpus, mapping);
    let candidate_sets = if allowed_eligible.is_empty() {
        vec![allowed_cpus]
    } else {
        vec![&allowed_eligible, allowed_cpus]
    };
    let current_domain = u32::try_from(task.current_domain).ok();
    let current_cpu = u32::try_from(task.current_cpu).ok();
    for candidate_cpus in candidate_sets {
        let mut best: Option<(u32, usize, u32, u32, u32, bool, u32)> = None;
        for &domain in &mapping.eligible_domains {
            let Some(cpu) = choose_cpu_in_domain(
                domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                planned_cpu_states,
                candidate_cpus,
                cpu_high_util_x100,
                task.tid as u32,
            ) else {
                continue;
            };
            if !effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cpu_high_util_x100) {
                continue;
            }

            let state = cpu_states.get(cpu as usize).copied().unwrap_or_default();
            let util = cpu_utils_x100.get(cpu as usize).copied().unwrap_or(0);
            let planned_jobs = planned_jobs_len(planned_cpu_states, cpu);
            let task_count = planned_states
                .get(domain as usize)
                .map(|value| value.task_count)
                .unwrap_or(0);
            let same_cpu = current_cpu == Some(cpu);
            let candidate = (
                task_count,
                planned_jobs,
                util,
                state.cpu_dsq_depth,
                domain,
                same_cpu,
                cpu,
            );
            match best {
                None => best = Some(candidate),
                Some(existing) if candidate < existing => best = Some(candidate),
                _ => {}
            }
        }

        if let Some((_, _, _, _, domain, _, cpu)) = best {
            if current_domain == Some(domain) && current_cpu == Some(cpu) {
                return None;
            }
            return Some((domain, cpu));
        }
    }

    None
}

fn intersection_with_eligible(
    allowed_cpus: &BTreeSet<u32>,
    mapping: &MappingInfo,
) -> BTreeSet<u32> {
    allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| mapping.eligible_cpus.contains(cpu))
        .collect()
}

pub fn choose_placement(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    planned_states: &[PlannedDomainState],
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus: &BTreeSet<u32>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> PlacementDecision {
    choose_placement_with_planned_cpus(
        task,
        meta,
        topo,
        mapping,
        planned_states,
        llc_states,
        df_states,
        cpu_states,
        cpu_utils_x100,
        &[],
        allowed_cpus,
        now_ns,
        cfg,
    )
}

pub fn choose_placement_with_planned_cpus(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    planned_states: &[PlannedDomainState],
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> PlacementDecision {
    let allowed_eligible = intersection_with_eligible(allowed_cpus, mapping);
    let current_domain = u32::try_from(task.current_domain).ok();
    let current_llc =
        current_domain.and_then(|idx| llc_states.get(idx as usize).copied().flatten());
    let current_df = current_domain.and_then(|idx| df_states.get(idx as usize).copied().flatten());
    let class = classify_task(task, current_llc, now_ns, cfg);
    let current_excluded = current_domain
        .map(|domain| mapping.excluded_domains.contains(&domain))
        .unwrap_or(true);
    let source_df_capacity_mib_s_x100 = current_domain
        .map(|domain| mapping.df_capacity_mib_s_x100(domain))
        .unwrap_or_else(|| mapping.max_df_capacity_mib_s_x100());
    let trigger = QueueTrigger::from_u32(task.trigger);
    let source_effect = task_signature_effect(task, meta, source_df_capacity_mib_s_x100);
    let signature_valid = source_effect.signature_valid;
    let live_source_df_x100 = usable_df(current_df, now_ns, cfg.df_stale_ms)
        .map(|value| {
            value
                .raw_read_bw_mib_s_x100
                .saturating_add(value.raw_write_bw_mib_s_x100)
        })
        .unwrap_or(0);
    let live_source_llc_x100 = usable_llc(current_llc, now_ns, cfg.llc_stale_ms)
        .map(|value| value.ewma_pressure_pct_x100)
        .unwrap_or(0);
    let mut debug = PlacementDebugInfo {
        live_source_df_x100,
        observed_candidate_domain: None,
        observed_candidate_cpu: None,
        ..PlacementDebugInfo::default()
    };
    let current_plan = current_domain.and_then(|domain| planned_states.get(domain as usize));
    let current_source_signature_df_x100 = current_plan
        .filter(|state| state.contributor_count > 0)
        .map(|state| state.df_pressure_x100)
        .or_else(|| signature_valid.then_some(source_effect.df_x100))
        .unwrap_or(0);
    let current_source_df_x100 = current_source_signature_df_x100;
    let current_source_llc_x100 = if current_plan
        .map(|state| state.contributor_count > 0)
        .unwrap_or(false)
    {
        current_plan
            .map(|state| state.llc_pressure_x100)
            .unwrap_or(0)
    } else {
        live_source_llc_x100
    };
    let source_task_count = current_plan.map(|state| state.task_count).unwrap_or(0);
    let source_budget_exhausted = !current_excluded
        && current_plan
            .map(|state| state.outgoing_migrations >= state.migration_budget)
            .unwrap_or(false);
    let estimated_source_df_effect_x100 = source_effect.df_x100;
    let estimated_source_llc_effect_x100 = if current_excluded || source_task_count == 0 {
        source_effect.llc_x100
    } else {
        source_effect
            .llc_x100
            .min(current_source_llc_x100.div_ceil(source_task_count))
    };
    let stay_tick_decision = if matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice)
    {
        TickDecision::Stay
    } else {
        TickDecision::None
    };
    let move_tick_decision = if matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice)
    {
        TickDecision::Move
    } else {
        TickDecision::None
    };

    let stay_cpu = if let Some(domain) = current_domain {
        let current_cpu_u32 = u32::try_from(task.current_cpu).ok();
        let task_tid = task.tid as u32;
        let allowed_eligible_in_domain = allowed_eligible
            .iter()
            .copied()
            .filter(|cpu| topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(domain))
            .collect::<BTreeSet<_>>();
        let allowed_in_domain = allowed_cpus
            .iter()
            .copied()
            .filter(|cpu| topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(domain))
            .collect::<BTreeSet<_>>();
        let tick_current_cpu = if !current_excluded
            && matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice)
        {
            current_cpu_in_domain(task.current_cpu, domain, topo, &allowed_eligible)
                .or_else(|| current_cpu_in_domain(task.current_cpu, domain, topo, allowed_cpus))
        } else {
            None
        };
        let current_idle_cpu = current_idle_cpu_in_domain(
            task.current_cpu,
            domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            &allowed_eligible,
            cfg.cpu_high_util_x100,
        )
        .filter(|cpu| {
            current_cpu_available_for_task(
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
                domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                allowed_cpus,
                cfg.cpu_high_util_x100,
            )
            .filter(|cpu| {
                current_cpu_available_for_task(
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
            domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            &allowed_eligible,
            cfg.cpu_high_util_x100,
        )
        .or_else(|| {
            choose_idle_cpu_in_domain(
                domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                planned_cpu_states,
                allowed_cpus,
                cfg.cpu_high_util_x100,
            )
        });
        let rebalance_idle_cpu = best_idle_cpu.filter(|candidate| {
            should_rebalance_to_idle_sibling(
                current_cpu_u32,
                task.tid as u32,
                cpu_states,
                *candidate,
            )
        });
        tick_current_cpu
            .or(current_idle_cpu)
            .or(rebalance_idle_cpu)
            .or_else(|| {
                current_acceptable_cpu_in_domain(
                    task.current_cpu,
                    domain,
                    topo,
                    cpu_states,
                    &allowed_eligible,
                    task.tid as u32,
                )
                .filter(|cpu| {
                    current_cpu_available_for_task(
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
                    domain,
                    topo,
                    cpu_states,
                    allowed_cpus,
                    task.tid as u32,
                )
                .filter(|cpu| {
                    current_cpu_available_for_task(
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
                    domain,
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
                    domain,
                    topo,
                    cpu_states,
                    cpu_utils_x100,
                    planned_cpu_states,
                    allowed_cpus,
                    cfg.cpu_high_util_x100,
                    task.tid as u32,
                )
            })
    } else {
        None
    };
    let stay_selected_cpu = stay_cpu.or_else(|| u32::try_from(task.current_cpu).ok());
    let stay_selected_cpu_idle = stay_selected_cpu
        .map(|cpu| effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100))
        .unwrap_or(false);
    let stay_selected_domain = current_domain.or_else(|| u32::try_from(task.current_domain).ok());
    let current_domain_has_idle_cpu = current_domain
        .and_then(|domain| topo.domains.get(domain as usize))
        .map(|domain| {
            domain.cpus.iter().copied().any(|cpu| {
                allowed_cpus.contains(&cpu)
                    && effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100)
            })
        })
        .unwrap_or(false);
    let current_cpu_unavailable_for_task = current_domain
        .map(|domain| {
            current_acceptable_cpu_in_domain(
                task.current_cpu,
                domain,
                topo,
                cpu_states,
                allowed_cpus,
                task.tid as u32,
            )
            .is_none()
        })
        .unwrap_or(false);
    let current_domain_has_planned_cpu_pressure = current_domain
        .and_then(|domain| topo.domains.get(domain as usize))
        .map(|domain| {
            domain.cpus.iter().copied().any(|cpu| {
                allowed_cpus.contains(&cpu)
                    && planned_cpu_states
                        .get(cpu as usize)
                        .map(|state| state.jobs.iter().any(|tid| *tid != task.tid as u32))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let cpu_improvement_requires_cross_domain_move = !current_domain_has_idle_cpu
        && (current_cpu_unavailable_for_task || current_domain_has_planned_cpu_pressure);

    if young_task_needs_idle_spread(task, meta) {
        if let Some((spread_domain, spread_cpu)) = choose_idle_spread_target(
            task,
            topo,
            mapping,
            planned_states,
            planned_cpu_states,
            cpu_states,
            cpu_utils_x100,
            allowed_cpus,
            cfg.cpu_high_util_x100,
        ) {
            return placement_decision(
                class,
                Some(spread_domain),
                Some(spread_cpu),
                DecisionReason::IdleSpread,
                trigger,
                if matches!(trigger, QueueTrigger::Tick | QueueTrigger::VillainReslice) {
                    TickDecision::Move
                } else {
                    TickDecision::None
                },
                signature_valid,
                0,
                false,
                0,
                DEFAULT_SLICE_US * 1_000,
                current_source_df_x100,
                current_source_df_x100,
                current_source_llc_x100,
                current_source_llc_x100,
                current_source_df_x100,
                current_source_llc_x100,
                0,
                0,
                debug.clone(),
                meta,
            );
        }
    }

    if !current_excluded && now_ns < meta.map(|value| value.settle_until_ns).unwrap_or(0) {
        return placement_decision(
            class,
            stay_selected_domain,
            stay_selected_cpu,
            DecisionReason::StayCurrentDomain,
            trigger,
            stay_tick_decision,
            signature_valid,
            0,
            false,
            0,
            DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            current_source_df_x100,
            current_source_llc_x100,
            current_source_llc_x100,
            current_source_df_x100,
            current_source_llc_x100,
            0,
            0,
            debug.clone(),
            meta,
        );
    }

    if !current_excluded && matches!(class, ThreadClass::Cold) {
        return placement_decision(
            class,
            stay_selected_domain,
            stay_selected_cpu,
            DecisionReason::StayCurrentDomain,
            trigger,
            stay_tick_decision,
            signature_valid,
            0,
            false,
            0,
            DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            current_source_df_x100,
            current_source_llc_x100,
            current_source_llc_x100,
            current_source_df_x100,
            current_source_llc_x100,
            0,
            0,
            debug.clone(),
            meta,
        );
    }

    if !current_excluded && (!signature_valid || current_source_signature_df_x100 == 0) {
        return placement_decision(
            class,
            stay_selected_domain,
            stay_selected_cpu,
            DecisionReason::StayCurrentDomain,
            trigger,
            stay_tick_decision,
            signature_valid,
            0,
            false,
            0,
            DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            current_source_df_x100,
            current_source_llc_x100,
            current_source_llc_x100,
            current_source_df_x100,
            current_source_llc_x100,
            0,
            0,
            debug.clone(),
            meta,
        );
    }

    let baseline_source_penalty = if current_excluded {
        0
    } else {
        domain_signature_balance_penalty(current_source_signature_df_x100)
    };
    let predicted_source_df_x100 =
        current_source_signature_df_x100.saturating_sub(estimated_source_df_effect_x100);
    let predicted_source_live_df_x100 =
        live_source_df_x100.saturating_sub(estimated_source_df_effect_x100);
    let predicted_source_llc_x100 =
        current_source_llc_x100.saturating_sub(estimated_source_llc_effect_x100);
    let migrate_margin_x100 = effective_migrate_margin_x100(
        live_source_df_x100,
        cfg.migrate_margin_x100,
        source_df_capacity_mib_s_x100,
    );

    if source_budget_exhausted {
        return placement_decision(
            class,
            stay_selected_domain,
            stay_selected_cpu,
            DecisionReason::MigrationBudget,
            trigger,
            stay_tick_decision,
            signature_valid,
            0,
            false,
            0,
            DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            predicted_source_df_x100,
            current_source_llc_x100,
            predicted_source_llc_x100,
            current_source_df_x100,
            current_source_llc_x100,
            0,
            0,
            debug.clone(),
            meta,
        );
    }

    let mut scan = CandidateScan::default();
    let mut observed_candidate_cost = None::<CostBreakdown>;
    for &domain in &mapping.eligible_domains {
        let Some(df) = usable_df(
            df_states.get(domain as usize).copied().flatten(),
            now_ns,
            cfg.df_stale_ms,
        ) else {
            debug.skipped_missing_df = debug.skipped_missing_df.saturating_add(1);
            continue;
        };
        let Some(llc) = usable_llc(
            llc_states.get(domain as usize).copied().flatten(),
            now_ns,
            cfg.llc_stale_ms,
        ) else {
            debug.skipped_missing_llc = debug.skipped_missing_llc.saturating_add(1);
            continue;
        };
        let Some(cpu) = choose_cpu_in_domain(
            domain,
            topo,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            &allowed_eligible,
            cfg.cpu_high_util_x100,
            task.tid as u32,
        ) else {
            debug.skipped_no_cpu = debug.skipped_no_cpu.saturating_add(1);
            continue;
        };
        if !current_excluded && Some(domain) == current_domain {
            debug.skipped_same_domain = debug.skipped_same_domain.saturating_add(1);
            continue;
        }
        let aggregate = planned_states.get(domain as usize);
        if !current_excluded
            && aggregate
                .map(|state| state.incoming_migrations >= state.migration_budget)
                .unwrap_or(false)
        {
            scan.stats.blocked_by_budget = true;
            debug.skipped_budget = debug.skipped_budget.saturating_add(1);
            continue;
        }
        let current_destination_signature_df_x100 =
            aggregate.map(|state| state.df_pressure_x100).unwrap_or(0);
        let current_destination_task_count = aggregate.map(|state| state.task_count).unwrap_or(0);
        let destination_df_capacity_mib_s_x100 = mapping.df_capacity_mib_s_x100(domain);
        let current_destination_live_df_x100 = df
            .raw_read_bw_mib_s_x100
            .saturating_add(df.raw_write_bw_mib_s_x100);
        let destination_effect =
            task_signature_effect(task, meta, destination_df_capacity_mib_s_x100);
        let destination_df_effect_x100 = destination_effect.df_x100;
        let candidate_cpu_idle =
            effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100);
        let predicted_df_x100 =
            current_destination_signature_df_x100.saturating_add(destination_df_effect_x100);
        let predicted_destination_live_df_x100 =
            current_destination_live_df_x100.saturating_add(destination_df_effect_x100);
        let current_destination_llc_x100 = if aggregate
            .map(|state| state.contributor_count > 0)
            .unwrap_or(false)
        {
            aggregate.map(|state| state.llc_pressure_x100).unwrap_or(0)
        } else {
            llc.ewma_pressure_pct_x100
        };
        let predicted_destination_penalty = domain_signature_balance_penalty(predicted_df_x100);
        let baseline_destination_penalty =
            domain_signature_balance_penalty(current_destination_signature_df_x100);
        let baseline_pair_penalty = if current_excluded {
            baseline_destination_penalty
        } else {
            pair_signature_balance_penalty(
                current_source_signature_df_x100,
                current_destination_signature_df_x100,
            )
        };
        let candidate_pair_penalty = if current_excluded {
            predicted_destination_penalty
        } else {
            pair_signature_balance_penalty(predicted_source_df_x100, predicted_df_x100)
        };
        let baseline_source_overload_x100 =
            overload_x100_with_capacity(live_source_df_x100, source_df_capacity_mib_s_x100);
        let baseline_destination_overload_x100 = overload_x100_with_capacity(
            current_destination_live_df_x100,
            destination_df_capacity_mib_s_x100,
        );
        let candidate_source_overload_x100 = overload_x100_with_capacity(
            predicted_source_live_df_x100,
            source_df_capacity_mib_s_x100,
        );
        let candidate_destination_overload_x100 = overload_x100_with_capacity(
            predicted_destination_live_df_x100,
            destination_df_capacity_mib_s_x100,
        );
        let baseline_total_overload_x100 =
            baseline_source_overload_x100.saturating_add(baseline_destination_overload_x100);
        let candidate_total_overload_x100 =
            candidate_source_overload_x100.saturating_add(candidate_destination_overload_x100);
        let baseline_max_overload_x100 =
            baseline_source_overload_x100.max(baseline_destination_overload_x100);
        let candidate_max_overload_x100 =
            candidate_source_overload_x100.max(candidate_destination_overload_x100);
        let baseline_slot_constraint = current_domain
            .map(|source_domain| {
                domain_task_slot_constraint_with_allowed_cpus(
                    topo,
                    source_domain,
                    source_task_count,
                    allowed_cpus,
                )
            })
            .unwrap_or_default()
            .combine(domain_task_slot_constraint_with_allowed_cpus(
                topo,
                domain,
                current_destination_task_count,
                allowed_cpus,
            ));
        let candidate_slot_constraint = current_domain
            .map(|source_domain| {
                domain_task_slot_constraint_with_allowed_cpus(
                    topo,
                    source_domain,
                    source_task_count.saturating_sub(1),
                    allowed_cpus,
                )
            })
            .unwrap_or_default()
            .combine(domain_task_slot_constraint_with_allowed_cpus(
                topo,
                domain,
                current_destination_task_count.saturating_add(1),
                allowed_cpus,
            ));
        let baseline_cpu_selection_cost = cpu_selection_soft_cost(stay_selected_cpu_idle);
        let candidate_cpu_selection_cost = cpu_selection_soft_cost(candidate_cpu_idle);
        let baseline_cost = CostBreakdown {
            total_overload_x100: baseline_total_overload_x100,
            max_overload_x100: baseline_max_overload_x100,
            pair_penalty: baseline_pair_penalty,
            pressure_cost: move_pressure_cost(
                baseline_total_overload_x100,
                baseline_max_overload_x100,
                baseline_pair_penalty,
                live_source_df_x100,
                if current_excluded {
                    destination_df_capacity_mib_s_x100
                } else {
                    source_df_capacity_mib_s_x100
                },
            ),
            slot_constraint: baseline_slot_constraint,
            cpu_selection_cost: baseline_cpu_selection_cost,
        };
        let candidate_cost = CostBreakdown {
            total_overload_x100: candidate_total_overload_x100,
            max_overload_x100: candidate_max_overload_x100,
            pair_penalty: candidate_pair_penalty,
            pressure_cost: move_pressure_cost(
                candidate_total_overload_x100,
                candidate_max_overload_x100,
                candidate_pair_penalty,
                live_source_df_x100,
                if current_excluded {
                    destination_df_capacity_mib_s_x100
                } else {
                    source_df_capacity_mib_s_x100
                },
            ),
            slot_constraint: candidate_slot_constraint,
            cpu_selection_cost: candidate_cpu_selection_cost,
        };
        let baseline_combined_cost = baseline_cost.total_cost();
        let candidate_combined_cost = candidate_cost.total_cost();
        let candidate = CandidateEval {
            domain,
            cpu,
            destination_df_before_x100: current_destination_signature_df_x100,
            destination_llc_before_x100: current_destination_llc_x100,
            destination_df_delta_x100: destination_df_effect_x100,
            destination_llc_delta_x100: estimated_source_llc_effect_x100,
            cost: candidate_cost,
        };
        debug.considered_candidates = debug.considered_candidates.saturating_add(1);
        if observed_candidate_cost
            .map(|observed| candidate.cost.is_better_than(observed))
            .unwrap_or(true)
        {
            debug.observed_candidate_domain = Some(domain);
            debug.observed_candidate_cpu = Some(cpu);
            debug.observed_destination_live_df_x100 = current_destination_live_df_x100;
            debug.baseline_total_overload_x100 = baseline_total_overload_x100;
            debug.candidate_total_overload_x100 = candidate_total_overload_x100;
            debug.baseline_max_overload_x100 = baseline_max_overload_x100;
            debug.candidate_max_overload_x100 = candidate_max_overload_x100;
            debug.baseline_pair_penalty = baseline_pair_penalty;
            debug.candidate_pair_penalty = candidate_pair_penalty;
            debug.baseline_combined_cost = baseline_combined_cost;
            debug.candidate_combined_cost = candidate_combined_cost;
            observed_candidate_cost = Some(candidate.cost);
        }
        if !current_excluded
            && reverse_move_blocked(meta, current_domain, domain, now_ns)
            && !reverse_hysteresis_can_yield(
                live_source_df_x100,
                predicted_source_live_df_x100,
                baseline_total_overload_x100,
                candidate_total_overload_x100,
                source_df_capacity_mib_s_x100,
            )
        {
            scan.stats.blocked_by_reverse = true;
            debug.blocked_reverse = debug.blocked_reverse.saturating_add(1);
            continue;
        }
        if !current_excluded
            && under_subscribed_soft_only_move_blocked(
                baseline_cost,
                candidate_cost,
                baseline_total_overload_x100,
                candidate_total_overload_x100,
                cpu_improvement_requires_cross_domain_move,
            )
        {
            scan.stats.blocked_by_soft_only_stay = true;
            debug.blocked_destination = debug.blocked_destination.saturating_add(1);
            continue;
        }
        if !current_excluded
            && overloaded_cpu_only_cross_domain_move_blocked(baseline_cost, candidate_cost)
        {
            scan.stats.blocked_by_destination = true;
            debug.blocked_destination = debug.blocked_destination.saturating_add(1);
            continue;
        }
        if !current_excluded && !candidate_cost.is_better_than(baseline_cost) {
            scan.stats.blocked_by_destination = true;
            debug.blocked_destination = debug.blocked_destination.saturating_add(1);
            continue;
        }
        if !current_excluded
            && !tick_move_phase_allows(
                task,
                current_domain,
                domain,
                live_source_df_x100,
                source_df_capacity_mib_s_x100,
                cfg,
            )
        {
            scan.stats.blocked_by_phase = true;
            debug.blocked_phase = debug.blocked_phase.saturating_add(1);
            continue;
        }

        scan.note_candidate(candidate);
    }

    let Some(best) = scan.best else {
        let (tick_decision, slice_ns) =
            tick_rate_control_fallback(trigger, live_source_df_x100, source_df_capacity_mib_s_x100)
                .unwrap_or((stay_tick_decision, DEFAULT_SLICE_US * 1_000));
        return placement_decision(
            class,
            stay_selected_domain,
            stay_selected_cpu,
            if scan.stats.blocked_by_soft_only_stay {
                DecisionReason::StayCurrentDomain
            } else if scan.stats.blocked_by_phase {
                DecisionReason::MovePhaseGate
            } else if scan.stats.blocked_by_reverse {
                DecisionReason::ReverseHysteresis
            } else if scan.stats.blocked_by_destination {
                DecisionReason::DestinationGuard
            } else if scan.stats.blocked_by_budget {
                DecisionReason::MigrationBudget
            } else {
                DecisionReason::NoCandidate
            },
            trigger,
            tick_decision,
            signature_valid,
            0,
            false,
            0,
            slice_ns,
            current_source_df_x100,
            current_source_df_x100,
            current_source_llc_x100,
            current_source_llc_x100,
            0,
            0,
            0,
            0,
            debug.clone(),
            meta,
        );
    };
    let best_cost = best.cost.total_cost();
    let best_total_overload_x100 = best.cost.total_overload_x100;
    let best_max_overload_x100 = best.cost.max_overload_x100;
    let best_penalty = best.cost.pair_penalty;
    let best_domain = best.domain;
    let best_cpu = best.cpu;
    let best_df_x100 = best.destination_df_before_x100;
    let best_llc_x100 = best.destination_llc_before_x100;
    let best_destination_df_capacity_mib_s_x100 = mapping.df_capacity_mib_s_x100(best_domain);
    let best_df_effect_x100 =
        task_signature_effect(task, meta, best_destination_df_capacity_mib_s_x100).df_x100;

    if !current_excluded {
        let best_destination_live_df_x100 = usable_df(
            df_states.get(best_domain as usize).copied().flatten(),
            now_ns,
            cfg.df_stale_ms,
        )
        .map(|value| {
            value
                .raw_read_bw_mib_s_x100
                .saturating_add(value.raw_write_bw_mib_s_x100)
        })
        .unwrap_or(0);
        let baseline_source_overload_x100 =
            overload_x100_with_capacity(live_source_df_x100, source_df_capacity_mib_s_x100);
        let baseline_destination_overload_x100 = overload_x100_with_capacity(
            best_destination_live_df_x100,
            best_destination_df_capacity_mib_s_x100,
        );
        let baseline_total_overload_x100 =
            baseline_source_overload_x100.saturating_add(baseline_destination_overload_x100);
        let baseline_max_overload_x100 =
            baseline_source_overload_x100.max(baseline_destination_overload_x100);
        let baseline_pair_penalty =
            pair_signature_balance_penalty(current_source_signature_df_x100, best_df_x100);
        let baseline_slot_constraint = current_domain
            .map(|source_domain| {
                domain_task_slot_constraint_with_allowed_cpus(
                    topo,
                    source_domain,
                    source_task_count,
                    allowed_cpus,
                )
            })
            .unwrap_or_default()
            .combine(domain_task_slot_constraint_with_allowed_cpus(
                topo,
                best_domain,
                planned_states
                    .get(best_domain as usize)
                    .map(|state| state.task_count)
                    .unwrap_or(0),
                allowed_cpus,
            ));
        let baseline_cost = CostBreakdown {
            total_overload_x100: baseline_total_overload_x100,
            max_overload_x100: baseline_max_overload_x100,
            pair_penalty: baseline_pair_penalty,
            pressure_cost: move_pressure_cost(
                baseline_total_overload_x100,
                baseline_max_overload_x100,
                baseline_pair_penalty,
                live_source_df_x100,
                source_df_capacity_mib_s_x100,
            ),
            slot_constraint: baseline_slot_constraint,
            cpu_selection_cost: cpu_selection_soft_cost(stay_selected_cpu_idle),
        };
        let baseline_combined_cost = baseline_cost.total_cost();
        debug.observed_candidate_domain = Some(best_domain);
        debug.observed_candidate_cpu = Some(best_cpu);
        debug.observed_destination_live_df_x100 = best_destination_live_df_x100;
        debug.baseline_total_overload_x100 = baseline_total_overload_x100;
        debug.candidate_total_overload_x100 = best_total_overload_x100;
        debug.baseline_max_overload_x100 = baseline_max_overload_x100;
        debug.candidate_max_overload_x100 = best_max_overload_x100;
        debug.baseline_pair_penalty = baseline_pair_penalty;
        debug.candidate_pair_penalty = best_penalty;
        debug.baseline_combined_cost = baseline_combined_cost;
        debug.candidate_combined_cost = best_cost;
        if !best.cost.is_better_than(baseline_cost) {
            return placement_decision(
                class,
                stay_selected_domain,
                stay_selected_cpu,
                DecisionReason::StayCurrentDomain,
                trigger,
                stay_tick_decision,
                signature_valid,
                0,
                false,
                0,
                DEFAULT_SLICE_US * 1_000,
                current_source_df_x100,
                predicted_source_df_x100,
                current_source_llc_x100,
                predicted_source_llc_x100,
                best_df_x100,
                best_llc_x100,
                best_df_effect_x100,
                estimated_source_llc_effect_x100,
                debug.clone(),
                meta,
            );
        }
        if !best
            .cost
            .clears_margin_against(baseline_cost, migrate_margin_x100)
        {
            return placement_decision(
                class,
                stay_selected_domain,
                stay_selected_cpu,
                DecisionReason::MarginNotMet,
                trigger,
                stay_tick_decision,
                signature_valid,
                0,
                false,
                0,
                DEFAULT_SLICE_US * 1_000,
                current_source_df_x100,
                predicted_source_df_x100,
                current_source_llc_x100,
                predicted_source_llc_x100,
                best_df_x100,
                best_llc_x100,
                best_df_effect_x100,
                estimated_source_llc_effect_x100,
                debug.clone(),
                meta,
            );
        }

        return placement_decision(
            class,
            Some(best_domain),
            Some(best_cpu),
            DecisionReason::MoveCoolerDomain,
            trigger,
            move_tick_decision,
            signature_valid,
            0,
            false,
            baseline_combined_cost.saturating_sub(best_cost),
            DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            predicted_source_df_x100,
            current_source_llc_x100,
            predicted_source_llc_x100,
            best_df_x100,
            best_llc_x100,
            best_df_effect_x100,
            estimated_source_llc_effect_x100,
            debug.clone(),
            meta,
        );
    }

    placement_decision(
        class,
        Some(best_domain),
        Some(best_cpu),
        DecisionReason::ExcludedDomainEscape,
        trigger,
        move_tick_decision,
        signature_valid,
        0,
        false,
        baseline_source_penalty.saturating_sub(best_penalty),
        DEFAULT_SLICE_US * 1_000,
        current_source_df_x100,
        predicted_source_df_x100,
        current_source_llc_x100,
        predicted_source_llc_x100,
        best_df_x100,
        best_llc_x100,
        source_effect.df_x100,
        estimated_source_llc_effect_x100,
        debug,
        meta,
    )
}

#[cfg(feature = "diagnostics")]
pub fn build_record(
    task: &QueuedTask,
    decision: &PlacementDecision,
    comm: String,
) -> DecisionRecord {
    DecisionRecord {
        ts_ns: crate::types::now_ns(),
        tid: task.tid as u32,
        tgid: task.tgid as u32,
        comm,
        source_cpu: u32::try_from(task.current_cpu).ok(),
        source_domain: u32::try_from(task.current_domain).ok(),
        selected_cpu: decision.selected_cpu,
        selected_domain: decision.selected_domain,
        class: decision.class,
        current_source_df_x100: decision.current_source_df_x100,
        predicted_source_df_x100: decision.predicted_source_df_x100,
        current_source_llc_x100: decision.current_source_llc_x100,
        predicted_source_llc_x100: decision.predicted_source_llc_x100,
        current_destination_df_x100: decision.current_destination_df_x100,
        predicted_destination_df_x100: decision.predicted_destination_df_x100,
        current_destination_llc_x100: decision.current_destination_llc_x100,
        predicted_destination_llc_x100: decision.predicted_destination_llc_x100,
        trigger: decision.trigger,
        tick_seq: task.tick_seq,
        tick_decision: decision.tick_decision,
        fill_total_bw_mib_s_x100: task
            .last_fill_bw_mib_s_x100
            .iter()
            .copied()
            .fold(0u32, |acc, value| acc.saturating_add(value)),
        last_ipc_x1000: task.last_ipc_x1000,
        ewma_ipc_x1000: task.ewma_ipc_x1000,
        last_stall_pct_x100: task.last_stall_pct_x100,
        ewma_stall_pct_x100: task.ewma_stall_pct_x100,
        stall_delta_pct_x100: task
            .last_stall_pct_x100
            .saturating_sub(task.ewma_stall_pct_x100),
        slice_ns: decision.slice_ns,
        villain_score: decision.villain_score,
        signature_valid: decision.signature_valid,
        signature_sample_count: decision.signature_sample_count,
        signature_confidence_x100: decision.signature_confidence_x100,
        signature_stable: decision.signature_stable,
        io_cs_signature_valid: false,
        io_cs_signature_sample_count: 0,
        io_cs_signature_confidence_x100: 0,
        io_cs_signature_stable: false,
        io_cs_top_link: None,
        io_cs_top_contrib_x100: 0,
        io_cs_raw_top_link: None,
        io_cs_raw_top_contrib_x100: 0,
        live_source_df_x100: decision.debug.live_source_df_x100,
        observed_candidate_domain: decision.debug.observed_candidate_domain,
        observed_candidate_cpu: decision.debug.observed_candidate_cpu,
        observed_destination_live_df_x100: decision.debug.observed_destination_live_df_x100,
        baseline_total_overload_x100: decision.debug.baseline_total_overload_x100,
        candidate_total_overload_x100: decision.debug.candidate_total_overload_x100,
        baseline_max_overload_x100: decision.debug.baseline_max_overload_x100,
        candidate_max_overload_x100: decision.debug.candidate_max_overload_x100,
        baseline_pair_penalty: decision.debug.baseline_pair_penalty,
        candidate_pair_penalty: decision.debug.candidate_pair_penalty,
        baseline_combined_cost: decision.debug.baseline_combined_cost,
        candidate_combined_cost: decision.debug.candidate_combined_cost,
        considered_candidates: decision.debug.considered_candidates,
        skipped_missing_df: decision.debug.skipped_missing_df,
        skipped_missing_llc: decision.debug.skipped_missing_llc,
        skipped_no_cpu: decision.debug.skipped_no_cpu,
        skipped_same_domain: decision.debug.skipped_same_domain,
        skipped_budget: decision.debug.skipped_budget,
        blocked_destination: decision.debug.blocked_destination,
        blocked_phase: decision.debug.blocked_phase,
        blocked_reverse: decision.debug.blocked_reverse,
        planner_epoch: decision.planner_epoch,
        plan_revision: decision.plan_revision,
        plan_used: decision.plan_used,
        fallback_reason: decision.fallback_reason.to_string(),
        planned_domain: decision.planned_domain,
        planned_cpu: decision.planned_cpu,
        sync_group_id: decision.sync_group_id,
        sync_anchor_domain: decision.sync_anchor_domain,
        sync_override: decision.sync_override,
        control_plane_cpu_policy: "mixed".to_string(),
        control_plane_cpu: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        select_cpu_in_domain_debug_enabled: false,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        selected_cpu_planned_jobs: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        selected_cpu_idle: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        selected_cpu_dsq_depth: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        selected_cpu_util_x100: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        selected_cpu_current_tid: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        best_idle_cpu_in_domain: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        best_idle_cpu_planned_jobs: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        min_planned_jobs_in_domain: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        target_cpu_prepass: None,
        #[cfg(feature = "select-cpu-in-domain-debug")]
        reserved_cpu_override_used: false,
        reason: decision.reason,
    }
}

#[cfg(test)]
mod tests {
    use super::task_slot_constraint;
    use crate::policy::SlotCapacityConstraint;

    #[test]
    fn task_slot_constraint_is_neutral_at_exact_capacity() {
        assert_eq!(
            task_slot_constraint(0, 0),
            SlotCapacityConstraint::WithinCapacity
        );
        assert_eq!(
            task_slot_constraint(1, 0),
            SlotCapacityConstraint::OverCapacity {
                extra_tasks: 1,
                domain_count: 1
            }
        );
        assert_eq!(
            task_slot_constraint(1, 2),
            SlotCapacityConstraint::WithinCapacity
        );
        assert_eq!(
            task_slot_constraint(2, 2),
            SlotCapacityConstraint::WithinCapacity
        );
        assert_eq!(
            task_slot_constraint(3, 2),
            SlotCapacityConstraint::OverCapacity {
                extra_tasks: 1,
                domain_count: 1
            }
        );
    }
}
