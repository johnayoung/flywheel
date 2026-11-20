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

# cargo-nextest runs test binaries in parallel (one process per test); it
# does not run doctests, so those get their own step. Without nextest the
# plain `cargo test` covers both.
run_tests() {
  if cargo nextest --version >/dev/null 2>&1; then
    scripts/run_silent.sh "cargo test" cargo nextest run --workspace --no-fail-fast
    scripts/run_silent.sh "doctests"   cargo test --workspace --doc
  else
    scripts/run_silent.sh "cargo test" cargo test --workspace --no-fail-fast
  fi
}

# The tests alone, under the lease below (check.sh runs itself again).
if [[ "${FLYWHEEL_CHECK_PHASE:-}" == tests ]]; then
  run_tests
  exit
fi

scripts/run_silent.sh "rustfmt"    cargo fmt --all --check
scripts/run_silent.sh "clippy"     cargo clippy --workspace --all-targets -- -D warnings
# Compile the test binaries before taking the lock, so only running them
# is serialized.
scripts/run_silent.sh "build tests" cargo test --workspace --no-run
# One test suite at a time per machine: each run uses every core, and
# several at once (parallel agent worktrees) run 3-4x slower and trip the
# hub's timing-sensitive tests. Others wait here; build and clippy above
# stay parallel. With a running hub that has leases, the turn is the hub
# lease cpu:test-suite (fair, visible on the board, dropped if this run
# dies) and the file lock too, so runs with and without a hub take turns;
# else the file lock alone. FLYWHEEL_CHECK_LOCK=0 opts out.
#
# The wait is bounded (FLYWHEEL_CHECK_LOCK_WAIT seconds, default 4h) and
# names the holder. A run nested inside one holding the lock (a tool or a
# check.sh its tests start) fails at once instead of waiting on itself:
# the holder lists itself in FLYWHEEL_FLOCKS_HELD for what it runs.
lock="${XDG_RUNTIME_DIR:-/tmp}/flywheel-check.lock"
wait_secs="${FLYWHEEL_CHECK_LOCK_WAIT:-14400}"

# This shell's ancestors' pids, one per line.
ancestors() {
  local p=$$ up
  while up=$(sed 's/.*) //' "/proc/$p/stat" 2>/dev/null | cut -d' ' -f2) \
    && [[ -n "$up" && "$up" != 0 ]]; do
    echo "$up"
    p=$up
  done
}

# Who holds the lock: a live holder /proc/locks names, else the one its
# file names (a holder notes itself there).
holder() {
  local pid label ino
  ino=$(stat -c %i "$lock" 2>/dev/null || true)
  local taker=""
  for pid in $(awk -v ino=":$ino" '$2 == "FLOCK" && substr($6, length($6) - length(ino) + 1) == ino { print $5 }' /proc/locks 2>/dev/null); do
    if [[ -r "/proc/$pid/cmdline" ]]; then
      echo "pid $pid ($(tr '\0' ' ' <"/proc/$pid/cmdline" | cut -c1-160))"
      return
    fi
    taker=$pid
  done
  read -r pid label <"$lock" 2>/dev/null || true
  if [[ -n "${pid:-}" && -e "/proc/$pid" ]]; then
    echo "pid $pid (${label:-?})"
  elif [[ -n "${taker}" ]]; then
    echo "a process that inherited it from pid $taker, now exited (see: fuser -v $lock)"
  else
    echo "an unknown process (see: fuser -v $lock)"
  fi
}

if [[ "${FLYWHEEL_CHECK_LOCK:-1}" != 0 && -n "${FLYWHEEL_FLOCKS_HELD:-}" && -e "$lock" ]]; then
  ups=" $(ancestors | tr '\n' ' ') "
  while read -r pid path; do
    if [[ -n "${pid:-}" && "$ups" == *" $pid "* && "$path" -ef "$lock" ]]; then
      cmd=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | cut -c1-160)
      echo "  FAIL scripts/check.sh: the test-suite lock $lock is already held by your ancestor pid $pid ($cmd): this run is inside that hold, so waiting would wait on itself (set FLYWHEEL_CHECK_LOCK=0 for nested runs)" >&2
      exit 1
    fi
  done <<<"$FLYWHEEL_FLOCKS_HELD"
fi
if [[ "${FLYWHEEL_CHECK_LOCK:-1}" != 0 ]] && command -v fw >/dev/null 2>&1 \
  && fw hub lease --help >/dev/null 2>&1; then
  FLYWHEEL_CHECK_PHASE=tests exec fw hub lease run cpu:test-suite --ttl 45m --wait "$wait_secs" \
    --note "scripts/check.sh in $REPO_ROOT" --or-flock "$lock" -- "$REPO_ROOT/scripts/check.sh"
fi
if [[ "${FLYWHEEL_CHECK_LOCK:-1}" != 0 ]] && command -v flock >/dev/null 2>&1; then
  # Opened to append: a waiter must not wipe the holder's note.
  exec 9>>"$lock"
  if ! flock -n 9; then
    echo "  ..   waiting for another scripts/check.sh test run on this machine: $(holder)"
    if ! flock -w "$wait_secs" 9; then
      echo "  FAIL waited ${wait_secs}s for the test-suite lock $lock, held by $(holder)" >&2
      exit 1
    fi
  fi
  printf '%s %s\n' "$$" "scripts/check.sh in $REPO_ROOT" >"$lock"
  export FLYWHEEL_FLOCKS_HELD="${FLYWHEEL_FLOCKS_HELD:+$FLYWHEEL_FLOCKS_HELD$'\n'}$$ $lock"
fi
run_tests
