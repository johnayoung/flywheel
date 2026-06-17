#!/usr/bin/env bash
# Build the full installable artifact set for a foreign (out-of-checkout) install
# of flywheel. Produces wheels for every workspace package an install of
# `flywheel` transitively needs -- flywheel, flywheel-core, flywheel-orchestrator,
# flywheel-worktree -- into a caller-specified out-dir, so a resolver in an
# environment with NO workspace source on its path can complete the install graph
# from artifact metadata alone (inherits the inter-package version constraints).
#
# Usage:
#   scripts/build-dist.sh <out-dir>
#
# Then, in a venv outside this checkout:
#   uv pip install --no-index --find-links <out-dir> flywheel
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/build-dist.sh <out-dir>" >&2
    exit 2
fi

OUT_DIR="$1"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mkdir -p "$OUT_DIR"

# Build every workspace package. `--all-packages` emits a wheel + sdist for each
# member; the runtime package data (`_schema/*.sql`, `_skill_templates/*.md`,
# `py.typed`) is bundled by the uv_build backend by default.
uv build --all-packages --out-dir "$OUT_DIR" --directory "$REPO_ROOT"

echo "Built install artifact set into $OUT_DIR"
ls "$OUT_DIR"
