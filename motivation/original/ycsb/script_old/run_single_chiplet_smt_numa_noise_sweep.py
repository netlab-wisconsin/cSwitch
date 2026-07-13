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
    ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_256mib-1chiplet.json",
    ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q10-1chiplet.json",
    ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q21-1chiplet.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_cg_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_ep_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_ft_class_b_single_thread.json",
    # ROOT_DIR / "configs" / "chiplet_fixed_1perchiplet_npb_mg_class_b_single_thread.json",
]
NOISE_READY_MARKER = "Running bandwidth measurement for"
TARGET_PHYSICAL_CORES = tuple(range(7))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("counts", nargs="*", type=int, default=list(range(1, 8)))
    parser.add_argument(
        "--config",
        dest="configs",
        action="append",
        default=None,
        help="Base config to run. Repeat to select a subset.",
    )
    parser.add_argument(
        "--numa",
        dest="numas",
        action="append",
        type=int,
        default=None,
        help="NUMA node for workload memory. Repeat for multiple nodes. Default: 0 and 1.",
    )
    parser.add_argument(
        "--noise-numa",
        dest="noise_numa",
        type=int,
        default=None,
        help="NUMA node for noise memory. Default: same as workload NUMA.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the resolved mappings and temp config plan without executing.",
    )
    return parser.parse_args()


def join_csv(values: list[int]) -> str:
    return ",".join(str(value) for value in values)


def discover_smt_pairs() -> dict[int, tuple[int, int]]:
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
    for core in TARGET_PHYSICAL_CORES:
        siblings = sorted(pairs.get(core, []))
        if len(siblings) != 2:
            raise RuntimeError(f"expected 2 SMT siblings for core {core}, found {siblings}")
        resolved[core] = (siblings[0], siblings[1])
    return resolved


def generate_noise_xml(noise_cpus: list[int], noise_numa: int, latency_path: Path) -> str:
    numas = [noise_numa] * len(noise_cpus)
    rates = [0] * len(noise_cpus)
    modes = [0] * len(noise_cpus)
    return f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{len(noise_cpus)}</nthreads>
  <memory>128m</memory>
  <worker_memory_mb>256</worker_memory_mb>
  <output>{latency_path}</output>
  <bandwidth>true</bandwidth>
  <time>86400</time>
  <clock>2.25</clock>
  <silent>true</silent>
  <core>{join_csv(noise_cpus)}</core>
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


def noise_warmup_seconds(count: int) -> float:
    env_value = os.environ.get("NOISE_WARMUP_SECONDS")
    if env_value is not None:
        return float(env_value)
    return float(count + 2)


def wait_for_noise_ready(proc: subprocess.Popen[str], stdout_log: Path, count: int) -> None:
    ready_timeout = float(os.environ.get("NOISE_READY_TIMEOUT_SECONDS", "120"))
    deadline = time.time() + ready_timeout

    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(
                f"memory_benchmark exited early with code {proc.returncode} before becoming ready"
            )
        if stdout_log.exists():
            contents = stdout_log.read_text(encoding="utf-8", errors="replace")
            if NOISE_READY_MARKER in contents:
                time.sleep(noise_warmup_seconds(count))
                return
        time.sleep(0.2)

    raise RuntimeError(
        f"memory_benchmark did not reach ready marker within {ready_timeout} seconds"
    )


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


def build_temp_config(
    base_config_path: Path,
    count: int,
    workload_numa: int,
    workload_cpus: list[int],
    noise_cpus: list[int],
) -> tuple[str, dict]:
    base_config = load_base_config(base_config_path)
    result_prefix = f"{base_config['result_prefix']}_single_chiplet_smt_numa{workload_numa}_x{count}"
    base_config["result_prefix"] = result_prefix
    base_config.pop("assignment_generator", None)
    base_config["assignments"] = [
        {
            "label": f"single_chiplet_smt_x{count}",
            "cores": workload_cpus,
            "numas": [workload_numa] * len(workload_cpus),
            "metadata": {
                "noise_cores": noise_cpus,
                "physical_cores": list(range(count)),
                "workload_numa": workload_numa,
            },
        }
    ]
    return result_prefix, base_config


def run_one(
    base_config_path: Path,
    count: int,
    workload_numa: int,
    noise_numa: int,
    smt_pairs: dict[int, tuple[int, int]],
    dry_run: bool,
) -> None:
    selected_cores = list(TARGET_PHYSICAL_CORES[:count])
    workload_cpus = [smt_pairs[core][0] for core in selected_cores]
    noise_cpus = [smt_pairs[core][1] for core in selected_cores]
    result_prefix, temp_config = build_temp_config(
        base_config_path=base_config_path,
        count=count,
        workload_numa=workload_numa,
        workload_cpus=workload_cpus,
        noise_cpus=noise_cpus,
    )

    print(
        f"plan config={base_config_path.name} numa={workload_numa} count={count} "
        f"workload_cpus={workload_cpus} noise_cpus={noise_cpus} noise_numa={noise_numa}"
    )
    if dry_run:
        return

    with tempfile.TemporaryDirectory(
        dir=ROOT_DIR,
        prefix=f"{base_config_path.stem}.numa{workload_numa}.x{count}.",
    ) as temp_dir_raw:
        temp_dir = Path(temp_dir_raw)
        xml_path = temp_dir / f"single_chiplet_smt_noise_x{count}.xml"
        latency_path = temp_dir / f"single_chiplet_smt_noise_x{count}.latency.txt"
        config_path = temp_dir / f"{base_config_path.stem}.numa{workload_numa}.x{count}.json"
        stdout_log = temp_dir / "memory_benchmark.stdout.log"
        stderr_log = temp_dir / "memory_benchmark.stderr.log"

        xml_path.write_text(generate_noise_xml(noise_cpus, noise_numa, latency_path), encoding="utf-8")
        config_path.write_text(json.dumps(temp_config, indent=2) + "\n", encoding="utf-8")

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
                wait_for_noise_ready(noise_proc, stdout_log, count)

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
    numas = args.numas or [0, 1]
    config_paths = [Path(path).resolve() for path in (args.configs or DEFAULT_BASE_CONFIGS)]
    smt_pairs = discover_smt_pairs()

    for count in args.counts:
        if count < 1 or count > len(TARGET_PHYSICAL_CORES):
            raise SystemExit(f"count must be between 1 and {len(TARGET_PHYSICAL_CORES)}: {count}")
        for workload_numa in numas:
            if workload_numa not in (0, 1):
                raise SystemExit(f"unsupported workload NUMA node: {workload_numa}")
            noise_numa = workload_numa if args.noise_numa is None else args.noise_numa
            if noise_numa not in (0, 1):
                raise SystemExit(f"unsupported noise NUMA node: {noise_numa}")
            for config_path in config_paths:
                run_one(
                    base_config_path=config_path,
                    count=count,
                    workload_numa=workload_numa,
                    noise_numa=noise_numa,
                    smt_pairs=smt_pairs,
                    dry_run=args.dry_run,
                )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
