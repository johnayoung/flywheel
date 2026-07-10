"""Shared Claude normalization: usage coercion and stop-reason mapping.

Used by both transports (CLI stream-json and the agent SDK) so a run folds
identically regardless of how it was driven.
"""

from __future__ import annotations

from collections.abc import Mapping

from flywheel_agents.models import StopReason

USAGE_KEYS = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)

# Result subtypes are the most reliable stop signal in --print mode; assistant
# stop_reason strings are the fallback.
_SUBTYPE_STOPS: dict[str, StopReason] = {
    "success": StopReason.COMPLETED,
    "error_max_turns": StopReason.MAX_TURNS,
    "error_during_execution": StopReason.ERROR,
}
_NATIVE_STOPS: dict[str, StopReason] = {
    "end_turn": StopReason.COMPLETED,
    "stop_sequence": StopReason.COMPLETED,
    "max_tokens": StopReason.MAX_TOKENS,
}


def coerce_usage(raw: object) -> dict[str, int]:
    """Project a native usage mapping onto flywheel's four canonical counters.

    Mirrors the container backend's historical behavior: when any canonical
    key is present the result is zero-filled across all four; bools are
    excluded (``bool`` is an ``int`` subclass).
    """
    if not isinstance(raw, Mapping):
        return {}
    picked: dict[str, int] = {}
    for key in USAGE_KEYS:
        value = raw.get(key)
        if isinstance(value, bool):
            continue
        if isinstance(value, (int, float)):
            picked[key] = int(value)
    if not picked:
        return {}
    return {key: picked.get(key, 0) for key in USAGE_KEYS}


def normalize_stop(
    *, subtype: str | None, native_stop: str | None, is_error: bool
) -> StopReason:
    if subtype is not None and subtype in _SUBTYPE_STOPS:
        return _SUBTYPE_STOPS[subtype]
    if native_stop is not None and native_stop in _NATIVE_STOPS:
        return _NATIVE_STOPS[native_stop]
    return StopReason.ERROR if is_error else StopReason.UNKNOWN
