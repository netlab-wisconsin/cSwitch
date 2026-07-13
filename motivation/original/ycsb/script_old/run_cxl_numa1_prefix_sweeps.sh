#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CONFIGS=(
  "configs/cxl_numa1_prefix_ycsb_256mib.json"
  "configs/cxl_numa1_prefix_duckdb_tpch_sf1_single_thread.json"
  "configs/cxl_numa1_prefix_npb_cg_class_b.json"
  "configs/cxl_numa1_prefix_npb_ep_class_b.json"
  "configs/cxl_numa1_prefix_npb_ft_class_b.json"
  "configs/cxl_numa1_prefix_npb_mg_class_b.json"
  "configs/cxl_numa1_prefix_npb_cg_class_b_single_thread.json"
  "configs/cxl_numa1_prefix_npb_ep_class_b_single_thread.json"
  "configs/cxl_numa1_prefix_npb_ft_class_b_single_thread.json"
  "configs/cxl_numa1_prefix_npb_mg_class_b_single_thread.json"
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
  else
    sudo python3 scripts/run_chiplet_ycsb_harness.py --config "${config}"
  fi
done
