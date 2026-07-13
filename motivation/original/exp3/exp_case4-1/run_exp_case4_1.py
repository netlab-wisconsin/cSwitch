#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import itertools
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
    parse_gapbs_trial_time,
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
    DEFAULT_ROTATIONS,
    DEFAULT_SKEWNESS_LEVELS,
    EEVDF_WORKLOAD_MASK,
    EMULATED_EEVDF_BINARY,
    GAPBS_DIR,
    MEMORY_BENCHMARK,
    OUR_SCHEDULER_BINARY,
    SCHEDULER_POLICY_BINARIES,
    build_scheduler_managed_argv,
    build_noise_rates_by_chiplet,
    format_combo_counts,
    format_noise_rate_map,
    format_skew_map,
    format_workload_core_map,
    is_scheduler_managed_policy,
    omp_places_from_cores,
    optimal_counts_for_skew,
    parse_combo_counts,
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


BENCHMARKS = {
    'bfs': GAPBS_DIR / 'bfs',
    'bc': GAPBS_DIR / 'bc',
    'pr': GAPBS_DIR / 'pr',
}
POLICY_ORDER = {
    'eevdf': 0,
    'eevdf-emulated': 1,
    'la-default': 2,
    'optimal': 3,
    'same-chiplet': 4,
}
CANONICAL_COUNT_VECTORS = (
    (4, 4, 1, 0),
    (4, 3, 2, 0),
    (4, 3, 1, 1),
    (4, 2, 2, 1),
    (3, 3, 3, 0),
    (3, 3, 2, 1),
    (3, 2, 2, 2),
)
CALIBRATION_FIELDNAMES = [
    'target_pct',
    'rate',
    'measured_total_noise_bandwidth_mbs',
    'measured_ccd0_noise_bandwidth_mbs',
    'measured_ccd1_noise_bandwidth_mbs',
    'measured_ccd3_noise_bandwidth_mbs',
    'measured_ccd4_noise_bandwidth_mbs',
]
RESULT_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'rotation',
    'repeat',
    'combo_id',
    'combo_counts',
    'skew_map',
    'workload_core_map',
    'noise_rate_map',
    'ccd0_noise_bandwidth_mbs',
    'ccd1_noise_bandwidth_mbs',
    'ccd3_noise_bandwidth_mbs',
    'ccd4_noise_bandwidth_mbs',
    'total_noise_bandwidth_mbs',
    'trial_time_s',
    'slowdown_vs_skew0',
]
SUMMARY_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'runs',
    'mean_trial_time_s',
    'stdev_trial_time_s',
    'mean_slowdown_vs_skew0',
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
    'combo_counts',
    'workload_core_map',
    'runs',
    'mean_trial_time_s',
    'stdev_trial_time_s',
    'mean_total_noise_bandwidth_mbs',
]
BEST_COMBO_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'rotation',
    'combo_id',
    'combo_counts',
    'workload_core_map',
    'runs',
    'mean_trial_time_s',
    'stdev_trial_time_s',
    'mean_total_noise_bandwidth_mbs',
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='Run exp_case4-1 GAPBS skew experiment.')
    parser.add_argument(
        '--benchmarks',
        nargs='+',
        default=['bfs', 'pr'],
        choices=sorted(BENCHMARKS),
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
    parser.add_argument('--graph-scale', type=int, default=20, help='Kronecker graph scale.')
    parser.add_argument('--graph-file', type=Path, help='Optional GAPBS graph file; overrides --graph-scale when set.')
    parser.add_argument('--pr-iterations', type=int, default=100, help='PageRank iteration count.')
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
        '--noise-memory-nodes',
        default='0',
        help='Comma-separated NUMA nodes used by the synthetic noise workload. Default uses NUMA 0 only.',
    )
    parser.add_argument(
        '--optimal-combo-summary',
        type=Path,
        help='Optional combo-summary CSV used to choose the best optimal count vector per benchmark/skewness/rotation.',
    )
    parser.add_argument(
        '--sweep-canonical-count-vectors',
        action='store_true',
        help='For optimal, sweep the seven canonical 9-thread count vectors instead of only 4,4,1,0.',
    )
    parser.add_argument(
        '--optimal-strict-pinning',
        action='store_true',
        help='Pin optimal OpenMP threads to the selected cores in ascending per-chiplet order.',
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


def load_gapbs_best_combo_summary(summary_path: Path) -> dict[tuple[str, str, int, int], dict[str, object]]:
    best_rows: dict[tuple[str, str, int, int], dict[str, object]] = {}
    with summary_path.open(newline='') as handle:
        reader = csv.DictReader(handle)
        for row in reader:
            key = (
                row['benchmark'],
                row['policy'],
                int(row['skewness']),
                int(row['rotation']),
            )
            trial_time = float(row['mean_trial_time_s'])
            candidate = {
                'combo_id': row['combo_id'],
                'combo_counts': row['combo_counts'],
                'counts_by_chiplet': parse_combo_counts(row['combo_counts']),
                'workload_core_map': row.get('workload_core_map', ''),
                'mean_trial_time_s': trial_time,
            }
            current = best_rows.get(key)
            if current is None:
                best_rows[key] = candidate
                continue
            if trial_time < float(current['mean_trial_time_s']):
                best_rows[key] = candidate
                continue
            if trial_time == float(current['mean_trial_time_s']) and row['combo_id'] < str(current['combo_id']):
                best_rows[key] = candidate
    return best_rows


def build_gapbs_base_argv(
    benchmark: str,
    graph_scale: int,
    pr_iterations: int,
    graph_file: Path | None,
) -> list[str]:
    argv = [str(BENCHMARKS[benchmark])]
    if graph_file is not None:
        argv.extend(['-f', str(graph_file.resolve())])
    else:
        argv.extend(['-g', str(graph_scale)])
    argv.append('-n1')
    if benchmark == 'pr':
        argv.extend(['-i', str(pr_iterations)])
    return argv


def build_policy_launch(
    benchmark: str,
    policy: str,
    graph_scale: int,
    pr_iterations: int,
    graph_file: Path | None,
    counts_by_chiplet: dict[str, int] | None,
    *,
    optimal_strict_pinning: bool,
    output_dir: Path,
    run_id: str,
) -> tuple[list[str], dict[str, str], str, str]:
    base_argv = build_gapbs_base_argv(benchmark, graph_scale, pr_iterations, graph_file)
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


def combos_for_policy(
    benchmark: str,
    policy: str,
    skewness: int,
    skew_map: dict[str, int],
    *,
    sweep_canonical_count_vectors: bool,
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
            combos: list[tuple[str, dict[str, int] | None]] = []
            for count_vector in CANONICAL_COUNT_VECTORS:
                combos.extend(physical_assignment_combos(count_vector))
            return combos
        if best_combo_map is not None:
            best_row = best_combo_map.get((benchmark, policy, skewness, rotation))
            if best_row is not None:
                return [(str(best_row['combo_id']), dict(best_row['counts_by_chiplet']))]
        return [('optimal', optimal_counts_for_skew(skew_map))]
    if policy == 'same-chiplet':
        if same_chiplet_single_combo:
            return [
                (f'same-chiplet-rot{index}', counts_from_sorted_vector(skew_map, count_vector))
                for index, count_vector in enumerate(rotated_count_vectors(CANONICAL_COUNT_VECTORS[0]))
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
                for rotation in selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation):
                    combo_count = len(
                        combos_for_policy(
                            benchmark,
                            policy,
                            skewness,
                            rotation_skew_map(skewness, rotation),
                            sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                            same_chiplet_single_combo=args.same_chiplet_single_combo,
                            rotation=rotation,
                            best_combo_map=best_combo_map,
                        )
                    )
                    total += args.repeats * combo_count
    return total


def print_dry_run(
    args: argparse.Namespace,
    *,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> None:
    print(f'exp_case4_1 output directory: {args.output_dir}')
    print(f'exp_case4_1 planned runs: {planned_run_count(args, best_combo_map=best_combo_map)}')
    print(
        'exp_case4_1 calibration candidates: '
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
                        same_chiplet_single_combo=args.same_chiplet_single_combo,
                        rotation=rotation,
                        best_combo_map=best_combo_map,
                    ):
                        run_id = (
                            f'{benchmark}__{policy}__skew{skewness}__rot{rotation}'
                            f'__r1__{combo_id}'
                        )
                        argv, _env, workload_core_map, combo_counts = build_policy_launch(
                            benchmark,
                            policy,
                            args.graph_scale,
                            args.pr_iterations,
                            args.graph_file,
                            counts_by_chiplet,
                            optimal_strict_pinning=args.optimal_strict_pinning,
                            output_dir=args.output_dir.resolve(),
                            run_id=run_id,
                        )
                        print(
                            f"[exp_case4_1 dry-run] benchmark={benchmark} policy={policy} "
                            f"skewness={skewness} rotation={rotation} combo_id={combo_id}"
                        )
                        print(f'  skew_map: {format_skew_map(skew_map)}')
                        print(f'  combo_counts: {combo_counts}')
                        print(f'  workload_core_map: {workload_core_map}')
                        print('  noise_rate_map: <runtime-calibrated>')
                        print(f'  gapbs: {format_command(argv)}')


def summarize_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    baseline_map: dict[tuple[str, str], float] = {}
    grouped_zero: defaultdict[tuple[str, str], list[float]] = defaultdict(list)
    for row in results:
        if int(row['skewness']) == 0:
            grouped_zero[(str(row['benchmark']), str(row['policy']))].append(float(row['trial_time_s']))

    for key, values in grouped_zero.items():
        baseline_map[key] = mean(values)

    final_rows: list[dict[str, object]] = []
    for row in results:
        baseline_key = (str(row['benchmark']), str(row['policy']))
        if baseline_key not in baseline_map:
            raise RuntimeError(f'Missing skew0 baseline for {baseline_key}')
        slowdown = float(row['trial_time_s']) / baseline_map[baseline_key]
        final_row = dict(row)
        final_row['slowdown_vs_skew0'] = f'{slowdown:.6f}'
        final_rows.append(final_row)

    final_rows.sort(
        key=lambda row: (
            str(row['benchmark']),
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
        key=lambda key: (key[0], POLICY_ORDER[key[1]], key[2]),
    ):
        rows = grouped_rows[(benchmark, policy, skewness)]
        trial_values = [float(row['trial_time_s']) for row in rows]
        slowdown_values = [float(row['slowdown_vs_skew0']) for row in rows]
        summary_rows.append(
            {
                'benchmark': benchmark,
                'policy': policy,
                'skewness': skewness,
                'runs': len(rows),
                'mean_trial_time_s': f'{mean(trial_values):.6f}',
                'stdev_trial_time_s': f'{stdev(trial_values):.6f}',
                'mean_slowdown_vs_skew0': f'{mean(slowdown_values):.6f}',
                'mean_ccd0_noise_bandwidth_mbs': f"{mean([float(row['ccd0_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd1_noise_bandwidth_mbs': f"{mean([float(row['ccd1_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd3_noise_bandwidth_mbs': f"{mean([float(row['ccd3_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd4_noise_bandwidth_mbs': f"{mean([float(row['ccd4_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_total_noise_bandwidth_mbs': f"{mean([float(row['total_noise_bandwidth_mbs']) for row in rows]):.6f}",
            }
        )
    return final_rows, summary_rows


def summarize_combo_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    grouped: defaultdict[tuple[str, str, int, int, str, str, str], list[dict[str, object]]] = defaultdict(list)
    for row in results:
        if str(row['combo_counts']) == 'dynamic':
            continue
        key = (
            str(row['benchmark']),
            str(row['policy']),
            int(row['skewness']),
            int(row['rotation']),
            str(row['combo_id']),
            str(row['combo_counts']),
            str(row['workload_core_map']),
        )
        grouped[key].append(row)

    combo_rows: list[dict[str, object]] = []
    best_rows: list[dict[str, object]] = []
    ranked_by_condition: defaultdict[tuple[str, str, int, int], list[dict[str, object]]] = defaultdict(list)

    for key, rows in grouped.items():
        benchmark, policy, skewness, rotation, combo_id, combo_counts, workload_core_map = key
        trial_values = [float(row['trial_time_s']) for row in rows]
        combo_row = {
            'benchmark': benchmark,
            'policy': policy,
            'skewness': skewness,
            'rotation': rotation,
            'combo_id': combo_id,
            'combo_counts': combo_counts,
            'workload_core_map': workload_core_map,
            'runs': len(rows),
            'mean_trial_time_s': f'{mean(trial_values):.6f}',
            'stdev_trial_time_s': f'{stdev(trial_values):.6f}',
            'mean_total_noise_bandwidth_mbs': f"{mean([float(row['total_noise_bandwidth_mbs']) for row in rows]):.6f}",
        }
        combo_rows.append(combo_row)
        ranked_by_condition[(benchmark, policy, skewness, rotation)].append(combo_row)

    combo_rows.sort(
        key=lambda row: (
            str(row['benchmark']),
            POLICY_ORDER[str(row['policy'])],
            int(row['skewness']),
            int(row['rotation']),
            float(row['mean_trial_time_s']),
            str(row['combo_id']),
        )
    )

    for condition in sorted(
        ranked_by_condition,
        key=lambda key: (key[0], POLICY_ORDER[key[1]], key[2], key[3]),
    ):
        best_row = min(
            ranked_by_condition[condition],
            key=lambda row: (
                float(row['mean_trial_time_s']),
                str(row['combo_id']),
            ),
        )
        best_rows.append(best_row)

    return combo_rows, best_rows


def main() -> int:
    args = parse_args()
    noise_memory_nodes = parse_node_list(args.noise_memory_nodes)
    best_combo_map = None
    if args.optimal_combo_summary is not None:
        require_path(args.optimal_combo_summary.resolve(), 'exp_case4_1 optimal combo summary')
        best_combo_map = load_gapbs_best_combo_summary(args.optimal_combo_summary.resolve())

    if args.repeats <= 0:
        raise RuntimeError('--repeats must be positive')
    if 0 not in args.skewnesses:
        raise RuntimeError('A skewness 0 baseline is required.')

    require_path(MEMORY_BENCHMARK, 'memory_benchmark', executable=True)
    if args.graph_file is not None:
        require_path(args.graph_file.resolve(), 'GAPBS graph file')
    for benchmark in args.benchmarks:
        require_path(BENCHMARKS[benchmark], f'GAPBS {benchmark}', executable=True)
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
    calibration_path = output_dir / 'exp_case4_1_calibration.csv'
    results_path = output_dir / 'exp_case4_1_results.csv'
    summary_path = output_dir / 'exp_case4_1_summary_stats.csv'
    combo_summary_path = output_dir / 'exp_case4_1_combo_summary.csv'
    best_combo_path = output_dir / 'exp_case4_1_best_combos.csv'
    metadata_path = output_dir / 'run_metadata.txt'

    print(f'exp_case4_1 output directory: {output_dir}')
    tier_rate_map, calibration_rows = run_case4_calibration(
        output_dir=output_dir,
        raw_dir=raw_dir,
        calibration_duration_seconds=args.calibration_duration_seconds,
        memory_nodes=noise_memory_nodes,
    )
    write_csv_rows(calibration_path, CALIBRATION_FIELDNAMES, calibration_rows)

    results: list[dict[str, object]] = []
    metadata_lines = [
        'experiment=exp_case4_1',
        'workload_family=gapbs',
        f'graph_source={"file" if args.graph_file is not None else "kronecker"}',
        f'graph_scale={args.graph_scale}',
        f'graph_file={args.graph_file.resolve() if args.graph_file else ""}',
        f'pr_iterations={args.pr_iterations}',
        'omp_threads=9',
        f"benchmarks={','.join(args.benchmarks)}",
        f"policies={','.join(args.policies)}",
        f"skewnesses={','.join(str(skewness) for skewness in args.skewnesses)}",
        f"rotations={','.join(str(rotation) for rotation in args.rotations)}",
        f'fixed_rotation={args.fixed_rotation if args.fixed_rotation is not None else ""}',
        f'repeats={args.repeats}',
        f'noise_memory_nodes={",".join(str(node) for node in noise_memory_nodes)}',
        f'optimal_combo_summary={args.optimal_combo_summary.resolve() if args.optimal_combo_summary else ""}',
        f'optimal_strict_pinning={int(args.optimal_strict_pinning)}',
        f'sweep_canonical_count_vectors={int(args.sweep_canonical_count_vectors)}',
        f'same_chiplet_single_combo={int(args.same_chiplet_single_combo)}',
        f'eevdf_workload_mask={EEVDF_WORKLOAD_MASK}',
        'count_patterns='
        + ';'.join(','.join(str(count) for count in count_vector) for count_vector in CANONICAL_COUNT_VECTORS),
        f'same_chiplet_combo_count={len(same_chiplet_combinations())}',
        f"calibration_rate_candidates={','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)}",
        'calibrated_tier_rates='
        + ','.join(f'{tier}:{tier_rate_map[tier]}' for tier in sorted(tier_rate_map)),
        f'settle_seconds={args.settle_seconds}',
        f'cooldown_seconds={args.cooldown_seconds}',
        f'noise_time_seconds={args.noise_time_seconds}',
    ]
    if 'eevdf-emulated' in args.policies:
        metadata_lines.append(f'eevdf_emulated_binary={EMULATED_EEVDF_BINARY}')
    if 'la-default' in args.policies:
        metadata_lines.append(f'la_default_binary={OUR_SCHEDULER_BINARY}')
    if any(policy in SCHEDULER_POLICY_BINARIES for policy in args.policies):
        metadata_lines.append(f'ccm_mapping_path={CCM_MAPPING_PATH}')

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
                            same_chiplet_single_combo=args.same_chiplet_single_combo,
                            rotation=rotation,
                            best_combo_map=best_combo_map,
                        ):
                            run_id = (
                                f'{benchmark}__{policy}__skew{skewness}__rot{rotation}'
                                f'__r{repeat}__{combo_id}'
                            )
                            argv, env, workload_core_map, combo_counts = build_policy_launch(
                                benchmark,
                                policy,
                                args.graph_scale,
                                args.pr_iterations,
                                args.graph_file,
                                counts_by_chiplet,
                                optimal_strict_pinning=args.optimal_strict_pinning,
                                output_dir=output_dir,
                                run_id=run_id,
                            )
                            config_path, _noise_output_path = write_case4_noise_config(
                                output_dir,
                                name=f'{run_id}__noise',
                                noise_rates_by_chiplet=noise_rates_by_chiplet,
                                duration_seconds=args.noise_time_seconds,
                                memory_nodes=noise_memory_nodes,
                            )
                            noise_log = raw_dir / f'{run_id}__noise.log'
                            gapbs_log = raw_dir / f'{run_id}__gapbs.log'
                            noise_argv = [str(MEMORY_BENCHMARK), '--config', str(config_path)]

                            print(
                                f'[exp_case4_1] repeat={repeat} benchmark={benchmark} policy={policy} '
                                f'skewness={skewness} rotation={rotation} combo_id={combo_id}'
                            )
                            print(f'  skew_map: {format_skew_map(skew_map)}')
                            print(f'  combo_counts: {combo_counts}')
                            print(f'  noise_rate_map: {format_noise_rate_map(noise_rates_by_chiplet)}')
                            print(f'  noise: {format_command(noise_argv)}')
                            print(f'  gapbs: {format_command(argv)}')

                            background = None
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
                                result = run_capture(argv, cwd=GAPBS_DIR, env=env, log_path=gapbs_log)
                                trial_time = parse_gapbs_trial_time(result.output)
                            finally:
                                if background is not None:
                                    elapsed_noise = time.monotonic() - noise_launch_started
                                    if elapsed_noise < 1.0:
                                        time.sleep(1.0 - elapsed_noise)
                                    stop_background(background)

                            noise_by_chiplet = parse_noise_bandwidth_by_chiplet(noise_log)
                            total_noise = sum(noise_by_chiplet.values())
                            print(
                                f'  trial_time_s={trial_time:.6f} '
                                f'total_noise_bandwidth_mbs={total_noise:.6f}'
                            )
                            results.append(
                                {
                                    'benchmark': benchmark,
                                    'policy': policy,
                                    'skewness': skewness,
                                    'rotation': rotation,
                                    'repeat': repeat,
                                    'combo_id': combo_id,
                                    'combo_counts': combo_counts,
                                    'skew_map': format_skew_map(skew_map),
                                    'workload_core_map': workload_core_map,
                                    'noise_rate_map': format_noise_rate_map(noise_rates_by_chiplet),
                                    'ccd0_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd0']:.6f}",
                                    'ccd1_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd1']:.6f}",
                                    'ccd3_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd3']:.6f}",
                                    'ccd4_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd4']:.6f}",
                                    'total_noise_bandwidth_mbs': f'{total_noise:.6f}',
                                    'trial_time_s': f'{trial_time:.6f}',
                                    'slowdown_vs_skew0': '',
                                }
                            )
                            time.sleep(args.cooldown_seconds)

    final_rows, summary_rows = summarize_results(results)
    write_csv_rows(results_path, RESULT_FIELDNAMES, final_rows)
    write_csv_rows(summary_path, SUMMARY_FIELDNAMES, summary_rows)
    if any(str(row['combo_counts']) != 'dynamic' for row in results):
        combo_rows, best_rows = summarize_combo_results(results)
        write_csv_rows(combo_summary_path, COMBO_SUMMARY_FIELDNAMES, combo_rows)
        write_csv_rows(best_combo_path, BEST_COMBO_FIELDNAMES, best_rows)
    metadata_path.write_text('\n'.join(metadata_lines) + '\n')

    print(f'exp_case4_1 results written to {results_path}')
    print(f'exp_case4_1 summary stats written to {summary_path}')
    if any(str(row['combo_counts']) != 'dynamic' for row in results):
        print(f'exp_case4_1 combo summary written to {combo_summary_path}')
        print(f'exp_case4_1 best combos written to {best_combo_path}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
