"""In-process reactive notification for the per-run audit log.

A :class:`RunNotifier` is a *doorbell with a watermark*, not a delivery
truck. A producer (a store) calls :meth:`notify` with the per-run
``sequence`` it just committed; a consumer (the audit follower) calls
:meth:`wait` and, once woken, re-reads the authoritative records via
``read_audit_since``. The notification carries only a watermark, never a
payload, so a dropped or coalesced signal costs latency, never
correctness — every :meth:`wait` is bounded by a timeout, so the consumer
makes progress even if a notification is missed entirely.

Scope: in-process only. A wakeup bridges a producer and a consumer only
when they share the same :class:`RunNotifier` instance (in practice, the
same store object). Separate store instances or separate processes fall
back to the consumer's poll timeout. Cross-process push is a later,
backend-specific layer (e.g. Postgres ``LISTEN``/``NOTIFY``).

Thread-safety: every method is safe to call from any thread. Producers
typically run on the harness thread; consumers run on the audit
follower's thread.
"""

from __future__ import annotations

import threading


class _RunState:
    """Per-run condition variable and the highest sequence seen so far."""

    __slots__ = ("cond", "watermark")

    def __init__(self) -> None:
        self.cond = threading.Condition()
        self.watermark = 0


class RunNotifier:
    """Per-run sequence watermarks with condition-variable wakeups."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._runs: dict[str, _RunState] = {}

    def _state(self, run_id: str) -> _RunState:
        with self._lock:
            state = self._runs.get(run_id)
            if state is None:
                state = _RunState()
                self._runs[run_id] = state
            return state

    def notify(self, run_id: str, sequence: int) -> None:
        """Advance ``run_id``'s watermark to ``sequence`` and wake waiters.

        Monotonic: an out-of-order or duplicate ``sequence`` never lowers
        the watermark. Waiters are always notified so a consumer that
        passed a stale ``after`` re-checks promptly.
        """
        state = self._state(run_id)
        with state.cond:
            if sequence > state.watermark:
                state.watermark = sequence
            state.cond.notify_all()

    def wait(self, run_id: str, after: int, timeout: float) -> int:
        """Block until ``run_id``'s watermark exceeds ``after`` or
        ``timeout`` elapses; return the current watermark.

        Returning the observed watermark lets the caller advance its own
        ``after`` past sequences it has been woken for — including
        state-bearing domain events that the audit stream does not surface
        — so a subsequent ``wait`` does not return immediately in a spin.
        """
        state = self._state(run_id)
        with state.cond:
            if state.watermark > after:
                return state.watermark
            state.cond.wait(timeout)
            return state.watermark

    def wake(self, run_id: str) -> None:
        """Wake any waiters on ``run_id`` without advancing the watermark.

        Used to make a blocked consumer re-check a condition other than the
        watermark — e.g. its own stop flag — promptly, so cancellation does
        not have to wait out the consumer's timeout. A woken ``wait``
        returns the unchanged watermark.
        """
        state = self._state(run_id)
        with state.cond:
            state.cond.notify_all()

    def forget(self, run_id: str) -> None:
        """Drop the cached state for ``run_id`` to bound memory.

        Optional cleanup for long-lived processes. A waiter blocked on the
        forgotten state simply times out (a later ``notify`` allocates a
        fresh state); callers that race ``forget`` against live consumers
        accept that bounded-latency miss.
        """
        with self._lock:
            self._runs.pop(run_id, None)


__all__ = ["RunNotifier"]
