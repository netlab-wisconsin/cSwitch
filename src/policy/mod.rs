mod candidate;
mod context;
mod cpu_select;
mod decision;
mod pipeline;
mod signature;

pub use decision::{PlacementDebugInfo, PlacementDecision};
pub use pipeline::{choose_placement, choose_placement_with_planned_cpus};
pub use signature::{task_signature_effect, thread_signature_effect, TaskSignatureEffect};

#[cfg(feature = "diagnostics")]
pub use pipeline::build_record;

pub(crate) use candidate::SlotCapacityConstraint;
#[allow(unused_imports)]
#[allow(unused_imports)]
pub(crate) use cpu_select::{
    choose_cpu_in_domain, choose_idle_cpu_in_domain, current_acceptable_cpu_in_domain,
    current_cpu_available_for_task, current_cpu_in_domain, current_idle_cpu_in_domain,
    effectively_idle_cpu, planned_cpu_available_for_task, planned_jobs_len,
    preferred_cpu_in_domain, should_rebalance_to_idle_sibling,
};
pub(crate) use decision::DecisionBuilder;
#[allow(unused_imports)]
#[allow(unused_imports)]
pub(crate) use pipeline::{
    classify_task, domain_allowed_cpu_capacity, domain_signature_balance_penalty,
    domain_task_slot_constraint_with_allowed_cpus, effective_migrate_margin_x100,
    move_pressure_cost, overload_cost, overload_x100_with_capacity, task_slot_constraint,
    usable_llc,
};
pub(crate) use signature::l2_bw_to_pressure_x100;
