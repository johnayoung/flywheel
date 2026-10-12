#!/usr/bin/env bash
# check-rust.sh -- run the Rust CI gate locally, quietly.
#
# Mirrors scripts/check.sh for the Cargo workspace: rustfmt -> clippy ->
# cargo test, each through the run_silent.sh backpressure wrapper. A clean
# tree prints three "ok" lines; the first failing gate dumps only its own
# output and stops.
#
# Usage:
#   scripts/check-rust.sh
#
# Set RUN_SILENT_VERBOSE=1 to stream each gate's output live. Docker-backed
# tests (Postgres, container backend) skip themselves when no daemon is
# reachable.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

scripts/run_silent.sh "rustfmt" cargo fmt --all --check
scripts/run_silent.sh "clippy"  cargo clippy --workspace --all-targets -- -D warnings
scripts/run_silent.sh "cargo test" cargo test --workspace
