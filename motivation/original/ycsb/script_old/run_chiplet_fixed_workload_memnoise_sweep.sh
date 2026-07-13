#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$ROOT_DIR/scripts/run_chiplet_fixed_workload_memnoise_sweep.py" "$@"
