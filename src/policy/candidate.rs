#![allow(dead_code)]

use std::cmp::Ordering;

pub(crate) const PRESSURE_COST_UNIT: u64 = 4_000_000_000_000;
pub(crate) const NON_IDLE_CPU_SELECTION_SOFT_COST: u64 = PRESSURE_COST_UNIT;
const SLOT_OVER_CAPACITY_BASE_DIAGNOSTIC_COST_MULTIPLIER: u64 = 2_500;
const SLOT_OVER_CAPACITY_PER_EXTRA_TASK_DIAGNOSTIC_COST_MULTIPLIER: u64 = 25_000;
pub(crate) const SLOT_OVER_CAPACITY_BASE_DIAGNOSTIC_COST: u64 =
    PRESSURE_COST_UNIT * SLOT_OVER_CAPACITY_BASE_DIAGNOSTIC_COST_MULTIPLIER;
pub(crate) const SLOT_OVER_CAPACITY_PER_EXTRA_TASK_DIAGNOSTIC_COST: u64 =
    PRESSURE_COST_UNIT * SLOT_OVER_CAPACITY_PER_EXTRA_TASK_DIAGNOSTIC_COST_MULTIPLIER;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SlotCapacityConstraint {
    #[default]
    WithinCapacity,
    OverCapacity {
        extra_tasks: u32,
        domain_count: u32,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CostBreakdown {
    pub(crate) total_overload_x100: u32,
    pub(crate) max_overload_x100: u32,
    pub(crate) pair_penalty: u64,
    pub(crate) pressure_cost: u64,
    pub(crate) slot_constraint: SlotCapacityConstraint,
    pub(crate) cpu_selection_cost: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CostKey {
    hard_constraint: SlotCapacityConstraint,
    soft_move_cost: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CandidateTieBreakKey {
    total_overload_x100: u32,
    max_overload_x100: u32,
    pair_penalty: u64,
    cpu: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CandidateEval {
    pub(crate) domain: u32,
    pub(crate) cpu: u32,
    pub(crate) destination_df_before_x100: u32,
    pub(crate) destination_llc_before_x100: u32,
    pub(crate) destination_df_delta_x100: u32,
    pub(crate) destination_llc_delta_x100: u32,
    pub(crate) cost: CostBreakdown,
}

impl CandidateEval {
    pub(crate) fn is_better_than(self, other: Self) -> bool {
        match self.cost.cmp_for_placement(other.cost) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => self.tie_break_key() < other.tie_break_key(),
        }
    }

    fn tie_break_key(self) -> CandidateTieBreakKey {
        CandidateTieBreakKey {
            total_overload_x100: self.cost.total_overload_x100,
            max_overload_x100: self.cost.max_overload_x100,
            pair_penalty: self.cost.pair_penalty,
            cpu: self.cpu,
        }
    }
}

impl CostBreakdown {
    pub(crate) fn hard_constraint(self) -> SlotCapacityConstraint {
        self.slot_constraint
    }

    pub(crate) fn hard_constraint_diagnostic_cost(self) -> u64 {
        self.slot_constraint.diagnostic_cost()
    }

    pub(crate) fn soft_cost(self) -> u64 {
        self.pressure_cost.saturating_add(self.cpu_selection_cost)
    }

    pub(crate) fn total_cost(self) -> u64 {
        self.hard_constraint_diagnostic_cost()
            .saturating_add(self.soft_cost())
    }

    pub(crate) fn placement_key(self) -> CostKey {
        CostKey {
            hard_constraint: self.hard_constraint(),
            soft_move_cost: self.soft_cost(),
        }
    }

    pub(crate) fn cmp_for_placement(self, other: CostBreakdown) -> Ordering {
        self.placement_key().cmp(&other.placement_key())
    }

    pub(crate) fn is_better_than(self, other: CostBreakdown) -> bool {
        self.cmp_for_placement(other).is_lt()
    }

    pub(crate) fn clears_margin_against(self, baseline: CostBreakdown, margin_x100: u32) -> bool {
        if !self.is_better_than(baseline) {
            return false;
        }
        if self.hard_constraint() < baseline.hard_constraint() {
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
        candidate_soft <= cost_upper_bound
    }
}

impl Ord for CostKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hard_constraint
            .cmp(&other.hard_constraint)
            .then_with(|| self.soft_move_cost.cmp(&other.soft_move_cost))
    }
}

impl PartialOrd for CostKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl SlotCapacityConstraint {
    pub(crate) fn from_task_count(task_count: u32, cpu_capacity: u32) -> Self {
        let extra_tasks = if cpu_capacity == 0 {
            task_count
        } else {
            task_count.saturating_sub(cpu_capacity)
        };
        if extra_tasks == 0 {
            SlotCapacityConstraint::WithinCapacity
        } else {
            SlotCapacityConstraint::OverCapacity {
                extra_tasks,
                domain_count: 1,
            }
        }
    }

    pub(crate) fn combine(self, other: Self) -> Self {
        match (self, other) {
            (SlotCapacityConstraint::WithinCapacity, constraint)
            | (constraint, SlotCapacityConstraint::WithinCapacity) => constraint,
            (
                SlotCapacityConstraint::OverCapacity {
                    extra_tasks: left_extra_tasks,
                    domain_count: left_domain_count,
                },
                SlotCapacityConstraint::OverCapacity {
                    extra_tasks: right_extra_tasks,
                    domain_count: right_domain_count,
                },
            ) => SlotCapacityConstraint::OverCapacity {
                extra_tasks: left_extra_tasks.saturating_add(right_extra_tasks),
                domain_count: left_domain_count.saturating_add(right_domain_count),
            },
        }
    }

    pub(crate) fn extra_tasks(self) -> u32 {
        match self {
            SlotCapacityConstraint::WithinCapacity => 0,
            SlotCapacityConstraint::OverCapacity { extra_tasks, .. } => extra_tasks,
        }
    }

    pub(crate) fn diagnostic_cost(self) -> u64 {
        match self {
            SlotCapacityConstraint::WithinCapacity => 0,
            SlotCapacityConstraint::OverCapacity {
                extra_tasks,
                domain_count,
            } => SLOT_OVER_CAPACITY_BASE_DIAGNOSTIC_COST
                .saturating_mul(u64::from(domain_count))
                .saturating_add(
                    SLOT_OVER_CAPACITY_PER_EXTRA_TASK_DIAGNOSTIC_COST
                        .saturating_mul(u64::from(extra_tasks)),
                ),
        }
    }
}

impl Ord for SlotCapacityConstraint {
    fn cmp(&self, other: &Self) -> Ordering {
        use SlotCapacityConstraint::{OverCapacity, WithinCapacity};
        match (*self, *other) {
            (WithinCapacity, WithinCapacity) => Ordering::Equal,
            (WithinCapacity, OverCapacity { .. }) => Ordering::Less,
            (OverCapacity { .. }, WithinCapacity) => Ordering::Greater,
            (
                OverCapacity {
                    extra_tasks: left_extra_tasks,
                    domain_count: left_domain_count,
                },
                OverCapacity {
                    extra_tasks: right_extra_tasks,
                    domain_count: right_domain_count,
                },
            ) => left_extra_tasks
                .cmp(&right_extra_tasks)
                .then_with(|| left_domain_count.cmp(&right_domain_count)),
        }
    }
}

impl PartialOrd for SlotCapacityConstraint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CandidateScanStats {
    pub(crate) blocked_by_budget: bool,
    pub(crate) blocked_by_phase: bool,
    pub(crate) blocked_by_reverse: bool,
    pub(crate) blocked_by_destination: bool,
    pub(crate) blocked_by_soft_only_stay: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CandidateScan {
    pub(crate) best: Option<CandidateEval>,
    pub(crate) stats: CandidateScanStats,
}

impl CandidateScan {
    pub(crate) fn note_candidate(&mut self, candidate: CandidateEval) {
        if self
            .best
            .map(|best| candidate.is_better_than(best))
            .unwrap_or(true)
        {
            self.best = Some(candidate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CandidateEval, CostBreakdown, SlotCapacityConstraint};

    #[test]
    fn fallback_cost_comparator_prioritizes_hard_constraints() {
        let lower_soft_worse_hard = CostBreakdown {
            pressure_cost: 1,
            slot_constraint: SlotCapacityConstraint::OverCapacity {
                extra_tasks: 1,
                domain_count: 1,
            },
            ..CostBreakdown::default()
        };
        let higher_soft_clean_hard = CostBreakdown {
            pressure_cost: 10_000,
            ..CostBreakdown::default()
        };

        assert!(!lower_soft_worse_hard.is_better_than(higher_soft_clean_hard));
        assert!(higher_soft_clean_hard.is_better_than(lower_soft_worse_hard));
    }

    #[test]
    fn fallback_cost_margin_applies_inside_same_hard_constraint_level() {
        let baseline = CostBreakdown {
            pressure_cost: 1_000,
            ..CostBreakdown::default()
        };
        let too_small = CostBreakdown {
            pressure_cost: 960,
            ..CostBreakdown::default()
        };
        let large_enough = CostBreakdown {
            pressure_cost: 950,
            ..CostBreakdown::default()
        };
        let hard_baseline = CostBreakdown {
            pressure_cost: 1,
            slot_constraint: SlotCapacityConstraint::OverCapacity {
                extra_tasks: 1,
                domain_count: 1,
            },
            ..CostBreakdown::default()
        };
        let hard_improvement = CostBreakdown {
            pressure_cost: 10_000,
            ..CostBreakdown::default()
        };

        assert!(!too_small.clears_margin_against(baseline, 500));
        assert!(large_enough.clears_margin_against(baseline, 500));
        assert!(hard_improvement.clears_margin_against(hard_baseline, 500));
    }

    #[test]
    fn slot_capacity_constraint_orders_extra_tasks_before_domain_count() {
        let one_extra_two_domains = SlotCapacityConstraint::OverCapacity {
            extra_tasks: 1,
            domain_count: 2,
        };
        let two_extra_one_domain = SlotCapacityConstraint::OverCapacity {
            extra_tasks: 2,
            domain_count: 1,
        };

        assert!(SlotCapacityConstraint::WithinCapacity < one_extra_two_domains);
        assert!(one_extra_two_domains < two_extra_one_domain);
    }

    #[test]
    fn candidate_tie_breakers_still_use_pressure_shape_and_cpu() {
        let higher_cpu = CandidateEval {
            cpu: 9,
            cost: CostBreakdown {
                pressure_cost: 100,
                total_overload_x100: 10,
                max_overload_x100: 5,
                pair_penalty: 1,
                ..CostBreakdown::default()
            },
            ..CandidateEval::default()
        };
        let lower_cpu = CandidateEval {
            cpu: 8,
            cost: CostBreakdown {
                pressure_cost: 100,
                total_overload_x100: 10,
                max_overload_x100: 5,
                pair_penalty: 1,
                ..CostBreakdown::default()
            },
            ..CandidateEval::default()
        };
        assert!(lower_cpu.is_better_than(higher_cpu));
    }
}
