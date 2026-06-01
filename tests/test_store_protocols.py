"""Contract tests for the persistence Protocols.

These tests demonstrate that the protocols are satisfiable without
instantiating any concrete store: each `_Stub` class below is a no-op
shape check, asserted against the runtime-checkable Protocol via
`isinstance`. They also pin down the typed conflict signals and record
dataclasses that downstream stores (in-memory, SQLite) and the harness
will depend on.
"""

from __future__ import annotations

from dataclasses import fields, is_dataclass
from datetime import datetime, timezone

import pytest

from flywheel import (
    CURRENT_SCHEMA_VERSION,
    AgentSessionStore,
    Attempt,
    AttemptStore,
    AuditRecord,
    AuditStore,
    ClaudeSessionEntry,
    EventRecord,
    EventStore,
    GraderResultRecord,
    GraderResultStore,
    Lifecycle,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    SdkMessageRecord,
    SdkMessageStore,
    StoreConflictError,
    StoreSchemaError,
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
    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        return None

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        return None

    def list_attempts(self, run_id: str) -> list[Attempt]:
        return []


class _EventStub:
    def append_event(self, event: EventRecord) -> EventRecord:
        return event

    def list_events(self, run_id: str) -> list[EventRecord]:
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


class _AgentSessionStub:
    def append_session_entry(
        self, entry: ClaudeSessionEntry
    ) -> ClaudeSessionEntry:
        return entry

    def list_session_entries(
        self,
        project_key: str,
        session_id: str,
        subpath: str = "",
    ) -> list[ClaudeSessionEntry]:
        return []


class _SdkMessageStub:
    def append_sdk_message(
        self, message: SdkMessageRecord
    ) -> SdkMessageRecord:
        return message

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: object,
    ) -> list[SdkMessageRecord]:
        return []

    def list_sdk_messages(self, run_id: str) -> list[SdkMessageRecord]:
        return []


class _AuditStub:
    def read_audit_since(
        self, run_id: str, cursor: int
    ) -> list[AuditRecord]:
        return []


def test_lifecycle_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_LifecycleStub(), LifecycleStore)


def test_attempt_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_AttemptStub(), AttemptStore)


def test_event_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_EventStub(), EventStore)


def test_grader_result_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_GraderResultStub(), GraderResultStore)


def test_agent_session_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_AgentSessionStub(), AgentSessionStore)


def test_sdk_message_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_SdkMessageStub(), SdkMessageStore)


def test_audit_store_protocol_is_satisfiable_by_stub() -> None:
    assert isinstance(_AuditStub(), AuditStore)


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
        "category",
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


def test_claude_session_entry_is_dataclass_with_schema_fields() -> None:
    assert is_dataclass(ClaudeSessionEntry)
    names = {f.name for f in fields(ClaudeSessionEntry)}
    assert names == {
        "seq",
        "project_key",
        "session_id",
        "subpath",
        "entry",
        "mtime",
    }


def test_claude_session_entry_subpath_defaults_to_empty_string() -> None:
    e = ClaudeSessionEntry(
        project_key="proj",
        session_id="sess",
        entry="{}",
        mtime=0,
    )
    assert e.subpath == ""


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
