"""Serialization between domain events and their persisted row shape.

:mod:`flywheel_core.events` is pure — it knows nothing about how an event is
stored. This module is the bridge: it converts a :class:`DomainEvent` to a
JSON-compatible payload mapping (the ``events.payload_json`` column) and
reconstructs the typed event from a stored row. Datetimes become ISO-8601
strings and enums become their ``.value``; the common columns
(``run_id``, ``ts``, ``attempt_number``, ``sequence``, ``id``) live on the
row, not in the payload.

Keeping this out of :mod:`flywheel_core.events` preserves that module's purity
(no json/pathlib/io) and mirrors the existing split where concrete stores
own serialization.
"""

from __future__ import annotations

from collections.abc import Mapping
from datetime import datetime
from typing import Any

from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    AwaitingApproval,
    Blocked,
    CommandApplied,
    DomainEvent,
    DomainEventKind,
    GateGraderReceipt,
    GraderEvaluated,
    HeldOutGateEvaluated,
    Landed,
    LandingParked,
    LandingRedriven,
    LifecycleInitialized,
    RetryScheduled,
    SessionRecorded,
    TransitionedTo,
    Unblocked,
)
from flywheel_core.lifecycle import Outcome, Status


def event_kind(event: DomainEvent) -> str:
    """Return the stable wire discriminator for ``event``."""
    return event.KIND.value


def event_payload(event: DomainEvent) -> dict[str, Any]:
    """Project the event-specific fields into a JSON-compatible mapping.

    Excludes the common columns (``run_id``/``ts``/``attempt_number``/
    ``sequence``/``id``) which the store persists as dedicated columns.
    """
    if isinstance(event, LifecycleInitialized):
        return {
            "task_id": event.task_id,
            "worker_id": event.worker_id,
            "artifacts_dir": event.artifacts_dir,
            "task_content_hash": event.task_content_hash,
            "source": event.source,
        }
    if isinstance(event, TransitionedTo):
        return {"target": event.target.value, "error": event.error}
    if isinstance(event, Blocked):
        return {"requires_json": event.requires_json}
    if isinstance(event, Unblocked):
        return {}
    if isinstance(event, AwaitingApproval):
        return {"awaiting_ordinal": event.awaiting_ordinal}
    if isinstance(event, RetryScheduled):
        return {
            "retries_used": event.retries_used,
            "max_retries": event.max_retries,
        }
    if isinstance(event, AttemptStarted):
        return {
            "number": event.number,
            "attempt_run_id": event.attempt_run_id,
            "started_at": event.started_at.isoformat(),
            "agent_context": dict(event.agent_context),
        }
    if isinstance(event, AttemptFinalized):
        return {
            "number": event.number,
            "outcome": event.outcome.value,
            "ended_at": event.ended_at.isoformat(),
            "agent_output": event.agent_output,
            "error": event.error,
        }
    if isinstance(event, SessionRecorded):
        return {"session_id": event.session_id}
    if isinstance(event, GraderEvaluated):
        return {
            "ordinal": event.ordinal,
            "grader_type": event.grader_type,
            "passed": event.passed,
            "duration_ms": event.duration_ms,
            "grader_name": event.grader_name,
            "grader_spec": dict(event.grader_spec),
            "payload": dict(event.payload),
        }
    if isinstance(event, CommandApplied):
        return {
            "command_kind": event.command_kind,
            "command_payload": dict(event.command_payload),
            "command_id": event.command_id,
        }
    if isinstance(event, LandingParked):
        parked: dict[str, Any] = {
            "park_kind": event.park_kind,
            "detail": event.detail,
        }
        # Emitted only when a check decided the park, so a park with no
        # receipts round-trips to the original two-key payload unchanged.
        if event.receipts:
            parked["receipts"] = [
                {
                    "grader_name": receipt.grader_name,
                    "passed": receipt.passed,
                    "output_excerpt": receipt.output_excerpt,
                }
                for receipt in event.receipts
            ]
        return parked
    if isinstance(event, Landed):
        landed: dict[str, Any] = {
            "strategy": event.strategy,
            "landed_ref": event.landed_ref,
        }
        # Emitted only when a recovery rung is named, so a land whose rung is
        # the default (a PR land, or any record from before this field existed)
        # round-trips to the original two-key payload unchanged.
        if event.rung:
            landed["rung"] = event.rung
        return landed
    if isinstance(event, LandingRedriven):
        return {"result": event.result, "park_kind": event.park_kind}
    if isinstance(event, HeldOutGateEvaluated):
        return {
            "outcome": event.outcome,
            "reason": event.reason,
            "receipts": [
                {
                    "grader_name": receipt.grader_name,
                    "passed": receipt.passed,
                    "output_excerpt": receipt.output_excerpt,
                }
                for receipt in event.receipts
            ],
        }
    raise TypeError(f"cannot serialize unknown domain event {type(event)!r}")


def event_from_record(
    *,
    kind: str,
    payload: Mapping[str, Any],
    run_id: str,
    ts: datetime,
    attempt_number: int | None,
    sequence: int | None,
    id: int | None,
) -> DomainEvent:
    """Reconstruct a typed :class:`DomainEvent` from a stored row.

    Raises :class:`ValueError` for an unrecognized ``kind`` so a corrupt or
    forward-incompatible row surfaces loudly rather than silently dropping.
    """
    common: dict[str, Any] = {
        "run_id": run_id,
        "ts": ts,
        "attempt_number": attempt_number,
        "sequence": sequence,
        "id": id,
    }
    event_kind_enum = DomainEventKind(kind)
    if event_kind_enum is DomainEventKind.LIFECYCLE_INITIALIZED:
        return LifecycleInitialized(
            task_id=payload["task_id"],
            worker_id=payload.get("worker_id", ""),
            artifacts_dir=payload.get("artifacts_dir", ""),
            task_content_hash=payload.get("task_content_hash", ""),
            source=payload.get("source", ""),
            **common,
        )
    if event_kind_enum is DomainEventKind.TRANSITIONED_TO:
        return TransitionedTo(
            target=Status(payload["target"]),
            error=payload.get("error", ""),
            **common,
        )
    if event_kind_enum is DomainEventKind.BLOCKED:
        return Blocked(requires_json=payload["requires_json"], **common)
    if event_kind_enum is DomainEventKind.UNBLOCKED:
        return Unblocked(**common)
    if event_kind_enum is DomainEventKind.AWAITING_APPROVAL:
        return AwaitingApproval(
            awaiting_ordinal=payload["awaiting_ordinal"], **common
        )
    if event_kind_enum is DomainEventKind.RETRY_SCHEDULED:
        return RetryScheduled(
            retries_used=payload["retries_used"],
            max_retries=payload["max_retries"],
            **common,
        )
    if event_kind_enum is DomainEventKind.ATTEMPT_STARTED:
        return AttemptStarted(
            number=payload["number"],
            attempt_run_id=payload["attempt_run_id"],
            started_at=datetime.fromisoformat(payload["started_at"]),
            agent_context=dict(payload.get("agent_context", {})),
            **common,
        )
    if event_kind_enum is DomainEventKind.ATTEMPT_FINALIZED:
        return AttemptFinalized(
            number=payload["number"],
            outcome=Outcome(payload["outcome"]),
            ended_at=datetime.fromisoformat(payload["ended_at"]),
            agent_output=payload.get("agent_output", ""),
            error=payload.get("error", ""),
            **common,
        )
    if event_kind_enum is DomainEventKind.SESSION_RECORDED:
        return SessionRecorded(session_id=payload["session_id"], **common)
    if event_kind_enum is DomainEventKind.GRADER_EVALUATED:
        return GraderEvaluated(
            ordinal=payload["ordinal"],
            grader_type=payload["grader_type"],
            passed=payload["passed"],
            duration_ms=payload["duration_ms"],
            grader_name=payload.get("grader_name"),
            grader_spec=dict(payload.get("grader_spec", {})),
            payload=dict(payload.get("payload", {})),
            **common,
        )
    if event_kind_enum is DomainEventKind.COMMAND_APPLIED:
        return CommandApplied(
            command_kind=payload["command_kind"],
            command_payload=dict(payload.get("command_payload", {})),
            command_id=payload.get("command_id"),
            **common,
        )
    if event_kind_enum is DomainEventKind.LANDING_PARKED:
        return LandingParked(
            park_kind=payload["park_kind"],
            detail=payload.get("detail", ""),
            receipts=tuple(
                GateGraderReceipt(
                    grader_name=receipt.get("grader_name"),
                    passed=receipt["passed"],
                    output_excerpt=receipt.get("output_excerpt", ""),
                )
                for receipt in payload.get("receipts", [])
            ),
            **common,
        )
    if event_kind_enum is DomainEventKind.LANDED:
        return Landed(
            strategy=payload["strategy"],
            landed_ref=payload["landed_ref"],
            rung=payload.get("rung", ""),
            **common,
        )
    if event_kind_enum is DomainEventKind.LANDING_REDRIVEN:
        return LandingRedriven(
            result=payload["result"],
            park_kind=payload.get("park_kind", ""),
            **common,
        )
    if event_kind_enum is DomainEventKind.HELD_OUT_GATE_EVALUATED:
        return HeldOutGateEvaluated(
            outcome=payload["outcome"],
            reason=payload.get("reason", ""),
            receipts=tuple(
                GateGraderReceipt(
                    grader_name=receipt.get("grader_name"),
                    passed=receipt["passed"],
                    output_excerpt=receipt.get("output_excerpt", ""),
                )
                for receipt in payload.get("receipts", [])
            ),
            **common,
        )
    raise ValueError(f"unknown domain event kind {kind!r}")


__all__ = ["event_from_record", "event_kind", "event_payload"]
