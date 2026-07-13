#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT_DIR = Path(__file__).resolve().parents[1]
HARNESS = ROOT_DIR / "scripts" / "run_chiplet_ycsb_harness.py"
MEMORY_BENCHMARK = ROOT_DIR / "memory_benchmark"
RESULTS_DIR = ROOT_DIR / "results"
DEFAULT_BASE_CONFIGS = [
    ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_256mib.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q10.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q21.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_cg_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_ep_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_ft_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_mg_class_b_single_thread.json",
]
CHIPLET_IDS = list(range(1, 13))
NOISE_READY_MARKER = "Running bandwidth measurement for"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("densities", nargs="*", type=int, default=list(range(1, 8)))
    parser.add_argument(
        "--config",
        dest="configs",
        action="append",
        default=None,
        help="Base config to run. Repeat to select a subset.",
    )
    return parser.parse_args()


def join_csv(values: list[int]) -> str:
    return ",".join(str(value) for value in values)


def generate_noise_xml(density: int, latency_path: Path) -> str:
    cores: list[int] = []
    numas: list[int] = []
    rates: list[int] = []
    modes: list[int] = []
    for chiplet_id in CHIPLET_IDS:
        for _ in range(density):
            cores.append(-chiplet_id)
            numas.append(0)
            rates.append(0)
            modes.append(0)
    return f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{len(cores)}</nthreads>
  <memory>128m</memory>
  <worker_memory_mb>256</worker_memory_mb>
  <output>{latency_path}</output>
  <bandwidth>true</bandwidth>
  <time>86400</time>
  <clock>2.25</clock>
  <silent>true</silent>
  <core>{join_csv(cores)}</core>
  <numa>{join_csv(numas)}</numa>
  <rate>{join_csv(rates)}</rate>
  <Mode>{join_csv(modes)}</Mode>
  <thread_alloc>false</thread_alloc>
  <alloc_core>0</alloc_core>
  <alloc_numa>0</alloc_numa>
  <alloc_rate>0</alloc_rate>
  <alloc_Mode>0</alloc_Mode>
</benchmark>
"""


def stop_noise(proc: subprocess.Popen[str] | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    deadline = time.time() + 10.0
    while time.time() < deadline:
        if proc.poll() is not None:
            break
        time.sleep(0.2)
    if proc.poll() is None:
        proc.kill()
    proc.wait()


def noise_warmup_seconds(density: int) -> float:
    env_value = os.environ.get("NOISE_WARMUP_SECONDS")
    if env_value is not None:
        return float(env_value)
    return float(density + 2)


def wait_for_noise_ready(
    proc: subprocess.Popen[str],
    stdout_log: Path,
    density: int,
) -> None:
    ready_timeout = float(os.environ.get("NOISE_READY_TIMEOUT_SECONDS", "120"))
    deadline = time.time() + ready_timeout
    marker_seen = False

    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(
                f"memory_benchmark exited early with code {proc.returncode} before becoming ready"
            )
        if stdout_log.exists():
            contents = stdout_log.read_text(encoding="utf-8", errors="replace")
            if NOISE_READY_MARKER in contents:
                marker_seen = True
                break
        time.sleep(0.2)

    if not marker_seen:
        raise RuntimeError(
            f"memory_benchmark did not reach ready marker within {ready_timeout} seconds"
        )

    warmup = noise_warmup_seconds(density)
    time.sleep(warmup)


def latest_result_dir_for_prefix(result_prefix: str) -> Path | None:
    candidates = sorted(RESULTS_DIR.glob(f"{result_prefix}_*"))
    return candidates[-1] if candidates else None


def persist_noise_artifacts(
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


def load_base_config(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def memory_benchmark_cmd(xml_path: Path) -> list[str]:
    stdbuf = shutil.which("stdbuf")
    if stdbuf:
        return [stdbuf, "-oL", "-eL", str(MEMORY_BENCHMARK), "--config", str(xml_path)]
    return [str(MEMORY_BENCHMARK), "--config", str(xml_path)]


def run_one(base_config_path: Path, density: int) -> None:
    base_config = load_base_config(base_config_path)
    result_prefix = f"{base_config['result_prefix']}_x{density}"
    base_config["result_prefix"] = result_prefix

    with tempfile.TemporaryDirectory(
        dir=ROOT_DIR,
        prefix=f"{base_config_path.stem}_x{density}.",
    ) as temp_dir_raw:
        temp_dir = Path(temp_dir_raw)
        xml_path = temp_dir / f"one_read_x{density}.xml"
        latency_path = temp_dir / f"one_read_x{density}.latency.txt"
        config_path = temp_dir / f"{base_config_path.stem}_x{density}.json"
        stdout_log = temp_dir / "memory_benchmark.stdout.log"
        stderr_log = temp_dir / "memory_benchmark.stderr.log"

        xml_path.write_text(generate_noise_xml(density, latency_path), encoding="utf-8")
        config_path.write_text(json.dumps(base_config, indent=2) + "\n", encoding="utf-8")

        noise_proc: subprocess.Popen[str] | None = None
        with stdout_log.open("w", encoding="utf-8") as stdout_fh, stderr_log.open(
            "w", encoding="utf-8"
        ) as stderr_fh:
            try:
                noise_proc = subprocess.Popen(
                    memory_benchmark_cmd(xml_path),
                    cwd=ROOT_DIR,
                    stdout=stdout_fh,
                    stderr=stderr_fh,
                    text=True,
                )
                wait_for_noise_ready(noise_proc, stdout_log, density)

                subprocess.run(
                    [os.environ.get("PYTHON_BIN", "python3"), str(HARNESS), "--config", str(config_path)],
                    cwd=ROOT_DIR,
                    check=True,
                    text=True,
                )
            finally:
                stop_noise(noise_proc)

        persist_noise_artifacts(result_prefix, xml_path, latency_path, stdout_log, stderr_log)


def main() -> int:
    args = parse_args()
    config_paths = [Path(path).resolve() for path in (args.configs or DEFAULT_BASE_CONFIGS)]
    for density in args.densities:
        if density < 1 or density > 7:
            raise SystemExit(f"density must be between 1 and 7: {density}")
        for config_path in config_paths:
            run_one(config_path, density)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
