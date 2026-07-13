#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import shutil
from dataclasses import dataclass
from pathlib import Path

from active_configs import is_active_result_experiment
from merge_utils import parse_result_dir_name


ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
ARCHIVE_DIR = ROOT_DIR / "results_old"
MANIFEST_PATH = ARCHIVE_DIR / "archive_manifest.tsv"


@dataclass(frozen=True)
class ResultEntry:
    path: Path
    experiment: str
    host: str
    timestamp: str
    pid: int
    active: bool
    has_group_summary: bool


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Archive outdated and duplicate result directories into results_old/"
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would move without modifying the filesystem.",
    )
    return parser.parse_args()


def load_result_entries() -> tuple[list[ResultEntry], list[Path]]:
    active_entries: list[ResultEntry] = []
    unparseable_dirs: list[Path] = []
    for path in sorted(RESULTS_DIR.iterdir()):
        if not path.is_dir():
            continue
        parsed = parse_result_dir_name(path)
        if parsed is None:
            unparseable_dirs.append(path)
            continue
        experiment, host, timestamp, pid = parsed
        active_entries.append(
            ResultEntry(
                path=path,
                experiment=experiment,
                host=host,
                timestamp=timestamp,
                pid=pid,
                active=is_active_result_experiment(experiment),
                has_group_summary=(path / "group_summary.tsv").is_file()
                and (path / "group_summary.tsv").stat().st_size > 0,
            )
        )
    return active_entries, unparseable_dirs


def latest_entry(entries: list[ResultEntry]) -> ResultEntry:
    return max(entries, key=lambda entry: (entry.timestamp, entry.pid))


def choose_kept_entries(entries: list[ResultEntry]) -> set[Path]:
    grouped: dict[str, list[ResultEntry]] = {}
    for entry in entries:
        if not entry.active:
            continue
        grouped.setdefault(entry.experiment, []).append(entry)

    keep: set[Path] = set()
    for experiment_entries in grouped.values():
        completed = [entry for entry in experiment_entries if entry.has_group_summary]
        candidates = completed or experiment_entries
        keep.add(latest_entry(candidates).path)
    return keep


def unique_archive_path(path: Path, reason: str) -> Path:
    target_dir = ARCHIVE_DIR / reason
    target_dir.mkdir(parents=True, exist_ok=True)
    destination = target_dir / path.name
    if not destination.exists():
        return destination
    index = 1
    while True:
        candidate = target_dir / f"{path.name}.dup{index}"
        if not candidate.exists():
            return candidate
        index += 1


def archive_directories(
    *,
    dry_run: bool,
    active_entries: list[ResultEntry],
    unparseable_dirs: list[Path],
) -> tuple[int, list[dict[str, str]]]:
    keep = choose_kept_entries(active_entries)
    moves: list[tuple[Path, str, str]] = []
    manifest_rows: list[dict[str, str]] = []

    for path in unparseable_dirs:
        moves.append((path, "unparseable_name", "directory name does not match result naming scheme"))

    for entry in active_entries:
        if not entry.active:
            moves.append((entry.path, "legacy_experiment", "experiment prefix is not in the active config catalog"))
            continue
        if entry.path not in keep:
            moves.append((entry.path, "older_duplicate_run", "older or incomplete run for an active experiment"))

    for source, reason, detail in moves:
        destination = unique_archive_path(source, reason)
        manifest_rows.append(
            {
                "source_dir": str(source),
                "archived_dir": str(destination),
                "reason": reason,
                "detail": detail,
            }
        )
        print(f"{'would move' if dry_run else 'moving'} {source} -> {destination} [{reason}]")
        if not dry_run:
            shutil.move(str(source), str(destination))

    return len(keep), manifest_rows


def write_manifest(rows: list[dict[str, str]], *, dry_run: bool) -> None:
    if dry_run:
        return
    ARCHIVE_DIR.mkdir(parents=True, exist_ok=True)
    fieldnames = ["source_dir", "archived_dir", "reason", "detail"]
    with MANIFEST_PATH.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=fieldnames, delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def main() -> int:
    args = parse_args()
    active_entries, unparseable_dirs = load_result_entries()
    kept_count, manifest_rows = archive_directories(
        dry_run=args.dry_run,
        active_entries=active_entries,
        unparseable_dirs=unparseable_dirs,
    )
    write_manifest(manifest_rows, dry_run=args.dry_run)
    print(
        f"kept {kept_count} representative active result directories; "
        f"{'would archive' if args.dry_run else 'archived'} {len(manifest_rows)} directories"
    )
    if not args.dry_run:
        print(f"archive manifest: {MANIFEST_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
