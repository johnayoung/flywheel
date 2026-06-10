"""Streaming consumer surface for the per-run audit log.

This module is the canonical reader for the totally-ordered audit stream
that :class:`flywheel_core.store_protocols.AuditStore` exposes via
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
from collections.abc import Callable, Iterator
from dataclasses import asdict, is_dataclass
from typing import Any, Protocol, runtime_checkable

from flywheel_core.lifecycle import Status
from flywheel_core.notifier import RunNotifier
from flywheel_core.redaction import Redactor
from flywheel_core.store_protocols import (
    AuditRecord,
    AuditStore,
    EventRecord,
    SdkMessageRecord,
)


_LOGGER = logging.getLogger(__name__)


@runtime_checkable
class EventHandler(Protocol):
    """A read-only subscriber to the per-run audit stream.

    Handlers receive each :class:`AuditRecord` in ascending ``sequence``
    order and return nothing. They observe only committed state and are
    handed no store or lifecycle handle, so a subscriber cannot mutate
    authoritative state — the harness remains the sole owner of
    transitions. A handler that raises is isolated (see
    :class:`Subscription`); it never breaks the dispatcher or other
    subscribers.
    """

    def on_record(self, record: AuditRecord) -> None: ...


# Terminal lifecycle states for the follow loop's exit predicate. Mirrors
# the leaves in ``flywheel_core.lifecycle._VALID_EDGES`` plus the
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
    :class:`~flywheel_core.store_protocols.LifecycleStore` protocol but does
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
    redactor: Redactor | None = None,
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

    When ``redactor`` is supplied, every record passes through
    ``redactor.redact`` at the single yield seam — *after* the internal
    cursor/watermark math has read the original ``record.sequence`` — so
    follow semantics, ordering, and cursor advancement are unchanged.
    A redactor exception propagates to the caller (the iterator surfaces
    other errors the same way). ``redactor=None`` (the default) is
    verbatim and byte-for-byte identical to the pre-redaction behavior.
    """
    cursor = 0
    drained, cursor = _drain(store, run_id, cursor)
    for record in drained:
        yield record if redactor is None else redactor.redact(record)
    if not follow:
        return
    if not _lifecycle_exists(store, run_id):
        # Spec error-handling table: an unknown run_id yields nothing
        # and returns even under follow=True. Without this guard the
        # follow loop would spin forever waiting on a lifecycle that
        # will never exist.
        return
    for record in _follow(
        store, run_id, cursor=cursor, poll_interval=poll_interval
    ):
        yield record if redactor is None else redactor.redact(record)


class Subscription:
    """A background follower that dispatches each audit record to a handler.

    Spawns one daemon thread that drains the run's backlog, then follows
    the live stream (reusing :func:`_follow`, so it consumes notifier push
    wakeups with poll as the bounded fallback). Each record is handed to
    ``handler`` in ascending ``sequence`` order.

    **Error isolation.** Every ``handler`` call is wrapped: a raising
    handler is reported (via ``on_error`` if supplied, else logged at
    WARNING) and the dispatcher continues to the next record. A faulty
    subscriber therefore never breaks the follow loop. Because each
    subscription owns its own thread, it also cannot affect sibling
    subscribers. This mirrors the harness's best-effort audit discipline.

    **Read-only.** The handler receives only the :class:`AuditRecord`; it
    is given no store or lifecycle handle, so it cannot mutate authoritative
    state. The harness stays the sole owner of transitions.

    **Lifecycle.** The thread exits on its own when the lifecycle reaches a
    terminal status (and the final drain is empty) or when the ``run_id``
    is unknown. :meth:`unsubscribe` stops it early and joins with a short
    timeout so a slow handler cannot block the caller indefinitely.
    """

    _JOIN_TIMEOUT_SECONDS: float = 1.0

    def __init__(
        self,
        *,
        run_id: str,
        store: AuditStore,
        handler: Callable[[AuditRecord], None],
        poll_interval: float = 0.25,
        on_error: Callable[[AuditRecord, BaseException], None] | None = None,
        redactor: Redactor | None = None,
    ) -> None:
        self._run_id = run_id
        self._store = store
        self._handler = handler
        self._poll_interval = poll_interval
        self._on_error = on_error
        self._redactor = redactor
        self._stop_event = threading.Event()
        self._thread = threading.Thread(
            target=self._run,
            name=f"flywheel-subscription-{run_id}",
            daemon=True,
        )
        self._thread.start()

    def _run(self) -> None:
        cursor = 0
        drained, cursor = _drain(self._store, self._run_id, cursor)
        for record in drained:
            self._dispatch(record)
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
            self._dispatch(record)
            if self._stop_event.is_set():
                return

    def _dispatch(self, record: AuditRecord) -> None:
        try:
            visible = (
                record
                if self._redactor is None
                else self._redactor.redact(record)
            )
            self._handler(visible)
        except Exception as exc:  # noqa: BLE001 - isolation is the point
            if self._on_error is not None:
                try:
                    self._on_error(record, exc)
                except Exception:  # noqa: BLE001 - never let on_error loop
                    pass
            else:
                _LOGGER.warning(
                    "audit subscriber for run=%s raised on seq=%s: %s: %s",
                    self._run_id,
                    _record_sequence(record),
                    type(exc).__name__,
                    exc,
                )

    def unsubscribe(self) -> None:
        """Stop the background dispatcher and join its thread.

        Idempotent: calling it after the thread has exited is a no-op. The
        join uses a short timeout so a thread stuck in a slow handler does
        not block the caller indefinitely. When the store exposes a
        notifier, the dispatcher may be parked in a (possibly long) wait;
        ``wake`` nudges it so it re-checks the stop flag and exits promptly
        regardless of ``poll_interval``.
        """
        self._stop_event.set()
        notifier = _resolve_notifier(self._store)
        if notifier is not None:
            notifier.wake(self._run_id)
        if self._thread.is_alive():
            self._thread.join(timeout=self._JOIN_TIMEOUT_SECONDS)

    @property
    def is_alive(self) -> bool:
        """Whether the background thread is still running."""
        return self._thread.is_alive()


def subscribe(
    handler: EventHandler | Callable[[AuditRecord], None],
    *,
    run_id: str,
    store: AuditStore,
    poll_interval: float = 0.25,
    on_error: Callable[[AuditRecord, BaseException], None] | None = None,
    redactor: Redactor | None = None,
) -> Subscription:
    """Subscribe ``handler`` to ``run_id``'s audit stream.

    ``handler`` may be an :class:`EventHandler` (an object with
    ``on_record``) or a plain ``Callable[[AuditRecord], None]``. The
    subscriber runs on its own daemon thread, observes records in
    ``sequence`` order, and is isolated: a raising handler is reported and
    the stream continues. Returns a :class:`Subscription` whose
    ``unsubscribe()`` stops and joins the thread.

    When ``redactor`` is supplied, every record passes through
    ``redactor.redact`` before the handler sees it, applied inside the
    same per-record isolation that wraps the handler — a raising redactor
    is reported (via ``on_error`` or the module logger) and the stream
    continues. ``redactor=None`` is verbatim.

    Plugins register here without touching the harness and cannot corrupt
    lifecycle state — they only read committed records.
    """
    if isinstance(handler, EventHandler):
        callback: Callable[[AuditRecord], None] = handler.on_record
    elif callable(handler):
        callback = handler
    else:  # pragma: no cover - guarded by the type signature
        raise TypeError(
            "handler must be an EventHandler or a callable taking one "
            "AuditRecord"
        )
    return Subscription(
        run_id=run_id,
        store=store,
        handler=callback,
        poll_interval=poll_interval,
        on_error=on_error,
        redactor=redactor,
    )


class AuditLoggerHandle(Subscription):
    """Handle returned by :func:`attach_logger`.

    Thin back-compat shim over :class:`Subscription`: ``detach()`` is an
    alias for :meth:`Subscription.unsubscribe`. Holds the background daemon
    thread and a stop flag; stopping joins the thread with a short timeout
    so callers don't hang on a sleeping consumer.
    """

    def __init__(
        self,
        *,
        run_id: str,
        store: AuditStore,
        logger: logging.Logger,
        poll_interval: float,
        redactor: Redactor | None = None,
    ) -> None:
        self._logger = logger
        super().__init__(
            run_id=run_id,
            store=store,
            handler=self._emit,
            poll_interval=poll_interval,
            redactor=redactor,
        )

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
        """Stop the background emitter and join the thread (alias for
        :meth:`Subscription.unsubscribe`)."""
        self.unsubscribe()


def attach_logger(
    logger: logging.Logger,
    *,
    run_id: str,
    store: AuditStore,
    poll_interval: float = 0.25,
    redactor: Redactor | None = None,
) -> AuditLoggerHandle:
    """Emit every audit record for ``run_id`` through ``logger``.

    A convenience subscriber: spawns a daemon thread that follows the
    audit stream and calls ``logger.log(logging.INFO, msg,
    extra={'audit_record': ...})`` for each record. ``msg`` is a short
    human-readable label of the form ``audit run=<id> seq=<n> kind=<k>``;
    structured fields live in the ``extra`` dict so handlers can route
    them to JSON sinks.

    When ``redactor`` is supplied, every record passes through
    ``redactor.redact`` before it is emitted to ``logger``; redactor
    exceptions are isolated per-record exactly like a raising handler
    (reported and the stream continues). ``redactor=None`` is verbatim.

    The returned handle's ``detach()`` stops emission and joins the
    thread. There are no global side effects: calling ``attach_logger``
    twice with the same logger returns distinct handles, each owning its
    own thread. Equivalent to :func:`subscribe` with a logging handler.
    """
    return AuditLoggerHandle(
        run_id=run_id,
        store=store,
        logger=logger,
        poll_interval=poll_interval,
        redactor=redactor,
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
    "EventHandler",
    "Subscription",
    "attach_logger",
    "stream",
    "subscribe",
]
