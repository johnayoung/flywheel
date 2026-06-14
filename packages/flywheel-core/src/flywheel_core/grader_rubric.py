"""Rubric grader runner — fresh-session LLM-as-judge verdicts.

Each :class:`flywheel_core.task.RubricGrader` on a task triggers a fresh
``claude-agent-sdk.query`` call against the attempt's worktree. The judge
sees the goal, the assertions, the working-agent transcript, and the
verdict-envelope contract; it terminates its response with one fenced
``<!-- RUBRIC_VERDICT -->`` JSON block. The runner parses that envelope
(taxonomy mirrors :mod:`flywheel_core.envelope`), persists one
:class:`GraderResultRecord` per ``RubricGrader``, and short-circuits on
the first hard-failed verdict within the rubric type.

Layered surface (matches :mod:`flywheel_core.grader_command`):

* :func:`parse_verdict` is pure — no IO, no SDK imports. The verdict
  taxonomy (:data:`VerdictResult`) is closed so every input maps to
  exactly one variant.
* :func:`run_rubric_graders` is the public async runner. It accepts a
  ``judge_invoke`` seam so tests can inject a fake without going near
  the SDK.
* The production default of ``judge_invoke`` lives in
  :func:`_make_default_judge_invoke` and delegates to
  :func:`claude_agent_sdk.query` with cwd set to the supplied worktree,
  full tool surface, and no ``session_id`` (fresh session — the judge
  never inherits the working agent's context).

Cost-order short-circuit: if ``command_passed`` or ``transcript_passed``
is ``False``, the runner returns ``[]`` without invoking any judge.
Within the rubric type, the first hard failure (``passed=false`` and
``unknown=false``) aborts the remaining rubric graders. An ``unknown``
verdict counts as a pass for lifecycle purposes and the runner proceeds
to the next rubric grader.
"""

from __future__ import annotations

import json
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal

from flywheel_core.store_protocols import GraderResultRecord, GraderResultStore
from flywheel_core.task import RubricGrader, Task


# --- Verdict envelope -------------------------------------------------------


OPENING_FENCE = "<!-- RUBRIC_VERDICT -->"
CLOSING_FENCE = "<!-- /RUBRIC_VERDICT -->"


@dataclass(frozen=True, kw_only=True)
class ValidVerdict:
    passed: bool
    summary: str
    unknown: bool = False
    kind: Literal["valid"] = "valid"


@dataclass(frozen=True, kw_only=True)
class MissingVerdict:
    kind: Literal["missing"] = "missing"


@dataclass(frozen=True, kw_only=True)
class TruncatedVerdict:
    detail: str
    kind: Literal["truncated"] = "truncated"


@dataclass(frozen=True, kw_only=True)
class DuplicateVerdict:
    count: int
    kind: Literal["duplicate"] = "duplicate"


@dataclass(frozen=True, kw_only=True)
class MalformedVerdict:
    reason: str
    offending: str | None = None
    kind: Literal["malformed"] = "malformed"


VerdictResult = (
    ValidVerdict
    | MissingVerdict
    | TruncatedVerdict
    | DuplicateVerdict
    | MalformedVerdict
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


def parse_verdict(text: str) -> VerdictResult:
    """Parse the rubric-verdict envelope out of a judge's response.

    The contract is closed: every input maps to exactly one variant of
    :data:`VerdictResult`. Missing fences, duplicate fences, truncation,
    JSON-decode failure, and any field-level shape violation are all
    distinguishable failure modes rather than coerced into a default.
    """
    if not isinstance(text, str):
        raise TypeError("parse_verdict requires a str input")

    open_positions = _all_occurrences(text, OPENING_FENCE)
    close_positions = _all_occurrences(text, CLOSING_FENCE)

    if not open_positions:
        return MissingVerdict()

    if not close_positions:
        return TruncatedVerdict(
            detail="opening fence found without a matching closing fence",
        )

    if len(open_positions) > 1 or len(close_positions) > 1:
        return DuplicateVerdict(
            count=max(len(open_positions), len(close_positions)),
        )

    open_pos = open_positions[0]
    close_pos = close_positions[0]

    if close_pos < open_pos:
        return TruncatedVerdict(
            detail="closing fence appears before opening fence",
        )

    inner = text[open_pos + len(OPENING_FENCE) : close_pos]

    try:
        data = json.loads(inner)
    except json.JSONDecodeError as exc:
        return MalformedVerdict(
            reason=f"verdict payload is not valid JSON: {exc.msg}",
            offending=_truncate(inner),
        )

    if not isinstance(data, dict):
        return MalformedVerdict(
            reason=(
                "verdict payload must be a JSON object, "
                f"got {type(data).__name__}"
            ),
            offending=_truncate(inner),
        )

    if "passed" not in data:
        return MalformedVerdict(
            reason="verdict payload missing required 'passed' field",
            offending=_truncate(inner),
        )
    raw_passed = data["passed"]
    if not isinstance(raw_passed, bool):
        return MalformedVerdict(
            reason=(
                "verdict 'passed' must be a bool, "
                f"got {type(raw_passed).__name__}"
            ),
            offending=_truncate(inner),
        )

    if "summary" not in data:
        return MalformedVerdict(
            reason="verdict payload missing required 'summary' field",
            offending=_truncate(inner),
        )
    raw_summary = data["summary"]
    if not isinstance(raw_summary, str):
        return MalformedVerdict(
            reason=(
                "verdict 'summary' must be a string, "
                f"got {type(raw_summary).__name__}"
            ),
            offending=_truncate(inner),
        )

    if "unknown" in data:
        raw_unknown = data["unknown"]
        if not isinstance(raw_unknown, bool):
            return MalformedVerdict(
                reason=(
                    "verdict 'unknown' must be a bool when present, "
                    f"got {type(raw_unknown).__name__}"
                ),
                offending=_truncate(inner),
            )
        unknown = raw_unknown
    else:
        unknown = False

    return ValidVerdict(passed=raw_passed, summary=raw_summary, unknown=unknown)


# --- Runner -----------------------------------------------------------------


JudgeInvoke = Callable[[str, RubricGrader, "Path | str"], Awaitable[str]]


class RubricJudgeError(Exception):
    """Raised when a rubric judge cannot produce a parseable verdict.

    Carries ``grader_name`` and ``reason`` attributes so the harness can
    render ``'rubric judge failed: <grader_name>: <reason>'`` without
    re-introspecting the exception. SDK exceptions raised by
    ``judge_invoke`` are wrapped into this type; parse failures
    (``Missing``/``Malformed``/``Duplicate``/``Truncated``) are also
    surfaced through it. The harness routes this exception through
    ``INTERNAL_ERROR`` — no ``GraderResultRecord`` is persisted for a
    judge-infrastructure failure.
    """

    def __init__(self, *, grader_name: str, reason: str) -> None:
        super().__init__(f"rubric judge failed: {grader_name}: {reason}")
        self.grader_name = grader_name
        self.reason = reason


_VERDICT_CONTRACT = """\
# Verdict envelope

End your response with exactly one fenced JSON block of the form:

<!-- RUBRIC_VERDICT -->
{"passed": true, "summary": "<one or two sentences>", "unknown": false}
<!-- /RUBRIC_VERDICT -->

Fields:
- `passed` (bool, required): true if every assertion holds; false otherwise.
- `summary` (str, required): brief rationale for the verdict; empty string accepted.
- `unknown` (bool, optional, default false): set true only when evidence is
  insufficient to judge. Counts as a pass for the lifecycle but is logged.

Emit the envelope exactly once. Missing, malformed, duplicate, or
truncated envelopes are treated as judge-infrastructure failures.
"""


def _build_judge_prompt(
    task: Task, grader: RubricGrader, transcript: str
) -> str:
    assertions = "\n".join(f"- {a}" for a in grader.assertions)
    return (
        f"# Goal\n\n{task.goal}\n\n"
        f"# Assertions\n\n{assertions}\n\n"
        f"# Working agent transcript\n\n{transcript}\n\n"
        f"{_VERDICT_CONTRACT}"
    )


def _grader_spec_snapshot(grader: RubricGrader) -> dict[str, Any]:
    """Snapshot the grader object as it appeared in the task at run time."""
    spec: dict[str, Any] = {
        "type": "rubric",
        "assertions": list(grader.assertions),
        "retry_on_fail": grader.retry_on_fail,
    }
    if grader.name is not None:
        spec["name"] = grader.name
    if grader.judge_model is not None:
        spec["judge_model"] = grader.judge_model
    if grader.rubric is not None:
        spec["rubric"] = grader.rubric
    return spec


def _make_default_judge_invoke(
    *, judge_max_turns: int, judge_model: str | None
) -> JudgeInvoke:
    """Build the production ``judge_invoke``: a fresh ``claude_agent_sdk.query``.

    The returned callable constructs :class:`ClaudeAgentOptions` per-call
    so the judge starts cold (no ``session_id`` inheritance), with cwd
    set to the supplied worktree, full tool surface (``skills='all'``),
    ``permission_mode='bypassPermissions'``, and a turn cap. The judge
    model resolves as ``grader.judge_model or judge_model``; both
    ``None`` falls through to the SDK's own default.
    """

    async def _default_judge_invoke(
        prompt: str, grader: RubricGrader, worktree: Path | str
    ) -> str:
        from flywheel_core._sdk import (
            AssistantMessage,
            ClaudeAgentOptions,
            TextBlock,
        )
        from flywheel_core._sdk import query as _sdk_query

        options = ClaudeAgentOptions(
            cwd=str(worktree),
            add_dirs=[str(worktree)],
            permission_mode="bypassPermissions",
            skills="all",
            max_turns=judge_max_turns,
            model=grader.judge_model or judge_model,
        )
        chunks: list[str] = []
        async for msg in _sdk_query(prompt=prompt, options=options):
            if isinstance(msg, AssistantMessage):
                for block in msg.content:
                    if isinstance(block, TextBlock):
                        chunks.append(block.text)
        return "".join(chunks)

    return _default_judge_invoke


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _verdict_failure_reason(verdict: VerdictResult) -> str:
    if isinstance(verdict, MissingVerdict):
        return "missing verdict envelope"
    if isinstance(verdict, TruncatedVerdict):
        return f"truncated verdict envelope: {verdict.detail}"
    if isinstance(verdict, DuplicateVerdict):
        return f"duplicate verdict envelopes (count={verdict.count})"
    if isinstance(verdict, MalformedVerdict):
        return f"malformed verdict envelope: {verdict.reason}"
    raise AssertionError(
        f"unexpected verdict variant: {type(verdict).__name__}"
    )


def _first_rubric_grader_name(task: Task) -> str:
    for grader in task.graders:
        if isinstance(grader, RubricGrader):
            return grader.name or "<unnamed>"
    return "<unnamed>"


async def run_rubric_graders(
    task: Task,
    store: GraderResultStore,
    *,
    run_id: str,
    attempt_number: int,
    transcript: str,
    worktree: Path | str | None,
    command_passed: bool,
    transcript_passed: bool,
    judge_invoke: JudgeInvoke | None = None,
    judge_model: str | None = None,
    judge_max_turns: int = 8,
    now: Callable[[], datetime] | None = None,
) -> list[GraderResultRecord]:
    """Run every ``RubricGrader`` on ``task.graders`` in list order.

    For each rubric grader:

    1. Assemble a judge prompt from the task goal, the grader's
       assertions, the working-agent ``transcript``, and the verdict
       envelope contract.
    2. Invoke ``judge_invoke`` (defaults to a fresh
       ``claude_agent_sdk.query`` against ``worktree``).
    3. Parse the response with :func:`parse_verdict`. Any non-``valid``
       variant raises :class:`RubricJudgeError` without persisting a row
       (judge-infrastructure failure — routed through ``INTERNAL_ERROR``
       by the harness).
    4. Persist one :class:`GraderResultRecord` whose payload matches the
       documented rubric shape (``judge_model``, ``summary``,
       ``unknown``, empty ``per_assertion``/``artifacts``).

    Cost-order is enforced at two scales:

    * Across types: when ``command_passed`` or ``transcript_passed`` is
      ``False``, no judge is invoked and the runner returns ``[]``.
    * Within type: the first hard-failed verdict (``passed=False`` and
      ``unknown=False``) aborts the remaining rubric graders.

    An ``unknown=True`` verdict counts as a pass for the lifecycle —
    ``passed=True`` is persisted, ``payload.unknown`` is ``True``, and
    the runner proceeds to the next rubric grader.

    Non-rubric graders in ``task.graders`` are ignored without persisting
    any row; the ``ordinal`` on each persisted record is the grader's
    index in ``task.graders`` (so non-rubric grader ordinals survive).
    """
    if not command_passed or not transcript_passed:
        return []

    rubric_present = any(isinstance(g, RubricGrader) for g in task.graders)
    if not rubric_present:
        return []

    if worktree is None or worktree == "":
        raise RubricJudgeError(
            grader_name=_first_rubric_grader_name(task),
            reason="worktree not available",
        )

    clock = now or _utcnow
    invoker = judge_invoke or _make_default_judge_invoke(
        judge_max_turns=judge_max_turns, judge_model=judge_model
    )

    persisted: list[GraderResultRecord] = []
    aborted = False

    for ordinal, grader in enumerate(task.graders):
        if not isinstance(grader, RubricGrader):
            continue
        if aborted:
            break

        ts_start = clock()
        start_ns = time.monotonic_ns()

        prompt = _build_judge_prompt(task, grader, transcript)
        try:
            response = await invoker(prompt, grader, worktree)
        except RubricJudgeError:
            raise
        except Exception as exc:
            raise RubricJudgeError(
                grader_name=grader.name or "<unnamed>",
                reason=str(exc),
            ) from exc

        verdict = parse_verdict(response)
        if not isinstance(verdict, ValidVerdict):
            raise RubricJudgeError(
                grader_name=grader.name or "<unnamed>",
                reason=_verdict_failure_reason(verdict),
            )

        end_ns = time.monotonic_ns()
        duration_ms = (end_ns - start_ns) // 1_000_000

        resolved_model = grader.judge_model or judge_model
        payload: dict[str, Any] = {
            "judge_model": resolved_model,
            "summary": verdict.summary,
            "unknown": verdict.unknown,
            "per_assertion": [],
            "artifacts": [],
        }

        passed = verdict.passed or verdict.unknown

        record = GraderResultRecord(
            run_id=run_id,
            attempt_number=attempt_number,
            ordinal=ordinal,
            grader_type="rubric",
            grader_spec=_grader_spec_snapshot(grader),
            grader_name=grader.name,
            passed=passed,
            duration_ms=int(duration_ms),
            payload=payload,
            ts=ts_start,
        )
        persisted.append(store.append_grader_result(record))

        # Within-type cost-order short-circuit: stop on first hard failure.
        # ``unknown=True`` is a pass and does not abort.
        if not verdict.passed and not verdict.unknown:
            aborted = True

    return persisted


__all__ = [
    "CLOSING_FENCE",
    "DuplicateVerdict",
    "JudgeInvoke",
    "MalformedVerdict",
    "MissingVerdict",
    "OPENING_FENCE",
    "RubricJudgeError",
    "TruncatedVerdict",
    "ValidVerdict",
    "VerdictResult",
    "parse_verdict",
    "run_rubric_graders",
]
