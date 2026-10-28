#!/usr/bin/env bash
# check.sh -- run the full CI gate locally, quietly.
#
# Runs the same three checks as .github/workflows/ci.yml, in the same order
# (rustfmt -> clippy -> cargo test), each through the run_silent.sh
# backpressure wrapper. A clean tree prints three "ok" lines; the first failing
# gate dumps only its own output and stops. Run this before reporting a task
# done and before pushing to main -- a green run here is what CI runs.
#
# Usage:
#   scripts/check.sh
#
# Set RUN_SILENT_VERBOSE=1 to stream each gate's output live. Docker-backed
# tests (Postgres, container backend) skip themselves when no daemon is
# reachable; CI runs them.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

scripts/run_silent.sh "rustfmt"    cargo fmt --all --check
scripts/run_silent.sh "clippy"     cargo clippy --workspace --all-targets -- -D warnings
scripts/run_silent.sh "cargo test" cargo test --workspace
