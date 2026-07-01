"""Contract tests for the Claude invoker.

The invoker is a thin adapter over ``claude-agent-sdk`` — these tests
inject a fake message stream rather than spawning a live subprocess, so
they exercise the invoker's return shape and signal-extraction behavior
without depending on the CLI being installed or authenticated.
"""

import asyncio
import json
import sqlite3
from collections.abc import AsyncIterator
from typing import Any

import psycopg
import psycopg_pool
import pytest

from claude_agent_sdk import (
    AssistantMessage,
    HookEventMessage,
    Message,
    ProcessError,
    RateLimitEvent,
    RateLimitInfo,
    ResultMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

from flywheel_core.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    Intent,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
)
from flywheel_core.faults import (
    BackoffPolicy,
    FaultClass,
    classify_fault,
    wait_backoff,
)
from flywheel_core.invoker import (
    InvocationSignals,
    IterationResult,
    ToolInteraction,
    ToolResultObservation,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel_core.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    OptimisticConcurrencyError,
    StoreSchemaError,
)
from flywheel_orchestrator._claims import OrchestratorSchemaError


def _wrap_envelope(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


async def _stream(*items: Message) -> AsyncIterator[Message]:
    for item in items:
        yield item


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


def _result(
    *,
    is_error: bool = False,
    num_turns: int = 1,
    total_cost_usd: float | None = 0.01,
    stop_reason: str | None = "end_turn",
    permission_denials: list[Any] | None = None,
    api_error_status: int | None = None,
    result_text: str | None = None,
    subtype: str = "success",
    session_id: str = "sess-1",
) -> ResultMessage:
    return ResultMessage(
        subtype=subtype,
        duration_ms=10,
        duration_api_ms=8,
        is_error=is_error,
        num_turns=num_turns,
        session_id=session_id,
        stop_reason=stop_reason,
        total_cost_usd=total_cost_usd,
        permission_denials=permission_denials,
        api_error_status=api_error_status,
        result=result_text,
    )


def _assistant(
    *blocks: TextBlock | ToolUseBlock | ThinkingBlock,
    stop_reason: str | None = "end_turn",
    session_id: str = "sess-1",
    model: str = "claude-test",
) -> AssistantMessage:
    return AssistantMessage(
        content=list(blocks),
        model=model,
        stop_reason=stop_reason,
        session_id=session_id,
    )


def _user_with_tool_result(
    tool_use_id: str,
    *,
    is_error: bool | None = False,
    content: str | list[dict[str, Any]] | None = "ok",
) -> UserMessage:
    block = ToolResultBlock(
        tool_use_id=tool_use_id,
        content=content,
        is_error=is_error,
    )
    return UserMessage(content=[block])


class TestValidEnvelopeAndSignals:
    def test_valid_envelope_and_basic_signals_surface(self) -> None:
        transcript = _wrap_envelope('{"intent": "verify", "reason": "ready"}')
        stream = _stream(
            _assistant(TextBlock(text=transcript)),
            _result(num_turns=3, total_cost_usd=0.12),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )

        assert isinstance(result, IterationResult)
        assert isinstance(result.envelope, ValidEnvelope)
        assert result.envelope.intent is Intent.VERIFY
        assert result.transcript == transcript
        assert result.failure is None
        assert result.signals.num_turns == 3
        assert result.signals.total_cost_usd == 0.12
        assert result.signals.result_is_error is False
        assert result.signals.stop_reason == "end_turn"
        assert result.signals.session_id == "sess-1"
        assert result.signals.pending_tool_use_at_stop is False

    def test_transcript_falls_back_to_result_text_when_no_text_blocks(
        self,
    ) -> None:
        transcript = _wrap_envelope('{"intent": "continue"}')
        stream = _stream(_result(result_text=transcript))
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert result.transcript == transcript
        assert isinstance(result.envelope, ValidEnvelope)
        assert result.envelope.intent is Intent.CONTINUE


class TestEnvelopeOutcomes:
    def test_missing_envelope_when_no_text_emitted(self) -> None:
        stream = _stream(_result())
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert isinstance(result.envelope, MissingEnvelope)
        assert result.transcript == ""

    def test_truncated_envelope_surfaced(self) -> None:
        transcript = f'{OPENING_FENCE}\n{{"intent": "continue"'
        stream = _stream(
            _assistant(TextBlock(text=transcript)),
            _result(),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert isinstance(result.envelope, TruncatedEnvelope)


class TestToolInteractions:
    def test_tool_use_paired_with_tool_result(self) -> None:
        transcript = _wrap_envelope('{"intent": "verify"}')
        tool_use = ToolUseBlock(
            id="tu-1", name="Bash", input={"command": "ls"}
        )
        stream = _stream(
            _assistant(tool_use, stop_reason="tool_use"),
            _user_with_tool_result("tu-1", is_error=False, content="ok"),
            _assistant(TextBlock(text=transcript)),
            _result(num_turns=2),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )

        assert result.signals.tool_interactions == (
            ToolInteraction(
                tool_use_id="tu-1",
                tool_name="Bash",
                tool_input={"command": "ls"},
                result=ToolResultObservation(
                    tool_use_id="tu-1", is_error=False, content="ok"
                ),
            ),
        )
        assert len(result.signals.tool_result_blocks) == 1
        assert result.signals.tool_result_blocks[0].is_error is False
        assert result.signals.pending_tool_use_at_stop is False

    def test_pending_tool_use_when_no_matching_result(self) -> None:
        tool_use = ToolUseBlock(
            id="tu-orphan", name="Bash", input={"command": "noop"}
        )
        stream = _stream(
            _assistant(tool_use, stop_reason="end_turn"),
            _result(stop_reason="end_turn"),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert result.signals.pending_tool_use_at_stop is True
        interaction = result.signals.tool_interactions[0]
        assert interaction.tool_use_id == "tu-orphan"
        assert interaction.result is None

    def test_tool_result_is_error_repeats_observable(self) -> None:
        stream = _stream(
            _assistant(
                ToolUseBlock(id="tu-1", name="Bash", input={"command": "x"}),
                stop_reason="tool_use",
            ),
            _user_with_tool_result("tu-1", is_error=True, content="boom"),
            _assistant(
                ToolUseBlock(id="tu-2", name="Bash", input={"command": "x"}),
                stop_reason="tool_use",
            ),
            _user_with_tool_result("tu-2", is_error=True, content="boom"),
            _result(num_turns=4),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        errored = [
            obs for obs in result.signals.tool_result_blocks if obs.is_error
        ]
        assert len(errored) == 2


class TestRateLimitSignals:
    def test_rate_limit_event_surfaced_without_internal_retry(self) -> None:
        info = RateLimitInfo(
            status="allowed_warning",
            resets_at=1_700_000_000,
            rate_limit_type="five_hour",
            utilization=0.92,
        )
        event = RateLimitEvent(
            rate_limit_info=info, uuid="rl-1", session_id="sess-1"
        )
        transcript = _wrap_envelope('{"intent": "continue"}')
        stream = _stream(
            event,
            _assistant(TextBlock(text=transcript)),
            _result(),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert result.signals.rate_limit_events == (event,)
        assert (
            result.signals.rate_limit_events[0].rate_limit_info.resets_at
            == 1_700_000_000
        )
        assert result.failure is None


class TestPermissionDenials:
    def test_permission_denials_surfaced_as_structured_signal(self) -> None:
        denials = [
            {"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}},
            {"tool_name": "WebFetch", "tool_input": {"url": "http://x"}},
        ]
        stream = _stream(
            _assistant(TextBlock(text=_wrap_envelope('{"intent": "abort"}'))),
            _result(permission_denials=denials),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        assert result.signals.permission_denials == tuple(denials)


class TestFailureSurfacing:
    def test_process_error_surfaces_exit_code_and_stderr(self) -> None:
        async def stream() -> AsyncIterator[Message]:
            yield _assistant(TextBlock(text="partial output"))
            raise ProcessError(
                "claude exited non-zero", exit_code=137, stderr="oom"
            )

        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream())
        )
        assert result.failure is not None
        assert result.failure.error_type == "ProcessError"
        assert result.failure.exit_code == 137
        assert result.failure.stderr == "oom"
        assert isinstance(result.envelope, MissingEnvelope)
        assert result.transcript == "partial output"

    def test_non_zero_exit_with_partial_envelope_keeps_parser_verdict(
        self,
    ) -> None:
        partial = f'{OPENING_FENCE}\n{{"intent": "continue"'

        async def stream() -> AsyncIterator[Message]:
            yield _assistant(TextBlock(text=partial))
            raise ProcessError("died mid-envelope", exit_code=1)

        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream())
        )
        assert isinstance(result.envelope, TruncatedEnvelope)
        assert result.failure is not None
        assert result.failure.exit_code == 1

    def test_generic_iterator_error_surfaced_without_classification(
        self,
    ) -> None:
        async def stream() -> AsyncIterator[Message]:
            yield _assistant(TextBlock(text="before crash"))
            raise RuntimeError("transport died")

        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream())
        )
        assert result.failure is not None
        assert result.failure.error_type == "RuntimeError"
        assert result.failure.message == "transport died"
        assert result.failure.exit_code is None
        assert result.failure.stderr is None


class TestSdkSignalCoverage:
    """Every SDK surface in the docs/loop.md detection map must be reachable."""

    def test_all_detection_map_signals_surface_in_one_iteration(self) -> None:
        rate_event = RateLimitEvent(
            rate_limit_info=RateLimitInfo(status="allowed"),
            uuid="rl-1",
            session_id="sess-1",
        )
        denials: list[Any] = [{"tool_name": "Bash"}]
        transcript = _wrap_envelope('{"intent": "verify"}')

        stream = _stream(
            rate_event,
            _assistant(
                ToolUseBlock(id="tu-1", name="Read", input={"path": "a"}),
                stop_reason="tool_use",
            ),
            _user_with_tool_result("tu-1", is_error=False, content="ok"),
            _assistant(TextBlock(text=transcript), stop_reason="end_turn"),
            _result(
                num_turns=2,
                total_cost_usd=0.05,
                permission_denials=denials,
                api_error_status=None,
            ),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=stream)
        )
        signals = result.signals

        assert isinstance(signals, InvocationSignals)
        # StopReason
        assert signals.stop_reason == "end_turn"
        # PostToolUse-equivalent (paired tool use + result)
        assert signals.tool_interactions
        assert signals.tool_interactions[0].tool_name == "Read"
        # ToolResultBlock.IsError
        assert signals.tool_result_blocks
        assert signals.tool_result_blocks[0].is_error is False
        # ResultMessage.IsError
        assert signals.result_is_error is False
        # NumTurns
        assert signals.num_turns == 2
        # TotalCostUSD
        assert signals.total_cost_usd == 0.05
        # RateLimitEvent
        assert signals.rate_limit_events == (rate_event,)
        # PermissionDenials
        assert signals.permission_denials == tuple(denials)


class TestInvokerScope:
    def test_invoker_does_not_import_retry_lifecycle_or_grader_logic(self) -> None:
        import ast
        import inspect

        import flywheel_core.invoker as invoker_module

        source_path = inspect.getsourcefile(invoker_module)
        assert source_path is not None
        with open(source_path, "r", encoding="utf-8") as fh:
            source = fh.read()

        tree = ast.parse(source)
        imported: set[str] = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.module:
                imported.add(node.module)

        # The invoker is a thin adapter — it must not reach into lifecycle
        # transitions, persistence, prompt construction, grader runners, or
        # task definitions. Those concerns are deferred to later roadmap
        # items.
        forbidden = {
            "flywheel_core.lifecycle",
            "flywheel_core.loaders",
            "flywheel_core.store_memory",
            "flywheel_core.store_sqlite",
            "flywheel_core.store_protocols",
            "flywheel_core.task",
        }
        leaked = imported & forbidden
        assert not leaked, f"invoker leaked imports: {leaked}"

    def test_invoker_delegates_envelope_parsing_to_parser_module(self) -> None:
        import ast
        import inspect

        import flywheel_core.invoker as invoker_module

        source_path = inspect.getsourcefile(invoker_module)
        assert source_path is not None
        with open(source_path, "r", encoding="utf-8") as fh:
            source = fh.read()

        tree = ast.parse(source)
        delegates = False
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.module == "flywheel_core.envelope":
                for alias in node.names:
                    if alias.name == "parse_envelope":
                        delegates = True
        assert delegates, "invoker must import parse_envelope from flywheel_core.envelope"


# --- SDK message capture --------------------------------------------------


class TestMessageCapturePreservesOrder:
    """``IterationResult.messages`` must reproduce the input stream order
    so the harness can persist a verbatim audit trail.
    """

    def test_iteration_result_messages_preserve_input_order(self) -> None:
        rate_event = RateLimitEvent(
            rate_limit_info=RateLimitInfo(status="allowed"),
            uuid="rl-1",
            session_id="sess-1",
        )
        hook_event = HookEventMessage(
            subtype="hook_started",
            data={"hook_event": "PreToolUse", "raw": "x"},
            hook_event_name="PreToolUse",
            session_id="sess-1",
            uuid="hk-1",
        )
        transcript = _wrap_envelope('{"intent": "verify"}')
        ordered: tuple[Message, ...] = (
            rate_event,
            _assistant(TextBlock(text="hello "), stop_reason="tool_use"),
            _user_with_tool_result("tu-1", content="ok"),
            hook_event,
            _assistant(TextBlock(text=transcript), stop_reason="end_turn"),
            _result(num_turns=2),
        )
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=_stream(*ordered))
        )
        assert result.messages == ordered


class TestSerializeSdkMessage:
    """``_serialize_sdk_message`` produces a JSON-roundtrippable dict that
    preserves every field of the SDK message and is tagged with
    ``message_type``.
    """

    def test_serializes_assistant_message_with_nested_blocks(self) -> None:
        msg = _assistant(
            TextBlock(text="hi"),
            ToolUseBlock(id="tu-1", name="Bash", input={"command": "ls"}),
        )
        payload = _serialize_sdk_message(msg)
        assert payload["message_type"] == "AssistantMessage"
        assert payload["model"] == "claude-test"
        assert payload["session_id"] == "sess-1"
        assert payload["stop_reason"] == "end_turn"
        # Nested blocks come out as plain dicts.
        assert payload["content"] == [
            {"text": "hi"},
            {"id": "tu-1", "name": "Bash", "input": {"command": "ls"}},
        ]
        # JSON-roundtrippable.
        assert json.loads(json.dumps(payload)) == payload

    def test_serializes_user_message_with_tool_result_block(self) -> None:
        msg = _user_with_tool_result(
            "tu-1", is_error=False, content="ok"
        )
        payload = _serialize_sdk_message(msg)
        assert payload["message_type"] == "UserMessage"
        assert payload["content"] == [
            {"tool_use_id": "tu-1", "content": "ok", "is_error": False}
        ]
        assert json.loads(json.dumps(payload)) == payload

    def test_serializes_result_message_with_full_body(self) -> None:
        msg = _result(num_turns=3, total_cost_usd=0.42)
        payload = _serialize_sdk_message(msg)
        assert payload["message_type"] == "ResultMessage"
        assert payload["subtype"] == "success"
        assert payload["num_turns"] == 3
        assert payload["total_cost_usd"] == 0.42
        assert payload["is_error"] is False
        assert json.loads(json.dumps(payload)) == payload

    def test_serializes_rate_limit_event_recursively(self) -> None:
        info = RateLimitInfo(
            status="allowed_warning",
            resets_at=1_700_000_000,
            rate_limit_type="five_hour",
            utilization=0.92,
        )
        event = RateLimitEvent(
            rate_limit_info=info, uuid="rl-1", session_id="sess-1"
        )
        payload = _serialize_sdk_message(event)
        assert payload["message_type"] == "RateLimitEvent"
        assert payload["uuid"] == "rl-1"
        assert payload["rate_limit_info"]["status"] == "allowed_warning"
        assert payload["rate_limit_info"]["utilization"] == 0.92
        assert payload["rate_limit_info"]["resets_at"] == 1_700_000_000
        assert json.loads(json.dumps(payload)) == payload

    def test_serializes_hook_event_message(self) -> None:
        msg = HookEventMessage(
            subtype="hook_response",
            data={"output": "ok", "exit_code": 0},
            hook_event_name="PostToolUse",
            session_id="sess-1",
            uuid="hk-1",
        )
        payload = _serialize_sdk_message(msg)
        assert payload["message_type"] == "HookEventMessage"
        assert payload["subtype"] == "hook_response"
        assert payload["data"] == {"output": "ok", "exit_code": 0}
        assert payload["hook_event_name"] == "PostToolUse"
        assert json.loads(json.dumps(payload)) == payload

    def test_serializer_never_truncates_large_tool_result_content(
        self,
    ) -> None:
        """Multi-MB tool outputs are persisted verbatim per spec."""
        big = "x" * (2 * 1024 * 1024)  # 2 MB string
        msg = _user_with_tool_result("tu-big", content=big)
        payload = _serialize_sdk_message(msg)
        assert payload["content"][0]["content"] == big
        # Size sanity: serialized text contains the full body.
        assert len(json.dumps(payload)) >= len(big)


class TestOnMessageObserver:
    """The ``on_message`` seam: pure, ordered, per-message observation."""

    def test_fires_once_per_message_in_arrival_order(self) -> None:
        a = _assistant(TextBlock(text="working"))
        u = _user_with_tool_result("t1", content="done")
        r = _result()
        stream = _stream(a, u, r)
        seen: list[Message] = []
        result = _run(
            invoke_iteration(
                prompt="ignored",
                message_stream=stream,
                on_message=seen.append,
            )
        )
        # Observed exactly the messages, in order, matching the result.
        assert seen == [a, u, r]
        assert result.messages == (a, u, r)

    def test_raising_observer_does_not_break_the_run(self) -> None:
        a = _assistant(TextBlock(text=_wrap_envelope('{"intent": "verify"}')))
        r = _result()
        stream = _stream(a, r)

        def boom(_msg: Message) -> None:
            raise RuntimeError("renderer blew up")

        # A faulty observer is swallowed; the iteration still completes and
        # every message is recorded in the result.
        result = _run(
            invoke_iteration(
                prompt="ignored", message_stream=stream, on_message=boom
            )
        )
        assert result.messages == (a, r)
        assert result.failure is None

    def test_absent_observer_is_a_no_op(self) -> None:
        a = _assistant(TextBlock(text="hi"))
        result = _run(
            invoke_iteration(prompt="ignored", message_stream=_stream(a))
        )
        assert result.messages == (a,)


# --- Fault classification + bounded backoff --------------------------------


_CLASSIFICATION_MATRIX: list[tuple[object, FaultClass | None]] = [
    # rate-limit / overload surfaced as an HTTP-ish status -> TRANSIENT
    (429, FaultClass.TRANSIENT),
    (529, FaultClass.TRANSIENT),
    # SQLite still busy after its busy_timeout elapsed -> TRANSIENT
    (sqlite3.OperationalError("database is locked"), FaultClass.TRANSIENT),
    # dropped Postgres connection -> TRANSIENT
    (
        psycopg.OperationalError("server closed the connection unexpectedly"),
        FaultClass.TRANSIENT,
    ),
    # Postgres pool checkout timeout -> TRANSIENT
    (
        psycopg_pool.PoolTimeout("couldn't get a connection after 30.0 sec"),
        FaultClass.TRANSIENT,
    ),
    # schema-version mismatch (core store) -> PERMANENT
    (
        StoreSchemaError(
            observed_version=1, expected_version=CURRENT_SCHEMA_VERSION
        ),
        FaultClass.PERMANENT,
    ),
    # schema-version mismatch (orchestrator store) -> PERMANENT
    (OrchestratorSchemaError(observed=1, expected=5), FaultClass.PERMANENT),
    # optimistic-concurrency conflict is NOT transient (harness loop-retries
    # it) -> classified out of scope, never TRANSIENT
    (
        OptimisticConcurrencyError(
            "run-1", expected_version=3, actual_version=4
        ),
        None,
    ),
]


class TestTransientClassificationMatrix:
    @pytest.mark.parametrize(
        "fault, expected",
        _CLASSIFICATION_MATRIX,
        ids=[type(f).__name__ if not isinstance(f, int) else str(f) for f, _ in _CLASSIFICATION_MATRIX],
    )
    def test_transient_classification_matrix(
        self, fault: object, expected: FaultClass | None
    ) -> None:
        assert classify_fault(fault) is expected


class TestBackoffBounded:
    def test_backoff_bounded(self) -> None:
        policy = BackoffPolicy(base_seconds=0.5, factor=2.0, cap_seconds=8.0)
        recorded: list[float] = []
        delays = [
            wait_backoff(attempt, policy=policy, sleep=recorded.append)
            for attempt in range(10)
        ]

        # The injected sleep captures every wait deterministically, in order.
        assert recorded == delays

        # Monotonic non-decreasing: each wait is >= the one before it.
        assert all(b >= a for a, b in zip(delays, delays[1:]))

        # It actually grows -- a constant delay (sleep(0) "backoff") would
        # produce no strictly-increasing step and fail this assertion.
        assert any(b > a for a, b in zip(delays, delays[1:]))
        assert delays[1] > delays[0]

        # Every wait is bounded by the configured cap.
        assert all(d <= policy.cap_seconds for d in delays)
        # The cap is reached and then held (the schedule is genuinely capped).
        assert delays[-1] == policy.cap_seconds

        # Guard the assertions above by showing what they reject: an uncapped
        # exponential exceeds the cap, and a constant delay never grows.
        uncapped = [
            policy.base_seconds * policy.factor**i for i in range(10)
        ]
        assert any(u > policy.cap_seconds for u in uncapped)
        constant = [1.0] * 10
        assert not any(b > a for a, b in zip(constant, constant[1:]))
