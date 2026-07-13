use super::*;

#[derive(Clone, Debug)]
pub(super) struct DomainModel {
    pub(super) df_capacity_mib_s_x100: u32,
    pub(super) live_df_x100: u32,
    pub(super) live_llc_x100: u32,
    pub(super) current_managed_df_x100: u32,
    pub(super) current_managed_llc_x100: u32,
    pub(super) current_item_df_x100: u32,
    pub(super) current_item_llc_x100: u32,
    pub(super) current_active_task_count: u32,
    pub(super) current_item_active_task_count: u32,
    pub(super) cpu_capacity: u32,
    pub(super) migration_budget: u32,
}

#[derive(Clone, Debug)]
pub(super) struct PlannerState {
    pub(super) members: BTreeMap<u32, MemberView>,
    pub(super) items: Vec<PlannerItem>,
    pub(super) item_index_by_tid: BTreeMap<u32, usize>,
    pub(super) sync_constraints: Vec<SyncConstraint>,
    pub(super) domains: Vec<DomainModel>,
    pub(super) assignments: Vec<Assignment>,
}
