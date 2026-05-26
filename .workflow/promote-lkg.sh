#!/usr/bin/env bash
# promote-lkg.sh - Promote the live src/flywheel tree to the LKG snapshot.
#
# The dogfooding worker (task-worker.sh) runs flywheel via the LKG copy at
# .workflow/lkg/, not the live tree, so an agent task that breaks the
# lifecycle state machine or the harness does not strand the next task.
# Promotion is explicit: live -> LKG only happens when this script runs.
#
# Usage:
#   ./promote-lkg.sh             # run tests; refuse to promote on failure
#   ./promote-lkg.sh --bootstrap # skip tests (first-time setup only)
#   ./promote-lkg.sh --force     # promote even if tests fail (escape hatch)

set -euo pipefail

BOOTSTRAP=0
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bootstrap) BOOTSTRAP=1; shift ;;
    --force)     FORCE=1; shift ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

REPO_ROOT="$(git rev-parse --show-toplevel)"
LKG_DIR="$REPO_ROOT/.workflow/lkg"

OLD_SHA=""
if [[ -f "$LKG_DIR/SOURCE_SHA" ]]; then
  OLD_SHA="$(cat "$LKG_DIR/SOURCE_SHA")"
fi

if [[ "$BOOTSTRAP" -eq 1 ]]; then
  echo "[promote-lkg] bootstrap mode: skipping tests" >&2
elif [[ "$FORCE" -eq 1 ]]; then
  echo "[promote-lkg] force mode: skipping tests" >&2
else
  echo "[promote-lkg] running pytest against live tree..." >&2
  if ! ( cd "$REPO_ROOT" && uv run --project "$REPO_ROOT" pytest -q ); then
    echo "ERROR: tests failed; refusing to promote. Re-run with --force to override." >&2
    exit 1
  fi
fi

NEW_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
if ! git -C "$REPO_ROOT" diff-index --quiet HEAD -- 2>/dev/null; then
  NEW_SHA="${NEW_SHA}-dirty"
fi

# Wipe the snapshot but preserve .venv so uv does not re-resolve dependencies
# on every promotion.
mkdir -p "$LKG_DIR"
find "$LKG_DIR" -mindepth 1 -maxdepth 1 ! -name '.venv' -exec rm -rf {} +

mkdir -p "$LKG_DIR/src"
cp -a "$REPO_ROOT/src/flywheel" "$LKG_DIR/src/flywheel"
cp "$REPO_ROOT/pyproject.toml" "$LKG_DIR/pyproject.toml"
cp "$REPO_ROOT/uv.lock"        "$LKG_DIR/uv.lock"
cp "$REPO_ROOT/README.md"      "$LKG_DIR/README.md"

printf '%s\n' "$NEW_SHA" > "$LKG_DIR/SOURCE_SHA"

if [[ -n "$OLD_SHA" ]]; then
  echo "[promote-lkg] ${OLD_SHA} -> ${NEW_SHA}" >&2
else
  echo "[promote-lkg] (no prior LKG) -> ${NEW_SHA}" >&2
fi
