#!/usr/bin/env python3

from __future__ import annotations

import argparse
import random
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
    build_env,
    ensure_directory,
    format_command,
    mean,
    parse_gapbs_trial_time,
    parse_noise_bandwidth_sum,
    require_path,
    run_capture,
    start_background,
    stdev,
    stop_background,
    write_csv_rows,
    write_memory_benchmark_config,
)


GAPBS_DIR = Path("/home/seunghyun/gapbs/gapbs")
MEMORY_BENCHMARK = Path("/home/seunghyun/sched_bench/build/memory_benchmark")
BENCHMARKS = {
    "bfs": str(GAPBS_DIR / "bfs"),
    "pr": str(GAPBS_DIR / "pr"),
}
PLACEMENT_MASKS = {
    "same-core": "4",
    "same-llc": "4-6",
    "optimal": "7-83",
}
PLACEMENT_ORDER = {name: index for index, name in enumerate(PLACEMENT_MASKS)}
DEFAULT_LOAD_PCTS = [0, 20, 40, 60, 80, 100]
TARGET_LOAD_PCTS = DEFAULT_LOAD_PCTS
CALIBRATION_RATE_CANDIDATES = [1000, 600, 500, 300, 220, 200, 150, 100, 50, 0]
NOISE_CORES = [0, 1, 2, 3]
NOISE_NUMA_NODE = 0
NOISE_WORKER_MEMORY_MB = 256
NOISE_RW_MODE = 0
CALIBRATION_DURATION_SECONDS = 3
SHUFFLE_SEED_BASE = 0

RESULT_FIELDNAMES = [
    "benchmark",
    "placement",
    "load_pct",
    "rate",
    "repeat",
    "trial_time_s",
    "slowdown_vs_load0",
    "sum_noise_bandwidth_mbs",
]
SUMMARY_FIELDNAMES = [
    "benchmark",
    "placement",
    "load_pct",
    "runs",
    "mean_trial_time_s",
    "stdev_trial_time_s",
    "mean_slowdown_vs_load0",
    "mean_noise_bandwidth_mbs",
]
CALIBRATION_FIELDNAMES = [
    "rate",
    "target_pct",
    "measured_sum_noise_bandwidth_mbs",
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run exp4 single-thread GAPBS placement experiment.")
    parser.add_argument(
        "--benchmarks",
        nargs="+",
        default=["bfs", "pr"],
        choices=["bfs", "pr"],
        help="Benchmark subset to run.",
    )
    parser.add_argument(
        "--placements",
        nargs="+",
        default=["same-core", "same-llc", "optimal"],
        choices=["same-core", "same-llc", "optimal"],
        help="Placement subset to run.",
    )
    parser.add_argument(
        "--load-pcts",
        nargs="+",
        type=int,
        default=DEFAULT_LOAD_PCTS,
        choices=DEFAULT_LOAD_PCTS,
        help="Background load percentages to run.",
    )
    parser.add_argument(
        "--load-rate-overrides",
        nargs="+",
        default=[],
        metavar="LOAD:RATE",
        help="Optional explicit load-to-rate overrides, for example 20:600 40:300 60:150.",
    )
    parser.add_argument("--repeats", type=int, default=5, help="Number of outer repeats per condition.")
    parser.add_argument(
        "--graph-scale",
        type=int,
        default=20,
        help="Kronecker graph scale used for GAPBS synthetic graph generation.",
    )
    parser.add_argument("--settle-seconds", type=float, default=2.0, help="Noise settle time before GAPBS.")
    parser.add_argument("--cooldown-seconds", type=float, default=2.0, help="Cooldown between runs.")
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
    parser.add_argument("--skip-topology-check", action="store_true", help="Skip CPU topology validation.")
    parser.add_argument("--dry-run", action="store_true", help="Print planned commands without executing them.")
    args = parser.parse_args()
    args.load_rate_override_map = parse_load_rate_overrides(args.load_rate_overrides)
    return args


def parse_load_rate_overrides(specs: list[str]) -> dict[int, int]:
    overrides: dict[int, int] = {}
    for spec in specs:
        if ":" not in spec:
            raise RuntimeError(f"Invalid --load-rate-overrides entry: {spec!r}; expected LOAD:RATE")
        load_text, rate_text = spec.split(":", 1)
        load_pct = int(load_text)
        rate = int(rate_text)
        if load_pct not in DEFAULT_LOAD_PCTS or load_pct == 0:
            raise RuntimeError(
                f"Override load percentage must be one of {DEFAULT_LOAD_PCTS[1:]}, got {load_pct}"
            )
        if rate < 0:
            raise RuntimeError(f"Override rate must be non-negative, got {rate}")
        overrides[load_pct] = rate
    return overrides


def build_gapbs_argv(benchmark: str, placement: str, graph_scale: int) -> tuple[list[str], dict[str, str]]:
    argv = [BENCHMARKS[benchmark], "-g", str(graph_scale)]
    if benchmark == "bfs":
        argv.extend(["-n1"])
    elif benchmark == "pr":
        argv.extend(["-i100", "-n1"])
    else:
        raise ValueError(f"Unknown benchmark: {benchmark}")

    env = build_env(updates={"OMP_NUM_THREADS": "1"}, removals=["OMP_PLACES", "OMP_PROC_BIND"])
    return ["taskset", "-c", PLACEMENT_MASKS[placement], *argv], env


def read_cpu_topology() -> dict[int, dict[str, int]]:
    result = run_capture(["lscpu", "-e=CPU,CORE,CACHE"])
    topology: dict[int, dict[str, int]] = {}
    for raw_line in result.output.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("CPU "):
            continue
        parts = line.split()
        if len(parts) < 3:
            continue
        cpu = int(parts[0])
        core = int(parts[1])
        l3 = int(parts[2].split(":")[-1])
        topology[cpu] = {"core": core, "l3": l3}
    return topology


def validate_exp4_topology() -> None:
    topology = read_cpu_topology()
    required_cpus = [0, 1, 2, 3, 4, 5, 6, 7]
    missing = [cpu for cpu in required_cpus if cpu not in topology]
    if missing:
        raise RuntimeError(f"Missing CPUs in lscpu topology output: {missing}")

    same_llc_l3s = {topology[cpu]["l3"] for cpu in [0, 1, 2, 3, 4, 5, 6]}
    if len(same_llc_l3s) != 1:
        raise RuntimeError(f"Expected CPUs 0-6 to share one L3, got {same_llc_l3s}")

    local_l3 = next(iter(same_llc_l3s))
    if topology[7]["l3"] == local_l3:
        raise RuntimeError(f"Expected CPU 7 to be on a different L3 than CPUs 0-6, got L3 {local_l3}")


def write_noise_config(
    output_dir: Path,
    *,
    name: str,
    rates: list[int],
    duration_seconds: int,
) -> Path:
    config_path = output_dir / "configs" / f"{name}.xml"
    result_path = output_dir / "noise_outputs" / f"{name}.txt"
    write_memory_benchmark_config(
        config_path,
        nthreads=len(NOISE_CORES),
        cores=NOISE_CORES,
        rates=rates,
        worker_memory_mb=NOISE_WORKER_MEMORY_MB,
        output_path=result_path,
        modes=[NOISE_RW_MODE] * len(NOISE_CORES),
        numa_nodes=[NOISE_NUMA_NODE] * len(NOISE_CORES),
        duration_seconds=duration_seconds,
        worker_numa_policy="bind",
    )
    return config_path


def select_calibration_rates(measured_rows: list[dict[str, float]]) -> dict[int, dict[str, float]]:
    if len(measured_rows) < len(TARGET_LOAD_PCTS) - 1:
        raise RuntimeError(
            f"Need at least {len(TARGET_LOAD_PCTS) - 1} calibration rows to map load percentages."
        )

    ordered = sorted(measured_rows, key=lambda row: (row["measured_sum_noise_bandwidth_mbs"], row["rate"]))
    max_bw = ordered[-1]["measured_sum_noise_bandwidth_mbs"]
    if max_bw <= 0.0:
        raise RuntimeError("Calibration produced zero bandwidth for all candidate rates.")

    selected: dict[int, dict[str, float]] = {}
    start_index = 0
    targets = [load_pct for load_pct in TARGET_LOAD_PCTS if 0 < load_pct < 100]
    for offset, target_pct in enumerate(targets):
        future_slots = len(targets) - offset
        end_index = len(ordered) - future_slots
        candidate_indices = list(range(start_index, end_index))
        if not candidate_indices:
            candidate_indices = [start_index]
        target_bw = max_bw * (target_pct / 100.0)
        chosen_index = min(
            candidate_indices,
            key=lambda index: abs(ordered[index]["measured_sum_noise_bandwidth_mbs"] - target_bw),
        )
        selected[target_pct] = ordered[chosen_index]
        start_index = chosen_index + 1

    selected[100] = ordered[-1]
    return selected


def run_calibration(
    *,
    output_dir: Path,
    raw_dir: Path,
    noise_time_seconds: int,
) -> tuple[dict[int, int], list[dict[str, object]]]:
    measured_rows: list[dict[str, float]] = []
    for rate in CALIBRATION_RATE_CANDIDATES:
        name = f"calibration__rate{rate}"
        config_path = write_noise_config(
            output_dir,
            name=name,
            rates=[rate] * len(NOISE_CORES),
            duration_seconds=CALIBRATION_DURATION_SECONDS,
        )
        argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
        log_path = raw_dir / f"{name}__noise.log"
        print(f"[exp4 calibration] rate={rate}")
        print(f"  noise: {format_command(argv)}")
        run_capture(argv, log_path=log_path)
        bandwidth = parse_noise_bandwidth_sum(log_path)
        print(f"  measured_sum_noise_bandwidth_mbs={bandwidth:.6f}")
        measured_rows.append(
            {
                "rate": float(rate),
                "measured_sum_noise_bandwidth_mbs": bandwidth,
            }
        )

    selected = select_calibration_rates(measured_rows)
    load_rate_map: dict[int, int] = {0: -1}
    calibration_rows: list[dict[str, object]] = []
    for target_pct in [load_pct for load_pct in TARGET_LOAD_PCTS if load_pct > 0]:
        selected_row = selected[target_pct]
        rate = int(selected_row["rate"])
        load_rate_map[target_pct] = rate
        calibration_rows.append(
            {
                "rate": rate,
                "target_pct": target_pct,
                "measured_sum_noise_bandwidth_mbs": f"{selected_row['measured_sum_noise_bandwidth_mbs']:.6f}",
            }
        )
        write_noise_config(
            output_dir,
            name=f"load{target_pct}__rate{rate}",
            rates=[rate] * len(NOISE_CORES),
            duration_seconds=noise_time_seconds,
        )
    return load_rate_map, calibration_rows


def write_explicit_rate_configs(
    *,
    output_dir: Path,
    load_rate_map: dict[int, int],
    noise_time_seconds: int,
) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for load_pct in sorted(load_pct for load_pct in load_rate_map if load_pct > 0):
        rate = int(load_rate_map[load_pct])
        write_noise_config(
            output_dir,
            name=f"load{load_pct}__rate{rate}",
            rates=[rate] * len(NOISE_CORES),
            duration_seconds=noise_time_seconds,
        )
        rows.append(
            {
                "rate": rate,
                "target_pct": load_pct,
                "measured_sum_noise_bandwidth_mbs": "",
            }
        )
    return rows


def build_conditions(args: argparse.Namespace) -> list[tuple[str, str, int]]:
    return [
        (benchmark, placement, load_pct)
        for benchmark in args.benchmarks
        for placement in args.placements
        for load_pct in args.load_pcts
    ]


def print_dry_run(args: argparse.Namespace, conditions: list[tuple[str, str, int]]) -> None:
    print(f"exp4 output directory: {args.output_dir}")
    if args.load_rate_override_map:
        print(
            "exp4 load-rate overrides: "
            + ",".join(f"{load}:{rate}" for load, rate in sorted(args.load_rate_override_map.items()))
        )
    if any(load_pct > 0 for load_pct in args.load_pcts) and not all(
        load_pct == 0 or load_pct in args.load_rate_override_map for load_pct in args.load_pcts
    ):
        print(
            "exp4 calibration candidates: "
            + ",".join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)
        )
    print(f"exp4 condition count: {len(conditions)}")
    print(f"exp4 repeats per condition: {args.repeats}")

    for benchmark, placement, load_pct in conditions:
        gapbs_argv, _gapbs_env = build_gapbs_argv(benchmark, placement, args.graph_scale)
        if load_pct == 0:
            rate_label = "none"
        else:
            rate_label = "<runtime-calibrated>"
        print(
            f"[exp4 dry-run] benchmark={benchmark} placement={placement} "
            f"load_pct={load_pct} rate={rate_label}"
        )
        if load_pct > 0:
            config_path = args.output_dir / "configs" / f"load{load_pct}__rateCALIBRATED.xml"
            noise_argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
            print(f"  noise: {format_command(noise_argv)}")
        print(f"  gapbs: {format_command(gapbs_argv)}")


def summarize_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    baseline_map: dict[tuple[str, str], float] = {}
    grouped_zero: defaultdict[tuple[str, str], list[float]] = defaultdict(list)
    for row in results:
        if int(row["load_pct"]) == 0:
            grouped_zero[(str(row["benchmark"]), str(row["placement"]))].append(float(row["trial_time_s"]))

    for key, values in grouped_zero.items():
        baseline_map[key] = mean(values)

    final_rows: list[dict[str, object]] = []
    for row in results:
        baseline_key = (str(row["benchmark"]), str(row["placement"]))
        if baseline_key not in baseline_map:
            raise RuntimeError(f"Missing 0-load baseline for {baseline_key}")
        slowdown = float(row["trial_time_s"]) / baseline_map[baseline_key]
        final_row = dict(row)
        final_row["slowdown_vs_load0"] = f"{slowdown:.6f}"
        final_rows.append(final_row)

    final_rows.sort(
        key=lambda row: (
            str(row["benchmark"]),
            PLACEMENT_ORDER[str(row["placement"])],
            int(row["load_pct"]),
            int(row["repeat"]),
        )
    )

    grouped_rows: defaultdict[tuple[str, str, int], list[dict[str, object]]] = defaultdict(list)
    for row in final_rows:
        grouped_rows[(str(row["benchmark"]), str(row["placement"]), int(row["load_pct"]))].append(row)

    summary_rows: list[dict[str, object]] = []
    for benchmark, placement, load_pct in sorted(
        grouped_rows,
        key=lambda key: (key[0], PLACEMENT_ORDER[key[1]], key[2]),
    ):
        rows = grouped_rows[(benchmark, placement, load_pct)]
        trial_values = [float(row["trial_time_s"]) for row in rows]
        slowdown_values = [float(row["slowdown_vs_load0"]) for row in rows]
        noise_values = [float(row["sum_noise_bandwidth_mbs"]) for row in rows]
        summary_rows.append(
            {
                "benchmark": benchmark,
                "placement": placement,
                "load_pct": load_pct,
                "runs": len(rows),
                "mean_trial_time_s": f"{mean(trial_values):.6f}",
                "stdev_trial_time_s": f"{stdev(trial_values):.6f}",
                "mean_slowdown_vs_load0": f"{mean(slowdown_values):.6f}",
                "mean_noise_bandwidth_mbs": f"{mean(noise_values):.6f}",
            }
        )

    return final_rows, summary_rows


def main() -> int:
    args = parse_args()

    if args.repeats <= 0:
        raise RuntimeError("--repeats must be positive")
    if any(load_pct > 0 for load_pct in args.load_pcts) and 0 not in args.load_pcts:
        raise RuntimeError("A 0-load baseline is required when any non-zero load is selected.")

    require_path(MEMORY_BENCHMARK, "memory_benchmark", executable=True)
    require_path(GAPBS_DIR / "bfs", "GAPBS bfs", executable=True)
    require_path(GAPBS_DIR / "pr", "GAPBS pr", executable=True)

    if not args.skip_topology_check:
        validate_exp4_topology()

    conditions = build_conditions(args)
    if args.dry_run:
        print_dry_run(args, conditions)
        return 0

    output_dir = ensure_directory(args.output_dir)
    raw_dir = ensure_directory(output_dir / "raw")
    ensure_directory(output_dir / "configs")
    calibration_path = output_dir / "exp4_calibration.csv"
    results_path = output_dir / "exp4_results.csv"
    summary_path = output_dir / "exp4_summary_stats.csv"
    metadata_path = output_dir / "run_metadata.txt"

    print(f"exp4 output directory: {output_dir}")

    load_rate_map: dict[int, int] = {0: -1}
    calibration_rows: list[dict[str, object]] = []
    requested_positive_loads = sorted(load_pct for load_pct in args.load_pcts if load_pct > 0)
    missing_override_loads = [
        load_pct for load_pct in requested_positive_loads if load_pct not in args.load_rate_override_map
    ]
    if requested_positive_loads and missing_override_loads:
        load_rate_map, calibration_rows = run_calibration(
            output_dir=output_dir,
            raw_dir=raw_dir,
            noise_time_seconds=args.noise_time_seconds,
        )
    load_rate_map.update(args.load_rate_override_map)
    if args.load_rate_override_map:
        calibration_rows = [
            row
            for row in calibration_rows
            if int(row["target_pct"]) not in args.load_rate_override_map
        ]
        calibration_rows.extend(
            write_explicit_rate_configs(
                output_dir=output_dir,
                load_rate_map=args.load_rate_override_map,
                noise_time_seconds=args.noise_time_seconds,
            )
        )
    if requested_positive_loads:
        write_csv_rows(calibration_path, CALIBRATION_FIELDNAMES, calibration_rows)
        for row in calibration_rows:
            print(
                f"[exp4 calibration] target_pct={row['target_pct']} "
                f"rate={row['rate']} "
                f"measured_sum_noise_bandwidth_mbs={row['measured_sum_noise_bandwidth_mbs']}"
            )

    results: list[dict[str, object]] = []
    metadata_lines = [
        "experiment=exp4",
        "graph_source=kronecker",
        f"graph_scale={args.graph_scale}",
        f"benchmarks={','.join(args.benchmarks)}",
        f"placements={','.join(args.placements)}",
        "placement_masks="
        + ",".join(f"{name}:{PLACEMENT_MASKS[name]}" for name in args.placements),
        f"load_pcts={','.join(str(load_pct) for load_pct in args.load_pcts)}",
        f"repeats={args.repeats}",
        f"noise_cores={','.join(str(core) for core in NOISE_CORES)}",
        f"noise_numa_node={NOISE_NUMA_NODE}",
        f"noise_worker_memory_mb={NOISE_WORKER_MEMORY_MB}",
        f"noise_mode={NOISE_RW_MODE}",
        "worker_numa_policy=bind",
        f"calibration_rate_candidates={','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)}",
        "load_rate_overrides="
        + ",".join(f"{load}:{rate}" for load, rate in sorted(args.load_rate_override_map.items())),
        f"settle_seconds={args.settle_seconds}",
        f"cooldown_seconds={args.cooldown_seconds}",
        f"noise_time_seconds={args.noise_time_seconds}",
        f"shuffle_seed_base={SHUFFLE_SEED_BASE}",
    ]
    if calibration_rows:
        metadata_lines.append(
            "selected_load_rates="
            + ",".join(
                f"{row['target_pct']}:{row['rate']}"
                for row in calibration_rows
            )
        )

    for repeat in range(1, args.repeats + 1):
        repeat_seed = SHUFFLE_SEED_BASE + repeat
        repeat_conditions = list(conditions)
        random.Random(repeat_seed).shuffle(repeat_conditions)
        metadata_lines.append(
            f"repeat_{repeat}_seed={repeat_seed}"
        )
        metadata_lines.append(
            "repeat_{}_order={}".format(
                repeat,
                "|".join(
                    f"{benchmark}:{placement}:{load_pct}"
                    for benchmark, placement, load_pct in repeat_conditions
                ),
            )
        )

        for benchmark, placement, load_pct in repeat_conditions:
            gapbs_argv, gapbs_env = build_gapbs_argv(benchmark, placement, args.graph_scale)
            rate = load_rate_map.get(load_pct, -1)
            run_prefix = f"{benchmark}__{placement}__load{load_pct}__r{repeat}"
            gapbs_log = raw_dir / f"{run_prefix}__gapbs.log"
            sum_noise_bandwidth = 0.0
            background = None
            noise_log: Path | None = None

            print(
                f"[exp4] repeat={repeat} benchmark={benchmark} placement={placement} "
                f"load_pct={load_pct} rate={'none' if load_pct == 0 else rate}"
            )

            if load_pct > 0:
                config_name = f"load{load_pct}__rate{rate}.xml"
                config_path = output_dir / "configs" / config_name
                noise_argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
                noise_log = raw_dir / f"{run_prefix}__noise.log"
                print(f"  noise: {format_command(noise_argv)}")
                background = start_background(noise_argv, log_path=noise_log)
                time.sleep(args.settle_seconds)

            print(f"  gapbs: {format_command(gapbs_argv)}")
            try:
                result = run_capture(gapbs_argv, cwd=GAPBS_DIR, env=gapbs_env, log_path=gapbs_log)
                trial_time = parse_gapbs_trial_time(result.output)
            finally:
                if background is not None:
                    stop_background(background)
                    if noise_log is not None:
                        sum_noise_bandwidth = parse_noise_bandwidth_sum(noise_log)

            print(
                f"  trial_time_s={trial_time:.6f} "
                f"sum_noise_bandwidth_mbs={sum_noise_bandwidth:.6f}"
            )
            results.append(
                {
                    "benchmark": benchmark,
                    "placement": placement,
                    "load_pct": load_pct,
                    "rate": "" if load_pct == 0 else rate,
                    "repeat": repeat,
                    "trial_time_s": f"{trial_time:.6f}",
                    "slowdown_vs_load0": "",
                    "sum_noise_bandwidth_mbs": f"{sum_noise_bandwidth:.6f}",
                }
            )

            time.sleep(args.cooldown_seconds)

    metadata_path.write_text("\n".join(metadata_lines) + "\n")

    final_rows, summary_rows = summarize_results(results)
    write_csv_rows(results_path, RESULT_FIELDNAMES, final_rows)
    write_csv_rows(summary_path, SUMMARY_FIELDNAMES, summary_rows)

    print(f"exp4 results written to: {results_path}")
    print(f"exp4 summary written to: {summary_path}")
    if calibration_rows:
        print(f"exp4 calibration written to: {calibration_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
