"""Streaming consumer surface for the per-run audit log.

This module is the canonical reader for the totally-ordered audit stream
that :class:`flywheel.store_protocols.AuditStore` exposes via
``read_audit_since``. It serves two consumers from one iterator:

* Replay (``follow=False``): drain every persisted record for the run
  and stop.
* Live tail (``follow=True``): drain, then poll the store until the
  lifecycle reaches a terminal status. The follow loop intentionally
  drains again *after* observing the terminal status so the final write
  is never dropped.

A separate convenience, :func:`attach_logger`, spawns a daemon thread
that subscribes to ``follow=True`` and emits each record through a
caller-supplied :class:`logging.Logger`. The returned
:class:`AuditLoggerHandle` exposes ``detach()`` to stop emission and
join the thread.

Implementation notes:

* The module owns no persistence and serializes nothing. It traffics in
  the typed dataclasses produced by the store; consumers re-key onto
  them by ``isinstance``.
* Cursor handling is local to one call: each ``stream()`` invocation
  starts at ``cursor=0`` and advances on the maximum ``sequence`` seen.
  Concurrent consumers therefore observe the same total ordering with
  independent cursors.
* Polling uses :func:`time.sleep`; sleeps happen between pages, never
  in the middle of consuming one.
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Iterator
from dataclasses import asdict, is_dataclass
from typing import Any

from flywheel.lifecycle import Status
from flywheel.notifier import RunNotifier
from flywheel.store_protocols import (
    AuditRecord,
    AuditStore,
    EventRecord,
    SdkMessageRecord,
)


# Terminal lifecycle states for the follow loop's exit predicate. Mirrors
# the leaves in ``flywheel.lifecycle._VALID_EDGES`` plus the
# "abort-style" terminals (``INTERNAL_ERROR``, ``INTERRUPTED``,
# ``BLOCKED``-equivalent gating). The follow loop stops when the
# lifecycle is in one of these AND a subsequent page yielded no new
# records.
_TERMINAL_STATUSES: frozenset[Status] = frozenset(
    {
        Status.DONE,
        Status.FAILED,
        Status.INTERNAL_ERROR,
        Status.INTERRUPTED,
    }
)


def _record_sequence(record: AuditRecord) -> int:
    """Return ``record.sequence`` or ``0`` if the store omitted it.

    The store contract assigns a non-``None`` sequence on every record it
    returns from ``read_audit_since``, so this fallback exists only as a
    defensive guard for downstream cursor math.
    """
    seq = record.sequence
    return 0 if seq is None else seq


def _lifecycle_status(store: object, run_id: str) -> Status | None:
    """Look up the lifecycle's current status, or ``None`` if missing.

    ``stream(follow=True)`` is an opt-in consumer of the
    :class:`~flywheel.store_protocols.LifecycleStore` protocol but does
    not statically require it; the iterator works against any object
    that satisfies :class:`AuditStore` and may *additionally* expose
    ``load_lifecycle``. Returns ``None`` when the store does not expose
    ``load_lifecycle`` or when no row exists for ``run_id``.
    """
    loader = getattr(store, "load_lifecycle", None)
    if loader is None:
        return None
    lifecycle = loader(run_id)
    if lifecycle is None:
        return None
    return lifecycle.status


def _lifecycle_exists(store: object, run_id: str) -> bool:
    """Return ``True`` if the store has a lifecycle row for ``run_id``.

    Used by the follow loop to enforce the spec error-handling rule:
    an unknown ``run_id`` yields nothing and returns immediately, even
    under ``follow=True``. Stores without a ``load_lifecycle`` method
    are treated as if every ``run_id`` exists (we cannot prove
    otherwise from the audit surface alone).
    """
    loader = getattr(store, "load_lifecycle", None)
    if loader is None:
        return True
    return loader(run_id) is not None


def _drain(store: AuditStore, run_id: str, cursor: int) -> tuple[list[AuditRecord], int]:
    """Drain every currently persisted record above ``cursor``.

    Repeatedly calls ``read_audit_since`` until the store returns an
    empty page. Returns the concatenated records and the new cursor (the
    maximum sequence observed, or the original cursor when the drain
    produced nothing).
    """
    drained: list[AuditRecord] = []
    while True:
        page = store.read_audit_since(run_id, cursor)
        if not page:
            return drained, cursor
        drained.extend(page)
        cursor = max(_record_sequence(r) for r in page)


def _resolve_notifier(store: object) -> RunNotifier | None:
    """Return the store's in-process notifier, or ``None`` for poll-only.

    Read by duck typing so the audit surface stays decoupled from the
    persistence layer: any store may expose a ``notifier`` attribute to
    opt into push wakeups; one that does not falls back to bounded poll.
    """
    notifier = getattr(store, "notifier", None)
    return notifier if isinstance(notifier, RunNotifier) else None


def _wait(
    notifier: RunNotifier | None,
    run_id: str,
    watermark: int,
    timeout: float,
    stop: threading.Event | None,
) -> int:
    """Block until the next write or ``timeout``; return the new watermark.

    With a notifier this is a push wakeup bounded by ``timeout`` (so a
    missed signal only costs latency). Without one it degrades to a sleep
    — interruptible via ``stop`` for responsive cancellation. The returned
    watermark lets the caller advance past sequences it was woken for
    (including domain events the audit stream does not surface) so a
    subsequent wait does not spin.
    """
    if notifier is not None:
        return notifier.wait(run_id, watermark, timeout)
    if stop is not None:
        stop.wait(timeout)
    else:
        time.sleep(timeout)
    return watermark


def _follow(
    store: AuditStore,
    run_id: str,
    *,
    cursor: int,
    poll_interval: float,
    stop: threading.Event | None = None,
) -> Iterator[AuditRecord]:
    """Yield records as they land until the lifecycle is terminal.

    The single follow loop shared by :func:`stream` and
    :class:`AuditLoggerHandle`. ``cursor`` is the read position after the
    caller's initial drain; ``stop`` (when given) makes the loop and its
    waits cooperatively cancellable. A terminal lifecycle triggers one
    final drain before the loop exits, so the write committed alongside
    the terminal transition is never dropped.
    """
    notifier = _resolve_notifier(store)
    watermark = cursor
    while True:
        if stop is not None and stop.is_set():
            return
        if not _lifecycle_exists(store, run_id):
            return
        terminal = _lifecycle_status(store, run_id) in _TERMINAL_STATUSES
        if not terminal:
            watermark = _wait(
                notifier, run_id, watermark, poll_interval, stop
            )
        drained, cursor = _drain(store, run_id, cursor)
        if drained:
            yield from drained
            watermark = max(watermark, cursor)
            continue
        if terminal:
            return


def stream(
    run_id: str,
    *,
    store: AuditStore,
    follow: bool = False,
    poll_interval: float = 0.25,
) -> Iterator[AuditRecord]:
    """Yield audit records for ``run_id`` in ascending sequence order.

    ``follow=False`` drains every persisted record and returns. An
    unknown ``run_id`` yields nothing and returns — no exception, per
    the spec error-handling table.

    ``follow=True`` drains, then polls ``store.read_audit_since`` every
    ``poll_interval`` seconds until the lifecycle reaches a terminal
    status (:attr:`Status.DONE`, :attr:`Status.FAILED`,
    :attr:`Status.INTERNAL_ERROR`, :attr:`Status.INTERRUPTED`). After
    observing a terminal status the loop performs one more drain so the
    final write committed alongside the terminal transition is never
    dropped.
    """
    cursor = 0
    drained, cursor = _drain(store, run_id, cursor)
    for record in drained:
        yield record
    if not follow:
        return
    if not _lifecycle_exists(store, run_id):
        # Spec error-handling table: an unknown run_id yields nothing
        # and returns even under follow=True. Without this guard the
        # follow loop would spin forever waiting on a lifecycle that
        # will never exist.
        return
    yield from _follow(
        store, run_id, cursor=cursor, poll_interval=poll_interval
    )


class AuditLoggerHandle:
    """Handle returned by :func:`attach_logger`.

    Holds the background daemon thread and a stop flag. ``detach()``
    flips the flag, the thread finishes the current page, and the
    handle joins it with a short timeout so callers don't hang on a
    sleeping consumer.
    """

    _JOIN_TIMEOUT_SECONDS: float = 1.0

    def __init__(
        self,
        *,
        run_id: str,
        store: AuditStore,
        logger: logging.Logger,
        poll_interval: float,
    ) -> None:
        self._run_id = run_id
        self._store = store
        self._logger = logger
        self._poll_interval = poll_interval
        self._stop_event = threading.Event()
        self._thread = threading.Thread(
            target=self._run,
            name=f"flywheel-audit-logger-{run_id}",
            daemon=True,
        )
        self._thread.start()

    def _run(self) -> None:
        """Background loop: drain pages and emit one LogRecord per row.

        Shares the initial drain and the :func:`_follow` loop with
        :func:`stream`, passing its stop flag so ``detach()`` cancels the
        loop (and its waits) cooperatively. The stop flag is also checked
        between emits so a large drained batch does not delay cancellation.
        """
        cursor = 0
        drained, cursor = _drain(self._store, self._run_id, cursor)
        for record in drained:
            self._emit(record)
            if self._stop_event.is_set():
                return
        if self._stop_event.is_set():
            return
        for record in _follow(
            self._store,
            self._run_id,
            cursor=cursor,
            poll_interval=self._poll_interval,
            stop=self._stop_event,
        ):
            self._emit(record)
            if self._stop_event.is_set():
                return

    def _emit(self, record: AuditRecord) -> None:
        kind = _record_kind(record)
        seq = _record_sequence(record)
        msg = f"audit run={self._run_id} seq={seq} kind={kind}"
        self._logger.log(
            logging.INFO,
            msg,
            extra={"audit_record": _record_as_dict(record)},
        )

    def detach(self) -> None:
        """Stop the background emitter and join the thread.

        Idempotent: calling ``detach()`` after the thread has already
        exited is a no-op. The join uses a short timeout so a thread
        stuck in a slow logger handler does not block the caller
        indefinitely.
        """
        self._stop_event.set()
        if self._thread.is_alive():
            self._thread.join(timeout=self._JOIN_TIMEOUT_SECONDS)

    @property
    def is_alive(self) -> bool:
        """Whether the background thread is still running."""
        return self._thread.is_alive()


def attach_logger(
    logger: logging.Logger,
    *,
    run_id: str,
    store: AuditStore,
    poll_interval: float = 0.25,
) -> AuditLoggerHandle:
    """Emit every audit record for ``run_id`` through ``logger``.

    Spawns a daemon thread that follows the audit stream and calls
    ``logger.log(logging.INFO, msg, extra={'audit_record': ...})`` for
    each record. ``msg`` is a short human-readable label of the form
    ``audit run=<id> seq=<n> kind=<k>``; structured fields live in the
    ``extra`` dict so handlers can route them to JSON sinks.

    The returned handle's ``detach()`` stops emission and joins the
    thread. There are no global side effects: calling ``attach_logger``
    twice with the same logger returns distinct handles, each owning
    its own thread.
    """
    return AuditLoggerHandle(
        run_id=run_id,
        store=store,
        logger=logger,
        poll_interval=poll_interval,
    )


def _record_kind(record: AuditRecord) -> str:
    """Return a short label for the record type for log lines."""
    if isinstance(record, EventRecord):
        return f"event:{record.kind}"
    if isinstance(record, SdkMessageRecord):
        return f"sdk:{record.message_type}"
    return type(record).__name__


def _record_as_dict(record: AuditRecord) -> dict[str, Any]:
    """Convert a record dataclass to a plain dict for logging ``extra``.

    Uses :func:`dataclasses.asdict` when available so nested
    ``Mapping`` payloads are deep-copied. Falls back to a manual cast
    for non-dataclass types (defensive — both record types are
    dataclasses today).
    """
    if is_dataclass(record):
        return asdict(record)
    return {"record": record}


__all__ = [
    "AuditLoggerHandle",
    "AuditRecord",
    "attach_logger",
    "stream",
]
