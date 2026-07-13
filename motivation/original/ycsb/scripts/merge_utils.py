from __future__ import annotations

import re
from pathlib import Path
from typing import Iterable


TIMESTAMP_RE = re.compile(r"^\d{8}T\d{6}Z$")


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


def select_latest_summary_paths(results_dir: Path, patterns: Iterable[str], summary_name: str = "group_summary.tsv") -> dict[str, Path]:
    latest: dict[str, tuple[str, int, Path]] = {}
    for pattern in patterns:
        for result_dir in sorted(results_dir.glob(pattern)):
            summary_path = result_dir / summary_name
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


def build_union_fieldnames(rows: list[dict[str, str]], leading: list[str] | None = None) -> list[str]:
    leading = leading or []
    seen = set(leading)
    fieldnames = list(leading)
    for row in rows:
        for key in row:
            if key not in seen:
                fieldnames.append(key)
                seen.add(key)
    return fieldnames
