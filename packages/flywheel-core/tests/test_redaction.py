"""Behavior tests for the pure :mod:`flywheel.redaction` core.

Covers the FRs that land in this task:

* FR-2 type-preserving transform; non-payload fields unchanged.
* FR-3 pattern substitution (including the ``default_policy()`` factory).
* FR-4 env-value redaction with absent/empty/short value guards.
* FR-5 field-path redaction with nested keys and list wildcards.
* FR-6 tool-denylist redaction over ``SdkMessageRecord`` payloads.
* FR-7 deterministic composition.
* FR-8 salted HMAC-SHA256 stable tokens.
* FR-13 Redactor is a pure single-record transform with no store coupling.
* NFR-3 byte-identical output across runs/processes.
* Edge cases: empty payloads, empty strings, nested dicts/lists, non-string
  leaves passed through untouched, secret embedded in larger string.
"""

from __future__ import annotations

import hashlib
import hmac
from collections.abc import Mapping
from datetime import datetime, timezone
from typing import Any

import pytest

from flywheel.redaction import (
    EnvValueRedactor,
    FieldPathRedactor,
    PatternRedactor,
    Redactor,
    ToolDenylistRedactor,
    compose,
    default_policy,
)
from flywheel.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)


# --- Fixtures ---------------------------------------------------------------


def _event(payload: Mapping[str, Any]) -> EventRecord:
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


def _sdk(payload: Mapping[str, Any]) -> SdkMessageRecord:
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


# --- EnvValueRedactor (FR-4) ------------------------------------------------


def test_env_value_redactor_redacts_known_env_value(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_TEST_SECRET", "supersecretvalue12345")
    redactor = EnvValueRedactor(["FLYWHEEL_TEST_SECRET"])

    record = _event(
        {"prompt": "leaked=supersecretvalue12345 end", "ok": "no secret"}
    )
    out = redactor.redact(record)

    assert out.payload == {
        "prompt": "leaked=[REDACTED:FLYWHEEL_TEST_SECRET] end",
        "ok": "no secret",
    }


def test_env_value_redactor_matches_value_in_nested_payload(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_TEST_SECRET", "supersecretvalue12345")
    redactor = EnvValueRedactor(["FLYWHEEL_TEST_SECRET"])

    record = _event(
        {
            "nested": {
                "items": ["supersecretvalue12345", "clean"],
                "deep": {"k": "before supersecretvalue12345 after"},
            }
        }
    )
    out = redactor.redact(record)

    assert out.payload == {
        "nested": {
            "items": ["[REDACTED:FLYWHEEL_TEST_SECRET]", "clean"],
            "deep": {
                "k": "before [REDACTED:FLYWHEEL_TEST_SECRET] after",
            },
        }
    }


def test_env_value_redactor_ignores_absent_env_var(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("FLYWHEEL_MISSING", raising=False)
    redactor = EnvValueRedactor(["FLYWHEEL_MISSING"])

    record = _event({"prompt": "nothing to redact"})
    out = redactor.redact(record)

    # Absent env var contributes no pattern.
    assert out.payload == {"prompt": "nothing to redact"}


def test_env_value_redactor_ignores_empty_env_var(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_EMPTY", "")
    redactor = EnvValueRedactor(["FLYWHEEL_EMPTY"])

    record = _event({"prompt": "would be wrecked if empty matched"})
    out = redactor.redact(record)

    # An empty value must never be a redaction pattern (it would inject
    # tokens between every character).
    assert out.payload == {"prompt": "would be wrecked if empty matched"}


def test_env_value_redactor_excludes_short_values(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_SHORT", "abc")
    redactor = EnvValueRedactor(["FLYWHEEL_SHORT"], minimum_length=8)

    record = _event({"prompt": "abc shows up abc and abc"})
    out = redactor.redact(record)

    # Below the floor: the common substring "abc" is left alone.
    assert out.payload == {"prompt": "abc shows up abc and abc"}


def test_env_value_redactor_one_char_value_excluded_by_default(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_TINY", "x")
    redactor = EnvValueRedactor(["FLYWHEEL_TINY"])

    record = _event({"prompt": "x marks the spot"})
    out = redactor.redact(record)

    # The default ``minimum_length`` keeps a 1-char env value from blanking
    # every "x" in any payload string (spec edge case).
    assert out.payload == {"prompt": "x marks the spot"}


def test_env_value_redactor_captures_at_construction_time(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_DRIFT", "originalvalue12345678")
    redactor = EnvValueRedactor(["FLYWHEEL_DRIFT"])

    # Mutating the env after construction must not change what is redacted.
    monkeypatch.setenv("FLYWHEEL_DRIFT", "newrotatedvalue98765432")

    record = _event(
        {"prompt": "old=originalvalue12345678 new=newrotatedvalue98765432"}
    )
    out = redactor.redact(record)

    assert out.payload == {
        "prompt": (
            "old=[REDACTED:FLYWHEEL_DRIFT] new=newrotatedvalue98765432"
        )
    }


def test_env_value_redactor_supports_label_override(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("ANTHROPIC_API_KEY", "abcdefghijklmnopqrstu")
    redactor = EnvValueRedactor(
        ["ANTHROPIC_API_KEY"],
        labels={"ANTHROPIC_API_KEY": "anthropic_key"},
    )

    record = _event({"prompt": "leak=abcdefghijklmnopqrstu"})
    out = redactor.redact(record)

    assert out.payload == {"prompt": "leak=[REDACTED:anthropic_key]"}


def test_env_value_redactor_handles_regex_metacharacters_in_value(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_RX", "abc[.*]+?value")
    redactor = EnvValueRedactor(["FLYWHEEL_RX"])

    record = _event({"prompt": "x=abc[.*]+?value y"})
    out = redactor.redact(record)

    # Literal substring match — never interprets the value as a regex.
    assert out.payload == {"prompt": "x=[REDACTED:FLYWHEEL_RX] y"}


def test_env_value_redactor_no_captured_values_returns_fresh_record(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("FLYWHEEL_NOPE", raising=False)
    redactor = EnvValueRedactor(["FLYWHEEL_NOPE"])

    payload = {"x": "y", "nested": {"k": "v"}}
    record = _event(payload)
    out = redactor.redact(record)

    assert out.payload == payload
    assert out.payload is not record.payload
    assert out.payload["nested"] is not payload["nested"]


def test_env_value_redactor_rejects_zero_minimum_length() -> None:
    with pytest.raises(ValueError):
        EnvValueRedactor(["X"], minimum_length=0)


def test_env_value_redactor_rejects_empty_var_name() -> None:
    with pytest.raises(ValueError):
        EnvValueRedactor([""])


# --- FieldPathRedactor (FR-5) -----------------------------------------------


def test_field_path_redactor_replaces_top_level_value() -> None:
    redactor = FieldPathRedactor(["env"])
    record = _event({"env": {"K": "V", "K2": "V2"}, "other": "ok"})

    out = redactor.redact(record)

    assert out.payload == {"env": "[REDACTED:field]", "other": "ok"}


def test_field_path_redactor_replaces_nested_path() -> None:
    redactor = FieldPathRedactor(["tool_input.command"])
    record = _event(
        {"tool_input": {"command": "rm -rf /", "args": ["x"]}, "other": "y"}
    )

    out = redactor.redact(record)

    assert out.payload == {
        "tool_input": {"command": "[REDACTED:field]", "args": ["x"]},
        "other": "y",
    }


def test_field_path_redactor_supports_list_wildcard_terminal() -> None:
    redactor = FieldPathRedactor(["items.*"])
    record = _event({"items": ["a", "b", "c"]})

    out = redactor.redact(record)

    assert out.payload == {
        "items": ["[REDACTED:field]", "[REDACTED:field]", "[REDACTED:field]"]
    }


def test_field_path_redactor_supports_list_wildcard_nested() -> None:
    redactor = FieldPathRedactor(["items.*.secret"])
    record = _event(
        {
            "items": [
                {"secret": "x", "label": "a"},
                {"secret": "y", "label": "b"},
            ]
        }
    )

    out = redactor.redact(record)

    assert out.payload == {
        "items": [
            {"secret": "[REDACTED:field]", "label": "a"},
            {"secret": "[REDACTED:field]", "label": "b"},
        ]
    }


def test_field_path_redactor_silent_on_missing_path() -> None:
    redactor = FieldPathRedactor(["does.not.exist", "also_missing"])
    record = _event({"a": 1, "b": "two", "c": {"d": "e"}})

    out = redactor.redact(record)

    assert out.payload == {"a": 1, "b": "two", "c": {"d": "e"}}
    # Still a fresh payload container.
    assert out.payload is not record.payload


def test_field_path_redactor_preserves_non_string_value_token_format() -> None:
    redactor = FieldPathRedactor(["count"])
    record = _event({"count": 42})

    out = redactor.redact(record)

    # Non-string value still gets a bare token (digest is undefined for
    # non-UTF-8 cleartext).
    assert out.payload == {"count": "[REDACTED:field]"}


def test_field_path_redactor_does_not_mutate_input_payload() -> None:
    redactor = FieldPathRedactor(["tool_input.command"])
    payload = {"tool_input": {"command": "rm -rf /"}}
    snapshot = {"tool_input": {"command": "rm -rf /"}}
    record = _event(payload)

    redactor.redact(record)

    assert payload == snapshot


def test_field_path_redactor_rejects_empty_path() -> None:
    with pytest.raises(ValueError):
        FieldPathRedactor([""])


def test_field_path_redactor_rejects_empty_segment() -> None:
    with pytest.raises(ValueError):
        FieldPathRedactor(["a..b"])


def test_field_path_redactor_satisfies_protocol() -> None:
    redactor = FieldPathRedactor(["x"])
    assert isinstance(redactor, Redactor)


# --- ToolDenylistRedactor (FR-6) --------------------------------------------


def test_tool_denylist_redactor_redacts_input_of_named_tool() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    record = _sdk(
        {
            "message_type": "AssistantMessage",
            "content": [
                {"name": "Bash", "input": {"command": "rm -rf /"}, "id": "tu-1"},
                {"text": "hello"},
            ],
        }
    )

    out = redactor.redact(record)

    content = out.payload["content"]
    assert isinstance(content, list)
    assert content[0]["input"] == "[REDACTED:tool:Bash]"
    # The tool's name itself is preserved so a reader sees which tool ran.
    assert content[0]["name"] == "Bash"
    assert content[0]["id"] == "tu-1"
    # Other content blocks pass through.
    assert content[1] == {"text": "hello"}


def test_tool_denylist_redactor_redacts_content_of_named_tool() -> None:
    redactor = ToolDenylistRedactor(["ReadCredentials"])
    record = _sdk(
        {
            "content": [
                {
                    "name": "ReadCredentials",
                    "content": "aws_secret=raw_value_here",
                    "id": "tu-7",
                }
            ]
        }
    )

    out = redactor.redact(record)

    block = out.payload["content"][0]
    assert block["content"] == "[REDACTED:tool:ReadCredentials]"
    assert block["name"] == "ReadCredentials"


def test_tool_denylist_redactor_preserves_envelope() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    record = _sdk(
        {"content": [{"name": "Bash", "input": {"command": "x"}}]}
    )

    out = redactor.redact(record)

    assert isinstance(out, SdkMessageRecord)
    assert out.run_id == record.run_id
    assert out.attempt_number == record.attempt_number
    assert out.iteration_number == record.iteration_number
    assert out.message_type == record.message_type
    assert out.ts == record.ts
    assert out.sequence == record.sequence
    assert out.id == record.id


def test_tool_denylist_redactor_ignores_non_denylisted_tools() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    record = _sdk(
        {"content": [{"name": "Read", "input": {"file": "ok.txt"}}]}
    )

    out = redactor.redact(record)

    assert out.payload == {
        "content": [{"name": "Read", "input": {"file": "ok.txt"}}]
    }


def test_tool_denylist_redactor_recurses_into_nested_blocks() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    record = _sdk(
        {
            "outer": {
                "wrapper": [
                    {"name": "Bash", "input": {"command": "rm -rf /"}},
                    {"name": "Read", "input": {"file": "x"}},
                ]
            }
        }
    )

    out = redactor.redact(record)

    nested = out.payload["outer"]["wrapper"]
    assert nested[0]["input"] == "[REDACTED:tool:Bash]"
    assert nested[1]["input"] == {"file": "x"}


def test_tool_denylist_redactor_event_record_is_identity_but_fresh() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    payload = {"content": [{"name": "Bash", "input": {"command": "rm -rf /"}}]}
    record = _event(payload)

    out = redactor.redact(record)

    # FR-6 scopes denylist behavior to SdkMessageRecord; on EventRecord the
    # redactor is identity but must still return a fresh container.
    assert isinstance(out, EventRecord)
    assert out.payload == payload
    assert out.payload is not record.payload


def test_tool_denylist_redactor_no_mutation_of_input_payload() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    payload = {"content": [{"name": "Bash", "input": {"command": "ls"}}]}
    snapshot = {"content": [{"name": "Bash", "input": {"command": "ls"}}]}
    record = _sdk(payload)

    redactor.redact(record)

    assert payload == snapshot
    assert payload["content"][0]["input"] == {"command": "ls"}


def test_tool_denylist_redactor_rejects_empty_tool_name() -> None:
    with pytest.raises(ValueError):
        ToolDenylistRedactor([""])


def test_tool_denylist_redactor_satisfies_protocol() -> None:
    redactor = ToolDenylistRedactor(["Bash"])
    assert isinstance(redactor, Redactor)


# --- default_policy (FR-3) --------------------------------------------------


def test_default_policy_returns_a_redactor() -> None:
    assert isinstance(default_policy(), Redactor)


def test_default_policy_redacts_anthropic_key() -> None:
    redactor = default_policy()
    record = _event(
        {"prompt": "ANTHROPIC_API_KEY=sk-ant-abc123DEF456ghi789JKLmno"}
    )

    out = redactor.redact(record)

    assert "sk-ant-" not in out.payload["prompt"]
    assert "[REDACTED:anthropic_key]" in out.payload["prompt"]


def test_default_policy_redacts_generic_sk_api_key() -> None:
    redactor = default_policy()
    record = _event({"prompt": "key=sk-proj1234567890abcdefghij"})

    out = redactor.redact(record)

    assert "sk-proj" not in out.payload["prompt"]
    assert "[REDACTED:generic_api_key]" in out.payload["prompt"]


def test_default_policy_redacts_aws_access_key_id() -> None:
    redactor = default_policy()
    record = _event({"creds": "id=AKIAIOSFODNN7EXAMPLE here"})

    out = redactor.redact(record)

    assert out.payload["creds"] == "id=[REDACTED:aws_access_key_id] here"


def test_default_policy_redacts_aws_secret_access_key() -> None:
    redactor = default_policy()
    record = _event(
        {"creds": "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"}
    )

    out = redactor.redact(record)

    assert (
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        not in out.payload["creds"]
    )
    assert "[REDACTED:aws_secret_access_key]" in out.payload["creds"]


def test_default_policy_redacts_pem_private_key() -> None:
    redactor = default_policy()
    pem = (
        "-----BEGIN RSA PRIVATE KEY-----\n"
        "MIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Q\n"
        "u8VCt4N3p/AYbXk7w==\n"
        "-----END RSA PRIVATE KEY-----"
    )
    record = _event({"key": pem})

    out = redactor.redact(record)

    assert out.payload["key"] == "[REDACTED:pem_private_key]"


def test_default_policy_redacts_unprefixed_pem_private_key() -> None:
    redactor = default_policy()
    pem = (
        "-----BEGIN PRIVATE KEY-----\n"
        "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQ==\n"
        "-----END PRIVATE KEY-----"
    )
    record = _event({"key": pem})

    out = redactor.redact(record)

    assert out.payload["key"] == "[REDACTED:pem_private_key]"


def test_default_policy_redacts_jwt() -> None:
    redactor = default_policy()
    jwt = (
        "eyJhbGciOiJIUzI1NiJ9"
        ".eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ"
        ".SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    )
    record = _event({"auth": jwt})

    out = redactor.redact(record)

    assert "eyJ" not in out.payload["auth"]
    assert "[REDACTED:jwt]" in out.payload["auth"]


def test_default_policy_redacts_github_token() -> None:
    redactor = default_policy()
    record = _event(
        {"auth": "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD"}
    )

    out = redactor.redact(record)

    assert "ghp_" not in out.payload["auth"]
    assert "[REDACTED:github_token]" in out.payload["auth"]


def test_default_policy_redacts_bearer_token() -> None:
    redactor = default_policy()
    record = _event(
        {"header": "Authorization: Bearer abcdef1234567890ABCDEFghij"}
    )

    out = redactor.redact(record)

    assert "Bearer abcdef" not in out.payload["header"]
    assert "[REDACTED:bearer_token]" in out.payload["header"]


def test_default_policy_passes_clean_text_through() -> None:
    redactor = default_policy()
    record = _event({"prompt": "Hello, world. No secrets."})

    out = redactor.redact(record)

    assert out.payload == {"prompt": "Hello, world. No secrets."}


# --- Salted HMAC stable tokens (FR-8 / NFR-3) -------------------------------


def _expected_digest(salt: bytes, cleartext: str) -> str:
    return hmac.new(salt, cleartext.encode("utf-8"), hashlib.sha256).hexdigest()[:8]


def test_pattern_redactor_with_salt_produces_digest_token() -> None:
    salt = b"flywheel-test-salt-1"
    redactor = PatternRedactor([_ANTHROPIC_PATTERN], salt=salt)
    record = _event({"x": "sk-ant-AAA"})

    out = redactor.redact(record)

    expected = f"[REDACTED:anthropic_key:{_expected_digest(salt, 'sk-ant-AAA')}]"
    assert out.payload["x"] == expected


def test_pattern_redactor_salt_yields_stable_token_across_records() -> None:
    salt = b"stable"
    redactor = PatternRedactor([_ANTHROPIC_PATTERN], salt=salt)
    r1 = _event({"a": "sk-ant-SAME"})
    r2 = _sdk({"b": "sk-ant-SAME"})

    out1 = redactor.redact(r1)
    out2 = redactor.redact(r2)

    # Identical cleartext -> identical token across records and record types.
    assert out1.payload["a"] == out2.payload["b"]


def test_pattern_redactor_salt_different_cleartext_yields_different_tokens() -> None:
    salt = b"stable"
    redactor = PatternRedactor([_ANTHROPIC_PATTERN], salt=salt)
    record = _event({"a": "sk-ant-AAA", "b": "sk-ant-BBB"})

    out = redactor.redact(record)

    assert out.payload["a"] != out.payload["b"]
    assert out.payload["a"].startswith("[REDACTED:anthropic_key:")
    assert out.payload["b"].startswith("[REDACTED:anthropic_key:")


def test_pattern_redactor_salt_changes_token_digest() -> None:
    record = _event({"x": "sk-ant-AAA"})

    out_a = PatternRedactor([_ANTHROPIC_PATTERN], salt=b"saltA").redact(record)
    out_b = PatternRedactor([_ANTHROPIC_PATTERN], salt=b"saltB").redact(record)

    assert out_a.payload["x"] != out_b.payload["x"]


def test_pattern_redactor_accepts_str_salt() -> None:
    redactor_str = PatternRedactor([_ANTHROPIC_PATTERN], salt="shared")
    redactor_bytes = PatternRedactor([_ANTHROPIC_PATTERN], salt=b"shared")
    record = _event({"x": "sk-ant-AAA"})

    out_str = redactor_str.redact(record)
    out_bytes = redactor_bytes.redact(record)

    # str salt is encoded as UTF-8; the two redactors are equivalent.
    assert out_str.payload["x"] == out_bytes.payload["x"]


def test_pattern_redactor_rejects_invalid_salt_type() -> None:
    with pytest.raises(TypeError):
        PatternRedactor([_ANTHROPIC_PATTERN], salt=12345)  # type: ignore[arg-type]


def test_env_value_redactor_salt_yields_digest_token(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_HMAC", "secretpayloadvalue1234")
    salt = b"keyed"
    redactor = EnvValueRedactor(["FLYWHEEL_HMAC"], salt=salt)
    record = _event({"prompt": "leak=secretpayloadvalue1234"})

    out = redactor.redact(record)

    expected_digest = _expected_digest(salt, "secretpayloadvalue1234")
    assert out.payload["prompt"] == (
        f"leak=[REDACTED:FLYWHEEL_HMAC:{expected_digest}]"
    )


def test_field_path_redactor_salt_yields_digest_for_string_values() -> None:
    salt = b"keyed"
    redactor = FieldPathRedactor(["nested.k"], salt=salt)
    record = _event({"nested": {"k": "hello-world-secret"}})

    out = redactor.redact(record)

    expected_digest = _expected_digest(salt, "hello-world-secret")
    assert out.payload["nested"]["k"] == (
        f"[REDACTED:field:{expected_digest}]"
    )


def test_field_path_redactor_salt_bare_token_for_non_string_value() -> None:
    redactor = FieldPathRedactor(["count"], salt=b"keyed")
    record = _event({"count": 42})

    out = redactor.redact(record)

    # Non-string cleartext has no defined UTF-8 digest; fall back to the
    # bare token.
    assert out.payload["count"] == "[REDACTED:field]"


def test_tool_denylist_redactor_salt_yields_digest_for_string_input() -> None:
    salt = b"keyed"
    redactor = ToolDenylistRedactor(["Bash"], salt=salt)
    record = _sdk(
        {"content": [{"name": "Bash", "input": "rm -rf /", "id": "tu-1"}]}
    )

    out = redactor.redact(record)

    expected_digest = _expected_digest(salt, "rm -rf /")
    assert out.payload["content"][0]["input"] == (
        f"[REDACTED:tool:Bash:{expected_digest}]"
    )


def test_default_policy_accepts_salt() -> None:
    salt = b"flywheel-salt"
    redactor = default_policy(salt=salt)
    record = _event({"x": "sk-ant-AAAAAAAAAAAAAAAA"})

    out = redactor.redact(record)

    assert out.payload["x"].startswith("[REDACTED:anthropic_key:")
    # Digest is 8 hex chars.
    digest_part = out.payload["x"].rsplit(":", 1)[1].rstrip("]")
    assert len(digest_part) == 8
    assert all(c in "0123456789abcdef" for c in digest_part)


def test_token_output_byte_identical_across_redactor_instances() -> None:
    """NFR-3: same record + same salt = byte-identical output every time."""

    salt = b"reproducible"
    record = _event({"x": "sk-ant-AAA", "log": "Bearer abcdefghij1234567890"})

    a = default_policy(salt=salt).redact(record)
    b = default_policy(salt=salt).redact(record)

    assert a.payload == b.payload


# --- NFR-2: compiled regexes are reused -------------------------------------


def test_pattern_redactor_compiles_each_regex_once() -> None:
    """A redactor must hold compiled pattern objects, not raw strings, so
    successive ``redact`` calls do not recompile."""

    redactor = PatternRedactor([_ANTHROPIC_PATTERN])
    # Drive a few records through, then assert the cached pattern is
    # still the same compiled object (no recompilation).
    first_pattern = redactor._patterns[0][1]  # type: ignore[attr-defined]
    for _ in range(5):
        redactor.redact(_event({"x": "sk-ant-AAA"}))
    second_pattern = redactor._patterns[0][1]  # type: ignore[attr-defined]
    assert first_pattern is second_pattern
    import re

    assert isinstance(first_pattern, re.Pattern)


# --- Construction-time failure (Error handling) -----------------------------


def test_default_policy_construction_does_not_raise() -> None:
    # The default patterns must be valid regexes; if any is broken, this
    # call would raise a ValueError. Guards against silent regression.
    default_policy()
    default_policy(salt=b"x")
