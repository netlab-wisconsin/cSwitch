#!/usr/bin/env python3

from __future__ import annotations

import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from motivation.original.ycsb.chiplet_harness import runner as CHIPLET_RUNNER


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

    def test_ycsb_metric_uses_parsed_operations_instead_of_configured_aggregate(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            run_dir = Path(temp_dir)
            result_dir = run_dir / "harness_results" / "odb"
            result_dir.mkdir(parents=True)
            with (result_dir / "group_summary.tsv").open(
                "w", encoding="utf-8", newline=""
            ) as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=(
                        "status",
                        "group_wall_clock_ms",
                        "aggregate_wall_throughput_ops_per_sec",
                    ),
                    delimiter="\t",
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "status": "ok",
                        "group_wall_clock_ms": "2000",
                        "aggregate_wall_throughput_ops_per_sec": "999999",
                    }
                )
            with (result_dir / "instance_operation_summary.tsv").open(
                "w", encoding="utf-8", newline=""
            ) as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=("instance_id", "operation", "operations"),
                    delimiter="\t",
                )
                writer.writeheader()
                writer.writerow({"instance_id": 0, "operation": "READ", "operations": 100})
                writer.writerow({"instance_id": 1, "operation": "READ", "operations": 100})
                writer.writerow({"instance_id": 1, "operation": "CLEANUP", "operations": 1})

            spec = HARNESS.RunSpec(
                "fig10", "ycsb_orientdb_256mib", "loaded", "paper-greedy", 28, 1
            )
            payload = HARNESS.parse_metric(run_dir, spec)
            self.assertEqual(payload["metric_value"], 100.0)

    def test_ycsb_metric_rejects_failed_group_with_bogus_aggregate(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            run_dir = Path(temp_dir)
            result_dir = run_dir / "harness_results" / "odb"
            result_dir.mkdir(parents=True)
            with (result_dir / "group_summary.tsv").open(
                "w", encoding="utf-8", newline=""
            ) as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=("status", "group_wall_clock_ms", "aggregate_wall_throughput_ops_per_sec"),
                    delimiter="\t",
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "status": "latency_merge_failed",
                        "group_wall_clock_ms": "1471",
                        "aggregate_wall_throughput_ops_per_sec": "135936",
                    }
                )

            spec = HARNESS.RunSpec(
                "fig10", "ycsb_orientdb_256mib", "loaded", "eevdf", 28, 1
            )
            payload = HARNESS.parse_metric(run_dir, spec)
            self.assertIsNone(payload["metric_value"])
            self.assertIn("latency_merge_failed", payload["metric_error"])


class Fig10AffinityTests(unittest.TestCase):
    @staticmethod
    def args() -> SimpleNamespace:
        return SimpleNamespace(
            fig10_loaded_workload_cpus="0-4,7-11,21-25,28-32",
            workload_cpus="0-13,21-34",
        )

    def test_chiplet_instances_share_the_full_loaded_cpu_mask(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            spec = HARNESS.RunSpec(
                "fig10", "filebench_fileserver", "loaded", "paper-greedy", 28, 1
            )
            path = HARNESS.write_chiplet_harness_config(spec, self.args(), Path(temp_dir))
            payload = json.loads(path.read_text(encoding="utf-8"))
            assignment = payload["assignments"][0]
            self.assertEqual(assignment["metadata"]["instance_count"], 20)
            self.assertEqual(
                assignment["metadata"]["shared_cpu_selector"],
                "0-4,7-11,21-25,28-32",
            )
            self.assertFalse(payload["monitoring"]["perf_enabled"])

    def test_orientdb_uses_configured_java_home(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            spec = HARNESS.RunSpec(
                "fig10", "ycsb_orientdb_256mib", "clean", "paper-greedy", 28, 1
            )
            path = HARNESS.write_chiplet_harness_config(spec, self.args(), Path(temp_dir))
            payload = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(
                payload["backends"][0]["env"]["JAVA_HOME"],
                str(HARNESS.ORIENTDB_JAVA_HOME),
            )
            self.assertEqual(payload["execution_cgroups"]["setup"], "/sys/fs/cgroup")
            self.assertEqual(
                payload["execution_cgroups"]["workload"],
                "/sys/fs/cgroup/scx-ae-ycsb_orientdb_256mib__clean__paper-greedy__t28__r01",
            )

    def test_filebench_does_not_leave_the_scheduler_cgroup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            spec = HARNESS.RunSpec(
                "fig10", "filebench_fileserver", "clean", "paper-greedy", 28, 1
            )
            path = HARNESS.write_chiplet_harness_config(spec, self.args(), Path(temp_dir))
            payload = json.loads(path.read_text(encoding="utf-8"))
            self.assertNotIn("execution_cgroups", payload)

    def test_java_cgroup_wrapper_executes_the_configured_runtime(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            wrapper_home = CHIPLET_RUNNER.prepare_cgroup_java_home(
                Path(temp_dir) / "java-home",
                Path("/opt/java8/bin/java"),
                Path("/sys/fs/cgroup/scx-ae-test"),
            )
            wrapper = (wrapper_home / "bin" / "java").read_text(encoding="utf-8")
            self.assertIn("/sys/fs/cgroup/scx-ae-test/cgroup.procs", wrapper)
            self.assertIn("os.sched_setscheduler(0, 7", wrapper)
            self.assertIn("real_java = '/opt/java8/bin/java'", wrapper)
            self.assertIn("os.execv(real_java", wrapper)

    def test_external_instances_share_the_full_loaded_cpu_mask(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            spec = HARNESS.RunSpec(
                "fig10", "gapbs_bc_kron20_twitter", "loaded", "paper-greedy", 28, 1
            )
            path = HARNESS.write_external_workload_config(spec, self.args(), Path(temp_dir))
            payload = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(payload["instance_count"], 20)
            self.assertEqual(
                payload["shared_workload_cpu_mask"],
                "0-4,7-11,21-25,28-32",
            )


class Fig13InputTests(unittest.TestCase):
    def test_figure13_uses_one_twitter_trial(self) -> None:
        graph = Path("/graphs/twitter.sg")
        args = SimpleNamespace(
            fig13_twitter_graph=graph,
            fig13_trials=1,
            gapbs_iterations=16,
        )
        spec = HARNESS.RunSpec(
            "fig13", "gapbs_pr_kron20", "clean", "paper-greedy", 18, 1
        )

        self.assertEqual(
            HARNESS.direct_gapbs_pr_args(spec, args),
            ["-f", str(graph), "-n", "1"],
        )
        self.assertEqual(HARNESS.benchmark_label(spec), "GAPBS PR Twitter")

    def test_figure11_keeps_the_kronecker_input(self) -> None:
        args = SimpleNamespace(
            fig13_twitter_graph=Path("/graphs/twitter.sg"),
            fig13_trials=1,
            gapbs_iterations=16,
        )
        spec = HARNESS.RunSpec(
            "fig11", "gapbs_pr_kron20", "free-cores", "paper-greedy", 3, 1
        )

        self.assertEqual(
            HARNESS.direct_gapbs_pr_args(spec, args),
            ["-g", "20", "-i", "100", "-n", "1"],
        )


if __name__ == "__main__":
    unittest.main()
