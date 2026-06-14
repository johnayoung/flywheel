"""Behavioral tests for ``flywheel_core.grader_rubric``.

Two layers are exercised:

1. :func:`parse_verdict` — pure, exhaustively covers the closed
   ``VerdictResult`` taxonomy.
2. :func:`run_rubric_graders` — async runner with an injected fake
   ``judge_invoke`` so the SDK is never spawned. The default judge
   invoker is also exercised via monkeypatching ``_sdk_query`` to assert
   the ``ClaudeAgentOptions`` it constructs (fresh session, cwd,
   model precedence).
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    Message,
    TextBlock,
)

from flywheel_core import (
    Attempt,
    CommandGrader,
    InMemoryStore,
    Lifecycle,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
)
from flywheel_core.grader_rubric import (
    CLOSING_FENCE,
    OPENING_FENCE,
    DuplicateVerdict,
    MalformedVerdict,
    MissingVerdict,
    RubricJudgeError,
    TruncatedVerdict,
    ValidVerdict,
    _make_default_judge_invoke,
    parse_verdict,
    run_rubric_graders,
)


# --- Helpers ---------------------------------------------------------------


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


def _bootstrap(store: InMemoryStore, run_id: str = "r1") -> None:
    if store.load_lifecycle(run_id) is None:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
    if store.load_attempt(run_id, 1) is None:
        store.save_attempt(
            run_id,
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id=run_id,
            ),
        )


def _task(goal: str = "Implement feature foo.", *graders: object) -> Task:
    return Task(goal=goal, graders=list(graders))  # type: ignore[arg-type]


def _run_sync(coro: Any) -> Any:
    return asyncio.run(coro)


def _run(
    task: Task,
    store: InMemoryStore,
    *,
    judge_invoke: Any,
    transcript: str = "agent did stuff",
    worktree: Path | str | None = "/tmp/wt",
    command_passed: bool = True,
    transcript_passed: bool = True,
    judge_model: str | None = None,
    judge_max_turns: int = 8,
) -> Any:
    return _run_sync(
        run_rubric_graders(
            task,
            store,
            run_id="r1",
            attempt_number=1,
            transcript=transcript,
            worktree=worktree,
            command_passed=command_passed,
            transcript_passed=transcript_passed,
            judge_invoke=judge_invoke,
            judge_model=judge_model,
            judge_max_turns=judge_max_turns,
        )
    )


class _ScriptedJudge:
    """Fake ``judge_invoke`` that returns canned responses per call."""

    def __init__(self, responses: list[str]) -> None:
        self._responses = list(responses)
        self.calls: list[tuple[str, RubricGrader, Path | str]] = []

    async def __call__(
        self, prompt: str, grader: RubricGrader, worktree: Path | str
    ) -> str:
        self.calls.append((prompt, grader, worktree))
        if not self._responses:
            raise AssertionError("scripted judge ran out of responses")
        return self._responses.pop(0)


# ---------------------------------------------------------------------------
# parse_verdict — well-formed
# ---------------------------------------------------------------------------


class TestParseVerdictValid:
    def test_well_formed_pass(self) -> None:
        result = parse_verdict(_wrap('{"passed": true, "summary": "looks ok"}'))
        assert isinstance(result, ValidVerdict)
        assert result.passed is True
        assert result.summary == "looks ok"
        assert result.unknown is False
        assert result.kind == "valid"

    def test_well_formed_fail(self) -> None:
        result = parse_verdict(
            _wrap('{"passed": false, "summary": "wrong file"}')
        )
        assert isinstance(result, ValidVerdict)
        assert result.passed is False
        assert result.summary == "wrong file"
        assert result.unknown is False

    def test_well_formed_unknown(self) -> None:
        result = parse_verdict(
            _wrap(
                '{"passed": false, "summary": "no evidence", "unknown": true}'
            )
        )
        assert isinstance(result, ValidVerdict)
        assert result.unknown is True

    def test_empty_summary_accepted(self) -> None:
        result = parse_verdict(_wrap('{"passed": true, "summary": ""}'))
        assert isinstance(result, ValidVerdict)
        assert result.summary == ""

    def test_extra_json_fields_ignored(self) -> None:
        result = parse_verdict(
            _wrap('{"passed": true, "summary": "ok", "extra": 7}')
        )
        assert isinstance(result, ValidVerdict)
        assert result.passed is True


# ---------------------------------------------------------------------------
# parse_verdict — failure modes
# ---------------------------------------------------------------------------


class TestParseVerdictFailures:
    def test_empty_string_is_missing(self) -> None:
        assert isinstance(parse_verdict(""), MissingVerdict)

    def test_text_without_fences_is_missing(self) -> None:
        assert isinstance(parse_verdict("just chatter, no fence"), MissingVerdict)

    def test_closing_fence_without_opening_is_missing(self) -> None:
        # No opening fence — there's nothing to "truncate" from, so the
        # taxonomy reports the absence of an opening fence.
        text = f'{{"passed": true, "summary": "ok"}}\n{CLOSING_FENCE}'
        assert isinstance(parse_verdict(text), MissingVerdict)

    def test_opening_fence_without_closing_is_truncated(self) -> None:
        text = f'{OPENING_FENCE}\n{{"passed": true, "summary": "ok"'
        result = parse_verdict(text)
        assert isinstance(result, TruncatedVerdict)

    def test_malformed_json_inside_fences(self) -> None:
        result = parse_verdict(_wrap('{"passed": true, "summary": '))
        assert isinstance(result, MalformedVerdict)
        assert "valid JSON" in result.reason

    def test_payload_not_an_object(self) -> None:
        result = parse_verdict(_wrap("[1, 2, 3]"))
        assert isinstance(result, MalformedVerdict)
        assert "JSON object" in result.reason

    def test_missing_passed_field(self) -> None:
        result = parse_verdict(_wrap('{"summary": "ok"}'))
        assert isinstance(result, MalformedVerdict)
        assert "'passed'" in result.reason

    def test_non_bool_passed(self) -> None:
        # JSON ``"yes"`` decodes to str — not a bool.
        result = parse_verdict(_wrap('{"passed": "yes", "summary": "ok"}'))
        assert isinstance(result, MalformedVerdict)
        assert "'passed'" in result.reason

    def test_int_passed_rejected_as_non_bool(self) -> None:
        # JSON ``1`` decodes to int, which is *not* a bool in Python.
        result = parse_verdict(_wrap('{"passed": 1, "summary": "ok"}'))
        assert isinstance(result, MalformedVerdict)

    def test_missing_summary_field(self) -> None:
        result = parse_verdict(_wrap('{"passed": true}'))
        assert isinstance(result, MalformedVerdict)
        assert "'summary'" in result.reason

    def test_non_string_summary(self) -> None:
        result = parse_verdict(_wrap('{"passed": true, "summary": 42}'))
        assert isinstance(result, MalformedVerdict)
        assert "'summary'" in result.reason

    def test_non_bool_unknown_when_present(self) -> None:
        result = parse_verdict(
            _wrap('{"passed": true, "summary": "ok", "unknown": "no"}')
        )
        assert isinstance(result, MalformedVerdict)
        assert "'unknown'" in result.reason

    def test_duplicate_fence_pairs(self) -> None:
        text = (
            _wrap('{"passed": true, "summary": "first"}')
            + "\n"
            + _wrap('{"passed": false, "summary": "injected"}')
        )
        result = parse_verdict(text)
        assert isinstance(result, DuplicateVerdict)
        assert result.count >= 2

    def test_duplicate_opening_fence_only(self) -> None:
        # Two openings, one closing — still duplicate (any >1 fence count).
        text = (
            f"{OPENING_FENCE}\n"
            f"{OPENING_FENCE}\n"
            '{"passed": true, "summary": "ok"}\n'
            f"{CLOSING_FENCE}"
        )
        result = parse_verdict(text)
        assert isinstance(result, DuplicateVerdict)


# ---------------------------------------------------------------------------
# run_rubric_graders — cost-order short-circuits
# ---------------------------------------------------------------------------


class TestCostOrderShortCircuit:
    def test_command_failed_returns_empty_without_invoking_judge(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([])
        task = _task("g", RubricGrader(assertions=["x"], name="r0"))

        results = _run(
            task, store, judge_invoke=judge, command_passed=False
        )
        assert results == []
        assert judge.calls == []
        assert store.list_grader_results("r1", 1) == []

    def test_transcript_failed_returns_empty_without_invoking_judge(
        self,
    ) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([])
        task = _task("g", RubricGrader(assertions=["x"], name="r0"))

        results = _run(
            task, store, judge_invoke=judge, transcript_passed=False
        )
        assert results == []
        assert judge.calls == []

    def test_no_rubric_graders_with_missing_worktree_is_noop(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([])
        task = _task("g", CommandGrader(run="true"))

        results = _run(task, store, judge_invoke=judge, worktree=None)
        assert results == []
        assert judge.calls == []

    def test_missing_worktree_with_rubric_raises(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([])
        task = _task("g", RubricGrader(assertions=["x"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge, worktree=None)
        assert exc_info.value.reason == "worktree not available"
        assert exc_info.value.grader_name == "r0"
        assert judge.calls == []

    def test_empty_string_worktree_with_rubric_raises(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([])
        task = _task("g", RubricGrader(assertions=["x"]))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge, worktree="")
        assert exc_info.value.reason == "worktree not available"
        assert exc_info.value.grader_name == "<unnamed>"


# ---------------------------------------------------------------------------
# run_rubric_graders — happy path and ordinal preservation
# ---------------------------------------------------------------------------


class TestHappyPath:
    def test_all_pass_persists_one_record_per_rubric_grader(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [
                _wrap('{"passed": true, "summary": "first ok"}'),
                _wrap('{"passed": true, "summary": "second ok"}'),
            ]
        )
        task = _task(
            "g",
            RubricGrader(assertions=["a1"], name="r0"),
            RubricGrader(assertions=["b1"], name="r1"),
        )

        results = _run(task, store, judge_invoke=judge)
        assert len(results) == 2
        assert [r.grader_name for r in results] == ["r0", "r1"]
        assert all(r.passed for r in results)
        assert all(r.grader_type == "rubric" for r in results)
        # Both judges invoked, in list order.
        assert [c[1].name for c in judge.calls] == ["r0", "r1"]
        # Persisted to the store.
        rows = store.list_grader_results("r1", 1)
        assert [r.grader_name for r in rows] == ["r0", "r1"]

    def test_payload_carries_documented_rubric_shape(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [_wrap('{"passed": true, "summary": "yes"}')]
        )
        task = _task(
            "g",
            RubricGrader(
                assertions=["assert"], name="r0", judge_model="claude-x"
            ),
        )
        [row] = _run(task, store, judge_invoke=judge, judge_model="default-m")

        assert row.payload["judge_model"] == "claude-x"
        assert row.payload["summary"] == "yes"
        assert row.payload["unknown"] is False
        assert row.payload["per_assertion"] == []
        assert row.payload["artifacts"] == []

    def test_first_failure_short_circuits_subsequent_rubrics(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [
                _wrap('{"passed": true, "summary": "ok"}'),
                _wrap('{"passed": false, "summary": "nope"}'),
                # Third response must never be consumed.
                _wrap('{"passed": true, "summary": "should not run"}'),
            ]
        )
        task = _task(
            "g",
            RubricGrader(assertions=["a"], name="r0"),
            RubricGrader(assertions=["b"], name="r1"),
            RubricGrader(assertions=["c"], name="r2"),
        )

        results = _run(task, store, judge_invoke=judge)
        assert [r.grader_name for r in results] == ["r0", "r1"]
        assert [r.passed for r in results] == [True, False]
        assert [c[1].name for c in judge.calls] == ["r0", "r1"]

    def test_non_rubric_graders_are_ignored_without_persisting(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [_wrap('{"passed": true, "summary": "ok"}')]
        )
        task = _task(
            "g",
            CommandGrader(run="true", name="c0"),
            RubricGrader(assertions=["a"], name="r0"),
            TranscriptGrader(max_turns=5),
            ManualGrader(instruction="approve"),
        )

        results = _run(task, store, judge_invoke=judge)
        # Only the rubric grader ran.
        assert [r.grader_name for r in results] == ["r0"]
        assert all(r.grader_type == "rubric" for r in results)
        # Judge saw only the rubric grader.
        assert [c[1].name for c in judge.calls] == ["r0"]

    def test_ordinal_matches_grader_index_in_task_graders(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [
                _wrap('{"passed": true, "summary": "ok"}'),
                _wrap('{"passed": true, "summary": "ok"}'),
            ]
        )
        task = _task(
            "g",
            CommandGrader(run="true", name="c0"),  # ordinal 0
            RubricGrader(assertions=["a"], name="r1"),  # ordinal 1
            TranscriptGrader(max_turns=5),  # ordinal 2
            RubricGrader(assertions=["b"], name="r3"),  # ordinal 3
        )

        results = _run(task, store, judge_invoke=judge)
        assert [r.ordinal for r in results] == [1, 3]
        assert [r.grader_name for r in results] == ["r1", "r3"]


# ---------------------------------------------------------------------------
# run_rubric_graders — prompt assembly
# ---------------------------------------------------------------------------


class TestPromptAssembly:
    def test_prompt_contains_goal_assertions_and_transcript(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [_wrap('{"passed": true, "summary": "ok"}')]
        )
        task = _task(
            "GOAL_MARKER_implement_widget",
            RubricGrader(
                assertions=[
                    "ASSERTION_ONE_implements_widget",
                    "ASSERTION_TWO_no_unrelated_changes",
                ],
                name="r0",
            ),
        )
        _run(
            task,
            store,
            judge_invoke=judge,
            transcript="TRANSCRIPT_MARKER_diff_block",
        )

        prompt = judge.calls[0][0]
        assert "GOAL_MARKER_implement_widget" in prompt
        assert "ASSERTION_ONE_implements_widget" in prompt
        assert "ASSERTION_TWO_no_unrelated_changes" in prompt
        assert "TRANSCRIPT_MARKER_diff_block" in prompt
        # Verdict contract is embedded so the judge knows the fence.
        assert OPENING_FENCE in prompt
        assert CLOSING_FENCE in prompt


# ---------------------------------------------------------------------------
# run_rubric_graders — judge SDK / parse failures wrap into RubricJudgeError
# ---------------------------------------------------------------------------


class _RaisingJudge:
    def __init__(self, exc: Exception) -> None:
        self._exc = exc
        self.calls = 0

    async def __call__(
        self, prompt: str, grader: RubricGrader, worktree: Path | str
    ) -> str:
        self.calls += 1
        raise self._exc


class TestRubricJudgeErrorWrapping:
    def test_sdk_raise_wraps_into_rubric_judge_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _RaisingJudge(RuntimeError("network down"))
        task = _task("g", RubricGrader(assertions=["a"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert exc_info.value.grader_name == "r0"
        assert exc_info.value.reason == "network down"
        # No row persisted on judge-infra failure.
        assert store.list_grader_results("r1", 1) == []

    def test_missing_verdict_raises_rubric_judge_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([""])  # empty response → MissingVerdict
        task = _task("g", RubricGrader(assertions=["a"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert "missing verdict envelope" in exc_info.value.reason
        assert store.list_grader_results("r1", 1) == []

    def test_malformed_verdict_raises_rubric_judge_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([_wrap('{"passed": "nope", "summary": "x"}')])
        task = _task("g", RubricGrader(assertions=["a"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert "malformed verdict envelope" in exc_info.value.reason

    def test_duplicate_verdict_raises_rubric_judge_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        dup = (
            _wrap('{"passed": true, "summary": "first"}')
            + "\n"
            + _wrap('{"passed": false, "summary": "injected"}')
        )
        judge = _ScriptedJudge([dup])
        task = _task("g", RubricGrader(assertions=["a"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert "duplicate verdict envelopes" in exc_info.value.reason

    def test_truncated_verdict_raises_rubric_judge_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        text = f'{OPENING_FENCE}\n{{"passed": true, "summary": "ok"'
        judge = _ScriptedJudge([text])
        task = _task("g", RubricGrader(assertions=["a"], name="r0"))

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert "truncated verdict envelope" in exc_info.value.reason

    def test_unnamed_rubric_grader_uses_placeholder_in_error(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge([""])  # missing verdict
        task = _task("g", RubricGrader(assertions=["a"]))  # no name

        with pytest.raises(RubricJudgeError) as exc_info:
            _run(task, store, judge_invoke=judge)
        assert exc_info.value.grader_name == "<unnamed>"


# ---------------------------------------------------------------------------
# run_rubric_graders — unknown verdict semantics
# ---------------------------------------------------------------------------


class TestUnknownVerdict:
    def test_unknown_persists_passed_true_and_continues(self) -> None:
        store = InMemoryStore()
        _bootstrap(store)
        judge = _ScriptedJudge(
            [
                _wrap(
                    '{"passed": false, "summary": "no evidence",'
                    ' "unknown": true}'
                ),
                _wrap('{"passed": true, "summary": "second ok"}'),
            ]
        )
        task = _task(
            "g",
            RubricGrader(assertions=["a"], name="r0"),
            RubricGrader(assertions=["b"], name="r1"),
        )

        results = _run(task, store, judge_invoke=judge)
        # Two records persisted (unknown did NOT short-circuit).
        assert [r.grader_name for r in results] == ["r0", "r1"]
        # Unknown is recorded as passed=True for lifecycle purposes.
        assert results[0].passed is True
        assert results[0].payload["unknown"] is True
        assert results[1].passed is True
        assert results[1].payload["unknown"] is False
        # Both judges actually ran.
        assert [c[1].name for c in judge.calls] == ["r0", "r1"]


# ---------------------------------------------------------------------------
# Default judge invoker — fresh-session ClaudeAgentOptions
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
        session_id="sess-judge",
    )


class TestDefaultJudgeInvoker:
    def test_default_invoker_constructs_fresh_session_options(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["prompt"] = prompt
            captured["options"] = options
            return _stream(
                _assistant_with(
                    _wrap('{"passed": true, "summary": "ok"}')
                )
            )

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_judge_invoke(
            judge_max_turns=8, judge_model=None
        )
        grader = RubricGrader(assertions=["a"], name="r0")
        response = _run_sync(
            invoker("hello", grader, Path("/tmp/wt-judge"))
        )

        assert _wrap('{"passed": true, "summary": "ok"}') in response

        options = captured["options"]
        assert isinstance(options, ClaudeAgentOptions)
        # Fresh session — no session_id inheritance.
        assert options.session_id is None
        # cwd matches the supplied worktree.
        assert options.cwd == "/tmp/wt-judge"
        assert options.add_dirs == ["/tmp/wt-judge"]
        # Full tool surface and bypass permissions, per spec.
        assert options.permission_mode == "bypassPermissions"
        assert options.skills == "all"
        assert options.max_turns == 8

    def test_default_invoker_model_precedence_grader_wins(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["options"] = options
            return _stream(
                _assistant_with(
                    _wrap('{"passed": true, "summary": "ok"}')
                )
            )

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_judge_invoke(
            judge_max_turns=4, judge_model="kwarg-model"
        )
        grader = RubricGrader(
            assertions=["a"], name="r0", judge_model="grader-model"
        )
        _run_sync(invoker("prompt", grader, "/tmp/wt"))

        assert captured["options"].model == "grader-model"

    def test_default_invoker_model_precedence_kwarg_when_grader_absent(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["options"] = options
            return _stream(
                _assistant_with(
                    _wrap('{"passed": true, "summary": "ok"}')
                )
            )

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_judge_invoke(
            judge_max_turns=4, judge_model="kwarg-model"
        )
        grader = RubricGrader(assertions=["a"], name="r0")
        _run_sync(invoker("prompt", grader, "/tmp/wt"))

        assert captured["options"].model == "kwarg-model"

    def test_default_invoker_model_precedence_none_when_both_absent(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured: dict[str, Any] = {}

        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            captured["options"] = options
            return _stream(
                _assistant_with(
                    _wrap('{"passed": true, "summary": "ok"}')
                )
            )

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_judge_invoke(
            judge_max_turns=4, judge_model=None
        )
        grader = RubricGrader(assertions=["a"], name="r0")
        _run_sync(invoker("prompt", grader, "/tmp/wt"))

        assert captured["options"].model is None

    def test_default_invoker_concatenates_assistant_text_blocks(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def fake_query(
            *, prompt: str, options: ClaudeAgentOptions
        ) -> AsyncIterator[Message]:
            return _stream(
                AssistantMessage(
                    content=[TextBlock(text="part-A "), TextBlock(text="part-B")],
                    model="m",
                    stop_reason="end_turn",
                    session_id="s",
                ),
                AssistantMessage(
                    content=[TextBlock(text=" tail")],
                    model="m",
                    stop_reason="end_turn",
                    session_id="s",
                ),
            )

        monkeypatch.setattr(
            "flywheel_core._sdk.query", fake_query
        )

        invoker = _make_default_judge_invoke(
            judge_max_turns=2, judge_model=None
        )
        grader = RubricGrader(assertions=["a"], name="r0")
        response = _run_sync(invoker("prompt", grader, "/tmp/wt"))
        assert response == "part-A part-B tail"
