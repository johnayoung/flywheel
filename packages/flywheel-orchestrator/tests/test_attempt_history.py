"""Per-attempt grader history in the run-detail surface (spec 00073, D-4).

Criterion 6: the per-run record includes *every* attempt's grader verdicts
keyed to their attempt, not only the last attempt's. Criterion 7: the run
window and totals span all attempts (``tokens_total`` sums every attempt,
``started_at`` is no later than attempt 1's start).

The fixture is a two-attempt run whose attempt 1 grader outcome differs from
attempt 2's (grader ``alpha`` FAILs on attempt 1 and PASSes on attempt 2), so
an implementation that duplicated the last attempt's receipts across every
attempt would be caught by these assertions.
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._history import collect_run_detail
from flywheel_orchestrator._workflow import _run_detail_to_dict
from flywheel_orchestrator._workflow import main as orch_main

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
_ATTEMPT1_START = _T0 + timedelta(seconds=1)


def _grader(
    run_id: str,
    attempt_number: int,
    *,
    name: str,
    passed: bool,
    ordinal: int = 0,
) -> GraderResultRecord:
    return GraderResultRecord(
        run_id=run_id,
        attempt_number=attempt_number,
        ordinal=ordinal,
        grader_type="command",
        grader_spec={"type": "command", "run": "true"},
        passed=passed,
        duration_ms=5,
        payload={"exit_code": 0 if passed else 1},
        ts=_T0,
        grader_name=name,
    )


def _seed_two_attempt_run(
    store: SqliteStore, *, run_id: str, task_id: str, source: str = ""
) -> None:
    """A run that failed verification on attempt 1 and passed on attempt 2.

    Attempt 1's grader ``alpha`` FAILs; attempt 2's ``alpha`` PASSes and a
    second grader ``beta`` PASSes. The retry re-enters READY/RUNNING/
    VALIDATING, overwriting those pre-retry stamps, so only attempt 1's own
    start still carries the true run start (pins criterion 7).
    """
    lc = Lifecycle(task_id=task_id, run_id=run_id, source=source)
    lc.transition_to(Status.READY, now=_T0)
    lc.transition_to(Status.RUNNING, now=_ATTEMPT1_START)
    lc.transition_to(Status.VALIDATING, now=_T0 + timedelta(minutes=5))
    lc.transition_to(
        Status.FAILED_VALIDATION,
        error="graders failed",
        now=_T0 + timedelta(minutes=6),
    )
    lc.transition_to(Status.READY, now=_T0 + timedelta(minutes=10))
    lc.transition_to(Status.RUNNING, now=_T0 + timedelta(minutes=11))
    lc.transition_to(Status.VALIDATING, now=_T0 + timedelta(minutes=15))
    lc.transition_to(Status.DONE, now=_T0 + timedelta(minutes=16))
    store.create_lifecycle(lc)

    store.save_attempt(
        run_id,
        Attempt(
            number=1,
            run_id=run_id,
            started_at=_ATTEMPT1_START,
            ended_at=_T0 + timedelta(minutes=6),
            outcome=Outcome.VALIDATION_FAILED,
            input_tokens=1000,
            iterations_completed=1,
            turns=5,
            total_cost_usd=0.5,
        ),
    )
    store.save_attempt(
        run_id,
        Attempt(
            number=2,
            run_id=run_id,
            started_at=_T0 + timedelta(minutes=11),
            ended_at=_T0 + timedelta(minutes=16),
            outcome=Outcome.SUCCEEDED,
            input_tokens=400,
            iterations_completed=1,
            turns=3,
            total_cost_usd=0.25,
        ),
    )
    store.append_grader_result(_grader(run_id, 1, name="alpha", passed=False))
    store.append_grader_result(_grader(run_id, 2, name="alpha", passed=True))
    store.append_grader_result(
        _grader(run_id, 2, name="beta", passed=True, ordinal=1)
    )


# --- data model: collect_run_detail ------------------------------------------


def test_run_detail_keys_grader_verdicts_by_attempt() -> None:
    store = SqliteStore(":memory:")
    _seed_two_attempt_run(store, run_id="run-x", task_id="t1")

    detail = collect_run_detail(store, "run-x")
    assert detail is not None

    by_attempt = {a.number: a for a in detail.attempts}
    assert set(by_attempt) == {1, 2}

    # Every receipt is keyed to its own attempt: the discarded attempt 1
    # keeps its FAIL verdict; attempt 2 keeps its two PASS verdicts.
    assert [
        (g.grader_name, g.passed) for g in by_attempt[1].grader_results
    ] == [("alpha", False)]
    assert [
        (g.grader_name, g.passed) for g in by_attempt[2].grader_results
    ] == [("alpha", True), ("beta", True)]
    assert all(g.attempt_number == 1 for g in by_attempt[1].grader_results)
    assert all(g.attempt_number == 2 for g in by_attempt[2].grader_results)

    # Duplicating the last attempt across attempts would make attempt 1's
    # alpha read PASS; the differing outcome catches that.
    assert by_attempt[1].grader_results[0].passed is False
    assert by_attempt[2].grader_results[0].passed is True

    # The flat view carries every attempt's receipts, not just the last.
    assert {
        (g.attempt_number, g.grader_name) for g in detail.grader_results
    } == {(1, "alpha"), (2, "alpha"), (2, "beta")}


def test_run_detail_window_and_totals_span_all_attempts() -> None:
    store = SqliteStore(":memory:")
    _seed_two_attempt_run(store, run_id="run-x", task_id="t1")

    detail = collect_run_detail(store, "run-x")
    assert detail is not None

    # tokens_total sums every attempt (1000 + 400).
    assert detail.run.tokens_total == 1400
    # started_at is no later than attempt 1's start despite the retry
    # overwriting the pre-retry READY/RUNNING stamps.
    assert detail.run.started_at is not None
    assert detail.run.started_at <= _ATTEMPT1_START


# --- CLI JSON: flywheel show --json ------------------------------------------


def test_show_json_nests_grader_results_per_attempt(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    detail_source = str(tmp_path / "tasks/active/30-a/t1.json")
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_two_attempt_run(
        store, run_id="run-x", task_id="t1", source=detail_source
    )
    store.close()

    rc = orch_main(
        [
            "show",
            "run-x",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
            "--json",
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    payload = json.loads(out)

    # Criterion 7: window + totals span all attempts.
    assert payload["run"]["tokens_total"] == 1400

    # Criterion 6: each attempt's verdicts keyed to its own attempt number.
    attempts = {a["number"]: a for a in payload["attempts"]}
    assert [
        (g["grader_name"], g["passed"])
        for g in attempts[1]["grader_results"]
    ] == [("alpha", False)]
    assert [
        (g["grader_name"], g["passed"])
        for g in attempts[2]["grader_results"]
    ] == [("alpha", True), ("beta", True)]


def test_run_detail_dict_nests_grader_results_per_attempt() -> None:
    store = SqliteStore(":memory:")
    _seed_two_attempt_run(store, run_id="run-x", task_id="t1")
    detail = collect_run_detail(store, "run-x")
    assert detail is not None

    payload = _run_detail_to_dict(detail)
    # Round-trip through JSON to prove the payload is serializable.
    payload = json.loads(json.dumps(payload))

    attempts = {a["number"]: a for a in payload["attempts"]}
    assert set(attempts) == {1, 2}
    assert [
        (g["grader_name"], g["passed"])
        for g in attempts[1]["grader_results"]
    ] == [("alpha", False)]
    assert [
        (g["grader_name"], g["passed"])
        for g in attempts[2]["grader_results"]
    ] == [("alpha", True), ("beta", True)]
    assert all(
        g["attempt_number"] == 1 for g in attempts[1]["grader_results"]
    )
    assert all(
        g["attempt_number"] == 2 for g in attempts[2]["grader_results"]
    )


def test_show_text_prints_graders_under_each_attempt(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_two_attempt_run(store, run_id="run-x", task_id="t1")
    store.close()

    rc = orch_main(
        [
            "show",
            "run-x",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    # Both attempts and their differing verdicts render.
    assert "1  validation_failed" in out
    assert "2  succeeded" in out
    assert "FAIL" in out
    assert "pass" in out
