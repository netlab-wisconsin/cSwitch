#!/usr/bin/env python3

from __future__ import annotations

import csv
import hashlib
import itertools
import re
import time
from pathlib import Path
from typing import Sequence

from experiment_utils import (
    ensure_directory,
    run_capture,
    validate_chiplet_groups,
    write_memory_benchmark_config,
)


GAPBS_DIR = Path('/home/seunghyun/gapbs/gapbs')
MEMORY_BENCHMARK = Path('/home/seunghyun/sched_bench/build/memory_benchmark')
NPB_EP_B = Path('/home/seunghyun/benchmarks/npb/NPB3.4.4/NPB3.4-OMP/bin/ep.B.x')
EMULATED_EEVDF_BINARY = Path('/home/seunghyun/scx_rustland_eevdf/target/release/scx-rustland-eevdf')
OUR_SCHEDULER_BINARY = Path('/home/seunghyun/gapbs/scx_rustland_la/target/release/scx-rustland-la')
CCM_MAPPING_PATH = Path('/home/seunghyun/gapbs/profiler/ccm_mapping.txt')
CHIPLET_ORDER = ['ccd0', 'ccd1', 'ccd3', 'ccd4']
CHIPLETS = {
    'ccd0': [0, 1, 2, 3, 4, 5, 6],
    'ccd1': [7, 8, 9, 10, 11, 12, 13],
    'ccd3': [21, 22, 23, 24, 25, 26, 27],
    'ccd4': [28, 29, 30, 31, 32, 33, 34],
}
WORKLOAD_CORES = {
    'ccd0': [0, 1, 2, 3],
    'ccd1': [7, 8, 9, 10],
    'ccd3': [21, 22, 23, 24],
    'ccd4': [28, 29, 30, 31],
}
NOISE_CORES = {
    'ccd0': [4, 5, 6],
    'ccd1': [11, 12, 13],
    'ccd3': [25, 26, 27],
    'ccd4': [32, 33, 34],
}
CORE_TO_CHIPLET = {
    core: chiplet
    for chiplet, cores in CHIPLETS.items()
    for core in cores
}
EEVDF_WORKLOAD_MASK = '0-3,7-10,21-24,28-31'
DEFAULT_SKEWNESS_LEVELS = [0, 25, 50, 75, 100]
DEFAULT_ROTATIONS = [0, 1, 2, 3]
INTERLEAVE_NODES = list(range(12))
SKEWNESS_PATTERNS = {
    0: [50, 50, 50, 50],
    25: [40, 45, 55, 60],
    50: [30, 40, 60, 70],
    75: [20, 35, 65, 80],
    100: [0, 25, 75, 100],
}
COUNT_PATTERN = (4, 4, 1, 0)
CANONICAL_COUNTS = {
    'ccd0': 4,
    'ccd1': 4,
    'ccd3': 0,
    'ccd4': 1,
}
CALIBRATION_RATE_CANDIDATES = [1000, 500, 220, 150, 50, 0]
NOISE_WORKER_MEMORY_MB = 256
NOISE_RW_MODE = 0
CALIBRATION_DURATION_SECONDS = 3
SCHEDULER_POLICY_BINARIES = {
    'eevdf-emulated': EMULATED_EEVDF_BINARY,
    'la-default': OUR_SCHEDULER_BINARY,
}
NOISE_BANDWIDTH_RE = re.compile(
    r'Core\s+(-?\d+)\s+->\s+NUMA(?:\s+interleave\s+\[[^\]]+\]|\s+\d+)\).*?Bandwidth:\s*([0-9]+(?:\.[0-9]+)?)\s*MB/s'
)


def chiplet_l3(mapping: dict[int, int], chiplet: str) -> int:
    return mapping[CHIPLETS[chiplet][0]]


def validate_case4_topology() -> None:
    mapping = validate_chiplet_groups([CHIPLETS[name] for name in CHIPLET_ORDER])
    for chiplet in CHIPLET_ORDER:
        workload = WORKLOAD_CORES[chiplet]
        noise = NOISE_CORES[chiplet]
        if len(workload) != 4:
            raise RuntimeError(f'{chiplet} workload pool must contain 4 cores, got {workload}')
        if len(noise) != 3:
            raise RuntimeError(f'{chiplet} noise pool must contain 3 cores, got {noise}')
        if set(workload) & set(noise):
            raise RuntimeError(f'Workload/noise core overlap detected for {chiplet}.')
        if sorted(workload + noise) != CHIPLETS[chiplet]:
            raise RuntimeError(f'{chiplet} workload/noise pools do not cover the full chiplet.')

        expected_l3 = chiplet_l3(mapping, chiplet)
        if {mapping[core] for core in workload} != {expected_l3}:
            raise RuntimeError(f'Workload cores for {chiplet} do not stay within one L3 domain.')
        if {mapping[core] for core in noise} != {expected_l3}:
            raise RuntimeError(f'Noise cores for {chiplet} do not stay within one L3 domain.')


def rotation_ids_for_skew(skewness: int, rotations: Sequence[int]) -> list[int]:
    if skewness not in SKEWNESS_PATTERNS:
        raise ValueError(f'Unsupported skewness: {skewness}')
    if skewness == 0:
        return [0]
    ordered: list[int] = []
    for rotation in rotations:
        if rotation not in DEFAULT_ROTATIONS:
            raise ValueError(f'Unsupported rotation: {rotation}')
        if rotation not in ordered:
            ordered.append(rotation)
    return ordered


def rotation_skew_map(skewness: int, rotation: int) -> dict[str, int]:
    if skewness not in SKEWNESS_PATTERNS:
        raise ValueError(f'Unsupported skewness: {skewness}')
    if rotation not in DEFAULT_ROTATIONS:
        raise ValueError(f'Unsupported rotation: {rotation}')
    pattern = SKEWNESS_PATTERNS[skewness]
    return {
        chiplet: pattern[(index - rotation) % len(pattern)]
        for index, chiplet in enumerate(CHIPLET_ORDER)
    }


def format_skew_map(skew_map: dict[str, int]) -> str:
    return ','.join(f'{chiplet}={skew_map[chiplet]}' for chiplet in CHIPLET_ORDER)


def format_noise_rate_map(noise_rates_by_chiplet: dict[str, int]) -> str:
    return ';'.join(f'{chiplet}:{noise_rates_by_chiplet[chiplet]}' for chiplet in CHIPLET_ORDER)


def format_workload_core_map(workload_core_map: dict[str, list[int]]) -> str:
    return ';'.join(
        f"{chiplet}:{','.join(str(core) for core in workload_core_map[chiplet])}"
        for chiplet in CHIPLET_ORDER
    )


def format_combo_counts(counts_by_chiplet: dict[str, int]) -> str:
    return ','.join(f'{chiplet}={counts_by_chiplet[chiplet]}' for chiplet in CHIPLET_ORDER)


def taskset_mask_from_cores(cores: Sequence[int]) -> str:
    return ','.join(str(core) for core in cores)


def omp_places_from_cores(cores: Sequence[int]) -> str:
    return ','.join(f'{{{core}}}' for core in cores)


def workload_core_map_from_counts(counts_by_chiplet: dict[str, int]) -> dict[str, list[int]]:
    return {
        chiplet: list(WORKLOAD_CORES[chiplet][:counts_by_chiplet[chiplet]])
        for chiplet in CHIPLET_ORDER
    }


def workload_cores_from_counts(counts_by_chiplet: dict[str, int]) -> list[int]:
    workload_core_map = workload_core_map_from_counts(counts_by_chiplet)
    cores: list[int] = []
    for chiplet in CHIPLET_ORDER:
        count = counts_by_chiplet[chiplet]
        if count < 0 or count > len(WORKLOAD_CORES[chiplet]):
            raise ValueError(f'Invalid workload count for {chiplet}: {count}')
        cores.extend(workload_core_map[chiplet])
    if len(cores) != 9:
        raise RuntimeError(f'Expected exactly 9 workload cores, got {cores}')
    return cores


def optimal_counts_for_skew(skew_map: dict[str, int]) -> dict[str, int]:
    ordered_chiplets = sorted(CHIPLET_ORDER, key=lambda chiplet: (skew_map[chiplet], chiplet))
    counts_by_chiplet = {chiplet: 0 for chiplet in CHIPLET_ORDER}
    for chiplet, count in zip(ordered_chiplets, COUNT_PATTERN, strict=True):
        counts_by_chiplet[chiplet] = count
    return counts_by_chiplet


def rotated_count_vectors(
    count_vector: tuple[int, int, int, int],
) -> list[tuple[int, int, int, int]]:
    rotations: list[tuple[int, int, int, int]] = []
    seen: set[tuple[int, int, int, int]] = set()
    for offset in range(len(count_vector)):
        rotated = count_vector[offset:] + count_vector[:offset]
        if rotated in seen:
            continue
        seen.add(rotated)
        rotations.append(rotated)
    return rotations


def parse_combo_counts(text: str) -> dict[str, int]:
    counts_by_chiplet: dict[str, int] = {}
    for field in text.split(','):
        if not field.strip():
            continue
        chiplet, value = field.split('=', 1)
        chiplet = chiplet.strip()
        if chiplet not in CHIPLET_ORDER:
            raise ValueError(f'Unexpected chiplet in combo counts: {chiplet}')
        counts_by_chiplet[chiplet] = int(value)
    missing = [chiplet for chiplet in CHIPLET_ORDER if chiplet not in counts_by_chiplet]
    if missing:
        raise ValueError(f'Missing chiplets in combo counts {text!r}: {missing}')
    return counts_by_chiplet


def is_scheduler_managed_policy(policy: str) -> bool:
    return policy in SCHEDULER_POLICY_BINARIES


def build_scheduler_managed_argv(
    policy: str,
    *,
    inner_argv: Sequence[str],
    workload_env: dict[str, str] | None,
    output_dir: Path,
    run_id: str,
) -> list[str]:
    if policy not in SCHEDULER_POLICY_BINARIES:
        raise ValueError(f'Unsupported scheduler-managed policy: {policy}')

    binary = SCHEDULER_POLICY_BINARIES[policy]
    scheduler_dir = ensure_directory(output_dir / 'scheduler_logs')
    run_hash = hashlib.sha1(run_id.encode('utf-8')).hexdigest()[:12]
    slug = re.sub(r'[^A-Za-z0-9_.-]+', '-', run_id).strip('-')[:64] or 'run'
    decision_log = scheduler_dir / f'{slug}__decision.log'
    runtime_log = scheduler_dir / f'{slug}__runtime.log'
    cgroup_path = f'/sys/fs/cgroup/exp3-{policy}-{run_hash}'
    wrapped_inner_argv = list(inner_argv)
    if workload_env:
        wrapped_inner_argv = [
            'env',
            *[f'{key}={value}' for key, value in sorted(workload_env.items())],
            *wrapped_inner_argv,
        ]
    return [
        'sudo',
        '-n',
        str(binary),
        '--cgroup-path',
        cgroup_path,
        '--ccm-mapping-path',
        str(CCM_MAPPING_PATH),
        '--monitor',
        '0.5',
        '--decision-log-path',
        str(decision_log),
        '--runtime-log-path',
        str(runtime_log),
        '--',
        *wrapped_inner_argv,
    ]


def load_best_combo_summary(summary_path: Path) -> dict[tuple[str, str, int, int], dict[str, object]]:
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
            metric = float(row['mean_primary_metric_value'])
            time_field = row.get('mean_primary_time_s') or row.get('mean_npb_time_s') or ''
            mean_time = float(time_field) if str(time_field).strip() else float('inf')
            candidate = {
                'combo_id': row['combo_id'],
                'combo_counts': row['combo_counts'],
                'counts_by_chiplet': parse_combo_counts(row['combo_counts']),
                'workload_core_map': row.get('workload_core_map', ''),
                'mean_primary_metric_value': metric,
                'mean_primary_time_s': mean_time,
            }
            current = best_rows.get(key)
            if current is None:
                best_rows[key] = candidate
                continue
            if metric > float(current['mean_primary_metric_value']):
                best_rows[key] = candidate
                continue
            if metric == float(current['mean_primary_metric_value']):
                if mean_time < float(current['mean_primary_time_s']):
                    best_rows[key] = candidate
                    continue
                if mean_time == float(current['mean_primary_time_s']) and row['combo_id'] < str(current['combo_id']):
                    best_rows[key] = candidate
    return best_rows


def same_chiplet_combinations() -> list[dict[str, int]]:
    combinations: list[dict[str, int]] = []
    seen: set[tuple[int, ...]] = set()
    for perm in itertools.permutations(COUNT_PATTERN):
        if perm in seen:
            continue
        seen.add(perm)
        combinations.append({chiplet: perm[index] for index, chiplet in enumerate(CHIPLET_ORDER)})
    combinations.sort(key=lambda counts: tuple(counts[chiplet] for chiplet in CHIPLET_ORDER))
    return combinations


def flatten_noise_assignment(noise_rates_by_chiplet: dict[str, int]) -> tuple[list[int], list[int]]:
    flat_cores: list[int] = []
    flat_rates: list[int] = []
    for chiplet in CHIPLET_ORDER:
        flat_cores.extend(NOISE_CORES[chiplet])
        flat_rates.extend([noise_rates_by_chiplet[chiplet]] * len(NOISE_CORES[chiplet]))
    return flat_cores, flat_rates


def build_noise_rates_by_chiplet(
    tier_rate_map: dict[int, int],
    skew_map: dict[str, int],
) -> dict[str, int]:
    tier_points = sorted(tier_rate_map)
    noise_rates: dict[str, int] = {}
    for chiplet in CHIPLET_ORDER:
        target_pct = skew_map[chiplet]
        if target_pct in tier_rate_map:
            noise_rates[chiplet] = tier_rate_map[target_pct]
            continue
        if target_pct < tier_points[0] or target_pct > tier_points[-1]:
            raise RuntimeError(f'Skew tier {target_pct} is outside calibrated range {tier_points[0]}-{tier_points[-1]}')

        lower_tier = max(point for point in tier_points if point < target_pct)
        upper_tier = min(point for point in tier_points if point > target_pct)
        lower_rate = tier_rate_map[lower_tier]
        upper_rate = tier_rate_map[upper_tier]
        weight = (target_pct - lower_tier) / float(upper_tier - lower_tier)
        interpolated_rate = lower_rate + (upper_rate - lower_rate) * weight
        noise_rates[chiplet] = int(round(interpolated_rate))
    return noise_rates


def write_case4_noise_config(
    output_dir: Path,
    *,
    name: str,
    noise_rates_by_chiplet: dict[str, int],
    duration_seconds: int,
    memory_nodes: Sequence[int] | None = None,
) -> tuple[Path, Path]:
    config_path = output_dir / 'configs' / f'{name}.xml'
    output_path = output_dir / 'noise_outputs' / f'{name}.txt'
    flat_cores, flat_rates = flatten_noise_assignment(noise_rates_by_chiplet)
    selected_nodes = list(memory_nodes or INTERLEAVE_NODES)
    if not selected_nodes:
        raise ValueError('memory_nodes must not be empty')
    worker_numa_policy = 'bind' if len(selected_nodes) == 1 else 'interleave'
    interleave_nodes = selected_nodes if worker_numa_policy == 'interleave' else None
    write_memory_benchmark_config(
        config_path,
        nthreads=len(flat_cores),
        cores=flat_cores,
        rates=flat_rates,
        worker_memory_mb=NOISE_WORKER_MEMORY_MB,
        output_path=output_path,
        modes=[NOISE_RW_MODE] * len(flat_cores),
        numa_nodes=[selected_nodes[0]] * len(flat_cores),
        duration_seconds=duration_seconds,
        worker_numa_policy=worker_numa_policy,
        worker_numa_interleave_nodes=interleave_nodes,
    )
    return config_path, output_path


def write_case4_calibration_config(
    output_dir: Path,
    *,
    name: str,
    rate: int,
    duration_seconds: int,
    memory_nodes: Sequence[int] | None = None,
) -> tuple[Path, Path]:
    per_chiplet_rates = {chiplet: rate for chiplet in CHIPLET_ORDER}
    return write_case4_noise_config(
        output_dir,
        name=name,
        noise_rates_by_chiplet=per_chiplet_rates,
        duration_seconds=duration_seconds,
        memory_nodes=memory_nodes,
    )


def parse_noise_bandwidth_by_chiplet(log_path: Path) -> dict[str, float]:
    totals = {chiplet: 0.0 for chiplet in CHIPLET_ORDER}
    if not log_path.exists():
        return totals

    for raw_line in log_path.read_text().splitlines():
        match = NOISE_BANDWIDTH_RE.search(raw_line)
        if not match:
            continue
        core = int(match.group(1))
        bandwidth = float(match.group(2))
        chiplet = CORE_TO_CHIPLET.get(core)
        if chiplet is None:
            continue
        totals[chiplet] += bandwidth
    return totals


def wait_for_noise_ready(log_path: Path, *, timeout_seconds: float) -> bool:
    deadline = time.monotonic() + max(timeout_seconds, 0.0)
    ready_markers = ('Running bandwidth measurement for', 'Running latency measurement for')
    while time.monotonic() < deadline:
        if log_path.exists():
            text = log_path.read_text(errors='ignore')
            if any(marker in text for marker in ready_markers):
                return True
        time.sleep(0.05)

    if log_path.exists():
        text = log_path.read_text(errors='ignore')
        return any(marker in text for marker in ready_markers)
    return False


def select_calibration_rates(
    measured_rows: list[dict[str, float]],
    *,
    bandwidth_field: str,
) -> dict[int, dict[str, float]]:
    if len(measured_rows) < 4:
        raise RuntimeError('Need at least four calibration rows to map load percentages.')

    ordered = sorted(measured_rows, key=lambda row: (row[bandwidth_field], row['rate']))
    max_bw = ordered[-1][bandwidth_field]
    if max_bw <= 0.0:
        raise RuntimeError('Calibration produced zero bandwidth for all candidate rates.')

    selected: dict[int, dict[str, float]] = {}
    start_index = 0
    for offset, target_pct in enumerate([25, 50, 75]):
        future_slots = 3 - offset
        end_index = len(ordered) - future_slots
        candidate_indices = list(range(start_index, end_index))
        if not candidate_indices:
            candidate_indices = [start_index]
        target_bw = max_bw * (target_pct / 100.0)
        chosen_index = min(
            candidate_indices,
            key=lambda index: abs(ordered[index][bandwidth_field] - target_bw),
        )
        selected[target_pct] = ordered[chosen_index]
        start_index = chosen_index + 1

    selected[100] = ordered[-1]
    return selected


def run_case4_calibration(
    *,
    output_dir: Path,
    raw_dir: Path,
    calibration_duration_seconds: int = CALIBRATION_DURATION_SECONDS,
    memory_nodes: Sequence[int] | None = None,
) -> tuple[dict[int, int], list[dict[str, object]]]:
    measured_rows: list[dict[str, float]] = []
    for rate in CALIBRATION_RATE_CANDIDATES:
        name = f'calibration__rate{rate}'
        config_path, _output_path = write_case4_calibration_config(
            output_dir,
            name=name,
            rate=rate,
            duration_seconds=calibration_duration_seconds,
            memory_nodes=memory_nodes,
        )
        log_path = raw_dir / f'{name}__noise.log'
        run_capture([str(MEMORY_BENCHMARK), '--config', str(config_path)], log_path=log_path)
        bandwidths = parse_noise_bandwidth_by_chiplet(log_path)
        measured_rows.append(
            {
                'rate': float(rate),
                'measured_total_noise_bandwidth_mbs': sum(bandwidths.values()),
                'measured_ccd0_noise_bandwidth_mbs': bandwidths['ccd0'],
                'measured_ccd1_noise_bandwidth_mbs': bandwidths['ccd1'],
                'measured_ccd3_noise_bandwidth_mbs': bandwidths['ccd3'],
                'measured_ccd4_noise_bandwidth_mbs': bandwidths['ccd4'],
            }
        )

    selected_rows = select_calibration_rates(
        measured_rows,
        bandwidth_field='measured_total_noise_bandwidth_mbs',
    )
    tier_rate_map = {0: 0}
    calibration_rows: list[dict[str, object]] = [
        {
            'target_pct': 0,
            'rate': 0,
            'measured_total_noise_bandwidth_mbs': '0.000000',
            'measured_ccd0_noise_bandwidth_mbs': '0.000000',
            'measured_ccd1_noise_bandwidth_mbs': '0.000000',
            'measured_ccd3_noise_bandwidth_mbs': '0.000000',
            'measured_ccd4_noise_bandwidth_mbs': '0.000000',
        }
    ]
    for target_pct in [25, 50, 75, 100]:
        selected = selected_rows[target_pct]
        rate = int(selected['rate'])
        tier_rate_map[target_pct] = rate
        calibration_rows.append(
            {
                'target_pct': target_pct,
                'rate': rate,
                'measured_total_noise_bandwidth_mbs': f"{selected['measured_total_noise_bandwidth_mbs']:.6f}",
                'measured_ccd0_noise_bandwidth_mbs': f"{selected['measured_ccd0_noise_bandwidth_mbs']:.6f}",
                'measured_ccd1_noise_bandwidth_mbs': f"{selected['measured_ccd1_noise_bandwidth_mbs']:.6f}",
                'measured_ccd3_noise_bandwidth_mbs': f"{selected['measured_ccd3_noise_bandwidth_mbs']:.6f}",
                'measured_ccd4_noise_bandwidth_mbs': f"{selected['measured_ccd4_noise_bandwidth_mbs']:.6f}",
            }
        )
    return tier_rate_map, calibration_rows
