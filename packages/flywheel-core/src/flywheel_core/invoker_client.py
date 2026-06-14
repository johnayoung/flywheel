"""Bidirectional invoker built on :class:`claude_agent_sdk.ClaudeSDKClient`.

Wraps the persistent-session client so a watcher coroutine, running
concurrently with the agent's message stream, can claim operator-issued
control commands from a :class:`flywheel_core.store_protocols.ControlCommandStore`
and apply them live against the open session
(``interrupt`` / ``set_model`` / ``say``).

The watcher is the cross-process steering channel: a CLI producer enqueues
a command into the store, the in-process watcher inside the worker claims
it on its next tick (claim-once, enqueue-ordered) and dispatches it to the
live :class:`ClaudeSDKClient`. Each successful application emits a
``harness.control_command_applied`` telemetry event; a failed dispatch
emits ``harness.control_command_failed`` and is best-effort — the run
continues so a faulty command can never abort the iteration, matching
the per-message persistence contract.

A store-triggered ``interrupt`` mirrors a SIGINT/SIGTERM cancellation:
after :meth:`ClaudeSDKClient.interrupt` returns the watcher raises
:exc:`asyncio.CancelledError` into the iteration task so the harness's
``_run_attempt`` boundary routes the lifecycle through
``_handle_interrupt`` exactly the way operator-driven shutdown does.
The additional ``harness.control_command_applied`` event distinguishes a
store-triggered stop from a signal-driven one in the audit stream.

The invoker delegates message draining and signal extraction to
:func:`flywheel_core.invoker.invoke_iteration` — it supplies
``client.receive_response()`` as the ``message_stream`` so every existing
SDK-signal mapping (envelopes, tool interactions, usage, failures)
keeps the single source of truth.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Mapping
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from flywheel_core.invoker import IterationResult, invoke_iteration

if TYPE_CHECKING:
    from claude_agent_sdk import (
        ClaudeAgentOptions,
        ClaudeSDKClient,
        ContextUsageResponse,
        Message,
    )
from flywheel_core.store_protocols import ControlCommandRecord, ControlCommandStore


# Default poll tick the watcher uses when the store has no pending row.
# Short enough that an operator's CLI subcommand is applied with a
# noticeable but bounded delay; long enough that idle iterations do not
# hammer the store.
DEFAULT_CONTROL_POLL_INTERVAL: float = 0.25


# Recognized control-command verbs. ``interrupt`` / ``set_model`` / ``say``
# each map to a single dispatch on the live :class:`ClaudeSDKClient`
# session. ``approve`` / ``reject`` are out-of-band verbs that resolve a
# parked ``AWAITING_APPROVAL`` lifecycle via the harness's
# ``resolve_manual_approval`` sweep; the live in-session watcher treats
# them as not-applicable rather than dispatching them to a session.
CONTROL_COMMAND_INTERRUPT: str = "interrupt"
CONTROL_COMMAND_SET_MODEL: str = "set_model"
CONTROL_COMMAND_SAY: str = "say"
CONTROL_COMMAND_APPROVE: str = "approve"
CONTROL_COMMAND_REJECT: str = "reject"


# Audit-event kinds emitted by the watcher. ``applied`` lands on every
# successful dispatch (the store-triggered control plane's authoritative
# audit signal); ``failed`` lands when the dispatch raised but the run
# continues; ``claim_failed`` lands when the store call itself raised
# (the watcher swallowed the error and will retry on the next tick);
# ``not_applicable`` lands when the watcher claims a verb that has no
# live-session semantics (``approve`` / ``reject`` belong to the
# out-of-band ``resolve_manual_approval`` sweep, not the live SDK
# session), so the audit stream records that the watcher saw the row but
# left it for the out-of-band resolver.
EVENT_CONTROL_APPLIED: str = "harness.control_command_applied"
EVENT_CONTROL_FAILED: str = "harness.control_command_failed"
EVENT_CONTROL_CLAIM_FAILED: str = "harness.control_command_claim_failed"
EVENT_CONTROL_NOT_APPLICABLE: str = "harness.control_command_not_applicable"


AuditEmit = Callable[[str, Mapping[str, Any]], None]

# Observer callback invoked once per watcher poll with the live client's
# :meth:`ClaudeSDKClient.get_context_usage` reading. The harness uses this
# seam (spec 00019) to feed exact mid-turn context occupancy into the
# safety-net policy without piling a second SDK subscription onto the
# message stream. The watcher calls the observer only when the live client
# exposes ``get_context_usage`` AND the call returns a reading; absence or
# error is silently swallowed so the harness's accumulated
# ``AssistantMessage.usage`` estimate remains the fallback.
ContextUsageObserver = Callable[["ContextUsageResponse"], None]


class HarnessRecoveryRequested(Exception):
    """Sentinel raised when a harness-initiated mid-turn recovery interrupts.

    The harness signals ``recovery_interrupt_event`` to ask the watcher
    to interrupt the live :class:`ClaudeSDKClient` session for a mid-turn
    summarize-restart recovery (spec 00019). Once
    :meth:`ClaudeSDKClient.interrupt` has been dispatched, the watcher
    cancels the iteration task and
    :func:`invoke_iteration_with_client` translates the resulting
    :exc:`asyncio.CancelledError` into this sentinel so the harness can
    route the attempt into the spec 00018 recovery path.

    Distinct from:

    - the operator ``interrupt`` control command, which propagates as a
      raw :exc:`asyncio.CancelledError` via the ``interrupt_flag`` path
      so the harness's ``_run_attempt`` boundary routes through
      ``_handle_interrupt`` (the SIGINT/SIGTERM-shaped shutdown);
    - an external SIGINT/SIGTERM cancellation of the outer task, which
      also propagates as a raw :exc:`asyncio.CancelledError`.

    Only a harness-initiated mid-turn recovery raises this exception, so
    the harness can tell the three channels apart at the
    ``_run_attempt`` boundary the same way :class:`_HangDetected`
    separates a watchdog cancel from an outside cancel
    (``harness.py:2569``).
    """


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _payload_text(payload: Mapping[str, Any]) -> str:
    """Coerce a ``say`` payload to its operator-message text.

    Raises :class:`ValueError` when the payload is not a non-empty string
    so the watcher records a failed-application event rather than relaying
    an unusable value to the SDK.
    """
    text = payload.get("text")
    if not isinstance(text, str) or not text:
        raise ValueError(
            "control command 'say' payload requires non-empty 'text' (str)"
        )
    return text


def _payload_model(payload: Mapping[str, Any]) -> str | None:
    """Coerce a ``set_model`` payload's ``model`` field.

    ``None`` is permitted (the SDK treats it as "use the default model"
    per :meth:`ClaudeSDKClient.set_model`); any other non-string value is
    rejected so the watcher emits a failed-application event before the
    SDK call rather than after.
    """
    model = payload.get("model")
    if model is None:
        return None
    if not isinstance(model, str):
        raise ValueError(
            "control command 'set_model' payload 'model' must be str or null"
        )
    return model


def _payload_feedback(payload: Mapping[str, Any]) -> str | None:
    """Coerce a ``reject`` payload's optional ``feedback`` field.

    A reject row may carry an operator-authored critique that flows into
    the next attempt's prompt via the reviewer-feedback retry path. The
    field is optional (absent means "(no feedback provided)"); any
    non-string value is rejected as a payload validation error mirroring
    :func:`_payload_model` so a malformed enqueue is recorded before the
    resolver writes a manual receipt.
    """
    if "feedback" not in payload:
        return None
    feedback = payload["feedback"]
    if not isinstance(feedback, str):
        raise ValueError(
            "control command 'reject' payload 'feedback' must be str if provided"
        )
    return feedback


async def _apply_command(
    client: ClaudeSDKClient,
    command: ControlCommandRecord,
) -> bool:
    """Dispatch one claimed control command against the live SDK session.

    Returns ``True`` when the verb was dispatched to the SDK and
    ``False`` when the verb has no live-session semantics (``approve`` /
    ``reject`` are resolved by the out-of-band ``resolve_manual_approval``
    sweep against an ``AWAITING_APPROVAL`` lifecycle; the live watcher
    only records that the claim was seen, leaving the row for the
    resolver to interpret).

    Raises whatever the SDK raises; the watcher converts the exception
    into a ``harness.control_command_failed`` event. ``interrupt`` is
    surfaced via :meth:`ClaudeSDKClient.interrupt`; the watcher's caller
    decides whether to escalate the dispatch into iteration cancellation.
    A non-string ``reject`` payload raises :class:`ValueError` from
    :func:`_payload_feedback` and lands as a failed-application event
    before any out-of-band consumer sees the malformed row.
    """
    kind = command.kind
    payload = dict(command.payload)
    if kind == CONTROL_COMMAND_INTERRUPT:
        await client.interrupt()
        return True
    if kind == CONTROL_COMMAND_SET_MODEL:
        await client.set_model(_payload_model(payload))
        return True
    if kind == CONTROL_COMMAND_SAY:
        await client.query(_payload_text(payload))
        return True
    if kind == CONTROL_COMMAND_APPROVE:
        # The live watcher has no AWAITING_APPROVAL session to act on;
        # the out-of-band resolver owns this verb. Recorded as
        # not-applicable so the audit stream attributes the no-op.
        return False
    if kind == CONTROL_COMMAND_REJECT:
        # Validate the optional feedback payload eagerly so a malformed
        # enqueue lands as a failed event in the live watcher's audit
        # stream rather than reaching the resolver as a poisoned row.
        _payload_feedback(payload)
        return False
    raise ValueError(f"unknown control command kind: {kind!r}")


async def _drain_pending(
    *,
    client: ClaudeSDKClient,
    control_store: ControlCommandStore,
    run_id: str,
    audit_emit: AuditEmit | None,
    now: Callable[[], datetime],
    interrupt_flag: list[bool],
    on_applied: Callable[[ControlCommandRecord], None] | None = None,
) -> None:
    """Claim every pending row for ``run_id`` and apply it in order.

    A claim-once primitive is shared across watcher restarts and
    concurrent workers, so a second drain tick (or a concurrent peer)
    returns nothing for already-claimed rows — the watcher cannot
    double-apply a command. Each apply is independent: a failure on one
    row records a failed-application event and the loop continues with
    the next row, matching the spec's "best-effort, never abort"
    contract. A failed dispatch also never reaches ``on_applied``: the
    row stays claimed in the store as the visible trace (spec 00025
    FR-10 edge case), and no ledger fact is recorded for an application
    that did not happen.

    ``on_applied`` is the steering-ledger seam (spec 00025 FR-10): it is
    invoked once per successfully dispatched command, after the audit
    event lands, so the harness can append the ``CommandApplied`` domain
    event and delete the applied queue row. Calls are guarded like
    ``audit_emit`` — a raising callback never breaks the drain.

    ``interrupt_flag`` is the sole channel back to the watcher loop: a
    successful ``interrupt`` dispatch flips the flag so the iteration
    task is cancelled on the same tick (after the audit event and the
    steering ledger record land).
    """
    try:
        pending = control_store.claim_commands(run_id, now=now())
    except Exception as exc:  # noqa: BLE001 — best-effort; retry next tick.
        if audit_emit is not None:
            _emit_safe(
                audit_emit,
                EVENT_CONTROL_CLAIM_FAILED,
                {
                    "error_type": type(exc).__name__,
                    "message": str(exc),
                },
            )
        return

    for command in pending:
        try:
            dispatched = await _apply_command(client, command)
        except Exception as exc:  # noqa: BLE001 — best-effort apply.
            if audit_emit is not None:
                _emit_safe(
                    audit_emit,
                    EVENT_CONTROL_FAILED,
                    {
                        "command_id": command.id,
                        "kind": command.kind,
                        "payload": dict(command.payload),
                        "error_type": type(exc).__name__,
                        "message": str(exc),
                    },
                )
            continue
        if audit_emit is not None:
            _emit_safe(
                audit_emit,
                EVENT_CONTROL_APPLIED if dispatched else EVENT_CONTROL_NOT_APPLICABLE,
                {
                    "command_id": command.id,
                    "kind": command.kind,
                    "payload": dict(command.payload),
                },
            )
        if dispatched and on_applied is not None:
            # Steering ledger (spec 00025 FR-10): record the applied
            # command before a possible interrupt cancellation below, so
            # the ledger fact lands even when this tick stops the run.
            try:
                on_applied(command)
            except Exception:  # noqa: BLE001 - never break the drain.
                pass
        if command.kind == CONTROL_COMMAND_INTERRUPT:
            interrupt_flag[0] = True


def _emit_safe(
    audit_emit: AuditEmit,
    kind: str,
    payload: Mapping[str, Any],
) -> None:
    """Invoke ``audit_emit`` and swallow its exceptions.

    The control-plane audit path is best-effort, mirroring the
    ``on_message`` persistence contract: a faulty emit must never abort
    the run or mask the dispatch outcome.
    """
    try:
        audit_emit(kind, payload)
    except Exception:  # noqa: BLE001 - never let audit failure break the run.
        pass


async def _read_context_usage(
    client: ClaudeSDKClient,
) -> ContextUsageResponse | None:
    """Read :meth:`ClaudeSDKClient.get_context_usage` for the observer seam.

    Returns ``None`` when the live client does not expose the method
    (plain query path, older SDK) or when the call itself raises -- both
    are treated as "no SDK reading this poll" and surface nothing, so
    the harness falls back silently to the accumulated
    ``AssistantMessage.usage`` estimate (spec 00019 Error Handling).
    """
    get_usage = getattr(client, "get_context_usage", None)
    if get_usage is None:
        return None
    try:
        return await get_usage()
    except Exception:  # noqa: BLE001 - fall back silently on any SDK error.
        return None


async def invoke_iteration_with_client(
    *,
    prompt: str,
    options: ClaudeAgentOptions,
    control_store: ControlCommandStore,
    run_id: str,
    audit_emit: AuditEmit | None = None,
    on_message: Callable[[Message], None] | None = None,
    poll_interval: float = DEFAULT_CONTROL_POLL_INTERVAL,
    now: Callable[[], datetime] = _utcnow,
    client_factory: Callable[[ClaudeAgentOptions], ClaudeSDKClient]
    | None = None,
    context_observer: ContextUsageObserver | None = None,
    recovery_interrupt_event: asyncio.Event | None = None,
    on_applied: Callable[[ControlCommandRecord], None] | None = None,
) -> IterationResult:
    """Drive one iteration through :class:`ClaudeSDKClient` with a watcher.

    Opens a persistent client session, sends ``prompt``, and concurrently
    runs a watcher coroutine that claims pending control commands for
    ``run_id`` every ``poll_interval`` seconds and applies them live.

    Message draining delegates to
    :func:`flywheel_core.invoker.invoke_iteration` with
    ``client.receive_response()`` as the upstream stream so the existing
    SDK-signal contract (envelope, tool interactions, usage, failure
    capture) is preserved exactly.

    A store-triggered ``interrupt`` propagates as
    :class:`asyncio.CancelledError` raised from this function so the
    harness's ``_run_attempt`` boundary routes the lifecycle through
    ``_handle_interrupt`` (the same path SIGINT/SIGTERM take). The
    ``harness.control_command_applied`` event lands before the
    cancellation so the audit stream attributes the stop to the operator
    command.

    ``client_factory`` is a test seam — production callers leave it at
    the default :class:`ClaudeSDKClient` constructor. The factory is
    invoked with ``options`` and the returned instance must satisfy the
    ``ClaudeSDKClient`` async-context-manager protocol.

    Spec 00019 mid-turn seam parameters (both off-by-default; existing
    callers and tests are unaffected when omitted):

    - ``context_observer``: when supplied, the watcher calls
      :meth:`ClaudeSDKClient.get_context_usage` at most once per
      ``poll_interval`` and hands the reading to the observer. When the
      live client does not expose ``get_context_usage`` or the call
      raises, the observer is silently skipped so the harness falls back
      to its accumulated ``AssistantMessage.usage`` estimate.
    - ``recovery_interrupt_event``: when supplied and set by the
      harness, the watcher dispatches :meth:`ClaudeSDKClient.interrupt`
      and cancels the in-flight iteration. The resulting
      :exc:`asyncio.CancelledError` is translated to
      :class:`HarnessRecoveryRequested` so the harness can tell a
      mid-turn recovery cancel apart from the operator ``interrupt``
      control command and from external SIGINT/SIGTERM. The interrupt
      dispatch is best-effort: an exception from ``client.interrupt``
      is swallowed and the recovery signal is still raised so the
      harness still routes the attempt into recovery.

    ``on_applied`` is the steering-ledger seam (spec 00025 FR-10): the
    watcher invokes it once per successfully dispatched command so the
    harness can append the ``CommandApplied`` domain event and delete
    the applied queue row. Guarded like ``audit_emit`` — a raising
    callback never breaks the drain. Failed dispatches and
    not-applicable verbs (``approve`` / ``reject``) never reach it.
    """
    interrupt_flag: list[bool] = [False]
    recovery_requested: list[bool] = [False]
    stop = asyncio.Event()

    if client_factory is None:
        # Agent-driving default: import the SDK lazily so this module stays
        # importable without the optional extra.
        from flywheel_core._sdk import ClaudeSDKClient

        client_factory = ClaudeSDKClient
    client = client_factory(options)
    async with client:
        await client.query(prompt)

        # Wrap the iteration in a child task so the watcher's cancel
        # operates on a distinct task from the one this function is
        # running in. This mirrors :func:`_invoke_with_watchdog`
        # (``harness.py:2575``) and gives clean race semantics: if the
        # iteration completes before a watcher-triggered cancel lands,
        # awaiting the now-done task returns the result; only when the
        # await actually raises :exc:`asyncio.CancelledError` do we
        # consult the recovery flag and translate to
        # :class:`HarnessRecoveryRequested`.
        async def _drive_invocation() -> IterationResult:
            return await invoke_iteration(
                prompt=prompt,
                message_stream=client.receive_response(),
                on_message=on_message,
            )

        iteration_task: asyncio.Task[IterationResult] = asyncio.create_task(
            _drive_invocation()
        )

        async def _watcher() -> None:
            """Poll the store every ``poll_interval`` and drain.

            The loop is structured as drain-then-wait so a command that
            was already pending when the watcher started fires on the
            first tick. ``stop`` lets the wait short-circuit when the
            iteration finishes (or cancellation propagates here).

            After draining, the watcher reads
            :meth:`ClaudeSDKClient.get_context_usage` for the
            ``context_observer`` seam and checks
            ``recovery_interrupt_event`` for a harness-initiated
            mid-turn recovery (spec 00019). Both are off when the
            corresponding parameter was not supplied.
            """
            while not stop.is_set():
                await _drain_pending(
                    client=client,
                    control_store=control_store,
                    run_id=run_id,
                    audit_emit=audit_emit,
                    now=now,
                    interrupt_flag=interrupt_flag,
                    on_applied=on_applied,
                )
                if interrupt_flag[0]:
                    # Cancel the iteration task so the harness's
                    # _run_attempt boundary routes through
                    # _handle_interrupt — the same path SIGINT/SIGTERM
                    # take. Schedule a single cancel; idempotent re-entry
                    # is harmless (asyncio.Task.cancel is itself
                    # idempotent), but we exit the loop immediately so
                    # we do not stack repeated cancels on the iteration
                    # task while it is unwinding.
                    iteration_task.cancel()
                    return
                if context_observer is not None:
                    reading = await _read_context_usage(client)
                    if reading is not None:
                        try:
                            context_observer(reading)
                        except Exception:  # noqa: BLE001
                            # Observer is best-effort: a faulty observer
                            # must never abort the run or mask the
                            # iteration outcome, mirroring the
                            # ``on_message`` persistence contract.
                            pass
                if (
                    recovery_interrupt_event is not None
                    and recovery_interrupt_event.is_set()
                ):
                    # Harness-initiated mid-turn recovery: dispatch the
                    # SDK interrupt (best-effort) and cancel the
                    # iteration task. The ``recovery_requested`` flag
                    # is the sole channel by which the outer try/except
                    # tells a recovery cancel apart from operator
                    # interrupt and external cancel.
                    try:
                        await client.interrupt()
                    except Exception:  # noqa: BLE001
                        # Best-effort: a failed interrupt dispatch must
                        # not block the recovery -- the harness still
                        # owns the recovery decision. Swallow and let
                        # the cancel below run regardless.
                        pass
                    recovery_requested[0] = True
                    iteration_task.cancel()
                    return
                try:
                    await asyncio.wait_for(
                        stop.wait(), timeout=poll_interval
                    )
                except asyncio.TimeoutError:
                    pass

        watcher_task = asyncio.create_task(_watcher())
        try:
            try:
                return await iteration_task
            except asyncio.CancelledError:
                if recovery_requested[0]:
                    # Translate the watcher-induced cancel into the
                    # distinguishable harness-recovery signal. Operator
                    # interrupt (``interrupt_flag``) and external cancel
                    # both leave ``recovery_requested`` False, so they
                    # continue to propagate as raw CancelledError --
                    # mirrors ``_HangDetected`` vs external cancel at
                    # ``harness.py:2569``.
                    raise HarnessRecoveryRequested() from None
                raise
        finally:
            stop.set()
            # Drain the iteration task if it is still running (e.g.,
            # the outer task was cancelled from outside): cancel and
            # await to a terminal state so no orphan keeps the SDK
            # connection alive past ``async with client`` cleanup. A
            # done task awaits to its already-produced result/exception
            # without blocking.
            if not iteration_task.done():
                iteration_task.cancel()
            try:
                await iteration_task
            except BaseException:  # noqa: BLE001 - already finalized.
                pass
            # The watcher is best-effort: a CancelledError or exception
            # here must not mask the original outcome. asyncio.shield is
            # not needed because watcher_task does not own any state the
            # caller reads after this point — we only need to drain it.
            if not watcher_task.done():
                watcher_task.cancel()
            try:
                await watcher_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
    # Unreachable: ``async with`` only exits via the ``return`` above or by
    # propagating an exception. This line satisfies pyright's "all code
    # paths return" check without a noqa pragma.
    raise AssertionError("invoke_iteration_with_client: unreachable")


__all__ = [
    "AuditEmit",
    "CONTROL_COMMAND_APPROVE",
    "CONTROL_COMMAND_INTERRUPT",
    "CONTROL_COMMAND_REJECT",
    "CONTROL_COMMAND_SAY",
    "CONTROL_COMMAND_SET_MODEL",
    "ContextUsageObserver",
    "DEFAULT_CONTROL_POLL_INTERVAL",
    "EVENT_CONTROL_APPLIED",
    "EVENT_CONTROL_CLAIM_FAILED",
    "EVENT_CONTROL_FAILED",
    "EVENT_CONTROL_NOT_APPLICABLE",
    "HarnessRecoveryRequested",
    "invoke_iteration_with_client",
]
