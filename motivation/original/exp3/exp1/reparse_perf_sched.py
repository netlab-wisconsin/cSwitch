#!/usr/bin/env python3

from __future__ import annotations

import argparse
import sys
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
ROOT_DIR = SCRIPT_DIR.parent
if str(ROOT_DIR) not in sys.path:
    sys.path.insert(0, str(ROOT_DIR))

from experiment_utils import write_csv_rows  # noqa: E402
from run_exp1 import (  # noqa: E402
    THREAD_FIELDNAMES,
    THREAD_STATS_FIELDNAMES,
    build_thread_stats_rows,
    summarize_thread_domains,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Reparse exp1 perf sched timehist logs.")
    parser.add_argument(
        "--result-dir",
        type=Path,
        required=True,
        help="exp1 result directory containing raw perf.timehist logs.",
    )
    return parser.parse_args()


def parse_run_id(path: Path) -> tuple[str, str, str, str, int]:
    suffix = "__perf.timehist.log"
    name = path.name
    if not name.endswith(suffix):
        raise ValueError(f"Unexpected perf timehist filename: {path.name}")
    run_id = name[: -len(suffix)]
    parts = run_id.split("__")
    if len(parts) == 4:
        scenario, policy, benchmark, repeat_text = parts
        placement_tag = "legacy"
    elif len(parts) == 5:
        scenario, policy, benchmark, placement_tag, repeat_text = parts
    else:
        raise ValueError(f"Unexpected perf timehist filename: {path.name}")
    return scenario, policy, benchmark, placement_tag, int(repeat_text.removeprefix("r"))


def main() -> int:
    args = parse_args()
    raw_dir = args.result_dir / "raw"
    thread_path = args.result_dir / "exp1_thread_domains.csv"
    thread_stats_path = args.result_dir / "exp1_thread_domain_summary.csv"

    timehist_paths = sorted(raw_dir.glob("*__perf.timehist.log"))
    if not timehist_paths:
        raise FileNotFoundError(f"No perf.timehist logs found under {raw_dir}")

    thread_rows: list[dict[str, object]] = []
    for timehist_path in timehist_paths:
        scenario, policy, benchmark, placement_tag, repeat = parse_run_id(timehist_path)
        rows, _ = summarize_thread_domains(
            timehist_path,
            scenario=scenario,
            policy=policy,
            benchmark=benchmark,
            placement_tag=placement_tag,
            repeat=repeat,
        )
        thread_rows.extend(rows)

    write_csv_rows(thread_path, THREAD_FIELDNAMES, thread_rows)
    write_csv_rows(thread_stats_path, THREAD_STATS_FIELDNAMES, build_thread_stats_rows(thread_rows))

    print(f"reparsed exp1 thread domains -> {thread_path}")
    print(f"reparsed exp1 thread summary -> {thread_stats_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
