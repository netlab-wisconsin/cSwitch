#!/usr/bin/env python3
"""Prepare paper-style gnuplot inputs from standalone AE summaries."""

from __future__ import annotations

import argparse
import csv
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


CSWITCH_VARIANT = "paper-greedy"
LEGACY_CSWITCH_VARIANT = "la-default"
VARIANTS = ("eevdf", "arcas", "nsdi-delay-range", CSWITCH_VARIANT)
PLOT_COLUMNS = ("EEVDF", "ARCAS", "Caladan+", "cSwitch")
RESULT_VARIANT_KEYS = {
    "EEVDF": "eevdf",
    "ARCAS": "arcas",
    "Caladan+": "nsdi-delay-range",
    "cSwitch": CSWITCH_VARIANT,
    "cSwitch (legacy)": LEGACY_CSWITCH_VARIANT,
}
RESULT_VARIANT_LABELS = {
    "eevdf": "EEVDF",
    "arcas": "ARCAS",
    "nsdi-delay-range": "Caladan+",
    CSWITCH_VARIANT: "cSwitch",
    LEGACY_CSWITCH_VARIANT: "cSwitch (legacy)",
}
FIGURE_GROUPS = {
    "all": ("fig10", "fig11", "fig12a", "fig12b", "fig13"),
    "fig10": ("fig10",),
    "fig11": ("fig11",),
    "fig12": ("fig12a", "fig12b"),
    "fig13": ("fig13",),
}

FIG10_BENCHMARKS = (
    ("ycsb_rocksdb_256mib", "RDB"),
    ("ycsb_orientdb_256mib", "ODB"),
    ("ycsb_elasticsearch_256mib_lvalue", "ES"),
    ("gapbs_bc_kron20_twitter", "BC"),
    ("gapbs_pr_kron20", "PR"),
    ("llamacpp_llama31_8b", "llama"),
    ("node_replication_skiplist_rw50", "NRS"),
    ("node_replication_rwlock_rw50", "NRL"),
    ("filebench_fileserver", "FileS"),
    ("filebench_webproxy", "WebP"),
    ("filebench_webserver", "WebS"),
    ("filebench_varmail", "VAR"),
)

FIG11_BENCHMARKS = (
    ("llamacpp_llama31_8b", "llama"),
    ("gapbs_pr_kron20", "PR"),
    ("filebench_fileserver", "FileS"),
)


@dataclass(frozen=True)
class AggregateRow:
    benchmark: str
    case: str
    variant: str
    threads: int
    noise_rate: int
    value: float


@dataclass(frozen=True)
class MissingValue:
    figure: str
    panel: str
    point: str
    variant: str
    reason: str


def parse_args() -> argparse.Namespace:
    repo_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(
        description="Prepare normalized data files used by ae/plots/*.plt."
    )
    parser.add_argument(
        "--results-root",
        type=Path,
        default=repo_root / "ae" / "results",
        help="AE result root containing fig*/results (default: ae/results)",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Plot output root (default: RESULTS_ROOT/figures)",
    )
    parser.add_argument(
        "--figure",
        choices=tuple(FIGURE_GROUPS),
        default="all",
        help="Figure data to prepare (default: all)",
    )
    return parser.parse_args()


def read_tsv(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        raise FileNotFoundError(
            f"missing {path}; run ae/summarize.sh all before ae/plot.sh"
        )
    with path.open("r", encoding="utf-8", newline="") as handle:
        return list(csv.DictReader(handle, delimiter="\t"))


def result_variant_key(value: str) -> str:
    return RESULT_VARIANT_KEYS.get(value, value)


def result_variant_label(value: str) -> str:
    return RESULT_VARIANT_LABELS.get(value, value)


def read_aggregates(results_root: Path, figure: str) -> list[AggregateRow]:
    path = results_root / figure / "results" / "aggregate_results.tsv"
    rows: list[AggregateRow] = []
    for row in read_tsv(path):
        raw_value = row.get("median_metric_value", "").strip()
        if not raw_value:
            continue
        rows.append(
            AggregateRow(
                benchmark=row["benchmark"],
                case=row["case"],
                variant=result_variant_key(row["variant"]),
                threads=int(row["threads"]),
                noise_rate=int(row.get("noise_rate", "") or 0),
                value=float(raw_value),
            )
        )
    return rows


def read_directions(results_root: Path, figures: Iterable[str]) -> dict[str, bool]:
    directions: dict[str, bool] = {}
    for figure in figures:
        path = results_root / figure / "results" / "raw_results.tsv"
        for row in read_tsv(path):
            benchmark = row.get("benchmark", "")
            raw = row.get("higher_is_better", "").strip().lower()
            if benchmark and raw in {"true", "false"}:
                value = raw == "true"
                previous = directions.setdefault(benchmark, value)
                if previous != value:
                    raise ValueError(
                        f"inconsistent metric direction for {benchmark} in {path}"
                    )
    return directions


def normalized_ratio(value: float, baseline: float, higher_is_better: bool) -> float:
    if value == 0.0 or baseline == 0.0:
        raise ValueError("cannot normalize a zero metric")
    return value / baseline if higher_is_better else baseline / value


def write_tsv(
    path: Path, header: tuple[str, ...], rows: Iterable[tuple[object, ...]]
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(header)
        writer.writerows(rows)


def formatted(value: float | None) -> str:
    return "NaN" if value is None else f"{value:.6f}"


def variant_value(
    index: dict[tuple[object, ...], float], variant: str, *point: object
) -> float | None:
    value = index.get((variant, *point))
    if value is None and variant == CSWITCH_VARIANT:
        return index.get((LEGACY_CSWITCH_VARIANT, *point))
    return value


def simple_index(rows: Iterable[AggregateRow]) -> dict[tuple[str, str, str], float]:
    index: dict[tuple[str, str, str], float] = {}
    for row in rows:
        key = (row.variant, row.benchmark, row.case)
        if key in index:
            raise ValueError(f"duplicate aggregate point: {key}")
        index[key] = row.value
    return index


def prepare_baseline_histogram(
    *,
    figure: str,
    panel: str,
    benchmarks: tuple[tuple[str, str], ...],
    aggregates: list[AggregateRow],
    directions: dict[str, bool],
    output_path: Path,
    missing: list[MissingValue],
) -> None:
    index = simple_index(aggregates)
    output_rows: list[tuple[object, ...]] = []
    for benchmark, label in benchmarks:
        baseline = variant_value(index, CSWITCH_VARIANT, benchmark, panel)
        values: dict[str, float | None] = {}
        for variant in VARIANTS:
            value = variant_value(index, variant, benchmark, panel)
            if baseline is None:
                values[variant] = None
                reason = (
                    "measurement unavailable"
                    if value is None
                    else "cSwitch baseline unavailable"
                )
                missing.append(MissingValue(figure, panel, label, variant, reason))
            elif value is None:
                values[variant] = None
                missing.append(
                    MissingValue(
                        figure, panel, label, variant, "measurement unavailable"
                    )
                )
            else:
                values[variant] = normalized_ratio(
                    value, baseline, directions[benchmark]
                )
        output_rows.append(
            (
                label,
                formatted(values["eevdf"]),
                formatted(values["arcas"]),
                formatted(values["nsdi-delay-range"]),
                formatted(values[CSWITCH_VARIANT]),
            )
        )
    write_tsv(output_path, ("benchmark", *PLOT_COLUMNS), output_rows)


def prepare_figure12(
    *,
    figure: str,
    aggregates: list[AggregateRow],
    directions: dict[str, bool],
    output_path: Path,
    missing: list[MissingValue],
) -> None:
    benchmark = "gapbs_pr_kron20"
    case = "cc-io-load" if figure == "fig12a" else "io-chiplet-load"
    available_rates = {row.noise_rate for row in aggregates}
    if 0 not in available_rates:
        raise ValueError(
            f"{figure} has no no-noise baseline; run "
            f"AE_NOISE_RATES=0 ae/run.sh {figure} full and summarize again"
        )
    rates = [0, *sorted((rate for rate in available_rates if rate > 0), reverse=True)]
    index = {
        (row.variant, row.noise_rate): row.value
        for row in aggregates
        if row.benchmark == benchmark and row.case == case
    }
    baseline_rate = 0
    output_rows: list[tuple[object, ...]] = []
    for point_index, rate in enumerate(rates):
        values: dict[str, float | None] = {}
        for variant in VARIANTS:
            baseline = variant_value(index, variant, baseline_rate)
            value = variant_value(index, variant, rate)
            if baseline is None or value is None:
                values[variant] = None
                reason = (
                    f"rate {baseline_rate} baseline unavailable"
                    if baseline is None
                    else "measurement unavailable"
                )
                missing.append(MissingValue(figure, case, str(rate), variant, reason))
            else:
                values[variant] = 100.0 * normalized_ratio(
                    value, baseline, directions[benchmark]
                )
        output_rows.append(
            (
                point_index,
                rate,
                formatted(values["eevdf"]),
                formatted(values["arcas"]),
                formatted(values["nsdi-delay-range"]),
                formatted(values[CSWITCH_VARIANT]),
            )
        )
    write_tsv(output_path, ("point", "rate", *PLOT_COLUMNS), output_rows)


def prepare_figure13(
    *,
    panel: str,
    aggregates: list[AggregateRow],
    directions: dict[str, bool],
    output_path: Path,
    missing: list[MissingValue],
) -> None:
    benchmark = "gapbs_pr_kron20"
    threads = sorted({row.threads for row in aggregates if row.case == panel})
    index = {
        (row.variant, row.threads): row.value
        for row in aggregates
        if row.benchmark == benchmark and row.case == panel
    }
    output_rows: list[tuple[object, ...]] = []
    for thread_count in threads:
        values: dict[str, float | None] = {}
        for variant in VARIANTS:
            baseline = variant_value(index, variant, 1)
            value = variant_value(index, variant, thread_count)
            if baseline is None or value is None:
                values[variant] = None
                reason = (
                    "single-thread baseline unavailable"
                    if baseline is None
                    else "measurement unavailable"
                )
                missing.append(
                    MissingValue("fig13", panel, str(thread_count), variant, reason)
                )
            else:
                values[variant] = normalized_ratio(
                    value, baseline, directions[benchmark]
                )
        output_rows.append(
            (
                thread_count,
                formatted(values["eevdf"]),
                formatted(values["arcas"]),
                formatted(values["nsdi-delay-range"]),
                formatted(values[CSWITCH_VARIANT]),
            )
        )
    write_tsv(output_path, ("threads", *PLOT_COLUMNS), output_rows)


def write_missing(path: Path, missing: list[MissingValue]) -> None:
    write_tsv(
        path,
        ("figure", "panel", "point", "variant", "reason"),
        (
            (
                item.figure,
                item.panel,
                item.point,
                result_variant_label(item.variant),
                item.reason,
            )
            for item in missing
        ),
    )


def write_notes(
    path: Path,
    results_root: Path,
    missing: list[MissingValue],
    figures: Iterable[str],
) -> None:
    counts = Counter(item.figure for item in missing)
    lines = [
        "# AE Figure Plot Data",
        "",
        f"- Input root: `{results_root}`",
        "- Figure 10/11: each workload and panel is normalized to cSwitch, "
        "with cSwitch equal to 1. Legacy cSwitch result trees remain supported "
        "as a fallback.",
        "- Figure 12: each scheduler is normalized to its true no-noise "
        "(`rate=0` sentinel) result, which is 100%. Positive x-axis labels are "
        "concrete `memory_benchmark` rate-limiter values ordered from lower to "
        "higher traffic; smaller limiter values generate more traffic.",
        "- Figure 13: each scheduler is normalized to its own single-thread "
        "result, matching the paper.",
        "- Missing measurements and points without a cSwitch baseline are written "
        "as `NaN`; gnuplot leaves them blank.",
        "",
        "## Blank Values",
        "",
        "| figure | blank series points |",
        "| :----- | ------------------: |",
    ]
    for figure in figures:
        lines.append(f"| {figure} | {counts.get(figure, 0)} |")
    lines.extend(
        [
            "",
            "See `missing_values.tsv` for the exact panel, point, variant, and reason.",
        ]
    )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    results_root = args.results_root.resolve()
    output_root = (args.output_root or (results_root / "figures")).resolve()
    data_root = output_root / "data"
    data_root.mkdir(parents=True, exist_ok=True)

    figures = FIGURE_GROUPS[args.figure]
    aggregates = {figure: read_aggregates(results_root, figure) for figure in figures}
    directions = read_directions(results_root, figures)
    missing: list[MissingValue] = []

    if "fig10" in figures:
        for panel in ("clean", "loaded"):
            prepare_baseline_histogram(
                figure="fig10",
                panel=panel,
                benchmarks=FIG10_BENCHMARKS,
                aggregates=aggregates["fig10"],
                directions=directions,
                output_path=data_root / f"fig10_{panel}.tsv",
                missing=missing,
            )
    if "fig11" in figures:
        for panel in ("free-cores", "busy-cores"):
            prepare_baseline_histogram(
                figure="fig11",
                panel=panel,
                benchmarks=FIG11_BENCHMARKS,
                aggregates=aggregates["fig11"],
                directions=directions,
                output_path=data_root / f"fig11_{panel}.tsv",
                missing=missing,
            )
    for figure in (item for item in ("fig12a", "fig12b") if item in figures):
        prepare_figure12(
            figure=figure,
            aggregates=aggregates[figure],
            directions=directions,
            output_path=data_root / f"{figure}.tsv",
            missing=missing,
        )
    if "fig13" in figures:
        for panel in ("clean", "loaded"):
            prepare_figure13(
                panel=panel,
                aggregates=aggregates["fig13"],
                directions=directions,
                output_path=data_root / f"fig13_{panel}.tsv",
                missing=missing,
            )

    write_missing(output_root / "missing_values.tsv", missing)
    write_notes(output_root / "README.md", results_root, missing, figures)
    print(f"prepared plot data under {data_root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
