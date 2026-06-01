"""Bidirectional invoker built on :class:`claude_agent_sdk.ClaudeSDKClient`.

Wraps the persistent-session client so a watcher coroutine, running
concurrently with the agent's message stream, can claim operator-issued
control commands from a :class:`flywheel.store_protocols.ControlCommandStore`
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
:func:`flywheel.invoker.invoke_iteration` — it supplies
``client.receive_response()`` as the ``message_stream`` so every existing
SDK-signal mapping (envelopes, tool interactions, usage, failures)
keeps the single source of truth.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Mapping
from datetime import datetime, timezone
from typing import Any

from claude_agent_sdk import (
    ClaudeAgentOptions,
    ClaudeSDKClient,
    Message,
)

from flywheel.invoker import IterationResult, invoke_iteration
from flywheel.store_protocols import ControlCommandRecord, ControlCommandStore


# Default poll tick the watcher uses when the store has no pending row.
# Short enough that an operator's CLI subcommand is applied with a
# noticeable but bounded delay; long enough that idle iterations do not
# hammer the store.
DEFAULT_CONTROL_POLL_INTERVAL: float = 0.25


# Recognized control-command verbs. Each maps to a single dispatch on the
# live :class:`ClaudeSDKClient` session.
CONTROL_COMMAND_INTERRUPT: str = "interrupt"
CONTROL_COMMAND_SET_MODEL: str = "set_model"
CONTROL_COMMAND_SAY: str = "say"


# Audit-event kinds emitted by the watcher. ``applied`` lands on every
# successful dispatch (the store-triggered control plane's authoritative
# audit signal); ``failed`` lands when the dispatch raised but the run
# continues; ``claim_failed`` lands when the store call itself raised
# (the watcher swallowed the error and will retry on the next tick).
EVENT_CONTROL_APPLIED: str = "harness.control_command_applied"
EVENT_CONTROL_FAILED: str = "harness.control_command_failed"
EVENT_CONTROL_CLAIM_FAILED: str = "harness.control_command_claim_failed"


AuditEmit = Callable[[str, Mapping[str, Any]], None]


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


async def _apply_command(
    client: ClaudeSDKClient,
    command: ControlCommandRecord,
) -> None:
    """Dispatch one claimed control command against the live SDK session.

    Raises whatever the SDK raises; the watcher converts the exception
    into a ``harness.control_command_failed`` event. ``interrupt`` is
    surfaced via :meth:`ClaudeSDKClient.interrupt`; the watcher's caller
    decides whether to escalate the dispatch into iteration cancellation.
    """
    kind = command.kind
    payload = dict(command.payload)
    if kind == CONTROL_COMMAND_INTERRUPT:
        await client.interrupt()
        return
    if kind == CONTROL_COMMAND_SET_MODEL:
        await client.set_model(_payload_model(payload))
        return
    if kind == CONTROL_COMMAND_SAY:
        await client.query(_payload_text(payload))
        return
    raise ValueError(f"unknown control command kind: {kind!r}")


async def _drain_pending(
    *,
    client: ClaudeSDKClient,
    control_store: ControlCommandStore,
    run_id: str,
    audit_emit: AuditEmit | None,
    now: Callable[[], datetime],
    interrupt_flag: list[bool],
) -> None:
    """Claim every pending row for ``run_id`` and apply it in order.

    A claim-once primitive is shared across watcher restarts and
    concurrent workers, so a second drain tick (or a concurrent peer)
    returns nothing for already-claimed rows — the watcher cannot
    double-apply a command. Each apply is independent: a failure on one
    row records a failed-application event and the loop continues with
    the next row, matching the spec's "best-effort, never abort"
    contract.

    ``interrupt_flag`` is the sole channel back to the watcher loop: a
    successful ``interrupt`` dispatch flips the flag so the iteration
    task is cancelled on the same tick (after the audit event lands).
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
            await _apply_command(client, command)
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
                EVENT_CONTROL_APPLIED,
                {
                    "command_id": command.id,
                    "kind": command.kind,
                    "payload": dict(command.payload),
                },
            )
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
    client_factory: Callable[
        [ClaudeAgentOptions], ClaudeSDKClient
    ] = ClaudeSDKClient,
) -> IterationResult:
    """Drive one iteration through :class:`ClaudeSDKClient` with a watcher.

    Opens a persistent client session, sends ``prompt``, and concurrently
    runs a watcher coroutine that claims pending control commands for
    ``run_id`` every ``poll_interval`` seconds and applies them live.

    Message draining delegates to
    :func:`flywheel.invoker.invoke_iteration` with
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
    """
    iteration_task = asyncio.current_task()
    if iteration_task is None:
        raise RuntimeError(
            "invoke_iteration_with_client must run inside an asyncio task"
        )

    interrupt_flag: list[bool] = [False]
    stop = asyncio.Event()

    client = client_factory(options)
    async with client:
        await client.query(prompt)

        async def _watcher() -> None:
            """Poll the store every ``poll_interval`` and drain.

            The loop is structured as drain-then-wait so a command that
            was already pending when the watcher started fires on the
            first tick. ``stop`` lets the wait short-circuit when the
            iteration finishes (or cancellation propagates here).
            """
            while not stop.is_set():
                await _drain_pending(
                    client=client,
                    control_store=control_store,
                    run_id=run_id,
                    audit_emit=audit_emit,
                    now=now,
                    interrupt_flag=interrupt_flag,
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
                try:
                    await asyncio.wait_for(
                        stop.wait(), timeout=poll_interval
                    )
                except asyncio.TimeoutError:
                    pass

        watcher_task = asyncio.create_task(_watcher())
        try:
            return await invoke_iteration(
                prompt=prompt,
                message_stream=client.receive_response(),
                on_message=on_message,
            )
        finally:
            stop.set()
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
    "CONTROL_COMMAND_INTERRUPT",
    "CONTROL_COMMAND_SAY",
    "CONTROL_COMMAND_SET_MODEL",
    "DEFAULT_CONTROL_POLL_INTERVAL",
    "EVENT_CONTROL_APPLIED",
    "EVENT_CONTROL_CLAIM_FAILED",
    "EVENT_CONTROL_FAILED",
    "invoke_iteration_with_client",
]
