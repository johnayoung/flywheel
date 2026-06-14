"""Thin adapter over ``claude-agent-sdk`` that drives one agent iteration.

The invoker owns nothing else. It does not enforce retry policy, mutate
lifecycle state, run graders, or construct prompts — each of those belongs
to a separate roadmap item. Its single responsibility is to turn one call
into one iteration and surface every SDK signal enumerated in
``docs/loop.md``'s state-detection map so the harness never has to re-parse
raw messages.

Envelope extraction delegates entirely to :func:`flywheel_core.envelope.parse_envelope`.
"""

from __future__ import annotations

import dataclasses
from collections.abc import AsyncIterable, Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from flywheel_core.envelope import EnvelopeResult, parse_envelope

if TYPE_CHECKING:
    from claude_agent_sdk import (
        ClaudeAgentOptions,
        HookEventMessage,
        Message,
        RateLimitEvent,
        ToolUseBlock,
    )


@dataclass(frozen=True, kw_only=True)
class ToolResultObservation:
    """One ``ToolResultBlock`` observed in the stream.

    Surfaced raw because the ``blocked_implicit`` counter in
    ``docs/loop.md`` keys on ``ToolResultBlock.is_error == True`` repeats.
    """

    tool_use_id: str
    is_error: bool | None
    content: str | list[dict[str, Any]] | None


@dataclass(frozen=True, kw_only=True)
class ToolInteraction:
    """A ``ToolUseBlock`` paired with its matching ``ToolResultBlock``.

    The harness keys ``blocked_implicit`` on ``(tool_name, sha256(input))``
    and feeds thrash detection the same ``(tool, input)`` tuple — both need
    name + input + outcome together, which this dataclass surfaces directly
    without depending on the SDK's optional hook event stream.
    """

    tool_use_id: str
    tool_name: str
    tool_input: dict[str, Any]
    result: ToolResultObservation | None


@dataclass(frozen=True, kw_only=True)
class InvocationFailure:
    """Iterator / subprocess failure captured without classification.

    The harness — not the invoker — decides whether a given failure is
    infrastructure, task, or protocol. The invoker surfaces the raw
    evidence (exception type, message, exit code, stderr).
    """

    error_type: str
    message: str
    exit_code: int | None = None
    stderr: str | None = None


@dataclass(frozen=True, kw_only=True)
class InvocationSignals:
    """Structured projection of one iteration's SDK signals.

    Every surface listed in the ``docs/loop.md`` detection map is reachable
    from here:

    - ``StopReason`` -> :attr:`stop_reason`
    - ``PostToolUse`` -> :attr:`tool_interactions` (and :attr:`hook_events`
      when the caller enabled ``include_hook_events`` on the SDK options)
    - ``ToolResultBlock.is_error`` -> :attr:`tool_result_blocks`
    - ``ResultMessage.is_error`` -> :attr:`result_is_error`
    - ``NumTurns`` -> :attr:`num_turns`
    - ``TotalCostUSD`` -> :attr:`total_cost_usd`
    - ``RateLimitEvent`` -> :attr:`rate_limit_events`
    - ``ResultMessage.permission_denials`` -> :attr:`permission_denials`
    """

    stop_reason: str | None
    num_turns: int | None
    total_cost_usd: float | None
    result_is_error: bool | None
    result_subtype: str | None
    api_error_status: int | None
    session_id: str | None
    permission_denials: tuple[Any, ...] = field(default_factory=tuple)
    rate_limit_events: tuple[RateLimitEvent, ...] = field(default_factory=tuple)
    tool_interactions: tuple[ToolInteraction, ...] = field(default_factory=tuple)
    tool_result_blocks: tuple[ToolResultObservation, ...] = field(
        default_factory=tuple
    )
    hook_events: tuple[HookEventMessage, ...] = field(default_factory=tuple)
    pending_tool_use_at_stop: bool = False


@dataclass(frozen=True, kw_only=True)
class IterationResult:
    """One iteration's transcript, envelope verdict, and structured signals.

    ``failure`` is set when the SDK raised — non-zero exit, iterator error,
    cancellation propagated as a regular exception. Even on failure the
    other fields reflect whatever was observed before the failure,
    including a ``parse_envelope`` verdict (typically
    :class:`~flywheel_core.envelope.MissingEnvelope` or
    :class:`~flywheel_core.envelope.TruncatedEnvelope`).
    """

    transcript: str
    messages: tuple[Message, ...]
    envelope: EnvelopeResult
    signals: InvocationSignals
    failure: InvocationFailure | None = None


async def invoke_iteration(
    *,
    prompt: str,
    options: ClaudeAgentOptions | None = None,
    message_stream: AsyncIterable[Message] | None = None,
    on_message: Callable[[Message], None] | None = None,
) -> IterationResult:
    """Drive exactly one agent iteration and return its structured result.

    ``message_stream`` is the seam tests use to exercise the contract
    without spawning a Claude subprocess — supply an ``AsyncIterable`` of
    pre-built SDK messages and the invoker consumes it directly. When
    omitted, the invoker delegates to :func:`claude_agent_sdk.query`.

    ``on_message`` is a pure observation seam: when set, it is called with
    each SDK :class:`Message` the instant it arrives, before the message is
    processed. It lets a caller surface the agent's turns live (e.g. the
    workflow CLI streaming them to stdout interleaved with harness events)
    without the invoker taking on any rendering or persistence concern. A
    raising observer is swallowed so a faulty live renderer can never break
    the agent run; the message is still recorded in the returned result.

    The invoker drives exactly one iteration per call. Multi-iteration
    orchestration belongs to the harness.
    """

    # Agent-driving path: import the SDK lazily so importing this module
    # (and flywheel_core) never requires the optional extra.
    from flywheel_core._sdk import (
        AssistantMessage,
        ClaudeSDKError,
        HookEventMessage,
        ProcessError,
        RateLimitEvent,
        ResultMessage,
        TextBlock,
        ToolResultBlock,
        ToolUseBlock,
        UserMessage,
    )
    from flywheel_core._sdk import query as _sdk_query

    messages: list[Message] = []
    transcript_chunks: list[str] = []
    result_text: str | None = None

    tool_uses: dict[str, ToolUseBlock] = {}
    tool_use_order: list[str] = []
    tool_results: dict[str, ToolResultObservation] = {}
    tool_result_observations: list[ToolResultObservation] = []
    rate_limit_events: list[RateLimitEvent] = []
    hook_events: list[HookEventMessage] = []

    last_assistant_stop_reason: str | None = None
    result_stop_reason: str | None = None
    num_turns: int | None = None
    total_cost_usd: float | None = None
    result_is_error: bool | None = None
    result_subtype: str | None = None
    api_error_status: int | None = None
    session_id: str | None = None
    permission_denials: list[Any] = []

    failure: InvocationFailure | None = None

    source: AsyncIterable[Message]
    if message_stream is not None:
        source = message_stream
    else:
        source = _sdk_query(prompt=prompt, options=options)

    try:
        async for msg in source:
            messages.append(msg)

            if on_message is not None:
                try:
                    on_message(msg)
                except Exception:  # noqa: BLE001 - a live renderer must
                    # never break the agent run; observation is best-effort.
                    pass

            if isinstance(msg, AssistantMessage):
                if msg.session_id is not None:
                    session_id = msg.session_id
                if msg.stop_reason is not None:
                    last_assistant_stop_reason = msg.stop_reason
                for block in msg.content:
                    if isinstance(block, TextBlock):
                        transcript_chunks.append(block.text)
                    elif isinstance(block, ToolUseBlock):
                        if block.id not in tool_uses:
                            tool_use_order.append(block.id)
                        tool_uses[block.id] = block

            elif isinstance(msg, UserMessage):
                if isinstance(msg.content, list):
                    for block in msg.content:
                        if isinstance(block, ToolResultBlock):
                            observation = ToolResultObservation(
                                tool_use_id=block.tool_use_id,
                                is_error=block.is_error,
                                content=block.content,
                            )
                            tool_result_observations.append(observation)
                            tool_results[block.tool_use_id] = observation

            elif isinstance(msg, ResultMessage):
                if msg.session_id:
                    session_id = msg.session_id
                num_turns = msg.num_turns
                total_cost_usd = msg.total_cost_usd
                result_is_error = msg.is_error
                result_subtype = msg.subtype
                api_error_status = msg.api_error_status
                if msg.stop_reason is not None:
                    result_stop_reason = msg.stop_reason
                if msg.permission_denials:
                    permission_denials.extend(msg.permission_denials)
                if msg.result and not transcript_chunks:
                    result_text = msg.result

            elif isinstance(msg, RateLimitEvent):
                rate_limit_events.append(msg)
                if msg.session_id:
                    session_id = msg.session_id

            elif isinstance(msg, HookEventMessage):
                hook_events.append(msg)
                if msg.session_id:
                    session_id = msg.session_id

    except ProcessError as exc:
        failure = InvocationFailure(
            error_type=type(exc).__name__,
            message=str(exc),
            exit_code=exc.exit_code,
            stderr=exc.stderr,
        )
    except ClaudeSDKError as exc:
        failure = InvocationFailure(
            error_type=type(exc).__name__,
            message=str(exc),
        )
    except Exception as exc:
        failure = InvocationFailure(
            error_type=type(exc).__name__,
            message=str(exc),
        )

    if transcript_chunks:
        transcript = "".join(transcript_chunks)
    else:
        transcript = result_text or ""

    envelope = parse_envelope(transcript)

    tool_interactions: list[ToolInteraction] = []
    for tool_use_id in tool_use_order:
        block = tool_uses[tool_use_id]
        tool_interactions.append(
            ToolInteraction(
                tool_use_id=tool_use_id,
                tool_name=block.name,
                tool_input=block.input,
                result=tool_results.get(tool_use_id),
            )
        )

    pending_tool_use_at_stop = any(
        tool_use_id not in tool_results for tool_use_id in tool_use_order
    )

    signals = InvocationSignals(
        stop_reason=last_assistant_stop_reason or result_stop_reason,
        num_turns=num_turns,
        total_cost_usd=total_cost_usd,
        result_is_error=result_is_error,
        result_subtype=result_subtype,
        api_error_status=api_error_status,
        session_id=session_id,
        permission_denials=tuple(permission_denials),
        rate_limit_events=tuple(rate_limit_events),
        tool_interactions=tuple(tool_interactions),
        tool_result_blocks=tuple(tool_result_observations),
        hook_events=tuple(hook_events),
        pending_tool_use_at_stop=pending_tool_use_at_stop,
    )

    return IterationResult(
        transcript=transcript,
        messages=tuple(messages),
        envelope=envelope,
        signals=signals,
        failure=failure,
    )


def _to_jsonable(value: Any) -> Any:
    """Recursively convert ``value`` to JSON-compatible primitives.

    Handles dataclasses (via :func:`dataclasses.fields`), lists/tuples,
    dicts, and primitives. For SDK objects that are neither dataclasses
    nor primitives, falls back to ``vars(value)`` so new fields appear
    in the payload automatically without code changes; if even that
    fails (no ``__dict__``), falls back to ``repr``. Never truncates.
    """
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {
            f.name: _to_jsonable(getattr(value, f.name))
            for f in dataclasses.fields(value)
        }
    if isinstance(value, dict):
        return {str(k): _to_jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_to_jsonable(v) for v in value]
    try:
        return {str(k): _to_jsonable(v) for k, v in vars(value).items()}
    except TypeError:
        return repr(value)


def _serialize_sdk_message(msg: Message) -> dict[str, Any]:
    """Serialize one ``claude-agent-sdk`` :class:`Message` to a
    JSON-compatible dict.

    The result includes a top-level ``message_type`` key (the SDK
    class name like ``"AssistantMessage"``) plus every field of the
    message body. Nested content blocks (:class:`TextBlock`,
    :class:`ToolUseBlock`, :class:`ToolResultBlock`, etc.) are
    serialized recursively as plain dicts via :func:`_to_jsonable`.

    Used by :mod:`flywheel_core.harness` to feed each streamed message
    to the run's telemetry sink. Lives here because every SDK
    :class:`Message` subtype is already imported in this module. The
    harness re-exports nothing — it imports this helper directly.
    """
    body = _to_jsonable(msg)
    if not isinstance(body, dict):
        body = {"value": body}
    body["message_type"] = type(msg).__name__
    return body


__all__ = [
    "InvocationFailure",
    "InvocationSignals",
    "IterationResult",
    "ToolInteraction",
    "ToolResultObservation",
    "invoke_iteration",
]
