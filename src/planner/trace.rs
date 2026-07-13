use super::*;

pub(super) fn format_u32_list(values: impl IntoIterator<Item = u32>) -> String {
    let mut out = String::new();
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{value}");
    }
    out
}

pub(super) fn format_optional_u32(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "na".to_string())
}

pub(super) fn format_member_domains(item: &PlannerItem, state: &PlannerState) -> String {
    let mut out = String::new();
    for (index, tid) in item.member_tids.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let current_domain = state
            .members
            .get(tid)
            .and_then(|member| member.current_domain)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "na".to_string());
        let _ = write!(out, "{tid}:{current_domain}");
    }
    out
}

pub(super) fn format_triggers(triggers: &[PlannerTrigger]) -> String {
    let mut out = String::new();
    for (index, trigger) in triggers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        match trigger {
            PlannerTrigger::SweepComplete(epoch) => {
                let _ = write!(out, "sweep_complete:{epoch}");
            }
            PlannerTrigger::RunnableDelta(tid) => {
                let _ = write!(out, "runnable_delta:{tid}");
            }
            PlannerTrigger::SignatureChange(tid) => {
                let _ = write!(out, "signature_change:{tid}");
            }
            PlannerTrigger::DomainStateFlip(domain) => {
                let _ = write!(out, "domain_flip:{domain}");
            }
            PlannerTrigger::TickPressure(domain) => {
                let _ = write!(out, "tick_pressure:{domain}");
            }
        }
    }
    out
}
