use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_CCM_MAPPING: &str = "/home/seunghyun/gapbs/profiler/ccm_mapping.txt";
const DEFAULT_CPU_MASK: &str = "0-2";
const DEFAULT_WORKER_CPUS: &str = "";
const DEFAULT_FLUSH_CPUS: &str = "2";
const SCHEDULER_TIMEOUT: &str = "28s";
const TARGET_CS_LINK: usize = 0;
const DEFAULT_TARGET_CAPACITY_MIB_S: &str = "1000";
const DEFAULT_PRESSURE_FLOOR_MIB_S: f64 = 500.0;
const DEFAULT_TARGET_TOP_RATIO_FLOOR: f64 = 0.85;

#[derive(Clone, Debug, Default)]
struct LinkStats {
    samples: usize,
    sum_bw_mib_s: f64,
    max_bw_mib_s: f64,
    hot_samples: usize,
}

impl LinkStats {
    fn record(&mut self, bw_mib_s: f64) {
        self.samples += 1;
        self.sum_bw_mib_s += bw_mib_s;
        self.max_bw_mib_s = self.max_bw_mib_s.max(bw_mib_s);
        if bw_mib_s >= DEFAULT_PRESSURE_FLOOR_MIB_S {
            self.hot_samples += 1;
        }
    }

    fn avg_bw_mib_s(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.sum_bw_mib_s / self.samples as f64
        }
    }
}

#[derive(Clone, Debug)]
struct StallControl {
    link_id: usize,
    action: String,
    tokens_ns: Option<u64>,
    token_capacity_ns: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct RuntimeObservations {
    stats: Vec<LinkStats>,
    stall_signal_links: Vec<usize>,
    stall_controls: Vec<StallControl>,
}

#[test]
#[ignore = "requires root sched_ext, DF MSR access, and mc_stride_probe"]
fn mc_stride_probe_cs0_overload_triggers_token_throttle() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scheduler = scheduler_binary(&manifest_dir);
    let probe = mc_stride_probe_binary(&manifest_dir);
    let ccm_mapping_source = env_path("LA_CCM_MAPPING_PATH", DEFAULT_CCM_MAPPING);
    let cpu_mask = env_string("LA_HW_CS_THROTTLE_CPU_MASK", DEFAULT_CPU_MASK);
    let worker_cpus = env_string("LA_HW_CS_THROTTLE_WORKER_CPUS", DEFAULT_WORKER_CPUS);
    let flush_cpus = env_string("LA_HW_CS_THROTTLE_FLUSH_CPUS", DEFAULT_FLUSH_CPUS);
    let size_mib = env_string("LA_HW_CS_THROTTLE_SIZE_MIB", "4096");
    let duration_ms = env_string("LA_HW_CS_THROTTLE_DURATION_MS", "8000");
    let selected_lines = env_string("LA_HW_CS_THROTTLE_SELECTED_LINES", "64");
    let capacity_mib_s = env_string(
        "LA_HW_CS_THROTTLE_CAPACITY_MIB_S",
        DEFAULT_TARGET_CAPACITY_MIB_S,
    );
    let pressure_floor_mib_s = env_f64(
        "LA_HW_CS_THROTTLE_PRESSURE_FLOOR_MIB_S",
        DEFAULT_PRESSURE_FLOOR_MIB_S,
    );
    let target_top_ratio_floor = env_f64(
        "LA_HW_CS_THROTTLE_TARGET_TOP_RATIO_FLOOR",
        DEFAULT_TARGET_TOP_RATIO_FLOOR,
    );

    assert!(
        scheduler.exists(),
        "scheduler binary {:?} does not exist; run `cargo build` first",
        scheduler
    );
    assert!(
        probe.exists(),
        "mc_stride_probe binary {:?} does not exist; run `cargo build --bin mc_stride_probe` or set LA_MC_STRIDE_PROBE",
        probe
    );
    assert!(
        ccm_mapping_source.exists(),
        "ccm mapping {:?} does not exist; set LA_CCM_MAPPING_PATH",
        ccm_mapping_source
    );
    assert!(
        Path::new("/dev/cpu/0/msr").exists(),
        "/dev/cpu/0/msr is missing; load msr support before this ignored smoke test"
    );
    assert!(
        Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .expect("failed to execute sudo")
            .success(),
        "passwordless sudo is required for this ignored smoke test"
    );

    let workdir = make_workdir();
    fs::create_dir_all(&workdir).expect("failed to create smoke workdir");
    let ccm_mapping = write_target_capacity_mapping(&ccm_mapping_source, &workdir, &capacity_mib_s)
        .expect("failed to write throttle ccm mapping");
    let decision_log = workdir.join("decision.log");
    let runtime_log = workdir.join("runtime.log");
    let overhead_log = workdir.join("overhead.log");
    let scheduler_out = workdir.join("scheduler.out");
    let scheduler_err = workdir.join("scheduler.err");
    let probe_tsv = workdir.join("mc_stride_probe.tsv");
    let cgroup_path = format!(
        "/sys/fs/cgroup/scx-rustland-la-hw-cs-throttle-{}",
        std::process::id()
    );

    let mut command = Command::new("sudo");
    command
        .arg("-n")
        .arg("timeout")
        .arg("-k")
        .arg("5s")
        .arg(SCHEDULER_TIMEOUT)
        .arg(&scheduler)
        .arg("--cgroup-path")
        .arg(&cgroup_path)
        .arg("--ccm-mapping-path")
        .arg(&ccm_mapping)
        .arg("--monitor")
        .arg("0.25")
        .arg("--df-window-ms")
        .arg("5")
        .arg("--df-stale-ms")
        .arg("500")
        .arg("--stall-victim-min-pct")
        .arg("0")
        .arg("--stall-victim-delta-pct")
        .arg("0")
        .arg("--tick-reeval-every")
        .arg("1")
        .arg("--tick-defer-max")
        .arg("1")
        .arg("--decision-log-path")
        .arg(&decision_log)
        .arg("--runtime-log-path")
        .arg(&runtime_log)
        .arg("--overhead-profile-path")
        .arg(&overhead_log)
        .arg("--overhead-profile-interval-ms")
        .arg("250")
        .arg("--guard-clean-cgroup")
        .arg("true")
        .arg("--")
        .arg("taskset")
        .arg("-c")
        .arg(&cpu_mask)
        .arg(&probe)
        .arg("run")
        .arg("--measure-cs=false")
        .arg("--size-mib")
        .arg(&size_mib)
        .arg("--duration-ms")
        .arg(&duration_ms)
        .arg("--threads")
        .arg("8");
    if !worker_cpus.trim().is_empty() {
        command.arg("--cpus").arg(&worker_cpus);
    }
    let status = command
        .arg("--numa-policy")
        .arg("interleave")
        .arg("--numa-nodes")
        .arg("0,1")
        .arg("--page-mode")
        .arg("thp")
        .arg("--pattern")
        .arg("chase")
        .arg("--address-mode")
        .arg("linear")
        .arg("--stride")
        .arg("4096")
        .arg("--lines-per-step")
        .arg("1")
        .arg("--chase-nodes")
        .arg(&selected_lines)
        .arg("--predicted-cs-select")
        .arg(&selected_lines)
        .arg("--predicted-cs-link")
        .arg(TARGET_CS_LINK.to_string())
        .arg("--predicted-cs-line-offsets")
        .arg("0:4032:64")
        .arg("--predicted-cs-autotune=false")
        .arg("--flush-threads")
        .arg("4")
        .arg("--flush-cpus")
        .arg(&flush_cpus)
        .arg("--flush-target")
        .arg("chase")
        .arg("--out")
        .arg(&probe_tsv)
        .current_dir(&workdir)
        .stdout(Stdio::from(
            File::create(&scheduler_out).expect("failed to create scheduler stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&scheduler_err).expect("failed to create scheduler stderr file"),
        ))
        .status()
        .expect("failed to run scheduler throttle smoke command");
    cleanup_by_runtime_log(&runtime_log);

    let command_stdout = display_file(&scheduler_out);
    let command_stderr = display_file(&scheduler_err);
    let observations = parse_runtime_observations(&runtime_log).unwrap_or_else(|err| {
        panic!(
            "failed to parse runtime observations from {:?}: {err}; stdout={} stderr={}",
            runtime_log, command_stdout, command_stderr
        )
    });
    assert_cs0_pressure(
        &observations.stats,
        pressure_floor_mib_s,
        target_top_ratio_floor,
    );
    assert_token_throttle(&observations);
    assert_overhead_counter_clean(&overhead_log);
    assert!(
        acceptable_smoke_exit(status),
        "CS throttle logs validated, but scheduler smoke command ended unexpectedly with {status}; stdout={command_stdout} stderr={command_stderr}",
    );

    println!(
        "artifacts: workdir={} runtime_log={} overhead_log={} probe_tsv={} status={}",
        workdir.display(),
        runtime_log.display(),
        overhead_log.display(),
        probe_tsv.display(),
        status
    );
    println!("{}", format_stats_table(&observations.stats));
    println!("{}", format_throttle_summary(&observations));
}

fn scheduler_binary(manifest_dir: &Path) -> PathBuf {
    option_env!("CARGO_BIN_EXE_scx-rustland-la")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target/debug/scx-rustland-la"))
}

fn mc_stride_probe_binary(manifest_dir: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("LA_MC_STRIDE_PROBE") {
        return PathBuf::from(path);
    }
    let release = manifest_dir.join("target/release/mc_stride_probe");
    if release.exists() {
        return release;
    }
    option_env!("CARGO_BIN_EXE_mc_stride_probe")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target/debug/mc_stride_probe"))
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default)
}

fn make_workdir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before UNIX_EPOCH")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "scx-rustland-la-hw-cs-throttle-{}-{nanos}",
        std::process::id()
    ))
}

fn write_target_capacity_mapping(
    source: &Path,
    workdir: &Path,
    capacity_mib_s: &str,
) -> io::Result<PathBuf> {
    let text = fs::read_to_string(source)?;
    let mut out = text;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("cs");
    out.push_str(&TARGET_CS_LINK.to_string());
    out.push_str("\n    capacity_mib_s ");
    out.push_str(capacity_mib_s);
    out.push('\n');

    let path = workdir.join("ccm_mapping.cs0-throttle.txt");
    fs::write(&path, out)?;
    Ok(path)
}

fn cleanup_by_runtime_log(runtime_log: &Path) {
    let _ = Command::new("sudo")
        .arg("-n")
        .arg("pkill")
        .arg("-KILL")
        .arg("-f")
        .arg(runtime_log.as_os_str())
        .status();
}

fn acceptable_smoke_exit(status: std::process::ExitStatus) -> bool {
    if status.success() || status.code() == Some(124) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return matches!(status.signal(), Some(9 | 15));
    }
    #[allow(unreachable_code)]
    false
}

fn parse_runtime_observations(path: &Path) -> io::Result<RuntimeObservations> {
    let content = fs::read_to_string(path)?;
    let mut observations = RuntimeObservations::default();
    observations.stats.resize_with(12, LinkStats::default);

    for line in content.lines() {
        if let Some((link_id, bw_mib_s)) = parse_cs_bw_line(line) {
            if link_id >= observations.stats.len() {
                observations
                    .stats
                    .resize_with(link_id + 1, LinkStats::default);
            }
            observations.stats[link_id].record(bw_mib_s);
            continue;
        }
        if line.starts_with("stall_signal ") {
            if let Some(link_id) = parse_usize_field(line, "link=") {
                observations.stall_signal_links.push(link_id);
            }
            continue;
        }
        if line.starts_with("stall_control ") {
            if let (Some(link_id), Some(action)) = (
                parse_usize_field(line, "link="),
                field_value(line, "action="),
            ) {
                observations.stall_controls.push(StallControl {
                    link_id,
                    action: action.to_string(),
                    tokens_ns: parse_u64_field(line, "tokens_ns="),
                    token_capacity_ns: parse_u64_field(line, "token_capacity_ns="),
                });
            }
        }
    }
    Ok(observations)
}

fn parse_cs_bw_line(line: &str) -> Option<(usize, f64)> {
    let rest = line.strip_prefix("cs_link=")?;
    let (link_id, fields) = rest.split_once(' ')?;
    let bw = fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix("bw="))?;
    Some((link_id.parse().ok()?, bw.parse().ok()?))
}

fn field_value<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(prefix))
}

fn parse_usize_field(line: &str, prefix: &str) -> Option<usize> {
    field_value(line, prefix).and_then(|value| value.parse().ok())
}

fn parse_u64_field(line: &str, prefix: &str) -> Option<u64> {
    field_value(line, prefix).and_then(|value| value.parse().ok())
}

fn assert_cs0_pressure(
    stats: &[LinkStats],
    pressure_floor_mib_s: f64,
    target_top_ratio_floor: f64,
) {
    let summary = format_stats_table(stats);
    let target = stats
        .get(TARGET_CS_LINK)
        .unwrap_or_else(|| panic!("missing target CS{TARGET_CS_LINK} stats\n{summary}"));
    assert!(
        target.samples > 0,
        "CS{TARGET_CS_LINK} had no monitor samples\n{summary}"
    );
    assert!(
        target.max_bw_mib_s >= pressure_floor_mib_s,
        "CS{TARGET_CS_LINK} max {:.2} MiB/s stayed below floor {:.2} MiB/s\n{summary}",
        target.max_bw_mib_s,
        pressure_floor_mib_s
    );
    assert!(
        target.hot_samples > 0,
        "CS{TARGET_CS_LINK} never produced a hot sample\n{summary}"
    );

    let (top_link, top_max_bw_mib_s) = stats
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            left.max_bw_mib_s
                .partial_cmp(&right.max_bw_mib_s)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, stat)| (idx, stat.max_bw_mib_s))
        .unwrap_or((TARGET_CS_LINK, 0.0));
    assert!(
        target.max_bw_mib_s >= top_max_bw_mib_s * target_top_ratio_floor,
        "CS{TARGET_CS_LINK} max {:.2} MiB/s is below {:.0}% of top CS{top_link} max {:.2} MiB/s\n{summary}",
        target.max_bw_mib_s,
        target_top_ratio_floor * 100.0,
        top_max_bw_mib_s
    );
}

fn assert_token_throttle(observations: &RuntimeObservations) {
    let summary = format_throttle_summary(observations);
    assert!(
        observations
            .stall_signal_links
            .iter()
            .any(|link| *link == TARGET_CS_LINK),
        "no stall_signal was recorded for CS{TARGET_CS_LINK}\n{summary}"
    );
    assert!(
        observations
            .stall_signal_links
            .iter()
            .all(|link| *link == TARGET_CS_LINK),
        "stall_signal fired for non-target links\n{summary}"
    );

    let target_controls = observations
        .stall_controls
        .iter()
        .filter(|control| control.link_id == TARGET_CS_LINK)
        .collect::<Vec<_>>();
    assert!(
        !target_controls.is_empty(),
        "no stall_control was recorded for CS{TARGET_CS_LINK}\n{summary}"
    );
    assert!(
        observations
            .stall_controls
            .iter()
            .all(|control| control.link_id == TARGET_CS_LINK),
        "stall_control fired for non-target links\n{summary}"
    );
    assert!(
        target_controls
            .iter()
            .any(|control| control.action == "reslice_token"),
        "no reslice_token action was recorded\n{summary}"
    );
    assert!(
        target_controls
            .iter()
            .any(|control| control.action == "defer_token"),
        "no defer_token action was recorded\n{summary}"
    );
    assert!(
        target_controls.iter().all(|control| {
            control.tokens_ns.is_some() && control.token_capacity_ns.unwrap_or(0) > 0
        }),
        "stall_control rows did not report token bucket fields\n{summary}"
    );
}

fn assert_overhead_counter_clean(path: &Path) {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read overhead log {:?}: {err}", path));
    let line = content
        .lines()
        .find(|line| line.contains("scope=df_cs_sample_resource_total"))
        .unwrap_or_else(|| {
            panic!(
                "overhead log {:?} missing df_cs_sample_resource_total",
                path
            )
        });
    let count = parse_usize_field(line, "count=").unwrap_or(0);
    let error_count = parse_usize_field(line, "error_count=").unwrap_or(usize::MAX);
    assert!(
        count > 0 && error_count == 0,
        "unexpected DF-CS overhead counter: {line}"
    );
}

fn format_stats_table(stats: &[LinkStats]) -> String {
    let mut lines = vec![String::from("cs_link samples avg_bw max_bw hot_samples")];
    for (link_id, stat) in stats.iter().enumerate() {
        lines.push(format!(
            "{link_id:>7} {:>7} {:>6.2} {:>6.2} {:>11}",
            stat.samples,
            stat.avg_bw_mib_s(),
            stat.max_bw_mib_s,
            stat.hot_samples,
        ));
    }
    lines.join("\n")
}

fn format_throttle_summary(observations: &RuntimeObservations) -> String {
    let signal_count = observations
        .stall_signal_links
        .iter()
        .filter(|link| **link == TARGET_CS_LINK)
        .count();
    let reslice_count = observations
        .stall_controls
        .iter()
        .filter(|control| control.link_id == TARGET_CS_LINK && control.action == "reslice_token")
        .count();
    let defer_count = observations
        .stall_controls
        .iter()
        .filter(|control| control.link_id == TARGET_CS_LINK && control.action == "defer_token")
        .count();
    let non_target_controls = observations
        .stall_controls
        .iter()
        .filter(|control| control.link_id != TARGET_CS_LINK)
        .count();
    format!(
        "throttle_summary target_link={} stall_signals={} reslice_token={} defer_token={} non_target_controls={} total_controls={}",
        TARGET_CS_LINK,
        signal_count,
        reslice_count,
        defer_count,
        non_target_controls,
        observations.stall_controls.len()
    )
}

fn display_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| format!("<failed to read {:?}: {err}>", path))
}
