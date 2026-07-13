use crate::policy::SlotCapacityConstraint;
use std::cmp::Ordering;

pub(super) const PLANNER_HARD_CONSTRAINT_DIAGNOSTIC_COST: u64 = 4_000_000_000_000;

#[derive(Clone, Debug, Default)]
pub(super) struct ConstraintViolations {
    pub(super) migration_budget_domains: Vec<u32>,
    pub(super) sync_group_splits: Vec<u32>,
    pub(super) residual_pair_splits: Vec<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PlannerCostBreakdown {
    pub(super) df_overload_cost: u64,
    pub(super) llc_pressure_cost: u64,
    pub(super) signature_balance_cost: u64,
    pub(super) hard_constraints: PlannerHardConstraints,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PlannerHardConstraints {
    pub(super) slot_over_capacity: SlotCapacityConstraint,
    pub(super) migration_budget_violations: u32,
    pub(super) sync_split_violations: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PlannerCostKey {
    hard_constraints: PlannerHardConstraints,
    soft_pressure_cost: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DomainScore {
    pub(super) predicted_df_x100: u32,
    pub(super) predicted_llc_x100: u32,
    pub(super) predicted_active_task_count: u32,
    pub(super) predicted_cpu_capacity: u32,
    pub(super) overload_x100: u32,
    pub(super) cost: PlannerCostBreakdown,
    pub(super) migrations_in: u32,
    pub(super) migrations_out: u32,
    pub(super) migration_budget: u32,
    pub(super) migration_penalty: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PlanEvaluation {
    pub(super) cost: PlannerCostBreakdown,
    pub(super) domains: Vec<DomainScore>,
    pub(super) violations: ConstraintViolations,
}

#[derive(Clone, Debug)]
pub(super) struct AssignmentTotals {
    pub(super) assigned_df_x100: Vec<u32>,
    pub(super) assigned_llc_x100: Vec<u32>,
    pub(super) assigned_active_task_count: Vec<u32>,
    pub(super) predicted_cpu_capacity: Vec<u32>,
    pub(super) affinity_slot_constraint: Vec<SlotCapacityConstraint>,
    pub(super) migrations_in: Vec<u32>,
    pub(super) migrations_out: Vec<u32>,
}

impl ConstraintViolations {
    pub(super) fn count(&self) -> usize {
        self.migration_budget_domains.len()
            + self.sync_group_splits.len()
            + self.residual_pair_splits.len()
    }
}

impl PlannerCostBreakdown {
    pub(super) fn soft_cost(self) -> u64 {
        self.df_overload_cost
            .saturating_add(self.llc_pressure_cost)
            .saturating_add(self.signature_balance_cost)
    }

    pub(super) fn hard_constraints(self) -> PlannerHardConstraints {
        self.hard_constraints
    }

    pub(super) fn hard_constraint_diagnostic_cost(self) -> u64 {
        self.hard_constraints.diagnostic_cost()
    }

    pub(super) fn total_cost(self) -> u64 {
        self.soft_cost()
            .saturating_add(self.hard_constraint_diagnostic_cost())
    }

    pub(super) fn placement_key(self) -> PlannerCostKey {
        PlannerCostKey {
            hard_constraints: self.hard_constraints(),
            soft_pressure_cost: self.soft_cost(),
        }
    }

    pub(super) fn cmp_for_placement(self, other: PlannerCostBreakdown) -> Ordering {
        self.placement_key().cmp(&other.placement_key())
    }

    pub(super) fn is_better_than(self, other: PlannerCostBreakdown) -> bool {
        self.cmp_for_placement(other).is_lt()
    }

    pub(super) fn clears_margin_against(
        self,
        baseline: PlannerCostBreakdown,
        margin_x100: u32,
    ) -> bool {
        if !self.is_better_than(baseline) {
            return false;
        }
        if self.hard_constraints() < baseline.hard_constraints() {
            return true;
        }
        if margin_x100 == 0 {
            return true;
        }
        let baseline_soft = baseline.soft_cost();
        let candidate_soft = self.soft_cost();
        if candidate_soft >= baseline_soft {
            return false;
        }
        let cost_upper_bound = baseline_soft
            .saturating_sub(baseline_soft.saturating_mul(u64::from(margin_x100)) / 10_000);
        candidate_soft < cost_upper_bound
    }

    pub(super) fn add_saturating(&mut self, other: PlannerCostBreakdown) {
        self.df_overload_cost = self.df_overload_cost.saturating_add(other.df_overload_cost);
        self.llc_pressure_cost = self
            .llc_pressure_cost
            .saturating_add(other.llc_pressure_cost);
        self.signature_balance_cost = self
            .signature_balance_cost
            .saturating_add(other.signature_balance_cost);
        self.hard_constraints.add_saturating(other.hard_constraints);
    }
}

impl Ord for PlannerCostKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hard_constraints
            .cmp(&other.hard_constraints)
            .then_with(|| self.soft_pressure_cost.cmp(&other.soft_pressure_cost))
    }
}

impl PartialOrd for PlannerCostKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PlannerHardConstraints {
    pub(super) fn diagnostic_cost(self) -> u64 {
        self.slot_over_capacity
            .diagnostic_cost()
            .saturating_add(
                PLANNER_HARD_CONSTRAINT_DIAGNOSTIC_COST
                    .saturating_mul(u64::from(self.migration_budget_violations)),
            )
            .saturating_add(
                PLANNER_HARD_CONSTRAINT_DIAGNOSTIC_COST
                    .saturating_mul(u64::from(self.sync_split_violations)),
            )
    }

    pub(super) fn add_saturating(&mut self, other: PlannerHardConstraints) {
        self.slot_over_capacity = self.slot_over_capacity.combine(other.slot_over_capacity);
        self.migration_budget_violations = self
            .migration_budget_violations
            .saturating_add(other.migration_budget_violations);
        self.sync_split_violations = self
            .sync_split_violations
            .saturating_add(other.sync_split_violations);
    }

    fn secondary_violation_count(self) -> u32 {
        self.migration_budget_violations
            .saturating_add(self.sync_split_violations)
    }
}

impl Ord for PlannerHardConstraints {
    fn cmp(&self, other: &Self) -> Ordering {
        self.slot_over_capacity
            .cmp(&other.slot_over_capacity)
            .then_with(|| {
                self.secondary_violation_count()
                    .cmp(&other.secondary_violation_count())
            })
            .then_with(|| {
                self.migration_budget_violations
                    .cmp(&other.migration_budget_violations)
            })
            .then_with(|| self.sync_split_violations.cmp(&other.sync_split_violations))
    }
}

impl PartialOrd for PlannerHardConstraints {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::{PlannerCostBreakdown, PlannerHardConstraints};
    use crate::policy::SlotCapacityConstraint;

    #[test]
    fn planner_cost_breakdown_separates_soft_and_hard_costs() {
        let mut cost = PlannerCostBreakdown {
            df_overload_cost: 10,
            llc_pressure_cost: 20,
            signature_balance_cost: 30,
            hard_constraints: PlannerHardConstraints {
                slot_over_capacity: SlotCapacityConstraint::OverCapacity {
                    extra_tasks: 1,
                    domain_count: 1,
                },
                migration_budget_violations: 2,
                sync_split_violations: 3,
            },
        };
        cost.add_saturating(PlannerCostBreakdown {
            df_overload_cost: 1,
            llc_pressure_cost: 2,
            signature_balance_cost: 3,
            hard_constraints: PlannerHardConstraints {
                slot_over_capacity: SlotCapacityConstraint::OverCapacity {
                    extra_tasks: 2,
                    domain_count: 1,
                },
                migration_budget_violations: 5,
                sync_split_violations: 6,
            },
        });

        assert_eq!(cost.soft_cost(), 66);
        assert_eq!(
            cost.hard_constraints.slot_over_capacity,
            SlotCapacityConstraint::OverCapacity {
                extra_tasks: 3,
                domain_count: 2
            }
        );
        assert_eq!(cost.hard_constraints.migration_budget_violations, 7);
        assert_eq!(cost.hard_constraints.sync_split_violations, 9);
        assert_eq!(
            cost.total_cost(),
            cost.soft_cost() + cost.hard_constraints.diagnostic_cost()
        );
    }

    #[test]
    fn planner_cost_comparator_prioritizes_hard_constraints() {
        let lower_soft_worse_hard = PlannerCostBreakdown {
            df_overload_cost: 1,
            hard_constraints: PlannerHardConstraints {
                migration_budget_violations: 1,
                ..PlannerHardConstraints::default()
            },
            ..PlannerCostBreakdown::default()
        };
        let higher_soft_clean_hard = PlannerCostBreakdown {
            df_overload_cost: 10_000,
            ..PlannerCostBreakdown::default()
        };
        assert!(!lower_soft_worse_hard.is_better_than(higher_soft_clean_hard));
        assert!(higher_soft_clean_hard.is_better_than(lower_soft_worse_hard));
    }

    #[test]
    fn planner_cost_margin_applies_inside_same_hard_constraint_level() {
        let baseline = PlannerCostBreakdown {
            df_overload_cost: 1_000,
            ..PlannerCostBreakdown::default()
        };
        let too_small = PlannerCostBreakdown {
            df_overload_cost: 960,
            ..PlannerCostBreakdown::default()
        };
        let large_enough = PlannerCostBreakdown {
            df_overload_cost: 940,
            ..PlannerCostBreakdown::default()
        };
        let hard_improvement = PlannerCostBreakdown {
            df_overload_cost: 10_000,
            ..PlannerCostBreakdown::default()
        };
        let hard_baseline = PlannerCostBreakdown {
            df_overload_cost: 1,
            hard_constraints: PlannerHardConstraints {
                slot_over_capacity: SlotCapacityConstraint::OverCapacity {
                    extra_tasks: 1,
                    domain_count: 1,
                },
                ..PlannerHardConstraints::default()
            },
            ..PlannerCostBreakdown::default()
        };

        assert!(!too_small.clears_margin_against(baseline, 500));
        assert!(large_enough.clears_margin_against(baseline, 500));
        assert!(hard_improvement.clears_margin_against(hard_baseline, 500));
    }
}
