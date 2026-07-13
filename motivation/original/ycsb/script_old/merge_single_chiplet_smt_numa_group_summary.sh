#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

exec "${PYTHON_BIN:-python3}" \
  "${ROOT_DIR}/scripts/merge_single_chiplet_smt_numa_group_summary.py" \
  "$@"
