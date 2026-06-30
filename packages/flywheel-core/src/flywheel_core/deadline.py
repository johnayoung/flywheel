"""Reusable wall-clock deadline primitive.

A single seam that bounds an ``awaitable`` or an ``async for`` stream by total
wall-clock time elapsed since the call started, raising a distinguishable
:class:`DeadlineExceeded` when the ceiling is exceeded -- even while the wrapped
stream is still producing items. This is *not* an idle/silence timer: a stream
that keeps yielding forever is still cut off once the ceiling passes (see
``.flywheel/specs/00066-FEATURE-universal-deadlines.md`` D-2). The existing
inter-message silence watchdog in :mod:`flywheel_core.harness` is orthogonal;
this bound is additive, not a replacement.

The module lives outside the pure ``task``/``lifecycle`` core (it imports
``asyncio``) and carries no agent-SDK dependency: ``import flywheel_core`` works
without the ``claude`` extra.
"""

import asyncio
from collections.abc import AsyncIterable, AsyncIterator, Awaitable, Coroutine
from typing import Any, TypeVar

_T = TypeVar("_T")


class DeadlineExceeded(Exception):
    """Raised when a wall-clock deadline is exceeded.

    Deliberately a plain :class:`Exception` (not a subclass of
    :class:`asyncio.CancelledError`) so callers can tell a deadline timeout
    apart from an ordinary cancellation and classify it as a timeout outcome.
    ``ceiling_seconds`` is the configured bound; ``elapsed_seconds`` is the
    wall-clock time observed when the deadline fired, when known.
    """

    def __init__(
        self,
        ceiling_seconds: float,
        *,
        elapsed_seconds: float | None = None,
    ) -> None:
        self.ceiling_seconds = ceiling_seconds
        self.elapsed_seconds = elapsed_seconds
        detail = (
            f"deadline of {ceiling_seconds}s exceeded"
            if elapsed_seconds is None
            else (
                f"deadline of {ceiling_seconds}s exceeded "
                f"after {elapsed_seconds:.6f}s"
            )
        )
        super().__init__(detail)


async def run_with_deadline(
    awaitable: Awaitable[_T] | Coroutine[Any, Any, _T],
    ceiling_seconds: float,
) -> _T:
    """Await ``awaitable``, raising :class:`DeadlineExceeded` past the ceiling.

    A thin wall-clock bound over a single awaitable. If the awaitable does not
    complete within ``ceiling_seconds`` it is cancelled and
    :class:`DeadlineExceeded` is raised; any other exception (including an
    ordinary :class:`asyncio.CancelledError` from outside) propagates unchanged.
    """

    loop = asyncio.get_running_loop()
    started = loop.time()
    try:
        return await asyncio.wait_for(awaitable, timeout=ceiling_seconds)
    except (asyncio.TimeoutError, TimeoutError) as exc:
        elapsed = loop.time() - started
        raise DeadlineExceeded(
            ceiling_seconds, elapsed_seconds=elapsed
        ) from exc


async def stream_with_deadline(
    source: AsyncIterable[_T],
    ceiling_seconds: float,
) -> AsyncIterator[_T]:
    """Yield from ``source`` until the wall-clock ceiling is exceeded.

    The bound is total elapsed time since iteration started, not idle time
    between items: a ``source`` that keeps yielding forever is still cut off
    with :class:`DeadlineExceeded` once ``ceiling_seconds`` passes, and a
    ``source`` whose next item never arrives is cut off the same way. The
    remaining budget is recomputed before each item, so neither a fast,
    never-ending stream nor a wedged ``__anext__`` can outlast the ceiling.
    """

    loop = asyncio.get_running_loop()
    deadline = loop.time() + ceiling_seconds
    iterator = source.__aiter__()
    while True:
        remaining = deadline - loop.time()
        if remaining <= 0:
            raise DeadlineExceeded(
                ceiling_seconds,
                elapsed_seconds=loop.time() - (deadline - ceiling_seconds),
            )
        try:
            item = await asyncio.wait_for(
                iterator.__anext__(), timeout=remaining
            )
        except StopAsyncIteration:
            return
        except (asyncio.TimeoutError, TimeoutError) as exc:
            raise DeadlineExceeded(
                ceiling_seconds,
                elapsed_seconds=loop.time() - (deadline - ceiling_seconds),
            ) from exc
        yield item


__all__ = [
    "DeadlineExceeded",
    "run_with_deadline",
    "stream_with_deadline",
]
