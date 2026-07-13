#!/usr/bin/env python3

from __future__ import annotations

import csv
import re
import sys
from pathlib import Path

from active_configs import SINGLE_CHIPLET_RESULT_PATTERN
from merge_utils import build_union_fieldnames, select_latest_summary_paths

ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_OUT = ROOT_DIR / "single_chiplet_smt_numa_merged_group_summary.tsv"
PATTERN = SINGLE_CHIPLET_RESULT_PATTERN
EXPERIMENT_RE = re.compile(r"^(?P<prefix>.+)_single_chiplet_smt_numa(?P<numa>\d+)_x(?P<count>\d+)$")


def parse_experiment_metadata(experiment: str) -> tuple[str, int, int]:
    match = EXPERIMENT_RE.match(experiment)
    if match is None:
        raise ValueError(f"could not parse single-chiplet SMT metadata from {experiment}")
    return (
        match.group("prefix"),
        int(match.group("numa")),
        int(match.group("count")),
    )


def row_sort_key(row: dict[str, str]) -> tuple[object, ...]:
    def as_int(name: str) -> int:
        try:
            return int(row.get(name, ""))
        except ValueError:
            return -1

    return (
        row.get("backend", ""),
        row.get("benchmark_name", ""),
        row.get("benchmark_class", ""),
        row.get("primary_operation", ""),
        as_int("workload_numa"),
        as_int("noise_count"),
        row.get("assignment_label", ""),
        row.get("timestamp_utc", ""),
    )


def main() -> int:
    out_file = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_OUT
    selected = select_latest_summary_paths(RESULTS_DIR, [PATTERN])
    if not selected:
        raise SystemExit("no single_chiplet_smt_numa group_summary.tsv files found")

    rows: list[dict[str, str]] = []
    skipped = 0
    for experiment in sorted(selected):
        summary_path = selected[experiment]
        experiment_prefix, workload_numa, noise_count = parse_experiment_metadata(experiment)
        with summary_path.open("r", encoding="utf-8", newline="") as fh:
            reader = csv.DictReader(fh, delimiter="\t")
            if reader.fieldnames is None:
                skipped += 1
                print(f"skipping missing header: {summary_path}", file=sys.stderr)
                continue
            for row in reader:
                row["experiment_prefix"] = experiment_prefix
                row["workload_numa"] = str(workload_numa)
                row["noise_count"] = str(noise_count)
                rows.append(row)

    if not rows:
        raise SystemExit("no mergeable rows found for single_chiplet_smt_numa results")

    rows.sort(key=row_sort_key)
    fieldnames = build_union_fieldnames(rows, leading=["experiment_prefix", "workload_numa", "noise_count"])

    with out_file.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.DictWriter(
            fh,
            fieldnames=fieldnames,
            delimiter="\t",
            lineterminator="\n",
            extrasaction="ignore",
        )
        writer.writeheader()
        writer.writerows(rows)

    print(
        f"merged {len(rows)} rows from {len(selected)} latest result folders into {out_file} "
        f"(skipped {skipped} empty/malformed folders)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
