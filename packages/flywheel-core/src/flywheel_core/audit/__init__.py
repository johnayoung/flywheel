"""Streaming consumer surface for the per-run observability stream.

This module is the canonical reader for a run's telemetry stream. Since
spec 00025 the durable destination is the per-run JSONL file written by
:class:`flywheel_core.telemetry_file.FileTelemetrySink` at
``<logs_root>/runs/<run_id>.jsonl`` — not the store. The reader tails
that file (cursor = byte offset + line count, via
:mod:`flywheel_core.audit._file`) and reconstructs the same
:class:`AuditRecord` shapes consumers always received, so the record
contract, the :class:`~flywheel_core.redaction.Redactor`, and the TUI
classifier are unchanged by the destination move.

Two consumers are served from one iterator:

* Replay (``follow=False``): drain every line currently in the file and
  stop. A missing file reads as empty.
* Live tail (``follow=True``): drain, then poll the file until the
  lifecycle reaches a terminal status. The store is still the terminal
  oracle — ``load_lifecycle`` decides when the run is over; the file
  decides what happened. The follow loop drains once more *after*
  observing the terminal status so the final write is never dropped,
  and a partial trailing line (crash mid-write) is withheld until
  complete or discarded on that final pass.

A separate convenience, :func:`attach_logger`, spawns a daemon thread
that subscribes to ``follow=True`` and emits each record through a
caller-supplied :class:`logging.Logger`. The returned
:class:`AuditLoggerHandle` exposes ``detach()`` to stop emission and
join the thread.

Implementation notes:

* Cursor handling is local to one call: each ``stream()`` invocation
  starts at the beginning of the file and advances on complete lines.
  Concurrent consumers therefore observe the same total ordering with
  independent cursors.
* Read-time redaction (spec 00014) applies unchanged at the single
  yield seam; the file is verbatim and sensitive-by-default like the
  store was.
* Polling uses interruptible sleeps; sleeps happen between pages, never
  in the middle of consuming one.
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import asdict, is_dataclass
from pathlib import Path
from typing import Any, Protocol, runtime_checkable

from flywheel_core.audit._file import (
    FileCursor,
    read_records_since,
    run_file_path,
)
from flywheel_core.event_serde import event_kind, event_payload
from flywheel_core.events import DomainEventKind
from flywheel_core.lifecycle import Status
from flywheel_core.redaction import Redactor
from flywheel_core.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)
from flywheel_core.telemetry_file import DEFAULT_LOGS_ROOT


_LOGGER = logging.getLogger(__name__)


@runtime_checkable
class EventHandler(Protocol):
    """A read-only subscriber to the per-run observability stream.

    Handlers receive each :class:`AuditRecord` in file order and return
    nothing. They observe only committed lines and are handed no store
    or lifecycle handle, so a subscriber cannot mutate authoritative
    state — the harness remains the sole owner of transitions. A
    handler that raises is isolated (see :class:`Subscription`); it
    never breaks the dispatcher or other subscribers.
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


# Kind prefix the in-run mirror (``_RunTelemetry.mirror_domain``) stamps onto a
# ledger event it writes into the run file. Reused here so a landing-stage
# record projected from the store carries the identical ``domain.<kind>`` label,
# and the CLI/logger renderers see one shape regardless of the source.
_DOMAIN_MIRROR_KIND_PREFIX: str = "domain."

# Landing-stage ledger records the stream projects at the tail, after every file
# record (i.e. after attempt finalization). These decisions are appended to the
# store *after* the run finalized, so the harness's in-run mirror never wrote
# them to the run file; the reader would skip a ``domain.*`` line anyway. They
# live only in the store (store-only durability), so the stream reads them from
# the ledger at yield time rather than the file. A run with no landing decision
# has none of these events and streams exactly as before.
_LANDING_STAGE_KINDS: frozenset[str] = frozenset(
    {
        DomainEventKind.HELD_OUT_GATE_EVALUATED.value,
        DomainEventKind.LANDING_PARKED.value,
        DomainEventKind.LANDED.value,
    }
)


def _landing_stage_records(
    store: object, run_id: str, *, start_sequence: int
) -> list[EventRecord]:
    """Project the run's landing-stage ledger events as tail records.

    Reads the authoritative domain-event ledger via the store's optional
    ``list_domain_events`` and reconstructs each landing-stage event (held-out
    gate verdict, landing-parked witness, or landed record) as an
    :class:`EventRecord` carrying the same ``domain.<kind>`` label and payload
    projection the in-run mirror uses. Records keep ledger order and are
    numbered strictly after ``start_sequence`` (the last file record's
    sequence) so the audit stream's ascending-sequence contract holds across
    the file/ledger seam.

    Best-effort and read-only: a store without ``list_domain_events`` or a read
    that raises yields no tail records — the ledger is authoritative and the
    stream is a disposable projection, so a lookup failure must never break the
    live file tail. Returns an empty list when the run has no landing decision,
    which keeps a non-landing run byte-identical to the pre-projection stream.
    """
    lister = getattr(store, "list_domain_events", None)
    if lister is None:
        return []
    try:
        events = lister(run_id)
    except Exception:  # noqa: BLE001 - a ledger read must not break the stream
        return []
    records: list[EventRecord] = []
    seq = start_sequence
    for event in events:
        if event_kind(event) not in _LANDING_STAGE_KINDS:
            continue
        seq += 1
        records.append(
            EventRecord(
                run_id=run_id,
                ts=event.ts,
                kind=f"{_DOMAIN_MIRROR_KIND_PREFIX}{event_kind(event)}",
                payload=event_payload(event),
                attempt_number=event.attempt_number,
                sequence=seq,
            )
        )
    return records


def _record_sequence(record: AuditRecord) -> int:
    """Return ``record.sequence`` or ``0`` if it was omitted.

    The file reader assigns a line-count sequence on every record it
    reconstructs, so this fallback exists only as a defensive guard for
    synthetic records fed in directly by tests.
    """
    seq = record.sequence
    return 0 if seq is None else seq


def _lifecycle_status(store: object, run_id: str) -> Status | None:
    """Look up the lifecycle's current status, or ``None`` if missing.

    ``stream(follow=True)`` is an opt-in consumer of the
    :class:`~flywheel_core.store_protocols.LifecycleStore` protocol but does
    not statically require it; the iterator works against any object
    that may expose ``load_lifecycle``. Returns ``None`` when the store
    does not expose ``load_lifecycle`` or when no row exists for
    ``run_id``.
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
    otherwise from the read surface alone).
    """
    loader = getattr(store, "load_lifecycle", None)
    if loader is None:
        return True
    return loader(run_id) is not None


def _resolve_logs_root(logs_root: str | Path | None) -> Path:
    """Default the logs root to the spec's ``.flywheel/logs``."""
    return Path(logs_root) if logs_root is not None else DEFAULT_LOGS_ROOT


def _follow(
    store: object,
    run_id: str,
    path: Path,
    *,
    cursor: FileCursor,
    poll_interval: float,
    stop: threading.Event | None = None,
) -> Iterator[AuditRecord]:
    """Yield records as lines land until the lifecycle is terminal.

    The single follow loop shared by :func:`stream` and
    :class:`Subscription`. ``cursor`` is the read position after the
    caller's initial drain; ``stop`` (when given) makes the loop and its
    waits cooperatively cancellable. A terminal lifecycle triggers one
    final drain before the loop exits, so the line written alongside the
    terminal transition is never dropped; that final pass also discards
    a partial trailing line (crash mid-write) instead of waiting for a
    completion that will never come. A run whose file does not exist
    yet reads as empty — the loop keeps waiting for it to appear.
    """
    while True:
        if stop is not None and stop.is_set():
            return
        if not _lifecycle_exists(store, run_id):
            return
        terminal = _lifecycle_status(store, run_id) in _TERMINAL_STATUSES
        if not terminal:
            if stop is not None:
                stop.wait(poll_interval)
            else:
                time.sleep(poll_interval)
        drained, cursor = read_records_since(
            path, cursor, eof_final=terminal
        )
        if drained:
            yield from drained
            continue
        if terminal:
            return


def stream(
    run_id: str,
    *,
    store: object,
    logs_root: str | Path | None = None,
    follow: bool = False,
    poll_interval: float = 0.25,
    redactor: Redactor | None = None,
) -> Iterator[AuditRecord]:
    """Yield the run's observability records in file order.

    Records come from ``<logs_root>/runs/<run_id>.jsonl`` (default logs
    root ``.flywheel/logs``); ``store`` supplies only the lifecycle row
    for the unknown-run guard and the terminal-status exit.

    ``follow=False`` drains every complete line currently in the file
    and returns. A missing file (or unknown ``run_id``) yields nothing
    and returns — no exception, per the spec error-handling table.

    ``follow=True`` drains, then polls the file every ``poll_interval``
    seconds until the lifecycle reaches a terminal status
    (:attr:`Status.DONE`, :attr:`Status.FAILED`,
    :attr:`Status.INTERNAL_ERROR`, :attr:`Status.INTERRUPTED`). After
    observing a terminal status the loop performs one more drain so the
    final write committed alongside the terminal transition is never
    dropped. A partial trailing line is withheld until the writer
    completes it (or discarded on the final pass).

    When ``redactor`` is supplied, every record passes through
    ``redactor.redact`` at the single yield seam — after the internal
    cursor math has consumed the line — so follow semantics, ordering,
    and cursor advancement are unchanged. A redactor exception
    propagates to the caller. ``redactor=None`` (the default) is
    verbatim.

    After the file is exhausted (the drain for ``follow=False``, or the
    terminal-status exit for ``follow=True``) the run's landing-stage
    ledger records — the held-out gate verdict, the landing-parked
    witness, and the landed record — are projected from ``store`` and
    yielded at the tail, numbered strictly after the last file record.
    Landing is decided after the attempt finalized, so these records
    always sort last; a run with no landing decision adds none and
    streams exactly as before.
    """
    path = run_file_path(_resolve_logs_root(logs_root), run_id)
    cursor = FileCursor()
    drained, cursor = read_records_since(path, cursor)
    last_sequence = 0
    for record in drained:
        seq = record.sequence
        if seq is not None and seq > last_sequence:
            last_sequence = seq
        yield record if redactor is None else redactor.redact(record)
    if not follow:
        for record in _landing_stage_records(
            store, run_id, start_sequence=last_sequence
        ):
            yield record if redactor is None else redactor.redact(record)
        return
    if not _lifecycle_exists(store, run_id):
        # Spec error-handling table: an unknown run_id yields nothing
        # and returns even under follow=True. Without this guard the
        # follow loop would spin forever waiting on a lifecycle that
        # will never exist.
        return
    for record in _follow(
        store, run_id, path, cursor=cursor, poll_interval=poll_interval
    ):
        seq = record.sequence
        if seq is not None and seq > last_sequence:
            last_sequence = seq
        yield record if redactor is None else redactor.redact(record)
    for record in _landing_stage_records(
        store, run_id, start_sequence=last_sequence
    ):
        yield record if redactor is None else redactor.redact(record)


class Subscription:
    """A background follower that dispatches each record to a handler.

    Spawns one daemon thread that drains the run file's backlog, then
    follows the live stream (reusing :func:`_follow`). Each record is
    handed to ``handler`` in file order.

    **Error isolation.** Every ``handler`` call is wrapped: a raising
    handler is reported (via ``on_error`` if supplied, else logged at
    WARNING) and the dispatcher continues to the next record. A faulty
    subscriber therefore never breaks the follow loop. Because each
    subscription owns its own thread, it also cannot affect sibling
    subscribers. This mirrors the harness's best-effort telemetry
    discipline.

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
        store: object,
        handler: Callable[[AuditRecord], None],
        logs_root: str | Path | None = None,
        poll_interval: float = 0.25,
        on_error: Callable[[AuditRecord, BaseException], None] | None = None,
        redactor: Redactor | None = None,
    ) -> None:
        self._run_id = run_id
        self._store = store
        self._path = run_file_path(_resolve_logs_root(logs_root), run_id)
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
        cursor = FileCursor()
        drained, cursor = read_records_since(self._path, cursor)
        for record in drained:
            self._dispatch(record)
            if self._stop_event.is_set():
                return
        if self._stop_event.is_set():
            return
        for record in _follow(
            self._store,
            self._run_id,
            self._path,
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
        not block the caller indefinitely; the stop event also interrupts
        the poll sleep so exit is prompt regardless of ``poll_interval``.
        """
        self._stop_event.set()
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
    store: object,
    logs_root: str | Path | None = None,
    poll_interval: float = 0.25,
    on_error: Callable[[AuditRecord, BaseException], None] | None = None,
    redactor: Redactor | None = None,
) -> Subscription:
    """Subscribe ``handler`` to ``run_id``'s observability stream.

    ``handler`` may be an :class:`EventHandler` (an object with
    ``on_record``) or a plain ``Callable[[AuditRecord], None]``. The
    subscriber runs on its own daemon thread, observes records in file
    order, and is isolated: a raising handler is reported and the
    stream continues. Returns a :class:`Subscription` whose
    ``unsubscribe()`` stops and joins the thread.

    When ``redactor`` is supplied, every record passes through
    ``redactor.redact`` before the handler sees it, applied inside the
    same per-record isolation that wraps the handler — a raising redactor
    is reported (via ``on_error`` or the module logger) and the stream
    continues. ``redactor=None`` is verbatim.

    Plugins register here without touching the harness and cannot corrupt
    lifecycle state — they only read committed lines.
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
        logs_root=logs_root,
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
        store: object,
        logger: logging.Logger,
        poll_interval: float,
        logs_root: str | Path | None = None,
        redactor: Redactor | None = None,
    ) -> None:
        self._logger = logger
        super().__init__(
            run_id=run_id,
            store=store,
            handler=self._emit,
            logs_root=logs_root,
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
    store: object,
    logs_root: str | Path | None = None,
    poll_interval: float = 0.25,
    redactor: Redactor | None = None,
) -> AuditLoggerHandle:
    """Emit every observability record for ``run_id`` through ``logger``.

    A convenience subscriber: spawns a daemon thread that follows the
    run's stream and calls ``logger.log(logging.INFO, msg,
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
        logs_root=logs_root,
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
    "FileCursor",
    "Subscription",
    "attach_logger",
    "stream",
    "subscribe",
]
