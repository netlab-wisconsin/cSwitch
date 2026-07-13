#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import socket
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="AE-local external workload runner.")
    parser.add_argument("--config", required=True, help="Generated AE external workload config.")
    parser.add_argument("--dry-run", action="store_true", help="Validate the config and dependencies without running.")
    return parser.parse_args()


def now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def ensure_directory(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    return path


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, payload: dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")


def write_tsv(path: Path, rows: list[dict[str, Any]]) -> None:
    if not rows:
        path.write_text("", encoding="utf-8")
        return
    fieldnames = list(rows[0].keys())
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)


def require_path(path: Path, description: str, *, executable: bool = False) -> None:
    if not path.exists():
        raise FileNotFoundError(f"{description} not found: {path}")
    if executable and not os.access(path, os.X_OK):
        raise PermissionError(f"{description} is not executable: {path}")


def build_env(*, updates: dict[str, str] | None = None) -> dict[str, str]:
    env = os.environ.copy()
    if updates:
        env.update(updates)
    return env


def run_logged(
    argv: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None,
    stdout_path: Path,
    stderr_path: Path,
    timeout_seconds: float | None = None,
) -> tuple[str, str]:
    ensure_directory(stdout_path.parent)
    ensure_directory(stderr_path.parent)
    with stdout_path.open("w", encoding="utf-8") as stdout_handle, stderr_path.open(
        "w", encoding="utf-8"
    ) as stderr_handle:
        completed = subprocess.run(
            argv,
            cwd=str(cwd),
            env=env,
            stdout=stdout_handle,
            stderr=stderr_handle,
            text=True,
            check=False,
            timeout=timeout_seconds,
        )
    stdout_text = stdout_path.read_text(encoding="utf-8", errors="replace")
    stderr_text = stderr_path.read_text(encoding="utf-8", errors="replace")
    if completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(argv)}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
        )
    return stdout_text, stderr_text


def float_to_metric_rate(value: float) -> float:
    if value <= 0.0:
        return 0.0
    return 1.0 / value


def config_instance_count(config: dict[str, Any]) -> int:
    raw = config.get("instance_count")
    if raw in (None, ""):
        return len([int(cpu) for cpu in config["workload_cpus"]])
    return int(raw)


def shared_workload_cpu_mask(config: dict[str, Any]) -> str | None:
    raw = config.get("shared_workload_cpu_mask")
    if raw in (None, ""):
        return None
    return str(raw)


def parse_gapbs_average_time(output: str) -> float:
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if line.startswith("Average Time:"):
            return float(line.split(":", 1)[1].strip())
    raise RuntimeError(f"could not parse GAPBS Average Time from output:\n{output}")


def find_nr_binary(package_root: Path, prefix: str) -> Path | None:
    deps_dir = package_root / "target" / "release" / "deps"
    if not deps_dir.exists():
        return None
    candidates = [
        path
        for path in deps_dir.glob(f"{prefix}-*")
        if path.is_file() and path.suffix == "" and os.access(path, os.X_OK)
    ]
    if not candidates:
        return None
    candidates.sort(key=lambda path: path.stat().st_mtime, reverse=True)
    return candidates[0]


def ensure_nr_binary(config: dict[str, Any], result_dir: Path) -> Path:
    options = dict(config["runner_options"])
    package_root = Path(str(options["package_root"]))
    manifest_path = Path(str(options["manifest_path"]))
    require_path(package_root, "node-replication package root")
    require_path(manifest_path, "node-replication Cargo.toml")
    prefix = str(options["binary_prefix"])
    existing = find_nr_binary(package_root, prefix)
    if existing is not None:
        return existing

    build_stdout = result_dir / "raw" / f"{config['benchmark']}__build.stdout.log"
    build_stderr = result_dir / "raw" / f"{config['benchmark']}__build.stderr.log"
    command = ["cargo", "bench", "--bench", str(options["cargo_bench_name"]), "--no-run"]
    command.extend(str(item) for item in options.get("cargo_extra_args", []))
    run_logged(
        command,
        cwd=package_root,
        env=build_env(updates={"RUSTUP_TOOLCHAIN": "stable", "RUSTC_BOOTSTRAP": "1"}),
        stdout_path=build_stdout,
        stderr_path=build_stderr,
    )
    built = find_nr_binary(package_root, prefix)
    if built is None:
        raise RuntimeError(f"failed to locate node-replication binary with prefix={prefix}")
    return built


def parse_node_replication_result(csv_path: Path, *, expected_threads: int, kind: str) -> tuple[str, float]:
    with csv_path.open(newline="") as handle:
        rows = list(csv.DictReader(handle))
    if not rows:
        raise RuntimeError(f"node-replication benchmark did not write rows to {csv_path}")

    if kind == "skiplist-rw50":
        predicate = lambda name: name.startswith("skiplist-mlnr") and "-wr50" in name
    elif kind == "rwlock-rw50":
        predicate = lambda name: name.startswith("std-scaleout-wr50")
    else:
        raise ValueError(f"unsupported node-replication kind: {kind}")

    grouped_rows: dict[str, list[dict[str, str]]] = {}
    for row in rows:
        name = str(row.get("name", ""))
        if predicate(name):
            grouped_rows.setdefault(name, []).append(row)
    if not grouped_rows:
        raise RuntimeError(f"could not find matching node-replication rows in {csv_path}")

    best_name: str | None = None
    best_mops: float | None = None
    for name, name_rows in grouped_rows.items():
        thread_counts = {int(row["threads"]) for row in name_rows}
        if thread_counts != {expected_threads}:
            raise RuntimeError(
                f"unexpected thread counts in {csv_path} for {name}: {sorted(thread_counts)} "
                f"(expected {expected_threads})"
            )
        intervals = {int(row["exp_time_in_sec"]) for row in name_rows}
        if not intervals:
            raise RuntimeError(f"no intervals recorded for {name} in {csv_path}")
        total_iterations = sum(int(row["iterations"]) for row in name_rows)
        throughput_mops = total_iterations / len(intervals) / 1_000_000.0
        if best_mops is None or throughput_mops > best_mops:
            best_name = name
            best_mops = throughput_mops

    if best_name is None or best_mops is None:
        raise RuntimeError(f"failed to compute node-replication throughput from {csv_path}")
    return best_name, best_mops


def prepare_result_dir(config: dict[str, Any]) -> Path:
    result_root = ensure_directory(Path(str(config["result_root"])))
    result_prefix = str(config["result_prefix"])
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    result_dir = result_root / f"{result_prefix}_{socket.gethostname()}_{stamp}_{os.getpid()}"
    ensure_directory(result_dir / "runs")
    ensure_directory(result_dir / "raw")
    return result_dir


def write_result_tables(
    *,
    config: dict[str, Any],
    result_dir: Path,
    group_rows: list[dict[str, Any]],
    instance_rows: list[dict[str, Any]],
) -> None:
    write_tsv(result_dir / "group_summary.tsv", group_rows)
    write_tsv(result_dir / "instance_summary.tsv", instance_rows)
    write_json(result_dir / "resolved_config.json", config)
    write_tsv(
        result_dir / "manifest.tsv",
        [
            {
                "benchmark": config["benchmark"],
                "backend": config["backend"],
                "style": config["style"],
                "effective_style": config["effective_style"],
                "state": config["state"],
                "result_dir": str(result_dir),
            }
        ],
    )


def write_result_tsvs(
    *,
    config: dict[str, Any],
    result_dir: Path,
    benchmark_name: str,
    benchmark_class: str,
    benchmark_time_s: float,
    benchmark_rate: float,
    benchmark_rate_unit: str,
    verification: str,
    run_stdout_log: Path,
    run_stderr_log: Path,
) -> None:
    timestamp = now_utc()
    workload_cpus = [int(cpu) for cpu in config["workload_cpus"]]
    cpu_selector = ",".join(str(cpu) for cpu in workload_cpus)
    numa_selector = ",".join("0" for _ in workload_cpus)
    common = {
        "timestamp_utc": timestamp,
        "host": socket.gethostname(),
        "arch": platform.machine(),
        "backend": str(config["backend"]),
        "mode": "analytic",
        "execution_model": "per_instance_external",
        "assignment_label": "workload",
        "instance_count": int(config["instance_count"]),
        "selected_cpu_count": len(workload_cpus),
        "cores": cpu_selector,
        "numas": numa_selector,
        "numa_binding_applied": True,
        "threads_per_instance": int(config["effective_threads"]),
        "benchmark_name": benchmark_name,
        "benchmark_class": benchmark_class,
        "benchmark_time_s": benchmark_time_s,
        "benchmark_rate": benchmark_rate,
        "benchmark_rate_unit": benchmark_rate_unit,
        "benchmark_verification": verification,
        "status": "ok",
        "run_stdout_log": str(run_stdout_log),
        "run_stderr_log": str(run_stderr_log),
        "result_dir": str(result_dir / "runs"),
    }
    write_result_tables(config=config, result_dir=result_dir, group_rows=[common], instance_rows=[{**common, "instance_index": 0}])


def gapbs_benchmark_name(kind: str) -> str:
    return f"GAPBS_{kind.upper()}"


def gapbs_argv(config: dict[str, Any]) -> tuple[list[str], str]:
    options = dict(config["runner_options"])
    style_graphs = dict(options["style_graphs"])
    style_graph = dict(style_graphs[str(config["style"])])
    benchmark_class = str(style_graph["class"])
    argv: list[str] = []
    mode = str(style_graph["mode"])
    if mode == "generated":
        if str(style_graph["generator"]) != "kronecker":
            raise RuntimeError(f"unsupported GAPBS generator mode: {style_graph['generator']}")
        argv.extend(["-g", str(style_graph["scale"])])
    elif mode == "file":
        argv.extend(["-f", str(style_graph["path"])])
    else:
        raise RuntimeError(f"unsupported GAPBS graph mode: {mode}")

    if options.get("trial_count") is not None:
        argv.extend(["-n", str(int(options["trial_count"]))])
    if options.get("iterations") is not None:
        argv.extend(["-i", str(int(options["iterations"]))])
    if options.get("delta") is not None:
        argv.extend(["-d", str(options["delta"])])
    return argv, benchmark_class


def run_gapbs(config: dict[str, Any], result_dir: Path) -> None:
    options = dict(config["runner_options"])
    root = Path(str(options["root"]))
    binary = Path(str(options["binary_path"]))
    require_path(root, "GAPBS root")
    require_path(binary, "GAPBS kernel binary", executable=True)

    workload_cpus = [int(cpu) for cpu in config["workload_cpus"]]
    single_instance = bool(config["single_instance"])
    instance_count = config_instance_count(config)
    shared_cpu_mask = shared_workload_cpu_mask(config)
    effective_threads = int(config["effective_threads"])
    argv_suffix, benchmark_class = gapbs_argv(config)
    benchmark_name = gapbs_benchmark_name(str(options["kind"]))

    if single_instance:
        run_root = ensure_directory(result_dir / "runs" / str(config["backend"]) / "workload" / "inst00")
        run_stdout_log = run_root / "run.stdout.log"
        run_stderr_log = run_root / "run.stderr.log"
        stdout_text, _ = run_logged(
            ["taskset", "-c", str(config["workload_cpu_mask"]), str(binary), *argv_suffix],
            cwd=root,
            env=build_env(updates={"OMP_NUM_THREADS": str(effective_threads)}),
            stdout_path=run_stdout_log,
            stderr_path=run_stderr_log,
            timeout_seconds=6 * 3600,
        )
        benchmark_time_s = parse_gapbs_average_time(stdout_text)
        write_result_tsvs(
            config=config,
            result_dir=result_dir,
            benchmark_name=benchmark_name,
            benchmark_class=benchmark_class,
            benchmark_time_s=benchmark_time_s,
            benchmark_rate=float_to_metric_rate(benchmark_time_s),
            benchmark_rate_unit="runs/s",
            verification="OK",
            run_stdout_log=run_stdout_log,
            run_stderr_log=run_stderr_log,
        )
        return

    timestamp = now_utc()
    instance_rows: list[dict[str, Any]] = []
    procs: list[tuple[int, Path, Path, subprocess.Popen[str], Any, Any]] = []
    try:
        for instance_index in range(instance_count):
            run_root = ensure_directory(
                result_dir / "runs" / str(config["backend"]) / "workload" / f"inst{instance_index:02d}"
            )
            run_stdout_log = run_root / "run.stdout.log"
            run_stderr_log = run_root / "run.stderr.log"
            stdout_handle = run_stdout_log.open("w", encoding="utf-8")
            stderr_handle = run_stderr_log.open("w", encoding="utf-8")
            taskset_selector = shared_cpu_mask if shared_cpu_mask is not None else str(workload_cpus[instance_index])
            proc = subprocess.Popen(
                ["taskset", "-c", taskset_selector, str(binary), *argv_suffix],
                cwd=str(root),
                env=build_env(updates={"OMP_NUM_THREADS": "1"}),
                stdout=stdout_handle,
                stderr=stderr_handle,
                text=True,
            )
            procs.append((instance_index, run_stdout_log, run_stderr_log, proc, stdout_handle, stderr_handle))

        for instance_index, run_stdout_log, run_stderr_log, proc, stdout_handle, stderr_handle in procs:
            try:
                proc.wait(timeout=6 * 3600)
            finally:
                stdout_handle.close()
                stderr_handle.close()
            stdout_text = run_stdout_log.read_text(encoding="utf-8", errors="replace")
            stderr_text = run_stderr_log.read_text(encoding="utf-8", errors="replace")
            if proc.returncode != 0:
                raise RuntimeError(
                    f"GAPBS instance {instance_index} failed ({proc.returncode}): {' '.join(proc.args)}\n"
                    f"stdout:\n{stdout_text}\nstderr:\n{stderr_text}"
                )
            benchmark_time_s = parse_gapbs_average_time(stdout_text)
            instance_rows.append(
                {
                    "timestamp_utc": timestamp,
                    "host": socket.gethostname(),
                    "arch": platform.machine(),
                    "backend": str(config["backend"]),
                    "mode": "analytic",
                    "execution_model": "per_instance_external",
                    "assignment_label": "workload",
                    "instance_index": instance_index,
                    "instance_count": instance_count,
                    "selected_cpu_count": len(workload_cpus) if shared_cpu_mask is not None else 1,
                    "cores": shared_cpu_mask if shared_cpu_mask is not None else str(workload_cpus[instance_index]),
                    "numas": ",".join("0" for _ in workload_cpus) if shared_cpu_mask is not None else "0",
                    "numa_binding_applied": True,
                    "threads_per_instance": 1,
                    "benchmark_name": benchmark_name,
                    "benchmark_class": benchmark_class,
                    "benchmark_time_s": benchmark_time_s,
                    "benchmark_rate": float_to_metric_rate(benchmark_time_s),
                    "benchmark_rate_unit": "runs/s",
                    "benchmark_verification": "OK",
                    "status": "ok",
                    "run_stdout_log": str(run_stdout_log),
                    "run_stderr_log": str(run_stderr_log),
                    "result_dir": str(result_dir / "runs"),
                }
            )
    except BaseException:
        for _, _, _, proc, stdout_handle, stderr_handle in procs:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            stdout_handle.close()
            stderr_handle.close()
        raise

    benchmark_time_s = max(float(row["benchmark_time_s"]) for row in instance_rows)
    group_rows = [
        {
            "timestamp_utc": timestamp,
            "host": socket.gethostname(),
            "arch": platform.machine(),
            "backend": str(config["backend"]),
            "mode": "analytic",
            "execution_model": "per_instance_external",
            "assignment_label": "workload",
            "instance_count": instance_count,
            "selected_cpu_count": len(workload_cpus),
            "cores": shared_cpu_mask if shared_cpu_mask is not None else ",".join(str(cpu) for cpu in workload_cpus),
            "numas": ",".join("0" for _ in workload_cpus),
            "numa_binding_applied": True,
            "threads_per_instance": 1,
            "benchmark_name": benchmark_name,
            "benchmark_class": benchmark_class,
            "benchmark_time_s": benchmark_time_s,
            "benchmark_rate": instance_count * float_to_metric_rate(benchmark_time_s),
            "benchmark_rate_unit": "runs/s",
            "benchmark_verification": "OK",
            "status": "ok",
            "run_stdout_log": "",
            "run_stderr_log": "",
            "result_dir": str(result_dir / "runs"),
        }
    ]
    write_result_tables(config=config, result_dir=result_dir, group_rows=group_rows, instance_rows=instance_rows)


def run_node_replication(config: dict[str, Any], result_dir: Path) -> None:
    options = dict(config["runner_options"])
    benchmark_kind = str(options["kind"])
    binary = ensure_nr_binary(config, result_dir)
    workload_mask = str(config["workload_cpu_mask"])
    effective_threads = int(config["effective_threads"])
    duration_seconds = int(options["duration_seconds"])
    run_root = ensure_directory(result_dir / "runs" / str(config["backend"]) / "workload" / "inst00")
    run_stdout_log = run_root / "run.stdout.log"
    run_stderr_log = run_root / "run.stderr.log"
    csv_path = result_dir / "raw" / (
        "scaleout_benchmarks_cnr.csv" if benchmark_kind == "skiplist-rw50" else "scaleout_benchmarks.csv"
    )
    if csv_path.exists():
        csv_path.unlink()

    if benchmark_kind == "skiplist-rw50":
        env = build_env(
            updates={
                "BENCH_UTILS_SKIP_DVFS": "1",
                "NR_SKIP_DVFS": "1",
                "RUST_LOG": "error",
                "BENCH_UTILS_ALLOWED_CPUS": workload_mask,
                "NR_ALLOWED_CPUS": workload_mask,
                "BENCH_UTILS_DISABLE_PINNING": "1",
                "NR_DISABLE_PINNING": "1",
                "BENCH_UTILS_CNR_CSV_PATH": str(csv_path),
                "NR_SKIPLIST_WRITE_RATIOS": "50",
                "NR_SKIPLIST_THREAD_COUNTS": str(effective_threads),
                "NR_SKIPLIST_LOG_COUNTS": str(options["log_counts"]),
                "NR_SKIPLIST_INITIAL_CAPACITY": str(options["initial_capacity"]),
                "NR_SKIPLIST_KEY_SPACE": str(options["key_space"]),
                "NR_SKIPLIST_OPS": str(options["ops"]),
                "NR_BENCH_DURATION_SECONDS": str(duration_seconds),
            }
        )
    else:
        env = build_env(
            updates={
                "BENCH_UTILS_SKIP_DVFS": "1",
                "NR_SKIP_DVFS": "1",
                "RUST_LOG": "error",
                "BENCH_UTILS_ALLOWED_CPUS": workload_mask,
                "NR_ALLOWED_CPUS": workload_mask,
                "BENCH_UTILS_DISABLE_PINNING": "1",
                "NR_DISABLE_PINNING": "1",
                "BENCH_UTILS_CSV_PATH": str(csv_path),
                "NR_HASHMAP_VARIANTS": "std",
                "NR_HASHMAP_WRITE_RATIOS": "50",
                "NR_HASHMAP_THREAD_COUNTS": str(effective_threads),
                "NR_HASHMAP_INITIAL_CAPACITY": str(options["initial_capacity"]),
                "NR_HASHMAP_KEY_SPACE": str(options["key_space"]),
                "NR_HASHMAP_OPS": str(options["ops"]),
                "NR_HASHMAP_BENCH_DURATION_SECONDS": str(duration_seconds),
                "NR_BENCH_DURATION_SECONDS": str(duration_seconds),
            }
        )

    timeout_seconds = float(options.get("timeout_seconds", max(300.0, float(duration_seconds) + 240.0)))
    run_logged(
        ["taskset", "-c", workload_mask, str(binary)],
        cwd=Path(str(options["package_root"])),
        env=env,
        stdout_path=run_stdout_log,
        stderr_path=run_stderr_log,
        timeout_seconds=timeout_seconds,
    )
    if not csv_path.exists():
        raise RuntimeError(f"node-replication benchmark did not create {csv_path}")
    benchmark_variant, mops_total = parse_node_replication_result(
        csv_path,
        expected_threads=effective_threads,
        kind=benchmark_kind,
    )
    write_result_tsvs(
        config=config,
        result_dir=result_dir,
        benchmark_name="NODE_REPLICATION",
        benchmark_class=benchmark_variant,
        benchmark_time_s=float(duration_seconds),
        benchmark_rate=mops_total,
        benchmark_rate_unit="Mop/s",
        verification="OK",
        run_stdout_log=run_stdout_log,
        run_stderr_log=run_stderr_log,
    )


def validate_config(config: dict[str, Any]) -> None:
    if str(config.get("runner_kind")) != "external":
        raise RuntimeError(f"unsupported runner_kind in external config: {config.get('runner_kind')}")
    workload_cpus = config.get("workload_cpus")
    if not isinstance(workload_cpus, list) or not workload_cpus:
        raise RuntimeError(f"{config.get('benchmark')}: workload_cpus must be a non-empty list")
    instance_count = config_instance_count(config)
    if instance_count <= 0:
        raise RuntimeError(f"{config.get('benchmark')}: instance_count must be positive")
    if shared_workload_cpu_mask(config) is None and instance_count > len(workload_cpus):
        raise RuntimeError(
            f"{config.get('benchmark')}: instance_count={instance_count} exceeds workload_cpus={len(workload_cpus)}"
        )
    options = dict(config.get("runner_options", {}))
    family = str(options.get("family", ""))
    if family == "node-replication":
        require_path(Path(str(options["package_root"])), "node-replication package root")
        require_path(Path(str(options["manifest_path"])), "node-replication Cargo.toml")
    elif family == "gapbs":
        root = Path(str(options["root"]))
        binary_path = Path(str(options["binary_path"]))
        require_path(root, "GAPBS root")
        require_path(binary_path, "GAPBS kernel binary", executable=True)
        style_graph = dict(dict(options["style_graphs"])[str(config["style"])])
        if str(style_graph["mode"]) == "file":
            require_path(Path(str(style_graph["path"])), "GAPBS graph")
    else:
        raise RuntimeError(f"{config.get('benchmark')}: unsupported external benchmark family: {family}")


def main() -> int:
    args = parse_args()
    config = read_json(Path(args.config))
    validate_config(config)
    if args.dry_run:
        print(f"[ae-external] validated {config['benchmark']} style={config['style']} state={config['state']}")
        return 0

    result_dir = prepare_result_dir(config)
    family = str(config["runner_options"]["family"])
    if family == "node-replication":
        run_node_replication(config, result_dir)
    elif family == "gapbs":
        run_gapbs(config, result_dir)
    else:
        raise RuntimeError(f"unsupported external benchmark family: {family}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
