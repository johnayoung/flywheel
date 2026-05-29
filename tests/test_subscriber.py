"""Tests for the read-only audit subscriber registry (P2).

Subscribers observe the per-run audit stream in order, on their own
threads, isolated from each other and from the follow loop. They receive
only committed :class:`AuditRecord`s and hold no store handle, so they
cannot corrupt lifecycle state.
"""

from __future__ import annotations

import threading
import time
from datetime import datetime, timezone

from flywheel import (
    EventHandler,
    EventRecord,
    InMemoryStore,
    LifecycleInitialized,
    Status,
    TransitionedTo,
    subscribe,
)
from flywheel.store_protocols import AuditRecord

_BASE = datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)


def _ts(n: int) -> datetime:
    return _BASE.replace(second=n)


def _drive_to_running(store: InMemoryStore, run_id: str) -> None:
    store.append_domain_event(
        LifecycleInitialized(run_id=run_id, ts=_ts(0), task_id="t"),
        expected_version=0,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_ts(1), target=Status.READY),
        expected_version=1,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_ts(2), target=Status.RUNNING),
        expected_version=2,
    )


def _finish(store: InMemoryStore, run_id: str, *, version: int) -> None:
    store.append_domain_event(
        TransitionedTo(
            run_id=run_id, ts=_ts(9), target=Status.FAILED, error="done"
        ),
        expected_version=version,
    )


def _wait_until_done(sub: object, timeout: float = 2.0) -> None:
    deadline = time.monotonic() + timeout
    while getattr(sub, "is_alive") and time.monotonic() < deadline:
        time.sleep(0.01)


def test_callable_subscriber_receives_records_in_order() -> None:
    store = InMemoryStore()
    run_id = "run-cb"
    _drive_to_running(store, run_id)
    seen: list[AuditRecord] = []

    sub = subscribe(seen.append, run_id=run_id, store=store, poll_interval=5.0)

    def writer() -> None:
        time.sleep(0.05)
        store.append_event(
            EventRecord(run_id=run_id, ts=_ts(3), kind="a")
        )
        store.append_event(
            EventRecord(run_id=run_id, ts=_ts(4), kind="b")
        )
        _finish(store, run_id, version=3)

    t = threading.Thread(target=writer)
    t.start()
    t.join(2.0)
    _wait_until_done(sub)

    kinds = [r.kind for r in seen if isinstance(r, EventRecord)]
    assert kinds == ["a", "b"]
    seqs = [r.sequence for r in seen]
    assert seqs == sorted(seqs)
    assert not sub.is_alive


def test_event_handler_object_is_accepted() -> None:
    store = InMemoryStore()
    run_id = "run-obj"
    _drive_to_running(store, run_id)

    class Collector:
        def __init__(self) -> None:
            self.records: list[AuditRecord] = []

        def on_record(self, record: AuditRecord) -> None:
            self.records.append(record)

    collector = Collector()
    assert isinstance(collector, EventHandler)
    sub = subscribe(
        collector, run_id=run_id, store=store, poll_interval=5.0
    )
    store.append_event(EventRecord(run_id=run_id, ts=_ts(3), kind="x"))
    _finish(store, run_id, version=3)
    _wait_until_done(sub)

    assert any(
        isinstance(r, EventRecord) and r.kind == "x"
        for r in collector.records
    )


def test_raising_handler_is_isolated_and_keeps_receiving() -> None:
    store = InMemoryStore()
    run_id = "run-iso"
    _drive_to_running(store, run_id)
    seen: list[str] = []

    def handler(record: AuditRecord) -> None:
        if isinstance(record, EventRecord):
            seen.append(record.kind)
            if record.kind == "boom":
                raise RuntimeError("handler blew up")

    sub = subscribe(handler, run_id=run_id, store=store, poll_interval=5.0)
    store.append_event(EventRecord(run_id=run_id, ts=_ts(3), kind="boom"))
    store.append_event(EventRecord(run_id=run_id, ts=_ts(4), kind="after"))
    _finish(store, run_id, version=3)
    _wait_until_done(sub)

    # The raise on "boom" did not stop delivery of the later record.
    assert "boom" in seen
    assert "after" in seen


def test_on_error_callback_receives_the_failure() -> None:
    store = InMemoryStore()
    run_id = "run-onerr"
    _drive_to_running(store, run_id)
    errors: list[tuple[AuditRecord, BaseException]] = []

    def handler(record: AuditRecord) -> None:
        if isinstance(record, EventRecord) and record.kind == "boom":
            raise ValueError("nope")

    def on_error(record: AuditRecord, exc: BaseException) -> None:
        errors.append((record, exc))

    sub = subscribe(
        handler,
        run_id=run_id,
        store=store,
        poll_interval=5.0,
        on_error=on_error,
    )
    store.append_event(EventRecord(run_id=run_id, ts=_ts(3), kind="boom"))
    _finish(store, run_id, version=3)
    _wait_until_done(sub)

    assert len(errors) == 1
    record, exc = errors[0]
    assert isinstance(record, EventRecord) and record.kind == "boom"
    assert isinstance(exc, ValueError)


def test_two_subscribers_are_independent() -> None:
    store = InMemoryStore()
    run_id = "run-two"
    _drive_to_running(store, run_id)
    collected: list[str] = []

    def good(record: AuditRecord) -> None:
        if isinstance(record, EventRecord):
            collected.append(record.kind)

    def bad(record: AuditRecord) -> None:
        raise RuntimeError("always fails")

    good_sub = subscribe(
        good, run_id=run_id, store=store, poll_interval=5.0
    )
    bad_sub = subscribe(bad, run_id=run_id, store=store, poll_interval=5.0)

    store.append_event(EventRecord(run_id=run_id, ts=_ts(3), kind="one"))
    store.append_event(EventRecord(run_id=run_id, ts=_ts(4), kind="two"))
    _finish(store, run_id, version=3)
    _wait_until_done(good_sub)
    _wait_until_done(bad_sub)

    # The always-failing subscriber did not starve the healthy one.
    assert collected == ["one", "two"]


def test_unsubscribe_stops_the_thread() -> None:
    store = InMemoryStore()
    run_id = "run-stop"
    _drive_to_running(store, run_id)  # stays RUNNING (non-terminal)

    sub = subscribe(
        lambda record: None,
        run_id=run_id,
        store=store,
        poll_interval=5.0,
    )
    assert sub.is_alive
    sub.unsubscribe()
    assert not sub.is_alive
    # Idempotent.
    sub.unsubscribe()
