#!/usr/bin/env python3

from __future__ import annotations

import argparse
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
    parse_llama_bench_metrics,
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
    CANONICAL_COUNTS,
    CHIPLET_ORDER,
    DEFAULT_ROTATIONS,
    DEFAULT_SKEWNESS_LEVELS,
    EEVDF_WORKLOAD_MASK,
    MEMORY_BENCHMARK,
    build_noise_rates_by_chiplet,
    format_combo_counts,
    format_noise_rate_map,
    format_skew_map,
    format_workload_core_map,
    load_best_combo_summary,
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


LLAMA_CPP_ROOT = Path('/home/seunghyun/ycsb/benchmarks/llama.cpp')
LLAMA_BENCH = LLAMA_CPP_ROOT / 'build' / 'bin' / 'llama-bench'
DEFAULT_MODEL = Path('/home/seunghyun/llama.cpp/models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf')
BENCHMARK_NAME = 'llama-8b'
PRIMARY_METRIC_CHOICES = ['prompt', 'gen', 'total']
POLICY_ORDER = {
    'eevdf': 0,
    'optimal': 1,
    'optimal-pinned': 2,
    'same-chiplet': 3,
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
    'measured_ccd2_noise_bandwidth_mbs',
    'measured_ccd3_noise_bandwidth_mbs',
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
    'ccd2_noise_bandwidth_mbs',
    'ccd3_noise_bandwidth_mbs',
    'total_noise_bandwidth_mbs',
    'primary_metric_name',
    'primary_metric_value',
    'primary_time_s',
    'prompt_tokens_per_s',
    'gen_tokens_per_s',
    'total_tokens_per_s',
    'model_filename',
    'slowdown_vs_skew0',
]
SUMMARY_FIELDNAMES = [
    'benchmark',
    'policy',
    'skewness',
    'primary_metric_name',
    'runs',
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_primary_time_s',
    'stdev_primary_time_s',
    'mean_prompt_tokens_per_s',
    'mean_gen_tokens_per_s',
    'mean_total_tokens_per_s',
    'mean_slowdown_vs_skew0',
    'mean_ccd0_noise_bandwidth_mbs',
    'mean_ccd1_noise_bandwidth_mbs',
    'mean_ccd2_noise_bandwidth_mbs',
    'mean_ccd3_noise_bandwidth_mbs',
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
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_primary_time_s',
    'stdev_primary_time_s',
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
    'mean_primary_metric_value',
    'stdev_primary_metric_value',
    'mean_primary_time_s',
    'stdev_primary_time_s',
    'mean_total_noise_bandwidth_mbs',
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='Run exp_case4-1 skew experiment with llama.cpp llama-bench.')
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
    parser.add_argument('--model', type=Path, default=DEFAULT_MODEL, help='llama.cpp GGUF model path.')
    parser.add_argument('--threads', type=int, default=9, help='llama-bench thread count.')
    parser.add_argument(
        '--primary-metric',
        choices=PRIMARY_METRIC_CHOICES,
        default='prompt',
        help='Primary throughput metric to optimize and summarize.',
    )
    parser.add_argument('--n-prompt', type=int, default=512, help='Prompt token count.')
    parser.add_argument('--n-gen', type=int, default=0, help='Generation token count.')
    parser.add_argument('--batch-size', type=int, default=512, help='llama-bench batch size.')
    parser.add_argument('--ubatch-size', type=int, default=512, help='llama-bench micro-batch size.')
    parser.add_argument('--llama-repetitions', type=int, default=1, help='llama-bench internal repetitions.')
    parser.add_argument('--n-gpu-layers', type=int, default=0, help='llama-bench GPU layers.')
    parser.add_argument('--numa', default='numactl', help='llama-bench NUMA mode.')
    parser.add_argument(
        '--noise-memory-nodes',
        default='0',
        help='Comma-separated NUMA nodes used by the synthetic noise workload. Default uses NUMA 0 only.',
    )
    parser.add_argument(
        '--optimal-combo-summary',
        type=Path,
        help='Optional combo-summary CSV used to choose the best optimal/optimal-pinned count vector per skewness/rotation.',
    )
    parser.add_argument('--mmap', type=int, choices=[0, 1], default=1, help='llama-bench mmap setting.')
    parser.add_argument('--no-warmup', action='store_true', help='Disable llama-bench warmup.')
    parser.add_argument(
        '--sweep-canonical-count-vectors',
        action='store_true',
        help='For optimal policies, sweep the seven canonical 9-thread count vectors instead of only 4,4,1,0.',
    )
    parser.add_argument(
        '--optimal-strict-pinning',
        action='store_true',
        help='Pin optimal threads to the selected cores in ascending per-chiplet order.',
    )
    parser.add_argument(
        '--same-chiplet-single-combo',
        action='store_true',
        help='Use only the rotated 4,4,1,0 placement for same-chiplet instead of sweeping all 12 permutations.',
    )
    parser.add_argument(
        '--same-chiplet-all-combos-at-skew0',
        action='store_true',
        help='Run all 12 same-chiplet combinations even for skewness 0.',
    )
    parser.add_argument(
        '--output-dir',
        type=Path,
        default=SCRIPT_DIR / 'results' / datetime.now(timezone.utc).strftime('llama-%Y%m%dT%H%M%SZ'),
        help='Directory for CSVs, configs, logs, and metadata.',
    )
    parser.add_argument('--skip-topology-check', action='store_true', help='Skip topology validation.')
    parser.add_argument('--dry-run', action='store_true', help='Print commands without executing them.')
    return parser.parse_args()


def cpu_mask_hex_from_cores(cores: list[int]) -> str:
    mask = 0
    for core in cores:
        mask |= 1 << core
    return hex(mask)


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


def build_llama_base_argv(args: argparse.Namespace) -> list[str]:
    argv = [
        str(LLAMA_BENCH),
        '--model',
        str(args.model.resolve()),
        '--threads',
        str(args.threads),
        '--n-prompt',
        str(args.n_prompt),
        '--n-gen',
        str(args.n_gen),
        '--batch-size',
        str(args.batch_size),
        '--ubatch-size',
        str(args.ubatch_size),
        '--repetitions',
        str(args.llama_repetitions),
        '--n-gpu-layers',
        str(args.n_gpu_layers),
        '--numa',
        str(args.numa),
        '--mmap',
        str(args.mmap),
        '--output',
        'jsonl',
    ]
    if args.no_warmup:
        argv.append('--no-warmup')
    return argv


def select_primary_metric(metrics, primary_metric: str) -> tuple[str, float, float]:
    if primary_metric == 'prompt':
        if metrics.prompt_tokens_per_s is None or metrics.prompt_time_s is None:
            raise RuntimeError('prompt_tokens_per_s requested but prompt metrics are unavailable')
        return 'prompt_tokens_per_s', metrics.prompt_tokens_per_s, metrics.prompt_time_s
    if primary_metric == 'gen':
        if metrics.gen_tokens_per_s is None or metrics.gen_time_s is None:
            raise RuntimeError('gen_tokens_per_s requested but generation metrics are unavailable')
        return 'gen_tokens_per_s', metrics.gen_tokens_per_s, metrics.gen_time_s
    if primary_metric == 'total':
        if metrics.total_tokens_per_s is None or metrics.total_time_s is None:
            raise RuntimeError('total_tokens_per_s requested but total metrics are unavailable')
        return 'total_tokens_per_s', metrics.total_tokens_per_s, metrics.total_time_s
    raise ValueError(f'Unsupported primary metric: {primary_metric}')


def build_policy_launch(
    policy: str,
    counts_by_chiplet: dict[str, int] | None,
    args: argparse.Namespace,
) -> tuple[list[str], dict[str, str], str, str]:
    base_argv = build_llama_base_argv(args)
    env = build_env(
        removals=['OMP_PLACES', 'OMP_PROC_BIND', 'GOMP_CPU_AFFINITY', 'KMP_AFFINITY']
    )
    if policy == 'eevdf':
        return (
            ['taskset', '-c', EEVDF_WORKLOAD_MASK, *base_argv],
            env,
            f'mask:{EEVDF_WORKLOAD_MASK}',
            'dynamic',
        )

    if counts_by_chiplet is None:
        raise RuntimeError(f'counts_by_chiplet is required for policy={policy}')

    selected_cores = workload_cores_from_counts(counts_by_chiplet)
    taskset_mask = taskset_mask_from_cores(selected_cores)
    workload_core_map = format_workload_core_map(workload_core_map_from_counts(counts_by_chiplet))
    combo_counts = format_combo_counts(counts_by_chiplet)
    argv = ['taskset', '-c', taskset_mask, *base_argv]
    if policy in {'same-chiplet', 'optimal-pinned'} or (
        policy == 'optimal' and args.optimal_strict_pinning
    ):
        argv.extend(['--cpu-mask', cpu_mask_hex_from_cores(selected_cores), '--cpu-strict', '1'])
    return argv, env, workload_core_map, combo_counts


def combos_for_policy(
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
    if policy == 'optimal':
        if sweep_canonical_count_vectors:
            combos: list[tuple[str, dict[str, int] | None]] = []
            for count_vector in CANONICAL_COUNT_VECTORS:
                combos.extend(physical_assignment_combos(count_vector))
            return combos
        if best_combo_map is not None:
            best_row = best_combo_map.get((BENCHMARK_NAME, policy, skewness, rotation))
            if best_row is not None:
                return [(str(best_row['combo_id']), dict(best_row['counts_by_chiplet']))]
        return [('optimal', optimal_counts_for_skew(skew_map))]
    if policy == 'optimal-pinned':
        if sweep_canonical_count_vectors:
            combos: list[tuple[str, dict[str, int] | None]] = []
            for count_vector in CANONICAL_COUNT_VECTORS:
                combos.extend(physical_assignment_combos(count_vector))
            return combos
        if best_combo_map is not None:
            best_row = best_combo_map.get((BENCHMARK_NAME, policy, skewness, rotation))
            if best_row is not None:
                return [(str(best_row['combo_id']), dict(best_row['counts_by_chiplet']))]
        return [('optimal-pinned', optimal_counts_for_skew(skew_map))]
    if policy == 'same-chiplet':
        if same_chiplet_single_combo:
            return [
                (f'same-chiplet-rot{index}', counts_from_sorted_vector(skew_map, count_vector))
                for index, count_vector in enumerate(rotated_count_vectors(CANONICAL_COUNT_VECTORS[0]))
            ]
        combos = [dict(CANONICAL_COUNTS)] if skewness == 0 else same_chiplet_combinations()
        return [(f'combo{index:02d}', counts) for index, counts in enumerate(combos)]
    raise ValueError(f'Unsupported policy: {policy}')


def resolved_combos_for_policy(
    policy: str,
    skewness: int,
    skew_map: dict[str, int],
    *,
    sweep_canonical_count_vectors: bool,
    same_chiplet_single_combo: bool,
    same_chiplet_all_combos_at_skew0: bool,
    rotation: int,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> list[tuple[str, dict[str, int] | None]]:
    if policy != 'same-chiplet' or skewness != 0 or not same_chiplet_all_combos_at_skew0:
        return combos_for_policy(
            policy,
            skewness,
            skew_map,
            sweep_canonical_count_vectors=sweep_canonical_count_vectors,
            same_chiplet_single_combo=same_chiplet_single_combo,
            rotation=rotation,
            best_combo_map=best_combo_map,
        )
    combos = same_chiplet_combinations()
    return [(f'combo{index:02d}', counts) for index, counts in enumerate(combos)]


def planned_run_count(
    args: argparse.Namespace,
    *,
    best_combo_map: dict[tuple[str, str, int, int], dict[str, object]] | None,
) -> int:
    total = 0
    for policy in args.policies:
        for skewness in args.skewnesses:
            rotations = selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation)
            for rotation in rotations:
                combo_count = len(
                    resolved_combos_for_policy(
                        policy,
                        skewness,
                        rotation_skew_map(skewness, rotation),
                        sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                        same_chiplet_single_combo=args.same_chiplet_single_combo,
                        same_chiplet_all_combos_at_skew0=args.same_chiplet_all_combos_at_skew0,
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
    print(f'exp_case4_1_llama output directory: {args.output_dir}')
    print(f'exp_case4_1_llama planned runs: {planned_run_count(args, best_combo_map=best_combo_map)}')
    print(
        'exp_case4_1_llama calibration candidates: '
        + ','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)
    )
    for policy in args.policies:
        for skewness in args.skewnesses:
            for rotation in selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation):
                skew_map = rotation_skew_map(skewness, rotation)
                for combo_id, counts_by_chiplet in resolved_combos_for_policy(
                    policy,
                    skewness,
                    skew_map,
                    sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                    same_chiplet_single_combo=args.same_chiplet_single_combo,
                    same_chiplet_all_combos_at_skew0=args.same_chiplet_all_combos_at_skew0,
                    rotation=rotation,
                    best_combo_map=best_combo_map,
                ):
                    argv, _env, workload_core_map, combo_counts = build_policy_launch(
                        policy,
                        counts_by_chiplet,
                        args,
                    )
                    print(
                        f"[exp_case4_1_llama dry-run] policy={policy} "
                        f"skewness={skewness} rotation={rotation} combo_id={combo_id}"
                    )
                    print(f"  skew_map: {format_skew_map(skew_map)}")
                    print(f"  combo_counts: {combo_counts}")
                    print(f"  workload_core_map: {workload_core_map}")
                    print('  noise_rate_map: <runtime-calibrated>')
                    print(f'  llama: {format_command(argv)}')


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
            throughput = float(row['primary_metric_value'])
            slowdown = baseline / throughput
            final_row['slowdown_vs_skew0'] = f'{slowdown:.6f}'
        else:
            final_row['slowdown_vs_skew0'] = ''
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
        primary_metric_name = str(rows[0]['primary_metric_name'])
        primary_values = [float(row['primary_metric_value']) for row in rows]
        primary_time_values = [float(row['primary_time_s']) for row in rows]
        prompt_values = [float(row['prompt_tokens_per_s']) for row in rows if str(row['prompt_tokens_per_s']).strip()]
        gen_values = [float(row['gen_tokens_per_s']) for row in rows if str(row['gen_tokens_per_s']).strip()]
        total_values = [float(row['total_tokens_per_s']) for row in rows]
        mean_primary_value = mean(primary_values)
        mean_slowdown = ''
        if (benchmark, policy) in baseline_map:
            baseline = baseline_map[(benchmark, policy)]
            mean_slowdown = f'{baseline / mean_primary_value:.6f}'
        summary_rows.append(
            {
                'benchmark': benchmark,
                'policy': policy,
                'skewness': skewness,
                'primary_metric_name': primary_metric_name,
                'runs': len(rows),
                'mean_primary_metric_value': f'{mean_primary_value:.6f}',
                'stdev_primary_metric_value': f'{stdev(primary_values):.6f}',
                'mean_primary_time_s': f'{mean(primary_time_values):.6f}',
                'stdev_primary_time_s': f'{stdev(primary_time_values):.6f}',
                'mean_prompt_tokens_per_s': f'{mean(prompt_values):.6f}' if prompt_values else '',
                'mean_gen_tokens_per_s': f'{mean(gen_values):.6f}' if gen_values else '',
                'mean_total_tokens_per_s': f'{mean(total_values):.6f}',
                'mean_slowdown_vs_skew0': mean_slowdown,
                'mean_ccd0_noise_bandwidth_mbs': f"{mean([float(row['ccd0_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd1_noise_bandwidth_mbs': f"{mean([float(row['ccd1_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd2_noise_bandwidth_mbs': f"{mean([float(row['ccd2_noise_bandwidth_mbs']) for row in rows]):.6f}",
                'mean_ccd3_noise_bandwidth_mbs': f"{mean([float(row['ccd3_noise_bandwidth_mbs']) for row in rows]):.6f}",
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
        primary_values = [float(row['primary_metric_value']) for row in rows]
        primary_time_values = [float(row['primary_time_s']) for row in rows]
        combo_row = {
            'benchmark': benchmark,
            'policy': policy,
            'skewness': skewness,
            'rotation': rotation,
            'combo_id': combo_id,
            'combo_counts': combo_counts,
            'workload_core_map': workload_core_map,
            'runs': len(rows),
            'mean_primary_metric_value': f'{mean(primary_values):.6f}',
            'stdev_primary_metric_value': f'{stdev(primary_values):.6f}',
            'mean_primary_time_s': f'{mean(primary_time_values):.6f}',
            'stdev_primary_time_s': f'{stdev(primary_time_values):.6f}',
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
            -float(row['mean_primary_metric_value']),
            str(row['combo_id']),
        )
    )

    for condition in sorted(
        ranked_by_condition,
        key=lambda key: (key[0], POLICY_ORDER[key[1]], key[2], key[3]),
    ):
        best_row = max(
            ranked_by_condition[condition],
            key=lambda row: (
                float(row['mean_primary_metric_value']),
                -float(row['mean_primary_time_s']),
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
        best_combo_map = load_best_combo_summary(args.optimal_combo_summary.resolve())

    if args.repeats <= 0:
        raise RuntimeError('--repeats must be positive')
    if args.threads != 9:
        raise RuntimeError('exp_case4_1_llama expects --threads 9 to match the 9-thread count-vector placements.')
    if args.primary_metric == 'prompt' and args.n_prompt <= 0:
        raise RuntimeError('--primary-metric prompt requires --n-prompt > 0')
    if args.primary_metric == 'gen' and args.n_gen <= 0:
        raise RuntimeError('--primary-metric gen requires --n-gen > 0')

    require_path(MEMORY_BENCHMARK, 'memory_benchmark', executable=True)
    require_path(LLAMA_BENCH, 'llama-bench', executable=True)
    require_path(args.model.resolve(), 'llama.cpp model')

    if not args.skip_topology_check:
        validate_case4_topology()

    if args.dry_run:
        print_dry_run(args, best_combo_map=best_combo_map)
        return 0

    output_dir = ensure_directory(args.output_dir.resolve())
    raw_dir = ensure_directory(output_dir / 'raw')
    ensure_directory(output_dir / 'configs')
    ensure_directory(output_dir / 'noise_outputs')
    calibration_path = output_dir / 'exp_case4_1_llama_calibration.csv'
    results_path = output_dir / 'exp_case4_1_llama_results.csv'
    summary_path = output_dir / 'exp_case4_1_llama_summary_stats.csv'
    combo_summary_path = output_dir / 'exp_case4_1_llama_combo_summary.csv'
    best_combo_path = output_dir / 'exp_case4_1_llama_best_combos.csv'
    metadata_path = output_dir / 'run_metadata.txt'

    print(f'exp_case4_1_llama output directory: {output_dir}')
    tier_rate_map, calibration_rows = run_case4_calibration(
        output_dir=output_dir,
        raw_dir=raw_dir,
        calibration_duration_seconds=args.calibration_duration_seconds,
        memory_nodes=noise_memory_nodes,
    )
    write_csv_rows(calibration_path, CALIBRATION_FIELDNAMES, calibration_rows)

    results: list[dict[str, object]] = []
    metadata_lines = [
        'experiment=exp_case4_1_llama',
        f'benchmark={BENCHMARK_NAME}',
        f'model={args.model.resolve()}',
        f'threads={args.threads}',
        f'primary_metric={args.primary_metric}',
        f'n_prompt={args.n_prompt}',
        f'n_gen={args.n_gen}',
        f'batch_size={args.batch_size}',
        f'ubatch_size={args.ubatch_size}',
        f'llama_repetitions={args.llama_repetitions}',
        f'n_gpu_layers={args.n_gpu_layers}',
        f'numa={args.numa}',
        f'noise_memory_nodes={",".join(str(node) for node in noise_memory_nodes)}',
        f'fixed_rotation={args.fixed_rotation if args.fixed_rotation is not None else ""}',
        f'optimal_combo_summary={args.optimal_combo_summary.resolve() if args.optimal_combo_summary else ""}',
        f'optimal_strict_pinning={int(args.optimal_strict_pinning)}',
        f'mmap={args.mmap}',
        f'no_warmup={int(args.no_warmup)}',
        f'sweep_canonical_count_vectors={int(args.sweep_canonical_count_vectors)}',
        f'same_chiplet_single_combo={int(args.same_chiplet_single_combo)}',
        f'same_chiplet_all_combos_at_skew0={int(args.same_chiplet_all_combos_at_skew0)}',
        f"policies={','.join(args.policies)}",
        f"skewnesses={','.join(str(skewness) for skewness in args.skewnesses)}",
        f"rotations={','.join(str(rotation) for rotation in args.rotations)}",
        f'repeats={args.repeats}',
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

    for repeat in range(1, args.repeats + 1):
        for policy in args.policies:
            for skewness in args.skewnesses:
                for rotation in selected_rotations_for_skew(skewness, args.rotations, args.fixed_rotation):
                    skew_map = rotation_skew_map(skewness, rotation)
                    noise_rates_by_chiplet = build_noise_rates_by_chiplet(tier_rate_map, skew_map)
                    for combo_id, counts_by_chiplet in resolved_combos_for_policy(
                        policy,
                        skewness,
                        skew_map,
                        sweep_canonical_count_vectors=args.sweep_canonical_count_vectors,
                        same_chiplet_single_combo=args.same_chiplet_single_combo,
                        same_chiplet_all_combos_at_skew0=args.same_chiplet_all_combos_at_skew0,
                        rotation=rotation,
                        best_combo_map=best_combo_map,
                    ):
                        argv, env, workload_core_map, combo_counts = build_policy_launch(
                            policy,
                            counts_by_chiplet,
                            args,
                        )
                        run_id = (
                            f'{BENCHMARK_NAME}__{policy}__skew{skewness}__rot{rotation}'
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
                        llama_log = raw_dir / f'{run_id}__llama.log'
                        noise_argv = [str(MEMORY_BENCHMARK), '--config', str(config_path)]

                        print(
                            f'[exp_case4_1_llama] repeat={repeat} policy={policy} '
                            f'skewness={skewness} rotation={rotation} combo_id={combo_id}'
                        )
                        print(f'  skew_map: {format_skew_map(skew_map)}')
                        print(f'  combo_counts: {combo_counts}')
                        print(f'  noise_rate_map: {format_noise_rate_map(noise_rates_by_chiplet)}')
                        print(f'  noise: {format_command(noise_argv)}')
                        print(f'  llama: {format_command(argv)}')

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
                            result = run_capture(argv, cwd=LLAMA_CPP_ROOT, env=env, log_path=llama_log)
                            metrics = parse_llama_bench_metrics(result.output)
                        finally:
                            if background is not None:
                                elapsed_noise = time.monotonic() - noise_launch_started
                                if elapsed_noise < 1.0:
                                    time.sleep(1.0 - elapsed_noise)
                                stop_background(background)

                        noise_by_chiplet = parse_noise_bandwidth_by_chiplet(noise_log)
                        total_noise = sum(noise_by_chiplet.values())
                        primary_metric_name, primary_metric_value, primary_time_s = select_primary_metric(
                            metrics,
                            args.primary_metric,
                        )
                        print(
                            f'  {primary_metric_name}={primary_metric_value:.6f} '
                            f'primary_time_s={primary_time_s:.6f} '
                            f'total_noise_bandwidth_mbs={total_noise:.6f}'
                        )
                        results.append(
                            {
                                'benchmark': BENCHMARK_NAME,
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
                                'ccd2_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd2']:.6f}",
                                'ccd3_noise_bandwidth_mbs': f"{noise_by_chiplet['ccd3']:.6f}",
                                'total_noise_bandwidth_mbs': f'{total_noise:.6f}',
                                'primary_metric_name': primary_metric_name,
                                'primary_metric_value': f'{primary_metric_value:.6f}',
                                'primary_time_s': f'{primary_time_s:.6f}',
                                'prompt_tokens_per_s': (
                                    f'{metrics.prompt_tokens_per_s:.6f}'
                                    if metrics.prompt_tokens_per_s is not None
                                    else ''
                                ),
                                'gen_tokens_per_s': (
                                    f'{metrics.gen_tokens_per_s:.6f}'
                                    if metrics.gen_tokens_per_s is not None
                                    else ''
                                ),
                                'total_tokens_per_s': f'{metrics.total_tokens_per_s:.6f}',
                                'model_filename': metrics.model_filename or '',
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

    print(f'exp_case4_1_llama results written to {results_path}')
    print(f'exp_case4_1_llama summary stats written to {summary_path}')
    if combo_rows:
        print(f'exp_case4_1_llama combo summary written to {combo_summary_path}')
        print(f'exp_case4_1_llama best combos written to {best_combo_path}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
