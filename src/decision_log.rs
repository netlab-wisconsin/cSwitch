use crate::types::DecisionRecord;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

fn fmt_optional_u32(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "na".to_string())
}

pub struct DecisionLogger {
    writer: Option<BufWriter<File>>,
    echo_stderr: bool,
}

impl DecisionLogger {
    pub fn new(path: Option<&str>, echo_stderr: bool) -> Result<Self> {
        let writer = match path {
            Some(path) => Some(BufWriter::new(
                File::create(Path::new(path))
                    .with_context(|| format!("failed to create decision log at {path}"))?,
            )),
            None => None,
        };
        Ok(Self {
            writer,
            echo_stderr,
        })
    }

    pub fn write(&mut self, record: &DecisionRecord) -> Result<()> {
        let select_cpu_debug_fields = select_cpu_in_domain_debug_fields(record);
        let line = format!(
            "ts_ns={} tid={} tgid={} comm={} src_cpu={} src_domain={} dst_cpu={} dst_domain={} class={} trigger={} tick_seq={} tick_decision={} fill_total_bw={} last_ipc={} ewma_ipc={} last_stall_pct={} ewma_stall_pct={} stall_delta_pct={} slice_ns={} villain_score={} src_df_cur={} src_df_pred={} dst_df_cur={} dst_df_pred={} src_llc_cur={} src_llc_pred={} dst_llc_cur={} dst_llc_pred={} signature_valid={} signature_sample_count={} signature_confidence={} signature_stable={} io_cs_signature_valid={} io_cs_signature_sample_count={} io_cs_signature_confidence={} io_cs_signature_stable={} io_cs_top_link={} io_cs_top_contrib={} io_cs_raw_top_link={} io_cs_raw_top_contrib={} live_src_df={} obs_dst_domain={} obs_dst_cpu={} obs_dst_live_df={} baseline_total_overload={} candidate_total_overload={} baseline_max_overload={} candidate_max_overload={} baseline_pair_penalty={} candidate_pair_penalty={} baseline_cost={} candidate_cost={} considered_candidates={} skip_missing_df={} skip_missing_llc={} skip_no_cpu={} skip_same_domain={} skip_budget={} blocked_destination={} blocked_phase={} blocked_reverse={} planner_epoch={} plan_revision={} plan_used={} fallback_reason={} planned_domain={} planned_cpu={} sync_group_id={} sync_anchor_domain={} sync_override={} control_plane_cpu_policy={} control_plane_cpu={}{} reason={}\n",
            record.ts_ns,
            record.tid,
            record.tgid,
            record.comm,
            fmt_optional_u32(record.source_cpu),
            fmt_optional_u32(record.source_domain),
            fmt_optional_u32(record.selected_cpu),
            fmt_optional_u32(record.selected_domain),
            record.class.as_str(),
            record.trigger.as_str(),
            record.tick_seq,
            record.tick_decision.as_str(),
            record.fill_total_bw_mib_s_x100,
            record.last_ipc_x1000,
            record.ewma_ipc_x1000,
            record.last_stall_pct_x100,
            record.ewma_stall_pct_x100,
            record.stall_delta_pct_x100,
            record.slice_ns,
            record.villain_score,
            record.current_source_df_x100,
            record.predicted_source_df_x100,
            record.current_destination_df_x100,
            record.predicted_destination_df_x100,
            record.current_source_llc_x100,
            record.predicted_source_llc_x100,
            record.current_destination_llc_x100,
            record.predicted_destination_llc_x100,
            record.signature_valid,
            record.signature_sample_count,
            record.signature_confidence_x100,
            record.signature_stable,
            record.io_cs_signature_valid,
            record.io_cs_signature_sample_count,
            record.io_cs_signature_confidence_x100,
            record.io_cs_signature_stable,
            fmt_optional_u32(record.io_cs_top_link),
            record.io_cs_top_contrib_x100,
            fmt_optional_u32(record.io_cs_raw_top_link),
            record.io_cs_raw_top_contrib_x100,
            record.live_source_df_x100,
            fmt_optional_u32(record.observed_candidate_domain),
            fmt_optional_u32(record.observed_candidate_cpu),
            record.observed_destination_live_df_x100,
            record.baseline_total_overload_x100,
            record.candidate_total_overload_x100,
            record.baseline_max_overload_x100,
            record.candidate_max_overload_x100,
            record.baseline_pair_penalty,
            record.candidate_pair_penalty,
            record.baseline_combined_cost,
            record.candidate_combined_cost,
            record.considered_candidates,
            record.skipped_missing_df,
            record.skipped_missing_llc,
            record.skipped_no_cpu,
            record.skipped_same_domain,
            record.skipped_budget,
            record.blocked_destination,
            record.blocked_phase,
            record.blocked_reverse,
            record.planner_epoch,
            record.plan_revision,
            record.plan_used,
            record.fallback_reason,
            fmt_optional_u32(record.planned_domain),
            fmt_optional_u32(record.planned_cpu),
            fmt_optional_u32(record.sync_group_id),
            fmt_optional_u32(record.sync_anchor_domain),
            record.sync_override,
            record.control_plane_cpu_policy,
            fmt_optional_u32(record.control_plane_cpu),
            select_cpu_debug_fields,
            record.reason.as_str(),
        );

        if self.echo_stderr {
            eprint!("{line}");
        }
        if let Some(writer) = self.writer.as_mut() {
            writer.write_all(line.as_bytes())?;
        }
        Ok(())
    }
}

#[cfg(feature = "select-cpu-in-domain-debug")]
fn select_cpu_in_domain_debug_fields(record: &DecisionRecord) -> String {
    if !record.select_cpu_in_domain_debug_enabled {
        return String::new();
    }
    format!(
        " selected_cpu_planned_jobs={} selected_cpu_idle={} selected_cpu_dsq_depth={} selected_cpu_util_x100={} selected_cpu_current_tid={} best_idle_cpu_in_domain={} best_idle_cpu_planned_jobs={} min_planned_jobs_in_domain={} target_cpu_prepass={} reserved_cpu_override_used={}",
        fmt_optional_u32(record.selected_cpu_planned_jobs),
        fmt_optional_u32(record.selected_cpu_idle),
        fmt_optional_u32(record.selected_cpu_dsq_depth),
        fmt_optional_u32(record.selected_cpu_util_x100),
        fmt_optional_u32(record.selected_cpu_current_tid),
        fmt_optional_u32(record.best_idle_cpu_in_domain),
        fmt_optional_u32(record.best_idle_cpu_planned_jobs),
        fmt_optional_u32(record.min_planned_jobs_in_domain),
        fmt_optional_u32(record.target_cpu_prepass),
        record.reserved_cpu_override_used,
    )
}

#[cfg(not(feature = "select-cpu-in-domain-debug"))]
fn select_cpu_in_domain_debug_fields(_record: &DecisionRecord) -> &'static str {
    ""
}
