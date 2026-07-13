#!/usr/bin/env python3

from __future__ import annotations

import argparse
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
ROOT_DIR = SCRIPT_DIR.parent
if str(ROOT_DIR) not in sys.path:
    sys.path.insert(0, str(ROOT_DIR))

from experiment_utils import (  # noqa: E402
    append_csv_row,
    ensure_directory,
    format_command,
    mean,
    parse_mean_bandwidth_gibs,
    parse_noise_bandwidth_sum,
    require_path,
    run_capture,
    start_background,
    stdev,
    stop_background,
    validate_chiplet_groups,
    write_memory_benchmark_config,
)


C2C_BIN = Path("/home/seunghyun/core-to-core-latency/target/release/core-to-core-latency")
MEMORY_BENCHMARK = Path("/home/seunghyun/sched_bench/build/memory_benchmark")
CHIPLETS = [
    [0, 1, 2, 3, 4, 5, 6],
    [7, 8, 9, 10, 11, 12, 13],
    [14, 15, 16, 17, 18, 19, 20],
    [21, 22, 23, 24, 25, 26, 27],
]
PAIR_DEFS = {
    "intra": {"cores": (0, 1), "l3_noise_pool": [2, 3, 4, 5, 6]},
    "inter": {"cores": (0, 7), "l3_noise_pool": [1, 2, 3, 4, 5]},
}
IO_CORES = list(range(14, 84))
PHYSICAL_CORES = list(range(0, 84))
DEFAULT_SWEEP_PAYLOAD_KIB = [64, 128, 256, 512]
BENCH_LABELS = {
    4: "synchronized one-way cache-to-cache bandwidth",
    5: "duplex double-buffer streaming bandwidth",
}
FIELDNAMES = [
    "pair",
    "traffic_type",
    "traffic_threads",
    "payload_kib",
    "bandwidth_runs",
    "mean_bandwidth_gibs",
    "stdev_bandwidth_gibs",
    "sum_noise_bandwidth_mbs",
]
RAW_FIELDNAMES = [
    "pair",
    "traffic_type",
    "traffic_threads",
    "payload_kib",
    "repeat",
    "bandwidth_gibs",
]
SWEEP_FIELDNAMES = [
    "pair",
    "payload_kib",
    "bandwidth_runs",
    "mean_bandwidth_gibs",
    "stdev_bandwidth_gibs",
]
SWEEP_RAW_FIELDNAMES = [
    "pair",
    "payload_kib",
    "repeat",
    "bandwidth_gibs",
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run exp3 synchronized cache-to-cache bandwidth experiment."
    )
    parser.add_argument(
        "--pairs",
        nargs="+",
        default=["intra", "inter"],
        choices=["intra", "inter"],
        help="Pair subset to run.",
    )
    parser.add_argument(
        "--traffic-types",
        nargs="+",
        default=["none", "l3", "io", "full-load"],
        choices=["none", "l3", "io", "full-load"],
        help="Traffic subset to run.",
    )
    parser.add_argument("--iterations", type=int, default=256, help="Bandwidth benchmark iterations.")
    parser.add_argument("--samples", type=int, default=1, help="Bandwidth benchmark samples.")
    parser.add_argument(
        "--bench-id",
        type=int,
        default=4,
        choices=sorted(BENCH_LABELS),
        help="core-to-core-latency benchmark id to run.",
    )
    parser.add_argument(
        "--bandwidth-repeats",
        type=int,
        default=10,
        help="Number of outer bandwidth runs per condition.",
    )
    parser.add_argument(
        "--sweep-repeats",
        type=int,
        default=3,
        help="Number of no-traffic repeats per payload in the pre-run sweep.",
    )
    parser.add_argument(
        "--sweep-payload-kib",
        nargs="+",
        type=int,
        default=DEFAULT_SWEEP_PAYLOAD_KIB,
        help="Payload sizes for the no-traffic sweep, in KiB.",
    )
    parser.add_argument(
        "--plateau-threshold",
        type=float,
        default=0.95,
        help="Select 256KiB when it reaches this fraction of the 512KiB throughput.",
    )
    parser.add_argument(
        "--fixed-payload-bytes",
        type=int,
        help="Skip the sweep and force a specific payload size in bytes.",
    )
    parser.add_argument("--l3-threads", type=int, default=5, help="Fixed L3 noise thread count.")
    parser.add_argument(
        "--l3-worker-memory-mb",
        type=int,
        default=4,
        help="Per-thread memory footprint for the L3 noise condition.",
    )
    parser.add_argument("--settle-seconds", type=float, default=2.0, help="Noise settle time.")
    parser.add_argument("--cooldown-seconds", type=float, default=2.0, help="Cooldown between conditions.")
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
    parser.add_argument("--skip-topology-check", action="store_true", help="Skip L3 topology validation.")
    parser.add_argument("--dry-run", action="store_true", help="Print the commands without executing them.")
    return parser.parse_args()


def build_bandwidth_command(
    *,
    bench_id: int,
    iterations: int,
    samples: int,
    pair: str,
    payload_bytes: int,
) -> list[str]:
    core_a, core_b = PAIR_DEFS[pair]["cores"]
    return [
        str(C2C_BIN),
        str(iterations),
        str(samples),
        "--csv",
        "--bench",
        str(bench_id),
        "--payload-bytes",
        str(payload_bytes),
        "--cores",
        f"{core_a},{core_b}",
    ]


def write_noise_config(
    output_dir: Path,
    *,
    name: str,
    cores: list[int],
    worker_memory_mb: int,
    duration_seconds: int,
) -> Path:
    config_path = output_dir / "configs" / f"{name}.xml"
    result_path = output_dir / "noise_outputs" / f"{name}.txt"
    write_memory_benchmark_config(
        config_path,
        nthreads=len(cores),
        cores=cores,
        rates=[0] * len(cores),
        worker_memory_mb=worker_memory_mb,
        output_path=result_path,
        duration_seconds=duration_seconds,
    )
    return config_path


def build_full_load_cores(pair: str) -> list[int]:
    measured = set(PAIR_DEFS[pair]["cores"])
    return [core for core in PHYSICAL_CORES if core not in measured]


def run_bandwidth_repeats(
    *,
    run_prefix: str,
    raw_dir: Path,
    bandwidth_argv: list[str],
    repeats: int,
    raw_results_path: Path,
    raw_fieldnames: list[str],
    raw_base_row: dict[str, object],
) -> list[float]:
    values: list[float] = []
    for repeat in range(1, repeats + 1):
        log_path = raw_dir / f"{run_prefix}__r{repeat}__bandwidth.log"
        result = run_capture(bandwidth_argv, log_path=log_path)
        bandwidth_gibs = parse_mean_bandwidth_gibs(result.output)
        row = dict(raw_base_row)
        row["repeat"] = repeat
        row["bandwidth_gibs"] = f"{bandwidth_gibs:.6f}"
        append_csv_row(raw_results_path, raw_fieldnames, row)
        print(f"    repeat={repeat} bandwidth_gibs={bandwidth_gibs:.6f}")
        values.append(bandwidth_gibs)
    return values


def summarize_bandwidth_condition(
    *,
    pair: str,
    traffic_type: str,
    traffic_threads: int,
    payload_kib: int,
    bandwidth_values: list[float],
    sum_noise_bandwidth: float,
) -> dict[str, object]:
    return {
        "pair": pair,
        "traffic_type": traffic_type,
        "traffic_threads": traffic_threads,
        "payload_kib": payload_kib,
        "bandwidth_runs": len(bandwidth_values),
        "mean_bandwidth_gibs": f"{mean(bandwidth_values):.6f}",
        "stdev_bandwidth_gibs": f"{stdev(bandwidth_values):.6f}",
        "sum_noise_bandwidth_mbs": f"{sum_noise_bandwidth:.6f}",
    }


def choose_payload_bytes(
    *,
    sweep_means: dict[str, dict[int, float]],
    pairs: list[str],
    threshold: float,
) -> int:
    if all(256 in sweep_means.get(pair, {}) and 512 in sweep_means.get(pair, {}) for pair in pairs):
        if all(sweep_means[pair][256] >= threshold * sweep_means[pair][512] for pair in pairs):
            return 256 * 1024
        return 512 * 1024

    available_payloads = sorted({payload for pair_rows in sweep_means.values() for payload in pair_rows})
    if not available_payloads:
        return 256 * 1024
    return available_payloads[-1] * 1024


def write_metadata(
    metadata_path: Path,
    *,
    args: argparse.Namespace,
    payload_bytes: int,
    sweep_ran: bool,
) -> None:
    metadata_path.write_text(
        "\n".join(
            [
                "experiment=exp3",
                f"bench_id={args.bench_id}",
                f"benchmark_label={BENCH_LABELS[args.bench_id]}",
                f"pairs={','.join(args.pairs)}",
                f"traffic_types={','.join(args.traffic_types)}",
                f"iterations={args.iterations}",
                f"samples={args.samples}",
                f"bandwidth_repeats={args.bandwidth_repeats}",
                f"sweep_repeats={args.sweep_repeats}",
                f"sweep_payload_kib={','.join(str(value) for value in args.sweep_payload_kib)}",
                f"plateau_threshold={args.plateau_threshold}",
                f"payload_selection={'sweep' if sweep_ran else 'fixed'}",
                f"chosen_payload_bytes={payload_bytes}",
                f"chosen_payload_kib={payload_bytes // 1024}",
                f"l3_threads={args.l3_threads}",
                f"l3_worker_memory_mb={args.l3_worker_memory_mb}",
                f"settle_seconds={args.settle_seconds}",
                f"cooldown_seconds={args.cooldown_seconds}",
            ]
        )
        + "\n"
    )


def main() -> int:
    args = parse_args()

    require_path(C2C_BIN, "core-to-core-latency", executable=True)
    require_path(MEMORY_BENCHMARK, "memory_benchmark", executable=True)

    if not args.skip_topology_check:
        mapping = validate_chiplet_groups(CHIPLETS)
        if mapping[0] != mapping[1]:
            raise RuntimeError("Expected cores 0 and 1 to share the same L3 domain.")
        if mapping[0] == mapping[7]:
            raise RuntimeError("Expected cores 0 and 7 to be on different L3 domains.")

    output_dir = ensure_directory(args.output_dir)
    raw_dir = ensure_directory(output_dir / "raw")
    ensure_directory(output_dir / "configs")
    summary_path = output_dir / "exp3_results.csv"
    raw_results_path = output_dir / "exp3_bandwidth_runs.csv"
    sweep_summary_path = output_dir / "exp3_payload_sweep.csv"
    sweep_raw_path = output_dir / "exp3_payload_sweep_runs.csv"
    metadata_path = output_dir / "run_metadata.txt"

    print(f"exp3 output directory: {output_dir}")

    sweep_means: dict[str, dict[int, float]] = {}
    sweep_ran = args.fixed_payload_bytes is None

    if args.fixed_payload_bytes is not None:
        payload_bytes = args.fixed_payload_bytes
        print(f"[exp3] fixed payload selected: {payload_bytes} bytes")
    else:
        payload_bytes = 256 * 1024
        for pair in args.pairs:
            for payload_kib in args.sweep_payload_kib:
                bandwidth_argv = build_bandwidth_command(
                    bench_id=args.bench_id,
                    iterations=args.iterations,
                    samples=args.samples,
                    pair=pair,
                    payload_bytes=payload_kib * 1024,
                )
                run_prefix = f"sweep__{pair}__{payload_kib}kib"
                print(f"[exp3 sweep] pair={pair} payload={payload_kib}KiB")
                print(f"  bandwidth: {format_command(bandwidth_argv)}")
                if args.dry_run:
                    continue

                values = run_bandwidth_repeats(
                    run_prefix=run_prefix,
                    raw_dir=raw_dir,
                    bandwidth_argv=bandwidth_argv,
                    repeats=args.sweep_repeats,
                    raw_results_path=sweep_raw_path,
                    raw_fieldnames=SWEEP_RAW_FIELDNAMES,
                    raw_base_row={"pair": pair, "payload_kib": payload_kib},
                )
                row = {
                    "pair": pair,
                    "payload_kib": payload_kib,
                    "bandwidth_runs": len(values),
                    "mean_bandwidth_gibs": f"{mean(values):.6f}",
                    "stdev_bandwidth_gibs": f"{stdev(values):.6f}",
                }
                append_csv_row(sweep_summary_path, SWEEP_FIELDNAMES, row)
                sweep_means.setdefault(pair, {})[payload_kib] = mean(values)
                print(f"  mean_bandwidth_gibs={row['mean_bandwidth_gibs']}")
                print(f"  stdev_bandwidth_gibs={row['stdev_bandwidth_gibs']}")
                time.sleep(args.cooldown_seconds)

        if not args.dry_run:
            payload_bytes = choose_payload_bytes(
                sweep_means=sweep_means,
                pairs=list(args.pairs),
                threshold=args.plateau_threshold,
            )
        print(f"[exp3] selected payload: {payload_bytes} bytes ({payload_bytes // 1024}KiB)")

    write_metadata(
        metadata_path,
        args=args,
        payload_bytes=payload_bytes,
        sweep_ran=sweep_ran,
    )

    payload_kib = payload_bytes // 1024

    for pair in args.pairs:
        bandwidth_argv = build_bandwidth_command(
            bench_id=args.bench_id,
            iterations=args.iterations,
            samples=args.samples,
            pair=pair,
            payload_bytes=payload_bytes,
        )

        if "none" in args.traffic_types:
            print(f"[exp3] pair={pair} traffic=none payload={payload_kib}KiB")
            print(f"  bandwidth: {format_command(bandwidth_argv)}")
            if not args.dry_run:
                values = run_bandwidth_repeats(
                    run_prefix=f"{pair}__none__0t",
                    raw_dir=raw_dir,
                    bandwidth_argv=bandwidth_argv,
                    repeats=args.bandwidth_repeats,
                    raw_results_path=raw_results_path,
                    raw_fieldnames=RAW_FIELDNAMES,
                    raw_base_row={
                        "pair": pair,
                        "traffic_type": "none",
                        "traffic_threads": 0,
                        "payload_kib": payload_kib,
                    },
                )
                row = summarize_bandwidth_condition(
                    pair=pair,
                    traffic_type="none",
                    traffic_threads=0,
                    payload_kib=payload_kib,
                    bandwidth_values=values,
                    sum_noise_bandwidth=0.0,
                )
                append_csv_row(summary_path, FIELDNAMES, row)
                print(f"  mean_bandwidth_gibs={row['mean_bandwidth_gibs']}")
                print(f"  stdev_bandwidth_gibs={row['stdev_bandwidth_gibs']}")
                time.sleep(args.cooldown_seconds)

        if "l3" in args.traffic_types:
            noise_cores = list(PAIR_DEFS[pair]["l3_noise_pool"][: args.l3_threads])
            config_path = write_noise_config(
                output_dir,
                name=f"{pair}__l3__{args.l3_threads}t",
                cores=noise_cores,
                worker_memory_mb=args.l3_worker_memory_mb,
                duration_seconds=args.noise_time_seconds,
            )
            noise_argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
            noise_log = raw_dir / f"{pair}__l3__{args.l3_threads}t__noise.log"
            print(f"[exp3] pair={pair} traffic=l3 threads={args.l3_threads} payload={payload_kib}KiB")
            print(f"  noise    : {format_command(noise_argv)}")
            print(f"  bandwidth: {format_command(bandwidth_argv)}")
            if not args.dry_run:
                background = start_background(noise_argv, log_path=noise_log)
                try:
                    time.sleep(args.settle_seconds)
                    values = run_bandwidth_repeats(
                        run_prefix=f"{pair}__l3__{args.l3_threads}t",
                        raw_dir=raw_dir,
                        bandwidth_argv=bandwidth_argv,
                        repeats=args.bandwidth_repeats,
                        raw_results_path=raw_results_path,
                        raw_fieldnames=RAW_FIELDNAMES,
                        raw_base_row={
                            "pair": pair,
                            "traffic_type": "l3",
                            "traffic_threads": args.l3_threads,
                            "payload_kib": payload_kib,
                        },
                    )
                finally:
                    stop_background(background)

                sum_noise_bandwidth = parse_noise_bandwidth_sum(noise_log)
                row = summarize_bandwidth_condition(
                    pair=pair,
                    traffic_type="l3",
                    traffic_threads=args.l3_threads,
                    payload_kib=payload_kib,
                    bandwidth_values=values,
                    sum_noise_bandwidth=sum_noise_bandwidth,
                )
                append_csv_row(summary_path, FIELDNAMES, row)
                print(f"  mean_bandwidth_gibs={row['mean_bandwidth_gibs']}")
                print(f"  stdev_bandwidth_gibs={row['stdev_bandwidth_gibs']}")
                print(f"  sum_noise_bandwidth_mbs={sum_noise_bandwidth:.6f}")
                time.sleep(args.cooldown_seconds)

        if pair == "inter" and "io" in args.traffic_types:
            config_path = write_noise_config(
                output_dir,
                name="inter__io",
                cores=IO_CORES,
                worker_memory_mb=64,
                duration_seconds=args.noise_time_seconds,
            )
            noise_argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
            noise_log = raw_dir / "inter__io__noise.log"
            print(f"[exp3] pair=inter traffic=io threads={len(IO_CORES)} payload={payload_kib}KiB")
            print(f"  noise    : {format_command(noise_argv)}")
            print(f"  bandwidth: {format_command(bandwidth_argv)}")
            if not args.dry_run:
                background = start_background(noise_argv, log_path=noise_log)
                try:
                    time.sleep(args.settle_seconds)
                    values = run_bandwidth_repeats(
                        run_prefix="inter__io",
                        raw_dir=raw_dir,
                        bandwidth_argv=bandwidth_argv,
                        repeats=args.bandwidth_repeats,
                        raw_results_path=raw_results_path,
                        raw_fieldnames=RAW_FIELDNAMES,
                        raw_base_row={
                            "pair": "inter",
                            "traffic_type": "io",
                            "traffic_threads": len(IO_CORES),
                            "payload_kib": payload_kib,
                        },
                    )
                finally:
                    stop_background(background)

                sum_noise_bandwidth = parse_noise_bandwidth_sum(noise_log)
                row = summarize_bandwidth_condition(
                    pair="inter",
                    traffic_type="io",
                    traffic_threads=len(IO_CORES),
                    payload_kib=payload_kib,
                    bandwidth_values=values,
                    sum_noise_bandwidth=sum_noise_bandwidth,
                )
                append_csv_row(summary_path, FIELDNAMES, row)
                print(f"  mean_bandwidth_gibs={row['mean_bandwidth_gibs']}")
                print(f"  stdev_bandwidth_gibs={row['stdev_bandwidth_gibs']}")
                print(f"  sum_noise_bandwidth_mbs={sum_noise_bandwidth:.6f}")
                time.sleep(args.cooldown_seconds)

        if "full-load" in args.traffic_types:
            full_load_cores = build_full_load_cores(pair)
            config_path = write_noise_config(
                output_dir,
                name=f"{pair}__full-load__{len(full_load_cores)}t",
                cores=full_load_cores,
                worker_memory_mb=64,
                duration_seconds=args.noise_time_seconds,
            )
            noise_argv = [str(MEMORY_BENCHMARK), "--config", str(config_path)]
            noise_log = raw_dir / f"{pair}__full-load__{len(full_load_cores)}t__noise.log"
            print(
                f"[exp3] pair={pair} traffic=full-load threads={len(full_load_cores)} "
                f"payload={payload_kib}KiB"
            )
            print(f"  noise    : {format_command(noise_argv)}")
            print(f"  bandwidth: {format_command(bandwidth_argv)}")
            if not args.dry_run:
                background = start_background(noise_argv, log_path=noise_log)
                try:
                    time.sleep(args.settle_seconds)
                    values = run_bandwidth_repeats(
                        run_prefix=f"{pair}__full-load__{len(full_load_cores)}t",
                        raw_dir=raw_dir,
                        bandwidth_argv=bandwidth_argv,
                        repeats=args.bandwidth_repeats,
                        raw_results_path=raw_results_path,
                        raw_fieldnames=RAW_FIELDNAMES,
                        raw_base_row={
                            "pair": pair,
                            "traffic_type": "full-load",
                            "traffic_threads": len(full_load_cores),
                            "payload_kib": payload_kib,
                        },
                    )
                finally:
                    stop_background(background)

                sum_noise_bandwidth = parse_noise_bandwidth_sum(noise_log)
                row = summarize_bandwidth_condition(
                    pair=pair,
                    traffic_type="full-load",
                    traffic_threads=len(full_load_cores),
                    payload_kib=payload_kib,
                    bandwidth_values=values,
                    sum_noise_bandwidth=sum_noise_bandwidth,
                )
                append_csv_row(summary_path, FIELDNAMES, row)
                print(f"  mean_bandwidth_gibs={row['mean_bandwidth_gibs']}")
                print(f"  stdev_bandwidth_gibs={row['stdev_bandwidth_gibs']}")
                print(f"  sum_noise_bandwidth_mbs={sum_noise_bandwidth:.6f}")
                time.sleep(args.cooldown_seconds)

    if not args.dry_run:
        print(f"exp3 results written to {summary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
