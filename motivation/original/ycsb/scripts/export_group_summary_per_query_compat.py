#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
from collections import defaultdict
from pathlib import Path


COMPAT_FIELDS = [
    "timestamp_utc",
    "host",
    "arch",
    "backend",
    "mode",
    "event_name",
    "perf_scope",
    "assignment_label",
    "instance_count",
    "cores",
    "numas",
    "numa_binding_applied",
    "working_set_mb_per_instance",
    "total_working_set_mb",
    "recordcount_per_instance",
    "total_recordcount",
    "operationcount_per_instance",
    "total_operationcount",
    "threads_per_instance",
    "java_active_processor_count_per_instance",
    "load_threads",
    "fieldcount",
    "fieldlength",
    "approx_value_bytes_per_record",
    "primary_operation",
    "latency_operations",
    "latency_average_us",
    "latency_min_us",
    "latency_max_us",
    "latency_p50_us",
    "latency_p95_us",
    "latency_p99_us",
    "latency_p99_9_us",
    "latency_p99_99_us",
    "summed_stall_cycles",
    "summed_cycles",
    "summed_instructions",
    "summed_task_clock_ms",
    "summed_instance_throughput_ops_per_sec",
    "group_wall_clock_ms",
    "aggregate_wall_throughput_ops_per_sec",
    "ServerPerfUserTimeMs",
    "ServerPerfKernelTimeMs",
    "ServerPerfCacheRefs",
    "ServerPerfCacheMisses",
    "ServerPerfLLCHitRatio",
    "df_duration_s",
    "df_total_read_mib_s",
    "df_total_write_mib_s",
    "df_total_mib_s",
    "df_ccm0_read_mib_s",
    "df_ccm0_write_mib_s",
    "df_ccm0_total_mib_s",
    "df_ccm1_read_mib_s",
    "df_ccm1_write_mib_s",
    "df_ccm1_total_mib_s",
    "df_ccm2_read_mib_s",
    "df_ccm2_write_mib_s",
    "df_ccm2_total_mib_s",
    "df_ccm3_read_mib_s",
    "df_ccm3_write_mib_s",
    "df_ccm3_total_mib_s",
    "df_ccm4_read_mib_s",
    "df_ccm4_write_mib_s",
    "df_ccm4_total_mib_s",
    "df_ccm5_read_mib_s",
    "df_ccm5_write_mib_s",
    "df_ccm5_total_mib_s",
    "df_ccm6_read_mib_s",
    "df_ccm6_write_mib_s",
    "df_ccm6_total_mib_s",
    "df_ccm7_read_mib_s",
    "df_ccm7_write_mib_s",
    "df_ccm7_total_mib_s",
    "status",
    "perf_event",
    "workload_file",
    "result_dir",
]


LATENCY_FIELD_MAP = {
    "latency_operations": "operations",
    "latency_average_us": "average_us",
    "latency_min_us": "min_us",
    "latency_max_us": "max_us",
    "latency_p50_us": "p50_us",
    "latency_p95_us": "p95_us",
    "latency_p99_us": "p99_us",
    "latency_p99_9_us": "p99_9_us",
    "latency_p99_99_us": "p99_99_us",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Export per-query group rows in compatibility format from chiplet harness outputs."
    )
    parser.add_argument("group_summary", type=Path, help="Path to group_summary.tsv")
    parser.add_argument("operation_summary", type=Path, help="Path to operation_summary.tsv")
    parser.add_argument("output", type=Path, help="Path to output TSV")
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    operations_by_result: dict[str, list[dict[str, str]]] = defaultdict(list)
    with args.operation_summary.open(encoding="utf-8", newline="") as op_handle:
        reader = csv.DictReader(op_handle, delimiter="\t")
        for row in reader:
            operations_by_result[row["result_dir"]].append(row)

    with args.group_summary.open(encoding="utf-8", newline="") as group_handle:
        group_reader = csv.DictReader(group_handle, delimiter="\t")
        with args.output.open("w", encoding="utf-8", newline="") as out_handle:
            writer = csv.DictWriter(out_handle, fieldnames=COMPAT_FIELDS, delimiter="\t")
            writer.writeheader()

            for group_row in group_reader:
                group_result_dir = group_row["result_dir"]
                operation_rows = operations_by_result.get(group_result_dir, [])
                if not operation_rows:
                    compat_row = {field: group_row.get(field, "") for field in COMPAT_FIELDS}
                    writer.writerow(compat_row)
                    continue

                for operation_row in operation_rows:
                    compat_row = {field: group_row.get(field, "") for field in COMPAT_FIELDS}
                    compat_row["primary_operation"] = operation_row["operation"]
                    for compat_key, op_key in LATENCY_FIELD_MAP.items():
                        compat_row[compat_key] = operation_row.get(op_key, "")
                    writer.writerow(compat_row)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
