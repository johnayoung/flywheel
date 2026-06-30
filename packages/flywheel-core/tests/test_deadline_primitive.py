"""Behavioral coverage for :mod:`flywheel_core.deadline`.

Cross-references criterion #1 of
``.flywheel/specs/00066-FEATURE-universal-deadlines.md``: the wall-clock
deadline primitive must raise its distinguishable timeout signal within roughly
``ceiling + epsilon``, even while the wrapped stream is still yielding items. An
idle/silence timer that resets on each yield would fail the "yields forever"
test below.

Async coroutines are driven via ``asyncio.run`` inside sync test functions,
mirroring ``test_invoker_client.py`` -- the suite ships no pytest-asyncio.
"""

import asyncio
from collections.abc import AsyncIterator

import pytest

from flywheel_core.deadline import (
    DeadlineExceeded,
    run_with_deadline,
    stream_with_deadline,
)


# A ceiling small enough to keep the suite fast, and an epsilon ceiling on how
# much wall time the bounded call may take past it. Generous enough to absorb
# event-loop scheduling jitter without making the bound meaningless.
_CEILING = 0.05
_EPSILON = 1.0


# --- awaitable bound ---


def test_coroutine_sleeping_past_ceiling_raises_deadline_exceeded() -> None:
    async def _run() -> None:
        loop = asyncio.get_running_loop()
        started = loop.time()
        with pytest.raises(DeadlineExceeded) as excinfo:
            await run_with_deadline(
                asyncio.sleep(3600), ceiling_seconds=_CEILING
            )
        elapsed = loop.time() - started
        assert elapsed < _CEILING + _EPSILON
        assert excinfo.value.ceiling_seconds == _CEILING

    asyncio.run(_run())


def test_coroutine_completing_before_ceiling_returns_its_value() -> None:
    async def _run() -> None:
        async def quick() -> str:
            await asyncio.sleep(0)
            return "done"

        result = await run_with_deadline(quick(), ceiling_seconds=_CEILING)
        assert result == "done"

    asyncio.run(_run())


# --- stream bound ---


def test_async_for_over_forever_yielding_generator_is_cut_off() -> None:
    """A stream that keeps yielding forever must still hit the deadline.

    This is the load-bearing case: an idle/silence timer that resets on each
    yield would let this run forever. The wall-clock bound must fire because
    total elapsed time crosses the ceiling, not because the stream went quiet.
    """

    async def _run() -> None:
        async def yields_forever() -> AsyncIterator[int]:
            i = 0
            while True:
                yield i
                i += 1
                # A brief await so the loop can advance wall-clock time; the
                # generator never stops on its own.
                await asyncio.sleep(0.001)

        loop = asyncio.get_running_loop()
        started = loop.time()
        seen = 0
        with pytest.raises(DeadlineExceeded) as excinfo:
            async for _item in stream_with_deadline(
                yields_forever(), ceiling_seconds=_CEILING
            ):
                seen += 1
        elapsed = loop.time() - started
        assert elapsed < _CEILING + _EPSILON
        assert seen > 0  # it really did stream items before the deadline fired
        assert excinfo.value.ceiling_seconds == _CEILING

    asyncio.run(_run())


def test_async_for_over_wedged_anext_is_cut_off() -> None:
    """A stream whose next item never arrives is bounded the same way."""

    async def _run() -> None:
        async def wedges() -> AsyncIterator[int]:
            yield 0
            await asyncio.sleep(3600)
            yield 1  # never reached

        loop = asyncio.get_running_loop()
        started = loop.time()
        with pytest.raises(DeadlineExceeded):
            async for _item in stream_with_deadline(
                wedges(), ceiling_seconds=_CEILING
            ):
                pass
        elapsed = loop.time() - started
        assert elapsed < _CEILING + _EPSILON

    asyncio.run(_run())


def test_finite_stream_within_ceiling_yields_all_items() -> None:
    async def _run() -> None:
        async def three() -> AsyncIterator[int]:
            for i in range(3):
                await asyncio.sleep(0)
                yield i

        collected = [
            item
            async for item in stream_with_deadline(
                three(), ceiling_seconds=_CEILING
            )
        ]
        assert collected == [0, 1, 2]

    asyncio.run(_run())


# --- distinguishability ---


def test_deadline_exceeded_is_not_a_cancelled_error() -> None:
    """Callers must be able to tell a timeout apart from a cancellation."""

    assert not issubclass(DeadlineExceeded, asyncio.CancelledError)

    async def _run() -> None:
        with pytest.raises(DeadlineExceeded) as excinfo:
            await run_with_deadline(
                asyncio.sleep(3600), ceiling_seconds=_CEILING
            )
        assert not isinstance(excinfo.value, asyncio.CancelledError)

    asyncio.run(_run())


def test_external_cancellation_propagates_not_classified_as_deadline() -> None:
    """An ordinary cancel is not swallowed into a DeadlineExceeded."""

    async def _run() -> None:
        async def cancels_itself() -> None:
            raise asyncio.CancelledError()

        with pytest.raises(asyncio.CancelledError):
            await run_with_deadline(
                cancels_itself(), ceiling_seconds=_CEILING
            )

    asyncio.run(_run())
