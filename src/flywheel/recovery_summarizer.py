"""Recovery summarizer runner -- fresh-session structured handoff.

When the harness's context-recovery policy (spec 00018) fires, the loop
needs a structured handoff that lets a fresh attempt resume without
re-running the prior attempt's discovery. This module produces that
handoff via a separate, fresh-context ``claude_agent_sdk.query`` call --
exactly mirroring how :mod:`flywheel.grader_rubric` runs the rubric
judge: the summarizer is never the working-agent invoker (which is near
capacity and degraded) and is fully driven by an injected seam in tests.

Layered surface (matches :mod:`flywheel.grader_rubric`):

* :func:`parse_handoff` is pure -- no IO, no SDK imports. The result
  taxonomy (:data:`HandoffResult`) is closed so every input maps to
  exactly one variant (missing / truncated / duplicate / malformed /
  valid).
* :func:`run_recovery_summarizer` is the public async runner. It accepts
  a ``summarizer_invoke`` seam so tests can inject a scripted invoker
  without going near the SDK.
* The production default of ``summarizer_invoke`` lives in
  :func:`_make_default_summarizer_invoke` and delegates to
  :func:`claude_agent_sdk.query` with cwd set to the supplied worktree,
  full tool surface, and no ``session_id`` (fresh session -- the
  summarizer never inherits the working agent's context).

Error contract: any failure to produce a parseable handoff -- an
invoke that raises or times out, an empty/missing envelope, malformed
JSON, missing fields -- raises :class:`RecoverySummarizerError`. The
runner never silently returns an empty handoff; the harness routes the
exception through ``INTERNAL_ERROR`` (spec Error Handling table).
"""

from __future__ import annotations

import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from claude_agent_sdk import AssistantMessage, ClaudeAgentOptions, TextBlock
from claude_agent_sdk import query as _sdk_query

from flywheel.prompt import RecoveryHandoff
from flywheel.task import Task


# --- Handoff envelope -------------------------------------------------------


OPENING_FENCE = "<!-- RECOVERY_HANDOFF -->"
CLOSING_FENCE = "<!-- /RECOVERY_HANDOFF -->"


_REQUIRED_FIELDS: tuple[str, ...] = (
    "work_done",
    "work_remaining",
    "key_decisions",
    "suggested_next_step",
)


@dataclass(frozen=True, kw_only=True)
class ValidHandoff:
    handoff: RecoveryHandoff
    kind: Literal["valid"] = "valid"


@dataclass(frozen=True, kw_only=True)
class MissingHandoff:
    kind: Literal["missing"] = "missing"


@dataclass(frozen=True, kw_only=True)
class TruncatedHandoff:
    detail: str
    kind: Literal["truncated"] = "truncated"


@dataclass(frozen=True, kw_only=True)
class DuplicateHandoff:
    count: int
    kind: Literal["duplicate"] = "duplicate"


@dataclass(frozen=True, kw_only=True)
class MalformedHandoff:
    reason: str
    offending: str | None = None
    kind: Literal["malformed"] = "malformed"


HandoffResult = (
    ValidHandoff
    | MissingHandoff
    | TruncatedHandoff
    | DuplicateHandoff
    | MalformedHandoff
)


_OFFENDING_LIMIT = 200


def _all_occurrences(haystack: str, needle: str) -> list[int]:
    positions: list[int] = []
    start = 0
    while True:
        i = haystack.find(needle, start)
        if i == -1:
            return positions
        positions.append(i)
        start = i + len(needle)


def _truncate(payload: str) -> str:
    snippet = payload.strip()
    if len(snippet) > _OFFENDING_LIMIT:
        return snippet[:_OFFENDING_LIMIT]
    return snippet


def parse_handoff(text: str) -> HandoffResult:
    """Parse the recovery-handoff envelope out of a summarizer response.

    The contract is closed: every input maps to exactly one variant of
    :data:`HandoffResult`. Missing fences, duplicate fences, truncation,
    JSON-decode failure, missing fields, and any field-level shape
    violation are all distinguishable failure modes rather than coerced
    into a default.
    """
    if not isinstance(text, str):
        raise TypeError("parse_handoff requires a str input")

    open_positions = _all_occurrences(text, OPENING_FENCE)
    close_positions = _all_occurrences(text, CLOSING_FENCE)

    if not open_positions:
        return MissingHandoff()

    if not close_positions:
        return TruncatedHandoff(
            detail="opening fence found without a matching closing fence",
        )

    if len(open_positions) > 1 or len(close_positions) > 1:
        return DuplicateHandoff(
            count=max(len(open_positions), len(close_positions)),
        )

    open_pos = open_positions[0]
    close_pos = close_positions[0]

    if close_pos < open_pos:
        return TruncatedHandoff(
            detail="closing fence appears before opening fence",
        )

    inner = text[open_pos + len(OPENING_FENCE) : close_pos]

    try:
        data = json.loads(inner)
    except json.JSONDecodeError as exc:
        return MalformedHandoff(
            reason=f"handoff payload is not valid JSON: {exc.msg}",
            offending=_truncate(inner),
        )

    if not isinstance(data, dict):
        return MalformedHandoff(
            reason=(
                "handoff payload must be a JSON object, "
                f"got {type(data).__name__}"
            ),
            offending=_truncate(inner),
        )

    values: dict[str, str] = {}
    for field_name in _REQUIRED_FIELDS:
        if field_name not in data:
            return MalformedHandoff(
                reason=(
                    f"handoff payload missing required '{field_name}' field"
                ),
                offending=_truncate(inner),
            )
        raw = data[field_name]
        if not isinstance(raw, str):
            return MalformedHandoff(
                reason=(
                    f"handoff '{field_name}' must be a string, "
                    f"got {type(raw).__name__}"
                ),
                offending=_truncate(inner),
            )
        values[field_name] = raw

    return ValidHandoff(
        handoff=RecoveryHandoff(
            work_done=values["work_done"],
            work_remaining=values["work_remaining"],
            key_decisions=values["key_decisions"],
            suggested_next_step=values["suggested_next_step"],
        )
    )


# --- Runner -----------------------------------------------------------------


SummarizerInvoke = Callable[[str, "Path | str"], Awaitable[str]]


class RecoverySummarizerError(Exception):
    """Raised when the recovery summarizer cannot produce a parseable handoff.

    Carries a ``reason`` attribute so the harness can render
    ``'recovery summarizer failed: <reason>'`` without re-introspecting
    the exception. SDK exceptions raised by ``summarizer_invoke`` are
    wrapped into this type; parse failures
    (``Missing``/``Malformed``/``Duplicate``/``Truncated``) are also
    surfaced through it. The harness routes this exception through
    ``INTERNAL_ERROR`` (spec 00018 Error Handling table) -- no recovery
    is attempted with an empty handoff.
    """

    def __init__(self, *, reason: str) -> None:
        super().__init__(f"recovery summarizer failed: {reason}")
        self.reason = reason


_HANDOFF_CONTRACT = """\
# Handoff envelope

End your response with exactly one fenced JSON block of the form:

<!-- RECOVERY_HANDOFF -->
{"work_done": "<...>", "work_remaining": "<...>", "key_decisions": "<...>", "suggested_next_step": "<...>"}
<!-- /RECOVERY_HANDOFF -->

Fields (all required, strings; empty string accepted when a slot has nothing to report):
- `work_done`: concrete progress made by the prior attempt (files touched, behaviors implemented, tests added).
- `work_remaining`: what still has to happen for the task goal to be reached.
- `key_decisions`: design choices, trade-offs, or constraints the resuming agent must respect.
- `suggested_next_step`: the single next action the resuming agent should take.

Emit the envelope exactly once. Missing, malformed, duplicate, or
truncated envelopes are treated as summarizer-infrastructure failures
and abort recovery.
"""


def _build_summarizer_prompt(
    task: Task, cumulative_diff: str, transcript_tail: str
) -> str:
    return (
        "# Role\n\n"
        "You are the recovery summarizer for a coding agent whose prior "
        "attempt approached its context-window capacity and was "
        "force-finalized. Produce a structured handoff that lets a fresh "
        "agent resume without re-discovering what the prior attempt "
        "already learned.\n\n"
        f"# Goal\n\n{task.goal}\n\n"
        f"# Cumulative diff / artifacts\n\n{cumulative_diff}\n\n"
        f"# Recent transcript tail\n\n{transcript_tail}\n\n"
        f"{_HANDOFF_CONTRACT}"
    )


def _make_default_summarizer_invoke(
    *, summarizer_max_turns: int, summarizer_model: str | None
) -> SummarizerInvoke:
    """Build the production ``summarizer_invoke``: a fresh
    ``claude_agent_sdk.query``.

    The returned callable constructs :class:`ClaudeAgentOptions` per-call
    so the summarizer starts cold (no ``session_id`` inheritance), with
    cwd set to the supplied worktree, full tool surface
    (``skills='all'``), ``permission_mode='bypassPermissions'``, and a
    turn cap. ``summarizer_model`` of ``None`` falls through to the
    SDK's own default.
    """

    async def _default_summarizer_invoke(
        prompt: str, worktree: Path | str
    ) -> str:
        options = ClaudeAgentOptions(
            cwd=str(worktree),
            add_dirs=[str(worktree)],
            permission_mode="bypassPermissions",
            skills="all",
            max_turns=summarizer_max_turns,
            model=summarizer_model,
        )
        chunks: list[str] = []
        async for msg in _sdk_query(prompt=prompt, options=options):
            if isinstance(msg, AssistantMessage):
                for block in msg.content:
                    if isinstance(block, TextBlock):
                        chunks.append(block.text)
        return "".join(chunks)

    return _default_summarizer_invoke


def _handoff_failure_reason(result: HandoffResult) -> str:
    if isinstance(result, MissingHandoff):
        return "missing handoff envelope"
    if isinstance(result, TruncatedHandoff):
        return f"truncated handoff envelope: {result.detail}"
    if isinstance(result, DuplicateHandoff):
        return f"duplicate handoff envelopes (count={result.count})"
    if isinstance(result, MalformedHandoff):
        return f"malformed handoff envelope: {result.reason}"
    raise AssertionError(
        f"unexpected handoff variant: {type(result).__name__}"
    )


async def run_recovery_summarizer(
    task: Task,
    *,
    transcript_tail: str,
    cumulative_diff: str,
    worktree: Path | str,
    summarizer_invoke: SummarizerInvoke | None = None,
    summarizer_model: str | None = None,
    summarizer_max_turns: int = 8,
) -> RecoveryHandoff:
    """Produce a structured recovery handoff from the prior attempt's state.

    Assembles a summarizer prompt from the task goal, the cumulative
    diff/artifacts, and the recent transcript tail; invokes
    ``summarizer_invoke`` (defaults to a fresh ``claude_agent_sdk.query``
    against ``worktree``); parses the response with
    :func:`parse_handoff` and returns the structured handoff.

    Any invoke raise/timeout or non-``valid`` parse result raises
    :class:`RecoverySummarizerError` -- the runner never silently
    returns an empty handoff. The harness routes this exception through
    ``INTERNAL_ERROR`` per spec 00018 Error Handling.

    The summarizer is a separate fresh-context call by design: the
    working agent is near capacity and degraded; a distinct invoker
    mirrors the rubric "separate LLM, distinct from the working agent"
    philosophy and is deterministically testable.
    """
    if worktree is None or worktree == "":
        raise RecoverySummarizerError(reason="worktree not available")

    invoker = summarizer_invoke or _make_default_summarizer_invoke(
        summarizer_max_turns=summarizer_max_turns,
        summarizer_model=summarizer_model,
    )

    prompt = _build_summarizer_prompt(task, cumulative_diff, transcript_tail)

    try:
        response = await invoker(prompt, worktree)
    except RecoverySummarizerError:
        raise
    except Exception as exc:
        raise RecoverySummarizerError(reason=str(exc)) from exc

    result = parse_handoff(response)
    if not isinstance(result, ValidHandoff):
        raise RecoverySummarizerError(reason=_handoff_failure_reason(result))

    return result.handoff


__all__ = [
    "CLOSING_FENCE",
    "DuplicateHandoff",
    "HandoffResult",
    "MalformedHandoff",
    "MissingHandoff",
    "OPENING_FENCE",
    "RecoveryHandoff",
    "RecoverySummarizerError",
    "SummarizerInvoke",
    "TruncatedHandoff",
    "ValidHandoff",
    "parse_handoff",
    "run_recovery_summarizer",
]
