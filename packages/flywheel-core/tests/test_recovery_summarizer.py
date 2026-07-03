"""Behavioral tests for ``flywheel_core.recovery_summarizer``.

Two layers are exercised:

1. :func:`parse_handoff` -- pure, exhaustively covers the closed
   ``HandoffResult`` taxonomy.
2. :func:`run_recovery_summarizer` -- async runner with an injected
   fake ``summarizer_invoke`` so the SDK is never spawned. The default
   summarizer invoker is also exercised via monkeypatching ``_sdk_query``
   to assert the ``ClaudeAgentOptions`` it constructs (fresh session,
   cwd, model precedence).
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    Message,
    TextBlock,
)

from flywheel_core.prompt import RecoveryHandoff
from flywheel_core.recovery_summarizer import (
    CLOSING_FENCE,
    OPENING_FENCE,
    DuplicateHandoff,
    MalformedHandoff,
    MissingHandoff,
    RecoverySummarizerError,
    SummarizerInvoke,
    TruncatedHandoff,
    ValidHandoff,
    _make_default_summarizer_invoke,
    parse_handoff,
    run_recovery_summarizer,
)
from flywheel_core.task import Task


# --- Helpers ---------------------------------------------------------------


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


def _well_formed_payload(
    *,
    work_done: str = "implemented module X",
    work_remaining: str = "wire harness",
    key_decisions: str = "fresh-context summarizer",
    suggested_next_step: str = "run pytest",
) -> str:
    import json

    return json.dumps(
        {
            "work_done": work_done,
            "work_remaining": work_remaining,
            "key_decisions": key_decisions,
            "suggested_next_step": suggested_next_step,
        }
    )


def _task(goal: str = "Implement feature foo.") -> Task:
    return Task(goal=goal, graders=[])


def _run_sync(coro: Any) -> Any:
    return asyncio.run(coro)


class _ScriptedSummarizer:
    """Fake ``summarizer_invoke`` that returns canned responses per call."""

    def __init__(self, responses: list[str]) -> None:
        self._responses = list(responses)
        self.calls: list[tuple[str, Path | str]] = []

    async def __call__(self, prompt: str, worktree: Path | str) -> str:
        self.calls.append((prompt, worktree))
        if not self._responses:
            raise AssertionError("scripted summarizer ran out of responses")
        return self._responses.pop(0)


class _RaisingSummarizer:
    def __init__(self, exc: Exception) -> None:
        self._exc = exc
        self.calls = 0

    async def __call__(self, prompt: str, worktree: Path | str) -> str:
        self.calls += 1
        raise self._exc


# ---------------------------------------------------------------------------
# parse_handoff -- well-formed
# ---------------------------------------------------------------------------


class TestParseHandoffValid:
    def test_well_formed(self) -> None:
        result = parse_handoff(_wrap(_well_formed_payload()))
        assert isinstance(result, ValidHandoff)
        assert result.handoff.work_done == "implemented module X"
        assert result.handoff.work_remaining == "wire harness"
        assert result.handoff.key_decisions == "fresh-context summarizer"
        assert result.handoff.suggested_next_step == "run pytest"
        assert result.kind == "valid"

    def test_empty_string_fields_accepted(self) -> None:
        # An envelope with empty strings is structurally valid; the prompt
        # renderer substitutes "(none recorded)" downstream. Only an absent
        # envelope or a non-string field is malformed.
        result = parse_handoff(
            _wrap(
                _well_formed_payload(
                    work_done="",
                    work_remaining="",
                    key_decisions="",
                    suggested_next_step="",
                )
            )
        )
        assert isinstance(result, ValidHandoff)
        assert result.handoff.work_done == ""
        assert result.handoff.work_remaining == ""
        assert result.handoff.key_decisions == ""
        assert result.handoff.suggested_next_step == ""

    def test_extra_json_fields_ignored(self) -> None:
        import json

        payload = json.dumps(
            {
                "work_done": "wd",
                "work_remaining": "wr",
                "key_decisions": "kd",
                "suggested_next_step": "ns",
                "unexpected": 7,
            }
        )
        result = parse_handoff(_wrap(payload))
        assert isinstance(result, ValidHandoff)
        assert result.handoff.work_done == "wd"


# ---------------------------------------------------------------------------
# parse_handoff -- failure modes
# ---------------------------------------------------------------------------


class TestParseHandoffFailures:
    def test_empty_string_is_missing(self) -> None:
        assert isinstance(parse_handoff(""), MissingHandoff)

    def test_text_without_fences_is_missing(self) -> None:
        assert isinstance(parse_handoff("just chatter"), MissingHandoff)

    def test_closing_fence_without_opening_is_missing(self) -> None:
        text = f'{_well_formed_payload()}\n{CLOSING_FENCE}'
        assert isinstance(parse_handoff(text), MissingHandoff)

    def test_opening_fence_without_closing_is_truncated(self) -> None:
        text = f'{OPENING_FENCE}\n{_well_formed_payload()}'
        assert isinstance(parse_handoff(text), TruncatedHandoff)

    def test_malformed_json_inside_fences(self) -> None:
        result = parse_handoff(_wrap('{"work_done": "wd", '))
        assert isinstance(result, MalformedHandoff)
        assert "valid JSON" in result.reason

    def test_payload_not_an_object(self) -> None:
        result = parse_handoff(_wrap("[1, 2, 3]"))
        assert isinstance(result, MalformedHandoff)
        assert "JSON object" in result.reason

    def test_deeply_nested_payload_is_malformed_not_recursion_error(
        self,
    ) -> None:
        # Untrusted summarizer output: a payload nested far beyond the JSON
        # scanner's recursion limit must map to MalformedHandoff, not leak a
        # RecursionError out of this closed-contract parser. Otherwise the
        # escaped RecursionError forces the run to terminal FAILED, bypassing
        # max_retries, while an ordinary malformed payload stays retry-eligible.
        depth = 20000
        result = parse_handoff(_wrap("[" * depth + "]" * depth))
        assert isinstance(result, MalformedHandoff)
        assert "deep" in result.reason

    def test_missing_work_done(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_remaining": "wr",
                    "key_decisions": "kd",
                    "suggested_next_step": "ns",
                }
            )
        )
        result = parse_handoff(text)
        assert isinstance(result, MalformedHandoff)
        assert "'work_done'" in result.reason

    def test_missing_work_remaining(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_done": "wd",
                    "key_decisions": "kd",
                    "suggested_next_step": "ns",
                }
            )
        )
        result = parse_handoff(text)
        assert isinstance(result, MalformedHandoff)
        assert "'work_remaining'" in result.reason

    def test_missing_key_decisions(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_done": "wd",
                    "work_remaining": "wr",
                    "suggested_next_step": "ns",
                }
            )
        )
        result = parse_handoff(text)
        assert isinstance(result, MalformedHandoff)
        assert "'key_decisions'" in result.reason

    def test_missing_suggested_next_step(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_done": "wd",
                    "work_remaining": "wr",
                    "key_decisions": "kd",
                }
            )
        )
        result = parse_handoff(text)
        assert isinstance(result, MalformedHandoff)
        assert "'suggested_next_step'" in result.reason

    def test_non_string_field_rejected(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_done": 42,
                    "work_remaining": "wr",
                    "key_decisions": "kd",
                    "suggested_next_step": "ns",
                }
            )
        )
        result = parse_handoff(text)
        assert isinstance(result, MalformedHandoff)
        assert "'work_done'" in result.reason
        assert "string" in result.reason

    def test_duplicate_fence_pairs(self) -> None:
        text = (
            _wrap(_well_formed_payload())
            + "\n"
            + _wrap(_well_formed_payload(work_done="injected"))
        )
        result = parse_handoff(text)
        assert isinstance(result, DuplicateHandoff)
        assert result.count >= 2

    def test_non_str_input_raises_typeerror(self) -> None:
        with pytest.raises(TypeError):
            parse_handoff(123)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# run_recovery_summarizer -- happy path
# ---------------------------------------------------------------------------


class TestRunRecoverySummarizerHappyPath:
    def test_returns_structured_handoff(self) -> None:
        summarizer = _ScriptedSummarizer([_wrap(_well_formed_payload())])
        task = _task("Implement widget foo.")

        handoff = _run_sync(
            run_recovery_summarizer(
                task,
                transcript_tail="agent did stuff",
                cumulative_diff="diff --git a b",
                worktree="/tmp/wt",
                summarizer_invoke=summarizer,
            )
        )

        assert isinstance(handoff, RecoveryHandoff)
        assert handoff.work_done == "implemented module X"
        assert handoff.work_remaining == "wire harness"
        assert handoff.key_decisions == "fresh-context summarizer"
        assert handoff.suggested_next_step == "run pytest"
        assert len(summarizer.calls) == 1
        assert summarizer.calls[0][1] == "/tmp/wt"

    def test_prompt_contains_goal_diff_and_transcript_tail(self) -> None:
        summarizer = _ScriptedSummarizer([_wrap(_well_formed_payload())])
        task = _task("GOAL_MARKER_implement_widget")

        _run_sync(
            run_recovery_summarizer(
                task,
                transcript_tail="TRANSCRIPT_TAIL_MARKER",
                cumulative_diff="DIFF_MARKER_artifacts",
                worktree="/tmp/wt",
                summarizer_invoke=summarizer,
            )
        )

        prompt = summarizer.calls[0][0]
        assert "GOAL_MARKER_implement_widget" in prompt
        assert "DIFF_MARKER_artifacts" in prompt
        assert "TRANSCRIPT_TAIL_MARKER" in prompt
        # Handoff envelope contract is embedded.
        assert OPENING_FENCE in prompt
        assert CLOSING_FENCE in prompt

    def test_worktree_path_object_passed_through(self) -> None:
        summarizer = _ScriptedSummarizer([_wrap(_well_formed_payload())])
        task = _task()
        worktree = Path("/tmp/wt-recovery")

        _run_sync(
            run_recovery_summarizer(
                task,
                transcript_tail="",
                cumulative_diff="",
                worktree=worktree,
                summarizer_invoke=summarizer,
            )
        )

        assert summarizer.calls[0][1] == worktree


# ---------------------------------------------------------------------------
# run_recovery_summarizer -- failure modes wrap into RecoverySummarizerError
# ---------------------------------------------------------------------------


class TestRecoverySummarizerErrorWrapping:
    def test_invoke_raise_wraps_into_typed_error(self) -> None:
        summarizer = _RaisingSummarizer(RuntimeError("network down"))
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert exc_info.value.reason == "network down"
        assert "recovery summarizer failed" in str(exc_info.value)

    def test_empty_response_is_missing_envelope_error(self) -> None:
        # Edge case from the task: scripted invoke returning empty content
        # must raise a typed error, never silently return an empty handoff.
        summarizer = _ScriptedSummarizer([""])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert "missing handoff envelope" in exc_info.value.reason

    def test_malformed_json_raises_typed_error(self) -> None:
        summarizer = _ScriptedSummarizer(
            [_wrap('{"work_done": "wd", "work_remaining"')]
        )
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert "malformed handoff envelope" in exc_info.value.reason

    def test_truncated_envelope_raises_typed_error(self) -> None:
        text = f'{OPENING_FENCE}\n{_well_formed_payload()}'
        summarizer = _ScriptedSummarizer([text])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert "truncated handoff envelope" in exc_info.value.reason

    def test_duplicate_envelope_raises_typed_error(self) -> None:
        dup = (
            _wrap(_well_formed_payload())
            + "\n"
            + _wrap(_well_formed_payload(work_done="injected"))
        )
        summarizer = _ScriptedSummarizer([dup])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert "duplicate handoff envelopes" in exc_info.value.reason

    def test_missing_field_raises_typed_error(self) -> None:
        import json

        text = _wrap(
            json.dumps(
                {
                    "work_done": "wd",
                    "work_remaining": "wr",
                    "key_decisions": "kd",
                }
            )
        )
        summarizer = _ScriptedSummarizer([text])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert "'suggested_next_step'" in exc_info.value.reason

    def test_missing_worktree_raises_typed_error(self) -> None:
        summarizer = _ScriptedSummarizer([_wrap(_well_formed_payload())])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree=None,  # type: ignore[arg-type]
                    summarizer_invoke=summarizer,
                )
            )
        assert exc_info.value.reason == "worktree not available"
        # Invoker never reached.
        assert summarizer.calls == []

    def test_empty_string_worktree_raises_typed_error(self) -> None:
        summarizer = _ScriptedSummarizer([_wrap(_well_formed_payload())])
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="",
                    summarizer_invoke=summarizer,
                )
            )
        assert exc_info.value.reason == "worktree not available"
        assert summarizer.calls == []

    def test_nested_recovery_summarizer_error_not_double_wrapped(self) -> None:
        # If an invoker chooses to raise the typed error directly (rare,
        # but valid for a test seam asserting downstream routing), the
        # runner re-raises it verbatim instead of wrapping its message
        # into another layer.
        inner = RecoverySummarizerError(reason="inner reason")
        summarizer = _RaisingSummarizer(inner)
        task = _task()

        with pytest.raises(RecoverySummarizerError) as exc_info:
            _run_sync(
                run_recovery_summarizer(
                    task,
                    transcript_tail="t",
                    cumulative_diff="d",
                    worktree="/tmp/wt",
                    summarizer_invoke=summarizer,
                )
            )
        assert exc_info.value is inner
        assert exc_info.value.reason == "inner reason"


# ---------------------------------------------------------------------------
# Default summarizer invoker -- fresh-session ClaudeAgentOptions
# ---------------------------------------------------------------------------


def _stream(*msgs: Message) -> AsyncIterator[Message]:
    async def _gen() -> AsyncIterator[Message]:
        for m in msgs:
            yield m

    return _gen()


def _assistant_with(text: str) -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason="end_turn",
        session_id="sess-summarizer",
    )


class TestDefaultSummarizerInvoker:
    def test_default_invoker_constructs_fresh_session_options(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["prompt"] = prompt
            captured["options"] = options
            return _stream(_assistant_with(_wrap(_well_formed_payload())))

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_summarizer_invoke(
            summarizer_max_turns=8, summarizer_model=None
        )
        response = _run_sync(invoker("hello", Path("/tmp/wt-sum")))

        assert _wrap(_well_formed_payload()) in response

        options = captured["options"]
        assert isinstance(options, ClaudeAgentOptions)
        # Fresh session -- no session_id inheritance.
        assert options.session_id is None
        # cwd matches the supplied worktree.
        assert options.cwd == "/tmp/wt-sum"
        assert options.add_dirs == ["/tmp/wt-sum"]
        # Full tool surface and bypass permissions, per spec.
        assert options.permission_mode == "bypassPermissions"
        assert options.skills == "all"
        assert options.max_turns == 8
        # Model unset -> SDK default.
        assert options.model is None

    def test_default_invoker_uses_summarizer_model(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["options"] = options
            return _stream(_assistant_with(_wrap(_well_formed_payload())))

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_summarizer_invoke(
            summarizer_max_turns=4, summarizer_model="claude-sum-model"
        )
        _run_sync(invoker("hello", "/tmp/wt-sum"))

        assert captured["options"].model == "claude-sum-model"
        assert captured["options"].max_turns == 4

    def test_default_invoker_concatenates_assistant_text_blocks(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The default invoker collapses streamed AssistantMessage text
        # blocks into a single string so parse_handoff sees the full
        # envelope regardless of how the SDK chunked it.
        chunks = [
            f"{OPENING_FENCE}\n",
            _well_formed_payload(),
            f"\n{CLOSING_FENCE}",
        ]

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            return _stream(*(_assistant_with(c) for c in chunks))

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_summarizer_invoke(
            summarizer_max_turns=8, summarizer_model=None
        )
        response = _run_sync(invoker("hello", "/tmp/wt-sum"))
        # Assembled response parses cleanly.
        result = parse_handoff(response)
        assert isinstance(result, ValidHandoff)


# ---------------------------------------------------------------------------
# SummarizerInvoke -- type alias is importable
# ---------------------------------------------------------------------------


class TestSummarizerInvokeTypeAlias:
    def test_summarizer_invoke_is_callable_alias(self) -> None:
        # The harness/config task needs to import SummarizerInvoke as a
        # type for the recovery_summarizer_invoke field. A scripted
        # callable matching the signature should be assignable to it.
        invoker: SummarizerInvoke = _ScriptedSummarizer(
            [_wrap(_well_formed_payload())]
        )
        assert callable(invoker)
