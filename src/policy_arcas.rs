use crate::bpf::QueuedTask;
use crate::policy::{
    choose_cpu_in_domain, choose_idle_cpu_in_domain, classify_task, current_cpu_available_for_task,
    current_cpu_in_domain, effectively_idle_cpu, preferred_cpu_in_domain,
    should_rebalance_to_idle_sibling, task_signature_effect, PlacementDebugInfo, PlacementDecision,
};
use crate::types::{
    CpuStateValue, DecisionReason, LlcStateValue, ManagedThreadState, MappingInfo, PlannedCpuState,
    PlannedDomainState, PolicyConfig, QueueTrigger, TickDecision, TopologyLayout, DEFAULT_SLICE_US,
};
use std::collections::BTreeSet;

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

fn source_metrics(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    mapping: &MappingInfo,
    planned_states: &[PlannedDomainState],
) -> (u32, u32, u32, u32, bool) {
    let current_domain = u32::try_from(task.current_domain).ok();
    let source_capacity_mib_s_x100 = current_domain
        .map(|domain| mapping.df_capacity_mib_s_x100(domain))
        .unwrap_or_else(|| mapping.max_df_capacity_mib_s_x100());
    let source_effect = task_signature_effect(task, meta, source_capacity_mib_s_x100);
    let current_plan = current_domain.and_then(|domain| planned_states.get(domain as usize));
    let current_source_df_x100 = current_plan
        .filter(|state| state.contributor_count > 0)
        .map(|state| state.df_pressure_x100)
        .or_else(|| {
            source_effect
                .signature_valid
                .then_some(source_effect.df_x100)
        })
        .unwrap_or(0);
    let current_source_llc_x100 = current_plan
        .filter(|state| state.contributor_count > 0)
        .map(|state| state.llc_pressure_x100)
        .unwrap_or(source_effect.llc_x100);
    let source_task_count = current_plan.map(|state| state.task_count).unwrap_or(0);
    let estimated_source_llc_effect_x100 = if source_task_count == 0 {
        source_effect.llc_x100
    } else {
        source_effect
            .llc_x100
            .min(current_source_llc_x100.div_ceil(source_task_count))
    };
    (
        current_source_df_x100,
        current_source_llc_x100,
        source_effect.df_x100,
        estimated_source_llc_effect_x100,
        source_effect.signature_valid,
    )
}

fn choose_stay_cpu(
    task: &QueuedTask,
    domain: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
) -> Option<u32> {
    let current_cpu = u32::try_from(task.current_cpu).ok();
    let task_tid = task.tid as u32;
    let allowed_in_domain = allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| topo.cpu_to_domain.get(*cpu as usize).copied().flatten() == Some(domain))
        .collect::<BTreeSet<_>>();
    let best_idle_cpu = choose_idle_cpu_in_domain(
        domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        allowed_cpus,
        cfg.cpu_high_util_x100,
    );
    let rebalance_idle_cpu = best_idle_cpu.filter(|candidate| {
        should_rebalance_to_idle_sibling(current_cpu, task.tid as u32, cpu_states, *candidate)
    });
    rebalance_idle_cpu
        .or_else(|| {
            current_cpu_in_domain(task.current_cpu, domain, topo, allowed_cpus).filter(|cpu| {
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
            preferred_cpu_in_domain(
                task.current_cpu,
                domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                allowed_cpus,
                cfg.cpu_high_util_x100,
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
                allowed_cpus,
                cfg.cpu_high_util_x100,
                task.tid as u32,
            )
        })
}

fn choose_preferred_target_cpu(
    preferred_cpu: Option<u32>,
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
    task_tid: u32,
) -> Option<u32> {
    let cpu = preferred_cpu_in_domain(
        preferred_cpu
            .and_then(|cpu| i32::try_from(cpu).ok())
            .unwrap_or(-1),
        domain_id,
        topo,
        cpu_states,
        cpu_utils_x100,
        allowed_cpus,
        cfg.cpu_high_util_x100,
        task_tid,
    )?;
    if !effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cfg.cpu_high_util_x100) {
        return None;
    }
    let planned_jobs = planned_cpu_states
        .get(cpu as usize)
        .map(|state| state.jobs.as_slice())
        .unwrap_or(&[]);
    if planned_jobs.is_empty() || planned_jobs.iter().all(|tid| *tid == task_tid) {
        Some(cpu)
    } else {
        None
    }
}

fn choose_cpu_for_domain(
    task: &QueuedTask,
    domain: u32,
    preferred_cpu: Option<u32>,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cfg: PolicyConfig,
) -> Option<u32> {
    choose_preferred_target_cpu(
        preferred_cpu,
        domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        allowed_cpus,
        cfg,
        task.tid as u32,
    )
    .or_else(|| {
        if Some(domain) == u32::try_from(task.current_domain).ok() {
            choose_stay_cpu(
                task,
                domain,
                topo,
                cpu_states,
                cpu_utils_x100,
                planned_cpu_states,
                allowed_cpus,
                cfg,
            )
        } else {
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
        }
    })
}

fn order_candidate_domains(
    active_domains: &[u32],
    target_domain: Option<u32>,
    current_domain: Option<u32>,
) -> Vec<u32> {
    let mut ordered = Vec::new();
    if let Some(target) = target_domain.filter(|domain| active_domains.contains(domain)) {
        ordered.push(target);
    }
    if let Some(current) = current_domain.filter(|domain| active_domains.contains(domain)) {
        if !ordered.contains(&current) {
            ordered.push(current);
        }
    }
    for domain in active_domains {
        if !ordered.contains(domain) {
            ordered.push(*domain);
        }
    }
    ordered
}

fn active_domain_list(mapping: &MappingInfo, active_domains: &[u32]) -> Vec<u32> {
    if !active_domains.is_empty() {
        let eligible = active_domains
            .iter()
            .copied()
            .filter(|domain| mapping.eligible_domains.contains(domain))
            .collect::<Vec<_>>();
        if !eligible.is_empty() {
            return eligible;
        }
    }
    mapping.eligible_domains.iter().copied().collect()
}

fn allowed_eligible_cpus(allowed_cpus: &BTreeSet<u32>, mapping: &MappingInfo) -> BTreeSet<u32> {
    let eligible = allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| mapping.eligible_cpus.contains(cpu))
        .collect::<BTreeSet<_>>();
    if eligible.is_empty() {
        allowed_cpus.clone()
    } else {
        eligible
    }
}

fn first_allowed_cpu_and_domain(
    topo: &TopologyLayout,
    allowed_cpus: &BTreeSet<u32>,
) -> Option<(u32, u32)> {
    allowed_cpus.iter().copied().find_map(|cpu| {
        topo.cpu_to_domain
            .get(cpu as usize)
            .copied()
            .flatten()
            .map(|domain| (cpu, domain))
    })
}

pub fn choose_placement_with_targets(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    planned_states: &[PlannedDomainState],
    llc_states: &[Option<LlcStateValue>],
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    active_domains: &[u32],
    target_domain: Option<u32>,
    target_cpu: Option<u32>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> PlacementDecision {
    let current_domain = u32::try_from(task.current_domain).ok();
    let current_llc =
        current_domain.and_then(|idx| llc_states.get(idx as usize).copied().flatten());
    let class = classify_task(task, current_llc, now_ns, cfg);
    let trigger = QueueTrigger::from_u32(task.trigger);
    let (
        current_source_df_x100,
        current_source_llc_x100,
        source_df_effect_x100,
        source_llc_effect_x100,
        signature_valid,
    ) = source_metrics(task, meta, mapping, planned_states);
    let active_domains = active_domain_list(mapping, active_domains);
    let candidate_domains = order_candidate_domains(&active_domains, target_domain, current_domain);
    let effective_allowed_cpus = allowed_eligible_cpus(allowed_cpus, mapping);

    for domain in candidate_domains {
        let preferred_cpu = (Some(domain) == target_domain)
            .then_some(target_cpu)
            .flatten();
        let cpu = choose_cpu_for_domain(
            task,
            domain,
            preferred_cpu,
            topo,
            cpu_states,
            cpu_utils_x100,
            planned_cpu_states,
            &effective_allowed_cpus,
            cfg,
        );
        let Some(cpu) = cpu else {
            continue;
        };
        let moved = Some(domain) != current_domain;
        let destination_effect =
            task_signature_effect(task, meta, mapping.df_capacity_mib_s_x100(domain));
        let destination_plan = planned_states.get(domain as usize);
        let current_destination_df_x100 = destination_plan
            .map(|state| state.df_pressure_x100)
            .unwrap_or(0);
        let current_destination_llc_x100 = destination_plan
            .map(|state| state.llc_pressure_x100)
            .unwrap_or(0);
        return PlacementDecision {
            class,
            selected_domain: Some(domain),
            selected_cpu: Some(cpu),
            reason: if moved {
                DecisionReason::MoveCoolerDomain
            } else {
                DecisionReason::StayCurrentDomain
            },
            trigger,
            tick_decision: tick_decision_for(trigger, moved),
            signature_valid,
            villain_score: 0,
            defer_dispatch: false,
            score_gain: 0,
            slice_ns: DEFAULT_SLICE_US * 1_000,
            current_source_df_x100,
            predicted_source_df_x100: current_source_df_x100.saturating_sub(source_df_effect_x100),
            current_source_llc_x100,
            predicted_source_llc_x100: current_source_llc_x100
                .saturating_sub(source_llc_effect_x100),
            current_destination_df_x100,
            predicted_destination_df_x100: current_destination_df_x100
                .saturating_add(destination_effect.df_x100),
            current_destination_llc_x100,
            predicted_destination_llc_x100: current_destination_llc_x100
                .saturating_add(destination_effect.llc_x100),
            signature_sample_count: meta.map(|value| value.signature.sample_count).unwrap_or(0),
            signature_confidence_x100: meta
                .map(|value| value.signature.confidence_x100)
                .unwrap_or(0),
            signature_stable: meta.map(|value| value.signature.stable).unwrap_or(false),
            planner_epoch: 0,
            plan_revision: 0,
            plan_used: false,
            fallback_reason: "",
            planned_domain: Some(domain),
            planned_cpu: Some(cpu),
            sync_group_id: None,
            sync_anchor_domain: None,
            sync_override: false,
            debug: PlacementDebugInfo::default(),
        };
    }

    let fallback = first_allowed_cpu_and_domain(topo, &effective_allowed_cpus);
    let stay_domain = current_domain
        .filter(|domain| active_domains.contains(domain))
        .or_else(|| fallback.map(|(_, domain)| domain))
        .unwrap_or_default();
    let stay_cpu = choose_stay_cpu(
        task,
        stay_domain,
        topo,
        cpu_states,
        cpu_utils_x100,
        planned_cpu_states,
        &effective_allowed_cpus,
        cfg,
    )
    .or_else(|| {
        u32::try_from(task.current_cpu)
            .ok()
            .filter(|cpu| effective_allowed_cpus.contains(cpu))
    })
    .or_else(|| fallback.map(|(cpu, _)| cpu))
    .unwrap_or_default();
    PlacementDecision {
        class,
        selected_domain: Some(stay_domain),
        selected_cpu: Some(stay_cpu),
        reason: DecisionReason::NoCandidate,
        trigger,
        tick_decision: tick_decision_for(trigger, false),
        signature_valid,
        villain_score: 0,
        defer_dispatch: false,
        score_gain: 0,
        slice_ns: DEFAULT_SLICE_US * 1_000,
        current_source_df_x100,
        predicted_source_df_x100: current_source_df_x100,
        current_source_llc_x100,
        predicted_source_llc_x100: current_source_llc_x100,
        current_destination_df_x100: current_source_df_x100,
        predicted_destination_df_x100: current_source_df_x100,
        current_destination_llc_x100: current_source_llc_x100,
        predicted_destination_llc_x100: current_source_llc_x100,
        signature_sample_count: meta.map(|value| value.signature.sample_count).unwrap_or(0),
        signature_confidence_x100: meta
            .map(|value| value.signature.confidence_x100)
            .unwrap_or(0),
        signature_stable: meta.map(|value| value.signature.stable).unwrap_or(false),
        planner_epoch: 0,
        plan_revision: 0,
        plan_used: false,
        fallback_reason: "",
        planned_domain: Some(stay_domain),
        planned_cpu: Some(stay_cpu),
        sync_group_id: None,
        sync_anchor_domain: None,
        sync_override: false,
        debug: PlacementDebugInfo::default(),
    }
}
