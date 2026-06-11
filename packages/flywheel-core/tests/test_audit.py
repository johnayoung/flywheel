"""Tests for the ``flywheel_core.audit`` streaming reader and logger emitter.

The spec lists five acceptance criteria for this module; this file
covers each one against the in-memory store substrate so the library
suite stays sqlite/postgres-independent.
"""

from __future__ import annotations

import io
import json
import logging
import re
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from flywheel_core import (
    AuditLoggerHandle,
    EventRecord,
    InMemoryStore,
    Lifecycle,
    SdkMessageRecord,
    SqliteStore,
    Status,
    attach_logger,
    stream,
)
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.telemetry_file import FileTelemetrySink

from flywheel_core.audit._cli import (
    PREVIEW_MAX_CHARS,
    TRUNCATION_HINT,
    main as cli_main,
)


def _make_store_with_lifecycle(run_id: str = "r1") -> InMemoryStore:
    store = InMemoryStore()
    store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
    # Advance to RUNNING so the harness-style writes below don't pretend
    # the lifecycle is still PENDING. The audit stream itself doesn't
    # care about the source status, only the terminal predicate.
    lc = store.load_lifecycle(run_id)
    assert lc is not None
    lc.transition_to(Status.READY)
    store.update_lifecycle(lc, expected_version=1)
    lc = store.load_lifecycle(run_id)
    assert lc is not None
    lc.transition_to(Status.RUNNING)
    store.update_lifecycle(lc, expected_version=2)
    return store


def _append_event(
    sink: FileTelemetrySink, run_id: str, kind: str
) -> None:
    """Write one harness telemetry line to the run's JSONL file."""
    sink.append_telemetry(
        TelemetryRecord(
            run_id=run_id,
            ts=datetime.now(timezone.utc),
            kind=kind,
            payload={"k": kind},
        )
    )


def _save_sdk(
    sink: FileTelemetrySink,
    run_id: str,
    payloads: list[dict[str, Any]],
    *,
    attempt_number: int = 1,
    iteration_number: int = 1,
) -> None:
    """Write one SDK-message line per payload, the sink-era shape."""
    for payload in payloads:
        sink.append_telemetry(
            TelemetryRecord(
                run_id=run_id,
                ts=datetime.now(timezone.utc),
                kind=str(payload.get("message_type", "assistant")),
                payload=payload,
                attempt_number=attempt_number,
                iteration_number=iteration_number,
            )
        )


# --- (a) stream(follow=False) drains everything in sequence order -----------


def test_stream_drains_mixed_records_in_sequence_order(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.iteration_start")
    _save_sdk(sink, "r1", [{"message_type": "assistant", "text": "hi"}])
    _append_event(sink, "r1", "harness.envelope_observed")
    _save_sdk(
        sink,
        "r1",
        [
            {"message_type": "tool_use", "name": "Read"},
            {"message_type": "tool_result", "content": "ok"},
        ],
    )
    _append_event(sink, "r1", "harness.iteration_end")

    collected = list(stream("r1", store=store, logs_root=logs_root))

    # Six records (3 events + 3 SDK messages) total.
    assert len(collected) == 6
    sequences = [r.sequence for r in collected]
    assert sequences == sorted(s for s in sequences if s is not None)
    assert sequences == [1, 2, 3, 4, 5, 6]

    # Type interleaving matches the write order.
    assert isinstance(collected[0], EventRecord)
    assert isinstance(collected[1], SdkMessageRecord)
    assert isinstance(collected[2], EventRecord)
    assert isinstance(collected[3], SdkMessageRecord)
    assert isinstance(collected[4], SdkMessageRecord)
    assert isinstance(collected[5], EventRecord)


# --- (b) unknown run_id yields nothing --------------------------------------


def test_stream_unknown_run_id_yields_nothing(tmp_path: Path) -> None:
    store = InMemoryStore()
    assert list(stream(
        "does-not-exist", store=store, logs_root=tmp_path / "logs"
    )) == []


def test_stream_unknown_run_id_with_follow_false_terminates(tmp_path: Path) -> None:
    store = InMemoryStore()
    # follow=False on an unknown run should also return immediately
    # without spinning or polling.
    assert list(stream(
        "nope", store=store, logs_root=tmp_path / "logs", follow=False
    )) == []


# --- (c) follow=True observes concurrent writes and exits on terminal --------


def test_stream_follow_exits_when_lifecycle_reaches_terminal(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    collected: list[Any] = []
    errors: list[BaseException] = []
    consumer_started = threading.Event()

    def consume() -> None:
        consumer_started.set()
        try:
            for record in stream(
                "r1",
                store=store,
                logs_root=logs_root,
                follow=True,
                poll_interval=0.02,
            ):
                collected.append(record)
        except BaseException as exc:  # pragma: no cover - surfaced by assert
            errors.append(exc)

    thread = threading.Thread(target=consume)
    thread.start()
    consumer_started.wait(timeout=1.0)

    # Write records while the consumer thread is following.
    _append_event(sink, "r1", "harness.iteration_start")
    time.sleep(0.05)
    _save_sdk(sink, "r1", [{"message_type": "assistant", "text": "ok"}])
    time.sleep(0.05)
    _append_event(sink, "r1", "harness.iteration_end")
    time.sleep(0.05)

    # Transition to a terminal lifecycle state so the follow loop exits.
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.VALIDATING)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.DONE)
    store.update_lifecycle(lc, expected_version=lc.version - 1)

    thread.join(timeout=2.0)
    assert not thread.is_alive(), "follow loop did not exit on DONE"
    assert not errors, f"consumer raised: {errors}"

    # We should have observed every persisted record exactly once.
    assert len(collected) == 3
    assert [r.sequence for r in collected] == [1, 2, 3]


def test_stream_follow_against_already_terminal_run_returns_after_drain(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.iteration_start")
    _append_event(sink, "r1", "harness.iteration_end")
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.VALIDATING)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.DONE)
    store.update_lifecycle(lc, expected_version=lc.version - 1)

    start = time.monotonic()
    collected = list(
        stream(
            "r1", store=store, logs_root=logs_root, follow=True, poll_interval=5.0
        )
    )
    elapsed = time.monotonic() - start
    # poll_interval=5.0 would dominate the runtime if the follow loop
    # slept before exiting. The drain-then-exit ordering must skip sleep
    # when the lifecycle is already terminal.
    assert elapsed < 1.0
    assert len(collected) == 2


def test_stream_follow_does_not_drop_final_write_on_terminal(tmp_path: Path) -> None:
    """Race-style check for drain-then-exit ordering.

    A record committed *after* the terminal transition but *before* the
    follow loop's final drain must still be yielded. Simulated by
    transitioning the lifecycle and immediately writing one more event,
    all before the consumer wakes up for its next poll.
    """
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.first")

    collected: list[Any] = []

    def consume() -> None:
        for record in stream(
            "r1",
            store=store,
            logs_root=logs_root,
            follow=True,
            poll_interval=0.05,
        ):
            collected.append(record)

    thread = threading.Thread(target=consume)
    thread.start()
    # Give the consumer a chance to drain "first" and enter the poll
    # loop.
    time.sleep(0.1)

    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.VALIDATING)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.DONE)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    # Write the final record after the terminal transition. The follow
    # loop must drain again before exiting.
    _append_event(sink, "r1", "harness.last")

    thread.join(timeout=2.0)
    assert not thread.is_alive()
    kinds = [r.kind for r in collected if isinstance(r, EventRecord)]
    assert kinds == ["harness.first", "harness.last"]


# --- FR-8 file-cursor edge cases ---------------------------------------------


def test_file_reader_withholds_partial_trailing_line(tmp_path: Path) -> None:
    """A partial trailing line (crash mid-write or a read racing the
    sink's flush) is withheld until the writer completes it; the cursor
    does not advance past it."""
    from flywheel_core.audit._file import FileCursor, read_records_since

    run_file = tmp_path / "runs" / "r1.jsonl"
    run_file.parent.mkdir(parents=True)
    complete = json.dumps(
        {"kind": "harness.one", "run_id": "r1", "ts": "2026-06-11T00:00:00+00:00", "payload": {}}
    )
    partial = '{"kind": "harness.two", "run_id": "r1"'
    run_file.write_text(complete + "\n" + partial, encoding="utf-8")

    records, cursor = read_records_since(run_file, FileCursor())
    assert [r.kind for r in records if isinstance(r, EventRecord)] == [
        "harness.one"
    ]
    assert len(records) == 1
    # Re-reading from the same cursor yields nothing new while the
    # partial line is incomplete.
    again, cursor2 = read_records_since(run_file, cursor)
    assert again == []
    assert cursor2 == cursor

    # The writer completes the line: the withheld record now surfaces.
    with open(run_file, "a", encoding="utf-8") as handle:
        handle.write(', "ts": "2026-06-11T00:00:01+00:00", "payload": {}}\n')
    completed, cursor3 = read_records_since(run_file, cursor2)
    assert [r.kind for r in completed if isinstance(r, EventRecord)] == [
        "harness.two"
    ]
    assert len(completed) == 1
    assert cursor3.line == 2


def test_file_reader_discards_partial_line_on_eof_final(tmp_path: Path) -> None:
    """With ``eof_final=True`` (terminal lifecycle, final drain) the
    partial trailing line is discarded so a finished run re-reads
    idempotently."""
    from flywheel_core.audit._file import FileCursor, read_records_since

    run_file = tmp_path / "runs" / "r1.jsonl"
    run_file.parent.mkdir(parents=True)
    run_file.write_text(
        '{"kind": "harness.one", "run_id": "r1", "payload": {}}\n'
        '{"kind": "harness.tru',
        encoding="utf-8",
    )

    records, cursor = read_records_since(
        run_file, FileCursor(), eof_final=True
    )
    assert [r.kind for r in records if isinstance(r, EventRecord)] == [
        "harness.one"
    ]
    assert len(records) == 1
    # The discarded partial advanced the offset to EOF.
    again, cursor2 = read_records_since(run_file, cursor, eof_final=True)
    assert again == []
    assert cursor2 == cursor


def test_stream_missing_file_reads_as_empty(tmp_path: Path) -> None:
    """FR-8: a run with no file yet is an empty stream, not an error."""
    store = _make_store_with_lifecycle("r1")
    assert list(stream("r1", store=store, logs_root=tmp_path / "logs")) == []


def test_stream_follow_waits_for_file_to_appear(tmp_path: Path) -> None:
    """Follow mode keeps polling until the run file exists, then streams
    its lines."""
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    collected: list[Any] = []

    def consume() -> None:
        for record in stream(
            "r1",
            store=store,
            logs_root=logs_root,
            follow=True,
            poll_interval=0.02,
        ):
            collected.append(record)

    thread = threading.Thread(target=consume)
    thread.start()
    time.sleep(0.1)
    assert collected == []  # no file yet

    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.appeared")
    sink.close()
    time.sleep(0.1)

    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.VALIDATING)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    lc = store.load_lifecycle("r1")
    assert lc is not None
    lc.transition_to(Status.DONE)
    store.update_lifecycle(lc, expected_version=lc.version - 1)

    thread.join(timeout=2.0)
    assert not thread.is_alive()
    assert [r.kind for r in collected] == ["harness.appeared"]


# --- (d) attach_logger captures every record exactly once -------------------


class _RecordingHandler(logging.Handler):
    """Logging handler that records every LogRecord it sees."""

    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.records: list[logging.LogRecord] = []
        self._lock = threading.Lock()

    def emit(self, record: logging.LogRecord) -> None:
        with self._lock:
            self.records.append(record)


def _build_isolated_logger(name: str) -> tuple[logging.Logger, _RecordingHandler]:
    logger = logging.getLogger(name)
    logger.handlers = []
    logger.propagate = False
    logger.setLevel(logging.DEBUG)
    handler = _RecordingHandler()
    logger.addHandler(handler)
    return logger, handler


def test_attach_logger_emits_one_record_per_audit_row(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.iteration_start")
    _save_sdk(sink, "r1", [{"message_type": "assistant", "text": "ok"}])
    _append_event(sink, "r1", "harness.iteration_end")

    logger, handler = _build_isolated_logger(
        "test_audit.attach_logger.one_per_row"
    )
    handle = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.02,
    )
    try:
        # Wait until all three persisted rows have been emitted.
        deadline = time.monotonic() + 2.0
        while len(handler.records) < 3 and time.monotonic() < deadline:
            time.sleep(0.02)
    finally:
        handle.detach()

    assert len(handler.records) == 3
    seqs = [r.__dict__["audit_record"]["sequence"] for r in handler.records]
    assert seqs == [1, 2, 3]
    # Each LogRecord carries the human-readable label.
    for rec, seq in zip(handler.records, seqs):
        assert f"seq={seq}" in rec.getMessage()
        assert "run=r1" in rec.getMessage()
        assert rec.levelno == logging.INFO


def test_attach_logger_returns_distinct_handles_when_called_twice(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    logger, _handler = _build_isolated_logger(
        "test_audit.attach_logger.distinct"
    )
    h1 = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.05,
    )
    h2 = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.05,
    )
    try:
        assert h1 is not h2
        assert isinstance(h1, AuditLoggerHandle)
        assert isinstance(h2, AuditLoggerHandle)
    finally:
        h1.detach()
        h2.detach()


# --- (e) detach() stops emission and joins the thread ----------------------


def test_detach_stops_emission_and_joins_thread(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    _append_event(sink, "r1", "harness.early")

    logger, handler = _build_isolated_logger("test_audit.detach.stops")
    handle = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.02,
    )

    deadline = time.monotonic() + 2.0
    while len(handler.records) < 1 and time.monotonic() < deadline:
        time.sleep(0.02)

    assert len(handler.records) == 1
    handle.detach()
    assert not handle.is_alive

    # After detach, additional writes must NOT be emitted.
    pre_count = len(handler.records)
    _append_event(sink, "r1", "harness.late")
    time.sleep(0.1)
    assert len(handler.records) == pre_count


def test_detach_is_idempotent(tmp_path: Path) -> None:
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    logger, _handler = _build_isolated_logger("test_audit.detach.idempotent")
    handle = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.02,
    )
    handle.detach()
    # Calling detach again must not raise even though the thread is gone.
    handle.detach()


def test_detach_during_emission_does_not_raise(tmp_path: Path) -> None:
    """Detach while the background thread is emitting must not deadlock
    or raise. We pre-seed enough records that the thread is busy when
    detach() is called, then assert detach completes within the join
    timeout."""
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    for i in range(50):
        _append_event(sink, "r1", f"harness.e{i}")

    logger, _handler = _build_isolated_logger(
        "test_audit.detach.during_emission"
    )
    handle = attach_logger(
        logger,
        run_id="r1",
        store=store,
        logs_root=logs_root,
        poll_interval=0.02,
    )

    start = time.monotonic()
    handle.detach()
    elapsed = time.monotonic() - start
    # Join timeout is 1.0s; detach should complete well within that
    # since the thread cooperatively checks the stop flag between rows.
    assert elapsed < 1.5
    assert not handle.is_alive


# --- Misc -------------------------------------------------------------------


def test_stream_is_lazy_iterator(tmp_path: Path) -> None:
    """``stream`` returns an iterator, not a materialized list."""
    store = _make_store_with_lifecycle("r1")
    logs_root = tmp_path / "logs"
    sink = FileTelemetrySink(logs_root)
    it = stream("r1", store=store, logs_root=logs_root)
    # The audit module is documented as returning an Iterator. The
    # type-level test here is just that next() works without indexing.
    with pytest.raises(StopIteration):
        next(it)


def test_audit_stream_carries_harness_awaiting_approval_payload_shape(
    tmp_path: Path,
) -> None:
    """The ``harness.awaiting_approval`` event reaches the run's
    telemetry stream with every field the operator-surfacing path
    consumes — ``instructions``, ``awaiting_ordinal``, ``grader_name``,
    ``run_id``, ``attempt_number``, and ``artifacts_dir``. This is the
    spec 00016 FR-10 oracle, retargeted by spec 00025: telemetry events
    stream to the sink rather than the store, so the shape is asserted
    on the sink record (the file-reader round-trip lands with FR-8).
    """

    from flywheel_core.grader_manual import ManualGate
    from flywheel_core.harness import _enter_manual_gate, _RunTelemetry
    from flywheel_core.lifecycle import Attempt, Outcome
    from flywheel_core.store_protocols import TelemetryRecord

    store = _make_store_with_lifecycle("run-await")
    # Drive RUNNING -> VALIDATING so the in-memory version matches the
    # store's persisted version before _enter_manual_gate appends.
    lc = store.load_lifecycle("run-await")
    assert lc is not None
    lc.transition_to(Status.VALIDATING)
    store.update_lifecycle(lc, expected_version=lc.version - 1)
    lc = store.load_lifecycle("run-await")
    assert lc is not None

    # The manual gate parks the lifecycle after the attempt is closed
    # (SUCCEEDED semantics, per spec 00016): persist a finalized attempt
    # so the gate event is keyed to a real attempt number.
    now = datetime(2026, 1, 1, tzinfo=timezone.utc)
    attempt = Attempt(
        number=1,
        started_at=now,
        run_id=lc.run_id,
        ended_at=now,
        outcome=Outcome.SUCCEEDED,
    )
    store.save_attempt(lc.run_id, attempt)

    gate = ManualGate(
        ordinal=3,
        instruction="Confirm the migration is safe.",
        grader_name="confirm-migration",
    )
    artifacts_dir = tmp_path / "artifacts" / "run-await" / "attempt-1"

    captured: list[TelemetryRecord] = []

    class _Sink:
        def append_telemetry(self, record: TelemetryRecord) -> None:
            captured.append(record)

    _enter_manual_gate(
        store=store,
        telemetry=_RunTelemetry(_Sink(), run_id=lc.run_id, clock=lambda: now),
        lifecycle=lc,
        attempt=attempt,
        gate=gate,
        attempt_dir=artifacts_dir,
        clock=lambda: now,
    )

    awaiting = [
        r for r in captured if r.kind == "harness.awaiting_approval"
    ]
    assert len(awaiting) == 1
    payload = awaiting[0].payload
    assert payload == {
        "instructions": "Confirm the migration is safe.",
        "awaiting_ordinal": 3,
        "grader_name": "confirm-migration",
        "run_id": "run-await",
        "attempt_number": 1,
        "artifacts_dir": str(artifacts_dir),
    }
    # The event is keyed to the attempt that just finalized so audit
    # consumers can group it with the SUCCEEDED attempt's events.
    assert awaiting[0].attempt_number == 1


# --- python -m flywheel_core.audit CLI -------------------------------------------
#
# These tests carry the keyword "cli" so ``pytest -k cli`` selects only
# the CLI surface. They write fixtures into a SqliteStore (the canonical
# durable backend) and exercise the CLI via either ``cli_main`` for
# speed or a real ``python -m flywheel_core.audit`` subprocess for the
# end-to-end integration acceptance criterion.


def _build_sqlite_fixture(
    db_path: Path,
    *,
    run_id: str = "run-cli-fixture",
    extra_payload: dict[str, Any] | None = None,
    terminal: bool = False,
) -> str:
    """Populate ``db_path`` with one lifecycle, three audit records.

    Returns the ``run_id`` so callers can pass it back into the CLI.
    The fixture is deterministic: every test sees the same six-field
    payload layout unless it asks for an ``extra_payload`` override
    (used by the truncation test to inject a multi-MB blob).
    """
    store = SqliteStore(db_path)
    try:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.READY)
        store.update_lifecycle(lc, expected_version=1)
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.RUNNING)
        store.update_lifecycle(lc, expected_version=2)

        # Telemetry lives in the per-run JSONL file next to the db
        # (spec 00025); the CLI derives the same logs root from --db.
        with FileTelemetrySink(db_path.parent / "logs") as sink:
            sink.append_telemetry(
                TelemetryRecord(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    kind="harness.iteration_start",
                    payload={"attempt": 1, "iteration": 1},
                    attempt_number=1,
                )
            )
            sdk_payload: dict[str, Any] = {
                "message_type": "assistant",
                "text": "hello world",
            }
            if extra_payload is not None:
                sdk_payload.update(extra_payload)
            sink.append_telemetry(
                TelemetryRecord(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    kind="assistant",
                    payload=sdk_payload,
                    attempt_number=1,
                    iteration_number=1,
                )
            )
            sink.append_telemetry(
                TelemetryRecord(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    kind="harness.iteration_end",
                    payload={"attempt": 1, "iteration": 1},
                    attempt_number=1,
                )
            )

        if terminal:
            lc = store.load_lifecycle(run_id)
            assert lc is not None
            lc.transition_to(Status.VALIDATING)
            store.update_lifecycle(lc, expected_version=lc.version - 1)
            lc = store.load_lifecycle(run_id)
            assert lc is not None
            lc.transition_to(Status.DONE)
            store.update_lifecycle(lc, expected_version=lc.version - 1)
    finally:
        store.close()
    return run_id


def _run_cli(
    *args: str,
) -> tuple[int, str, str]:
    """Invoke ``cli_main`` with captured stdout/stderr.

    Returns ``(exit_code, stdout, stderr)``. Faster than spawning a
    subprocess; used by every test that doesn't specifically assert the
    ``python -m flywheel_core.audit`` boot path.
    """
    stdout = io.StringIO()
    stderr = io.StringIO()
    code = cli_main(list(args), stdout=stdout, stderr=stderr)
    return code, stdout.getvalue(), stderr.getvalue()


def test_cli_default_mode_prints_records_in_chronological_order_via_subprocess(
    tmp_path: Path,
) -> None:
    """End-to-end: ``python -m flywheel_core.audit`` against a SqliteStore.

    This is the spec's named integration test -- it opens a tmp sqlite
    db, writes a fixture run, and invokes the module via subprocess so
    the ``__main__`` boot path is exercised too.
    """
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "flywheel_core.audit",
            run_id,
            "--db",
            str(db),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    lines = [line for line in completed.stdout.splitlines() if line]
    # Three persisted records: two events + one SDK message.
    assert len(lines) == 3
    # Sequence numbers appear in ascending order.
    seqs = [int(line.split(" seq=", 1)[1].split(" ", 1)[0]) for line in lines]
    assert seqs == sorted(seqs)
    assert seqs == [1, 2, 3]
    # The first and third records are events; the middle one is the SDK
    # assistant message. The kind=<k> column carries the type prefix.
    assert "kind=event:harness.iteration_start" in lines[0]
    assert "kind=sdk:assistant" in lines[1]
    assert "kind=event:harness.iteration_end" in lines[2]


def test_cli_json_mode_emits_ndjson_with_full_payload(
    tmp_path: Path,
) -> None:
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    code, stdout, _stderr = _run_cli(run_id, "--db", str(db), "--json")
    assert code == 0
    lines = [line for line in stdout.splitlines() if line]
    assert len(lines) == 3
    parsed = [json.loads(line) for line in lines]
    # Schema: every record carries the spec'd field set.
    for obj in parsed:
        assert set(obj.keys()) == {
            "ts",
            "sequence",
            "run_id",
            "attempt_number",
            "iteration_number",
            "kind_or_message_type",
            "payload",
        }
        assert obj["run_id"] == run_id
        assert isinstance(obj["payload"], dict)
    # Events have iteration_number == None; the SDK message has 1.
    iters = [obj["iteration_number"] for obj in parsed]
    assert iters == [None, 1, None]
    discriminators = [obj["kind_or_message_type"] for obj in parsed]
    assert discriminators == [
        "harness.iteration_start",
        "assistant",
        "harness.iteration_end",
    ]


def test_cli_unknown_run_id_prints_empty_state_message_and_exits_zero(
    tmp_path: Path,
) -> None:
    db = tmp_path / "flywheel.sqlite"
    # Bootstrap an empty sqlite store so the file exists but has no
    # lifecycle rows.
    SqliteStore(db).close()

    code, stdout, stderr = _run_cli("nonexistent-run", "--db", str(db))
    assert code == 0
    assert stdout == ""
    assert "no records for run_id nonexistent-run" in stderr


def test_cli_follow_exits_when_lifecycle_reaches_done(
    tmp_path: Path,
) -> None:
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db, terminal=True)

    start = time.monotonic()
    code, stdout, _stderr = _run_cli(
        run_id,
        "--db",
        str(db),
        "--follow",
        "--poll-interval",
        "5.0",
    )
    elapsed = time.monotonic() - start
    assert code == 0
    # Already-terminal lifecycle must drain and exit immediately; the
    # 5.0s poll interval would dominate runtime otherwise.
    assert elapsed < 2.0
    lines = [line for line in stdout.splitlines() if line]
    assert len(lines) == 3


def test_cli_preview_truncates_large_payload_in_default_but_full_in_json(
    tmp_path: Path,
) -> None:
    db = tmp_path / "flywheel.sqlite"
    huge_blob = "X" * 4096
    run_id = _build_sqlite_fixture(
        db, extra_payload={"content": huge_blob}
    )

    code, default_stdout, _ = _run_cli(run_id, "--db", str(db))
    assert code == 0
    default_lines = [
        line for line in default_stdout.splitlines() if line
    ]
    assert len(default_lines) == 3
    sdk_line = next(
        line for line in default_lines if "kind=sdk:assistant" in line
    )
    # The preview is the tail after " | ". It must contain the
    # truncation hint and stay within the byte budget so terminals do
    # not wrap. The huge_blob must NOT appear in full.
    _, preview = sdk_line.split(" | ", 1)
    assert TRUNCATION_HINT in preview
    assert len(preview) <= PREVIEW_MAX_CHARS
    assert huge_blob not in preview

    code, json_stdout, _ = _run_cli(run_id, "--db", str(db), "--json")
    assert code == 0
    json_lines = [line for line in json_stdout.splitlines() if line]
    sdk_obj = next(
        json.loads(line)
        for line in json_lines
        if json.loads(line)["kind_or_message_type"] == "assistant"
    )
    # Full payload survives the JSON path byte-for-byte.
    assert sdk_obj["payload"]["content"] == huge_blob


def test_cli_help_runs_cleanly() -> None:
    """``--help`` is part of the verification harness; ensure it exits 0
    and emits something on stdout."""
    completed = subprocess.run(
        [sys.executable, "-m", "flywheel_core.audit", "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 0
    assert "python -m flywheel_core.audit" in completed.stdout
    # Spec-required flags are documented.
    assert "--json" in completed.stdout
    assert "--follow" in completed.stdout
    assert "--poll-interval" in completed.stdout
    # Redaction surface: every new flag is in the help text plus the
    # NFR-1 best-effort caveat so the help reader cannot mistake the
    # default for a safety guarantee.
    assert "--redact" in completed.stdout
    assert "--redact-policy" in completed.stdout
    assert "--raw" in completed.stdout
    assert "--dry-run" in completed.stdout
    assert "--redact-salt" in completed.stdout
    assert "best-effort" in completed.stdout
    # ``argparse`` wraps the description across lines, so check for the
    # marker keyword rather than the literal multi-word phrase.
    assert "unredacted" in completed.stdout


# --- CLI redaction surface --------------------------------------------------
#
# These tests carry the keyword "cli" so ``pytest -k cli`` selects them
# alongside the existing CLI tests, matching the verify grader command.


def _build_sqlite_fixture_with_secret(
    db_path: Path,
    secret: str,
    *,
    run_id: str = "run-cli-redact",
) -> str:
    """Populate ``db_path`` with one record whose payload embeds ``secret``.

    The SDK message carries the secret as a top-level ``text`` field and
    repeats it in a nested ``tool_input`` block so coverage tests can
    distinguish per-record hits from per-occurrence hits.
    """

    store = SqliteStore(db_path)
    try:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.READY)
        store.update_lifecycle(lc, expected_version=1)
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.RUNNING)
        store.update_lifecycle(lc, expected_version=2)

    finally:
        store.close()
    with FileTelemetrySink(db_path.parent / "logs") as sink:
        sink.append_telemetry(
            TelemetryRecord(
                run_id=run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_start",
                payload={"attempt": 1, "iteration": 1},
                attempt_number=1,
            )
        )
        sink.append_telemetry(
            TelemetryRecord(
                run_id=run_id,
                ts=datetime.now(timezone.utc),
                kind="assistant",
                payload={
                    "message_type": "assistant",
                    "text": f"key is {secret}",
                    "tool_input": {"command": f"echo {secret}"},
                },
                attempt_number=1,
                iteration_number=1,
            )
        )
        sink.append_telemetry(
            TelemetryRecord(
                run_id=run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_end",
                payload={"attempt": 1, "iteration": 1},
                attempt_number=1,
            )
        )
    return run_id


def test_cli_default_mode_redacts_anthropic_pattern_and_emits_notice(
    tmp_path: Path,
) -> None:
    """Default mode applies ``default_policy`` to every record and emits the
    one-line FR-10 stderr notice exactly once. The Anthropic-key pattern
    catches a leaked ``sk-ant-...`` value in payload prose."""
    db = tmp_path / "flywheel.sqlite"
    secret = "sk-ant-abcdef0123456789ABCDEF"
    run_id = _build_sqlite_fixture_with_secret(db, secret)

    code, stdout, stderr = _run_cli(run_id, "--db", str(db), "--json")
    assert code == 0
    # The literal secret never reaches stdout.
    assert secret not in stdout
    # The token does.
    assert "[REDACTED:anthropic_key]" in stdout
    # FR-10 notice fires on stderr exactly once.
    assert (
        "redaction: default policy applied (use --raw for verbatim)" in stderr
    )
    # NFR-1 best-effort caveat is part of the notice.
    assert "best-effort" in stderr
    assert "unredacted source of truth" in stderr
    # Only one notice block, even though we streamed three records.
    assert stderr.count("redaction: default policy applied") == 1


def test_cli_raw_disables_redaction_and_suppresses_notice(
    tmp_path: Path,
) -> None:
    """``--raw`` restores the pre-redaction verbatim output and does NOT
    emit the redaction notice (NFR-4)."""
    db = tmp_path / "flywheel.sqlite"
    secret = "sk-ant-abcdef0123456789ABCDEF"
    run_id = _build_sqlite_fixture_with_secret(db, secret)

    code, stdout, stderr = _run_cli(run_id, "--db", str(db), "--json", "--raw")
    assert code == 0
    # Verbatim: the secret survives intact in NDJSON.
    assert secret in stdout
    assert "[REDACTED:" not in stdout
    # No notice on stderr.
    assert "redaction:" not in stderr


def test_cli_redact_policy_strict_denylists_tools(tmp_path: Path) -> None:
    """``--redact-policy strict`` adds a tool denylist on top of the default
    pattern set, blanking ``Read`` / ``Bash`` tool inputs/outputs."""
    db = tmp_path / "flywheel.sqlite"
    run_id = "run-strict"
    store = SqliteStore(db)
    try:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.READY)
        store.update_lifecycle(lc, expected_version=1)
        lc = store.load_lifecycle(run_id)
        assert lc is not None
        lc.transition_to(Status.RUNNING)
        store.update_lifecycle(lc, expected_version=2)
    finally:
        store.close()
    with FileTelemetrySink(db.parent / "logs") as sink:
        sink.append_telemetry(
            TelemetryRecord(
                run_id=run_id,
                ts=datetime.now(timezone.utc),
                kind="assistant",
                payload={
                    "message_type": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "/home/me/.env"},
                        }
                    ],
                },
                attempt_number=1,
                iteration_number=1,
            )
        )

    code, stdout, stderr = _run_cli(
        run_id, "--db", str(db), "--json", "--redact-policy", "strict"
    )
    assert code == 0
    # The Read tool's input is redacted; the tool's name survives so the
    # audit trail still shows which tool ran.
    assert "/home/me/.env" not in stdout
    assert "[REDACTED:tool:Read]" in stdout
    assert '"name": "Read"' in stdout or '"name":"Read"' in stdout
    # Notice header reflects the active policy name.
    assert "redaction: strict policy applied" in stderr


def test_cli_redact_policy_unknown_name_fails_fast(tmp_path: Path) -> None:
    """An unrecognized policy name fails before any record is streamed."""
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    code, stdout, stderr = _run_cli(
        run_id, "--db", str(db), "--redact-policy", "no-such-policy"
    )
    assert code == 2
    assert stdout == ""
    assert "unknown built-in policy" in stderr


def test_cli_redact_policy_bad_dotted_path_fails_fast(
    tmp_path: Path,
) -> None:
    """A dotted path to a missing module fails before streaming (FR-11)."""
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    code, stdout, stderr = _run_cli(
        run_id,
        "--db",
        str(db),
        "--redact-policy",
        "this_module_does_not_exist:factory",
    )
    assert code == 2
    assert stdout == ""
    assert "cannot import module" in stderr


def test_cli_redact_policy_non_redactor_return_fails_fast(
    tmp_path: Path,
) -> None:
    """A dotted path whose callable returns a non-``Redactor`` fails before
    streaming (FR-11)."""
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    # ``str`` is callable and importable but returns ``str``, not a Redactor.
    code, stdout, stderr = _run_cli(
        run_id, "--db", str(db), "--redact-policy", "builtins:str"
    )
    assert code == 2
    assert stdout == ""
    assert "not a Redactor" in stderr


def test_cli_redact_salt_emits_stable_digest_tokens(tmp_path: Path) -> None:
    """``--redact-salt`` switches every token to the salted
    ``[REDACTED:label:digest]`` form (FR-8)."""
    db = tmp_path / "flywheel.sqlite"
    secret = "sk-ant-abcdef0123456789ABCDEF"
    run_id = _build_sqlite_fixture_with_secret(db, secret)

    code, stdout, _ = _run_cli(
        run_id, "--db", str(db), "--json", "--redact-salt", "test-salt"
    )
    assert code == 0
    # Salted token format: [REDACTED:label:<8 hex>].
    salted = re.findall(
        r"\[REDACTED:anthropic_key:([0-9a-f]{8})\]", stdout
    )
    assert salted, "no salted anthropic_key tokens emitted"
    # Stable across occurrences of the same cleartext.
    assert len(set(salted)) == 1


def test_cli_dry_run_emits_coverage_without_payload_content(
    tmp_path: Path,
) -> None:
    """``--dry-run`` reports per-label hit counts without any payload
    content reaching stdout (FR-12)."""
    db = tmp_path / "flywheel.sqlite"
    secret = "sk-ant-abcdef0123456789ABCDEF"
    run_id = _build_sqlite_fixture_with_secret(db, secret)

    code, stdout, stderr = _run_cli(
        run_id, "--db", str(db), "--dry-run"
    )
    assert code == 0
    # Report header and counts.
    assert "dry-run coverage report" in stdout
    assert "policy: default" in stdout
    assert "records scanned: 3" in stdout
    # The SDK record contains the secret in two places (``text`` and
    # ``tool_input.command``); the per-label total reflects both.
    assert "records redacted: 1" in stdout
    assert "anthropic_key: 2" in stdout
    # No payload content leaks; the secret never appears in stdout.
    assert secret not in stdout
    # The notice still fires on stderr even under dry-run.
    assert "redaction: default policy applied" in stderr


def test_cli_dry_run_with_no_matches_reports_zero_redactions(
    tmp_path: Path,
) -> None:
    """``--dry-run`` over a benign run emits zero payload content even when
    no patterns match (edge case)."""
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    code, stdout, _ = _run_cli(run_id, "--db", str(db), "--dry-run")
    assert code == 0
    assert "records scanned: 3" in stdout
    assert "records redacted: 0" in stdout
    assert "total redactions: 0" in stdout
    assert "hits by label: (none)" in stdout
    # No record-body fields leak into the report.
    assert "harness.iteration_start" not in stdout
    assert "hello world" not in stdout


def test_cli_raw_rejects_combined_redaction_flags(tmp_path: Path) -> None:
    """``--raw`` is mutually exclusive with every redaction-shaping flag."""
    db = tmp_path / "flywheel.sqlite"
    run_id = _build_sqlite_fixture(db)

    for extra in (
        ("--redact",),
        ("--redact-policy", "strict"),
        ("--dry-run",),
        ("--redact-salt", "x"),
    ):
        code, stdout, stderr = _run_cli(
            run_id, "--db", str(db), "--raw", *extra
        )
        assert code == 2, f"--raw + {extra} should fail"
        assert stdout == ""
        assert "--raw cannot be combined with" in stderr


def test_cli_env_value_redactor_catches_anthropic_api_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The CLI seeds an ``EnvValueRedactor`` from the ambient
    ``ANTHROPIC_API_KEY`` so a leaked literal value is redacted even when
    it does not match the ``sk-ant-`` regex (e.g. a custom test key)."""
    db = tmp_path / "flywheel.sqlite"
    # A value that does NOT match the default ``sk-ant-`` pattern but is
    # long enough to clear EnvValueRedactor's minimum length floor.
    secret = "unusual-test-shaped-key-value-1234"
    monkeypatch.setenv("ANTHROPIC_API_KEY", secret)
    run_id = _build_sqlite_fixture_with_secret(db, secret)

    code, stdout, _ = _run_cli(run_id, "--db", str(db), "--json")
    assert code == 0
    assert secret not in stdout
    assert "[REDACTED:ANTHROPIC_API_KEY]" in stdout


def test_cli_unknown_run_id_does_not_emit_redaction_notice(
    tmp_path: Path,
) -> None:
    """The redaction notice fires only on the streaming path. An unknown
    run_id stays a single-line stderr message so existing scripted
    handling does not regress."""
    db = tmp_path / "flywheel.sqlite"
    SqliteStore(db).close()

    code, stdout, stderr = _run_cli("missing-run", "--db", str(db))
    assert code == 0
    assert stdout == ""
    assert "no records for run_id missing-run" in stderr
    assert "redaction:" not in stderr
