#!/usr/bin/env bash
set -euo pipefail

WRAPPER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${WRAPPER_DIR}/common.sh"

usage() {
  cat <<'EOF'
Usage: run_local_full_experiments.sh [options]

Run the current-host YCSB experiment bundle and emit combined TSVs.

Experiments:
  multi-instance     First experiment. Fixed working set size per instance and sweep instance_count.
  working-set-size   Second experiment. Single instance and sweep working set size.
  all                Run both experiments. This is the default.

Options:
  --experiment MODE              all | multi-instance | working-set-size
  --backends "LIST"              Backends to run: rocksdb, elasticsearch, orientdb
  --multi-instance-size-mb N     Working set size for the first experiment. Default: 16
  --instance-counts "LIST"       Instance counts for the first experiment. Default: 1..16
  --working-set-sizes "LIST"     Sizes for the second experiment. Default: built-in size list
  --record-bytes N               Approximate bytes per record used for working set sizing
  --field-length N               YCSB fieldlength override to pair with --record-bytes
  --output-dir DIR               Bundle output directory under results/
  --help                         Show this help

Examples:
  run_local_full_experiments.sh
  run_local_full_experiments.sh --backends "rocksdb orientdb"
  run_local_full_experiments.sh --experiment multi-instance --multi-instance-size-mb 64
  run_local_full_experiments.sh --experiment all --multi-instance-size-mb 32 --instance-counts "1 2 4 8 16"
  run_local_full_experiments.sh --experiment working-set-size --record-bytes 2048 --field-length 2038
EOF
}

detect_arch() {
  local vendor
  vendor="$(lscpu | awk -F: '/Vendor ID:/ {gsub(/^[[:space:]]+/, "", $2); print $2; exit}')"

  case "${vendor}" in
    AuthenticAMD)
      printf 'amd\n'
      ;;
    GenuineIntel)
      printf 'intel\n'
      ;;
    *)
      die "unsupported CPU vendor: ${vendor:-unknown}"
      ;;
  esac
}

resolve_backend_list() {
  local backend_spec="$1"
  local -n output_ref="$2"
  local normalized_spec
  local backend
  local -A seen=()

  if [[ -z "${backend_spec}" ]]; then
    normalized_spec="rocksdb elasticsearch orientdb"
  else
    normalized_spec="${backend_spec//,/ }"
  fi

  output_ref=()
  for backend in ${normalized_spec}; do
    case "${backend}" in
      rocksdb|elasticsearch|orientdb)
        ;;
      *)
        die "unsupported backend: ${backend}"
        ;;
    esac

    if [[ -z "${seen[$backend]+x}" ]]; then
      output_ref+=("${backend}")
      seen["${backend}"]=1
    fi
  done

  [[ "${#output_ref[@]}" -gt 0 ]] || die "--backends resolved to an empty set"
}

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

append_manifest_row() {
  local manifest_file="$1"
  local experiment="$2"
  local backend="$3"
  local run_dir="$4"
  local summary_file="$5"
  local instance_summary_file="$6"

  printf '%s\t%s\t%s\t%s\t%s\n' \
    "${experiment}" "${backend}" "${run_dir}" "${summary_file}" "${instance_summary_file}" \
    >> "${manifest_file}"
}

append_single_instance_summary_header() {
  local output_file="$1"

  printf 'timestamp_utc\thost\tarch\tbackend\tmode\tevent_name\tperf_scope\tsize_mb\trecordcount\toperationcount\tthreads\tcpu_affinity\tjava_active_processor_count\tload_threads\tload_cpu_affinity\tload_java_active_processor_count\trocksdb_parallelism\trocksdb_max_background_jobs\trocksdb_max_background_compactions\trocksdb_max_background_flushes\tes_java_heap_opts\tload_es_processors\tes_processors\tes_http_enabled\torientdb_url\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tstall_cycles\tcycles\tinstructions\ttask_clock_ms\tthroughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\n' > "${output_file}"
}

append_normalized_rocksdb_rows() {
  local input_file="$1"
  local output_file="$2"

  awk -F'\t' 'BEGIN { OFS = "\t" } NR > 1 {
    print $1, $2, $3, "rocksdb", "embedded", $4, $5, $6, $7, $8, $9, $10, $11,
          "NA", "NA", "NA", $12, $13, $14, $15,
          "NA", "NA", "NA", "NA", "NA",
          $16, $17, $18, $19, $20, $21, $22, $23,
          $24, $25, $26, $27, $28, $29, $30, $31, $32
  }' "${input_file}" >> "${output_file}"
}

append_normalized_elasticsearch_rows() {
  local input_file="$1"
  local output_file="$2"

  awk -F'\t' 'BEGIN { OFS = "\t" } NR > 1 {
    print $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
          $12, $14, $16, $11, $13, $15,
          "NA", "NA", "NA", "NA",
          $17, $18, $19, $20, "NA",
          $24, $25, $26, $27, $28, $29, $30, $31,
          $32, $33, $34, $35, $36, $37, $38, $39, $40
  }' "${input_file}" >> "${output_file}"
}

append_normalized_orientdb_rows() {
  local input_file="$1"
  local output_file="$2"

  awk -F'\t' 'BEGIN { OFS = "\t" } NR > 1 {
    print $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
          $11, $12, $13,
          "NA", "NA", "NA",
          "NA", "NA", "NA", "NA",
          "NA", "NA", "NA", "NA", $31,
          $14, $15, $16, $17, $18, $19, $20, $21,
          $22, $23, $24, $25, $26, $27, $28, $29, $30
  }' "${input_file}" >> "${output_file}"
}

run_multi_instance_bundle() {
  local arch="$1"
  local size_mb="$2"
  local instance_counts="$3"
  local bundle_dir="$4"
  local manifest_file="$5"
  local -n backends_ref="$6"
  local combined_summary="${bundle_dir}/multi_instance_summary.tsv"
  local combined_instance_summary="${bundle_dir}/multi_instance_instance_summary.tsv"
  local -a summary_files=()
  local -a instance_summary_files=()
  local saved_sizes="${SIZES_MB-}"
  local saved_instance_counts="${INSTANCE_COUNTS-}"
  local had_sizes=0
  local had_instance_counts=0

  if [[ ${SIZES_MB+x} ]]; then
    had_sizes=1
  fi
  if [[ ${INSTANCE_COUNTS+x} ]]; then
    had_instance_counts=1
  fi

  export SIZES_MB="${size_mb}"
  export INSTANCE_COUNTS="${instance_counts}"

  for backend in "${backends_ref[@]}"; do
    case "${backend}" in
      rocksdb)
        run_rocksdb_multi_instance_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        ;;
      elasticsearch)
        run_elasticsearch_multi_instance_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        ;;
      orientdb)
        run_orientdb_multi_instance_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        ;;
    esac

    summary_files+=("${LAST_MULTI_INSTANCE_SUMMARY_FILE}")
    instance_summary_files+=("${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}")
    append_manifest_row "${manifest_file}" "multi-instance" "${backend}" "${LAST_MULTI_INSTANCE_RUN_DIR}" "${LAST_MULTI_INSTANCE_SUMMARY_FILE}" "${LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE}"
  done

  combine_tsvs "${combined_summary}" "${summary_files[@]}"
  combine_tsvs "${combined_instance_summary}" "${instance_summary_files[@]}"

  if (( had_sizes )); then
    export SIZES_MB="${saved_sizes}"
  else
    unset SIZES_MB
  fi

  if (( had_instance_counts )); then
    export INSTANCE_COUNTS="${saved_instance_counts}"
  else
    unset INSTANCE_COUNTS
  fi

  LAST_BUNDLE_MULTI_SUMMARY="${combined_summary}"
  LAST_BUNDLE_MULTI_INSTANCE_SUMMARY="${combined_instance_summary}"
}

run_working_set_size_bundle() {
  local arch="$1"
  local working_set_sizes="$2"
  local bundle_dir="$3"
  local manifest_file="$4"
  local -n backends_ref="$5"
  local combined_summary="${bundle_dir}/working_set_size_summary.tsv"
  local backend
  local summary_file
  local saved_sizes="${SIZES_MB-}"
  local had_sizes=0

  if [[ ${SIZES_MB+x} ]]; then
    had_sizes=1
  fi

  if [[ -n "${working_set_sizes}" ]]; then
    export SIZES_MB="${working_set_sizes}"
  else
    unset SIZES_MB
  fi

  append_single_instance_summary_header "${combined_summary}"

  for backend in "${backends_ref[@]}"; do
    case "${backend}" in
      rocksdb)
        run_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        summary_file="${LAST_SINGLE_INSTANCE_SUMMARY_FILE}"
        append_manifest_row "${manifest_file}" "working-set-size" "${backend}" "${LAST_SINGLE_INSTANCE_RUN_DIR}" "${summary_file}" "NA"
        append_normalized_rocksdb_rows "${summary_file}" "${combined_summary}"
        ;;
      elasticsearch)
        run_elasticsearch_embedded_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        summary_file="${LAST_SINGLE_INSTANCE_SUMMARY_FILE}"
        append_manifest_row "${manifest_file}" "working-set-size" "${backend}" "${LAST_SINGLE_INSTANCE_RUN_DIR}" "${summary_file}" "NA"
        append_normalized_elasticsearch_rows "${summary_file}" "${combined_summary}"
        ;;
      orientdb)
        run_orientdb_embedded_sweep "${arch}" "${NO_RETIRED_INST_EVENT_NAME}" "${NO_RETIRED_INST_PERF_EVENT}"
        summary_file="${LAST_SINGLE_INSTANCE_SUMMARY_FILE}"
        append_manifest_row "${manifest_file}" "working-set-size" "${backend}" "${LAST_SINGLE_INSTANCE_RUN_DIR}" "${summary_file}" "NA"
        append_normalized_orientdb_rows "${summary_file}" "${combined_summary}"
        ;;
    esac
  done

  if (( had_sizes )); then
    export SIZES_MB="${saved_sizes}"
  else
    unset SIZES_MB
  fi

  LAST_BUNDLE_WORKING_SET_SUMMARY="${combined_summary}"
}

main() {
  local experiment="all"
  local multi_instance_size_mb="16"
  local instance_counts="${DEFAULT_INSTANCE_COUNTS[*]}"
  local working_set_sizes=""
  local backend_spec=""
  local record_bytes=""
  local field_length=""
  local output_dir=""
  local arch
  local host
  local now
  local manifest_file
  local -a selected_backends=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --experiment)
        [[ $# -ge 2 ]] || die "--experiment requires a value"
        experiment="$2"
        shift 2
        ;;
      --backends)
        [[ $# -ge 2 ]] || die "--backends requires a value"
        backend_spec="$2"
        shift 2
        ;;
      --multi-instance-size-mb)
        [[ $# -ge 2 ]] || die "--multi-instance-size-mb requires a value"
        multi_instance_size_mb="$2"
        shift 2
        ;;
      --instance-counts)
        [[ $# -ge 2 ]] || die "--instance-counts requires a value"
        instance_counts="$2"
        shift 2
        ;;
      --working-set-sizes)
        [[ $# -ge 2 ]] || die "--working-set-sizes requires a value"
        working_set_sizes="$2"
        shift 2
        ;;
      --record-bytes|--approx-record-bytes)
        [[ $# -ge 2 ]] || die "--record-bytes requires a value"
        record_bytes="$2"
        shift 2
        ;;
      --field-length)
        [[ $# -ge 2 ]] || die "--field-length requires a value"
        field_length="$2"
        shift 2
        ;;
      --output-dir)
        [[ $# -ge 2 ]] || die "--output-dir requires a value"
        output_dir="$2"
        shift 2
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done

  case "${experiment}" in
    all|multi-instance|working-set-size)
      ;;
    *)
      die "--experiment must be one of: all, multi-instance, working-set-size"
      ;;
  esac

  [[ "${multi_instance_size_mb}" =~ ^[0-9]+$ ]] || die "--multi-instance-size-mb must be an integer MiB value"
  [[ -z "${record_bytes}" || "${record_bytes}" =~ ^[1-9][0-9]*$ ]] || die "--record-bytes must be a positive integer"
  [[ -z "${field_length}" || "${field_length}" =~ ^[1-9][0-9]*$ ]] || die "--field-length must be a positive integer"
  resolve_backend_list "${backend_spec}" selected_backends

  arch="$(detect_arch)"
  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"

  if [[ -n "${record_bytes}" ]]; then
    export APPROX_RECORD_BYTES="${record_bytes}"
  fi

  if [[ -n "${field_length}" ]]; then
    export FIELD_LENGTH="${field_length}"
  fi

  if [[ -z "${output_dir}" ]]; then
    output_dir="${RESULTS_DIR}/${arch}_experiment_bundle_${host}_${now}_$$"
  fi

  mkdir -p "${output_dir}"
  manifest_file="${output_dir}/manifest.tsv"
  printf 'experiment\tbackend\trun_dir\tsummary_file\tinstance_summary_file\n' > "${manifest_file}"

  log "arch=${arch} experiment=${experiment} backends=${selected_backends[*]} multi_instance_size_mb=${multi_instance_size_mb} instance_counts=${instance_counts} working_set_sizes=${working_set_sizes:-default} record_bytes=${APPROX_RECORD_BYTES:-${DEFAULT_RECORD_VALUE_BYTES}} field_length=${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"

  if [[ "${experiment}" == "all" || "${experiment}" == "multi-instance" ]]; then
    run_multi_instance_bundle "${arch}" "${multi_instance_size_mb}" "${instance_counts}" "${output_dir}" "${manifest_file}" selected_backends
    log "Multi-instance summary: ${LAST_BUNDLE_MULTI_SUMMARY}"
    log "Multi-instance instance summary: ${LAST_BUNDLE_MULTI_INSTANCE_SUMMARY}"
  fi

  if [[ "${experiment}" == "all" || "${experiment}" == "working-set-size" ]]; then
    run_working_set_size_bundle "${arch}" "${working_set_sizes}" "${output_dir}" "${manifest_file}" selected_backends
    log "Working-set-size summary: ${LAST_BUNDLE_WORKING_SET_SUMMARY}"
  fi

  printf 'output_dir=%s\n' "${output_dir}"
  [[ -n "${LAST_BUNDLE_MULTI_SUMMARY:-}" ]] && printf 'multi_instance_summary=%s\n' "${LAST_BUNDLE_MULTI_SUMMARY}"
  [[ -n "${LAST_BUNDLE_MULTI_INSTANCE_SUMMARY:-}" ]] && printf 'multi_instance_instance_summary=%s\n' "${LAST_BUNDLE_MULTI_INSTANCE_SUMMARY}"
  [[ -n "${LAST_BUNDLE_WORKING_SET_SUMMARY:-}" ]] && printf 'working_set_size_summary=%s\n' "${LAST_BUNDLE_WORKING_SET_SUMMARY}"
  printf 'manifest=%s\n' "${manifest_file}"
}

main "$@"
