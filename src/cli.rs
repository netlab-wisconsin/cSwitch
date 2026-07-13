use clap::{Parser, ValueEnum};

#[cfg(feature = "scheduler-nsdi")]
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum NsdiPolicyKind {
    Static,
    DelayRange,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum ControlPlaneCpuPolicy {
    Mixed,
    AutoDedicated,
    Dedicated,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "scx-rustland-la")]
#[command(about = "Rustland-based LLC/DF-aware sched_ext scheduler")]
pub struct Opts {
    #[arg(long, default_value = "/sys/fs/cgroup/scx-rustland-la")]
    pub cgroup_path: String,

    #[arg(long, default_value = "/home/seunghyun/gapbs/profiler/ccm_mapping.txt")]
    pub ccm_mapping_path: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub restrict_mapped_cpus: bool,

    #[arg(long, default_value_t = 83)]
    pub managed_cpu_max: u32,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub primary_smt_only: bool,

    #[cfg(feature = "diagnostics")]
    #[arg(long)]
    pub monitor: Option<f64>,

    #[arg(long, default_value_t = 512)]
    pub l2_need_mib_s: u32,

    #[arg(long, default_value_t = 70)]
    pub llc_hot_pct: u32,

    #[arg(long, default_value_t = 55)]
    pub llc_cool_pct: u32,

    #[arg(long, default_value_t = 3)]
    pub llc_hot_persist: u32,

    #[arg(long, default_value_t = 3)]
    pub llc_cool_persist: u32,

    #[arg(long, default_value_t = 30)]
    pub llc_ewma_alpha_pct: u32,

    #[arg(long, default_value_t = 20)]
    pub llc_window_ms: u64,

    #[arg(long, default_value_t = 30)]
    pub llc_period_ms: u64,

    #[arg(long, default_value_t = 120)]
    pub llc_stale_ms: u64,

    #[arg(long, default_value_t = 40)]
    pub dram_hot_ns: u32,

    #[arg(long, default_value_t = 50_000_000, alias = "miss-ratio-hot-pct")]
    pub miss_ppm_hot: u32,

    #[arg(long, default_value_t = 10)]
    pub df_window_ms: u64,

    #[arg(long, default_value_t = 120)]
    pub df_stale_ms: u64,

    #[arg(long, default_value_t = 75)]
    pub df_hot_pct: u32,

    #[arg(long, default_value_t = 60)]
    pub df_cool_pct: u32,

    #[arg(long, default_value_t = 2)]
    pub df_hot_persist: u32,

    #[arg(long, default_value_t = 2)]
    pub df_cool_persist: u32,

    #[arg(long, default_value_t = 20)]
    pub df_ewma_alpha_pct: u32,

    #[arg(long, default_value_t = 1)]
    pub migrate_margin: u32,

    #[arg(long, default_value_t = 85)]
    pub cpu_high_util_pct: u32,

    #[arg(long, default_value_t = 2)]
    pub cpu_rebalance_job_delta: u32,

    #[arg(long, default_value_t = 50)]
    pub stall_victim_min_pct: u32,

    #[arg(long, default_value_t = 15)]
    pub stall_victim_delta_pct: u32,

    #[arg(long, default_value_t = 50)]
    pub cpu_util_sample_ms: u64,

    #[arg(long, default_value_t = 80)]
    pub migrate_settle_ms: u64,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub signature_snapshots: bool,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub cs_villain_throttle: bool,

    #[arg(long, default_value_t = 1)]
    pub tick_reeval_every: u32,

    #[arg(long, default_value_t = 1)]
    pub tick_defer_max: u32,

    #[arg(long, default_value_t = 1_000)]
    pub cs_villain_reslice_us: u64,

    #[arg(long, default_value_t = 4)]
    pub cs_villain_refill_divisor: u64,

    #[arg(long)]
    pub cs_villain_settle_ms: Option<u64>,

    #[arg(long)]
    pub cs_villain_release_samples: Option<u32>,

    #[cfg(feature = "stall-filler-spinner")]
    #[arg(long, default_value_t = 1)]
    pub stall_filler_slice_ms: u64,

    #[arg(long, default_value_t = 2)]
    pub tick_move_phase_mod: u32,

    #[arg(long, action = clap::ArgAction::Append)]
    pub sync_tgid: Vec<u32>,

    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub disable_auto_sync_hints: bool,

    #[arg(long, value_enum, default_value_t = ControlPlaneCpuPolicy::Mixed)]
    pub control_plane_cpu_policy: ControlPlaneCpuPolicy,

    #[cfg(feature = "scheduler-arcas")]
    #[arg(long, default_value_t = 100)]
    pub arcas_interval_ms: u64,

    #[cfg(feature = "scheduler-arcas")]
    #[arg(long, default_value_t = 256)]
    pub arcas_remote_fill_threshold_mib_s: u32,

    #[cfg(feature = "scheduler-arcas")]
    #[arg(long, default_value_t = 1)]
    pub arcas_min_domains: u32,

    #[cfg(feature = "scheduler-arcas")]
    #[arg(long, default_value_t = 0)]
    pub arcas_max_domains: u32,

    #[cfg(feature = "scheduler-arcas")]
    #[arg(long, default_value_t = 1)]
    pub arcas_step: u32,

    #[cfg(feature = "scheduler-nsdi")]
    #[arg(long, value_enum, default_value_t = NsdiPolicyKind::DelayRange)]
    pub nsdi_policy: NsdiPolicyKind,

    #[cfg(feature = "scheduler-nsdi")]
    #[arg(long, default_value_t = 1)]
    pub nsdi_control_ms: u64,

    #[cfg(feature = "scheduler-nsdi")]
    #[arg(long, default_value_t = 500)]
    pub nsdi_delay_low_us: u64,

    #[cfg(feature = "scheduler-nsdi")]
    #[arg(long, default_value_t = 1_000)]
    pub nsdi_delay_high_us: u64,

    #[arg(long, action = clap::ArgAction::Append)]
    pub spawn_shell: Vec<String>,

    #[arg(long, default_value_t = 0)]
    pub spawn_stagger_ms: u64,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub guard_clean_cgroup: bool,

    #[cfg(feature = "diagnostics")]
    #[arg(long)]
    pub decision_log_path: Option<String>,

    #[cfg(all(feature = "diagnostics", feature = "select-cpu-in-domain-debug"))]
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub select_cpu_in_domain: bool,

    #[cfg(feature = "diagnostics")]
    #[arg(long)]
    pub runtime_log_path: Option<String>,

    #[cfg(feature = "diagnostics")]
    #[arg(long)]
    pub planner_move_trace_path: Option<String>,

    #[cfg(feature = "diagnostics")]
    #[arg(long)]
    pub planner_plan_debug_path: Option<String>,

    #[arg(long)]
    pub overhead_profile_path: Option<String>,

    #[arg(long, default_value_t = 1000)]
    pub overhead_profile_interval_ms: u64,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
    pub overhead_profile_alloc: bool,

    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,

    #[arg(last = true)]
    pub command: Vec<String>,
}
