#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

assert_cpu_vendor "GenuineIntel"

combine_tsvs() {
  local output_file="$1"
  shift
  local first=1
  local input_file

  : > "${output_file}"
  for input_file in "$@"; do
    if (( first )); then
      cat "${input_file}" > "${output_file}"
      first=0
    else
      tail -n +2 "${input_file}" >> "${output_file}"
    fi
  done
}

host="$(hostname -s)"
now="$(date -u +'%Y%m%dT%H%M%SZ')"
combined_dir="${RESULTS_DIR}/intel_all_backends_multi_instance_${host}_${now}_$$"
combined_summary="${combined_dir}/combined_summary.tsv"
combined_instance_summary="${combined_dir}/combined_instance_summary.tsv"
manifest_file="${combined_dir}/manifest.tsv"
summary_files=()
instance_summary_files=()

mkdir -p "${combined_dir}"
printf 'backend\trun_dir\tsummary_file\tinstance_summary_file\n' > "${manifest_file}"

run_rocksdb_multi_instance_sweep "intel" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
summary_files+=("${LAST_MULTI_INSTANCE_SUMMARY_FILE}")
instance_summary_files+=("${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}")
printf 'rocksdb\t%s\t%s\t%s\n' "${LAST_MULTI_INSTANCE_RUN_DIR}" "${LAST_MULTI_INSTANCE_SUMMARY_FILE}" "${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}" >> "${manifest_file}"

run_elasticsearch_multi_instance_sweep "intel" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
summary_files+=("${LAST_MULTI_INSTANCE_SUMMARY_FILE}")
instance_summary_files+=("${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}")
printf 'elasticsearch\t%s\t%s\t%s\n' "${LAST_MULTI_INSTANCE_RUN_DIR}" "${LAST_MULTI_INSTANCE_SUMMARY_FILE}" "${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}" >> "${manifest_file}"

run_orientdb_multi_instance_sweep "intel" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
summary_files+=("${LAST_MULTI_INSTANCE_SUMMARY_FILE}")
instance_summary_files+=("${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}")
printf 'orientdb\t%s\t%s\t%s\n' "${LAST_MULTI_INSTANCE_RUN_DIR}" "${LAST_MULTI_INSTANCE_SUMMARY_FILE}" "${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}" >> "${manifest_file}"

combine_tsvs "${combined_summary}" "${summary_files[@]}"
combine_tsvs "${combined_instance_summary}" "${instance_summary_files[@]}"

log "Combined multi-instance summaries: ${combined_summary}"
