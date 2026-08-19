#!/usr/bin/env python3

from __future__ import annotations

import csv
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


HARNESS = load_module("cswitch_ae_harness", REPO_ROOT / "ae" / "harness.py")
PLOT = load_module("cswitch_ae_plot", REPO_ROOT / "ae" / "plot.py")


class MetricValidationTests(unittest.TestCase):
    def test_only_positive_finite_metrics_satisfy_attempt(self) -> None:
        self.assertTrue(HARNESS.attempt_succeeded({"status": "ok", "metric_value": 1.0}))
        self.assertTrue(
            HARNESS.attempt_succeeded({"status": "salvaged", "metric_value": "2.5"})
        )
        self.assertFalse(HARNESS.attempt_succeeded({"status": "ok", "metric_value": 0.0}))
        self.assertFalse(
            HARNESS.attempt_succeeded({"status": "ok", "metric_value": float("nan")})
        )
        self.assertFalse(
            HARNESS.attempt_succeeded({"status": "failed", "metric_value": 1.0})
        )

    def test_plot_reader_ignores_invalid_legacy_aggregates(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            results_root = Path(temp_dir)
            results_dir = results_root / "fig10" / "results"
            results_dir.mkdir(parents=True)
            path = results_dir / "aggregate_results.tsv"
            with path.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=(
                        "benchmark",
                        "case",
                        "variant",
                        "threads",
                        "noise_rate",
                        "median_metric_value",
                    ),
                    delimiter="\t",
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "benchmark": "invalid_zero",
                        "case": "clean",
                        "variant": "cSwitch",
                        "threads": 1,
                        "noise_rate": 0,
                        "median_metric_value": "0.0",
                    }
                )
                writer.writerow(
                    {
                        "benchmark": "valid",
                        "case": "clean",
                        "variant": "cSwitch",
                        "threads": 1,
                        "noise_rate": 0,
                        "median_metric_value": "42.0",
                    }
                )

            rows = PLOT.read_aggregates(results_root, "fig10")
            self.assertEqual([row.benchmark for row in rows], ["valid"])


if __name__ == "__main__":
    unittest.main()
