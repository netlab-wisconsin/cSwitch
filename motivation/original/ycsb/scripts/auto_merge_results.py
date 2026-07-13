#!/usr/bin/env python3

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from active_configs import (
    is_bw_ineq_result,
    is_fixed_workload_noise_result,
    is_single_chiplet_smt_result,
    is_smt14_numa_result,
)


ROOT_DIR = Path(__file__).resolve().parents[1]


def merge_scripts_for_result_prefix(result_prefix: str) -> list[Path]:
    scripts: list[Path] = []
    if is_fixed_workload_noise_result(result_prefix):
        scripts.append(ROOT_DIR / "scripts" / "merge_fixed_workload_noise_group_summary.py")
    if is_single_chiplet_smt_result(result_prefix):
        scripts.append(ROOT_DIR / "scripts" / "merge_single_chiplet_smt_numa_group_summary.py")
    if is_smt14_numa_result(result_prefix):
        scripts.append(ROOT_DIR / "scripts" / "merge_smt14_numa_group_summary.py")
    if is_bw_ineq_result(result_prefix):
        scripts.append(ROOT_DIR / "scripts" / "merge_bw_ineq_group_summary.py")
    return scripts


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Auto-dispatch family merge scripts for a new harness result.")
    parser.add_argument("--result-prefix", required=True, help="Result prefix without host/timestamp/pid.")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    merge_scripts = merge_scripts_for_result_prefix(args.result_prefix)
    if not merge_scripts:
        return 0

    for merge_script in merge_scripts:
        result = subprocess.run(
            [sys.executable, str(merge_script)],
            cwd=ROOT_DIR,
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            if result.stdout:
                print(result.stdout, file=sys.stderr, end="" if result.stdout.endswith("\n") else "\n")
            if result.stderr:
                print(result.stderr, file=sys.stderr, end="" if result.stderr.endswith("\n") else "\n")
            raise SystemExit(
                f"auto-merge failed for result_prefix={args.result_prefix} via {merge_script.name}"
            )
        if result.stdout:
            print(result.stdout, end="" if result.stdout.endswith("\n") else "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
