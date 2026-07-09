"""Contract tests for :mod:`flywheel_core.invoker_client`.

The client-based invoker is the production path for the workflow CLI:
it opens a :class:`claude_agent_sdk.ClaudeSDKClient`, runs a watcher
coroutine that claims operator-issued control commands from a
:class:`ControlCommandStore`, and applies them live against the open
session. These tests inject a fake client + the in-memory store so the
contract surfaces (claim-once, audit-event emission, best-effort apply,
store-triggered cancellation) are exercised without a live SDK
subprocess.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from datetime import datetime, timezone
from typing import Any, cast

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ClaudeSDKClient,
    Message,
    ResultMessage,
    TextBlock,
)

from flywheel_core.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    Intent,
    MissingEnvelope,
    ValidEnvelope,
)
from flywheel_core.deadline import DeadlineExceeded, run_with_deadline
from flywheel_core.invoker_client import (
    CHECKPOINT_NUDGE_PROMPT,
    CONTROL_COMMAND_APPROVE,
    CONTROL_COMMAND_INTERRUPT,
    CONTROL_COMMAND_REJECT,
    CONTROL_COMMAND_SAY,
    CONTROL_COMMAND_SET_MODEL,
    ENVELOPE_SALVAGE_PROMPT,
    EVENT_CHECKPOINT_NUDGE,
    EVENT_CONTROL_APPLIED,
    EVENT_CONTROL_CLAIM_FAILED,
    EVENT_CONTROL_FAILED,
    EVENT_CONTROL_NOT_APPLICABLE,
    HarnessRecoveryRequested,
    invoke_iteration_with_client,
)
from flywheel_core.store_memory import InMemoryStore


def _wrap_envelope(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


def _assistant_text(text: str) -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason="end_turn",
        session_id="sess-1",
    )


def _result(*, total_cost_usd: float | None = 0.01) -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=1,
        duration_api_ms=1,
        is_error=False,
        num_turns=1,
        session_id="sess-1",
        stop_reason="end_turn",
        total_cost_usd=total_cost_usd,
    )


class _FakeClient:
    """In-process stand-in for :class:`ClaudeSDKClient`.

    Exposes only the surface the watcher depends on so the contract tests
    do not require a CLI subprocess. ``messages`` is the message stream
    handed to ``invoke_iteration`` via :meth:`receive_response`; control
    methods record their calls so tests can assert on dispatch order.

    ``messages`` is consumed lazily — when ``hold_until_event`` is set,
    the stream waits on the event before yielding the next message so a
    test can deterministically interleave a watcher tick with the
    stream.
    """

    def __init__(
        self,
        messages: list[Message],
        *,
        hold_until_event: asyncio.Event | None = None,
        interrupt_should_raise: BaseException | None = None,
        set_model_should_raise: BaseException | None = None,
        say_should_raise: BaseException | None = None,
    ) -> None:
        self._messages = list(messages)
        self.options: ClaudeAgentOptions | None = None
        self.initial_prompt: str | None = None
        self.injected: list[str] = []
        self.set_models: list[str | None] = []
        self.interrupt_calls = 0
        self._hold = hold_until_event
        self._interrupt_raises = interrupt_should_raise
        self._set_model_raises = set_model_should_raise
        self._say_raises = say_should_raise
        self.entered = False
        self.exited = False

    async def __aenter__(self) -> "_FakeClient":
        self.entered = True
        return self

    async def __aexit__(self, *exc: object) -> bool:
        self.exited = True
        return False

    async def query(self, prompt: str, session_id: str = "default") -> None:
        if self.initial_prompt is None:
            self.initial_prompt = prompt
        else:
            self.injected.append(prompt)

    async def interrupt(self) -> None:
        self.interrupt_calls += 1
        if self._interrupt_raises is not None:
            raise self._interrupt_raises

    async def set_model(self, model: str | None = None) -> None:
        if self._set_model_raises is not None:
            raise self._set_model_raises
        self.set_models.append(model)

    async def receive_response(self) -> AsyncIterator[Message]:
        for msg in self._messages:
            if self._hold is not None and not self._hold.is_set():
                await self._hold.wait()
            yield msg


def _factory(client: _FakeClient) -> Any:
    def _make(_options: ClaudeAgentOptions) -> ClaudeSDKClient:
        # The fake satisfies the structural surface the invoker exercises;
        # cast quiets the type-checker because Python's structural-typing
        # doesn't extend to non-Protocol third-party classes.
        return cast(ClaudeSDKClient, client)

    return _make


def _make_audit() -> tuple[
    list[tuple[str, dict[str, Any]]],
    Any,
]:
    log: list[tuple[str, dict[str, Any]]] = []

    def _emit(kind: str, payload: Any) -> None:
        log.append((kind, dict(payload)))

    return log, _emit


class TestBasicDispatch:
    """The watcher is a no-op when no commands are enqueued."""

    def test_returns_iteration_result_when_no_commands(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify", "reason": "go"}')
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                audit_emit=emit,
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            assert isinstance(result.envelope, ValidEnvelope)
            assert result.envelope.intent is Intent.VERIFY
            assert client.entered and client.exited
            assert client.initial_prompt == "go"
            assert client.interrupt_calls == 0
            assert client.set_models == []
            assert client.injected == []

        asyncio.run(_run())
        # The watcher made claim_commands calls (empty) but emitted no
        # apply/fail events because nothing was pending.
        assert audit_log == []


class TestSayCommand:
    """``say`` dispatches via :meth:`ClaudeSDKClient.query`."""

    def test_say_injects_user_message_and_emits_applied_event(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SAY,
            {"text": "please address rubric finding"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release_after_tick() -> None:
                # Give the watcher one tick to drain the pending row, then
                # release the stream so the iteration completes.
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release_after_tick())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == ["please address rubric finding"]
        kinds = [kind for kind, _ in audit_log]
        assert EVENT_CONTROL_APPLIED in kinds
        applied = next(p for k, p in audit_log if k == EVENT_CONTROL_APPLIED)
        assert applied["kind"] == CONTROL_COMMAND_SAY
        assert applied["payload"] == {"text": "please address rubric finding"}


class TestSetModelCommand:
    """``set_model`` dispatches via :meth:`ClaudeSDKClient.set_model`."""

    def test_set_model_applies_and_emits_event(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SET_MODEL,
            {"model": "claude-sonnet-4-5"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.set_models == ["claude-sonnet-4-5"]
        applied = next(p for k, p in audit_log if k == EVENT_CONTROL_APPLIED)
        assert applied["kind"] == CONTROL_COMMAND_SET_MODEL


class TestInterruptCommand:
    """``interrupt`` cancels the iteration after dispatching."""

    def test_interrupt_cancels_iteration_task(self) -> None:
        # Stream that never yields; only the interrupt cancels it.
        async def _block() -> AsyncIterator[Message]:
            await asyncio.Event().wait()
            yield _result()  # pragma: no cover

        gate = asyncio.Event()
        # Pre-load one assistant message that we hold until the gate is
        # set; we'll never set it so the only exit is cancellation from
        # the watcher.
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_INTERRUPT,
            {},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            with pytest.raises(asyncio.CancelledError):
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )

        asyncio.run(_run())
        assert client.interrupt_calls == 1
        applied = next(p for k, p in audit_log if k == EVENT_CONTROL_APPLIED)
        assert applied["kind"] == CONTROL_COMMAND_INTERRUPT


class TestClaimOnce:
    """The watcher applies each command at most once."""

    def test_command_is_not_double_applied_across_ticks(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SET_MODEL,
            {"model": "m"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                # Let the watcher tick several times before completing
                # the iteration so we catch any double-claim regression.
                await asyncio.sleep(0.08)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.005,
                )
            finally:
                await release

        asyncio.run(_run())
        # Only one set_model dispatch, regardless of how many ticks fired.
        assert client.set_models == ["m"]
        applied = [k for k, _ in audit_log if k == EVENT_CONTROL_APPLIED]
        assert len(applied) == 1


class TestFailedDispatchDoesNotAbort:
    """A failing apply records a failed-application event and continues."""

    def test_set_model_raises_records_failed_event(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
            set_model_should_raise=RuntimeError("invalid model"),
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SET_MODEL,
            {"model": "bogus"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
                # The iteration still produces a normal result -- the
                # failed apply must not abort the run.
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        failed = [p for k, p in audit_log if k == EVENT_CONTROL_FAILED]
        assert len(failed) == 1
        assert failed[0]["kind"] == CONTROL_COMMAND_SET_MODEL
        assert failed[0]["error_type"] == "RuntimeError"
        # No applied event should land for this command.
        applied = [p for k, p in audit_log if k == EVENT_CONTROL_APPLIED]
        assert applied == []

    def test_say_with_bad_payload_records_failed_event(self) -> None:
        """A non-string ``text`` payload is rejected before the SDK call."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SAY,
            {"text": 42},  # not a string
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        failed = [p for k, p in audit_log if k == EVENT_CONTROL_FAILED]
        assert len(failed) == 1
        assert failed[0]["kind"] == CONTROL_COMMAND_SAY
        # No SDK call was ever dispatched.
        assert client.injected == []


class TestUnknownCommandKind:
    """An unknown command kind lands as a failed-application event."""

    def test_unknown_kind_emits_failed_and_does_not_raise(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            "rewind",  # unknown verb
            {},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        failed = [p for k, p in audit_log if k == EVENT_CONTROL_FAILED]
        assert len(failed) == 1
        assert failed[0]["kind"] == "rewind"


class TestCommandsScopedByRunId:
    """A command for a different run is never applied here."""

    def test_command_for_other_run_is_left_alone(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-OTHER",
            CONTROL_COMMAND_INTERRUPT,
            {},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.interrupt_calls == 0
        # The other run's command is still pending in the store.
        remaining = store.claim_commands(
            "run-OTHER", now=datetime.now(timezone.utc)
        )
        assert len(remaining) == 1
        assert audit_log == []


class TestStoreFailureIsBestEffort:
    """A store-side failure on ``claim_commands`` does not abort the run."""

    def test_claim_failure_records_event_and_retries(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )

        class _FlakeyStore:
            def __init__(self) -> None:
                self.calls = 0

            def claim_commands(
                self, run_id: str, *, now: datetime
            ) -> list[Any]:
                self.calls += 1
                if self.calls == 1:
                    raise OSError("transient store failure")
                return []

            def enqueue_command(self, *_args: object, **_kwargs: object) -> Any:
                raise NotImplementedError  # not used in this test

        store = _FlakeyStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                # Wait long enough for at least two ticks.
                await asyncio.sleep(0.08)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=cast(Any, store),
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.005,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        # The first tick raised; a claim_failed event recorded it.
        claim_failed = [
            p for k, p in audit_log if k == EVENT_CONTROL_CLAIM_FAILED
        ]
        assert len(claim_failed) == 1
        assert claim_failed[0]["error_type"] == "OSError"
        # The watcher retried after the failure (still made more calls).
        assert store.calls >= 2


class TestEnqueueOrderApply:
    """Multiple pending commands are applied in enqueue order."""

    def test_commands_dispatch_in_enqueue_order(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        now = datetime.now(timezone.utc)
        store.enqueue_command("run-1", CONTROL_COMMAND_SAY, {"text": "first"}, now=now)
        store.enqueue_command(
            "run-1", CONTROL_COMMAND_SET_MODEL, {"model": "m-1"}, now=now
        )
        store.enqueue_command(
            "run-1", CONTROL_COMMAND_SAY, {"text": "second"}, now=now
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == ["first", "second"]
        assert client.set_models == ["m-1"]
        applied = [p for k, p in audit_log if k == EVENT_CONTROL_APPLIED]
        # All three landed in enqueue order.
        assert [a["payload"].get("text") or a["payload"].get("model") for a in applied] == [
            "first",
            "m-1",
            "second",
        ]


class TestApproveRejectVerbConstants:
    """The new manual-approval verbs are exposed with the documented values."""

    def test_approve_constant_matches_wire_value(self) -> None:
        # The wire value is the source of truth for the persisted
        # ``control_commands.kind`` column and the
        # ``resolve_manual_approval`` matcher.
        assert CONTROL_COMMAND_APPROVE == "approve"

    def test_reject_constant_matches_wire_value(self) -> None:
        assert CONTROL_COMMAND_REJECT == "reject"


class TestApproveVerbInLiveWatcher:
    """``approve`` is owned by the out-of-band resolver, not the live watcher.

    The live watcher claims the row to record it on the audit stream but
    does not call any SDK method — the AWAITING_APPROVAL lifecycle has
    no live ``ClaudeSDKClient`` for the watcher to drive. The watcher
    must not crash on the verb, and must surface a not-applicable event
    distinct from the dispatched-to-SDK ``applied`` event.
    """

    def test_approve_is_not_dispatched_and_emits_not_applicable(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        # No SDK method was called for approve.
        assert client.injected == []
        assert client.set_models == []
        assert client.interrupt_calls == 0
        # A not-applicable event lands; no applied or failed event for
        # this row.
        not_applicable = [
            p for k, p in audit_log if k == EVENT_CONTROL_NOT_APPLICABLE
        ]
        assert len(not_applicable) == 1
        assert not_applicable[0]["kind"] == CONTROL_COMMAND_APPROVE
        assert not_applicable[0]["payload"] == {}
        assert [k for k, _ in audit_log if k == EVENT_CONTROL_APPLIED] == []
        assert [k for k, _ in audit_log if k == EVENT_CONTROL_FAILED] == []


class TestRejectVerbInLiveWatcher:
    """``reject`` mirrors ``approve`` for live-watcher semantics.

    Additionally the reject payload's optional ``feedback`` field is
    validated eagerly: an absent feedback is permitted, a non-string
    feedback raises before the row reaches the out-of-band resolver.
    """

    def test_reject_without_feedback_is_not_dispatched(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_REJECT,
            {},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        not_applicable = [
            p for k, p in audit_log if k == EVENT_CONTROL_NOT_APPLICABLE
        ]
        assert len(not_applicable) == 1
        assert not_applicable[0]["kind"] == CONTROL_COMMAND_REJECT
        assert not_applicable[0]["payload"] == {}
        assert [k for k, _ in audit_log if k == EVENT_CONTROL_FAILED] == []

    def test_reject_with_string_feedback_is_not_dispatched(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_REJECT,
            {"feedback": "gate it behind a feature flag first"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        not_applicable = [
            p for k, p in audit_log if k == EVENT_CONTROL_NOT_APPLICABLE
        ]
        assert len(not_applicable) == 1
        assert not_applicable[0]["kind"] == CONTROL_COMMAND_REJECT
        # The feedback payload round-trips so the audit stream records
        # what the resolver will eventually see.
        assert not_applicable[0]["payload"] == {
            "feedback": "gate it behind a feature flag first"
        }
        assert [k for k, _ in audit_log if k == EVENT_CONTROL_FAILED] == []

    def test_reject_with_non_string_feedback_records_failed_event(self) -> None:
        """A non-string ``feedback`` is a payload validation error.

        Mirrors :func:`_payload_model`'s behavior: the malformed row
        lands as a failed-application event in the live watcher's audit
        stream so an out-of-band consumer never reads a poisoned payload.
        """
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_REJECT,
            {"feedback": 42},  # not a string
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                )
            finally:
                await release

        asyncio.run(_run())
        failed = [p for k, p in audit_log if k == EVENT_CONTROL_FAILED]
        assert len(failed) == 1
        assert failed[0]["kind"] == CONTROL_COMMAND_REJECT
        assert failed[0]["error_type"] == "ValueError"
        # No not-applicable / applied events for this malformed row.
        assert [
            k
            for k, _ in audit_log
            if k in (EVENT_CONTROL_NOT_APPLICABLE, EVENT_CONTROL_APPLIED)
        ] == []
        # No SDK calls regardless.
        assert client.injected == []


# ---------------------------------------------------------------------------
# Spec 00019 mid-turn seam: context observer + harness recovery interrupt.
# ---------------------------------------------------------------------------


class _FakeClientWithContextUsage(_FakeClient):
    """Variant that exposes :meth:`get_context_usage` for the observer seam.

    The default ``_FakeClient`` deliberately omits ``get_context_usage`` so
    the existing tests exercise the "client lacks the method" fallback
    path; the spec 00019 seam tests use this subclass to feed scripted
    readings to the watcher.
    """

    def __init__(
        self,
        messages: list[Message],
        *,
        hold_until_event: asyncio.Event | None = None,
        interrupt_should_raise: BaseException | None = None,
        set_model_should_raise: BaseException | None = None,
        say_should_raise: BaseException | None = None,
        usage_readings: list[dict[str, Any]] | None = None,
        usage_should_raise: BaseException | None = None,
    ) -> None:
        super().__init__(
            messages,
            hold_until_event=hold_until_event,
            interrupt_should_raise=interrupt_should_raise,
            set_model_should_raise=set_model_should_raise,
            say_should_raise=say_should_raise,
        )
        # When ``usage_readings`` is exhausted the fake keeps returning
        # the last reading so a poll loop that ticks more often than the
        # test scripted still gets a deterministic value.
        self._usage_queue: list[dict[str, Any]] = list(usage_readings or [])
        self._usage_default: dict[str, Any] = {
            "categories": [],
            "totalTokens": 0,
            "maxTokens": 200_000,
            "rawMaxTokens": 200_000,
            "percentage": 0.0,
        }
        self._usage_raises = usage_should_raise
        self.usage_calls = 0

    async def get_context_usage(self) -> dict[str, Any]:
        self.usage_calls += 1
        if self._usage_raises is not None:
            raise self._usage_raises
        if self._usage_queue:
            return self._usage_queue.pop(0)
        return dict(self._usage_default)


class TestContextObserverSeam:
    """``context_observer`` receives ``get_context_usage`` readings each poll.

    The seam is off when ``context_observer`` is omitted (default
    ``None``); the existing test class TestBasicDispatch already covers
    that case end-to-end. These tests assert the seam behavior when the
    harness opts in.
    """

    def test_observer_receives_readings_for_each_poll(self) -> None:
        """At least one reading reaches the observer mid-iteration."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        reading_a = {
            "categories": [],
            "totalTokens": 50_000,
            "maxTokens": 200_000,
            "rawMaxTokens": 200_000,
            "percentage": 25.0,
        }
        reading_b = {
            "categories": [],
            "totalTokens": 100_000,
            "maxTokens": 200_000,
            "rawMaxTokens": 200_000,
            "percentage": 50.0,
        }
        client = _FakeClientWithContextUsage(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
            usage_readings=[reading_a, reading_b],
        )
        store = InMemoryStore()
        observed: list[dict[str, Any]] = []

        def _observe(reading: Any) -> None:
            observed.append(dict(reading))

        async def _run() -> None:
            async def _release() -> None:
                # Hold the stream long enough for several watcher ticks.
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    context_observer=_observe,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        # At least one reading reached the observer; the first reading
        # must be the first scripted reading (queue is FIFO).
        assert len(observed) >= 1
        assert client.usage_calls >= 1
        assert observed[0] == reading_a

    def test_observer_not_called_when_client_lacks_get_context_usage(
        self,
    ) -> None:
        """An older client without ``get_context_usage`` skips the seam."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        # _FakeClient (parent) has no get_context_usage method.
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        observed: list[Any] = []

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    context_observer=observed.append,
                )
            finally:
                await release

        asyncio.run(_run())
        # Observer was never called because the client lacks the method.
        assert observed == []
        # Sanity check that the parent fake really has no method bound.
        assert not hasattr(client, "get_context_usage")

    def test_get_context_usage_error_is_swallowed_silently(self) -> None:
        """A raise from the SDK call surfaces nothing to the observer."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClientWithContextUsage(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
            usage_should_raise=RuntimeError("transport blip"),
        )
        store = InMemoryStore()
        observed: list[Any] = []

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    context_observer=observed.append,
                )
                # The iteration still completes normally; the failed
                # reading was swallowed.
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        # The fake was called (at least once), but the observer never saw
        # a reading because every call raised.
        assert client.usage_calls >= 1
        assert observed == []

    def test_observer_exception_does_not_abort_iteration(self) -> None:
        """A faulty observer must never break the run."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClientWithContextUsage(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()

        def _bad_observer(_reading: Any) -> None:
            raise RuntimeError("observer crashed")

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    context_observer=_bad_observer,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())


class TestHarnessRecoveryInterrupt:
    """``recovery_interrupt_event`` is a distinct cancellation channel.

    The harness sets the event to ask the watcher to interrupt the
    in-flight iteration for a mid-turn summarize-restart recovery
    (spec 00019 FR-4). The watcher dispatches
    :meth:`ClaudeSDKClient.interrupt` and translates the resulting
    cancel into :class:`HarnessRecoveryRequested` so the
    ``_run_attempt`` boundary can route the attempt into the spec 00018
    recovery path rather than ``_handle_interrupt`` (operator-interrupt)
    or the external-cancel path.
    """

    def test_event_set_raises_harness_recovery_requested(self) -> None:
        """The event triggers ``client.interrupt`` and a distinct signal."""
        gate = asyncio.Event()  # never set: stream blocks indefinitely
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        recovery_event = asyncio.Event()

        async def _run() -> None:
            async def _request_recovery() -> None:
                # Let the watcher complete its first drain, then ask
                # for recovery -- mirrors the harness flipping the
                # event from its on_message tap after occupancy crosses
                # the recovery ratio.
                await asyncio.sleep(0.02)
                recovery_event.set()

            requester = asyncio.create_task(_request_recovery())
            try:
                with pytest.raises(HarnessRecoveryRequested):
                    await invoke_iteration_with_client(
                        prompt="go",
                        options=ClaudeAgentOptions(),
                        control_store=store,
                        run_id="run-1",
                        client_factory=_factory(client),
                        poll_interval=0.005,
                        recovery_interrupt_event=recovery_event,
                    )
            finally:
                await requester

        asyncio.run(_run())
        # The SDK interrupt was dispatched exactly once before the cancel.
        assert client.interrupt_calls == 1

    def test_signal_is_distinct_from_operator_interrupt(self) -> None:
        """Operator-interrupt still raises raw CancelledError, not the new signal."""
        gate = asyncio.Event()  # never set
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        # An operator-interrupt control command flips the operator
        # channel; the recovery event is supplied but never set.
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_INTERRUPT,
            {},
            now=datetime.now(timezone.utc),
        )
        recovery_event = asyncio.Event()

        async def _run() -> None:
            # Operator-interrupt must NOT raise HarnessRecoveryRequested.
            with pytest.raises(asyncio.CancelledError):
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    recovery_interrupt_event=recovery_event,
                )

        asyncio.run(_run())
        assert client.interrupt_calls == 1
        # Recovery channel was never asserted.
        assert not recovery_event.is_set()

    def test_signal_is_distinct_from_external_cancel(self) -> None:
        """An external cancel of the outer task still surfaces CancelledError."""
        gate = asyncio.Event()  # never set
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        recovery_event = asyncio.Event()  # never set

        async def _run() -> None:
            async def _invoke() -> Any:
                return await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.005,
                    recovery_interrupt_event=recovery_event,
                )

            outer = asyncio.create_task(_invoke())
            # Give the watcher a couple of ticks to wire up, then cancel
            # from outside -- mirroring SIGINT/SIGTERM.
            await asyncio.sleep(0.02)
            outer.cancel()
            with pytest.raises(asyncio.CancelledError):
                await outer

        asyncio.run(_run())
        # External cancel never went through the SDK interrupt path.
        assert client.interrupt_calls == 0

    def test_interrupt_dispatch_failure_still_raises_recovery_signal(
        self,
    ) -> None:
        """A faulty ``client.interrupt`` must not block the recovery signal."""
        gate = asyncio.Event()  # never set
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
            interrupt_should_raise=RuntimeError("interrupt blew up"),
        )
        store = InMemoryStore()
        recovery_event = asyncio.Event()

        async def _run() -> None:
            async def _request_recovery() -> None:
                await asyncio.sleep(0.02)
                recovery_event.set()

            requester = asyncio.create_task(_request_recovery())
            try:
                with pytest.raises(HarnessRecoveryRequested):
                    await invoke_iteration_with_client(
                        prompt="go",
                        options=ClaudeAgentOptions(),
                        control_store=store,
                        run_id="run-1",
                        client_factory=_factory(client),
                        poll_interval=0.005,
                        recovery_interrupt_event=recovery_event,
                    )
            finally:
                await requester

        asyncio.run(_run())
        # The interrupt was attempted exactly once even though it raised.
        assert client.interrupt_calls == 1

    def test_normal_completion_wins_race_against_recovery_event(self) -> None:
        """If the iteration finishes naturally, no recovery signal fires."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        # No hold gate -- the stream completes immediately.
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
        )
        store = InMemoryStore()
        recovery_event = asyncio.Event()  # never set during the iteration

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.005,
                recovery_interrupt_event=recovery_event,
            )
            # Normal IterationResult: the await never raised.
            assert isinstance(result.envelope, ValidEnvelope)
            # The watcher never asked for a recovery -- the event stays clear.
            assert not recovery_event.is_set()
            assert client.interrupt_calls == 0

        asyncio.run(_run())


class TestSteeringLedgerSeam:
    """``on_applied`` fires once per successfully dispatched command.

    The seam is how the harness ledgers steering (spec 00025 FR-10):
    failed dispatches and not-applicable verbs never reach it, a raising
    callback never breaks the drain, and an ``interrupt`` reaches it
    before the cancellation propagates so the ledger fact lands even
    when the tick stops the run.
    """

    def test_on_applied_receives_each_dispatched_command(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SAY,
            {"text": "steer"},
            now=datetime.now(timezone.utc),
        )
        applied: list[Any] = []

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    on_applied=applied.append,
                )
            finally:
                await release

        asyncio.run(_run())
        assert len(applied) == 1
        assert applied[0].kind == CONTROL_COMMAND_SAY
        assert dict(applied[0].payload) == {"text": "steer"}
        assert applied[0].id is not None

    def test_on_applied_skipped_when_dispatch_fails(self) -> None:
        """Edge case (spec 00025 FR-10): a claimed command whose SDK
        application fails leaves no ledger record -- the row stays
        claimed in the store as the visible trace."""
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
            set_model_should_raise=RuntimeError("transport down"),
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SET_MODEL,
            {"model": "claude-opus-4-8"},
            now=datetime.now(timezone.utc),
        )
        applied: list[Any] = []
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    on_applied=applied.append,
                )
            finally:
                await release

        asyncio.run(_run())
        assert applied == []
        kinds = [kind for kind, _ in audit_log]
        assert EVENT_CONTROL_FAILED in kinds
        # The claimed row is retained: a fresh claim sweep returns
        # nothing (claim-once), so the row's continued existence is the
        # visible trace of the failed application.
        assert store.claim_commands("run-1", now=datetime.now(timezone.utc)) == []

    def test_on_applied_skipped_for_not_applicable_verbs(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )
        applied: list[Any] = []

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    on_applied=applied.append,
                )
            finally:
                await release

        asyncio.run(_run())
        assert applied == []

    def test_raising_on_applied_does_not_break_the_drain(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_SAY,
            {"text": "steer"},
            now=datetime.now(timezone.utc),
        )
        audit_log, emit = _make_audit()

        def _explode(_command: Any) -> None:
            raise RuntimeError("ledger offline")

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    on_applied=_explode,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == ["steer"]
        kinds = [kind for kind, _ in audit_log]
        assert EVENT_CONTROL_APPLIED in kinds

    def test_interrupt_reaches_on_applied_before_cancellation(self) -> None:
        gate = asyncio.Event()  # never set: only the interrupt exits.
        client = _FakeClient(
            messages=[_assistant_text("never-emitted"), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        store.enqueue_command(
            "run-1",
            CONTROL_COMMAND_INTERRUPT,
            {},
            now=datetime.now(timezone.utc),
        )
        applied: list[Any] = []

        async def _run() -> None:
            with pytest.raises(asyncio.CancelledError):
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    on_applied=applied.append,
                )

        asyncio.run(_run())
        assert [c.kind for c in applied] == [CONTROL_COMMAND_INTERRUPT]


def _assistant_text_stop(text: str, stop_reason: str) -> AssistantMessage:
    """An assistant text message with an explicit non-default stop reason."""
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason=stop_reason,
        session_id="sess-1",
    )


def _result_stop(stop_reason: str) -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=1,
        duration_api_ms=1,
        is_error=False,
        num_turns=1,
        session_id="sess-1",
        stop_reason=stop_reason,
        total_cost_usd=0.01,
    )


class _MultiTurnFakeClient(_FakeClient):
    """Fake client that hands a distinct message list per ``receive_response``.

    The lost-envelope salvage drives the live session a second time on the
    same client. This fake pops the next queued turn's messages on each
    ``receive_response`` call so a test can script the initial (envelope-less)
    turn followed by the salvage turn's re-emitted envelope. ``query`` records
    the initial prompt first, then the salvage prompt into ``injected`` (the
    inherited contract), so a test asserts exactly how many salvage re-prompts
    were sent.
    """

    def __init__(self, turns: list[list[Message]]) -> None:
        super().__init__(messages=[])
        self._turns = [list(t) for t in turns]
        self.receive_calls = 0

    async def receive_response(self) -> AsyncIterator[Message]:
        index = self.receive_calls
        self.receive_calls += 1
        turn = self._turns[index] if index < len(self._turns) else []
        for msg in turn:
            yield msg


class TestLostEnvelopeSalvage:
    """A clean end_turn with a missing/truncated envelope is salvaged in place."""

    def test_missing_envelope_is_salvaged_via_reprompt(self) -> None:
        recovered = _wrap_envelope('{"intent": "verify", "reason": "done"}')
        client = _MultiTurnFakeClient(
            turns=[
                [_assistant_text("did the work, forgot the envelope"), _result()],
                [_assistant_text(recovered), _result()],
            ]
        )
        store = InMemoryStore()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            # The recovered envelope is adopted verbatim.
            assert isinstance(result.envelope, ValidEnvelope)
            assert result.envelope.intent is Intent.VERIFY
            # Exactly one bounded salvage re-prompt was sent on the session.
            assert client.injected == [ENVELOPE_SALVAGE_PROMPT]
            assert client.receive_calls == 2
            # Both turns' transcripts are preserved in agent_output.
            assert "did the work" in result.transcript
            # The salvage turn's messages fold into the merged result so the
            # harness observation counts the whole iteration.
            assert len(result.messages) == 4

        asyncio.run(_run())

    def test_truncated_envelope_is_salvaged_via_reprompt(self) -> None:
        # An opening fence with no matching close -- the envelope was cut off.
        truncated = f"{OPENING_FENCE}\n" + '{"intent": "verify"'
        recovered = _wrap_envelope('{"intent": "verify", "reason": "ok"}')
        client = _MultiTurnFakeClient(
            turns=[
                [_assistant_text(truncated), _result()],
                [_assistant_text(recovered), _result()],
            ]
        )
        store = InMemoryStore()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            assert isinstance(result.envelope, ValidEnvelope)
            assert client.injected == [ENVELOPE_SALVAGE_PROMPT]

        asyncio.run(_run())

    def test_salvage_that_stays_missing_keeps_original_and_is_bounded(self) -> None:
        client = _MultiTurnFakeClient(
            turns=[
                [_assistant_text("no envelope here"), _result()],
                [_assistant_text("still no envelope"), _result()],
            ]
        )
        store = InMemoryStore()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            # Salvage did not produce a valid envelope: the original
            # missing-envelope verdict stands (so the harness still finalizes
            # a genuinely broken agent as a protocol failure).
            assert isinstance(result.envelope, MissingEnvelope)
            # Exactly one salvage attempt -- never an unbounded re-prompt loop.
            assert client.injected == [ENVELOPE_SALVAGE_PROMPT]
            assert client.receive_calls == 2

        asyncio.run(_run())

    def test_valid_envelope_is_not_salvaged(self) -> None:
        envelope = _wrap_envelope('{"intent": "verify", "reason": "go"}')
        client = _MultiTurnFakeClient(
            turns=[[_assistant_text(envelope), _result()]]
        )
        store = InMemoryStore()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            assert isinstance(result.envelope, ValidEnvelope)
            # No salvage re-prompt and no second receive_response.
            assert client.injected == []
            assert client.receive_calls == 1

        asyncio.run(_run())

    def test_missing_envelope_without_end_turn_is_not_salvaged(self) -> None:
        # A non-end_turn stop (e.g. a limit / tool stop) is NOT the lost-tail
        # fingerprint -- it routes to existing handling, not the salvage path.
        client = _MultiTurnFakeClient(
            turns=[
                [
                    _assistant_text_stop("cut off mid-work", "max_tokens"),
                    _result_stop("max_tokens"),
                ],
                [_assistant_text(_wrap_envelope('{"intent": "verify"}')), _result()],
            ]
        )
        store = InMemoryStore()

        async def _run() -> None:
            result = await invoke_iteration_with_client(
                prompt="go",
                options=ClaudeAgentOptions(),
                control_store=store,
                run_id="run-1",
                client_factory=_factory(client),
                poll_interval=0.01,
            )
            assert isinstance(result.envelope, MissingEnvelope)
            assert client.injected == []
            assert client.receive_calls == 1

        asyncio.run(_run())


class TestCheckpointNudge:
    """The watcher injects a single checkpoint-commit instruction when the
    remaining wall time to the resolved AGENT_ITERATION ceiling drops to the
    threshold and an injected progress probe reports no new progress.

    The nudge rides the same ``ClaudeSDKClient.query`` surface an operator
    ``say`` uses, fires at most once per in-flight invocation, and never
    fabricates an operator control-command row.
    """

    def test_checkpoint_nudge_injects_once_when_no_progress(self) -> None:
        # threshold (5s) >= ceiling (1s): the nudge is eligible from the very
        # first watcher check. Held past several poll intervals, the
        # at-most-once flag must still yield exactly ONE injection (a
        # fires-every-poll mutation would inject repeatedly).
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release_after_polls() -> None:
                # Many poll_intervals (0.01s) elapse before release, so the
                # watcher ticks past the threshold repeatedly.
                await asyncio.sleep(0.08)
                gate.set()

            release = asyncio.create_task(_release_after_polls())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=1.0,
                    checkpoint_progress_probe=lambda: "unchanged",
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        # Exactly one injection despite many polls past the threshold; it is
        # the checkpoint-commit instruction, not an operator command.
        assert client.injected == [CHECKPOINT_NUDGE_PROMPT]
        assert "checkpoint" in CHECKPOINT_NUDGE_PROMPT.lower()
        assert "commit" in CHECKPOINT_NUDGE_PROMPT.lower()
        nudge_events = [
            p for k, p in audit_log if k == EVENT_CHECKPOINT_NUDGE
        ]
        assert len(nudge_events) == 1
        assert nudge_events[0]["ceiling_seconds"] == 1.0
        assert nudge_events[0]["threshold_seconds"] == 5.0
        assert "remaining_seconds" in nudge_events[0]
        # The nudge is harness-initiated -- it does NOT masquerade as an
        # applied operator control command.
        assert all(k != EVENT_CONTROL_APPLIED for k, _ in audit_log)

    def test_checkpoint_nudge_skipped_when_progress_reported(self) -> None:
        # A probe whose token changes on every read means the agent keeps
        # making progress: the nudge never fires even though the remaining
        # time is under the threshold.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()
        calls = {"n": 0}

        def _probe() -> int:
            calls["n"] += 1
            # baseline reads 1; every subsequent check reads a fresh value,
            # so token != baseline -> progress -> no nudge.
            return calls["n"]

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.06)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=1.0,
                    checkpoint_progress_probe=_probe,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)
        # The probe was actually re-read on the checks (not just baseline).
        assert calls["n"] >= 2

    def test_checkpoint_nudge_dormant_when_ceiling_unbounded(self) -> None:
        # AGENT_ITERATION opted out (resolved ceiling None): with no deadline
        # there is no remaining time to cross, so the nudge never fires.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=None,
                    checkpoint_progress_probe=lambda: "unchanged",
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)

    def test_checkpoint_nudge_dormant_without_probe(self) -> None:
        # No probe (the default): the nudge is fully dormant even when the
        # threshold and ceiling would otherwise make it eligible.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=1.0,
                    checkpoint_progress_probe=None,
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)

    def test_checkpoint_nudge_not_fired_before_threshold(self) -> None:
        # Large ceiling, tiny threshold: the remaining wall time never drops
        # to the threshold in the test window, so the nudge does not fire.
        # A mutation that ignores the threshold (fires whenever armed) would
        # inject here.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=0.001,
                    agent_iteration_ceiling_seconds=1000.0,
                    checkpoint_progress_probe=lambda: "unchanged",
                )
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)

    def test_checkpoint_nudge_contained_when_probe_raises_at_check(
        self,
    ) -> None:
        # Baseline read succeeds (armed), but every check-time probe read
        # raises: the exception is contained (never unwinds the iteration)
        # and the nudge is skipped.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()
        calls = {"n": 0}

        def _probe() -> str:
            calls["n"] += 1
            if calls["n"] == 1:
                return "baseline"
            raise RuntimeError("probe boom")

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.06)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=1.0,
                    checkpoint_progress_probe=_probe,
                )
                # The iteration finished normally -- the raising probe did
                # not unwind it.
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)
        # The probe was re-read at check time (and raised).
        assert calls["n"] >= 2

    def test_checkpoint_nudge_disarmed_when_baseline_probe_raises(
        self,
    ) -> None:
        # A probe that raises on the baseline read at iteration start leaves
        # the nudge disarmed for the whole invocation -- contained, never
        # unwinds the iteration.
        envelope = _wrap_envelope('{"intent": "verify"}')
        gate = asyncio.Event()
        client = _FakeClient(
            messages=[_assistant_text(envelope), _result()],
            hold_until_event=gate,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()

        def _probe() -> str:
            raise RuntimeError("boom at baseline")

        async def _run() -> None:
            async def _release() -> None:
                await asyncio.sleep(0.05)
                gate.set()

            release = asyncio.create_task(_release())
            try:
                result = await invoke_iteration_with_client(
                    prompt="go",
                    options=ClaudeAgentOptions(),
                    control_store=store,
                    run_id="run-1",
                    audit_emit=emit,
                    client_factory=_factory(client),
                    poll_interval=0.01,
                    checkpoint_nudge_seconds=5.0,
                    agent_iteration_ceiling_seconds=1.0,
                    checkpoint_progress_probe=_probe,
                )
                assert isinstance(result.envelope, ValidEnvelope)
            finally:
                await release

        asyncio.run(_run())
        assert client.injected == []
        assert all(k != EVENT_CHECKPOINT_NUDGE for k, _ in audit_log)

    def test_checkpoint_nudge_does_not_extend_outer_deadline(self) -> None:
        # After a nudge fires, an OUTER run_with_deadline (exactly how the
        # harness bounds the invocation) still cancels the invocation at the
        # original ceiling: the nudge's client.query neither resets the
        # deadline nor swallows the deadline cancel (a nudge-extends-deadline
        # mutation would let the invocation outlast the ceiling).
        never = asyncio.Event()  # never set -> the stream blocks forever
        client = _FakeClient(
            messages=[_assistant_text("blocked"), _result()],
            hold_until_event=never,
        )
        store = InMemoryStore()
        audit_log, emit = _make_audit()
        ceiling = 0.3

        async def _run() -> None:
            with pytest.raises(DeadlineExceeded):
                await run_with_deadline(
                    invoke_iteration_with_client(
                        prompt="go",
                        options=ClaudeAgentOptions(),
                        control_store=store,
                        run_id="run-1",
                        audit_emit=emit,
                        client_factory=_factory(client),
                        poll_interval=0.01,
                        checkpoint_nudge_seconds=5.0,
                        agent_iteration_ceiling_seconds=ceiling,
                        checkpoint_progress_probe=lambda: "unchanged",
                    ),
                    ceiling,
                )

        asyncio.run(_run())
        # The nudge fired exactly once before the deadline cut the run off.
        assert client.injected == [CHECKPOINT_NUDGE_PROMPT]
        assert sum(
            1 for k, _ in audit_log if k == EVENT_CHECKPOINT_NUDGE
        ) == 1
