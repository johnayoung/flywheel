#!/usr/bin/env bash
# task-worker.sh - Autonomous worker loop that drives flywheel tasks.
#
# Selects eligible tasks from .workflow/tasks/active/<phase>/ via
# `python -m flywheel.workflow next`, spawns each in its own git worktree
# at .workflow/worktrees/<task-id>/ on branch flywheel/<phase>/<task-id>,
# and fast-forward-merges the branch back into the worker's starting
# branch on lifecycle.status=done. Worktrees from failed, interrupted,
# or uncommitted-at-DONE runs are parked on disk for forensics; a
# startup sweep removes worktrees older than the retention window.
#
# Up to --max-parallel tasks run concurrently (default 1). The workflow
# subprocess runs in a background subshell; merges into the base branch
# stay serialized in the main loop so git never has two merges open at
# once on the same branch. A single-instance lock at .workflow/.worker.lock
# prevents two worker processes from racing on those merges.
#
# SQLite at .workflow/flywheel.sqlite is the source of truth for
# lifecycle completion; task JSON files are immutable.
#
# Usage:
#   ./task-worker.sh [--log-dir <path>] [--tasks-dir <path>]
#                    [--db <path>] [--model <name>]
#                    [--max-turns <N>] [--max-retries <N>]
#                    [--max-parallel <N>]
#                    [--worktree-retention-days <N>]

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TASKS_DIR="$REPO_ROOT/.workflow/tasks"
DB_PATH="$REPO_ROOT/.workflow/flywheel.sqlite"
LOG_DIR="$REPO_ROOT/logs/worker"
WORKTREES_DIR="$REPO_ROOT/.workflow/worktrees"
LOCK_FILE="$REPO_ROOT/.workflow/.worker.lock"
MODEL=""
MAX_TURNS=500
MAX_RETRIES=1
MAX_PARALLEL=1
RETENTION_DAYS=7
SHUTDOWN=0

# Circuit breaker: skip a task after this many consecutive worktree-creation
# failures so one broken task can't starve the loop. Keyed by task_id.
SPAWN_FAILURE_THRESHOLD=3
declare -A SPAWN_FAILURES=()

# In-flight tracking. Arrays are parallel (same index = same task).
# Background subshells run the workflow; the main loop owns all merges so
# only one merge into PHASE_BASE happens at a time.
declare -a RUNNING_PIDS=()
declare -a RUNNING_TASKS=()
declare -a RUNNING_PHASES=()
declare -a RUNNING_WORKTREES=()
declare -a RUNNING_LOGS=()

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
    --max-parallel)   MAX_PARALLEL="$2"; shift 2 ;;
    --max-parallel=*) MAX_PARALLEL="${1#*=}"; shift ;;
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
  --max-parallel <N>               In-flight tasks the worker drives concurrently
                                   (default: 1; merges into the base branch
                                   stay serialized in the parent process)  (default: 1)
  --worktree-retention-days <N>    Days a parked worktree may live before
                                   the startup sweep removes it           (default: 7)
  -h, --help                       Show this help

A single-instance lock is held at .workflow/.worker.lock for the duration
of the run. Stale locks (PID no longer alive) are reclaimed automatically;
if a healthy instance is already running the worker refuses to start.
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

if ! [[ "$MAX_PARALLEL" =~ ^[0-9]+$ ]] || [[ "$MAX_PARALLEL" -lt 1 ]]; then
  echo "ERROR: --max-parallel must be a positive integer, got: $MAX_PARALLEL" >&2
  exit 1
fi

if ! mkdir -p "$LOG_DIR"; then
  echo "ERROR: Could not create log directory: $LOG_DIR" >&2
  exit 1
fi
mkdir -p "$(dirname "$DB_PATH")"
mkdir -p "$WORKTREES_DIR"
mkdir -p "$(dirname "$LOCK_FILE")"

# Phase base: the branch the worker started on. Recorded once so each new
# worktree branches off its live tip at creation time (post-merge of any
# prior task in the same worker run).
PHASE_BASE="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)"
if [[ "$PHASE_BASE" == "HEAD" ]]; then
  echo "ERROR: worker started on detached HEAD; cannot resolve phase base branch." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Single-instance lock
# ---------------------------------------------------------------------------
# Prevents two workers from racing on PHASE_BASE merges. A stale lock (PID
# no longer alive) is reclaimed; a healthy lock causes the new instance to
# refuse to start so an operator can investigate.
acquire_lock() {
  if [[ -f "$LOCK_FILE" ]]; then
    local old_pid
    old_pid=$(cat "$LOCK_FILE" 2>/dev/null || echo "")
    if [[ -n "$old_pid" ]] && kill -0 "$old_pid" 2>/dev/null; then
      echo "ERROR: Another task-worker is running (PID $old_pid). Remove $LOCK_FILE if stale." >&2
      exit 1
    fi
    echo "[worker] Removing stale lock file (PID ${old_pid:-unknown} no longer running)" >&2
    rm -f "$LOCK_FILE"
  fi
  echo $$ > "$LOCK_FILE"
}

release_lock() {
  # Only release the lock if it still belongs to this process. Avoids a
  # parallel cleanup path stealing another instance's lock by accident.
  if [[ -f "$LOCK_FILE" ]] && [[ "$(cat "$LOCK_FILE" 2>/dev/null)" == "$$" ]]; then
    rm -f "$LOCK_FILE"
  fi
}

acquire_lock
trap 'release_lock' EXIT

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

commit_task_files() {
  # Worktrees branch from PHASE_BASE at HEAD, so untracked task JSON dropped
  # into active/ won't appear in the worktree until it's committed. Stage
  # only UNTRACKED files here; modified ones may be transient state during a
  # rebase/checkout and re-committing them could overwrite real work.
  local active_dir="$TASKS_DIR/active"
  [[ -d "$active_dir" ]] || return 0

  local untracked_list
  untracked_list=$(git -C "$REPO_ROOT" status --porcelain -- "$active_dir" \
    | awk '/^\?\? / {sub(/^\?\? /, ""); print}')
  [[ -z "$untracked_list" ]] && return 0

  while IFS= read -r path; do
    [[ -n "$path" ]] && git -C "$REPO_ROOT" add -- "$path"
  done <<< "$untracked_list"

  if git -C "$REPO_ROOT" diff --cached --quiet; then
    return 0
  fi
  git -C "$REPO_ROOT" commit -m "chore: stage task files for worker" >/dev/null
  echo "[worker] Committed new task files so worktrees can access them" >&2
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
  # Reuse a parked worktree+branch when both exist (retry of a prior
  # failed/interrupted attempt on the same task -- per-task workspace
  # isolation is the spec, so prior commits carry forward).
  # Recreate the worktree on an existing branch when only the branch
  # survives (retention sweep cleaned the directory but left the ref).
  # Bail when only the directory exists (no branch) -- that's an
  # operator-cleanup state, not something to clobber.
  local task_id="$1"
  local phase="$2"
  local worktree="$WORKTREES_DIR/$task_id"
  local branch="flywheel/$phase/$task_id"

  local worktree_present=0 branch_present=0
  [[ -d "$worktree" ]] && worktree_present=1
  if git -C "$REPO_ROOT" show-ref --verify --quiet "refs/heads/$branch"; then
    branch_present=1
  fi

  if [[ $worktree_present -eq 1 && $branch_present -eq 1 ]]; then
    if git -C "$REPO_ROOT" worktree list --porcelain \
         | grep -q "^worktree $worktree$"; then
      echo "[worker] Reusing parked worktree on $branch; prior commits carry forward." >&2
      return 0
    fi
    echo "[worker] ERROR: $worktree exists but is not a registered worktree; refusing to clobber." >&2
    return 1
  fi

  if [[ $worktree_present -eq 0 && $branch_present -eq 1 ]]; then
    echo "[worker] Recreating worktree on existing branch $branch (directory was removed; ref survived)." >&2
    git -C "$REPO_ROOT" worktree add "$worktree" "$branch"
    return $?
  fi

  if [[ $worktree_present -eq 1 && $branch_present -eq 0 ]]; then
    echo "[worker] ERROR: $worktree exists but no branch $branch; refusing to clobber. Remove the directory manually." >&2
    return 1
  fi

  git -C "$REPO_ROOT" worktree add "$worktree" -b "$branch" "$PHASE_BASE"
}

merge_worktree() {
  # Returns 0 if commits were merged into PHASE_BASE; non-zero if nothing
  # was merged (parked for forensics OR no-op because the agent did no work).
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

  # Zero-commit branch: agent claimed done but produced no commits. Nothing
  # to park forensically, so clean up -- but signal failure so an operator
  # notices that a "done" lifecycle row has no work behind it.
  local commit_count
  commit_count=$(git -C "$REPO_ROOT" rev-list --count "${PHASE_BASE}..${branch}")
  if [[ "$commit_count" -eq 0 ]]; then
    echo "[worker] WARNING: $task_id reached DONE with zero commits on $branch -- agent did no work; treating as task failure" >&2
    git -C "$REPO_ROOT" worktree remove "$worktree"
    git -C "$REPO_ROOT" branch -d "$branch"
    return 1
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

is_task_in_flight() {
  # Defense against the brief window between `next_task_file` picking a
  # task and the subshell writing its running lifecycle row -- without
  # this dedup, two slots could grab the same task.
  local task_id="$1"
  local t
  for t in "${RUNNING_TASKS[@]+"${RUNNING_TASKS[@]}"}"; do
    [[ "$t" == "$task_id" ]] && return 0
  done
  return 1
}

spawn_task() {
  # Launch one workflow run in a background subshell. The subshell exits
  # with the workflow's exit code so `wait $pid` propagates it. The merge
  # is deliberately NOT done here -- only the main loop merges into
  # PHASE_BASE so git never has two merges open at once.
  local task_file="$1"
  local task_id="$2"
  local phase="$3"
  local worktree="$4"
  local logfile="$5"

  local run_args=(run "$task_file"
                  --db "$DB_PATH"
                  --sandbox "$worktree"
                  --max-turns "$MAX_TURNS"
                  --max-retries "$MAX_RETRIES")
  if [[ -n "$MODEL" ]]; then
    run_args+=(--model "$MODEL")
  fi

  (
    set +e
    run_workflow "${run_args[@]}" &> "$logfile"
    exit $?
  ) &

  local pid=$!
  RUNNING_PIDS+=("$pid")
  RUNNING_TASKS+=("$task_id")
  RUNNING_PHASES+=("$phase")
  RUNNING_WORKTREES+=("$worktree")
  RUNNING_LOGS+=("$logfile")
}

remove_finished() {
  # Harvest finished subshells: read lifecycle, run merge_worktree (or
  # preserve), rebuild the in-flight arrays without the harvested entries.
  local new_pids=() new_tasks=() new_phases=() new_worktrees=() new_logs=()
  local i
  for i in "${!RUNNING_PIDS[@]}"; do
    local pid="${RUNNING_PIDS[$i]}"
    local task_id="${RUNNING_TASKS[$i]}"
    local phase="${RUNNING_PHASES[$i]}"
    local worktree="${RUNNING_WORKTREES[$i]}"

    if kill -0 "$pid" 2>/dev/null; then
      new_pids+=("$pid")
      new_tasks+=("$task_id")
      new_phases+=("$phase")
      new_worktrees+=("$worktree")
      new_logs+=("${RUNNING_LOGS[$i]}")
      continue
    fi

    set +e
    wait "$pid" 2>/dev/null
    local exit_code=$?
    set -e

    echo "[worker] End           : $task_id $(date -Iseconds) (exit=$exit_code)" >&2

    local status
    status=$(read_lifecycle_status "$task_id")
    echo "[worker] Lifecycle     : $task_id ${status:-unknown}" >&2

    case "$status" in
      done)
        if merge_worktree "$task_id" "$phase"; then
          echo "[worker] Merged flywheel/$phase/$task_id into $PHASE_BASE" >&2
        fi
        ;;
      failed|interrupted)
        echo "[worker] Lifecycle $status; worktree preserved at $worktree" >&2
        ;;
      *)
        echo "[worker] Unexpected lifecycle status '${status:-unknown}'; worktree preserved at $worktree" >&2
        ;;
    esac
  done

  RUNNING_PIDS=("${new_pids[@]+"${new_pids[@]}"}")
  RUNNING_TASKS=("${new_tasks[@]+"${new_tasks[@]}"}")
  RUNNING_PHASES=("${new_phases[@]+"${new_phases[@]}"}")
  RUNNING_WORKTREES=("${new_worktrees[@]+"${new_worktrees[@]}"}")
  RUNNING_LOGS=("${new_logs[@]+"${new_logs[@]}"}")
}

spawn_eligible() {
  # Fill open slots up to MAX_PARALLEL. Each iteration of the inner loop
  # picks a task via flywheel.workflow next, dedups against the in-flight
  # set, applies the spawn-failure circuit breaker, creates the worktree,
  # and launches a subshell.
  while [[ ${#RUNNING_PIDS[@]} -lt $MAX_PARALLEL ]]; do
    [[ "$SHUTDOWN" -eq 1 ]] && break

    local task_file
    task_file=$(next_task_file || true)
    [[ -z "$task_file" ]] && break

    local task_id phase
    task_id=$(basename "$task_file" .json)
    phase=$(phase_of_task_file "$task_file")

    if is_task_in_flight "$task_id"; then
      # next_task_file returned a task we've already spawned but whose
      # lifecycle row hasn't landed yet. Stop filling this iteration; the
      # outer poll loop will retry shortly.
      break
    fi

    if [[ "${SPAWN_FAILURES[$task_id]:-0}" -ge "$SPAWN_FAILURE_THRESHOLD" ]]; then
      echo "[worker] Skipping $task_id -- spawn failed $SPAWN_FAILURE_THRESHOLD times; check $WORKTREES_DIR/$task_id and refs/heads/flywheel/*/$task_id" >&2
      # next is deterministic -- the same skipped task will come back
      # until lifecycle changes or operator clears it. Stop here so we
      # don't busy-loop within one iteration.
      break
    fi

    local worktree="$WORKTREES_DIR/$task_id"
    local logfile
    logfile=$(log_path "$task_file")

    echo "[worker] Selected task : $task_file" >&2
    echo "[worker] Log file      : $logfile" >&2
    echo "[worker] Worktree      : $worktree on flywheel/$phase/$task_id (from $PHASE_BASE)" >&2
    echo "[worker] Start         : $(date -Iseconds)" >&2

    if ! create_worktree "$task_id" "$phase"; then
      SPAWN_FAILURES[$task_id]=$(( ${SPAWN_FAILURES[$task_id]:-0} + 1 ))
      if [[ "${SPAWN_FAILURES[$task_id]}" -ge "$SPAWN_FAILURE_THRESHOLD" ]]; then
        echo "[worker] ERROR: $task_id failed to spawn $SPAWN_FAILURE_THRESHOLD times -- giving up. Inspect $WORKTREES_DIR/$task_id and branch flywheel/$phase/$task_id, then clear manually." >&2
      else
        echo "[worker] Worktree creation failed for $task_id (attempt ${SPAWN_FAILURES[$task_id]}/$SPAWN_FAILURE_THRESHOLD); will retry next iteration." >&2
      fi
      break
    fi
    SPAWN_FAILURES[$task_id]=0

    spawn_task "$task_file" "$task_id" "$phase" "$worktree" "$logfile"
  done
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
echo "[worker] Parallel  : $MAX_PARALLEL"       >&2
echo "[worker] PID       : $$"                  >&2
echo ""                                         >&2

retention_sweep

# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------
# Outer iteration does the expensive things (commit_task_files, archive,
# spawn). Inner poll loop reaps finished subshells at 1s granularity so
# slots refill quickly without spawning a Python process every second.
while [[ "$SHUTDOWN" -eq 0 ]]; do
  commit_task_files
  archive_completed
  remove_finished
  spawn_eligible

  prior_count=${#RUNNING_PIDS[@]}
  poll_budget=5
  [[ "$prior_count" -eq 0 ]] && poll_budget=60

  for (( i=0; i<poll_budget; i++ )); do
    [[ "$SHUTDOWN" -eq 1 ]] && break
    sleep 1
    if [[ ${#RUNNING_PIDS[@]} -gt 0 ]]; then
      remove_finished
      # If a slot freed up, break to the outer iteration to refill it.
      [[ ${#RUNNING_PIDS[@]} -lt $prior_count ]] && break
    fi
  done
done

# ---------------------------------------------------------------------------
# Shutdown: stop spawning, let in-flight tasks drain naturally so their
# worktrees end up in the same parked-on-disk state as a normal interrupt.
# Operators wanting faster shutdown can SIGKILL the worker.
# ---------------------------------------------------------------------------
if [[ ${#RUNNING_PIDS[@]} -gt 0 ]]; then
  echo "[worker] Shutdown requested -- waiting for ${#RUNNING_PIDS[@]} in-flight task(s) to finish..." >&2
  for pid in "${RUNNING_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  remove_finished
fi
echo "[worker] Shutting down." >&2
