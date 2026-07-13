use crate::types::{DecisionReason, QueueTrigger, ThreadClass, TickDecision};

#[derive(Clone, Debug, Default)]
pub struct PlacementDebugInfo {
    pub live_source_df_x100: u32,
    pub observed_candidate_domain: Option<u32>,
    pub observed_candidate_cpu: Option<u32>,
    pub observed_destination_live_df_x100: u32,
    pub baseline_total_overload_x100: u32,
    pub candidate_total_overload_x100: u32,
    pub baseline_max_overload_x100: u32,
    pub candidate_max_overload_x100: u32,
    pub baseline_pair_penalty: u64,
    pub candidate_pair_penalty: u64,
    pub baseline_combined_cost: u64,
    pub candidate_combined_cost: u64,
    pub considered_candidates: u32,
    pub skipped_missing_df: u32,
    pub skipped_missing_llc: u32,
    pub skipped_no_cpu: u32,
    pub skipped_same_domain: u32,
    pub skipped_budget: u32,
    pub blocked_destination: u32,
    pub blocked_phase: u32,
    pub blocked_reverse: u32,
}

#[derive(Clone, Debug)]
pub struct PlacementDecision {
    pub class: ThreadClass,
    pub selected_domain: Option<u32>,
    pub selected_cpu: Option<u32>,
    pub reason: DecisionReason,
    pub trigger: QueueTrigger,
    pub tick_decision: TickDecision,
    pub signature_valid: bool,
    pub villain_score: u32,
    pub defer_dispatch: bool,
    pub score_gain: u64,
    pub slice_ns: u64,
    pub current_source_df_x100: u32,
    pub predicted_source_df_x100: u32,
    pub current_source_llc_x100: u32,
    pub predicted_source_llc_x100: u32,
    pub current_destination_df_x100: u32,
    pub predicted_destination_df_x100: u32,
    pub current_destination_llc_x100: u32,
    pub predicted_destination_llc_x100: u32,
    pub signature_sample_count: u32,
    pub signature_confidence_x100: u32,
    pub signature_stable: bool,
    pub planner_epoch: u64,
    pub plan_revision: u64,
    pub plan_used: bool,
    pub fallback_reason: &'static str,
    pub planned_domain: Option<u32>,
    pub planned_cpu: Option<u32>,
    pub sync_group_id: Option<u32>,
    pub sync_anchor_domain: Option<u32>,
    pub sync_override: bool,
    pub debug: PlacementDebugInfo,
}

#[derive(Clone, Debug)]
pub(crate) struct DecisionBuilder {
    decision: PlacementDecision,
}

impl DecisionBuilder {
    pub(crate) fn new(class: ThreadClass, reason: DecisionReason, trigger: QueueTrigger) -> Self {
        Self {
            decision: PlacementDecision {
                class,
                selected_domain: None,
                selected_cpu: None,
                reason,
                trigger,
                tick_decision: TickDecision::None,
                signature_valid: false,
                villain_score: 0,
                defer_dispatch: false,
                score_gain: 0,
                slice_ns: 0,
                current_source_df_x100: 0,
                predicted_source_df_x100: 0,
                current_source_llc_x100: 0,
                predicted_source_llc_x100: 0,
                current_destination_df_x100: 0,
                predicted_destination_df_x100: 0,
                current_destination_llc_x100: 0,
                predicted_destination_llc_x100: 0,
                signature_sample_count: 0,
                signature_confidence_x100: 0,
                signature_stable: false,
                planner_epoch: 0,
                plan_revision: 0,
                plan_used: false,
                fallback_reason: "",
                planned_domain: None,
                planned_cpu: None,
                sync_group_id: None,
                sync_anchor_domain: None,
                sync_override: false,
                debug: PlacementDebugInfo::default(),
            },
        }
    }

    pub(crate) fn target(mut self, domain: Option<u32>, cpu: Option<u32>) -> Self {
        self.decision.selected_domain = domain;
        self.decision.selected_cpu = cpu;
        self.decision.planned_domain = domain;
        self.decision.planned_cpu = cpu;
        self
    }

    pub(crate) fn tick(mut self, tick_decision: TickDecision) -> Self {
        self.decision.tick_decision = tick_decision;
        self
    }

    pub(crate) fn signature(
        mut self,
        valid: bool,
        sample_count: u32,
        confidence_x100: u32,
        stable: bool,
    ) -> Self {
        self.decision.signature_valid = valid;
        self.decision.signature_sample_count = sample_count;
        self.decision.signature_confidence_x100 = confidence_x100;
        self.decision.signature_stable = stable;
        self
    }

    pub(crate) fn score(mut self, score_gain: u64) -> Self {
        self.decision.score_gain = score_gain;
        self
    }

    pub(crate) fn villain_score(mut self, villain_score: u32) -> Self {
        self.decision.villain_score = villain_score;
        self
    }

    pub(crate) fn defer_dispatch(mut self, defer_dispatch: bool) -> Self {
        self.decision.defer_dispatch = defer_dispatch;
        self
    }

    pub(crate) fn slice(mut self, slice_ns: u64) -> Self {
        self.decision.slice_ns = slice_ns;
        self
    }

    pub(crate) fn source(
        mut self,
        current_df_x100: u32,
        predicted_df_x100: u32,
        current_llc_x100: u32,
        predicted_llc_x100: u32,
    ) -> Self {
        self.decision.current_source_df_x100 = current_df_x100;
        self.decision.predicted_source_df_x100 = predicted_df_x100;
        self.decision.current_source_llc_x100 = current_llc_x100;
        self.decision.predicted_source_llc_x100 = predicted_llc_x100;
        self
    }

    pub(crate) fn destination(
        mut self,
        current_df_x100: u32,
        predicted_df_x100: u32,
        current_llc_x100: u32,
        predicted_llc_x100: u32,
    ) -> Self {
        self.decision.current_destination_df_x100 = current_df_x100;
        self.decision.predicted_destination_df_x100 = predicted_df_x100;
        self.decision.current_destination_llc_x100 = current_llc_x100;
        self.decision.predicted_destination_llc_x100 = predicted_llc_x100;
        self
    }

    pub(crate) fn debug(mut self, debug: PlacementDebugInfo) -> Self {
        self.decision.debug = debug;
        self
    }

    pub(crate) fn build(self) -> PlacementDecision {
        self.decision
    }
}

#[cfg(test)]
mod tests {
    use super::DecisionBuilder;
    use crate::types::{DecisionReason, QueueTrigger, ThreadClass};

    #[test]
    fn builder_defaults_optional_planner_and_sync_metadata_to_none() {
        let decision = DecisionBuilder::new(
            ThreadClass::LinkNeed,
            DecisionReason::StayCurrentDomain,
            QueueTrigger::Enqueue,
        )
        .build();

        assert_eq!(decision.selected_domain, None);
        assert_eq!(decision.selected_cpu, None);
        assert_eq!(decision.planned_domain, None);
        assert_eq!(decision.planned_cpu, None);
        assert_eq!(decision.sync_group_id, None);
        assert_eq!(decision.sync_anchor_domain, None);
        assert!(!decision.plan_used);
        assert_eq!(decision.fallback_reason, "");
    }

    #[test]
    fn builder_target_sets_selected_and_planned_domains_as_options() {
        let decision = DecisionBuilder::new(
            ThreadClass::LinkNeed,
            DecisionReason::MoveCoolerDomain,
            QueueTrigger::Tick,
        )
        .target(Some(3), Some(17))
        .build();

        assert_eq!(decision.selected_domain, Some(3));
        assert_eq!(decision.selected_cpu, Some(17));
        assert_eq!(decision.planned_domain, Some(3));
        assert_eq!(decision.planned_cpu, Some(17));
    }
}
