#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import subprocess
import tempfile
from pathlib import Path

from active_configs import FIXED_WORKLOAD_CONFIGS, resolve_config_paths
from experiment_utils import HARNESS, ROOT_DIR, load_json, write_json


DEFAULT_BASE_CONFIGS = FIXED_WORKLOAD_CONFIGS
TARGET_CPUS = (0, 1, 2, 3, 4, 5, 6, 84, 85, 86, 87, 88, 89, 90)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run fixed-workload backends on a shared SMT-aware 14-CPU range, letting Linux schedule instances within the allowed CPU set."
    )
    parser.add_argument("counts", nargs="*", type=int, default=list(range(1, len(TARGET_CPUS) + 1)))
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
        "--dry-run",
        action="store_true",
        help="Print the resolved shared CPU-set plan without running the harness.",
    )
    return parser.parse_args()


def join_cpus(values: list[int]) -> str:
    return ",".join(str(value) for value in values)


def derived_result_prefix(base_result_prefix: str, workload_numa: int, count: int) -> str:
    stem = f"{base_result_prefix.removesuffix('_fixed_workload_noise')}_smt14_schedrange"
    return f"{stem}_numa{workload_numa}_x{count}"


def build_temp_config(base_config_path: Path, count: int, workload_numa: int) -> tuple[str, dict]:
    base_config = load_json(base_config_path)
    result_prefix = derived_result_prefix(str(base_config["result_prefix"]), workload_numa, count)
    selected_cpus = list(TARGET_CPUS)
    shared_cpu_selector = join_cpus(selected_cpus)

    base_config["result_prefix"] = result_prefix
    base_config.pop("assignment_generator", None)
    base_config.pop("background_noise", None)
    monitoring = dict(base_config.get("monitoring", {}))
    monitoring["df_enabled"] = True
    monitoring["df_resource_family"] = "CCM"
    monitoring["df_resource_ids"] = [0]
    base_config["monitoring"] = monitoring
    base_config["assignments"] = [
        {
            "label": f"smt14_schedrange_x{count}",
            "cores": selected_cpus,
            "numas": [workload_numa] * len(selected_cpus),
            "metadata": {
                "workload_numa": workload_numa,
                "selected_cpus": selected_cpus,
                "instance_count": count,
                "shared_cpu_selector": shared_cpu_selector,
                "scheduler_mode": "shared_cpu_range",
            },
        }
    ]
    return result_prefix, base_config


def run_one(base_config_path: Path, count: int, workload_numa: int, dry_run: bool) -> None:
    result_prefix, temp_config = build_temp_config(base_config_path, count, workload_numa)
    assignment = temp_config["assignments"][0]
    print(
        f"plan config={base_config_path.name} numa={workload_numa} count={count} "
        f"shared_cpus={assignment['metadata']['shared_cpu_selector']} result_prefix={result_prefix} df=CCM[0]"
    )
    if dry_run:
        return

    with tempfile.TemporaryDirectory(
        dir=ROOT_DIR,
        prefix=f"{base_config_path.stem}.smt14.schedrange.numa{workload_numa}.x{count}.",
    ) as temp_dir_raw:
        temp_dir = Path(temp_dir_raw)
        config_path = temp_dir / f"{base_config_path.stem}.smt14.schedrange.numa{workload_numa}.x{count}.json"
        write_json(config_path, temp_config)
        subprocess.run(
            [os.environ.get("PYTHON_BIN", "python3"), str(HARNESS), "--config", str(config_path)],
            cwd=ROOT_DIR,
            check=True,
            text=True,
        )


def main() -> int:
    args = parse_args()
    numas = args.numas or [0, 1]
    config_paths = resolve_config_paths(args.configs, DEFAULT_BASE_CONFIGS)

    for count in args.counts:
        if count < 1 or count > len(TARGET_CPUS):
            raise SystemExit(f"count must be between 1 and {len(TARGET_CPUS)}: {count}")
        for workload_numa in numas:
            if workload_numa not in (0, 1):
                raise SystemExit(f"unsupported workload NUMA node: {workload_numa}")
            for config_path in config_paths:
                run_one(config_path, count, workload_numa, args.dry_run)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
