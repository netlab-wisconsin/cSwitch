#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Export chiplet harness group_summary.tsv in compatibility format.")
    parser.add_argument("input", type=Path, help="Path to group_summary.tsv")
    parser.add_argument("output", type=Path, help="Path to output TSV")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    with args.input.open(encoding="utf-8", newline="") as in_handle:
        reader = csv.DictReader(in_handle, delimiter="\t")
        with args.output.open("w", encoding="utf-8", newline="") as out_handle:
            writer = csv.DictWriter(out_handle, fieldnames=COMPAT_FIELDS, delimiter="\t")
            writer.writeheader()
            for row in reader:
                writer.writerow({field: row.get(field, "") for field in COMPAT_FIELDS})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
