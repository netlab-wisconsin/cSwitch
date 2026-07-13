#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

assert_cpu_vendor "GenuineIntel"
run_orientdb_multi_instance_sweep "intel" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
