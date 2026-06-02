"""Behavior tests for the pure :mod:`flywheel.redaction` core.

Covers the FRs that land in this task:

* FR-2 type-preserving transform; non-payload fields unchanged.
* FR-3 pattern substitution (subset: the redactor itself; built-in
  default policy comes in the next task).
* FR-7 deterministic composition.
* FR-13 Redactor is a pure single-record transform with no store coupling.
* Edge cases: empty payloads, empty strings, nested dicts/lists, non-string
  leaves passed through untouched, secret embedded in larger string.
"""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from flywheel.redaction import PatternRedactor, Redactor, compose
from flywheel.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)


# --- Fixtures ---------------------------------------------------------------


def _event(payload: dict[str, object]) -> EventRecord:
    return EventRecord(
        run_id="run-1",
        ts=datetime(2026, 6, 2, 12, 0, tzinfo=timezone.utc),
        kind="iteration",
        payload=payload,
        attempt_number=1,
        id=42,
        sequence=7,
        category="telemetry",
    )


def _sdk(payload: dict[str, object]) -> SdkMessageRecord:
    return SdkMessageRecord(
        run_id="run-1",
        attempt_number=1,
        iteration_number=2,
        message_type="assistant",
        payload=payload,
        ts=datetime(2026, 6, 2, 12, 0, tzinfo=timezone.utc),
        sequence=11,
        id=99,
    )


_ANTHROPIC_PATTERN: tuple[str, str] = (
    "anthropic_key",
    r"sk-ant-[A-Za-z0-9\-_]+",
)


# --- Protocol shape ---------------------------------------------------------


def test_pattern_redactor_satisfies_redactor_protocol() -> None:
    r = PatternRedactor([_ANTHROPIC_PATTERN])
    assert isinstance(r, Redactor)


def test_compose_returns_a_redactor() -> None:
    composed = compose(PatternRedactor([_ANTHROPIC_PATTERN]))
    assert isinstance(composed, Redactor)


# --- Type preservation (FR-2) -----------------------------------------------


def test_redact_preserves_event_record_type_and_metadata() -> None:
    record = _event({"prompt": "key=sk-ant-AAA"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert isinstance(out, EventRecord)
    assert out.run_id == record.run_id
    assert out.ts == record.ts
    assert out.kind == record.kind
    assert out.attempt_number == record.attempt_number
    assert out.id == record.id
    assert out.sequence == record.sequence
    assert out.category == record.category


def test_redact_preserves_sdk_message_type_and_metadata() -> None:
    record = _sdk({"text": "leak: sk-ant-XYZ"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert isinstance(out, SdkMessageRecord)
    assert out.run_id == record.run_id
    assert out.attempt_number == record.attempt_number
    assert out.iteration_number == record.iteration_number
    assert out.message_type == record.message_type
    assert out.ts == record.ts
    assert out.sequence == record.sequence
    assert out.id == record.id


def test_redact_returns_new_record_instance_not_input() -> None:
    record = _event({"prompt": "ok"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    # Even when nothing matches, a fresh record is returned and the input
    # payload is not aliased.
    assert out is not record
    assert out.payload is not record.payload


def test_redact_does_not_mutate_input_payload() -> None:
    payload = {"prompt": "key=sk-ant-AAA", "nested": {"k": "sk-ant-BBB"}}
    record = _event(payload)
    snapshot = {"prompt": "key=sk-ant-AAA", "nested": {"k": "sk-ant-BBB"}}
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    redactor.redact(record)

    assert payload == snapshot
    # The original nested dict is also untouched.
    assert payload["nested"] == {"k": "sk-ant-BBB"}


# --- Pattern substitution (FR-3 subset) -------------------------------------


def test_pattern_redactor_replaces_match_in_top_level_string() -> None:
    record = _event({"prompt": "key=sk-ant-abc123"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {"prompt": "key=[REDACTED:anthropic_key]"}


def test_pattern_redactor_replaces_secret_embedded_in_larger_string() -> None:
    record = _event({"log": "user said: sk-ant-leaked then continued typing"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {
        "log": "user said: [REDACTED:anthropic_key] then continued typing"
    }


def test_pattern_redactor_uses_bare_token_format() -> None:
    record = _event({"x": "sk-ant-AAA"})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    # Spec for this task: bare `[REDACTED:<label>]`. The salted HMAC variant
    # lands in redaction-builtins.
    assert out.payload["x"] == "[REDACTED:anthropic_key]"


# --- Deep traversal ---------------------------------------------------------


def test_redact_traverses_nested_dicts() -> None:
    record = _event(
        {
            "outer": {
                "inner": {"secret": "sk-ant-deep"},
                "fine": "hello",
            }
        }
    )
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {
        "outer": {
            "inner": {"secret": "[REDACTED:anthropic_key]"},
            "fine": "hello",
        }
    }


def test_redact_traverses_lists_and_mixed_containers() -> None:
    record = _event(
        {
            "items": [
                "sk-ant-a",
                {"nested": ["sk-ant-b", "clean"]},
                ["sk-ant-c"],
            ],
        }
    )
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {
        "items": [
            "[REDACTED:anthropic_key]",
            {"nested": ["[REDACTED:anthropic_key]", "clean"]},
            ["[REDACTED:anthropic_key]"],
        ],
    }


def test_redact_passes_non_string_leaves_through_untouched() -> None:
    record = _event(
        {
            "count": 7,
            "flag": True,
            "missing": None,
            "fraction": 1.5,
            "nested": {"n": 0, "items": [1, 2, 3, False, None]},
        }
    )
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {
        "count": 7,
        "flag": True,
        "missing": None,
        "fraction": 1.5,
        "nested": {"n": 0, "items": [1, 2, 3, False, None]},
    }


# --- Edge cases -------------------------------------------------------------


def test_redact_empty_payload_passes_through_with_new_dict() -> None:
    record = _event({})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {}
    assert out.payload is not record.payload


def test_redact_empty_string_passes_through_unchanged() -> None:
    record = _event({"prompt": ""})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {"prompt": ""}


def test_redact_no_match_passes_through_with_equal_value() -> None:
    record = _event({"prompt": "no secrets here", "tags": ["a", "b"]})
    redactor = PatternRedactor([_ANTHROPIC_PATTERN])

    out = redactor.redact(record)

    assert out.payload == {"prompt": "no secrets here", "tags": ["a", "b"]}


# --- PatternRedactor input validation ---------------------------------------


def test_pattern_redactor_rejects_malformed_regex_at_construction() -> None:
    # FR + Error handling: a bad regex must fail fast at construction, not
    # mid-stream.
    with pytest.raises(ValueError):
        PatternRedactor([("bad", "(unclosed")])


def test_pattern_redactor_rejects_empty_label() -> None:
    with pytest.raises(ValueError):
        PatternRedactor([("", r"sk-ant-\w+")])


# --- Composition (FR-7) -----------------------------------------------------


def test_compose_applies_redactors_in_order() -> None:
    first = PatternRedactor([("first", r"foo")])
    second = PatternRedactor([("second", r"\[REDACTED:first\]")])

    composed = compose(first, second)
    out = composed.redact(_event({"x": "foo"}))

    # first replaces foo with [REDACTED:first]; second then redacts that.
    assert out.payload["x"] == "[REDACTED:second]"


def test_compose_is_deterministic_across_calls() -> None:
    redactor = compose(
        PatternRedactor([_ANTHROPIC_PATTERN]),
        PatternRedactor([("aws_key", r"AKIA[0-9A-Z]{16}")]),
    )
    record = _event(
        {
            "msg": "anthropic=sk-ant-zzz aws=AKIAIOSFODNN7EXAMPLE other=clean",
        }
    )

    out_a = redactor.redact(record)
    out_b = redactor.redact(record)

    assert out_a.payload == out_b.payload
    assert out_a.payload == {
        "msg": (
            "anthropic=[REDACTED:anthropic_key] aws=[REDACTED:aws_key] "
            "other=clean"
        ),
    }


def test_compose_with_no_redactors_is_identity() -> None:
    record = _event({"prompt": "sk-ant-untouched"})
    composed = compose()

    out = composed.redact(record)

    # Identity: payload equals input but is still a fresh container per
    # FR-2 (never mutate the input record).
    assert out.payload == {"prompt": "sk-ant-untouched"}


def test_compose_threads_record_type_through() -> None:
    record = _sdk({"text": "sk-ant-keep-type"})
    composed = compose(PatternRedactor([_ANTHROPIC_PATTERN]))

    out = composed.redact(record)

    assert isinstance(out, SdkMessageRecord)


# --- FR-13: pure transform with no store coupling ---------------------------


def test_redactor_protocol_is_record_only() -> None:
    """The Redactor protocol takes one AuditRecord in and returns one out.

    Guards against accidentally coupling Redactor to the store, stream, or
    read path (FR-13).
    """

    import inspect

    sig = inspect.signature(PatternRedactor.redact)
    parameters = list(sig.parameters.values())
    # self + record
    assert len(parameters) == 2
    assert parameters[1].name == "record"


def test_pattern_redactor_returns_an_audit_record() -> None:
    record: AuditRecord = _event({"prompt": "ok"})
    out = PatternRedactor([_ANTHROPIC_PATTERN]).redact(record)
    assert isinstance(out, (EventRecord, SdkMessageRecord))
