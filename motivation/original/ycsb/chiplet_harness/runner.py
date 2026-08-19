from __future__ import annotations

import argparse
import csv
import functools
import json
import os
import pwd
import re
import signal
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .backends import ExternalInvocation, InvocationSpec, YcsbInvocation, get_backend_adapter
from .config import (
    Assignment,
    BackendConfig,
    BackgroundNoiseConfig,
    HarnessConfig,
    config_to_dict,
    load_config,
)
from .df_bw import DfBandwidthMonitor, DfBandwidthSummary
from .hdr_merge import merge_histograms
from .monitoring import (
    BenchmarkRunMetrics,
    ExternalRunMetrics,
    OperationMetrics,
    PerfMetrics,
    merge_operation_metrics,
    operation_metrics_from_samples,
    parse_external_run_log,
    parse_npb_run_log,
    parse_perf_metrics,
    parse_ycsb_run_log,
    perf_scope_label,
    sum_optional_floats,
    sum_optional_ints,
    throughput_from_wall_ms,
)


SAFE_NAME_RE = re.compile(r"[^A-Za-z0-9._-]+")


@dataclass
class InstanceArtifacts:
    instance_id: int
    core: int | None
    numa: int
    core_list: tuple[int, ...]
    numa_list: tuple[int, ...]
    cpu_selector: str
    apply_numa_binding: bool
    instance_dir: Path
    load_stdout_log: Path
    load_stderr_log: Path
    run_stdout_log: Path
    run_stderr_log: Path
    perf_file: Path
    time_file: Path
    hdr_dir: Path
    db_target: str
    cleanup_path: Path


@dataclass
class ManagedProcess:
    popen: subprocess.Popen[Any]
    stdout_handle: Any
    stderr_handle: Any


@dataclass(frozen=True)
class BackgroundNoiseArtifacts:
    config_file: Path
    stdout_log: Path
    stderr_log: Path
    latency_output: Path


def log(message: str) -> None:
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"[{timestamp}] {message}", file=sys.stderr)


def now_utc_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def now_utc_compact() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def safe_name(value: str) -> str:
    sanitized = SAFE_NAME_RE.sub("_", value).strip("_")
    return sanitized or "assignment"


def join_ints(values: tuple[int, ...] | list[int]) -> str:
    return ",".join(str(value) for value in values)


def detect_arch() -> str:
    try:
        output = subprocess.run(["lscpu"], check=True, capture_output=True, text=True).stdout
    except Exception:
        return os.uname().machine.lower()
    vendor = ""
    for line in output.splitlines():
        if line.startswith("Vendor ID:"):
            vendor = line.split(":", 1)[1].strip()
            break
    if vendor == "AuthenticAMD":
        return "amd"
    if vendor == "GenuineIntel":
        return "intel"
    return vendor.lower() or os.uname().machine.lower()


def load_workload_properties(path: Path) -> dict[str, str]:
    props: dict[str, str] = {}
    for raw_line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        props[key.strip()] = value.strip()
    return props


def command_exists(name: str) -> bool:
    return shutil.which(name) is not None


def candidate_perf_paths() -> list[Path]:
    candidates: list[Path] = []

    perf_bin = os.environ.get("PERF_BIN")
    if perf_bin:
        candidates.append(Path(perf_bin))

    which_perf = shutil.which("perf")
    if which_perf:
        candidates.append(Path(which_perf))

    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user:
        try:
            sudo_home = Path(pwd.getpwnam(sudo_user).pw_dir)
            candidates.append(sudo_home / "bin" / "perf")
        except KeyError:
            pass

    candidates.append(Path.home() / "bin" / "perf")

    for pattern in ("/usr/lib/linux-tools-*/perf", "/usr/lib/linux-hwe-*-tools-*/perf"):
        candidates.extend(sorted(Path("/").glob(pattern.lstrip("/"))))

    unique_candidates: list[Path] = []
    seen: set[str] = set()
    for candidate in candidates:
        key = str(candidate)
        if key in seen:
            continue
        seen.add(key)
        unique_candidates.append(candidate)
    return unique_candidates


def is_usable_perf(candidate: Path) -> bool:
    if not candidate.exists() or not os.access(candidate, os.X_OK):
        return False
    try:
        result = subprocess.run(
            [str(candidate), "--version"],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except Exception:
        return False
    output = f"{result.stdout}\n{result.stderr}"
    return result.returncode == 0 and "perf version" in output and "perf not found" not in output


@functools.lru_cache(maxsize=1)
def resolve_perf_binary() -> str:
    checked: list[str] = []
    for candidate in candidate_perf_paths():
        checked.append(str(candidate))
        if is_usable_perf(candidate):
            return str(candidate)
    checked_list = ", ".join(checked) if checked else "<none>"
    raise RuntimeError(
        "unable to find a usable perf binary; checked "
        f"{checked_list}. Set PERF_BIN to a working perf executable if needed."
    )


def validate_config(config: HarnessConfig, enforce_runtime_requirements: bool = True) -> None:
    adapters = [get_backend_adapter(backend.name) for backend in config.backends]
    required = ["taskset", "lscpu"]
    if any(adapter.requires_ycsb_launcher for adapter in adapters):
        required.extend(["java", "javac", "mvn"])
    if any(assignment.apply_numa_binding for assignment in config.assignments):
        required.append("numactl")
    missing = [name for name in required if not command_exists(name)]
    if missing:
        raise RuntimeError(f"missing required commands: {', '.join(missing)}")
    if config.monitoring.perf_enabled:
        resolve_perf_binary()
        if not Path("/usr/bin/time").exists():
            raise RuntimeError("missing required command: /usr/bin/time")
    for backend, adapter in zip(config.backends, adapters):
        java_home = backend.env.get("JAVA_HOME")
        if adapter.requires_ycsb_launcher and java_home:
            java_binary = Path(java_home) / "bin" / "java"
            if not java_binary.is_file() or not os.access(java_binary, os.X_OK):
                raise RuntimeError(
                    f"backend {backend.name} has unusable JAVA_HOME: {java_home}"
                )
    if any(adapter.requires_ycsb_launcher for adapter in adapters) and not (
        config.ycsb_root / "bin" / "ycsb.sh"
    ).exists():
        raise RuntimeError(f"missing YCSB launcher: {config.ycsb_root / 'bin' / 'ycsb.sh'}")
    if config.background_noise is not None:
        if not config.background_noise.binary.exists():
            raise RuntimeError(
                f"background noise binary not found: {config.background_noise.binary}"
            )
        if not os.access(config.background_noise.binary, os.X_OK):
            raise RuntimeError(
                f"background noise binary is not executable: {config.background_noise.binary}"
            )
    if config.workload is not None and not config.workload.file.exists():
        raise RuntimeError(f"missing workload file: {config.workload.file}")
    if any(adapter.requires_workload for adapter in adapters) and config.workload is None:
        raise RuntimeError("config.workload is required when using YCSB backends")
    max_cpu = (os.cpu_count() or 0) - 1
    for assignment in config.assignments:
        if len(set(assignment.cores)) != len(assignment.cores):
            raise RuntimeError(f"assignment {assignment.label} contains duplicate cores")
        if min(assignment.cores) < 0 or max(assignment.cores) > max_cpu:
            raise RuntimeError(
                f"assignment {assignment.label} uses CPUs outside online range 0-{max_cpu}"
            )
    if enforce_runtime_requirements and config.monitoring.df_enabled:
        if os.geteuid() != 0:
            raise RuntimeError("DF monitoring requires root so /dev/cpu/0/msr can be programmed")
        if not Path("/dev/cpu/0/msr").exists():
            raise RuntimeError("DF monitoring requires /dev/cpu/0/msr; load the msr module first")


def print_dry_run(config: HarnessConfig) -> None:
    print(f"config: {config.config_path}")
    print(f"ycsb_root: {config.ycsb_root}")
    if config.workload is None:
        default_operationcount = 0
        print("workload: <none>")
    else:
        workload_props = load_workload_properties(config.workload.file)
        recordcount = int(workload_props.get("recordcount", "0"))
        default_operationcount = int(workload_props.get("operationcount", "0"))
        print(f"workload: {config.workload.file}")
        print(f"working_set_mb_per_instance: {config.workload.working_set_mb_per_instance}")
        print(f"recordcount: {recordcount}")
        print(f"default_operationcount: {default_operationcount}")
    print(f"df_enabled: {config.monitoring.df_enabled}")
    print(f"df_resource_family: {config.monitoring.df_resource_family}")
    print(f"df_resource_ids: {join_ints(config.monitoring.df_resource_ids)}")
    print(f"df_sample_slot_ms: {config.monitoring.df_sample_slot_ms}")
    if config.background_noise is None:
        print("background_noise: <none>")
    else:
        print(
            "background_noise: "
            f"{config.background_noise.kind} binary={config.background_noise.binary} "
            f"startup_seconds={config.background_noise.startup_seconds} "
            f"worker_memory_mb={config.background_noise.worker_memory_mb} "
            f"rate={config.background_noise.rate} mode={config.background_noise.mode}"
        )
    print("backends:")
    for backend in config.backends:
        adapter = get_backend_adapter(backend.name)
        option_suffix = ""
        if backend.options:
            option_suffix = f" options={json.dumps(backend.options, sort_keys=True)}"
        print(
            f"  - {backend.name}: operationcount={backend.operationcount or default_operationcount} "
            f"threads={backend.threads_per_instance} load_threads={backend.load_threads or 'default'} "
            f"execution_model={adapter.execution_model}{option_suffix}"
        )
    print("assignments:")
    for assignment in config.assignments:
        print(
            f"  - {assignment.label}: instances={len(assignment.cores)} cores={join_ints(assignment.cores)} "
            f"numas={join_ints(assignment.numas)} bind_numa={assignment.apply_numa_binding}"
        )


def props_to_args(props: dict[str, str]) -> list[str]:
    args: list[str] = []
    for key in sorted(props):
        args.extend(["-p", f"{key}={props[key]}"])
    return args


def merged_java_opts(active_processor_count: int, extra_java_opts: str) -> str:
    parts: list[str] = []
    existing = os.environ.get("JAVA_OPTS", "").strip()
    if existing:
        parts.append(existing)
    if active_processor_count > 0:
        parts.append(f"-XX:ActiveProcessorCount={active_processor_count}")
    if extra_java_opts.strip():
        parts.append(extra_java_opts.strip())
    return " ".join(parts).strip()


def build_perf_prefix(
    config: HarnessConfig,
    cpu_selector: str,
    membind_selector: str | None,
    time_file: Path | None,
    perf_file: Path | None,
) -> list[str]:
    command: list[str] = []
    if membind_selector is not None:
        command.extend(["numactl", "--membind", membind_selector])
    command.extend(["taskset", "-c", cpu_selector])
    if not config.monitoring.perf_enabled:
        return command
    perf_binary = resolve_perf_binary()
    if time_file is not None and perf_file is not None:
        command.extend(
            [
                "/usr/bin/time",
                "-f",
                "user=%U\nsys=%S\nelapsed=%e",
                "-o",
                str(time_file),
                perf_binary,
                "stat",
                "-x",
                ";",
                "-o",
                str(perf_file),
                "-e",
                config.monitoring.perf_event,
                "-e",
                "cycles",
                "-e",
                "instructions",
                "-e",
                "task-clock",
                "-e",
                "cache-references",
                "-e",
                "cache-misses",
                "-e",
                "LLC-loads",
                "-e",
                "LLC-load-misses",
                "--",
            ]
        )
    return command


def perf_metrics_are_required_and_missing(config: HarnessConfig, perf_metrics: PerfMetrics) -> bool:
    return config.monitoring.perf_enabled and (
        perf_metrics.stall_cycles is None or perf_metrics.cycles is None
    )


def completed_ycsb_operations(operations: dict[str, OperationMetrics]) -> int | None:
    values = [
        metrics.operations
        for operation, metrics in operations.items()
        if "FAILED" not in operation.upper()
        and operation.upper() != "CLEANUP"
        and metrics.operations is not None
        and metrics.operations > 0
    ]
    return int(sum(values)) if values else None


def failed_ycsb_operations(operations: dict[str, OperationMetrics]) -> int:
    return int(
        sum(
            metrics.operations
            for operation, metrics in operations.items()
            if "FAILED" in operation.upper()
            and metrics.operations is not None
            and metrics.operations > 0
        )
    )


def build_command(
    config: HarnessConfig,
    invocation: YcsbInvocation,
    phase: str,
    cpu_selector: str,
    membind_selector: str | None,
    time_file: Path | None,
    perf_file: Path | None,
    hdr_dir: Path | None,
    active_processor_count: int,
    backend_env: dict[str, str],
) -> tuple[list[str], dict[str, str]]:
    props = dict(invocation.extra_props)
    if phase == "run":
        if hdr_dir is None or time_file is None or perf_file is None:
            raise ValueError("run phase requires hdr_dir, time_file, and perf_file")
        hdr_dir.mkdir(parents=True, exist_ok=True)
        props["measurementtype"] = "hdrhistogram"
        props["hdrhistogram.percentiles"] = ",".join(
            str(percentile) for percentile in config.workload.hdr_percentiles
        )
        props["hdrhistogram.fileoutput"] = "true"
        props["hdrhistogram.output.path"] = f"{hdr_dir}{os.sep}"

    command = build_perf_prefix(
        config,
        cpu_selector=cpu_selector,
        membind_selector=membind_selector,
        time_file=time_file if phase == "run" else None,
        perf_file=perf_file if phase == "run" else None,
    )
    command.extend(
        [
            str(config.ycsb_root / "bin" / "ycsb.sh"),
            phase,
            invocation.binding,
            "-s",
            "-P",
            str(config.workload.file),
        ]
    )
    command.extend(props_to_args(props))
    command.extend(["-threads", str(invocation.threads)])

    env = os.environ.copy()
    env.update(backend_env)
    java_opts = merged_java_opts(active_processor_count, invocation.java_opts)
    if java_opts:
        env["JAVA_OPTS"] = java_opts
    return command, env


def build_external_command(
    config: HarnessConfig,
    invocation: ExternalInvocation,
    cpu_selector: str,
    membind_selector: str | None,
    time_file: Path,
    perf_file: Path,
) -> tuple[list[str], dict[str, str], Path | None]:
    command = build_perf_prefix(
        config,
        cpu_selector=cpu_selector,
        membind_selector=membind_selector,
        time_file=time_file,
        perf_file=perf_file,
    )
    command.extend(invocation.command)
    env = os.environ.copy()
    env.update(invocation.env)
    return command, env, invocation.working_dir


def launch_process(
    command: list[str],
    env: dict[str, str],
    stdout_path: Path,
    stderr_path: Path,
    cwd: Path | None = None,
) -> ManagedProcess:
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    stderr_path.parent.mkdir(parents=True, exist_ok=True)
    stdout_handle = stdout_path.open("w", encoding="utf-8")
    stderr_handle = stderr_path.open("w", encoding="utf-8")
    popen = subprocess.Popen(command, stdout=stdout_handle, stderr=stderr_handle, env=env, cwd=cwd)
    return ManagedProcess(popen=popen, stdout_handle=stdout_handle, stderr_handle=stderr_handle)


def wait_processes(processes: list[ManagedProcess]) -> list[int]:
    exit_codes: list[int] = []
    for process in processes:
        try:
            exit_codes.append(process.popen.wait())
        finally:
            process.stdout_handle.close()
            process.stderr_handle.close()
    return exit_codes


def stop_process(process: ManagedProcess, interrupt_timeout_s: float = 5.0) -> int:
    try:
        if process.popen.poll() is None:
            process.popen.send_signal(signal.SIGINT)
            try:
                return process.popen.wait(timeout=interrupt_timeout_s)
            except subprocess.TimeoutExpired:
                process.popen.terminate()
                try:
                    return process.popen.wait(timeout=interrupt_timeout_s)
                except subprocess.TimeoutExpired:
                    process.popen.kill()
        return process.popen.wait()
    finally:
        process.stdout_handle.close()
        process.stderr_handle.close()


def bool_to_xml(value: bool) -> str:
    return "true" if value else "false"


def repeated_csv(value: int, count: int) -> str:
    return ",".join(str(value) for _ in range(count))


def write_memory_benchmark_config(
    noise_config: BackgroundNoiseConfig,
    assignment: Assignment,
    output_file: Path,
    latency_output: Path,
) -> None:
    thread_count = len(assignment.cores)
    output_file.parent.mkdir(parents=True, exist_ok=True)
    output_text = f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{thread_count}</nthreads>
  <memory>{noise_config.memory}</memory>
  <worker_memory_mb>{noise_config.worker_memory_mb}</worker_memory_mb>
  <output>{latency_output}</output>
  <bandwidth>{bool_to_xml(noise_config.bandwidth)}</bandwidth>
  <time>{noise_config.time_seconds}</time>
  <clock>{noise_config.clock_ghz}</clock>
  <silent>{bool_to_xml(noise_config.silent)}</silent>
  <core>{join_ints(assignment.cores)}</core>
  <numa>{join_ints(assignment.numas)}</numa>
  <rate>{repeated_csv(noise_config.rate, thread_count)}</rate>
  <Mode>{repeated_csv(noise_config.mode, thread_count)}</Mode>
  <thread_alloc>{bool_to_xml(noise_config.thread_alloc)}</thread_alloc>
  <alloc_core>{noise_config.alloc_core}</alloc_core>
  <alloc_numa>{noise_config.alloc_numa}</alloc_numa>
  <alloc_rate>{noise_config.alloc_rate}</alloc_rate>
  <alloc_Mode>{noise_config.alloc_mode}</alloc_Mode>
</benchmark>
"""
    output_file.write_text(output_text, encoding="utf-8")


def start_background_noise(
    config: HarnessConfig,
    assignment: Assignment,
    group_dir: Path,
    noise_dir_name: str = "noise",
) -> tuple[ManagedProcess | None, BackgroundNoiseArtifacts | None]:
    noise_config = config.background_noise
    if noise_config is None:
        return None, None

    noise_dir = group_dir / noise_dir_name
    artifacts = BackgroundNoiseArtifacts(
        config_file=noise_dir / "memory_benchmark.xml",
        stdout_log=noise_dir / "memory_benchmark.stdout.log",
        stderr_log=noise_dir / "memory_benchmark.stderr.log",
        latency_output=noise_dir / "memory_benchmark.latency.txt",
    )
    write_memory_benchmark_config(
        noise_config,
        assignment,
        artifacts.config_file,
        artifacts.latency_output,
    )
    process = launch_process(
        [str(noise_config.binary), "--config", str(artifacts.config_file)],
        os.environ.copy(),
        artifacts.stdout_log,
        artifacts.stderr_log,
    )
    if noise_config.startup_seconds > 0:
        time.sleep(noise_config.startup_seconds)
    exit_code = process.popen.poll()
    if exit_code is not None:
        stop_process(process)
        raise RuntimeError(
            "background noise exited early "
            f"(exit_code={exit_code}); see {artifacts.stderr_log}"
        )
    return process, artifacts


def build_instance_artifacts(
    assignment: Assignment,
    instance_id: int,
    instance_dir: Path,
    db_target: str,
    cleanup_path: Path,
    core_list: tuple[int, ...] | None = None,
    numa_list: tuple[int, ...] | None = None,
    cpu_selector: str | None = None,
) -> InstanceArtifacts:
    resolved_core_list = assignment.cores[instance_id : instance_id + 1] if core_list is None else core_list
    resolved_numa_list = assignment.numas[instance_id : instance_id + 1] if numa_list is None else numa_list
    resolved_cpu_selector = str(resolved_core_list[0]) if cpu_selector is None else cpu_selector
    display_core: int | None = resolved_core_list[0] if cpu_selector is None else None
    return InstanceArtifacts(
        instance_id=instance_id,
        core=display_core,
        numa=resolved_numa_list[0],
        core_list=tuple(resolved_core_list),
        numa_list=tuple(resolved_numa_list),
        cpu_selector=resolved_cpu_selector,
        apply_numa_binding=assignment.apply_numa_binding,
        instance_dir=instance_dir,
        load_stdout_log=instance_dir / "load.stdout.log",
        load_stderr_log=instance_dir / "load.stderr.log",
        run_stdout_log=instance_dir / "run.stdout.log",
        run_stderr_log=instance_dir / "run.stderr.log",
        perf_file=instance_dir / "perf.stat",
        time_file=instance_dir / "time.txt",
        hdr_dir=instance_dir / "hdr",
        db_target=db_target,
        cleanup_path=cleanup_path,
    )


def optional_int(value: Any) -> int | None:
    if value in (None, ""):
        return None
    return int(value)


def optional_float(value: Any) -> float | None:
    if value in (None, ""):
        return None
    return float(value)


def assignment_instance_count(assignment: Assignment) -> int:
    raw = assignment.metadata.get("instance_count")
    if raw in (None, ""):
        return len(assignment.cores)
    return int(raw)


def assignment_shared_cpu_selector(assignment: Assignment) -> str | None:
    raw = assignment.metadata.get("shared_cpu_selector")
    if raw in (None, ""):
        return None
    return str(raw)


def create_writers(
    run_dir: Path,
    df_resource_family: str,
    df_resource_ids: tuple[int, ...],
) -> tuple[dict[str, csv.DictWriter], dict[str, Any]]:
    handles: dict[str, Any] = {}
    writers: dict[str, csv.DictWriter] = {}
    df_prefix = df_resource_family.lower()

    group_fields = [
        "timestamp_utc",
        "host",
        "arch",
        "backend",
        "mode",
        "execution_model",
        "event_name",
        "perf_scope",
        "assignment_label",
        "instance_count",
        "selected_cpu_count",
        "cores",
        "numas",
        "numa_binding_applied",
        "working_set_mb_per_instance",
        "total_working_set_mb",
        "recordcount_per_instance",
        "total_recordcount",
        "operationcount_per_instance",
        "total_operationcount",
        "threads_per_instance",
        "java_active_processor_count_per_instance",
        "load_threads",
        "fieldcount",
        "fieldlength",
        "approx_value_bytes_per_record",
        "primary_operation",
        "benchmark_name",
        "benchmark_class",
        "benchmark_time_s",
        "benchmark_rate",
        "benchmark_rate_unit",
        "benchmark_verification",
        "latency_operations",
        "latency_average_us",
        "latency_min_us",
        "latency_max_us",
        "latency_p50_us",
        "latency_p95_us",
        "latency_p99_us",
        "latency_p99_9_us",
        "latency_p99_99_us",
        "summed_stall_cycles",
        "summed_cycles",
        "summed_instructions",
        "summed_task_clock_ms",
        "summed_instance_throughput_ops_per_sec",
        "group_wall_clock_ms",
        "aggregate_wall_throughput_ops_per_sec",
        "ServerPerfUserTimeMs",
        "ServerPerfKernelTimeMs",
        "ServerPerfCacheRefs",
        "ServerPerfCacheMisses",
        "ServerPerfLLCHitRatio",
        "df_duration_s",
        "df_total_read_mib_s",
        "df_total_write_mib_s",
        "df_total_mib_s",
    ]
    for resource_id in df_resource_ids:
        group_fields.extend(
            [
                f"df_{df_prefix}{resource_id}_read_mib_s",
                f"df_{df_prefix}{resource_id}_write_mib_s",
                f"df_{df_prefix}{resource_id}_total_mib_s",
            ]
        )
    group_fields.extend(["status", "perf_event", "workload_file", "result_dir"])

    instance_fields = [
        "timestamp_utc",
        "host",
        "arch",
        "backend",
        "mode",
        "execution_model",
        "event_name",
        "perf_scope",
        "assignment_label",
        "instance_count",
        "instance_id",
        "cpu_core",
        "cpu_core_list",
        "numa_node",
        "numa_list",
        "selected_cpu_count",
        "numa_binding_applied",
        "working_set_mb_per_instance",
        "recordcount",
        "operationcount",
        "threads_per_instance",
        "java_active_processor_count_per_instance",
        "load_threads",
        "fieldcount",
        "fieldlength",
        "approx_value_bytes_per_record",
        "primary_operation",
        "benchmark_name",
        "benchmark_class",
        "benchmark_time_s",
        "benchmark_rate",
        "benchmark_rate_unit",
        "benchmark_verification",
        "latency_operations",
        "latency_average_us",
        "latency_min_us",
        "latency_max_us",
        "latency_p50_us",
        "latency_p95_us",
        "latency_p99_us",
        "latency_p99_9_us",
        "latency_p99_99_us",
        "throughput_ops_per_sec",
        "stall_cycles",
        "cycles",
        "instructions",
        "task_clock_ms",
        "ServerPerfUserTimeMs",
        "ServerPerfKernelTimeMs",
        "ServerPerfCacheRefs",
        "ServerPerfCacheMisses",
        "ServerPerfLLCHitRatio",
        "status",
        "perf_event",
        "workload_file",
        "db_target",
        "load_stdout_log",
        "load_stderr_log",
        "run_stdout_log",
        "run_stderr_log",
        "perf_stat_file",
        "time_file",
        "hdr_dir",
        "result_dir",
    ]

    op_fields = [
        "timestamp_utc",
        "host",
        "arch",
        "backend",
        "mode",
        "execution_model",
        "event_name",
        "perf_scope",
        "assignment_label",
        "instance_count",
        "operation",
        "hdr_file_count",
        "operations",
        "average_us",
        "min_us",
        "max_us",
        "p50_us",
        "p95_us",
        "p99_us",
        "p99_9_us",
        "p99_99_us",
        "status",
        "result_dir",
    ]

    instance_op_fields = [
        "timestamp_utc",
        "host",
        "arch",
        "backend",
        "mode",
        "execution_model",
        "event_name",
        "perf_scope",
        "assignment_label",
        "instance_count",
        "instance_id",
        "cpu_core",
        "cpu_core_list",
        "operation",
        "operations",
        "average_us",
        "min_us",
        "max_us",
        "p50_us",
        "p95_us",
        "p99_us",
        "p99_9_us",
        "p99_99_us",
        "status",
        "run_stdout_log",
        "result_dir",
    ]

    manifest_fields = [
        "backend",
        "execution_model",
        "assignment_label",
        "instance_count",
        "selected_cpu_count",
        "cores",
        "numas",
        "numa_binding_applied",
        "result_dir",
        "status",
    ]

    definitions = {
        "group": (run_dir / "group_summary.tsv", group_fields),
        "instance": (run_dir / "instance_summary.tsv", instance_fields),
        "operation": (run_dir / "operation_summary.tsv", op_fields),
        "instance_operation": (run_dir / "instance_operation_summary.tsv", instance_op_fields),
        "manifest": (run_dir / "manifest.tsv", manifest_fields),
    }

    for key, (path, fieldnames) in definitions.items():
        handle = path.open("w", encoding="utf-8", newline="")
        writer = csv.DictWriter(handle, fieldnames=fieldnames, delimiter="\t", extrasaction="ignore")
        writer.writeheader()
        handles[key] = handle
        writers[key] = writer
    return writers, handles


def close_handles(handles: dict[str, Any]) -> None:
    for handle in handles.values():
        handle.close()


def cleanup_paths(paths: list[Path], keep_result_db: bool) -> None:
    if keep_result_db:
        return
    for path in paths:
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)


def operation_row(metrics: dict[str, float] | OperationMetrics | None) -> dict[str, Any]:
    if metrics is None:
        return {}
    if isinstance(metrics, OperationMetrics):
        return {
            "operations": metrics.operations,
            "average_us": metrics.average_us,
            "min_us": metrics.min_us,
            "max_us": metrics.max_us,
            "p50_us": metrics.p50_us,
            "p95_us": metrics.p95_us,
            "p99_us": metrics.p99_us,
            "p99_9_us": metrics.p99_9_us,
            "p99_99_us": metrics.p99_99_us,
        }
    return {
        "operations": optional_int(metrics.get("operations")),
        "average_us": optional_float(metrics.get("average_us")),
        "min_us": optional_float(metrics.get("min_us")),
        "max_us": optional_float(metrics.get("max_us")),
        "p50_us": optional_float(metrics.get("p50_us")),
        "p95_us": optional_float(metrics.get("p95_us")),
        "p99_us": optional_float(metrics.get("p99_us")),
        "p99_9_us": optional_float(metrics.get("p99_9_us")),
        "p99_99_us": optional_float(metrics.get("p99_99_us")),
    }


def latency_row(metrics: dict[str, float] | OperationMetrics | None) -> dict[str, Any]:
    row = operation_row(metrics)
    return {
        "latency_operations": row.get("operations"),
        "latency_average_us": row.get("average_us"),
        "latency_min_us": row.get("min_us"),
        "latency_max_us": row.get("max_us"),
        "latency_p50_us": row.get("p50_us"),
        "latency_p95_us": row.get("p95_us"),
        "latency_p99_us": row.get("p99_us"),
        "latency_p99_9_us": row.get("p99_9_us"),
        "latency_p99_99_us": row.get("p99_99_us"),
    }


def summarize_external_latency(
    external_metrics: ExternalRunMetrics,
) -> tuple[str | None, OperationMetrics | None]:
    if external_metrics.primary_operation is not None:
        return external_metrics.primary_operation, external_metrics.operation_metrics.get(
            external_metrics.primary_operation
        )
    if not external_metrics.operation_metrics:
        return None, None

    sample_values: list[float] = []
    for metrics in external_metrics.operation_metrics.values():
        sample_values.extend(metrics.samples_us)
    if not sample_values:
        return "QUERY_MIX", None
    return "QUERY_MIX", operation_metrics_from_samples(sample_values)


def cpu_list_row(cores: tuple[int, ...], numas: tuple[int, ...]) -> dict[str, Any]:
    return {
        "cpu_core": cores[0],
        "cpu_core_list": join_ints(cores),
        "numa_node": numas[0],
        "numa_list": join_ints(numas),
        "selected_cpu_count": len(cores),
    }


def benchmark_row(metrics: BenchmarkRunMetrics | None) -> dict[str, Any]:
    if metrics is None:
        return {
            "benchmark_name": None,
            "benchmark_class": None,
            "benchmark_time_s": None,
            "benchmark_rate": None,
            "benchmark_rate_unit": None,
            "benchmark_verification": None,
        }
    return {
        "benchmark_name": metrics.benchmark_name,
        "benchmark_class": metrics.benchmark_class,
        "benchmark_time_s": metrics.time_s,
        "benchmark_rate": metrics.rate_value,
        "benchmark_rate_unit": metrics.rate_unit,
        "benchmark_verification": metrics.verification,
    }


def df_row(
    summary: DfBandwidthSummary | None,
    df_resource_family: str,
    df_resource_ids: tuple[int, ...],
) -> dict[str, Any]:
    row: dict[str, Any] = {}
    df_prefix = df_resource_family.lower()
    if summary is None:
        for resource_id in df_resource_ids:
            row[f"df_{df_prefix}{resource_id}_read_mib_s"] = None
            row[f"df_{df_prefix}{resource_id}_write_mib_s"] = None
            row[f"df_{df_prefix}{resource_id}_total_mib_s"] = None
        return row
    row["df_duration_s"] = summary.duration_s
    row["df_total_read_mib_s"] = summary.total_read_mib_s
    row["df_total_write_mib_s"] = summary.total_write_mib_s
    row["df_total_mib_s"] = summary.total_mib_s
    for resource_id in df_resource_ids:
        metric = summary.per_resource.get(resource_id)
        row[f"df_{df_prefix}{resource_id}_read_mib_s"] = None if metric is None else metric.read_mib_s
        row[f"df_{df_prefix}{resource_id}_write_mib_s"] = None if metric is None else metric.write_mib_s
        row[f"df_{df_prefix}{resource_id}_total_mib_s"] = None if metric is None else metric.total_mib_s
    return row


def assignment_membind_selector(assignment: Assignment) -> str | None:
    if not assignment.apply_numa_binding:
        return None
    return join_ints(sorted(set(assignment.numas)))


def run_openmp_group(
    config: HarnessConfig,
    backend_config: BackendConfig,
    assignment: Assignment,
    writers: dict[str, csv.DictWriter],
    group_dir: Path,
    host: str,
    arch: str,
    perf_scope: str,
) -> None:
    adapter = get_backend_adapter(backend_config.name)
    instance_dir = group_dir / "inst00"
    instance_dir.mkdir(parents=True, exist_ok=True)

    invocation = adapter.assignment_invocation(
        config.config_path,
        instance_dir,
        assignment.cores,
        assignment.label,
        backend_config,
    )
    artifacts = build_instance_artifacts(
        assignment,
        0,
        instance_dir,
        db_target=invocation.db_target,
        cleanup_path=invocation.cleanup_path,
        core_list=assignment.cores,
        numa_list=assignment.numas,
    )
    artifacts.load_stdout_log.write_text("load phase is not applicable for this backend\n", encoding="utf-8")
    artifacts.load_stderr_log.write_text("", encoding="utf-8")

    instance_count = 1
    threads_per_instance = backend_config.threads_per_instance
    benchmark_default = BenchmarkRunMetrics(
        benchmark_name=invocation.benchmark_name,
        benchmark_class=invocation.benchmark_class,
    )

    command, env, cwd = build_external_command(
        config,
        invocation,
        cpu_selector=join_ints(assignment.cores),
        membind_selector=assignment_membind_selector(assignment),
        time_file=artifacts.time_file,
        perf_file=artifacts.perf_file,
    )

    run_processes: list[ManagedProcess] = []
    noise_process: ManagedProcess | None = None
    df_monitor: DfBandwidthMonitor | None = None
    df_summary: DfBandwidthSummary | None = None
    df_error: Exception | None = None
    group_start = time.monotonic()
    try:
        noise_process, _ = start_background_noise(config, assignment, group_dir)
        if config.monitoring.df_enabled:
            df_monitor = DfBandwidthMonitor(
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
                config.monitoring.df_sample_slot_ms,
            )
            df_monitor.start()
        run_processes.append(
            launch_process(command, env, artifacts.run_stdout_log, artifacts.run_stderr_log, cwd=cwd)
        )
        run_exit_codes = wait_processes(run_processes)
    finally:
        if df_monitor is not None:
            try:
                df_summary = df_monitor.stop()
            except Exception as exc:  # pragma: no cover - hardware path
                df_error = exc
        if noise_process is not None:
            stop_process(noise_process)
    group_wall_ms = (time.monotonic() - group_start) * 1000.0

    perf_metrics = parse_perf_metrics(artifacts.perf_file, artifacts.time_file, config.monitoring.perf_event)
    benchmark_metrics = parse_npb_run_log(artifacts.run_stdout_log, artifacts.run_stderr_log)
    if benchmark_metrics.benchmark_name is None or benchmark_metrics.benchmark_class is None:
        benchmark_metrics = BenchmarkRunMetrics(
            benchmark_name=benchmark_metrics.benchmark_name or benchmark_default.benchmark_name,
            benchmark_class=benchmark_metrics.benchmark_class or benchmark_default.benchmark_class,
            time_s=benchmark_metrics.time_s,
            rate_value=benchmark_metrics.rate_value,
            rate_unit=benchmark_metrics.rate_unit,
            verification=benchmark_metrics.verification,
        )

    group_status = "ok"
    if run_exit_codes[0] != 0:
        group_status = "run_failed"
    elif benchmark_metrics.verification_ok is False:
        group_status = "verification_failed"
    elif (
        benchmark_metrics.rate_value is None
        or benchmark_metrics.rate_value <= 0.0
        or perf_metrics_are_required_and_missing(config, perf_metrics)
    ):
        group_status = "parse_failed"
    if group_status == "ok" and df_error is not None:
        group_status = "df_failed"

    writers["instance"].writerow(
        {
            "timestamp_utc": now_utc_iso(),
            "host": host,
            "arch": arch,
            "backend": backend_config.name,
            "mode": adapter.mode,
            "execution_model": adapter.execution_model,
            "event_name": config.monitoring.event_name,
            "perf_scope": perf_scope,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "instance_id": artifacts.instance_id,
            **cpu_list_row(artifacts.core_list, artifacts.numa_list),
            "numa_binding_applied": artifacts.apply_numa_binding,
            "working_set_mb_per_instance": None,
            "recordcount": None,
            "operationcount": None,
            "threads_per_instance": threads_per_instance,
            "java_active_processor_count_per_instance": None,
            "load_threads": None,
            "fieldcount": None,
            "fieldlength": None,
            "approx_value_bytes_per_record": None,
            "primary_operation": benchmark_metrics.benchmark_name,
            **benchmark_row(benchmark_metrics),
            **latency_row(None),
            "throughput_ops_per_sec": benchmark_metrics.rate_value,
            "stall_cycles": perf_metrics.stall_cycles,
            "cycles": perf_metrics.cycles,
            "instructions": perf_metrics.instructions,
            "task_clock_ms": perf_metrics.task_clock_ms,
            "ServerPerfUserTimeMs": perf_metrics.user_time_ms,
            "ServerPerfKernelTimeMs": perf_metrics.kernel_time_ms,
            "ServerPerfCacheRefs": perf_metrics.cache_refs,
            "ServerPerfCacheMisses": perf_metrics.cache_misses,
            "ServerPerfLLCHitRatio": perf_metrics.llc_hit_ratio,
            "status": group_status,
            "perf_event": config.monitoring.perf_event,
            "workload_file": "",
            "db_target": artifacts.db_target,
            "load_stdout_log": str(artifacts.load_stdout_log),
            "load_stderr_log": str(artifacts.load_stderr_log),
            "run_stdout_log": str(artifacts.run_stdout_log),
            "run_stderr_log": str(artifacts.run_stderr_log),
            "perf_stat_file": str(artifacts.perf_file),
            "time_file": str(artifacts.time_file),
            "hdr_dir": "",
            "result_dir": str(group_dir),
        }
    )
    writers["group"].writerow(
        {
            "timestamp_utc": now_utc_iso(),
            "host": host,
            "arch": arch,
            "backend": backend_config.name,
            "mode": adapter.mode,
            "execution_model": adapter.execution_model,
            "event_name": config.monitoring.event_name,
            "perf_scope": perf_scope,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "working_set_mb_per_instance": None,
            "total_working_set_mb": None,
            "recordcount_per_instance": None,
            "total_recordcount": None,
            "operationcount_per_instance": None,
            "total_operationcount": None,
            "threads_per_instance": threads_per_instance,
            "java_active_processor_count_per_instance": None,
            "load_threads": None,
            "fieldcount": None,
            "fieldlength": None,
            "approx_value_bytes_per_record": None,
            "primary_operation": benchmark_metrics.benchmark_name,
            **benchmark_row(benchmark_metrics),
            **latency_row(None),
            "summed_stall_cycles": perf_metrics.stall_cycles,
            "summed_cycles": perf_metrics.cycles,
            "summed_instructions": perf_metrics.instructions,
            "summed_task_clock_ms": perf_metrics.task_clock_ms,
            "summed_instance_throughput_ops_per_sec": benchmark_metrics.rate_value,
            "group_wall_clock_ms": group_wall_ms,
            "aggregate_wall_throughput_ops_per_sec": benchmark_metrics.rate_value,
            "ServerPerfUserTimeMs": perf_metrics.user_time_ms,
            "ServerPerfKernelTimeMs": perf_metrics.kernel_time_ms,
            "ServerPerfCacheRefs": perf_metrics.cache_refs,
            "ServerPerfCacheMisses": perf_metrics.cache_misses,
            "ServerPerfLLCHitRatio": perf_metrics.llc_hit_ratio,
            "status": group_status,
            "perf_event": config.monitoring.perf_event,
            "workload_file": "",
            "result_dir": str(group_dir),
            **df_row(
                df_summary,
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
            ),
        }
    )
    writers["manifest"].writerow(
        {
            "backend": backend_config.name,
            "execution_model": adapter.execution_model,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "result_dir": str(group_dir),
            "status": group_status,
        }
    )

    cleanup_paths([artifacts.cleanup_path], config.keep_result_db)


def run_external_per_instance_group(
    config: HarnessConfig,
    backend_config: BackendConfig,
    assignment: Assignment,
    writers: dict[str, csv.DictWriter],
    group_dir: Path,
    host: str,
    arch: str,
    perf_scope: str,
) -> None:
    adapter = get_backend_adapter(backend_config.name)
    instance_count = assignment_instance_count(assignment)
    threads_per_instance = backend_config.threads_per_instance
    shared_cpu_selector = assignment_shared_cpu_selector(assignment)
    log(
        f"backend={backend_config.name} assignment={assignment.label} instances={instance_count} "
        f"cores={join_ints(assignment.cores)}"
    )

    invocations: list[tuple[InstanceArtifacts, ExternalInvocation]] = []
    for instance_id in range(instance_count):
        instance_dir = group_dir / f"inst{instance_id:02d}"
        instance_dir.mkdir(parents=True, exist_ok=True)
        invocation = adapter.instance_external_invocation(
            config.config_path,
            instance_dir,
            assignment.label,
            instance_id,
            backend_config,
        )
        artifacts = build_instance_artifacts(
            assignment,
            instance_id,
            instance_dir,
            db_target=invocation.db_target,
            cleanup_path=invocation.cleanup_path,
            core_list=assignment.cores if shared_cpu_selector is not None else None,
            numa_list=assignment.numas if shared_cpu_selector is not None else None,
            cpu_selector=shared_cpu_selector,
        )
        artifacts.load_stdout_log.write_text(
            "load phase is not applicable for this backend\n",
            encoding="utf-8",
        )
        artifacts.load_stderr_log.write_text("", encoding="utf-8")
        invocations.append((artifacts, invocation))

    run_processes: list[ManagedProcess] = []
    noise_process: ManagedProcess | None = None
    df_monitor: DfBandwidthMonitor | None = None
    df_summary: DfBandwidthSummary | None = None
    df_error: Exception | None = None
    group_start = time.monotonic()
    try:
        noise_process, _ = start_background_noise(config, assignment, group_dir)
        if config.monitoring.df_enabled:
            df_monitor = DfBandwidthMonitor(
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
                config.monitoring.df_sample_slot_ms,
            )
            df_monitor.start()
        for artifacts, invocation in invocations:
            command, env, cwd = build_external_command(
                config,
                invocation,
                cpu_selector=artifacts.cpu_selector,
                membind_selector=str(artifacts.numa) if artifacts.apply_numa_binding else None,
                time_file=artifacts.time_file,
                perf_file=artifacts.perf_file,
            )
            run_processes.append(
                launch_process(command, env, artifacts.run_stdout_log, artifacts.run_stderr_log, cwd=cwd)
            )
        run_exit_codes = wait_processes(run_processes)
    finally:
        if df_monitor is not None:
            try:
                df_summary = df_monitor.stop()
            except Exception as exc:  # pragma: no cover - hardware path
                df_error = exc
        if noise_process is not None:
            stop_process(noise_process)
    group_wall_ms = (time.monotonic() - group_start) * 1000.0

    benchmark_names: set[str] = set()
    benchmark_classes: set[str] = set()
    benchmark_units: set[str] = set()
    stall_values: list[int | None] = []
    cycle_values: list[int | None] = []
    instruction_values: list[int | None] = []
    task_values: list[float | None] = []
    throughput_values: list[float | None] = []
    user_values: list[float | None] = []
    kernel_values: list[float | None] = []
    cache_ref_values: list[int | None] = []
    cache_miss_values: list[int | None] = []
    llc_load_values: list[int | None] = []
    llc_miss_values: list[int | None] = []
    verification_states: list[bool | None] = []
    parsed_runs: list[ExternalRunMetrics] = []
    group_status = "ok"
    for idx, (artifacts, invocation) in enumerate(invocations):
        perf_metrics = parse_perf_metrics(artifacts.perf_file, artifacts.time_file, config.monitoring.perf_event)
        external_metrics = parse_external_run_log(
            artifacts.run_stdout_log,
            artifacts.run_stderr_log,
            expected_benchmark_name=invocation.benchmark_name,
        )
        benchmark_metrics = external_metrics.benchmark
        if benchmark_metrics.benchmark_name is None or benchmark_metrics.benchmark_class is None:
            benchmark_metrics = BenchmarkRunMetrics(
                benchmark_name=benchmark_metrics.benchmark_name or invocation.benchmark_name,
                benchmark_class=benchmark_metrics.benchmark_class or invocation.benchmark_class,
                time_s=benchmark_metrics.time_s,
                rate_value=benchmark_metrics.rate_value,
                rate_unit=benchmark_metrics.rate_unit,
                verification=benchmark_metrics.verification,
            )
            external_metrics = ExternalRunMetrics(
                benchmark=benchmark_metrics,
                throughput_ops_per_sec=external_metrics.throughput_ops_per_sec,
                primary_operation=external_metrics.primary_operation,
                operation_metrics=external_metrics.operation_metrics,
            )
        parsed_runs.append(external_metrics)
        primary_operation, primary_latency_metrics = summarize_external_latency(external_metrics)

        instance_status = "ok"
        if run_exit_codes[idx] != 0:
            instance_status = "run_failed"
        elif benchmark_metrics.verification_ok is False:
            instance_status = "verification_failed"
        elif (
            external_metrics.throughput_ops_per_sec is None
            or external_metrics.throughput_ops_per_sec <= 0.0
            or perf_metrics_are_required_and_missing(config, perf_metrics)
        ):
            instance_status = "parse_failed"
        if instance_status != "ok" and group_status == "ok":
            group_status = instance_status

        if benchmark_metrics.benchmark_name:
            benchmark_names.add(benchmark_metrics.benchmark_name)
        if benchmark_metrics.benchmark_class:
            benchmark_classes.add(benchmark_metrics.benchmark_class)
        if benchmark_metrics.rate_unit:
            benchmark_units.add(benchmark_metrics.rate_unit)
        verification_states.append(benchmark_metrics.verification_ok)

        stall_values.append(perf_metrics.stall_cycles)
        cycle_values.append(perf_metrics.cycles)
        instruction_values.append(perf_metrics.instructions)
        task_values.append(perf_metrics.task_clock_ms)
        throughput_values.append(external_metrics.throughput_ops_per_sec)
        user_values.append(perf_metrics.user_time_ms)
        kernel_values.append(perf_metrics.kernel_time_ms)
        cache_ref_values.append(perf_metrics.cache_refs)
        cache_miss_values.append(perf_metrics.cache_misses)
        llc_load_values.append(perf_metrics.llc_loads)
        llc_miss_values.append(perf_metrics.llc_load_misses)

        writers["instance"].writerow(
            {
                "timestamp_utc": now_utc_iso(),
                "host": host,
                "arch": arch,
                "backend": backend_config.name,
                "mode": adapter.mode,
                "execution_model": adapter.execution_model,
                "event_name": config.monitoring.event_name,
                "perf_scope": perf_scope,
                "assignment_label": assignment.label,
                "instance_count": instance_count,
                "instance_id": artifacts.instance_id,
                **cpu_list_row(artifacts.core_list, artifacts.numa_list),
                "numa_binding_applied": artifacts.apply_numa_binding,
                "working_set_mb_per_instance": None,
                "recordcount": None,
                "operationcount": None,
                "threads_per_instance": threads_per_instance,
                "java_active_processor_count_per_instance": None,
                "load_threads": None,
                "fieldcount": None,
                "fieldlength": None,
                "approx_value_bytes_per_record": None,
                "primary_operation": primary_operation,
                **benchmark_row(benchmark_metrics),
                **latency_row(primary_latency_metrics),
                "throughput_ops_per_sec": external_metrics.throughput_ops_per_sec,
                "stall_cycles": perf_metrics.stall_cycles,
                "cycles": perf_metrics.cycles,
                "instructions": perf_metrics.instructions,
                "task_clock_ms": perf_metrics.task_clock_ms,
                "ServerPerfUserTimeMs": perf_metrics.user_time_ms,
                "ServerPerfKernelTimeMs": perf_metrics.kernel_time_ms,
                "ServerPerfCacheRefs": perf_metrics.cache_refs,
                "ServerPerfCacheMisses": perf_metrics.cache_misses,
                "ServerPerfLLCHitRatio": perf_metrics.llc_hit_ratio,
                "status": instance_status,
                "perf_event": config.monitoring.perf_event,
                "workload_file": "",
                "db_target": artifacts.db_target,
                "load_stdout_log": str(artifacts.load_stdout_log),
                "load_stderr_log": str(artifacts.load_stderr_log),
                "run_stdout_log": str(artifacts.run_stdout_log),
                "run_stderr_log": str(artifacts.run_stderr_log),
                "perf_stat_file": str(artifacts.perf_file),
                "time_file": str(artifacts.time_file),
                "hdr_dir": "",
                "result_dir": str(group_dir),
            }
        )

        for operation_name, metrics in sorted(external_metrics.operation_metrics.items()):
            writers["instance_operation"].writerow(
                {
                    "timestamp_utc": now_utc_iso(),
                    "host": host,
                    "arch": arch,
                    "backend": backend_config.name,
                    "mode": adapter.mode,
                    "execution_model": adapter.execution_model,
                    "event_name": config.monitoring.event_name,
                    "perf_scope": perf_scope,
                    "assignment_label": assignment.label,
                    "instance_count": instance_count,
                    "instance_id": artifacts.instance_id,
                    "cpu_core": artifacts.core,
                    "cpu_core_list": join_ints(artifacts.core_list),
                    "operation": operation_name,
                    **operation_row(metrics),
                    "status": instance_status,
                    "run_stdout_log": str(artifacts.run_stdout_log),
                    "result_dir": str(group_dir),
                }
            )

    if group_status == "ok" and df_error is not None:
        group_status = "df_failed"

    if group_status == "ok":
        operation_names = sorted(
            {
                operation_name
                for external_metrics in parsed_runs
                for operation_name in external_metrics.operation_metrics.keys()
            }
        )
        for operation_name in operation_names:
            contributing_instances = 0
            contributing_metrics: list[OperationMetrics] = []
            for external_metrics in parsed_runs:
                metrics = external_metrics.operation_metrics.get(operation_name)
                if metrics is None:
                    continue
                contributing_instances += 1
                contributing_metrics.append(metrics)
            merged_metrics = merge_operation_metrics(contributing_metrics)
            if merged_metrics is None:
                continue
            writers["operation"].writerow(
                {
                    "timestamp_utc": now_utc_iso(),
                    "host": host,
                    "arch": arch,
                    "backend": backend_config.name,
                    "mode": adapter.mode,
                    "execution_model": adapter.execution_model,
                    "event_name": config.monitoring.event_name,
                    "perf_scope": perf_scope,
                    "assignment_label": assignment.label,
                    "instance_count": instance_count,
                    "operation": operation_name,
                    "hdr_file_count": contributing_instances,
                    **operation_row(merged_metrics),
                    "status": "ok",
                    "result_dir": str(group_dir),
                }
            )

    group_primary_operation: str | None = None
    group_latency_metrics: OperationMetrics | None = None
    if group_status == "ok":
        operation_names = sorted(
            {
                operation_name
                for external_metrics in parsed_runs
                for operation_name in external_metrics.operation_metrics.keys()
            }
        )
        if len(operation_names) == 1:
            group_primary_operation = operation_names[0]
            group_metrics_list: list[OperationMetrics] = []
            for external_metrics in parsed_runs:
                metrics = external_metrics.operation_metrics.get(group_primary_operation)
                if metrics is not None:
                    group_metrics_list.append(metrics)
            group_latency_metrics = merge_operation_metrics(group_metrics_list)
        elif operation_names:
            group_primary_operation = "QUERY_MIX"
            group_sample_values = []
            for external_metrics in parsed_runs:
                for metrics in external_metrics.operation_metrics.values():
                    group_sample_values.extend(metrics.samples_us)
            if group_sample_values:
                group_latency_metrics = operation_metrics_from_samples(group_sample_values)

    summed_stall_cycles = sum_optional_ints(stall_values) if group_status == "ok" else None
    summed_cycles = sum_optional_ints(cycle_values) if group_status == "ok" else None
    summed_instructions = sum_optional_ints(instruction_values) if group_status == "ok" else None
    summed_task_clock_ms = sum_optional_floats(task_values) if group_status == "ok" else None
    summed_throughput = sum_optional_floats(throughput_values) if group_status == "ok" else None
    summed_user_time = sum_optional_floats(user_values) if group_status == "ok" else None
    summed_kernel_time = sum_optional_floats(kernel_values) if group_status == "ok" else None
    summed_cache_refs = sum_optional_ints(cache_ref_values) if group_status == "ok" else None
    summed_cache_misses = sum_optional_ints(cache_miss_values) if group_status == "ok" else None
    summed_llc_loads = sum_optional_ints(llc_load_values) if group_status == "ok" else None
    summed_llc_misses = sum_optional_ints(llc_miss_values) if group_status == "ok" else None
    llc_ratio = (
        None
        if summed_llc_loads in (None, 0) or summed_llc_misses is None
        else float(summed_llc_loads - summed_llc_misses) / float(summed_llc_loads)
    )

    verification_text: str | None = None
    if verification_states:
        if all(state is True for state in verification_states):
            verification_text = "SUCCESSFUL"
        elif any(state is False for state in verification_states):
            verification_text = "FAILED"

    benchmark_summary = (
        BenchmarkRunMetrics(
            benchmark_name=next(iter(benchmark_names)) if len(benchmark_names) == 1 else None,
            benchmark_class=next(iter(benchmark_classes)) if len(benchmark_classes) == 1 else None,
            time_s=None,
            rate_value=summed_throughput,
            rate_unit=next(iter(benchmark_units)) if len(benchmark_units) == 1 else None,
            verification=verification_text,
        )
        if group_status == "ok"
        else None
    )

    writers["group"].writerow(
        {
            "timestamp_utc": now_utc_iso(),
            "host": host,
            "arch": arch,
            "backend": backend_config.name,
            "mode": adapter.mode,
            "execution_model": adapter.execution_model,
            "event_name": config.monitoring.event_name,
            "perf_scope": perf_scope,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "working_set_mb_per_instance": None,
            "total_working_set_mb": None,
            "recordcount_per_instance": None,
            "total_recordcount": None,
            "operationcount_per_instance": None,
            "total_operationcount": None,
            "threads_per_instance": threads_per_instance,
            "java_active_processor_count_per_instance": None,
            "load_threads": None,
            "fieldcount": None,
            "fieldlength": None,
            "approx_value_bytes_per_record": None,
            "primary_operation": group_primary_operation,
            **benchmark_row(benchmark_summary),
            **latency_row(group_latency_metrics),
            "summed_stall_cycles": summed_stall_cycles,
            "summed_cycles": summed_cycles,
            "summed_instructions": summed_instructions,
            "summed_task_clock_ms": summed_task_clock_ms,
            "summed_instance_throughput_ops_per_sec": summed_throughput,
            "group_wall_clock_ms": group_wall_ms,
            "aggregate_wall_throughput_ops_per_sec": summed_throughput,
            "ServerPerfUserTimeMs": summed_user_time,
            "ServerPerfKernelTimeMs": summed_kernel_time,
            "ServerPerfCacheRefs": summed_cache_refs,
            "ServerPerfCacheMisses": summed_cache_misses,
            "ServerPerfLLCHitRatio": llc_ratio,
            "status": group_status,
            "perf_event": config.monitoring.perf_event,
            "workload_file": "",
            "result_dir": str(group_dir),
            **df_row(
                df_summary,
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
            ),
        }
    )
    writers["manifest"].writerow(
        {
            "backend": backend_config.name,
            "execution_model": adapter.execution_model,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "result_dir": str(group_dir),
            "status": group_status,
        }
    )

    cleanup_paths([artifacts.cleanup_path for artifacts, _ in invocations], config.keep_result_db)


def run_group(
    harness_root: Path,
    config: HarnessConfig,
    backend_config: BackendConfig,
    assignment: Assignment,
    writers: dict[str, csv.DictWriter],
    run_root: Path,
    host: str,
    arch: str,
    perf_scope: str,
    workload_props: dict[str, str],
) -> None:
    adapter = get_backend_adapter(backend_config.name)
    group_dir = run_root / "runs" / adapter.name / safe_name(assignment.label)
    group_dir.mkdir(parents=True, exist_ok=False)

    if adapter.execution_model == "assignment_openmp":
        run_openmp_group(
            config,
            backend_config,
            assignment,
            writers,
            group_dir,
            host,
            arch,
            perf_scope,
        )
        return

    if adapter.execution_model == "per_instance_external":
        run_external_per_instance_group(
            config,
            backend_config,
            assignment,
            writers,
            group_dir,
            host,
            arch,
            perf_scope,
        )
        return

    if config.workload is None:
        raise RuntimeError(f"backend {backend_config.name} requires config.workload")

    base_recordcount = int(workload_props.get("recordcount", "0"))
    if base_recordcount <= 0:
        raise RuntimeError(f"workload {config.workload.file} is missing a positive recordcount")
    base_operationcount = int(workload_props.get("operationcount", "0"))
    operationcount = backend_config.operationcount or base_operationcount or adapter.default_operationcount()
    fieldcount = optional_int(workload_props.get("fieldcount"))
    fieldlength = optional_int(workload_props.get("fieldlength"))
    approx_value_bytes = (config.workload.working_set_mb_per_instance * 1024 * 1024) // base_recordcount
    instance_count = assignment_instance_count(assignment)
    total_working_set_mb = config.workload.working_set_mb_per_instance * instance_count
    total_recordcount = base_recordcount * instance_count
    total_operationcount = operationcount * instance_count
    load_threads = backend_config.load_threads or adapter.default_load_threads(backend_config)
    run_token = f"{now_utc_compact()}_{os.getpid()}"

    shared_cpu_selector = assignment_shared_cpu_selector(assignment)

    log(
        f"backend={backend_config.name} assignment={assignment.label} instances={instance_count} "
        f"cores={join_ints(assignment.cores)}"
    )

    invocations: list[tuple[InstanceArtifacts, YcsbInvocation, YcsbInvocation]] = []
    for instance_id in range(instance_count):
        instance_dir = group_dir / f"inst{instance_id:02d}"
        instance_dir.mkdir(parents=True, exist_ok=True)
        spec = InvocationSpec(
            host=host,
            run_token=run_token,
            assignment_label=safe_name(assignment.label),
            instance_id=instance_id,
            instance_dir=instance_dir,
            workload_file=config.workload.file,
            recordcount=base_recordcount,
            operationcount=operationcount,
            fieldcount=fieldcount,
            fieldlength=fieldlength,
            threads=backend_config.threads_per_instance,
        )
        load_invocation = adapter.load_invocation(spec, backend_config)
        run_invocation = adapter.run_invocation(spec, backend_config)
        artifacts = build_instance_artifacts(
            assignment,
            instance_id,
            instance_dir,
            db_target=run_invocation.db_target,
            cleanup_path=run_invocation.cleanup_path,
            core_list=assignment.cores if shared_cpu_selector is not None else None,
            numa_list=assignment.numas if shared_cpu_selector is not None else None,
            cpu_selector=shared_cpu_selector,
        )
        invocations.append((artifacts, load_invocation, run_invocation))

    load_processes: list[ManagedProcess] = []
    load_noise_process: ManagedProcess | None = None
    try:
        if config.background_noise is not None and config.background_noise.apply_during_load:
            load_noise_process, _ = start_background_noise(
                config,
                assignment,
                group_dir,
                noise_dir_name="load_noise",
            )
        for artifacts, load_invocation, _ in invocations:
            command, env = build_command(
                config,
                load_invocation,
                phase="load",
                cpu_selector=artifacts.cpu_selector,
                membind_selector=str(artifacts.numa) if artifacts.apply_numa_binding else None,
                time_file=None,
                perf_file=None,
                hdr_dir=None,
                active_processor_count=backend_config.java_active_processor_count,
                backend_env=backend_config.env,
            )
            load_processes.append(
                launch_process(command, env, artifacts.load_stdout_log, artifacts.load_stderr_log)
            )
        load_exit_codes = wait_processes(load_processes)
    finally:
        if load_noise_process is not None:
            stop_process(load_noise_process)

    load_operation_counts: list[int | None] = []
    load_failed_operation_counts: list[int] = []
    for artifacts, _, _ in invocations:
        _, load_operations = parse_ycsb_run_log(artifacts.load_stdout_log)
        load_operation_counts.append(completed_ycsb_operations(load_operations))
        load_failed_operation_counts.append(failed_ycsb_operations(load_operations))

    load_failed = any(code != 0 for code in load_exit_codes) or any(
        count != base_recordcount or failed_count > 0
        for count, failed_count in zip(load_operation_counts, load_failed_operation_counts)
    )
    if load_failed:
        status = "load_failed"
        for artifacts, _, _ in invocations:
            writers["instance"].writerow(
                {
                    "timestamp_utc": now_utc_iso(),
                    "host": host,
                    "arch": arch,
                    "backend": backend_config.name,
                    "mode": adapter.mode,
                    "execution_model": adapter.execution_model,
                    "event_name": config.monitoring.event_name,
                    "perf_scope": perf_scope,
                    "assignment_label": assignment.label,
                    "instance_count": instance_count,
                    "instance_id": artifacts.instance_id,
                    **cpu_list_row(artifacts.core_list, artifacts.numa_list),
                    "numa_binding_applied": artifacts.apply_numa_binding,
                    "working_set_mb_per_instance": config.workload.working_set_mb_per_instance,
                    "recordcount": base_recordcount,
                    "operationcount": operationcount,
                    "threads_per_instance": backend_config.threads_per_instance,
                    "java_active_processor_count_per_instance": backend_config.java_active_processor_count,
                    "load_threads": load_threads,
                    "fieldcount": fieldcount,
                    "fieldlength": fieldlength,
                    "approx_value_bytes_per_record": approx_value_bytes,
                    "primary_operation": config.workload.primary_operation,
                    **benchmark_row(None),
                    "status": status,
                    "perf_event": config.monitoring.perf_event,
                    "workload_file": str(config.workload.file),
                    "db_target": artifacts.db_target,
                    "load_stdout_log": str(artifacts.load_stdout_log),
                    "load_stderr_log": str(artifacts.load_stderr_log),
                    "run_stdout_log": str(artifacts.run_stdout_log),
                    "run_stderr_log": str(artifacts.run_stderr_log),
                    "perf_stat_file": str(artifacts.perf_file),
                    "time_file": str(artifacts.time_file),
                    "hdr_dir": str(artifacts.hdr_dir),
                    "result_dir": str(group_dir),
                }
            )
        writers["group"].writerow(
            {
                "timestamp_utc": now_utc_iso(),
                "host": host,
                "arch": arch,
                "backend": backend_config.name,
                "mode": adapter.mode,
                "execution_model": adapter.execution_model,
                "event_name": config.monitoring.event_name,
                "perf_scope": perf_scope,
                "assignment_label": assignment.label,
                "instance_count": instance_count,
                "selected_cpu_count": len(assignment.cores),
                "cores": join_ints(assignment.cores),
                "numas": join_ints(assignment.numas),
                "numa_binding_applied": assignment.apply_numa_binding,
                "working_set_mb_per_instance": config.workload.working_set_mb_per_instance,
                "total_working_set_mb": total_working_set_mb,
                "recordcount_per_instance": base_recordcount,
                "total_recordcount": total_recordcount,
                "operationcount_per_instance": operationcount,
                "total_operationcount": total_operationcount,
                "threads_per_instance": backend_config.threads_per_instance,
                "java_active_processor_count_per_instance": backend_config.java_active_processor_count,
                "load_threads": load_threads,
                "fieldcount": fieldcount,
                "fieldlength": fieldlength,
                "approx_value_bytes_per_record": approx_value_bytes,
                "primary_operation": config.workload.primary_operation,
                **benchmark_row(None),
                "status": status,
                "perf_event": config.monitoring.perf_event,
                "workload_file": str(config.workload.file),
                "result_dir": str(group_dir),
                **df_row(
                    None,
                    config.monitoring.df_resource_family,
                    config.monitoring.df_resource_ids,
                ),
            }
        )
        writers["manifest"].writerow(
            {
                "backend": backend_config.name,
                "execution_model": adapter.execution_model,
                "assignment_label": assignment.label,
                "instance_count": instance_count,
                "selected_cpu_count": len(assignment.cores),
                "cores": join_ints(assignment.cores),
                "numas": join_ints(assignment.numas),
                "numa_binding_applied": assignment.apply_numa_binding,
                "result_dir": str(group_dir),
                "status": status,
            }
        )
        cleanup_paths([artifacts.cleanup_path for artifacts, _, _ in invocations], config.keep_result_db)
        return

    run_processes: list[ManagedProcess] = []
    noise_process: ManagedProcess | None = None
    df_monitor: DfBandwidthMonitor | None = None
    df_summary: DfBandwidthSummary | None = None
    group_status = "ok"
    df_error: Exception | None = None
    group_start = time.monotonic()
    try:
        noise_process, _ = start_background_noise(config, assignment, group_dir)
        if config.monitoring.df_enabled:
            df_monitor = DfBandwidthMonitor(
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
                config.monitoring.df_sample_slot_ms,
            )
            df_monitor.start()
        for artifacts, _, run_invocation in invocations:
            command, env = build_command(
                config,
                run_invocation,
                phase="run",
                cpu_selector=artifacts.cpu_selector,
                membind_selector=str(artifacts.numa) if artifacts.apply_numa_binding else None,
                time_file=artifacts.time_file,
                perf_file=artifacts.perf_file,
                hdr_dir=artifacts.hdr_dir,
                active_processor_count=backend_config.java_active_processor_count,
                backend_env=backend_config.env,
            )
            run_processes.append(
                launch_process(command, env, artifacts.run_stdout_log, artifacts.run_stderr_log)
            )
        run_exit_codes = wait_processes(run_processes)
    finally:
        if df_monitor is not None:
            try:
                df_summary = df_monitor.stop()
            except Exception as exc:  # pragma: no cover - hardware path
                df_error = exc
        if noise_process is not None:
            stop_process(noise_process)
    group_wall_ms = (time.monotonic() - group_start) * 1000.0

    if any(code != 0 for code in run_exit_codes):
        group_status = "run_failed"

    stall_values: list[int | None] = []
    cycle_values: list[int | None] = []
    instruction_values: list[int | None] = []
    task_values: list[float | None] = []
    throughput_values: list[float | None] = []
    user_values: list[float | None] = []
    kernel_values: list[float | None] = []
    cache_ref_values: list[int | None] = []
    cache_miss_values: list[int | None] = []
    llc_load_values: list[int | None] = []
    llc_miss_values: list[int | None] = []
    parsed_ops_per_instance: list[dict[str, OperationMetrics]] = []
    completed_ops_per_instance: list[int | None] = []
    instance_statuses: list[str] = []

    for idx, (artifacts, _, _) in enumerate(invocations):
        perf_metrics = parse_perf_metrics(artifacts.perf_file, artifacts.time_file, config.monitoring.perf_event)
        throughput, operations = parse_ycsb_run_log(artifacts.run_stdout_log)
        parsed_ops_per_instance.append(operations)
        completed_operations = completed_ycsb_operations(operations)
        failed_operations = failed_ycsb_operations(operations)
        completed_ops_per_instance.append(completed_operations)
        instance_status = "ok"
        if run_exit_codes[idx] != 0:
            instance_status = "run_failed"
        elif (
            throughput is None
            or throughput <= 0.0
            or completed_operations is None
            or failed_operations > 0
            or perf_metrics_are_required_and_missing(config, perf_metrics)
        ):
            instance_status = "parse_failed"
        if instance_status != "ok" and group_status == "ok":
            group_status = instance_status
        instance_statuses.append(instance_status)

        primary_metrics = operations.get(config.workload.primary_operation)
        stall_values.append(perf_metrics.stall_cycles)
        cycle_values.append(perf_metrics.cycles)
        instruction_values.append(perf_metrics.instructions)
        task_values.append(perf_metrics.task_clock_ms)
        throughput_values.append(throughput)
        user_values.append(perf_metrics.user_time_ms)
        kernel_values.append(perf_metrics.kernel_time_ms)
        cache_ref_values.append(perf_metrics.cache_refs)
        cache_miss_values.append(perf_metrics.cache_misses)
        llc_load_values.append(perf_metrics.llc_loads)
        llc_miss_values.append(perf_metrics.llc_load_misses)

        writers["instance"].writerow(
            {
                "timestamp_utc": now_utc_iso(),
                "host": host,
                "arch": arch,
                "backend": backend_config.name,
                "mode": adapter.mode,
                "execution_model": adapter.execution_model,
                "event_name": config.monitoring.event_name,
                "perf_scope": perf_scope,
                "assignment_label": assignment.label,
                "instance_count": instance_count,
                "instance_id": artifacts.instance_id,
                **cpu_list_row(artifacts.core_list, artifacts.numa_list),
                "numa_binding_applied": artifacts.apply_numa_binding,
                "working_set_mb_per_instance": config.workload.working_set_mb_per_instance,
                "recordcount": base_recordcount,
                "operationcount": operationcount,
                "threads_per_instance": backend_config.threads_per_instance,
                "java_active_processor_count_per_instance": backend_config.java_active_processor_count,
                "load_threads": load_threads,
                "fieldcount": fieldcount,
                "fieldlength": fieldlength,
                "approx_value_bytes_per_record": approx_value_bytes,
                "primary_operation": config.workload.primary_operation,
                **benchmark_row(None),
                **latency_row(primary_metrics),
                "throughput_ops_per_sec": throughput,
                "stall_cycles": perf_metrics.stall_cycles,
                "cycles": perf_metrics.cycles,
                "instructions": perf_metrics.instructions,
                "task_clock_ms": perf_metrics.task_clock_ms,
                "ServerPerfUserTimeMs": perf_metrics.user_time_ms,
                "ServerPerfKernelTimeMs": perf_metrics.kernel_time_ms,
                "ServerPerfCacheRefs": perf_metrics.cache_refs,
                "ServerPerfCacheMisses": perf_metrics.cache_misses,
                "ServerPerfLLCHitRatio": perf_metrics.llc_hit_ratio,
                "status": instance_status,
                "perf_event": config.monitoring.perf_event,
                "workload_file": str(config.workload.file),
                "db_target": artifacts.db_target,
                "load_stdout_log": str(artifacts.load_stdout_log),
                "load_stderr_log": str(artifacts.load_stderr_log),
                "run_stdout_log": str(artifacts.run_stdout_log),
                "run_stderr_log": str(artifacts.run_stderr_log),
                "perf_stat_file": str(artifacts.perf_file),
                "time_file": str(artifacts.time_file),
                "hdr_dir": str(artifacts.hdr_dir),
                "result_dir": str(group_dir),
            }
        )

        for operation, metrics in operations.items():
            writers["instance_operation"].writerow(
                {
                    "timestamp_utc": now_utc_iso(),
                    "host": host,
                    "arch": arch,
                    "backend": backend_config.name,
                    "mode": adapter.mode,
                    "execution_model": adapter.execution_model,
                    "event_name": config.monitoring.event_name,
                    "perf_scope": perf_scope,
                    "assignment_label": assignment.label,
                    "instance_count": instance_count,
                    "instance_id": artifacts.instance_id,
                    "cpu_core": artifacts.core,
                    "cpu_core_list": join_ints(artifacts.core_list),
                    "operation": operation,
                    **operation_row(metrics),
                    "status": instance_status,
                    "run_stdout_log": str(artifacts.run_stdout_log),
                    "result_dir": str(group_dir),
                }
            )

    if group_status == "ok" and df_error is not None:
        group_status = "df_failed"

    merged_operations: dict[str, dict[str, float]] = {}
    if group_status == "ok":
        operation_names = sorted(
            {
                operation
                for instance_ops in parsed_ops_per_instance
                for operation in instance_ops.keys()
            }
        )
        for operation in operation_names:
            hdr_files = [
                artifacts.hdr_dir / f"{operation}.hdr"
                for artifacts, _, _ in invocations
                if (artifacts.hdr_dir / f"{operation}.hdr").exists()
            ]
            if len(hdr_files) != instance_count:
                group_status = "latency_merge_failed"
                break
            merged = merge_histograms(
                harness_root,
                config.ycsb_root,
                hdr_files,
                config.workload.hdr_percentiles,
            )
            merged_operations[operation] = merged
            writers["operation"].writerow(
                {
                    "timestamp_utc": now_utc_iso(),
                    "host": host,
                    "arch": arch,
                    "backend": backend_config.name,
                    "mode": adapter.mode,
                    "execution_model": adapter.execution_model,
                    "event_name": config.monitoring.event_name,
                    "perf_scope": perf_scope,
                    "assignment_label": assignment.label,
                    "instance_count": instance_count,
                    "operation": operation,
                    "hdr_file_count": len(hdr_files),
                    **operation_row(merged),
                    "status": "ok",
                    "result_dir": str(group_dir),
                }
            )

    if group_status != "ok":
        primary_row: dict[str, Any] = {}
        summed_stall_cycles = None
        summed_cycles = None
        summed_instructions = None
        summed_task_clock_ms = None
        summed_throughput = None
        aggregate_wall_throughput = None
        summed_user_time = None
        summed_kernel_time = None
        summed_cache_refs = None
        summed_cache_misses = None
        summed_llc_loads = None
        summed_llc_misses = None
        llc_ratio = None
    else:
        primary_merged = merged_operations.get(config.workload.primary_operation)
        if primary_merged is None:
            group_status = "latency_merge_failed"
            primary_row = {}
        else:
            primary_row = operation_row(primary_merged)
        summed_stall_cycles = sum_optional_ints(stall_values)
        summed_cycles = sum_optional_ints(cycle_values)
        summed_instructions = sum_optional_ints(instruction_values)
        summed_task_clock_ms = sum_optional_floats(task_values)
        summed_throughput = sum_optional_floats(throughput_values)
        completed_operationcount = sum_optional_ints(completed_ops_per_instance)
        aggregate_wall_throughput = (
            throughput_from_wall_ms(completed_operationcount, group_wall_ms)
            if completed_operationcount is not None and completed_operationcount > 0
            else None
        )
        summed_user_time = sum_optional_floats(user_values)
        summed_kernel_time = sum_optional_floats(kernel_values)
        summed_cache_refs = sum_optional_ints(cache_ref_values)
        summed_cache_misses = sum_optional_ints(cache_miss_values)
        summed_llc_loads = sum_optional_ints(llc_load_values)
        summed_llc_misses = sum_optional_ints(llc_miss_values)
        llc_ratio = (
            None
            if summed_llc_loads in (None, 0) or summed_llc_misses is None
            else float(summed_llc_loads - summed_llc_misses) / float(summed_llc_loads)
        )
        if group_status == "latency_merge_failed":
            primary_row = {}

    writers["group"].writerow(
        {
            "timestamp_utc": now_utc_iso(),
            "host": host,
            "arch": arch,
            "backend": backend_config.name,
            "mode": adapter.mode,
            "execution_model": adapter.execution_model,
            "event_name": config.monitoring.event_name,
            "perf_scope": perf_scope,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "working_set_mb_per_instance": config.workload.working_set_mb_per_instance,
            "total_working_set_mb": total_working_set_mb,
            "recordcount_per_instance": base_recordcount,
            "total_recordcount": total_recordcount,
            "operationcount_per_instance": operationcount,
            "total_operationcount": total_operationcount,
            "threads_per_instance": backend_config.threads_per_instance,
            "java_active_processor_count_per_instance": backend_config.java_active_processor_count,
            "load_threads": load_threads,
            "fieldcount": fieldcount,
            "fieldlength": fieldlength,
            "approx_value_bytes_per_record": approx_value_bytes,
            "primary_operation": config.workload.primary_operation,
            **benchmark_row(None),
            "latency_operations": primary_row.get("operations"),
            "latency_average_us": primary_row.get("average_us"),
            "latency_min_us": primary_row.get("min_us"),
            "latency_max_us": primary_row.get("max_us"),
            "latency_p50_us": primary_row.get("p50_us"),
            "latency_p95_us": primary_row.get("p95_us"),
            "latency_p99_us": primary_row.get("p99_us"),
            "latency_p99_9_us": primary_row.get("p99_9_us"),
            "latency_p99_99_us": primary_row.get("p99_99_us"),
            "summed_stall_cycles": summed_stall_cycles,
            "summed_cycles": summed_cycles,
            "summed_instructions": summed_instructions,
            "summed_task_clock_ms": summed_task_clock_ms,
            "summed_instance_throughput_ops_per_sec": summed_throughput,
            "group_wall_clock_ms": group_wall_ms,
            "aggregate_wall_throughput_ops_per_sec": aggregate_wall_throughput,
            "ServerPerfUserTimeMs": summed_user_time,
            "ServerPerfKernelTimeMs": summed_kernel_time,
            "ServerPerfCacheRefs": summed_cache_refs,
            "ServerPerfCacheMisses": summed_cache_misses,
            "ServerPerfLLCHitRatio": llc_ratio,
            "status": group_status,
            "perf_event": config.monitoring.perf_event,
            "workload_file": str(config.workload.file),
            "result_dir": str(group_dir),
            **df_row(
                df_summary,
                config.monitoring.df_resource_family,
                config.monitoring.df_resource_ids,
            ),
        }
    )
    writers["manifest"].writerow(
        {
            "backend": backend_config.name,
            "execution_model": adapter.execution_model,
            "assignment_label": assignment.label,
            "instance_count": instance_count,
            "selected_cpu_count": len(assignment.cores),
            "cores": join_ints(assignment.cores),
            "numas": join_ints(assignment.numas),
            "numa_binding_applied": assignment.apply_numa_binding,
            "result_dir": str(group_dir),
            "status": group_status,
        }
    )

    cleanup_paths([artifacts.cleanup_path for artifacts, _, _ in invocations], config.keep_result_db)


def run_harness(config: HarnessConfig) -> int:
    validate_config(config)
    host = subprocess.run(["hostname", "-s"], check=True, capture_output=True, text=True).stdout.strip()
    arch = detect_arch()
    perf_scope = perf_scope_label()
    workload_props = {} if config.workload is None else load_workload_properties(config.workload.file)
    if config.monitoring.perf_enabled:
        log(f"using perf binary: {resolve_perf_binary()}")
    else:
        log("performance counters disabled")

    run_dir = config.result_root / f"{config.result_prefix}_{host}_{now_utc_compact()}_{os.getpid()}"
    run_dir.mkdir(parents=True, exist_ok=False)
    (run_dir / "runs").mkdir(parents=True, exist_ok=True)
    (run_dir / "resolved_config.json").write_text(
        json.dumps(config_to_dict(config), indent=2), encoding="utf-8"
    )

    writers, handles = create_writers(
        run_dir,
        config.monitoring.df_resource_family,
        config.monitoring.df_resource_ids,
    )
    try:
        for backend_config in config.backends:
            adapter = get_backend_adapter(backend_config.name)
            log(f"ensuring backend assets are built for backend={backend_config.name}")
            adapter.ensure_built(config.ycsb_root, backend_config, config.config_path)
            for assignment in config.assignments:
                try:
                    run_group(
                        Path(__file__).resolve().parent.parent,
                        config,
                        backend_config,
                        assignment,
                        writers,
                        run_dir,
                        host,
                        arch,
                        perf_scope,
                        workload_props,
                    )
                except Exception as exc:
                    log(f"backend={backend_config.name} assignment={assignment.label} failed: {exc}")
                    writers["manifest"].writerow(
                        {
                            "backend": backend_config.name,
                            "execution_model": adapter.execution_model,
                            "assignment_label": assignment.label,
                            "instance_count": len(assignment.cores),
                            "selected_cpu_count": len(assignment.cores),
                            "cores": join_ints(assignment.cores),
                            "numas": join_ints(assignment.numas),
                            "numa_binding_applied": assignment.apply_numa_binding,
                            "result_dir": str(run_dir / "runs" / backend_config.name / safe_name(assignment.label)),
                            "status": f"error:{exc}",
                        }
                    )
                    if config.fail_fast:
                        raise
    finally:
        close_handles(handles)

    auto_merge_script = Path(__file__).resolve().parent.parent / "scripts" / "auto_merge_results.py"
    if os.environ.get("HARNESS_AUTO_MERGE", "1") != "0" and auto_merge_script.exists():
        auto_merge = subprocess.run(
            [sys.executable, str(auto_merge_script), "--result-prefix", config.result_prefix],
            check=False,
            capture_output=True,
            text=True,
        )
        if auto_merge.stdout:
            for line in auto_merge.stdout.splitlines():
                log(f"auto-merge: {line}")
        if auto_merge.returncode != 0:
            if auto_merge.stderr:
                for line in auto_merge.stderr.splitlines():
                    log(f"auto-merge stderr: {line}")
            log(
                f"auto-merge failed for result_prefix={config.result_prefix}; "
                f"continuing because experiment results were already written"
            )

    log(f"completed harness run: {run_dir}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Chiplet-aware YCSB harness")
    parser.add_argument("--config", required=True, help="Path to harness JSON config")
    parser.add_argument("--dry-run", action="store_true", help="Validate config and print the plan")
    args = parser.parse_args(argv)

    config = load_config(Path(args.config))
    if args.dry_run:
        validate_config(config, enforce_runtime_requirements=False)
        print_dry_run(config)
        return 0
    return run_harness(config)


if __name__ == "__main__":
    raise SystemExit(main())
