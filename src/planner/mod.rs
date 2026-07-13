use crate::cli::ControlPlaneCpuPolicy;
use crate::overhead_profile::ThreadClockSample;
use crate::planner_move_trace::PlannerMoveTraceLogger;
use crate::policy::{
    domain_signature_balance_penalty, effective_migrate_margin_x100, overload_cost,
    overload_x100_with_capacity, task_slot_constraint, thread_signature_effect, usable_llc,
    SlotCapacityConstraint,
};
use crate::types::{
    CcmDfStateValue, LlcStateValue, ManagedThreadState, MappingInfo, PolicyConfig, TopologyLayout,
    MEM_SOURCE_DRAM_NEAR, MEM_SOURCE_NEAR_CACHE, SIGNATURE_CONFIDENCE_THRESHOLD_X100,
};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod constraints;
mod cost;
mod mode;
mod optimizer;
mod state;
mod trace;
mod worker;

use constraints::SyncConstraint;
use cost::{
    AssignmentTotals, ConstraintViolations, DomainScore, PlanEvaluation, PlannerCostBreakdown,
    PlannerHardConstraints,
};
use mode::{needs_global_rebuild, resolve_plan_mode, PlanMode, PlannerScope};
use state::{DomainModel, PlannerState};
use trace::{format_member_domains, format_optional_u32, format_triggers, format_u32_list};
pub use worker::{maybe_dedicated_control_plane_cpu, start_worker, sync_qualified_for_planner};

const SYNC_NEAR_CACHE_SHARE_MIN_X100: u32 = 4_500;
const SYNC_DRAM_NEAR_SHARE_MAX_X100: u32 = 1_500;
const SYNC_NEAR_CACHE_BW_MIN_MIB_S_X100: u32 = 12_800;
const SYNC_EWMA_STALL_MIN_X100: u32 = 3_500;
const SYNC_STALL_DELTA_MIN_X100: u32 = 1_000;

#[derive(Clone, Debug)]
pub enum PlannerTrigger {
    SweepComplete(u64),
    RunnableDelta(u32),
    SignatureChange(u32),
    DomainStateFlip(u32),
    TickPressure(u32),
}

#[derive(Clone, Debug, Default)]
pub struct PlacementPlanEntry {
    pub target_domain: u32,
    pub target_cpu: Option<u32>,
    pub built_from_sweep_epoch: u64,
    pub plan_revision: u64,
    pub planned_at_ns: u64,
    pub sync_group_id: Option<u32>,
    pub sync_anchor_domain: Option<u32>,
    pub sync_override: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PlacementPlan {
    pub built_from_sweep_epoch: u64,
    pub plan_revision: u64,
    pub planned_at_ns: u64,
    pub entries: BTreeMap<u32, PlacementPlanEntry>,
}

#[derive(Clone, Debug)]
pub struct PlannerInput {
    pub topo: TopologyLayout,
    pub mapping: MappingInfo,
    pub policy: PolicyConfig,
    pub now_ns: u64,
    pub latest_df_sweep_epoch: u64,
    pub pending_tids: BTreeSet<u32>,
    pub threads: BTreeMap<u32, ManagedThreadState>,
    pub allowed_cpus: BTreeMap<u32, BTreeSet<u32>>,
    pub allowed_domains: BTreeMap<u32, BTreeSet<u32>>,
    pub llc_states: Vec<Option<LlcStateValue>>,
    pub df_states: Vec<Option<CcmDfStateValue>>,
    pub previous_plan: Option<PlacementPlan>,
}

#[derive(Clone, Debug)]
pub struct PlannerRequest {
    pub snapshot: PlannerInput,
    pub triggers: Vec<PlannerTrigger>,
}

#[derive(Clone, Debug)]
pub struct PlannerOutput {
    pub plan: PlacementPlan,
    pub was_global: bool,
}

#[derive(Clone, Debug)]
pub struct PlannerConfig {
    pub disable_auto_sync_hints: bool,
    pub sync_tgid_overrides: BTreeSet<u32>,
    pub incremental_item_limit: usize,
    pub incremental_domain_limit: usize,
    pub max_passes: u32,
    pub swap_pass_items: usize,
    pub debounce: Duration,
    pub move_trace_path: Option<String>,
    pub plan_debug_path: Option<String>,
}

#[derive(Clone, Debug)]
struct MemberView {
    tid: u32,
    tgid: u32,
    current_domain: Option<u32>,
    baseline_domain: Option<u32>,
    allowed_domains: BTreeSet<u32>,
    allowed_cpus_by_domain: BTreeMap<u32, BTreeSet<u32>>,
    active: bool,
    sync_qualified: bool,
    sync_override: bool,
    effect_df_x100: u32,
    effect_llc_x100: u32,
}

#[derive(Clone, Debug)]
struct PlannerItem {
    member_tids: Vec<u32>,
    allowed_domains: BTreeSet<u32>,
    sync_group_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Assignment {
    CurrentHome,
    Domain(u32),
}

#[cfg(test)]
fn compute_plan(
    config: &PlannerConfig,
    snapshot: PlannerInput,
    triggers: &[PlannerTrigger],
    move_trace: &mut PlannerMoveTraceLogger,
) -> PlannerOutput {
    let mut plan_debug = PlannerMoveTraceLogger::new(None)
        .expect("disabled planner plan debug logger should not create a file");
    compute_plan_with_debug(config, snapshot, triggers, move_trace, &mut plan_debug)
}

fn compute_plan_with_debug(
    config: &PlannerConfig,
    snapshot: PlannerInput,
    triggers: &[PlannerTrigger],
    move_trace: &mut PlannerMoveTraceLogger,
    plan_debug: &mut PlannerMoveTraceLogger,
) -> PlannerOutput {
    let profiler = crate::overhead_profile::global();
    let compute_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let previous_revision = snapshot
        .previous_plan
        .as_ref()
        .map(|plan| plan.plan_revision)
        .unwrap_or(0);
    let mut mode = resolve_plan_mode(&snapshot, triggers);
    let build_state_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let mut state = build_state(config, &snapshot, &mode);
    if needs_global_rebuild(&state, config, &mode) {
        mode = PlanMode::Global;
        state = build_state(config, &snapshot, &mode);
    }
    if let (Some(profiler), Some(start)) = (&profiler, build_state_start) {
        profiler.record_scope("planner_build_state", start, true);
    }
    #[cfg(feature = "scheduler-paper-greedy")]
    {
        let paper_seed_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        apply_paper_greedy_seed(&snapshot, &mut state, mode.is_global());
        if let (Some(profiler), Some(start)) = (&profiler, paper_seed_start) {
            profiler.record_scope("planner_paper_greedy_seed", start, true);
        }
    }
    let optimize_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    optimize_state(&snapshot, config, &mut state, mode.is_global());
    if let (Some(profiler), Some(start)) = (&profiler, optimize_start) {
        profiler.record_scope("planner_optimize_state", start, true);
    }
    let next_revision = previous_revision.saturating_add(1);
    let move_trace_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    if let Err(err) = emit_move_trace(
        move_trace,
        &snapshot,
        &mut state,
        triggers,
        mode.is_global(),
        next_revision,
    ) {
        log::warn!("planner move trace write failed: {err}");
        if let (Some(profiler), Some(start)) = (&profiler, move_trace_start) {
            profiler.record_scope("planner_emit_move_trace", start, false);
        }
    } else if let (Some(profiler), Some(start)) = (&profiler, move_trace_start) {
        profiler.record_scope("planner_emit_move_trace", start, true);
    }
    let materialize_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let mut entries = BTreeMap::new();
    for (tid, member) in &state.members {
        let previous_domain = (!mode.is_global())
            .then(|| {
                previous_plan_domain_for_member(
                    *tid,
                    member.current_domain,
                    &member.allowed_domains,
                    &snapshot,
                )
            })
            .flatten();
        let target_domain = effective_member_domain(*tid, &state)
            .or(previous_domain)
            .or(member.current_domain);
        let Some(target_domain) = target_domain else {
            continue;
        };
        let item = state
            .item_index_by_tid
            .get(tid)
            .and_then(|index| state.items.get(*index));
        let sync_group_id = item.and_then(|item| item.sync_group_id);
        let sync_anchor_domain = item
            .and_then(|item| item.sync_group_id.map(|_| target_domain))
            .or_else(|| member.sync_qualified.then_some(target_domain));
        entries.insert(
            *tid,
            PlacementPlanEntry {
                target_domain,
                target_cpu: None,
                built_from_sweep_epoch: snapshot.latest_df_sweep_epoch,
                plan_revision: next_revision,
                planned_at_ns: snapshot.now_ns,
                sync_group_id,
                sync_anchor_domain,
                sync_override: member.sync_override,
            },
        );
    }
    assign_plan_target_cpus(&snapshot, &mut entries);
    if let (Some(profiler), Some(start)) = (&profiler, materialize_start) {
        profiler.record_scope("planner_materialize_entries", start, true);
    }
    let plan_debug_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    if let Err(err) = emit_plan_debug(
        plan_debug,
        &snapshot,
        &state,
        triggers,
        mode.is_global(),
        next_revision,
        &entries,
    ) {
        log::warn!("planner plan debug write failed: {err}");
        if let (Some(profiler), Some(start)) = (&profiler, plan_debug_start) {
            profiler.record_scope("planner_emit_plan_debug", start, false);
        }
    } else if let (Some(profiler), Some(start)) = (&profiler, plan_debug_start) {
        profiler.record_scope("planner_emit_plan_debug", start, true);
    }

    let output = PlannerOutput {
        plan: PlacementPlan {
            built_from_sweep_epoch: snapshot.latest_df_sweep_epoch,
            plan_revision: next_revision,
            planned_at_ns: snapshot.now_ns,
            entries,
        },
        was_global: mode.is_global(),
    };
    if let (Some(profiler), Some(start)) = (profiler, compute_start) {
        profiler.record_scope("planner_compute", start, true);
    }
    output
}

fn build_state(config: &PlannerConfig, snapshot: &PlannerInput, mode: &PlanMode) -> PlannerState {
    let active_cutoff_ns = snapshot.now_ns.saturating_sub(
        snapshot
            .policy
            .df_stale_ms
            .max(snapshot.policy.llc_stale_ms)
            * 1_000_000,
    );
    let mut members = BTreeMap::<u32, MemberView>::new();
    for (tid, thread) in &snapshot.threads {
        let current_domain = thread_home(thread);
        let allowed_domains = snapshot
            .allowed_domains
            .get(tid)
            .cloned()
            .unwrap_or_default();
        let allowed_cpus = snapshot.allowed_cpus.get(tid).cloned().unwrap_or_default();
        let allowed_cpus_by_domain = allowed_cpus_by_domain(snapshot, &allowed_cpus);
        let baseline_domain =
            baseline_domain_for_member(*tid, current_domain, &allowed_domains, snapshot, mode);
        let active = snapshot.pending_tids.contains(tid) || thread.last_seen_ns >= active_cutoff_ns;
        let sync_override = config.sync_tgid_overrides.contains(&thread.tgid);
        let sync_qualified =
            sync_override || (!config.disable_auto_sync_hints && thread_sync_qualified(thread));
        let effect = member_effect(thread);
        members.insert(
            *tid,
            MemberView {
                tid: *tid,
                tgid: thread.tgid,
                current_domain,
                baseline_domain,
                allowed_domains,
                allowed_cpus_by_domain,
                active,
                sync_qualified,
                sync_override,
                effect_df_x100: effect.0,
                effect_llc_x100: effect.1,
            },
        );
    }

    let (items, item_index_by_tid, sync_constraints) =
        build_items(&members, snapshot, config, mode);
    let mut assignments = items
        .iter()
        .map(|item| seed_assignment(item, snapshot, &members, mode.is_global()))
        .collect::<Vec<_>>();
    if assignments.len() != items.len() {
        assignments.resize(items.len(), Assignment::CurrentHome);
    }

    let domain_count = snapshot.topo.domains.len();
    let mut current_managed_df_x100 = vec![0u32; domain_count];
    let mut current_managed_llc_x100 = vec![0u32; domain_count];
    let mut current_active_task_count = vec![0u32; domain_count];
    let mut migration_budget = vec![1u32; domain_count];
    let mut task_count = vec![0u32; domain_count];
    for member in members.values() {
        if let Some(domain) = member.current_domain {
            if let Some(value) = task_count.get_mut(domain as usize) {
                *value = value.saturating_add(1);
            }
        }
        let Some(baseline_domain) = member.baseline_domain else {
            continue;
        };
        if !member.active {
            continue;
        }
        if let Some(value) = current_active_task_count.get_mut(baseline_domain as usize) {
            *value = value.saturating_add(1);
        }
        if let Some(value) = current_managed_df_x100.get_mut(baseline_domain as usize) {
            *value = value.saturating_add(member.effect_df_x100);
        }
        if let Some(value) = current_managed_llc_x100.get_mut(baseline_domain as usize) {
            *value = value.saturating_add(member.effect_llc_x100);
        }
    }
    for (idx, count) in task_count.into_iter().enumerate() {
        migration_budget[idx] = round_migration_budget(count.max(1));
    }

    let live_df_x100 = snapshot
        .df_states
        .iter()
        .map(|state| live_df_for_domain(*state, snapshot.now_ns, snapshot.policy.df_stale_ms))
        .collect::<Vec<_>>();
    let live_llc_x100 = snapshot
        .llc_states
        .iter()
        .map(|state| {
            usable_llc(*state, snapshot.now_ns, snapshot.policy.llc_stale_ms)
                .map(|value| value.ewma_pressure_pct_x100)
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut current_item_df_x100 = vec![0u32; domain_count];
    let mut current_item_llc_x100 = vec![0u32; domain_count];
    let mut current_item_active_task_count = vec![0u32; domain_count];
    for item in &items {
        for tid in &item.member_tids {
            let Some(member) = members.get(tid) else {
                continue;
            };
            let Some(domain) = member.baseline_domain else {
                continue;
            };
            if let Some(value) = current_item_df_x100.get_mut(domain as usize) {
                *value = value.saturating_add(member.effect_df_x100);
            }
            if let Some(value) = current_item_llc_x100.get_mut(domain as usize) {
                *value = value.saturating_add(member.effect_llc_x100);
            }
            if !member.active {
                continue;
            }
            if let Some(value) = current_item_active_task_count.get_mut(domain as usize) {
                *value = value.saturating_add(1);
            }
        }
    }
    let mut usable_cpus_by_domain = vec![BTreeSet::<u32>::new(); domain_count];
    for member in members.values() {
        if !member.active {
            continue;
        }
        let Some(allowed_cpus) = snapshot.allowed_cpus.get(&member.tid) else {
            continue;
        };
        for &cpu in allowed_cpus {
            if !snapshot.mapping.eligible_cpus.contains(&cpu) {
                continue;
            }
            let Some(domain) = snapshot
                .topo
                .cpu_to_domain
                .get(cpu as usize)
                .copied()
                .flatten()
            else {
                continue;
            };
            if let Some(cpus) = usable_cpus_by_domain.get_mut(domain as usize) {
                cpus.insert(cpu);
            }
        }
    }
    let domain_cpu_capacity = usable_cpus_by_domain
        .into_iter()
        .map(|cpus| cpus.len() as u32)
        .collect::<Vec<_>>();
    let domains = (0..domain_count)
        .map(|idx| DomainModel {
            df_capacity_mib_s_x100: snapshot.mapping.df_capacity_mib_s_x100(idx as u32),
            live_df_x100: live_df_x100.get(idx).copied().unwrap_or(0),
            live_llc_x100: live_llc_x100.get(idx).copied().unwrap_or(0),
            current_managed_df_x100: current_managed_df_x100.get(idx).copied().unwrap_or(0),
            current_managed_llc_x100: current_managed_llc_x100.get(idx).copied().unwrap_or(0),
            current_item_df_x100: current_item_df_x100.get(idx).copied().unwrap_or(0),
            current_item_llc_x100: current_item_llc_x100.get(idx).copied().unwrap_or(0),
            current_active_task_count: current_active_task_count.get(idx).copied().unwrap_or(0),
            current_item_active_task_count: current_item_active_task_count
                .get(idx)
                .copied()
                .unwrap_or(0),
            cpu_capacity: domain_cpu_capacity.get(idx).copied().unwrap_or(0),
            migration_budget: migration_budget.get(idx).copied().unwrap_or(0),
        })
        .collect::<Vec<_>>();

    PlannerState {
        members,
        items,
        item_index_by_tid,
        sync_constraints,
        domains,
        assignments,
    }
}

fn allowed_cpus_by_domain(
    snapshot: &PlannerInput,
    allowed_cpus: &BTreeSet<u32>,
) -> BTreeMap<u32, BTreeSet<u32>> {
    let mut by_domain = BTreeMap::<u32, BTreeSet<u32>>::new();
    for &cpu in allowed_cpus {
        if !snapshot.mapping.eligible_cpus.contains(&cpu) {
            continue;
        }
        let Some(domain) = snapshot
            .topo
            .cpu_to_domain
            .get(cpu as usize)
            .copied()
            .flatten()
        else {
            continue;
        };
        by_domain.entry(domain).or_default().insert(cpu);
    }
    by_domain
}

fn build_items(
    members: &BTreeMap<u32, MemberView>,
    snapshot: &PlannerInput,
    _config: &PlannerConfig,
    mode: &PlanMode,
) -> (Vec<PlannerItem>, BTreeMap<u32, usize>, Vec<SyncConstraint>) {
    let mut included_tids = BTreeSet::<u32>::new();
    let target_tids = if mode.is_global() {
        members
            .values()
            .filter(|member| member.active)
            .map(|member| member.tid)
            .collect::<BTreeSet<_>>()
    } else if let PlanMode::Incremental(scope) = mode {
        affected_tids(members, snapshot, scope)
    } else {
        BTreeSet::new()
    };
    let mut items = Vec::<PlannerItem>::new();
    let mut item_index_by_tid = BTreeMap::<u32, usize>::new();
    let mut sync_constraints = Vec::<SyncConstraint>::new();
    let mut per_tgid = BTreeMap::<u32, Vec<u32>>::new();
    for tid in &target_tids {
        let Some(member) = members.get(tid) else {
            continue;
        };
        if member.sync_qualified && member.active {
            per_tgid.entry(member.tgid).or_default().push(*tid);
        }
    }

    for (tgid, tids) in per_tgid {
        if tids.len() < 2 {
            continue;
        }
        let group_df_x100 = tids
            .iter()
            .filter_map(|tid| members.get(tid))
            .fold(0u32, |acc, member| {
                acc.saturating_add(member.effect_df_x100)
            });
        let Some(common_domains) = tids.iter().fold(None::<BTreeSet<u32>>, |acc, tid| {
            let domains = members
                .get(tid)
                .map(|member| member.allowed_domains.clone())
                .unwrap_or_default();
            Some(match acc {
                None => domains,
                Some(previous) => previous.intersection(&domains).copied().collect(),
            })
        }) else {
            continue;
        };
        if !common_domains.is_empty()
            && (!cfg!(feature = "scheduler-paper-greedy")
                || common_domains
                    .iter()
                    .map(|domain| snapshot.mapping.df_capacity_mib_s_x100(*domain))
                    .max()
                    .is_some_and(|capacity| group_df_x100 <= capacity))
        {
            let index = items.len();
            items.push(PlannerItem {
                member_tids: tids.clone(),
                allowed_domains: common_domains,
                sync_group_id: Some(tgid),
            });
            for tid in &tids {
                included_tids.insert(*tid);
                item_index_by_tid.insert(*tid, index);
            }
            sync_constraints.push(SyncConstraint::CoLocateGroup { tgid, tids });
            continue;
        }

        for i in 0..tids.len() {
            for j in (i + 1)..tids.len() {
                let left = tids[i];
                let right = tids[j];
                let left_domains = members
                    .get(&left)
                    .map(|member| member.allowed_domains.clone())
                    .unwrap_or_default();
                let right_domains = members
                    .get(&right)
                    .map(|member| member.allowed_domains.clone())
                    .unwrap_or_default();
                let shared = left_domains
                    .intersection(&right_domains)
                    .copied()
                    .collect::<BTreeSet<_>>();
                if !shared.is_empty() {
                    sync_constraints.push(SyncConstraint::ResidualPair { left, right });
                }
            }
        }
    }

    for tid in &target_tids {
        if included_tids.contains(tid) {
            continue;
        }
        let Some(member) = members.get(tid) else {
            continue;
        };
        let index = items.len();
        items.push(PlannerItem {
            member_tids: vec![*tid],
            allowed_domains: member.allowed_domains.clone(),
            sync_group_id: member.sync_qualified.then_some(member.tgid),
        });
        item_index_by_tid.insert(*tid, index);
    }

    (items, item_index_by_tid, sync_constraints)
}

fn seed_assignment(
    item: &PlannerItem,
    snapshot: &PlannerInput,
    members: &BTreeMap<u32, MemberView>,
    was_global: bool,
) -> Assignment {
    if was_global {
        return Assignment::CurrentHome;
    }
    let Some(previous_plan) = snapshot.previous_plan.as_ref() else {
        return Assignment::CurrentHome;
    };
    let mut previous_target = None::<u32>;
    for tid in &item.member_tids {
        let Some(entry) = previous_plan.entries.get(tid) else {
            return Assignment::CurrentHome;
        };
        if let Some(existing) = previous_target {
            if existing != entry.target_domain {
                return Assignment::CurrentHome;
            }
        } else {
            previous_target = Some(entry.target_domain);
        }
    }
    let Some(target_domain) = previous_target else {
        return Assignment::CurrentHome;
    };
    if !item.allowed_domains.contains(&target_domain)
        && !item.member_tids.iter().all(|tid| {
            members.get(tid).and_then(|member| member.current_domain) == Some(target_domain)
        })
    {
        return Assignment::CurrentHome;
    }
    Assignment::Domain(target_domain)
}

fn previous_plan_domain_for_member(
    tid: u32,
    current_domain: Option<u32>,
    allowed_domains: &BTreeSet<u32>,
    snapshot: &PlannerInput,
) -> Option<u32> {
    let target_domain = snapshot
        .previous_plan
        .as_ref()?
        .entries
        .get(&tid)?
        .target_domain;
    if target_domain as usize >= snapshot.topo.domains.len() {
        return None;
    }
    (allowed_domains.contains(&target_domain) || current_domain == Some(target_domain))
        .then_some(target_domain)
}

fn baseline_domain_for_member(
    tid: u32,
    current_domain: Option<u32>,
    allowed_domains: &BTreeSet<u32>,
    snapshot: &PlannerInput,
    mode: &PlanMode,
) -> Option<u32> {
    if mode.is_global() {
        return current_domain;
    }
    previous_plan_domain_for_member(tid, current_domain, allowed_domains, snapshot)
        .or(current_domain)
}

fn thread_cpu(thread: &ManagedThreadState) -> Option<u32> {
    thread.last_selected_cpu.or(thread.last_observed_cpu)
}

fn candidate_cpus_for_plan_target(
    snapshot: &PlannerInput,
    tid: u32,
    target_domain: u32,
) -> BTreeSet<u32> {
    let Some(allowed_cpus) = snapshot.allowed_cpus.get(&tid) else {
        return BTreeSet::new();
    };
    let eligible = allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| snapshot.mapping.eligible_cpus.contains(cpu))
        .filter(|cpu| {
            snapshot
                .topo
                .cpu_to_domain
                .get(*cpu as usize)
                .copied()
                .flatten()
                == Some(target_domain)
        })
        .collect::<BTreeSet<_>>();
    if !eligible.is_empty() {
        return eligible;
    }
    allowed_cpus
        .iter()
        .copied()
        .filter(|cpu| {
            snapshot
                .topo
                .cpu_to_domain
                .get(*cpu as usize)
                .copied()
                .flatten()
                == Some(target_domain)
        })
        .collect()
}

fn plan_cpu_load(cpu_loads: &BTreeMap<u32, Vec<u32>>, cpu: u32) -> usize {
    cpu_loads.get(&cpu).map(|jobs| jobs.len()).unwrap_or(0)
}

fn legal_plan_cpu(
    snapshot: &PlannerInput,
    tid: u32,
    target_domain: u32,
    cpu: Option<u32>,
) -> Option<u32> {
    let cpu = cpu?;
    candidate_cpus_for_plan_target(snapshot, tid, target_domain)
        .contains(&cpu)
        .then_some(cpu)
}

fn active_for_cpu_plan(snapshot: &PlannerInput, tid: u32) -> bool {
    let active_cutoff_ns = snapshot.now_ns.saturating_sub(
        snapshot
            .policy
            .df_stale_ms
            .max(snapshot.policy.llc_stale_ms)
            * 1_000_000,
    );
    snapshot.pending_tids.contains(&tid)
        || snapshot
            .threads
            .get(&tid)
            .is_some_and(|thread| thread.last_seen_ns >= active_cutoff_ns)
}

fn assign_plan_target_cpus(
    snapshot: &PlannerInput,
    entries: &mut BTreeMap<u32, PlacementPlanEntry>,
) {
    let mut candidates_by_tid = BTreeMap::<u32, BTreeSet<u32>>::new();
    let mut order_by_domain = BTreeMap::<u32, Vec<(usize, usize, u32)>>::new();
    for (&tid, entry) in entries.iter() {
        let candidates = candidate_cpus_for_plan_target(snapshot, tid, entry.target_domain);
        if candidates.is_empty() {
            continue;
        }
        let allowed_count = snapshot
            .allowed_cpus
            .get(&tid)
            .map(|cpus| cpus.len())
            .unwrap_or(usize::MAX);
        order_by_domain
            .entry(entry.target_domain)
            .or_default()
            .push((candidates.len(), allowed_count, tid));
        candidates_by_tid.insert(tid, candidates);
    }

    let mut cpu_loads = BTreeMap::<u32, Vec<u32>>::new();
    for (_domain, mut order) in order_by_domain {
        order.sort_by_key(|(candidate_count, allowed_count, tid)| {
            (*candidate_count, *allowed_count, *tid)
        });

        for (_, _, tid) in order {
            let Some(entry) = entries.get(&tid) else {
                continue;
            };
            let target_domain = entry.target_domain;
            let Some(candidates) = candidates_by_tid.get(&tid) else {
                continue;
            };
            let previous_cpu = snapshot
                .previous_plan
                .as_ref()
                .and_then(|plan| plan.entries.get(&tid))
                .and_then(|entry| entry.target_cpu)
                .filter(|cpu| candidates.contains(cpu));
            let recent_cpu = snapshot
                .threads
                .get(&tid)
                .and_then(thread_cpu)
                .filter(|cpu| candidates.contains(cpu));
            let min_candidate_load = candidates
                .iter()
                .copied()
                .map(|cpu| plan_cpu_load(&cpu_loads, cpu))
                .min()
                .unwrap_or(0);
            let selected_cpu = previous_cpu
                .or_else(|| {
                    recent_cpu.filter(|cpu| plan_cpu_load(&cpu_loads, *cpu) <= min_candidate_load)
                })
                .or_else(|| {
                    candidates
                        .iter()
                        .copied()
                        .min_by_key(|cpu| (plan_cpu_load(&cpu_loads, *cpu), *cpu))
                });
            let Some(selected_cpu) = selected_cpu else {
                continue;
            };
            if let Some(entry) = entries.get_mut(&tid) {
                entry.target_cpu = legal_plan_cpu(snapshot, tid, target_domain, Some(selected_cpu));
            }
            if active_for_cpu_plan(snapshot, tid) {
                cpu_loads.entry(selected_cpu).or_default().push(tid);
            }
        }
    }

    repair_duplicate_plan_target_cpus(snapshot, entries, &candidates_by_tid, &mut cpu_loads);
}

fn repair_duplicate_plan_target_cpus(
    snapshot: &PlannerInput,
    entries: &mut BTreeMap<u32, PlacementPlanEntry>,
    candidates_by_tid: &BTreeMap<u32, BTreeSet<u32>>,
    cpu_loads: &mut BTreeMap<u32, Vec<u32>>,
) {
    let target_domains = entries
        .values()
        .map(|entry| entry.target_domain)
        .collect::<BTreeSet<_>>();

    for domain in target_domains {
        loop {
            let idle_cpus = entries
                .iter()
                .filter(|(_, entry)| entry.target_domain == domain)
                .filter_map(|(tid, _)| candidates_by_tid.get(tid))
                .flat_map(|candidates| candidates.iter().copied())
                .filter(|cpu| plan_cpu_load(cpu_loads, *cpu) == 0)
                .collect::<BTreeSet<_>>();
            if idle_cpus.is_empty() {
                break;
            }

            let duplicate_cpus = cpu_loads
                .iter()
                .filter(|(cpu, tids)| {
                    tids.len() > 1
                        && snapshot
                            .topo
                            .cpu_to_domain
                            .get(**cpu as usize)
                            .copied()
                            .flatten()
                            == Some(domain)
                })
                .map(|(cpu, _)| *cpu)
                .collect::<Vec<_>>();
            if duplicate_cpus.is_empty() {
                break;
            }

            let mut repair = None;
            'find_repair: for idle_cpu in idle_cpus {
                for duplicate_cpu in &duplicate_cpus {
                    let mut movable_tids = cpu_loads
                        .get(duplicate_cpu)
                        .into_iter()
                        .flat_map(|tids| tids.iter().copied())
                        .filter(|tid| {
                            candidates_by_tid
                                .get(tid)
                                .map(|candidates| candidates.contains(&idle_cpu))
                                .unwrap_or(false)
                        })
                        .collect::<Vec<_>>();
                    movable_tids.sort_by_key(|tid| {
                        let recent_cpu = snapshot.threads.get(tid).and_then(thread_cpu);
                        (
                            recent_cpu != Some(idle_cpu),
                            recent_cpu == Some(*duplicate_cpu),
                            *tid,
                        )
                    });
                    if let Some(tid) = movable_tids.first().copied() {
                        repair = Some((*duplicate_cpu, idle_cpu, tid));
                        break 'find_repair;
                    }
                }
            }

            let Some((duplicate_cpu, idle_cpu, tid)) = repair else {
                break;
            };
            if let Some(entry) = entries.get_mut(&tid) {
                entry.target_cpu = Some(idle_cpu);
            }
            if let Some(tids) = cpu_loads.get_mut(&duplicate_cpu) {
                tids.retain(|candidate_tid| *candidate_tid != tid);
            }
            cpu_loads.entry(idle_cpu).or_default().push(tid);
        }
    }
}

fn affected_tids(
    members: &BTreeMap<u32, MemberView>,
    snapshot: &PlannerInput,
    scope: &PlannerScope,
) -> BTreeSet<u32> {
    let touched_tids = &scope.touched_tids;
    let mut touched_domains = scope.touched_domains.clone();
    for tid in touched_tids {
        if let Some(domain) = members.get(tid).and_then(|member| member.current_domain) {
            touched_domains.insert(domain);
        }
    }
    let mut affected = BTreeSet::<u32>::new();
    for tid in touched_tids {
        affected.insert(*tid);
        if let Some(member) = members.get(tid) {
            for peer in members.values() {
                if peer.active && peer.tgid == member.tgid && peer.sync_qualified {
                    affected.insert(peer.tid);
                }
            }
        }
    }
    for member in members.values() {
        if member.active
            && member
                .current_domain
                .is_some_and(|domain| touched_domains.contains(&domain))
        {
            affected.insert(member.tid);
        }
    }
    if affected.is_empty() {
        affected.extend(snapshot.pending_tids.iter().copied());
    }
    affected
}

#[derive(Clone, Debug)]
#[cfg(feature = "scheduler-paper-greedy")]
struct PaperGreedyTotals {
    assigned_df_x100: Vec<u32>,
    assigned_llc_x100: Vec<u32>,
    assigned_active_task_count: Vec<u32>,
}

#[derive(Clone, Copy, Debug)]
#[cfg(feature = "scheduler-paper-greedy")]
struct PaperGreedyDomainMetrics {
    after_df_x100: u32,
    residual_before_x100: u32,
    df_overload_after_x100: u32,
    after_llc_x100: u32,
    slot_extra: u32,
}

#[cfg(feature = "scheduler-paper-greedy")]
fn apply_paper_greedy_seed(snapshot: &PlannerInput, state: &mut PlannerState, was_global: bool) {
    if !was_global || state.items.is_empty() || state.assignments.len() != state.items.len() {
        return;
    }

    let domain_count = state.domains.len();
    let external_df_x100 = state
        .domains
        .iter()
        .map(|domain| {
            domain
                .live_df_x100
                .saturating_sub(domain.current_managed_df_x100)
        })
        .collect::<Vec<_>>();
    let external_llc_x100 = state
        .domains
        .iter()
        .map(|domain| {
            domain
                .live_llc_x100
                .saturating_sub(domain.current_managed_llc_x100)
        })
        .collect::<Vec<_>>();
    let external_active_task_count = state
        .domains
        .iter()
        .map(|domain| {
            domain
                .current_active_task_count
                .saturating_sub(domain.current_item_active_task_count)
        })
        .collect::<Vec<_>>();
    if external_df_x100.len() != domain_count
        || external_llc_x100.len() != domain_count
        || external_active_task_count.len() != domain_count
    {
        return;
    }

    let mut totals = PaperGreedyTotals {
        assigned_df_x100: vec![0; domain_count],
        assigned_llc_x100: vec![0; domain_count],
        assigned_active_task_count: vec![0; domain_count],
    };
    for item_index in 0..state.items.len() {
        add_item_assignment_to_paper_totals(
            &mut totals,
            state,
            item_index,
            state.assignments[item_index],
        );
    }

    let mut item_order = (0..state.items.len())
        .filter_map(|item_index| {
            let demand_df_x100 = item_df_demand_x100(item_index, state);
            (demand_df_x100 > 0).then_some((item_index, demand_df_x100))
        })
        .collect::<Vec<_>>();
    item_order.sort_by(|(left_index, left_demand), (right_index, right_demand)| {
        right_demand
            .cmp(left_demand)
            .then_with(|| left_index.cmp(right_index))
    });

    for (item_index, _) in item_order {
        let previous_assignment = state.assignments[item_index];
        remove_item_assignment_from_paper_totals(
            &mut totals,
            state,
            item_index,
            previous_assignment,
        );
        let Some(target_domain) = choose_paper_greedy_domain(
            snapshot,
            state,
            &totals,
            &external_df_x100,
            &external_llc_x100,
            &external_active_task_count,
            item_index,
        ) else {
            add_item_assignment_to_paper_totals(
                &mut totals,
                state,
                item_index,
                previous_assignment,
            );
            continue;
        };
        let next_assignment = Assignment::Domain(target_domain);
        state.assignments[item_index] = next_assignment;
        add_item_assignment_to_paper_totals(&mut totals, state, item_index, next_assignment);
    }
}

#[cfg(feature = "scheduler-paper-greedy")]
fn item_df_demand_x100(item_index: usize, state: &PlannerState) -> u32 {
    state
        .items
        .get(item_index)
        .map(|item| {
            item.member_tids
                .iter()
                .filter_map(|tid| state.members.get(tid))
                .fold(0u32, |acc, member| {
                    acc.saturating_add(member.effect_df_x100)
                })
        })
        .unwrap_or(0)
}

#[cfg(feature = "scheduler-paper-greedy")]
fn item_llc_demand_x100(item_index: usize, state: &PlannerState) -> u32 {
    state
        .items
        .get(item_index)
        .map(|item| {
            item.member_tids
                .iter()
                .filter_map(|tid| state.members.get(tid))
                .fold(0u32, |acc, member| {
                    acc.saturating_add(member.effect_llc_x100)
                })
        })
        .unwrap_or(0)
}

#[cfg(feature = "scheduler-paper-greedy")]
fn item_current_domains(item_index: usize, state: &PlannerState) -> BTreeSet<u32> {
    state
        .items
        .get(item_index)
        .map(|item| {
            item.member_tids
                .iter()
                .filter_map(|tid| state.members.get(tid))
                .filter_map(|member| member.current_domain)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

#[cfg(feature = "scheduler-paper-greedy")]
fn update_item_assignment_in_paper_totals(
    totals: &mut PaperGreedyTotals,
    state: &PlannerState,
    item_index: usize,
    assignment: Assignment,
    add: bool,
) {
    let Some(item) = state.items.get(item_index) else {
        return;
    };
    for tid in &item.member_tids {
        let Some(member) = state.members.get(tid) else {
            continue;
        };
        let target_domain = target_domain_for_home(assignment, member.current_domain);
        let Some(domain) = target_domain else {
            continue;
        };
        let domain_index = domain as usize;
        if domain_index >= totals.assigned_df_x100.len() {
            continue;
        }
        if add {
            totals.assigned_df_x100[domain_index] =
                totals.assigned_df_x100[domain_index].saturating_add(member.effect_df_x100);
            totals.assigned_llc_x100[domain_index] =
                totals.assigned_llc_x100[domain_index].saturating_add(member.effect_llc_x100);
            if member.active {
                totals.assigned_active_task_count[domain_index] =
                    totals.assigned_active_task_count[domain_index].saturating_add(1);
            }
        } else {
            totals.assigned_df_x100[domain_index] =
                totals.assigned_df_x100[domain_index].saturating_sub(member.effect_df_x100);
            totals.assigned_llc_x100[domain_index] =
                totals.assigned_llc_x100[domain_index].saturating_sub(member.effect_llc_x100);
            if member.active {
                totals.assigned_active_task_count[domain_index] =
                    totals.assigned_active_task_count[domain_index].saturating_sub(1);
            }
        }
    }
}

#[cfg(feature = "scheduler-paper-greedy")]
fn add_item_assignment_to_paper_totals(
    totals: &mut PaperGreedyTotals,
    state: &PlannerState,
    item_index: usize,
    assignment: Assignment,
) {
    update_item_assignment_in_paper_totals(totals, state, item_index, assignment, true);
}

#[cfg(feature = "scheduler-paper-greedy")]
fn remove_item_assignment_from_paper_totals(
    totals: &mut PaperGreedyTotals,
    state: &PlannerState,
    item_index: usize,
    assignment: Assignment,
) {
    update_item_assignment_in_paper_totals(totals, state, item_index, assignment, false);
}

#[cfg(feature = "scheduler-paper-greedy")]
fn choose_paper_greedy_domain(
    snapshot: &PlannerInput,
    state: &PlannerState,
    totals: &PaperGreedyTotals,
    external_df_x100: &[u32],
    external_llc_x100: &[u32],
    external_active_task_count: &[u32],
    item_index: usize,
) -> Option<u32> {
    let item = state.items.get(item_index)?;
    let source_domains = item_current_domains(item_index, state);

    let legal_domains = item
        .allowed_domains
        .iter()
        .copied()
        .filter(|domain| {
            candidate_allowed(item_index, Assignment::Domain(*domain), snapshot, state)
        })
        .collect::<Vec<_>>();
    if legal_domains.is_empty() {
        return None;
    }
    if let Some(stay_domain) = paper_greedy_under_subscribed_stay_domain(
        state,
        totals,
        external_df_x100,
        external_llc_x100,
        external_active_task_count,
        item_index,
        &legal_domains,
    ) {
        return Some(stay_domain);
    }

    legal_domains
        .into_iter()
        .filter_map(|domain| {
            let metrics = paper_greedy_domain_metrics(
                state,
                totals,
                external_df_x100,
                external_llc_x100,
                external_active_task_count,
                item_index,
                domain,
            )?;
            let current_domain_penalty = u32::from(!source_domains.contains(&domain));
            let key = (
                metrics.slot_extra,
                metrics.df_overload_after_x100,
                std::cmp::Reverse(metrics.residual_before_x100),
                metrics.after_llc_x100,
                current_domain_penalty,
                domain,
            );
            Some((key, domain))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, domain)| domain)
}

#[cfg(feature = "scheduler-paper-greedy")]
fn paper_greedy_domain_metrics(
    state: &PlannerState,
    totals: &PaperGreedyTotals,
    external_df_x100: &[u32],
    external_llc_x100: &[u32],
    _external_active_task_count: &[u32],
    item_index: usize,
    domain: u32,
) -> Option<PaperGreedyDomainMetrics> {
    let domain_index = domain as usize;
    let item_df_x100 = item_df_demand_x100(item_index, state);
    let item_llc_x100 = item_llc_demand_x100(item_index, state);
    let before_df_x100 = external_df_x100
        .get(domain_index)
        .copied()
        .unwrap_or(0)
        .saturating_add(
            totals
                .assigned_df_x100
                .get(domain_index)
                .copied()
                .unwrap_or(0),
        );
    let after_df_x100 = before_df_x100.saturating_add(item_df_x100);
    let df_capacity_mib_s_x100 = state
        .domains
        .get(domain_index)
        .map(|model| model.df_capacity_mib_s_x100)?;
    let residual_before_x100 = df_capacity_mib_s_x100.saturating_sub(before_df_x100);
    let slot_extra =
        paper_greedy_candidate_slot_constraint(state, item_index, domain).extra_tasks();
    let after_llc_x100 = external_llc_x100
        .get(domain_index)
        .copied()
        .unwrap_or(0)
        .saturating_add(
            totals
                .assigned_llc_x100
                .get(domain_index)
                .copied()
                .unwrap_or(0),
        )
        .saturating_add(item_llc_x100)
        .min(10_000);

    Some(PaperGreedyDomainMetrics {
        after_df_x100,
        residual_before_x100,
        df_overload_after_x100: after_df_x100.saturating_sub(df_capacity_mib_s_x100),
        after_llc_x100,
        slot_extra,
    })
}

#[cfg(feature = "scheduler-paper-greedy")]
fn paper_greedy_candidate_slot_constraint(
    state: &PlannerState,
    item_index: usize,
    candidate_domain: u32,
) -> SlotCapacityConstraint {
    let domain_count = state.domains.len();
    let mut predicted_cpu_sets = vec![BTreeSet::<u32>::new(); domain_count];
    let mut predicted_affinity_groups = vec![BTreeMap::<Vec<u32>, u32>::new(); domain_count];

    for member in state.members.values() {
        if !member.active {
            continue;
        }
        let target_domain = state
            .item_index_by_tid
            .get(&member.tid)
            .and_then(|index| {
                if *index == item_index {
                    Some(candidate_domain)
                } else {
                    state.assignments.get(*index).and_then(|assignment| {
                        target_domain_for_home(*assignment, member.current_domain)
                    })
                }
            })
            .or(member.baseline_domain);
        add_predicted_member_cpu_slots(
            member,
            target_domain,
            &mut predicted_cpu_sets,
            &mut predicted_affinity_groups,
        );
    }

    let domain_index = candidate_domain as usize;
    let Some(cpu_set) = predicted_cpu_sets.get(domain_index) else {
        return SlotCapacityConstraint::WithinCapacity;
    };
    let Some(groups) = predicted_affinity_groups.get(domain_index) else {
        return SlotCapacityConstraint::WithinCapacity;
    };
    let task_count = groups.values().copied().sum::<u32>();
    let total_slot_constraint = task_slot_constraint(task_count, cpu_set.len() as u32);
    let affinity_slot_constraint = affinity_group_slot_constraint(groups.clone());
    total_slot_constraint.max(affinity_slot_constraint)
}

#[cfg(feature = "scheduler-paper-greedy")]
fn paper_greedy_under_subscribed_stay_domain(
    state: &PlannerState,
    totals: &PaperGreedyTotals,
    external_df_x100: &[u32],
    external_llc_x100: &[u32],
    external_active_task_count: &[u32],
    item_index: usize,
    legal_domains: &[u32],
) -> Option<u32> {
    let source_domains = item_current_domains(item_index, state);
    if source_domains.len() != 1 {
        return None;
    }
    let source_domain = source_domains.iter().next().copied()?;
    if !legal_domains.contains(&source_domain) {
        return None;
    }
    if state
        .domains
        .get(source_domain as usize)
        .map(|model| model.live_df_x100 > model.df_capacity_mib_s_x100)
        .unwrap_or(true)
    {
        return None;
    }
    let source_metrics = paper_greedy_domain_metrics(
        state,
        totals,
        external_df_x100,
        external_llc_x100,
        external_active_task_count,
        item_index,
        source_domain,
    )?;
    state
        .domains
        .get(source_domain as usize)
        .filter(|model| source_metrics.after_df_x100 <= model.df_capacity_mib_s_x100)
        .filter(|_| source_metrics.slot_extra == 0)
        .map(|_| source_domain)
}

fn optimize_state(
    snapshot: &PlannerInput,
    config: &PlannerConfig,
    state: &mut PlannerState,
    was_global: bool,
) {
    for _ in 0..config.max_passes {
        let baseline = evaluate_plan(state);
        let mut best_move: Option<(usize, Assignment, PlannerCostBreakdown)> = None;
        for item_index in 0..state.items.len() {
            let item = &state.items[item_index];
            let current_assignment = state.assignments[item_index];
            for target_domain in &item.allowed_domains {
                let candidate_assignment = Assignment::Domain(*target_domain);
                if candidate_assignment == current_assignment {
                    continue;
                }
                #[cfg(feature = "scheduler-paper-greedy")]
                if paper_greedy_blocks_under_subscribed_soft_move(
                    item_index,
                    *target_domain,
                    snapshot,
                    state,
                ) {
                    continue;
                }
                if !candidate_allowed(item_index, candidate_assignment, snapshot, state) {
                    continue;
                }
                state.assignments[item_index] = candidate_assignment;
                let candidate_cost = evaluate_plan(state);
                state.assignments[item_index] = current_assignment;
                if !improvement_clears_margin(
                    baseline.cost,
                    candidate_cost.cost,
                    item_index,
                    state,
                    snapshot,
                ) {
                    continue;
                }
                match best_move {
                    None => {
                        best_move = Some((item_index, candidate_assignment, candidate_cost.cost))
                    }
                    Some((_, _, best_cost)) if candidate_cost.cost.is_better_than(best_cost) => {
                        best_move = Some((item_index, candidate_assignment, candidate_cost.cost))
                    }
                    _ => {}
                }
            }
            if !was_global {
                let current_home_allowed =
                    candidate_allowed(item_index, Assignment::CurrentHome, snapshot, state);
                if current_home_allowed {
                    let candidate_assignment = Assignment::CurrentHome;
                    if candidate_assignment != current_assignment {
                        state.assignments[item_index] = candidate_assignment;
                        let candidate_cost = evaluate_plan(state);
                        state.assignments[item_index] = current_assignment;
                        if improvement_clears_margin(
                            baseline.cost,
                            candidate_cost.cost,
                            item_index,
                            state,
                            snapshot,
                        ) {
                            match best_move {
                                None => {
                                    best_move = Some((
                                        item_index,
                                        candidate_assignment,
                                        candidate_cost.cost,
                                    ))
                                }
                                Some((_, _, best_cost))
                                    if candidate_cost.cost.is_better_than(best_cost) =>
                                {
                                    best_move = Some((
                                        item_index,
                                        candidate_assignment,
                                        candidate_cost.cost,
                                    ))
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        let Some((item_index, assignment, _)) = best_move else {
            break;
        };
        state.assignments[item_index] = assignment;
    }

    let mut heaviest = state
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let weight = item
                .member_tids
                .iter()
                .filter_map(|tid| state.members.get(tid))
                .fold(0u32, |acc, member| {
                    acc.saturating_add(member.effect_df_x100)
                });
            (index, weight)
        })
        .collect::<Vec<_>>();
    heaviest.sort_by_key(|(_, weight)| std::cmp::Reverse(*weight));
    heaviest.truncate(config.swap_pass_items);
    let baseline = evaluate_plan(state);
    let mut best_swap: Option<(usize, usize, Assignment, Assignment, PlannerCostBreakdown)> = None;
    for left in 0..heaviest.len() {
        for right in (left + 1)..heaviest.len() {
            let left_index = heaviest[left].0;
            let right_index = heaviest[right].0;
            let left_assignment = state.assignments[left_index];
            let right_assignment = state.assignments[right_index];
            let (Assignment::Domain(left_domain), Assignment::Domain(right_domain)) =
                (left_assignment, right_assignment)
            else {
                continue;
            };
            if left_domain == right_domain {
                continue;
            }
            if !candidate_allowed(
                left_index,
                Assignment::Domain(right_domain),
                snapshot,
                state,
            ) || !candidate_allowed(
                right_index,
                Assignment::Domain(left_domain),
                snapshot,
                state,
            ) {
                continue;
            }
            state.assignments[left_index] = Assignment::Domain(right_domain);
            state.assignments[right_index] = Assignment::Domain(left_domain);
            let candidate = evaluate_plan(state);
            state.assignments[left_index] = left_assignment;
            state.assignments[right_index] = right_assignment;
            if !candidate.cost.is_better_than(baseline.cost) {
                continue;
            }
            if snapshot.previous_plan.is_some()
                && candidate.cost.hard_constraints() >= baseline.cost.hard_constraints()
            {
                continue;
            }
            match best_swap {
                None => {
                    best_swap = Some((
                        left_index,
                        right_index,
                        Assignment::Domain(right_domain),
                        Assignment::Domain(left_domain),
                        candidate.cost,
                    ))
                }
                Some((_, _, _, _, best_cost)) if candidate.cost.is_better_than(best_cost) => {
                    best_swap = Some((
                        left_index,
                        right_index,
                        Assignment::Domain(right_domain),
                        Assignment::Domain(left_domain),
                        candidate.cost,
                    ))
                }
                _ => {}
            }
        }
    }
    if let Some((left_index, right_index, left_assignment, right_assignment, _)) = best_swap {
        state.assignments[left_index] = left_assignment;
        state.assignments[right_index] = right_assignment;
    }
}

#[cfg(feature = "scheduler-paper-greedy")]
fn paper_greedy_blocks_under_subscribed_soft_move(
    item_index: usize,
    target_domain: u32,
    snapshot: &PlannerInput,
    state: &PlannerState,
) -> bool {
    let source_domains = item_current_domains(item_index, state);
    if source_domains.len() != 1 {
        return false;
    }
    let Some(source_domain) = source_domains.iter().next().copied() else {
        return false;
    };
    if target_domain == source_domain {
        return false;
    }
    match state.assignments.get(item_index).copied() {
        Some(Assignment::CurrentHome) => {}
        Some(Assignment::Domain(domain)) if domain == source_domain => {}
        _ => return false,
    }
    if state
        .domains
        .get(source_domain as usize)
        .map(|model| model.live_df_x100 > model.df_capacity_mib_s_x100)
        .unwrap_or(true)
    {
        return false;
    }
    if !candidate_allowed(
        item_index,
        Assignment::Domain(source_domain),
        snapshot,
        state,
    ) {
        return false;
    }

    let baseline = evaluate_plan(state);
    if baseline
        .cost
        .hard_constraints
        .slot_over_capacity
        .extra_tasks()
        > 0
        || baseline.cost.hard_constraints.migration_budget_violations > 0
        || baseline.cost.hard_constraints.sync_split_violations > 0
    {
        return false;
    }
    baseline
        .domains
        .get(source_domain as usize)
        .map(|source| {
            state
                .domains
                .get(source_domain as usize)
                .map(|model| source.predicted_df_x100 <= model.df_capacity_mib_s_x100)
                .unwrap_or(false)
                && source
                    .cost
                    .hard_constraints
                    .slot_over_capacity
                    .extra_tasks()
                    == 0
        })
        .unwrap_or(false)
}

fn affected_domain_count(state: &PlannerState) -> usize {
    let mut domains = BTreeSet::<u32>::new();
    for item in &state.items {
        for tid in &item.member_tids {
            if let Some(domain) = state
                .members
                .get(tid)
                .and_then(|member| member.current_domain)
            {
                domains.insert(domain);
            }
        }
    }
    domains.len()
}

fn candidate_allowed(
    item_index: usize,
    assignment: Assignment,
    snapshot: &PlannerInput,
    state: &PlannerState,
) -> bool {
    let Some(item) = state.items.get(item_index) else {
        return false;
    };
    match assignment {
        Assignment::CurrentHome => item.member_tids.iter().all(|tid| {
            state
                .members
                .get(tid)
                .and_then(|member| member.current_domain)
                .is_some()
        }),
        Assignment::Domain(target_domain) => {
            item.allowed_domains.contains(&target_domain)
                && item.member_tids.iter().all(|tid| {
                    let Some(member) = state.members.get(tid) else {
                        return false;
                    };
                    let current_domain = member.current_domain.unwrap_or(target_domain);
                    current_domain == target_domain
                        || move_allowed(member, current_domain, target_domain, snapshot)
                })
        }
    }
}

fn target_domain_for_home(assignment: Assignment, current_domain: Option<u32>) -> Option<u32> {
    match assignment {
        Assignment::CurrentHome => current_domain,
        Assignment::Domain(domain) => Some(domain),
    }
}

fn move_allowed(
    member: &MemberView,
    current_domain: u32,
    target_domain: u32,
    snapshot: &PlannerInput,
) -> bool {
    if current_domain == target_domain {
        return true;
    }
    if !member.allowed_domains.contains(&target_domain) {
        return false;
    }
    let current_excluded = snapshot.mapping.excluded_domains.contains(&current_domain);
    if !current_excluded {
        let Some(thread) = snapshot.threads.get(&member.tid) else {
            return false;
        };
        if snapshot.now_ns < thread.settle_until_ns {
            return false;
        }
        if snapshot.now_ns < thread.reverse_protect_until_ns
            && thread.last_migration_to_domain == Some(current_domain)
            && thread.last_migration_from_domain == Some(target_domain)
        {
            return false;
        }
    }
    true
}

fn improvement_clears_margin(
    baseline_cost: PlannerCostBreakdown,
    candidate_cost: PlannerCostBreakdown,
    item_index: usize,
    state: &PlannerState,
    snapshot: &PlannerInput,
) -> bool {
    if !candidate_cost.is_better_than(baseline_cost) {
        return false;
    }
    let Some(item) = state.items.get(item_index) else {
        return false;
    };
    let max_live_source = item
        .member_tids
        .iter()
        .filter_map(|tid| state.members.get(tid))
        .filter_map(|member| member.current_domain)
        .map(|domain| {
            let model = state.domains.get(domain as usize)?;
            Some((model.live_df_x100, model.df_capacity_mib_s_x100))
        })
        .flatten()
        .max_by_key(|(live_df_x100, capacity_mib_s_x100)| {
            (u64::from(*live_df_x100) * 10_000) / u64::from((*capacity_mib_s_x100).max(1))
        })
        .unwrap_or_else(|| {
            (
                0,
                state
                    .domains
                    .iter()
                    .map(|domain| domain.df_capacity_mib_s_x100)
                    .max()
                    .unwrap_or(1),
            )
        });
    let any_excluded = item.member_tids.iter().any(|tid| {
        state
            .members
            .get(tid)
            .and_then(|member| member.current_domain)
            .is_some_and(|domain| snapshot.mapping.excluded_domains.contains(&domain))
    });
    if any_excluded {
        return true;
    }
    let margin_x100 = effective_migrate_margin_x100(
        max_live_source.0,
        snapshot.policy.migrate_margin_x100,
        max_live_source.1,
    );
    candidate_cost.clears_margin_against(baseline_cost, margin_x100)
}

fn collect_assignment_totals(state: &PlannerState) -> AssignmentTotals {
    let domain_count = state.domains.len();
    let mut assigned_df_x100 = vec![0u32; domain_count];
    let mut assigned_llc_x100 = vec![0u32; domain_count];
    let mut assigned_active_task_count = vec![0u32; domain_count];
    let mut predicted_cpu_sets = vec![BTreeSet::<u32>::new(); domain_count];
    let mut predicted_affinity_groups = vec![BTreeMap::<Vec<u32>, u32>::new(); domain_count];
    let mut migrations_in = vec![0u32; domain_count];
    let mut migrations_out = vec![0u32; domain_count];

    for (item_index, item) in state.items.iter().enumerate() {
        let assignment = state.assignments[item_index];
        for tid in &item.member_tids {
            let Some(member) = state.members.get(tid) else {
                continue;
            };
            let current_domain = member.current_domain;
            let Some(target_domain) = target_domain_for_home(assignment, current_domain) else {
                continue;
            };
            if member.active {
                if let Some(value) = assigned_active_task_count.get_mut(target_domain as usize) {
                    *value = value.saturating_add(1);
                }
            }
            if let Some(value) = assigned_df_x100.get_mut(target_domain as usize) {
                *value = value.saturating_add(member.effect_df_x100);
            }
            if let Some(value) = assigned_llc_x100.get_mut(target_domain as usize) {
                *value = value.saturating_add(member.effect_llc_x100);
            }
            if current_domain != Some(target_domain) {
                if let Some(source_domain) = current_domain {
                    if let Some(value) = migrations_out.get_mut(source_domain as usize) {
                        *value = value.saturating_add(1);
                    }
                }
                if let Some(value) = migrations_in.get_mut(target_domain as usize) {
                    *value = value.saturating_add(1);
                }
            }
        }
    }

    for member in state.members.values() {
        if !member.active {
            continue;
        }
        let target_domain = state
            .item_index_by_tid
            .get(&member.tid)
            .and_then(|item_index| {
                state.assignments.get(*item_index).and_then(|assignment| {
                    target_domain_for_home(*assignment, member.current_domain)
                })
            })
            .or(member.baseline_domain);
        add_predicted_member_cpu_slots(
            member,
            target_domain,
            &mut predicted_cpu_sets,
            &mut predicted_affinity_groups,
        );
    }
    let predicted_cpu_capacity = predicted_cpu_sets
        .into_iter()
        .map(|cpus| cpus.len() as u32)
        .collect::<Vec<_>>();
    let affinity_slot_constraint = predicted_affinity_groups
        .into_iter()
        .map(affinity_group_slot_constraint)
        .collect::<Vec<_>>();

    AssignmentTotals {
        assigned_df_x100,
        assigned_llc_x100,
        assigned_active_task_count,
        predicted_cpu_capacity,
        affinity_slot_constraint,
        migrations_in,
        migrations_out,
    }
}

fn add_predicted_member_cpu_slots(
    member: &MemberView,
    target_domain: Option<u32>,
    predicted_cpu_sets: &mut [BTreeSet<u32>],
    predicted_affinity_groups: &mut [BTreeMap<Vec<u32>, u32>],
) {
    let Some(target_domain) = target_domain else {
        return;
    };
    let domain_index = target_domain as usize;
    if domain_index >= predicted_cpu_sets.len() || domain_index >= predicted_affinity_groups.len() {
        return;
    }
    let cpus = member
        .allowed_cpus_by_domain
        .get(&target_domain)
        .cloned()
        .unwrap_or_default();
    predicted_cpu_sets[domain_index].extend(cpus.iter().copied());
    let key = cpus.into_iter().collect::<Vec<_>>();
    *predicted_affinity_groups[domain_index]
        .entry(key)
        .or_default() += 1;
}

fn affinity_group_slot_constraint(groups: BTreeMap<Vec<u32>, u32>) -> SlotCapacityConstraint {
    groups
        .into_iter()
        .map(|(cpus, task_count)| task_slot_constraint(task_count, cpus.len() as u32))
        .fold(SlotCapacityConstraint::WithinCapacity, |acc, constraint| {
            acc.combine(constraint)
        })
}

fn evaluate_plan(state: &PlannerState) -> PlanEvaluation {
    let totals = collect_assignment_totals(state);
    let domain_count = state.domains.len();
    let mut cost = PlannerCostBreakdown::default();
    let mut domains = Vec::<DomainScore>::with_capacity(domain_count);
    let mut violations = ConstraintViolations::default();

    for domain in 0..domain_count {
        let domain_model = &state.domains[domain];
        let non_scoped_current_df = domain_model
            .current_managed_df_x100
            .saturating_sub(domain_model.current_item_df_x100);
        let predicted_managed_df =
            non_scoped_current_df.saturating_add(totals.assigned_df_x100[domain]);
        let predicted_df = domain_model
            .live_df_x100
            .saturating_add(totals.assigned_df_x100[domain])
            .saturating_sub(domain_model.current_item_df_x100);
        let predicted_llc = domain_model
            .live_llc_x100
            .saturating_add(totals.assigned_llc_x100[domain])
            .saturating_sub(domain_model.current_item_llc_x100)
            .min(10_000);
        let predicted_active_task_count = domain_model
            .current_active_task_count
            .saturating_add(totals.assigned_active_task_count[domain])
            .saturating_sub(domain_model.current_item_active_task_count);
        let overload =
            overload_x100_with_capacity(predicted_df, domain_model.df_capacity_mib_s_x100);
        let capacity = domain_model.df_capacity_mib_s_x100.max(1);
        let balance_weight_x100 =
            ((u64::from(predicted_df.min(capacity)) * 10_000) / u64::from(capacity)) as u64;
        let balance_penalty = domain_signature_balance_penalty(predicted_managed_df);
        let balance_cost = balance_penalty.saturating_mul(balance_weight_x100) / 10_000;
        let cpu_capacity = totals
            .predicted_cpu_capacity
            .get(domain)
            .copied()
            .unwrap_or(domain_model.cpu_capacity);
        let total_slot_constraint = task_slot_constraint(predicted_active_task_count, cpu_capacity);
        let affinity_slot_constraint = totals
            .affinity_slot_constraint
            .get(domain)
            .copied()
            .unwrap_or_default();
        let slot_constraint = total_slot_constraint.max(affinity_slot_constraint);
        let llc_cost = u64::from(predicted_llc).saturating_mul(u64::from(predicted_llc));
        let df_overload_cost = overload_cost(overload, overload);
        let migration_penalty = totals.migrations_in[domain] > domain_model.migration_budget
            || totals.migrations_out[domain] > domain_model.migration_budget;
        if migration_penalty {
            violations.migration_budget_domains.push(domain as u32);
        }
        let domain_cost = PlannerCostBreakdown {
            df_overload_cost,
            llc_pressure_cost: llc_cost,
            signature_balance_cost: balance_cost,
            hard_constraints: PlannerHardConstraints {
                slot_over_capacity: slot_constraint,
                migration_budget_violations: u32::from(migration_penalty),
                sync_split_violations: 0,
            },
        };
        cost.add_saturating(domain_cost);
        domains.push(DomainScore {
            predicted_df_x100: predicted_df,
            predicted_llc_x100: predicted_llc,
            predicted_active_task_count,
            predicted_cpu_capacity: cpu_capacity,
            overload_x100: overload,
            cost: domain_cost,
            migrations_in: totals.migrations_in[domain],
            migrations_out: totals.migrations_out[domain],
            migration_budget: domain_model.migration_budget,
            migration_penalty,
        });
    }

    for constraint in &state.sync_constraints {
        match constraint {
            SyncConstraint::CoLocateGroup { tgid, tids } => {
                let domains_for_group = tids
                    .iter()
                    .filter_map(|tid| effective_member_domain(*tid, state))
                    .collect::<BTreeSet<_>>();
                if domains_for_group.len() > 1 {
                    violations.sync_group_splits.push(*tgid);
                    cost.hard_constraints.sync_split_violations = cost
                        .hard_constraints
                        .sync_split_violations
                        .saturating_add(1);
                }
            }
            SyncConstraint::ResidualPair { left, right } => {
                let left_domain = effective_member_domain(*left, state);
                let right_domain = effective_member_domain(*right, state);
                if left_domain.is_some() && right_domain.is_some() && left_domain != right_domain {
                    violations.residual_pair_splits.push((*left, *right));
                    cost.hard_constraints.sync_split_violations = cost
                        .hard_constraints
                        .sync_split_violations
                        .saturating_add(1);
                }
            }
        }
    }

    PlanEvaluation {
        cost,
        domains,
        violations,
    }
}

fn emit_plan_debug(
    logger: &mut PlannerMoveTraceLogger,
    snapshot: &PlannerInput,
    state: &PlannerState,
    triggers: &[PlannerTrigger],
    was_global: bool,
    plan_revision: u64,
    entries: &BTreeMap<u32, PlacementPlanEntry>,
) -> anyhow::Result<()> {
    if !logger.enabled() {
        return Ok(());
    }

    let totals = collect_assignment_totals(state);
    let evaluation = evaluate_plan(state);
    let mut plan_entries_by_domain = vec![0u32; state.domains.len()];
    for entry in entries.values() {
        if let Some(count) = plan_entries_by_domain.get_mut(entry.target_domain as usize) {
            *count = count.saturating_add(1);
        }
    }

    for (domain, domain_model) in state.domains.iter().enumerate() {
        let score = evaluation.domains.get(domain).copied().unwrap_or_default();
        let assigned_active_jobs = totals
            .assigned_active_task_count
            .get(domain)
            .copied()
            .unwrap_or(0);
        let assigned_df_x100 = totals.assigned_df_x100.get(domain).copied().unwrap_or(0);
        let assigned_llc_x100 = totals.assigned_llc_x100.get(domain).copied().unwrap_or(0);
        let plan_entries = plan_entries_by_domain.get(domain).copied().unwrap_or(0);
        let eligible = snapshot.mapping.eligible_domains.contains(&(domain as u32));
        let line = format!(
            "ts_ns={} planner_epoch={} plan_revision={} was_global={} kind=domain triggers={} domain={} eligible={} cpu_capacity={} current_active_jobs={} current_item_active_jobs={} assigned_active_jobs={} allocated_active_jobs={} plan_entries={} live_df_x100={} current_managed_df_x100={} assigned_df_x100={} predicted_df_x100={} df_overload_x100={} live_llc_x100={} current_managed_llc_x100={} assigned_llc_x100={} predicted_llc_x100={} migrations_in={} migrations_out={} migration_budget={} migration_penalty={} slot_extra={} domain_soft={} domain_hard_diag={} domain_total={} plan_soft={} plan_hard_diag={} plan_total={}\n",
            snapshot.now_ns,
            snapshot.latest_df_sweep_epoch,
            plan_revision,
            was_global,
            format_triggers(triggers),
            domain,
            eligible,
            score.predicted_cpu_capacity,
            domain_model.current_active_task_count,
            domain_model.current_item_active_task_count,
            assigned_active_jobs,
            score.predicted_active_task_count,
            plan_entries,
            domain_model.live_df_x100,
            domain_model.current_managed_df_x100,
            assigned_df_x100,
            score.predicted_df_x100,
            score.overload_x100,
            domain_model.live_llc_x100,
            domain_model.current_managed_llc_x100,
            assigned_llc_x100,
            score.predicted_llc_x100,
            score.migrations_in,
            score.migrations_out,
            score.migration_budget,
            score.migration_penalty,
            score.cost.hard_constraints.slot_over_capacity.extra_tasks(),
            score.cost.soft_cost(),
            score.cost.hard_constraint_diagnostic_cost(),
            score.cost.total_cost(),
            evaluation.cost.soft_cost(),
            evaluation.cost.hard_constraint_diagnostic_cost(),
            evaluation.cost.total_cost(),
        );
        logger.write_line(&line)?;
    }

    let trigger_summary = format_triggers(triggers);
    for (tid, entry) in entries {
        let Some(member) = state.members.get(tid) else {
            continue;
        };
        let allowed_cpus = snapshot
            .allowed_cpus
            .get(tid)
            .map(|cpus| format_u32_list(cpus.iter().copied()))
            .unwrap_or_else(|| "na".to_string());
        let previous_entry = snapshot
            .previous_plan
            .as_ref()
            .and_then(|plan| plan.entries.get(tid));
        let target_cpu_legal = entry.target_cpu.is_some_and(|cpu| {
            legal_plan_cpu(snapshot, *tid, entry.target_domain, Some(cpu)).is_some()
        });
        let target_domain_eligible = snapshot
            .mapping
            .eligible_domains
            .contains(&entry.target_domain);
        let line = format!(
            "ts_ns={} planner_epoch={} plan_revision={} was_global={} kind=entry triggers={} tid={} tgid={} active={} current_cpu={} current_domain={} baseline_domain={} previous_domain={} previous_cpu={} allowed_domains={} allowed_cpus={} target_domain={} target_cpu={} target_domain_eligible={} target_cpu_legal={} sync_group_id={} sync_anchor_domain={} sync_override={} effect_df_x100={} effect_llc_x100={}\n",
            snapshot.now_ns,
            snapshot.latest_df_sweep_epoch,
            plan_revision,
            was_global,
            trigger_summary,
            tid,
            member.tgid,
            member.active,
            format_optional_u32(
                snapshot
                    .threads
                    .get(tid)
                    .and_then(|thread| thread.last_observed_cpu),
            ),
            format_optional_u32(member.current_domain),
            format_optional_u32(member.baseline_domain),
            format_optional_u32(previous_entry.map(|entry| entry.target_domain)),
            format_optional_u32(previous_entry.and_then(|entry| entry.target_cpu)),
            format_u32_list(member.allowed_domains.iter().copied()),
            allowed_cpus,
            entry.target_domain,
            format_optional_u32(entry.target_cpu),
            target_domain_eligible,
            target_cpu_legal,
            format_optional_u32(entry.sync_group_id),
            format_optional_u32(entry.sync_anchor_domain),
            entry.sync_override,
            member.effect_df_x100,
            member.effect_llc_x100,
        );
        logger.write_line(&line)?;
    }

    Ok(())
}

fn emit_move_trace(
    logger: &mut PlannerMoveTraceLogger,
    snapshot: &PlannerInput,
    state: &mut PlannerState,
    triggers: &[PlannerTrigger],
    was_global: bool,
    plan_revision: u64,
) -> anyhow::Result<()> {
    if !logger.enabled() {
        return Ok(());
    }

    let chosen_eval = evaluate_plan(state);
    let chosen_total = chosen_eval.cost.total_cost();
    let _chosen_violation_count = chosen_eval.violations.count();
    for item_index in 0..state.items.len() {
        let Some(item) = state.items.get(item_index).cloned() else {
            continue;
        };
        let current_assignment = state.assignments[item_index];
        let Assignment::Domain(target_domain) = current_assignment else {
            continue;
        };
        let moved_members = item
            .member_tids
            .iter()
            .filter(|tid| {
                let current_domain = state
                    .members
                    .get(tid)
                    .and_then(|member| member.current_domain);
                current_domain != Some(target_domain)
            })
            .count();
        if moved_members == 0 {
            continue;
        }

        let unique_source_domains = item
            .member_tids
            .iter()
            .filter_map(|tid| {
                state
                    .members
                    .get(tid)
                    .and_then(|member| member.current_domain)
            })
            .collect::<BTreeSet<_>>();
        let source_domain = if unique_source_domains.len() == 1 {
            unique_source_domains.iter().copied().next()
        } else {
            None
        };

        let original_assignment = state.assignments[item_index];
        let mut home_total = None::<u64>;
        if candidate_allowed(item_index, Assignment::CurrentHome, snapshot, state) {
            state.assignments[item_index] = Assignment::CurrentHome;
            home_total = Some(evaluate_plan(state).cost.total_cost());
        }
        state.assignments[item_index] = original_assignment;

        let mut candidates = Vec::<String>::new();
        for domain in &item.allowed_domains {
            let candidate_assignment = Assignment::Domain(*domain);
            if !candidate_allowed(item_index, candidate_assignment, snapshot, state) {
                candidates.push(format!("{domain}[legal=false]"));
                continue;
            }
            state.assignments[item_index] = candidate_assignment;
            let candidate_eval = evaluate_plan(state);
            let candidate_total = candidate_eval.cost.total_cost();
            let target_summary = candidate_eval
                .domains
                .get(*domain as usize)
                .copied()
                .unwrap_or_default();
            let source_segment = if let Some(source_domain) = source_domain {
                let source_summary = candidate_eval
                    .domains
                    .get(source_domain as usize)
                    .copied()
                    .unwrap_or_default();
                format!(
                    " src_domain={source_domain} src_df={} src_llc={} src_tasks={} src_overload={} src_soft={} src_hard_diag={} src_slot_extra={} src_balance={} src_llc_cost={} src_df_overload_cost={} src_local={} src_mig_in={} src_mig_out={} src_budget={} src_mig_penalty={} src_mig_violations={}",
                    source_summary.predicted_df_x100,
                    source_summary.predicted_llc_x100,
                    source_summary.predicted_active_task_count,
                    source_summary.overload_x100,
                    source_summary.cost.soft_cost(),
                    source_summary.cost.hard_constraint_diagnostic_cost(),
                    source_summary.cost.hard_constraints.slot_over_capacity.extra_tasks(),
                    source_summary.cost.signature_balance_cost,
                    source_summary.cost.llc_pressure_cost,
                    source_summary.cost.df_overload_cost,
                    source_summary.cost.total_cost(),
                    source_summary.migrations_in,
                    source_summary.migrations_out,
                    source_summary.migration_budget,
                    source_summary.migration_penalty,
                    source_summary.cost.hard_constraints.migration_budget_violations,
                )
            } else {
                String::new()
            };
            candidates.push(format!(
                "{domain}[legal=true total={candidate_total} soft={} hard_diag={} sync_violations={} target_df={} target_llc={} target_tasks={} target_overload={} target_soft={} target_hard_diag={} target_slot_extra={} target_balance={} target_llc_cost={} target_df_overload_cost={} target_local={} target_mig_in={} target_mig_out={} target_budget={} target_mig_penalty={} target_mig_violations={}{}]",
                candidate_eval.cost.soft_cost(),
                candidate_eval.cost.hard_constraint_diagnostic_cost(),
                candidate_eval.cost.hard_constraints.sync_split_violations,
                target_summary.predicted_df_x100,
                target_summary.predicted_llc_x100,
                target_summary.predicted_active_task_count,
                target_summary.overload_x100,
                target_summary.cost.soft_cost(),
                target_summary.cost.hard_constraint_diagnostic_cost(),
                target_summary.cost.hard_constraints.slot_over_capacity.extra_tasks(),
                target_summary.cost.signature_balance_cost,
                target_summary.cost.llc_pressure_cost,
                target_summary.cost.df_overload_cost,
                target_summary.cost.total_cost(),
                target_summary.migrations_in,
                target_summary.migrations_out,
                target_summary.migration_budget,
                target_summary.migration_penalty,
                target_summary.cost.hard_constraints.migration_budget_violations,
                source_segment,
            ));
        }
        state.assignments[item_index] = original_assignment;

        let line = format!(
            "ts_ns={} planner_epoch={} plan_revision={} was_global={} item_index={} member_tids={} moved_members={} current_domains={} target_domain={} sync_group_id={} triggers={} allowed_domains={} chosen_total={} chosen_soft={} chosen_hard_diag={} chosen_sync_violations={} home_total={} candidates={}\n",
            snapshot.now_ns,
            snapshot.latest_df_sweep_epoch,
            plan_revision,
            was_global,
            item_index,
            format_u32_list(item.member_tids.iter().copied()),
            moved_members,
            format_member_domains(&item, state),
            target_domain,
            item.sync_group_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "na".to_string()),
            format_triggers(triggers),
            format_u32_list(item.allowed_domains.iter().copied()),
            chosen_total,
            chosen_eval.cost.soft_cost(),
            chosen_eval.cost.hard_constraint_diagnostic_cost(),
            chosen_eval.cost.hard_constraints.sync_split_violations,
            home_total
                .map(|value| value.to_string())
                .unwrap_or_else(|| "na".to_string()),
            candidates.join("|"),
        );
        logger.write_line(&line)?;
    }

    Ok(())
}

fn effective_member_domain(tid: u32, state: &PlannerState) -> Option<u32> {
    let item_index = *state.item_index_by_tid.get(&tid)?;
    let assignment = *state.assignments.get(item_index)?;
    let current_domain = state.members.get(&tid)?.current_domain;
    target_domain_for_home(assignment, current_domain)
}

fn member_effect(thread: &ManagedThreadState) -> (u32, u32) {
    let effect = thread_signature_effect(thread);
    (effect.df_x100, effect.llc_x100)
}

fn thread_home(thread: &ManagedThreadState) -> Option<u32> {
    thread.last_selected_domain.or(thread.last_observed_domain)
}

fn thread_sync_qualified(thread: &ManagedThreadState) -> bool {
    if !thread.signature.valid {
        return false;
    }
    if !(thread.signature.stable
        || thread.signature.confidence_x100 >= SIGNATURE_CONFIDENCE_THRESHOLD_X100)
    {
        return false;
    }
    let near_cache_share = thread.signature.fill_share_x100[MEM_SOURCE_NEAR_CACHE];
    let dram_near_share = thread.signature.fill_share_x100[MEM_SOURCE_DRAM_NEAR];
    let near_cache_bw = thread.signature.fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE];
    let stall_ok = thread.ewma_stall_pct_x100 >= SYNC_EWMA_STALL_MIN_X100
        || thread.last_stall_delta_pct_x100 >= SYNC_STALL_DELTA_MIN_X100;
    near_cache_share >= SYNC_NEAR_CACHE_SHARE_MIN_X100
        && dram_near_share <= SYNC_DRAM_NEAR_SHARE_MAX_X100
        && near_cache_bw >= SYNC_NEAR_CACHE_BW_MIN_MIB_S_X100
        && stall_ok
}

fn live_df_for_domain(state: Option<CcmDfStateValue>, now_ns: u64, stale_ms: u64) -> u32 {
    match state {
        Some(state)
            if state.valid != 0
                && now_ns.saturating_sub(state.sample_ts_ns)
                    <= stale_ms.saturating_mul(1_000_000) =>
        {
            state
                .raw_read_bw_mib_s_x100
                .saturating_add(state.raw_write_bw_mib_s_x100)
        }
        _ => 0,
    }
}

fn round_migration_budget(task_count: u32) -> u32 {
    task_count.saturating_add(3).div_ceil(4).clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CcmDfStateValue, DomainInfo, HotCoolState, LlcStateValue, ThreadSignature, MAX_DOMAINS,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn sample_thread(tid: u32, tgid: u32) -> ManagedThreadState {
        let mut domains = [0u32; MAX_DOMAINS];
        domains[0] = 500_000;
        ManagedThreadState {
            tid,
            tgid,
            signature: ThreadSignature {
                valid: true,
                stable: true,
                confidence_x100: 5_000,
                fill_bw_mib_s_x100: [0, 20_000, 1_000],
                fill_share_x100: [0, 6_000, 500],
                projected_df_pressure_x100: 20_000,
                projected_llc_pressure_x100: 1_000,
                df_domain_delta_x100: domains,
                ..ThreadSignature::default()
            },
            ewma_stall_pct_x100: 4_000,
            ..ManagedThreadState::default()
        }
    }

    fn policy() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 8_500,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 5_000,
            stall_victim_delta_pct_x100: 1_500,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: false,
            cs_villain_throttle: true,
            tick_reeval_every: 1,
            tick_defer_max: 1,
            cs_villain_reslice_ns: 1_000_000,
            cs_villain_refill_divisor: 4,
            cs_villain_settle_ns: 20_000_000,
            cs_villain_release_samples: 2,
            tick_move_phase_mod: 1,
        }
    }

    fn planner_config() -> PlannerConfig {
        PlannerConfig {
            disable_auto_sync_hints: false,
            sync_tgid_overrides: BTreeSet::new(),
            incremental_item_limit: 64,
            incremental_domain_limit: 4,
            max_passes: 8,
            swap_pass_items: 16,
            debounce: Duration::from_millis(2),
            move_trace_path: None,
            plan_debug_path: None,
        }
    }

    fn df_state(read_centi_mib_per_s: u32) -> Option<CcmDfStateValue> {
        Some(CcmDfStateValue {
            sample_ts_ns: 1_000_000,
            raw_read_bw_mib_s_x100: read_centi_mib_per_s,
            raw_write_bw_mib_s_x100: 0,
            valid: 1,
            ..CcmDfStateValue::default()
        })
    }

    fn llc_state(pressure_basis_points: u32, state: HotCoolState) -> Option<LlcStateValue> {
        Some(LlcStateValue {
            sample_ts_ns: 1_000_000,
            raw_l3_bw_mib_s_x100: pressure_basis_points,
            raw_pressure_pct_x100: pressure_basis_points,
            ewma_pressure_pct_x100: pressure_basis_points,
            state: state as u32,
            valid: 1,
            ..LlcStateValue::default()
        })
    }

    fn ccd_topology() -> TopologyLayout {
        let domains = vec![
            DomainInfo {
                domain_id: 0,
                kernel_l3_id: 0,
                rep_cpu: 0,
                cpus: vec![0, 1],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 1,
                kernel_l3_id: 1,
                rep_cpu: 7,
                cpus: vec![7, 8],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 2,
                kernel_l3_id: 3,
                rep_cpu: 21,
                cpus: vec![21, 22],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 3,
                kernel_l3_id: 4,
                rep_cpu: 28,
                cpus: vec![28, 29],
                l3_size_mb: 32.0,
            },
        ];
        let mut cpu_to_domain = vec![None; 30];
        for domain in &domains {
            for &cpu in &domain.cpus {
                cpu_to_domain[cpu as usize] = Some(domain.domain_id);
            }
        }
        TopologyLayout {
            nr_cpu_ids: 30,
            domains,
            cpu_to_domain,
        }
    }

    fn ccd_mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1, 3, 4],
            domain_to_ccm: vec![Some(0), Some(1), Some(3), Some(4)],
            domain_to_df_capacity_mib_s_x100: vec![
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
            ],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1, 2, 3]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 7, 8, 21, 22, 28, 29]),
        }
    }

    fn traffic_thread(
        tid: u32,
        current_domain: u32,
        current_cpu: u32,
        df_centi_mib_per_s: u32,
    ) -> ManagedThreadState {
        ManagedThreadState {
            tid,
            tgid: tid,
            last_seen_ns: 2_000_000,
            last_observed_domain: Some(current_domain),
            last_selected_domain: Some(current_domain),
            last_observed_cpu: Some(current_cpu),
            last_selected_cpu: Some(current_cpu),
            signature: ThreadSignature {
                valid: true,
                stable: true,
                sample_count: 2,
                confidence_x100: 90_00,
                projected_df_pressure_x100: df_centi_mib_per_s,
                projected_llc_pressure_x100: 10_00,
                ..ThreadSignature::default()
            },
            ..ManagedThreadState::default()
        }
    }

    #[cfg(feature = "scheduler-paper-greedy")]
    fn sync_traffic_thread(
        tid: u32,
        tgid: u32,
        current_domain: u32,
        current_cpu: u32,
        df_centi_mib_per_s: u32,
    ) -> ManagedThreadState {
        let mut thread = traffic_thread(tid, current_domain, current_cpu, df_centi_mib_per_s);
        thread.tgid = tgid;
        thread
    }

    #[test]
    fn sync_classifier_matches_expected_shape() {
        assert!(thread_sync_qualified(&sample_thread(1, 10)));
        let mut thread = sample_thread(2, 10);
        thread.signature.fill_share_x100[MEM_SOURCE_DRAM_NEAR] = 2_000;
        assert!(!thread_sync_qualified(&thread));
    }

    // Verifies the placement plan built from a mock IO load map and per-thread
    // traffic signatures. Expected: hot traffic currently on the overloaded
    // ccd0 is planned onto the two lowest-pressure legal CCDs, with target CPUs
    // materialized inside those domains.
    #[test]
    fn domain_plan_io_load_map_moves_hot_threads_to_low_pressure_domains() {
        let ccd0_domain = 0;
        let ccd1_domain = 1;
        let ccd3_domain = 2;
        let ccd4_domain = 3;
        let hot_thread_a = 101;
        let hot_thread_b = 102;
        let now_ns = 2_000_000;
        let sweep_epoch = 7;
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let all_allowed_cpus = BTreeSet::from([0, 1, 7, 8, 21, 22, 28, 29]);
        let all_allowed_domains =
            BTreeSet::from([ccd0_domain, ccd1_domain, ccd3_domain, ccd4_domain]);
        let mut threads = BTreeMap::new();
        // Raw mock traffic labels: the two hot source threads contribute
        // 8,000 MiB/s and 7,000 MiB/s, respectively. They both start on ccd0.
        threads.insert(
            hot_thread_a,
            traffic_thread(hot_thread_a, ccd0_domain, 0, 8_000_00),
        );
        threads.insert(
            hot_thread_b,
            traffic_thread(hot_thread_b, ccd0_domain, 1, 7_000_00),
        );
        let allowed_cpus = BTreeMap::from([
            (hot_thread_a, all_allowed_cpus.clone()),
            (hot_thread_b, all_allowed_cpus),
        ]);
        let allowed_domains = BTreeMap::from([
            (hot_thread_a, all_allowed_domains.clone()),
            (hot_thread_b, all_allowed_domains),
        ]);
        // Raw mock IO load map:
        // ccd0: 25,000 MiB/s DF read, 90% LLC pressure, overloaded source.
        // ccd1: 12,000 MiB/s DF read, 30% LLC pressure, legal but warmer.
        // ccd3:  4,000 MiB/s DF read, 10% LLC pressure, coolest target.
        // ccd4:  8,000 MiB/s DF read, 20% LLC pressure, next best target.
        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: policy(),
            now_ns,
            latest_df_sweep_epoch: sweep_epoch,
            pending_tids: BTreeSet::from([hot_thread_a, hot_thread_b]),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![
                llc_state(90_00, HotCoolState::Hot),
                llc_state(30_00, HotCoolState::Cool),
                llc_state(10_00, HotCoolState::Cool),
                llc_state(20_00, HotCoolState::Cool),
            ],
            df_states: vec![
                df_state(25_000_00),
                df_state(12_000_00),
                df_state(4_000_00),
                df_state(8_000_00),
            ],
            previous_plan: None,
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SweepComplete(sweep_epoch)],
            &mut move_trace,
        );
        let target_a = output.plan.entries[&hot_thread_a].target_domain;
        let target_b = output.plan.entries[&hot_thread_b].target_domain;
        let planned_targets = BTreeSet::from([target_a, target_b]);

        assert!(output.was_global);
        assert_eq!(output.plan.built_from_sweep_epoch, sweep_epoch);
        assert_eq!(output.plan.entries.len(), 2);
        assert_eq!(planned_targets, BTreeSet::from([ccd3_domain, ccd4_domain]));
        assert_ne!(target_a, ccd0_domain);
        assert_ne!(target_b, ccd0_domain);
        assert_ne!(target_a, ccd1_domain);
        assert_ne!(target_b, ccd1_domain);
        for entry in output.plan.entries.values() {
            let target_cpu = entry.target_cpu.expect("planned target CPU");
            assert_eq!(
                ccd_topology()
                    .cpu_to_domain
                    .get(target_cpu as usize)
                    .copied()
                    .flatten(),
                Some(entry.target_domain)
            );
        }
    }

    fn next_seeded_u32(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 32) as u32
    }

    fn next_seeded_range(seed: &mut u64, upper: u32) -> u32 {
        if upper == 0 {
            0
        } else {
            next_seeded_u32(seed) % upper
        }
    }

    const HOT_DF_TAIL_REPRO_SEED: u64 = 13_237_814_481_653_833_671;

    fn domains_from_allowed_cpus(topo: &TopologyLayout, cpus: &BTreeSet<u32>) -> BTreeSet<u32> {
        cpus.iter()
            .copied()
            .filter_map(|cpu| topo.cpu_to_domain.get(cpu as usize).copied().flatten())
            .collect()
    }

    fn previous_affinity_entry(target_domain: u32, target_cpu: u32) -> PlacementPlanEntry {
        PlacementPlanEntry {
            target_domain,
            target_cpu: Some(target_cpu),
            built_from_sweep_epoch: 31,
            plan_revision: 41,
            planned_at_ns: 1_000_000,
            sync_group_id: None,
            sync_anchor_domain: None,
            sync_override: false,
        }
    }

    fn load75_topology() -> TopologyLayout {
        let domains = vec![
            DomainInfo {
                domain_id: 0,
                kernel_l3_id: 0,
                rep_cpu: 0,
                cpus: vec![0, 1, 2, 3, 4, 5, 6],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 1,
                kernel_l3_id: 1,
                rep_cpu: 7,
                cpus: vec![7, 8, 9, 10, 11, 12, 13],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 2,
                kernel_l3_id: 3,
                rep_cpu: 21,
                cpus: vec![21, 22, 23, 24, 25, 26, 27],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 3,
                kernel_l3_id: 4,
                rep_cpu: 28,
                cpus: vec![28, 29, 30, 31, 32, 33, 34],
                l3_size_mb: 32.0,
            },
        ];
        let mut cpu_to_domain = vec![None; 35];
        for domain in &domains {
            for &cpu in &domain.cpus {
                cpu_to_domain[cpu as usize] = Some(domain.domain_id);
            }
        }
        TopologyLayout {
            nr_cpu_ids: 35,
            domains,
            cpu_to_domain,
        }
    }

    fn load75_mapping() -> MappingInfo {
        let topo = load75_topology();
        MappingInfo {
            domain_to_ccx: vec![0, 1, 3, 4],
            domain_to_ccm: vec![Some(0), Some(1), Some(3), Some(4)],
            domain_to_df_capacity_mib_s_x100: vec![
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
                Some(2_000_000),
            ],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1, 2, 3]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: topo
                .domains
                .iter()
                .flat_map(|domain| domain.cpus.iter().copied())
                .collect(),
        }
    }

    fn load75_time_series_seed_state(
        topo: &TopologyLayout,
        mapping: &MappingInfo,
        demand_df_x100: u32,
    ) -> (
        BTreeMap<u32, ManagedThreadState>,
        BTreeMap<u32, BTreeSet<u32>>,
        BTreeMap<u32, BTreeSet<u32>>,
        PlacementPlan,
    ) {
        let all_cpus = mapping
            .eligible_cpus
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let all_domains = mapping
            .eligible_domains
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        let mut idx = 0u32;
        for domain in &topo.domains {
            for &cpu in domain.cpus.iter().take(5) {
                let tid = 70_000 + idx;
                threads.insert(
                    tid,
                    traffic_thread(tid, domain.domain_id, cpu, demand_df_x100),
                );
                allowed_cpus.insert(tid, all_cpus.clone());
                allowed_domains.insert(tid, all_domains.clone());
                previous_entries.insert(tid, previous_affinity_entry(domain.domain_id, cpu));
                idx += 1;
            }
        }
        let previous_plan = PlacementPlan {
            built_from_sweep_epoch: 70,
            plan_revision: 70,
            planned_at_ns: 1_000_000,
            entries: previous_entries,
        };
        (threads, allowed_cpus, allowed_domains, previous_plan)
    }

    #[derive(Clone, Debug)]
    struct TailChurnFuzzKnobs {
        case_seed: u64,
        df_base_x100: u32,
        df_jitter_x100: u32,
        llc_base_x100: u32,
        llc_jitter_x100: u32,
        llama_demand_base_x100: u32,
        llama_demand_jitter_x100: u32,
        sidecar_demand_base_x100: u32,
        sidecar_demand_jitter_x100: u32,
        sidecars_by_domain: [u32; 4],
        sweep_every: u64,
    }

    fn tail_churn_fuzz_knobs(seed: u64) -> TailChurnFuzzKnobs {
        let mut rng = seed;
        let mut sidecars_by_domain = [1_u32; 4];
        let total_sidecars = 5 + next_seeded_range(&mut rng, 3);
        let mut remaining = total_sidecars.saturating_sub(4);
        while remaining > 0 {
            let domain = next_seeded_range(&mut rng, 4) as usize;
            if sidecars_by_domain[domain] < 2 {
                sidecars_by_domain[domain] += 1;
                remaining -= 1;
            }
        }

        TailChurnFuzzKnobs {
            case_seed: seed,
            df_base_x100: 2_150_000 + next_seeded_range(&mut rng, 450_000),
            df_jitter_x100: 40_000 + next_seeded_range(&mut rng, 240_000),
            llc_base_x100: 9_200 + next_seeded_range(&mut rng, 500),
            llc_jitter_x100: 50 + next_seeded_range(&mut rng, 500),
            llama_demand_base_x100: 70_000 + next_seeded_range(&mut rng, 60_000),
            llama_demand_jitter_x100: 5_000 + next_seeded_range(&mut rng, 70_000),
            sidecar_demand_base_x100: 10_000 + next_seeded_range(&mut rng, 40_000),
            sidecar_demand_jitter_x100: 2_000 + next_seeded_range(&mut rng, 25_000),
            sidecars_by_domain,
            sweep_every: 2 + u64::from(next_seeded_range(&mut rng, 4)),
        }
    }

    fn load75_tail_churn_seed_state(
        topo: &TopologyLayout,
        mapping: &MappingInfo,
        knobs: &TailChurnFuzzKnobs,
    ) -> (
        BTreeMap<u32, ManagedThreadState>,
        BTreeMap<u32, BTreeSet<u32>>,
        BTreeMap<u32, BTreeSet<u32>>,
        PlacementPlan,
    ) {
        let mut rng = knobs.case_seed ^ 0xd15a_e5ed_f00d_2026_u64;
        let workload_cpus = topo
            .domains
            .iter()
            .flat_map(|domain| domain.cpus.iter().take(5).copied())
            .collect::<BTreeSet<_>>();
        let workload_domains = mapping
            .eligible_domains
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        let mut next_tid = 80_000_u32;

        for domain in &topo.domains {
            for &cpu in domain.cpus.iter().take(5) {
                let demand = knobs
                    .llama_demand_base_x100
                    .saturating_add(next_seeded_range(&mut rng, knobs.llama_demand_jitter_x100));
                let tid = next_tid;
                next_tid += 1;
                threads.insert(tid, traffic_thread(tid, domain.domain_id, cpu, demand));
                allowed_cpus.insert(tid, workload_cpus.clone());
                allowed_domains.insert(tid, workload_domains.clone());
                previous_entries.insert(tid, previous_affinity_entry(domain.domain_id, cpu));
            }
        }

        for domain in &topo.domains {
            let sidecar_count = knobs
                .sidecars_by_domain
                .get(domain.domain_id as usize)
                .copied()
                .unwrap_or(1) as usize;
            for &cpu in domain.cpus.iter().skip(5).take(sidecar_count) {
                let demand = knobs
                    .sidecar_demand_base_x100
                    .saturating_add(next_seeded_range(
                        &mut rng,
                        knobs.sidecar_demand_jitter_x100,
                    ));
                let tid = next_tid;
                next_tid += 1;
                threads.insert(tid, traffic_thread(tid, domain.domain_id, cpu, demand));
                allowed_cpus.insert(tid, BTreeSet::from([cpu]));
                allowed_domains.insert(tid, BTreeSet::from([domain.domain_id]));
                previous_entries.insert(tid, previous_affinity_entry(domain.domain_id, cpu));
            }
        }

        let previous_plan = PlacementPlan {
            built_from_sweep_epoch: 400,
            plan_revision: 400,
            planned_at_ns: 1_000_000,
            entries: previous_entries,
        };
        (threads, allowed_cpus, allowed_domains, previous_plan)
    }

    fn apply_plan_as_runtime_observation(
        topo: &TopologyLayout,
        threads: &mut BTreeMap<u32, ManagedThreadState>,
        plan: &PlacementPlan,
    ) {
        for (tid, entry) in &plan.entries {
            let Some(thread) = threads.get_mut(tid) else {
                continue;
            };
            thread.last_selected_domain = Some(entry.target_domain);
            thread.last_selected_cpu = entry.target_cpu;
            thread.last_observed_domain = Some(entry.target_domain);
            thread.last_observed_cpu = entry.target_cpu;
            if let Some(cpu) = entry.target_cpu {
                assert_eq!(
                    topo.cpu_to_domain.get(cpu as usize).copied().flatten(),
                    Some(entry.target_domain)
                );
            }
        }
    }

    fn count_plan_changes(previous: &PlacementPlan, current: &PlacementPlan) -> (u32, u32) {
        let mut domain_changes = 0u32;
        let mut cpu_changes = 0u32;
        for (tid, current_entry) in &current.entries {
            let Some(previous_entry) = previous.entries.get(tid) else {
                continue;
            };
            if previous_entry.target_domain != current_entry.target_domain {
                domain_changes += 1;
            }
            if previous_entry.target_cpu != current_entry.target_cpu {
                cpu_changes += 1;
            }
        }
        (domain_changes, cpu_changes)
    }

    fn plan_domain_counts(plan: &PlacementPlan) -> BTreeMap<u32, usize> {
        let mut counts = BTreeMap::<u32, usize>::new();
        for entry in plan.entries.values() {
            *counts.entry(entry.target_domain).or_default() += 1;
        }
        counts
    }

    #[test]
    fn planner_repairs_duplicate_previous_target_cpu_when_domain_has_free_legal_cpu() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let target_domain = 0;
        let target_cpus = BTreeSet::from([0_u32, 1, 2, 3, 4]);
        let tids = [90_100_u32, 90_101, 90_102, 90_103, 90_104];
        let current_cpus = [0_u32, 1, 2, 3, 4];
        let previous_target_cpus = [0_u32, 1, 2, 2, 3];

        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        for ((tid, current_cpu), previous_cpu) in tids
            .into_iter()
            .zip(current_cpus.into_iter())
            .zip(previous_target_cpus.into_iter())
        {
            threads.insert(tid, traffic_thread(tid, target_domain, current_cpu, 80_000));
            allowed_cpus.insert(tid, target_cpus.clone());
            allowed_domains.insert(tid, BTreeSet::from([target_domain]));
            previous_entries.insert(tid, previous_affinity_entry(target_domain, previous_cpu));
        }

        let snapshot = PlannerInput {
            topo: topo.clone(),
            mapping,
            policy: policy(),
            now_ns: 2_000_000,
            latest_df_sweep_epoch: 91,
            pending_tids: tids.into_iter().collect(),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![llc_state(8_000, HotCoolState::Hot); topo.domains.len()],
            df_states: vec![df_state(1_500_000); topo.domains.len()],
            previous_plan: Some(PlacementPlan {
                built_from_sweep_epoch: 90,
                plan_revision: 90,
                planned_at_ns: 1_000_000,
                entries: previous_entries,
            }),
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SweepComplete(91)],
            &mut move_trace,
        );

        let planned_cpus = tids
            .into_iter()
            .map(|tid| {
                let entry = &output.plan.entries[&tid];
                assert_eq!(entry.target_domain, target_domain, "tid={tid}");
                entry.target_cpu.expect("planned target CPU")
            })
            .collect::<Vec<_>>();
        let unique_planned = planned_cpus.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(
            unique_planned, target_cpus,
            "planned_cpus={planned_cpus:?} unique_planned={unique_planned:?}"
        );
    }

    #[test]
    fn planner_cpu_repair_ignores_inactive_previous_cpu_reservations() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let target_domain = 0;
        let target_cpus = BTreeSet::from([0_u32, 1, 2, 3, 4]);
        let active_tids = [91_100_u32, 91_101, 91_102, 91_103, 91_104];
        let active_current_cpus = [1_u32, 2, 2, 4, 4];
        let inactive_tids = [91_200_u32, 91_201];
        let inactive_current_cpus = [0_u32, 3];

        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        let mut entries = BTreeMap::new();
        for (tid, current_cpu) in active_tids.into_iter().zip(active_current_cpus.into_iter()) {
            threads.insert(tid, traffic_thread(tid, target_domain, current_cpu, 80_000));
            allowed_cpus.insert(tid, target_cpus.clone());
            allowed_domains.insert(tid, BTreeSet::from([target_domain]));
            previous_entries.insert(tid, previous_affinity_entry(target_domain, current_cpu));
            entries.insert(
                tid,
                PlacementPlanEntry {
                    target_domain,
                    ..PlacementPlanEntry::default()
                },
            );
        }
        for (tid, current_cpu) in inactive_tids
            .into_iter()
            .zip(inactive_current_cpus.into_iter())
        {
            let mut thread = traffic_thread(tid, target_domain, current_cpu, 1);
            thread.last_seen_ns = 0;
            threads.insert(tid, thread);
            allowed_cpus.insert(tid, target_cpus.clone());
            allowed_domains.insert(tid, BTreeSet::from([target_domain]));
            previous_entries.insert(tid, previous_affinity_entry(target_domain, current_cpu));
            entries.insert(
                tid,
                PlacementPlanEntry {
                    target_domain,
                    ..PlacementPlanEntry::default()
                },
            );
        }

        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: policy(),
            now_ns: 200_000_000,
            latest_df_sweep_epoch: 91,
            pending_tids: active_tids.into_iter().collect(),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: Vec::new(),
            df_states: Vec::new(),
            previous_plan: Some(PlacementPlan {
                built_from_sweep_epoch: 90,
                plan_revision: 90,
                planned_at_ns: 1_000_000,
                entries: previous_entries,
            }),
        };

        assign_plan_target_cpus(&snapshot, &mut entries);

        let planned_cpus = active_tids
            .into_iter()
            .map(|tid| {
                entries[&tid]
                    .target_cpu
                    .expect("active thread should get planned target CPU")
            })
            .collect::<Vec<_>>();
        let unique_planned = planned_cpus.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(
            unique_planned, target_cpus,
            "inactive entries must not block active CPU repair: planned_cpus={planned_cpus:?}"
        );
    }

    #[test]
    fn incremental_replan_keeps_unscoped_previous_domain_plan_repro() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let workload_cpus_by_domain = topo
            .domains
            .iter()
            .map(|domain| domain.cpus.iter().take(5).copied().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let workload_cpus = workload_cpus_by_domain
            .iter()
            .flat_map(|cpus| cpus.iter().copied())
            .collect::<BTreeSet<_>>();
        let workload_domains = mapping
            .eligible_domains
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let current_domains = [
            0_u32, 0, 0, 0, 0, 0, 0, // observed overpack
            1, 1, 1, 1, 1, 1, 1, // observed overpack
            3, 3, 3, 3, 3, 3, // domain 2 is observed empty
        ];
        let mut current_domain_slots = [0_usize; 4];
        let mut previous_domain_slots = [0_usize; 4];
        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        let mut tids = Vec::new();

        for (idx, current_domain) in current_domains.into_iter().enumerate() {
            let tid = 91_000 + idx as u32;
            let previous_domain = (idx / 5) as u32;
            let current_slot = current_domain_slots[current_domain as usize] % 5;
            let previous_slot = previous_domain_slots[previous_domain as usize] % 5;
            current_domain_slots[current_domain as usize] += 1;
            previous_domain_slots[previous_domain as usize] += 1;
            let current_cpu = workload_cpus_by_domain[current_domain as usize][current_slot];
            let previous_cpu = workload_cpus_by_domain[previous_domain as usize][previous_slot];

            threads.insert(
                tid,
                traffic_thread(tid, current_domain, current_cpu, 100_000),
            );
            allowed_cpus.insert(tid, workload_cpus.clone());
            allowed_domains.insert(tid, workload_domains.clone());
            previous_entries.insert(tid, previous_affinity_entry(previous_domain, previous_cpu));
            tids.push(tid);
        }

        let previous_plan = PlacementPlan {
            built_from_sweep_epoch: 510,
            plan_revision: 510,
            planned_at_ns: 1_000_000,
            entries: previous_entries,
        };
        assert_eq!(
            plan_domain_counts(&previous_plan),
            BTreeMap::from([(0, 5), (1, 5), (2, 5), (3, 5)])
        );

        let mut policy = policy();
        policy.migrate_margin_x100 = 100;
        let snapshot = PlannerInput {
            topo: topo.clone(),
            mapping: mapping.clone(),
            policy,
            now_ns: 2_000_000,
            latest_df_sweep_epoch: 511,
            pending_tids: tids.iter().copied().collect(),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![llc_state(9_500, HotCoolState::Hot); topo.domains.len()],
            df_states: vec![df_state(2_200_000); topo.domains.len()],
            previous_plan: Some(previous_plan.clone()),
        };
        let changed_tid = tids[0];
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SignatureChange(changed_tid)],
            &mut move_trace,
        );
        let counts = plan_domain_counts(&output.plan);

        assert!(!output.was_global);
        assert_eq!(
            counts,
            BTreeMap::from([(0, 5), (1, 5), (2, 5), (3, 5)]),
            "incremental replan should keep the balanced previous plan for unscoped threads; changed_tid={changed_tid} counts={counts:?}"
        );
    }

    #[test]
    fn all_hot_seeded_fuzz_keeps_domains_and_cpus_stable() {
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let mut seed = 0x5eed_c0de_cafe_f00d_u64;

        for case_idx in 0..128_u32 {
            let mut cpus = mapping.eligible_cpus.iter().copied().collect::<Vec<_>>();
            for idx in 0..cpus.len() {
                let swap_idx = (next_seeded_u32(&mut seed) as usize) % cpus.len();
                cpus.swap(idx, swap_idx);
            }

            let mut threads = BTreeMap::new();
            let mut allowed_cpus = BTreeMap::new();
            let mut allowed_domains = BTreeMap::new();
            let mut previous_entries = BTreeMap::new();
            for (idx, cpu) in cpus.iter().copied().enumerate() {
                let tid = 10_000 + case_idx * 100 + idx as u32;
                let current_domain = topo.cpu_to_domain[cpu as usize].unwrap();
                threads.insert(tid, traffic_thread(tid, current_domain, cpu, 250_000));

                let mut allowed = BTreeSet::from([cpu]);
                for candidate in mapping.eligible_cpus.iter().copied() {
                    if next_seeded_u32(&mut seed) % 3 == 0 {
                        allowed.insert(candidate);
                    }
                }
                allowed_cpus.insert(tid, allowed.clone());
                allowed_domains.insert(tid, domains_from_allowed_cpus(&topo, &allowed));
                previous_entries.insert(
                    tid,
                    PlacementPlanEntry {
                        target_domain: current_domain,
                        target_cpu: Some(cpu),
                        built_from_sweep_epoch: 7,
                        plan_revision: 11,
                        planned_at_ns: 1_000_000,
                        sync_group_id: None,
                        sync_anchor_domain: None,
                        sync_override: false,
                    },
                );
            }

            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy: policy(),
                now_ns: 2_000_000,
                latest_df_sweep_epoch: 8,
                pending_tids: threads.keys().copied().collect(),
                threads,
                allowed_cpus,
                allowed_domains,
                llc_states: vec![
                    llc_state(9_000, HotCoolState::Hot),
                    llc_state(9_000, HotCoolState::Hot),
                    llc_state(9_000, HotCoolState::Hot),
                    llc_state(9_000, HotCoolState::Hot),
                ],
                df_states: vec![
                    df_state(1_800_000),
                    df_state(1_800_000),
                    df_state(1_800_000),
                    df_state(1_800_000),
                ],
                previous_plan: Some(PlacementPlan {
                    built_from_sweep_epoch: 7,
                    plan_revision: 11,
                    planned_at_ns: 1_000_000,
                    entries: previous_entries.clone(),
                }),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let output = compute_plan(
                &planner_config(),
                snapshot,
                &[PlannerTrigger::SweepComplete(8)],
                &mut move_trace,
            );

            for (tid, previous_entry) in &previous_entries {
                let entry =
                    output.plan.entries.get(tid).unwrap_or_else(|| {
                        panic!("case {case_idx} missing plan entry for tid {tid}")
                    });
                assert_eq!(
                    entry.target_domain, previous_entry.target_domain,
                    "case {case_idx} seed={seed:#x} tid={tid}"
                );
                assert_eq!(
                    entry.target_cpu, previous_entry.target_cpu,
                    "case {case_idx} seed={seed:#x} tid={tid}"
                );
            }
        }
    }

    #[test]
    fn small_jitter_seeded_fuzz_keeps_previous_domain_cpu_affinity() {
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let mut seed = 0xa11_0c_u64;

        for case_idx in 0..128_u32 {
            let mut cpus = mapping.eligible_cpus.iter().copied().collect::<Vec<_>>();
            for idx in 0..cpus.len() {
                let swap_idx = (next_seeded_u32(&mut seed) as usize) % cpus.len();
                cpus.swap(idx, swap_idx);
            }

            let mut threads = BTreeMap::new();
            let mut allowed_cpus = BTreeMap::new();
            let mut allowed_domains = BTreeMap::new();
            let mut previous_entries = BTreeMap::new();
            for (idx, cpu) in cpus.iter().copied().enumerate() {
                let tid = 20_000 + case_idx * 100 + idx as u32;
                let current_domain = topo.cpu_to_domain[cpu as usize].unwrap();
                let demand_jitter = next_seeded_u32(&mut seed) % 2_000;
                threads.insert(
                    tid,
                    traffic_thread(
                        tid,
                        current_domain,
                        cpu,
                        120_000_u32.saturating_add(demand_jitter),
                    ),
                );

                let mut allowed = BTreeSet::from([cpu]);
                for candidate in mapping.eligible_cpus.iter().copied() {
                    if next_seeded_u32(&mut seed) % 2 == 0 {
                        allowed.insert(candidate);
                    }
                }
                allowed_cpus.insert(tid, allowed.clone());
                allowed_domains.insert(tid, domains_from_allowed_cpus(&topo, &allowed));
                previous_entries.insert(
                    tid,
                    PlacementPlanEntry {
                        target_domain: current_domain,
                        target_cpu: Some(cpu),
                        built_from_sweep_epoch: 13,
                        plan_revision: 21,
                        planned_at_ns: 1_000_000,
                        sync_group_id: None,
                        sync_anchor_domain: None,
                        sync_override: false,
                    },
                );
            }

            let mut jittered_df_states = Vec::new();
            let mut jittered_llc_states = Vec::new();
            for _ in 0..topo.domains.len() {
                let df_jitter = next_seeded_u32(&mut seed) % 2_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 50;
                jittered_df_states.push(df_state(1_200_000_u32.saturating_add(df_jitter)));
                jittered_llc_states.push(llc_state(
                    5_000_u32.saturating_add(llc_jitter),
                    HotCoolState::Hot,
                ));
            }

            let mut policy = policy();
            policy.migrate_margin_x100 = 100;
            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy,
                now_ns: 2_000_000,
                latest_df_sweep_epoch: 14,
                pending_tids: threads.keys().copied().collect(),
                threads,
                allowed_cpus,
                allowed_domains,
                llc_states: jittered_llc_states,
                df_states: jittered_df_states,
                previous_plan: Some(PlacementPlan {
                    built_from_sweep_epoch: 13,
                    plan_revision: 21,
                    planned_at_ns: 1_000_000,
                    entries: previous_entries.clone(),
                }),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let output = compute_plan(
                &planner_config(),
                snapshot,
                &[PlannerTrigger::SweepComplete(14)],
                &mut move_trace,
            );

            for (tid, previous_entry) in &previous_entries {
                let entry =
                    output.plan.entries.get(tid).unwrap_or_else(|| {
                        panic!("case {case_idx} missing plan entry for tid {tid}")
                    });
                assert_eq!(
                    entry.target_domain, previous_entry.target_domain,
                    "case {case_idx} seed={seed:#x} tid={tid}"
                );
                assert_eq!(
                    entry.target_cpu, previous_entry.target_cpu,
                    "case {case_idx} seed={seed:#x} tid={tid}"
                );
            }
        }
    }

    #[test]
    fn planner_time_series_seeded_fuzz_keeps_balanced_affinity_stable() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let (mut threads, allowed_cpus, allowed_domains, mut previous_plan) =
            load75_time_series_seed_state(&topo, &mapping, 80_000);
        let mut seed = 0x71_5e_e1e5_u64;
        let mut total_domain_changes = 0u32;
        let mut total_cpu_changes = 0u32;

        for step in 0..128_u64 {
            let mut df_states = Vec::new();
            let mut llc_states = Vec::new();
            for _ in 0..topo.domains.len() {
                let df_jitter = next_seeded_u32(&mut seed) % 4_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 100;
                df_states.push(df_state(1_000_000_u32.saturating_add(df_jitter)));
                llc_states.push(llc_state(
                    4_000_u32.saturating_add(llc_jitter),
                    HotCoolState::Hot,
                ));
            }

            let mut policy = policy();
            policy.migrate_margin_x100 = 100;
            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy,
                now_ns: 2_000_000 + step * 1_000_000,
                latest_df_sweep_epoch: 80 + step,
                pending_tids: threads.keys().copied().collect(),
                threads: threads.clone(),
                allowed_cpus: allowed_cpus.clone(),
                allowed_domains: allowed_domains.clone(),
                llc_states,
                df_states,
                previous_plan: Some(previous_plan.clone()),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let output = compute_plan(
                &planner_config(),
                snapshot,
                &[PlannerTrigger::SweepComplete(80 + step)],
                &mut move_trace,
            );
            let (domain_changes, cpu_changes) = count_plan_changes(&previous_plan, &output.plan);
            total_domain_changes = total_domain_changes.saturating_add(domain_changes);
            total_cpu_changes = total_cpu_changes.saturating_add(cpu_changes);
            apply_plan_as_runtime_observation(&topo, &mut threads, &output.plan);
            previous_plan = output.plan;
        }

        assert_eq!(total_domain_changes, 0);
        assert_eq!(total_cpu_changes, 0);
    }

    #[test]
    fn planner_time_series_seeded_fuzz_keeps_near_capacity_affinity_stable() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let (mut threads, allowed_cpus, allowed_domains, mut previous_plan) =
            load75_time_series_seed_state(&topo, &mapping, 100_000);
        let mut seed = 0xcafe_71c_u64;
        let mut total_domain_changes = 0u32;
        let mut total_cpu_changes = 0u32;

        let tids = threads.keys().copied().collect::<Vec<_>>();
        for step in 0..128_u64 {
            for thread in threads.values_mut() {
                let demand_jitter = next_seeded_u32(&mut seed) % 40_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 1_500;
                thread.signature.projected_df_pressure_x100 =
                    80_000_u32.saturating_add(demand_jitter);
                thread.signature.projected_llc_pressure_x100 = 1_000_u32.saturating_add(llc_jitter);
            }

            let mut df_states = Vec::new();
            let mut llc_states = Vec::new();
            for _ in 0..topo.domains.len() {
                let df_jitter = next_seeded_u32(&mut seed) % 80_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 600;
                df_states.push(df_state(1_450_000_u32.saturating_add(df_jitter)));
                llc_states.push(llc_state(
                    6_000_u32.saturating_add(llc_jitter),
                    HotCoolState::Hot,
                ));
            }

            let mut policy = policy();
            policy.migrate_margin_x100 = 100;
            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy,
                now_ns: 2_000_000 + step * 1_000_000,
                latest_df_sweep_epoch: 180 + step,
                pending_tids: threads.keys().copied().collect(),
                threads: threads.clone(),
                allowed_cpus: allowed_cpus.clone(),
                allowed_domains: allowed_domains.clone(),
                llc_states,
                df_states,
                previous_plan: Some(previous_plan.clone()),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let changed_tid = tids[(next_seeded_u32(&mut seed) as usize) % tids.len()];
            let triggers = if step == 0 {
                vec![PlannerTrigger::SweepComplete(180)]
            } else {
                vec![PlannerTrigger::SignatureChange(changed_tid)]
            };
            let output = compute_plan(&planner_config(), snapshot, &triggers, &mut move_trace);
            let (domain_changes, cpu_changes) = count_plan_changes(&previous_plan, &output.plan);
            total_domain_changes = total_domain_changes.saturating_add(domain_changes);
            total_cpu_changes = total_cpu_changes.saturating_add(cpu_changes);
            apply_plan_as_runtime_observation(&topo, &mut threads, &output.plan);
            previous_plan = output.plan;
        }

        assert_eq!(total_domain_changes, 0);
        assert_eq!(total_cpu_changes, 0);
    }

    #[test]
    fn near_capacity_jitter_keeps_legal_previous_affinity_without_hard_improvement() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let (mut threads, allowed_cpus, allowed_domains, mut previous_plan) =
            load75_time_series_seed_state(&topo, &mapping, 100_000);
        let mut seed = 0xcafe_71c_u64;

        let tids = threads.keys().copied().collect::<Vec<_>>();
        for step in 0..128_u64 {
            for thread in threads.values_mut() {
                let demand_jitter = next_seeded_u32(&mut seed) % 40_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 1_500;
                thread.signature.projected_df_pressure_x100 =
                    80_000_u32.saturating_add(demand_jitter);
                thread.signature.projected_llc_pressure_x100 = 1_000_u32.saturating_add(llc_jitter);
            }

            let mut df_states = Vec::new();
            let mut llc_states = Vec::new();
            for _ in 0..topo.domains.len() {
                let df_jitter = next_seeded_u32(&mut seed) % 80_000;
                let llc_jitter = next_seeded_u32(&mut seed) % 600;
                df_states.push(df_state(1_450_000_u32.saturating_add(df_jitter)));
                llc_states.push(llc_state(
                    6_000_u32.saturating_add(llc_jitter),
                    HotCoolState::Hot,
                ));
            }

            let mut policy = policy();
            policy.migrate_margin_x100 = 100;
            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy,
                now_ns: 2_000_000 + step * 1_000_000,
                latest_df_sweep_epoch: 280 + step,
                pending_tids: threads.keys().copied().collect(),
                threads: threads.clone(),
                allowed_cpus: allowed_cpus.clone(),
                allowed_domains: allowed_domains.clone(),
                llc_states,
                df_states,
                previous_plan: Some(previous_plan.clone()),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let changed_tid = tids[(next_seeded_u32(&mut seed) as usize) % tids.len()];
            let triggers = if step == 0 {
                vec![PlannerTrigger::SweepComplete(280)]
            } else {
                vec![PlannerTrigger::SignatureChange(changed_tid)]
            };
            let output = compute_plan(&planner_config(), snapshot, &triggers, &mut move_trace);
            let (domain_changes, cpu_changes) = count_plan_changes(&previous_plan, &output.plan);

            assert_eq!(
                domain_changes, 0,
                "step={step} changed_tid={changed_tid} domain_changes={domain_changes} cpu_changes={cpu_changes}"
            );
            assert_eq!(
                cpu_changes, 0,
                "step={step} changed_tid={changed_tid} domain_changes={domain_changes} cpu_changes={cpu_changes}"
            );

            apply_plan_as_runtime_observation(&topo, &mut threads, &output.plan);
            previous_plan = output.plan;
        }
    }

    #[test]
    fn hot_df_tail_seeded_fuzz_keeps_affinity_until_hard_improvement() {
        let topo = load75_topology();
        let mapping = load75_mapping();
        let mut corpus_seed = 0x711a_2026_df00_0001_u64;

        for case_idx in 0..64_u32 {
            let case_seed = if case_idx == 0 {
                HOT_DF_TAIL_REPRO_SEED
            } else {
                (u64::from(next_seeded_u32(&mut corpus_seed)) << 32)
                    | u64::from(next_seeded_u32(&mut corpus_seed))
            };
            let knobs = tail_churn_fuzz_knobs(case_seed);
            let (mut threads, allowed_cpus, allowed_domains, mut previous_plan) =
                load75_tail_churn_seed_state(&topo, &mapping, &knobs);
            let tids = threads.keys().copied().collect::<Vec<_>>();
            let mut rng = knobs.case_seed ^ 0x9e37_79b9_7f4a_7c15;

            for step in 0..48_u64 {
                // This models the llama.cpp tail shape: all workload domains report
                // hot/near-over-capacity DF pressure, while the previous placement is
                // still legal. The global seed must keep legal source candidates in
                // play so small random residual differences do not turn into
                // whole-plan churn.
                for thread in threads.values_mut() {
                    let base = if thread.tid < 80_020 {
                        knobs.llama_demand_base_x100
                    } else {
                        knobs.sidecar_demand_base_x100
                    };
                    let jitter = if thread.tid < 80_020 {
                        knobs.llama_demand_jitter_x100
                    } else {
                        knobs.sidecar_demand_jitter_x100
                    };
                    thread.signature.projected_df_pressure_x100 =
                        base.saturating_add(next_seeded_range(&mut rng, jitter));
                    thread.signature.projected_llc_pressure_x100 =
                        800_u32.saturating_add(next_seeded_range(&mut rng, 2_000));
                }

                let mut df_states = Vec::new();
                let mut llc_states = Vec::new();
                for _ in 0..topo.domains.len() {
                    df_states.push(df_state(
                        knobs
                            .df_base_x100
                            .saturating_add(next_seeded_range(&mut rng, knobs.df_jitter_x100)),
                    ));
                    llc_states.push(llc_state(
                        knobs
                            .llc_base_x100
                            .saturating_add(next_seeded_range(&mut rng, knobs.llc_jitter_x100))
                            .min(10_000),
                        HotCoolState::Hot,
                    ));
                }

                let mut policy = policy();
                policy.migrate_margin_x100 = 100;
                let snapshot = PlannerInput {
                    topo: topo.clone(),
                    mapping: mapping.clone(),
                    policy,
                    now_ns: 2_000_000 + step * 1_000_000,
                    latest_df_sweep_epoch: 400 + step,
                    pending_tids: threads.keys().copied().collect(),
                    threads: threads.clone(),
                    allowed_cpus: allowed_cpus.clone(),
                    allowed_domains: allowed_domains.clone(),
                    llc_states,
                    df_states,
                    previous_plan: Some(previous_plan.clone()),
                };
                let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
                let changed_tid = tids[(next_seeded_u32(&mut rng) as usize) % tids.len()];
                let triggers = if step % knobs.sweep_every == 0 {
                    vec![PlannerTrigger::SweepComplete(400 + step)]
                } else {
                    vec![PlannerTrigger::SignatureChange(changed_tid)]
                };
                let output = compute_plan(&planner_config(), snapshot, &triggers, &mut move_trace);
                let (domain_changes, cpu_changes) =
                    count_plan_changes(&previous_plan, &output.plan);

                assert_eq!(
                    domain_changes, 0,
                    "tail churn repro case_idx={case_idx} step={step} changed_tid={changed_tid} domain_changes={domain_changes} cpu_changes={cpu_changes} knobs={knobs:?}"
                );
                assert_eq!(
                    cpu_changes, 0,
                    "tail churn repro case_idx={case_idx} step={step} changed_tid={changed_tid} domain_changes={domain_changes} cpu_changes={cpu_changes} knobs={knobs:?}"
                );

                apply_plan_as_runtime_observation(&topo, &mut threads, &output.plan);
                previous_plan = output.plan;
            }
        }
    }

    #[test]
    fn source_overload_seeded_fuzz_breaks_previous_domain_cpu_affinity() {
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let mut seed = 0x50ce_0a0d_f00d_u64;

        for case_idx in 0..64_u32 {
            let source_domain = next_seeded_u32(&mut seed) % topo.domains.len() as u32;
            let target_domain =
                (source_domain + 1 + (next_seeded_u32(&mut seed) % 3)) % topo.domains.len() as u32;
            let source_cpus = topo.domains[source_domain as usize].cpus.clone();
            let target_cpus = topo.domains[target_domain as usize].cpus.clone();
            let allowed = source_cpus
                .iter()
                .chain(target_cpus.iter())
                .copied()
                .collect::<BTreeSet<_>>();
            let allowed_domain_set = BTreeSet::from([source_domain, target_domain]);
            let tids = [30_000 + case_idx * 10, 30_000 + case_idx * 10 + 1];

            let mut threads = BTreeMap::new();
            let mut allowed_cpus = BTreeMap::new();
            let mut allowed_domains = BTreeMap::new();
            let mut previous_entries = BTreeMap::new();
            for (tid, cpu) in tids.into_iter().zip(source_cpus.iter().copied().cycle()) {
                threads.insert(tid, traffic_thread(tid, source_domain, cpu, 500_000));
                allowed_cpus.insert(tid, allowed.clone());
                allowed_domains.insert(tid, allowed_domain_set.clone());
                previous_entries.insert(tid, previous_affinity_entry(source_domain, cpu));
            }

            let mut df_states = vec![df_state(200_000); topo.domains.len()];
            df_states[source_domain as usize] = df_state(2_400_000);
            df_states[target_domain as usize] = df_state(200_000);
            let snapshot = PlannerInput {
                topo: topo.clone(),
                mapping: mapping.clone(),
                policy: policy(),
                now_ns: 2_000_000,
                latest_df_sweep_epoch: 32,
                pending_tids: tids.into_iter().collect(),
                threads,
                allowed_cpus,
                allowed_domains,
                llc_states: vec![llc_state(1_000, HotCoolState::Cool); topo.domains.len()],
                df_states,
                previous_plan: Some(PlacementPlan {
                    built_from_sweep_epoch: 31,
                    plan_revision: 41,
                    planned_at_ns: 1_000_000,
                    entries: previous_entries,
                }),
            };
            let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
            let output = compute_plan(
                &planner_config(),
                snapshot,
                &[PlannerTrigger::SweepComplete(32)],
                &mut move_trace,
            );
            let moved = tids
                .into_iter()
                .filter(|tid| output.plan.entries[tid].target_domain != source_domain)
                .count();

            assert!(
                moved > 0,
                "case {case_idx} seed={seed:#x} source={source_domain} target={target_domain}"
            );
        }
    }

    #[test]
    fn hard_constraint_improvement_breaks_previous_domain_cpu_affinity() {
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let source_domain = 0;
        let target_domain = 1;
        let tids = [40_001_u32, 40_002, 40_003];
        let source_cpus = topo.domains[source_domain as usize].cpus.clone();
        let target_cpus = topo.domains[target_domain as usize].cpus.clone();
        let allowed = source_cpus
            .iter()
            .chain(target_cpus.iter())
            .copied()
            .collect::<BTreeSet<_>>();
        let allowed_domain_set = BTreeSet::from([source_domain, target_domain]);

        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        for (tid, cpu) in tids.into_iter().zip(source_cpus.iter().copied().cycle()) {
            threads.insert(tid, traffic_thread(tid, source_domain, cpu, 100_000));
            allowed_cpus.insert(tid, allowed.clone());
            allowed_domains.insert(tid, allowed_domain_set.clone());
            previous_entries.insert(tid, previous_affinity_entry(source_domain, cpu));
        }

        let snapshot = PlannerInput {
            topo: topo.clone(),
            mapping,
            policy: policy(),
            now_ns: 2_000_000,
            latest_df_sweep_epoch: 42,
            pending_tids: tids.into_iter().collect(),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![llc_state(1_000, HotCoolState::Cool); topo.domains.len()],
            df_states: vec![df_state(100_000); topo.domains.len()],
            previous_plan: Some(PlacementPlan {
                built_from_sweep_epoch: 41,
                plan_revision: 51,
                planned_at_ns: 1_000_000,
                entries: previous_entries,
            }),
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SweepComplete(42)],
            &mut move_trace,
        );
        let mut domain_counts = BTreeMap::<u32, usize>::new();
        for entry in output.plan.entries.values() {
            *domain_counts.entry(entry.target_domain).or_default() += 1;
        }

        assert_eq!(
            domain_counts.get(&source_domain).copied().unwrap_or(0),
            source_cpus.len()
        );
        assert_eq!(domain_counts.get(&target_domain).copied().unwrap_or(0), 1);
    }

    #[test]
    #[cfg(not(feature = "scheduler-paper-greedy"))]
    fn migration_margin_exceeded_breaks_previous_domain_cpu_affinity() {
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let source_domain = 0;
        let target_domain = 1;
        let tid = 50_001_u32;
        let source_cpu = topo.domains[source_domain as usize].cpus[0];
        let target_cpu = topo.domains[target_domain as usize].cpus[0];
        let allowed = BTreeSet::from([source_cpu, target_cpu]);
        let mut policy = policy();
        policy.migrate_margin_x100 = 1_000;

        let snapshot = PlannerInput {
            topo: topo.clone(),
            mapping,
            policy,
            now_ns: 2_000_000,
            latest_df_sweep_epoch: 52,
            pending_tids: BTreeSet::from([tid]),
            threads: BTreeMap::from([(
                tid,
                traffic_thread(tid, source_domain, source_cpu, 100_000),
            )]),
            allowed_cpus: BTreeMap::from([(tid, allowed)]),
            allowed_domains: BTreeMap::from([(
                tid,
                BTreeSet::from([source_domain, target_domain]),
            )]),
            llc_states: vec![
                llc_state(8_000, HotCoolState::Hot),
                llc_state(1_000, HotCoolState::Cool),
                llc_state(1_000, HotCoolState::Cool),
                llc_state(1_000, HotCoolState::Cool),
            ],
            df_states: vec![df_state(0), df_state(0), df_state(0), df_state(0)],
            previous_plan: Some(PlacementPlan {
                built_from_sweep_epoch: 51,
                plan_revision: 61,
                planned_at_ns: 1_000_000,
                entries: BTreeMap::from([(
                    tid,
                    previous_affinity_entry(source_domain, source_cpu),
                )]),
            }),
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SweepComplete(52)],
            &mut move_trace,
        );
        let entry = &output.plan.entries[&tid];

        assert_eq!(entry.target_domain, target_domain);
        assert_eq!(entry.target_cpu, Some(target_cpu));
    }

    // Verifies the paper-level MegaCFlow behavior for a synchronized group
    // whose aggregate bandwidth demand exceeds a single chiplet path. Expected:
    // the planner decomposes/splits the group across multiple less-contended
    // domains instead of forcing every member to remain co-located.
    #[test]
    #[cfg(feature = "scheduler-paper-greedy")]
    fn planner_pdf_megacflow_over_capacity_splits_group() {
        let source_domain = 0;
        let secondary_domain = 1;
        let tgid = 9_000;
        let tids = [901u32, 902, 903, 904];
        let now_ns = 10_000_000;
        let sweep_epoch = 11;
        let topo = ccd_topology();
        let mapping = ccd_mapping();
        let allowed_cpus_for_group = BTreeSet::from([0, 1, 7, 8]);
        let allowed_domains_for_group = BTreeSet::from([source_domain, secondary_domain]);
        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        for (tid, cpu) in tids.into_iter().zip([0u32, 1, 0, 1]) {
            // Raw mock traffic label: each synchronized thread contributes
            // 7,000 MiB/s. The 28,000 MiB/s aggregate exceeds one 20,000 MiB/s
            // policy-capacity path, while a 2/2 split would keep both domains
            // below capacity.
            threads.insert(
                tid,
                sync_traffic_thread(tid, tgid, source_domain, cpu, 7_000_00),
            );
            allowed_cpus.insert(tid, allowed_cpus_for_group.clone());
            allowed_domains.insert(tid, allowed_domains_for_group.clone());
        }
        let mut config = planner_config();
        config.sync_tgid_overrides = BTreeSet::from([tgid]);
        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: policy(),
            now_ns,
            latest_df_sweep_epoch: sweep_epoch,
            pending_tids: BTreeSet::from(tids),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![
                llc_state(10_00, HotCoolState::Cool),
                llc_state(10_00, HotCoolState::Cool),
                llc_state(10_00, HotCoolState::Cool),
                llc_state(10_00, HotCoolState::Cool),
            ],
            df_states: vec![df_state(0), df_state(0), df_state(0), df_state(0)],
            previous_plan: None,
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &config,
            snapshot,
            &[PlannerTrigger::SweepComplete(sweep_epoch)],
            &mut move_trace,
        );
        let mut domain_counts = BTreeMap::<u32, usize>::new();
        for entry in output.plan.entries.values() {
            *domain_counts.entry(entry.target_domain).or_default() += 1;
        }

        assert_eq!(output.plan.entries.len(), tids.len());
        assert_eq!(domain_counts.get(&source_domain).copied().unwrap_or(0), 2);
        assert_eq!(
            domain_counts.get(&secondary_domain).copied().unwrap_or(0),
            2
        );
    }

    #[test]
    fn dedicated_control_plane_cpu_uses_first_excluded_domain_rep() {
        let topo = TopologyLayout {
            nr_cpu_ids: 4,
            domains: vec![],
            cpu_to_domain: vec![],
        };
        let mapping = MappingInfo {
            domain_to_ccx: vec![],
            domain_to_ccm: vec![],
            domain_to_df_capacity_mib_s_x100: vec![],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::new(),
            excluded_domains: BTreeSet::from([2]),
            eligible_cpus: BTreeSet::new(),
        };
        let err =
            maybe_dedicated_control_plane_cpu(ControlPlaneCpuPolicy::Dedicated, &topo, &mapping);
        assert!(err.is_err());
    }

    #[test]
    fn incremental_limit_rebuilds_full_global_scope() {
        let topo = TopologyLayout {
            nr_cpu_ids: 4,
            domains: vec![
                crate::types::DomainInfo {
                    domain_id: 0,
                    kernel_l3_id: 0,
                    rep_cpu: 0,
                    cpus: vec![0, 1],
                    l3_size_mb: 32.0,
                },
                crate::types::DomainInfo {
                    domain_id: 1,
                    kernel_l3_id: 1,
                    rep_cpu: 2,
                    cpus: vec![2, 3],
                    l3_size_mb: 32.0,
                },
            ],
            cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1)],
        };
        let mapping = MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
        };
        let mut threads = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut previous_entries = BTreeMap::new();
        for (tid, domain) in [(11u32, 0u32), (12, 0), (13, 1)] {
            threads.insert(
                tid,
                ManagedThreadState {
                    tid,
                    tgid: tid,
                    last_seen_ns: 10_000_000,
                    last_observed_domain: Some(domain),
                    last_selected_domain: Some(domain),
                    ..ManagedThreadState::default()
                },
            );
            allowed_domains.insert(tid, BTreeSet::from([0, 1]));
            allowed_cpus.insert(tid, BTreeSet::from([0, 1, 2, 3]));
            previous_entries.insert(
                tid,
                PlacementPlanEntry {
                    target_domain: domain,
                    target_cpu: None,
                    built_from_sweep_epoch: 1,
                    plan_revision: 1,
                    planned_at_ns: 9_000_000,
                    sync_group_id: None,
                    sync_anchor_domain: None,
                    sync_override: false,
                },
            );
        }
        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: crate::types::PolicyConfig {
                l2_need_mib_s_x100: 0,
                migrate_margin_x100: 0,
                cpu_high_util_x100: 8_500,
                cpu_rebalance_job_delta: 2,
                stall_victim_min_pct_x100: 5_000,
                stall_victim_delta_pct_x100: 1_500,
                llc_stale_ms: 100,
                df_stale_ms: 100,
                migrate_settle_ms: 80,
                signature_snapshots: false,
                cs_villain_throttle: true,
                tick_reeval_every: 1,
                tick_defer_max: 1,
                cs_villain_reslice_ns: 1_000_000,
                cs_villain_refill_divisor: 4,
                cs_villain_settle_ns: 20_000_000,
                cs_villain_release_samples: 2,
                tick_move_phase_mod: 1,
            },
            now_ns: 10_000_000,
            latest_df_sweep_epoch: 2,
            pending_tids: BTreeSet::from([11, 12, 13]),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![None, None],
            df_states: vec![None, None],
            previous_plan: Some(PlacementPlan {
                built_from_sweep_epoch: 1,
                plan_revision: 1,
                planned_at_ns: 9_000_000,
                entries: previous_entries,
            }),
        };
        let config = PlannerConfig {
            disable_auto_sync_hints: false,
            sync_tgid_overrides: BTreeSet::new(),
            incremental_item_limit: 0,
            incremental_domain_limit: 0,
            max_passes: 1,
            swap_pass_items: 0,
            debounce: Duration::from_millis(2),
            move_trace_path: None,
            plan_debug_path: None,
        };

        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &config,
            snapshot,
            &[PlannerTrigger::RunnableDelta(11)],
            &mut move_trace,
        );

        assert!(output.was_global);
        assert_eq!(output.plan.entries.len(), 3);
        assert!(output.plan.entries.contains_key(&12));
        assert!(output.plan.entries.contains_key(&13));
    }

    #[test]
    fn planner_spills_from_full_domain_into_domain_with_free_cpu_slots() {
        let topo = TopologyLayout {
            nr_cpu_ids: 4,
            domains: vec![
                crate::types::DomainInfo {
                    domain_id: 0,
                    kernel_l3_id: 0,
                    rep_cpu: 0,
                    cpus: vec![0, 1],
                    l3_size_mb: 32.0,
                },
                crate::types::DomainInfo {
                    domain_id: 1,
                    kernel_l3_id: 1,
                    rep_cpu: 2,
                    cpus: vec![2, 3],
                    l3_size_mb: 32.0,
                },
            ],
            cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1)],
        };
        let mapping = MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
        };
        let mut threads = BTreeMap::new();
        for tid in [11u32, 12, 13] {
            threads.insert(
                tid,
                ManagedThreadState {
                    tid,
                    tgid: 1_000,
                    last_seen_ns: 10_000_000,
                    last_observed_domain: Some(1),
                    last_selected_domain: Some(1),
                    ..ManagedThreadState::default()
                },
            );
        }
        let mut allowed_domains = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        for tid in [11u32, 12, 13] {
            allowed_domains.insert(tid, BTreeSet::from([0, 1]));
            allowed_cpus.insert(tid, BTreeSet::from([0, 1, 2, 3]));
        }
        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: crate::types::PolicyConfig {
                l2_need_mib_s_x100: 0,
                migrate_margin_x100: 0,
                cpu_high_util_x100: 8_500,
                cpu_rebalance_job_delta: 2,
                stall_victim_min_pct_x100: 5_000,
                stall_victim_delta_pct_x100: 1_500,
                llc_stale_ms: 100,
                df_stale_ms: 100,
                migrate_settle_ms: 80,
                signature_snapshots: false,
                cs_villain_throttle: true,
                tick_reeval_every: 1,
                tick_defer_max: 1,
                cs_villain_reslice_ns: 1_000_000,
                cs_villain_refill_divisor: 4,
                cs_villain_settle_ns: 20_000_000,
                cs_villain_release_samples: 2,
                tick_move_phase_mod: 1,
            },
            now_ns: 10_000_000,
            latest_df_sweep_epoch: 1,
            pending_tids: BTreeSet::from([11, 12, 13]),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![None, None],
            df_states: vec![None, None],
            previous_plan: None,
        };
        let config = PlannerConfig {
            disable_auto_sync_hints: false,
            sync_tgid_overrides: BTreeSet::new(),
            incremental_item_limit: 64,
            incremental_domain_limit: 2,
            max_passes: 4,
            swap_pass_items: 16,
            debounce: Duration::from_millis(2),
            move_trace_path: None,
            plan_debug_path: None,
        };

        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &config,
            snapshot,
            &[PlannerTrigger::SweepComplete(1)],
            &mut move_trace,
        );
        let mut domain_counts = BTreeMap::<u32, usize>::new();
        for entry in output.plan.entries.values() {
            *domain_counts.entry(entry.target_domain).or_default() += 1;
        }

        assert_eq!(domain_counts.get(&0).copied().unwrap_or(0), 1);
        assert_eq!(domain_counts.get(&1).copied().unwrap_or(0), 2);
    }

    #[test]
    fn planner_uses_affinity_constrained_domain_capacity() {
        let topo = TopologyLayout {
            nr_cpu_ids: 4,
            domains: vec![
                crate::types::DomainInfo {
                    domain_id: 0,
                    kernel_l3_id: 0,
                    rep_cpu: 0,
                    cpus: vec![0, 1],
                    l3_size_mb: 32.0,
                },
                crate::types::DomainInfo {
                    domain_id: 1,
                    kernel_l3_id: 1,
                    rep_cpu: 2,
                    cpus: vec![2, 3],
                    l3_size_mb: 32.0,
                },
            ],
            cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1)],
        };
        let mapping = MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
        };
        let mut threads = BTreeMap::new();
        for tid in [21u32, 22, 23] {
            threads.insert(
                tid,
                ManagedThreadState {
                    tid,
                    tgid: 2_000,
                    last_seen_ns: 10_000_000,
                    last_observed_domain: Some(1),
                    last_selected_domain: Some(1),
                    ..ManagedThreadState::default()
                },
            );
        }
        let mut allowed_domains = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        for tid in [21u32, 22, 23] {
            allowed_domains.insert(tid, BTreeSet::from([0, 1]));
            allowed_cpus.insert(tid, BTreeSet::from([0, 1, 2]));
        }
        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: crate::types::PolicyConfig {
                l2_need_mib_s_x100: 0,
                migrate_margin_x100: 0,
                cpu_high_util_x100: 8_500,
                cpu_rebalance_job_delta: 2,
                stall_victim_min_pct_x100: 5_000,
                stall_victim_delta_pct_x100: 1_500,
                llc_stale_ms: 100,
                df_stale_ms: 100,
                migrate_settle_ms: 80,
                signature_snapshots: false,
                cs_villain_throttle: true,
                tick_reeval_every: 1,
                tick_defer_max: 1,
                cs_villain_reslice_ns: 1_000_000,
                cs_villain_refill_divisor: 4,
                cs_villain_settle_ns: 20_000_000,
                cs_villain_release_samples: 2,
                tick_move_phase_mod: 1,
            },
            now_ns: 10_000_000,
            latest_df_sweep_epoch: 1,
            pending_tids: BTreeSet::from([21, 22, 23]),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![None, None],
            df_states: vec![None, None],
            previous_plan: None,
        };
        let config = PlannerConfig {
            disable_auto_sync_hints: false,
            sync_tgid_overrides: BTreeSet::new(),
            incremental_item_limit: 64,
            incremental_domain_limit: 2,
            max_passes: 4,
            swap_pass_items: 16,
            debounce: Duration::from_millis(2),
            move_trace_path: None,
            plan_debug_path: None,
        };

        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &config,
            snapshot,
            &[PlannerTrigger::SweepComplete(1)],
            &mut move_trace,
        );
        let mut domain_counts = BTreeMap::<u32, usize>::new();
        for entry in output.plan.entries.values() {
            *domain_counts.entry(entry.target_domain).or_default() += 1;
        }

        assert_eq!(domain_counts.get(&0).copied().unwrap_or(0), 2);
        assert_eq!(domain_counts.get(&1).copied().unwrap_or(0), 1);
    }

    #[test]
    fn planner_does_not_let_sidecar_affinity_inflate_workload_slots() {
        let topo = TopologyLayout {
            nr_cpu_ids: 6,
            domains: vec![
                crate::types::DomainInfo {
                    domain_id: 0,
                    kernel_l3_id: 0,
                    rep_cpu: 0,
                    cpus: vec![0, 1, 2],
                    l3_size_mb: 32.0,
                },
                crate::types::DomainInfo {
                    domain_id: 1,
                    kernel_l3_id: 1,
                    rep_cpu: 3,
                    cpus: vec![3, 4, 5],
                    l3_size_mb: 32.0,
                },
            ],
            cpu_to_domain: vec![Some(0), Some(0), Some(0), Some(1), Some(1), Some(1)],
        };
        let mapping = MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1, 2, 3, 4, 5]),
        };
        let workload_allowed_cpus = BTreeSet::from([0, 1, 3, 4]);
        let sidecar_allowed_cpus = BTreeSet::from([2, 5]);
        let allowed_domain_set = BTreeSet::from([0, 1]);
        let workload_tids = [101_u32, 102, 103];
        let sidecar_tid = 201_u32;

        let mut threads = BTreeMap::new();
        let mut allowed_cpus = BTreeMap::new();
        let mut allowed_domains = BTreeMap::new();
        for (idx, tid) in workload_tids.iter().copied().enumerate() {
            threads.insert(tid, traffic_thread(tid, 0, idx as u32 % 2, 50_000));
            allowed_cpus.insert(tid, workload_allowed_cpus.clone());
            allowed_domains.insert(tid, allowed_domain_set.clone());
        }
        threads.insert(sidecar_tid, traffic_thread(sidecar_tid, 1, 5, 1));
        allowed_cpus.insert(sidecar_tid, sidecar_allowed_cpus);
        allowed_domains.insert(sidecar_tid, allowed_domain_set);

        let snapshot = PlannerInput {
            topo,
            mapping,
            policy: policy(),
            now_ns: 10_000_000,
            latest_df_sweep_epoch: 1,
            pending_tids: workload_tids.into_iter().chain([sidecar_tid]).collect(),
            threads,
            allowed_cpus,
            allowed_domains,
            llc_states: vec![
                llc_state(1_000, HotCoolState::Cool),
                llc_state(1_000, HotCoolState::Cool),
            ],
            df_states: vec![df_state(100_000), df_state(1_800_000)],
            previous_plan: None,
        };
        let mut move_trace = PlannerMoveTraceLogger::new(None).unwrap();
        let output = compute_plan(
            &planner_config(),
            snapshot,
            &[PlannerTrigger::SweepComplete(1)],
            &mut move_trace,
        );

        let mut workload_counts = BTreeMap::<u32, usize>::new();
        let mut workload_cpus_by_domain = BTreeMap::<u32, BTreeSet<u32>>::new();
        for tid in workload_tids {
            let entry = output.plan.entries.get(&tid).expect("workload plan entry");
            *workload_counts.entry(entry.target_domain).or_default() += 1;
            if let Some(cpu) = entry.target_cpu {
                assert!(
                    workload_cpus_by_domain
                        .entry(entry.target_domain)
                        .or_default()
                        .insert(cpu),
                    "duplicate workload target CPU in domain {}: {:?}",
                    entry.target_domain,
                    output.plan.entries
                );
            }
        }

        assert!(
            workload_counts.values().all(|count| *count <= 2),
            "sidecar-only CPUs must not inflate workload capacity: {workload_counts:?}"
        );
    }
}

#[cfg(all(test, feature = "scheduler-paper-greedy"))]
#[path = "../planner_paper_tests.rs"]
mod paper_tests;
