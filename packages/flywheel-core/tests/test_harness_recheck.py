"""Contract tests for :func:`flywheel_core.harness.recheck_blocked_lifecycle`.

The recheck primitive re-evaluates a blocked lifecycle's persisted
``requires`` predicates against the worker CWD/env and, when every
predicate is satisfied (and ``dry_run`` is ``False``), transitions
``INTERRUPTED -> READY`` via the same optimistic-concurrency path the
rest of the harness uses.

Every test runs against both :class:`InMemoryStore` and
:class:`SqliteStore` via parametrization so the contract holds across
both stores the harness officially supports.
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from flywheel_core.store_protocols import TelemetryRecord

from flywheel_core import (
    CommandGrader,
    CommandGraderRequirement,
    EnvVarSetRequirement,
    FileExistsRequirement,
    InMemoryStore,
    Lifecycle,
    RecheckOutcome,
    SqliteStore,
    Status,
    Task,
    recheck_blocked_lifecycle,
)


# --- Fixtures -------------------------------------------------------------


def _make_store(kind: str, tmp_path: Path) -> Any:
    if kind == "memory":
        return InMemoryStore()
    if kind == "sqlite":
        return SqliteStore(tmp_path / "recheck.db")
    raise ValueError(f"unknown store kind: {kind!r}")


def _blocked_lifecycle(
    store: Any,
    *,
    run_id: str,
    requires_payload: list[dict[str, Any]] | None,
    status: Status = Status.INTERRUPTED,
) -> Lifecycle:
    """Build a Lifecycle directly in ``status`` with the supplied
    ``blocked_requires_json`` payload, bypassing the full harness drive
    so the recheck tests stay focused on the primitive.
    """
    lifecycle = Lifecycle(task_id="t1", run_id=run_id, status=status)
    if requires_payload is not None:
        lifecycle.blocked_requires_json = json.dumps(requires_payload)
    if status not in lifecycle.timestamps:
        lifecycle.timestamps[status] = datetime(
            2026, 5, 28, tzinfo=timezone.utc
        )
    store.create_lifecycle(lifecycle)
    return lifecycle


class _ListSink:
    """In-memory TelemetrySink capturing recheck telemetry in order."""

    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)


def _events_of(sink: _ListSink, run_id: str, *kinds: str) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for record in sink.records:
        if record.run_id != run_id:
            continue
        if not record.kind.startswith("harness."):
            continue
        if not kinds or record.kind in kinds:
            out.append(
                {"kind": record.kind, "payload": dict(record.payload)}
            )
    return out


def _grader_rows(store: Any, run_id: str) -> int:
    rows = store.list_grader_results(run_id, attempt_number=1)
    return len(rows)


STORE_KINDS = ["memory", "sqlite"]


# --- Helpers --------------------------------------------------------------


def _exit_ok_cmd() -> str:
    return f"{sys.executable} -c 'raise SystemExit(0)'"


def _exit_fail_cmd() -> str:
    return f"{sys.executable} -c 'raise SystemExit(7)'"


# --- All-satisfied path ---------------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_all_satisfied_transitions_clears_column_and_emits_both_events(
    store_kind: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    store = _make_store(store_kind, tmp_path)
    monkeypatch.setenv("RECHECK_TEST_VAR", "1")
    (tmp_path / "present.flag").write_text("ok", encoding="utf-8")
    monkeypatch.chdir(tmp_path)

    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-ok",
        requires_payload=[
            {"type": "command_grader", "name": "full-suite"},
            {"type": "file_exists", "path": "present.flag", "present": True},
            {"type": "env_var_set", "name": "RECHECK_TEST_VAR"},
        ],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is True
    assert outcome.reason == "unblocked"
    assert all(bool(p["satisfied"]) for p in outcome.per_predicate)

    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.READY
    assert reloaded.blocked_requires_json is None

    events = _events_of(
        sink, lifecycle.run_id, "harness.recheck_attempted", "harness.unblocked"
    )
    kinds = [e["kind"] for e in events]
    assert kinds == ["harness.recheck_attempted", "harness.unblocked"]
    recheck_payload = events[0]["payload"]
    assert recheck_payload["all_satisfied"] is True
    assert recheck_payload["dry_run"] is False
    unblocked_payload = events[1]["payload"]
    assert unblocked_payload == {
        "from_status": "interrupted",
        "to_status": "ready",
    }


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_cwd_argument_resolves_predicates_against_sandbox(
    store_kind: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The relative file_exists path and the command grader must be
    # evaluated against the passed-in sandbox cwd, NOT the process CWD.
    # Regression guard for the orchestrator threading the per-task sandbox
    # into recheck_blocked_lifecycle (in-process callers cannot chdir).
    sandbox = tmp_path / "sandbox"
    sandbox.mkdir()
    (sandbox / "present.flag").write_text("ok", encoding="utf-8")
    # Process CWD is deliberately elsewhere and lacks the flag, so a
    # regression to os.getcwd() would leave the predicate unsatisfied.
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)

    store = _make_store(store_kind, tmp_path)
    task = Task(
        goal="g",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c "
                f"\"import os,sys; sys.exit(0 if "
                f"os.path.exists('present.flag') else 1)\"",
                name="full-suite",
            )
        ],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-cwd",
        requires_payload=[
            {"type": "command_grader", "name": "full-suite"},
            {"type": "file_exists", "path": "present.flag", "present": True},
        ],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, cwd=sandbox, sink=sink
    )

    assert outcome.applied is True
    assert all(bool(p["satisfied"]) for p in outcome.per_predicate)


# --- Partial satisfied path ----------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_any_unsatisfied_emits_only_recheck_attempted_and_preserves_column(
    store_kind: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    store = _make_store(store_kind, tmp_path)
    monkeypatch.delenv("RECHECK_TEST_VAR", raising=False)
    monkeypatch.chdir(tmp_path)

    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    payload = [
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "RECHECK_TEST_VAR"},
    ]
    lifecycle = _blocked_lifecycle(
        store, run_id="run-partial", requires_payload=payload
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.reason == "unsatisfied"
    sat_by_type = {p["type"]: p["satisfied"] for p in outcome.per_predicate}
    assert sat_by_type == {"command_grader": True, "env_var_set": False}

    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json == json.dumps(payload)

    events = _events_of(
        sink, lifecycle.run_id, "harness.recheck_attempted", "harness.unblocked"
    )
    kinds = [e["kind"] for e in events]
    assert kinds == ["harness.recheck_attempted"]
    assert events[0]["payload"]["all_satisfied"] is False


# --- Not blocked paths ---------------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_non_interrupted_lifecycle_is_silent_noop(
    store_kind: str, tmp_path: Path
) -> None:
    store = _make_store(store_kind, tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    # READY is a legal non-interrupted status for which recheck must be
    # an unconditional no-op (no events, no transitions).
    lifecycle = Lifecycle(task_id="t1", run_id="run-ready", status=Status.READY)
    lifecycle.timestamps[Status.READY] = datetime(
        2026, 5, 28, tzinfo=timezone.utc
    )
    lifecycle.blocked_requires_json = json.dumps(
        [{"type": "env_var_set", "name": "X"}]
    )
    store.create_lifecycle(lifecycle)

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome == RecheckOutcome(
        applied=False, reason="not_blocked", per_predicate=()
    )
    assert _events_of(sink, lifecycle.run_id) == []
    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.READY


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_interrupted_without_blocked_requires_json_is_silent_noop(
    store_kind: str, tmp_path: Path
) -> None:
    """SIGINT-paused lifecycles land in INTERRUPTED without populating
    ``blocked_requires_json``. The recheck primitive must skip them
    silently so the existing run_task entry-time normalization remains
    the recovery path for that class."""
    store = _make_store(store_kind, tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    lifecycle = _blocked_lifecycle(
        store, run_id="run-sigint", requires_payload=None
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome == RecheckOutcome(
        applied=False, reason="not_blocked", per_predicate=()
    )
    assert _events_of(sink, lifecycle.run_id) == []
    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is None


# --- command_grader execution & detail -----------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_command_grader_predicate_runs_named_grader_and_surfaces_exit_code(
    store_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    store = _make_store(store_kind, tmp_path)
    monkeypatch.chdir(tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_fail_cmd(), name="full-suite")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-runs",
        requires_payload=[{"type": "command_grader", "name": "full-suite"}],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.reason == "unsatisfied"
    assert len(outcome.per_predicate) == 1
    predicate = outcome.per_predicate[0]
    assert predicate["type"] == "command_grader"
    assert predicate["identifier"] == "full-suite"
    assert predicate["satisfied"] is False
    assert predicate["detail"] == "exit_code=7"


# --- command_grader missing ----------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_command_grader_predicate_for_unknown_name_is_unsatisfied(
    store_kind: str, tmp_path: Path
) -> None:
    """The persisted blocked_requires_json may reference a grader name
    that has since been removed from the task definition. Spec FR-5 says
    treat as unsatisfied with detail 'grader not found' and emit only
    harness.recheck_attempted (no transition)."""
    store = _make_store(store_kind, tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="other")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-missing",
        requires_payload=[
            {"type": "command_grader", "name": "full-suite"}
        ],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.reason == "unsatisfied"
    predicate = outcome.per_predicate[0]
    assert predicate["satisfied"] is False
    assert predicate["detail"] == "grader not found"

    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is not None
    events = _events_of(
        sink, lifecycle.run_id, "harness.recheck_attempted", "harness.unblocked"
    )
    assert [e["kind"] for e in events] == ["harness.recheck_attempted"]


# --- Dry-run --------------------------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_dry_run_emits_recheck_attempted_only_even_when_all_satisfied(
    store_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    store = _make_store(store_kind, tmp_path)
    monkeypatch.setenv("RECHECK_TEST_VAR", "1")
    monkeypatch.chdir(tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    payload = [
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "RECHECK_TEST_VAR"},
    ]
    lifecycle = _blocked_lifecycle(
        store, run_id="run-dry", requires_payload=payload
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, dry_run=True, sink=sink
    )

    assert outcome.applied is False
    assert outcome.reason == "dry_run"
    assert all(bool(p["satisfied"]) for p in outcome.per_predicate)

    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json == json.dumps(payload)

    events = _events_of(
        sink, lifecycle.run_id, "harness.recheck_attempted", "harness.unblocked"
    )
    assert [e["kind"] for e in events] == ["harness.recheck_attempted"]
    assert events[0]["payload"]["dry_run"] is True
    assert events[0]["payload"]["all_satisfied"] is True


# --- No grader_results side-effect ---------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_command_grader_recheck_does_not_write_grader_results_row(
    store_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Per spec FR-5 Out of Scope: recheck is a control-plane operation;
    the audit surface is the event payload, not grader_results."""
    store = _make_store(store_kind, tmp_path)
    monkeypatch.chdir(tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="full-suite")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-no-rows",
        requires_payload=[
            {"type": "command_grader", "name": "full-suite"}
        ],
    )

    sink = _ListSink()
    recheck_blocked_lifecycle(store, lifecycle.run_id, task, sink=sink)

    assert _grader_rows(store, lifecycle.run_id) == 0


# --- env_var_set semantics -----------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_env_var_set_empty_string_is_unsatisfied(
    store_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Spec FR-2 edge case: set-but-empty fails the non-empty rule."""
    store = _make_store(store_kind, tmp_path)
    monkeypatch.setenv("RECHECK_TEST_VAR", "")
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="x")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-empty-env",
        requires_payload=[{"type": "env_var_set", "name": "RECHECK_TEST_VAR"}],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.per_predicate[0]["satisfied"] is False


# --- file_exists.present=False --------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_file_exists_present_false_is_unsatisfied_when_path_present(
    store_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    store = _make_store(store_kind, tmp_path)
    (tmp_path / "must-not-exist").write_text("oops", encoding="utf-8")
    monkeypatch.chdir(tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="x")],
    )
    lifecycle = _blocked_lifecycle(
        store,
        run_id="run-absent",
        requires_payload=[
            {
                "type": "file_exists",
                "path": "must-not-exist",
                "present": False,
            }
        ],
    )

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.per_predicate[0]["satisfied"] is False


# --- Corrupted snapshot ---------------------------------------------------


@pytest.mark.parametrize("store_kind", STORE_KINDS)
def test_corrupted_blocked_requires_json_surfaces_parse_error(
    store_kind: str, tmp_path: Path
) -> None:
    """Unparseable persisted snapshot is data corruption: do not crash;
    surface as RecheckOutcome(applied=False) and record the failure in
    the recheck_attempted payload."""
    store = _make_store(store_kind, tmp_path)
    task = Task(
        goal="g",
        graders=[CommandGrader(run=_exit_ok_cmd(), name="x")],
    )
    lifecycle = Lifecycle(
        task_id="t1", run_id="run-corrupt", status=Status.INTERRUPTED
    )
    lifecycle.timestamps[Status.INTERRUPTED] = datetime(
        2026, 5, 28, tzinfo=timezone.utc
    )
    lifecycle.blocked_requires_json = "{not json"
    store.create_lifecycle(lifecycle)

    sink = _ListSink()
    outcome = recheck_blocked_lifecycle(
        store, lifecycle.run_id, task, sink=sink
    )

    assert outcome.applied is False
    assert outcome.reason.startswith("parse_error:")
    assert outcome.per_predicate[0]["type"] == "parse_error"

    reloaded = store.load_lifecycle(lifecycle.run_id)
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED

    events = _events_of(
        sink, lifecycle.run_id, "harness.recheck_attempted", "harness.unblocked"
    )
    assert [e["kind"] for e in events] == ["harness.recheck_attempted"]


# --- Round-trip envelope-side dataclasses --------------------------------


def test_blocked_requirement_classes_round_trip_through_module_surface() -> None:
    """Sanity: the public dataclasses survive a direct construction
    round-trip — the recheck path leans on these instances, so the
    contract is worth pinning down at the test layer."""
    cg = CommandGraderRequirement(name="x")
    fe = FileExistsRequirement(path="p", present=False)
    ev = EnvVarSetRequirement(name="V")
    assert cg.type == "command_grader"
    assert fe.type == "file_exists"
    assert ev.type == "env_var_set"
