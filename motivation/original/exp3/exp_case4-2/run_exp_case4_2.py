#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import itertools
import os
import sys
import time
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
ROOT_DIR = SCRIPT_DIR.parent
if str(ROOT_DIR) not in sys.path:
    sys.path.insert(0, str(ROOT_DIR))

from experiment_utils import (  # noqa: E402
    build_env,
    ensure_directory,
    format_command,
    mean,
    parse_npb_metrics,
    require_path,
    run_capture,
    start_background,
    stdev,
    stop_background,
    write_csv_rows,
)
from exp_case4_common import (  # noqa: E402
    CALIBRATION_DURATION_SECONDS,
    CALIBRATION_RATE_CANDIDATES,
    CCM_MAPPING_PATH,
    CANONICAL_COUNTS,
    CHIPLET_ORDER,
    COUNT_PATTERN,
    DEFAULT_ROTATIONS,
    DEFAULT_SKEWNESS_LEVELS,
    EEVDF_WORKLOAD_MASK,
    EMULATED_EEVDF_BINARY,
    MEMORY_BENCHMARK,
    NPB_EP_B,
    OUR_SCHEDULER_BINARY,
    SCHEDULER_POLICY_BINARIES,
    build_scheduler_managed_argv,
    build_noise_rates_by_chiplet,
    format_combo_counts,
    format_noise_rate_map,
    format_skew_map,
    format_workload_core_map,
    is_scheduler_managed_policy,
    load_best_combo_summary,
    omp_places_from_cores,
    optimal_counts_for_skew,
    parse_noise_bandwidth_by_chiplet,
    rotated_count_vectors,
    rotation_ids_for_skew,
    rotation_skew_map,
    run_case4_calibration,
    same_chiplet_combinations,
    taskset_mask_from_cores,
    validate_case4_topology,
    wait_for_noise_ready,
    workload_core_map_from_counts,
    workload_cores_from_counts,
    write_case4_noise_config,
)


BENCHMARK_CHOICES = ['ep', 'skiplist-rw50', 'rwlock-rw50', 'wis-lock1', 'wis-page_fault1']
BENCHMARK_ORDER = {name: index for index, name in enumerate(BENCHMARK_CHOICES)}
POLICY_ORDER = {
    'eevdf': 0,
    'eevdf-emulated': 1,
    'la-default': 2,
    'optimal': 3,
    'optimal-pinned': 4,
    'same-chiplet': 5,
}
NODE_REPLICATION_ROOT = ROOT_DIR / 'node-replication'
NODE_REPLICATION_PACKAGE = NODE_REPLICATION_ROOT / 'node-replication'
NODE_REPLICATION_MANIFEST = NODE_REPLICATION_PACKAGE / 'Cargo.toml'
WILL_IT_SCALE_ROOT = ROOT_DIR / 'will-it-scale'
NR_SKIPLIST_DEFAULT_INITIAL_CAPACITY = 1 << 22
NR_SKIPLIST_DEFAULT_KEY_SPACE = 5_000_000
NR_SKIPLIST_DEFAULT_OPS = 2_500_000
NR_SKIPLIST_DEFAULT_LOG_COUNTS = '1'
NR_RWLOCK_DEFAULT_INITIAL_CAPACITY = 1 << 22
NR_RWLOCK_DEFAULT_KEY_SPACE = 5_000_000
NR_RWLOCK_DEFAULT_OPS = 2_500_000
WIS_DEFAULT_DURATION_SECONDS = 5
CANONICAL_COUNT_VECTORS = (
    (4, 4, 1, 0),
    (4, 3, 2, 0),
    (4, 3, 1, 1),
    (4, 2, 2, 1),
    (3, 3, 3, 0),
    (3, 3, 2, 1),
    (3, 2, 2, 2),
)
RESULT_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'rotation',
    'repeat',
    'combo_id',
    'benchmark_variant',
    'combo_counts',
    'skew_map',
    'workload_core_map',
    'noise_rate_map',
    'ccd0_noise_bandwidth_mbs',
    'ccd1_noise_bandwidth_mbs',
    'ccd3_noise_bandwidth_mbs',
    'ccd4_noise_bandwidth_mbs',
    'total_noise_bandwidth_mbs',
    'primary_metric_name',
    'primary_metric_value',
    'npb_time_s',
    'mops_total',
    'verification',
    'slowdown_vs_skew0',
]
SUMMARY_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'primary_metric_name',
    'runs',
    'successful_runs',
    'verification_ok_ratio',
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_npb_time_s',
    'stdev_npb_time_s',
    'mean_slowdown_vs_skew0',
    'mean_mops_total',
    'stdev_mops_total',
    'mean_ccd0_noise_bandwidth_mbs',
    'mean_ccd1_noise_bandwidth_mbs',
    'mean_ccd3_noise_bandwidth_mbs',
    'mean_ccd4_noise_bandwidth_mbs',
    'mean_total_noise_bandwidth_mbs',
]
COMBO_SUMMARY_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'rotation',
    'combo_id',
    'benchmark_variant',
    'combo_counts',
    'workload_core_map',
    'runs',
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_npb_time_s',
    'stdev_npb_time_s',
    'mean_mops_total',
    'stdev_mops_total',
    'mean_total_noise_bandwidth_mbs',
]
BEST_COMBO_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'rotation',
    'combo_id',
    'benchmark_variant',
    'combo_counts',
    'workload_core_map',
    'runs',
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_npb_time_s',
    'stdev_npb_time_s',
    'mean_mops_total',
    'stdev_mops_total',
    'mean_total_noise_bandwidth_mbs',
]
CALIBRATION_FIELDNAMES = [
    'target_pct',
    'rate',
    'measured_total_noise_bandwidth_mbs',
    'measured_ccd0_noise_bandwidth_mbs',
    'measured_ccd1_noise_bandwidth_mbs',
    'measured_ccd3_noise_bandwidth_mbs',
    'measured_ccd4_noise_bandwidth_mbs',
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='Run exp_case4-2 skew experiment for EP and node-replication skiplist.')
    parser.add_argument(
        '--benchmarks',
        nargs='+',
        default=list(BENCHMARK_CHOICES),
        choices=BENCHMARK_CHOICES,
        help='Benchmark subset to run.',
    )
    parser.add_argument(
        '--policies',
        nargs='+',
        default=list(POLICY_ORDER),
        choices=list(POLICY_ORDER),
        help='Policy subset to run.',
    )
    parser.add_argument(
        '--skewnesses',
        nargs='+',
        type=int,
        default=DEFAULT_SKEWNESS_LEVELS,
        choices=DEFAULT_SKEWNESS_LEVELS,
        help='Skewness levels to run.',
    )
    parser.add_argument(
        '--rotations',
        nargs='+',
        type=int,
        default=DEFAULT_ROTATIONS,
        choices=DEFAULT_ROTATIONS,
        help='Rotation ids to run for non-zero skewness.',
    )
    parser.add_argument(
        '--fixed-rotation',
        type=int,
        choices=DEFAULT_ROTATIONS,
        help='If set, use this single rotation for every skewness level, including skewness 0.',
    )
    parser.add_argument('--repeats', type=int, default=1, help='Outer repeats per condition.')
    parser.add_argument('--settle-seconds', type=float, default=2.0, help='Noise settle time before workload launch.')
    parser.add_argument('--cooldown-seconds', type=float, default=2.0, help='Cooldown after each run.')
    parser.add_argument(
        '--noise-time-seconds',
        type=int,
        default=3600,
        help='Long-running timer passed to memory_benchmark before termination.',
    )
    parser.add_argument(
        '--calibration-duration-seconds',
        type=int,
        default=CALIBRATION_DURATION_SECONDS,
        help='Duration used for short calibration runs.',
    )
    parser.add_argument(
        '--nr-benchmark-duration-seconds',
        type=int,
        default=5,
        help='Measurement duration passed to the node-replication skiplist benchmark.',
    )
    parser.add_argument(
        '--wis-duration-seconds',
        type=int,
        default=WIS_DEFAULT_DURATION_SECONDS,
        help='Measurement duration passed to will-it-scale workloads.',
    )
    parser.add_argument(
        '--node-replication-timeout-seconds',
        type=float,
        default=45.0,
        help='Kill and skip a node-replication run if it exceeds this timeout.',
    )
    parser.add_argument(
        '--nr-skiplist-initial-capacity',
        type=int,
        default=NR_SKIPLIST_DEFAULT_INITIAL_CAPACITY,
        help='Initial skiplist population used by the node-replication benchmark.',
    )
    parser.add_argument(
        '--nr-skiplist-key-space',
        type=int,
        default=NR_SKIPLIST_DEFAULT_KEY_SPACE,
        help='Key space used when generating skiplist operations.',
    )
    parser.add_argument(
        '--nr-skiplist-ops',
        type=int,
        default=NR_SKIPLIST_DEFAULT_OPS,
        help='Operation count used to pre-generate skiplist benchmark traces.',
    )
    parser.add_argument(
        '--nr-skiplist-log-counts',
        default=NR_SKIPLIST_DEFAULT_LOG_COUNTS,
        help='Comma-separated node-replication log counts to benchmark. Default keeps a single fixed variant.',
    )
    parser.add_argument(
        '--nr-rwlock-initial-capacity',
        type=int,
        default=NR_RWLOCK_DEFAULT_INITIAL_CAPACITY,
        help='Initial hashmap population used by the rwlock benchmark.',
    )
    parser.add_argument(
        '--nr-rwlock-key-space',
        type=int,
        default=NR_RWLOCK_DEFAULT_KEY_SPACE,
        help='Key space used when generating rwlock benchmark operations.',
    )
    parser.add_argument(
        '--nr-rwlock-ops',
        type=int,
        default=NR_RWLOCK_DEFAULT_OPS,
        help='Operation count used to pre-generate rwlock benchmark traces.',
    )
    parser.add_argument(
        '--noise-memory-nodes',
        default='0',
        help='Comma-separated NUMA nodes used by the synthetic noise workload. Default uses NUMA 0 only.',
    )
    parser.add_argument(
        '--optimal-combo-summary',
        type=Path,
        help='Optional combo-summary CSV used to choose the best optimal/optimal-pinned count vector per benchmark/skewness/rotation.',
    )
    parser.add_argument(
        '--sweep-canonical-count-vectors',
        action='store_true',
        help='For optimal policies, sweep the seven canonical 9-thread count vectors instead of only 4,4,1,0.',
    )
    parser.add_argument(
        '--sweep-physical-assignments',
        action='store_true',
        help='When sweeping canonical count vectors, also enumerate unique physical chiplet assignments.',
    )
    parser.add_argument(
        '--optimal-strict-pinning',
        action='store_true',
        help='Pin optimal EP threads to the selected cores in ascending per-chiplet order.',
    )
    parser.add_argument(
        '--same-chiplet-single-combo',
        action='store_true',
        help='Use only the rotated 4,4,1,0 placement for same-chiplet instead of sweeping all 12 permutations.',
    )
    parser.add_argument(
        '--output-dir',
        type=Path,
        default=SCRIPT_DIR / 'results' / datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'),
        help='Directory for CSVs, configs, logs, and metadata.',
    )
    parser.add_argument('--skip-topology-check', action='store_true', help='Skip topology validation.')
    parser.add_argument('--dry-run', action='store_true', help='Print commands without executing them.')
    return parser.parse_args()


def count_vector_combo_id(count_vector: tuple[int, int, int, int]) -> str:
    return 'vec_' + '_'.join(str(count) for count in count_vector)


def parse_node_list(text: str) -> list[int]:
    nodes = [int(field.strip()) for field in text.split(',') if field.strip()]
    if not nodes:
        raise RuntimeError('At least one NUMA node must be provided.')
    return nodes


def counts_from_sorted_vector(
    skew_map: dict[str, int],
    count_vector: tuple[int, int, int, int],
) -> dict[str, int]:
    ordered_chiplets = sorted(CHIPLET_ORDER, key=lambda chiplet: (skew_map[chiplet], chiplet))
    counts_by_chiplet = {chiplet: 0 for chiplet in CHIPLET_ORDER}
    for chiplet, count in zip(ordered_chiplets, count_vector, strict=True):
        counts_by_chiplet[chiplet] = count
    return counts_by_chiplet


def counts_from_physical_vector(
    count_vector: tuple[int, int, int, int],
) -> dict[str, int]:
    return {
        chiplet: count
        for chiplet, count in zip(CHIPLET_ORDER, count_vector, strict=True)
    }


def physical_assignment_combos(
    count_vector: tuple[int, int, int, int],
) -> list[tuple[str, dict[str, int]]]:
    seen: set[tuple[int, int, int, int]] = set()
    combos: list[tuple[str, dict[str, int]]] = []
    base_id = count_vector_combo_id(count_vector)
    for permuted in itertools.permutations(count_vector):
        if permuted in seen:
            continue
        seen.add(permuted)
        combo_id = f"{base_id}__p_{'_'.join(str(value) for value in permuted)}"
        combos.append((combo_id, counts_from_physical_vector(permuted)))
    combos.sort(key=lambda item: tuple(item[1][chiplet] for chiplet in CHIPLET_ORDER))
    return combos


def selected_rotations_for_skew(
    skewness: int,
    rotations: list[int],
    fixed_rotation: int | None,
) -> list[int]:
    if fixed_rotation is not None:
        return [fixed_rotation]
    return rotation_ids_for_skew(skewness, rotations)


def build_ep_launch(
    policy: str,
    counts_by_chiplet: dict[str, int] | None,
    *,
    optimal_strict_pinning: bool,
    output_dir: Path,
    run_id: str,
) -> tuple[list[str], dict[str, str], str, str]:
    base_argv = [str(NPB_EP_B)]
    removals = ['OMP_PLACES', 'OMP_PROC_BIND', 'GOMP_CPU_AFFINITY', 'KMP_AFFINITY']
    if policy in {'eevdf', 'eevdf-emulated', 'la-default'}:
        env = build_env(updates={'OMP_NUM_THREADS': '9'}, removals=removals)
        inner_argv = ['taskset', '-c', EEVDF_WORKLOAD_MASK, *base_argv]
        if policy == 'eevdf':
            return (
                inner_argv,
                env,
                f'mask:{EEVDF_WORKLOAD_MASK}',
                'dynamic',
            )
        return (
            build_scheduler_managed_argv(
                policy,
                inner_argv=inner_argv,
                workload_env={'OMP_NUM_THREADS': '9'},
                output_dir=output_dir,
                run_id=run_id,
            ),
            env,
            f'mask:{EEVDF_WORKLOAD_MASK}',
            'dynamic',
        )

    if counts_by_chiplet is None:
        raise RuntimeError(f'counts_by_chiplet is required for policy={policy}')

    selected_cores = workload_cores_from_counts(counts_by_chiplet)
    env_updates = {'OMP_NUM_THREADS': '9'}
    if policy != 'optimal' or optimal_strict_pinning:
        env_updates.update(
            {
                'OMP_PROC_BIND': 'true',
                'OMP_PLACES': omp_places_from_cores(selected_cores),
            }
        )
    env = build_env(updates=env_updates, removals=removals)
    return (
        ['taskset', '-c', taskset_mask_from_cores(selected_cores), *base_argv],
        env,
        format_workload_core_map(workload_core_map_from_counts(counts_by_chiplet)),
        format_combo_counts(counts_by_chiplet),
    )


def build_skiplist_launch(
    policy: str,
    counts_by_chiplet: dict[str, int] | None,
    nr_binary: Path,
    benchmark_duration_seconds: int,
    skiplist_initial_capacity: int,
    skiplist_key_space: int,
    skiplist_ops: int,
    skiplist_log_counts: str,
    output_dir: Path,
    run_id: str,
) -> tuple[list[str], dict[str, str], str, str]:
    removals = [
        'BENCH_UTILS_ALLOWED_CPUS',
        'NR_ALLOWED_CPUS',
        'BENCH_UTILS_DISABLE_PINNING',
        'NR_DISABLE_PINNING',
        'BENCH_UTILS_SKIP_DVFS',
        'NR_SKIP_DVFS',
        'BENCH_UTILS_CNR_CSV_PATH',
        'NR_SKIPLIST_WRITE_RATIOS',
        'NR_SKIPLIST_THREAD_COUNTS',
        'NR_SKIPLIST_LOG_COUNTS',
        'NR_SKIPLIST_INITIAL_CAPACITY',
        'NR_SKIPLIST_KEY_SPACE',
        'NR_SKIPLIST_OPS',
        'NR_BENCH_DURATION_SECONDS',
        'RUST_LOG',
    ]
    env_updates = {
        'BENCH_UTILS_SKIP_DVFS': '1',
        'NR_SKIP_DVFS': '1',
        'RUST_LOG': 'error',
        'NR_SKIPLIST_WRITE_RATIOS': '50',
        'NR_SKIPLIST_THREAD_COUNTS': '9',
        'NR_SKIPLIST_LOG_COUNTS': skiplist_log_counts,
        'NR_SKIPLIST_INITIAL_CAPACITY': str(skiplist_initial_capacity),
        'NR_SKIPLIST_KEY_SPACE': str(skiplist_key_space),
        'NR_SKIPLIST_OPS': str(skiplist_ops),
        'NR_BENCH_DURATION_SECONDS': str(benchmark_duration_seconds),
    }

    if policy in {'eevdf', 'eevdf-emulated', 'la-default'}:
        mask = EEVDF_WORKLOAD_MASK
        env_updates.update(
            {
                'BENCH_UTILS_ALLOWED_CPUS': mask,
                'NR_ALLOWED_CPUS': mask,
                'BENCH_UTILS_DISABLE_PINNING': '1',
                'NR_DISABLE_PINNING': '1',
            }
        )
        env = build_env(updates=env_updates, removals=removals)
        inner_argv = ['taskset', '-c', mask, str(nr_binary)]
        if policy == 'eevdf':
            return (
                inner_argv,
                env,
                f'mask:{mask}',
                'dynamic',
            )
        return (
            build_scheduler_managed_argv(
                policy,
                inner_argv=inner_argv,
                workload_env=dict(env_updates),
                output_dir=output_dir,
                run_id=run_id,
            ),
            env,
            f'mask:{mask}',
            'dynamic',
        )

    if counts_by_chiplet is None:
        raise RuntimeError(f'counts_by_chiplet is required for policy={policy}')

    selected_cores = workload_cores_from_counts(counts_by_chiplet)
    mask = taskset_mask_from_cores(selected_cores)
    env_updates.update(
        {
            'BENCH_UTILS_ALLOWED_CPUS': mask,
            'NR_ALLOWED_CPUS': mask,
        }
    )
    if policy == 'optimal':
        env_updates.update(
            {
                'BENCH_UTILS_DISABLE_PINNING': '1',
                'NR_DISABLE_PINNING': '1',
            }
        )
    env = build_env(updates=env_updates, removals=removals)
    return (
        ['taskset', '-c', mask, str(nr_binary)],
        env,
        format_workload_core_map(workload_core_map_from_counts(counts_by_chiplet)),
        format_combo_counts(counts_by_chiplet),
    )


def build_rwlock_launch(
    policy: str,
    counts_by_chiplet: dict[str, int] | None,
    nr_binary: Path,
    benchmark_duration_seconds: int,
    initial_capacity: int,
    key_space: int,
    ops: int,
    output_dir: Path,
    run_id: str,
) -> tuple[list[str], dict[str, str], str, str]:
    removals = [
        'BENCH_UTILS_ALLOWED_CPUS',
        'NR_ALLOWED_CPUS',
        'BENCH_UTILS_DISABLE_PINNING',
        'NR_DISABLE_PINNING',
        'BENCH_UTILS_SKIP_DVFS',
        'NR_SKIP_DVFS',
        'BENCH_UTILS_CSV_PATH',
        'NR_HASHMAP_VARIANTS',
        'NR_HASHMAP_WRITE_RATIOS',
        'NR_HASHMAP_THREAD_COUNTS',
        'NR_HASHMAP_INITIAL_CAPACITY',
        'NR_HASHMAP_KEY_SPACE',
        'NR_HASHMAP_OPS',
        'NR_HASHMAP_BENCH_DURATION_SECONDS',
        'NR_BENCH_DURATION_SECONDS',
        'RUST_LOG',
    ]
    env_updates = {
        'BENCH_UTILS_SKIP_DVFS': '1',
        'NR_SKIP_DVFS': '1',
        'RUST_LOG': 'error',
        'NR_HASHMAP_VARIANTS': 'std',
        'NR_HASHMAP_WRITE_RATIOS': '50',
        'NR_HASHMAP_THREAD_COUNTS': '9',
        'NR_HASHMAP_INITIAL_CAPACITY': str(initial_capacity),
        'NR_HASHMAP_KEY_SPACE': str(key_space),
        'NR_HASHMAP_OPS': str(ops),
        'NR_HASHMAP_BENCH_DURATION_SECONDS': str(benchmark_duration_seconds),
        'NR_BENCH_DURATION_SECONDS': str(benchmark_duration_seconds),
    }

    if policy in {'eevdf', 'eevdf-emulated', 'la-default'}:
        mask = EEVDF_WORKLOAD_MASK
        env_updates.update(
            {
                'BENCH_UTILS_ALLOWED_CPUS': mask,
                'NR_ALLOWED_CPUS': mask,
                'BENCH_UTILS_DISABLE_PINNING': '1',
                'NR_DISABLE_PINNING': '1',
            }
        )
        env = build_env(updates=env_updates, removals=removals)
        inner_argv = ['taskset', '-c', mask, str(nr_binary)]
        if policy == 'eevdf':
            return (
                inner_argv,
                env,
                f'mask:{mask}',
                'dynamic',
            )
        return (
            build_scheduler_managed_argv(
                policy,
                inner_argv=inner_argv,
                workload_env=dict(env_updates),
                output_dir=output_dir,
                run_id=run_id,
            ),
            env,
            f'mask:{mask}',
            'dynamic',
        )

    if counts_by_chiplet is None:
        raise RuntimeError(f'counts_by_chiplet is required for policy={policy}')

    selected_cores = workload_cores_from_counts(counts_by_chiplet)
    mask = taskset_mask_from_cores(selected_cores)
    env_updates.update(
        {
            'BENCH_UTILS_ALLOWED_CPUS': mask,
            'NR_ALLOWED_CPUS': mask,
        }
    )
    if policy == 'optimal':
        env_updates.update(
            {
                'BENCH_UTILS_DISABLE_PINNING': '1',
                'NR_DISABLE_PINNING': '1',
            }
        )
    env = build_env(updates=env_updates, removals=removals)
    return (
        ['taskset', '-c', mask, str(nr_binary)],
        env,
        format_workload_core_map(workload_core_map_from_counts(counts_by_chiplet)),
        format_combo_counts(counts_by_chiplet),
    )


def build_wis_launch(
    benchmark: str,
    policy: str,
    counts_by_chiplet: dict[str, int] | None,
    wis_binary: Path,
    duration_seconds: int,
    output_dir: Path,
    run_id: str,
) -> tuple[list[str], dict[str, str], str, str]:
    wis_args = [str(wis_binary), '-t', '9', '-s', str(duration_seconds)]

    if policy in {'eevdf', 'eevdf-emulated', 'la-default'}:
        mask = EEVDF_WORKLOAD_MASK
        inner_argv = ['taskset', '-c', mask, *wis_args, '-n']
        if policy == 'eevdf':
            return (
                inner_argv,
                {},
                f'mask:{mask}',
                'dynamic',
            )
        return (
            build_scheduler_managed_argv(
                policy,
                inner_argv=inner_argv,
                workload_env={},
                output_dir=output_dir,
                run_id=run_id,
            ),
            {},
            f'mask:{mask}',
            'dynamic',
        )

    if counts_by_chiplet is None:
        raise RuntimeError(f'counts_by_chiplet is required for policy={policy}')

    selected_cores = workload_cores_from_counts(counts_by_chiplet)
    mask = taskset_mask_from_cores(selected_cores)
    argv = ['taskset', '-c', mask, *wis_args]
    if policy == 'optimal':
        argv.append('-n')
    return (
        argv,
        {},
        format_workload_core_map(workload_core_map_from_counts(counts_by_chiplet)),
        format_combo_counts(counts_by_chiplet),
    )


def combos_for_policy(
    benchmark: str,
    policy: str,
    skewness: int,
    skew_map: dict[str, int],
    *,
    sweep_canonical_count_vectors: bool,
    sweep_physical_assignments: bool,
    same_chiplet_single_combo: bool,
    rotation: int,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> list[tuple[str, dict[str, int] | None]]:
    if policy == 'eevdf':
        return [('eevdf', None)]
    if is_scheduler_managed_policy(policy):
        return [(policy, None)]
    if policy == 'optimal':
        if sweep_canonical_count_vectors:
            if sweep_physical_assignments:
                combos: list[tuple[str, dict[str, int] | None]] = []
                for count_vector in CANONICAL_COUNT_VECTORS:
                    combos.extend(physical_assignment_combos(count_vector))
                return combos
            return [
                (count_vector_combo_id(count_vector), counts_from_sorted_vector(skew_map, count_vector))
                for count_vector in CANONICAL_COUNT_VECTORS
            ]
        if best_combo_map is not None:
            best_row = best_combo_map.get((benchmark, policy, skewness, rotation))
            if best_row is not None:
                return [(str(best_row['combo_id']), dict(best_row['counts_by_chiplet']))]
        return [('optimal', optimal_counts_for_skew(skew_map))]
    if policy == 'optimal-pinned':
        if sweep_canonical_count_vectors:
            if sweep_physical_assignments:
                combos: list[tuple[str, dict[str, int] | None]] = []
                for count_vector in CANONICAL_COUNT_VECTORS:
                    combos.extend(physical_assignment_combos(count_vector))
                return combos
            return [
                (count_vector_combo_id(count_vector), counts_from_sorted_vector(skew_map, count_vector))
                for count_vector in CANONICAL_COUNT_VECTORS
            ]
        if best_combo_map is not None:
            best_row = best_combo_map.get((benchmark, policy, skewness, rotation))
            if best_row is not None:
                return [(str(best_row['combo_id']), dict(best_row['counts_by_chiplet']))]
        return [('optimal-pinned', optimal_counts_for_skew(skew_map))]
    if policy == 'same-chiplet':
        if same_chiplet_single_combo:
            return [
                (f'same-chiplet-rot{index}', counts_from_sorted_vector(skew_map, count_vector))
                for index, count_vector in enumerate(rotated_count_vectors(COUNT_PATTERN))
            ]
        combos = [dict(CANONICAL_COUNTS)] if skewness == 0 else same_chiplet_combinations()
        return [(f'combo{index:02d}', counts) for index, counts in enumerate(combos)]
    raise ValueError(f'Unsupported policy: {policy}')


def planned_run_count(
    args: argparse.Namespace,
    *,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> int:
    total = 0
    for benchmark in args.benchmarks:
        for policy in args.policies:
            for skewness in args.skewnesses:
                rotations = selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation)
                for rotation in rotations:
                    combo_count = len(
                        combos_for_policy(
                            benchmark,
                            policy,
                            skewness,
                            rotation_skew_map(skewness, rotation),
                            sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                            sweep_physical_assignments=args.sweep_physical_assignments,
                            same_chiplet_single_combo=args.same_chiplet_single_combo,
                            rotation=rotation,
                            best_combo_map=best_combo_map,
                        )
                    )
                    total += args.repeats * combo_count
    return total


def find_nr_bench_binary(prefix: str) -> Path | None:
    deps_dir = NODE_REPLICATION_PACKAGE / 'target' / 'release' / 'deps'
    if not deps_dir.exists():
        return None
    candidates = [
        path
        for path in deps_dir.glob(f'{prefix}-*')
        if path.is_file() and path.suffix == '' and path.stat().st_mode & 0o111
    ]
    if not candidates:
        return None
    candidates.sort(key=lambda path: path.stat().st_mtime, reverse=True)
    return candidates[0]


def ensure_nr_bench_binary(
    output_dir: Path,
    *,
    bench_name: str,
    prefix: str,
    extra_args: list[str] | None = None,
    extra_env: dict[str, str] | None = None,
    log_name: str | None = None,
) -> Path:
    require_path(NODE_REPLICATION_MANIFEST, 'node-replication lockfree bench manifest')
    binary = find_nr_bench_binary(prefix)
    if binary is not None:
        require_path(binary, f'node-replication {bench_name} bench', executable=True)
        return binary

    build_log = output_dir / 'raw' / (log_name or f'node_replication_{bench_name}_build.log')
    compile_env = build_env(updates={'RUSTUP_TOOLCHAIN': 'stable', 'RUSTC_BOOTSTRAP': '1'})
    if extra_env:
        compile_env.update(extra_env)
    run_capture(
        ['cargo', 'bench', '--bench', bench_name, '--no-run', *(extra_args or [])],
        cwd=NODE_REPLICATION_PACKAGE,
        env=compile_env,
        log_path=build_log,
    )
    binary = find_nr_bench_binary(prefix)
    if binary is None:
        raise RuntimeError(f'Failed to locate node-replication {bench_name} bench binary after cargo bench --no-run')
    require_path(binary, f'node-replication {bench_name} bench', executable=True)
    return binary


def ensure_nr_lockfree_binary(output_dir: Path) -> Path:
    return ensure_nr_bench_binary(output_dir, bench_name='lockfree', prefix='lockfree')


def ensure_nr_hashmap_binary(output_dir: Path) -> Path:
    return ensure_nr_bench_binary(
        output_dir,
        bench_name='hashmap',
        prefix='hashmap',
        extra_args=['--features', 'cmp'],
    )


def wis_test_name(benchmark: str) -> str:
    mapping = {
        'wis-lock1': 'lock1',
        'wis-page_fault1': 'page_fault1',
    }
    try:
        return mapping[benchmark]
    except KeyError as exc:
        raise ValueError(f'Unsupported will-it-scale benchmark: {benchmark}') from exc


def wis_binary_path(benchmark: str) -> Path:
    return WILL_IT_SCALE_ROOT / f'{wis_test_name(benchmark)}_threads'


def ensure_wis_binary(output_dir: Path, benchmark: str) -> Path:
    binary = wis_binary_path(benchmark)
    if binary.exists():
        require_path(binary, f'will-it-scale {benchmark} binary', executable=True)
        return binary

    require_path(WILL_IT_SCALE_ROOT / 'Makefile', 'will-it-scale Makefile')
    build_log = output_dir / 'raw' / f'will_it_scale_{wis_test_name(benchmark)}_build.log'
    run_capture(
        ['make', f'-j{os.cpu_count() or 1}'],
        cwd=WILL_IT_SCALE_ROOT,
        log_path=build_log,
    )
    require_path(binary, f'will-it-scale {benchmark} binary', executable=True)
    return binary


def parse_wis_average_ops(output: str) -> float:
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line.startswith('average:'):
            continue
        return float(line.split(':', 1)[1].strip())
    raise RuntimeError(f'Could not parse will-it-scale average from output:\n{output}')


def parse_node_replication_result(csv_path: Path) -> tuple[str, float]:
    with csv_path.open(newline='') as handle:
        rows = list(csv.DictReader(handle))

    if not rows:
        raise RuntimeError(f'Node-replication benchmark did not write any rows to {csv_path}')

    grouped_rows: defaultdict[str, list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        name = row.get('name', '')
        if not name.startswith('skiplist-mlnr') or '-wr50' not in name:
            continue
        grouped_rows[name].append(row)

    if not grouped_rows:
        raise RuntimeError(f'Could not find skiplist wr50 rows in {csv_path}')

    best_name: str | None = None
    best_mops: float | None = None
    for name, name_rows in grouped_rows.items():
        thread_counts = {int(row['threads']) for row in name_rows}
        if thread_counts != {9}:
            raise RuntimeError(f'Unexpected thread counts in {csv_path} for {name}: {sorted(thread_counts)}')
        intervals = {int(row['exp_time_in_sec']) for row in name_rows}
        if not intervals:
            raise RuntimeError(f'No intervals recorded for {name} in {csv_path}')
        total_iterations = sum(int(row['iterations']) for row in name_rows)
        throughput_mops = total_iterations / len(intervals) / 1_000_000.0
        if best_mops is None or throughput_mops > best_mops:
            best_name = name
            best_mops = throughput_mops

    if best_name is None or best_mops is None:
        raise RuntimeError(f'Failed to extract throughput from {csv_path}')
    return best_name, best_mops


def parse_scaleout_result(csv_path: Path, *, name_prefix: str) -> tuple[str, float]:
    with csv_path.open(newline='') as handle:
        rows = list(csv.DictReader(handle))

    if not rows:
        raise RuntimeError(f'Benchmark did not write any rows to {csv_path}')

    grouped_rows: defaultdict[str, list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        name = row.get('name', '')
        if not name.startswith(name_prefix):
            continue
        grouped_rows[name].append(row)

    if not grouped_rows:
        raise RuntimeError(f'Could not find rows matching {name_prefix!r} in {csv_path}')

    best_name: str | None = None
    best_mops: float | None = None
    for name, name_rows in grouped_rows.items():
        thread_counts = {int(row['threads']) for row in name_rows}
        if thread_counts != {9}:
            raise RuntimeError(f'Unexpected thread counts in {csv_path} for {name}: {sorted(thread_counts)}')
        intervals = {int(row['exp_time_in_sec']) for row in name_rows}
        if not intervals:
            raise RuntimeError(f'No intervals recorded for {name} in {csv_path}')
        total_iterations = sum(int(row['iterations']) for row in name_rows)
        throughput_mops = total_iterations / len(intervals) / 1_000_000.0
        if best_mops is None or throughput_mops > best_mops:
            best_name = name
            best_mops = throughput_mops

    if best_name is None or best_mops is None:
        raise RuntimeError(f'Failed to extract throughput from {csv_path}')
    return best_name, best_mops


def verification_ok_value(verification: str) -> float:
    value = verification.strip().upper()
    if not value or value in {'OK', 'N/A'}:
        return 1.0
    if 'SUCCESS' in value:
        return 1.0
    if 'FAIL' in value or 'UNSUCCESS' in value:
        return 0.0
    return 1.0


def summarize_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    baseline_map: dict[tuple[str, str], float] = {}
    grouped_zero: defaultdict[tuple[str, str], list[float]] = defaultdict(list)
    for row in results:
        if int(row['skewness']) == 0:
            grouped_zero[(str(row['benchmark']), str(row['policy']))].append(float(row['primary_metric_value']))

    for key, values in grouped_zero.items():
        baseline_map[key] = mean(values)

    final_rows: list[dict[str, object]] = []
    for row in results:
        baseline_key = (str(row['benchmark']), str(row['policy']))
        final_row = dict(row)
        if baseline_key in baseline_map:
            baseline = baseline_map[baseline_key]
            primary_metric = float(row['primary_metric_value'])
            if str(row['primary_metric_name']) == 'time_s':
                slowdown = primary_metric / baseline
            else:
                slowdown = baseline / primary_metric
            final_row['slowdown_vs_skew0'] = f'{slowdown:.6f}'
        else:
            final_row['slowdown_vs_skew0'] = ''
        final_rows.append(final_row)

    final_rows.sort(
        key=lambda row: (
            BENCHMARK_ORDER[str(row['benchmark'])],
            POLICY_ORDER[str(row['policy'])],
            int(row['skewness']),
            int(row['rotation']),
            int(row['repeat']),
            str(row['combo_id']),
        )
    )

    grouped_rows: defaultdict[tuple[str, str, int], list[dict[str, object]]] = defaultdict(list)
    for row in final_rows:
        grouped_rows[(str(row['benchmark']), str(row['policy']), int(row['skewness']))].append(row)

    summary_rows: list[dict[str, object]] = []
    for benchmark, policy, skewness in sorted(
        grouped_rows,
        key=lambda key: (BENCHMARK_ORDER[key[0]], POLICY_ORDER[key[1]], key[2]),
    ):
        rows = grouped_rows[(benchmark, policy, skewness)]
        primary_metric_name = str(rows[0]['primary_metric_name'])
        primary_values = [float(row['primary_metric_value']) for row in rows]
        mops_values = [float(row['mops_total']) for row in rows]
        verification_ok_values = [verification_ok_value(str(row['verification'])) for row in rows]
        npb_time_values = [float(row['npb_time_s']) for row in rows if str(row['npb_time_s']).strip()]
        mean_primary_value = mean(primary_values)
        mean_slowdown_vs_skew0 = ''
        if (benchmark, policy) in baseline_map:
            baseline = baseline_map[(benchmark, policy)]
            if primary_metric_name == 'time_s':
                mean_slowdown_vs_skew0 = f'{mean_primary_value / baseline:.6f}'
            else:
                mean_slowdown_vs_skew0 = f'{baseline / mean_primary_value:.6f}'
        summary_rows.append(
            {
                'benchmark': benchmark,
                'policy': policy,
                'skewness': skewness,
                'primary_metric_name': primary_metric_name,
                'runs': len(rows),
                'successful_runs': int(sum(1 for value in verification_ok_values if value >= 1.0)),
                'verification_ok_ratio': f'{mean(verification_ok_values):.6f}',
                'mean_primary_metric_value': f'{mean_primary_value:.6f}',
                'stdev_primary_metric_value': f'{stdev(primary_values):.6f}',
                'mean_npb_time_s': f'{mean(npb_time_values):.6f}' if npb_time_values else '',
                'stdev_npb_time_s': f'{stdev(npb_time_values):.6f}' if npb_time_values else '',
                'mean_slowdown_vs_skew0': mean_slowdown_vs_skew0,
                'mean_mops_total': f'{mean(mops_values):.6f}',
                'stdev_mops_total': f'{stdev(mops_values):.6f}',
                'mean_ccd0_noise_bandwidth_mbs': f"{mean([float(row['ccd0_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd1_noise_bandwidth_mbs': f"{mean([float(row['ccd1_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd3_noise_bandwidth_mbs': f"{mean([float(row['ccd3_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd4_noise_bandwidth_mbs': f"{mean([float(row['ccd4_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_total_noise_bandwidth_mbs': f"{mean([float(row['total_noise_bandwidth_mbs']) for row in rows]):.6f}",
            }
        )
    return final_rows, summary_rows


def summarize_combo_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    grouped: defaultdict[tuple[str, str, int, int, str, str, str, str], list[dict[str, object]]] = defaultdict(list)
    for row in results:
        if str(row['combo_counts']) == 'dynamic':
            continue
        key = (
            str(row['benchmark']),
            str(row['policy']),
            int(row['skewness']),
            int(row['rotation']),
            str(row['combo_id']),
            str(row['benchmark_variant']),
            str(row['combo_counts']),
            str(row['workload_core_map']),
        )
        grouped[key].append(row)

    combo_rows: list[dict[str, object]] = []
    best_rows: list[dict[str, object]] = []
    ranked_by_condition: defaultdict[tuple[str, str, int, int], list[dict[str, object]]] = defaultdict(list)

    for key, rows in grouped.items():
        benchmark, policy, skewness, rotation, combo_id, benchmark_variant, combo_counts, workload_core_map = key
        primary_values = [float(row['primary_metric_value']) for row in rows]
        npb_time_values = [float(row['npb_time_s']) for row in rows if str(row['npb_time_s']).strip()]
        mops_values = [float(row['mops_total']) for row in rows]
        combo_row = {
            'benchmark': benchmark,
            'policy': policy,
            'skewness': skewness,
            'rotation': rotation,
            'combo_id': combo_id,
            'benchmark_variant': benchmark_variant,
            'combo_counts': combo_counts,
            'workload_core_map': workload_core_map,
            'runs': len(rows),
            'mean_primary_metric_value': f'{mean(primary_values):.6f}',
            'stdev_primary_metric_value': f'{stdev(primary_values):.6f}',
            'mean_npb_time_s': f'{mean(npb_time_values):.6f}' if npb_time_values else '',
            'stdev_npb_time_s': f'{stdev(npb_time_values):.6f}' if npb_time_values else '',
            'mean_mops_total': f'{mean(mops_values):.6f}',
            'stdev_mops_total': f'{stdev(mops_values):.6f}',
            'mean_total_noise_bandwidth_mbs': f"{mean([float(row['total_noise_bandwidth_mbs']) for row in rows]):.6f}",
        }
        combo_rows.append(combo_row)
        ranked_by_condition[(benchmark, policy, skewness, rotation)].append(combo_row)

    combo_rows.sort(
        key=lambda row: (
            BENCHMARK_ORDER[str(row['benchmark'])],
            POLICY_ORDER[str(row['policy'])],
            int(row['skewness']),
            int(row['rotation']),
            -float(row['mean_primary_metric_value']),
            str(row['combo_id']),
        )
    )

    for condition in sorted(
        ranked_by_condition,
        key=lambda key: (BENCHMARK_ORDER[key[0]], POLICY_ORDER[key[1]], key[2], key[3]),
    ):
        best_row = max(
            ranked_by_condition[condition],
            key=lambda row: (
                float(row['mean_primary_metric_value']),
                -float(row['mean_npb_time_s'] or 0.0),
                str(row['combo_id']),
            ),
        )
        best_rows.append(best_row)

    return combo_rows, best_rows


def print_dry_run(
    args: argparse.Namespace,
    *,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> None:
    nr_lockfree_binary = find_nr_bench_binary('lockfree') if 'skiplist-rw50' in args.benchmarks else None
    nr_hashmap_binary = find_nr_bench_binary('hashmap') if 'rwlock-rw50' in args.benchmarks else None
    wis_lock1_binary = wis_binary_path('wis-lock1') if 'wis-lock1' in args.benchmarks else None
    wis_page_fault1_binary = wis_binary_path('wis-page_fault1') if 'wis-page_fault1' in args.benchmarks else None
    nr_lockfree_display = str(nr_lockfree_binary) if nr_lockfree_binary is not None else '<nr-lockfree-bench>'
    nr_hashmap_display = str(nr_hashmap_binary) if nr_hashmap_binary is not None else '<nr-hashmap-bench>'
    wis_lock1_display = str(wis_lock1_binary) if wis_lock1_binary is not None else '<wis-lock1-threads>'
    wis_page_fault1_display = (
        str(wis_page_fault1_binary) if wis_page_fault1_binary is not None else '<wis-page_fault1-threads>'
    )
    print(f'exp_case4_2 output directory: {args.output_dir}')
    print(f'exp_case4_2 planned runs: {planned_run_count(args, best_combo_map=best_combo_map)}')
    print(
        'exp_case4_2 calibration candidates: '
        + ','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)
    )
    for benchmark in args.benchmarks:
        for policy in args.policies:
            for skewness in args.skewnesses:
                for rotation in selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation):
                    skew_map = rotation_skew_map(skewness, rotation)
                    for combo_id, counts_by_chiplet in combos_for_policy(
                        benchmark,
                        policy,
                        skewness,
                        skew_map,
                        sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                        sweep_physical_assignments=args.sweep_physical_assignments,
                        same_chiplet_single_combo=args.same_chiplet_single_combo,
                        rotation=rotation,
                        best_combo_map=best_combo_map,
                    ):
                        run_id = (
                            f'{benchmark}__{policy}__skew{skewness}__rot{rotation}'
                            f'__r1__{combo_id}'
                        )
                        if benchmark == 'ep':
                            argv, _env, workload_core_map, combo_counts = build_ep_launch(
                                policy,
                                counts_by_chiplet,
                                optimal_strict_pinning=args.optimal_strict_pinning,
                                output_dir=args.output_dir.resolve(),
                                run_id=run_id,
                            )
                        elif benchmark == 'skiplist-rw50':
                            argv, _env, workload_core_map, combo_counts = build_skiplist_launch(
                                policy,
                                counts_by_chiplet,
                                Path(nr_lockfree_display),
                                args.nr_benchmark_duration_seconds,
                                args.nr_skiplist_initial_capacity,
                                args.nr_skiplist_key_space,
                                args.nr_skiplist_ops,
                                args.nr_skiplist_log_counts,
                                output_dir=args.output_dir.resolve(),
                                run_id=run_id,
                            )
                        elif benchmark in {'wis-lock1', 'wis-page_fault1'}:
                            wis_display = wis_lock1_display if benchmark == 'wis-lock1' else wis_page_fault1_display
                            argv, _env, workload_core_map, combo_counts = build_wis_launch(
                                benchmark,
                                policy,
                                counts_by_chiplet,
                                Path(wis_display),
                                args.wis_duration_seconds,
                                output_dir=args.output_dir.resolve(),
                                run_id=run_id,
                            )
                        else:
                            argv, _env, workload_core_map, combo_counts = build_rwlock_launch(
                                policy,
                                counts_by_chiplet,
                                Path(nr_hashmap_display),
                                args.nr_benchmark_duration_seconds,
                                args.nr_rwlock_initial_capacity,
                                args.nr_rwlock_key_space,
                                args.nr_rwlock_ops,
                                output_dir=args.output_dir.resolve(),
                                run_id=run_id,
                            )
                        print(
                            f"[exp_case4_2 dry-run] benchmark={benchmark} policy={policy} "
                            f"skewness={skewness} rotation={rotation} combo_id={combo_id}"
                        )
                        print(f'  skew_map: {format_skew_map(skew_map)}')
                        print(f'  combo_counts: {combo_counts}')
                        print(f'  workload_core_map: {workload_core_map}')
                        print('  noise_rate_map: <runtime-calibrated>')
                        print(f'  workload: {format_command(argv)}')


def main() -> int:
    args = parse_args()
    noise_memory_nodes = parse_node_list(args.noise_memory_nodes)
    best_combo_map = None
    if args.optimal_combo_summary is not None:
        require_path(args.optimal_combo_summary.resolve(), 'exp_case4_2 optimal combo summary')
        best_combo_map = load_best_combo_summary(args.optimal_combo_summary.resolve())

    if args.repeats <= 0:
        raise RuntimeError('--repeats must be positive')
    if args.nr_benchmark_duration_seconds <= 0:
        raise RuntimeError('--nr-benchmark-duration-seconds must be positive')
    if args.nr_skiplist_initial_capacity <= 0:
        raise RuntimeError('--nr-skiplist-initial-capacity must be positive')
    if args.nr_skiplist_key_space <= 0:
        raise RuntimeError('--nr-skiplist-key-space must be positive')
    if args.nr_skiplist_ops <= 0:
        raise RuntimeError('--nr-skiplist-ops must be positive')
    if not args.nr_skiplist_log_counts.strip():
        raise RuntimeError('--nr-skiplist-log-counts must not be empty')
    if args.nr_rwlock_initial_capacity <= 0:
        raise RuntimeError('--nr-rwlock-initial-capacity must be positive')
    if args.nr_rwlock_key_space <= 0:
        raise RuntimeError('--nr-rwlock-key-space must be positive')
    if args.nr_rwlock_ops <= 0:
        raise RuntimeError('--nr-rwlock-ops must be positive')
    if args.wis_duration_seconds <= 0:
        raise RuntimeError('--wis-duration-seconds must be positive')

    require_path(MEMORY_BENCHMARK, 'memory_benchmark', executable=True)
    if 'ep' in args.benchmarks:
        require_path(NPB_EP_B, 'NPB EP class B', executable=True)
    if 'skiplist-rw50' in args.benchmarks or 'rwlock-rw50' in args.benchmarks:
        require_path(NODE_REPLICATION_MANIFEST, 'node-replication lockfree manifest')
    if 'wis-lock1' in args.benchmarks or 'wis-page_fault1' in args.benchmarks:
        require_path(WILL_IT_SCALE_ROOT / 'Makefile', 'will-it-scale Makefile')
    for policy in args.policies:
        if policy in SCHEDULER_POLICY_BINARIES:
            require_path(SCHEDULER_POLICY_BINARIES[policy], f'{policy} scheduler binary', executable=True)
            require_path(CCM_MAPPING_PATH, 'CCM mapping path')

    if not args.skip_topology_check:
        validate_case4_topology()

    if args.dry_run:
        print_dry_run(args, best_combo_map=best_combo_map)
        return 0

    output_dir = ensure_directory(args.output_dir.resolve())
    raw_dir = ensure_directory(output_dir / 'raw')
    ensure_directory(output_dir / 'configs')
    ensure_directory(output_dir / 'noise_outputs')
    calibration_path = output_dir / 'exp_case4_2_calibration.csv'
    results_path = output_dir / 'exp_case4_2_results.csv'
    summary_path = output_dir / 'exp_case4_2_summary_stats.csv'
    combo_summary_path = output_dir / 'exp_case4_2_combo_summary.csv'
    best_combo_path = output_dir / 'exp_case4_2_best_combos.csv'
    metadata_path = output_dir / 'run_metadata.txt'

    print(f'exp_case4_2 output directory: {output_dir}')
    tier_rate_map, calibration_rows = run_case4_calibration(
        output_dir=output_dir,
        raw_dir=raw_dir,
        calibration_duration_seconds=args.calibration_duration_seconds,
        memory_nodes=noise_memory_nodes,
    )
    write_csv_rows(calibration_path, CALIBRATION_FIELDNAMES, calibration_rows)

    nr_lockfree_binary = ensure_nr_lockfree_binary(output_dir) if 'skiplist-rw50' in args.benchmarks else None
    nr_hashmap_binary = ensure_nr_hashmap_binary(output_dir) if 'rwlock-rw50' in args.benchmarks else None
    wis_lock1_binary = ensure_wis_binary(output_dir, 'wis-lock1') if 'wis-lock1' in args.benchmarks else None
    wis_page_fault1_binary = ensure_wis_binary(output_dir, 'wis-page_fault1') if 'wis-page_fault1' in args.benchmarks else None

    results: list[dict[str, object]] = []
    metadata_lines = [
        'experiment=exp_case4_2',
        f"benchmarks={','.join(args.benchmarks)}",
        'workload_family=npb,node-replication,will-it-scale',
        'benchmark_ep_class=B',
        'benchmark_skiplist_variant=skiplist-mlnr-wr50',
        'benchmark_rwlock_variant=std-scaleout-wr50',
        'benchmark_wis_variants=lock1_threads,page_fault1_threads',
        'benchmark_threads=9',
        f'nr_skiplist_initial_capacity={args.nr_skiplist_initial_capacity}',
        f'nr_skiplist_key_space={args.nr_skiplist_key_space}',
        f'nr_skiplist_ops={args.nr_skiplist_ops}',
        f'nr_skiplist_log_counts={args.nr_skiplist_log_counts}',
        f'nr_rwlock_initial_capacity={args.nr_rwlock_initial_capacity}',
        f'nr_rwlock_key_space={args.nr_rwlock_key_space}',
        f'nr_rwlock_ops={args.nr_rwlock_ops}',
        f"policies={','.join(args.policies)}",
        f"skewnesses={','.join(str(skewness) for skewness in args.skewnesses)}",
        f"rotations={','.join(str(rotation) for rotation in args.rotations)}",
        f'fixed_rotation={args.fixed_rotation if args.fixed_rotation is not None else ""}',
        f'repeats={args.repeats}',
        f'noise_memory_nodes={",".join(str(node) for node in noise_memory_nodes)}',
        f'optimal_combo_summary={args.optimal_combo_summary.resolve() if args.optimal_combo_summary else ""}',
        f'optimal_strict_pinning={int(args.optimal_strict_pinning)}',
        f'eevdf_workload_mask={EEVDF_WORKLOAD_MASK}',
        f'sweep_canonical_count_vectors={int(args.sweep_canonical_count_vectors)}',
        f'sweep_physical_assignments={int(args.sweep_physical_assignments)}',
        f'same_chiplet_single_combo={int(args.same_chiplet_single_combo)}',
        'count_patterns='
        + ';'.join(','.join(str(count) for count in count_vector) for count_vector in CANONICAL_COUNT_VECTORS),
        f'same_chiplet_combo_count={len(same_chiplet_combinations())}',
        f"calibration_rate_candidates={','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)}",
        'calibrated_tier_rates='
        + ','.join(f'{tier}:{tier_rate_map[tier]}' for tier in sorted(tier_rate_map)),
        f'settle_seconds={args.settle_seconds}',
        f'cooldown_seconds={args.cooldown_seconds}',
        f'noise_time_seconds={args.noise_time_seconds}',
        f'nr_benchmark_duration_seconds={args.nr_benchmark_duration_seconds}',
        f'wis_duration_seconds={args.wis_duration_seconds}',
        f'node_replication_timeout_seconds={args.node_replication_timeout_seconds}',
    ]
    if 'eevdf-emulated' in args.policies:
        metadata_lines.append(f'eevdf_emulated_binary={EMULATED_EEVDF_BINARY}')
    if 'la-default' in args.policies:
        metadata_lines.append(f'la_default_binary={OUR_SCHEDULER_BINARY}')
    if any(policy in SCHEDULER_POLICY_BINARIES for policy in args.policies):
        metadata_lines.append(f'ccm_mapping_path={CCM_MAPPING_PATH}')
    if nr_lockfree_binary is not None:
        metadata_lines.append(f'nr_lockfree_binary={nr_lockfree_binary}')
    if nr_hashmap_binary is not None:
        metadata_lines.append(f'nr_hashmap_binary={nr_hashmap_binary}')
    if wis_lock1_binary is not None:
        metadata_lines.append(f'wis_lock1_binary={wis_lock1_binary}')
    if wis_page_fault1_binary is not None:
        metadata_lines.append(f'wis_page_fault1_binary={wis_page_fault1_binary}')

    for repeat in range(1, args.repeats + 1):
        for benchmark in args.benchmarks:
            for policy in args.policies:
                for skewness in args.skewnesses:
                    for rotation in selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation):
                        skew_map = rotation_skew_map(skewness, rotation)
                        noise_rates_by_chiplet = build_noise_rates_by_chiplet(tier_rate_map, skew_map)
                        for combo_id, counts_by_chiplet in combos_for_policy(
                            benchmark,
                            policy,
                            skewness,
                            skew_map,
                            sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                            sweep_physical_assignments=args.sweep_physical_assignments,
                            same_chiplet_single_combo=args.same_chiplet_single_combo,
                            rotation=rotation,
                            best_combo_map=best_combo_map,
                        ):
                            safe_benchmark = benchmark.replace('-', '_')
                            run_id = (
                                f'{safe_benchmark}__{policy}__skew{skewness}__rot{rotation}'
                                f'__r{repeat}__{combo_id}'
                            )
                            config_path, _noise_output_path = write_case4_noise_config(
                                output_dir,
                                name=f'{run_id}__noise',
                                noise_rates_by_chiplet=noise_rates_by_chiplet,
                                duration_seconds=args.noise_time_seconds,
                                memory_nodes=noise_memory_nodes,
                            )
                            noise_log = raw_dir / f'{run_id}__noise.log'
                            noise_argv = [str(MEMORY_BENCHMARK), '--config', str(config_path)]

                            if benchmark == 'ep':
                                argv, env, workload_core_map, combo_counts = build_ep_launch(
                                    policy,
                                    counts_by_chiplet,
                                    optimal_strict_pinning=args.optimal_strict_pinning,
                                    output_dir=output_dir,
                                    run_id=run_id,
                                )
                                workload_log = raw_dir / f'{run_id}__npb.log'
                                workload_label = 'npb'
                                workload_cwd = output_dir
                                nr_csv_path = None
                            elif benchmark == 'skiplist-rw50':
                                if nr_lockfree_binary is None:
                                    raise RuntimeError('node-replication binary was not prepared')
                                argv, env, workload_core_map, combo_counts = build_skiplist_launch(
                                    policy,
                                    counts_by_chiplet,
                                    nr_lockfree_binary,
                                    args.nr_benchmark_duration_seconds,
                                    args.nr_skiplist_initial_capacity,
                                    args.nr_skiplist_key_space,
                                    args.nr_skiplist_ops,
                                    args.nr_skiplist_log_counts,
                                    output_dir=output_dir,
                                    run_id=run_id,
                                )
                                workload_log = raw_dir / f'{run_id}__nr.log'
                                workload_label = 'skiplist'
                                nr_csv_path = raw_dir / f'{run_id}__scaleout_benchmarks_cnr.csv'
                                if nr_csv_path.exists():
                                    nr_csv_path.unlink()
                                env = dict(env)
                                env['BENCH_UTILS_CNR_CSV_PATH'] = str(nr_csv_path)
                                workload_cwd = output_dir
                            elif benchmark in {'wis-lock1', 'wis-page_fault1'}:
                                wis_binary = wis_lock1_binary if benchmark == 'wis-lock1' else wis_page_fault1_binary
                                if wis_binary is None:
                                    raise RuntimeError(f'will-it-scale binary was not prepared for {benchmark}')
                                argv, env, workload_core_map, combo_counts = build_wis_launch(
                                    benchmark,
                                    policy,
                                    counts_by_chiplet,
                                    wis_binary,
                                    args.wis_duration_seconds,
                                    output_dir=output_dir,
                                    run_id=run_id,
                                )
                                workload_log = raw_dir / f'{run_id}__wis.log'
                                workload_label = 'wis'
                                workload_cwd = WILL_IT_SCALE_ROOT
                                nr_csv_path = None
                            else:
                                if nr_hashmap_binary is None:
                                    raise RuntimeError('node-replication hashmap binary was not prepared')
                                argv, env, workload_core_map, combo_counts = build_rwlock_launch(
                                    policy,
                                    counts_by_chiplet,
                                    nr_hashmap_binary,
                                    args.nr_benchmark_duration_seconds,
                                    args.nr_rwlock_initial_capacity,
                                    args.nr_rwlock_key_space,
                                    args.nr_rwlock_ops,
                                    output_dir=output_dir,
                                    run_id=run_id,
                                )
                                workload_log = raw_dir / f'{run_id}__rwlock.log'
                                workload_label = 'rwlock'
                                nr_csv_path = raw_dir / f'{run_id}__scaleout_benchmarks.csv'
                                if nr_csv_path.exists():
                                    nr_csv_path.unlink()
                                env = dict(env)
                                env['BENCH_UTILS_CSV_PATH'] = str(nr_csv_path)
                                workload_cwd = output_dir

                            print(
                                f'[exp_case4_2] repeat={repeat} benchmark={benchmark} policy={policy} '
                                f'skewness={skewness} rotation={rotation} combo_id={combo_id}'
                            )
                            print(f'  skew_map: {format_skew_map(skew_map)}')
                            print(f'  combo_counts: {combo_counts}')
                            print(f'  noise_rate_map: {format_noise_rate_map(noise_rates_by_chiplet)}')
                            print(f'  noise: {format_command(noise_argv)}')
                            print(f'  {workload_label}: {format_command(argv)}')

                            background = None
                            workload_error: RuntimeError | None = None
                            workload_result = None
                            noise_launch_started = time.monotonic()
                            try:
                                background = start_background(noise_argv, log_path=noise_log)
                                ready = wait_for_noise_ready(
                                    noise_log,
                                    timeout_seconds=max(args.settle_seconds, 5.0),
                                )
                                elapsed_before_workload = time.monotonic() - noise_launch_started
                                if args.settle_seconds > elapsed_before_workload:
                                    time.sleep(args.settle_seconds - elapsed_before_workload)
                                if not ready:
                                    print('  warning: noise workload did not reach active measurement state before workload launch')
                                workload_timeout = (
                                    args.node_replication_timeout_seconds
                                    if benchmark in {'skiplist-rw50', 'rwlock-rw50'}
                                    else None
                                )
                                try:
                                    workload_result = run_capture(
                                        argv,
                                        cwd=workload_cwd,
                                        env=env,
                                        log_path=workload_log,
                                        timeout_seconds=workload_timeout,
                                    )
                                except RuntimeError as exc:
                                    workload_error = exc
                            finally:
                                if background is not None:
                                    elapsed_noise = time.monotonic() - noise_launch_started
                                    if elapsed_noise < 1.0:
                                        time.sleep(1.0 - elapsed_noise)
                                    stop_background(background)

                            noise_by_chiplet = parse_noise_bandwidth_by_chiplet(noise_log)
                            total_noise = sum(noise_by_chiplet.values())

                            try:
                                if workload_error is not None:
                                    raise workload_error
                                if workload_result is None:
                                    raise RuntimeError('Workload finished without a captured result')
                                if benchmark == 'ep':
                                    metrics = parse_npb_metrics(workload_result.output)
                                    if metrics.time_s is None:
                                        raise RuntimeError(f'Failed to parse NPB runtime from {workload_log}')
                                    if metrics.mops_total is None:
                                        raise RuntimeError(f'Failed to parse NPB Mop/s total from {workload_log}')
                                    if metrics.verification_ok is not True:
                                        raise RuntimeError(
                                            f'NPB verification failed or missing in {workload_log}: {metrics.verification}'
                                        )
                                    benchmark_variant = 'ep.B'
                                    primary_metric_name = 'mops_total'
                                    primary_metric_value = metrics.mops_total
                                    npb_time_s = metrics.time_s
                                    mops_total = metrics.mops_total
                                    verification = metrics.verification or ''
                                    print(
                                        f'  npb_time_s={npb_time_s:.6f} '
                                        f'mops_total={mops_total:.6f} '
                                        f'total_noise_bandwidth_mbs={total_noise:.6f}'
                                    )
                                elif benchmark in {'wis-lock1', 'wis-page_fault1'}:
                                    avg_ops_per_s = parse_wis_average_ops(workload_result.output)
                                    benchmark_variant = f'{wis_test_name(benchmark)}_threads'
                                    primary_metric_name = 'throughput_mops'
                                    primary_metric_value = avg_ops_per_s / 1_000_000.0
                                    npb_time_s = ''
                                    mops_total = primary_metric_value
                                    verification = 'OK'
                                    print(
                                        f'  benchmark_variant={benchmark_variant} '
                                        f'throughput_mops={mops_total:.6f} '
                                        f'total_noise_bandwidth_mbs={total_noise:.6f}'
                                    )
                                else:
                                    if not nr_csv_path.exists():
                                        raise RuntimeError(f'Node-replication benchmark did not create {nr_csv_path}')
                                    if benchmark == 'skiplist-rw50':
                                        benchmark_variant, mops_total = parse_node_replication_result(nr_csv_path)
                                    else:
                                        benchmark_variant, mops_total = parse_scaleout_result(
                                            nr_csv_path,
                                            name_prefix='std-scaleout-wr50',
                                        )
                                    primary_metric_name = 'throughput_mops'
                                    primary_metric_value = mops_total
                                    npb_time_s = ''
                                    verification = 'OK'
                                    print(
                                        f'  benchmark_variant={benchmark_variant} '
                                        f'throughput_mops={mops_total:.6f} '
                                        f'total_noise_bandwidth_mbs={total_noise:.6f}'
                                    )
                            except RuntimeError as exc:
                                if benchmark not in {'skiplist-rw50', 'rwlock-rw50'}:
                                    raise
                                print(f'  warning: skipping run after workload failure: {exc}')
                                time.sleep(args.cooldown_seconds)
                                continue

                            results.append(
                                {
                                    'benchmark': benchmark,
                                    'policy': policy,
                                    'skewness': skewness,
                                    'rotation': rotation,
                                    'repeat': repeat,
                                    'combo_id': combo_id,
                                    'benchmark_variant': benchmark_variant,
                                    'combo_counts': combo_counts,
                                    'skew_map': format_skew_map(skew_map),
                                    'workload_core_map': workload_core_map,
                                    'noise_rate_map': format_noise_rate_map(noise_rates_by_chiplet),
                                    'ccd0_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd0']:.6f}",
                                    'ccd1_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd1']:.6f}",
                                    'ccd3_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd3']:.6f}",
                                    'ccd4_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd4']:.6f}",
                                    'total_noise_bandwidth_mbs': f'{total_noise:.6f}',
                                    'primary_metric_name': primary_metric_name,
                                    'primary_metric_value': f'{primary_metric_value:.6f}',
                                    'npb_time_s': f'{npb_time_s:.6f}' if isinstance(npb_time_s, float) else '',
                                    'mops_total': f'{mops_total:.6f}',
                                    'verification': verification,
                                    'slowdown_vs_skew0': '',
                                }
                            )
                            time.sleep(args.cooldown_seconds)

    final_rows, summary_rows = summarize_results(results)
    combo_rows, best_rows = summarize_combo_results(final_rows)
    write_csv_rows(results_path, RESULT_FIELDNAMES, final_rows)
    write_csv_rows(summary_path, SUMMARY_FIELDNAMES, summary_rows)
    if combo_rows:
        write_csv_rows(combo_summary_path, COMBO_SUMMARY_FIELDNAMES, combo_rows)
        write_csv_rows(best_combo_path, BEST_COMBO_FIELDNAMES, best_rows)
    metadata_path.write_text('\n'.join(metadata_lines) + '\n')

    print(f'exp_case4_2 results written to {results_path}')
    print(f'exp_case4_2 summary stats written to {summary_path}')
    if combo_rows:
        print(f'exp_case4_2 combo summary written to {combo_summary_path}')
        print(f'exp_case4_2 best combos written to {best_combo_path}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
