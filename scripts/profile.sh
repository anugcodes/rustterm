#!/usr/bin/env bash
# Profile a workload in the rustterm binary.
#
# Uses gprofng (clock-based profiling, no kernel perf counters needed).
#
# Usage:
#   scripts/profile.sh                 # build + record, then report
#   scripts/profile.sh --report-only   # skip build/record, just report
#
# The recording runs until you quit the app. Drive the workload you care about
# (cat a large file, open a big SFTP folder, sort columns, transfer a tree, run
# a VNC viewer) and then close the window; collection stops with it.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=target/profiling/rustterm
OUTDIR=/tmp/opencode
EXP="$OUTDIR/rustterm.er"

mkdir -p "$OUTDIR"

if [[ "${1:-}" != "--report-only" ]]; then
    cargo build --profile profiling
    rm -rf "$EXP"
    gprofng collect app -o "$EXP" "$BIN"
fi

if [[ ! -d "$EXP" ]]; then
    echo "ERROR: no experiment at $EXP" >&2
    echo "Run the script without --report-only first to record a session." >&2
    exit 1
fi

echo "=== top 30 functions ==="
gprofng display text -functions "$EXP" 2>/dev/null | head -45

echo
echo "=== full report written to $EXP ==="
echo "Inspect interactively with:  gprofng display text $EXP"