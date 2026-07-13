#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import subprocess
import tempfile
from pathlib import Path

from active_configs import FIXED_WORKLOAD_CONFIGS, resolve_config_paths
from experiment_utils import (
    HARNESS,
    MEMORY_BENCHMARK,
    NOISE_READY_MARKER,
    ROOT_DIR,
    discover_smt_pairs,
    generate_memory_benchmark_xml,
    load_json,
    memory_benchmark_cmd,
    persist_external_noise_artifacts,
    stop_process,
    wait_for_process_ready,
    write_json,
)


DEFAULT_BASE_CONFIGS = FIXED_WORKLOAD_CONFIGS
TARGET_PHYSICAL_CORES = tuple(range(7))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("counts", nargs="*", type=int, default=list(range(1, 8)))
    parser.add_argument(
        "--config",
        dest="configs",
        action="append",
        default=None,
        help="Base config to run. Repeat to select a subset.",
    )
    parser.add_argument(
        "--numa",
        dest="numas",
        action="append",
        type=int,
        default=None,
        help="NUMA node for workload memory. Repeat for multiple nodes. Default: 0 and 1.",
    )
    parser.add_argument(
        "--noise-numa",
        dest="noise_numa",
        type=int,
        default=None,
        help="NUMA node for noise memory. Default: same as workload NUMA.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the resolved mappings and temp config plan without executing.",
    )
    return parser.parse_args()


def generate_noise_xml(noise_cpus: list[int], noise_numa: int, latency_path: Path) -> str:
    return generate_memory_benchmark_xml(
        cores=noise_cpus,
        numas=[noise_numa] * len(noise_cpus),
        rates=[0] * len(noise_cpus),
        modes=[0] * len(noise_cpus),
        latency_path=latency_path,
    )


def noise_warmup_seconds(count: int) -> float:
    env_value = os.environ.get("NOISE_WARMUP_SECONDS")
    if env_value is not None:
        return float(env_value)
    return float(count + 2)


def build_temp_config(
    base_config_path: Path,
    count: int,
    workload_numa: int,
    workload_cpus: list[int],
    noise_cpus: list[int],
) -> tuple[str, dict]:
    base_config = load_json(base_config_path)
    result_prefix = f"{base_config['result_prefix']}_single_chiplet_smt_numa{workload_numa}_x{count}"
    base_config["result_prefix"] = result_prefix
    base_config.pop("assignment_generator", None)
    base_config["assignments"] = [
        {
            "label": f"single_chiplet_smt_x{count}",
            "cores": workload_cpus,
            "numas": [workload_numa] * len(workload_cpus),
            "metadata": {
                "noise_cores": noise_cpus,
                "physical_cores": list(range(count)),
                "workload_numa": workload_numa,
            },
        }
    ]
    return result_prefix, base_config


def run_one(
    base_config_path: Path,
    count: int,
    workload_numa: int,
    noise_numa: int,
    smt_pairs: dict[int, tuple[int, int]],
    dry_run: bool,
) -> None:
    selected_cores = list(TARGET_PHYSICAL_CORES[:count])
    workload_cpus = [smt_pairs[core][0] for core in selected_cores]
    noise_cpus = [smt_pairs[core][1] for core in selected_cores]
    result_prefix, temp_config = build_temp_config(
        base_config_path=base_config_path,
        count=count,
        workload_numa=workload_numa,
        workload_cpus=workload_cpus,
        noise_cpus=noise_cpus,
    )

    print(
        f"plan config={base_config_path.name} numa={workload_numa} count={count} "
        f"workload_cpus={workload_cpus} noise_cpus={noise_cpus} noise_numa={noise_numa}"
    )
    if dry_run:
        return

    with tempfile.TemporaryDirectory(
        dir=ROOT_DIR,
        prefix=f"{base_config_path.stem}.numa{workload_numa}.x{count}.",
    ) as temp_dir_raw:
        temp_dir = Path(temp_dir_raw)
        xml_path = temp_dir / f"single_chiplet_smt_noise_x{count}.xml"
        latency_path = temp_dir / f"single_chiplet_smt_noise_x{count}.latency.txt"
        config_path = temp_dir / f"{base_config_path.stem}.numa{workload_numa}.x{count}.json"
        stdout_log = temp_dir / "memory_benchmark.stdout.log"
        stderr_log = temp_dir / "memory_benchmark.stderr.log"

        xml_path.write_text(generate_noise_xml(noise_cpus, noise_numa, latency_path), encoding="utf-8")
        write_json(config_path, temp_config)

        noise_proc: subprocess.Popen[str] | None = None
        with stdout_log.open("w", encoding="utf-8") as stdout_fh, stderr_log.open(
            "w", encoding="utf-8"
        ) as stderr_fh:
            try:
                noise_proc = subprocess.Popen(
                    memory_benchmark_cmd(MEMORY_BENCHMARK, xml_path),
                    cwd=ROOT_DIR,
                    stdout=stdout_fh,
                    stderr=stderr_fh,
                    text=True,
                )
                wait_for_process_ready(
                    noise_proc,
                    stdout_log,
                    NOISE_READY_MARKER,
                    warmup_seconds=noise_warmup_seconds(count),
                    ready_timeout_seconds=float(os.environ.get("NOISE_READY_TIMEOUT_SECONDS", "120")),
                )

                subprocess.run(
                    [os.environ.get("PYTHON_BIN", "python3"), str(HARNESS), "--config", str(config_path)],
                    cwd=ROOT_DIR,
                    check=True,
                    text=True,
                )
            finally:
                stop_process(noise_proc)

        persist_external_noise_artifacts(result_prefix, xml_path, latency_path, stdout_log, stderr_log)


def main() -> int:
    args = parse_args()
    numas = args.numas or [0, 1]
    config_paths = resolve_config_paths(args.configs, DEFAULT_BASE_CONFIGS)
    smt_pairs = discover_smt_pairs(TARGET_PHYSICAL_CORES)

    for count in args.counts:
        if count < 1 or count > len(TARGET_PHYSICAL_CORES):
            raise SystemExit(f"count must be between 1 and {len(TARGET_PHYSICAL_CORES)}: {count}")
        for workload_numa in numas:
            if workload_numa not in (0, 1):
                raise SystemExit(f"unsupported workload NUMA node: {workload_numa}")
            noise_numa = workload_numa if args.noise_numa is None else args.noise_numa
            if noise_numa not in (0, 1):
                raise SystemExit(f"unsupported noise NUMA node: {noise_numa}")
            for config_path in config_paths:
                run_one(
                    base_config_path=config_path,
                    count=count,
                    workload_numa=workload_numa,
                    noise_numa=noise_numa,
                    smt_pairs=smt_pairs,
                    dry_run=args.dry_run,
                )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
