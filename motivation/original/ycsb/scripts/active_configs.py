from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable, Sequence


ROOT_DIR = Path(__file__).resolve().parents[1]
CONFIGS_DIR = ROOT_DIR / "configs"


def _paths(*names: str) -> list[Path]:
    return [CONFIGS_DIR / name for name in names]


FIXED_WORKLOAD_CONFIG_NAMES = (
    # "chiplet_fixed_1perchiplet_256mib.json",
    # "chiplet_fixed_1perchiplet_256mib-lvalue.json",
    # "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q10.json",
    "chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q21.json",
    "chiplet_fixed_1perchiplet_npb_cg_class_b_single_thread.json",
    "gapbs_pagerank.example.json",
    "llamacpp.example.json",
    # "filebench_fileserver.example.json",
    "xsbench_bw_heavy.example.json",
    # "chiplet_fixed_1perchiplet_npb_ep_class_b_single_thread.json",
    "chiplet_fixed_1perchiplet_npb_ft_class_b_single_thread.json",
    "chiplet_fixed_1perchiplet_npb_mg_class_b_single_thread.json",
)
FIXED_WORKLOAD_CONFIGS = _paths(*FIXED_WORKLOAD_CONFIG_NAMES)


CHIPLET_DENSITY_CONFIG_NAMES = (
    "chiplet_density_1to7_256mib.json",
    "chiplet_density_1to7_256mib_memread_noise.json",
    "duckdb_tpch_sf1_single_thread_chiplet_density.json",
    "duckdb_tpch_sf1_single_thread_chiplet_density_q10.json",
    "duckdb_tpch_sf1_single_thread_chiplet_density_q21.json",
    "npb_cg_class_b_chiplet_density.json",
    "npb_cg_class_b_single_thread_chiplet_density.json",
    "npb_ep_class_b_chiplet_density.json",
    "npb_ep_class_b_single_thread_chiplet_density.json",
    "npb_ft_class_b_chiplet_density.json",
    "npb_ft_class_b_single_thread_chiplet_density.json",
    "npb_mg_class_b_chiplet_density.json",
    "npb_mg_class_b_single_thread_chiplet_density.json",
)
CHIPLET_DENSITY_CONFIGS = _paths(*CHIPLET_DENSITY_CONFIG_NAMES)


EXAMPLE_CONFIG_NAMES = (
    "custom_assignment.example.json",
    "duckdb_tpch.example.json",
    "duckdb_tpch_smoke_sf0_01.json",
    "filebench_fileserver.example.json",
    "gapbs_bfs.example.json",
    "gapbs_bfs_skewed_noise.example.json",
    "gapbs_pagerank.example.json",
    "gapbs_pagerank_skewed_noise.example.json",
    "chiplet_fixed_1perchiplet_filebench_fileserver.json",
    "chiplet_fixed_1perchiplet_llamacpp_qwen25_0_5b.json",
    "chiplet_fixed_1perchiplet_xsbench.json",
    "chiplet_fixed_1perchiplet_xsbench_bw_heavy.json",
    "llamacpp.example.json",
    "npb_instances.example.json",
    "npb_omp.example.json",
    "xsbench.example.json",
    "xsbench_bw_heavy.example.json",
)
EXAMPLE_CONFIGS = _paths(*EXAMPLE_CONFIG_NAMES)


ACTIVE_CONFIG_PATHS = FIXED_WORKLOAD_CONFIGS
ACTIVE_CONFIG_NAMES = {path.name for path in ACTIVE_CONFIG_PATHS}


def _result_prefix(config_name: str) -> str:
    payload = json.loads((CONFIGS_DIR / config_name).read_text(encoding="utf-8"))
    result_prefix = payload.get("result_prefix")
    if not isinstance(result_prefix, str) or not result_prefix:
        raise ValueError(f"config {config_name} does not define a usable result_prefix")
    return result_prefix


FIXED_WORKLOAD_RESULT_PREFIXES = tuple(_result_prefix(name) for name in FIXED_WORKLOAD_CONFIG_NAMES)
FIXED_WORKLOAD_RESULT_PATTERNS = tuple(f"{prefix}_x*" for prefix in FIXED_WORKLOAD_RESULT_PREFIXES)
CHIPLET_DENSITY_RESULT_PREFIXES = tuple(_result_prefix(name) for name in CHIPLET_DENSITY_CONFIG_NAMES)
SINGLE_CHIPLET_RESULT_PATTERN = "*_single_chiplet_smt_numa*_x*"
ACTIVE_RESULT_PREFIXES = FIXED_WORKLOAD_RESULT_PREFIXES + CHIPLET_DENSITY_RESULT_PREFIXES
SMT14_NUMA_RESULT_PREFIXES = tuple(
    f"{prefix.removesuffix('_fixed_workload_noise')}_smt14"
    for prefix in FIXED_WORKLOAD_RESULT_PREFIXES
)
SMT14_NUMA_RESULT_PATTERN = "*_smt14_numa*_x*"

BW_INEQ_EXPERIMENT_PREFIXES = (
    "bw-ineq-cc-io",
    "bw-ineq-intra-io",
    "bw-ineq-io-dimm",
    "bw-ineq-io-cxl",
)
BW_INEQ_RESULT_PATTERNS = tuple(f"{prefix}_*_numa*_x*" for prefix in BW_INEQ_EXPERIMENT_PREFIXES)


def resolve_config_paths(selected: Sequence[str] | None, default_paths: Iterable[Path]) -> list[Path]:
    if not selected:
        return [path.resolve() for path in default_paths]
    return [Path(path).resolve() for path in selected]


def is_fixed_workload_result(experiment: str) -> bool:
    return any(
        experiment == prefix or experiment.startswith(f"{prefix}_x")
        for prefix in FIXED_WORKLOAD_RESULT_PREFIXES
    )


def is_fixed_workload_noise_result(experiment: str) -> bool:
    return any(experiment.startswith(f"{prefix}_x") for prefix in FIXED_WORKLOAD_RESULT_PREFIXES)


def is_chiplet_density_result(experiment: str) -> bool:
    return experiment in CHIPLET_DENSITY_RESULT_PREFIXES


def is_single_chiplet_smt_result(experiment: str) -> bool:
    return any(
        experiment.startswith(f"{prefix}_single_chiplet_smt_numa")
        for prefix in FIXED_WORKLOAD_RESULT_PREFIXES
    )


def is_smt14_numa_result(experiment: str) -> bool:
    return any(
        experiment.startswith(f"{prefix}_numa")
        for prefix in SMT14_NUMA_RESULT_PREFIXES
    )


def is_bw_ineq_result(experiment: str) -> bool:
    return any(
        experiment.startswith(f"{prefix}_")
        and "_numa" in experiment
        and "_x" in experiment
        for prefix in BW_INEQ_EXPERIMENT_PREFIXES
    )


def is_active_result_experiment(experiment: str) -> bool:
    return (
        is_fixed_workload_result(experiment)
        or is_chiplet_density_result(experiment)
        or is_single_chiplet_smt_result(experiment)
        or is_smt14_numa_result(experiment)
        or is_bw_ineq_result(experiment)
    )
