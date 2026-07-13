#!/usr/bin/env bash
set -euo pipefail

SCHEDULER_ROOT="$(realpath "${1:?scheduler root is required}")"
SOURCE_COMMIT_FILE="$(realpath "${2:?source commit file is required}")"

if [[ ! -f "$SOURCE_COMMIT_FILE" ]]; then
  echo "missing scheduler source pin: $SOURCE_COMMIT_FILE" >&2
  exit 1
fi

expected_commit="$(tr -d '[:space:]' < "$SOURCE_COMMIT_FILE")"
observed_commit="$(git -C "$SCHEDULER_ROOT" rev-parse HEAD 2>/dev/null || true)"
checksum_file="$(dirname "$SOURCE_COMMIT_FILE")/SOURCE_SHA256SUMS"
source_paths=(
  Cargo.toml
  Cargo.lock
  build.rs
  config.load.unpin.xml
  intf.h
  main.bpf.c
  src
  tests
  vendor
)

if [[ -n "$observed_commit" ]] && git -C "$SCHEDULER_ROOT" cat-file -e "$expected_commit^{commit}" 2>/dev/null; then
  if git -C "$SCHEDULER_ROOT" diff --quiet "$expected_commit" -- "${source_paths[@]}"; then
    printf 'scheduler source matches paper base %s (checkout %s)\n' \
      "$expected_commit" "$observed_commit"
    exit 0
  fi
  if [[ "${AE_ALLOW_SOURCE_MISMATCH:-0}" == "1" ]]; then
    echo "warning: scheduler source differs from paper base $expected_commit" >&2
    exit 0
  fi
  echo "scheduler source differs from paper base $expected_commit" >&2
  git -C "$SCHEDULER_ROOT" diff --stat "$expected_commit" -- "${source_paths[@]}" >&2
  exit 1
fi

if [[ -f "$checksum_file" ]] && (
  cd "$SCHEDULER_ROOT"
  sha256sum --check --status "$checksum_file"
); then
  printf 'scheduler source matches paper base %s (SHA256 manifest)\n' "$expected_commit"
  exit 0
fi

if [[ "${AE_ALLOW_SOURCE_MISMATCH:-0}" == "1" ]]; then
  echo "warning: scheduler source cannot be verified against paper base $expected_commit" >&2
  exit 0
fi
echo "scheduler source cannot be verified against paper base $expected_commit" >&2
exit 1
