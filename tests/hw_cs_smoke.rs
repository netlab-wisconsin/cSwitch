use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MEMORY_BENCHMARK: &str = "/home/seunghyun/sched_bench/build/memory_benchmark";
const DEFAULT_CPU_MASK: &str = "0-13";
const DEFAULT_ACTIVE_CS_LINKS: &str = "0,3,4,5,6,9,10,11";
const DEFAULT_IDLE_CS_LINKS: &str = "1,2,7,8";
const SCHEDULER_TIMEOUT: &str = "16s";
const ACTIVE_BW_FLOOR_MIB_S: f64 = 1_000.0;
const IDLE_BW_CEILING_MIB_S: f64 = 100.0;
const ACTIVE_BALANCE_RATIO_CEILING: f64 = 2.0;

#[derive(Clone, Debug, Default)]
struct LinkStats {
    samples: usize,
    sum_bw_mib_s: f64,
    max_bw_mib_s: f64,
    hot_samples: usize,
    hot_sum_bw_mib_s: f64,
}

impl LinkStats {
    fn record(&mut self, bw_mib_s: f64) {
        self.samples += 1;
        self.sum_bw_mib_s += bw_mib_s;
        self.max_bw_mib_s = self.max_bw_mib_s.max(bw_mib_s);
        if bw_mib_s >= ACTIVE_BW_FLOOR_MIB_S {
            self.hot_samples += 1;
            self.hot_sum_bw_mib_s += bw_mib_s;
        }
    }

    fn avg_bw_mib_s(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.sum_bw_mib_s / self.samples as f64
        }
    }

    fn hot_avg_bw_mib_s(&self) -> f64 {
        if self.hot_samples == 0 {
            0.0
        } else {
            self.hot_sum_bw_mib_s / self.hot_samples as f64
        }
    }
}

#[test]
#[ignore = "requires root sched_ext, DF MSR access, and memory_benchmark"]
fn eight_second_memory_benchmark_records_interleaved_cs_links() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scheduler = scheduler_binary(&manifest_dir);
    let memory_benchmark = env_path("LA_MEMORY_BENCHMARK", DEFAULT_MEMORY_BENCHMARK);
    let cpu_mask =
        std::env::var("LA_HW_CS_SMOKE_CPU_MASK").unwrap_or_else(|_| DEFAULT_CPU_MASK.to_string());
    let active_links = parse_link_set_env("LA_CS_ACTIVE_LINKS", DEFAULT_ACTIVE_CS_LINKS);
    let idle_links = parse_link_set_env("LA_CS_IDLE_LINKS", DEFAULT_IDLE_CS_LINKS);

    assert!(
        scheduler.exists(),
        "scheduler binary {:?} does not exist; run `cargo build` first",
        scheduler
    );
    assert!(
        memory_benchmark.exists(),
        "memory_benchmark {:?} does not exist; set LA_MEMORY_BENCHMARK",
        memory_benchmark
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
    fs::create_dir_all(workdir.join("res")).expect("failed to create smoke workdir");
    let config_path = write_eight_second_config(&manifest_dir, &workdir)
        .expect("failed to write 8s memory_benchmark config");

    let decision_log = workdir.join("decision.log");
    let runtime_log = workdir.join("runtime.log");
    let overhead_log = workdir.join("overhead.log");
    let scheduler_out = workdir.join("scheduler.out");
    let scheduler_err = workdir.join("scheduler.err");
    let cgroup_path = format!(
        "/sys/fs/cgroup/scx-rustland-la-hw-cs-smoke-{}",
        std::process::id()
    );

    let status = Command::new("sudo")
        .arg("-n")
        .arg("timeout")
        .arg("-k")
        .arg("5s")
        .arg(SCHEDULER_TIMEOUT)
        .arg(&scheduler)
        .arg("--cgroup-path")
        .arg(&cgroup_path)
        .arg("--monitor")
        .arg("0.25")
        .arg("--df-window-ms")
        .arg("5")
        .arg("--df-stale-ms")
        .arg("500")
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
        .arg(&memory_benchmark)
        .arg("--config")
        .arg(&config_path)
        .current_dir(&workdir)
        .stdout(Stdio::from(
            File::create(&scheduler_out).expect("failed to create scheduler stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&scheduler_err).expect("failed to create scheduler stderr file"),
        ))
        .status()
        .expect("failed to run scheduler smoke command");
    cleanup_by_runtime_log(&runtime_log);

    let command_stdout = display_file(&scheduler_out);
    let command_stderr = display_file(&scheduler_err);
    let stats = parse_runtime_cs_stats(&runtime_log).unwrap_or_else(|err| {
        panic!(
            "failed to parse runtime CS rows from {:?}: {err}; stdout={} stderr={}",
            runtime_log, command_stdout, command_stderr
        )
    });
    assert_cs_links(&stats, &active_links, &idle_links);
    assert_overhead_counter_clean(&overhead_log);
    assert!(
        acceptable_smoke_exit(status),
        "CS logs validated, but scheduler smoke command ended unexpectedly with {status}; stdout={command_stdout} stderr={command_stderr}",
    );

    println!(
        "artifacts: workdir={} runtime_log={} overhead_log={} status={}",
        workdir.display(),
        runtime_log.display(),
        overhead_log.display(),
        status
    );
    println!("{}", format_stats_table(&stats));
}

fn scheduler_binary(manifest_dir: &Path) -> PathBuf {
    option_env!("CARGO_BIN_EXE_scx-rustland-la")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target/debug/scx-rustland-la"))
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn parse_link_set_env(name: &str, default: &str) -> BTreeSet<usize> {
    let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
    value
        .split(',')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            part.trim()
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("{name} contains non-numeric link id: {part:?}"))
        })
        .collect()
}

fn make_workdir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before UNIX_EPOCH")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "scx-rustland-la-hw-cs-smoke-{}-{nanos}",
        std::process::id()
    ))
}

fn write_eight_second_config(manifest_dir: &Path, workdir: &Path) -> io::Result<PathBuf> {
    let source = manifest_dir.join("config.load.unpin.xml");
    let config = fs::read_to_string(&source)?;
    let shortened = config.replace("<time>30</time>", "<time>8</time>");
    assert_ne!(
        config, shortened,
        "expected {:?} to contain `<time>30</time>`",
        source
    );
    let path = workdir.join("config.8s.xml");
    fs::write(&path, shortened)?;
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

fn parse_runtime_cs_stats(path: &Path) -> io::Result<Vec<LinkStats>> {
    let content = fs::read_to_string(path)?;
    let mut stats = vec![LinkStats::default(); 12];
    for line in content.lines() {
        let Some((link_id, bw_mib_s)) = parse_cs_bw_line(line) else {
            continue;
        };
        if link_id >= stats.len() {
            stats.resize_with(link_id + 1, LinkStats::default);
        }
        stats[link_id].record(bw_mib_s);
    }
    Ok(stats)
}

fn parse_cs_bw_line(line: &str) -> Option<(usize, f64)> {
    let rest = line.strip_prefix("cs_link=")?;
    let (link_id, fields) = rest.split_once(' ')?;
    let bw = fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix("bw="))?;
    Some((link_id.parse().ok()?, bw.parse().ok()?))
}

fn assert_cs_links(
    stats: &[LinkStats],
    active_links: &BTreeSet<usize>,
    idle_links: &BTreeSet<usize>,
) {
    let summary = format_stats_table(stats);
    let expected_links = active_links
        .union(idle_links)
        .copied()
        .collect::<BTreeSet<_>>();
    for link_id in expected_links {
        let samples = stats.get(link_id).map(|stat| stat.samples).unwrap_or(0);
        assert!(
            samples > 0,
            "CS link {link_id} had no monitor samples\n{summary}"
        );
    }

    let mut hot_avgs = Vec::new();
    for &link_id in active_links {
        let stat = stats
            .get(link_id)
            .unwrap_or_else(|| panic!("missing active CS link {link_id}\n{summary}"));
        assert!(
            stat.hot_samples > 0 && stat.max_bw_mib_s >= ACTIVE_BW_FLOOR_MIB_S,
            "active CS link {link_id} did not record bandwidth >= {ACTIVE_BW_FLOOR_MIB_S} MiB/s\n{summary}"
        );
        hot_avgs.push(stat.hot_avg_bw_mib_s());
    }
    for &link_id in idle_links {
        let stat = stats
            .get(link_id)
            .unwrap_or_else(|| panic!("missing idle CS link {link_id}\n{summary}"));
        assert!(
            stat.max_bw_mib_s <= IDLE_BW_CEILING_MIB_S,
            "idle CS link {link_id} recorded {} MiB/s, above {IDLE_BW_CEILING_MIB_S}\n{summary}",
            stat.max_bw_mib_s
        );
    }

    let min_hot = hot_avgs.iter().copied().fold(f64::INFINITY, f64::min);
    let max_hot = hot_avgs.iter().copied().fold(0.0, f64::max);
    assert!(
        max_hot / min_hot <= ACTIVE_BALANCE_RATIO_CEILING,
        "active CS links are not reasonably balanced: min_hot_avg={min_hot:.2} max_hot_avg={max_hot:.2}\n{summary}"
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

fn parse_usize_field(line: &str, prefix: &str) -> Option<usize> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(prefix))
        .and_then(|value| value.parse().ok())
}

fn format_stats_table(stats: &[LinkStats]) -> String {
    let mut lines = vec![String::from(
        "cs_link samples avg_bw max_bw hot_samples hot_avg_bw",
    )];
    for (link_id, stat) in stats.iter().enumerate() {
        lines.push(format!(
            "{link_id:>7} {:>7} {:>6.2} {:>6.2} {:>11} {:>10.2}",
            stat.samples,
            stat.avg_bw_mib_s(),
            stat.max_bw_mib_s,
            stat.hot_samples,
            stat.hot_avg_bw_mib_s()
        ));
    }
    lines.join("\n")
}

fn display_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| format!("<failed to read {:?}: {err}>", path))
}
