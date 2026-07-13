#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path

from run_bw_ineq_memload_sweep import ExperimentPreset, main_with_preset


CONFIGS_DIR = Path(__file__).resolve().parents[1] / "configs"

DEFAULT_CONFIGS = (
    # CONFIGS_DIR / "llamacpp.example.json",
    # CONFIGS_DIR / "duckdb_tpch_sf1_single_thread_chiplet_density_q21.json",
    CONFIGS_DIR / "chiplet_fixed_1perchiplet_256mib-lvalue-cc-io-small.json",
)

DEFAULT_NOISE_COUNTS = tuple([0] + list(range(11, 78, 11)))
# DEFAULT_NOISE_COUNTS = tuple([0])

PRESET = ExperimentPreset(
    script_name="run_bw_ineq_io_cxl.py",
    description=(
        "Run bw-ineq-io-cxl: duplicate of the intra-io setup but bind workload "
        "and noise memory to NUMA 1."
    ),
    experiment_prefix="bw-ineq-io-cxl",
    default_configs=DEFAULT_CONFIGS,
    default_noise_counts=DEFAULT_NOISE_COUNTS,
    workload_cpu_selector="0",
    noise_cpu_selector="7-83,91-167",
    workload_numa=1,
    noise_numa=1,
    noise_mode=0,
    noise_rate=500,
    df_resource_family="IOM",
    df_resource_ids=(0, 1, 2, 3),
)


def main() -> int:
    return main_with_preset(None, PRESET)


if __name__ == "__main__":
    raise SystemExit(main())
