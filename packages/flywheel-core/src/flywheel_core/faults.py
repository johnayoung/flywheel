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

import sqlite3
import time
from collections.abc import Callable
from dataclasses import dataclass
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


__all__ = [
    "BackoffPolicy",
    "DEFAULT_BACKOFF",
    "DEFAULT_BASE_SECONDS",
    "DEFAULT_CAP_SECONDS",
    "DEFAULT_FACTOR",
    "FaultClass",
    "classify_fault",
    "wait_backoff",
]
