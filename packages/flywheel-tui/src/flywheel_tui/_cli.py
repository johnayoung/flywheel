"""``flywheel-tui`` CLI entry point.

Bare ``flywheel-tui`` opens the Textual dashboard against the store the
orchestrator's policy resolves to; ``flywheel-tui --json`` (or any
invocation whose stdout is not a TTY) prints one machine-readable
:class:`DashboardSnapshot` and exits 0 with no ANSI sequences — matches
Claude Code's interactive-default + print-mode pattern.

Store path resolution reuses
:func:`flywheel_orchestrator.resolve_db_path` and
:func:`flywheel_orchestrator.load_effective_policy` so the precedence is
identical to ``flywheel-orchestrate live``/``status``; the work source
(used only for the summary header's task-state counts) mirrors the same
explicit ``--tasks-dir`` -> policy -> default fallback chain.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path

from flywheel.lifecycle import Status
from flywheel.store_sqlite import SqliteStore
from flywheel.task import ManualGrader
from flywheel_orchestrator import (
    DEFAULT_POLICY_FILENAME,
    DEFAULT_TASKS_DIR,
    DirectoryWorkSource,
    PolicyError,
    WorkPolicy,
    WorkSource,
    build_work_source,
    load_effective_policy,
    resolve_db_path,
)

from flywheel_tui._dashboard import (
    DEFAULT_POLL_INTERVAL_SECONDS,
    DashboardApp,
)
from flywheel_tui._session import TranscriptEntry, TranscriptTailer
from flywheel_tui._session_screen import SessionScreen, SessionStatus
from flywheel_tui._snapshot import (
    DashboardSnapshot,
    build_snapshot,
    snapshot_to_dict,
)


def main(argv: Sequence[str] | None = None) -> int:
    """Resolve the store, then dispatch to the TUI or the JSON snapshot.

    Returns 0 on a clean exit, 2 when the resolved store does not exist
    (the operator-facing remedy is ``flywheel-orchestrate init``).
    Snapshot mode auto-engages when stdout is not a TTY so the command
    is pipe-safe by default.
    """
    args = _build_parser().parse_args(argv)
    try:
        policy = load_effective_policy(args.policy)
    except PolicyError as exc:
        print(f"flywheel-tui: policy error: {exc}", file=sys.stderr)
        return 2
    db_path = resolve_db_path(args.db, policy=policy)
    if not db_path.exists():
        return _emit_missing_store(db_path)

    work_source = _resolve_work_source(args.tasks_dir, policy)
    json_mode = args.json or not sys.stdout.isatty()

    if json_mode:
        return _run_snapshot(db_path, work_source)
    return _run_dashboard(db_path, work_source)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="flywheel-tui",
        description=(
            "Interactive realtime dashboard of in-flight flywheel runs. "
            "Bare invocation opens the Textual app; --json (or a "
            "non-TTY stdout) prints one snapshot and exits."
        ),
    )
    parser.add_argument(
        "--db",
        default=None,
        help=(
            "Store path. Defaults to the policy's [paths] db (else "
            ".flywheel/flywheel.sqlite). Matches flywheel-orchestrate."
        ),
    )
    parser.add_argument(
        "--policy",
        default=None,
        help=(
            f"Work-policy file selecting the work source (default: "
            f"{DEFAULT_POLICY_FILENAME} if present)."
        ),
    )
    parser.add_argument(
        "--tasks-dir",
        default=None,
        help=(
            f"Tasks root directory for the summary's task-state counts "
            f"(default: policy source, else {DEFAULT_TASKS_DIR})."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help=(
            "Print one JSON snapshot of the dashboard data and exit. "
            "Implied when stdout is not a TTY."
        ),
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=DEFAULT_POLL_INTERVAL_SECONDS,
        help=(
            f"Seconds between dashboard polls (default: "
            f"{DEFAULT_POLL_INTERVAL_SECONDS})."
        ),
    )
    return parser


def _resolve_work_source(
    tasks_dir_arg: str | None, policy: WorkPolicy | None
) -> WorkSource:
    """Mirror ``flywheel-orchestrate``'s work-source precedence.

    Explicit ``--tasks-dir`` always selects the directory source (so the
    summary counts can be pinned at the CLI), else the policy decides,
    else the default ``.flywheel/tasks`` directory.
    """
    if tasks_dir_arg:
        return DirectoryWorkSource(Path(tasks_dir_arg))
    if policy is not None:
        return build_work_source(policy)
    return DirectoryWorkSource(DEFAULT_TASKS_DIR)


def _emit_missing_store(db_path: Path) -> int:
    print(
        f"flywheel-tui: no store found at {db_path}",
        file=sys.stderr,
    )
    print(
        "  run 'flywheel-orchestrate init' to scaffold .flywheel/ "
        "and the work policy, then start the orchestrator to populate "
        "the store.",
        file=sys.stderr,
    )
    return 2


def _run_snapshot(db_path: Path, work_source: WorkSource) -> int:
    """Build one snapshot, print it as JSON, exit 0.

    Reads the store once, closes it, prints the payload. No ANSI escape
    sequences ever touch stdout in this mode.
    """
    now = datetime.now(timezone.utc)
    store = SqliteStore(db_path)
    try:
        snapshot = build_snapshot(
            store,
            work_source=work_source,
            now=now,
            started_at=now,
        )
    finally:
        store.close()
    print(json.dumps(snapshot_to_dict(snapshot), indent=2))
    return 0


def _lookup_awaiting_instruction(
    store: SqliteStore,
    *,
    run_id: str,
    status: Status,
    awaiting_ordinal: int | None,
) -> str | None:
    """Resolve the pending manual gate's instruction for a parked run.

    Local mirror of the awaiting-instruction lookup
    :func:`flywheel_orchestrator.collect_live_rows` performs for the
    dashboard row: returns ``None`` unless the lifecycle is parked at
    :class:`flywheel.lifecycle.Status.AWAITING_APPROVAL` with a valid
    ordinal that addresses a :class:`flywheel.task.ManualGrader` on
    the task definition pinned to the run. A stale ordinal or
    archived task degrades to ``None`` so the session screen still
    renders -- the banner just won't carry an instruction.
    """

    if status != Status.AWAITING_APPROVAL:
        return None
    if not isinstance(awaiting_ordinal, int):
        return None
    task = store.load_task_for_run(run_id)
    if task is None:
        return None
    if awaiting_ordinal < 0 or awaiting_ordinal >= len(task.graders):
        return None
    grader = task.graders[awaiting_ordinal]
    if not isinstance(grader, ManualGrader):
        return None
    return grader.instruction


def _run_dashboard(db_path: Path, work_source: WorkSource) -> int:
    """Open the Textual dashboard, polling ``db_path`` until the operator quits."""
    started_at = datetime.now(timezone.utc)
    store = SqliteStore(db_path)

    def poll() -> DashboardSnapshot:
        return build_snapshot(
            store,
            work_source=work_source,
            now=datetime.now(timezone.utc),
            started_at=started_at,
        )

    def open_session(run_id: str, task_id: str) -> SessionScreen | None:
        """Construct the per-run session screen on Enter.

        A fresh :class:`TranscriptTailer` per push means re-opening a
        run rewinds the cursor and re-renders the recent window from
        the top; the alternative (one global tailer per run cached
        across pushes) would skip records the operator already saw.
        Status is sampled directly from the shared store handle so the
        awaiting-gate instruction and terminal-state banner stay live
        for the duration of the screen.
        """

        tailer = TranscriptTailer(store, run_id)

        def fetch() -> list[TranscriptEntry]:
            return tailer.fetch()

        def status() -> SessionStatus:
            lifecycle = store.load_lifecycle(run_id)
            if lifecycle is None:
                return SessionStatus(
                    status=None, awaiting_instruction=None, missing=True
                )
            return SessionStatus(
                status=lifecycle.status,
                awaiting_instruction=_lookup_awaiting_instruction(
                    store,
                    run_id=run_id,
                    status=lifecycle.status,
                    awaiting_ordinal=lifecycle.awaiting_manual_ordinal,
                ),
            )

        return SessionScreen(
            run_id=run_id,
            task_id=task_id,
            fetch=fetch,
            status=status,
        )

    app = DashboardApp(poll=poll, open_session=open_session)
    try:
        rc = app.run()
    finally:
        store.close()
    return rc if isinstance(rc, int) else 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
