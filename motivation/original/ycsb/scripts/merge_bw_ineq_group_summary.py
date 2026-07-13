#!/usr/bin/env python3

from __future__ import annotations

import csv
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

from active_configs import BW_INEQ_EXPERIMENT_PREFIXES, BW_INEQ_RESULT_PATTERNS
from merge_utils import build_union_fieldnames, select_latest_summary_paths


ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_OUT = ROOT_DIR / "bw_ineq_merged_group_summary.tsv"
DEFAULT_STATE = ROOT_DIR / "scripts" / "last_merge_bw_ineq.json"
PATTERNS = BW_INEQ_RESULT_PATTERNS
SUFFIX_RE = re.compile(r"_numa(?P<numa>\d+)_x(?P<count>\d+)$")
EXPERIMENT_PREFIX_ORDER = {
    experiment_prefix: index for index, experiment_prefix in enumerate(BW_INEQ_EXPERIMENT_PREFIXES)
}
BACKEND_ORDER = {
    "duckdb_tpch": 0,
    "llamacpp": 1,
    "filebench_fileserver": 2,
    "xsbench": 3,
    "gapbs_pagerank": 4,
    "gapbs_bfs": 5,
}


def parse_experiment_metadata(experiment: str) -> tuple[str, str, int, int]:
    suffix = SUFFIX_RE.search(experiment)
    if suffix is None:
        raise ValueError(f"could not parse bw-ineq metadata from {experiment}")
    base = experiment[: suffix.start()]
    workload_numa = int(suffix.group("numa"))
    noise_count = int(suffix.group("count"))
    for experiment_prefix in BW_INEQ_EXPERIMENT_PREFIXES:
        marker = f"{experiment_prefix}_"
        if base.startswith(marker):
            base_result_prefix = base[len(marker) :]
            if not base_result_prefix:
                break
            return experiment_prefix, base_result_prefix, workload_numa, noise_count
    raise ValueError(f"could not determine bw-ineq experiment prefix from {experiment}")


def row_sort_key(row: dict[str, str]) -> tuple[object, ...]:
    def as_int(name: str) -> int:
        try:
            return int(row.get(name, ""))
        except ValueError:
            return 10**9

    def ordered_index(name: str, order: dict[str, int]) -> int:
        value = row.get(name, "")
        return order.get(value, 10**9)

    return (
        ordered_index("experiment_prefix", EXPERIMENT_PREFIX_ORDER),
        row.get("experiment_prefix", ""),
        row.get("base_result_prefix", ""),
        as_int("workload_numa"),
        as_int("noise_count"),
        ordered_index("backend", BACKEND_ORDER),
        row.get("backend", ""),
        row.get("benchmark_name", ""),
        row.get("benchmark_class", ""),
        row.get("primary_operation", ""),
        row.get("assignment_label", ""),
        row.get("timestamp_utc", ""),
    )


def read_existing_rows(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        return []
    with path.open("r", encoding="utf-8", newline="") as fh:
        reader = csv.DictReader(fh, delimiter="\t")
        if reader.fieldnames is None:
            return []
        return list(reader)


def read_state(path: Path) -> dict[str, object]:
    if not path.is_file():
        return {"merged_at_utc": None, "experiments": {}}
    payload = json.loads(path.read_text(encoding="utf-8"))
    experiments = payload.get("experiments")
    if not isinstance(experiments, dict):
        experiments = {}
    return {
        "merged_at_utc": payload.get("merged_at_utc"),
        "experiments": {str(key): str(value) for key, value in experiments.items()},
    }


def write_state(path: Path, selected: dict[str, Path]) -> None:
    payload = {
        "merged_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "experiments": {experiment: str(summary_path) for experiment, summary_path in sorted(selected.items())},
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def experiment_key(experiment: str) -> tuple[str, str, str, str]:
    experiment_prefix, base_result_prefix, workload_numa, noise_count = parse_experiment_metadata(experiment)
    return experiment_prefix, base_result_prefix, str(workload_numa), str(noise_count)


def main() -> int:
    out_file = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_OUT
    state_file = DEFAULT_STATE
    selected = select_latest_summary_paths(RESULTS_DIR, PATTERNS)
    if not selected:
        raise SystemExit("no bw-ineq group_summary.tsv files found")

    state = read_state(state_file)
    previous_experiments = state["experiments"]
    assert isinstance(previous_experiments, dict)
    changed_experiments = {
        experiment
        for experiment, summary_path in selected.items()
        if previous_experiments.get(experiment) != str(summary_path)
    }

    existing_rows = read_existing_rows(out_file)
    rows: list[dict[str, str]] = []
    changed_keys = {experiment_key(experiment) for experiment in changed_experiments}
    for row in existing_rows:
        row_key = (
            row.get("experiment_prefix", ""),
            row.get("base_result_prefix", ""),
            row.get("workload_numa", ""),
            row.get("noise_count", ""),
        )
        if row_key not in changed_keys:
            rows.append(row)

    skipped = 0
    for experiment in sorted(changed_experiments):
        summary_path = selected[experiment]
        experiment_prefix, base_result_prefix, workload_numa, noise_count = parse_experiment_metadata(experiment)
        with summary_path.open("r", encoding="utf-8", newline="") as fh:
            reader = csv.DictReader(fh, delimiter="\t")
            if reader.fieldnames is None:
                skipped += 1
                print(f"skipping missing header: {summary_path}", file=sys.stderr)
                continue
            for row in reader:
                row["experiment_prefix"] = experiment_prefix
                row["base_result_prefix"] = base_result_prefix
                row["workload_numa"] = str(workload_numa)
                row["noise_count"] = str(noise_count)
                rows.append(row)

    if not rows:
        raise SystemExit("no mergeable rows found for bw-ineq results")

    rows.sort(key=row_sort_key)
    fieldnames = build_union_fieldnames(
        rows,
        leading=["experiment_prefix", "base_result_prefix", "workload_numa", "noise_count"],
    )

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

    write_state(state_file, selected)

    print(
        f"merged {len(rows)} rows into {out_file}; updated {len(changed_experiments)} experiment(s), "
        f"tracked state in {state_file} (skipped {skipped} empty/malformed folders)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
