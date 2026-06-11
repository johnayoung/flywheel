"""Tests for the in-process reactive notifier (P1).

Split into three layers:

* the :class:`RunNotifier` mechanism in isolation (deterministic, no
  reliance on wall-clock thresholds beyond a generous timeout), and
* the store signalling its notifier on every write.

The audit follow loop no longer consumes notifier wakeups: since spec
00025 the observability stream is tailed from the per-run JSONL file
(see ``test_audit.py``), so the notifier serves in-process store
consumers (e.g. live dashboards) only.
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
    assert isinstance(InMemoryStore().notifier, RunNotifier)
