#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

assert_cpu_vendor "AuthenticAMD"
run_elasticsearch_multi_instance_sweep "amd" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
