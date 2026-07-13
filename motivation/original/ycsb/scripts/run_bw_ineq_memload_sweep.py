#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from experiment_utils import (
    HARNESS,
    MEMORY_BENCHMARK,
    NOISE_READY_MARKER,
    ROOT_DIR,
    generate_memory_benchmark_xml,
    join_csv,
    latest_result_dir_for_prefix,
    load_json,
    memory_benchmark_cmd,
    stop_process,
    write_json,
)


FREE_ROAM_CORE = 987


@dataclass(frozen=True)
class ExperimentPreset:
    script_name: str
    description: str
    experiment_prefix: str
    default_configs: tuple[Path, ...]
    default_noise_counts: tuple[int, ...]
    workload_cpu_selector: str
    noise_cpu_selector: str
    workload_numa: int
    noise_numa: int
    noise_mode: int
    noise_rate: int
    noise_worker_memory_mb: int = 256
    df_resource_family: str = "CCM"
    df_resource_ids: tuple[int, ...] = (0,)
    df_sample_slot_ms: int = 20


@dataclass
class NoiseProcess:
    index: int
    xml_path: Path
    latency_path: Path
    stdout_log: Path
    stderr_log: Path
    proc: subprocess.Popen[str] | None = None


def parse_cpu_selector(selector: str) -> list[int]:
    values: list[int] = []
    for part in selector.split(","):
        token = part.strip()
        if not token:
            continue
        if "-" in token:
            start_s, end_s = token.split("-", 1)
            start = int(start_s)
            end = int(end_s)
            if end < start:
                raise ValueError(f"invalid cpu range: {token}")
            values.extend(range(start, end + 1))
        else:
            values.append(int(token))
    if not values:
        raise ValueError("cpu selector resolved to an empty set")
    deduped: list[int] = []
    seen: set[int] = set()
    for value in values:
        if value in seen:
            continue
        seen.add(value)
        deduped.append(value)
    return deduped


def resolve_config_paths(selected: Sequence[str] | None, default_paths: Sequence[Path]) -> list[Path]:
    if not selected:
        return [path.resolve() for path in default_paths]
    return [Path(path).resolve() for path in selected]


def build_parser(preset: ExperimentPreset) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=preset.description)
    parser.add_argument(
        "noise_counts",
        nargs="*",
        type=int,
        default=list(preset.default_noise_counts),
        help="Noise process counts to run. Default comes from the preset.",
    )
    parser.add_argument(
        "--config",
        dest="configs",
        action="append",
        default=None,
        help="Base config to run. Repeat to select a subset.",
    )
    parser.add_argument(
        "--workload-cpus",
        default=preset.workload_cpu_selector,
        help=f"CPU selector for the workload instance. Default: {preset.workload_cpu_selector}",
    )
    parser.add_argument(
        "--noise-cpus",
        default=preset.noise_cpu_selector,
        help=f"CPU selector for every noise process. Default: {preset.noise_cpu_selector}",
    )
    parser.add_argument(
        "--workload-numa",
        type=int,
        default=preset.workload_numa,
        help=f"NUMA node for workload memory binding. Default: {preset.workload_numa}",
    )
    parser.add_argument(
        "--noise-numa",
        type=int,
        default=preset.noise_numa,
        help=f"NUMA node for memory_benchmark allocation. Default: {preset.noise_numa}",
    )
    parser.add_argument(
        "--noise-rate",
        type=int,
        default=preset.noise_rate,
        help=f"memory_benchmark rate value. Default: {preset.noise_rate}",
    )
    parser.add_argument(
        "--noise-mode",
        type=int,
        default=preset.noise_mode,
        help=f"memory_benchmark mode value. Default: {preset.noise_mode}",
    )
    parser.add_argument(
        "--noise-worker-memory-mb",
        type=int,
        default=preset.noise_worker_memory_mb,
        help=f"memory_benchmark worker_memory_mb. Default: {preset.noise_worker_memory_mb}",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the resolved plan without launching memory_benchmark or the harness.",
    )
    return parser


def derived_result_prefix(experiment_prefix: str, base_result_prefix: str, workload_numa: int, noise_count: int) -> str:
    return f"{experiment_prefix}_{base_result_prefix}_numa{workload_numa}_x{noise_count}"


def build_temp_config(
    *,
    base_config_path: Path,
    experiment_prefix: str,
    workload_cpus: Sequence[int],
    workload_numa: int,
    noise_count: int,
    df_resource_family: str,
    df_resource_ids: Sequence[int],
    df_sample_slot_ms: int,
) -> tuple[str, dict]:
    base_config = load_json(base_config_path)
    base_result_prefix = str(base_config["result_prefix"])
    result_prefix = derived_result_prefix(experiment_prefix, base_result_prefix, workload_numa, noise_count)
    workload_cpu_selector = join_csv(workload_cpus)

    base_config["result_prefix"] = result_prefix
    base_config.pop("assignment_generator", None)
    base_config.pop("background_noise", None)
    base_config["assignments"] = [
        {
            "label": f"{experiment_prefix}_x{noise_count}",
            "cores": list(workload_cpus),
            "numas": [workload_numa] * len(workload_cpus),
            "metadata": {
                "instance_count": 1,
                "shared_cpu_selector": workload_cpu_selector,
                "scheduler_mode": "shared_cpu_range",
                "workload_numa": workload_numa,
                "noise_count": noise_count,
                "experiment_prefix": experiment_prefix,
            },
        }
    ]
    monitoring = dict(base_config.get("monitoring", {}))
    monitoring["df_enabled"] = True
    monitoring["df_resource_family"] = str(df_resource_family).upper()
    monitoring["df_resource_ids"] = list(df_resource_ids)
    monitoring["df_sample_slot_ms"] = int(df_sample_slot_ms)
    base_config["monitoring"] = monitoring
    return result_prefix, base_config


def noise_warmup_seconds(noise_count: int) -> float:
    env_value = os.environ.get("NOISE_WARMUP_SECONDS")
    if env_value is not None:
        return float(env_value)
    return min(5.0, 1.0 + 0.1 * noise_count)


def generate_noise_xml(
    *,
    noise_numa: int,
    noise_rate: int,
    noise_mode: int,
    noise_worker_memory_mb: int,
    latency_path: Path,
) -> str:
    return generate_memory_benchmark_xml(
        cores=[FREE_ROAM_CORE],
        numas=[noise_numa],
        rates=[noise_rate],
        modes=[noise_mode],
        latency_path=latency_path,
        worker_memory_mb=noise_worker_memory_mb,
    )


def noise_command(noise_cpu_selector: str, xml_path: Path) -> list[str]:
    taskset = shutil.which("taskset") or "taskset"
    return [taskset, "-c", noise_cpu_selector, *memory_benchmark_cmd(MEMORY_BENCHMARK, xml_path)]


def start_noise_processes(
    *,
    temp_dir: Path,
    noise_count: int,
    noise_cpu_selector: str,
    noise_numa: int,
    noise_rate: int,
    noise_mode: int,
    noise_worker_memory_mb: int,
) -> list[NoiseProcess]:
    processes: list[NoiseProcess] = []
    for index in range(noise_count):
        xml_path = temp_dir / f"noise{index:02d}.xml"
        latency_path = temp_dir / f"noise{index:02d}.latency.txt"
        stdout_log = temp_dir / f"noise{index:02d}.stdout.log"
        stderr_log = temp_dir / f"noise{index:02d}.stderr.log"
        xml_path.write_text(
            generate_noise_xml(
                noise_numa=noise_numa,
                noise_rate=noise_rate,
                noise_mode=noise_mode,
                noise_worker_memory_mb=noise_worker_memory_mb,
                latency_path=latency_path,
            ),
            encoding="utf-8",
        )
        stdout_fh = stdout_log.open("w", encoding="utf-8")
        stderr_fh = stderr_log.open("w", encoding="utf-8")
        try:
            proc = subprocess.Popen(
                noise_command(noise_cpu_selector, xml_path),
                cwd=ROOT_DIR,
                stdout=stdout_fh,
                stderr=stderr_fh,
                text=True,
            )
        finally:
            stdout_fh.close()
            stderr_fh.close()
        processes.append(
            NoiseProcess(
                index=index,
                xml_path=xml_path,
                latency_path=latency_path,
                stdout_log=stdout_log,
                stderr_log=stderr_log,
                proc=proc,
            )
        )
    return processes


def wait_for_noise_processes(processes: Sequence[NoiseProcess], noise_count: int) -> None:
    if noise_count <= 0 or not processes:
        return
    ready_timeout = float(os.environ.get("NOISE_READY_TIMEOUT_SECONDS", "180"))
    warmup = noise_warmup_seconds(noise_count)
    deadline = time.time() + ready_timeout
    pending = {noise.index: noise for noise in processes}
    while pending:
        if time.time() >= deadline:
            stuck = ",".join(f"noise{index:02d}" for index in sorted(pending))
            raise RuntimeError(f"noise processes did not reach ready state within {ready_timeout} seconds: {stuck}")
        ready_now: list[int] = []
        for index, noise in pending.items():
            if noise.proc is None:
                raise RuntimeError(f"noise process {index} was not launched")
            if noise.proc.poll() is not None:
                raise RuntimeError(
                    f"noise process {index} exited early with code {noise.proc.returncode} before reaching '{NOISE_READY_MARKER}'"
                )
            if noise.stdout_log.exists():
                contents = noise.stdout_log.read_text(encoding="utf-8", errors="replace")
                if NOISE_READY_MARKER in contents:
                    ready_now.append(index)
        for index in ready_now:
            pending.pop(index, None)
        time.sleep(0.2)
    time.sleep(warmup)


def stop_noise_processes(processes: Sequence[NoiseProcess]) -> None:
    for noise in processes:
        stop_process(noise.proc)


def persist_noise_artifacts(
    *,
    result_prefix: str,
    noise_processes: Sequence[NoiseProcess],
    metadata: dict,
) -> None:
    noise_count = int(metadata.get("noise_count", 0) or 0)
    if noise_count <= 0 and not noise_processes:
        return
    result_dir = latest_result_dir_for_prefix(result_prefix)
    if result_dir is None or not result_dir.is_dir():
        print(f"warning: could not find result dir for prefix {result_prefix}")
        return

    external_noise_dir = result_dir / "external_noise"
    external_noise_dir.mkdir(parents=True, exist_ok=True)
    (external_noise_dir / "noise_plan.json").write_text(
        json.dumps(metadata, indent=2) + "\n",
        encoding="utf-8",
    )

    for noise in noise_processes:
        noise_dir = external_noise_dir / f"noise{noise.index:02d}"
        noise_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(noise.xml_path, noise_dir / "memory_benchmark.xml")
        shutil.copy2(noise.stdout_log, noise_dir / "memory_benchmark.stdout.log")
        shutil.copy2(noise.stderr_log, noise_dir / "memory_benchmark.stderr.log")
        if noise.latency_path.exists():
            shutil.copy2(noise.latency_path, noise_dir / "memory_benchmark.latency.txt")


def run_one(
    *,
    base_config_path: Path,
    experiment_prefix: str,
    workload_cpus: Sequence[int],
    workload_numa: int,
    noise_cpus: Sequence[int],
    noise_numa: int,
    noise_rate: int,
    noise_mode: int,
    noise_worker_memory_mb: int,
    noise_count: int,
    df_resource_family: str,
    df_resource_ids: Sequence[int],
    df_sample_slot_ms: int,
    dry_run: bool,
) -> None:
    result_prefix, temp_config = build_temp_config(
        base_config_path=base_config_path,
        experiment_prefix=experiment_prefix,
        workload_cpus=workload_cpus,
        workload_numa=workload_numa,
        noise_count=noise_count,
        df_resource_family=df_resource_family,
        df_resource_ids=df_resource_ids,
        df_sample_slot_ms=df_sample_slot_ms,
    )
    workload_cpu_selector = join_csv(workload_cpus)
    noise_cpu_selector = join_csv(noise_cpus)
    print(
        f"plan config={base_config_path.name} prefix={experiment_prefix} "
        f"workload_cpus={workload_cpu_selector} workload_numa={workload_numa} "
        f"noise_cpus={noise_cpu_selector} noise_numa={noise_numa} "
        f"noise_count={noise_count}{' (disabled)' if noise_count <= 0 else ''} "
        f"mode={noise_mode} rate={noise_rate} "
        f"df={str(df_resource_family).upper()}[{join_csv(df_resource_ids)}] "
        f"result_prefix={result_prefix}"
    )
    if dry_run:
        return

    run_error: Exception | None = None
    with tempfile.TemporaryDirectory(
        dir=ROOT_DIR,
        prefix=f"{base_config_path.stem}.{experiment_prefix}.x{noise_count}.",
    ) as temp_dir_raw:
        temp_dir = Path(temp_dir_raw)
        config_path = temp_dir / f"{base_config_path.stem}.{experiment_prefix}.x{noise_count}.json"
        write_json(config_path, temp_config)

        noise_processes: list[NoiseProcess] = []
        try:
            if noise_count > 0:
                noise_processes = start_noise_processes(
                    temp_dir=temp_dir,
                    noise_count=noise_count,
                    noise_cpu_selector=noise_cpu_selector,
                    noise_numa=noise_numa,
                    noise_rate=noise_rate,
                    noise_mode=noise_mode,
                    noise_worker_memory_mb=noise_worker_memory_mb,
                )
                wait_for_noise_processes(noise_processes, noise_count)
            subprocess.run(
                [os.environ.get("PYTHON_BIN", "python3"), str(HARNESS), "--config", str(config_path)],
                cwd=ROOT_DIR,
                check=True,
                text=True,
            )
        except Exception as exc:  # noqa: BLE001
            run_error = exc
        finally:
            stop_noise_processes(noise_processes)
            persist_noise_artifacts(
                result_prefix=result_prefix,
                noise_processes=noise_processes,
                metadata={
                    "experiment_prefix": experiment_prefix,
                    "base_config": str(base_config_path),
                    "result_prefix": result_prefix,
                    "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "workload_cpus": list(workload_cpus),
                    "workload_numa": workload_numa,
                    "noise_cpus": list(noise_cpus),
                    "noise_numa": noise_numa,
                    "noise_count": noise_count,
                    "noise_mode": noise_mode,
                    "noise_rate": noise_rate,
                    "noise_worker_memory_mb": noise_worker_memory_mb,
                    "df_resource_family": str(df_resource_family).upper(),
                    "df_resource_ids": list(df_resource_ids),
                    "df_sample_slot_ms": df_sample_slot_ms,
                },
            )
        if run_error is not None:
            raise run_error


def main_with_preset(argv: Sequence[str] | None, preset: ExperimentPreset) -> int:
    parser = build_parser(preset)
    args = parser.parse_args(list(argv) if argv is not None else None)

    workload_cpus = parse_cpu_selector(args.workload_cpus)
    noise_cpus = parse_cpu_selector(args.noise_cpus)
    config_paths = resolve_config_paths(args.configs, preset.default_configs)

    for noise_count in args.noise_counts:
        if noise_count < 0:
            raise SystemExit(f"noise_count must be >= 0: {noise_count}")
        for config_path in config_paths:
            run_one(
                base_config_path=config_path,
                experiment_prefix=preset.experiment_prefix,
                workload_cpus=workload_cpus,
                workload_numa=args.workload_numa,
                noise_cpus=noise_cpus,
                noise_numa=args.noise_numa,
                noise_rate=args.noise_rate,
                noise_mode=args.noise_mode,
                noise_worker_memory_mb=args.noise_worker_memory_mb,
                noise_count=noise_count,
                df_resource_family=preset.df_resource_family,
                df_resource_ids=preset.df_resource_ids,
                df_sample_slot_ms=preset.df_sample_slot_ms,
                dry_run=args.dry_run,
            )
    return 0
