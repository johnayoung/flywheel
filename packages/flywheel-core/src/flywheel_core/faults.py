"""Shared fault taxonomy and bounded-backoff helper for retry sites.

One classifier and one backoff policy that every downstream retry site
(agent invocation, SQLite/Postgres store, connection pool, worker loop)
reuses, so the TRANSIENT/PERMANENT buckets and the wait schedule never fork
into per-site copies.

* :func:`classify_fault` buckets an in-scope fault as
  :attr:`FaultClass.TRANSIENT` (worth retrying after a backoff) or
  :attr:`FaultClass.PERMANENT` (retrying cannot help), returning ``None`` for
  anything out of scope. An optimistic-concurrency version conflict is out of
  scope on purpose: the harness already loop-retries it, so it must never be
  treated as a transient fault here.
* :class:`BackoffPolicy` / :func:`wait_backoff` produce a
  monotonic-non-decreasing, cap-bounded wait schedule with an injectable
  sleep, so the waits are captured deterministically in tests instead of
  blocking on a real clock.

Lives beside the invoker seam, not in the pure ``task``/``lifecycle`` modules.
``psycopg`` is only touched behind a lazy import, so ``import flywheel_core``
never requires the optional ``postgres`` extra.
"""

from __future__ import annotations

import re
import sqlite3
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from enum import Enum

from flywheel_core.store_protocols import (
    OptimisticConcurrencyError,
    SchemaMismatchError,
)


class FaultClass(Enum):
    """The two retry buckets a classified fault can fall into."""

    TRANSIENT = "transient"
    PERMANENT = "permanent"


# HTTP-status codes that mark a retryable rate-limit / overload condition,
# matching what the invoker surfaces as
# ``InvocationSignals.api_error_status``: 429 rate-limited, 529 overloaded,
# plus the transient 5xx gateway set.
_TRANSIENT_STATUSES: frozenset[int] = frozenset({429, 500, 502, 503, 504, 529})


def classify_fault(fault: object) -> FaultClass | None:
    """Bucket ``fault`` as TRANSIENT/PERMANENT, or ``None`` when out of scope.

    ``fault`` is either a raised exception or an integer HTTP-ish status code
    (the invoker's ``api_error_status``). Anything the classifier does not
    recognize returns ``None`` — notably an optimistic-concurrency version
    conflict, which the harness already loop-retries and must never be retried
    here as if it were transient.
    """
    if isinstance(fault, BaseException):
        return _classify_exception(fault)
    # ``bool`` is an ``int`` subclass; a stray ``True``/``False`` is not a
    # status code, so reject it before the integer path.
    if isinstance(fault, bool):
        return None
    if isinstance(fault, int):
        return _classify_status(fault)
    return None


def _classify_status(status: int) -> FaultClass | None:
    return FaultClass.TRANSIENT if status in _TRANSIENT_STATUSES else None


def _classify_exception(exc: BaseException) -> FaultClass | None:
    # PERMANENT: a schema-version mismatch (core store or orchestrator store,
    # both subclass the shared marker). Retrying cannot reconcile an
    # incompatible on-disk schema.
    if isinstance(exc, SchemaMismatchError):
        return FaultClass.PERMANENT
    # Out of scope: an optimistic-concurrency conflict is already loop-retried
    # by the harness, so it is neither a transient retry candidate here nor a
    # permanent failure.
    if isinstance(exc, OptimisticConcurrencyError):
        return None
    # TRANSIENT: SQLite still busy after its ``busy_timeout`` elapsed.
    if _is_sqlite_locked(exc):
        return FaultClass.TRANSIENT
    # TRANSIENT: a dropped Postgres connection or a pool-checkout timeout
    # (``psycopg_pool.PoolTimeout`` subclasses ``psycopg.OperationalError``).
    if _is_psycopg_operational(exc):
        return FaultClass.TRANSIENT
    return None


def _is_sqlite_locked(exc: BaseException) -> bool:
    return (
        isinstance(exc, sqlite3.OperationalError)
        and "locked" in str(exc).lower()
    )


def _is_psycopg_operational(exc: BaseException) -> bool:
    # Lazy import: ``psycopg`` is the optional ``postgres`` extra, so a store
    # running without it (no PG faults possible) simply never matches here.
    try:
        import psycopg
    except ImportError:
        return False
    return isinstance(exc, psycopg.OperationalError)


DEFAULT_BASE_SECONDS: float = 0.5
DEFAULT_FACTOR: float = 2.0
DEFAULT_CAP_SECONDS: float = 30.0


@dataclass(frozen=True)
class BackoffPolicy:
    """A capped exponential backoff schedule.

    ``delay_for(attempt)`` (0-based) grows as
    ``base_seconds * factor ** attempt`` until it reaches ``cap_seconds`` and
    then holds there, so the schedule is monotonic-non-decreasing and every
    wait is ``<= cap_seconds``. ``factor > 1`` makes the pre-cap region
    strictly grow; a ``factor == 1`` policy is a constant delay, not backoff.
    """

    base_seconds: float = DEFAULT_BASE_SECONDS
    factor: float = DEFAULT_FACTOR
    cap_seconds: float = DEFAULT_CAP_SECONDS

    def __post_init__(self) -> None:
        if self.base_seconds <= 0:
            raise ValueError("base_seconds must be > 0")
        if self.factor < 1:
            raise ValueError("factor must be >= 1")
        if self.cap_seconds < self.base_seconds:
            raise ValueError("cap_seconds must be >= base_seconds")

    def delay_for(self, attempt: int) -> float:
        """Return the (capped) wait for 0-based retry ``attempt``."""
        if attempt < 0:
            raise ValueError("attempt must be >= 0")
        try:
            raw = self.base_seconds * (self.factor**attempt)
        except OverflowError:
            # A large enough exponent overflows a float; the result would be
            # far past the cap anyway, so the cap is the answer.
            return self.cap_seconds
        return min(raw, self.cap_seconds)


DEFAULT_BACKOFF: BackoffPolicy = BackoffPolicy()


def wait_backoff(
    attempt: int,
    *,
    policy: BackoffPolicy = DEFAULT_BACKOFF,
    sleep: Callable[[float], None] = time.sleep,
) -> float:
    """Sleep for the backoff delay of 0-based retry ``attempt``; return it.

    ``sleep`` is injectable so tests capture the waits deterministically
    (pass a list's ``append``) instead of blocking on a real clock.
    """
    delay = policy.delay_for(attempt)
    sleep(delay)
    return delay


@dataclass(frozen=True)
class SessionLimitReset:
    """A derived session-limit reset instant and the ladder rung it came from.

    ``reset_at`` is an aware UTC :class:`~datetime.datetime`; ``source`` names
    the derivation rung so telemetry can record which surface answered:
    ``"rate_limit_event"`` (a rejected rate-limit event's ``resets_at``),
    ``"refusal_pipe_epoch"`` (the ``...usage limit reached|<epoch>`` refusal),
    or ``"refusal_clock_time"`` (a ``resets 6pm`` human refusal).
    """

    reset_at: datetime
    source: str


# The Claude CLI session-limit refusal carries the reset epoch after a pipe:
# ``Claude AI usage limit reached|1751990400`` (epoch seconds, UTC).
_PIPE_EPOCH_RE = re.compile(r"usage limit reached\s*\|\s*(\d+)", re.IGNORECASE)

# The human-readable refusal names a wall-clock reset time, e.g. ``resets 6pm``
# or ``resets 6:30 am``. Interpreted in ``now``'s timezone with
# next-occurrence semantics (a clock time already past today rolls to
# tomorrow, never a negative interval).
_CLOCK_TIME_RE = re.compile(
    r"resets\s+(\d{1,2})(?::(\d{2}))?\s*([ap]m)", re.IGNORECASE
)


def derive_session_limit_reset(
    *,
    rate_limit_events: Sequence[object] = (),
    transcript: str = "",
    now: datetime,
) -> SessionLimitReset | None:
    """Derive a session-limit reset instant, strongest source first.

    The ladder, in order:

    1. A ``rejected`` rate-limit event's ``rate_limit_info.resets_at`` (epoch
       seconds, UTC). Attributes are read via ``getattr`` so the optional
       agent SDK is never imported here; an ``allowed_warning`` event is
       ignored on purpose.
    2. A parseable refusal in ``transcript``: the pipe-epoch form
       (``...usage limit reached|<epoch>``) or the human clock-time form
       (``resets 6pm``). The clock time is read in ``now``'s timezone with
       next-occurrence semantics, so a time already past today resolves to
       tomorrow rather than a negative interval.
    3. Neither derivable -> ``None``.

    The returned ``reset_at`` is always aware UTC. The function is total: a
    malformed, garbage, or unrecognized refusal yields ``None``, never a
    raised exception. Whether a derived reset is actually in the future is
    the caller's policy decision -- a past epoch is returned verbatim, not
    filtered here (so the caller can preserve its existing behavior when the
    reset is at-or-before ``now``).
    """
    from_event = _reset_from_rate_limit_events(rate_limit_events)
    if from_event is not None:
        return from_event
    return _reset_from_refusal_text(transcript, now=now)


def _reset_from_rate_limit_events(
    events: Sequence[object],
) -> SessionLimitReset | None:
    for event in events:
        info = getattr(event, "rate_limit_info", None)
        # Only a hard ``rejected`` block names a reset worth acting on; an
        # ``allowed_warning`` must never drive the fast-abort.
        if getattr(info, "status", None) != "rejected":
            continue
        resets_at = getattr(info, "resets_at", None)
        # ``bool`` is an ``int`` subclass; reject a stray flag before the
        # numeric path.
        if isinstance(resets_at, bool) or not isinstance(
            resets_at, (int, float)
        ):
            continue
        reset_at = _epoch_to_utc(resets_at)
        if reset_at is not None:
            return SessionLimitReset(
                reset_at=reset_at, source="rate_limit_event"
            )
    return None


def _reset_from_refusal_text(
    transcript: str, *, now: datetime
) -> SessionLimitReset | None:
    if not transcript:
        return None
    pipe = _PIPE_EPOCH_RE.search(transcript)
    if pipe is not None:
        reset_at = _epoch_to_utc(int(pipe.group(1)))
        if reset_at is not None:
            return SessionLimitReset(
                reset_at=reset_at, source="refusal_pipe_epoch"
            )
    clock = _CLOCK_TIME_RE.search(transcript)
    if clock is not None:
        reset_at = _next_clock_occurrence(clock, now=now)
        if reset_at is not None:
            return SessionLimitReset(
                reset_at=reset_at, source="refusal_clock_time"
            )
    return None


def _epoch_to_utc(epoch: float) -> datetime | None:
    try:
        return datetime.fromtimestamp(epoch, tz=timezone.utc)
    except (OverflowError, OSError, ValueError):
        # An out-of-range epoch is unusable; the parser stays total.
        return None


def _next_clock_occurrence(
    match: re.Match[str], *, now: datetime
) -> datetime | None:
    hour = int(match.group(1))
    minute = int(match.group(2)) if match.group(2) is not None else 0
    if not (1 <= hour <= 12) or not (0 <= minute <= 59):
        return None
    hour24 = hour % 12
    if match.group(3).lower() == "pm":
        hour24 += 12
    # A naive ``now`` is treated as UTC so the parser never raises on a
    # caller that forgot to attach a timezone; production passes aware UTC.
    local_now = (
        now if now.tzinfo is not None else now.replace(tzinfo=timezone.utc)
    )
    candidate = local_now.replace(
        hour=hour24, minute=minute, second=0, microsecond=0
    )
    if candidate <= local_now:
        candidate += timedelta(days=1)
    return candidate.astimezone(timezone.utc)


__all__ = [
    "BackoffPolicy",
    "DEFAULT_BACKOFF",
    "DEFAULT_BASE_SECONDS",
    "DEFAULT_CAP_SECONDS",
    "DEFAULT_FACTOR",
    "FaultClass",
    "SessionLimitReset",
    "classify_fault",
    "derive_session_limit_reset",
    "wait_backoff",
]
