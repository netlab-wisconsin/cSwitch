from __future__ import annotations

import csv
import json
import math
import os
import re
import shlex
import signal
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence


PHYSICAL_CORE_MIN = 0
PHYSICAL_CORE_MAX = 83
DEFAULT_CLOCK_GHZ = 2.25
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
NPB_NAME_RE = re.compile(
    r"^\s*NAS Parallel Benchmarks.*-\s*([A-Za-z0-9_+-]+)\s+Benchmark",
    re.IGNORECASE,
)
NPB_CLASS_RE = re.compile(r"^\s*Class\s*=\s*([A-Za-z0-9_+-]+)\s*$", re.IGNORECASE)
NPB_TIME_RE = re.compile(r"^\s*Time in seconds\s*=\s*([0-9.eE+-]+)\s*$", re.IGNORECASE)
NPB_MOPS_RE = re.compile(r"^\s*(?:Mop/s total|MOPS total|MFLOPS)\s*=\s*([0-9.eE+-]+)\s*$", re.IGNORECASE)
NPB_VERIFICATION_RE = re.compile(
    r"^\s*Verification(?:\s*=\s*|\s+)(.+?)\s*$",
    re.IGNORECASE,
)


@dataclass
class CommandResult:
    argv: list[str]
    returncode: int
    output: str


@dataclass
class BackgroundProcess:
    argv: list[str]
    process: subprocess.Popen[str]
    log_path: Path
    log_handle: object


@dataclass(frozen=True)
class NpbMetrics:
    benchmark_name: str | None = None
    benchmark_class: str | None = None
    time_s: float | None = None
    mops_total: float | None = None
    verification: str | None = None

    @property
    def verification_ok(self) -> bool | None:
        if self.verification is None:
            return None
        value = self.verification.strip().upper()
        if "SUCCESS" in value:
            return True
        if "UNSUCCESS" in value or "FAIL" in value:
            return False
        return None


@dataclass(frozen=True)
class LlamaBenchMetrics:
    model_filename: str | None = None
    prompt_tokens_per_s: float | None = None
    gen_tokens_per_s: float | None = None
    total_tokens_per_s: float | None = None
    primary_tokens_per_s: float | None = None
    prompt_time_s: float | None = None
    gen_time_s: float | None = None
    total_time_s: float | None = None
    primary_time_s: float | None = None


def format_command(argv: Sequence[str]) -> str:
    return shlex.join(argv)


def ensure_directory(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    return path


def require_path(path: Path, description: str, *, executable: bool = False) -> None:
    if not path.exists():
        raise FileNotFoundError(f"{description} not found: {path}")
    if executable and not os.access(path, os.X_OK):
        raise PermissionError(f"{description} is not executable: {path}")


def build_env(
    *,
    updates: Mapping[str, str] | None = None,
    removals: Sequence[str] = (),
) -> dict[str, str]:
    env = os.environ.copy()
    for key in removals:
        env.pop(key, None)
    if updates:
        env.update(updates)
    return env


def run_capture(
    argv: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    log_path: Path | None = None,
    timeout_seconds: float | None = None,
) -> CommandResult:
    try:
        completed = subprocess.run(
            list(argv),
            cwd=str(cwd) if cwd else None,
            env=dict(env) if env is not None else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
            timeout=timeout_seconds,
        )
        output = completed.stdout
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout or ""
        if isinstance(output, bytes):
            output = output.decode(errors="replace")
        if log_path is not None:
            ensure_directory(log_path.parent)
            log_path.write_text(output)
        raise RuntimeError(
            f"Command timed out after {timeout_seconds}s: {format_command(argv)}\n{output}"
        ) from exc
    if log_path is not None:
        ensure_directory(log_path.parent)
        log_path.write_text(output)
    if completed.returncode != 0:
        raise RuntimeError(
            f"Command failed ({completed.returncode}): {format_command(argv)}\n{output}"
        )
    return CommandResult(list(argv), completed.returncode, output)


def start_background(
    argv: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    log_path: Path,
) -> BackgroundProcess:
    ensure_directory(log_path.parent)
    handle = log_path.open("w")
    process = subprocess.Popen(
        list(argv),
        cwd=str(cwd) if cwd else None,
        env=dict(env) if env is not None else None,
        stdout=handle,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return BackgroundProcess(list(argv), process, log_path, handle)


def stop_background(background: BackgroundProcess, *, timeout: float = 10.0) -> None:
    process = background.process
    try:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=timeout)
        returncode = process.returncode
    finally:
        background.log_handle.close()

    if returncode not in (0, -signal.SIGTERM, 128 + signal.SIGTERM):
        raise RuntimeError(
            f"Background command exited unexpectedly ({returncode}): "
            f"{format_command(background.argv)}\nSee log: {background.log_path}"
        )


def strip_ansi(text: str) -> str:
    return ANSI_ESCAPE_RE.sub("", text)


def parse_gapbs_trial_time(output: str) -> float:
    clean = strip_ansi(output)
    match = re.search(r"Trial Time:\s*([0-9]+(?:\.[0-9]+)?)", clean)
    if not match:
        raise ValueError(f"Could not find GAPBS trial time in output:\n{clean}")
    return float(match.group(1))


def parse_mean_latency_ns(output: str) -> float:
    clean = strip_ansi(output)
    mean_match = re.search(r"Mean latency:\s*([0-9]+(?:\.[0-9]+)?)ns", clean)
    if mean_match:
        return float(mean_match.group(1))

    csv_matches = re.findall(r",\s*([0-9]+(?:\.[0-9]+)?)\s*,?", clean)
    if csv_matches:
        return float(csv_matches[-1])

    raise ValueError(f"Could not parse latency output:\n{clean}")


def parse_mean_bandwidth_gibs(output: str) -> float:
    clean = strip_ansi(output)
    csv_matches = re.findall(r"(?:^|[,\n])\s*([0-9]+(?:\.[0-9]+)?)\s*,", clean)
    if csv_matches:
        return float(csv_matches[-1])

    mean_match = re.search(r"Mean bandwidth:\s*([0-9]+(?:\.[0-9]+)?)GiB/s", clean)
    if mean_match:
        return float(mean_match.group(1))

    raise ValueError(f"Could not parse bandwidth output:\n{clean}")


def parse_npb_metrics(output: str) -> NpbMetrics:
    clean = strip_ansi(output)
    metrics = NpbMetrics()

    for raw_line in clean.splitlines():
        line = raw_line.strip()
        if not line:
            continue

        match = NPB_NAME_RE.match(line)
        if match:
            metrics = NpbMetrics(
                benchmark_name=match.group(1).upper(),
                benchmark_class=metrics.benchmark_class,
                time_s=metrics.time_s,
                mops_total=metrics.mops_total,
                verification=metrics.verification,
            )
            continue

        match = NPB_CLASS_RE.match(line)
        if match:
            metrics = NpbMetrics(
                benchmark_name=metrics.benchmark_name,
                benchmark_class=match.group(1).upper(),
                time_s=metrics.time_s,
                mops_total=metrics.mops_total,
                verification=metrics.verification,
            )
            continue

        match = NPB_TIME_RE.match(line)
        if match:
            metrics = NpbMetrics(
                benchmark_name=metrics.benchmark_name,
                benchmark_class=metrics.benchmark_class,
                time_s=float(match.group(1)),
                mops_total=metrics.mops_total,
                verification=metrics.verification,
            )
            continue

        match = NPB_MOPS_RE.match(line)
        if match:
            metrics = NpbMetrics(
                benchmark_name=metrics.benchmark_name,
                benchmark_class=metrics.benchmark_class,
                time_s=metrics.time_s,
                mops_total=float(match.group(1)),
                verification=metrics.verification,
            )
            continue

        match = NPB_VERIFICATION_RE.match(line)
        if match:
            metrics = NpbMetrics(
                benchmark_name=metrics.benchmark_name,
                benchmark_class=metrics.benchmark_class,
                time_s=metrics.time_s,
                mops_total=metrics.mops_total,
                verification=match.group(1).strip(),
            )

    return metrics


def parse_npb_metrics_from_log(log_path: Path) -> NpbMetrics:
    if not log_path.exists():
        raise FileNotFoundError(f"NPB log not found: {log_path}")
    return parse_npb_metrics(log_path.read_text())


def parse_llama_bench_metrics(output: str) -> LlamaBenchMetrics:
    clean = strip_ansi(output)
    model_filename: str | None = None
    prompt_tokens_per_s: float | None = None
    gen_tokens_per_s: float | None = None
    prompt_time_s: float | None = None
    gen_time_s: float | None = None
    total_tokens = 0
    total_time_ns = 0.0

    for raw_line in clean.splitlines():
        line = raw_line.strip()
        if not line.startswith("{") or not line.endswith("}"):
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(payload, dict):
            continue

        avg_ns_raw = payload.get("avg_ns")
        avg_ts_raw = payload.get("avg_ts")
        n_prompt_raw = payload.get("n_prompt")
        n_gen_raw = payload.get("n_gen")
        if avg_ns_raw is None or avg_ts_raw is None:
            continue

        try:
            avg_ns = float(avg_ns_raw)
            avg_ts = float(avg_ts_raw)
            n_prompt = int(n_prompt_raw or 0)
            n_gen = int(n_gen_raw or 0)
        except (TypeError, ValueError):
            continue

        model_filename = str(payload.get("model_filename") or model_filename or "")
        total_tokens += n_prompt + n_gen
        total_time_ns += avg_ns

        if n_prompt > 0 and n_gen == 0:
            prompt_tokens_per_s = avg_ts
            prompt_time_s = avg_ns / 1_000_000_000.0
        elif n_gen > 0 and n_prompt == 0:
            gen_tokens_per_s = avg_ts
            gen_time_s = avg_ns / 1_000_000_000.0

    if total_tokens <= 0 or total_time_ns <= 0.0:
        raise ValueError(f"Could not parse llama-bench output:\n{clean}")

    total_time_s = total_time_ns / 1_000_000_000.0
    total_tokens_per_s = float(total_tokens) / total_time_s
    primary_tokens_per_s = gen_tokens_per_s if gen_tokens_per_s is not None else total_tokens_per_s
    primary_time_s = gen_time_s if gen_time_s is not None else total_time_s

    return LlamaBenchMetrics(
        model_filename=model_filename or None,
        prompt_tokens_per_s=prompt_tokens_per_s,
        gen_tokens_per_s=gen_tokens_per_s,
        total_tokens_per_s=total_tokens_per_s,
        primary_tokens_per_s=primary_tokens_per_s,
        prompt_time_s=prompt_time_s,
        gen_time_s=gen_time_s,
        total_time_s=total_time_s,
        primary_time_s=primary_time_s,
    )


def parse_llama_bench_metrics_from_log(log_path: Path) -> LlamaBenchMetrics:
    if not log_path.exists():
        raise FileNotFoundError(f"llama-bench log not found: {log_path}")
    return parse_llama_bench_metrics(log_path.read_text())


def parse_noise_bandwidth_sum(log_path: Path) -> float:
    if not log_path.exists():
        return 0.0

    total = 0.0
    for line in log_path.read_text().splitlines():
        match = re.search(r"Bandwidth:\s*([0-9]+(?:\.[0-9]+)?)\s*MB/s", line)
        if match:
            total += float(match.group(1))
    return total


def parse_perf_sched_cpu_list(
    log_path: Path,
    *,
    comm_names: Sequence[str],
) -> str:
    if not log_path.exists():
        return ""

    cpu_re = re.compile(r"\[(\d+)\]\s+(?:sched:)?sched_[a-z_]+:")
    observed: list[str] = []
    seen: set[str] = set()

    for raw_line in log_path.read_text().splitlines():
        line = strip_ansi(raw_line)
        if not any(
            f"prev_comm={comm}" in line or f"next_comm={comm}" in line or line.startswith(f"{comm} ")
            for comm in comm_names
        ):
            continue
        match = cpu_re.search(line)
        if not match:
            continue
        cpu = match.group(1)
        if cpu in seen:
            continue
        seen.add(cpu)
        observed.append(cpu)

    return ",".join(observed)


def parse_perf_sched_timehist(
    log_path: Path,
    *,
    comm_names: Sequence[str],
) -> list[dict[str, object]]:
    if not log_path.exists():
        return []

    line_re = re.compile(
        r"^\s*[0-9]+\.[0-9]+\s+\[(\d+)\]\s+(\S+)\[(\d+)(?:/(\d+))?\]\s+"
        r"([0-9]+\.[0-9]+)\s+([0-9]+\.[0-9]+)\s+([0-9]+\.[0-9]+)\s*$"
    )
    rows: list[dict[str, object]] = []

    for raw_line in log_path.read_text().splitlines():
        line = strip_ansi(raw_line)
        match = line_re.match(line)
        if not match:
            continue
        comm = match.group(2)
        if comm not in comm_names:
            continue

        cpu = int(match.group(1))
        tid = int(match.group(3))
        pid = int(match.group(4) or match.group(3))
        run_time_ms = float(match.group(7))
        if run_time_ms <= 0.0:
            continue

        rows.append(
            {
                "comm": comm,
                "cpu": cpu,
                "tid": tid,
                "pid": pid,
                "wait_time_ms": float(match.group(5)),
                "sched_delay_ms": float(match.group(6)),
                "run_time_ms": run_time_ms,
            }
        )

    return rows


def write_memory_benchmark_config(
    path: Path,
    *,
    nthreads: int,
    cores: Sequence[int],
    rates: Sequence[int],
    worker_memory_mb: int,
    output_path: Path,
    modes: Sequence[int] | None = None,
    numa_nodes: Sequence[int] | None = None,
    duration_seconds: int = 3600,
    memory_size: str = "128m",
    bandwidth: bool = True,
    silent: bool = True,
    clock_ghz: float = DEFAULT_CLOCK_GHZ,
    worker_numa_policy: str = "bind",
    worker_numa_interleave_nodes: Sequence[int] | None = None,
) -> None:
    if nthreads <= 0:
        raise ValueError("nthreads must be positive")
    if len(cores) < nthreads:
        raise ValueError("Not enough cores for requested nthreads")
    if len(rates) < nthreads:
        raise ValueError("Not enough rates for requested nthreads")

    active_cores = list(cores[:nthreads])
    active_rates = list(rates[:nthreads])
    active_modes = list((modes or [0] * nthreads)[:nthreads])
    active_numa = list((numa_nodes or [0] * nthreads)[:nthreads])
    interleave_nodes = list(worker_numa_interleave_nodes or [])

    if worker_numa_policy not in {"bind", "interleave"}:
        raise ValueError(f"Unsupported worker_numa_policy: {worker_numa_policy}")
    if worker_numa_policy == "interleave" and not interleave_nodes:
        raise ValueError("worker_numa_interleave_nodes must be provided for interleave mode")

    validate_core_range(active_cores)
    ensure_directory(path.parent)
    ensure_directory(output_path.parent)

    xml = f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{nthreads}</nthreads>
  <memory>{memory_size}</memory>
  <worker_memory_mb>{worker_memory_mb}</worker_memory_mb>
  <output>{output_path}</output>
  <bandwidth>{"true" if bandwidth else "false"}</bandwidth>
  <time>{duration_seconds}</time>
  <clock>{clock_ghz}</clock>
  <silent>{"true" if silent else "false"}</silent>
  <core>{",".join(str(core) for core in active_cores)}</core>
  <numa>{",".join(str(node) for node in active_numa)}</numa>
  <worker_numa_policy>{worker_numa_policy}</worker_numa_policy>
  <worker_numa_interleave_nodes>{",".join(str(node) for node in interleave_nodes)}</worker_numa_interleave_nodes>
  <rate>{",".join(str(rate) for rate in active_rates)}</rate>
  <Mode>{",".join(str(mode) for mode in active_modes)}</Mode>
  <thread_alloc>false</thread_alloc>
  <alloc_core>987</alloc_core>
  <alloc_numa>0</alloc_numa>
  <alloc_rate>0</alloc_rate>
  <alloc_Mode>3</alloc_Mode>
</benchmark>
"""
    path.write_text(xml)


def validate_core_range(cores: Sequence[int]) -> None:
    for core in cores:
        if core < PHYSICAL_CORE_MIN or core > PHYSICAL_CORE_MAX:
            raise ValueError(
                f"Core {core} is outside the allowed physical range "
                f"{PHYSICAL_CORE_MIN}-{PHYSICAL_CORE_MAX}"
            )


def read_l3_cache_mapping() -> dict[int, int]:
    result = run_capture(["lscpu", "-e=CPU,CACHE"])
    mapping: dict[int, int] = {}
    for raw_line in result.output.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("CPU "):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        cpu = int(parts[0])
        caches = parts[1].split(":")
        mapping[cpu] = int(caches[-1])
    return mapping


def validate_chiplet_groups(groups: Sequence[Sequence[int]]) -> dict[int, int]:
    mapping = read_l3_cache_mapping()
    seen_l3: set[int] = set()
    for group in groups:
        validate_core_range(group)
        l3_ids = {mapping[core] for core in group}
        if len(l3_ids) != 1:
            raise RuntimeError(f"Expected one L3 per chiplet group, got {group} -> {l3_ids}")
        l3_id = next(iter(l3_ids))
        if l3_id in seen_l3:
            raise RuntimeError(f"Chiplet groups are not distinct: repeated L3 {l3_id}")
        seen_l3.add(l3_id)
    return mapping


def append_csv_row(path: Path, fieldnames: Sequence[str], row: Mapping[str, object]) -> None:
    ensure_directory(path.parent)
    write_header = not path.exists()
    with path.open("a", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        if write_header:
            writer.writeheader()
        writer.writerow(row)


def write_csv_rows(path: Path, fieldnames: Sequence[str], rows: Sequence[Mapping[str, object]]) -> None:
    ensure_directory(path.parent)
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)


def mean(values: Sequence[float]) -> float:
    if not values:
        raise ValueError("Cannot compute mean of empty sequence")
    return sum(values) / len(values)


def stdev(values: Sequence[float]) -> float:
    if len(values) < 2:
        return 0.0
    avg = mean(values)
    variance = sum((value - avg) ** 2 for value in values) / (len(values) - 1)
    return math.sqrt(variance)
