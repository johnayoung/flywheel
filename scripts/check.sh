#!/usr/bin/env bash
# check.sh -- run the full CI gate locally, quietly.
#
# Runs the same three checks as .github/workflows/ci.yml, in the same order
# (ruff -> pyright -> pytest), each through the run_silent.sh backpressure
# wrapper. A clean tree prints three "ok" lines; the first failing gate dumps
# only its own output and stops. Run this before reporting a task done and
# before pushing to main -- a green run here is what CI runs.
#
# Usage:
#   scripts/check.sh
#
# Set RUN_SILENT_VERBOSE=1 to stream each gate's output live.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

scripts/run_silent.sh "ruff"    uv run ruff check .
scripts/run_silent.sh "pyright" uv run pyright
# Parallelize the full suite with pytest-xdist. --dist loadscope keeps each
# module's tests on one worker, so the subprocess-/daemon-heavy modules never
# get split across workers (which would contend on ports, docker, and the merge
# flock). 4 workers is the measured sweet spot (~2.3x faster, stable); override
# with PYTEST_WORKERS (e.g. =1 for a serial run, =auto to match core count).
scripts/run_silent.sh "pytest"  uv run pytest -n "${PYTEST_WORKERS:-4}" --dist loadscope
