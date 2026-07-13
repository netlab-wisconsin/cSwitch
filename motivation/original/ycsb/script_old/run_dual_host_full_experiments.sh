#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

usage() {
  cat <<'EOF'
Usage: run_dual_host_full_experiments.sh [options]

Run the experiment bundle on the current host and on the Intel remote host,
then fetch the remote results back into the local results directory.

Options:
  --experiment MODE              all | multi-instance | working-set-size
  --backends "LIST"              Backends to run: rocksdb, elasticsearch, orientdb
  --multi-instance-size-mb N     Fixed per-instance working set for experiment 1
  --instance-counts "LIST"       Instance counts for experiment 1
  --working-set-sizes "LIST"     Sizes for experiment 2
  --record-bytes N               Approximate bytes per record used for sizing
  --field-length N               YCSB fieldlength override
  --intel-host HOST              Remote Intel host. Default: seunghyun@madracks-snode1
  --remote-root DIR              Remote YCSB root. Default: /home/seunghyun/ycsb
  --bundle-dir DIR               Local directory for dual-host logs/manifest
  --skip-sync                    Do not rsync the local YCSB tree to the remote host
  --help                         Show this help

Examples:
  run_dual_host_full_experiments.sh
  run_dual_host_full_experiments.sh --backends "rocksdb orientdb"
  run_dual_host_full_experiments.sh --experiment multi-instance --multi-instance-size-mb 64
  run_dual_host_full_experiments.sh --record-bytes 2048 --field-length 2038
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

ensure_dual_host_commands() {
  local cmd
  for cmd in rsync ssh tee; do
    command -v "${cmd}" >/dev/null 2>&1 || die "required command not found: ${cmd}"
  done
}

run_with_live_logs() {
  local stdout_log="$1"
  local stderr_log="$2"
  shift 2

  "$@" > >(tee "${stdout_log}") 2> >(tee "${stderr_log}" >&2)
}

append_manifest_row() {
  local manifest_file="$1"
  local host_role="$2"
  local host_name="$3"
  local output_dir="$4"
  local stdout_log="$5"
  local stderr_log="$6"

  printf '%s\t%s\t%s\t%s\t%s\n' \
    "${host_role}" "${host_name}" "${output_dir}" "${stdout_log}" "${stderr_log}" \
    >> "${manifest_file}"
}

build_remote_env_args() {
  local -n output_ref="$1"
  local var_name
  local -a forwarded_vars=(
    OPERATION_COUNT
    LOAD_THREADS
    THREADS
    CPU_START
    CPU_AFFINITY
    LOAD_CPU_AFFINITY
    JAVA_ACTIVE_PROCESSOR_COUNT
    LOAD_JAVA_ACTIVE_PROCESSOR_COUNT
    ROCKSDB_PARALLELISM
    ROCKSDB_MAX_BACKGROUND_JOBS
    ROCKSDB_MAX_BACKGROUND_COMPACTIONS
    ROCKSDB_MAX_BACKGROUND_FLUSHES
    ES_JAVA_HEAP_OPTS
    LOAD_ES_PROCESSORS
    ES_PROCESSORS
    ES_HTTP_ENABLED
    ES_INDEX_KEY
    ES_NUMBER_OF_SHARDS
    ES_NUMBER_OF_REPLICAS
    FIELD_COUNT
    FIELD_LENGTH
    APPROX_RECORD_BYTES
    RECORD_BYTES
    FAIL_FAST
  )

  output_ref=()
  for var_name in "${forwarded_vars[@]}"; do
    if [[ ${!var_name+x} ]]; then
      output_ref+=("${var_name}=${!var_name}")
    fi
  done
}

main() {
  local experiment="all"
  local backends=""
  local multi_instance_size_mb="16"
  local instance_counts="${DEFAULT_INSTANCE_COUNTS[*]}"
  local working_set_sizes=""
  local record_bytes=""
  local field_length=""
  local intel_host="seunghyun@madracks-snode1"
  local remote_root="/home/seunghyun/ycsb"
  local bundle_dir=""
  local skip_sync=0
  local local_arch
  local local_host
  local local_tag
  local remote_host_label
  local manifest_file
  local local_output_dir
  local remote_output_dir
  local fetched_remote_dir
  local local_stdout_log
  local local_stderr_log
  local remote_stdout_log
  local remote_stderr_log
  local -a runner_args=()
  local -a local_cmd=()
  local -a remote_env_args=()
  local -a remote_cmd=()
  local remote_cmd_str

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --experiment)
        [[ $# -ge 2 ]] || die "--experiment requires a value"
        experiment="$2"
        shift 2
        ;;
      --backends)
        [[ $# -ge 2 ]] || die "--backends requires a value"
        backends="$2"
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
      --intel-host)
        [[ $# -ge 2 ]] || die "--intel-host requires a value"
        intel_host="$2"
        shift 2
        ;;
      --remote-root)
        [[ $# -ge 2 ]] || die "--remote-root requires a value"
        remote_root="$2"
        shift 2
        ;;
      --bundle-dir)
        [[ $# -ge 2 ]] || die "--bundle-dir requires a value"
        bundle_dir="$2"
        shift 2
        ;;
      --skip-sync)
        skip_sync=1
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

  ensure_dual_host_commands

  local_arch="$(detect_arch)"
  local_host="$(hostname -s)"
  local_tag="$(date -u +'%Y%m%dT%H%M%SZ')"
  remote_host_label="${intel_host##*@}"
  remote_host_label="${remote_host_label%%.*}"

  if [[ -z "${bundle_dir}" ]]; then
    bundle_dir="${RESULTS_DIR}/dual_host_experiment_bundle_${local_host}_${remote_host_label}_${local_tag}_$$"
  fi

  mkdir -p "${bundle_dir}"
  manifest_file="${bundle_dir}/manifest.tsv"
  printf 'host_role\thost_name\toutput_dir\tstdout_log\tstderr_log\n' > "${manifest_file}"

  local_output_dir="${RESULTS_DIR}/${local_arch}_experiment_bundle_${local_host}_${local_tag}_$$"
  remote_output_dir="${remote_root}/results/intel_experiment_bundle_${remote_host_label}_${local_tag}_$$"
  fetched_remote_dir="${RESULTS_DIR}/$(basename "${remote_output_dir}")"
  local_stdout_log="${bundle_dir}/local.stdout.log"
  local_stderr_log="${bundle_dir}/local.stderr.log"
  remote_stdout_log="${bundle_dir}/remote.stdout.log"
  remote_stderr_log="${bundle_dir}/remote.stderr.log"

  runner_args=(--experiment "${experiment}" --multi-instance-size-mb "${multi_instance_size_mb}" --instance-counts "${instance_counts}")
  if [[ -n "${backends}" ]]; then
    runner_args+=(--backends "${backends}")
  fi
  if [[ -n "${working_set_sizes}" ]]; then
    runner_args+=(--working-set-sizes "${working_set_sizes}")
  fi
  if [[ -n "${record_bytes}" ]]; then
    runner_args+=(--record-bytes "${record_bytes}")
  fi
  if [[ -n "${field_length}" ]]; then
    runner_args+=(--field-length "${field_length}")
  fi

  log "dual-host bundle=${bundle_dir} local_output_dir=${local_output_dir} remote_output_dir=${remote_output_dir}"

  if (( ! skip_sync )); then
    log "Syncing ${YCSB_ROOT} to ${intel_host}:${remote_root}"
    rsync -a --exclude 'results/' "${YCSB_ROOT}/" "${intel_host}:${remote_root}/"
  fi

  local_cmd=("${SCRIPT_DIR}/run_local_full_experiments.sh" "${runner_args[@]}" --output-dir "${local_output_dir}")
  log "Running locally on ${local_host}"
  run_with_live_logs "${local_stdout_log}" "${local_stderr_log}" "${local_cmd[@]}"
  append_manifest_row "${manifest_file}" "local" "${local_host}" "${local_output_dir}" "${local_stdout_log}" "${local_stderr_log}"

  remote_cmd=("${remote_root}/scripts/run_local_full_experiments.sh" "${runner_args[@]}" --output-dir "${remote_output_dir}")
  build_remote_env_args remote_env_args
  if [[ "${#remote_env_args[@]}" -gt 0 ]]; then
    remote_cmd=(env "${remote_env_args[@]}" "${remote_cmd[@]}")
  fi
  printf -v remote_cmd_str '%q ' "${remote_cmd[@]}"

  log "Running remotely on ${intel_host}"
  run_with_live_logs "${remote_stdout_log}" "${remote_stderr_log}" ssh "${intel_host}" "bash -lc $(printf '%q' "${remote_cmd_str}")"
  append_manifest_row "${manifest_file}" "remote" "${remote_host_label}" "${remote_output_dir}" "${remote_stdout_log}" "${remote_stderr_log}"

  log "Fetching remote results into ${RESULTS_DIR}"
  rsync -a "${intel_host}:${remote_output_dir}" "${RESULTS_DIR}/"

  printf 'bundle_dir=%s\n' "${bundle_dir}"
  printf 'local_output_dir=%s\n' "${local_output_dir}"
  printf 'fetched_remote_output_dir=%s\n' "${fetched_remote_dir}"
  printf 'manifest=%s\n' "${manifest_file}"
}

main "$@"
