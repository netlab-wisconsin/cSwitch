use crate::filter::is_stale;
use crate::types::{
    CcmDfStateValue, ManagedThreadState, MappingInfo, PolicyConfig, MAX_DOMAINS,
    VILLAIN_RESLICE_MIN_NS,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const VILLAIN_DEFER_UPPER_BOUND_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LinkContender {
    pub(crate) tid: u32,
    pub(crate) score: u32,
    pub(crate) contender_count: u32,
}

#[derive(Default)]
struct LinkContenders {
    candidate_count: u32,
    best: Option<LinkContender>,
    second_best: u32,
}

impl LinkContenders {
    fn push(&mut self, mut contender: LinkContender) {
        self.candidate_count = self.candidate_count.saturating_add(1);
        contender.contender_count = self.candidate_count;
        match self.best {
            None => self.best = Some(contender),
            Some(best) if contender.score >= best.score => {
                self.second_best = best.score;
                self.best = Some(contender);
            }
            _ if contender.score > self.second_best => self.second_best = contender.score,
            _ => {}
        }
    }

    fn winner(self) -> Option<LinkContender> {
        let mut contender = self.best?;
        if self.candidate_count < 2 {
            return None;
        }
        let threshold_met = if self.second_best > 0 {
            contender.score
                >= self
                    .second_best
                    .saturating_add(self.second_best / 10)
                    .max(self.second_best + 1)
        } else {
            contender.score >= 500
        };
        contender.contender_count = self.candidate_count;
        threshold_met.then_some(contender)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VillainThrottleAction {
    Defer { tokens_ns: u64 },
    Reslice { slice_ns: u64, tokens_ns: u64 },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VillainThrottleControl {
    pub(crate) link_id: u32,
    pub(crate) score: u32,
    pub(crate) action: VillainThrottleAction,
    pub(crate) token_capacity_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CsVillainLatchAction {
    Acquire,
    Hold,
    Release,
    Expire,
    Invalid,
}

impl CsVillainLatchAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Acquire => "acquire",
            Self::Hold => "hold",
            Self::Release => "release",
            Self::Expire => "expire",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CsVillainLatchEvent {
    pub(crate) action: CsVillainLatchAction,
    pub(crate) link_id: u32,
    pub(crate) tid: u32,
    pub(crate) score: u32,
    pub(crate) overload_x100: u32,
    pub(crate) sample_ts_ns: u64,
    pub(crate) hold_until_ns: u64,
    pub(crate) clear_samples: u32,
    pub(crate) pressure_epoch: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CsVillainLatchOutput {
    pub(crate) effective_villains: Vec<Option<LinkContender>>,
    pub(crate) events: Vec<CsVillainLatchEvent>,
}

pub(crate) trait CsVillainPressurePolicy {
    fn apply(
        &mut self,
        entry: &mut ManagedThreadState,
        current_villain: Option<(u32, LinkContender)>,
        is_tick_trigger: bool,
        now_ns: u64,
        policy: PolicyConfig,
    ) -> Option<VillainThrottleControl>;
}

#[derive(Default)]
pub(crate) struct TokenBucketPressurePolicy;

impl CsVillainPressurePolicy for TokenBucketPressurePolicy {
    fn apply(
        &mut self,
        entry: &mut ManagedThreadState,
        current_villain: Option<(u32, LinkContender)>,
        is_tick_trigger: bool,
        now_ns: u64,
        policy: PolicyConfig,
    ) -> Option<VillainThrottleControl> {
        maybe_apply_villain_token_throttle(entry, current_villain, is_tick_trigger, now_ns, policy)
    }
}

fn thread_io_cs_signature_valid(thread: &ManagedThreadState) -> bool {
    thread.io_cs_signature.valid
}

fn thread_villain_score(thread: &ManagedThreadState, link_id: u32) -> u32 {
    if !thread_io_cs_signature_valid(thread) {
        return 0;
    }
    thread
        .io_cs_signature
        .df_domain_delta_x100
        .get(link_id as usize)
        .copied()
        .unwrap_or(0)
}

fn live_df_state(
    df_states: &[Option<CcmDfStateValue>],
    domain_id: usize,
    now_ns: u64,
    cfg: PolicyConfig,
) -> Option<CcmDfStateValue> {
    df_states
        .get(domain_id)
        .and_then(|state| state.as_ref())
        .copied()
        .filter(|state| state.valid != 0 && !is_stale(state.sample_ts_ns, now_ns, cfg.df_stale_ms))
}

fn live_df_bw_x100(
    df_states: &[Option<CcmDfStateValue>],
    domain_id: usize,
    now_ns: u64,
    cfg: PolicyConfig,
) -> Option<u32> {
    let df = live_df_state(df_states, domain_id, now_ns, cfg)?;
    Some(
        df.raw_read_bw_mib_s_x100
            .saturating_add(df.raw_write_bw_mib_s_x100),
    )
}

pub(crate) fn live_df_overload_x100_with_capacity(
    df_states: &[Option<CcmDfStateValue>],
    index: usize,
    capacity_mib_s_x100: u32,
    now_ns: u64,
    cfg: PolicyConfig,
) -> u32 {
    live_df_bw_x100(df_states, index, now_ns, cfg)
        .unwrap_or(0)
        .saturating_sub(capacity_mib_s_x100)
}

#[derive(Clone, Copy, Debug, Default)]
struct LinkVillainLatch {
    contender: Option<LinkContender>,
    hold_until_ns: u64,
    clear_samples: u32,
    last_clear_sample_ts_ns: u64,
    last_hold_log_sample_ts_ns: u64,
    pressure_epoch: u64,
}

impl LinkVillainLatch {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub(crate) struct CsVillainLatchController {
    links: Vec<LinkVillainLatch>,
    next_pressure_epoch: u64,
}

impl CsVillainLatchController {
    fn ensure_link_count(&mut self, count: usize) {
        if self.links.len() < count {
            self.links.resize(count, LinkVillainLatch::default());
        }
    }

    fn next_epoch(&mut self) -> u64 {
        self.next_pressure_epoch = self.next_pressure_epoch.saturating_add(1);
        self.next_pressure_epoch
    }

    pub(crate) fn effective_villains(
        &mut self,
        mapping: &MappingInfo,
        fresh_villains: &[Option<LinkContender>],
        df_link_states: &[Option<CcmDfStateValue>],
        threads: &BTreeMap<u32, ManagedThreadState>,
        active_cutoff_ns: u64,
        now_ns: u64,
        policy: PolicyConfig,
    ) -> CsVillainLatchOutput {
        let link_count = df_link_states.len().min(MAX_DOMAINS);
        if !policy.cs_villain_throttle {
            for latch in &mut self.links {
                latch.clear();
            }
            let mut effective_villains = vec![None; link_count];
            for (idx, contender) in fresh_villains.iter().take(link_count).copied().enumerate() {
                effective_villains[idx] = contender;
            }
            return CsVillainLatchOutput {
                effective_villains,
                events: Vec::new(),
            };
        }

        self.ensure_link_count(link_count);
        let mut output = CsVillainLatchOutput {
            effective_villains: vec![None; link_count],
            events: Vec::new(),
        };

        for link_idx in 0..link_count {
            let link_id = link_idx as u32;
            let sample = live_df_state(df_link_states, link_idx, now_ns, policy);
            let sample_ts_ns = sample.map(|state| state.sample_ts_ns).unwrap_or(0);
            let overload_x100 = sample
                .map(|state| {
                    state
                        .raw_read_bw_mib_s_x100
                        .saturating_add(state.raw_write_bw_mib_s_x100)
                        .saturating_sub(mapping.cs_capacity_mib_s_x100(link_id))
                })
                .unwrap_or(0);
            let fresh = fresh_villains.get(link_idx).copied().flatten();
            let mut acquire_fresh = fresh;

            {
                let latch = &mut self.links[link_idx];
                if let Some(latched) = latch.contender {
                    let live = threads
                        .get(&latched.tid)
                        .map(|thread| thread.last_seen_ns >= active_cutoff_ns)
                        .unwrap_or(false);
                    if !live {
                        output.events.push(CsVillainLatchEvent {
                            action: CsVillainLatchAction::Invalid,
                            link_id,
                            tid: latched.tid,
                            score: latched.score,
                            overload_x100,
                            sample_ts_ns,
                            hold_until_ns: latch.hold_until_ns,
                            clear_samples: latch.clear_samples,
                            pressure_epoch: latch.pressure_epoch,
                        });
                        latch.clear();
                    } else if sample.is_some() && overload_x100 == 0 {
                        if sample_ts_ns != 0 && sample_ts_ns != latch.last_clear_sample_ts_ns {
                            latch.clear_samples = latch.clear_samples.saturating_add(1);
                            latch.last_clear_sample_ts_ns = sample_ts_ns;
                        }
                        if latch.clear_samples >= policy.cs_villain_release_samples.max(1) {
                            output.events.push(CsVillainLatchEvent {
                                action: CsVillainLatchAction::Release,
                                link_id,
                                tid: latched.tid,
                                score: latched.score,
                                overload_x100,
                                sample_ts_ns,
                                hold_until_ns: latch.hold_until_ns,
                                clear_samples: latch.clear_samples,
                                pressure_epoch: latch.pressure_epoch,
                            });
                            latch.clear();
                            acquire_fresh = None;
                        } else if now_ns >= latch.hold_until_ns {
                            output.events.push(CsVillainLatchEvent {
                                action: CsVillainLatchAction::Expire,
                                link_id,
                                tid: latched.tid,
                                score: latched.score,
                                overload_x100,
                                sample_ts_ns,
                                hold_until_ns: latch.hold_until_ns,
                                clear_samples: latch.clear_samples,
                                pressure_epoch: latch.pressure_epoch,
                            });
                            latch.clear();
                        } else {
                            output.effective_villains[link_idx] = Some(latched);
                            if sample_ts_ns != 0 && sample_ts_ns != latch.last_hold_log_sample_ts_ns
                            {
                                latch.last_hold_log_sample_ts_ns = sample_ts_ns;
                                output.events.push(CsVillainLatchEvent {
                                    action: CsVillainLatchAction::Hold,
                                    link_id,
                                    tid: latched.tid,
                                    score: latched.score,
                                    overload_x100,
                                    sample_ts_ns,
                                    hold_until_ns: latch.hold_until_ns,
                                    clear_samples: latch.clear_samples,
                                    pressure_epoch: latch.pressure_epoch,
                                });
                            }
                            continue;
                        }
                    } else if now_ns >= latch.hold_until_ns {
                        output.events.push(CsVillainLatchEvent {
                            action: CsVillainLatchAction::Expire,
                            link_id,
                            tid: latched.tid,
                            score: latched.score,
                            overload_x100,
                            sample_ts_ns,
                            hold_until_ns: latch.hold_until_ns,
                            clear_samples: latch.clear_samples,
                            pressure_epoch: latch.pressure_epoch,
                        });
                        latch.clear();
                    } else {
                        if sample.is_some() && overload_x100 > 0 {
                            latch.clear_samples = 0;
                            latch.last_clear_sample_ts_ns = 0;
                        }
                        output.effective_villains[link_idx] = Some(latched);
                        if sample_ts_ns != 0 && sample_ts_ns != latch.last_hold_log_sample_ts_ns {
                            latch.last_hold_log_sample_ts_ns = sample_ts_ns;
                            output.events.push(CsVillainLatchEvent {
                                action: CsVillainLatchAction::Hold,
                                link_id,
                                tid: latched.tid,
                                score: latched.score,
                                overload_x100,
                                sample_ts_ns,
                                hold_until_ns: latch.hold_until_ns,
                                clear_samples: latch.clear_samples,
                                pressure_epoch: latch.pressure_epoch,
                            });
                        }
                        continue;
                    }
                }
            }

            if let Some(contender) = acquire_fresh {
                let pressure_epoch = self.next_epoch();
                let latch = &mut self.links[link_idx];
                latch.contender = Some(contender);
                latch.hold_until_ns = now_ns.saturating_add(policy.cs_villain_settle_ns.max(1));
                latch.clear_samples = 0;
                latch.last_clear_sample_ts_ns = 0;
                latch.last_hold_log_sample_ts_ns = sample_ts_ns;
                latch.pressure_epoch = pressure_epoch;
                output.effective_villains[link_idx] = Some(contender);
                output.events.push(CsVillainLatchEvent {
                    action: CsVillainLatchAction::Acquire,
                    link_id,
                    tid: contender.tid,
                    score: contender.score,
                    overload_x100,
                    sample_ts_ns,
                    hold_until_ns: latch.hold_until_ns,
                    clear_samples: latch.clear_samples,
                    pressure_epoch,
                });
            }
        }

        output
    }
}

fn villain_token_capacity_ns(policy: PolicyConfig) -> u64 {
    policy
        .cs_villain_reslice_ns
        .max(VILLAIN_RESLICE_MIN_NS)
        .saturating_mul(u64::from(policy.tick_defer_max.max(1)))
}

fn villain_token_refill_divisor(policy: PolicyConfig) -> u64 {
    policy
        .cs_villain_refill_divisor
        .max(1)
        .saturating_mul(u64::from(policy.tick_reeval_every.max(1)))
        .max(1)
}

fn policy_villain_reslice_slice_ns(policy: PolicyConfig) -> u64 {
    policy.cs_villain_reslice_ns.max(VILLAIN_RESLICE_MIN_NS)
}

pub(crate) fn reset_villain_throttle(entry: &mut ManagedThreadState) {
    entry.throttle_link_id = None;
    entry.throttle_tokens_ns = 0;
    entry.throttle_last_refill_ns = 0;
    entry.throttle_defer_started_ns = 0;
    entry.consecutive_tick_defer = 0;
}

fn refill_villain_tokens(
    entry: &mut ManagedThreadState,
    link_id: u32,
    now_ns: u64,
    policy: PolicyConfig,
) {
    let capacity_ns = villain_token_capacity_ns(policy);
    if entry.throttle_link_id != Some(link_id) || entry.throttle_last_refill_ns == 0 {
        entry.throttle_link_id = Some(link_id);
        entry.throttle_tokens_ns = capacity_ns;
        entry.throttle_last_refill_ns = now_ns;
        entry.throttle_defer_started_ns = 0;
        entry.consecutive_tick_defer = 0;
        return;
    }

    let elapsed_ns = now_ns.saturating_sub(entry.throttle_last_refill_ns);
    let refill_ns = elapsed_ns / villain_token_refill_divisor(policy);
    entry.throttle_tokens_ns = entry
        .throttle_tokens_ns
        .saturating_add(refill_ns)
        .min(capacity_ns);
    entry.throttle_last_refill_ns = now_ns;
}

pub(crate) fn apply_villain_token_throttle(
    entry: &mut ManagedThreadState,
    link_id: u32,
    now_ns: u64,
    policy: PolicyConfig,
) -> VillainThrottleAction {
    refill_villain_tokens(entry, link_id, now_ns, policy);
    let slice_ns = policy_villain_reslice_slice_ns(policy);
    if entry.throttle_tokens_ns >= slice_ns {
        entry.throttle_tokens_ns = entry.throttle_tokens_ns.saturating_sub(slice_ns);
        entry.throttle_defer_started_ns = 0;
        entry.consecutive_tick_defer = 0;
        VillainThrottleAction::Reslice {
            slice_ns,
            tokens_ns: entry.throttle_tokens_ns,
        }
    } else {
        let defer_started_ns = if entry.throttle_defer_started_ns == 0 {
            entry.throttle_defer_started_ns = now_ns;
            now_ns
        } else {
            entry.throttle_defer_started_ns
        };
        if now_ns.saturating_sub(defer_started_ns) >= VILLAIN_DEFER_UPPER_BOUND_NS {
            entry.throttle_tokens_ns = 0;
            entry.throttle_defer_started_ns = 0;
            entry.consecutive_tick_defer = 0;
            return VillainThrottleAction::Reslice {
                slice_ns,
                tokens_ns: entry.throttle_tokens_ns,
            };
        }
        entry.consecutive_tick_defer = entry.consecutive_tick_defer.saturating_add(1);
        VillainThrottleAction::Defer {
            tokens_ns: entry.throttle_tokens_ns,
        }
    }
}

pub(crate) fn maybe_apply_villain_token_throttle(
    entry: &mut ManagedThreadState,
    current_villain: Option<(u32, LinkContender)>,
    is_tick_trigger: bool,
    now_ns: u64,
    policy: PolicyConfig,
) -> Option<VillainThrottleControl> {
    if current_villain.is_none() || !policy.cs_villain_throttle {
        reset_villain_throttle(entry);
    }
    if !(policy.signature_snapshots && policy.cs_villain_throttle && is_tick_trigger) {
        return None;
    }
    let (link_id, villain) = current_villain?;
    let token_capacity_ns = villain_token_capacity_ns(policy);
    Some(VillainThrottleControl {
        link_id,
        score: villain.score,
        action: apply_villain_token_throttle(entry, link_id, now_ns, policy),
        token_capacity_ns,
    })
}

#[cfg(test)]
pub(crate) fn choose_link_villains(
    mapping: &MappingInfo,
    threads: &BTreeMap<u32, ManagedThreadState>,
    victim_links: &BTreeSet<u32>,
    df_link_states: &[Option<CcmDfStateValue>],
    now_ns: u64,
    cfg: PolicyConfig,
) -> Vec<Option<LinkContender>> {
    choose_link_villains_with_fallbacks(
        mapping,
        threads,
        victim_links,
        df_link_states,
        &BTreeMap::new(),
        now_ns,
        cfg,
    )
}

pub(crate) fn choose_link_villains_with_fallbacks(
    mapping: &MappingInfo,
    threads: &BTreeMap<u32, ManagedThreadState>,
    victim_links: &BTreeSet<u32>,
    df_link_states: &[Option<CcmDfStateValue>],
    victim_fallbacks: &BTreeMap<u32, LinkContender>,
    now_ns: u64,
    cfg: PolicyConfig,
) -> Vec<Option<LinkContender>> {
    let mut out = vec![None; df_link_states.len().min(MAX_DOMAINS)];
    for &link_id in victim_links {
        let Some(slot) = out.get_mut(link_id as usize) else {
            continue;
        };
        if live_df_overload_x100_with_capacity(
            df_link_states,
            link_id as usize,
            mapping.cs_capacity_mib_s_x100(link_id),
            now_ns,
            cfg,
        ) == 0
        {
            continue;
        }
        let mut contenders = LinkContenders::default();
        for thread in threads.values() {
            let score = thread_villain_score(thread, link_id);
            if score == 0 {
                continue;
            }
            contenders.push(LinkContender {
                tid: thread.tid,
                score,
                contender_count: 0,
            });
        }
        if let Some(winner) = contenders
            .winner()
            .or_else(|| victim_fallbacks.get(&link_id).copied())
        {
            *slot = Some(winner);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HotCoolState, ThreadClass};

    fn policy() -> PolicyConfig {
        PolicyConfig {
            l2_need_mib_s_x100: 0,
            migrate_margin_x100: 0,
            cpu_high_util_x100: 85_00,
            cpu_rebalance_job_delta: 2,
            stall_victim_min_pct_x100: 50_00,
            stall_victim_delta_pct_x100: 15_00,
            llc_stale_ms: 100,
            df_stale_ms: 100,
            migrate_settle_ms: 80,
            signature_snapshots: true,
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

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: vec![0, 1],
            domain_to_ccm: vec![Some(0), Some(1)],
            domain_to_df_capacity_mib_s_x100: vec![Some(20_000_00), Some(20_000_00)],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains: BTreeSet::from([0, 1]),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::from([0, 1]),
        }
    }

    fn df_state(raw_total_centi_mib_per_s: u32, sample_ts_ns: u64) -> Option<CcmDfStateValue> {
        Some(CcmDfStateValue {
            sample_ts_ns,
            raw_read_bw_mib_s_x100: raw_total_centi_mib_per_s,
            raw_write_bw_mib_s_x100: 0,
            raw_pressure_pct_x100: raw_total_centi_mib_per_s,
            ewma_pressure_pct_x100: raw_total_centi_mib_per_s,
            state: HotCoolState::Hot as u32,
            valid: 1,
            ..CcmDfStateValue::default()
        })
    }

    fn contender(tid: u32, score: u32) -> LinkContender {
        LinkContender {
            tid,
            score,
            contender_count: 2,
        }
    }

    fn threads(tids: &[u32], last_seen_ns: u64) -> BTreeMap<u32, ManagedThreadState> {
        tids.iter()
            .copied()
            .map(|tid| {
                (
                    tid,
                    ManagedThreadState {
                        tid,
                        last_seen_ns,
                        last_class: ThreadClass::Congested,
                        ..ManagedThreadState::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn latch_holds_first_villain_during_settle_window() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let policy = policy();
        let threads = threads(&[11, 22], 10_000_000);
        let mut df_states = vec![df_state(25_000_00, 1_000_000), None];

        let first = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &df_states,
            &threads,
            0,
            2_000_000,
            policy,
        );
        assert_eq!(first.effective_villains[0], Some(contender(11, 900)));
        assert_eq!(first.events[0].action, CsVillainLatchAction::Acquire);

        df_states[0] = df_state(26_000_00, 2_000_000);
        let held = controller.effective_villains(
            &mapping,
            &[Some(contender(22, 1_200)), None],
            &df_states,
            &threads,
            0,
            3_000_000,
            policy,
        );
        assert_eq!(held.effective_villains[0], Some(contender(11, 900)));
        assert!(held
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Hold && event.tid == 11));
    }

    #[test]
    fn expired_latch_accepts_fresh_villain() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let policy = policy();
        let threads = threads(&[11, 22], 10_000_000);
        let df_states = vec![df_state(25_000_00, 1_000_000), None];

        let _ = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &df_states,
            &threads,
            0,
            2_000_000,
            policy,
        );
        let expired = controller.effective_villains(
            &mapping,
            &[Some(contender(22, 1_200)), None],
            &df_states,
            &threads,
            0,
            30_000_000,
            policy,
        );
        assert_eq!(expired.effective_villains[0], Some(contender(22, 1_200)));
        assert!(expired
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Expire && event.tid == 11));
        assert!(expired
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Acquire && event.tid == 22));
    }

    #[test]
    fn release_requires_unique_capacity_cleared_samples() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let policy = policy();
        let threads = threads(&[11], 10_000_000);

        let _ = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &[df_state(25_000_00, 1_000_000), None],
            &threads,
            0,
            2_000_000,
            policy,
        );
        let first_clear = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(15_000_00, 2_000_000), None],
            &threads,
            0,
            3_000_000,
            policy,
        );
        assert_eq!(first_clear.effective_villains[0], Some(contender(11, 900)));

        let repeated_sample = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(15_000_00, 2_000_000), None],
            &threads,
            0,
            4_000_000,
            policy,
        );
        assert_eq!(
            repeated_sample.effective_villains[0],
            Some(contender(11, 900))
        );
        assert!(!repeated_sample
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Release));

        let second_clear = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(15_000_00, 3_000_000), None],
            &threads,
            0,
            5_000_000,
            policy,
        );
        assert_eq!(second_clear.effective_villains[0], None);
        assert!(
            second_clear
                .events
                .iter()
                .any(|event| event.action == CsVillainLatchAction::Release
                    && event.clear_samples == 2)
        );
    }

    #[test]
    fn overload_return_resets_release_sample_count() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let policy = policy();
        let threads = threads(&[11], 10_000_000);

        let _ = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &[df_state(25_000_00, 1_000_000), None],
            &threads,
            0,
            2_000_000,
            policy,
        );
        let _ = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(15_000_00, 2_000_000), None],
            &threads,
            0,
            3_000_000,
            policy,
        );
        let overloaded = controller.effective_villains(
            &mapping,
            &[Some(contender(22, 1_200)), None],
            &[df_state(25_000_00, 3_000_000), None],
            &threads,
            0,
            4_000_000,
            policy,
        );
        assert_eq!(overloaded.effective_villains[0], Some(contender(11, 900)));

        let clear_again = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(15_000_00, 4_000_000), None],
            &threads,
            0,
            5_000_000,
            policy,
        );
        assert_eq!(clear_again.effective_villains[0], Some(contender(11, 900)));
        assert!(!clear_again
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Release));
    }

    #[test]
    fn disappeared_or_stale_latched_tid_invalidates_latch() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let policy = policy();
        let active_threads = threads(&[11], 10_000_000);

        let _ = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &[df_state(25_000_00, 1_000_000), None],
            &active_threads,
            0,
            2_000_000,
            policy,
        );
        let stale_threads = threads(&[11], 1_000_000);
        let invalid = controller.effective_villains(
            &mapping,
            &[None, None],
            &[df_state(25_000_00, 2_000_000), None],
            &stale_threads,
            5_000_000,
            3_000_000,
            policy,
        );
        assert_eq!(invalid.effective_villains[0], None);
        assert!(invalid
            .events
            .iter()
            .any(|event| event.action == CsVillainLatchAction::Invalid && event.tid == 11));
    }

    #[test]
    fn throttle_disabled_skips_latch_state() {
        let mut controller = CsVillainLatchController::default();
        let mapping = mapping();
        let mut policy = policy();
        let threads = threads(&[11, 22], 10_000_000);
        let df_states = vec![df_state(25_000_00, 1_000_000), None];

        let _ = controller.effective_villains(
            &mapping,
            &[Some(contender(11, 900)), None],
            &df_states,
            &threads,
            0,
            2_000_000,
            policy,
        );

        policy.cs_villain_throttle = false;
        let disabled = controller.effective_villains(
            &mapping,
            &[Some(contender(22, 1_200)), None],
            &df_states,
            &threads,
            0,
            3_000_000,
            policy,
        );
        assert_eq!(disabled.effective_villains[0], Some(contender(22, 1_200)));
        assert!(disabled.events.is_empty());

        policy.cs_villain_throttle = true;
        let reenabled = controller.effective_villains(
            &mapping,
            &[None, None],
            &df_states,
            &threads,
            0,
            4_000_000,
            policy,
        );
        assert_eq!(reenabled.effective_villains[0], None);
        assert!(reenabled.events.is_empty());
    }
}
