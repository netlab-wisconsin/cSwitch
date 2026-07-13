#!/usr/bin/env python3

from __future__ import annotations

import csv
import re
import sys
from pathlib import Path


ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_OUT = ROOT_DIR / "single_chiplet_smt_numa_merged_group_summary.tsv"
PATTERN = "*_single_chiplet_smt_numa*_x*"
TIMESTAMP_RE = re.compile(r"^\d{8}T\d{6}Z$")
EXPERIMENT_RE = re.compile(r"^(?P<prefix>.+)_single_chiplet_smt_numa(?P<numa>\d+)_x(?P<count>\d+)$")


def parse_result_dir_name(path: Path) -> tuple[str, str, str, int] | None:
    parts = path.name.rsplit("_", 3)
    if len(parts) != 4:
        return None
    experiment, host, timestamp, pid_raw = parts
    if not TIMESTAMP_RE.match(timestamp):
        return None
    try:
        pid = int(pid_raw)
    except ValueError:
        return None
    return experiment, host, timestamp, pid


def parse_experiment_metadata(experiment: str) -> tuple[str, int, int]:
    match = EXPERIMENT_RE.match(experiment)
    if match is None:
        raise ValueError(f"could not parse single-chiplet SMT metadata from {experiment}")
    return (
        match.group("prefix"),
        int(match.group("numa")),
        int(match.group("count")),
    )


def select_latest_group_summaries() -> dict[str, Path]:
    latest: dict[str, tuple[str, int, Path]] = {}
    for result_dir in sorted(RESULTS_DIR.glob(PATTERN)):
        summary_path = result_dir / "group_summary.tsv"
        if not result_dir.is_dir() or not summary_path.is_file():
            continue
        parsed = parse_result_dir_name(result_dir)
        if parsed is None:
            continue
        experiment, _host, timestamp, pid = parsed
        current = latest.get(experiment)
        candidate_key = (timestamp, pid)
        if current is None or candidate_key > (current[0], current[1]):
            latest[experiment] = (timestamp, pid, summary_path)
    return {experiment: item[2] for experiment, item in latest.items()}


def build_fieldnames(rows: list[dict[str, str]]) -> list[str]:
    leading = ["experiment_prefix", "workload_numa", "noise_count"]
    seen = set(leading)
    fieldnames = list(leading)
    for row in rows:
        for key in row:
            if key not in seen:
                fieldnames.append(key)
                seen.add(key)
    return fieldnames


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
    selected = select_latest_group_summaries()
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
    fieldnames = build_fieldnames(rows)

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
