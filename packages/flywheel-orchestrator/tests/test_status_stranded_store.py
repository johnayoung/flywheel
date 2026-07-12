"""``flywheel status`` surfaces a landing strand from the store, not the
active listing (spec 00077, criterion 6).

An unlanded DONE run parked under a
:data:`~flywheel_core.events.LANDING_STRAND_KINDS` cause is a *landing strand*:
its verified work never merged. Until this behavior existed the strand was
rendered only for a task whose file was still under ``active/``, so archiving
the phase (or an operator moving/deleting the task file) made the strand vanish
even while it was genuinely unresolved -- visibility was an accident of phase
composition (the infrared id-14/15 shape). These tests pin the store-backed
surface: the strand renders with its park kind keyed by subject even when no
active-listing row exists for it, and ONLY an attributed resolution marker
(probe or operator) -- never archival, a plain supersession marker, or a
task-file move -- clears it. A park appended after a resolution is a fresh
recurrence and surfaces again.

Sqlite backend throughout: the same file the core store and the claim store's
stop-event ledger share, exercised through the ``status`` verb's own read path
(which routes the stop/strand reads through ``build_claim_store``).
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import pytest

from flywheel_core.events import (
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_HELD_OUT_GATE,
    PARK_KIND_STANDING_VERIFY,
    LandingParked,
)
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._claims import (
    RESOLUTION_ATTRIBUTION_OPERATOR,
    RESOLUTION_ATTRIBUTION_PROBE,
    STOP_RESOLVED,
    SqliteClaimStore,
)
from flywheel_orchestrator._workflow import main as orch_main

_T0 = datetime(2026, 7, 8, 12, 0, 0, tzinfo=timezone.utc)


# --------------------------------------------------------------------------- #
# Seed helpers                                                                 #
# --------------------------------------------------------------------------- #


def _write_task(path: Path, task_id: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )


def _seed_done(store: SqliteStore, task_id: str) -> str:
    """Persist a Lifecycle for ``task_id`` ending in DONE; return its run id."""
    run_id = f"run-{task_id}"
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=_T0)
    lc.transition_to(Status.RUNNING, now=_T0)
    lc.transition_to(Status.VALIDATING, now=_T0)
    lc.transition_to(Status.DONE, now=_T0)
    store.create_lifecycle(lc)
    return run_id


def _park(
    db: Path, run_id: str, *, park_kind: str, detail: str, ts: datetime
) -> None:
    """Append one LandingParked on ``run_id`` at ``ts`` -- what the submitter
    records when a DONE run's branch could not land."""
    store = SqliteStore(db)
    try:
        loaded = store.load_lifecycle(run_id)
        assert loaded is not None
        store.append_domain_event(
            LandingParked(
                run_id=run_id, ts=ts, park_kind=park_kind, detail=detail
            ),
            expected_version=loaded.version,
        )
    finally:
        store.close()


def _record_resolution(
    db: Path,
    *,
    subject: str,
    attribution: str,
    occurred_at: datetime,
    detail: str = "resolved",
) -> None:
    """Append a ``stop-resolved`` marker on the shared ledger."""
    claims = SqliteClaimStore(db)
    try:
        claims.record_stop_event(
            kind=STOP_RESOLVED,
            subject=subject,
            detail=detail,
            occurred_at=occurred_at,
            attribution=attribution,
        )
    finally:
        claims.close()


def _seed_strand(
    tmp_path: Path,
    *,
    task_id: str = "a",
    park_kind: str = PARK_KIND_DIVERGENT_BASE,
    detail: str = "cannot fast-forward",
    with_task_file: bool = False,
) -> Path:
    """A DONE-but-parked strand in the store whose task file is (by default)
    absent from ``active/`` -- the archived-phase / moved-file shape. Returns
    the db path."""
    db = tmp_path / "db.sqlite"
    # An empty active/ dir: the strand's task file is NOT in the listing unless
    # ``with_task_file`` asks for it.
    (tmp_path / "active" / "01").mkdir(parents=True, exist_ok=True)
    if with_task_file:
        _write_task(tmp_path / "active" / "01" / f"{task_id}.json", task_id)
    store = SqliteStore(db)
    try:
        run_id = _seed_done(store, task_id)
    finally:
        store.close()
    _park(db, run_id, park_kind=park_kind, detail=detail, ts=_T0)
    return db


def _status_json(tmp_path: Path, db: Path) -> list[dict[str, Any]]:
    rc = orch_main(
        ["status", "--tasks-dir", str(tmp_path), "--db", str(db), "--json"]
    )
    assert rc == 0
    return json.loads(_read_capsys())


def _status_text(tmp_path: Path, db: Path) -> str:
    rc = orch_main(["status", "--tasks-dir", str(tmp_path), "--db", str(db)])
    assert rc == 0
    return _read_capsys()


# capsys is injected per-test; stash it so the helpers above stay terse.
_CAP: pytest.CaptureFixture[str] | None = None


def _read_capsys() -> str:
    assert _CAP is not None
    return _CAP.readouterr().out


@pytest.fixture(autouse=True)
def _bind_capsys(capsys: pytest.CaptureFixture[str]):
    global _CAP
    _CAP = capsys
    yield
    _CAP = None


# --------------------------------------------------------------------------- #
# Criterion 6: the strand outlives the active listing                         #
# --------------------------------------------------------------------------- #


def test_strand_surfaces_by_subject_when_task_file_absent_json(
    tmp_path: Path,
) -> None:
    db = _seed_strand(tmp_path)

    payload = _status_json(tmp_path, db)
    strand = next((e for e in payload if e.get("subject") == "a"), None)
    assert strand is not None, (
        "a DONE-but-unlanded strand whose task file left the active listing "
        "did not surface on the --json status surface"
    )
    assert strand["stranded"] == {
        "park_kind": PARK_KIND_DIVERGENT_BASE,
        "detail": "cannot fast-forward",
    }


def test_strand_surfaces_by_subject_when_task_file_absent_text(
    tmp_path: Path,
) -> None:
    db = _seed_strand(tmp_path)

    text = _status_text(tmp_path, db)
    line = next(
        (ln for ln in text.splitlines() if "stranded:" in ln and "a" in ln),
        None,
    )
    assert line is not None, "the strand did not render on the text surface"
    assert PARK_KIND_DIVERGENT_BASE in line
    assert "cannot fast-forward" in line


def test_strand_visibility_independent_of_stop_kind(tmp_path: Path) -> None:
    """Every landing-strand park kind surfaces store-backed, not just one."""
    db = _seed_strand(
        tmp_path, park_kind=PARK_KIND_STANDING_VERIFY, detail="build invariant"
    )
    payload = _status_json(tmp_path, db)
    strand = next((e for e in payload if e.get("subject") == "a"), None)
    assert strand is not None
    assert strand["stranded"]["park_kind"] == PARK_KIND_STANDING_VERIFY


# --------------------------------------------------------------------------- #
# Only an attributed resolution marker clears the strand                       #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "attribution",
    [RESOLUTION_ATTRIBUTION_PROBE, RESOLUTION_ATTRIBUTION_OPERATOR],
)
def test_attributed_resolution_clears_rowless_strand(
    tmp_path: Path, attribution: str
) -> None:
    db = _seed_strand(tmp_path)
    _record_resolution(
        db,
        subject="a",
        attribution=attribution,
        occurred_at=_T0 + timedelta(hours=1),
    )

    payload = _status_json(tmp_path, db)
    assert all(e.get("subject") != "a" for e in payload), (
        f"a {attribution}-attributed resolution marker did not clear the "
        "strand from the --json surface"
    )
    text = _status_text(tmp_path, db)
    assert "stranded:" not in text


def test_plain_archival_marker_does_not_clear_strand(tmp_path: Path) -> None:
    """A plain (unattributed) ``stop-resolved`` -- the phase-archival
    supersession marker -- must NOT clear a landing strand: archiving the phase
    is not a resolution act for verified-but-unlanded work."""
    db = _seed_strand(tmp_path)
    _record_resolution(
        db,
        subject="a",
        attribution="",  # archival supersession: no attributor
        occurred_at=_T0 + timedelta(hours=1),
    )

    payload = _status_json(tmp_path, db)
    strand = next((e for e in payload if e.get("subject") == "a"), None)
    assert strand is not None, (
        "an unattributed archival-supersession marker wrongly cleared the "
        "landing strand -- only git-truth or an operator may clear it"
    )
    assert strand["stranded"]["park_kind"] == PARK_KIND_DIVERGENT_BASE


def test_recurrence_after_resolution_resurfaces(tmp_path: Path) -> None:
    """A park appended AFTER an attributed resolution is a fresh recurrence and
    surfaces again -- the marker is a supersession point, not a permanent
    mute."""
    db = _seed_strand(tmp_path)
    _record_resolution(
        db,
        subject="a",
        attribution=RESOLUTION_ATTRIBUTION_OPERATOR,
        occurred_at=_T0 + timedelta(hours=1),
    )
    # Cleared for now.
    assert all(e.get("subject") != "a" for e in _status_json(tmp_path, db))

    # A re-drive re-parks the same run after the resolution: fresh recurrence.
    _park(
        db,
        "run-a",
        park_kind=PARK_KIND_DIVERGENT_BASE,
        detail="re-parked after resolution",
        ts=_T0 + timedelta(hours=2),
    )
    payload = _status_json(tmp_path, db)
    strand = next((e for e in payload if e.get("subject") == "a"), None)
    assert strand is not None, (
        "a park appended after the resolution marker did not resurface"
    )
    assert strand["stranded"] == {
        "park_kind": PARK_KIND_DIVERGENT_BASE,
        "detail": "re-parked after resolution",
    }


# --------------------------------------------------------------------------- #
# Scope + must-not-regress guards                                              #
# --------------------------------------------------------------------------- #


def test_non_strand_park_not_surfaced_rowless(tmp_path: Path) -> None:
    """A non-strand park (held-out-gate) keeps its active-listing-only
    behavior: with the task file absent it is not surfaced store-backed, since
    the attributed-resolution vocabulary governs only landing strands."""
    db = _seed_strand(
        tmp_path, park_kind=PARK_KIND_HELD_OUT_GATE, detail="gate blocked"
    )
    payload = _status_json(tmp_path, db)
    assert all(e.get("subject") != "a" for e in payload)


def test_active_listing_strand_renders_on_row_not_duplicated(
    tmp_path: Path,
) -> None:
    """A strand whose task file is still in the listing renders on its row (the
    must-not-regress path) and is NOT also emitted as a bare-subject entry."""
    db = _seed_strand(tmp_path, with_task_file=True)

    payload = _status_json(tmp_path, db)
    row = next((e for e in payload if e.get("task_id") == "a"), None)
    assert row is not None
    assert row["stranded"] == {
        "park_kind": PARK_KIND_DIVERGENT_BASE,
        "detail": "cannot fast-forward",
    }
    # No duplicate bare-subject entry for the same strand.
    assert all("subject" not in e or e["subject"] != "a" for e in payload)


def test_clean_done_run_yields_no_strand(tmp_path: Path) -> None:
    """A DONE run that landed cleanly (no LandingParked) surfaces no strand,
    even with its task file absent."""
    db = tmp_path / "db.sqlite"
    (tmp_path / "active").mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    try:
        _seed_done(store, "a")
    finally:
        store.close()

    payload = _status_json(tmp_path, db)
    assert all(e.get("subject") != "a" for e in payload)
    text = _status_text(tmp_path, db)
    assert text.strip() == "(no active tasks)"


# --------------------------------------------------------------------------- #
# The per-row annotation honors resolutions too (weekend bug 8, item A)       #
# --------------------------------------------------------------------------- #


def test_attributed_resolution_clears_row_level_strand_annotation(
    tmp_path: Path,
) -> None:
    """A strand the operator resolved must stop rendering on its still-ACTIVE
    row exactly as it stops rendering on the rowless surface: the per-row
    ``stranded:`` annotation used to ignore resolutions entirely and render
    the strand forever."""
    db = _seed_strand(tmp_path, with_task_file=True)
    _record_resolution(
        db,
        subject="a",
        attribution=RESOLUTION_ATTRIBUTION_OPERATOR,
        occurred_at=_T0 + timedelta(minutes=1),
    )

    text = _status_text(tmp_path, db)
    assert "01/a" in text, text
    assert "stranded:" not in text, text

    rows = _status_json(tmp_path, db)
    assert [row["task_id"] for row in rows] == ["a"]
    assert "stranded" not in rows[0]


def test_fresh_park_after_resolution_surfaces_on_the_row_again(
    tmp_path: Path,
) -> None:
    """A park appended AFTER the resolution is a fresh recurrence and must
    surface again on the row -- the same newer-park-wins rule the rowless
    surface applies."""
    db = _seed_strand(tmp_path, with_task_file=True)
    _record_resolution(
        db,
        subject="a",
        attribution=RESOLUTION_ATTRIBUTION_OPERATOR,
        occurred_at=_T0 + timedelta(minutes=1),
    )
    _park(
        db,
        "run-a",
        park_kind=PARK_KIND_STANDING_VERIFY,
        detail="fresh recurrence",
        ts=_T0 + timedelta(minutes=2),
    )

    text = _status_text(tmp_path, db)
    assert "stranded: standing-verify" in text, text


def test_rowless_lines_carry_the_no_active_row_marker(tmp_path: Path) -> None:
    """Item B: a subject with no active row renders at the same indent as a
    task row, so it must carry the ``[no active row]`` marker -- otherwise a
    reader cannot tell a trailing subject line from a continuation of the
    row printed above it."""
    db = _seed_strand(tmp_path)
    text = _status_text(tmp_path, db)
    assert "a [no active row]  stranded:" in text, text
