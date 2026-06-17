#!/usr/bin/env bash
# Build a self-contained, offline-installable artifact set for a foreign
# (out-of-checkout) install of flywheel.
#
# Produces into the caller-specified out-dir:
#   * wheels + sdists for every workspace package an install of `flywheel`
#     transitively needs (flywheel, flywheel-core, flywheel-orchestrator,
#     flywheel-worktree), built by the uv_build backend -- which bundles the
#     runtime package data (`_schema/*.sql`, `_skill_templates/*.md`, `py.typed`)
#     by default;
#   * the full third-party dependency closure of `flywheel` (including the
#     `flywheel-core[claude]` extra) as wheels.
#
# Because the workspace wheels are downloaded with the out-dir itself on the
# find-links path, the resolver prefers our `flywheel==0.1.0` over the unrelated
# `flywheel` distribution on the public index -- so the resulting set installs
# cleanly with `--no-index` (no index, no workspace source on the path):
#
# Usage:
#   scripts/build-dist.sh <out-dir>
#
# Foreign install (in a venv created OUTSIDE this checkout):
#   uv pip install --no-index --find-links <out-dir> flywheel
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/build-dist.sh <out-dir>" >&2
    exit 2
fi

OUT_DIR="$1"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mkdir -p "$OUT_DIR"

# 1. Build every workspace package (wheel + sdist) into the out-dir.
uv build --all-packages --out-dir "$OUT_DIR" --directory "$REPO_ROOT"

# 2. Vendor the full third-party dependency closure as wheels into the same
#    out-dir. The out-dir is on --find-links so our freshly built workspace
#    wheels are used to satisfy the inter-flywheel edges (and so the resolver
#    prefers our flywheel over the unrelated public-index distribution of the
#    same name); the public index supplies the third-party transitive deps.
#    The [claude] extra is requested explicitly so the agent SDK closure ships.
uv run --directory "$REPO_ROOT" --with pip --no-project python -m pip download \
    --find-links "$OUT_DIR" \
    --dest "$OUT_DIR" \
    "flywheel==0.1.0" "flywheel-core[claude]==0.1.0"

echo "Built self-contained install artifact set into $OUT_DIR"
ls "$OUT_DIR"
