use crate::types::{CpuStateValue, PlannedCpuState, TopologyLayout};
use std::collections::BTreeSet;

pub(crate) fn planned_jobs_len(planned_cpu_states: &[PlannedCpuState], cpu: u32) -> usize {
    planned_cpu_states
        .get(cpu as usize)
        .map(|state| state.jobs.len())
        .unwrap_or(0)
}

pub(crate) fn planned_cpu_available_for_task(
    planned_cpu_states: &[PlannedCpuState],
    cpu: u32,
    task_tid: u32,
    allowed_cpus: &BTreeSet<u32>,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    cpu_high_util_x100: u32,
) -> bool {
    let Some(state) = planned_cpu_states.get(cpu as usize) else {
        return true;
    };
    if state.jobs.is_empty() || state.jobs.iter().all(|tid| *tid == task_tid) {
        return true;
    }

    let has_idle_alternative = allowed_cpus.iter().copied().any(|candidate| {
        candidate != cpu
            && effectively_idle_cpu(candidate, cpu_states, cpu_utils_x100, cpu_high_util_x100)
    });
    if has_idle_alternative {
        return false;
    }

    state.jobs.len() > 1
}

pub(crate) fn current_cpu_available_for_task(
    planned_cpu_states: &[PlannedCpuState],
    cpu: u32,
    task_tid: u32,
    allowed_cpus: &BTreeSet<u32>,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    cpu_high_util_x100: u32,
) -> bool {
    let Some(state) = planned_cpu_states.get(cpu as usize) else {
        return true;
    };
    // Current-CPU reuse treats multi-job entries that do not include this task
    // as aggregate load, not an exclusive target reservation.
    if state.jobs.len() > 1 && state.jobs.iter().all(|tid| *tid != task_tid) {
        return true;
    }
    planned_cpu_available_for_task(
        planned_cpu_states,
        cpu,
        task_tid,
        allowed_cpus,
        cpu_states,
        cpu_utils_x100,
        cpu_high_util_x100,
    )
}

pub(crate) fn choose_cpu_in_domain(
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cpu_high_util_x100: u32,
    task_tid: u32,
) -> Option<u32> {
    let domain = topo.domains.get(domain_id as usize)?;
    domain
        .cpus
        .iter()
        .copied()
        .filter(|cpu| allowed_cpus.contains(cpu))
        .min_by_key(|cpu| {
            let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
            let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
            let planned_jobs = planned_jobs_len(planned_cpu_states, *cpu);
            let effectively_idle =
                state.idle != 0 && state.cpu_dsq_depth == 0 && util < cpu_high_util_x100;
            let occupied_by_other = state.current_tid != 0 && state.current_tid != task_tid;
            (
                !effectively_idle,
                occupied_by_other,
                planned_jobs,
                util,
                state.cpu_dsq_depth,
                *cpu,
            )
        })
}

#[cfg_attr(not(feature = "scheduler-arcas"), allow(dead_code))]
pub(crate) fn preferred_cpu_in_domain(
    preferred_cpu: i32,
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus: &BTreeSet<u32>,
    cpu_high_util_x100: u32,
    task_tid: u32,
) -> Option<u32> {
    let cpu = u32::try_from(preferred_cpu).ok()?;
    if !allowed_cpus.contains(&cpu) {
        return None;
    }
    if topo.cpu_to_domain.get(cpu as usize).copied().flatten() != Some(domain_id) {
        return None;
    }

    let util = cpu_utils_x100.get(cpu as usize).copied().unwrap_or(0);
    if util >= cpu_high_util_x100 {
        return None;
    }
    let state = cpu_states.get(cpu as usize).copied().unwrap_or_default();
    if state.current_tid != 0 && state.current_tid != task_tid {
        return None;
    }

    Some(cpu)
}

pub(crate) fn current_cpu_in_domain(
    current_cpu: i32,
    domain_id: u32,
    topo: &TopologyLayout,
    allowed_cpus: &BTreeSet<u32>,
) -> Option<u32> {
    let cpu = u32::try_from(current_cpu).ok()?;
    if !allowed_cpus.contains(&cpu) {
        return None;
    }
    if topo.cpu_to_domain.get(cpu as usize).copied().flatten() != Some(domain_id) {
        return None;
    }
    Some(cpu)
}

pub(crate) fn current_idle_cpu_in_domain(
    current_cpu: i32,
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    allowed_cpus: &BTreeSet<u32>,
    cpu_high_util_x100: u32,
) -> Option<u32> {
    let cpu = current_cpu_in_domain(current_cpu, domain_id, topo, allowed_cpus)?;
    if effectively_idle_cpu(cpu, cpu_states, cpu_utils_x100, cpu_high_util_x100) {
        Some(cpu)
    } else {
        None
    }
}

pub(crate) fn current_acceptable_cpu_in_domain(
    current_cpu: i32,
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    allowed_cpus: &BTreeSet<u32>,
    task_tid: u32,
) -> Option<u32> {
    let cpu = current_cpu_in_domain(current_cpu, domain_id, topo, allowed_cpus)?;
    let state = cpu_states.get(cpu as usize).copied().unwrap_or_default();
    if state.current_tid != 0 && state.current_tid != task_tid {
        None
    } else {
        Some(cpu)
    }
}

pub(crate) fn choose_idle_cpu_in_domain(
    domain_id: u32,
    topo: &TopologyLayout,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    planned_cpu_states: &[PlannedCpuState],
    allowed_cpus: &BTreeSet<u32>,
    cpu_high_util_x100: u32,
) -> Option<u32> {
    let domain = topo.domains.get(domain_id as usize)?;
    domain
        .cpus
        .iter()
        .copied()
        .filter(|cpu| allowed_cpus.contains(cpu))
        .filter(|cpu| effectively_idle_cpu(*cpu, cpu_states, cpu_utils_x100, cpu_high_util_x100))
        .min_by_key(|cpu| {
            let state = cpu_states.get(*cpu as usize).copied().unwrap_or_default();
            let util = cpu_utils_x100.get(*cpu as usize).copied().unwrap_or(0);
            let planned_jobs = planned_jobs_len(planned_cpu_states, *cpu);
            (planned_jobs, util, state.cpu_dsq_depth, *cpu)
        })
}

pub(crate) fn should_rebalance_to_idle_sibling(
    current_cpu: Option<u32>,
    task_tid: u32,
    cpu_states: &[CpuStateValue],
    candidate_cpu: u32,
) -> bool {
    let Some(current_cpu) = current_cpu else {
        return false;
    };
    if current_cpu == candidate_cpu {
        return false;
    }

    let current_state = cpu_states
        .get(current_cpu as usize)
        .copied()
        .unwrap_or_default();
    current_state.current_tid != 0 && current_state.current_tid != task_tid
}

pub(crate) fn effectively_idle_cpu(
    cpu: u32,
    cpu_states: &[CpuStateValue],
    cpu_utils_x100: &[u32],
    cpu_high_util_x100: u32,
) -> bool {
    let state = cpu_states.get(cpu as usize).copied().unwrap_or_default();
    let util = cpu_utils_x100.get(cpu as usize).copied().unwrap_or(0);
    state.idle != 0 && state.cpu_dsq_depth == 0 && util < cpu_high_util_x100
}
