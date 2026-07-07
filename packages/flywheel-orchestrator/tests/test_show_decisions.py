"""The run-detail ``show`` surface promotes landing decisions (spec 00073).

Every landing-stage decision the loop recorded on a run -- the held-out gate
verdict with its per-grader output, the land-park witness, the positive
landing, and the re-drive disposition -- is projected onto ``flywheel show``
(JSON and text) straight from the domain-event ledger. Because they are read
from the store, never from the run's telemetry JSONL, they survive deletion of
that file (criterion 8); and because the excerpts they carry can hold captured
credential material, the default rendering scrubs secret-shaped values while
``--raw`` reveals them verbatim (criterion 10).

These tests seed a terminal lifecycle in a file-backed store, append the four
decision events directly to its ledger, and drive the real ``show`` CLI, so the
assertions pin the operator-visible surface end to end.
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import pytest

from flywheel_core.events import (
    DomainEvent,
    GateGraderReceipt,
    HeldOutGateEvaluated,
    Landed,
    LandingParked,
    LandingRedriven,
)
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._history import collect_run_detail
from flywheel_orchestrator._workflow import main as orch_main

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)

# A GitHub personal-access-token-shaped value: matched by the default
# redaction policy's ``github_token`` pattern (``gh[pousr]_[A-Za-z0-9]{20,}``).
_SECRET = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"
_REDACTED_TOKEN = "[REDACTED:github_token]"


def _seed_done(
    store: SqliteStore, task_id: str, *, run_id: str, at: datetime = _T0
) -> Lifecycle:
    """Persist a DONE lifecycle (landing runs after the run finalized)."""
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=at)
    lc.transition_to(Status.RUNNING, now=at + timedelta(seconds=1))
    lc.transition_to(Status.VALIDATING, now=at + timedelta(seconds=2))
    lc.transition_to(Status.DONE, now=at + timedelta(minutes=5))
    store.create_lifecycle(lc)
    return lc


def _append(
    store: SqliteStore, lc: Lifecycle, event: DomainEvent
) -> Lifecycle:
    """Append one identity-fold decision event; return the new lifecycle.

    Landing events fold as the identity (DONE stays DONE) but still advance
    the optimistic-concurrency ``version``, so callers thread the returned
    lifecycle into the next append.
    """
    return store.append_domain_event(event, expected_version=lc.version)


def _seed_full_decision_stream(
    store: SqliteStore, *, run_id: str, excerpt: str = "GATEFAIL_DIAG"
) -> None:
    """Seed a DONE run whose ledger carries all four decision kinds."""
    lc = _seed_done(store, "t-decide", run_id=run_id)
    lc = _append(
        store,
        lc,
        HeldOutGateEvaluated(
            run_id=run_id,
            ts=_T0 + timedelta(minutes=6),
            outcome="fail",
            reason="held-out oracle failed",
            receipts=(
                GateGraderReceipt(
                    grader_name="oracle-x",
                    passed=False,
                    output_excerpt=excerpt,
                ),
            ),
        ),
    )
    lc = _append(
        store,
        lc,
        LandingParked(
            run_id=run_id,
            ts=_T0 + timedelta(minutes=6, seconds=1),
            park_kind="held-out-gate",
            detail="gate blocked the land",
        ),
    )
    lc = _append(
        store,
        lc,
        LandingRedriven(
            run_id=run_id,
            ts=_T0 + timedelta(minutes=7),
            result="landed",
            park_kind="held-out-gate",
        ),
    )
    _append(
        store,
        lc,
        Landed(
            run_id=run_id,
            ts=_T0 + timedelta(minutes=7, seconds=1),
            strategy="merge",
            landed_ref="abc123",
        ),
    )


def _show_json(
    db: Path,
    tmp_path: Path,
    run_or_task: str,
    capsys: "pytest.CaptureFixture[str]",
    *extra: str,
) -> tuple[int, Any]:
    rc = orch_main(
        [
            "show",
            run_or_task,
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
            "--json",
            *extra,
        ]
    )
    return rc, json.loads(capsys.readouterr().out)


# --- retrieval halves of criteria 1/2/3/5 -----------------------------------


def test_show_json_surfaces_every_decision_in_ledger_order(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(store, run_id="run-1")
    store.close()

    rc, payload = _show_json(db, tmp_path, "run-1", capsys)
    assert rc == 0
    decisions = payload["decisions"]
    assert isinstance(decisions, list)

    # Every landing decision, in ascending-sequence (append) order.
    assert [d["kind"] for d in decisions] == [
        "held_out_gate_evaluated",
        "landing_parked",
        "landing_redriven",
        "landed",
    ]

    # Criterion 1: the gate verdict carries its outcome and each executed
    # grader's receipt (name, pass/fail, output excerpt).
    gate = decisions[0]
    assert gate["outcome"] == "fail"
    assert gate["reason"] == "held-out oracle failed"
    (receipt,) = gate["receipts"]
    assert receipt["grader_name"] == "oracle-x"
    assert receipt["passed"] is False
    assert "GATEFAIL_DIAG" in receipt["output_excerpt"]

    # Criterion 2: the park witness carries its cause and human-readable detail.
    park = decisions[1]
    assert park["park_kind"] == "held-out-gate"
    assert park["detail"] == "gate blocked the land"

    # Criterion 5: the re-drive disposition carries its result and the park it
    # was clearing.
    redriven = decisions[2]
    assert redriven["result"] == "landed"
    assert redriven["park_kind"] == "held-out-gate"

    # Criterion 3: the positive landing carries its strategy and landed ref.
    landed = decisions[3]
    assert landed["strategy"] == "merge"
    assert landed["landed_ref"] == "abc123"

    # Ledger coordinates ride along on every record.
    assert [d["sequence"] for d in decisions] == sorted(
        d["sequence"] for d in decisions
    )


def test_show_by_task_id_surfaces_decisions(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    """A task-id argument resolves to its latest run and shows its decisions."""
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(store, run_id="run-1")
    store.close()

    rc, payload = _show_json(db, tmp_path, "t-decide", capsys)
    assert rc == 0
    assert [d["kind"] for d in payload["decisions"]] == [
        "held_out_gate_evaluated",
        "landing_parked",
        "landing_redriven",
        "landed",
    ]


# --- criterion 8: decisions come from the store, not the telemetry JSONL -----


def test_decisions_survive_deleted_run_jsonl(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(store, run_id="run-1")
    store.close()

    # A run's telemetry JSONL is a separate artifact from the domain-event
    # ledger. Delete it: the decisions are read from the store, so the surface
    # is unaffected.
    telemetry = tmp_path / "runs" / "run-1.jsonl"
    telemetry.parent.mkdir(parents=True)
    telemetry.write_text('{"kind": "harness.iteration_completed"}\n')
    telemetry.unlink()
    assert not telemetry.exists()

    rc, payload = _show_json(db, tmp_path, "run-1", capsys)
    assert rc == 0
    assert [d["kind"] for d in payload["decisions"]] == [
        "held_out_gate_evaluated",
        "landing_parked",
        "landing_redriven",
        "landed",
    ]

    # The retrieval path reads the ledger directly, independent of any file.
    store = SqliteStore(db)
    try:
        detail = collect_run_detail(store, "run-1")
    finally:
        store.close()
    assert detail is not None
    assert len(detail.decisions) == 4


# --- criterion 10: secret-shaped values redacted by default, raw on demand ---


def test_show_json_redacts_secret_in_excerpt_by_default(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(
        store,
        run_id="run-1",
        excerpt=f"grader stderr near {_SECRET} then more",
    )
    store.close()

    rc, payload = _show_json(db, tmp_path, "run-1", capsys)
    assert rc == 0
    excerpt = payload["decisions"][0]["receipts"][0]["output_excerpt"]
    # The secret-shaped token is scrubbed; the surrounding excerpt survives.
    assert _SECRET not in excerpt
    assert _REDACTED_TOKEN in excerpt
    assert "grader stderr near" in excerpt
    # And nowhere in the whole rendered payload does the raw secret leak.
    assert _SECRET not in json.dumps(payload)


def test_show_json_raw_flag_reveals_secret(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(
        store,
        run_id="run-1",
        excerpt=f"grader stderr near {_SECRET} then more",
    )
    store.close()

    rc, payload = _show_json(db, tmp_path, "run-1", capsys, "--raw")
    assert rc == 0
    excerpt = payload["decisions"][0]["receipts"][0]["output_excerpt"]
    # --raw opts back into the stored value verbatim for authorized forensics.
    assert _SECRET in excerpt
    assert _REDACTED_TOKEN not in excerpt


def test_show_text_renders_and_redacts_decisions(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_full_decision_stream(
        store,
        run_id="run-1",
        excerpt=f"stderr {_SECRET} tail",
    )
    store.close()

    rc = orch_main(
        [
            "show",
            "run-1",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    # The text report grows a decisions section naming each disposition.
    assert "decisions:" in out
    assert "parked" in out
    assert "landed" in out
    assert "redriven" in out
    # Text view redacts identically to the JSON view.
    assert _SECRET not in out
    assert _REDACTED_TOKEN in out

    # --raw reveals the excerpt verbatim in the text view too.
    rc = orch_main(
        [
            "show",
            "run-1",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
            "--raw",
        ]
    )
    raw_out = capsys.readouterr().out
    assert rc == 0
    assert _SECRET in raw_out


# --- edge case: a run with no landing decisions renders cleanly --------------


def test_show_run_without_decisions_renders_empty(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_done(store, "t-plain", run_id="run-1")
    store.close()

    rc, payload = _show_json(db, tmp_path, "run-1", capsys)
    assert rc == 0
    assert payload["decisions"] == []

    # The text view of a decision-less run omits the section without error.
    rc = orch_main(
        [
            "show",
            "run-1",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "decisions:" not in out
