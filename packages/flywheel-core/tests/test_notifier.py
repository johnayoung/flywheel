"""Tests for the in-process reactive notifier (P1).

Split into three layers:

* the :class:`RunNotifier` mechanism in isolation (deterministic, no
  reliance on wall-clock thresholds beyond a generous timeout),
* the store signalling its notifier on every audit write, and
* the audit follow loop consuming push wakeups, plus the poll fallback
  when a store exposes no notifier.
"""

from __future__ import annotations

import threading
import time
from datetime import datetime, timezone

from flywheel_core import (
    EventRecord,
    InMemoryStore,
    LifecycleInitialized,
    RunNotifier,
    Status,
    TransitionedTo,
)
from flywheel_core.audit import _resolve_notifier, stream

_BASE = datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)


def _ts(n: int) -> datetime:
    return _BASE.replace(second=n)


# --- RunNotifier mechanism --------------------------------------------------


def test_wait_returns_immediately_when_watermark_already_ahead() -> None:
    notifier = RunNotifier()
    notifier.notify("r1", 3)
    start = time.monotonic()
    assert notifier.wait("r1", after=1, timeout=5.0) == 3
    # Did not block for anything near the timeout.
    assert time.monotonic() - start < 1.0


def test_wait_times_out_and_returns_current_watermark() -> None:
    notifier = RunNotifier()
    start = time.monotonic()
    assert notifier.wait("r1", after=0, timeout=0.05) == 0
    assert time.monotonic() - start >= 0.04


def test_notify_wakes_a_blocked_waiter() -> None:
    notifier = RunNotifier()
    result: dict[str, int] = {}
    entered = threading.Event()

    def waiter() -> None:
        entered.set()
        result["watermark"] = notifier.wait("r1", after=0, timeout=5.0)

    thread = threading.Thread(target=waiter)
    thread.start()
    assert entered.wait(1.0)
    # Give the waiter a beat to enter wait(); even if notify races ahead,
    # wait() sees watermark > after and returns without blocking.
    time.sleep(0.05)
    notifier.notify("r1", 7)
    thread.join(2.0)
    assert not thread.is_alive()
    assert result["watermark"] == 7


def test_watermark_is_monotonic() -> None:
    notifier = RunNotifier()
    notifier.notify("r1", 5)
    notifier.notify("r1", 2)  # stale: must not lower the watermark
    assert notifier.wait("r1", after=0, timeout=0.01) == 5


def test_watermarks_are_isolated_per_run() -> None:
    notifier = RunNotifier()
    notifier.notify("r1", 5)
    assert notifier.wait("r2", after=0, timeout=0.01) == 0


def test_forget_resets_run_state() -> None:
    notifier = RunNotifier()
    notifier.notify("r1", 5)
    notifier.forget("r1")
    assert notifier.wait("r1", after=0, timeout=0.01) == 0


# --- Store signalling -------------------------------------------------------


def _seed(store: InMemoryStore, run_id: str) -> None:
    store.append_domain_event(
        LifecycleInitialized(run_id=run_id, ts=_ts(0), task_id="t"),
        expected_version=0,
    )


def test_store_writes_advance_the_notifier_watermark() -> None:
    store = InMemoryStore()
    _seed(store, "r1")  # one domain append
    wm = store.notifier.wait("r1", after=0, timeout=0.01)
    assert wm >= 1

    store.append_event(
        EventRecord(run_id="r1", ts=_ts(1), kind="harness.x")
    )
    wm_after_event = store.notifier.wait("r1", after=wm, timeout=0.01)
    assert wm_after_event > wm

    store.save_sdk_messages("r1", 1, 1, [{"type": "assistant"}])
    wm_after_sdk = store.notifier.wait(
        "r1", after=wm_after_event, timeout=0.01
    )
    assert wm_after_sdk > wm_after_event


def test_default_store_exposes_a_notifier() -> None:
    assert _resolve_notifier(InMemoryStore()) is not None


# --- Audit follow: push path and poll fallback ------------------------------


def _drive_to_running(store: InMemoryStore, run_id: str) -> None:
    _seed(store, run_id)
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_ts(1), target=Status.READY),
        expected_version=1,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_ts(2), target=Status.RUNNING),
        expected_version=2,
    )


def test_follow_delivers_records_written_after_it_started() -> None:
    """A follower blocked on the notifier collects a record appended later
    and terminates once the lifecycle goes terminal. poll_interval is set
    high so the run relies on the push wakeup, not the poll fallback."""
    store = InMemoryStore()
    run_id = "run-follow"
    _drive_to_running(store, run_id)

    def writer() -> None:
        time.sleep(0.05)
        store.append_event(
            EventRecord(
                run_id=run_id,
                ts=_ts(3),
                kind="harness.iteration_completed",
            )
        )
        store.append_domain_event(
            TransitionedTo(
                run_id=run_id,
                ts=_ts(4),
                target=Status.FAILED,
                error="boom",
            ),
            expected_version=3,
        )

    thread = threading.Thread(target=writer)
    thread.start()
    records = list(
        stream(run_id, store=store, follow=True, poll_interval=5.0)
    )
    thread.join(2.0)

    kinds = [r.kind for r in records if isinstance(r, EventRecord)]
    assert "harness.iteration_completed" in kinds


class _PollOnlyStore:
    """Wraps a store but hides its notifier, forcing the poll fallback."""

    def __init__(self, inner: InMemoryStore) -> None:
        self._inner = inner

    def read_audit_since(self, run_id: str, cursor: int) -> list:
        return self._inner.read_audit_since(run_id, cursor)

    def load_lifecycle(self, run_id: str):
        return self._inner.load_lifecycle(run_id)


def test_follow_without_notifier_falls_back_to_poll() -> None:
    inner = InMemoryStore()
    run_id = "run-poll"
    _drive_to_running(inner, run_id)
    store = _PollOnlyStore(inner)
    assert _resolve_notifier(store) is None

    def writer() -> None:
        time.sleep(0.02)
        inner.append_event(
            EventRecord(run_id=run_id, ts=_ts(3), kind="late.event")
        )
        inner.append_domain_event(
            TransitionedTo(
                run_id=run_id,
                ts=_ts(4),
                target=Status.FAILED,
                error="boom",
            ),
            expected_version=3,
        )

    thread = threading.Thread(target=writer)
    thread.start()
    records = list(
        stream(run_id, store=store, follow=True, poll_interval=0.02)
    )
    thread.join(2.0)

    kinds = [r.kind for r in records if isinstance(r, EventRecord)]
    assert "late.event" in kinds
