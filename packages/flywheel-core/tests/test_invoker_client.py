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

from flywheel_core.envelope import CLOSING_FENCE, OPENING_FENCE, Intent, ValidEnvelope
from flywheel_core.invoker_client import (
    CONTROL_COMMAND_APPROVE,
    CONTROL_COMMAND_INTERRUPT,
    CONTROL_COMMAND_REJECT,
    CONTROL_COMMAND_SAY,
    CONTROL_COMMAND_SET_MODEL,
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
