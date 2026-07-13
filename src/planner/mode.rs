use super::*;

#[derive(Clone, Debug)]
pub(super) struct PlannerScope {
    pub(super) touched_tids: BTreeSet<u32>,
    pub(super) touched_domains: BTreeSet<u32>,
}

#[derive(Clone, Debug)]
pub(super) enum PlanMode {
    Global,
    Incremental(PlannerScope),
}

impl PlanMode {
    pub(super) fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
}

pub(super) fn resolve_plan_mode(snapshot: &PlannerInput, triggers: &[PlannerTrigger]) -> PlanMode {
    if snapshot.previous_plan.is_none()
        || triggers
            .iter()
            .any(|trigger| matches!(trigger, PlannerTrigger::SweepComplete(_)))
    {
        return PlanMode::Global;
    }

    let mut touched_tids = BTreeSet::<u32>::new();
    let mut touched_domains = BTreeSet::<u32>::new();
    for trigger in triggers {
        match *trigger {
            PlannerTrigger::RunnableDelta(tid) | PlannerTrigger::SignatureChange(tid) => {
                touched_tids.insert(tid);
            }
            PlannerTrigger::DomainStateFlip(domain) | PlannerTrigger::TickPressure(domain) => {
                touched_domains.insert(domain);
            }
            PlannerTrigger::SweepComplete(_) => {}
        }
    }

    PlanMode::Incremental(PlannerScope {
        touched_tids,
        touched_domains,
    })
}

pub(super) fn needs_global_rebuild(
    state: &PlannerState,
    config: &PlannerConfig,
    mode: &PlanMode,
) -> bool {
    !mode.is_global()
        && (state.items.len() > config.incremental_item_limit
            || affected_domain_count(state) > config.incremental_domain_limit)
}
