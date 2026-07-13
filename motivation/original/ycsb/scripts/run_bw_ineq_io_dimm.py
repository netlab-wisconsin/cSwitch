#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path

from run_bw_ineq_memload_sweep import ExperimentPreset, main_with_preset


CONFIGS_DIR = Path(__file__).resolve().parents[1] / "configs"

DEFAULT_CONFIGS = (
    CONFIGS_DIR / "llamacpp.example.json",
    CONFIGS_DIR / "duckdb_tpch_sf1_single_thread_chiplet_density_q21.json",
)

DEFAULT_NOISE_COUNTS = tuple([0] + list(range(11, 78, 11)))

PRESET = ExperimentPreset(
    script_name="run_bw_ineq_io_dimm.py",
    description=(
        "Run bw-ineq-io-dimm: duplicate of the intra-io setup with a separate "
        "prefix so parameters can diverge later."
    ),
    experiment_prefix="bw-ineq-io-dimm",
    default_configs=DEFAULT_CONFIGS,
    default_noise_counts=DEFAULT_NOISE_COUNTS,
    workload_cpu_selector="0",
    noise_cpu_selector="7-83,91-167",
    workload_numa=0,
    noise_numa=0,
    noise_mode=0,
    noise_rate=500,
    df_resource_family="CS",
    df_resource_ids=tuple(range(12)),
)


def main() -> int:
    return main_with_preset(None, PRESET)


if __name__ == "__main__":
    raise SystemExit(main())
