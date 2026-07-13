#!/usr/bin/env python3

from __future__ import annotations

import argparse
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
    parse_noise_bandwidth_sum,
    require_path,
    run_capture,
    start_background,
    stdev,
    stop_background,
    validate_chiplet_groups,
    write_csv_rows,
    write_memory_benchmark_config,
)
from exp_case4_common import (  # noqa: E402
    CALIBRATION_RATE_CANDIDATES,
    CHIPLETS,
    GAPBS_DIR,
    MEMORY_BENCHMARK,
    select_calibration_rates,
)


BENCHMARKS = {
    'bfs': GAPBS_DIR / 'bfs',
    'pr': GAPBS_DIR / 'pr',
}
NOISE_NUMA_NODE = 1
CASE_DEFS = {
    'culprit': {'benchmark_cpu': 4, 'membind_node': 1},
    'related-inter': {'benchmark_cpu': 7, 'membind_node': 1},
    'irrelevant-within': {'benchmark_cpu': 4, 'membind_node': 3},
    'irrelevant-inter': {'benchmark_cpu': 7, 'membind_node': 3},
}
CASE_ORDER = {name: index for index, name in enumerate(CASE_DEFS)}
CASE_CHIPLETS = {
    'culprit': 'ccd0',
    'related-inter': 'ccd1',
    'irrelevant-within': 'ccd0',
    'irrelevant-inter': 'ccd1',
}
DEFAULT_LOAD_PCTS = [0, 25, 50, 75, 100]
NOISE_CORES = [0, 1, 2, 3]
NOISE_WORKER_MEMORY_MB = 256
NOISE_RW_MODE = 0
CALIBRATION_DURATION_SECONDS = 3
CALIBRATION_FIELDNAMES = [
    'target_pct',
    'rate',
    'measured_sum_noise_bandwidth_mbs',
]
RESULT_FIELDNAMES = [
    'benchmark',
    'case',
    'benchmark_cpu',
    'membind_node',
    'load_pct',
    'rate',
    'repeat',
    'trial_time_s',
    'slowdown_vs_load0',
    'sum_noise_bandwidth_mbs',
]
SUMMARY_FIELDNAMES = [
    'benchmark',
    'case',
    'benchmark_cpu',
    'membind_node',
    'load_pct',
    'runs',
    'mean_trial_time_s',
    'stdev_trial_time_s',
    'mean_slowdown_vs_load0',
    'mean_sum_noise_bandwidth_mbs',
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='Run exp_case3 single-thread GAPBS culprit sweep.')
    parser.add_argument(
        '--benchmarks',
        nargs='+',
        default=['bfs', 'pr'],
        choices=sorted(BENCHMARKS),
        help='Benchmark subset to run.',
    )
    parser.add_argument(
        '--cases',
        nargs='+',
        default=list(CASE_DEFS),
        choices=list(CASE_DEFS),
        help='Placement cases to run.',
    )
    parser.add_argument(
        '--load-pcts',
        nargs='+',
        type=int,
        default=DEFAULT_LOAD_PCTS,
        choices=DEFAULT_LOAD_PCTS,
        help='Background load percentages to run.',
    )
    parser.add_argument('--repeats', type=int, default=5, help='Outer repeats per condition.')
    parser.add_argument('--graph-scale', type=int, default=20, help='Kronecker graph scale.')
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
        '--output-dir',
        type=Path,
        default=SCRIPT_DIR / 'results' / datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'),
        help='Directory for CSVs, configs, logs, and metadata.',
    )
    parser.add_argument('--skip-topology-check', action='store_true', help='Skip topology validation.')
    parser.add_argument('--dry-run', action='store_true', help='Print commands without executing them.')
    return parser.parse_args()


def read_cpu_numa_mapping() -> dict[int, int]:
    result = run_capture(['lscpu', '-e=CPU,NODE'])
    mapping: dict[int, int] = {}
    for raw_line in result.output.splitlines():
        line = raw_line.strip()
        if not line or line.startswith('CPU '):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        mapping[int(parts[0])] = int(parts[1])
    return mapping


def read_numa_node_sizes_mb() -> dict[int, int]:
    result = run_capture(['numactl', '-H'])
    sizes: dict[int, int] = {}
    for raw_line in result.output.splitlines():
        line = raw_line.strip().replace(':', '')
        if not line.startswith('node '):
            continue
        parts = line.split()
        if len(parts) < 4 or parts[2] != 'size':
            continue
        sizes[int(parts[1])] = int(parts[3])
    return sizes


def validate_exp_case3_topology() -> None:
    validate_chiplet_groups([CHIPLETS['ccd0'], CHIPLETS['ccd1']])
    node_mapping = read_cpu_numa_mapping()
    numa_sizes = read_numa_node_sizes_mb()

    for case_name, expected_chiplet in CASE_CHIPLETS.items():
        benchmark_cpu = CASE_DEFS[case_name]['benchmark_cpu']
        if benchmark_cpu not in CHIPLETS[expected_chiplet]:
            raise RuntimeError(
                f'Expected case {case_name} CPU {benchmark_cpu} to stay within {expected_chiplet}, '
                f'but {benchmark_cpu} is outside {CHIPLETS[expected_chiplet]}'
            )
        if benchmark_cpu not in node_mapping:
            raise RuntimeError(f'CPU {benchmark_cpu} was not present in lscpu NUMA mapping.')

    required_memory_nodes = {NOISE_NUMA_NODE}
    required_memory_nodes.update(int(case_def['membind_node']) for case_def in CASE_DEFS.values())
    for node in sorted(required_memory_nodes):
        if node not in numa_sizes:
            raise RuntimeError(f'NUMA node {node} was not present in numactl -H output.')
        if numa_sizes[node] <= 0:
            raise RuntimeError(f'NUMA node {node} has no memory attached (size={numa_sizes[node]} MB).')


def build_gapbs_argv(
    benchmark: str,
    benchmark_cpu: int,
    membind_node: int,
    graph_scale: int,
    pr_iterations: int,
) -> tuple[list[str], dict[str, str]]:
    benchmark_path = BENCHMARKS[benchmark]
    benchmark_argv = [str(benchmark_path), '-g', str(graph_scale), '-n1']
    if benchmark == 'pr':
        benchmark_argv.extend(['-i', str(pr_iterations)])
    env = build_env(
        updates={'OMP_NUM_THREADS': '1'},
        removals=['OMP_PLACES', 'OMP_PROC_BIND', 'GOMP_CPU_AFFINITY', 'KMP_AFFINITY'],
    )
    argv = [
        'numactl',
        f'--physcpubind={benchmark_cpu}',
        f'--membind={membind_node}',
        *benchmark_argv,
    ]
    return argv, env


def write_noise_config(
    output_dir: Path,
    *,
    name: str,
    rate: int,
    duration_seconds: int,
) -> Path:
    config_path = output_dir / 'configs' / f'{name}.xml'
    noise_output_path = output_dir / 'noise_outputs' / f'{name}.txt'
    write_memory_benchmark_config(
        config_path,
        nthreads=len(NOISE_CORES),
        cores=NOISE_CORES,
        rates=[rate] * len(NOISE_CORES),
        worker_memory_mb=NOISE_WORKER_MEMORY_MB,
        output_path=noise_output_path,
        modes=[NOISE_RW_MODE] * len(NOISE_CORES),
        numa_nodes=[NOISE_NUMA_NODE] * len(NOISE_CORES),
        duration_seconds=duration_seconds,
        worker_numa_policy='bind',
    )
    return config_path


def run_calibration(
    *,
    output_dir: Path,
    raw_dir: Path,
    noise_time_seconds: int,
    calibration_duration_seconds: int,
) -> tuple[dict[int, int], list[dict[str, object]]]:
    measured_rows: list[dict[str, float]] = []
    for rate in CALIBRATION_RATE_CANDIDATES:
        name = f'calibration__rate{rate}'
        config_path = write_noise_config(
            output_dir,
            name=name,
            rate=rate,
            duration_seconds=calibration_duration_seconds,
        )
        log_path = raw_dir / f'{name}__noise.log'
        run_capture([str(MEMORY_BENCHMARK), '--config', str(config_path)], log_path=log_path)
        measured_rows.append(
            {
                'rate': float(rate),
                'measured_sum_noise_bandwidth_mbs': parse_noise_bandwidth_sum(log_path),
            }
        )

    selected_rows = select_calibration_rates(
        measured_rows,
        bandwidth_field='measured_sum_noise_bandwidth_mbs',
    )
    load_rate_map = {0: 0}
    calibration_rows: list[dict[str, object]] = [
        {
            'target_pct': 0,
            'rate': 0,
            'measured_sum_noise_bandwidth_mbs': '0.000000',
        }
    ]
    for target_pct in [25, 50, 75, 100]:
        selected = selected_rows[target_pct]
        rate = int(selected['rate'])
        load_rate_map[target_pct] = rate
        calibration_rows.append(
            {
                'target_pct': target_pct,
                'rate': rate,
                'measured_sum_noise_bandwidth_mbs': f"{selected['measured_sum_noise_bandwidth_mbs']:.6f}",
            }
        )
        write_noise_config(
            output_dir,
            name=f'load{target_pct}__rate{rate}',
            rate=rate,
            duration_seconds=noise_time_seconds,
        )
    return load_rate_map, calibration_rows


def build_conditions(args: argparse.Namespace) -> list[tuple[str, str, int]]:
    return [
        (benchmark, case_name, load_pct)
        for benchmark in args.benchmarks
        for case_name in args.cases
        for load_pct in args.load_pcts
    ]


def print_dry_run(args: argparse.Namespace, conditions: list[tuple[str, str, int]]) -> None:
    print(f'exp_case3 output directory: {args.output_dir}')
    print(f'exp_case3 condition count: {len(conditions)}')
    print(f'exp_case3 repeats per condition: {args.repeats}')
    if any(load_pct > 0 for load_pct in args.load_pcts):
        print(
            'exp_case3 calibration candidates: '
            + ','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)
        )
    for benchmark, case_name, load_pct in conditions:
        case_def = CASE_DEFS[case_name]
        argv, _env = build_gapbs_argv(
            benchmark,
            case_def['benchmark_cpu'],
            case_def['membind_node'],
            args.graph_scale,
            args.pr_iterations,
        )
        print(
            f"[exp_case3 dry-run] benchmark={benchmark} case={case_name} "
            f"cpu={case_def['benchmark_cpu']} membind={case_def['membind_node']} load_pct={load_pct}"
        )
        if load_pct > 0:
            print(
                '  noise: '
                + format_command(
                    [
                        str(MEMORY_BENCHMARK),
                        '--config',
                        str(args.output_dir / 'configs' / f'load{load_pct}__rateCALIBRATED.xml'),
                    ]
                )
            )
        print(f'  gapbs: {format_command(argv)}')


def summarize_results(results: list[dict[str, object]]) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    baseline_map: dict[tuple[str, str], float] = {}
    grouped_zero: defaultdict[tuple[str, str], list[float]] = defaultdict(list)
    for row in results:
        if int(row['load_pct']) == 0:
            grouped_zero[(str(row['benchmark']), str(row['case']))].append(float(row['trial_time_s']))

    for key, values in grouped_zero.items():
        baseline_map[key] = mean(values)

    final_rows: list[dict[str, object]] = []
    for row in results:
        baseline_key = (str(row['benchmark']), str(row['case']))
        if baseline_key not in baseline_map:
            raise RuntimeError(f'Missing 0-load baseline for {baseline_key}')
        slowdown = float(row['trial_time_s']) / baseline_map[baseline_key]
        final_row = dict(row)
        final_row['slowdown_vs_load0'] = f'{slowdown:.6f}'
        final_rows.append(final_row)

    final_rows.sort(
        key=lambda row: (
            str(row['benchmark']),
            CASE_ORDER[str(row['case'])],
            int(row['load_pct']),
            int(row['repeat']),
        )
    )

    grouped_rows: defaultdict[tuple[str, str, int], list[dict[str, object]]] = defaultdict(list)
    for row in final_rows:
        grouped_rows[(str(row['benchmark']), str(row['case']), int(row['load_pct']))].append(row)

    summary_rows: list[dict[str, object]] = []
    for benchmark, case_name, load_pct in sorted(
        grouped_rows,
        key=lambda key: (key[0], CASE_ORDER[key[1]], key[2]),
    ):
        rows = grouped_rows[(benchmark, case_name, load_pct)]
        trial_values = [float(row['trial_time_s']) for row in rows]
        slowdown_values = [float(row['slowdown_vs_load0']) for row in rows]
        noise_values = [float(row['sum_noise_bandwidth_mbs']) for row in rows]
        case_def = CASE_DEFS[case_name]
        summary_rows.append(
            {
                'benchmark': benchmark,
                'case': case_name,
                'benchmark_cpu': case_def['benchmark_cpu'],
                'membind_node': case_def['membind_node'],
                'load_pct': load_pct,
                'runs': len(rows),
                'mean_trial_time_s': f'{mean(trial_values):.6f}',
                'stdev_trial_time_s': f'{stdev(trial_values):.6f}',
                'mean_slowdown_vs_load0': f'{mean(slowdown_values):.6f}',
                'mean_sum_noise_bandwidth_mbs': f'{mean(noise_values):.6f}',
            }
        )
    return final_rows, summary_rows


def main() -> int:
    args = parse_args()

    if args.repeats <= 0:
        raise RuntimeError('--repeats must be positive')
    if any(load_pct > 0 for load_pct in args.load_pcts) and 0 not in args.load_pcts:
        raise RuntimeError('A 0-load baseline is required when any non-zero load is selected.')

    require_path(MEMORY_BENCHMARK, 'memory_benchmark', executable=True)
    for benchmark in args.benchmarks:
        require_path(BENCHMARKS[benchmark], f'GAPBS {benchmark}', executable=True)

    if not args.skip_topology_check:
        validate_exp_case3_topology()

    conditions = build_conditions(args)
    if args.dry_run:
        print_dry_run(args, conditions)
        return 0

    output_dir = ensure_directory(args.output_dir.resolve())
    raw_dir = ensure_directory(output_dir / 'raw')
    ensure_directory(output_dir / 'configs')
    ensure_directory(output_dir / 'noise_outputs')
    calibration_path = output_dir / 'exp_case3_calibration.csv'
    results_path = output_dir / 'exp_case3_results.csv'
    summary_path = output_dir / 'exp_case3_summary_stats.csv'
    metadata_path = output_dir / 'run_metadata.txt'

    print(f'exp_case3 output directory: {output_dir}')

    load_rate_map = {0: 0}
    calibration_rows: list[dict[str, object]] = []
    if any(load_pct > 0 for load_pct in args.load_pcts):
        load_rate_map, calibration_rows = run_calibration(
            output_dir=output_dir,
            raw_dir=raw_dir,
            noise_time_seconds=args.noise_time_seconds,
            calibration_duration_seconds=args.calibration_duration_seconds,
        )
        write_csv_rows(calibration_path, CALIBRATION_FIELDNAMES, calibration_rows)

    results: list[dict[str, object]] = []
    metadata_lines = [
        'experiment=exp_case3',
        'workload_family=gapbs',
        'graph_source=kronecker',
        f'graph_scale={args.graph_scale}',
        f'pr_iterations={args.pr_iterations}',
        f"benchmarks={','.join(args.benchmarks)}",
        f"cases={','.join(args.cases)}",
        f"load_pcts={','.join(str(load_pct) for load_pct in args.load_pcts)}",
        f'repeats={args.repeats}',
        f"noise_cores={','.join(str(core) for core in NOISE_CORES)}",
        'noise_worker_memory_mb=256',
        'noise_mode=0',
        'noise_worker_numa_policy=bind',
        f'noise_numa_node={NOISE_NUMA_NODE}',
        f"calibration_rate_candidates={','.join(str(rate) for rate in CALIBRATION_RATE_CANDIDATES)}",
        f'settle_seconds={args.settle_seconds}',
        f'cooldown_seconds={args.cooldown_seconds}',
        f'noise_time_seconds={args.noise_time_seconds}',
        'case_bindings='
        + ';'.join(
            f"{case_name}:cpu{CASE_DEFS[case_name]['benchmark_cpu']}/membind{CASE_DEFS[case_name]['membind_node']}"
            for case_name in args.cases
        ),
    ]
    if calibration_rows:
        metadata_lines.append(
            'selected_load_rates='
            + ','.join(
                f"{row['target_pct']}:{row['rate']}"
                for row in calibration_rows
                if int(row['target_pct']) > 0
            )
        )

    for repeat in range(1, args.repeats + 1):
        for benchmark, case_name, load_pct in conditions:
            case_def = CASE_DEFS[case_name]
            argv, env = build_gapbs_argv(
                benchmark,
                case_def['benchmark_cpu'],
                case_def['membind_node'],
                args.graph_scale,
                args.pr_iterations,
            )
            rate = load_rate_map[load_pct]
            run_prefix = f'{benchmark}__{case_name}__load{load_pct}__r{repeat}'
            gapbs_log = raw_dir / f'{run_prefix}__gapbs.log'
            noise_log: Path | None = None
            background = None
            sum_noise_bandwidth = 0.0

            print(
                f'[exp_case3] repeat={repeat} benchmark={benchmark} case={case_name} '
                f"load_pct={load_pct} rate={'none' if load_pct == 0 else rate}"
            )
            if load_pct > 0:
                config_path = output_dir / 'configs' / f'load{load_pct}__rate{rate}.xml'
                noise_log = raw_dir / f'{run_prefix}__noise.log'
                noise_argv = [str(MEMORY_BENCHMARK), '--config', str(config_path)]
                print(f'  noise: {format_command(noise_argv)}')
                background = start_background(noise_argv, log_path=noise_log)
                time.sleep(args.settle_seconds)

            print(f'  gapbs: {format_command(argv)}')
            try:
                result = run_capture(argv, cwd=GAPBS_DIR, env=env, log_path=gapbs_log)
                trial_time = parse_gapbs_trial_time(result.output)
            finally:
                if background is not None:
                    stop_background(background)
                    if noise_log is not None:
                        sum_noise_bandwidth = parse_noise_bandwidth_sum(noise_log)

            print(
                f'  trial_time_s={trial_time:.6f} '
                f'sum_noise_bandwidth_mbs={sum_noise_bandwidth:.6f}'
            )
            results.append(
                {
                    'benchmark': benchmark,
                    'case': case_name,
                    'benchmark_cpu': case_def['benchmark_cpu'],
                    'membind_node': case_def['membind_node'],
                    'load_pct': load_pct,
                    'rate': '' if load_pct == 0 else rate,
                    'repeat': repeat,
                    'trial_time_s': f'{trial_time:.6f}',
                    'slowdown_vs_load0': '',
                    'sum_noise_bandwidth_mbs': f'{sum_noise_bandwidth:.6f}',
                }
            )
            time.sleep(args.cooldown_seconds)

    final_rows, summary_rows = summarize_results(results)
    write_csv_rows(results_path, RESULT_FIELDNAMES, final_rows)
    write_csv_rows(summary_path, SUMMARY_FIELDNAMES, summary_rows)
    metadata_path.write_text('\n'.join(metadata_lines) + '\n')

    print(f'exp_case3 results written to {results_path}')
    print(f'exp_case3 summary stats written to {summary_path}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
