#!/usr/bin/env bash
# Profile a workload in the rustterm binary.
#
# Usage:
#   scripts/profile.sh                 # build + record, then report
#   scripts/profile.sh --report-only   # skip build/record, just report
#
# The recording runs until you quit the app. Drive the workload you care about
# (cat a large file, open a big SFTP folder, sort columns, transfer a tree, run
# a VNC viewer) and then close the window; perf stops with it.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=target/profiling/rustterm
OUT=/tmp/opencode/rustterm.perf.data

if [[ "${1:-}" != "--report-only" ]]; then
    cargo build --profile profiling
    rm -f "$OUT"
    perf record -g --call-graph dwarf -o "$OUT" "$BIN"
fi

echo "=== top 30 functions ==="
perf report -i "$OUT" --stdio --no-children 2>/dev/null | head -40

echo
echo "=== full report written to $OUT ==="
echo "Inspect interactively with:  perf report -i $OUT"