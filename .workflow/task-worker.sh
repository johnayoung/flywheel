#!/usr/bin/env bash
# task-worker.sh - Autonomous worker loop that drives flywheel tasks.
#
# Selects the next eligible task from .workflow/tasks/active/<phase>/ via
# `python -m flywheel.workflow next`, runs it inside its own git worktree
# at .workflow/worktrees/<task-id>/ on branch flywheel/<phase>/<task-id>,
# and fast-forward-merges the branch back into the worker's starting
# branch on lifecycle.status=done. Worktrees from failed, interrupted,
# or uncommitted-at-DONE runs are parked on disk for forensics; a
# startup sweep removes worktrees older than the retention window.
#
# SQLite at .workflow/flywheel.sqlite is the source of truth for
# lifecycle completion; task JSON files are immutable.
#
# Usage:
#   ./task-worker.sh [--log-dir <path>] [--tasks-dir <path>]
#                    [--db <path>] [--model <name>]
#                    [--max-turns <N>] [--max-retries <N>]
#                    [--worktree-retention-days <N>]

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TASKS_DIR="$REPO_ROOT/.workflow/tasks"
DB_PATH="$REPO_ROOT/.workflow/flywheel.sqlite"
LOG_DIR="$REPO_ROOT/logs/worker"
WORKTREES_DIR="$REPO_ROOT/.workflow/worktrees"
MODEL=""
MAX_TURNS=500
MAX_RETRIES=1
RETENTION_DAYS=7
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
    --model)       MODEL="$2"; shift 2 ;;
    --model=*)     MODEL="${1#*=}"; shift ;;
    --max-turns)   MAX_TURNS="$2"; shift 2 ;;
    --max-turns=*) MAX_TURNS="${1#*=}"; shift ;;
    --max-retries)   MAX_RETRIES="$2"; shift 2 ;;
    --max-retries=*) MAX_RETRIES="${1#*=}"; shift ;;
    --worktree-retention-days)   RETENTION_DAYS="$2"; shift 2 ;;
    --worktree-retention-days=*) RETENTION_DAYS="${1#*=}"; shift ;;
    -h|--help)
      cat <<EOF
Usage: task-worker.sh [options]

Autonomous worker that drives flywheel tasks from
.workflow/tasks/active/<phase>/*.json via \`python -m flywheel.workflow\`.

Each task runs in its own git worktree at .workflow/worktrees/<task-id>/
on branch flywheel/<phase>/<task-id>, branched from the live tip of the
worker's starting branch. On lifecycle.status=done the task branch is
FF-only merged back into that branch and the worktree is removed.
Worktrees from failed, interrupted, or uncommitted-at-DONE runs are
preserved on disk; the startup retention sweep removes worktrees older
than --worktree-retention-days days (default 7) and deletes their
matching flywheel/<phase>/<task-id> branches.

Options:
  --log-dir <path>                 Per-task log directory      (default: logs/worker/)
  --tasks-dir <path>               Tasks root                  (default: .workflow/tasks)
  --db <path>                      SQLite database             (default: .workflow/flywheel.sqlite)
  --model <name>                   Override Claude model       (default: SDK default)
  --max-turns <N>                  SDK runaway backstop, not a per-task budget (default: 500)
  --max-retries <N>                Harness retry budget        (default: 1)
  --worktree-retention-days <N>    Days a parked worktree may live before
                                   the startup sweep removes it           (default: 7)
  -h, --help                       Show this help
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
for bin in git uv sqlite3; do
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
mkdir -p "$WORKTREES_DIR"

# Phase base: the branch the worker started on. Recorded once so each new
# worktree branches off its live tip at creation time (post-merge of any
# prior task in the same worker run).
PHASE_BASE="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)"
if [[ "$PHASE_BASE" == "HEAD" ]]; then
  echo "ERROR: worker started on detached HEAD; cannot resolve phase base branch." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Signal handling
# ---------------------------------------------------------------------------
trap 'SHUTDOWN=1; echo "[worker] Shutdown requested, waiting for current task..." >&2' SIGINT SIGTERM

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
run_workflow() {
  uv run python -m flywheel.workflow "$@"
}

next_task_file() {
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
  while IFS= read -r line; do
    [[ -n "$line" ]] && echo "[worker] Archived phase: $line" >&2
  done < <(run_workflow archive --tasks-dir "$TASKS_DIR" --db "$DB_PATH" 2>/dev/null)
}

phase_of_task_file() {
  # Phase is the directory name under .workflow/tasks/active/. Tasks that
  # live directly under active/ fall back to "_root".
  local task_file="$1"
  local active_root="$TASKS_DIR/active"
  local rel="${task_file#"$active_root"/}"
  if [[ "$rel" == "$task_file" ]]; then
    echo "_root"
    return
  fi
  local parent
  parent="$(dirname "$rel")"
  if [[ "$parent" == "." ]]; then
    echo "_root"
  else
    echo "$parent"
  fi
}

read_lifecycle_status() {
  # Most recent lifecycle row for the given task_id, by updated_at.
  local task_id="$1"
  sqlite3 "$DB_PATH" \
    "SELECT status FROM lifecycles WHERE task_id = '${task_id}' ORDER BY updated_at DESC LIMIT 1;" \
    2>/dev/null || true
}

create_worktree() {
  local task_id="$1"
  local phase="$2"
  local worktree="$WORKTREES_DIR/$task_id"
  local branch="flywheel/$phase/$task_id"
  git -C "$REPO_ROOT" worktree add "$worktree" -b "$branch" "$PHASE_BASE"
}

merge_worktree() {
  # Returns 0 if the worktree was cleaned up, non-zero if it was parked.
  local task_id="$1"
  local phase="$2"
  local worktree="$WORKTREES_DIR/$task_id"
  local branch="flywheel/$phase/$task_id"

  # Uncommitted-changes check (untracked files count -- porcelain default).
  local porcelain
  porcelain=$(git -C "$worktree" status --porcelain)
  if [[ -n "$porcelain" ]]; then
    echo "[worker] DONE with uncommitted changes on $branch; parking worktree at $worktree" >&2
    return 1
  fi

  # Zero-commit branch: no-op merge. Remove worktree, delete branch, leave
  # phase base where it was.
  local commit_count
  commit_count=$(git -C "$REPO_ROOT" rev-list --count "${PHASE_BASE}..${branch}")
  if [[ "$commit_count" -eq 0 ]]; then
    echo "[worker] no commits to merge on $branch" >&2
    git -C "$REPO_ROOT" worktree remove "$worktree"
    git -C "$REPO_ROOT" branch -d "$branch"
    return 0
  fi

  # Fast-forward merge attempt.
  if git -C "$REPO_ROOT" merge --ff-only "$branch"; then
    git -C "$REPO_ROOT" worktree remove "$worktree"
    git -C "$REPO_ROOT" branch -d "$branch"
    return 0
  fi

  # FF failed (phase base advanced). Rebase task branch onto current base
  # once, then retry FF. On rebase failure, abort and park the worktree.
  echo "[worker] FF failed for $branch; rebasing onto $PHASE_BASE" >&2
  if ! git -C "$worktree" rebase "$PHASE_BASE"; then
    git -C "$worktree" rebase --abort 2>/dev/null || true
    echo "[worker] rebase failed for $branch; parking worktree at $worktree" >&2
    return 1
  fi
  if git -C "$REPO_ROOT" merge --ff-only "$branch"; then
    git -C "$REPO_ROOT" worktree remove "$worktree"
    git -C "$REPO_ROOT" branch -d "$branch"
    return 0
  fi
  echo "[worker] post-rebase FF failed for $branch; parking worktree at $worktree" >&2
  return 1
}

retention_sweep() {
  # Clean dangling worktree entries first so directories an operator
  # removed by hand stop blocking future `git worktree add` calls.
  git -C "$REPO_ROOT" worktree prune
  local now cutoff
  now=$(date +%s)
  cutoff=$(( now - RETENTION_DAYS * 86400 ))
  shopt -s nullglob
  for wt in "$WORKTREES_DIR"/*; do
    [[ -d "$wt" ]] || continue
    local mt task_id
    mt=$(stat -c %Y "$wt" 2>/dev/null || stat -f %m "$wt" 2>/dev/null || echo 0)
    if (( mt < cutoff )); then
      task_id=$(basename "$wt")
      echo "[worker] Sweep: removing worktree $wt (age > ${RETENTION_DAYS}d)" >&2
      git -C "$REPO_ROOT" worktree remove --force "$wt" 2>/dev/null || rm -rf "$wt"
      while IFS= read -r br; do
        [[ -n "$br" ]] || continue
        git -C "$REPO_ROOT" branch -D "$br" 2>/dev/null || true
      done < <(git -C "$REPO_ROOT" for-each-ref --format='%(refname:short)' "refs/heads/flywheel/*/${task_id}")
    fi
  done
  shopt -u nullglob
}

# ---------------------------------------------------------------------------
# Startup
# ---------------------------------------------------------------------------
echo "[worker] Task worker started"            >&2
echo "[worker] Tasks dir : $TASKS_DIR"          >&2
echo "[worker] DB        : $DB_PATH"            >&2
echo "[worker] Log dir   : $LOG_DIR"            >&2
echo "[worker] Worktrees : $WORKTREES_DIR"      >&2
echo "[worker] Base      : $PHASE_BASE"         >&2
echo "[worker] Retention : ${RETENTION_DAYS}d"  >&2
echo "[worker] PID       : $$"                  >&2
echo ""                                         >&2

retention_sweep

# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------
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

  TASK_ID=$(basename "$TASK_FILE" .json)
  PHASE=$(phase_of_task_file "$TASK_FILE")
  WORKTREE="$WORKTREES_DIR/$TASK_ID"
  BRANCH="flywheel/$PHASE/$TASK_ID"

  LOGFILE=$(log_path "$TASK_FILE")
  echo "[worker] Selected task : $TASK_FILE"                          >&2
  echo "[worker] Log file      : $LOGFILE"                            >&2
  echo "[worker] Worktree      : $WORKTREE on $BRANCH (from $PHASE_BASE)" >&2
  echo "[worker] Start         : $(date -Iseconds)"                   >&2

  # FR-10: worktree-creation failure exits the wrapper without creating a
  # lifecycle row. The worker's eligibility loop reselects on next iter.
  if ! create_worktree "$TASK_ID" "$PHASE"; then
    echo "[worker] Worktree creation failed for $TASK_ID; will retry next iteration." >&2
    echo "" >&2
    sleep 5
    continue
  fi

  RUN_ARGS=(run "$TASK_FILE"
            --db "$DB_PATH"
            --sandbox "$WORKTREE"
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
    echo "[worker] Shutting down (worktree $WORKTREE preserved)." >&2
    exit 0
  fi

  STATUS=$(read_lifecycle_status "$TASK_ID")
  echo "[worker] Lifecycle     : ${STATUS:-unknown}" >&2

  case "$STATUS" in
    done)
      if merge_worktree "$TASK_ID" "$PHASE"; then
        echo "[worker] Merged $BRANCH into $PHASE_BASE" >&2
      fi
      ;;
    failed|interrupted)
      echo "[worker] Lifecycle $STATUS; worktree preserved at $WORKTREE" >&2
      ;;
    *)
      echo "[worker] Unexpected lifecycle status '${STATUS:-unknown}'; worktree preserved at $WORKTREE" >&2
      ;;
  esac

  archive_completed
  echo "" >&2
done
