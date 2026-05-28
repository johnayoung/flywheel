"""Tests for the ``flywheel.audit`` streaming reader and logger emitter.

The spec lists five acceptance criteria for this module; this file
covers each one against the in-memory store substrate so the library
suite stays sqlite/postgres-independent.
"""

from __future__ import annotations

import io
import json
import logging
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from flywheel import (
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
from flywheel.audit._cli import (
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


def _append_event(store: InMemoryStore, run_id: str, kind: str) -> None:
    store.append_event(
        EventRecord(
            run_id=run_id,
            ts=datetime.now(timezone.utc),
            kind=kind,
            payload={"k": kind},
        )
    )


def _save_sdk(
    store: InMemoryStore,
    run_id: str,
    payloads: list[dict[str, Any]],
    *,
    attempt_number: int = 1,
    iteration_number: int = 1,
) -> None:
    store.save_sdk_messages(run_id, attempt_number, iteration_number, payloads)


# --- (a) stream(follow=False) drains everything in sequence order -----------


def test_stream_drains_mixed_records_in_sequence_order() -> None:
    store = _make_store_with_lifecycle("r1")
    _append_event(store, "r1", "harness.iteration_start")
    _save_sdk(store, "r1", [{"message_type": "assistant", "text": "hi"}])
    _append_event(store, "r1", "harness.envelope_observed")
    _save_sdk(
        store,
        "r1",
        [
            {"message_type": "tool_use", "name": "Read"},
            {"message_type": "tool_result", "content": "ok"},
        ],
    )
    _append_event(store, "r1", "harness.iteration_end")

    collected = list(stream("r1", store=store))

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


def test_stream_unknown_run_id_yields_nothing() -> None:
    store = InMemoryStore()
    assert list(stream("does-not-exist", store=store)) == []


def test_stream_unknown_run_id_with_follow_false_terminates() -> None:
    store = InMemoryStore()
    # follow=False on an unknown run should also return immediately
    # without spinning or polling.
    assert list(stream("nope", store=store, follow=False)) == []


# --- (c) follow=True observes concurrent writes and exits on terminal --------


def test_stream_follow_exits_when_lifecycle_reaches_terminal() -> None:
    store = _make_store_with_lifecycle("r1")
    collected: list[Any] = []
    errors: list[BaseException] = []
    consumer_started = threading.Event()

    def consume() -> None:
        consumer_started.set()
        try:
            for record in stream(
                "r1", store=store, follow=True, poll_interval=0.02
            ):
                collected.append(record)
        except BaseException as exc:  # pragma: no cover - surfaced by assert
            errors.append(exc)

    thread = threading.Thread(target=consume)
    thread.start()
    consumer_started.wait(timeout=1.0)

    # Write records while the consumer thread is following.
    _append_event(store, "r1", "harness.iteration_start")
    time.sleep(0.05)
    _save_sdk(store, "r1", [{"message_type": "assistant", "text": "ok"}])
    time.sleep(0.05)
    _append_event(store, "r1", "harness.iteration_end")
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


def test_stream_follow_against_already_terminal_run_returns_after_drain() -> None:
    store = _make_store_with_lifecycle("r1")
    _append_event(store, "r1", "harness.iteration_start")
    _append_event(store, "r1", "harness.iteration_end")
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
        stream("r1", store=store, follow=True, poll_interval=5.0)
    )
    elapsed = time.monotonic() - start
    # poll_interval=5.0 would dominate the runtime if the follow loop
    # slept before exiting. The drain-then-exit ordering must skip sleep
    # when the lifecycle is already terminal.
    assert elapsed < 1.0
    assert len(collected) == 2


def test_stream_follow_does_not_drop_final_write_on_terminal() -> None:
    """Race-style check for drain-then-exit ordering.

    A record committed *after* the terminal transition but *before* the
    follow loop's final drain must still be yielded. Simulated by
    transitioning the lifecycle and immediately writing one more event,
    all before the consumer wakes up for its next poll.
    """
    store = _make_store_with_lifecycle("r1")
    _append_event(store, "r1", "first")

    collected: list[Any] = []

    def consume() -> None:
        for record in stream(
            "r1", store=store, follow=True, poll_interval=0.05
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
    _append_event(store, "r1", "last")

    thread.join(timeout=2.0)
    assert not thread.is_alive()
    kinds = [r.kind for r in collected if isinstance(r, EventRecord)]
    assert kinds == ["first", "last"]


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


def test_attach_logger_emits_one_record_per_audit_row() -> None:
    store = _make_store_with_lifecycle("r1")
    _append_event(store, "r1", "harness.iteration_start")
    _save_sdk(store, "r1", [{"message_type": "assistant", "text": "ok"}])
    _append_event(store, "r1", "harness.iteration_end")

    logger, handler = _build_isolated_logger(
        "test_audit.attach_logger.one_per_row"
    )
    handle = attach_logger(
        logger, run_id="r1", store=store, poll_interval=0.02
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


def test_attach_logger_returns_distinct_handles_when_called_twice() -> None:
    store = _make_store_with_lifecycle("r1")
    logger, _handler = _build_isolated_logger(
        "test_audit.attach_logger.distinct"
    )
    h1 = attach_logger(logger, run_id="r1", store=store, poll_interval=0.05)
    h2 = attach_logger(logger, run_id="r1", store=store, poll_interval=0.05)
    try:
        assert h1 is not h2
        assert isinstance(h1, AuditLoggerHandle)
        assert isinstance(h2, AuditLoggerHandle)
    finally:
        h1.detach()
        h2.detach()


# --- (e) detach() stops emission and joins the thread ----------------------


def test_detach_stops_emission_and_joins_thread() -> None:
    store = _make_store_with_lifecycle("r1")
    _append_event(store, "r1", "early")

    logger, handler = _build_isolated_logger("test_audit.detach.stops")
    handle = attach_logger(
        logger, run_id="r1", store=store, poll_interval=0.02
    )

    deadline = time.monotonic() + 2.0
    while len(handler.records) < 1 and time.monotonic() < deadline:
        time.sleep(0.02)

    assert len(handler.records) == 1
    handle.detach()
    assert not handle.is_alive

    # After detach, additional writes must NOT be emitted.
    pre_count = len(handler.records)
    _append_event(store, "r1", "late")
    time.sleep(0.1)
    assert len(handler.records) == pre_count


def test_detach_is_idempotent() -> None:
    store = _make_store_with_lifecycle("r1")
    logger, _handler = _build_isolated_logger("test_audit.detach.idempotent")
    handle = attach_logger(
        logger, run_id="r1", store=store, poll_interval=0.02
    )
    handle.detach()
    # Calling detach again must not raise even though the thread is gone.
    handle.detach()


def test_detach_during_emission_does_not_raise() -> None:
    """Detach while the background thread is emitting must not deadlock
    or raise. We pre-seed enough records that the thread is busy when
    detach() is called, then assert detach completes within the join
    timeout."""
    store = _make_store_with_lifecycle("r1")
    for i in range(50):
        _append_event(store, "r1", f"e{i}")

    logger, _handler = _build_isolated_logger(
        "test_audit.detach.during_emission"
    )
    handle = attach_logger(
        logger, run_id="r1", store=store, poll_interval=0.02
    )

    start = time.monotonic()
    handle.detach()
    elapsed = time.monotonic() - start
    # Join timeout is 1.0s; detach should complete well within that
    # since the thread cooperatively checks the stop flag between rows.
    assert elapsed < 1.5
    assert not handle.is_alive


# --- Misc -------------------------------------------------------------------


def test_stream_is_lazy_iterator() -> None:
    """``stream`` returns an iterator, not a materialized list."""
    store = _make_store_with_lifecycle("r1")
    it = stream("r1", store=store)
    # The audit module is documented as returning an Iterator. The
    # type-level test here is just that next() works without indexing.
    with pytest.raises(StopIteration):
        next(it)


# --- python -m flywheel.audit CLI -------------------------------------------
#
# These tests carry the keyword "cli" so ``pytest -k cli`` selects only
# the CLI surface. They write fixtures into a SqliteStore (the canonical
# durable backend) and exercise the CLI via either ``cli_main`` for
# speed or a real ``python -m flywheel.audit`` subprocess for the
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

        store.append_event(
            EventRecord(
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
        store.save_sdk_messages(run_id, 1, 1, [sdk_payload])
        store.append_event(
            EventRecord(
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
    ``python -m flywheel.audit`` boot path.
    """
    stdout = io.StringIO()
    stderr = io.StringIO()
    code = cli_main(list(args), stdout=stdout, stderr=stderr)
    return code, stdout.getvalue(), stderr.getvalue()


def test_cli_default_mode_prints_records_in_chronological_order_via_subprocess(
    tmp_path: Path,
) -> None:
    """End-to-end: ``python -m flywheel.audit`` against a SqliteStore.

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
            "flywheel.audit",
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
        [sys.executable, "-m", "flywheel.audit", "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 0
    assert "python -m flywheel.audit" in completed.stdout
    # Spec-required flags are documented.
    assert "--json" in completed.stdout
    assert "--follow" in completed.stdout
    assert "--poll-interval" in completed.stdout
