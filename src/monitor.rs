use crate::types::{
    CcmDfStateValue, DecisionRecord, HotCoolState, LlcStateValue, ManagedThreadState, MappingInfo,
    TickStatsSnapshot, TopologyLayout, MEM_SOURCE_COUNT, MEM_SOURCE_DRAM_NEAR,
};
use std::collections::{BTreeMap, VecDeque};

const IO_CS_MONITOR_MIN_INGRESS_X100: u32 = 100_00;
const IO_CS_MONITOR_INGRESS_SCALE_NUMERATOR: u64 = 7;
const IO_CS_MONITOR_INGRESS_SCALE_DENOMINATOR: u64 = 8;

#[derive(Clone, Copy, Debug, Default)]
struct IoCsLinkSummary {
    predicted_bw: u32,
    integral_sum: u32,
    contributors: u32,
    top_tid: Option<u32>,
    top_bw: u32,
    top_evidence_bw: u32,
    top_ingress_bw: u32,
    top_scaled_ingress_bw: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct IoCsTopProjection {
    top_bw: u32,
    ingress_bw: u32,
    scaled_ingress_bw: u32,
}

fn state_name(state: u32) -> &'static str {
    if state == HotCoolState::Hot as u32 {
        "hot"
    } else {
        "cool"
    }
}

fn format_llc(value: Option<LlcStateValue>) -> String {
    match value {
        Some(value) if value.valid != 0 => format!(
            "llc:{} raw={:.2} ewma={:.2}",
            state_name(value.state),
            value.raw_pressure_pct_x100 as f64 / 100.0,
            value.ewma_pressure_pct_x100 as f64 / 100.0,
        ),
        _ => "llc:missing".to_string(),
    }
}

fn format_df(value: Option<CcmDfStateValue>) -> String {
    match value {
        Some(value) if value.valid != 0 => format!(
            "df:{} bw={:.2} raw={:.2} ewma={:.2}",
            state_name(value.state),
            (value
                .raw_read_bw_mib_s_x100
                .saturating_add(value.raw_write_bw_mib_s_x100)) as f64
                / 100.0,
            value.raw_pressure_pct_x100 as f64 / 100.0,
            value.ewma_pressure_pct_x100 as f64 / 100.0,
        ),
        _ => "df:missing".to_string(),
    }
}

fn format_optional_u32(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "na".to_string())
}

fn cs_link_bw_x100(value: Option<CcmDfStateValue>) -> u32 {
    value
        .filter(|value| value.valid != 0)
        .map(|value| {
            value
                .raw_read_bw_mib_s_x100
                .saturating_add(value.raw_write_bw_mib_s_x100)
        })
        .unwrap_or(0)
}

fn total_fill_bw_x100(fill_bw: &[u32; MEM_SOURCE_COUNT]) -> u32 {
    fill_bw
        .iter()
        .copied()
        .fold(0u32, |acc, value| acc.saturating_add(value))
}

fn io_cs_live_ingress_x100(thread: &ManagedThreadState) -> u32 {
    let dram_near = thread.last_fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR]
        .max(thread.io_cs_signature.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR]);
    if dram_near > 0 {
        dram_near
    } else {
        total_fill_bw_x100(&thread.last_fill_bw_mib_s_x100).max(total_fill_bw_x100(
            &thread.io_cs_signature.fill_bw_mib_s_x100,
        ))
    }
}

fn projected_io_cs_top_bw_x100(
    thread: &ManagedThreadState,
    evidence_bw_x100: u32,
    current_bw_x100: u32,
) -> IoCsTopProjection {
    let ingress_x100 = io_cs_live_ingress_x100(thread);
    if ingress_x100 < IO_CS_MONITOR_MIN_INGRESS_X100 {
        return IoCsTopProjection {
            top_bw: evidence_bw_x100,
            ingress_bw: ingress_x100,
            scaled_ingress_bw: 0,
        };
    }
    let scaled_ingress_x100 = ((u64::from(ingress_x100) * IO_CS_MONITOR_INGRESS_SCALE_NUMERATOR)
        / IO_CS_MONITOR_INGRESS_SCALE_DENOMINATOR)
        .min(u64::from(u32::MAX)) as u32;
    let scaled_ingress_bw = scaled_ingress_x100.min(current_bw_x100);
    IoCsTopProjection {
        top_bw: evidence_bw_x100.max(scaled_ingress_bw),
        ingress_bw: ingress_x100,
        scaled_ingress_bw,
    }
}

fn io_cs_link_summary(
    link_id: usize,
    current_bw_x100: u32,
    threads: &BTreeMap<u32, ManagedThreadState>,
) -> IoCsLinkSummary {
    let mut summary = IoCsLinkSummary::default();
    let mut top_evidence_bw = 0u32;
    for thread in threads.values() {
        if !thread.io_cs_signature.valid {
            continue;
        }
        let Some(contribution) = thread
            .io_cs_signature
            .df_domain_delta_x100
            .get(link_id)
            .copied()
        else {
            continue;
        };
        if contribution == 0 {
            continue;
        }
        summary.contributors = summary.contributors.saturating_add(1);
        summary.integral_sum = summary.integral_sum.saturating_add(contribution);
        if contribution >= top_evidence_bw {
            top_evidence_bw = contribution;
            summary.top_tid = Some(thread.tid);
        }
    }

    let Some(top_tid) = summary.top_tid else {
        return summary;
    };
    let Some(top_thread) = threads.get(&top_tid) else {
        return summary;
    };
    let top_projection = projected_io_cs_top_bw_x100(top_thread, top_evidence_bw, current_bw_x100);
    summary.top_bw = top_projection.top_bw;
    summary.top_evidence_bw = top_evidence_bw;
    summary.top_ingress_bw = top_projection.ingress_bw;
    summary.top_scaled_ingress_bw = top_projection.scaled_ingress_bw;
    summary.predicted_bw = summary.top_bw;
    summary
}

pub fn render(
    queue_depth: usize,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
    llc_states: &[Option<LlcStateValue>],
    df_states: &[Option<CcmDfStateValue>],
    df_cs_states: Option<&[Option<CcmDfStateValue>]>,
    threads: &BTreeMap<u32, ManagedThreadState>,
    tick_stats: TickStatsSnapshot,
    recent: &VecDeque<DecisionRecord>,
) -> String {
    let mut per_domain_tasks = vec![0usize; topo.domains.len()];
    for thread in threads.values() {
        if let Some(domain) = thread.last_selected_domain.or(thread.last_observed_domain) {
            if let Some(slot) = per_domain_tasks.get_mut(domain as usize) {
                *slot += 1;
            }
        }
    }

    let mut lines = Vec::new();
    lines.push(format!("monitor runnable_queue_depth={queue_depth}"));
    lines.push(format!(
        "tick_stats events={} to_userspace={} backoff_skip={} fastpath_stay={} reslice={} reenqueue_local_fail={} villain_reslices={}",
        tick_stats.tick_events,
        tick_stats.tick_to_userspace,
        tick_stats.tick_backoff_skip,
        tick_stats.tick_fastpath_stay,
        tick_stats.tick_reslice,
        tick_stats.reenqueue_local_fail,
        tick_stats.villain_reslices,
    ));
    lines.push(format!(
        "dispatch_path ringbuf_drains={} task_missing={} cpu_insert={} cpu_consume={} shared_insert={} shared_consume={} sched_consume={} cpu_kick={} stale_rescue={} user={} kernel={} cancel={} bounce={} failed={} congested={}",
        tick_stats.dispatched_ringbuf_drains,
        tick_stats.dispatch_task_missing,
        tick_stats.dispatch_cpu_inserts,
        tick_stats.dispatch_cpu_consumes,
        tick_stats.dispatch_shared_inserts,
        tick_stats.dispatch_shared_consumes,
        tick_stats.dispatch_sched_consumes,
        tick_stats.dispatch_cpu_kicks,
        tick_stats.stale_dispatch_rescues,
        tick_stats.user_dispatches,
        tick_stats.kernel_dispatches,
        tick_stats.cancel_dispatches,
        tick_stats.bounce_dispatches,
        tick_stats.failed_dispatches,
        tick_stats.sched_congested,
    ));
    for domain in &topo.domains {
        let eligible = mapping.eligible_domains.contains(&domain.domain_id);
        lines.push(format!(
            "domain={} eligible={} tasks={} {} {}",
            domain.domain_id,
            u32::from(eligible),
            per_domain_tasks[domain.domain_id as usize],
            format_llc(llc_states.get(domain.domain_id as usize).copied().flatten()),
            format_df(df_states.get(domain.domain_id as usize).copied().flatten()),
        ));
    }
    if let Some(df_cs_states) = df_cs_states {
        for (link_id, state) in df_cs_states.iter().enumerate() {
            lines.push(format!(
                "cs_link={} {}",
                link_id,
                format_df(state.as_ref().copied()),
            ));
            let current_bw = cs_link_bw_x100(state.as_ref().copied());
            let summary = io_cs_link_summary(link_id, current_bw, threads);
            let ratio = if current_bw > 0 {
                ((u64::from(summary.predicted_bw) * 10_000) / u64::from(current_bw))
                    .min(u64::from(u32::MAX)) as u32
            } else {
                0
            };
            lines.push(format!(
                "io_cs_sum link={} predicted_bw={} current_bw={} ratio={} integral_sum={} contributors={} top_tid={} top_bw={} top_evidence_bw={} top_ingress_bw={} top_scaled_ingress_bw={}",
                link_id,
                summary.predicted_bw,
                current_bw,
                ratio,
                summary.integral_sum,
                summary.contributors,
                format_optional_u32(summary.top_tid),
                summary.top_bw,
                summary.top_evidence_bw,
                summary.top_ingress_bw,
                summary.top_scaled_ingress_bw,
            ));
        }
    }
    for decision in recent.iter().rev().take(8) {
        lines.push(format!(
            "recent ts_ns={} tid={} class={} trigger={} tick_seq={} tick_decision={} {}:{} -> {}:{} reason={}",
            decision.ts_ns,
            decision.tid,
            decision.class.as_str(),
            decision.trigger.as_str(),
            decision.tick_seq,
            decision.tick_decision.as_str(),
            format_optional_u32(decision.source_domain),
            format_optional_u32(decision.source_cpu),
            format_optional_u32(decision.selected_domain),
            format_optional_u32(decision.selected_cpu),
            decision.reason.as_str(),
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ThreadSignature, MAX_DOMAINS};

    fn thread_with_io_cs_evidence(
        tid: u32,
        link_id: usize,
        evidence_x100: u32,
        dram_near_x100: u32,
    ) -> ManagedThreadState {
        let mut df_domain_delta_x100 = [0u32; MAX_DOMAINS];
        df_domain_delta_x100[link_id] = evidence_x100;
        let mut last_fill_bw_mib_s_x100 = [0u32; MEM_SOURCE_COUNT];
        last_fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR] = dram_near_x100;
        ManagedThreadState {
            tid,
            last_fill_bw_mib_s_x100,
            io_cs_signature: ThreadSignature {
                valid: true,
                stable: true,
                df_domain_delta_x100,
                projected_df_pressure_x100: evidence_x100,
                ..ThreadSignature::default()
            },
            ..ManagedThreadState::default()
        }
    }

    #[test]
    fn io_cs_summary_reports_projected_top_without_adding_residual_evidence() {
        let mut threads = BTreeMap::new();
        threads.insert(11, thread_with_io_cs_evidence(11, 0, 1_200_00, 4_000_00));
        threads.insert(22, thread_with_io_cs_evidence(22, 0, 1_000_00, 8_000_00));

        let summary = io_cs_link_summary(0, 5_000_00, &threads);

        assert_eq!(summary.top_tid, Some(11));
        assert_eq!(summary.top_bw, 3_500_00);
        assert_eq!(summary.top_evidence_bw, 1_200_00);
        assert_eq!(summary.top_ingress_bw, 4_000_00);
        assert_eq!(summary.top_scaled_ingress_bw, 3_500_00);
        assert_eq!(summary.integral_sum, 2_200_00);
        assert_eq!(summary.predicted_bw, 3_500_00);
    }

    #[test]
    fn io_cs_summary_caps_projected_top_magnitude_at_current_link_bandwidth() {
        let mut threads = BTreeMap::new();
        threads.insert(11, thread_with_io_cs_evidence(11, 0, 1_200_00, 4_000_00));

        let summary = io_cs_link_summary(0, 3_000_00, &threads);

        assert_eq!(summary.top_tid, Some(11));
        assert_eq!(summary.top_bw, 3_000_00);
        assert_eq!(summary.top_evidence_bw, 1_200_00);
        assert_eq!(summary.top_ingress_bw, 4_000_00);
        assert_eq!(summary.top_scaled_ingress_bw, 3_000_00);
        assert_eq!(summary.integral_sum, 1_200_00);
        assert_eq!(summary.predicted_bw, 3_000_00);
    }

    #[test]
    fn io_cs_summary_uses_signature_ingress_when_last_sample_is_lower() {
        let mut threads = BTreeMap::new();
        let mut thread = thread_with_io_cs_evidence(11, 0, 1_200_00, 1_000_00);
        thread.io_cs_signature.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR] = 4_000_00;
        threads.insert(11, thread);

        let summary = io_cs_link_summary(0, 5_000_00, &threads);

        assert_eq!(summary.top_tid, Some(11));
        assert_eq!(summary.top_bw, 3_500_00);
        assert_eq!(summary.top_evidence_bw, 1_200_00);
        assert_eq!(summary.top_ingress_bw, 4_000_00);
        assert_eq!(summary.top_scaled_ingress_bw, 3_500_00);
        assert_eq!(summary.predicted_bw, 3_500_00);
    }
}
