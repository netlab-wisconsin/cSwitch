#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MEMORY_BENCHMARK="/home/seunghyun/sched_bench/build/memory_benchmark"

if [[ $# -gt 0 && "$1" != --* ]]; then
  OUTPUT_DIR="$1"
  shift
else
  OUTPUT_DIR="$SCRIPT_DIR/results/full-with-constant-traffic-$(date -u +%Y%m%dT%H%M%SZ)"
fi

EXTRA_DIR="$OUTPUT_DIR/extra_constant_traffic"
TRAFFIC_CONFIG="$EXTRA_DIR/core14_to_numa0_rate0.xml"
TRAFFIC_OUTPUT="$EXTRA_DIR/core14_to_numa0_rate0.txt"
TRAFFIC_LOG="$EXTRA_DIR/core14_to_numa0_rate0.log"
mkdir -p "$EXTRA_DIR"

python3 - "$TRAFFIC_CONFIG" "$TRAFFIC_OUTPUT" <<'PY'
from pathlib import Path
import sys

root_dir = Path('/home/seunghyun/exp3')
if str(root_dir) not in sys.path:
    sys.path.insert(0, str(root_dir))

from experiment_utils import write_memory_benchmark_config

config_path = Path(sys.argv[1])
output_path = Path(sys.argv[2])
write_memory_benchmark_config(
    config_path,
    nthreads=1,
    cores=[14],
    rates=[0],
    worker_memory_mb=256,
    output_path=output_path,
    modes=[0],
    numa_nodes=[0],
    duration_seconds=86400,
    worker_numa_policy='bind',
)
PY

traffic_pid=""
cleanup() {
  if [[ -n "$traffic_pid" ]] && kill -0 "$traffic_pid" 2>/dev/null; then
    kill "$traffic_pid" 2>/dev/null || true
    wait "$traffic_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

"$MEMORY_BENCHMARK" --config "$TRAFFIC_CONFIG" > "$TRAFFIC_LOG" 2>&1 &
traffic_pid="$!"
sleep 2

env PYTHONUNBUFFERED=1 python3 "$SCRIPT_DIR/run_exp_case3.py" --output-dir "$OUTPUT_DIR" "$@"
