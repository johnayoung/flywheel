#!/usr/bin/env bash
# run_silent.sh -- context-efficient backpressure wrapper.
#
# Runs a command with its output captured. On success, prints a single "ok"
# line and discards the output; on failure, prints a "FAIL" line, dumps the
# full captured output, and propagates the command's exit code. This keeps
# passing runs from flooding an agent's (or a human's) context with noise while
# preserving the full signal exactly when it is needed.
#
# Usage:
#   scripts/run_silent.sh <description> <command> [args...]
#
# Set RUN_SILENT_VERBOSE=1 to stream output live and skip capture (useful for
# debugging the command itself or for interactive sessions).
set -euo pipefail

if [ "$#" -lt 2 ]; then
    echo "usage: scripts/run_silent.sh <description> <command> [args...]" >&2
    exit 2
fi

description="$1"
shift

if [ "${RUN_SILENT_VERBOSE:-}" = "1" ]; then
    exec "$@"
fi

tmpfile=$(mktemp)
trap 'rm -f "$tmpfile"' EXIT

if "$@" >"$tmpfile" 2>&1; then
    echo "  ok   $description"
else
    exit_code=$?
    echo "  FAIL $description"
    cat "$tmpfile"
    exit "$exit_code"
fi
