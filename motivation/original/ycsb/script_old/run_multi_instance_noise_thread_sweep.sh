#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

readonly DEFAULT_NOISE_CONFIG="/home/seunghyun/gapbs/profiler/config_stress200s_72t.xml"
readonly DEFAULT_MEMORY_BENCHMARK_BIN="/home/seunghyun/ycsb/memory_benchmark"
readonly DEFAULT_YCSB_RUNNER="${SCRIPT_DIR}/run_local_full_experiments.sh"
readonly DEFAULT_NOISE_THREAD_COUNTS=(1 2 3 4 5 6 13 20 27 34 41 48 55 62 69 76 83)
readonly DEFAULT_NOISE_MULTI_INSTANCE_SIZE_MB=2048
readonly DEFAULT_NOISE_BACKENDS="rocksdb"
readonly DEFAULT_NOISE_INSTANCE_COUNTS="1"
readonly DEFAULT_NOISE_RECORD_BYTES=16384
readonly DEFAULT_NOISE_FIELD_LENGTH=16374
readonly DEFAULT_NOISE_STARTUP_SECONDS=2

CURRENT_NOISE_PID=""

usage() {
  cat <<'EOF'
Usage: run_multi_instance_noise_thread_sweep.sh [options]

Run YCSB multi-instance bundles while a background memory_benchmark noise job is active.

Defaults match the current RocksDB experiment:
  --backends "rocksdb"
  --instance-counts "1"
  --multi-instance-size-mb 2048
  --record-bytes 16384
  --field-length 16374
  --noise-thread-counts "1 2 3 4 5 6 13 20 27 34 41 48 55 62 69 76 83"

Options:
  --config PATH                 XML template for memory_benchmark
  --memory-benchmark PATH       memory_benchmark binary
  --ycsb-runner PATH            run_local_full_experiments.sh path
  --noise-thread-counts "LIST"  Space/comma-separated nthreads values
  --backends "LIST"             Backends for the YCSB bundle
  --instance-counts "LIST"      YCSB instance counts
  --multi-instance-size-mb N    Per-instance working set size
  --record-bytes N              Approximate bytes per record
  --field-length N              YCSB fieldlength
  --noise-startup-seconds N     Wait time after starting noise before YCSB
  --output-dir DIR              Bundle output directory
  --keep-going                  Continue after a failed sweep iteration
  --help                        Show this help
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

resolve_noise_thread_counts() {
  local spec="$1"
  local -n output_ref="$2"
  local normalized_spec
  local thread_count
  local -A seen=()

  if [[ -z "${spec}" ]]; then
    output_ref=("${DEFAULT_NOISE_THREAD_COUNTS[@]}")
    return
  fi

  normalized_spec="${spec//,/ }"
  output_ref=()
  for thread_count in ${normalized_spec}; do
    [[ "${thread_count}" =~ ^[1-9][0-9]*$ ]] || die "invalid noise thread count: ${thread_count}"
    if [[ -z "${seen[$thread_count]+x}" ]]; then
      output_ref+=("${thread_count}")
      seen["${thread_count}"]=1
    fi
  done

  [[ "${#output_ref[@]}" -gt 0 ]] || die "--noise-thread-counts resolved to an empty set"
}

stop_noise_process() {
  local pid="$1"
  local wait_status=0
  local attempt

  [[ -n "${pid}" ]] || return 0

  if ! kill -0 "${pid}" 2>/dev/null; then
    if ! wait "${pid}" 2>/dev/null; then
      wait_status="$?"
    fi
    return "${wait_status}"
  fi

  kill "${pid}" 2>/dev/null || true
  for attempt in $(seq 1 50); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      if ! wait "${pid}" 2>/dev/null; then
        wait_status="$?"
      fi
      return "${wait_status}"
    fi
    sleep 0.1
  done

  kill -9 "${pid}" 2>/dev/null || true
  if ! wait "${pid}" 2>/dev/null; then
    wait_status="$?"
  fi
  return "${wait_status}"
}

cleanup_current_noise() {
  if [[ -n "${CURRENT_NOISE_PID}" ]]; then
    stop_noise_process "${CURRENT_NOISE_PID}" || true
    CURRENT_NOISE_PID=""
  fi
}

write_noise_config() {
  local template_file="$1"
  local output_file="$2"
  local thread_count="$3"
  local latency_output="$4"

  NOISE_NTHREADS="${thread_count}" NOISE_OUTPUT="${latency_output}" \
    perl -0pe '
      s{<nthreads>\s*[^<]+\s*</nthreads>}{<nthreads>$ENV{NOISE_NTHREADS}</nthreads>}s;
      s{<output>\s*[^<]+\s*</output>}{<output>$ENV{NOISE_OUTPUT}</output>}s;
    ' "${template_file}" > "${output_file}"

  grep -q "<nthreads>${thread_count}</nthreads>" "${output_file}" || \
    die "failed to set nthreads=${thread_count} in ${output_file}"
  grep -q "<output>${latency_output}</output>" "${output_file}" || \
    die "failed to set output path in ${output_file}"
}

append_prefixed_header() {
  local input_file="$1"
  local output_file="$2"

  {
    printf 'noise_nthreads\tnoise_config_file\tnoise_stdout_log\tnoise_stderr_log\tnoise_latency_output\tycsb_bundle_dir\t'
    head -n 1 "${input_file}"
  } > "${output_file}"
}

append_prefixed_rows() {
  local input_file="$1"
  local output_file="$2"
  local thread_count="$3"
  local config_file="$4"
  local noise_stdout="$5"
  local noise_stderr="$6"
  local noise_latency="$7"
  local ycsb_bundle_dir="$8"

  awk -F'\t' -v OFS='\t' \
    -v thread_count="${thread_count}" \
    -v config_file="${config_file}" \
    -v noise_stdout="${noise_stdout}" \
    -v noise_stderr="${noise_stderr}" \
    -v noise_latency="${noise_latency}" \
    -v ycsb_bundle_dir="${ycsb_bundle_dir}" \
    'NR > 1 { print thread_count, config_file, noise_stdout, noise_stderr, noise_latency, ycsb_bundle_dir, $0 }' \
    "${input_file}" >> "${output_file}"
}

main() {
  local config_file="${DEFAULT_NOISE_CONFIG}"
  local memory_benchmark_bin="${DEFAULT_MEMORY_BENCHMARK_BIN}"
  local ycsb_runner="${DEFAULT_YCSB_RUNNER}"
  local noise_thread_spec=""
  local backends="${DEFAULT_NOISE_BACKENDS}"
  local instance_counts="${DEFAULT_NOISE_INSTANCE_COUNTS}"
  local multi_instance_size_mb="${DEFAULT_NOISE_MULTI_INSTANCE_SIZE_MB}"
  local record_bytes="${DEFAULT_NOISE_RECORD_BYTES}"
  local field_length="${DEFAULT_NOISE_FIELD_LENGTH}"
  local noise_startup_seconds="${DEFAULT_NOISE_STARTUP_SECONDS}"
  local output_dir=""
  local keep_going=0
  local -a noise_thread_counts=()
  local arch
  local host
  local now
  local manifest_file
  local combined_summary
  local combined_instance_summary
  local had_summary_header=0
  local had_instance_summary_header=0
  local thread_count

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --config)
        [[ $# -ge 2 ]] || die "--config requires a value"
        config_file="$2"
        shift 2
        ;;
      --memory-benchmark)
        [[ $# -ge 2 ]] || die "--memory-benchmark requires a value"
        memory_benchmark_bin="$2"
        shift 2
        ;;
      --ycsb-runner)
        [[ $# -ge 2 ]] || die "--ycsb-runner requires a value"
        ycsb_runner="$2"
        shift 2
        ;;
      --noise-thread-counts)
        [[ $# -ge 2 ]] || die "--noise-thread-counts requires a value"
        noise_thread_spec="$2"
        shift 2
        ;;
      --backends)
        [[ $# -ge 2 ]] || die "--backends requires a value"
        backends="$2"
        shift 2
        ;;
      --instance-counts)
        [[ $# -ge 2 ]] || die "--instance-counts requires a value"
        instance_counts="$2"
        shift 2
        ;;
      --multi-instance-size-mb)
        [[ $# -ge 2 ]] || die "--multi-instance-size-mb requires a value"
        multi_instance_size_mb="$2"
        shift 2
        ;;
      --record-bytes)
        [[ $# -ge 2 ]] || die "--record-bytes requires a value"
        record_bytes="$2"
        shift 2
        ;;
      --field-length)
        [[ $# -ge 2 ]] || die "--field-length requires a value"
        field_length="$2"
        shift 2
        ;;
      --noise-startup-seconds)
        [[ $# -ge 2 ]] || die "--noise-startup-seconds requires a value"
        noise_startup_seconds="$2"
        shift 2
        ;;
      --output-dir)
        [[ $# -ge 2 ]] || die "--output-dir requires a value"
        output_dir="$2"
        shift 2
        ;;
      --keep-going)
        keep_going=1
        shift
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

  [[ -f "${config_file}" ]] || die "config file not found: ${config_file}"
  [[ -x "${memory_benchmark_bin}" ]] || die "memory_benchmark binary not executable: ${memory_benchmark_bin}"
  [[ -x "${ycsb_runner}" ]] || die "YCSB runner not executable: ${ycsb_runner}"
  [[ "${multi_instance_size_mb}" =~ ^[1-9][0-9]*$ ]] || die "--multi-instance-size-mb must be a positive integer"
  [[ "${record_bytes}" =~ ^[1-9][0-9]*$ ]] || die "--record-bytes must be a positive integer"
  [[ "${field_length}" =~ ^[1-9][0-9]*$ ]] || die "--field-length must be a positive integer"
  [[ "${noise_startup_seconds}" =~ ^[0-9]+$ ]] || die "--noise-startup-seconds must be a non-negative integer"

  resolve_noise_thread_counts "${noise_thread_spec}" noise_thread_counts

  arch="$(detect_arch)"
  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"

  if [[ -z "${output_dir}" ]]; then
    output_dir="${RESULTS_DIR}/${arch}_noise_thread_sweep_${host}_${now}_$$"
  fi

  mkdir -p "${output_dir}"
  manifest_file="${output_dir}/manifest.tsv"
  combined_summary="${output_dir}/multi_instance_summary.tsv"
  combined_instance_summary="${output_dir}/multi_instance_instance_summary.tsv"
  printf 'noise_nthreads\tconfig_file\tnoise_stdout_log\tnoise_stderr_log\tnoise_latency_output\tycsb_driver_log\tycsb_bundle_dir\tmulti_instance_summary\tmulti_instance_instance_summary\tycsb_status\tnoise_stop_status\n' > "${manifest_file}"

  trap cleanup_current_noise EXIT INT TERM

  log "noise_thread_counts=$(join_by ',' "${noise_thread_counts[@]}") backends=${backends} instance_counts=${instance_counts} multi_instance_size_mb=${multi_instance_size_mb} record_bytes=${record_bytes} field_length=${field_length} config=${config_file}"

  for thread_count in "${noise_thread_counts[@]}"; do
    local run_tag
    local run_dir
    local rewritten_config
    local noise_stdout
    local noise_stderr
    local noise_latency_output
    local ycsb_driver_log
    local ycsb_bundle_dir
    local ycsb_status=0
    local noise_stop_status=0
    local ycsb_multi_summary=""
    local ycsb_multi_instance_summary=""

    run_tag="$(printf '%02dt' "${thread_count}")"
    run_dir="${output_dir}/runs/${run_tag}"
    rewritten_config="${run_dir}/memory_benchmark.${run_tag}.xml"
    noise_stdout="${run_dir}/memory_benchmark.stdout.log"
    noise_stderr="${run_dir}/memory_benchmark.stderr.log"
    noise_latency_output="${run_dir}/memory_benchmark.latency.txt"
    ycsb_driver_log="${run_dir}/run_local_full_experiments.log"
    ycsb_bundle_dir="${run_dir}/ycsb_bundle"

    mkdir -p "${run_dir}"
    write_noise_config "${config_file}" "${rewritten_config}" "${thread_count}" "${noise_latency_output}"

    log "[noise=${thread_count}] starting memory_benchmark"
    "${memory_benchmark_bin}" --config "${rewritten_config}" > "${noise_stdout}" 2> "${noise_stderr}" &
    CURRENT_NOISE_PID="$!"
    sleep "${noise_startup_seconds}"

    log "[noise=${thread_count}] starting YCSB multi-instance bundle"
    set +e
    "${ycsb_runner}" \
      --experiment multi-instance \
      --backends "${backends}" \
      --instance-counts "${instance_counts}" \
      --multi-instance-size-mb "${multi_instance_size_mb}" \
      --record-bytes "${record_bytes}" \
      --field-length "${field_length}" \
      --output-dir "${ycsb_bundle_dir}" 2>&1 | tee "${ycsb_driver_log}"
    ycsb_status=${PIPESTATUS[0]}
    set -e

    if ! stop_noise_process "${CURRENT_NOISE_PID}"; then
      noise_stop_status="$?"
    fi
    CURRENT_NOISE_PID=""

    if [[ -f "${ycsb_driver_log}" ]]; then
      ycsb_multi_summary="$(awk -F= '/^multi_instance_summary=/{print $2; exit}' "${ycsb_driver_log}")"
      ycsb_multi_instance_summary="$(awk -F= '/^multi_instance_instance_summary=/{print $2; exit}' "${ycsb_driver_log}")"
    fi

    if [[ "${ycsb_status}" == "0" ]]; then
      [[ -n "${ycsb_multi_summary}" && -f "${ycsb_multi_summary}" ]] || die "missing multi_instance_summary for noise=${thread_count}"
      [[ -n "${ycsb_multi_instance_summary}" && -f "${ycsb_multi_instance_summary}" ]] || die "missing multi_instance_instance_summary for noise=${thread_count}"

      if (( had_summary_header == 0 )); then
        append_prefixed_header "${ycsb_multi_summary}" "${combined_summary}"
        had_summary_header=1
      fi
      append_prefixed_rows "${ycsb_multi_summary}" "${combined_summary}" \
        "${thread_count}" "${rewritten_config}" "${noise_stdout}" "${noise_stderr}" "${noise_latency_output}" "${ycsb_bundle_dir}"

      if (( had_instance_summary_header == 0 )); then
        append_prefixed_header "${ycsb_multi_instance_summary}" "${combined_instance_summary}"
        had_instance_summary_header=1
      fi
      append_prefixed_rows "${ycsb_multi_instance_summary}" "${combined_instance_summary}" \
        "${thread_count}" "${rewritten_config}" "${noise_stdout}" "${noise_stderr}" "${noise_latency_output}" "${ycsb_bundle_dir}"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "${thread_count}" "${rewritten_config}" "${noise_stdout}" "${noise_stderr}" "${noise_latency_output}" \
      "${ycsb_driver_log}" "${ycsb_bundle_dir}" "${ycsb_multi_summary:-NA}" "${ycsb_multi_instance_summary:-NA}" \
      "${ycsb_status}" "${noise_stop_status}" >> "${manifest_file}"

    if [[ "${ycsb_status}" != "0" && "${keep_going}" != "1" ]]; then
      die "YCSB bundle failed for noise thread count ${thread_count}; see ${ycsb_driver_log}"
    fi
  done

  printf 'output_dir=%s\n' "${output_dir}"
  [[ -f "${combined_summary}" ]] && printf 'multi_instance_summary=%s\n' "${combined_summary}"
  [[ -f "${combined_instance_summary}" ]] && printf 'multi_instance_instance_summary=%s\n' "${combined_instance_summary}"
  printf 'manifest=%s\n' "${manifest_file}"
}

main "$@"
