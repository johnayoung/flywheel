"""Cross-task orchestrator — the consumer-side scheduler (P4).

``docs/strategy.md`` is explicit that *strategy lives in the consumer, not
the loop*: the harness owns a single task's lifecycle; deciding **which**
task runs next is a layer above it. This module is that layer. It only
*reads* authoritative lifecycle state and *decides what to run next*; it
never calls ``transition_to`` and holds no special harness privilege.

It replaces the poll loops a shell driver (``.workflow/task-worker.sh``)
otherwise runs — repeatedly shelling ``workflow next`` / ``workflow run`` /
``workflow recheck-blocked`` — with one in-process driver that re-evaluates
after each run it drives. Single-worker: the orchestrator *is* the worker,
so "react to a completion" is simply re-deriving eligibility once the run
it launched returns. (Multi-worker coordination is P5.)

Scheduling, reusing the exact predicates the pull-based CLI already uses
(:func:`flywheel.workflow.select_next_task` /
:func:`~flywheel.workflow.build_status_rows`):

* **Prerequisite promotion** — a fresh/retryable task runs only once every
  task in its ``prerequisites`` has a ``DONE`` lifecycle.
* **Reactive unblocking** — a blocked-interrupted lifecycle (``INTERRUPTED``
  with a persisted ``blocked_requires`` snapshot) is re-evaluated via
  :func:`flywheel.harness.recheck_blocked_lifecycle`; when its predicates
  now hold it is unblocked and **resumed on its own run_id** (continuing its
  history), not re-run from scratch. Blocked tasks are excluded from fresh
  selection so they are never wastefully re-run while still blocked.

Authoritative-read discipline: every decision is made from the persisted
projection, so the worst failure mode is wasted latency, never a wrong
schedule. Termination is guaranteed by two per-session guards — at most one
fresh run per task id and at most one resume per run id — so a perpetually
failing or re-blocking task cannot spin the loop.

Sandboxing: each task runs in ``sandbox_root/<task-id>``. Real
worktree/branch/merge mechanics remain a consumer "submit" concern layered
on top (out of scope here); this module owns selection and execution order.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal, TextIO

from flywheel.harness import InvokeFunc, recheck_blocked_lifecycle
from flywheel.lifecycle import Status
from flywheel.store_sqlite import SqliteStore
from flywheel.task import Task
from flywheel.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    TaskStatusRow,
    build_status_rows,
    recover_stranded_lifecycles,
    run_task_file,
    select_next_task,
)


@dataclass(frozen=True, kw_only=True)
class RunRecord:
    """One task execution the orchestrator drove, in launch order."""

    task_id: str
    run_id: str
    status: Status
    mode: Literal["fresh", "resume"]


@dataclass(frozen=True, kw_only=True)
class OrchestratorReport:
    """Outcome of an :func:`orchestrate` session.

    ``recovered`` lists run ids finalized from a stranded
    ``running``/``validating`` state at entry; ``runs`` lists every task
    execution the orchestrator launched, in order.
    """

    recovered: tuple[str, ...]
    runs: tuple[RunRecord, ...]


def _is_blocked_interrupted(row: TaskStatusRow) -> bool:
    """A lifecycle parked on a structured block (vs a bare SIGINT pause).

    Only these are routed through reactive recheck/resume; they are kept
    out of fresh selection so an unsatisfied block is never re-run blindly.
    """
    return (
        row.latest_status == Status.INTERRUPTED
        and row.blocked_requires is not None
    )


async def orchestrate(
    *,
    tasks_dir: Path,
    db_path: Path,
    sandbox_root: Path,
    invoke: InvokeFunc | None = None,
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
    max_retries: int = DEFAULT_MAX_RETRIES,
    stream: TextIO | None = None,
) -> OrchestratorReport:
    """Drive every eligible task in ``tasks_dir`` to quiescence.

    Returns once nothing remains to do: no blocked lifecycle can be
    unblocked and no fresh task is eligible. ``invoke`` defaults to the real
    Claude Code invoker (via :func:`flywheel.workflow.run_task_file`); tests
    inject a fake callable.
    """
    db_path.parent.mkdir(parents=True, exist_ok=True)
    sandbox_root.mkdir(parents=True, exist_ok=True)

    control = SqliteStore(db_path)
    try:
        recovered = tuple(recover_stranded_lifecycles(control))
        runs: list[RunRecord] = []
        attempted_fresh: set[str] = set()
        attempted_resume: set[str] = set()

        while True:
            rows = build_status_rows(tasks_dir, control)
            task_by_id: dict[str, Task] = {r.task.id: r.task for r in rows}
            file_by_id: dict[str, Path] = {
                r.task.id: r.task_file for r in rows
            }

            # 1. Reactive unblock + resume. Re-evaluate each blocked
            #    lifecycle's predicates against the current workspace; the
            #    first one that unblocks is resumed on its own run_id and we
            #    restart the loop so the freshly-changed state is re-read.
            resumed_this_pass = False
            for row in rows:
                if not _is_blocked_interrupted(row):
                    continue
                run_id = row.latest_run_id
                if run_id is None or run_id in attempted_resume:
                    continue
                outcome = recheck_blocked_lifecycle(
                    control, run_id, task_by_id[row.task.id]
                )
                if not outcome.applied:
                    continue
                attempted_resume.add(run_id)
                record = await _drive(
                    file_by_id[row.task.id],
                    db_path=db_path,
                    sandbox_root=sandbox_root,
                    task_id=row.task.id,
                    run_id=run_id,
                    invoke=invoke,
                    model=model,
                    max_turns=max_turns,
                    max_retries=max_retries,
                    stream=stream,
                )
                runs.append(record)
                resumed_this_pass = True
                break
            if resumed_this_pass:
                continue

            # 2. Fresh selection over the prerequisite graph. Exclude
            #    still-blocked lifecycles (handled by the recheck path) and
            #    tasks already run this session (the termination guard) from
            #    *candidacy* -- but keep the full row set so an excluded task
            #    can still satisfy a dependent's prerequisite (a DONE
            #    dependency is excluded by attempted_fresh yet must remain
            #    visible to promote its dependents).
            exclude_ids = frozenset(
                {row.task.id for row in rows if _is_blocked_interrupted(row)}
                | attempted_fresh
            )
            pick = select_next_task(rows, exclude_ids=exclude_ids)
            if pick is None:
                break
            attempted_fresh.add(pick.task.id)
            record = await _drive(
                pick.task_file,
                db_path=db_path,
                sandbox_root=sandbox_root,
                task_id=pick.task.id,
                run_id=None,
                invoke=invoke,
                model=model,
                max_turns=max_turns,
                max_retries=max_retries,
                stream=stream,
            )
            runs.append(record)

        return OrchestratorReport(recovered=recovered, runs=tuple(runs))
    finally:
        control.close()


async def _drive(
    task_file: Path,
    *,
    db_path: Path,
    sandbox_root: Path,
    task_id: str,
    run_id: str | None,
    invoke: InvokeFunc | None,
    model: str | None,
    max_turns: int,
    max_retries: int,
    stream: TextIO | None,
) -> RunRecord:
    """Run (or resume) one task in its own sandbox and snapshot the result.

    ``run_task_file`` opens and closes its own store, so its writes are
    committed and visible to the orchestrator's control store on the next
    query (separate SQLite connections under WAL).
    """
    outcome = await run_task_file(
        task_file,
        db_path=db_path,
        sandbox=sandbox_root / task_id,
        model=model,
        max_turns=max_turns,
        max_retries=max_retries,
        invoke=invoke,
        stream=stream,
        run_id=run_id,
    )
    return RunRecord(
        task_id=task_id,
        run_id=outcome.lifecycle.run_id,
        status=outcome.lifecycle.status,
        mode="resume" if run_id is not None else "fresh",
    )


__all__ = ["OrchestratorReport", "RunRecord", "orchestrate"]
