#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import subprocess
import tempfile
from pathlib import Path

from active_configs import FIXED_WORKLOAD_CONFIGS, resolve_config_paths
from experiment_utils import (
    HARNESS,
    MEMORY_BENCHMARK,
    NOISE_READY_MARKER,
    ROOT_DIR,
    generate_memory_benchmark_xml,
    load_json,
    memory_benchmark_cmd,
    persist_external_noise_artifacts,
    stop_process,
    wait_for_process_ready,
    write_json,
)


DEFAULT_BASE_CONFIGS = FIXED_WORKLOAD_CONFIGS
CHIPLET_IDS = list(range(1, 13))


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
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the resolved plan without launching memory_benchmark or the harness.",
    )
    return parser.parse_args()


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
    return generate_memory_benchmark_xml(
        cores=cores,
        numas=numas,
        rates=rates,
        modes=modes,
        latency_path=latency_path,
    )


def noise_warmup_seconds(density: int) -> float:
    env_value = os.environ.get("NOISE_WARMUP_SECONDS")
    if env_value is not None:
        return float(env_value)
    return float(density + 2)


def run_one(base_config_path: Path, density: int, dry_run: bool) -> None:
    base_config = load_json(base_config_path)
    result_prefix = f"{base_config['result_prefix']}_x{density}"
    base_config["result_prefix"] = result_prefix
    print(
        f"plan config={base_config_path.name} density={density} "
        f"chiplets={len(CHIPLET_IDS)} noise_threads={len(CHIPLET_IDS) * density} "
        f"result_prefix={result_prefix}"
    )
    if dry_run:
        return

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
        write_json(config_path, base_config)

        noise_proc: subprocess.Popen[str] | None = None
        with stdout_log.open("w", encoding="utf-8") as stdout_fh, stderr_log.open(
            "w", encoding="utf-8"
        ) as stderr_fh:
            try:
                noise_proc = subprocess.Popen(
                    memory_benchmark_cmd(MEMORY_BENCHMARK, xml_path),
                    cwd=ROOT_DIR,
                    stdout=stdout_fh,
                    stderr=stderr_fh,
                    text=True,
                )
                wait_for_process_ready(
                    noise_proc,
                    stdout_log,
                    NOISE_READY_MARKER,
                    warmup_seconds=noise_warmup_seconds(density),
                    ready_timeout_seconds=float(os.environ.get("NOISE_READY_TIMEOUT_SECONDS", "120")),
                )

                subprocess.run(
                    [os.environ.get("PYTHON_BIN", "python3"), str(HARNESS), "--config", str(config_path)],
                    cwd=ROOT_DIR,
                    check=True,
                    text=True,
                )
            finally:
                stop_process(noise_proc)

        persist_external_noise_artifacts(result_prefix, xml_path, latency_path, stdout_log, stderr_log)


def main() -> int:
    args = parse_args()
    config_paths = resolve_config_paths(args.configs, DEFAULT_BASE_CONFIGS)
    for density in args.densities:
        if density < 1 or density > 7:
            raise SystemExit(f"density must be between 1 and 7: {density}")
        for config_path in config_paths:
            run_one(config_path, density, args.dry_run)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
