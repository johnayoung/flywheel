"""Pure detector for repeated-failure (STUCK) and thrash (THRASH) loops.

The harness feeds each iteration's ``signals.tool_interactions`` into
``LoopGuard.observe`` in arrival order; a returned :class:`LoopGuardVerdict`
tells the harness to abort the attempt and which transition to route to. The
hang watchdog and harness wiring live in :mod:`flywheel_core.harness`; the
``hang_timeout_seconds`` knob is carried here as pure data only.

See ``.flywheel/specs/00015-FEATURE-loop-safety-net.md`` and ``docs/loop.md``
for the surrounding design. The module is pure: no ``json``/``pathlib``/``io``/
``asyncio``/``time`` imports, no ``open()``. Enforced by
``tests/test_loop_guard_module_purity.py``.
"""

import hashlib
from collections import deque
from collections.abc import Sequence
from dataclasses import dataclass
from enum import Enum
from typing import Any

from flywheel_core.invoker import ToolInteraction


class LoopGuardVerdictKind(str, Enum):
    """Closed set of detector kinds.

    ``STUCK`` is the repeated-failure block (FR-1) and ``THRASH`` is the
    identical-tuple repetition within a window (FR-2). The hang watchdog is
    orthogonal and produced by the harness, not this module.
    """

    STUCK = "stuck"
    THRASH = "thrash"


@dataclass(frozen=True, kw_only=True)
class LoopGuardVerdict:
    """One detector trip.

    ``tool_name`` and ``input_digest`` identify the offending tuple so the
    harness can name it in audit events and operator surfaces.
    """

    kind: LoopGuardVerdictKind
    reason: str
    tool_name: str
    input_digest: str


@dataclass(frozen=True, kw_only=True)
class LoopGuardConfig:
    """Threshold knobs. Each is independently disablable via ``None`` or ``0``.

    Defaults for the deterministic detectors are conservative on — they key
    on byte-identical input digests, so a false positive requires the agent
    to literally repeat itself. ``hang_timeout_seconds`` ships disabled: no
    grounded value exists yet (see ``docs/loop.md``), so this module carries
    the knob as pure data and the harness watchdog reads it.
    """

    repeated_tool_failure_threshold: int | None = 3
    thrash_repeat_threshold: int | None = 4
    thrash_window: int | None = 12
    hang_timeout_seconds: float | None = None


def _stable_encode(value: Any) -> str:
    """Deterministic structural encoding of a tool-input value.

    Designed so byte-identical structured inputs collide and key-order noise
    in mappings does not split them — dict keys are sorted by their own
    stable encoding and containers recurse. ``bool`` is checked before
    ``int`` because ``bool`` is a subclass of ``int`` in Python and would
    otherwise be encoded as a number.
    """

    if value is None:
        return "N"
    if isinstance(value, bool):
        return "T" if value else "F"
    if isinstance(value, int):
        return f"I:{value}"
    if isinstance(value, float):
        return f"X:{value!r}"
    if isinstance(value, str):
        return f"S:{len(value)}:{value}"
    if isinstance(value, bytes):
        return f"Y:{len(value)}:{value.hex()}"
    if isinstance(value, dict):
        items = sorted(
            ((_stable_encode(k), _stable_encode(v)) for k, v in value.items()),
            key=lambda kv: kv[0],
        )
        body = ",".join(f"{k}={v}" for k, v in items)
        return f"D{{{body}}}"
    if isinstance(value, (list, tuple)):
        body = ",".join(_stable_encode(v) for v in value)
        return f"L[{body}]"
    # Defensive fallback. ``tool_input`` from the SDK is JSON-shaped
    # (dicts / lists / primitives), so this branch is unreachable in
    # practice but keeps the function total.
    return f"R:{value!r}"


def _digest(tool_input: Any) -> str:
    encoded = _stable_encode(tool_input).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _enabled(threshold: int | None) -> bool:
    return threshold is not None and threshold > 0


class LoopGuard:
    """Per-attempt accumulator over tool interactions.

    Construct one per attempt and feed each iteration's
    ``signals.tool_interactions`` to :meth:`observe` in arrival order. State
    accumulates across calls so detectors spanning multiple iterations work
    correctly; the guard is not designed to be reset or reused across
    attempts -- the harness builds a fresh one per attempt.
    """

    def __init__(self, config: LoopGuardConfig | None = None) -> None:
        self._config: LoopGuardConfig = config or LoopGuardConfig()
        self._last_failing_key: tuple[str, str] | None = None
        self._consecutive_failures: int = 0
        window = self._config.thrash_window
        maxlen: int = window if window is not None and window > 0 else 1
        self._recent: deque[tuple[str, str]] = deque(maxlen=maxlen)

    def observe(
        self, interactions: Sequence[ToolInteraction]
    ) -> LoopGuardVerdict | None:
        """Feed one iteration's tool interactions in arrival order.

        Returns the first verdict produced by walking the sequence; the
        repeated-failure (STUCK) detector is evaluated before the thrash
        (THRASH) detector for the same interaction, so a call that is both
        failing-and-repeating and thrashing returns STUCK (FR-6 precedence).
        Empty sequences are a no-op and never produce a verdict.
        """

        for interaction in interactions:
            tool_name = interaction.tool_name
            digest = _digest(interaction.tool_input)
            key = (tool_name, digest)
            result = interaction.result
            is_failure = result is not None and result.is_error is True

            stuck = self._update_repeated_failure(key, is_failure)
            if stuck is not None:
                return stuck

            thrash = self._update_thrash(key)
            if thrash is not None:
                return thrash

        return None

    def _update_repeated_failure(
        self, key: tuple[str, str], is_failure: bool
    ) -> LoopGuardVerdict | None:
        threshold = self._config.repeated_tool_failure_threshold
        if threshold is None or threshold <= 0:
            return None

        if is_failure:
            if self._last_failing_key == key:
                self._consecutive_failures += 1
            else:
                self._last_failing_key = key
                self._consecutive_failures = 1
        else:
            # A non-failure (success, or pending result with no result block)
            # breaks the consecutive failing streak per FR-1.
            self._last_failing_key = None
            self._consecutive_failures = 0

        if self._consecutive_failures >= threshold:
            tool_name, digest = key
            return LoopGuardVerdict(
                kind=LoopGuardVerdictKind.STUCK,
                reason=(
                    f"{self._consecutive_failures} consecutive failing calls "
                    f"to {tool_name!r} with identical input "
                    f"(digest {digest[:12]}...) reached "
                    f"repeated_tool_failure_threshold={threshold}"
                ),
                tool_name=tool_name,
                input_digest=digest,
            )
        return None

    def _update_thrash(self, key: tuple[str, str]) -> LoopGuardVerdict | None:
        threshold = self._config.thrash_repeat_threshold
        window = self._config.thrash_window
        if threshold is None or threshold <= 0:
            return None
        if window is None or window <= 0:
            return None

        self._recent.append(key)
        occurrences = sum(1 for entry in self._recent if entry == key)
        if occurrences >= threshold:
            tool_name, digest = key
            return LoopGuardVerdict(
                kind=LoopGuardVerdictKind.THRASH,
                reason=(
                    f"{occurrences} repeats of {tool_name!r} with identical "
                    f"input (digest {digest[:12]}...) within trailing "
                    f"{window}-call window reached "
                    f"thrash_repeat_threshold={threshold}"
                ),
                tool_name=tool_name,
                input_digest=digest,
            )
        return None


__all__ = [
    "LoopGuard",
    "LoopGuardConfig",
    "LoopGuardVerdict",
    "LoopGuardVerdictKind",
]
