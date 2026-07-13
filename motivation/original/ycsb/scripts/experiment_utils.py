from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Sequence


ROOT_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT_DIR / "results"
HARNESS = ROOT_DIR / "scripts" / "run_chiplet_ycsb_harness.py"
MEMORY_BENCHMARK = ROOT_DIR / "memory_benchmark"
NOISE_READY_MARKER = "Running bandwidth measurement for"


def join_csv(values: Sequence[int]) -> str:
    return ",".join(str(value) for value in values)


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path: Path, payload: dict) -> None:
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def latest_result_dir_for_prefix(result_prefix: str) -> Path | None:
    candidates = sorted(RESULTS_DIR.glob(f"{result_prefix}_*"))
    return candidates[-1] if candidates else None


def stop_process(proc: subprocess.Popen[str] | None, grace_seconds: float = 10.0) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    deadline = time.time() + grace_seconds
    while time.time() < deadline:
        if proc.poll() is not None:
            break
        time.sleep(0.2)
    if proc.poll() is None:
        proc.kill()
    proc.wait()


def memory_benchmark_cmd(binary: Path, xml_path: Path) -> list[str]:
    stdbuf = shutil.which("stdbuf")
    if stdbuf:
        return [stdbuf, "-oL", "-eL", str(binary), "--config", str(xml_path)]
    return [str(binary), "--config", str(xml_path)]


def wait_for_process_ready(
    proc: subprocess.Popen[str],
    stdout_log: Path,
    marker: str,
    warmup_seconds: float,
    ready_timeout_seconds: float = 120.0,
) -> None:
    deadline = time.time() + ready_timeout_seconds
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(
                f"process exited early with code {proc.returncode} before marker '{marker}' appeared"
            )
        if stdout_log.exists():
            contents = stdout_log.read_text(encoding="utf-8", errors="replace")
            if marker in contents:
                time.sleep(warmup_seconds)
                return
        time.sleep(0.2)
    raise RuntimeError(f"process did not reach ready marker within {ready_timeout_seconds} seconds")


def persist_external_noise_artifacts(
    result_prefix: str,
    xml_path: Path,
    latency_path: Path,
    stdout_log: Path,
    stderr_log: Path,
) -> None:
    result_dir = latest_result_dir_for_prefix(result_prefix)
    if result_dir is None or not result_dir.is_dir():
        print(f"warning: could not find result dir for prefix {result_prefix}", file=sys.stderr)
        return

    noise_dir = result_dir / "external_noise"
    noise_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(xml_path, noise_dir / "memory_benchmark.xml")
    shutil.copy2(stdout_log, noise_dir / "memory_benchmark.stdout.log")
    shutil.copy2(stderr_log, noise_dir / "memory_benchmark.stderr.log")
    if latency_path.exists():
        shutil.copy2(latency_path, noise_dir / "memory_benchmark.latency.txt")


def generate_memory_benchmark_xml(
    *,
    cores: Sequence[int],
    numas: Sequence[int],
    rates: Sequence[int],
    modes: Sequence[int],
    latency_path: Path,
    worker_memory_mb: int = 256,
    memory: str = "128m",
    bandwidth: bool = True,
    duration_seconds: int = 86400,
    clock_ghz: float = 2.25,
    silent: bool = True,
    thread_alloc: bool = False,
    alloc_core: int = 0,
    alloc_numa: int = 0,
    alloc_rate: int = 0,
    alloc_mode: int = 0,
) -> str:
    return f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{len(cores)}</nthreads>
  <memory>{memory}</memory>
  <worker_memory_mb>{worker_memory_mb}</worker_memory_mb>
  <output>{latency_path}</output>
  <bandwidth>{str(bandwidth).lower()}</bandwidth>
  <time>{duration_seconds}</time>
  <clock>{clock_ghz}</clock>
  <silent>{str(silent).lower()}</silent>
  <core>{join_csv(cores)}</core>
  <numa>{join_csv(numas)}</numa>
  <rate>{join_csv(rates)}</rate>
  <Mode>{join_csv(modes)}</Mode>
  <thread_alloc>{str(thread_alloc).lower()}</thread_alloc>
  <alloc_core>{alloc_core}</alloc_core>
  <alloc_numa>{alloc_numa}</alloc_numa>
  <alloc_rate>{alloc_rate}</alloc_rate>
  <alloc_Mode>{alloc_mode}</alloc_Mode>
</benchmark>
"""


def discover_smt_pairs(target_cores: Sequence[int]) -> dict[int, tuple[int, int]]:
    output = subprocess.check_output(
        ["lscpu", "-e=CPU,CORE,NODE"],
        cwd=ROOT_DIR,
        text=True,
    )
    pairs: dict[int, list[int]] = {}
    for line in output.splitlines()[1:]:
        parts = line.split()
        if len(parts) != 3:
            continue
        cpu, core, _node = map(int, parts)
        pairs.setdefault(core, []).append(cpu)

    resolved: dict[int, tuple[int, int]] = {}
    for core in target_cores:
        siblings = sorted(pairs.get(core, []))
        if len(siblings) != 2:
            raise RuntimeError(f"expected 2 SMT siblings for core {core}, found {siblings}")
        resolved[core] = (siblings[0], siblings[1])
    return resolved
