#!/usr/bin/env python3

from __future__ import annotations

import csv
import re
import sys
from pathlib import Path


ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_OUT = ROOT_DIR / "fixed_workload_noise_merged_group_summary.tsv"
PATTERNS = (
    "chiplet_ycsb_256mib_fixed_workload_noise_x*",
    # "chiplet_duckdb_tpch_sf1_q10_fixed_workload_noise_x*",
    # "chiplet_duckdb_tpch_sf1_q21_fixed_workload_noise_x*",
    # "chiplet_npb_cg_class_b_fixed_workload_noise_x*",
    # "chiplet_npb_ep_class_b_fixed_workload_noise_x*",
    # "chiplet_npb_ft_class_b_fixed_workload_noise_x*",
    # "chiplet_npb_mg_class_b_fixed_workload_noise_x*",
)
TIMESTAMP_RE = re.compile(r"^\d{8}T\d{6}Z$")
NOISE_RE = re.compile(r"_x(\d+)$")


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


def noise_count_from_experiment(experiment: str) -> int:
    match = NOISE_RE.search(experiment)
    if match is None:
        raise ValueError(f"could not parse noise count from experiment name: {experiment}")
    return int(match.group(1))


def select_latest_group_summaries() -> dict[str, Path]:
    latest: dict[str, tuple[str, int, Path]] = {}
    for pattern in PATTERNS:
        for result_dir in sorted(RESULTS_DIR.glob(pattern)):
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


def merged_header(fieldnames: list[str]) -> list[str]:
    if "noise_count" in fieldnames:
        return fieldnames
    if "assignment_label" in fieldnames:
        index = fieldnames.index("assignment_label") + 1
        return fieldnames[:index] + ["noise_count"] + fieldnames[index:]
    return fieldnames + ["noise_count"]


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
        as_int("noise_count"),
        row.get("assignment_label", ""),
        row.get("timestamp_utc", ""),
    )


def main() -> int:
    out_file = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_OUT
    selected = select_latest_group_summaries()
    if not selected:
        raise SystemExit("no fixed_workload_noise group_summary.tsv files found")

    rows: list[dict[str, str]] = []
    header_groups: dict[tuple[str, ...], list[tuple[str, Path]]] = {}
    for experiment in sorted(selected):
        summary_path = selected[experiment]
        with summary_path.open("r", encoding="utf-8", newline="") as fh:
            reader = csv.DictReader(fh, delimiter="\t")
            if reader.fieldnames is None:
                raise SystemExit(f"missing header: {summary_path}")
            header_key = tuple(reader.fieldnames)
            header_groups.setdefault(header_key, []).append((experiment, summary_path))

    base_header_key, grouped_summaries = max(
        header_groups.items(),
        key=lambda item: (len(item[1]), item[0]),
    )
    base_fieldnames = list(base_header_key)

    skipped = 0
    for header_key, grouped_paths in header_groups.items():
        if header_key == base_header_key:
            continue
        for experiment, summary_path in grouped_paths:
            skipped += 1
            print(
                f"skipping header mismatch for experiment={experiment}: {summary_path}",
                file=sys.stderr,
            )

    for experiment, summary_path in sorted(grouped_summaries):
        with summary_path.open("r", encoding="utf-8", newline="") as fh:
            reader = csv.DictReader(fh, delimiter="\t")
            if reader.fieldnames is None:
                raise SystemExit(f"missing header: {summary_path}")
            noise_count = noise_count_from_experiment(experiment)
            for row in reader:
                row["noise_count"] = str(noise_count)
                rows.append(row)

    out_fieldnames = merged_header(base_fieldnames)
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
        f"merged {len(grouped_summaries)} latest result folders into {out_file} "
        f"(skipped {skipped} header-mismatch folders)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
