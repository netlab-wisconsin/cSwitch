from __future__ import annotations

import json
import math
import re
from dataclasses import dataclass, field
from pathlib import Path


_PERCENTILE_RE = re.compile(r"^([0-9]+(?:\.[0-9]+)?)(?:th)?PercentileLatency\(us\)$")
_NPB_NAME_RE = re.compile(r"^\s*NAS Parallel Benchmarks.*-\s*([A-Za-z0-9_+-]+)\s+Benchmark", re.IGNORECASE)
_NPB_CLASS_RE = re.compile(r"^\s*Class\s*=\s*([A-Za-z0-9_+-]+)")
_NPB_TIME_RE = re.compile(r"^\s*Time in seconds\s*=\s*([0-9.eE+-]+)")
_NPB_RATE_RE = re.compile(r"^\s*(Mop/s total|MOPS total|MFLOPS)\s*=\s*([0-9.eE+-]+)", re.IGNORECASE)
_NPB_VERIFICATION_RE = re.compile(r"^\s*Verification(?:\s*=\s*|\s+)(.+?)\s*$", re.IGNORECASE)
_DUCKDB_TPCH_JSON_PREFIX = "DUCKDB_TPCH_RESULT_JSON="
_GAPBS_TRIAL_TIME_RE = re.compile(r"^\s*Trial Time:\s*([0-9.eE+-]+)\s*$")
_GAPBS_AVG_TIME_RE = re.compile(r"^\s*Average Time:\s*([0-9.eE+-]+)\s*$")
_XSBENCH_SIM_METHOD_RE = re.compile(r"^\s*Simulation Method:\s*(.+?)\s*$", re.IGNORECASE)
_XSBENCH_GRID_TYPE_RE = re.compile(r"^\s*Grid Type:\s*(.+?)\s*$", re.IGNORECASE)
_XSBENCH_SIZE_RE = re.compile(r"^\s*H-M Benchmark Size:\s*(.+?)\s*$", re.IGNORECASE)
_XSBENCH_RUNTIME_RE = re.compile(r"^\s*Runtime:\s*([0-9.eE+-]+)\s+seconds\s*$", re.IGNORECASE)
_XSBENCH_LOOKUPS_RE = re.compile(r"^\s*Lookups:\s*([0-9,]+)\s*$", re.IGNORECASE)
_XSBENCH_LOOKUPS_PER_SEC_RE = re.compile(r"^\s*Lookups/s:\s*([0-9,]+(?:\.[0-9]+)?)\s*$", re.IGNORECASE)
_XSBENCH_VERIFICATION_RE = re.compile(
    r"^\s*Verification checksum:\s*([0-9,]+)(?:\s+\((.+)\))?\s*$",
    re.IGNORECASE,
)
_FILEBENCH_RUN_TOOK_RE = re.compile(
    r"^\s*(?:[0-9.eE+-]+:\s*)?Run took\s+([0-9.eE+-]+)\s+seconds\.\.\.\s*$",
    re.IGNORECASE,
)
_FILEBENCH_IO_SUMMARY_RE = re.compile(
    r"^\s*(?:[0-9.eE+-]+:\s*)?IO Summary:\s*(\d+)\s+ops\s+([0-9.eE+-]+)\s+ops/s\s+([0-9.eE+-]+)/([0-9.eE+-]+)\s+rd/wr\s+([0-9.eE+-]+)\s*mb/s\s+([0-9.eE+-]+)\s*ms/op(?:\s+\[([0-9.eE+-]+)ms\s*-\s*([0-9.eE+-]+)ms\])?(?:\s+p99=([0-9.eE+-]+)ms)?\s*$",
    re.IGNORECASE,
)


@dataclass
class OperationMetrics:
    operations: int | None = None
    average_us: float | None = None
    min_us: float | None = None
    max_us: float | None = None
    p50_us: float | None = None
    p95_us: float | None = None
    p99_us: float | None = None
    p99_9_us: float | None = None
    p99_99_us: float | None = None
    histogram_kind: str | None = None
    histogram: tuple[int, ...] = ()
    samples_us: tuple[float, ...] = ()
    raw_metrics: dict[str, float | int | str] = field(default_factory=dict)


@dataclass(frozen=True)
class PerfMetrics:
    stall_cycles: int | None
    cycles: int | None
    instructions: int | None
    task_clock_ms: float | None
    user_time_ms: float | None
    kernel_time_ms: float | None
    cache_refs: int | None
    cache_misses: int | None
    llc_loads: int | None
    llc_load_misses: int | None

    @property
    def llc_hit_ratio(self) -> float | None:
        return llc_hit_ratio(self.llc_loads, self.llc_load_misses)


@dataclass(frozen=True)
class BenchmarkRunMetrics:
    benchmark_name: str | None = None
    benchmark_class: str | None = None
    time_s: float | None = None
    rate_value: float | None = None
    rate_unit: str | None = None
    verification: str | None = None

    @property
    def verification_ok(self) -> bool | None:
        if self.verification is None:
            return None
        value = self.verification.strip().upper()
        if "SUCCESS" in value:
            return True
        if "UNSUCCESS" in value or "FAIL" in value:
            return False
        return None


@dataclass(frozen=True)
class ExternalRunMetrics:
    benchmark: BenchmarkRunMetrics
    throughput_ops_per_sec: float | None = None
    primary_operation: str | None = None
    operation_metrics: dict[str, OperationMetrics] = field(default_factory=dict)


def perf_scope_label() -> str:
    paranoid_path = Path("/proc/sys/kernel/perf_event_paranoid")
    if not paranoid_path.exists():
        return "all"
    try:
        paranoid = int(paranoid_path.read_text(encoding="utf-8").strip())
    except ValueError:
        return "all"
    if paranoid >= 2:
        return "user"
    return "all"


def _parse_numeric(text: str) -> float | int | None:
    value = text.strip().replace(",", "")
    if not value or value.startswith("<"):
        return None
    try:
        if any(marker in value for marker in (".", "e", "E")):
            return float(value)
        return int(value)
    except ValueError:
        return None


def parse_perf_counter(perf_file: Path, event_prefix: str) -> int | float | None:
    if not perf_file.exists():
        return None
    for line in perf_file.read_text(encoding="utf-8", errors="replace").splitlines():
        parts = line.split(";")
        if len(parts) < 3:
            continue
        if parts[2].startswith(event_prefix):
            return _parse_numeric(parts[0])
    return None


def parse_time_metric_ms(time_file: Path, key: str) -> float | None:
    if not time_file.exists():
        return None
    for line in time_file.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.startswith(f"{key}="):
            continue
        value = _parse_numeric(line.split("=", 1)[1])
        if value is None:
            return None
        return float(value) * 1000.0
    return None


def llc_hit_ratio(llc_loads: int | None, llc_load_misses: int | None) -> float | None:
    if llc_loads in (None, 0) or llc_load_misses is None:
        return None
    return float(llc_loads - llc_load_misses) / float(llc_loads)


def parse_perf_metrics(perf_file: Path, time_file: Path, perf_event: str) -> PerfMetrics:
    return PerfMetrics(
        stall_cycles=_coerce_int(parse_perf_counter(perf_file, perf_event)),
        cycles=_coerce_int(parse_perf_counter(perf_file, "cycles")),
        instructions=_coerce_int(parse_perf_counter(perf_file, "instructions")),
        task_clock_ms=_coerce_float(parse_perf_counter(perf_file, "task-clock")),
        user_time_ms=parse_time_metric_ms(time_file, "user"),
        kernel_time_ms=parse_time_metric_ms(time_file, "sys"),
        cache_refs=_coerce_int(parse_perf_counter(perf_file, "cache-references")),
        cache_misses=_coerce_int(parse_perf_counter(perf_file, "cache-misses")),
        llc_loads=_coerce_int(parse_perf_counter(perf_file, "LLC-loads")),
        llc_load_misses=_coerce_int(parse_perf_counter(perf_file, "LLC-load-misses")),
    )


def _coerce_int(value: int | float | None) -> int | None:
    if value is None:
        return None
    return int(value)


def _coerce_float(value: int | float | None) -> float | None:
    if value is None:
        return None
    return float(value)


def percentile_from_samples(samples: list[float], percentile: float) -> float | None:
    if not samples:
        return None
    ordered = sorted(float(sample) for sample in samples)
    if len(ordered) == 1:
        return ordered[0]
    rank = (percentile / 100.0) * (len(ordered) - 1)
    lower = int(math.floor(rank))
    upper = int(math.ceil(rank))
    if lower == upper:
        return ordered[lower]
    weight = rank - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * weight


def operation_metrics_from_samples(samples: list[float]) -> OperationMetrics:
    ordered = [float(sample) for sample in samples]
    if not ordered:
        return OperationMetrics()
    return OperationMetrics(
        operations=len(ordered),
        average_us=sum(ordered) / float(len(ordered)),
        min_us=min(ordered),
        max_us=max(ordered),
        p50_us=percentile_from_samples(ordered, 50.0),
        p95_us=percentile_from_samples(ordered, 95.0),
        p99_us=percentile_from_samples(ordered, 99.0),
        p99_9_us=percentile_from_samples(ordered, 99.9),
        p99_99_us=percentile_from_samples(ordered, 99.99),
        samples_us=tuple(ordered),
    )


def filebench_histogram_percentile_us(
    histogram: tuple[int, ...],
    percentile: float,
) -> float | None:
    if not histogram:
        return None
    total = sum(histogram)
    if total <= 0:
        return None
    if percentile <= 0.0:
        percentile = 0.0
    if percentile >= 100.0:
        percentile = 100.0

    rank = (percentile / 100.0) * total
    rank_index = int(math.ceil(rank))
    if rank_index <= 0:
        rank_index = 1

    cumulative = 0
    for bucket_index, bucket_count in enumerate(histogram):
        if bucket_count <= 0:
            continue
        previous = cumulative
        cumulative += bucket_count
        if cumulative < rank_index:
            continue
        lower_ns = 0 if bucket_index == 0 else (1 << (bucket_index - 1))
        upper_ns = 1 << bucket_index
        fraction = float(rank_index - previous) / float(bucket_count)
        fraction = max(0.0, min(1.0, fraction))
        return (lower_ns + ((upper_ns - lower_ns) * fraction)) / 1000.0
    return (1 << (len(histogram) - 1)) / 1000.0


def operation_metrics_from_filebench_histogram(
    histogram: tuple[int, ...],
    *,
    operations: int | None,
    average_us: float | None,
    min_us: float | None,
    max_us: float | None,
) -> OperationMetrics:
    return OperationMetrics(
        operations=operations if operations is not None else sum(histogram),
        average_us=average_us,
        min_us=min_us,
        max_us=max_us,
        p50_us=filebench_histogram_percentile_us(histogram, 50.0),
        p95_us=filebench_histogram_percentile_us(histogram, 95.0),
        p99_us=filebench_histogram_percentile_us(histogram, 99.0),
        p99_9_us=filebench_histogram_percentile_us(histogram, 99.9),
        p99_99_us=filebench_histogram_percentile_us(histogram, 99.99),
        histogram_kind="filebench_log2_ns",
        histogram=histogram,
    )


def merge_operation_metrics(metrics_list: list[OperationMetrics]) -> OperationMetrics | None:
    if not metrics_list:
        return None

    merged_samples: list[float] = []
    for metrics in metrics_list:
        merged_samples.extend(metrics.samples_us)
    if merged_samples:
        return operation_metrics_from_samples(merged_samples)

    if all(metrics.histogram_kind == "filebench_log2_ns" and metrics.histogram for metrics in metrics_list):
        max_hist_len = max(len(metrics.histogram) for metrics in metrics_list)
        merged_histogram = [0] * max_hist_len
        total_operations = 0
        weighted_average_total = 0.0
        min_values: list[float] = []
        max_values: list[float] = []
        for metrics in metrics_list:
            for index, bucket_count in enumerate(metrics.histogram):
                merged_histogram[index] += bucket_count
            if metrics.operations is not None:
                total_operations += metrics.operations
                if metrics.average_us is not None:
                    weighted_average_total += metrics.average_us * metrics.operations
            if metrics.min_us is not None:
                min_values.append(metrics.min_us)
            if metrics.max_us is not None:
                max_values.append(metrics.max_us)
        average_us = None
        if total_operations > 0 and weighted_average_total > 0.0:
            average_us = weighted_average_total / float(total_operations)
        return operation_metrics_from_filebench_histogram(
            tuple(merged_histogram),
            operations=total_operations if total_operations > 0 else None,
            average_us=average_us,
            min_us=min(min_values) if min_values else None,
            max_us=max(max_values) if max_values else None,
        )

    if len(metrics_list) == 1:
        return metrics_list[0]

    return None


def parse_ycsb_run_log(run_log: Path) -> tuple[float | None, dict[str, OperationMetrics]]:
    throughput: float | None = None
    operations: dict[str, OperationMetrics] = {}
    if not run_log.exists():
        return throughput, operations

    for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
        parts = raw_line.strip().split(", ", 2)
        if len(parts) != 3 or not parts[0].startswith("[") or not parts[0].endswith("]"):
            continue
        section = parts[0][1:-1]
        metric_name = parts[1].strip()
        value = _parse_numeric(parts[2])
        if section == "OVERALL" and metric_name == "Throughput(ops/sec)":
            throughput = _coerce_float(value)
            continue
        match = _PERCENTILE_RE.match(metric_name)
        if metric_name not in {"Operations", "AverageLatency(us)", "MinLatency(us)", "MaxLatency(us)"} and not match:
            continue
        metric_bucket = operations.setdefault(section, OperationMetrics())
        metric_bucket.raw_metrics[metric_name] = parts[2]
        if metric_name == "Operations":
            metric_bucket.operations = _coerce_int(value)
        elif metric_name == "AverageLatency(us)":
            metric_bucket.average_us = _coerce_float(value)
        elif metric_name == "MinLatency(us)":
            metric_bucket.min_us = _coerce_float(value)
        elif metric_name == "MaxLatency(us)":
            metric_bucket.max_us = _coerce_float(value)
        else:
            percentile = match.group(1)
            normalized_key = f"p{percentile.replace('.', '_')}_us"
            if hasattr(metric_bucket, normalized_key):
                setattr(metric_bucket, normalized_key, _coerce_float(value))
    return throughput, operations


def parse_npb_run_log(*run_logs: Path) -> BenchmarkRunMetrics:
    metrics = BenchmarkRunMetrics()
    for run_log in run_logs:
        if not run_log.exists():
            continue
        for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw_line.strip()
            if not line:
                continue
            match = _NPB_NAME_RE.match(line)
            if match:
                metrics = BenchmarkRunMetrics(
                    benchmark_name=match.group(1).upper(),
                    benchmark_class=metrics.benchmark_class,
                    time_s=metrics.time_s,
                    rate_value=metrics.rate_value,
                    rate_unit=metrics.rate_unit,
                    verification=metrics.verification,
                )
                continue
            match = _NPB_CLASS_RE.match(line)
            if match:
                metrics = BenchmarkRunMetrics(
                    benchmark_name=metrics.benchmark_name,
                    benchmark_class=match.group(1).upper(),
                    time_s=metrics.time_s,
                    rate_value=metrics.rate_value,
                    rate_unit=metrics.rate_unit,
                    verification=metrics.verification,
                )
                continue
            match = _NPB_TIME_RE.match(line)
            if match:
                metrics = BenchmarkRunMetrics(
                    benchmark_name=metrics.benchmark_name,
                    benchmark_class=metrics.benchmark_class,
                    time_s=_coerce_float(_parse_numeric(match.group(1))),
                    rate_value=metrics.rate_value,
                    rate_unit=metrics.rate_unit,
                    verification=metrics.verification,
                )
                continue
            match = _NPB_RATE_RE.match(line)
            if match:
                metrics = BenchmarkRunMetrics(
                    benchmark_name=metrics.benchmark_name,
                    benchmark_class=metrics.benchmark_class,
                    time_s=metrics.time_s,
                    rate_value=_coerce_float(_parse_numeric(match.group(2))),
                    rate_unit=match.group(1),
                    verification=metrics.verification,
                )
                continue
            match = _NPB_VERIFICATION_RE.match(line)
            if match:
                metrics = BenchmarkRunMetrics(
                    benchmark_name=metrics.benchmark_name,
                    benchmark_class=metrics.benchmark_class,
                    time_s=metrics.time_s,
                    rate_value=metrics.rate_value,
                    rate_unit=metrics.rate_unit,
                    verification=match.group(1).strip(),
                )
    return metrics


def parse_duckdb_tpch_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    payload_text: str | None = None
    for run_log in run_logs:
        if not run_log.exists():
            continue
        for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
            if raw_line.startswith(_DUCKDB_TPCH_JSON_PREFIX):
                payload_text = raw_line[len(_DUCKDB_TPCH_JSON_PREFIX) :].strip()
    if payload_text is None:
        return None

    try:
        payload = json.loads(payload_text)
    except json.JSONDecodeError:
        return None

    query_metrics: dict[str, OperationMetrics] = {}
    for raw_name, raw_entry in payload.get("queries", {}).items():
        if not isinstance(raw_entry, dict):
            continue
        latencies = [float(value) for value in raw_entry.get("latencies_us", [])]
        metrics = operation_metrics_from_samples(latencies)
        raw_name_text = str(raw_name)
        query_metrics[raw_name_text] = OperationMetrics(
            operations=metrics.operations,
            average_us=metrics.average_us,
            min_us=metrics.min_us,
            max_us=metrics.max_us,
            p50_us=metrics.p50_us,
            p95_us=metrics.p95_us,
            p99_us=metrics.p99_us,
            p99_9_us=metrics.p99_9_us,
            p99_99_us=metrics.p99_99_us,
            samples_us=metrics.samples_us,
            raw_metrics={"query_index": raw_entry.get("query_index", raw_name_text)},
        )

    scale_factor = payload.get("scale_factor")
    benchmark_class = None if scale_factor is None else f"SF{scale_factor}"
    benchmark = BenchmarkRunMetrics(
        benchmark_name=str(payload.get("benchmark_name", "DUCKDB_TPCH")),
        benchmark_class=benchmark_class,
        time_s=_coerce_float(payload.get("total_time_s")),
        rate_value=_coerce_float(payload.get("queries_per_sec")),
        rate_unit=str(payload.get("rate_unit", "queries/s")),
        verification=str(payload.get("verification", "SUCCESSFUL")),
    )
    primary_operation = next(iter(query_metrics)) if len(query_metrics) == 1 else None
    return ExternalRunMetrics(
        benchmark=benchmark,
        throughput_ops_per_sec=benchmark.rate_value,
        primary_operation=primary_operation,
        operation_metrics=query_metrics,
    )


def _iter_json_objects_from_logs(*run_logs: Path) -> list[dict[str, object]]:
    objects: list[dict[str, object]] = []
    for run_log in run_logs:
        if not run_log.exists():
            continue
        text = run_log.read_text(encoding="utf-8", errors="replace")
        stripped = text.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            try:
                payload = json.loads(stripped)
            except json.JSONDecodeError:
                payload = None
            if isinstance(payload, list):
                for entry in payload:
                    if isinstance(entry, dict):
                        objects.append(entry)
                continue
        for raw_line in text.splitlines():
            line = raw_line.strip()
            if not line.startswith("{") or not line.endswith("}"):
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(payload, dict):
                objects.append(payload)
    return objects


def parse_llama_bench_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    payloads = _iter_json_objects_from_logs(*run_logs)
    if not payloads:
        return None

    operation_metrics: dict[str, OperationMetrics] = {}
    merged_samples: dict[str, list[float]] = {}
    total_tokens = 0
    total_time_ns = 0.0
    model_filename: str | None = None

    for payload in payloads:
        avg_ns = _coerce_float(payload.get("avg_ns"))
        n_prompt = _coerce_int(payload.get("n_prompt")) or 0
        n_gen = _coerce_int(payload.get("n_gen")) or 0
        tokens = n_prompt + n_gen
        if avg_ns is None or tokens <= 0:
            continue

        total_tokens += tokens
        total_time_ns += avg_ns
        model_filename = str(payload.get("model_filename") or model_filename or "")

        if n_prompt > 0 and n_gen == 0:
            operation_name = "PROMPT"
        elif n_gen > 0 and n_prompt == 0:
            operation_name = "GEN"
        else:
            operation_name = "TOKEN"

        raw_samples_ns = payload.get("samples_ns")
        if isinstance(raw_samples_ns, list) and raw_samples_ns:
            sample_values = [
                float(sample) / 1000.0 / float(tokens)
                for sample in raw_samples_ns
                if _coerce_float(sample) is not None
            ]
        else:
            sample_values = [avg_ns / 1000.0 / float(tokens)]
        if sample_values:
            merged_samples.setdefault(operation_name, []).extend(sample_values)

    for operation_name, samples_us in merged_samples.items():
        metrics = operation_metrics_from_samples(samples_us)
        operation_metrics[operation_name] = OperationMetrics(
            operations=metrics.operations,
            average_us=metrics.average_us,
            min_us=metrics.min_us,
            max_us=metrics.max_us,
            p50_us=metrics.p50_us,
            p95_us=metrics.p95_us,
            p99_us=metrics.p99_us,
            p99_9_us=metrics.p99_9_us,
            p99_99_us=metrics.p99_99_us,
            samples_us=metrics.samples_us,
            raw_metrics={},
        )

    rate_value = None
    time_s = None
    if total_tokens > 0 and total_time_ns > 0:
        time_s = total_time_ns / 1_000_000_000.0
        rate_value = float(total_tokens) / time_s

    benchmark = BenchmarkRunMetrics(
        benchmark_name="LLAMA_BENCH",
        benchmark_class=None if not model_filename else Path(model_filename).stem,
        time_s=time_s,
        rate_value=rate_value,
        rate_unit="tokens/s",
        verification="SUCCESSFUL",
    )
    return ExternalRunMetrics(
        benchmark=benchmark,
        throughput_ops_per_sec=rate_value,
        primary_operation=next(iter(operation_metrics)) if len(operation_metrics) == 1 else None,
        operation_metrics=operation_metrics,
    )


def parse_gapbs_run_log(
    benchmark_name: str,
    operation_name: str,
    *run_logs: Path,
) -> ExternalRunMetrics | None:
    trial_times_s: list[float] = []
    average_time_s: float | None = None
    for run_log in run_logs:
        if not run_log.exists():
            continue
        for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw_line.strip()
            if not line:
                continue
            match = _GAPBS_TRIAL_TIME_RE.match(line)
            if match:
                value = _coerce_float(_parse_numeric(match.group(1)))
                if value is not None:
                    trial_times_s.append(value)
                continue
            match = _GAPBS_AVG_TIME_RE.match(line)
            if match:
                average_time_s = _coerce_float(_parse_numeric(match.group(1)))

    if average_time_s is None and not trial_times_s:
        return None
    if average_time_s is None and trial_times_s:
        average_time_s = sum(trial_times_s) / float(len(trial_times_s))

    operation_metrics: dict[str, OperationMetrics] = {}
    if trial_times_s:
        samples_us = [value * 1_000_000.0 for value in trial_times_s]
        metrics = operation_metrics_from_samples(samples_us)
        operation_metrics[operation_name] = OperationMetrics(
            operations=metrics.operations,
            average_us=metrics.average_us,
            min_us=metrics.min_us,
            max_us=metrics.max_us,
            p50_us=metrics.p50_us,
            p95_us=metrics.p95_us,
            p99_us=metrics.p99_us,
            p99_9_us=metrics.p99_9_us,
            p99_99_us=metrics.p99_99_us,
            samples_us=metrics.samples_us,
            raw_metrics={},
        )

    rate_value = None if average_time_s in (None, 0.0) else 1.0 / average_time_s
    benchmark = BenchmarkRunMetrics(
        benchmark_name=benchmark_name,
        benchmark_class=None,
        time_s=average_time_s,
        rate_value=rate_value,
        rate_unit="runs/s",
        verification="SUCCESSFUL",
    )
    return ExternalRunMetrics(
        benchmark=benchmark,
        throughput_ops_per_sec=rate_value,
        primary_operation=operation_name if operation_metrics else None,
        operation_metrics=operation_metrics,
    )


def parse_gapbs_pagerank_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    return parse_gapbs_run_log("GAPBS_PAGERANK", "PAGERANK", *run_logs)


def parse_gapbs_bfs_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    return parse_gapbs_run_log("GAPBS_BFS", "BFS", *run_logs)


def parse_xsbench_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    simulation_method: str | None = None
    grid_type: str | None = None
    size_label: str | None = None
    runtime_s: float | None = None
    lookups: int | None = None
    lookups_per_sec: float | None = None
    verification_note: str | None = None

    for run_log in run_logs:
        if not run_log.exists():
            continue
        for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw_line.strip()
            if not line:
                continue
            match = _XSBENCH_SIM_METHOD_RE.match(line)
            if match:
                simulation_method = match.group(1).strip()
                continue
            match = _XSBENCH_GRID_TYPE_RE.match(line)
            if match:
                grid_type = match.group(1).strip()
                continue
            match = _XSBENCH_SIZE_RE.match(line)
            if match:
                size_label = match.group(1).strip()
                continue
            match = _XSBENCH_RUNTIME_RE.match(line)
            if match:
                runtime_s = _coerce_float(_parse_numeric(match.group(1)))
                continue
            match = _XSBENCH_LOOKUPS_RE.match(line)
            if match:
                lookups = _coerce_int(_parse_numeric(match.group(1)))
                continue
            match = _XSBENCH_LOOKUPS_PER_SEC_RE.match(line)
            if match:
                lookups_per_sec = _coerce_float(_parse_numeric(match.group(1)))
                continue
            match = _XSBENCH_VERIFICATION_RE.match(line)
            if match:
                verification_note = match.group(2).strip() if match.group(2) else None

    if runtime_s is None and lookups_per_sec is None:
        return None

    benchmark_class_parts = []
    if simulation_method:
        benchmark_class_parts.append(simulation_method.replace(" ", "_"))
    if size_label:
        benchmark_class_parts.append(size_label)
    if grid_type:
        benchmark_class_parts.append(grid_type.replace(" ", "_"))
    benchmark_class = None if not benchmark_class_parts else "_".join(benchmark_class_parts)

    operation_metrics: dict[str, OperationMetrics] = {}
    if runtime_s not in (None, 0.0) and lookups is not None and lookups > 0:
        avg_us = runtime_s * 1_000_000.0 / float(lookups)
        operation_metrics["XSLOOKUP"] = OperationMetrics(
            operations=lookups,
            average_us=avg_us,
            min_us=avg_us,
            max_us=avg_us,
            p50_us=avg_us,
            p95_us=avg_us,
            p99_us=avg_us,
            p99_9_us=avg_us,
            p99_99_us=avg_us,
            samples_us=(avg_us,),
            raw_metrics={},
        )

    benchmark = BenchmarkRunMetrics(
        benchmark_name="XSBENCH",
        benchmark_class=benchmark_class,
        time_s=runtime_s,
        rate_value=lookups_per_sec,
        rate_unit="lookups/s",
        verification=verification_note,
    )
    return ExternalRunMetrics(
        benchmark=benchmark,
        throughput_ops_per_sec=lookups_per_sec,
        primary_operation="XSLOOKUP" if operation_metrics else None,
        operation_metrics=operation_metrics,
    )


def parse_filebench_run_log(*run_logs: Path) -> ExternalRunMetrics | None:
    match_text: re.Match[str] | None = None
    runtime_s: float | None = None
    summary_histogram: list[int] | None = None
    for run_log in run_logs:
        if not run_log.exists():
            continue
        for raw_line in run_log.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw_line.strip()
            runtime_match = _FILEBENCH_RUN_TOOK_RE.match(line)
            if runtime_match:
                runtime_s = _coerce_float(_parse_numeric(runtime_match.group(1)))
            match = _FILEBENCH_IO_SUMMARY_RE.match(line)
            if match:
                match_text = match
            if "\t[" in line and "ops/s" in line and "ms/op" in line:
                histogram_text = line.split("\t[", 1)[1].rsplit("]", 1)[0]
                try:
                    buckets = [int(value) for value in histogram_text.split()]
                except ValueError:
                    buckets = []
                if buckets:
                    if summary_histogram is None:
                        summary_histogram = [0] * len(buckets)
                    for index, value in enumerate(buckets):
                        if index >= len(summary_histogram):
                            summary_histogram.extend([0] * (index + 1 - len(summary_histogram)))
                        summary_histogram[index] += value
    if match_text is None:
        return None

    operations = _coerce_int(_parse_numeric(match_text.group(1)))
    ops_per_sec = _coerce_float(_parse_numeric(match_text.group(2)))
    mb_per_sec = _coerce_float(_parse_numeric(match_text.group(5)))
    ms_per_op = _coerce_float(_parse_numeric(match_text.group(6)))
    min_text = match_text.group(7)
    max_text = match_text.group(8)
    p99_text = match_text.group(9)
    min_ms = None if min_text is None else _coerce_float(_parse_numeric(min_text))
    max_ms = None if max_text is None else _coerce_float(_parse_numeric(max_text))
    p99_ms = None if p99_text is None else _coerce_float(_parse_numeric(p99_text))

    operation_metrics: dict[str, OperationMetrics] = {}
    if ms_per_op is not None:
        avg_us = ms_per_op * 1000.0
        metrics = (
            operation_metrics_from_filebench_histogram(
                tuple(summary_histogram),
                operations=operations,
                average_us=avg_us,
                min_us=(None if min_ms is None else min_ms * 1000.0),
                max_us=(None if max_ms is None else max_ms * 1000.0),
            )
            if summary_histogram
            else OperationMetrics(
                operations=operations,
                average_us=avg_us,
                min_us=(None if min_ms is None else min_ms * 1000.0),
                max_us=(None if max_ms is None else max_ms * 1000.0),
            )
        )
        if p99_ms is not None:
            metrics.p99_us = p99_ms * 1000.0
        metrics.raw_metrics = {
            "mb_per_sec": mb_per_sec,
            "min_ms": min_ms,
            "max_ms": max_ms,
            "p99_ms": p99_ms,
        }
        operation_metrics["FILESERVER"] = metrics

    benchmark = BenchmarkRunMetrics(
        benchmark_name="FILEBENCH_FILESERVER",
        benchmark_class="FILESERVER",
        time_s=runtime_s,
        rate_value=ops_per_sec,
        rate_unit="ops/s",
        verification="SUCCESSFUL",
    )
    return ExternalRunMetrics(
        benchmark=benchmark,
        throughput_ops_per_sec=ops_per_sec,
        primary_operation="FILESERVER" if operation_metrics else None,
        operation_metrics=operation_metrics,
    )


def parse_external_run_log(
    *run_logs: Path,
    expected_benchmark_name: str | None = None,
) -> ExternalRunMetrics:
    duckdb_metrics = parse_duckdb_tpch_run_log(*run_logs)
    if duckdb_metrics is not None:
        return duckdb_metrics
    llama_metrics = parse_llama_bench_run_log(*run_logs)
    if llama_metrics is not None:
        return llama_metrics
    if expected_benchmark_name == "GAPBS_BFS":
        gapbs_bfs_metrics = parse_gapbs_bfs_run_log(*run_logs)
        if gapbs_bfs_metrics is not None:
            return gapbs_bfs_metrics
    if expected_benchmark_name == "GAPBS_PAGERANK":
        gapbs_metrics = parse_gapbs_pagerank_run_log(*run_logs)
        if gapbs_metrics is not None:
            return gapbs_metrics
    gapbs_metrics = parse_gapbs_pagerank_run_log(*run_logs)
    if gapbs_metrics is not None:
        return gapbs_metrics
    gapbs_bfs_metrics = parse_gapbs_bfs_run_log(*run_logs)
    if gapbs_bfs_metrics is not None:
        return gapbs_bfs_metrics
    xsbench_metrics = parse_xsbench_run_log(*run_logs)
    if xsbench_metrics is not None:
        return xsbench_metrics
    filebench_metrics = parse_filebench_run_log(*run_logs)
    if filebench_metrics is not None:
        return filebench_metrics
    npb_metrics = parse_npb_run_log(*run_logs)
    return ExternalRunMetrics(
        benchmark=npb_metrics,
        throughput_ops_per_sec=npb_metrics.rate_value,
        primary_operation=None,
        operation_metrics={},
    )


def sum_optional_ints(values: list[int | None]) -> int | None:
    if any(value is None for value in values):
        return None
    return int(sum(value for value in values if value is not None))


def sum_optional_floats(values: list[float | None]) -> float | None:
    if any(value is None for value in values):
        return None
    return float(sum(value for value in values if value is not None))


def throughput_from_wall_ms(total_ops: int, wall_ms: float | None) -> float | None:
    if wall_ms is None or wall_ms <= 0:
        return None
    return float(total_ops) / (wall_ms / 1000.0)
