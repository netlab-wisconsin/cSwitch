#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Group-writable outputs are sufficient for an administrator-managed checkout.
# Evaluator-specific checkouts need no shared write permission.
umask 002

MACHINE_LOCK_PATH="${AE_MACHINE_LOCK:-/run/lock/cswitch-ae.lock}"
MACHINE_LOCK_FD=""

usage() {
  cat <<'USAGE'
Usage:
  ./reproduce.sh check
  ./reproduce.sh primary [dry-run|smoke|full|plot]
  ./reproduce.sh optional [smoke|plot]
  ./reproduce.sh fig10|fig11|fig12|fig12a|fig12b|fig13|all [dry-run|smoke|full|plot]
  ./reproduce.sh characterization smoke|plot
  ./reproduce.sh <characterization-figure> dry-run|optional|plot

Examples:
  ./reproduce.sh check
  ./reproduce.sh primary
  ./reproduce.sh optional
  ./reproduce.sh fig12 dry-run
  ./reproduce.sh fig13 smoke
  AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig12
  AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh all
  ./reproduce.sh characterization smoke
  ./reproduce.sh fig3 optional

Primary targets default to full when the action is omitted. A full run refreshes
summaries and renders the corresponding PDF and PNG figures. The primary/all
target covers Figures 10-13 and launches all four configured variants.

Characterization targets:
  fig2, fig2a, fig2b, fig3, fig4, fig5, fig6, fig6a, fig6b, fig6c,
  fig6d, fig8, fig8ab, fig8c, fig8d, fig8e, fig8f, fig8f-bw

Fresh Figure 4 and Figure 8d runs are outside the AE scope; use plot instead.

Experiment-launching commands acquire AE_MACHINE_LOCK (default:
/run/lock/cswitch-ae.lock). Set AE_LOCK_WAIT_SECONDS to wait instead of
failing immediately when another campaign owns the machine.
USAGE
}

die() {
  echo "error: $*" >&2
  exit 2
}

acquire_machine_lock() {
  [[ -z "$MACHINE_LOCK_FD" ]] || return 0
  command -v flock >/dev/null 2>&1 || die "flock is required for experiment runs"
  [[ -f "$MACHINE_LOCK_PATH" && -r "$MACHINE_LOCK_PATH" ]] || die \
    "machine lock is missing or unreadable: $MACHINE_LOCK_PATH; see README.md#machine-access-and-concurrency"

  local wait_seconds="${AE_LOCK_WAIT_SECONDS:-0}"
  [[ "$wait_seconds" =~ ^[0-9]+$ ]] || die "AE_LOCK_WAIT_SECONDS must be a non-negative integer"
  exec {MACHINE_LOCK_FD}<"$MACHINE_LOCK_PATH"
  if (( wait_seconds > 0 )); then
    flock --exclusive --wait "$wait_seconds" "$MACHINE_LOCK_FD" || die \
      "the AE machine is busy after waiting ${wait_seconds}s; another campaign holds $MACHINE_LOCK_PATH"
  else
    flock --exclusive --nonblock "$MACHINE_LOCK_FD" || die \
      "the AE machine is busy; another campaign holds $MACHINE_LOCK_PATH"
  fi
  printf 'acquired AE machine lock: %s\n' "$MACHINE_LOCK_PATH"
}

run_evaluation() {
  local target="$1"
  local mode="$2"
  local -a figures

  case "$target" in
    fig12) figures=(fig12a fig12b) ;;
    all) figures=(all) ;;
    *) figures=("$target") ;;
  esac

  for figure in "${figures[@]}"; do
    "$ROOT_DIR/ae/run.sh" "$figure" "$mode"
  done
}

render_evaluation() {
  local target="$1"

  case "$target" in
    fig10|fig11|fig13)
      "$ROOT_DIR/ae/summarize.sh" "$target"
      "$ROOT_DIR/ae/plot.sh" "$target"
      ;;
    fig12)
      "$ROOT_DIR/ae/summarize.sh" fig12a
      "$ROOT_DIR/ae/summarize.sh" fig12b
      "$ROOT_DIR/ae/plot.sh" fig12
      ;;
    fig12a|fig12b)
      "$ROOT_DIR/ae/summarize.sh" "$target"
      "$ROOT_DIR/ae/plot.sh" fig12
      ;;
    all)
      "$ROOT_DIR/ae/summarize.sh" all
      "$ROOT_DIR/ae/plot.sh" all
      ;;
  esac
}

run_optional_characterization() {
  local target="$1"
  local -a figures

  acquire_machine_lock

  case "$target" in
    fig4)
      die "fresh Figure 4 requires physical DIMM changes and is outside AE scope; use './reproduce.sh fig4 plot'"
      ;;
    fig8d)
      die "fresh Figure 8d requires NPS4/two-DIMM reconfiguration and is outside AE scope; use './reproduce.sh fig8d plot'"
      ;;
    fig8)
      figures=(fig8ab fig8c fig8e fig8f fig8f-bw)
      ;;
    *)
      figures=("$target")
      ;;
  esac

  "$ROOT_DIR/motivation/run.sh" "${figures[0]}" check
  for figure in "${figures[@]}"; do
    "$ROOT_DIR/motivation/run.sh" "$figure" full
  done

  # Fresh characterization outputs have panel-specific aggregation steps.
  # Render the preserved reference plot separately instead of relabeling it.
  if [[ "$target" != "fig8f-bw" ]]; then
    "$ROOT_DIR/motivation/run.sh" "$target" plot
  fi
}

target="${1:-}"
action="${2:-}"
[[ $# -le 2 ]] || die "too many arguments"

case "$target" in
  primary)
    target="all"
    ;;
  optional)
    target="characterization"
    action="${action:-smoke}"
    ;;
esac

case "$target" in
  ""|-h|--help|help)
    usage
    exit 0
    ;;
  check)
    [[ -z "$action" ]] || die "check does not accept a second argument"
    exec "$ROOT_DIR/ae/check.sh"
    ;;
  characterization|motivation)
    case "$action" in
      smoke)
        # The NPS1 check validates the complete frozen bundle and avoids
        # treating the intentionally unsupported NPS4 profile as a warning.
        "$ROOT_DIR/motivation/run.sh" fig3 check
        exec "$ROOT_DIR/motivation/run.sh" all plot
        ;;
      plot)
        exec "$ROOT_DIR/motivation/run.sh" all plot
        ;;
      *)
        die "characterization supports smoke or plot"
        ;;
    esac
    ;;
  fig10|fig11|fig12|fig12a|fig12b|fig13|all)
    action="${action:-full}"
    ;;
  fig2|fig2a|fig2b|fig3|fig4|fig5|fig6|fig6a|fig6b|fig6c|fig6d|fig8|fig8ab|fig8c|fig8d|fig8e|fig8f|fig8f-bw)
    case "$action" in
      dry-run)
        exec "$ROOT_DIR/motivation/run.sh" "$target" dry-run
        ;;
      optional)
        run_optional_characterization "$target"
        exit 0
        ;;
      plot)
        [[ "$target" != "fig8f-bw" ]] || die "Figure 8f bandwidth is a table artifact and has no standalone plot"
        exec "$ROOT_DIR/motivation/run.sh" "$target" plot
        ;;
      *)
        die "characterization figure $target supports dry-run, optional, or plot"
        ;;
    esac
    ;;
  *)
    usage >&2
    die "unknown target: $target"
    ;;
esac

case "$action" in
  dry-run)
    run_evaluation "$target" dry-run
    ;;
  smoke)
    acquire_machine_lock
    "$ROOT_DIR/ae/check.sh"
    run_evaluation "$target" smoke
    ;;
  full)
    acquire_machine_lock
    export AE_REPEATS="${AE_REPEATS:-1}"
    export AE_MAX_ATTEMPTS="${AE_MAX_ATTEMPTS:-3}"
    "$ROOT_DIR/ae/check.sh"
    run_evaluation "$target" full
    render_evaluation "$target"
    ;;
  plot)
    render_evaluation "$target"
    ;;
  "")
    usage >&2
    die "an action is required"
    ;;
  *)
    usage >&2
    die "unknown action: $action"
    ;;
esac
