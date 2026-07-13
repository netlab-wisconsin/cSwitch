#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_ROOT="${AE_OUT_ROOT:-$ROOT_DIR/ae/results}"
PLOT_ROOT="${AE_PLOT_OUT_ROOT:-$RESULTS_ROOT/figures}"

usage() {
  cat <<'USAGE'
Usage:
  ae/plot.sh [fig10|fig11|fig12|fig13|all]

Reads ae/results/fig*/results/aggregate_results.tsv and raw_results.tsv,
prepares normalized data, and renders Figures 10-13 as PDF and PNG.

Environment:
  AE_OUT_ROOT       Input result root, default ae/results
  AE_PLOT_OUT_ROOT  Figure output root, default $AE_OUT_ROOT/figures

Figure 10 and 11 use cSwitch as the normalization baseline. Legacy cSwitch
result trees remain supported as a fallback.
USAGE
}

TARGET="${1:-all}"
case "$TARGET" in
  -h|--help|help)
    usage
    exit 0
    ;;
  fig10|fig11|fig12|fig13|all)
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

for command in python3 gnuplot pdftoppm; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "error: required command not found: $command" >&2
    exit 1
  fi
done

mkdir -p "$PLOT_ROOT"
python3 "$ROOT_DIR/ae/plot.py" \
  --results-root "$RESULTS_ROOT" \
  --output-root "$PLOT_ROOT" \
  --figure "$TARGET"

case "$TARGET" in
  fig10) figures=(figure10) ;;
  fig11) figures=(figure11) ;;
  fig12) figures=(figure12) ;;
  fig13) figures=(figure13) ;;
  all) figures=(figure10 figure11 figure12 figure13) ;;
esac

for figure in "${figures[@]}"; do
  (
    cd "$ROOT_DIR/ae/plots"
    gnuplot -c "$figure.plt" "$PLOT_ROOT/data" "$PLOT_ROOT/$figure.pdf"
  )
  pdftoppm -singlefile -png -r 180 \
    "$PLOT_ROOT/$figure.pdf" "$PLOT_ROOT/$figure"
done

echo "rendered AE figures under $PLOT_ROOT"
