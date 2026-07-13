use crate::bpf::QueuedTask;
use crate::types::{ManagedThreadState, MEM_SOURCE_DRAM_NEAR, MEM_SOURCE_NEAR_CACHE};

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskSignatureEffect {
    pub df_x100: u32,
    pub llc_x100: u32,
    pub signature_valid: bool,
}

pub(crate) fn l2_bw_to_pressure_x100(l2_bw_mib_s_x100: u32, capacity_mib_s_x100: u32) -> u32 {
    (((l2_bw_mib_s_x100 as u64) * 10_000) / capacity_mib_s_x100.max(1) as u64).min(10_000) as u32
}

fn signature_is_valid(meta: Option<&ManagedThreadState>) -> bool {
    meta.map(|value| value.signature.valid).unwrap_or(false)
}

pub fn task_signature_effect(
    task: &QueuedTask,
    meta: Option<&ManagedThreadState>,
    capacity_mib_s_x100: u32,
) -> TaskSignatureEffect {
    let demand_pressure_x100 =
        l2_bw_to_pressure_x100(task.ewma_l2_bw_mib_s_x100, capacity_mib_s_x100);
    let demand_ccm_bw_x100 = task.ewma_l2_bw_mib_s_x100;
    let signature_valid = signature_is_valid(meta);
    let df_x100 = meta
        .filter(|_| signature_valid)
        .map(|value| {
            let fill_sum = value.signature.fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE]
                .saturating_add(value.signature.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR]);
            if fill_sum > 0 {
                fill_sum
            } else {
                value.signature.projected_df_pressure_x100
            }
        })
        .filter(|value| *value > 0)
        .unwrap_or(demand_ccm_bw_x100);
    let llc_x100 = meta
        .filter(|_| signature_valid)
        .map(|value| value.signature.projected_llc_pressure_x100)
        .filter(|value| *value > 0)
        .unwrap_or(demand_pressure_x100);

    TaskSignatureEffect {
        df_x100,
        llc_x100,
        signature_valid,
    }
}

pub fn thread_signature_effect(thread: &ManagedThreadState) -> TaskSignatureEffect {
    if !thread.signature.valid {
        return TaskSignatureEffect::default();
    }
    let fill_sum = thread.signature.fill_bw_mib_s_x100[MEM_SOURCE_NEAR_CACHE]
        .saturating_add(thread.signature.fill_bw_mib_s_x100[MEM_SOURCE_DRAM_NEAR]);
    TaskSignatureEffect {
        df_x100: if fill_sum > 0 {
            fill_sum
        } else {
            thread.signature.projected_df_pressure_x100
        },
        llc_x100: thread.signature.projected_llc_pressure_x100,
        signature_valid: true,
    }
}
