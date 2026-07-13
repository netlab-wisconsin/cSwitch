#!/usr/bin/env python3

from __future__ import annotations

import csv
import re
import sys
from pathlib import Path

from active_configs import FIXED_WORKLOAD_RESULT_PATTERNS
from merge_utils import build_union_fieldnames, select_latest_summary_paths

ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_OUT = ROOT_DIR / "fixed_workload_noise_merged_group_summary.tsv"
PATTERNS = FIXED_WORKLOAD_RESULT_PATTERNS
NOISE_RE = re.compile(r"_x(\d+)$")


def noise_count_from_experiment(experiment: str) -> int:
    match = NOISE_RE.search(experiment)
    if match is None:
        raise ValueError(f"could not parse noise count from experiment name: {experiment}")
    return int(match.group(1))


def row_sort_key(row: dict[str, str]) -> tuple[object, ...]:
    def as_int(name: str) -> int:
        try:
            return int(row.get(name, ""))
        except ValueError:
            return -1

    return (
        row.get("experiment_prefix", ""),
        row.get("backend", ""),
        row.get("benchmark_name", ""),
        row.get("benchmark_class", ""),
        row.get("primary_operation", ""),
        as_int("noise_count"),
        row.get("assignment_label", ""),
        row.get("timestamp_utc", ""),
    )


def main() -> int:
    out_file = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_OUT
    selected = select_latest_summary_paths(RESULTS_DIR, PATTERNS)
    if not selected:
        raise SystemExit("no fixed_workload_noise group_summary.tsv files found")

    rows: list[dict[str, str]] = []
    skipped = 0
    for experiment in sorted(selected):
        summary_path = selected[experiment]
        with summary_path.open("r", encoding="utf-8", newline="") as fh:
            reader = csv.DictReader(fh, delimiter="\t")
            if reader.fieldnames is None:
                skipped += 1
                print(f"skipping missing header: {summary_path}", file=sys.stderr)
                continue
            noise_count = noise_count_from_experiment(experiment)
            for row in reader:
                row["experiment_prefix"] = experiment
                row["noise_count"] = str(noise_count)
                rows.append(row)

    if not rows:
        raise SystemExit("no mergeable rows found for fixed_workload_noise results")

    out_fieldnames = build_union_fieldnames(rows, leading=["experiment_prefix", "noise_count"])
    rows.sort(key=row_sort_key)

    with out_file.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.DictWriter(
            fh,
            fieldnames=out_fieldnames,
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
