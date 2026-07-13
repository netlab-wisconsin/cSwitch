#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ROOT="${AE_OUT_ROOT:-$ROOT_DIR/ae/results}"

usage() {
  cat <<'USAGE'
Usage:
  ae/summarize.sh fig10|fig11|fig12a|fig12b|fig13|all [extra harness args...]

Refreshes summary files from ae/results using ae/harness.py only.
USAGE
}

figure="${1:-all}"
if [[ $# -gt 0 ]]; then
  shift
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

cd "$ROOT_DIR"
exec python3 ae/harness.py summarize --figure "$figure" --out-root "$OUT_ROOT" "$@"
