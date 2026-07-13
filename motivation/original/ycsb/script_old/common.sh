#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly YCSB_ROOT="${YCSB_ROOT:-/home/seunghyun/ycsb}"
readonly YCSB_HOME="${YCSB_HOME:-${YCSB_ROOT}/YCSB}"
readonly WORKLOAD_DIR="${WORKLOAD_DIR:-${YCSB_ROOT}/workloads}"
readonly RESULTS_DIR="${RESULTS_DIR:-${YCSB_ROOT}/results}"

readonly DEFAULT_FIELD_COUNT=1
readonly DEFAULT_FIELD_LENGTH=1014
readonly DEFAULT_RECORD_VALUE_BYTES=1024
readonly DEFAULT_OPERATION_COUNT=1000000
readonly DEFAULT_THREADS=1
readonly DEFAULT_CPU_AFFINITY=5
readonly DEFAULT_CPU_START=0
readonly DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT=1
readonly DEFAULT_ROCKSDB_PARALLELISM=1
readonly DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS=1
readonly DEFAULT_ROCKSDB_MAX_BACKGROUND_COMPACTIONS=1
readonly DEFAULT_ROCKSDB_MAX_BACKGROUND_FLUSHES=1
readonly DEFAULT_ES_OPERATION_COUNT=10000
readonly DEFAULT_ORIENTDB_OPERATION_COUNT=10000
readonly DEFAULT_ES_LOAD_THREADS=8
readonly DEFAULT_ES_MULTI_INSTANCE_LOAD_THREADS=1
readonly DEFAULT_ES_HTTP_ENABLED=false
readonly DEFAULT_ES_PROCESSORS=1
readonly DEFAULT_ES_JAVA_HEAP_OPTS="-Xms2g -Xmx2g"
readonly DEFAULT_ES_INDEX_KEY=es.ycsb
readonly DEFAULT_ES_NUMBER_OF_SHARDS=1
readonly DEFAULT_ES_NUMBER_OF_REPLICAS=0
readonly DEFAULT_MAX_INSTANCE_COUNT=16
readonly DEFAULT_INSTANCE_COUNTS=(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16)
readonly DEFAULT_SIZES_MB=(1 2 4 8 16 24 32 48 64 96 128 192 256 384 512)
readonly DEFAULT_KEEP_RESULT_DB=0
readonly NO_RETIRED_INST_EVENT_NAME="NO_RETIRED_INST_CYCLES"
readonly NO_RETIRED_INST_PERF_EVENT='cpu/event=0xc0,cmask=1,inv=1/'

log() {
  printf '[%s] %s\n' "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
}

die() {
  log "ERROR: $*"
  exit 1
}

ensure_required_commands() {
  local cmd
  for cmd in awk getconf git hostname java lscpu mkdir mvn perf rm sed sort taskset; do
    command -v "$cmd" >/dev/null 2>&1 || die "required command not found: ${cmd}"
  done
  [[ -x /usr/bin/time ]] || die "required command not found: /usr/bin/time"
}

ensure_ycsb_checkout() {
  if [[ -f "${YCSB_HOME}/pom.xml" ]]; then
    return
  fi

  log "Cloning YCSB into ${YCSB_HOME}"
  mkdir -p "${YCSB_ROOT}"
  git clone https://github.com/brianfrankcooper/YCSB.git "${YCSB_HOME}"
}

build_ycsb_rocksdb() {
  local core_jar="${YCSB_HOME}/core/target/core-0.18.0-SNAPSHOT.jar"
  local rocksdb_jar="${YCSB_HOME}/rocksdb/target/rocksdb-binding-0.18.0-SNAPSHOT.jar"
  local needs_rebuild=0

  if ! [[ -f "${core_jar}" && -f "${rocksdb_jar}" ]] || \
     ! compgen -G "${YCSB_HOME}/core/target/dependency/*.jar" >/dev/null 2>&1; then
    needs_rebuild=1
  elif [[ "${YCSB_HOME}/pom.xml" -nt "${rocksdb_jar}" || \
          "${YCSB_HOME}/core/pom.xml" -nt "${core_jar}" || \
          "${YCSB_HOME}/rocksdb/pom.xml" -nt "${rocksdb_jar}" || \
          "${YCSB_HOME}/binding-parent/pom.xml" -nt "${rocksdb_jar}" ]]; then
    needs_rebuild=1
  elif find "${YCSB_HOME}/core/src" "${YCSB_HOME}/rocksdb/src" -type f -newer "${rocksdb_jar}" -print -quit | grep -q .; then
    needs_rebuild=1
  fi

  if [[ "${needs_rebuild}" == "0" ]]; then
    return
  fi

  log "Building YCSB core + rocksdb binding (source-run profile)"
  (
    cd "${YCSB_HOME}"
    mvn -Psource-run -pl site.ycsb:rocksdb-binding -am package -DskipTests
  )
}

build_ycsb_elasticsearch() {
  local core_jar="${YCSB_HOME}/core/target/core-0.18.0-SNAPSHOT.jar"
  local elasticsearch_jar="${YCSB_HOME}/elasticsearch/target/elasticsearch-binding-0.18.0-SNAPSHOT.jar"
  local needs_rebuild=0

  if ! [[ -f "${core_jar}" && -f "${elasticsearch_jar}" ]] || \
     ! compgen -G "${YCSB_HOME}/elasticsearch/target/dependency/*.jar" >/dev/null 2>&1; then
    needs_rebuild=1
  elif [[ "${YCSB_HOME}/pom.xml" -nt "${elasticsearch_jar}" || \
          "${YCSB_HOME}/core/pom.xml" -nt "${core_jar}" || \
          "${YCSB_HOME}/elasticsearch/pom.xml" -nt "${elasticsearch_jar}" || \
          "${YCSB_HOME}/binding-parent/pom.xml" -nt "${elasticsearch_jar}" ]]; then
    needs_rebuild=1
  elif find "${YCSB_HOME}/core/src" "${YCSB_HOME}/elasticsearch/src" -type f -newer "${elasticsearch_jar}" -print -quit | grep -q .; then
    needs_rebuild=1
  fi

  if [[ "${needs_rebuild}" == "0" ]]; then
    return
  fi

  log "Building YCSB core + elasticsearch binding (source-run profile)"
  (
    cd "${YCSB_HOME}"
    mvn -Psource-run -pl site.ycsb:elasticsearch-binding -am package -DskipTests
  )
}

build_ycsb_orientdb() {
  local core_jar="${YCSB_HOME}/core/target/core-0.18.0-SNAPSHOT.jar"
  local orientdb_jar="${YCSB_HOME}/orientdb/target/orientdb-binding-0.18.0-SNAPSHOT.jar"
  local needs_rebuild=0

  if ! [[ -f "${core_jar}" && -f "${orientdb_jar}" ]] || \
     ! compgen -G "${YCSB_HOME}/orientdb/target/dependency/*.jar" >/dev/null 2>&1 || \
     ! compgen -G "${YCSB_HOME}/core/target/dependency/*.jar" >/dev/null 2>&1; then
    needs_rebuild=1
  elif [[ "${YCSB_HOME}/pom.xml" -nt "${orientdb_jar}" || \
          "${YCSB_HOME}/core/pom.xml" -nt "${core_jar}" || \
          "${YCSB_HOME}/orientdb/pom.xml" -nt "${orientdb_jar}" || \
          "${YCSB_HOME}/binding-parent/pom.xml" -nt "${orientdb_jar}" ]]; then
    needs_rebuild=1
  elif find "${YCSB_HOME}/core/src" "${YCSB_HOME}/orientdb/src" -type f -newer "${orientdb_jar}" -print -quit | grep -q .; then
    needs_rebuild=1
  fi

  if [[ "${needs_rebuild}" == "0" ]]; then
    return
  fi

  log "Building YCSB core + orientdb binding (source-run profile)"
  (
    cd "${YCSB_HOME}"
    mvn -Psource-run -pl site.ycsb:orientdb-binding -am package -DskipTests
  )
}

ensure_workloads() {
  "${SCRIPT_DIR}/generate_workloads.sh"
}

assert_cpu_vendor() {
  local expected="$1"
  local actual
  actual="$(lscpu | awk -F: '/Vendor ID:/ {gsub(/^[[:space:]]+/, "", $2); print $2; exit}')"
  [[ -n "${actual}" ]] || die "could not detect CPU vendor"

  if [[ "${actual}" != "${expected}" ]]; then
    die "expected CPU vendor ${expected}, found ${actual}"
  fi
}

list_sizes() {
  if [[ -n "${SIZES_MB:-}" ]]; then
    printf '%s\n' ${SIZES_MB}
    return
  fi

  printf '%s\n' "${DEFAULT_SIZES_MB[@]}"
}

recordcount_for_size_mb() {
  local size_mb="$1"
  local record_value_bytes
  local recordcount

  record_value_bytes="$(record_value_bytes_per_record)"
  [[ "${record_value_bytes}" =~ ^[1-9][0-9]*$ ]] || die "record bytes must be a positive integer, got: ${record_value_bytes}"

  recordcount="$(((size_mb * 1024 * 1024) / record_value_bytes))"
  (( recordcount > 0 )) || die "record bytes ${record_value_bytes} are too large for size ${size_mb}MiB"
  printf '%s\n' "${recordcount}"
}

size_tag() {
  local size_mb="$1"
  printf '%03d' "${size_mb}"
}

count_tag() {
  local count="$1"
  printf '%02d' "${count}"
}

workload_file_for_size_mb() {
  local size_mb="$1"
  printf '%s/working_set_%smb.properties\n' "${WORKLOAD_DIR}" "$(size_tag "${size_mb}")"
}

perf_scope_label() {
  if [[ "${EUID}" -ne 0 && -r /proc/sys/kernel/perf_event_paranoid ]]; then
    local paranoid
    paranoid="$(< /proc/sys/kernel/perf_event_paranoid)"
    if (( paranoid >= 2 )); then
      printf 'user\n'
      return
    fi
  fi

  printf 'all\n'
}

warn_perf_scope() {
  local scope
  scope="$(perf_scope_label)"
  if [[ "${scope}" == "user" ]]; then
    local paranoid
    paranoid="$(< /proc/sys/kernel/perf_event_paranoid)"
    log "perf_event_paranoid=${paranoid}; perf will count user-space events only. Use sudo or lower the sysctl for full-space counts."
  fi
}

merged_java_opts() {
  local active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local java_opts="${JAVA_OPTS:-}"

  if [[ -n "${active_processor_count}" ]]; then
    if [[ -n "${java_opts}" ]]; then
      printf '%s -XX:ActiveProcessorCount=%s\n' "${java_opts}" "${active_processor_count}"
    else
      printf '%s\n' "-XX:ActiveProcessorCount=${active_processor_count}"
    fi
    return
  fi

  printf '%s\n' "${java_opts}"
}

record_value_bytes_per_record() {
  if [[ -n "${APPROX_RECORD_BYTES:-}" ]]; then
    printf '%s\n' "${APPROX_RECORD_BYTES}"
  elif [[ -n "${RECORD_BYTES:-}" ]]; then
    printf '%s\n' "${RECORD_BYTES}"
  else
    printf '%s\n' "${DEFAULT_RECORD_VALUE_BYTES}"
  fi
}

keep_result_db() {
  printf '%s\n' "${KEEP_RESULT_DB:-${DEFAULT_KEEP_RESULT_DB}}"
}

cleanup_result_db_path() {
  local path="$1"
  local parent

  if [[ "$(keep_result_db)" == "1" ]]; then
    return
  fi

  [[ -n "${path}" ]] || return
  if [[ -e "${path}" ]]; then
    rm -rf "${path}"
  fi

  parent="$(dirname "${path}")"
  while [[ -n "${parent}" && "${parent}" != "/" ]]; do
    rmdir "${parent}" 2>/dev/null || break
    parent="$(dirname "${parent}")"
  done
}

run_bound_command_with_extra_java_opts() {
  local extra_java_opts="$1"
  shift
  local cpu_affinity="${CPU_AFFINITY-${DEFAULT_CPU_AFFINITY}}"
  local java_opts

  java_opts="$(merged_java_opts)"
  if [[ -n "${extra_java_opts}" ]]; then
    if [[ -n "${java_opts}" ]]; then
      java_opts="${java_opts} ${extra_java_opts}"
    else
      java_opts="${extra_java_opts}"
    fi
  fi

  if [[ -n "${cpu_affinity}" ]]; then
    env JAVA_OPTS="${java_opts}" taskset -c "${cpu_affinity}" "$@"
  else
    env JAVA_OPTS="${java_opts}" "$@"
  fi
}

run_bound_command() {
  run_bound_command_with_extra_java_opts "" "$@"
}

parse_perf_counter() {
  local perf_file="$1"
  local event_prefix="$2"

  awk -F';' -v prefix="${event_prefix}" '
    index($3, prefix) == 1 {
      if ($1 == "" || $1 ~ /^</) {
        print "NA"
      } else {
        gsub(/,/, "", $1)
        print $1
      }
      found = 1
      exit
    }
    END {
      if (!found) {
        print "NA"
      }
    }
  ' "${perf_file}"
}

parse_ycsb_throughput() {
  local run_log="$1"

  awk -F', ' '
    /^\[OVERALL\], Throughput\(ops\/sec\), / {
      print $3
      found = 1
      exit
    }
    END {
      if (!found) {
        print "NA"
      }
    }
  ' "${run_log}"
}

parse_time_metric_ms() {
  local time_file="$1"
  local key="$2"

  awk -F'=' -v metric_key="${key}" '
    $1 == metric_key {
      if ($2 == "") {
        print "NA"
      } else {
        printf "%.3f\n", $2 * 1000.0
      }
      found = 1
      exit
    }
    END {
      if (!found) {
        print "NA"
      }
    }
  ' "${time_file}"
}

llc_hit_ratio() {
  local llc_loads="$1"
  local llc_load_misses="$2"

  awk -v loads="${llc_loads}" -v misses="${llc_load_misses}" '
    BEGIN {
      if (loads == "" || misses == "" || loads == "NA" || misses == "NA" || loads <= 0) {
        print "NA"
        exit
      }
      printf "%.6f\n", (loads - misses) / loads
    }
  '
}

append_summary_header() {
  local summary_file="$1"

  if [[ -f "${summary_file}" ]]; then
    return
  fi

  printf 'timestamp_utc\thost\tarch\tevent_name\tperf_scope\tsize_mb\trecordcount\toperationcount\tthreads\tcpu_affinity\tjava_active_processor_count\trocksdb_parallelism\trocksdb_max_background_jobs\trocksdb_max_background_compactions\trocksdb_max_background_flushes\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tstall_cycles\tcycles\tinstructions\ttask_clock_ms\tthroughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\n' > "${summary_file}"
}

append_elasticsearch_summary_header() {
  local summary_file="$1"

  if [[ -f "${summary_file}" ]]; then
    return
  fi

  printf 'timestamp_utc\thost\tarch\tbackend\tmode\tevent_name\tperf_scope\tsize_mb\trecordcount\toperationcount\tload_threads\trun_threads\tload_cpu_affinity\trun_cpu_affinity\tload_java_active_processor_count\trun_java_active_processor_count\tes_java_heap_opts\tload_es_processors\trun_es_processors\tes_http_enabled\tes_index_key\tes_number_of_shards\tes_number_of_replicas\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tstall_cycles\tcycles\tinstructions\ttask_clock_ms\tthroughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\tpath_home\n' > "${summary_file}"
}

append_orientdb_summary_header() {
  local summary_file="$1"

  if [[ -f "${summary_file}" ]]; then
    return
  fi

  printf 'timestamp_utc\thost\tarch\tbackend\tmode\tevent_name\tperf_scope\tsize_mb\trecordcount\toperationcount\tthreads\tcpu_affinity\tjava_active_processor_count\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tstall_cycles\tcycles\tinstructions\ttask_clock_ms\tthroughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\torientdb_url\n' > "${summary_file}"
}

append_summary_row() {
  local summary_file="$1"
  shift
  local first=1
  local field

  {
    for field in "$@"; do
      if (( first )); then
        printf '%s' "${field}"
        first=0
      else
        printf '\t%s' "${field}"
      fi
    done
    printf '\n'
  } >> "${summary_file}"
}

run_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_OPERATION_COUNT}}"
  local threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local cpu_affinity="${CPU_AFFINITY-${DEFAULT_CPU_AFFINITY}}"
  local java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local rocksdb_parallelism="${ROCKSDB_PARALLELISM:-${DEFAULT_ROCKSDB_PARALLELISM}}"
  local rocksdb_max_background_jobs="${ROCKSDB_MAX_BACKGROUND_JOBS:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS}}"
  local rocksdb_max_background_compactions="${ROCKSDB_MAX_BACKGROUND_COMPACTIONS:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_COMPACTIONS}}"
  local rocksdb_max_background_flushes="${ROCKSDB_MAX_BACKGROUND_FLUSHES:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_FLUSHES}}"
  local host
  local now
  local scope
  local run_dir
  local summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_rocksdb
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"

  log "CPU affinity=${cpu_affinity} java_active_processor_count=${java_active_processor_count} rocksdb_parallelism=${rocksdb_parallelism} rocksdb_max_background_jobs=${rocksdb_max_background_jobs} rocksdb_max_background_compactions=${rocksdb_max_background_compactions} rocksdb_max_background_flushes=${rocksdb_max_background_flushes}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_summary_header "${summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local db_dir
    local load_log
    local run_log
    local perf_file
    local time_file
    local stall_cycles
    local cycles
    local instructions
    local task_clock_ms
    local throughput
    local server_perf_user_time_ms
    local server_perf_kernel_time_ms
    local server_perf_cache_refs
    local server_perf_cache_misses
    local server_perf_llc_loads
    local server_perf_llc_load_misses
    local server_perf_llc_hit_ratio
    local status
    local timestamp

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"
    db_dir="${run_dir}/db/${size_mb}mb"
    load_log="${run_dir}/${size_mb}mb.load.log"
    run_log="${run_dir}/${size_mb}mb.run.log"
    perf_file="${run_dir}/${size_mb}mb.perf.stat"
    time_file="${run_dir}/${size_mb}mb.time"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    rm -rf "${db_dir}"

    log "[${arch}] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} threads=${threads}"

    if ! run_bound_command "${YCSB_HOME}/bin/ycsb.sh" load rocksdb -s \
      -P "${workload_file}" \
      -p rocksdb.dir="${db_dir}" \
      -p rocksdb.parallelism="${rocksdb_parallelism}" \
      -p rocksdb.maxbackgroundjobs="${rocksdb_max_background_jobs}" \
      -p rocksdb.maxbackgroundcompactions="${rocksdb_max_background_compactions}" \
      -p rocksdb.maxbackgroundflushes="${rocksdb_max_background_flushes}" \
      -p recordcount="${recordcount}" \
      -p operationcount="${operationcount}" \
      -p fieldcount="${fieldcount}" \
      -p fieldlength="${fieldlength}" \
      -threads "${threads}" > "${load_log}" 2>&1; then
      status="load_failed"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
        "${cpu_affinity}" "${java_active_processor_count}" "${rocksdb_parallelism}" \
        "${rocksdb_max_background_jobs}" "${rocksdb_max_background_compactions}" \
        "${rocksdb_max_background_flushes}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "NA" "NA" "NA" "NA" "NA" \
        "NA" "NA" "NA" "NA" "NA" \
        "${status}" "${perf_event}" \
        "${workload_file}" "${run_dir}"
      cleanup_result_db_path "${db_dir}"
      [[ "${fail_fast}" == "1" ]] && die "load failed for ${size_mb}MB; see ${load_log}"
      continue
    fi

    if ! run_bound_command /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
      perf stat -x ';' -o "${perf_file}" \
      -e "${perf_event}" \
      -e cycles \
      -e instructions \
      -e task-clock \
      -e cache-references \
      -e cache-misses \
      -e LLC-loads \
      -e LLC-load-misses \
      -- "${YCSB_HOME}/bin/ycsb.sh" run rocksdb -s \
      -P "${workload_file}" \
      -p rocksdb.dir="${db_dir}" \
      -p rocksdb.parallelism="${rocksdb_parallelism}" \
      -p rocksdb.maxbackgroundjobs="${rocksdb_max_background_jobs}" \
      -p rocksdb.maxbackgroundcompactions="${rocksdb_max_background_compactions}" \
      -p rocksdb.maxbackgroundflushes="${rocksdb_max_background_flushes}" \
      -p recordcount="${recordcount}" \
      -p operationcount="${operationcount}" \
      -p fieldcount="${fieldcount}" \
      -p fieldlength="${fieldlength}" \
      -threads "${threads}" > "${run_log}" 2>&1; then
      status="run_failed"
      stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
      cycles="$(parse_perf_counter "${perf_file}" "cycles")"
      instructions="$(parse_perf_counter "${perf_file}" "instructions")"
      task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
      throughput="$(parse_ycsb_throughput "${run_log}")"
      server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
      server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
      server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
      server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
      server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
      server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
      server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
        "${cpu_affinity}" "${java_active_processor_count}" "${rocksdb_parallelism}" \
        "${rocksdb_max_background_jobs}" "${rocksdb_max_background_compactions}" \
        "${rocksdb_max_background_flushes}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
        "${throughput}" \
        "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
        "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${status}" "${perf_event}" "${workload_file}" \
        "${run_dir}"
      cleanup_result_db_path "${db_dir}"
      [[ "${fail_fast}" == "1" ]] && die "run failed for ${size_mb}MB; see ${run_log}"
      continue
    fi

    stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
    cycles="$(parse_perf_counter "${perf_file}" "cycles")"
    instructions="$(parse_perf_counter "${perf_file}" "instructions")"
    task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
    throughput="$(parse_ycsb_throughput "${run_log}")"
    server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
    server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
    server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
    server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
    server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
    server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
    server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
    status="ok"
    timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    append_summary_row "${summary_file}" \
      "${timestamp}" "${host}" "${arch}" "${event_name}" "${scope}" \
      "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
      "${cpu_affinity}" "${java_active_processor_count}" "${rocksdb_parallelism}" \
      "${rocksdb_max_background_jobs}" "${rocksdb_max_background_compactions}" \
      "${rocksdb_max_background_flushes}" \
      "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
      "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
      "${throughput}" \
      "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
      "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
      "${status}" "${perf_event}" "${workload_file}" \
      "${run_dir}"
    cleanup_result_db_path "${db_dir}"
  done < <(list_sizes)

  LAST_SINGLE_INSTANCE_RUN_DIR="${run_dir}"
  LAST_SINGLE_INSTANCE_SUMMARY_FILE="${summary_file}"
  log "Completed ${arch} sweep. Summary: ${summary_file}"
}

run_elasticsearch_embedded_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_ES_OPERATION_COUNT}}"
  local load_threads="${LOAD_THREADS:-${DEFAULT_ES_LOAD_THREADS}}"
  local run_threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local run_cpu_affinity="${CPU_AFFINITY-${DEFAULT_CPU_AFFINITY}}"
  local run_java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local load_cpu_affinity="${LOAD_CPU_AFFINITY-}"
  local load_java_active_processor_count="${LOAD_JAVA_ACTIVE_PROCESSOR_COUNT-}"
  local es_http_enabled="${ES_HTTP_ENABLED:-${DEFAULT_ES_HTTP_ENABLED}}"
  local load_es_processors="${LOAD_ES_PROCESSORS-}"
  local run_es_processors="${ES_PROCESSORS:-${DEFAULT_ES_PROCESSORS}}"
  local es_java_heap_opts="${ES_JAVA_HEAP_OPTS:-${DEFAULT_ES_JAVA_HEAP_OPTS}}"
  local es_index_key="${ES_INDEX_KEY:-${DEFAULT_ES_INDEX_KEY}}"
  local es_number_of_shards="${ES_NUMBER_OF_SHARDS:-${DEFAULT_ES_NUMBER_OF_SHARDS}}"
  local es_number_of_replicas="${ES_NUMBER_OF_REPLICAS:-${DEFAULT_ES_NUMBER_OF_REPLICAS}}"
  local host
  local now
  local scope
  local run_dir
  local summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_elasticsearch
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  if [[ "${run_threads}" != "1" ]]; then
    die "embedded elasticsearch sweep only supports THREADS=1 for the measured run phase"
  fi

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_elasticsearch_embedded_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"

  log "load_threads=${load_threads} load_cpu_affinity=${load_cpu_affinity:-none} load_java_active_processor_count=${load_java_active_processor_count:-system-default} run_cpu_affinity=${run_cpu_affinity:-none} run_java_active_processor_count=${run_java_active_processor_count:-system-default} es_java_heap_opts=${es_java_heap_opts} load_es_processors=${load_es_processors:-system-default} run_es_processors=${run_es_processors} es_http_enabled=${es_http_enabled}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_elasticsearch_summary_header "${summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local path_home
    local load_log
    local run_log
    local perf_file
    local time_file
    local stall_cycles
    local cycles
    local instructions
    local task_clock_ms
    local throughput
    local server_perf_user_time_ms
    local server_perf_kernel_time_ms
    local server_perf_cache_refs
    local server_perf_cache_misses
    local server_perf_llc_loads
    local server_perf_llc_load_misses
    local server_perf_llc_hit_ratio
    local status
    local timestamp

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"
    path_home="${run_dir}/db/${size_mb}mb"
    load_log="${run_dir}/${size_mb}mb.load.log"
    run_log="${run_dir}/${size_mb}mb.run.log"
    perf_file="${run_dir}/${size_mb}mb.perf.stat"
    time_file="${run_dir}/${size_mb}mb.time"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    rm -rf "${path_home}"

    log "[${arch}/elasticsearch] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} load_threads=${load_threads} run_threads=${run_threads}"

    local load_args=(
      -P "${workload_file}"
      -p path.home="${path_home}"
      -p es.newdb=true
      -p http.enabled="${es_http_enabled}"
      -p es.index.key="${es_index_key}"
      -p es.number_of_shards="${es_number_of_shards}"
      -p es.number_of_replicas="${es_number_of_replicas}"
      -p recordcount="${recordcount}"
      -p operationcount="${operationcount}"
      -p fieldcount="${fieldcount}"
      -p fieldlength="${fieldlength}"
    )
    local run_args=(
      -P "${workload_file}"
      -p path.home="${path_home}"
      -p http.enabled="${es_http_enabled}"
      -p processors="${run_es_processors}"
      -p es.index.key="${es_index_key}"
      -p es.number_of_shards="${es_number_of_shards}"
      -p es.number_of_replicas="${es_number_of_replicas}"
      -p recordcount="${recordcount}"
      -p operationcount="${operationcount}"
      -p fieldcount="${fieldcount}"
      -p fieldlength="${fieldlength}"
    )

    if [[ -n "${load_es_processors}" ]]; then
      load_args+=( -p "processors=${load_es_processors}" )
    fi

    if ! LOAD_CPU_AFFINITY="${load_cpu_affinity}" \
         CPU_AFFINITY="${load_cpu_affinity}" \
         LOAD_JAVA_ACTIVE_PROCESSOR_COUNT="${load_java_active_processor_count}" \
         JAVA_ACTIVE_PROCESSOR_COUNT="${load_java_active_processor_count}" \
         run_bound_command_with_extra_java_opts "${es_java_heap_opts}" "${YCSB_HOME}/bin/ycsb.sh" load elasticsearch -s \
           "${load_args[@]}" \
           -threads "${load_threads}" > "${load_log}" 2>&1; then
      status="load_failed"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${load_threads}" "${run_threads}" \
        "${load_cpu_affinity}" "${run_cpu_affinity}" \
        "${load_java_active_processor_count}" "${run_java_active_processor_count}" \
        "${es_java_heap_opts}" "${load_es_processors}" "${run_es_processors}" "${es_http_enabled}" "${es_index_key}" \
        "${es_number_of_shards}" "${es_number_of_replicas}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "NA" "NA" "NA" "NA" "NA" \
        "NA" "NA" "NA" "NA" "NA" \
        "${status}" "${perf_event}" \
        "${workload_file}" "${run_dir}" "${path_home}"
      cleanup_result_db_path "${path_home}"
      [[ "${fail_fast}" == "1" ]] && die "elasticsearch load failed for ${size_mb}MB; see ${load_log}"
      continue
    fi

    if ! CPU_AFFINITY="${run_cpu_affinity}" \
         JAVA_ACTIVE_PROCESSOR_COUNT="${run_java_active_processor_count}" \
         run_bound_command_with_extra_java_opts "${es_java_heap_opts}" /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
           perf stat -x ';' -o "${perf_file}" \
           -e "${perf_event}" \
           -e cycles \
           -e instructions \
           -e task-clock \
           -e cache-references \
           -e cache-misses \
           -e LLC-loads \
           -e LLC-load-misses \
           -- "${YCSB_HOME}/bin/ycsb.sh" run elasticsearch -s \
             "${run_args[@]}" \
             -threads "${run_threads}" > "${run_log}" 2>&1; then
      status="run_failed"
      stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
      cycles="$(parse_perf_counter "${perf_file}" "cycles")"
      instructions="$(parse_perf_counter "${perf_file}" "instructions")"
      task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
      throughput="$(parse_ycsb_throughput "${run_log}")"
      server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
      server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
      server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
      server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
      server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
      server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
      server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${load_threads}" "${run_threads}" \
        "${load_cpu_affinity}" "${run_cpu_affinity}" \
        "${load_java_active_processor_count}" "${run_java_active_processor_count}" \
        "${es_java_heap_opts}" "${load_es_processors}" "${run_es_processors}" "${es_http_enabled}" "${es_index_key}" \
        "${es_number_of_shards}" "${es_number_of_replicas}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
        "${throughput}" \
        "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
        "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${status}" "${perf_event}" "${workload_file}" \
        "${run_dir}" "${path_home}"
      cleanup_result_db_path "${path_home}"
      [[ "${fail_fast}" == "1" ]] && die "elasticsearch run failed for ${size_mb}MB; see ${run_log}"
      continue
    fi

    stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
    cycles="$(parse_perf_counter "${perf_file}" "cycles")"
    instructions="$(parse_perf_counter "${perf_file}" "instructions")"
    task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
    throughput="$(parse_ycsb_throughput "${run_log}")"
    server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
    server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
    server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
    server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
    server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
    server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
    server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
    status="ok"
    timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    append_summary_row "${summary_file}" \
      "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
      "${size_mb}" "${recordcount}" "${operationcount}" "${load_threads}" "${run_threads}" \
      "${load_cpu_affinity}" "${run_cpu_affinity}" \
      "${load_java_active_processor_count}" "${run_java_active_processor_count}" \
      "${es_java_heap_opts}" "${load_es_processors}" "${run_es_processors}" "${es_http_enabled}" "${es_index_key}" \
      "${es_number_of_shards}" "${es_number_of_replicas}" \
      "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
      "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
      "${throughput}" \
      "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
      "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
      "${status}" "${perf_event}" "${workload_file}" \
      "${run_dir}" "${path_home}"
    cleanup_result_db_path "${path_home}"
  done < <(list_sizes)

  LAST_SINGLE_INSTANCE_RUN_DIR="${run_dir}"
  LAST_SINGLE_INSTANCE_SUMMARY_FILE="${summary_file}"
  log "Completed ${arch} elasticsearch embedded sweep. Summary: ${summary_file}"
}

run_orientdb_embedded_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_ORIENTDB_OPERATION_COUNT}}"
  local threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local cpu_affinity="${CPU_AFFINITY-${DEFAULT_CPU_AFFINITY}}"
  local java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local host
  local now
  local scope
  local run_dir
  local summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_orientdb
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_orientdb_embedded_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"

  log "CPU affinity=${cpu_affinity} java_active_processor_count=${java_active_processor_count}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_orientdb_summary_header "${summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local db_path
    local orientdb_url
    local load_log
    local run_log
    local perf_file
    local time_file
    local stall_cycles
    local cycles
    local instructions
    local task_clock_ms
    local throughput
    local server_perf_user_time_ms
    local server_perf_kernel_time_ms
    local server_perf_cache_refs
    local server_perf_cache_misses
    local server_perf_llc_loads
    local server_perf_llc_load_misses
    local server_perf_llc_hit_ratio
    local status
    local timestamp

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"
    db_path="${run_dir}/db/${size_mb}mb"
    orientdb_url="plocal:${db_path}"
    load_log="${run_dir}/${size_mb}mb.load.log"
    run_log="${run_dir}/${size_mb}mb.run.log"
    perf_file="${run_dir}/${size_mb}mb.perf.stat"
    time_file="${run_dir}/${size_mb}mb.time"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    rm -rf "${db_path}"

    log "[${arch}/orientdb] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} threads=${threads}"

    if ! run_bound_command "${YCSB_HOME}/bin/ycsb.sh" load orientdb -s \
      -P "${workload_file}" \
      -p orientdb.url="${orientdb_url}" \
      -p orientdb.newdb=true \
      -p recordcount="${recordcount}" \
      -p operationcount="${operationcount}" \
      -p fieldcount="${fieldcount}" \
      -p fieldlength="${fieldlength}" \
      -threads "${threads}" > "${load_log}" 2>&1; then
      status="load_failed"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
        "${cpu_affinity}" "${java_active_processor_count}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "NA" "NA" "NA" "NA" "NA" \
        "NA" "NA" "NA" "NA" "NA" \
        "${status}" "${perf_event}" \
        "${workload_file}" "${run_dir}" "${orientdb_url}"
      cleanup_result_db_path "${db_path}"
      [[ "${fail_fast}" == "1" ]] && die "orientdb load failed for ${size_mb}MB; see ${load_log}"
      continue
    fi

    if ! run_bound_command /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
      perf stat -x ';' -o "${perf_file}" \
      -e "${perf_event}" \
      -e cycles \
      -e instructions \
      -e task-clock \
      -e cache-references \
      -e cache-misses \
      -e LLC-loads \
      -e LLC-load-misses \
      -- "${YCSB_HOME}/bin/ycsb.sh" run orientdb -s \
      -P "${workload_file}" \
      -p orientdb.url="${orientdb_url}" \
      -p orientdb.newdb=false \
      -p recordcount="${recordcount}" \
      -p operationcount="${operationcount}" \
      -p fieldcount="${fieldcount}" \
      -p fieldlength="${fieldlength}" \
      -threads "${threads}" > "${run_log}" 2>&1; then
      status="run_failed"
      stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
      cycles="$(parse_perf_counter "${perf_file}" "cycles")"
      instructions="$(parse_perf_counter "${perf_file}" "instructions")"
      task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
      throughput="$(parse_ycsb_throughput "${run_log}")"
      server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
      server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
      server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
      server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
      server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
      server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
      server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
        "${cpu_affinity}" "${java_active_processor_count}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
        "${throughput}" \
        "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
        "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${status}" "${perf_event}" "${workload_file}" \
        "${run_dir}" "${orientdb_url}"
      cleanup_result_db_path "${db_path}"
      [[ "${fail_fast}" == "1" ]] && die "orientdb run failed for ${size_mb}MB; see ${run_log}"
      continue
    fi

    stall_cycles="$(parse_perf_counter "${perf_file}" "${perf_event}")"
    cycles="$(parse_perf_counter "${perf_file}" "cycles")"
    instructions="$(parse_perf_counter "${perf_file}" "instructions")"
    task_clock_ms="$(parse_perf_counter "${perf_file}" "task-clock")"
    throughput="$(parse_ycsb_throughput "${run_log}")"
    server_perf_user_time_ms="$(parse_time_metric_ms "${time_file}" "user")"
    server_perf_kernel_time_ms="$(parse_time_metric_ms "${time_file}" "sys")"
    server_perf_cache_refs="$(parse_perf_counter "${perf_file}" "cache-references")"
    server_perf_cache_misses="$(parse_perf_counter "${perf_file}" "cache-misses")"
    server_perf_llc_loads="$(parse_perf_counter "${perf_file}" "LLC-loads")"
    server_perf_llc_load_misses="$(parse_perf_counter "${perf_file}" "LLC-load-misses")"
    server_perf_llc_hit_ratio="$(llc_hit_ratio "${server_perf_llc_loads}" "${server_perf_llc_load_misses}")"
    status="ok"
    timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    append_summary_row "${summary_file}" \
      "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
      "${size_mb}" "${recordcount}" "${operationcount}" "${threads}" \
      "${cpu_affinity}" "${java_active_processor_count}" \
      "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
      "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" \
      "${throughput}" \
      "${server_perf_user_time_ms}" "${server_perf_kernel_time_ms}" \
      "${server_perf_cache_refs}" "${server_perf_cache_misses}" "${server_perf_llc_hit_ratio}" \
      "${status}" "${perf_event}" "${workload_file}" \
      "${run_dir}" "${orientdb_url}"
    cleanup_result_db_path "${db_path}"
  done < <(list_sizes)

  LAST_SINGLE_INSTANCE_RUN_DIR="${run_dir}"
  LAST_SINGLE_INSTANCE_SUMMARY_FILE="${summary_file}"
  log "Completed ${arch} orientdb embedded sweep. Summary: ${summary_file}"
}

list_instance_counts() {
  if [[ -n "${INSTANCE_COUNTS:-}" ]]; then
    printf '%s\n' ${INSTANCE_COUNTS}
  else
    printf '%s\n' "${DEFAULT_INSTANCE_COUNTS[@]}"
  fi | awk 'NF { print $1 }' | sort -n -u
}

max_instance_count_from_list() {
  local max=0
  local value

  for value in "$@"; do
    if (( value > max )); then
      max="${value}"
    fi
  done

  printf '%s\n' "${max}"
}

cpu_for_instance_id() {
  local instance_id="$1"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"

  printf '%s\n' "$((cpu_start + instance_id))"
}

cpu_span_for_instance_count() {
  local instance_count="$1"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"

  printf '%s-%s\n' "${cpu_start}" "$((cpu_start + instance_count - 1))"
}

validate_multi_instance_cpu_plan() {
  local max_instances="$1"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"
  local available_cpus
  local max_cpu

  (( max_instances >= 1 )) || die "multi-instance sweep requires at least one instance count"
  (( cpu_start >= 0 )) || die "CPU_START must be >= 0"

  available_cpus="$(getconf _NPROCESSORS_ONLN)"
  [[ -n "${available_cpus}" ]] || die "could not determine online CPU count"

  max_cpu="$((cpu_start + max_instances - 1))"
  if (( max_cpu >= available_cpus )); then
    die "requested CPUs ${cpu_start}-${max_cpu}, but only ${available_cpus} CPUs are online"
  fi
}

join_by() {
  local delimiter="$1"
  shift
  local first=1
  local value

  for value in "$@"; do
    if (( first )); then
      printf '%s' "${value}"
      first=0
    else
      printf '%s%s' "${delimiter}" "${value}"
    fi
  done
}

sum_integer_values() {
  awk '
    BEGIN {
      sum = 0
      for (i = 1; i < ARGC; i++) {
        if (ARGV[i] == "" || ARGV[i] == "NA") {
          print "NA"
          exit
        }
        sum += ARGV[i]
      }
      printf "%.0f\n", sum
    }
  ' "$@"
}

sum_float_values() {
  awk '
    BEGIN {
      sum = 0
      for (i = 1; i < ARGC; i++) {
        if (ARGV[i] == "" || ARGV[i] == "NA") {
          print "NA"
          exit
        }
        sum += ARGV[i]
      }
      printf "%.6f\n", sum
    }
  ' "$@"
}

elapsed_ms_from_ns() {
  local start_ns="$1"
  local end_ns="$2"

  awk -v start="${start_ns}" -v end="${end_ns}" '
    BEGIN {
      if (start == "" || end == "" || end < start) {
        print "NA"
        exit
      }
      printf "%.3f\n", (end - start) / 1000000.0
    }
  '
}

throughput_from_wall_ms() {
  local total_ops="$1"
  local wall_ms="$2"

  awk -v ops="${total_ops}" -v ms="${wall_ms}" '
    BEGIN {
      if (ops == "" || ms == "" || ms == "NA" || ms <= 0) {
        print "NA"
        exit
      }
      printf "%.6f\n", ops / (ms / 1000.0)
    }
  '
}

wait_for_background_pids() {
  local -n pids_ref="$1"
  local -n exit_codes_ref="$2"
  local failed=0
  local idx
  local exit_code

  exit_codes_ref=()
  for idx in "${!pids_ref[@]}"; do
    if wait "${pids_ref[$idx]}"; then
      exit_code=0
    else
      exit_code="$?"
      failed=1
    fi
    exit_codes_ref+=("${exit_code}")
  done

  return "${failed}"
}

append_multi_instance_summary_header() {
  local summary_file="$1"

  if [[ -f "${summary_file}" ]]; then
    return
  fi

  printf 'timestamp_utc\thost\tarch\tbackend\tmode\tevent_name\tperf_scope\tsize_mb\tworking_set_mb_per_instance\ttotal_working_set_mb\tinstance_count\tcpu_span\trecordcount_per_instance\ttotal_recordcount\toperationcount_per_instance\ttotal_operationcount\tthreads_per_instance\tjava_active_processor_count_per_instance\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tsummed_stall_cycles\tsummed_cycles\tsummed_instructions\tsummed_task_clock_ms\tsummed_instance_throughput_ops_per_sec\tgroup_wall_clock_ms\taggregate_wall_throughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\n' > "${summary_file}"
}

append_multi_instance_instance_summary_header() {
  local summary_file="$1"

  if [[ -f "${summary_file}" ]]; then
    return
  fi

  printf 'timestamp_utc\thost\tarch\tbackend\tmode\tevent_name\tperf_scope\tsize_mb\tworking_set_mb_per_instance\ttotal_working_set_mb\tinstance_count\tinstance_id\tcpu_affinity\tdb_target\trecordcount\toperationcount\tthreads_per_instance\tjava_active_processor_count_per_instance\tfieldcount\tfieldlength\tapprox_value_bytes_per_record\tstall_cycles\tcycles\tinstructions\ttask_clock_ms\tthroughput_ops_per_sec\tServerPerfUserTimeMs\tServerPerfKernelTimeMs\tServerPerfCacheRefs\tServerPerfCacheMisses\tServerPerfLLCHitRatio\tstatus\tperf_event\tworkload_file\tresult_dir\n' > "${summary_file}"
}

run_rocksdb_multi_instance_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_OPERATION_COUNT}}"
  local threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local rocksdb_parallelism="${ROCKSDB_PARALLELISM:-${DEFAULT_ROCKSDB_PARALLELISM}}"
  local rocksdb_max_background_jobs="${ROCKSDB_MAX_BACKGROUND_JOBS:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS}}"
  local rocksdb_max_background_compactions="${ROCKSDB_MAX_BACKGROUND_COMPACTIONS:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_COMPACTIONS}}"
  local rocksdb_max_background_flushes="${ROCKSDB_MAX_BACKGROUND_FLUSHES:-${DEFAULT_ROCKSDB_MAX_BACKGROUND_FLUSHES}}"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"
  local -a instance_counts
  local max_instances
  local host
  local now
  local scope
  local run_dir
  local summary_file
  local instance_summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_rocksdb
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  if [[ "${threads}" != "1" ]]; then
    die "rocksdb multi-instance sweep only supports THREADS=1 per instance"
  fi

  mapfile -t instance_counts < <(list_instance_counts)
  [[ "${#instance_counts[@]}" -gt 0 ]] || die "INSTANCE_COUNTS resolved to an empty set"
  max_instances="$(max_instance_count_from_list "${instance_counts[@]}")"
  validate_multi_instance_cpu_plan "${max_instances}"

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_rocksdb_multi_instance_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"
  instance_summary_file="${run_dir}/instance_summary.tsv"

  log "backend=rocksdb instance_counts=$(join_by ',' "${instance_counts[@]}") cpu_start=${cpu_start} java_active_processor_count=${java_active_processor_count} rocksdb_parallelism=${rocksdb_parallelism} rocksdb_max_background_jobs=${rocksdb_max_background_jobs} rocksdb_max_background_compactions=${rocksdb_max_background_compactions} rocksdb_max_background_flushes=${rocksdb_max_background_flushes}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_multi_instance_summary_header "${summary_file}"
  append_multi_instance_instance_summary_header "${instance_summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local total_recordcount_max
    local load_failed=0
    local -a load_pids=()
    local -a load_exit_codes=()
    local instance_id

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"
    total_recordcount_max="$((recordcount * max_instances))"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    log "[${arch}/rocksdb-multi] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} max_instances=${max_instances}"

    for ((instance_id = 0; instance_id < max_instances; instance_id++)); do
      local cpu
      local db_dir
      local load_log

      cpu="$(cpu_for_instance_id "${instance_id}")"
      db_dir="${run_dir}/db/${size_mb}mb/inst${instance_id}"
      load_log="${run_dir}/${size_mb}mb.inst${instance_id}.load.log"

      rm -rf "${db_dir}"

      (
        CPU_AFFINITY="${cpu}" \
        JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
        run_bound_command "${YCSB_HOME}/bin/ycsb.sh" load rocksdb -s \
          -P "${workload_file}" \
          -p rocksdb.dir="${db_dir}" \
          -p rocksdb.parallelism="${rocksdb_parallelism}" \
          -p rocksdb.maxbackgroundjobs="${rocksdb_max_background_jobs}" \
          -p rocksdb.maxbackgroundcompactions="${rocksdb_max_background_compactions}" \
          -p rocksdb.maxbackgroundflushes="${rocksdb_max_background_flushes}" \
          -p recordcount="${recordcount}" \
          -p operationcount="${operationcount}" \
          -p fieldcount="${fieldcount}" \
          -p fieldlength="${fieldlength}" \
          -threads "${threads}" > "${load_log}" 2>&1
      ) &
      load_pids+=("$!")
    done

    if ! wait_for_background_pids load_pids load_exit_codes; then
      load_failed=1
    fi

    if (( load_failed )); then
      local timestamp
      local instance_count

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      for instance_count in "${instance_counts[@]}"; do
        append_summary_row "${summary_file}" \
          "${timestamp}" "${host}" "${arch}" "rocksdb" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "$((size_mb * instance_count))" "${instance_count}" \
          "$(cpu_span_for_instance_count "${instance_count}")" \
          "${recordcount}" "$((recordcount * instance_count))" \
          "${operationcount}" "$((operationcount * instance_count))" \
          "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "NA" "NA" "NA" "NA" "NA" "NA" "NA" \
          "NA" "NA" "NA" "NA" "NA" \
          "load_failed" "${perf_event}" "${workload_file}" "${run_dir}"
      done
      cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
      [[ "${fail_fast}" == "1" ]] && die "rocksdb multi-instance load failed for ${size_mb}MB; see ${run_dir}"
      continue
    fi

    local instance_count
    for instance_count in "${instance_counts[@]}"; do
      local group_start_ns
      local group_end_ns
      local group_wall_ms
      local group_status="ok"
      local total_working_set_mb
      local total_recordcount
      local total_operationcount
      local -a pids=()
      local -a exit_codes=()
      local -a run_logs=()
      local -a perf_files=()
      local -a time_files=()
      local -a db_targets=()
      local -a cpus=()
      local -a stall_values=()
      local -a cycle_values=()
      local -a instruction_values=()
      local -a task_values=()
      local -a throughput_values=()
      local -a user_time_values=()
      local -a kernel_time_values=()
      local -a cache_ref_values=()
      local -a cache_miss_values=()
      local -a llc_load_values=()
      local -a llc_miss_values=()
      local idx

      total_working_set_mb="$((size_mb * instance_count))"
      total_recordcount="$((recordcount * instance_count))"
      total_operationcount="$((operationcount * instance_count))"
      group_start_ns="$(date +%s%N)"

      for ((instance_id = 0; instance_id < instance_count; instance_id++)); do
        local cpu
        local db_dir
        local run_log
        local perf_file
        local time_file

        cpu="$(cpu_for_instance_id "${instance_id}")"
        db_dir="${run_dir}/db/${size_mb}mb/inst${instance_id}"
        run_log="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.run.log"
        perf_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.perf.stat"
        time_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.time"

        (
          CPU_AFFINITY="${cpu}" \
          JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
          run_bound_command /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
            perf stat -x ';' -o "${perf_file}" \
            -e "${perf_event}" \
            -e cycles \
            -e instructions \
            -e task-clock \
            -e cache-references \
            -e cache-misses \
            -e LLC-loads \
            -e LLC-load-misses \
            -- "${YCSB_HOME}/bin/ycsb.sh" run rocksdb -s \
              -P "${workload_file}" \
              -p rocksdb.dir="${db_dir}" \
              -p rocksdb.parallelism="${rocksdb_parallelism}" \
              -p rocksdb.maxbackgroundjobs="${rocksdb_max_background_jobs}" \
              -p rocksdb.maxbackgroundcompactions="${rocksdb_max_background_compactions}" \
              -p rocksdb.maxbackgroundflushes="${rocksdb_max_background_flushes}" \
              -p recordcount="${recordcount}" \
              -p operationcount="${operationcount}" \
              -p fieldcount="${fieldcount}" \
              -p fieldlength="${fieldlength}" \
              -threads "${threads}" > "${run_log}" 2>&1
        ) &
        pids+=("$!")
        run_logs+=("${run_log}")
        perf_files+=("${perf_file}")
        time_files+=("${time_file}")
        db_targets+=("${db_dir}")
        cpus+=("${cpu}")
      done

      if ! wait_for_background_pids pids exit_codes; then
        group_status="run_failed"
      fi
      group_end_ns="$(date +%s%N)"
      group_wall_ms="$(elapsed_ms_from_ns "${group_start_ns}" "${group_end_ns}")"

      for idx in "${!run_logs[@]}"; do
        local stall_cycles
        local cycles
        local instructions
        local task_clock_ms
        local throughput
        local user_time_ms
        local kernel_time_ms
        local cache_refs
        local cache_misses
        local llc_loads
        local llc_load_misses
        local llc_hit_ratio_value
        local instance_status="ok"
        local timestamp

        if [[ "${exit_codes[$idx]}" != "0" ]]; then
          instance_status="run_failed"
          stall_cycles="NA"
          cycles="NA"
          instructions="NA"
          task_clock_ms="NA"
          throughput="NA"
          user_time_ms="NA"
          kernel_time_ms="NA"
          cache_refs="NA"
          cache_misses="NA"
          llc_loads="NA"
          llc_load_misses="NA"
          llc_hit_ratio_value="NA"
          group_status="run_failed"
        else
          stall_cycles="$(parse_perf_counter "${perf_files[$idx]}" "${perf_event}")"
          cycles="$(parse_perf_counter "${perf_files[$idx]}" "cycles")"
          instructions="$(parse_perf_counter "${perf_files[$idx]}" "instructions")"
          task_clock_ms="$(parse_perf_counter "${perf_files[$idx]}" "task-clock")"
          throughput="$(parse_ycsb_throughput "${run_logs[$idx]}")"
          user_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "user")"
          kernel_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "sys")"
          cache_refs="$(parse_perf_counter "${perf_files[$idx]}" "cache-references")"
          cache_misses="$(parse_perf_counter "${perf_files[$idx]}" "cache-misses")"
          llc_loads="$(parse_perf_counter "${perf_files[$idx]}" "LLC-loads")"
          llc_load_misses="$(parse_perf_counter "${perf_files[$idx]}" "LLC-load-misses")"
          llc_hit_ratio_value="$(llc_hit_ratio "${llc_loads}" "${llc_load_misses}")"

          if [[ "${stall_cycles}" == "NA" || "${cycles}" == "NA" || "${instructions}" == "NA" || "${task_clock_ms}" == "NA" || "${throughput}" == "NA" || "${user_time_ms}" == "NA" || "${kernel_time_ms}" == "NA" || "${cache_refs}" == "NA" || "${cache_misses}" == "NA" ]]; then
            instance_status="parse_failed"
            if [[ "${group_status}" == "ok" ]]; then
              group_status="parse_failed"
            fi
          fi
        fi

        stall_values+=("${stall_cycles}")
        cycle_values+=("${cycles}")
        instruction_values+=("${instructions}")
        task_values+=("${task_clock_ms}")
        throughput_values+=("${throughput}")
        user_time_values+=("${user_time_ms}")
        kernel_time_values+=("${kernel_time_ms}")
        cache_ref_values+=("${cache_refs}")
        cache_miss_values+=("${cache_misses}")
        llc_load_values+=("${llc_loads}")
        llc_miss_values+=("${llc_load_misses}")

        timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
        append_summary_row "${instance_summary_file}" \
          "${timestamp}" "${host}" "${arch}" "rocksdb" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" "${idx}" \
          "${cpus[$idx]}" "${db_targets[$idx]}" \
          "${recordcount}" "${operationcount}" "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" "${throughput}" \
          "${user_time_ms}" "${kernel_time_ms}" "${cache_refs}" "${cache_misses}" "${llc_hit_ratio_value}" \
          "${instance_status}" "${perf_event}" "${workload_file}" "${run_dir}"
      done

      local summed_stall_cycles
      local summed_cycles
      local summed_instructions
      local summed_task_clock_ms
      local summed_instance_throughput
      local aggregate_wall_throughput
      local summed_user_time_ms
      local summed_kernel_time_ms
      local summed_cache_refs
      local summed_cache_misses
      local summed_llc_loads
      local summed_llc_load_misses
      local server_perf_llc_hit_ratio
      local timestamp

      if [[ "${group_status}" == "ok" ]]; then
        summed_stall_cycles="$(sum_integer_values "${stall_values[@]}")"
        summed_cycles="$(sum_integer_values "${cycle_values[@]}")"
        summed_instructions="$(sum_integer_values "${instruction_values[@]}")"
        summed_task_clock_ms="$(sum_float_values "${task_values[@]}")"
        summed_instance_throughput="$(sum_float_values "${throughput_values[@]}")"
        aggregate_wall_throughput="$(throughput_from_wall_ms "${total_operationcount}" "${group_wall_ms}")"
        summed_user_time_ms="$(sum_float_values "${user_time_values[@]}")"
        summed_kernel_time_ms="$(sum_float_values "${kernel_time_values[@]}")"
        summed_cache_refs="$(sum_integer_values "${cache_ref_values[@]}")"
        summed_cache_misses="$(sum_integer_values "${cache_miss_values[@]}")"
        summed_llc_loads="$(sum_integer_values "${llc_load_values[@]}")"
        summed_llc_load_misses="$(sum_integer_values "${llc_miss_values[@]}")"
        server_perf_llc_hit_ratio="$(llc_hit_ratio "${summed_llc_loads}" "${summed_llc_load_misses}")"
      else
        summed_stall_cycles="NA"
        summed_cycles="NA"
        summed_instructions="NA"
        summed_task_clock_ms="NA"
        summed_instance_throughput="NA"
        aggregate_wall_throughput="NA"
        summed_user_time_ms="NA"
        summed_kernel_time_ms="NA"
        summed_cache_refs="NA"
        summed_cache_misses="NA"
        summed_llc_loads="NA"
        summed_llc_load_misses="NA"
        server_perf_llc_hit_ratio="NA"
      fi

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "rocksdb" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" \
        "$(cpu_span_for_instance_count "${instance_count}")" \
        "${recordcount}" "${total_recordcount}" \
        "${operationcount}" "${total_operationcount}" \
        "${threads}" "${java_active_processor_count}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${summed_stall_cycles}" "${summed_cycles}" "${summed_instructions}" \
        "${summed_task_clock_ms}" "${summed_instance_throughput}" \
        "${group_wall_ms}" "${aggregate_wall_throughput}" \
        "${summed_user_time_ms}" "${summed_kernel_time_ms}" \
        "${summed_cache_refs}" "${summed_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${group_status}" "${perf_event}" "${workload_file}" "${run_dir}"
    done
    cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
  done < <(list_sizes)

  LAST_MULTI_INSTANCE_RUN_DIR="${run_dir}"
  LAST_MULTI_INSTANCE_SUMMARY_FILE="${summary_file}"
  LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE="${instance_summary_file}"
  log "Completed ${arch} rocksdb multi-instance sweep. Summary: ${summary_file}"
}

run_orientdb_multi_instance_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_ORIENTDB_OPERATION_COUNT}}"
  local threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"
  local -a instance_counts
  local max_instances
  local host
  local now
  local scope
  local run_dir
  local summary_file
  local instance_summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_orientdb
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  if [[ "${threads}" != "1" ]]; then
    die "orientdb multi-instance sweep only supports THREADS=1 per instance"
  fi

  mapfile -t instance_counts < <(list_instance_counts)
  [[ "${#instance_counts[@]}" -gt 0 ]] || die "INSTANCE_COUNTS resolved to an empty set"
  max_instances="$(max_instance_count_from_list "${instance_counts[@]}")"
  validate_multi_instance_cpu_plan "${max_instances}"

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_orientdb_multi_instance_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"
  instance_summary_file="${run_dir}/instance_summary.tsv"

  log "backend=orientdb instance_counts=$(join_by ',' "${instance_counts[@]}") cpu_start=${cpu_start} java_active_processor_count=${java_active_processor_count}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_multi_instance_summary_header "${summary_file}"
  append_multi_instance_instance_summary_header "${instance_summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local load_failed=0
    local -a load_pids=()
    local -a load_exit_codes=()
    local instance_id

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    log "[${arch}/orientdb-multi] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} max_instances=${max_instances}"

    for ((instance_id = 0; instance_id < max_instances; instance_id++)); do
      local cpu
      local db_path
      local orientdb_url
      local load_log

      cpu="$(cpu_for_instance_id "${instance_id}")"
      db_path="${run_dir}/db/${size_mb}mb/inst${instance_id}"
      orientdb_url="plocal:${db_path}"
      load_log="${run_dir}/${size_mb}mb.inst${instance_id}.load.log"

      rm -rf "${db_path}"

      (
        CPU_AFFINITY="${cpu}" \
        JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
        run_bound_command "${YCSB_HOME}/bin/ycsb.sh" load orientdb -s \
          -P "${workload_file}" \
          -p orientdb.url="${orientdb_url}" \
          -p orientdb.newdb=true \
          -p recordcount="${recordcount}" \
          -p operationcount="${operationcount}" \
          -p fieldcount="${fieldcount}" \
          -p fieldlength="${fieldlength}" \
          -threads "${threads}" > "${load_log}" 2>&1
      ) &
      load_pids+=("$!")
    done

    if ! wait_for_background_pids load_pids load_exit_codes; then
      load_failed=1
    fi

    if (( load_failed )); then
      local timestamp
      local instance_count

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      for instance_count in "${instance_counts[@]}"; do
        append_summary_row "${summary_file}" \
          "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "$((size_mb * instance_count))" "${instance_count}" \
          "$(cpu_span_for_instance_count "${instance_count}")" \
          "${recordcount}" "$((recordcount * instance_count))" \
          "${operationcount}" "$((operationcount * instance_count))" \
          "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "NA" "NA" "NA" "NA" "NA" "NA" "NA" \
          "NA" "NA" "NA" "NA" "NA" \
          "load_failed" "${perf_event}" "${workload_file}" "${run_dir}"
      done
      cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
      [[ "${fail_fast}" == "1" ]] && die "orientdb multi-instance load failed for ${size_mb}MB; see ${run_dir}"
      continue
    fi

    local instance_count
    for instance_count in "${instance_counts[@]}"; do
      local group_start_ns
      local group_end_ns
      local group_wall_ms
      local group_status="ok"
      local total_working_set_mb
      local total_recordcount
      local total_operationcount
      local -a pids=()
      local -a exit_codes=()
      local -a run_logs=()
      local -a perf_files=()
      local -a time_files=()
      local -a db_targets=()
      local -a cpus=()
      local -a stall_values=()
      local -a cycle_values=()
      local -a instruction_values=()
      local -a task_values=()
      local -a throughput_values=()
      local -a user_time_values=()
      local -a kernel_time_values=()
      local -a cache_ref_values=()
      local -a cache_miss_values=()
      local -a llc_load_values=()
      local -a llc_miss_values=()
      local idx

      total_working_set_mb="$((size_mb * instance_count))"
      total_recordcount="$((recordcount * instance_count))"
      total_operationcount="$((operationcount * instance_count))"
      group_start_ns="$(date +%s%N)"

      for ((instance_id = 0; instance_id < instance_count; instance_id++)); do
        local cpu
        local db_path
        local orientdb_url
        local run_log
        local perf_file
        local time_file

        cpu="$(cpu_for_instance_id "${instance_id}")"
        db_path="${run_dir}/db/${size_mb}mb/inst${instance_id}"
        orientdb_url="plocal:${db_path}"
        run_log="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.run.log"
        perf_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.perf.stat"
        time_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.time"

        (
          CPU_AFFINITY="${cpu}" \
          JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
          run_bound_command /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
            perf stat -x ';' -o "${perf_file}" \
            -e "${perf_event}" \
            -e cycles \
            -e instructions \
            -e task-clock \
            -e cache-references \
            -e cache-misses \
            -e LLC-loads \
            -e LLC-load-misses \
            -- "${YCSB_HOME}/bin/ycsb.sh" run orientdb -s \
              -P "${workload_file}" \
              -p orientdb.url="${orientdb_url}" \
              -p orientdb.newdb=false \
              -p recordcount="${recordcount}" \
              -p operationcount="${operationcount}" \
              -p fieldcount="${fieldcount}" \
              -p fieldlength="${fieldlength}" \
              -threads "${threads}" > "${run_log}" 2>&1
        ) &
        pids+=("$!")
        run_logs+=("${run_log}")
        perf_files+=("${perf_file}")
        time_files+=("${time_file}")
        db_targets+=("${orientdb_url}")
        cpus+=("${cpu}")
      done

      if ! wait_for_background_pids pids exit_codes; then
        group_status="run_failed"
      fi
      group_end_ns="$(date +%s%N)"
      group_wall_ms="$(elapsed_ms_from_ns "${group_start_ns}" "${group_end_ns}")"

      for idx in "${!run_logs[@]}"; do
        local stall_cycles
        local cycles
        local instructions
        local task_clock_ms
        local throughput
        local user_time_ms
        local kernel_time_ms
        local cache_refs
        local cache_misses
        local llc_loads
        local llc_load_misses
        local llc_hit_ratio_value
        local instance_status="ok"
        local timestamp

        if [[ "${exit_codes[$idx]}" != "0" ]]; then
          instance_status="run_failed"
          stall_cycles="NA"
          cycles="NA"
          instructions="NA"
          task_clock_ms="NA"
          throughput="NA"
          user_time_ms="NA"
          kernel_time_ms="NA"
          cache_refs="NA"
          cache_misses="NA"
          llc_loads="NA"
          llc_load_misses="NA"
          llc_hit_ratio_value="NA"
          group_status="run_failed"
        else
          stall_cycles="$(parse_perf_counter "${perf_files[$idx]}" "${perf_event}")"
          cycles="$(parse_perf_counter "${perf_files[$idx]}" "cycles")"
          instructions="$(parse_perf_counter "${perf_files[$idx]}" "instructions")"
          task_clock_ms="$(parse_perf_counter "${perf_files[$idx]}" "task-clock")"
          throughput="$(parse_ycsb_throughput "${run_logs[$idx]}")"
          user_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "user")"
          kernel_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "sys")"
          cache_refs="$(parse_perf_counter "${perf_files[$idx]}" "cache-references")"
          cache_misses="$(parse_perf_counter "${perf_files[$idx]}" "cache-misses")"
          llc_loads="$(parse_perf_counter "${perf_files[$idx]}" "LLC-loads")"
          llc_load_misses="$(parse_perf_counter "${perf_files[$idx]}" "LLC-load-misses")"
          llc_hit_ratio_value="$(llc_hit_ratio "${llc_loads}" "${llc_load_misses}")"

          if [[ "${stall_cycles}" == "NA" || "${cycles}" == "NA" || "${instructions}" == "NA" || "${task_clock_ms}" == "NA" || "${throughput}" == "NA" || "${user_time_ms}" == "NA" || "${kernel_time_ms}" == "NA" || "${cache_refs}" == "NA" || "${cache_misses}" == "NA" ]]; then
            instance_status="parse_failed"
            if [[ "${group_status}" == "ok" ]]; then
              group_status="parse_failed"
            fi
          fi
        fi

        stall_values+=("${stall_cycles}")
        cycle_values+=("${cycles}")
        instruction_values+=("${instructions}")
        task_values+=("${task_clock_ms}")
        throughput_values+=("${throughput}")
        user_time_values+=("${user_time_ms}")
        kernel_time_values+=("${kernel_time_ms}")
        cache_ref_values+=("${cache_refs}")
        cache_miss_values+=("${cache_misses}")
        llc_load_values+=("${llc_loads}")
        llc_miss_values+=("${llc_load_misses}")

        timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
        append_summary_row "${instance_summary_file}" \
          "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" "${idx}" \
          "${cpus[$idx]}" "${db_targets[$idx]}" \
          "${recordcount}" "${operationcount}" "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" "${throughput}" \
          "${user_time_ms}" "${kernel_time_ms}" "${cache_refs}" "${cache_misses}" "${llc_hit_ratio_value}" \
          "${instance_status}" "${perf_event}" "${workload_file}" "${run_dir}"
      done

      local summed_stall_cycles
      local summed_cycles
      local summed_instructions
      local summed_task_clock_ms
      local summed_instance_throughput
      local aggregate_wall_throughput
      local summed_user_time_ms
      local summed_kernel_time_ms
      local summed_cache_refs
      local summed_cache_misses
      local summed_llc_loads
      local summed_llc_load_misses
      local server_perf_llc_hit_ratio
      local timestamp

      if [[ "${group_status}" == "ok" ]]; then
        summed_stall_cycles="$(sum_integer_values "${stall_values[@]}")"
        summed_cycles="$(sum_integer_values "${cycle_values[@]}")"
        summed_instructions="$(sum_integer_values "${instruction_values[@]}")"
        summed_task_clock_ms="$(sum_float_values "${task_values[@]}")"
        summed_instance_throughput="$(sum_float_values "${throughput_values[@]}")"
        aggregate_wall_throughput="$(throughput_from_wall_ms "${total_operationcount}" "${group_wall_ms}")"
        summed_user_time_ms="$(sum_float_values "${user_time_values[@]}")"
        summed_kernel_time_ms="$(sum_float_values "${kernel_time_values[@]}")"
        summed_cache_refs="$(sum_integer_values "${cache_ref_values[@]}")"
        summed_cache_misses="$(sum_integer_values "${cache_miss_values[@]}")"
        summed_llc_loads="$(sum_integer_values "${llc_load_values[@]}")"
        summed_llc_load_misses="$(sum_integer_values "${llc_miss_values[@]}")"
        server_perf_llc_hit_ratio="$(llc_hit_ratio "${summed_llc_loads}" "${summed_llc_load_misses}")"
      else
        summed_stall_cycles="NA"
        summed_cycles="NA"
        summed_instructions="NA"
        summed_task_clock_ms="NA"
        summed_instance_throughput="NA"
        aggregate_wall_throughput="NA"
        summed_user_time_ms="NA"
        summed_kernel_time_ms="NA"
        summed_cache_refs="NA"
        summed_cache_misses="NA"
        summed_llc_loads="NA"
        summed_llc_load_misses="NA"
        server_perf_llc_hit_ratio="NA"
      fi

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "orientdb" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" \
        "$(cpu_span_for_instance_count "${instance_count}")" \
        "${recordcount}" "${total_recordcount}" \
        "${operationcount}" "${total_operationcount}" \
        "${threads}" "${java_active_processor_count}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${summed_stall_cycles}" "${summed_cycles}" "${summed_instructions}" \
        "${summed_task_clock_ms}" "${summed_instance_throughput}" \
        "${group_wall_ms}" "${aggregate_wall_throughput}" \
        "${summed_user_time_ms}" "${summed_kernel_time_ms}" \
        "${summed_cache_refs}" "${summed_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${group_status}" "${perf_event}" "${workload_file}" "${run_dir}"
    done
    cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
  done < <(list_sizes)

  LAST_MULTI_INSTANCE_RUN_DIR="${run_dir}"
  LAST_MULTI_INSTANCE_SUMMARY_FILE="${summary_file}"
  LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE="${instance_summary_file}"
  log "Completed ${arch} orientdb multi-instance sweep. Summary: ${summary_file}"
}

run_elasticsearch_multi_instance_sweep() {
  local arch="$1"
  local event_name="$2"
  local perf_event="$3"
  local operationcount="${OPERATION_COUNT:-${DEFAULT_ES_OPERATION_COUNT}}"
  local load_threads="${LOAD_THREADS:-${DEFAULT_ES_MULTI_INSTANCE_LOAD_THREADS}}"
  local threads="${THREADS:-${DEFAULT_THREADS}}"
  local fieldcount="${FIELD_COUNT:-${DEFAULT_FIELD_COUNT}}"
  local fieldlength="${FIELD_LENGTH:-${DEFAULT_FIELD_LENGTH}}"
  local record_value_bytes
  local fail_fast="${FAIL_FAST:-1}"
  local java_active_processor_count="${JAVA_ACTIVE_PROCESSOR_COUNT-${DEFAULT_JAVA_ACTIVE_PROCESSOR_COUNT}}"
  local es_http_enabled="${ES_HTTP_ENABLED:-${DEFAULT_ES_HTTP_ENABLED}}"
  local es_processors="${ES_PROCESSORS:-${DEFAULT_ES_PROCESSORS}}"
  local load_es_processors="${LOAD_ES_PROCESSORS:-${es_processors}}"
  local es_java_heap_opts="${ES_JAVA_HEAP_OPTS:-${DEFAULT_ES_JAVA_HEAP_OPTS}}"
  local es_index_key="${ES_INDEX_KEY:-${DEFAULT_ES_INDEX_KEY}}"
  local es_number_of_shards="${ES_NUMBER_OF_SHARDS:-${DEFAULT_ES_NUMBER_OF_SHARDS}}"
  local es_number_of_replicas="${ES_NUMBER_OF_REPLICAS:-${DEFAULT_ES_NUMBER_OF_REPLICAS}}"
  local cpu_start="${CPU_START:-${DEFAULT_CPU_START}}"
  local -a instance_counts
  local max_instances
  local host
  local now
  local scope
  local run_dir
  local summary_file
  local instance_summary_file

  ensure_required_commands
  ensure_ycsb_checkout
  build_ycsb_elasticsearch
  ensure_workloads
  warn_perf_scope
  record_value_bytes="$(record_value_bytes_per_record)"

  if [[ "${threads}" != "1" ]]; then
    die "elasticsearch multi-instance sweep only supports THREADS=1 per instance"
  fi

  mapfile -t instance_counts < <(list_instance_counts)
  [[ "${#instance_counts[@]}" -gt 0 ]] || die "INSTANCE_COUNTS resolved to an empty set"
  max_instances="$(max_instance_count_from_list "${instance_counts[@]}")"
  validate_multi_instance_cpu_plan "${max_instances}"

  host="$(hostname -s)"
  now="$(date -u +'%Y%m%dT%H%M%SZ')"
  scope="$(perf_scope_label)"
  run_dir="${RESULTS_DIR}/${arch}_elasticsearch_multi_instance_${host}_${now}_$$"
  summary_file="${run_dir}/summary.tsv"
  instance_summary_file="${run_dir}/instance_summary.tsv"

  log "backend=elasticsearch instance_counts=$(join_by ',' "${instance_counts[@]}") cpu_start=${cpu_start} java_active_processor_count=${java_active_processor_count} load_threads=${load_threads} es_processors=${es_processors} load_es_processors=${load_es_processors} es_java_heap_opts=${es_java_heap_opts} es_http_enabled=${es_http_enabled}"
  log "keep_result_db=$(keep_result_db)"

  mkdir -p "${run_dir}/db"
  append_multi_instance_summary_header "${summary_file}"
  append_multi_instance_instance_summary_header "${instance_summary_file}"

  while IFS= read -r size_mb; do
    [[ -n "${size_mb}" ]] || continue

    local recordcount
    local workload_file
    local load_failed=0
    local -a load_pids=()
    local -a load_exit_codes=()
    local instance_id

    recordcount="$(recordcount_for_size_mb "${size_mb}")"
    workload_file="$(workload_file_for_size_mb "${size_mb}")"

    [[ -f "${workload_file}" ]] || die "missing workload file: ${workload_file}"

    log "[${arch}/elasticsearch-multi] size=${size_mb}MB recordcount=${recordcount} operationcount=${operationcount} max_instances=${max_instances}"

    for ((instance_id = 0; instance_id < max_instances; instance_id++)); do
      local cpu
      local path_home
      local cluster_name
      local node_name
      local load_log

      cpu="$(cpu_for_instance_id "${instance_id}")"
      path_home="${run_dir}/db/${size_mb}mb/inst${instance_id}"
      cluster_name="es.ycsb.${host}.${now}.${size_mb}mb.inst${instance_id}"
      node_name="es-ycsb-${size_mb}mb-inst${instance_id}"
      load_log="${run_dir}/${size_mb}mb.inst${instance_id}.load.log"

      rm -rf "${path_home}"

      (
        CPU_AFFINITY="${cpu}" \
        JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
        run_bound_command_with_extra_java_opts "${es_java_heap_opts}" "${YCSB_HOME}/bin/ycsb.sh" load elasticsearch -s \
          -P "${workload_file}" \
          -p path.home="${path_home}" \
          -p cluster.name="${cluster_name}" \
          -p node.name="${node_name}" \
          -p es.newdb=true \
          -p http.enabled="${es_http_enabled}" \
          -p processors="${load_es_processors}" \
          -p es.index.key="${es_index_key}" \
          -p es.number_of_shards="${es_number_of_shards}" \
          -p es.number_of_replicas="${es_number_of_replicas}" \
          -p recordcount="${recordcount}" \
          -p operationcount="${operationcount}" \
          -p fieldcount="${fieldcount}" \
          -p fieldlength="${fieldlength}" \
          -threads "${load_threads}" > "${load_log}" 2>&1
      ) &
      load_pids+=("$!")
    done

    if ! wait_for_background_pids load_pids load_exit_codes; then
      load_failed=1
    fi

    if (( load_failed )); then
      local timestamp
      local instance_count

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      for instance_count in "${instance_counts[@]}"; do
        append_summary_row "${summary_file}" \
          "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "$((size_mb * instance_count))" "${instance_count}" \
          "$(cpu_span_for_instance_count "${instance_count}")" \
          "${recordcount}" "$((recordcount * instance_count))" \
          "${operationcount}" "$((operationcount * instance_count))" \
          "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "NA" "NA" "NA" "NA" "NA" "NA" "NA" \
          "NA" "NA" "NA" "NA" "NA" \
          "load_failed" "${perf_event}" "${workload_file}" "${run_dir}"
      done
      cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
      [[ "${fail_fast}" == "1" ]] && die "elasticsearch multi-instance load failed for ${size_mb}MB; see ${run_dir}"
      continue
    fi

    local instance_count
    for instance_count in "${instance_counts[@]}"; do
      local group_start_ns
      local group_end_ns
      local group_wall_ms
      local group_status="ok"
      local total_working_set_mb
      local total_recordcount
      local total_operationcount
      local -a pids=()
      local -a exit_codes=()
      local -a run_logs=()
      local -a perf_files=()
      local -a time_files=()
      local -a db_targets=()
      local -a cpus=()
      local -a stall_values=()
      local -a cycle_values=()
      local -a instruction_values=()
      local -a task_values=()
      local -a throughput_values=()
      local -a user_time_values=()
      local -a kernel_time_values=()
      local -a cache_ref_values=()
      local -a cache_miss_values=()
      local -a llc_load_values=()
      local -a llc_miss_values=()
      local idx

      total_working_set_mb="$((size_mb * instance_count))"
      total_recordcount="$((recordcount * instance_count))"
      total_operationcount="$((operationcount * instance_count))"
      group_start_ns="$(date +%s%N)"

      for ((instance_id = 0; instance_id < instance_count; instance_id++)); do
        local cpu
        local path_home
        local cluster_name
        local node_name
        local run_log
        local perf_file
        local time_file

        cpu="$(cpu_for_instance_id "${instance_id}")"
        path_home="${run_dir}/db/${size_mb}mb/inst${instance_id}"
        cluster_name="es.ycsb.${host}.${now}.${size_mb}mb.inst${instance_id}"
        node_name="es-ycsb-${size_mb}mb-inst${instance_id}"
        run_log="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.run.log"
        perf_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.perf.stat"
        time_file="${run_dir}/${size_mb}mb.c$(count_tag "${instance_count}").inst${instance_id}.time"

        (
          CPU_AFFINITY="${cpu}" \
          JAVA_ACTIVE_PROCESSOR_COUNT="${java_active_processor_count}" \
          run_bound_command_with_extra_java_opts "${es_java_heap_opts}" /usr/bin/time -f 'user=%U\nsys=%S\nelapsed=%e' -o "${time_file}" \
            perf stat -x ';' -o "${perf_file}" \
            -e "${perf_event}" \
            -e cycles \
            -e instructions \
            -e task-clock \
            -e cache-references \
            -e cache-misses \
            -e LLC-loads \
            -e LLC-load-misses \
            -- "${YCSB_HOME}/bin/ycsb.sh" run elasticsearch -s \
              -P "${workload_file}" \
              -p path.home="${path_home}" \
              -p cluster.name="${cluster_name}" \
              -p node.name="${node_name}" \
              -p http.enabled="${es_http_enabled}" \
              -p processors="${es_processors}" \
              -p es.index.key="${es_index_key}" \
              -p es.number_of_shards="${es_number_of_shards}" \
              -p es.number_of_replicas="${es_number_of_replicas}" \
              -p recordcount="${recordcount}" \
              -p operationcount="${operationcount}" \
              -p fieldcount="${fieldcount}" \
              -p fieldlength="${fieldlength}" \
              -threads "${threads}" > "${run_log}" 2>&1
        ) &
        pids+=("$!")
        run_logs+=("${run_log}")
        perf_files+=("${perf_file}")
        time_files+=("${time_file}")
        db_targets+=("${path_home}")
        cpus+=("${cpu}")
      done

      if ! wait_for_background_pids pids exit_codes; then
        group_status="run_failed"
      fi
      group_end_ns="$(date +%s%N)"
      group_wall_ms="$(elapsed_ms_from_ns "${group_start_ns}" "${group_end_ns}")"

      for idx in "${!run_logs[@]}"; do
        local stall_cycles
        local cycles
        local instructions
        local task_clock_ms
        local throughput
        local user_time_ms
        local kernel_time_ms
        local cache_refs
        local cache_misses
        local llc_loads
        local llc_load_misses
        local llc_hit_ratio_value
        local instance_status="ok"
        local timestamp

        if [[ "${exit_codes[$idx]}" != "0" ]]; then
          instance_status="run_failed"
          stall_cycles="NA"
          cycles="NA"
          instructions="NA"
          task_clock_ms="NA"
          throughput="NA"
          user_time_ms="NA"
          kernel_time_ms="NA"
          cache_refs="NA"
          cache_misses="NA"
          llc_loads="NA"
          llc_load_misses="NA"
          llc_hit_ratio_value="NA"
          group_status="run_failed"
        else
          stall_cycles="$(parse_perf_counter "${perf_files[$idx]}" "${perf_event}")"
          cycles="$(parse_perf_counter "${perf_files[$idx]}" "cycles")"
          instructions="$(parse_perf_counter "${perf_files[$idx]}" "instructions")"
          task_clock_ms="$(parse_perf_counter "${perf_files[$idx]}" "task-clock")"
          throughput="$(parse_ycsb_throughput "${run_logs[$idx]}")"
          user_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "user")"
          kernel_time_ms="$(parse_time_metric_ms "${time_files[$idx]}" "sys")"
          cache_refs="$(parse_perf_counter "${perf_files[$idx]}" "cache-references")"
          cache_misses="$(parse_perf_counter "${perf_files[$idx]}" "cache-misses")"
          llc_loads="$(parse_perf_counter "${perf_files[$idx]}" "LLC-loads")"
          llc_load_misses="$(parse_perf_counter "${perf_files[$idx]}" "LLC-load-misses")"
          llc_hit_ratio_value="$(llc_hit_ratio "${llc_loads}" "${llc_load_misses}")"

          if [[ "${stall_cycles}" == "NA" || "${cycles}" == "NA" || "${instructions}" == "NA" || "${task_clock_ms}" == "NA" || "${throughput}" == "NA" || "${user_time_ms}" == "NA" || "${kernel_time_ms}" == "NA" || "${cache_refs}" == "NA" || "${cache_misses}" == "NA" ]]; then
            instance_status="parse_failed"
            if [[ "${group_status}" == "ok" ]]; then
              group_status="parse_failed"
            fi
          fi
        fi

        stall_values+=("${stall_cycles}")
        cycle_values+=("${cycles}")
        instruction_values+=("${instructions}")
        task_values+=("${task_clock_ms}")
        throughput_values+=("${throughput}")
        user_time_values+=("${user_time_ms}")
        kernel_time_values+=("${kernel_time_ms}")
        cache_ref_values+=("${cache_refs}")
        cache_miss_values+=("${cache_misses}")
        llc_load_values+=("${llc_loads}")
        llc_miss_values+=("${llc_load_misses}")

        timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
        append_summary_row "${instance_summary_file}" \
          "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
          "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" "${idx}" \
          "${cpus[$idx]}" "${db_targets[$idx]}" \
          "${recordcount}" "${operationcount}" "${threads}" "${java_active_processor_count}" \
          "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
          "${stall_cycles}" "${cycles}" "${instructions}" "${task_clock_ms}" "${throughput}" \
          "${user_time_ms}" "${kernel_time_ms}" "${cache_refs}" "${cache_misses}" "${llc_hit_ratio_value}" \
          "${instance_status}" "${perf_event}" "${workload_file}" "${run_dir}"
      done

      local summed_stall_cycles
      local summed_cycles
      local summed_instructions
      local summed_task_clock_ms
      local summed_instance_throughput
      local aggregate_wall_throughput
      local summed_user_time_ms
      local summed_kernel_time_ms
      local summed_cache_refs
      local summed_cache_misses
      local summed_llc_loads
      local summed_llc_load_misses
      local server_perf_llc_hit_ratio
      local timestamp

      if [[ "${group_status}" == "ok" ]]; then
        summed_stall_cycles="$(sum_integer_values "${stall_values[@]}")"
        summed_cycles="$(sum_integer_values "${cycle_values[@]}")"
        summed_instructions="$(sum_integer_values "${instruction_values[@]}")"
        summed_task_clock_ms="$(sum_float_values "${task_values[@]}")"
        summed_instance_throughput="$(sum_float_values "${throughput_values[@]}")"
        aggregate_wall_throughput="$(throughput_from_wall_ms "${total_operationcount}" "${group_wall_ms}")"
        summed_user_time_ms="$(sum_float_values "${user_time_values[@]}")"
        summed_kernel_time_ms="$(sum_float_values "${kernel_time_values[@]}")"
        summed_cache_refs="$(sum_integer_values "${cache_ref_values[@]}")"
        summed_cache_misses="$(sum_integer_values "${cache_miss_values[@]}")"
        summed_llc_loads="$(sum_integer_values "${llc_load_values[@]}")"
        summed_llc_load_misses="$(sum_integer_values "${llc_miss_values[@]}")"
        server_perf_llc_hit_ratio="$(llc_hit_ratio "${summed_llc_loads}" "${summed_llc_load_misses}")"
      else
        summed_stall_cycles="NA"
        summed_cycles="NA"
        summed_instructions="NA"
        summed_task_clock_ms="NA"
        summed_instance_throughput="NA"
        aggregate_wall_throughput="NA"
        summed_user_time_ms="NA"
        summed_kernel_time_ms="NA"
        summed_cache_refs="NA"
        summed_cache_misses="NA"
        summed_llc_loads="NA"
        summed_llc_load_misses="NA"
        server_perf_llc_hit_ratio="NA"
      fi

      timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
      append_summary_row "${summary_file}" \
        "${timestamp}" "${host}" "${arch}" "elasticsearch" "embedded" "${event_name}" "${scope}" \
        "${size_mb}" "${size_mb}" "${total_working_set_mb}" "${instance_count}" \
        "$(cpu_span_for_instance_count "${instance_count}")" \
        "${recordcount}" "${total_recordcount}" \
        "${operationcount}" "${total_operationcount}" \
        "${threads}" "${java_active_processor_count}" \
        "${fieldcount}" "${fieldlength}" "${record_value_bytes}" \
        "${summed_stall_cycles}" "${summed_cycles}" "${summed_instructions}" \
        "${summed_task_clock_ms}" "${summed_instance_throughput}" \
        "${group_wall_ms}" "${aggregate_wall_throughput}" \
        "${summed_user_time_ms}" "${summed_kernel_time_ms}" \
        "${summed_cache_refs}" "${summed_cache_misses}" "${server_perf_llc_hit_ratio}" \
        "${group_status}" "${perf_event}" "${workload_file}" "${run_dir}"
    done
    cleanup_result_db_path "${run_dir}/db/${size_mb}mb"
  done < <(list_sizes)

  LAST_MULTI_INSTANCE_RUN_DIR="${run_dir}"
  LAST_MULTI_INSTANCE_SUMMARY_FILE="${summary_file}"
  LAST_MULTI_INSTANCE_INSTANCE_SUMMARY_FILE="${instance_summary_file}"
  log "Completed ${arch} elasticsearch multi-instance sweep. Summary: ${summary_file}"
}
