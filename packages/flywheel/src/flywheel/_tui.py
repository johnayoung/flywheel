"""Operator-console entry point for the ``fw`` / ``flywheel`` shell.

Bare ``fw`` opens the Textual dashboard against the store the
orchestrator's policy resolves to; ``fw --json`` (or any invocation
whose stdout is not a TTY) prints one machine-readable
:class:`DashboardSnapshot` and exits 0 with no ANSI sequences -- matches
Claude Code's interactive-default + print-mode pattern. Lives in
:mod:`flywheel` (formerly :mod:`flywheel_tui._cli`) so the console
and the verb router ship under one distribution.

Store path resolution reuses :func:`flywheel_orchestrator.resolve_db_path`
and :func:`flywheel_orchestrator.load_effective_policy` so the precedence
is identical to ``fw status`` / ``fw live``; the work source (used by
both the summary header's task-state counts and the ``/archive`` slash
command) mirrors the same explicit ``--tasks-dir`` -> policy -> default
fallback chain.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any

from flywheel_core.lifecycle import Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.task import ManualGrader

if TYPE_CHECKING:
    # Optional postgres backend, typing-only so this module never hard-depends
    # on the psycopg extra. The store factory returns SqliteStore |
    # PostgresStore and both answer these reads through the store protocol.
    from flywheel_core.store_postgres import PostgresStore
from flywheel_orchestrator import (
    DEFAULT_POLICY_FILENAME,
    DEFAULT_TASKS_DIR,
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    DirectoryWorkSource,
    PolicyError,
    StoreConfigError,
    SupervisionBudget,
    SupervisionPolicy,
    WorkPolicy,
    WorkSource,
    HistoryRow,
    archive_completed_phases,
    build_claim_store,
    build_task_phase_index,
    build_work_source,
    collect_history_rows,
    load_effective_policy,
    open_sqlite_bound_store,
    repo_root_for_tasks_dir,
    resolve_db_path,
    resolve_postgres_dsn,
)

from flywheel._dashboard import (
    DEFAULT_POLL_INTERVAL_SECONDS,
    DashboardApp,
)
from flywheel._history_screen import HistoryScreen
from flywheel._session import TranscriptEntry, TranscriptTailer
from flywheel._session_screen import SessionScreen, SessionStatus
from flywheel._snapshot import (
    DashboardSnapshot,
    build_snapshot,
    snapshot_to_dict,
)
from flywheel._autopilot_supervisor import AutopilotSupervisor
from flywheel._worker_supervisor import WorkerSupervisor

# Shared crash-loop budget for the two console-supervised daemons (spec 00070).
# The worker and the autopilot enforce the *same* budget -- a handful of
# respawns inside a rolling window -- so a transient death self-heals while a
# persistent boot-loop is contained before it can spin (for the autopilot, that
# caps unattended writes to the operator's base branch). Each supervisor is
# handed its own ``SupervisionPolicy`` built from this one budget so their
# windows decay independently while the policy they enforce stays identical.
_SUPERVISION_BUDGET = SupervisionBudget(max_respawns=5, window_seconds=300.0)


def main(argv: Sequence[str] | None = None) -> int:
    """Resolve the store, then dispatch to the TUI or the JSON snapshot.

    Returns 0 on a clean exit, 2 when the resolved store is missing.
    The missing-store check is backend-aware (spec 00024 FR-11): for
    sqlite (or no policy) it is file existence at ``db_path`` with the
    ``fw init`` remedy; for postgres it is DSN env var presence
    (``FLYWHEEL_PG_DSN`` / ``DATABASE_URL``), since no store file ever
    exists for that backend. Snapshot mode auto-engages when stdout is
    not a TTY so the command is pipe-safe by default.
    """
    args = _build_parser().parse_args(argv)
    try:
        policy = load_effective_policy(args.policy)
    except PolicyError as exc:
        print(f"fw: policy error: {exc}", file=sys.stderr)
        return 2
    db_path = resolve_db_path(args.db, policy=policy)
    if policy is not None and policy.store_backend == "postgres":
        if resolve_postgres_dsn() is None:
            return _emit_missing_dsn()
    elif not db_path.exists():
        return _emit_missing_store(db_path)

    work_source = _resolve_work_source(args.tasks_dir, policy)
    archive_tasks_dir = _resolve_archive_tasks_dir(args.tasks_dir, policy)
    json_mode = args.json or not sys.stdout.isatty()

    try:
        if json_mode:
            return _run_snapshot(db_path, work_source, policy=policy)
        tasks_dir = _resolve_tasks_dir_for_worker(args.tasks_dir, policy)
        model = _resolve_model_for_worker(args.model, policy)
        return _run_dashboard(
            db_path,
            work_source,
            archive_tasks_dir,
            tasks_dir=tasks_dir,
            model=model,
            no_worker=args.no_worker,
            policy=policy,
        )
    except StoreConfigError as exc:
        # The factory's fail-fast postgres errors (and the sqlite-bound
        # refusal) carry their own remedy; exit 2 like every other
        # configuration error.
        print(f"fw: {exc}", file=sys.stderr)
        return 2


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fw",
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
            ".flywheel/flywheel.sqlite). Matches fw status / fw live."
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
            f"and /archive (default: policy source, else {DEFAULT_TASKS_DIR})."
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
    parser.add_argument(
        "--model",
        default=None,
        help=(
            "Agent model id forwarded to the spawned worker as "
            "--model. Overrides the [agent] model setting in "
            "flywheel.toml; absent both, the SDK uses the Claude Code "
            "default."
        ),
    )
    parser.add_argument(
        "--no-worker",
        action="store_true",
        help=(
            "Skip spawning a supervised worker on console launch. The "
            "status bar shows worker: none until you type '/worker start' "
            "or run 'flywheel worker' in another terminal."
        ),
    )
    return parser


def _resolve_work_source(
    tasks_dir_arg: str | None, policy: WorkPolicy | None
) -> WorkSource:
    """Mirror ``fw status``'s work-source precedence.

    Explicit ``--tasks-dir`` always selects the directory source (so the
    summary counts can be pinned at the CLI), else the policy decides,
    else the default ``.flywheel/tasks`` directory.
    """
    if tasks_dir_arg:
        return DirectoryWorkSource(Path(tasks_dir_arg))
    if policy is not None:
        return build_work_source(policy)
    return DirectoryWorkSource(DEFAULT_TASKS_DIR)


def _resolve_tasks_dir_for_worker(
    tasks_dir_arg: str | None, policy: WorkPolicy | None
) -> Path | None:
    """Resolve the ``--tasks-dir`` value the spawned worker should use.

    The supervisor forwards this to the child as ``--tasks-dir`` so
    the worker reads the same active/ directory the console resolved
    for the summary header. ``None`` means "use the worker's own
    default" (``<repo>/.flywheel/tasks``) -- correct for the common
    case where the operator runs ``fw`` at the repo root and the
    policy left the source at its default.
    """

    if tasks_dir_arg:
        return Path(tasks_dir_arg)
    if policy is None:
        return None
    if policy.source_kind == "directory" and policy.tasks_dir is not None:
        return policy.tasks_dir
    return None


def _resolve_model_for_worker(
    model_arg: str | None, policy: WorkPolicy | None
) -> str | None:
    """Resolve the agent model id the spawned worker should run with.

    Precedence is exactly:

    * an explicit ``--model`` CLI flag wins;
    * else ``flywheel.toml`` ``[agent] model`` (when a policy loaded);
    * else ``None`` so the worker omits ``--model`` and the SDK
      falls through to the Claude Code default.

    Mirrors :func:`_resolve_tasks_dir_for_worker`'s explicit-flag ->
    policy -> default chain so both knobs share one mental model.
    """

    if model_arg:
        return model_arg
    if policy is None:
        return None
    return policy.model


def _resolve_archive_tasks_dir(
    tasks_dir_arg: str | None, policy: WorkPolicy | None
) -> Path | None:
    """Resolve the directory ``/archive`` should sweep, or ``None``.

    ``/archive`` is meaningful only for directory work sources (the same
    semantics as ``fw archive``); a tracker source returns ``None`` and
    the slash command degrades to an inline notice.
    """

    if tasks_dir_arg:
        return Path(tasks_dir_arg)
    if policy is None:
        return DEFAULT_TASKS_DIR
    if policy.source_kind == "directory" and policy.tasks_dir is not None:
        return policy.tasks_dir
    return None


def _emit_missing_store(db_path: Path) -> int:
    print(
        f"fw: no store found at {db_path}",
        file=sys.stderr,
    )
    print(
        "  run 'fw init' to scaffold .flywheel/ and the work policy, "
        "then start the orchestrator to populate the store.",
        file=sys.stderr,
    )
    return 2


def _emit_missing_dsn() -> int:
    """Postgres analogue of :func:`_emit_missing_store` (spec FR-11).

    A postgres backend has no store file to check; the equivalent of "no
    store found" is "no DSN env var set", and the remedy names both
    variables of the resolution contract.
    """
    print(
        f"fw: store backend is postgres but neither {PG_DSN_ENV} nor "
        f"{PG_DSN_FALLBACK_ENV} is set",
        file=sys.stderr,
    )
    print(
        "  export one with a postgres connection string, then rerun fw.",
        file=sys.stderr,
    )
    return 2


def _run_snapshot(
    db_path: Path, work_source: WorkSource, *, policy: WorkPolicy | None
) -> int:
    """Build one snapshot, print it as JSON, exit 0.

    Reads the store once (constructed through the orchestrator's store
    factory), closes it, prints the payload. No ANSI escape sequences
    ever touch stdout in this mode.
    """
    now = datetime.now(timezone.utc)
    store = open_sqlite_bound_store(policy, db_path=db_path)
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
    store: SqliteStore | PostgresStore,
    *,
    run_id: str,
    status: Status,
    awaiting_ordinal: int | None,
) -> str | None:
    """Resolve the pending manual gate's instruction for a parked run.

    Local mirror of the awaiting-instruction lookup
    :func:`flywheel_orchestrator.collect_live_rows` performs for the
    dashboard row: returns ``None`` unless the lifecycle is parked at
    :class:`flywheel_core.lifecycle.Status.AWAITING_APPROVAL` with a valid
    ordinal that addresses a :class:`flywheel_core.task.ManualGrader` on
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


def _run_dashboard(
    db_path: Path,
    work_source: WorkSource,
    archive_tasks_dir: Path | None,
    *,
    tasks_dir: Path | None,
    model: str | None,
    no_worker: bool,
    policy: WorkPolicy | None,
) -> int:
    """Open the Textual dashboard, polling ``db_path`` until the operator quits.

    ``archive_tasks_dir`` is the directory the ``/archive`` slash command
    sweeps; ``None`` for tracker-source policies disables the verb (the
    screen surfaces a "not wired" notice).

    The supervisor is constructed unconditionally so the status bar
    always shows worker state, but only spawned when ``no_worker`` is
    false (FR-2 honours ``--no-worker`` by skipping the auto-spawn;
    the operator can still type ``/worker start`` afterwards). The
    ``finally`` block detaches before closing the store -- never a
    silent kill, per the spec.
    """
    started_at = datetime.now(timezone.utc)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    supervisor = WorkerSupervisor(
        db_path=db_path,
        tasks_dir=tasks_dir,
        model=model,
        policy=SupervisionPolicy(_SUPERVISION_BUDGET),
    )
    if not no_worker:
        supervisor.start()
    # Autopilot is an independent supervised child (decision D-6): the operator
    # starts it on demand via ``/autopilot start`` -- it is never auto-spawned
    # on console launch (it writes to the base branch unattended). The
    # supervisor is constructed unconditionally so the status surface and the
    # console action exist; it is detached (never killed) on console exit.
    autopilot_supervisor = AutopilotSupervisor(
        tasks_dir=tasks_dir,
        model=model,
        policy=SupervisionPolicy(_SUPERVISION_BUDGET),
    )

    def poll() -> DashboardSnapshot:
        return build_snapshot(
            store,
            work_source=work_source,
            now=datetime.now(timezone.utc),
            started_at=started_at,
        )

    def enqueue_for_run(
        run_id: str, kind: str, payload: Mapping[str, Any]
    ) -> int:
        """Producer seam shared by the dashboard slash commands."""

        record = store.enqueue_command(
            run_id, kind, payload, now=datetime.now(timezone.utc)
        )
        assert record.id is not None
        return record.id

    def archive() -> list[str]:
        """``/archive`` action: sweep the directory work source.

        Returns the list of moved phase directories (relative paths)
        the operator sees in the inline notice. Disabled (returns an
        empty list, the screen treats it as a no-op) for tracker
        sources where ``fw archive`` itself errors.

        Threads the same gates the worker/CLI sweeps apply so a manual
        console archive is not a weaker path: ``repo_root`` +
        ``landing_base`` arm the landed predicate (a phase archives only
        when every DONE task's work is landed), ``true_base`` arms the
        phase-branch merge predicate (under the phase strategy a phase
        archives only when its integration branch merged into the true
        base -- criterion 8 keeps the console from being a gate-skip),
        ``phase_verify`` runs the configured phase-exit gate, and the claim
        store carries the stop-event ledger (stop-resolved markers on
        archive, and the fail-closed indeterminate-landing strand surfaced
        in ``status``). The landing/true base is the configured submit
        base, else ``HEAD``; the phase-merge gate arms only when a
        ``flywheel/phase/<phase>`` branch exists, so merge/pr repos are
        unchanged.
        """

        if archive_tasks_dir is None:
            return []
        repo_root = repo_root_for_tasks_dir(archive_tasks_dir)
        landing_base = (
            policy.submit_base if policy is not None else None
        ) or "HEAD"
        phase_verify = policy.phase_verify if policy is not None else None
        claims = build_claim_store(policy, db_path=db_path)
        try:
            moved = archive_completed_phases(
                archive_tasks_dir,
                store,
                repo_root=repo_root,
                phase_verify=phase_verify,
                landing_base=landing_base,
                true_base=landing_base,
                claims=claims,
            )
        finally:
            claims.close()
        return [str(path) for path in moved]

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

        run_file = db_path.parent / "logs" / "runs" / f"{run_id}.jsonl"
        tailer = TranscriptTailer(run_file, run_id)

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

        def enqueue(kind: str, payload: Mapping[str, Any]) -> int:
            """Enqueue a control command against ``run_id`` for the live store.

            Closes over ``store`` and ``run_id`` so the screen sees a
            single-argument-pair signature. The returned id is the
            store-assigned ``control_commands.id`` -- the same key the
            in-process watcher uses to attribute its
            ``harness.control_command_applied`` / ``_failed`` telemetry.
            """

            record = store.enqueue_command(
                run_id, kind, payload, now=datetime.now(timezone.utc)
            )
            assert record.id is not None
            return record.id

        return SessionScreen(
            run_id=run_id,
            task_id=task_id,
            fetch=fetch,
            status=status,
            enqueue=enqueue,
            archive=archive,
        )

    def open_history() -> HistoryScreen:
        """Construct the finished-run history screen (``h`` / ``/history``).

        The phase fallback index is rebuilt per open: it scans the task
        directories for runs recorded before ``lifecycles.source``
        existed, and an open is rare enough that freshness beats
        caching. Tracker-source policies have no directory to scan, so
        those legacy runs render ungrouped.
        """

        fallback = (
            build_task_phase_index(archive_tasks_dir)
            if archive_tasks_dir is not None
            else None
        )

        def fetch() -> list[HistoryRow]:
            return collect_history_rows(store, fallback_phases=fallback)

        return HistoryScreen(fetch=fetch, open_session=open_session)

    app = DashboardApp(
        poll=poll,
        open_session=open_session,
        open_history=open_history,
        enqueue=enqueue_for_run,
        archive=archive,
        worker_status=supervisor.status,
        worker_start=supervisor.start,
        worker_stop=supervisor.stop,
        worker_detach=supervisor.detach,
        autopilot_status=autopilot_supervisor.status,
        autopilot_start=autopilot_supervisor.start,
        autopilot_stop=autopilot_supervisor.stop,
        autopilot_detach=autopilot_supervisor.detach,
    )
    try:
        rc = app.run()
    finally:
        # ``close()`` is the detach-by-default path: the supervised
        # child (if any) keeps running so a SIGINT-on-the-console exit
        # never silently kills the worker (spec FR-3 / Edge Cases).
        # The operator's explicit ``stop`` choice has already signaled
        # the child by the time we get here. Autopilot detaches the same
        # way -- a started autopilot daemon survives console exit.
        supervisor.close()
        autopilot_supervisor.close()
        store.close()
    return rc if isinstance(rc, int) else 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
