#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
import os
import time
from pathlib import Path

import duckdb


JSON_PREFIX = "DUCKDB_TPCH_RESULT_JSON="


def parse_queries(spec: str) -> list[int]:
    text = spec.strip().lower()
    if text == "all":
        return list(range(1, 23))

    values: list[int] = []
    for chunk in spec.split(","):
        piece = chunk.strip()
        if not piece:
            continue
        if "-" in piece:
            start_text, end_text = piece.split("-", 1)
            start = int(start_text)
            end = int(end_text)
            if end < start:
                raise ValueError(f"invalid query range: {piece}")
            values.extend(range(start, end + 1))
        else:
            values.append(int(piece))

    normalized = sorted(set(values))
    for value in normalized:
        if value < 1 or value > 22:
            raise ValueError(f"TPC-H query number must be in [1, 22], got {value}")
    return normalized


def percentile(samples: list[float], pct: float) -> float | None:
    if not samples:
        return None
    ordered = sorted(samples)
    if len(ordered) == 1:
        return ordered[0]
    rank = (pct / 100.0) * (len(ordered) - 1)
    lower = int(math.floor(rank))
    upper = int(math.ceil(rank))
    if lower == upper:
        return ordered[lower]
    weight = rank - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * weight


def query_summary(latencies_us: list[float], query_index: int) -> dict[str, object]:
    average_us = None if not latencies_us else sum(latencies_us) / float(len(latencies_us))
    return {
        "query_index": query_index,
        "executions": len(latencies_us),
        "latencies_us": latencies_us,
        "average_us": average_us,
        "min_us": min(latencies_us) if latencies_us else None,
        "max_us": max(latencies_us) if latencies_us else None,
        "p50_us": percentile(latencies_us, 50.0),
        "p95_us": percentile(latencies_us, 95.0),
        "p99_us": percentile(latencies_us, 99.0),
        "p99_9_us": percentile(latencies_us, 99.9),
        "p99_99_us": percentile(latencies_us, 99.99),
    }


def set_memory_limit(connection: duckdb.DuckDBPyConnection, memory_limit: str | None) -> None:
    if memory_limit is None or not memory_limit.strip():
        return
    safe_value = memory_limit.replace("'", "''")
    connection.execute(f"SET memory_limit = '{safe_value}'")


def prepare_database(database_path: Path, scale_factor: str, memory_limit: str | None) -> None:
    if database_path.exists():
        return

    database_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = database_path.with_suffix(f"{database_path.suffix}.tmp")
    if tmp_path.exists():
        tmp_path.unlink()

    connection = duckdb.connect(str(tmp_path))
    try:
        set_memory_limit(connection, memory_limit)
        connection.execute("INSTALL tpch")
        connection.execute("LOAD tpch")
        connection.execute(f"CALL dbgen(sf={scale_factor})")
    finally:
        connection.close()

    os.replace(tmp_path, database_path)


def run_queries(
    database_path: Path,
    scale_factor: str,
    queries: list[int],
    repeat: int,
    memory_limit: str | None,
) -> dict[str, object]:
    connection = duckdb.connect(str(database_path), read_only=True)
    try:
        connection.execute("LOAD tpch")
        connection.execute("SET threads = 1")
        set_memory_limit(connection, memory_limit)

        query_rows: dict[str, object] = {}
        total_query_executions = 0
        started_at = time.perf_counter()
        for query_index in queries:
            latencies_us: list[float] = []
            for _ in range(repeat):
                query_started_at = time.perf_counter()
                connection.execute(f"PRAGMA tpch({query_index})").fetchall()
                query_finished_at = time.perf_counter()
                latencies_us.append((query_finished_at - query_started_at) * 1_000_000.0)
                total_query_executions += 1
            query_rows[f"Q{query_index}"] = query_summary(latencies_us, query_index)
        finished_at = time.perf_counter()
    finally:
        connection.close()

    total_time_s = finished_at - started_at
    queries_per_sec = None if total_time_s <= 0 else total_query_executions / total_time_s
    return {
        "benchmark_name": "DUCKDB_TPCH",
        "scale_factor": scale_factor,
        "verification": "SUCCESSFUL",
        "rate_unit": "queries/s",
        "total_query_executions": total_query_executions,
        "total_time_s": total_time_s,
        "queries_per_sec": queries_per_sec,
        "queries": query_rows,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run DuckDB TPC-H queries and emit JSON metrics.")
    parser.add_argument("--database", required=True, help="Path to the DuckDB database file")
    parser.add_argument("--scale-factor", required=True, help="TPC-H scale factor used for dbgen")
    parser.add_argument("--queries", default="1-22", help="Query list, e.g. '1-22' or '1,3,5'")
    parser.add_argument("--repeat", type=int, default=1, help="Number of repetitions per query")
    parser.add_argument("--memory-limit", default=None, help="Optional DuckDB memory_limit value")
    parser.add_argument(
        "--prepare-only",
        action="store_true",
        help="Create the DuckDB TPC-H database and exit without running queries",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.repeat <= 0:
        raise SystemExit("--repeat must be > 0")

    database_path = Path(args.database).resolve()
    prepare_database(database_path, args.scale_factor, args.memory_limit)
    if args.prepare_only:
        print(f"prepared DuckDB TPC-H database at {database_path}")
        return 0

    queries = parse_queries(args.queries)
    result = run_queries(
        database_path=database_path,
        scale_factor=args.scale_factor,
        queries=queries,
        repeat=args.repeat,
        memory_limit=args.memory_limit,
    )
    print(json.dumps(result, sort_keys=True))
    print(f"{JSON_PREFIX}{json.dumps(result, sort_keys=True)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
