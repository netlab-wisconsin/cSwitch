#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIGS=(
  "configs/cxl_numa1_prefix_duckdb_tpch_sf1_single_thread_q10.json"
  "configs/cxl_numa1_prefix_duckdb_tpch_sf1_single_thread_q21.json"
)

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
fi

cd "${ROOT_DIR}"

for config in "${CONFIGS[@]}"; do
  echo "==> ${config}"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    python3 scripts/run_chiplet_ycsb_harness.py --config "${config}" --dry-run
    continue
  fi

  sudo python3 scripts/run_chiplet_ycsb_harness.py --config "${config}"
done
