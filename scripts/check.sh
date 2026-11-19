#!/usr/bin/env bash
# check.sh -- run the full CI gate locally, quietly.
#
# Runs the same three checks as .github/workflows/ci.yml, in the same order
# (rustfmt -> clippy -> tests), each through the run_silent.sh
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
# Compile the test binaries before taking the lock, so only running them
# is serialized.
scripts/run_silent.sh "build tests" cargo test --workspace --no-run
# One test suite at a time per machine: each run uses every core, and
# several at once (parallel agent worktrees) run 3-4x slower and trip the
# hub's timing-sensitive tests. Others wait here; build and clippy above
# stay parallel. FLYWHEEL_CHECK_LOCK=0 opts out.
if [[ "${FLYWHEEL_CHECK_LOCK:-1}" != 0 ]] && command -v flock >/dev/null 2>&1; then
  lock="${XDG_RUNTIME_DIR:-/tmp}/flywheel-check.lock"
  exec 9>"$lock"
  if ! flock -n 9; then
    echo "  ..   waiting for another scripts/check.sh test run on this machine"
    flock 9
  fi
fi
# cargo-nextest runs test binaries in parallel (one process per test); it
# does not run doctests, so those get their own step. Without nextest the
# plain `cargo test` covers both.
if cargo nextest --version >/dev/null 2>&1; then
  scripts/run_silent.sh "cargo test" cargo nextest run --workspace --no-fail-fast
  scripts/run_silent.sh "doctests"   cargo test --workspace --doc
else
  scripts/run_silent.sh "cargo test" cargo test --workspace --no-fail-fast
fi
