#![allow(dead_code)]

use crate::bpf::QueuedTask;
use crate::types::{
    CcmDfStateValue, CpuStateValue, LlcStateValue, ManagedThreadState, MappingInfo,
    PlannedCpuState, PlannedDomainState, PolicyConfig, QueueTrigger, ThreadClass, TickDecision,
    TopologyLayout,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StayTarget {
    pub(crate) domain: Option<u32>,
    pub(crate) cpu: Option<u32>,
    pub(crate) tick_decision: TickDecision,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SourceState {
    pub(crate) live_df_x100: u32,
    pub(crate) live_llc_x100: u32,
    pub(crate) signature_df_x100: u32,
    pub(crate) signature_llc_x100: u32,
    pub(crate) predicted_df_after_move_x100: u32,
    pub(crate) predicted_llc_after_move_x100: u32,
    pub(crate) task_count: u32,
    pub(crate) migration_budget_exhausted: bool,
}

pub(crate) struct PlacementContext<'a> {
    pub(crate) task: &'a QueuedTask,
    pub(crate) meta: Option<&'a ManagedThreadState>,
    pub(crate) topo: &'a TopologyLayout,
    pub(crate) mapping: &'a MappingInfo,
    pub(crate) planned_states: &'a [PlannedDomainState],
    pub(crate) planned_cpu_states: &'a [PlannedCpuState],
    pub(crate) llc_states: &'a [Option<LlcStateValue>],
    pub(crate) df_states: &'a [Option<CcmDfStateValue>],
    pub(crate) cpu_states: &'a [CpuStateValue],
    pub(crate) cpu_utils_x100: &'a [u32],
    pub(crate) allowed_cpus: &'a BTreeSet<u32>,
    pub(crate) allowed_eligible: BTreeSet<u32>,
    pub(crate) now_ns: u64,
    pub(crate) cfg: PolicyConfig,
    pub(crate) current_domain: Option<u32>,
    pub(crate) current_excluded: bool,
    pub(crate) trigger: QueueTrigger,
    pub(crate) class: ThreadClass,
    pub(crate) stay_target: StayTarget,
    pub(crate) source: SourceState,
}
