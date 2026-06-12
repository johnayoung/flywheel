#!/usr/bin/env bash
# Seed a throwaway README-rewrite task into .flywheel/tasks/active/ to smoke-test
# the loop after changes. Repeatable: each run replaces the previous seed and
# stamps a fresh task id so the store never sees it as already completed.
#
# Usage:
#   scripts/seed-readme-task.sh
#   uv run flywheel worker --once
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PHASE_DIR="$REPO_ROOT/.flywheel/tasks/active/99-smoke-readme"
TASK_ID="smoke-readme-rewrite-$(date +%Y%m%d-%H%M%S)"

rm -rf "$PHASE_DIR"
mkdir -p "$PHASE_DIR"

cat > "$PHASE_DIR/readme-rewrite.json" <<EOF
{
  "id": "$TASK_ID",
  "goal": "Rewrite the root README.md from scratch as a concise onboarding page for the flywheel workspace: one-line pitch, quickstart (Python 3.13, uv sync, running a task via uv run flywheel worker --once), the four-package layout, and links to docs/.",
  "tags": ["docs", "smoke"],
  "context": {
    "relevant": [
      "CLAUDE.md",
      "pyproject.toml",
      "docs/vision.md",
      "docs/task-schema.md",
      "docs/task-lifecycle.md",
      "docs/loop.md",
      "packages/flywheel/src/flywheel/_cli.py"
    ],
    "constraints": [
      "Write the README greenfield -- do not preserve or merge the existing README.md content.",
      "Only README.md may change; touch no other file.",
      "Be extremely concise: short sections, examples over prose, no badges or images.",
      "No emojis anywhere.",
      "Commit with a Conventional Commits message using the docs: prefix before reporting verify."
    ],
    "non_goals": [
      "Do not create or edit CONTRIBUTING.md, LICENSE, or anything under docs/.",
      "Do not change any code or configuration."
    ]
  },
  "graders": [
    { "type": "command", "run": "test -s README.md", "name": "readme-exists" },
    { "type": "command", "run": "grep -qi flywheel README.md && grep -q 'uv sync' README.md", "name": "readme-content" },
    { "type": "command", "run": "git diff --name-only HEAD~1 HEAD | grep -qx README.md && [ \$(git diff --name-only HEAD~1 HEAD | wc -l) -eq 1 ]", "name": "only-readme-changed" },
    { "type": "command", "run": "git log -1 --format=%s | grep -q '^docs:'", "name": "commit-landed" },
    { "type": "transcript", "max_turns": 30 }
  ]
}
EOF

echo "Seeded $PHASE_DIR/readme-rewrite.json"
echo "Task id: $TASK_ID"
echo "Next: uv run flywheel worker --once"
