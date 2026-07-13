#!/usr/bin/env python3

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
ROOT_DIR = SCRIPT_DIR.parent
if str(ROOT_DIR) not in sys.path:
    sys.path.insert(0, str(ROOT_DIR))

from experiment_utils import (  # noqa: E402
    append_csv_row,
    build_env,
    ensure_directory,
    format_command,
    parse_gapbs_trial_time,
    parse_perf_sched_timehist,
    require_path,
    run_capture,
    start_background,
    stop_background,
    stdev,
    validate_chiplet_groups,
    write_csv_rows,
    write_memory_benchmark_config,
)


GAPBS_DIR = Path("/home/seunghyun/gapbs/gapbs")
MEMORY_BENCHMARK = Path("/home/seunghyun/sched_bench/build/memory_benchmark")
CHIPLETS = {
    "ccd0": [0, 1, 2, 3, 4, 5, 6],
    "ccd1": [7, 8, 9, 10, 11, 12, 13],
    "ccd2": [14, 15, 16, 17, 18, 19, 20],
    "ccd3": [21, 22, 23, 24, 25, 26, 27],
}
FREE_OPTIMAL_PLACEMENT = (4, 5, 6)
FREE_SAME_CHIPLET_PLACEMENTS = (
    (4, 5, 6),
    (11, 12, 13),
    (18, 19, 20),
    (25, 26, 27),
)
BUSY_OPTIMAL_AFFINITY = "0-6"
BUSY_SAME_CHIPLET_AFFINITY_MASKS = (
    "0-6",
    "7-13",
    "14-20",
    "21-27",
)
FREE_SCENARIO_NAME = "free-cores"
BUSY_SCENARIO_NAME = "busy-cores"
FIELDNAMES = [
    "scenario",
    "policy",
    "benchmark",
    "placement_tag",
    "placement_detail",
    "omp_threads",
    "repeat",
    "trial_time_s",
    "perf_sched_status",
    "perf_sched_trace_path",
    "perf_sched_cpus",
]
THREAD_FIELDNAMES = [
    "scenario",
    "policy",
    "benchmark",
    "placement_tag",
    "repeat",
    "thread_slot",
    "thread_tid",
    "thread_pid",
    "total_runtime_ms",
    "ccd0_pct",
    "ccd1_pct",
    "ccd2_pct",
    "ccd3_pct",
    "other_pct",
    "core_pct_map",
]
STATS_FIELDNAMES = [
    "scenario",
    "policy",
    "benchmark",
    "omp_threads",
    "runs",
    "mean_trial_time_s",
    "stdev_trial_time_s",
]
THREAD_STATS_FIELDNAMES = [
    "scenario",
    "policy",
    "benchmark",
    "thread_slot",
    "runs",
    "mean_total_runtime_ms",
    "ccd0_pct",
    "ccd1_pct",
    "ccd2_pct",
    "ccd3_pct",
    "other_pct",
    "core_pct_map",
]
BENCHMARKS = {
    "bfs": str(GAPBS_DIR / "bfs"),
    "pr": str(GAPBS_DIR / "pr"),
    "cc": str(GAPBS_DIR / "cc"),
}


def build_perf_argv(argv: list[str], *, use_sudo: bool) -> list[str]:
    if use_sudo:
        return ["sudo", "-n", *argv]
    return argv


def build_sudo_record_argv(env: dict[str, str], gapbs_argv: list[str], perf_data_path: Path) -> list[str]:
    env_assignments = [f"{key}={value}" for key, value in env.items() if key.startswith("OMP_")]
    return [
        "sudo",
        "-n",
        "env",
        *env_assignments,
        "perf",
        "sched",
        "record",
        "--output",
        str(perf_data_path),
        "--",
        *gapbs_argv,
    ]


def probe_perf_sched(use_sudo: bool) -> tuple[bool, str]:
    probe_path = "/tmp/exp3_perf_sched_probe.data"
    command = build_perf_argv(
        ["perf", "sched", "record", "--output", probe_path, "--", "sleep", "0.01"],
        use_sudo=use_sudo,
    )
    try:
        completed = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        return False, "perf binary not found in PATH"
    probe_output = " ".join(line.strip() for line in completed.stdout.splitlines() if line.strip())
    cleanup_command = ["rm", "-f", probe_path]
    if use_sudo:
        cleanup_command = ["sudo", "-n", *cleanup_command]
    subprocess.run(cleanup_command, check=False)
    if completed.returncode == 0:
        return True, "available"
    if probe_output:
        return False, probe_output
    return False, f"perf sched probe failed with return code {completed.returncode}"


def cpu_to_domain(cpu: int) -> str:
    for domain, cores in CHIPLETS.items():
        if cpu in cores:
            return domain
    return "other"


def format_pct_map(values: dict[int, float]) -> str:
    non_zero = [(key, value) for key, value in values.items() if value > 0.0]
    non_zero.sort(key=lambda item: (-item[1], item[0]))
    return ", ".join(f"{key}:{value:.2f}" for key, value in non_zero)


def format_cpu_pct_map(cpu_runtime_ms: defaultdict[int, float], total_runtime_ms: float) -> str:
    if total_runtime_ms <= 0.0:
        return ""
    pct_map = {
        cpu: runtime_ms / total_runtime_ms * 100.0
        for cpu, runtime_ms in cpu_runtime_ms.items()
        if runtime_ms > 0.0
    }
    return format_pct_map(pct_map)


def parse_cpu_pct_map(text: str) -> dict[int, float]:
    if not text:
        return {}
    values: dict[int, float] = {}
    for token in text.split(","):
        token = token.strip()
        if not token or ":" not in token:
            continue
        cpu_text, pct_text = token.split(":", 1)
        values[int(cpu_text)] = float(pct_text)
    return values


def format_omp_places(cpus: tuple[int, ...] | list[int]) -> str:
    return ",".join(f"{{{cpu}}}" for cpu in cpus)


def format_cpu_list(cpus: tuple[int, ...] | list[int]) -> str:
    return ",".join(str(cpu) for cpu in cpus)


def placement_tag_from_cpus(cpus: tuple[int, ...] | list[int]) -> str:
    return "cores-" + "-".join(str(cpu) for cpu in cpus)


def placement_tag_from_affinity(mask: str) -> str:
    return f"taskset-{mask}"


def build_thread_stats_rows(thread_results: list[dict[str, object]]) -> list[dict[str, object]]:
    grouped_threads: dict[tuple[str, str, str, str], list[dict[str, object]]] = defaultdict(list)
    for row in thread_results:
        key = (
            str(row["scenario"]),
            str(row["policy"]),
            str(row["benchmark"]),
            str(row["thread_slot"]),
        )
        grouped_threads[key].append(row)

    thread_stats_rows: list[dict[str, object]] = []
    for (scenario, policy, benchmark, thread_slot), rows in sorted(grouped_threads.items()):
        cpu_pct_totals: defaultdict[int, float] = defaultdict(float)
        for row in rows:
            for cpu, pct in parse_cpu_pct_map(str(row.get("core_pct_map", ""))).items():
                cpu_pct_totals[cpu] += pct

        mean_cpu_pct_map = {
            cpu: total_pct / len(rows)
            for cpu, total_pct in cpu_pct_totals.items()
            if total_pct > 0.0
        }

        thread_stats_rows.append(
            {
                "scenario": scenario,
                "policy": policy,
                "benchmark": benchmark,
                "thread_slot": thread_slot,
                "runs": len(rows),
                "mean_total_runtime_ms": f"{sum(float(row['total_runtime_ms']) for row in rows) / len(rows):.6f}",
                "ccd0_pct": f"{sum(float(row['ccd0_pct']) for row in rows) / len(rows):.2f}",
                "ccd1_pct": f"{sum(float(row['ccd1_pct']) for row in rows) / len(rows):.2f}",
                "ccd2_pct": f"{sum(float(row['ccd2_pct']) for row in rows) / len(rows):.2f}",
                "ccd3_pct": f"{sum(float(row['ccd3_pct']) for row in rows) / len(rows):.2f}",
                "other_pct": f"{sum(float(row['other_pct']) for row in rows) / len(rows):.2f}",
                "core_pct_map": format_pct_map(mean_cpu_pct_map),
            }
        )

    return thread_stats_rows


def summarize_thread_domains(
    timehist_path: Path,
    *,
    scenario: str,
    policy: str,
    benchmark: str,
    placement_tag: str,
    repeat: int,
) -> tuple[list[dict[str, object]], str]:
    entries = parse_perf_sched_timehist(timehist_path, comm_names=[benchmark])
    if not entries:
        return [], ""

    observed_cpus = sorted({int(entry["cpu"]) for entry in entries})
    per_tid: dict[int, dict[str, object]] = {}
    for entry in entries:
        tid = int(entry["tid"])
        pid = int(entry["pid"])
        cpu = int(entry["cpu"])
        run_time_ms = float(entry["run_time_ms"])
        domain = cpu_to_domain(cpu)

        if tid not in per_tid:
            per_tid[tid] = {
                "pid": pid,
                "total_runtime_ms": 0.0,
                "domains": defaultdict(float),
                "cpus": defaultdict(float),
            }

        per_tid[tid]["pid"] = pid
        per_tid[tid]["total_runtime_ms"] = float(per_tid[tid]["total_runtime_ms"]) + run_time_ms
        domains = per_tid[tid]["domains"]
        assert isinstance(domains, defaultdict)
        domains[domain] += run_time_ms
        cpus = per_tid[tid]["cpus"]
        assert isinstance(cpus, defaultdict)
        cpus[cpu] += run_time_ms

    thread_rows: list[dict[str, object]] = []
    worker_index = 0
    for tid, data in sorted(
        per_tid.items(),
        key=lambda item: (0 if item[0] == int(item[1]["pid"]) else 1, item[0]),
    ):
        pid = int(data["pid"])
        if tid == pid:
            thread_slot = "master"
        else:
            worker_index += 1
            thread_slot = f"worker-{worker_index}"

        total_runtime_ms = float(data["total_runtime_ms"])
        domains = data["domains"]
        assert isinstance(domains, defaultdict)
        cpus = data["cpus"]
        assert isinstance(cpus, defaultdict)
        thread_rows.append(
            {
                "scenario": scenario,
                "policy": policy,
                "benchmark": benchmark,
                "placement_tag": placement_tag,
                "repeat": repeat,
                "thread_slot": thread_slot,
                "thread_tid": tid,
                "thread_pid": pid,
                "total_runtime_ms": f"{total_runtime_ms:.6f}",
                "ccd0_pct": f"{(domains['ccd0'] / total_runtime_ms * 100.0) if total_runtime_ms else 0.0:.2f}",
                "ccd1_pct": f"{(domains['ccd1'] / total_runtime_ms * 100.0) if total_runtime_ms else 0.0:.2f}",
                "ccd2_pct": f"{(domains['ccd2'] / total_runtime_ms * 100.0) if total_runtime_ms else 0.0:.2f}",
                "ccd3_pct": f"{(domains['ccd3'] / total_runtime_ms * 100.0) if total_runtime_ms else 0.0:.2f}",
                "other_pct": f"{(domains['other'] / total_runtime_ms * 100.0) if total_runtime_ms else 0.0:.2f}",
                "core_pct_map": format_cpu_pct_map(cpus, total_runtime_ms),
            }
        )

    return thread_rows, ",".join(str(cpu) for cpu in observed_cpus)


def build_noise_layout(scenario: str) -> tuple[list[int], list[int]]:
    if scenario == FREE_SCENARIO_NAME:
        cores = [
            0, 1, 2, 3,
            7, 8, 9, 10,
            14, 15, 16, 17,
            21, 22, 23, 24,
        ]
        rates = [500] * 4 + [220] * 4 + [150] * 4 + [50] * 4
        return cores, rates

    if scenario == BUSY_SCENARIO_NAME:
        cores = list(range(0, 7)) + list(range(7, 14)) + list(range(14, 21)) + list(range(21, 28))
        rates = [500] * 7 + [220] * 7 + [150] * 7 + [50] * 7
        return cores, rates

    raise ValueError(f"Unknown scenario: {scenario}")


def build_benchmark_argv(benchmark: str, graph_scale: int) -> list[str]:
    argv = [BENCHMARKS[benchmark], "-g", str(graph_scale)]
    if benchmark == "bfs":
        argv.extend(["-n1"])
    elif benchmark == "pr":
        argv.extend(["-i100", "-n1"])
    elif benchmark == "cc":
        argv.extend(["-n1"])
    else:
        raise ValueError(f"Unknown benchmark: {benchmark}")
    return argv


def build_gapbs_variants(
    scenario: str,
    policy: str,
    benchmark: str,
    graph_scale: int,
) -> list[dict[str, object]]:
    argv = build_benchmark_argv(benchmark, graph_scale)
    base_env = build_env(updates={"OMP_NUM_THREADS": "3"}, removals=["OMP_PLACES", "OMP_PROC_BIND"])

    def build_variant(
        *,
        placement_tag: str,
        placement_detail: str,
        gapbs_argv: list[str],
        gapbs_env: dict[str, str],
    ) -> dict[str, object]:
        return {
            "placement_tag": placement_tag,
            "placement_detail": placement_detail,
            "gapbs_argv": gapbs_argv,
            "gapbs_env": gapbs_env,
        }

    if policy == "eevdf":
        return [
            build_variant(
                placement_tag="taskset-0-27",
                placement_detail="taskset -c 0-27",
                gapbs_argv=["taskset", "-c", "0-27", *argv],
                gapbs_env=base_env,
            )
        ]

    if scenario == FREE_SCENARIO_NAME and policy == "optimal":
        env = dict(base_env)
        env["OMP_PROC_BIND"] = "true"
        env["OMP_PLACES"] = format_omp_places(FREE_OPTIMAL_PLACEMENT)
        return [
            build_variant(
                placement_tag=placement_tag_from_cpus(FREE_OPTIMAL_PLACEMENT),
                placement_detail=f"OMP_PLACES={env['OMP_PLACES']}",
                gapbs_argv=list(argv),
                gapbs_env=env,
            )
        ]

    if scenario == FREE_SCENARIO_NAME and policy == "same-chiplet":
        variants: list[dict[str, object]] = []
        for cpus in FREE_SAME_CHIPLET_PLACEMENTS:
            env = dict(base_env)
            env["OMP_PROC_BIND"] = "true"
            env["OMP_PLACES"] = format_omp_places(cpus)
            variants.append(
                build_variant(
                    placement_tag=placement_tag_from_cpus(cpus),
                    placement_detail=f"OMP_PLACES={env['OMP_PLACES']}",
                    gapbs_argv=list(argv),
                    gapbs_env=env,
                )
            )
        return variants

    if scenario == BUSY_SCENARIO_NAME and policy == "optimal":
        env = dict(base_env)
        env["OMP_PROC_BIND"] = "true"
        return [
            build_variant(
                placement_tag=placement_tag_from_affinity(BUSY_OPTIMAL_AFFINITY),
                placement_detail=f"taskset -c {BUSY_OPTIMAL_AFFINITY}",
                gapbs_argv=["taskset", "-c", BUSY_OPTIMAL_AFFINITY, *argv],
                gapbs_env=env,
            )
        ]

    if scenario == BUSY_SCENARIO_NAME and policy == "same-chiplet":
        variants: list[dict[str, object]] = []
        for mask in BUSY_SAME_CHIPLET_AFFINITY_MASKS:
            env = dict(base_env)
            env["OMP_PROC_BIND"] = "true"
            variants.append(
                build_variant(
                    placement_tag=placement_tag_from_affinity(mask),
                    placement_detail=f"taskset -c {mask}",
                    gapbs_argv=["taskset", "-c", mask, *argv],
                    gapbs_env=env,
                )
            )
        return variants

    raise ValueError(f"Unsupported scenario/policy combination: {scenario}/{policy}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run exp1 GAPBS placement experiment.")
    parser.add_argument(
        "--scenarios",
        nargs="+",
        default=[FREE_SCENARIO_NAME, BUSY_SCENARIO_NAME],
        choices=[FREE_SCENARIO_NAME, BUSY_SCENARIO_NAME],
        help="Scenario subset to run.",
    )
    parser.add_argument(
        "--policies",
        nargs="+",
        default=["eevdf", "optimal", "same-chiplet"],
        choices=["eevdf", "optimal", "same-chiplet"],
        help="Policy subset to run.",
    )
    parser.add_argument(
        "--benchmarks",
        nargs="+",
        default=["bfs", "pr", "cc"],
        choices=sorted(BENCHMARKS),
        help="Benchmark subset to run.",
    )
    parser.add_argument("--repeats", type=int, default=5, help="Number of outer repeats per cell.")
    parser.add_argument(
        "--graph-scale",
        type=int,
        default=20,
        help="Kronecker graph scale used for GAPBS synthetic graph generation.",
    )
    parser.add_argument("--settle-seconds", type=float, default=2.0, help="Noise settle time before GAPBS.")
    parser.add_argument("--cooldown-seconds", type=float, default=2.0, help="Cooldown after each run.")
    parser.add_argument(
        "--noise-time-seconds",
        type=int,
        default=3600,
        help="Long-running timer passed to memory_benchmark before the runner terminates it.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=SCRIPT_DIR / "results" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"),
        help="Directory for CSVs, configs, and logs.",
    )
    parser.add_argument(
        "--perf-sched-mode",
        choices=["auto", "off", "required"],
        default="auto",
        help="Capture perf sched traces for each GAPBS run when tracepoint permissions allow it.",
    )
    parser.add_argument(
        "--perf-sched-sudo",
        action="store_true",
        help="Run perf sched commands through passwordless sudo.",
    )
    parser.add_argument("--skip-topology-check", action="store_true", help="Skip L3 topology validation.")
    parser.add_argument("--dry-run", action="store_true", help="Print the commands without executing them.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    require_path(MEMORY_BENCHMARK, "memory_benchmark", executable=True)
    require_path(GAPBS_DIR / "bfs", "GAPBS bfs", executable=True)
    require_path(GAPBS_DIR / "pr", "GAPBS pr", executable=True)
    require_path(GAPBS_DIR / "cc", "GAPBS cc", executable=True)

    if not args.skip_topology_check:
        validate_chiplet_groups(list(CHIPLETS.values()))

    output_dir = ensure_directory(args.output_dir)
    config_dir = ensure_directory(output_dir / "configs")
    raw_dir = ensure_directory(output_dir / "raw")
    summary_path = output_dir / "exp1_results.csv"
    stats_path = output_dir / "exp1_summary_stats.csv"
    thread_path = output_dir / "exp1_thread_domains.csv"
    thread_stats_path = output_dir / "exp1_thread_domain_summary.csv"
    metadata_path = output_dir / "run_metadata.txt"
    perf_sched_available = False
    perf_sched_reason = "disabled"

    if args.perf_sched_mode == "off":
        perf_sched_reason = "disabled by --perf-sched-mode off"
    else:
        perf_sched_available, perf_sched_reason = probe_perf_sched(args.perf_sched_sudo)
        if args.perf_sched_mode == "required" and not perf_sched_available:
            raise RuntimeError(f"perf sched is required but unavailable: {perf_sched_reason}")

    metadata_path.write_text(
        "\n".join(
            [
                "experiment=exp1",
                "graph_source=kronecker",
                f"graph_scale={args.graph_scale}",
                "omp_threads=3",
                f"repeats={args.repeats}",
                f"settle_seconds={args.settle_seconds}",
                f"cooldown_seconds={args.cooldown_seconds}",
                f"perf_sched_mode={args.perf_sched_mode}",
                f"perf_sched_transport={'sudo' if args.perf_sched_sudo else 'plain'}",
                f"perf_sched_status={'available' if perf_sched_available else 'unavailable'}",
                f"perf_sched_reason={perf_sched_reason}",
                f"free_cores_optimal_placement={format_cpu_list(FREE_OPTIMAL_PLACEMENT)}",
                "free_cores_same_chiplet_placements="
                + ";".join(format_cpu_list(cpus) for cpus in FREE_SAME_CHIPLET_PLACEMENTS),
                "free_cores_same_chiplet_aggregation=mean over all listed placements",
                f"busy_cores_optimal_affinity={BUSY_OPTIMAL_AFFINITY}",
                "busy_cores_same_chiplet_placements=" + ";".join(BUSY_SAME_CHIPLET_AFFINITY_MASKS),
                "busy_cores_same_chiplet_aggregation=mean over all listed placements",
            ]
        )
        + "\n"
    )

    noise_configs: dict[str, Path] = {}
    for scenario in args.scenarios:
        noise_cores, noise_rates = build_noise_layout(scenario)
        config_path = config_dir / f"{scenario}.xml"
        noise_output = output_dir / "noise_outputs" / f"{scenario}.txt"
        write_memory_benchmark_config(
            config_path,
            nthreads=len(noise_cores),
            cores=noise_cores,
            rates=noise_rates,
            worker_memory_mb=256,
            output_path=noise_output,
            duration_seconds=args.noise_time_seconds,
        )
        noise_configs[scenario] = config_path

    print(f"exp1 output directory: {output_dir}")
    print(f"exp1 graph source: kronecker scale {args.graph_scale}")
    print(
        "exp1 perf sched: "
        + ("available" if perf_sched_available else f"unavailable ({perf_sched_reason})")
    )
    results: list[dict[str, object]] = []
    thread_results: list[dict[str, object]] = []

    for scenario in args.scenarios:
        for policy in args.policies:
            for benchmark in args.benchmarks:
                gapbs_variants = build_gapbs_variants(
                    scenario,
                    policy,
                    benchmark,
                    args.graph_scale,
                )
                for gapbs_variant in gapbs_variants:
                    placement_tag = str(gapbs_variant["placement_tag"])
                    placement_detail = str(gapbs_variant["placement_detail"])
                    gapbs_argv = list(gapbs_variant["gapbs_argv"])
                    gapbs_env = dict(gapbs_variant["gapbs_env"])
                    for repeat in range(1, args.repeats + 1):
                        run_id = f"{scenario}__{policy}__{benchmark}__{placement_tag}__r{repeat}"
                        noise_argv = [str(MEMORY_BENCHMARK), "--config", str(noise_configs[scenario])]
                        noise_log = raw_dir / f"{run_id}__noise.log"
                        gapbs_log = raw_dir / f"{run_id}__gapbs.log"
                        perf_script_path = raw_dir / f"{run_id}__perf.script.log"
                        perf_timehist_path = raw_dir / f"{run_id}__perf.timehist.log"
                        perf_data_path = raw_dir / f"{run_id}__perf.data"

                        print(
                            f"[exp1] scenario={scenario} policy={policy} benchmark={benchmark} "
                            f"placement={placement_tag} repeat={repeat}"
                        )
                        print(f"  place : {placement_detail}")
                        print(f"  noise : {format_command(noise_argv)}")
                        print(f"  gapbs : {format_command(gapbs_argv)}")

                        if args.dry_run:
                            continue

                        background = start_background(noise_argv, log_path=noise_log)
                        try:
                            time.sleep(args.settle_seconds)
                            if perf_sched_available:
                                if args.perf_sched_sudo:
                                    record_argv = build_sudo_record_argv(gapbs_env, gapbs_argv, perf_data_path)
                                    record_env = None
                                else:
                                    record_argv = [
                                        "perf",
                                        "sched",
                                        "record",
                                        "--output",
                                        str(perf_data_path),
                                        "--",
                                        *gapbs_argv,
                                    ]
                                    record_env = gapbs_env
                                result = run_capture(
                                    record_argv,
                                    cwd=GAPBS_DIR,
                                    env=record_env,
                                    log_path=gapbs_log,
                                )
                                run_capture(
                                    build_perf_argv(
                                        [
                                            "perf",
                                            "script",
                                            "--input",
                                            str(perf_data_path),
                                            "--show-switch-events",
                                            "-F",
                                            "comm,tid,pid,time,cpu,event,trace",
                                        ],
                                        use_sudo=args.perf_sched_sudo,
                                    ),
                                    cwd=GAPBS_DIR,
                                    log_path=perf_script_path,
                                )
                                run_capture(
                                    build_perf_argv(
                                        [
                                            "perf",
                                            "sched",
                                            "timehist",
                                            "-i",
                                            str(perf_data_path),
                                        ],
                                        use_sudo=args.perf_sched_sudo,
                                    ),
                                    cwd=GAPBS_DIR,
                                    log_path=perf_timehist_path,
                                )
                                perf_status = "recorded"
                                perf_trace_value = str(perf_timehist_path)
                                thread_rows, perf_cpu_value = summarize_thread_domains(
                                    perf_timehist_path,
                                    scenario=scenario,
                                    policy=policy,
                                    benchmark=benchmark,
                                    placement_tag=placement_tag,
                                    repeat=repeat,
                                )
                                for thread_row in thread_rows:
                                    append_csv_row(thread_path, THREAD_FIELDNAMES, thread_row)
                                    thread_results.append(thread_row)
                            else:
                                result = run_capture(
                                    gapbs_argv,
                                    cwd=GAPBS_DIR,
                                    env=gapbs_env,
                                    log_path=gapbs_log,
                                )
                                perf_status = "unavailable"
                                perf_trace_value = ""
                                perf_cpu_value = ""
                        finally:
                            stop_background(background)

                        trial_time = parse_gapbs_trial_time(result.output)
                        row = {
                            "scenario": scenario,
                            "policy": policy,
                            "benchmark": benchmark,
                            "placement_tag": placement_tag,
                            "placement_detail": placement_detail,
                            "omp_threads": 3,
                            "repeat": repeat,
                            "trial_time_s": f"{trial_time:.6f}",
                            "perf_sched_status": perf_status,
                            "perf_sched_trace_path": perf_trace_value,
                            "perf_sched_cpus": perf_cpu_value,
                        }
                        append_csv_row(summary_path, FIELDNAMES, row)
                        results.append(row)
                        print(f"  trial_time_s={trial_time:.6f}")
                        if perf_trace_value:
                            print(f"  perf_sched_trace={perf_trace_value}")
                        if perf_cpu_value:
                            print(f"  perf_sched_cpus={perf_cpu_value}")
                        time.sleep(args.cooldown_seconds)

    if not args.dry_run and results:
        grouped: dict[tuple[str, str, str], list[float]] = defaultdict(list)
        for row in results:
            key = (str(row["scenario"]), str(row["policy"]), str(row["benchmark"]))
            grouped[key].append(float(row["trial_time_s"]))

        stats_rows: list[dict[str, object]] = []
        for (scenario, policy, benchmark), values in sorted(grouped.items()):
            stats_rows.append(
                {
                    "scenario": scenario,
                    "policy": policy,
                    "benchmark": benchmark,
                    "omp_threads": 3,
                    "runs": len(values),
                    "mean_trial_time_s": f"{sum(values) / len(values):.6f}",
                    "stdev_trial_time_s": f"{stdev(values):.6f}",
                }
            )
        write_csv_rows(stats_path, STATS_FIELDNAMES, stats_rows)
        if thread_results:
            thread_stats_rows = build_thread_stats_rows(thread_results)
            write_csv_rows(thread_stats_path, THREAD_STATS_FIELDNAMES, thread_stats_rows)
        print(f"exp1 results written to {summary_path}")
        print(f"exp1 summary stats written to {stats_path}")
        if thread_results:
            print(f"exp1 thread domains written to {thread_path}")
            print(f"exp1 thread domain summary written to {thread_stats_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
