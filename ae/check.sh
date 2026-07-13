#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AE_DIR="$PACKAGE_ROOT/ae"
SCHEDULER_ROOT="${AE_SCHEDULER_ROOT:-$PACKAGE_ROOT}"
GAPBS_ROOT="${AE_GAPBS_ROOT:-/home/seunghyun/gapbs/gapbs}"
YCSB_ROOT="${AE_YCSB_ROOT:-/home/seunghyun/ycsb}"
EEVDF_ROOT="${AE_EEVDF_ROOT:-/home/seunghyun/scx_rustland_eevdf}"
MACHINE_LOCK_PATH="${AE_MACHINE_LOCK:-/run/lock/cswitch-ae.lock}"
STRICT=0

if [[ "${1:-}" == "--strict" ]]; then
  STRICT=1
fi

status=0

ok() {
  printf '[ok] %s\n' "$*"
}

warn() {
  printf '[warn] %s\n' "$*"
  if (( STRICT )); then
    status=1
  fi
}

fail() {
  printf '[fail] %s\n' "$*"
  status=1
}

need_cmd() {
  local cmd="$1"
  local desc="$2"
  if command -v "$cmd" >/dev/null 2>&1; then
    ok "$desc: $(command -v "$cmd")"
  else
    fail "$desc: missing command '$cmd'"
  fi
}

need_path() {
  local path="$1"
  local desc="$2"
  if [[ -e "$path" ]]; then
    ok "$desc: $path"
  else
    fail "$desc: missing $path"
  fi
}

optional_path() {
  local path="$1"
  local desc="$2"
  if [[ -e "$path" ]]; then
    ok "$desc: $path"
  else
    warn "$desc: missing $path"
  fi
}

cd "$PACKAGE_ROOT"

printf 'cSwitch AE package: %s\n' "$PACKAGE_ROOT"
printf 'scheduler source: %s\n' "$SCHEDULER_ROOT"

need_cmd bash "bash"
need_cmd python3 "python3"
need_cmd cargo "cargo"
need_cmd sudo "sudo"
need_cmd taskset "taskset"
need_cmd timeout "timeout"
need_cmd flock "machine-wide campaign lock"
need_cmd gnuplot "gnuplot figure renderer"
need_cmd pdftoppm "PDF-to-PNG converter"
need_cmd git "Git scheduler pin checker"
need_cmd sha256sum "source archive checksum checker"

need_path "$PACKAGE_ROOT/README.md" "top-level evaluator README"
need_path "$PACKAGE_ROOT/reproduce.sh" "top-level reproduction entry point"
need_path "$SCHEDULER_ROOT/Cargo.toml" "scheduler Rust manifest"
need_path "$SCHEDULER_ROOT/src/main.rs" "scheduler binary entry point"
need_path "$AE_DIR/harness.py" "AE harness"
need_path "$AE_DIR/external_workload.py" "AE external workload helper"
need_path "$AE_DIR/run.sh" "AE run wrapper"
need_path "$AE_DIR/plot.py" "AE plot data preparer"
need_path "$AE_DIR/plot.sh" "AE plot wrapper"
need_path "$AE_DIR/ccm_mapping.txt" "AE CCM/CS-link mapping"
need_path "$AE_DIR/ARTIFACT_RELEASE" "pinned artifact release tag"
need_path "$AE_DIR/SOURCE_COMMIT" "pinned scheduler commit"
need_path "$AE_DIR/SOURCE_SHA256SUMS" "pinned scheduler checksums"
need_path "$AE_DIR/check_source_snapshot.sh" "scheduler source pin checker"
need_path "$GAPBS_ROOT/pr" "GAPBS PageRank"
need_path "$GAPBS_ROOT/bc" "GAPBS Betweenness Centrality"
need_path "$AE_DIR/templates/filebench_webserver_10s.f" "AE Filebench webserver template"
need_path "$AE_DIR/templates/filebench_webproxy_10s.f" "AE Filebench webproxy template"
need_path "$AE_DIR/templates/filebench_varmail_10s.f" "AE Filebench varmail template"
if [[ -n "${AE_MEMORY_BENCHMARK:-}" ]]; then
  optional_path "$AE_MEMORY_BENCHMARK" "memory_benchmark"
else
  optional_path "$YCSB_ROOT/memory_benchmark" "memory_benchmark"
  optional_path "/home/seunghyun/sched_bench/build/memory_benchmark" "alternate memory_benchmark"
fi
optional_path "$EEVDF_ROOT/Cargo.toml" "EEVDF baseline repo"

if [[ -f "$MACHINE_LOCK_PATH" && -r "$MACHINE_LOCK_PATH" ]]; then
  ok "machine-wide campaign lock: $MACHINE_LOCK_PATH"
else
  fail "machine-wide campaign lock is missing or unreadable: $MACHINE_LOCK_PATH"
fi

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  expected_release="$(tr -d '[:space:]' < "$AE_DIR/ARTIFACT_RELEASE")"
  expected_head="$(git rev-parse "$expected_release^{commit}" 2>/dev/null || true)"
  observed_head="$(git rev-parse HEAD)"
  if [[ -z "$expected_head" ]]; then
    fail "artifact release tag is unavailable: $expected_release"
  elif [[ "$observed_head" == "$expected_head" ]]; then
    ok "artifact release: $expected_release ($observed_head)"
  else
    fail "checkout HEAD $observed_head does not match $expected_release ($expected_head)"
  fi
else
  warn "Git metadata unavailable; release tag cannot be verified"
fi

if "$AE_DIR/check_source_snapshot.sh" "$SCHEDULER_ROOT" "$AE_DIR/SOURCE_COMMIT"; then
  ok "scheduler source snapshot"
else
  fail "scheduler source snapshot mismatch"
fi

if bash -n "$PACKAGE_ROOT/reproduce.sh" "$AE_DIR/run.sh" \
  && "$PACKAGE_ROOT/reproduce.sh" --help >/dev/null; then
  ok "top-level reproduction workflow"
else
  fail "top-level reproduction workflow validation failed"
fi

if python3 "$AE_DIR/harness.py" --help >/dev/null; then
  ok "ae/harness.py --help"
else
  fail "ae/harness.py --help failed"
fi

if AE_SCHEDULER_ROOT="$SCHEDULER_ROOT" python3 "$AE_DIR/harness.py" check; then
  ok "ae/harness.py check"
else
  fail "ae/harness.py check failed"
fi

if sudo -n true >/dev/null 2>&1; then
  ok "passwordless sudo"
else
  warn "passwordless sudo is unavailable; full scheduler runs will fail unless run as root or sudo is configured"
fi

if [[ -e /dev/cpu/0/msr ]]; then
  ok "/dev/cpu/0/msr"
else
  warn "/dev/cpu/0/msr is missing; hardware counter samplers may fail"
fi

if [[ -d /sys/fs/cgroup ]]; then
  ok "cgroup filesystem: /sys/fs/cgroup"
else
  fail "cgroup filesystem missing"
fi

if [[ -d /sys/kernel/sched_ext || -d /sys/kernel/debug/sched_ext ]]; then
  ok "sched_ext kernel interface detected"
else
  warn "sched_ext kernel interface not detected in /sys/kernel or /sys/kernel/debug"
fi

if (( status == 0 )); then
  printf 'AE preflight passed.\n'
else
  printf 'AE preflight found issues.\n'
fi

exit "$status"
