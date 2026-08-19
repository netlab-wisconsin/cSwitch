#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ROOT="${AE_OUT_ROOT:-$ROOT_DIR/ae/results}"
SCHEDULER_ROOT="${AE_SCHEDULER_ROOT:-$ROOT_DIR}"
SOURCE_COMMIT_FILE="$ROOT_DIR/ae/SOURCE_COMMIT"
SOURCE_CHECK="$ROOT_DIR/ae/check_source_snapshot.sh"

usage() {
  cat <<'USAGE'
Usage:
  ae/run.sh fig10|fig11|fig12a|fig12b|fig13|all dry-run|smoke|full [extra harness args...]
  ae/run.sh characterization smoke
  ae/run.sh <characterization-figure> optional

Examples:
  ae/run.sh fig13 dry-run
  ae/run.sh fig12a smoke
  ae/run.sh characterization smoke
  ae/run.sh fig3 optional
  AE_REPEATS=3 AE_MAX_ATTEMPTS=5 ae/run.sh fig13 full

Defaults:
  AE_LIGHT=1          Paper axes with lightweight repeat defaults
  AE_LIGHT_BUILD=1    Build local cSwitch variants without diagnostics logs
  AE_SCHEDULER_LOGS=0 Do not pass decision/runtime log paths
  AE_REPEATS=1        Successful repeats for full runs by default
  AE_MAX_ATTEMPTS=1   Max attempts per point by default
                      Set above AE_REPEATS to retry failed rXX attempts

Common env:
  AE_OUT_ROOT         Output root, default ae/results
  AE_SCHEDULER_ROOT   Scheduler source checkout, default repository root
  AE_GAPBS_ROOT       GAPBS checkout, default /home/seunghyun/gapbs/gapbs
  AE_GAPBS_GRAPH_ROOT GAPBS graphs, default below AE_GAPBS_ROOT
  AE_YCSB_ROOT        YCSB AE workspace, default /home/seunghyun/ycsb
  AE_YCSB_RUNNER      YCSB harness, default to the artifact-local runner
  AE_ORIENTDB_JAVA_HOME
                      Java 8 runtime used by OrientDB 2.2.37
  AE_EEVDF_ROOT       EEVDF checkout, default /home/seunghyun/scx_rustland_eevdf
  AE_EXP3_ROOT        exp3 workspace, default /home/seunghyun/exp3
  AE_NODE_REPLICATION_ROOT
                      Node-replication package, default below AE_EXP3_ROOT
  AE_LLAMA_ROOT       llama.cpp benchmark directory, default below AE_YCSB_ROOT
  AE_LLAMA_MODEL      llama.cpp model file on the author machine
  AE_MEMORY_BENCHMARK memory_benchmark binary; author paths are defaults
  AE_ALLOW_SOURCE_MISMATCH=1
                      Allow an unpinned scheduler checkout for development
  AE_PROFILE          Cargo profile, default release
  AE_USE_SUDO=0       Launch without sudo -n
  AE_SKIP_BUILD=1     Reuse existing AE-built binaries
  AE_VARIANTS         Space/comma separated variants
  AE_BENCHMARKS       Space/comma separated AE workloads
  AE_CORES            Figure 13 core list
  AE_NOISE_RATES      Figure 12 noise-rate list
  AE_FIG10_NOISE_RATE Fig10 loaded sidecar rate, default 50
USAGE
}

run_cmd() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

figure="${1:-}"
mode="${2:-dry-run}"
if [[ $# -gt 0 ]]; then
  shift
fi
if [[ $# -gt 0 ]]; then
  shift
fi

case "$mode" in
  optional)
    [[ $# -eq 0 ]] || {
      echo "error: tier aliases do not accept extra harness arguments" >&2
      exit 2
    }
    exec "$ROOT_DIR/reproduce.sh" "$figure" "$mode"
    ;;
esac
if [[ "$figure" == "characterization" ]]; then
  [[ $# -eq 0 ]] || {
    echo "error: characterization does not accept extra harness arguments" >&2
    exit 2
  }
  exec "$ROOT_DIR/reproduce.sh" "$figure" "$mode"
fi

case "$figure" in
  fig10|fig11|fig12a|fig12b|fig13|all)
    ;;
  ""|-h|--help|help)
    usage
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
esac

case "$mode" in
  dry-run|smoke|full)
    ;;
  *)
    usage
    exit 2
    ;;
esac

cd "$ROOT_DIR"
mkdir -p "$OUT_ROOT"
if [[ ! -f "$SCHEDULER_ROOT/Cargo.toml" ]]; then
  echo "error: scheduler source not found at $SCHEDULER_ROOT" >&2
  exit 1
fi
if ! "$SOURCE_CHECK" "$SCHEDULER_ROOT" "$SOURCE_COMMIT_FILE"; then
  echo "set AE_ALLOW_SOURCE_MISMATCH=1 only for intentional development runs" >&2
  exit 1
fi
export AE_SCHEDULER_ROOT="$SCHEDULER_ROOT"

run_cmd python3 ae/harness.py run \
  --figure "$figure" \
  --mode "$mode" \
  --out-root "$OUT_ROOT" \
  "$@"
