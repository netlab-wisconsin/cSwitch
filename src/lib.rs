#[cfg(all(feature = "tick-resched", feature = "light-compete"))]
compile_error!("features `tick-resched` and `light-compete` are mutually exclusive");
#[cfg(all(feature = "scheduler-arcas", feature = "scheduler-nsdi"))]
compile_error!("features `scheduler-arcas` and `scheduler-nsdi` are mutually exclusive");
#[cfg(all(feature = "scheduler-arcas", feature = "scheduler-paper-greedy"))]
compile_error!("features `scheduler-arcas` and `scheduler-paper-greedy` are mutually exclusive");
#[cfg(all(feature = "scheduler-nsdi", feature = "scheduler-paper-greedy"))]
compile_error!("features `scheduler-nsdi` and `scheduler-paper-greedy` are mutually exclusive");
#[cfg(all(any(
    all(
        feature = "scheduler-empty-minimal",
        feature = "scheduler-empty-observed"
    ),
    all(
        feature = "scheduler-empty-minimal",
        feature = "scheduler-empty-observed-shared-sixth"
    ),
    all(
        feature = "scheduler-empty-observed",
        feature = "scheduler-empty-observed-shared-sixth"
    )
)))]
compile_error!("empty scheduler features are mutually exclusive");
#[cfg(all(
    any(
        feature = "scheduler-empty-minimal",
        feature = "scheduler-empty-observed",
        feature = "scheduler-empty-observed-shared-sixth"
    ),
    any(
        feature = "scheduler-arcas",
        feature = "scheduler-nsdi",
        feature = "scheduler-paper-greedy"
    )
))]
compile_error!("empty scheduler features are mutually exclusive with policy scheduler variants");
#[cfg(all(feature = "stall-filler-spinner", feature = "scheduler-arcas"))]
compile_error!("feature `stall-filler-spinner` is only supported by the default/app scheduler");
#[cfg(all(feature = "stall-filler-spinner", feature = "scheduler-nsdi"))]
compile_error!("feature `stall-filler-spinner` is only supported by the default/app scheduler");
#[cfg(all(
    feature = "stall-filler-spinner",
    any(
        feature = "scheduler-empty-minimal",
        feature = "scheduler-empty-observed",
        feature = "scheduler-empty-observed-shared-sixth"
    )
))]
compile_error!("feature `stall-filler-spinner` is not supported by empty scheduler variants");
#[cfg(all(
    feature = "light-compete",
    any(
        feature = "scheduler-empty-minimal",
        feature = "scheduler-empty-observed",
        feature = "scheduler-empty-observed-shared-sixth"
    )
))]
compile_error!("feature `light-compete` is not supported by empty scheduler variants");

#[cfg(feature = "stall-filler-spinner")]
pub const STALL_FILLER_SPINNER_ARG: &str = "--rustland-stall-filler-spinner";

pub mod app;
#[cfg(feature = "scheduler-arcas")]
pub mod app_arcas;
#[cfg(feature = "scheduler-nsdi")]
pub mod app_nsdi;
pub mod cli;
pub mod cpu_util;
#[cfg(feature = "diagnostics")]
pub mod decision_log;
pub mod df_ccm_sampler;
pub mod df_cs_sampler;
mod df_msr;
pub mod filter;
pub mod host_map;
pub mod llc_sampler;
#[cfg(feature = "diagnostics")]
pub mod monitor;
pub mod overhead_profile;
pub mod planner;
pub mod planner_move_trace;
pub mod policy;
#[cfg(feature = "scheduler-arcas")]
pub mod policy_arcas;
#[cfg(feature = "scheduler-nsdi")]
pub mod policy_nsdi;
pub mod runtime_log;
pub mod topology;
pub mod types;
pub(crate) mod villain_control;
pub mod workload;

#[rustfmt::skip]
pub mod bpf;
pub mod bpf_intf;
pub mod bpf_skel;

#[cfg(not(any(feature = "scheduler-arcas", feature = "scheduler-nsdi")))]
pub use app::run;
#[cfg(feature = "scheduler-arcas")]
pub use app_arcas::run;
#[cfg(feature = "scheduler-nsdi")]
pub use app_nsdi::run;

#[cfg(feature = "diagnostics")]
#[macro_export]
macro_rules! diag_line {
    ($($arg:tt)*) => {{
        $crate::runtime_log::line(format!($($arg)*));
    }};
}

#[cfg(not(feature = "diagnostics"))]
#[macro_export]
macro_rules! diag_line {
    ($($arg:tt)*) => {{}};
}
