"""Tests for the ``flywheel.workflow`` CLI module.

Covers the pure logic — task discovery, status classification, eligibility,
archive — against an in-memory SQLite store and temp task dirs. The ``run``
subcommand's real-agent path is intentionally not exercised end-to-end here;
``run_task_file``'s seam is the ``invoke`` callable, which lower-level
harness tests already cover.
"""

from __future__ import annotations

import json
import signal
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import pytest
from claude_agent_sdk import Message

from flywheel.envelope import Intent, ValidEnvelope
from flywheel.harness import HarnessOutcome, InvocationRequest
from flywheel.invoker import InvocationSignals, IterationResult
from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.store_protocols import EventRecord
from flywheel.store_sqlite import SqliteStore
from flywheel.task import CommandGrader, RubricGrader, ValidationError
from flywheel.workflow import (
    EVENTS_JSON,
    EVENTS_NONE,
    EVENTS_PLAIN,
    build_inline_task,
    main,
    recover_stranded_lifecycles,
    run_task_object,
)
from flywheel_orchestrator._workflow import (
    LOOP_BASE_FILENAME,
    TaskState,
    archive_completed_phases,
    build_status_rows,
    collect_live_rows,
    iter_active_phase_dirs,
    iter_active_task_files,
    phase_diff_vs_base,
    read_phase_base,
    select_next_task,
    write_phase_base_if_missing,
)
from flywheel_orchestrator._workflow import main as orch_main


def _write_task(
    path: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    tags: list[str] | None = None,
) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    if tags:
        payload["tags"] = tags
    path.write_text(json.dumps(payload))
    return path


def _seed_done(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist a single Lifecycle for ``task_id`` ending in DONE."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-ok")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_failed(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.FAILED, error="boom", now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_interrupted(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-interrupted")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.INTERRUPTED, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_blocked(
    store: SqliteStore,
    task_id: str,
    requires_payload: list[dict[str, object]],
    *,
    run_id: str | None = None,
) -> Lifecycle:
    """Persist an INTERRUPTED lifecycle whose ``blocked_requires_json``
    captures ``requires_payload`` — the recheck-eligible shape.

    Mirrors what the harness's ``Intent.BLOCKED`` branch writes: status
    INTERRUPTED + a non-null persisted requires snapshot. Used by the
    recheck-blocked CLI tests so the scan filter sees them and the
    primitive has predicates to evaluate.
    """
    rid = run_id or f"run-{task_id}-blocked"
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=rid)
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.INTERRUPTED, now=now)
    lc.blocked_requires_json = json.dumps(requires_payload)
    store.create_lifecycle(lc)
    return lc


def _write_blocked_task(path: Path, task_id: str, grader_name: str) -> Path:
    """Task file with a single command grader the recheck primitive can
    resolve when a ``command_grader`` predicate references ``grader_name``.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [
            {"type": "command", "run": "true", "name": grader_name}
        ],
    }
    path.write_text(json.dumps(payload))
    return path


# ---------- Filesystem walking ----------


def test_iter_active_phase_dirs_orders_by_filename(tmp_path: Path) -> None:
    (tmp_path / "active" / "02-second").mkdir(parents=True)
    (tmp_path / "active" / "01-first").mkdir(parents=True)
    (tmp_path / "active" / "10-tenth").mkdir(parents=True)
    (tmp_path / "active" / ".hidden").mkdir(parents=True)

    dirs = list(iter_active_phase_dirs(tmp_path))
    names = [d.name for d in dirs]
    assert names == ["01-first", "02-second", "10-tenth"]


def test_iter_active_phase_dirs_handles_missing_root(tmp_path: Path) -> None:
    assert list(iter_active_phase_dirs(tmp_path)) == []


def test_iter_active_task_files_skips_underscore_and_hidden(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "active" / "01-only"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")
    (phase / "_phase.json").write_text("{}")
    (phase / ".secret.json").write_text("{}")
    (phase / "notes.md").write_text("text")

    files = [p.name for p in iter_active_task_files(tmp_path)]
    assert files == ["a.json", "b.json"]


# ---------- Status classification ----------


def test_build_status_rows_classifies_fresh_done_failed_running_interrupted(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "fresh.json", "fresh")
    _write_task(phase / "done.json", "done")
    _write_task(phase / "failed.json", "failed")
    _write_task(phase / "running.json", "running")
    _write_task(phase / "interrupted.json", "interrupted")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done")
        _seed_failed(store, "failed")
        _seed_running(store, "running")
        _seed_interrupted(store, "interrupted")

        rows = build_status_rows(tmp_path, store)
        state_by_id = {row.task.id: row.state for row in rows}
        assert state_by_id == {
            "fresh": TaskState.FRESH,
            "done": TaskState.DONE,
            "failed": TaskState.RETRYABLE,
            "running": TaskState.IN_PROGRESS,
            "interrupted": TaskState.INTERRUPTED,
        }
    finally:
        store.close()


def test_build_status_rows_treats_later_done_as_done(tmp_path: Path) -> None:
    """A task with an earlier failure + later done should classify DONE."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "retried.json", "retried")
    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "retried")
        _seed_done(store, "retried")
        rows = build_status_rows(tmp_path, store)
        assert rows[0].state == TaskState.DONE
    finally:
        store.close()


# ---------- Eligibility / next-task selection ----------


def test_select_next_picks_first_fresh_in_walk_order(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")

    store = SqliteStore(":memory:")
    try:
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None
        assert pick.task.id == "a"
    finally:
        store.close()


def test_select_next_respects_prerequisites(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "first.json", "first")
    _write_task(phase / "second.json", "second", prerequisites=["first"])

    store = SqliteStore(":memory:")
    try:
        # 'first' is the only eligible task.
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "first"

        # After 'first' is done, 'second' becomes eligible.
        _seed_done(store, "first")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "second"
    finally:
        store.close()


def test_select_next_skips_in_progress_and_done(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")
    _write_task(phase / "c.json", "c")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        _seed_running(store, "b")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "c"
    finally:
        store.close()


def test_select_next_retries_failed_task(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "broken.json", "broken")

    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "broken")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "broken"
        assert pick.state == TaskState.RETRYABLE
    finally:
        store.close()


def test_select_next_resumes_interrupted_task(tmp_path: Path) -> None:
    """Interrupted tasks are retry-eligible without operator unblock.

    The harness normalizes INTERRUPTED -> READY at entry (see
    docs/task-lifecycle.md), and the worker reconciles stranded
    lifecycles to INTERRUPTED on startup. The selector must agree:
    interrupted tasks block the phase otherwise."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "paused.json", "paused")

    store = SqliteStore(":memory:")
    try:
        _seed_interrupted(store, "paused")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "paused"
        assert pick.state == TaskState.INTERRUPTED
    finally:
        store.close()


def test_select_next_unblocks_downstream_after_interrupted(
    tmp_path: Path,
) -> None:
    """Interrupted root of a dependency chain must not freeze the phase.

    Regression guard for the symptom that triggered this change: the worker
    reconciles a SIGTERM'd task to INTERRUPTED, and every downstream task
    that lists it as a prerequisite would stall forever if INTERRUPTED were
    treated as ineligible. The selector picks the interrupted root first;
    downstream only unblocks once that root reaches DONE."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "root.json", "root")
    _write_task(phase / "leaf.json", "leaf", prerequisites=["root"])

    store = SqliteStore(":memory:")
    try:
        _seed_interrupted(store, "root")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "root"
        assert pick.state == TaskState.INTERRUPTED
    finally:
        store.close()


def test_select_next_returns_none_when_prereq_missing(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "only.json", "only", prerequisites=["ghost"])
    store = SqliteStore(":memory:")
    try:
        rows = build_status_rows(tmp_path, store)
        assert select_next_task(rows) is None
    finally:
        store.close()


def test_select_next_returns_none_when_all_done(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        rows = build_status_rows(tmp_path, store)
        assert select_next_task(rows) is None
    finally:
        store.close()


def test_select_next_spans_phases(tmp_path: Path) -> None:
    _write_task(tmp_path / "active" / "01-first" / "a.json", "a")
    _write_task(tmp_path / "active" / "02-second" / "b.json", "b")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "b"
    finally:
        store.close()


# ---------- Archive ----------


def test_archive_moves_only_fully_done_phases(tmp_path: Path) -> None:
    done_phase = tmp_path / "active" / "01-done"
    mixed_phase = tmp_path / "active" / "02-mixed"
    _write_task(done_phase / "a.json", "done-a")
    _write_task(done_phase / "b.json", "done-b")
    _write_task(mixed_phase / "c.json", "mixed-c")
    _write_task(mixed_phase / "d.json", "mixed-d")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done-a")
        _seed_done(store, "done-b")
        _seed_done(store, "mixed-c")
        # 'mixed-d' is left fresh — phase should not archive.
        moved = archive_completed_phases(tmp_path, store)
    finally:
        store.close()

    assert [d.name for d in moved] == ["01-done"]
    assert not done_phase.exists()
    assert (tmp_path / "archive" / "01-done").is_dir()
    assert mixed_phase.is_dir()


def test_archive_is_idempotent(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        first = archive_completed_phases(tmp_path, store)
        second = archive_completed_phases(tmp_path, store)
    finally:
        store.close()
    assert len(first) == 1
    assert second == []


def test_archive_ignores_loop_base_dotfile(tmp_path: Path) -> None:
    """A ``.loop-base`` dotfile alongside DONE tasks must not block archive.

    The ``archive_completed_phases`` task-file iteration already filters
    non-``.json`` and dot-prefixed entries (workflow.py task-file filter);
    this test pins that behavior so the loop-base capture cannot
    accidentally regress phase archiving.
    """
    phase = tmp_path / "active" / "01-done"
    _write_task(phase / "a.json", "done-a")
    # The committed-base dotfile lives alongside the task JSON.
    (phase / LOOP_BASE_FILENAME).write_text("abc123def456\n")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done-a")
        moved = archive_completed_phases(tmp_path, store)
    finally:
        store.close()

    assert [d.name for d in moved] == ["01-done"]
    assert not phase.exists()
    archived = tmp_path / "archive" / "01-done"
    assert archived.is_dir()
    # The dotfile travels with the phase into archive/ so an auditor can
    # re-derive the diff signal.
    assert (archived / LOOP_BASE_FILENAME).read_text().strip() == "abc123def456"


# ---------- Phase base capture / diff helpers (real git repo) ----------


def _git_init_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "-C", str(repo), "init", "-b", "main"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "config", "user.email", "wf-test@example.com"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "config", "user.name", "wf test"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(repo), "config", "commit.gpgsign", "false"],
        check=True,
        capture_output=True,
    )
    (repo / "README.md").write_text("base\n")
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "init"],
        check=True,
        capture_output=True,
    )


def _git_commit_file(repo: Path, name: str, body: str, message: str) -> None:
    (repo / name).write_text(body)
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", message],
        check=True,
        capture_output=True,
    )


def _git_head(repo: Path) -> str:
    out = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return out


def test_write_phase_base_if_missing_captures_head_and_is_idempotent(
    tmp_path: Path,
) -> None:
    """Real-git-repo test for FR-1 base capture.

    Two invariants:

    * The first call writes the current ``HEAD`` SHA into ``.loop-base``
      and returns ``True``.
    * A re-run after ``.loop-base`` already exists must not move the
      recorded base forward -- the first-seen SHA is the true base. The
      re-run returns ``False`` and the recorded SHA is unchanged even if
      ``HEAD`` has advanced.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    phase_dir = repo / ".workflow" / "tasks" / "active" / "01-phase"
    phase_dir.mkdir(parents=True)

    head_before = _git_head(repo)

    assert write_phase_base_if_missing(repo, phase_dir) is True
    recorded = read_phase_base(phase_dir)
    assert recorded == head_before

    # Advance HEAD to simulate the phase's tasks merging into base.
    _git_commit_file(repo, "advance.txt", "x\n", "advance base")
    assert _git_head(repo) != head_before

    # Re-run must be a no-op: the dotfile already exists, the recorded
    # SHA must not move forward.
    assert write_phase_base_if_missing(repo, phase_dir) is False
    assert read_phase_base(phase_dir) == head_before


def test_phase_diff_vs_base_returns_added_lines(tmp_path: Path) -> None:
    """The diff helper returns the phase's added lines as unified-diff text.

    Reproduces the production sequence: capture the base, commit the
    dotfile, then land a task commit -- the diff against the recorded base
    must include the task's added file content.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    phase_dir = repo / ".workflow" / "tasks" / "active" / "01-phase"
    phase_dir.mkdir(parents=True)

    assert write_phase_base_if_missing(repo, phase_dir) is True
    # Mimic the worker: commit the freshly-written .loop-base dotfile.
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "chore: record phase base sha"],
        check=True,
        capture_output=True,
    )

    # The phase's task lands a feature file.
    _git_commit_file(repo, "feature.txt", "hello phase\n", "feat: task work")

    diff = phase_diff_vs_base(repo, phase_dir)
    assert "feature.txt" in diff
    assert "+hello phase" in diff


def test_phase_diff_vs_base_returns_empty_when_no_base_recorded(
    tmp_path: Path,
) -> None:
    """No ``.loop-base`` => empty diff (degrades safely, never raises)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    phase_dir = repo / ".workflow" / "tasks" / "active" / "01-phase"
    phase_dir.mkdir(parents=True)

    # Advance HEAD so a buggy implementation that fell back to "HEAD..HEAD"
    # would still differ from the spec'd empty-string contract.
    _git_commit_file(repo, "advance.txt", "x\n", "advance")

    assert phase_diff_vs_base(repo, phase_dir) == ""


# ---------- Loop-path archive gate (FR-2) ----------
#
# The gate attaches at the operational "phase declared done" moment in
# :func:`archive_completed_phases`. It derives the loop-path marker from
# the phase's cumulative diff vs ``.loop-base`` and refuses to archive a
# marked phase that has neither a DONE ``in-loop-verification`` task nor
# a recorded ``loop-path-exempt.md`` opt-out artifact.
#
# The tests below build a real git repo so the diff helper has something
# to walk; the phase ``.loop-base`` is captured before any feature lands
# so the diff faithfully reflects the in-phase work. The watched signal
# is a new ``Status`` enum member -- the spec's rock-solid signal 1.


_LIFECYCLE_BASE_BODY = '''"""Stub lifecycle for the loop-path gate tests."""

from enum import Enum


class Status(str, Enum):
    READY = "ready"
    RUNNING = "running"
    DONE = "done"
'''

_LIFECYCLE_WITH_NEW_STATUS = '''"""Stub lifecycle for the loop-path gate tests."""

from enum import Enum


class Status(str, Enum):
    READY = "ready"
    RUNNING = "running"
    DONE = "done"
    DEFERRED = "deferred"
'''


def _setup_loop_path_phase(
    repo: Path, phase_name: str = "01-loop-path"
) -> Path:
    """Build a real-git phase whose cumulative diff hits a watched signal.

    Lands a baseline ``src/flywheel/lifecycle.py``, records the phase's
    base SHA into ``.loop-base``, commits the dotfile, then commits a
    follow-up edit that adds a new ``Status`` enum member. The resulting
    ``phase_diff_vs_base(repo, phase_dir)`` contains the new ``Status``
    member -- signal 1 of the FR-1 trigger set -- so the gate fires.
    """
    _git_init_repo(repo)
    lifecycle_path = repo / "src" / "flywheel" / "lifecycle.py"
    lifecycle_path.parent.mkdir(parents=True, exist_ok=True)
    lifecycle_path.write_text(_LIFECYCLE_BASE_BODY)
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "chore: stub lifecycle"],
        check=True,
        capture_output=True,
    )

    phase_dir = repo / ".workflow" / "tasks" / "active" / phase_name
    phase_dir.mkdir(parents=True)
    assert write_phase_base_if_missing(repo, phase_dir) is True
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "chore: record phase base"],
        check=True,
        capture_output=True,
    )

    # Land the loop-path-bearing change against the recorded base.
    lifecycle_path.write_text(_LIFECYCLE_WITH_NEW_STATUS)
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "feat: add DEFERRED status"],
        check=True,
        capture_output=True,
    )
    return phase_dir


def _tasks_dir_of(phase_dir: Path) -> Path:
    """Return the ``active/<phase>/..`` parent -- the tasks-dir root."""
    return phase_dir.parent.parent


def test_archive_gate_refuses_marked_phase_without_verify_or_optout(
    tmp_path: Path,
) -> None:
    """FR-2: a loop-path-marked phase with no verify task / opt-out stays active.

    All feature tasks reach DONE, but the phase's cumulative diff added a
    new ``Status`` member. With neither a DONE in-loop-verification task
    nor a ``loop-path-exempt.md`` artifact, the gate must refuse to
    archive and log the refusal reason.
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)

    _write_task(phase_dir / "feature-a.json", "feature-a")
    _write_task(phase_dir / "feature-b.json", "feature-b")

    logged: list[str] = []
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        _seed_done(store, "feature-b")
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=logged.append
        )
    finally:
        store.close()

    assert moved == []
    assert phase_dir.is_dir(), "gated phase must remain in active/"
    assert not (tasks_dir / "archive" / phase_dir.name).exists()
    assert any(
        "Refusing to archive" in line and phase_dir.name in line
        for line in logged
    ), f"expected refusal log entry, got {logged!r}"
    assert any(
        "in-loop-verification" in line for line in logged
    ), f"refusal must mention the verify-task remedy, got {logged!r}"


def test_archive_gate_allows_marked_phase_with_done_verify_task(
    tmp_path: Path,
) -> None:
    """FR-2: adding a DONE in-loop-verification task lets the next sweep archive.

    Re-runs ``archive_completed_phases`` after the verify task is DONE
    and asserts the phase moves into ``archive/`` (the same call shape
    that was refused on the first run).
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)

    _write_task(phase_dir / "feature-a.json", "feature-a")
    _write_task(
        phase_dir / "verify.json",
        "verify-loop-path",
        tags=["in-loop-verification"],
    )

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        # First sweep: verify task is not yet DONE -- gate refuses.
        logged_first: list[str] = []
        first = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=logged_first.append
        )
        assert first == []
        assert phase_dir.is_dir()

        # Wait -- a not-yet-attempted verify task means
        # ``_has_done_lifecycle`` returns False, so the all-tasks-done
        # short-circuit fires before the gate even runs. The gate's
        # spec-pinned shape -- "Verify task present but its lifecycle is
        # not DONE -> gate refuses (same as missing)" -- requires that
        # adding the verify task does not, by itself, unlock archive.
        # Drive it to DONE and re-sweep.
        _seed_done(store, "verify-loop-path")
        logged_second: list[str] = []
        second = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=logged_second.append
        )
    finally:
        store.close()

    assert [p.name for p in second] == [phase_dir.name]
    assert not phase_dir.exists()
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()
    assert logged_second == [], (
        f"clean archive must not log refusal, got {logged_second!r}"
    )


def test_archive_gate_refuses_when_verify_task_lifecycle_not_done(
    tmp_path: Path,
) -> None:
    """A verify task present but not DONE is treated identically to missing.

    Pins the spec's "Verify task present but its lifecycle is not DONE
    -> gate refuses (same as missing)" error-handling row.
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)

    _write_task(phase_dir / "feature-a.json", "feature-a")
    _write_task(
        phase_dir / "verify.json",
        "verify-loop-path",
        tags=["in-loop-verification"],
    )

    logged: list[str] = []
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        _seed_failed(store, "verify-loop-path")
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=logged.append
        )
    finally:
        store.close()

    # The feature task is DONE but the verify task is FAILED -- the
    # all-tasks-done short-circuit alone keeps the phase in active/
    # before the gate evaluates. The phase still stays active and no
    # refusal is logged (the all-tasks-done check is silent).
    assert moved == []
    assert phase_dir.is_dir()


def test_archive_gate_allows_marked_phase_with_optout_artifact(
    tmp_path: Path,
) -> None:
    """FR-2 + FR-5: a valid opt-out artifact alone clears the gate.

    The artifact downgrades the marker; an audit re-check (separate task)
    is the FR-6b backstop. From the gate's perspective the phase is
    eligible to archive even though the diff added a watched symbol.
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)

    _write_task(phase_dir / "feature-a.json", "feature-a")
    (phase_dir / "loop-path-exempt.md").write_text(
        "---\n"
        f"phase: {phase_dir.name}\n"
        "author: john.young\n"
        "reason: refactor only; no new lifecycle path\n"
        "---\n"
        "\nFree-form notes for the auditor.\n",
        encoding="utf-8",
    )

    logged: list[str] = []
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=logged.append
        )
    finally:
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert not phase_dir.exists()
    archived = tasks_dir / "archive" / phase_dir.name
    assert archived.is_dir()
    # The opt-out travels into archive/ so an auditor can re-check it.
    assert (archived / "loop-path-exempt.md").is_file()
    assert logged == []


def test_archive_gate_inert_without_repo_root(tmp_path: Path) -> None:
    """Legacy callers (no ``repo_root``) keep their previous archive contract.

    The synthetic phases in the existing archive tests don't pass a
    ``repo_root``, so the gate must never fire for them. This test
    exercises the same legacy call shape against a phase whose on-disk
    diff would otherwise have hit a watched signal, to pin that the
    skip-when-no-repo-root short-circuit is the load-bearing reason
    those tests continue to pass.
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)
    _write_task(phase_dir / "feature-a.json", "feature-a")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        moved = archive_completed_phases(tasks_dir, store)
    finally:
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert not phase_dir.exists()
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()


def test_archive_gate_inert_when_no_loop_base_recorded(
    tmp_path: Path,
) -> None:
    """A phase with no ``.loop-base`` archives even with ``repo_root`` passed.

    ``phase_diff_vs_base`` degrades to an empty string when the dotfile
    is absent, so the marker is empty and the gate stays silent. This
    matches the constraint that an empty marker archives exactly as
    before.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    # Land a diff that would otherwise hit signal 1, but never record a
    # base SHA -- so ``phase_diff_vs_base`` returns "".
    lifecycle_path = repo / "src" / "flywheel" / "lifecycle.py"
    lifecycle_path.parent.mkdir(parents=True, exist_ok=True)
    lifecycle_path.write_text(_LIFECYCLE_WITH_NEW_STATUS)
    subprocess.run(
        ["git", "-C", str(repo), "add", "-A"], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-m", "feat: add status"],
        check=True,
        capture_output=True,
    )

    phase_dir = repo / ".workflow" / "tasks" / "active" / "01-phase"
    phase_dir.mkdir(parents=True)
    tasks_dir = _tasks_dir_of(phase_dir)
    _write_task(phase_dir / "feature-a.json", "feature-a")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=lambda _msg: None
        )
    finally:
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert not phase_dir.exists()


def test_archive_gate_stays_idempotent_on_refusal(tmp_path: Path) -> None:
    """A second refused sweep must still report no moves and not raise.

    The function is documented idempotent; the gate must preserve that:
    re-running against the same gated phase yields the same outcome
    (phase still active, no exception, refusal re-logged).
    """
    repo = tmp_path / "repo"
    phase_dir = _setup_loop_path_phase(repo)
    tasks_dir = _tasks_dir_of(phase_dir)
    _write_task(phase_dir / "feature-a.json", "feature-a")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "feature-a")
        log_a: list[str] = []
        log_b: list[str] = []
        first = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=log_a.append
        )
        second = archive_completed_phases(
            tasks_dir, store, repo_root=repo, log=log_b.append
        )
    finally:
        store.close()

    assert first == []
    assert second == []
    assert phase_dir.is_dir()
    assert len(log_a) == 1 and len(log_b) == 1
    assert log_a == log_b


# ---------- CLI integration ----------


def test_main_next_prints_path_and_exits_zero(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    phase = tmp_path / "active" / "01-phase"
    written = _write_task(phase / "a.json", "a")
    db = tmp_path / "db.sqlite"
    rc = orch_main(
        [
            "next",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    out = capsys.readouterr().out.strip()
    assert Path(out) == written


def test_main_next_returns_one_when_no_tasks(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    rc = orch_main(["next", "--tasks-dir", str(tmp_path), "--db", str(db)])
    assert rc == 1
    assert capsys.readouterr().out == ""


def test_main_is_done_reflects_store(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    task_file = _write_task(tmp_path / "active" / "01" / "a.json", "a")
    db = tmp_path / "db.sqlite"
    # Before any lifecycle, exit non-zero.
    rc = main(["is-done", str(task_file), "--db", str(db)])
    assert rc == 1
    # After a DONE lifecycle, exit zero.
    store = SqliteStore(db)
    try:
        _seed_done(store, "a")
    finally:
        store.close()
    rc = main(["is-done", str(task_file), "--db", str(db)])
    assert rc == 0


# ---------- Live progress snapshot ----------


def test_live_skips_runs_that_are_not_in_flight(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "done-task")
        _seed_failed(store, "failed-task")
        _seed_interrupted(store, "interrupted-task")
        assert collect_live_rows(store) == []
    finally:
        store.close()


def test_live_reports_latest_sdk_message_when_newer(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-a")
        ts0 = datetime.now(timezone.utc)
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=ts0,
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=2,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Edit",
                            "input": {"file_path": "README.md"},
                        }
                    ],
                }
            ],
        )
        rows = collect_live_rows(store)
        assert len(rows) == 1
        row = rows[0]
        assert row.task_id == "task-a"
        assert row.status == Status.RUNNING
        assert row.iteration == 2
        assert row.last_kind == "ASSISTANT"
        assert "Edit" in row.last_detail
        assert "README.md" in row.last_detail
    finally:
        store.close()


def test_live_falls_back_to_event_when_no_sdk_messages(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-b")
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_completed",
                payload={},
                attempt_number=1,
            )
        )
        rows = collect_live_rows(store)
        assert len(rows) == 1
        assert rows[0].last_kind == "EVENT"
        assert rows[0].last_detail == "harness.iteration_completed"
        assert rows[0].iteration is None
    finally:
        store.close()


def test_live_marks_runs_with_no_activity(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-c")
        rows = collect_live_rows(store)
        assert len(rows) == 1
        assert rows[0].last_kind == "(none)"
        assert rows[0].last_ts is None
    finally:
        store.close()


def test_live_summarizes_user_tool_result(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-d")
        body = "x" * 1234
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "UserMessage",
                    "content": [
                        {"tool_use_id": "toolu_x", "content": body}
                    ],
                }
            ],
        )
        rows = collect_live_rows(store)
        assert rows[0].last_kind == "USER"
        assert rows[0].last_detail == f"tool_result({len(body)}B)"
    finally:
        store.close()


def test_main_live_prints_one_line_per_running_task(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running_a = _seed_running(store, "task-a")
        running_b = _seed_running(store, "task-b")
        store.save_sdk_messages(
            run_id=running_a.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "x.py"},
                        }
                    ],
                }
            ],
        )
        store.append_event(
            EventRecord(
                run_id=running_b.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
    finally:
        store.close()
    rc = orch_main(["live", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out
    lines = [ln for ln in out.splitlines() if ln.strip()]
    assert len(lines) == 2
    assert any("task-a" in ln and "ASSISTANT" in ln and "Read" in ln for ln in lines)
    assert any("task-b" in ln and "EVENT" in ln for ln in lines)


def test_main_live_empty_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    rc = orch_main(["live", "--db", str(db)])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no in-flight runs)"


# ---------- Live enrichment: breadcrumb + running totals ----------


def _iteration_completed_payload(
    *,
    iteration: int,
    total_tokens: int,
    total_cost_usd: float | None,
    num_turns: int | None,
) -> dict[str, object]:
    """Build a ``harness.iteration_completed`` payload matching the
    post-00009 shape (usage breakdown + cost + turns)."""
    return {
        "iteration": iteration,
        "envelope": {"kind": "valid", "intent": "continue"},
        "failure": None,
        "stop_reason": "end_turn",
        "rate_limited": False,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "total_tokens": total_tokens,
        },
        "total_cost_usd": total_cost_usd,
        "num_turns": num_turns,
    }


def test_live_sums_running_totals_across_iteration_events(
    tmp_path: Path,
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-totals")
        for i, (tt, cost, turns) in enumerate(
            [(100, 0.01, 2), (250, 0.05, 3), (50, 0.005, 1)], start=1
        ):
            store.append_event(
                EventRecord(
                    run_id=running.run_id,
                    ts=datetime.now(timezone.utc),
                    kind="harness.iteration_completed",
                    payload=_iteration_completed_payload(
                        iteration=i,
                        total_tokens=tt,
                        total_cost_usd=cost,
                        num_turns=turns,
                    ),
                    attempt_number=1,
                )
            )
        rows = collect_live_rows(store)
    finally:
        store.close()
    assert len(rows) == 1
    row = rows[0]
    assert row.iterations_completed == 3
    assert row.tokens_total == 400
    assert row.turns_total == 6
    # float sum; tolerate fp rounding.
    assert abs(row.cost_usd_total - 0.065) < 1e-9


def test_live_treats_missing_totals_fields_as_zero(tmp_path: Path) -> None:
    """Older / partial iteration events render the rest of the row instead
    of crashing — missing field = zero."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-partial")
        # Event 1: only usage.total_tokens; cost / turns null.
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_completed",
                payload=_iteration_completed_payload(
                    iteration=1,
                    total_tokens=200,
                    total_cost_usd=None,
                    num_turns=None,
                ),
                attempt_number=1,
            )
        )
        # Event 2: a legacy / partial payload missing every total.
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_completed",
                payload={"iteration": 2, "envelope": {"kind": "valid"}},
                attempt_number=1,
            )
        )
        rows = collect_live_rows(store)
    finally:
        store.close()
    row = rows[0]
    assert row.iterations_completed == 2
    assert row.tokens_total == 200
    assert row.cost_usd_total == 0.0
    assert row.turns_total == 0


def test_live_renders_zero_totals_when_no_iteration_completed_yet(
    tmp_path: Path,
) -> None:
    """A run that has not completed an iteration still renders breadcrumb
    + action line; totals display as zero / ``--``."""
    from flywheel_orchestrator._workflow import _format_live_line

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-empty")
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [{"type": "text", "text": "starting"}],
                }
            ],
        )
        rows = collect_live_rows(store)
    finally:
        store.close()
    row = rows[0]
    assert row.iterations_completed == 0
    assert row.tokens_total == 0
    assert row.cost_usd_total == 0.0
    assert row.turns_total == 0
    line = _format_live_line(row, datetime.now(timezone.utc))
    assert "tokens=0" in line
    assert "cost=--" in line
    assert "turns=0" in line
    # Breadcrumb and action line both present.
    assert "attempt=1" in line
    assert "iter=1" in line
    assert "ASSISTANT" in line


def test_live_breadcrumb_renders_attempt_and_iter_from_latest_activity(
    tmp_path: Path,
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-bc")
        # Earlier event; the sdk_message below has a higher sequence and
        # therefore drives the breadcrumb.
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=2,
            iteration_number=4,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [{"type": "text", "text": "go"}],
                }
            ],
        )
        rows = collect_live_rows(store)
    finally:
        store.close()
    assert rows[0].attempt == 2
    assert rows[0].iteration == 4


def test_live_breadcrumb_unknown_when_no_activity(tmp_path: Path) -> None:
    from flywheel_orchestrator._workflow import _format_live_line

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-na")
        rows = collect_live_rows(store)
    finally:
        store.close()
    row = rows[0]
    assert row.attempt is None
    assert row.iteration is None
    line = _format_live_line(row, datetime.now(timezone.utc))
    assert "attempt=?" in line
    assert "iter=?" in line


def test_live_truncates_overlong_detail(tmp_path: Path) -> None:
    """Very long tool args never wrap unboundedly — the assembled detail is
    capped (00011 edge case)."""
    from flywheel_orchestrator._workflow import _LIVE_DETAIL_MAX_WIDTH, _format_live_line

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-long")
        long_value = "y" * 5000
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Write",
                            "input": {"content": long_value},
                        }
                    ],
                }
            ],
        )
        rows = collect_live_rows(store)
    finally:
        store.close()
    line = _format_live_line(rows[0], datetime.now(timezone.utc))
    # The line is bounded — the detail portion ends at the cap; no 5000-char
    # blowup. We compare against the cap with a generous overhead allowance
    # for the prefix fields (task_id, status, breadcrumb, totals, age, kind).
    assert len(line) < _LIVE_DETAIL_MAX_WIDTH + 200


def test_live_unknown_message_type_falls_back_to_label(tmp_path: Path) -> None:
    """An sdk_message of a type the summarizer does not special-case still
    renders — using the upper-cased type as the label — and does not crash."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-unknown")
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "WeirdNewMessage",
                    "content": [{"foo": "bar"}],
                }
            ],
        )
        rows = collect_live_rows(store)
    finally:
        store.close()
    row = rows[0]
    assert row.last_kind == "WEIRDNEWMESSAGE"


def test_live_orders_runs_by_task_id(tmp_path: Path) -> None:
    """Multiple concurrent in-flight runs render in stable task-id order
    (00011 edge case)."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-zeta")
        _seed_running(store, "task-alpha")
        _seed_running(store, "task-mu")
        rows = collect_live_rows(store)
    finally:
        store.close()
    assert [r.task_id for r in rows] == [
        "task-alpha",
        "task-mu",
        "task-zeta",
    ]


def test_main_live_includes_totals_and_breadcrumb_in_rendered_output(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-render")
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=3,
            iteration_number=5,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "x.py"},
                        }
                    ],
                }
            ],
        )
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_completed",
                payload=_iteration_completed_payload(
                    iteration=5,
                    total_tokens=1234,
                    total_cost_usd=0.0567,
                    num_turns=4,
                ),
                attempt_number=3,
            )
        )
    finally:
        store.close()
    rc = orch_main(["live", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out
    line = next(ln for ln in out.splitlines() if "task-render" in ln)
    assert "attempt=3" in line
    # The iteration_completed event was written after the sdk_message, so
    # it carries the freshest sequence — iter is folded out of its payload.
    assert "iter=5" in line
    assert "tokens=1234" in line
    assert "cost=$0.0567" in line
    assert "turns=4" in line
    # Latest activity is the iteration-end event itself.
    assert "EVENT" in line
    assert "harness.iteration_completed" in line


def test_main_status_json_emits_machine_readable(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    phase = tmp_path / "active" / "01"
    _write_task(phase / "a.json", "a")
    db = tmp_path / "db.sqlite"
    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--json",
        ]
    )
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert len(payload) == 1
    assert payload[0]["task_id"] == "a"
    assert payload[0]["state"] == "fresh"


# ---------- Stranded-lifecycle recovery ----------


def _seed_validating(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist a lifecycle wedged in VALIDATING with an open attempt."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-validating")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    store.create_lifecycle(lc)
    store.save_attempt(
        lc.run_id,
        Attempt(number=1, started_at=now, run_id=lc.run_id),
    )
    return lc


def test_recover_finalizes_running_lifecycle(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-a")
        # A task already done in the store must be left untouched.
        done = _seed_done(store, "task-b")
        finalized = recover_stranded_lifecycles(store)
        assert finalized == [running.run_id]
        reloaded_running = store.load_lifecycle(running.run_id)
        assert reloaded_running is not None
        assert reloaded_running.status == Status.INTERRUPTED
        reloaded_done = store.load_lifecycle(done.run_id)
        assert reloaded_done is not None
        assert reloaded_done.status == Status.DONE
    finally:
        store.close()


def test_recover_finalizes_validating_and_closes_open_attempt(
    tmp_path: Path,
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        validating = _seed_validating(store, "task-v")
        finalized = recover_stranded_lifecycles(store)
        assert finalized == [validating.run_id]
        reloaded = store.load_lifecycle(validating.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        attempts = store.list_attempts(validating.run_id)
        assert len(attempts) == 1
        assert attempts[0].ended_at is not None
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR
        events = store.list_events(validating.run_id)
        kinds = [e.kind for e in events]
        assert "harness.crash" in kinds
        crash = next(e for e in events if e.kind == "harness.crash")
        assert crash.payload["classification"] == "worker_interrupted"
    finally:
        store.close()


def test_recover_filters_by_task_id(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        a = _seed_running(store, "task-a")
        b = _seed_running(store, "task-b")
        finalized = recover_stranded_lifecycles(store, task_id="task-a")
        assert finalized == [a.run_id]
        # task-b's stranded lifecycle stays stranded until its own
        # worker-start sweep runs.
        reloaded_b = store.load_lifecycle(b.run_id)
        assert reloaded_b is not None
        assert reloaded_b.status == Status.RUNNING
    finally:
        store.close()


def test_recover_does_not_consume_retry_budget(tmp_path: Path) -> None:
    """INTERRUPTED is not a retry-source state — retries must stay put."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = _seed_validating(store, "task-r")
        assert lc.retries == 0
        recover_stranded_lifecycles(store)
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.retries == 0
    finally:
        store.close()


def test_main_recover_prints_run_ids(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        seeded = _seed_running(store, "task-x")
    finally:
        store.close()
    rc = orch_main(["recover", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out.strip().splitlines()
    assert out == [seeded.run_id]


def test_main_recover_task_id_finalizes_only_that_task(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # The bash worker's reconcile_to_interrupted relies on this CLI flag to
    # recover exactly one task's stranded lifecycle (siblings running in
    # other subshells must stay running) via the event-sourced path.
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        a = _seed_running(store, "task-a")
        b = _seed_running(store, "task-b")
    finally:
        store.close()
    rc = orch_main(["recover", "--db", str(db), "--task-id", "task-a"])
    assert rc == 0
    out = capsys.readouterr().out.strip().splitlines()
    assert out == [a.run_id]
    store = SqliteStore(db)
    try:
        reloaded_a = store.load_lifecycle(a.run_id)
        reloaded_b = store.load_lifecycle(b.run_id)
        assert reloaded_a is not None and reloaded_a.status == Status.INTERRUPTED
        # The interrupted transition was event-sourced: replaying the domain
        # log reconstructs the same status, so log and projection agree.
        assert "harness.crash" in [
            e.kind for e in store.list_events(a.run_id)
        ]
        assert reloaded_b is not None and reloaded_b.status == Status.RUNNING
    finally:
        store.close()


def test_main_recover_empty_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    # Touch the store so it exists with no lifecycles.
    SqliteStore(db).close()
    rc = orch_main(["recover", "--db", str(db)])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no stranded lifecycles)"


# ---------- recheck-blocked CLI ----------


def test_main_recheck_blocked_empty_store_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """No blocked lifecycles -> the scan exits cleanly with a clear empty
    state line, never raises. Covers both 'no lifecycles at all' and
    'lifecycles exist but none with blocked_requires_json'."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = orch_main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no blocked lifecycles)"


def test_main_recheck_blocked_all_satisfied_transitions_and_prints_unblocked(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """All predicates satisfied -> recheck applies the transition and the
    CLI line announces the unblock. The lifecycle is now READY and
    blocked_requires_json was cleared by the harness."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-a.json", "task-a", "full-suite")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-a",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
        )
    finally:
        store.close()

    # file_exists predicate evaluates against the worker CWD. Make the
    # path real *in* that CWD so the predicate satisfies.
    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    rc = orch_main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{seeded.run_id}: unblocked"]

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.READY
    assert reloaded.blocked_requires_json is None


def test_main_recheck_blocked_partially_satisfied_reports_still_blocked(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """One satisfied + one unsatisfied predicate -> no transition; CLI
    line names only the misses, not every predicate."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-p.json", "task-p", "full-suite")
    db = tmp_path / "db.sqlite"

    monkeypatch.delenv("RECHECK_CLI_VAR", raising=False)
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-p",
            [
                {
                    "type": "file_exists",
                    "path": "ignored",
                    "present": False,
                },
                {"type": "env_var_set", "name": "RECHECK_CLI_VAR"},
            ],
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out.strip()
    assert rc == 0
    assert out.startswith(f"{seeded.run_id}: still blocked (")
    assert "env_var_set=RECHECK_CLI_VAR" in out
    # The satisfied predicate must not appear in the "still blocked" list.
    assert "file_exists=ignored" not in out

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is not None


def test_main_recheck_blocked_run_id_targets_one_lifecycle(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """--run-id processes the named lifecycle only; siblings stay
    interrupted with their persisted requires intact."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-a.json", "task-a", "full-suite")
    _write_blocked_task(phase / "task-b.json", "task-b", "full-suite")
    db = tmp_path / "db.sqlite"

    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        targeted = _seed_blocked(
            store,
            "task-a",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
            run_id="run-target",
        )
        other = _seed_blocked(
            store,
            "task-b",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
            run_id="run-other",
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--run-id",
            "run-target",
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{targeted.run_id}: unblocked"]

    store = SqliteStore(db)
    try:
        reloaded_target = store.load_lifecycle(targeted.run_id)
        reloaded_other = store.load_lifecycle(other.run_id)
    finally:
        store.close()
    assert reloaded_target is not None
    assert reloaded_target.status == Status.READY
    assert reloaded_other is not None
    assert reloaded_other.status == Status.INTERRUPTED
    assert reloaded_other.blocked_requires_json is not None


def test_main_recheck_blocked_dry_run_reports_without_transitioning(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """--dry-run on an all-satisfied lifecycle prints 'would unblock',
    leaves status INTERRUPTED, leaves blocked_requires_json intact, and
    emits harness.recheck_attempted but never harness.unblocked."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-d.json", "task-d", "full-suite")
    db = tmp_path / "db.sqlite"

    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-d",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--dry-run",
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{seeded.run_id}: would unblock"]

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
        events = [e.kind for e in store.list_events(seeded.run_id)]
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is not None
    assert "harness.recheck_attempted" in events
    assert "harness.unblocked" not in events


# ---------- status: blocked_on surface ----------


def test_main_status_text_includes_blocked_on_for_blocked_interrupted_row(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Interrupted row with blocked_requires_json -> text output carries
    a `blocked_on:` summary listing predicate type=identifier pairs."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-s.json", "task-s", "full-suite")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        _seed_blocked(
            store,
            "task-s",
            [
                {"type": "command_grader", "name": "full-suite"},
                {
                    "type": "file_exists",
                    "path": ".workflow/lkg/.venv",
                    "present": True,
                },
            ],
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "blocked_on:" in out
    assert "command_grader=full-suite" in out
    assert "file_exists=.workflow/lkg/.venv" in out


def test_main_status_text_omits_blocked_on_for_sigint_interrupted_row(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """SIGINT-paused lifecycle (INTERRUPTED, blocked_requires_json IS
    NULL) renders cleanly without a blocked_on line."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "task-i.json", "task-i")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        _seed_interrupted(store, "task-i")
    finally:
        store.close()

    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "interrupted" in out
    assert "blocked_on:" not in out


def test_main_status_json_includes_parsed_blocked_requires_when_present(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """JSON mode emits the parsed list (list of dicts) on blocked rows
    and OMITS the key entirely on rows without a snapshot — null is not a
    valid sentinel, the key must be absent."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "blocked.json", "blocked", "full-suite")
    _write_task(phase / "fresh.json", "fresh")
    db = tmp_path / "db.sqlite"

    persisted_requires: list[dict[str, object]] = [
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "READY"},
    ]
    store = SqliteStore(db)
    try:
        _seed_blocked(store, "blocked", persisted_requires)
    finally:
        store.close()

    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--json",
        ]
    )
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    by_id = {row["task_id"]: row for row in payload}

    assert "blocked_requires" in by_id["blocked"]
    assert by_id["blocked"]["blocked_requires"] == persisted_requires
    assert "blocked_requires" not in by_id["fresh"]


# ---------- run_task_file entry-time crash recording ----------


def test_run_task_file_records_crash_for_invoke_runtime_error(
    tmp_path: Path,
) -> None:
    """A stub invoke that raises mid-call must leave a non-empty
    lifecycles row and at least one harness.crash event in the
    SqliteStore. workflow.py's run_task_file re-raises the original
    exception so the worker subshell sees a non-zero exit.

    Backs the audit at
    ``.workflow/audits/08-recoverable-blocked-lifecycles.md`` finding
    "Crashes before create_lifecycle are invisible to every loop
    subsystem except the worker log": the new harness ordering must
    write the lifecycle row first so this exact failure shape is now
    visible in the DB.
    """
    import asyncio

    from flywheel.harness import InvocationRequest
    from flywheel.invoker import IterationResult
    from flywheel.workflow import run_task_file

    task_file = tmp_path / "task.json"
    _write_task(task_file, "probe")
    db = tmp_path / "db.sqlite"
    sandbox = tmp_path / "sandbox"

    async def _raising_invoke(_request: InvocationRequest) -> IterationResult:
        raise RuntimeError("workflow stub blew up")

    with pytest.raises(RuntimeError, match="workflow stub blew up"):
        asyncio.run(
            run_task_file(
                task_file,
                db_path=db,
                sandbox=sandbox,
                invoke=_raising_invoke,
            )
        )

    # Re-open the store to verify the DB recorded the crash before the
    # exception propagated to the caller.
    store = SqliteStore(db)
    try:
        conn = store._connection  # noqa: SLF001 — inspecting raw rows
        lifecycle_rows = conn.execute(
            "SELECT run_id, status FROM lifecycles WHERE task_id = ?",
            ("probe",),
        ).fetchall()
        assert len(lifecycle_rows) == 1
        row = lifecycle_rows[0]
        run_id = row["run_id"]
        # Terminal status: the entry-crash recorder walks the lifecycle
        # to FAILED so subsequent observers see the run is over.
        assert row["status"] == Status.FAILED.value
        # The harness.crash event is the audit-visible record of the
        # failure mode.
        crash_count = conn.execute(
            "SELECT COUNT(*) AS n FROM events "
            "WHERE run_id = ? AND kind = 'harness.crash'",
            (run_id,),
        ).fetchone()["n"]
        assert crash_count >= 1
    finally:
        store.close()


# ---------- run subcommand: inline goal + event streaming ----------


def _verify_iteration() -> IterationResult:
    """An IterationResult whose envelope signals ``intent=verify``.

    Enough to drive ``run_task`` through VALIDATING to a terminal status
    without a real agent — the same ``invoke`` seam the harness tests use.
    """
    return IterationResult(
        transcript="ok",
        messages=(),
        envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
        signals=InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=0.01,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="sess-1",
        ),
        failure=None,
    )


async def _verify_invoke(_request: InvocationRequest) -> IterationResult:
    return _verify_iteration()


def test_build_inline_task_is_graderless_by_default() -> None:
    task = build_inline_task("add retries to the http client")
    task.validate()
    assert task.graders == []
    assert task.goal == "add retries to the http client"


def test_build_inline_task_builds_command_and_rubric_graders() -> None:
    task = build_inline_task(
        "make it correct",
        checks=("pytest -q", "ruff check ."),
        rubric_assertions=("retries on 5xx only",),
    )
    types = [type(g).__name__ for g in task.graders]
    assert types == ["CommandGrader", "CommandGrader", "RubricGrader"]
    assert isinstance(task.graders[0], CommandGrader)
    assert task.graders[0].run == "pytest -q"
    assert isinstance(task.graders[2], RubricGrader)
    assert task.graders[2].assertions == ["retries on 5xx only"]


def test_build_inline_task_rejects_empty_goal() -> None:
    with pytest.raises(ValidationError, match="goal"):
        build_inline_task("   ")


def test_run_task_object_graderless_reaches_done_and_streams_events(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    import asyncio

    task = build_inline_task("do the thing")
    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sb",
            invoke=_verify_invoke,
            events=EVENTS_PLAIN,
            source="(inline goal)",
        )
    )
    assert outcome.lifecycle.status == Status.DONE
    captured = capsys.readouterr()
    # Events stream to stdout as readable lines; diagnostics to stderr.
    event_lines = [ln for ln in captured.out.splitlines() if ln.strip()]
    assert event_lines, "expected at least one event line on stdout"
    assert all("] " in ln for ln in event_lines)
    assert "harness.attempt_finalized" in captured.out
    # The unverified-run note lands on stderr, not in the event stream.
    assert "unverified run" in captured.err
    assert "unverified run" not in captured.out


@pytest.mark.parametrize("signum", [signal.SIGTERM, signal.SIGINT])
def test_run_task_object_finalizes_lifecycle_on_signal(
    tmp_path: Path, signum: int
) -> None:
    """A SIGTERM/SIGINT mid-run cancels the in-flight task and finalizes its
    lifecycle to INTERRUPTED with a harness.interrupted receipt -- it must not
    be stranded in `running`.

    SIGTERM is the production stop signal (``docker stop``, ``kubectl delete
    pod``, ``systemctl stop``); its default disposition would terminate the
    interpreter before run_task reaches its finalizer, which is the exact
    stranding the bash worker had to reconcile out-of-band
    (``.workflow/audits/02-harness-resilience.md``). run_task_object installs
    a loop signal handler that converts both signals into asyncio task
    cancellation, which the harness's :func:`_run_attempt` boundary catches
    and routes through :func:`_handle_interrupt` (emits ``harness.interrupted``
    and transitions to INTERRUPTED in-band). ``finalize_stranded_lifecycle``
    in ``run_task_object``'s except branch is then a no-op safety net for
    the rare case where the cancellation lands outside an attempt.
    """
    import asyncio
    import os

    db = tmp_path / "db.sqlite"
    sandbox = tmp_path / "sb"
    task = build_inline_task("long running thing")

    async def _signaling_invoke(
        _request: InvocationRequest,
    ) -> IterationResult:
        # Deliver the stop signal to ourselves, then block. The loop handler
        # run_task_object installed cancels the running task, raising
        # CancelledError into this await before the sleep resolves.
        os.kill(os.getpid(), signum)
        await asyncio.sleep(30)
        return _verify_iteration()  # pragma: no cover - cancelled first

    with pytest.raises(asyncio.CancelledError):
        asyncio.run(
            run_task_object(
                task,
                db_path=db,
                sandbox=sandbox,
                invoke=_signaling_invoke,
                source="(inline goal)",
            )
        )

    store = SqliteStore(db)
    try:
        rows = store._connection.execute(  # noqa: SLF001 — inspecting rows
            "SELECT run_id, status FROM lifecycles WHERE task_id = ?",
            (task.id,),
        ).fetchall()
        assert len(rows) == 1
        assert rows[0]["status"] == Status.INTERRUPTED.value
        events = store.list_events(rows[0]["run_id"])
        kinds = [e.kind for e in events]
        # The in-band path (00012) emits harness.interrupted at the
        # _run_attempt boundary; finalize_stranded_lifecycle's
        # harness.crash signature is reserved for the SIGKILL/OOM/reboot
        # backstop.
        assert "harness.interrupted" in kinds
        interrupted = next(
            e for e in events if e.kind == "harness.interrupted"
        )
        assert interrupted.payload["classification"] == "worker_interrupted"
    finally:
        store.close()


def test_run_task_object_json_events_are_ndjson(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    import asyncio

    task = build_inline_task("do the thing")
    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sb",
            invoke=_verify_invoke,
            events=EVENTS_JSON,
        )
    )
    assert outcome.lifecycle.status == Status.DONE
    lines = [ln for ln in capsys.readouterr().out.splitlines() if ln.strip()]
    assert lines
    for line in lines:
        record = json.loads(line)  # every stdout line is valid JSON
        assert {"run_id", "kind", "ts", "payload"} <= record.keys()


def test_run_task_object_quiet_suppresses_event_stream(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    import asyncio

    task = build_inline_task("do the thing")
    asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sb",
            invoke=_verify_invoke,
            events=EVENTS_NONE,
        )
    )
    captured = capsys.readouterr()
    assert captured.out == ""  # nothing on stdout when quiet
    assert "[workflow] status  : done" in captured.err


def test_main_run_inline_goal_dispatches_to_run_task_object(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``flywheel run "<goal>"`` (non-file target) builds an inline Task
    and routes it through ``run_task_object``, not ``run_task_file``."""
    import flywheel.workflow as workflow

    captured: dict[str, object] = {}

    async def _fake_run_task_object(task: object, **kwargs: object) -> object:
        captured["task"] = task
        captured["kwargs"] = kwargs
        lc = Lifecycle(task_id="x", run_id="r")
        lc.transition_to(Status.READY, now=datetime.now(timezone.utc))
        lc.transition_to(Status.RUNNING, now=datetime.now(timezone.utc))
        lc.transition_to(Status.VALIDATING, now=datetime.now(timezone.utc))
        lc.transition_to(Status.DONE, now=datetime.now(timezone.utc))
        return HarnessOutcome(lifecycle=lc, attempts=())

    monkeypatch.setattr(workflow, "run_task_object", _fake_run_task_object)
    rc = main(
        [
            "run",
            "add retries to the http client",
            "--db",
            str(tmp_path / "db.sqlite"),
            "--check",
            "pytest -q",
        ]
    )
    assert rc == 0
    task = captured["task"]
    assert isinstance(task, object)
    from flywheel.task import Task

    assert isinstance(task, Task)
    assert task.goal == "add retries to the http client"
    assert [type(g).__name__ for g in task.graders] == ["CommandGrader"]
    kwargs = captured["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["source"] == "(inline goal)"


def test_main_run_file_dispatches_to_run_task_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An existing-file target loads as a task file via ``run_task_file``."""
    import flywheel.workflow as workflow

    task_file = _write_task(tmp_path / "task.json", "probe")
    seen: dict[str, object] = {}

    async def _fake_run_task_file(path: Path, **kwargs: object) -> object:
        seen["path"] = path
        lc = Lifecycle(task_id="probe", run_id="r")
        lc.transition_to(Status.READY, now=datetime.now(timezone.utc))
        lc.transition_to(Status.RUNNING, now=datetime.now(timezone.utc))
        lc.transition_to(Status.VALIDATING, now=datetime.now(timezone.utc))
        lc.transition_to(Status.DONE, now=datetime.now(timezone.utc))
        return HarnessOutcome(lifecycle=lc, attempts=())

    monkeypatch.setattr(workflow, "run_task_file", _fake_run_task_file)
    rc = main(["run", str(task_file), "--db", str(tmp_path / "db.sqlite")])
    assert rc == 0
    assert seen["path"] == task_file


def test_main_run_rejects_inline_graders_on_task_file(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``--check`` against a task file is an error: the file owns graders."""
    task_file = _write_task(tmp_path / "task.json", "probe")
    rc = main(
        [
            "run",
            str(task_file),
            "--check",
            "pytest -q",
            "--db",
            str(tmp_path / "db.sqlite"),
        ]
    )
    assert rc == 2
    assert "task file" in capsys.readouterr().err


# ---------- live agent-message streaming ----------


def _sdk_messages_sample() -> list[Message]:
    from claude_agent_sdk import (
        AssistantMessage,
        ResultMessage,
        TextBlock,
        ToolResultBlock,
        ToolUseBlock,
        UserMessage,
    )

    return [
        AssistantMessage(
            content=[TextBlock(text="I will create the file.")],
            model="m",
            stop_reason="end_turn",
            session_id="s",
        ),
        AssistantMessage(
            content=[
                ToolUseBlock(
                    id="t1",
                    name="Write",
                    input={"file_path": "hello.txt", "content": "hi"},
                )
            ],
            model="m",
            stop_reason="tool_use",
            session_id="s",
        ),
        UserMessage(
            content=[
                ToolResultBlock(
                    tool_use_id="t1", content="created", is_error=False
                )
            ]
        ),
        ResultMessage(
            subtype="success",
            duration_ms=10,
            duration_api_ms=8,
            is_error=False,
            num_turns=2,
            session_id="s",
            stop_reason="end_turn",
            total_cost_usd=0.0123,
        ),
    ]


def test_format_live_message_renders_text_tool_result_and_result() -> None:
    from flywheel.workflow import _format_live_message

    text, tool, user, result = _sdk_messages_sample()
    assert "ASSISTANT  I will create the file." in _format_live_message(text)
    tool_line = _format_live_message(tool)
    assert "ASSISTANT  Write(file_path=hello.txt" in tool_line
    assert "USER  tool_result(7B)" in _format_live_message(user)
    result_line = _format_live_message(result)
    assert "RESULT  subtype=success turns=2 cost=$0.0123" in result_line


def test_make_message_observer_plain_writes_readable_lines() -> None:
    import io

    from flywheel.workflow import _make_message_observer

    buf = io.StringIO()
    observer = _make_message_observer(EVENTS_PLAIN, out=buf)
    assert observer is not None
    for msg in _sdk_messages_sample():
        observer(msg)
    lines = [ln for ln in buf.getvalue().splitlines() if ln.strip()]
    assert len(lines) == 4
    assert "Write(file_path=hello.txt" in lines[1]


def test_make_message_observer_json_writes_valid_ndjson() -> None:
    import io

    from flywheel.workflow import _make_message_observer

    buf = io.StringIO()
    observer = _make_message_observer(EVENTS_JSON, out=buf)
    assert observer is not None
    for msg in _sdk_messages_sample():
        observer(msg)
    lines = [ln for ln in buf.getvalue().splitlines() if ln.strip()]
    assert len(lines) == 4
    for line in lines:
        record = json.loads(line)
        assert "message_type" in record


def test_make_message_observer_none_mode_returns_none() -> None:
    import io

    from flywheel.workflow import _make_message_observer

    assert _make_message_observer(EVENTS_NONE, out=io.StringIO()) is None


def test_make_claude_code_invoke_forwards_on_message(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The default invoker threads ``on_message`` into ``invoke_iteration``
    so the agent's turns can be observed live (legacy fallback when no
    control store is wired in)."""
    import asyncio

    import flywheel.workflow as workflow

    captured: dict[str, object] = {}

    async def _fake_invoke_iteration(**kwargs: object) -> IterationResult:
        captured.update(kwargs)
        return _verify_iteration()

    monkeypatch.setattr(workflow, "invoke_iteration", _fake_invoke_iteration)

    def _observer(_msg: Message) -> None:
        pass

    invoker = workflow._make_claude_code_invoke(
        tmp_path / "sb", model=None, max_turns=5, on_message=_observer
    )
    request = InvocationRequest(
        prompt="go", transcript_graders=(), attempt_number=1, iteration_number=1
    )

    async def _drive() -> None:
        await invoker(request)

    asyncio.run(_drive())
    assert captured["on_message"] is _observer
    assert captured["prompt"] == "go"


def test_make_claude_code_invoke_uses_client_path_when_control_store_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When ``control_store`` and ``run_id`` are wired in, the production
    invoker routes through :func:`invoke_iteration_with_client` so the
    watcher runs against the open ClaudeSDKClient session."""
    import asyncio

    import flywheel.workflow as workflow
    from flywheel.store_memory import InMemoryStore

    captured: dict[str, object] = {}

    async def _fake_with_client(**kwargs: object) -> IterationResult:
        captured.update(kwargs)
        return _verify_iteration()

    monkeypatch.setattr(
        workflow, "invoke_iteration_with_client", _fake_with_client
    )

    store = InMemoryStore()

    invoker = workflow._make_claude_code_invoke(
        tmp_path / "sb",
        model=None,
        max_turns=5,
        on_message=None,
        control_store=store,
        run_id="run-1",
        audit_store=None,
    )
    request = InvocationRequest(
        prompt="go",
        transcript_graders=(),
        attempt_number=1,
        iteration_number=1,
    )

    async def _drive() -> None:
        await invoker(request)

    asyncio.run(_drive())
    assert captured["prompt"] == "go"
    assert captured["control_store"] is store
    assert captured["run_id"] == "run-1"


# ---------- Steering CLI subcommands ----------


def test_cmd_interrupt_enqueues_control_command(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel interrupt RUN_ID`` persists a kind=interrupt row."""
    db = tmp_path / "db.sqlite"
    # Seed a running lifecycle so the in-flight check passes silently.
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-a")
    finally:
        store.close()

    rc = main(
        ["interrupt", "run-task-a-running", "--db", str(db)]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "kind=interrupt" in out
    assert "run-task-a-running" in out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-a-running", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "interrupt"
    assert claimed[0].payload == {}


def test_cmd_steer_enqueues_say_with_message_text(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel steer RUN_ID MESSAGE`` persists kind=say with the text."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-b")
    finally:
        store.close()

    rc = main(
        [
            "steer",
            "run-task-b-running",
            "please double-check the rubric finding",
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert "kind=say" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-b-running", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "say"
    assert claimed[0].payload == {
        "text": "please double-check the rubric finding"
    }


def test_cmd_set_model_enqueues_set_model_with_model_id(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel set-model RUN_ID MODEL`` persists kind=set_model."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-c")
    finally:
        store.close()

    rc = main(
        [
            "set-model",
            "run-task-c-running",
            "claude-opus-4-1-20250805",
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert "kind=set_model" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-c-running", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "set_model"
    assert claimed[0].payload == {"model": "claude-opus-4-1-20250805"}


def test_cmd_interrupt_for_unknown_run_errors_without_enqueue(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """An unknown ``run_id`` is a producer-side error: SQLite's FK to
    ``lifecycles(run_id)`` makes the row unpersistable. The CLI reports
    that clearly (exit 2) rather than crash on the IntegrityError."""
    db = tmp_path / "db.sqlite"
    rc = main(["interrupt", "run-ghost", "--db", str(db)])
    assert rc == 2
    err = capsys.readouterr().err
    assert "unknown" in err
    assert "run-ghost" in err
    store = SqliteStore(db)
    try:
        # SqliteStore won't even create the lifecycles row at open, so no
        # control_commands row exists either.
        claimed = store.claim_commands(
            "run-ghost", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert claimed == []


def test_cmd_interrupt_for_not_in_flight_run_warns(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """A lifecycle in DONE/FAILED/INTERRUPTED still accepts an enqueue but
    a stderr note flags that the command will sit pending."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_interrupted(store, "task-d")
    finally:
        store.close()

    rc = main(
        ["interrupt", "run-task-d-interrupted", "--db", str(db)]
    )
    assert rc == 0
    err = capsys.readouterr().err
    assert "not in-flight" in err
    assert "interrupted" in err


def test_cmd_steer_rejects_empty_message(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """An empty operator message is a usage error; nothing is persisted."""
    db = tmp_path / "db.sqlite"
    rc = main(["steer", "run-x", "", "--db", str(db)])
    assert rc == 2
    assert "non-empty" in capsys.readouterr().err
    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-x", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert claimed == []


# ---------- approve / reject CLI subcommands ----------


def _seed_awaiting_approval(store: SqliteStore, task_id: str) -> Lifecycle:
    """Drive a lifecycle through to AWAITING_APPROVAL for producer tests.

    Mirrors the path the harness takes when every automated grader passes
    against a task that declares at least one ManualGrader:
    PENDING -> READY -> RUNNING -> VALIDATING -> AWAITING_APPROVAL.
    """
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-awaiting")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.AWAITING_APPROVAL, now=now)
    store.create_lifecycle(lc)
    return lc


def test_cmd_approve_enqueues_kind_approve_row(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel approve RUN_ID`` persists a kind=approve row and is silent
    about staleness when the lifecycle is correctly parked at
    AWAITING_APPROVAL."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_approval(store, "task-approve")
    finally:
        store.close()

    rc = main(["approve", "run-task-approve-awaiting", "--db", str(db)])
    assert rc == 0
    captured = capsys.readouterr()
    assert "kind=approve" in captured.out
    assert "run-task-approve-awaiting" in captured.out
    # AWAITING_APPROVAL is the valid in-flight status for approve, so no
    # stale-pending warning is emitted.
    assert "not in-flight" not in captured.err

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-approve-awaiting", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "approve"
    assert claimed[0].payload == {}


def test_cmd_reject_with_feedback_enqueues_payload(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel reject RUN_ID --feedback X`` persists kind=reject with
    the feedback in the payload."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_approval(store, "task-reject")
    finally:
        store.close()

    feedback = (
        "The migration drops a column still read by the billing service. "
        "Gate it behind a feature flag first."
    )
    rc = main(
        [
            "reject",
            "run-task-reject-awaiting",
            "--feedback",
            feedback,
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    captured = capsys.readouterr()
    assert "kind=reject" in captured.out
    # AWAITING_APPROVAL is the valid in-flight status for reject as well.
    assert "not in-flight" not in captured.err

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-reject-awaiting", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "reject"
    assert claimed[0].payload == {"feedback": feedback}


def test_cmd_reject_without_feedback_enqueues_empty_payload(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel reject RUN_ID`` (no --feedback) persists an empty
    payload; the resolver records ``"(no feedback provided)"``."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_approval(store, "task-reject-bare")
    finally:
        store.close()

    rc = main(
        ["reject", "run-task-reject-bare-awaiting", "--db", str(db)]
    )
    assert rc == 0
    assert "kind=reject" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-reject-bare-awaiting", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "reject"
    assert claimed[0].payload == {}


def test_cmd_approve_for_non_awaiting_run_warns(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Enqueuing approve against a run that is not AWAITING_APPROVAL still
    persists the row but prints the pending/stale stderr note: the
    resolver only acts on AWAITING_APPROVAL lifecycles, so the row will
    sit unprocessed until the lifecycle reaches that state (if ever)."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        # RUNNING is a valid in-flight status for interrupt/steer/set_model
        # but NOT for approve, which requires AWAITING_APPROVAL.
        _seed_running(store, "task-running")
    finally:
        store.close()

    rc = main(["approve", "run-task-running-running", "--db", str(db)])
    assert rc == 0
    captured = capsys.readouterr()
    assert "kind=approve" in captured.out
    err = captured.err
    assert "not in-flight" in err
    assert "running" in err


def test_cmd_reject_for_non_awaiting_run_warns(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Reject against a non-AWAITING_APPROVAL run also warns."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_interrupted(store, "task-interrupted")
    finally:
        store.close()

    rc = main(
        [
            "reject",
            "run-task-interrupted-interrupted",
            "--feedback",
            "no longer needed",
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    captured = capsys.readouterr()
    assert "kind=reject" in captured.out
    err = captured.err
    assert "not in-flight" in err
    assert "interrupted" in err


def test_cmd_approve_for_unknown_run_exits_two(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """An unknown ``run_id`` is a producer-side error (exit 2); no row
    is persisted, matching the existing interrupt/steer behavior."""
    db = tmp_path / "db.sqlite"
    rc = main(["approve", "run-ghost", "--db", str(db)])
    assert rc == 2
    err = capsys.readouterr().err
    assert "unknown" in err
    assert "run-ghost" in err
    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-ghost", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert claimed == []


def test_cmd_reject_for_unknown_run_exits_two(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """An unknown ``run_id`` is a producer-side error (exit 2) for
    reject too — including when --feedback is supplied."""
    db = tmp_path / "db.sqlite"
    rc = main(
        [
            "reject",
            "run-ghost",
            "--feedback",
            "ignored",
            "--db",
            str(db),
        ]
    )
    assert rc == 2
    err = capsys.readouterr().err
    assert "unknown" in err
    assert "run-ghost" in err
    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-ghost", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert claimed == []


# ---------- Operator surfacing: AWAITING_APPROVAL in status / live ----------


def _write_manual_task(
    path: Path,
    task_id: str,
    *,
    instruction: str,
    grader_name: str | None = None,
) -> Path:
    """Write a task file declaring one command + one manual gate.

    The cost-ordered chain runs the command grader first, then the
    manual gate parks the lifecycle in ``AWAITING_APPROVAL`` after the
    automated grader passes. The manual gate's ordinal in
    ``task.graders`` is ``1`` for tasks built by this helper.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    manual: dict[str, object] = {
        "type": "manual",
        "instruction": instruction,
    }
    if grader_name is not None:
        manual["name"] = grader_name
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [
            {"type": "command", "run": "true"},
            manual,
        ],
    }
    path.write_text(json.dumps(payload))
    return path


def _seed_awaiting_with_task(
    store: SqliteStore,
    task_id: str,
    *,
    instruction: str,
    grader_name: str | None = None,
    awaiting_ordinal: int = 1,
    run_id: str | None = None,
) -> tuple[Lifecycle, str]:
    """Drive a lifecycle to AWAITING_APPROVAL with the task pinned in store.

    Mirrors the harness's gate-entry path: persist the task (so
    ``load_task_for_run`` resolves it), seed the lifecycle through
    PENDING -> READY -> RUNNING -> VALIDATING -> AWAITING_APPROVAL,
    and pin the parked gate's ordinal via ``awaiting_manual_ordinal``.

    Returns ``(lifecycle, content_hash)`` so callers can both reload
    the run and reach back into the tasks table when they need to.
    """
    from flywheel.task import CommandGrader, ManualGrader, Task

    now = datetime.now(timezone.utc)
    task = Task(
        id=task_id,
        goal=f"Goal for {task_id}.",
        graders=[
            CommandGrader(run="true"),
            ManualGrader(instruction=instruction, name=grader_name),
        ],
    )
    content_hash = store.save_task(task, now=now)

    rid = run_id or f"run-{task_id}-awaiting"
    lc = Lifecycle(
        task_id=task_id,
        run_id=rid,
        task_content_hash=content_hash,
    )
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.awaiting_manual_ordinal = awaiting_ordinal
    lc.transition_to(Status.AWAITING_APPROVAL, now=now)
    store.create_lifecycle(lc)
    return lc, content_hash


def test_build_status_rows_classifies_awaiting_approval_distinctly(
    tmp_path: Path,
) -> None:
    """A parked manual-gate lifecycle classifies as AWAITING_APPROVAL —
    not the generic IN_PROGRESS fallback — so renderers can surface the
    owed decision and ``select_next_task`` does not pick it up as a
    fresh/retryable candidate."""
    phase = tmp_path / "active" / "01-phase"
    _write_manual_task(
        phase / "parked.json",
        "parked",
        instruction="Confirm the migration is safe.",
        grader_name="confirm-migration",
    )

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_with_task(
            store,
            "parked",
            instruction="Confirm the migration is safe.",
            grader_name="confirm-migration",
        )
        rows = build_status_rows(tmp_path, store)
    finally:
        store.close()

    assert len(rows) == 1
    row = rows[0]
    assert row.state == TaskState.AWAITING_APPROVAL
    assert row.latest_status == Status.AWAITING_APPROVAL
    assert row.awaiting_manual_ordinal == 1


def test_main_status_renders_awaiting_on_line_with_instruction(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel status`` prints an ``awaiting_on: <instruction>``
    follow-up line for a parked lifecycle so an operator sees what
    decision is owed without consulting the task file or audit stream."""
    phase = tmp_path / "active" / "01-phase"
    _write_manual_task(
        phase / "parked.json",
        "parked",
        instruction="Confirm the migration is safe.",
        grader_name="confirm-migration",
    )
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_with_task(
            store,
            "parked",
            instruction="Confirm the migration is safe.",
            grader_name="confirm-migration",
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "awaiting_approval" in out
    assert "awaiting_on: Confirm the migration is safe." in out


def test_main_status_json_emits_awaiting_on_payload(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel status --json`` carries an ``awaiting_on`` entry with
    the gate ordinal and instruction so machine readers can surface the
    same information as the human-readable view."""
    phase = tmp_path / "active" / "01-phase"
    _write_manual_task(
        phase / "parked.json",
        "parked",
        instruction="Confirm the migration is safe.",
        grader_name="confirm-migration",
    )
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_with_task(
            store,
            "parked",
            instruction="Confirm the migration is safe.",
            grader_name="confirm-migration",
        )
    finally:
        store.close()

    rc = orch_main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--json",
        ]
    )
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert len(payload) == 1
    entry = payload[0]
    assert entry["state"] == "awaiting_approval"
    assert entry["latest_status"] == "awaiting_approval"
    assert entry["awaiting_on"] == {
        "ordinal": 1,
        "instruction": "Confirm the migration is safe.",
    }


def test_collect_live_rows_includes_awaiting_approval_with_instruction(
    tmp_path: Path,
) -> None:
    """``collect_live_rows`` surfaces AWAITING_APPROVAL runs alongside
    running/validating ones and resolves the pending gate's instruction
    from the task pinned to the run via ``task_content_hash``."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_with_task(
            store,
            "parked",
            instruction="Confirm the migration is safe.",
            grader_name="confirm-migration",
        )
        rows = collect_live_rows(store)
    finally:
        store.close()

    assert len(rows) == 1
    row = rows[0]
    assert row.task_id == "parked"
    assert row.status == Status.AWAITING_APPROVAL
    assert row.awaiting_instruction == "Confirm the migration is safe."


def test_main_live_renders_awaiting_on_followup_line(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``flywheel live`` prints the awaiting state in the headline and
    the instruction on an indented follow-up line — matching the shape
    ``flywheel status`` uses so operators have one consistent surface."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting_with_task(
            store,
            "parked",
            instruction="Confirm the migration is safe.",
            grader_name="confirm-migration",
        )
    finally:
        store.close()
    rc = orch_main(["live", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out
    lines = [ln for ln in out.splitlines() if ln.strip()]
    # Two lines: the status headline plus the awaiting_on follow-up.
    assert len(lines) == 2
    head, tail = lines
    assert "parked" in head
    assert "awaiting_approval" in head
    assert tail.strip() == "awaiting_on: Confirm the migration is safe."


def test_collect_live_rows_omits_awaiting_instruction_when_task_missing(
    tmp_path: Path,
) -> None:
    """Edge case: an AWAITING_APPROVAL lifecycle whose pinned task is
    not in the tasks table (data skew / archive) still renders — the
    instruction is just absent rather than crashing the view."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        # Park the lifecycle without saving the task into the tasks
        # table — ``load_task_for_run`` returns None on this skew.
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="orphan", run_id="run-orphan-awaiting")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        lc.awaiting_manual_ordinal = 1
        lc.transition_to(Status.AWAITING_APPROVAL, now=now)
        store.create_lifecycle(lc)
        rows = collect_live_rows(store)
    finally:
        store.close()

    assert len(rows) == 1
    row = rows[0]
    assert row.status == Status.AWAITING_APPROVAL
    assert row.awaiting_instruction is None
