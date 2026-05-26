#!/usr/bin/env bash
# task-worker.sh - Autonomous worker loop that drives flywheel tasks.
#
# Selects the next eligible task from .workflow/tasks/active/<phase>/ via
# `python -m flywheel.workflow next`, executes it with `python -m
# flywheel.workflow run`, and loops until killed. SQLite at
# .workflow/flywheel.sqlite is the source of truth for task completion;
# task JSON files are immutable.
#
# Pattern: infinite loop, per-task logs, graceful signal handling.
#
# Usage:
#   ./task-worker.sh [--log-dir <path>] [--tasks-dir <path>]
#                    [--db <path>] [--model <name>]
#                    [--max-turns <N>] [--max-retries <N>] [--sandbox <path>]

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TASKS_DIR="$REPO_ROOT/.workflow/tasks"
DB_PATH="$REPO_ROOT/.workflow/flywheel.sqlite"
LOG_DIR="$REPO_ROOT/logs/worker"
SANDBOX="$REPO_ROOT"
LKG_DIR="$REPO_ROOT/.workflow/lkg"
PROMOTE_SCRIPT="$REPO_ROOT/.workflow/promote-lkg.sh"
MODEL=""
MAX_TURNS=500
MAX_RETRIES=1
SHUTDOWN=0

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --log-dir)     LOG_DIR="$2"; shift 2 ;;
    --log-dir=*)   LOG_DIR="${1#*=}"; shift ;;
    --tasks-dir)   TASKS_DIR="$2"; shift 2 ;;
    --tasks-dir=*) TASKS_DIR="${1#*=}"; shift ;;
    --db)          DB_PATH="$2"; shift 2 ;;
    --db=*)        DB_PATH="${1#*=}"; shift ;;
    --sandbox)     SANDBOX="$2"; shift 2 ;;
    --sandbox=*)   SANDBOX="${1#*=}"; shift ;;
    --model)       MODEL="$2"; shift 2 ;;
    --model=*)     MODEL="${1#*=}"; shift ;;
    --max-turns)   MAX_TURNS="$2"; shift 2 ;;
    --max-turns=*) MAX_TURNS="${1#*=}"; shift ;;
    --max-retries)   MAX_RETRIES="$2"; shift 2 ;;
    --max-retries=*) MAX_RETRIES="${1#*=}"; shift ;;
    -h|--help)
      cat <<EOF
Usage: task-worker.sh [options]

Autonomous worker that drives flywheel tasks from
.workflow/tasks/active/<phase>/*.json via \`python -m flywheel.workflow\`.

Options:
  --log-dir <path>       Per-task log directory      (default: logs/worker/)
  --tasks-dir <path>     Tasks root                  (default: .workflow/tasks)
  --db <path>            SQLite database             (default: .workflow/flywheel.sqlite)
  --sandbox <path>       Agent working directory     (default: repo root)
  --model <name>         Override Claude model       (default: SDK default)
  --max-turns <N>        SDK runaway backstop, not a per-task budget (default: 500)
  --max-retries <N>      Harness retry budget        (default: 1)
  -h, --help             Show this help
EOF
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------
for bin in git uv; do
  if ! command -v "$bin" &>/dev/null; then
    echo "ERROR: $bin not found in PATH." >&2
    exit 1
  fi
done

if ! mkdir -p "$LOG_DIR"; then
  echo "ERROR: Could not create log directory: $LOG_DIR" >&2
  exit 1
fi
mkdir -p "$(dirname "$DB_PATH")"

# ---------------------------------------------------------------------------
# LKG bootstrap
# ---------------------------------------------------------------------------
# The worker runs flywheel against a pinned snapshot at .workflow/lkg/ so that
# an in-flight task editing src/flywheel/ cannot break the next iteration.
# Promotion of live -> LKG is an explicit, named step (promote-lkg.sh). On
# first run we bootstrap from the live tree so existing checkouts keep working.
if [[ ! -f "$LKG_DIR/SOURCE_SHA" ]]; then
  if [[ ! -x "$PROMOTE_SCRIPT" ]]; then
    echo "ERROR: LKG snapshot missing and promote script not found at $PROMOTE_SCRIPT" >&2
    exit 1
  fi
  echo "[worker] No LKG snapshot found; bootstrapping from live tree." >&2
  "$PROMOTE_SCRIPT" --bootstrap
fi

# ---------------------------------------------------------------------------
# Signal handling
# ---------------------------------------------------------------------------
trap 'SHUTDOWN=1; echo "[worker] Shutdown requested, waiting for current task..." >&2' SIGINT SIGTERM

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
run_workflow() {
  # Wrap `python -m flywheel.workflow` so we route through uv consistently.
  # Routed through the pinned LKG snapshot so a mid-task edit to flywheel's
  # own source cannot break the next iteration; the live tree is what the
  # agent edits, the LKG is what the worker runs.
  uv run --project "$LKG_DIR" python -m flywheel.workflow "$@"
}

next_task_file() {
  # Exit 0 + path on stdout when a task is eligible; exit 1 otherwise.
  run_workflow next --tasks-dir "$TASKS_DIR" --db "$DB_PATH" 2>/dev/null
}

log_path() {
  local task_file="$1"
  local task_id
  task_id=$(basename "$task_file" .json)
  local commit
  commit=$(git -C "$REPO_ROOT" rev-parse --short=6 HEAD 2>/dev/null || echo "unknown")
  echo "${LOG_DIR}/${task_id}_${commit}_$(date +%Y%m%dT%H%M%S).log"
}

archive_completed() {
  # Print each archived phase to stderr for visibility.
  while IFS= read -r line; do
    [[ -n "$line" ]] && echo "[worker] Archived phase: $line" >&2
  done < <(run_workflow archive --tasks-dir "$TASKS_DIR" --db "$DB_PATH" 2>/dev/null)
}

# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------
LKG_SHA="$(cat "$LKG_DIR/SOURCE_SHA" 2>/dev/null || echo unknown)"
echo "[worker] Task worker started"      >&2
echo "[worker] Tasks dir : $TASKS_DIR"    >&2
echo "[worker] DB        : $DB_PATH"      >&2
echo "[worker] Log dir   : $LOG_DIR"      >&2
echo "[worker] Sandbox   : $SANDBOX"      >&2
echo "[worker] LKG       : $LKG_DIR ($LKG_SHA)" >&2
echo "[worker] PID       : $$"            >&2
echo ""                                    >&2

while true; do
  if [[ "$SHUTDOWN" -eq 1 ]]; then
    echo "[worker] Shutting down." >&2
    exit 0
  fi

  TASK_FILE=$(next_task_file || true)

  if [[ -z "$TASK_FILE" ]]; then
    archive_completed
    echo "[worker] No eligible tasks. Sleeping 60s..." >&2
    for (( i=0; i<60; i++ )); do
      if [[ "$SHUTDOWN" -eq 1 ]]; then
        echo "[worker] Shutting down." >&2
        exit 0
      fi
      sleep 1
    done
    continue
  fi

  LOGFILE=$(log_path "$TASK_FILE")
  echo "[worker] Selected task : $TASK_FILE" >&2
  echo "[worker] Log file      : $LOGFILE"   >&2
  echo "[worker] Start         : $(date -Iseconds)" >&2

  RUN_ARGS=(run "$TASK_FILE"
            --db "$DB_PATH"
            --sandbox "$SANDBOX"
            --max-turns "$MAX_TURNS"
            --max-retries "$MAX_RETRIES")
  if [[ -n "$MODEL" ]]; then
    RUN_ARGS+=(--model "$MODEL")
  fi

  set +e
  run_workflow "${RUN_ARGS[@]}" &> "$LOGFILE"
  EXIT_CODE=$?
  set -e

  echo "[worker] End           : $(date -Iseconds) (exit=$EXIT_CODE)" >&2

  if [[ "$SHUTDOWN" -eq 1 ]]; then
    echo "[worker] Shutting down." >&2
    exit 0
  fi

  if [[ "$EXIT_CODE" -eq 0 ]]; then
    echo "[worker] Task done. Selecting next task..." >&2
  else
    echo "[worker] Task did not reach DONE (exit=$EXIT_CODE). Will retry per eligibility rules." >&2
  fi
  archive_completed
  echo "" >&2
done
