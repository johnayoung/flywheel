"""Contract tests for the persistence Protocols.

These tests demonstrate that the protocols are satisfiable without
instantiating any concrete store: each `_Stub` class below is a no-op
shape check, asserted against the runtime-checkable Protocol via
`isinstance`. They also pin down the typed conflict signals and record
dataclasses that downstream stores (in-memory, SQLite) and the harness
will depend on.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import fields, is_dataclass
from datetime import datetime, timezone
from typing import Any

import pytest

from flywheel_core import (
    CURRENT_SCHEMA_VERSION,
    Attempt,
    AttemptStore,
    ControlCommandRecord,
    ControlCommandStore,
    EventRecord,
    GraderResultRecord,
    GraderResultStore,
    Lifecycle,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    SdkMessageRecord,
    StoreConflictError,
    StoreSchemaError,
    TelemetryRecord,
    TelemetrySink,
)


# --- Satisfiability stubs ---------------------------------------------------


class _LifecycleStub:
    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        return None

    def update_lifecycle(
        self, lifecycle: Lifecycle, *, expected_version: int
    ) -> None:
        return None

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        return None


class _AttemptStub:
    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None:
        return None

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        return None

    def list_attempts(self, run_id: str) -> list[Attempt]:
        return []


class _GraderResultStub:
    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        return result

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return []


class _TelemetrySinkStub:
    def append_telemetry(self, record: TelemetryRecord) -> None:
        return None


class _ControlCommandStub:
    def enqueue_command(
        self,
        run_id: str,
        kind: str,
        payload: Mapping[str, Any],
        *,
        now: datetime,
    ) -> ControlCommandRecord:
        return ControlCommandRecord(
            run_id=run_id,
            kind=kind,
            payload=dict(payload),
            enqueued_at=now,
        )

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]:
        return []

    def delete_command(self, command_id: int) -> None:
        return None


def test_lifecycle_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_LifecycleStub(), LifecycleStore)


def test_attempt_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_AttemptStub(), AttemptStore)


def test_grader_result_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_GraderResultStub(), GraderResultStore)


def test_control_command_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_ControlCommandStub(), ControlCommandStore)


def test_telemetry_sink_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_TelemetrySinkStub(), TelemetrySink)


def test_attempt_stub_does_not_satisfy_telemetry_sink_protocol() -> None:
    assert not isinstance(_AttemptStub(), TelemetrySink)


# --- Append-only contract on grader_results --------------------------------


def test_grader_result_store_exposes_no_update_or_delete_entry_point() -> None:
    forbidden = {
        "update_grader_result",
        "delete_grader_result",
        "remove_grader_result",
        "set_grader_result",
        "replace_grader_result",
    }
    exposed = {
        name for name in dir(GraderResultStore) if not name.startswith("_")
    }
    leaked = exposed & forbidden
    assert not leaked, (
        f"GraderResultStore must be append-only; got mutators: {leaked}"
    )


def test_grader_result_store_surface_is_exactly_append_and_list() -> None:
    exposed = {
        name for name in dir(GraderResultStore) if not name.startswith("_")
    }
    assert exposed == {"append_grader_result", "list_grader_results"}


# --- Typed conflict signals -------------------------------------------------


def test_optimistic_concurrency_error_carries_versions() -> None:
    err = OptimisticConcurrencyError(
        "run-1", expected_version=2, actual_version=3
    )
    assert isinstance(err, StoreConflictError)
    assert err.run_id == "run-1"
    assert err.expected_version == 2
    assert err.actual_version == 3


def test_lifecycle_already_exists_error_carries_run_id() -> None:
    err = LifecycleAlreadyExistsError("run-2")
    assert isinstance(err, StoreConflictError)
    assert err.run_id == "run-2"


def test_lifecycle_not_found_error_carries_run_id() -> None:
    err = LifecycleNotFoundError("run-3")
    assert isinstance(err, StoreConflictError)
    assert err.run_id == "run-3"


# --- Records are typed dataclasses, not dicts ------------------------------


def test_event_record_is_dataclass_with_schema_fields() -> None:
    assert is_dataclass(EventRecord)
    names = {f.name for f in fields(EventRecord)}
    assert names == {
        "id",
        "run_id",
        "attempt_number",
        "ts",
        "kind",
        "payload",
        "sequence",
    }


def test_sdk_message_record_is_dataclass_with_schema_fields() -> None:
    assert is_dataclass(SdkMessageRecord)
    names = {f.name for f in fields(SdkMessageRecord)}
    assert names == {
        "id",
        "run_id",
        "attempt_number",
        "iteration_number",
        "sequence",
        "message_type",
        "payload",
        "ts",
    }


def test_sdk_message_record_defaults_sequence_and_id_to_none() -> None:
    rec = SdkMessageRecord(
        run_id="r1",
        attempt_number=1,
        iteration_number=2,
        message_type="assistant",
        payload={"raw": "x"},
        ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
    )
    assert rec.sequence is None
    assert rec.id is None


def test_grader_result_record_is_dataclass_with_schema_fields() -> None:
    assert is_dataclass(GraderResultRecord)
    names = {f.name for f in fields(GraderResultRecord)}
    assert names == {
        "id",
        "run_id",
        "attempt_number",
        "ordinal",
        "grader_type",
        "grader_name",
        "grader_spec",
        "passed",
        "duration_ms",
        "payload",
        "ts",
    }


def test_control_command_record_is_dataclass_with_schema_fields() -> None:
    assert is_dataclass(ControlCommandRecord)
    names = {f.name for f in fields(ControlCommandRecord)}
    assert names == {
        "id",
        "run_id",
        "kind",
        "payload",
        "enqueued_at",
        "claimed_at",
    }


def test_control_command_record_defaults_id_and_claimed_at_to_none() -> None:
    rec = ControlCommandRecord(
        run_id="r1",
        kind="say",
        payload={"text": "focus on graders"},
        enqueued_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
    )
    assert rec.id is None
    assert rec.claimed_at is None


def test_event_record_payload_defaults_to_empty_mapping() -> None:
    e = EventRecord(
        run_id="r1",
        ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
        kind="started",
    )
    assert dict(e.payload) == {}
    assert e.attempt_number is None
    assert e.id is None
    assert e.sequence is None


def test_telemetry_record_is_dataclass_with_minimum_fields() -> None:
    assert is_dataclass(TelemetryRecord)
    names = {f.name for f in fields(TelemetryRecord)}
    assert names == {
        "run_id",
        "ts",
        "kind",
        "payload",
        "attempt_number",
        "iteration_number",
    }


def test_telemetry_record_defaults_coordinates_and_payload() -> None:
    rec = TelemetryRecord(
        run_id="r1",
        ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
        kind="message_turn",
    )
    assert dict(rec.payload) == {}
    assert rec.attempt_number is None
    assert rec.iteration_number is None


def test_grader_result_record_constructs_with_required_fields() -> None:
    rec = GraderResultRecord(
        run_id="r1",
        attempt_number=1,
        ordinal=0,
        grader_type="command",
        grader_spec={"type": "command", "run": "true"},
        passed=True,
        duration_ms=12,
        payload={"exit_code": 0},
        ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
    )
    assert rec.grader_type == "command"
    assert rec.grader_name is None
    assert rec.id is None


# --- Negative satisfiability: missing methods break the contract -----------


class _PartialLifecycleStub:
    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        return None

    # missing update_lifecycle and load_lifecycle


def test_partial_implementation_does_not_satisfy_lifecycle_protocol() -> None:
    assert not isinstance(_PartialLifecycleStub(), LifecycleStore)


# --- Cross-protocol independence: a stub satisfies only its own protocol ----


def test_attempt_stub_does_not_satisfy_lifecycle_protocol() -> None:
    assert not isinstance(_AttemptStub(), LifecycleStore)


def test_optimistic_concurrency_error_is_raisable() -> None:
    with pytest.raises(OptimisticConcurrencyError) as exc_info:
        raise OptimisticConcurrencyError(
            "run-x", expected_version=4, actual_version=7
        )
    assert exc_info.value.actual_version == 7


# --- Schema-version mismatch signal ----------------------------------------


def test_store_schema_error_carries_versions_and_message() -> None:
    err = StoreSchemaError(
        observed_version=1, expected_version=CURRENT_SCHEMA_VERSION
    )
    assert err.observed_version == 1
    assert err.expected_version == CURRENT_SCHEMA_VERSION
    msg = str(err)
    assert "store must be re-created" in msg
    assert str(CURRENT_SCHEMA_VERSION) in msg


def test_store_schema_error_handles_unknown_observed_version() -> None:
    err = StoreSchemaError(
        observed_version=None, expected_version=CURRENT_SCHEMA_VERSION
    )
    assert err.observed_version is None
    assert "store must be re-created" in str(err)


def test_current_schema_version_is_positive_integer() -> None:
    assert isinstance(CURRENT_SCHEMA_VERSION, int)
    assert CURRENT_SCHEMA_VERSION >= 1
