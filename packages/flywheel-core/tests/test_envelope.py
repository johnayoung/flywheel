import pytest

from flywheel_core.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    CommandGraderRequirement,
    DuplicateEnvelope,
    EnvVarSetRequirement,
    FileExistsRequirement,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
    parse_envelope,
)


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


_BLOCKED_REQUIRES_JSON = (
    '"requires": [{"type": "command_grader", "name": "full-suite"}]'
)


class TestValidEnvelopes:
    def test_minimal_envelope_with_only_intent(self) -> None:
        output = _wrap('{"intent": "continue"}')
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.CONTINUE
        assert result.reason is None
        assert result.requires == ()
        assert result.kind == "valid"

    def test_envelope_with_intent_and_reason(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "reason": "needs API key", '
            + _BLOCKED_REQUIRES_JSON
            + "}"
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.BLOCKED
        assert result.reason == "needs API key"
        assert len(result.requires) == 1
        assert isinstance(result.requires[0], CommandGraderRequirement)
        assert result.requires[0].name == "full-suite"

    @pytest.mark.parametrize(
        "intent_value, expected, extras",
        [
            ("verify", Intent.VERIFY, ""),
            ("blocked", Intent.BLOCKED, ", " + _BLOCKED_REQUIRES_JSON),
            ("continue", Intent.CONTINUE, ""),
            ("abort", Intent.ABORT, ""),
        ],
    )
    def test_each_closed_enum_intent_parses(
        self, intent_value: str, expected: Intent, extras: str
    ) -> None:
        output = _wrap(f'{{"intent": "{intent_value}"{extras}}}')
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is expected

    def test_surrounding_text_outside_fences_is_ignored(self) -> None:
        output = (
            "Some prose the agent wrote before signaling intent.\n"
            "Even with newlines and trailing characters around the fences.\n"
            + _wrap('{"intent": "verify", "reason": "ready for graders"}')
            + "\n\nA trailing note from the agent.\n"
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.VERIFY
        assert result.reason == "ready for graders"

    def test_envelope_with_extra_json_fields_is_accepted(self) -> None:
        output = _wrap(
            '{"intent": "continue", "reason": "more tools to run", "extra": 1}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.CONTINUE


class TestMissingEnvelope:
    def test_empty_string_is_missing(self) -> None:
        assert isinstance(parse_envelope(""), MissingEnvelope)

    def test_output_without_any_fence_is_missing(self) -> None:
        output = "All my chatter, no envelope at all."
        result = parse_envelope(output)
        assert isinstance(result, MissingEnvelope)
        assert result.kind == "missing"


class TestTruncatedEnvelope:
    def test_opening_fence_without_closing_fence(self) -> None:
        output = f'{OPENING_FENCE}\n{{"intent": "continue"'
        result = parse_envelope(output)
        assert isinstance(result, TruncatedEnvelope)
        assert result.kind == "truncated"
        assert "opening fence" in result.detail

    def test_opening_fence_with_no_payload(self) -> None:
        output = OPENING_FENCE
        result = parse_envelope(output)
        assert isinstance(result, TruncatedEnvelope)


class TestMalformedEnvelope:
    def test_closing_fence_without_opening_fence(self) -> None:
        output = f'{{"intent": "continue"}}\n{CLOSING_FENCE}'
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "closing fence" in result.reason

    def test_closing_fence_before_opening_fence(self) -> None:
        output = (
            f"{CLOSING_FENCE}\n"
            '{"intent": "continue"}\n'
            f"{OPENING_FENCE}"
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)

    def test_payload_is_not_json(self) -> None:
        output = _wrap("not json at all")
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "JSON" in result.reason
        assert result.offending is not None
        assert "not json" in result.offending

    def test_payload_json_is_array_not_object(self) -> None:
        output = _wrap('["intent", "continue"]')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "JSON object" in result.reason

    def test_payload_missing_intent_field(self) -> None:
        output = _wrap('{"reason": "no intent here"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "intent" in result.reason

    def test_intent_outside_closed_enum_is_malformed_not_coerced(self) -> None:
        output = _wrap('{"intent": "done"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "intent" in result.reason
        assert "'done'" in result.reason

    def test_intent_value_must_be_a_string(self) -> None:
        output = _wrap('{"intent": 42}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "intent" in result.reason

    def test_intent_capitalization_does_not_coerce(self) -> None:
        # The closed enum is exactly the lowercase values; uppercase forms
        # must be rejected, not silently lowercased.
        output = _wrap('{"intent": "Continue"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)

    def test_reason_must_be_string_when_present(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "reason": 123, '
            + _BLOCKED_REQUIRES_JSON
            + "}"
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "reason" in result.reason

    def test_offending_substring_is_surfaced(self) -> None:
        # Long non-JSON payload — verify offending field carries the substring
        # so callers can build informative protocol-failure events.
        payload = "this-is-not-json " * 25
        output = _wrap(payload)
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert result.offending is not None
        assert "this-is-not-json" in result.offending

    def test_deeply_nested_payload_is_malformed_not_recursion_error(
        self,
    ) -> None:
        # Untrusted agent output: a payload nested far beyond the JSON
        # scanner's recursion limit must map to MalformedEnvelope, not leak a
        # RecursionError out of this closed-contract parser and crash the run.
        depth = 20000
        output = _wrap("[" * depth + "]" * depth)
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "deep" in result.reason


class TestDuplicateEnvelope:
    def test_two_complete_envelopes_in_same_output(self) -> None:
        output = (
            _wrap('{"intent": "continue"}')
            + "\n"
            + _wrap('{"intent": "verify"}')
        )
        result = parse_envelope(output)
        assert isinstance(result, DuplicateEnvelope)
        assert result.kind == "duplicate"
        assert result.count == 2

    def test_three_envelopes(self) -> None:
        blocked_payload = (
            '{"intent": "blocked", ' + _BLOCKED_REQUIRES_JSON + "}"
        )
        output = "\n".join(
            [
                _wrap('{"intent": "continue"}'),
                _wrap('{"intent": "verify"}'),
                _wrap(blocked_payload),
            ]
        )
        result = parse_envelope(output)
        assert isinstance(result, DuplicateEnvelope)
        assert result.count == 3

    def test_duplicate_takes_precedence_even_when_second_intent_is_unknown(
        self,
    ) -> None:
        # We must not silently take the last (or first) — duplicate is its own
        # outcome irrespective of payload validity.
        output = (
            _wrap('{"intent": "continue"}')
            + "\n"
            + _wrap('{"intent": "bogus"}')
        )
        result = parse_envelope(output)
        assert isinstance(result, DuplicateEnvelope)


class TestApiContract:
    def test_parse_envelope_rejects_non_string_input(self) -> None:
        with pytest.raises(TypeError):
            parse_envelope(b"<!-- LOOP_STATUS -->")  # type: ignore[arg-type]

    def test_intent_enum_values_match_loop_spec_exactly(self) -> None:
        # The closed enum from docs/loop.md. Adding a new intent here would
        # silently expand the protocol; the test exists to prevent that.
        assert {member.value for member in Intent} == {
            "verify",
            "blocked",
            "continue",
            "abort",
        }


class TestBlockedRequires:
    def test_command_grader_predicate_parses(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "command_grader", "name": "full-suite"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.BLOCKED
        assert result.requires == (CommandGraderRequirement(name="full-suite"),)

    def test_file_exists_predicate_parses_with_explicit_present_true(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", '
            '"path": ".flywheel/lkg/.venv", "present": true}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.requires == (
            FileExistsRequirement(path=".flywheel/lkg/.venv", present=True),
        )

    def test_file_exists_predicate_defaults_present_to_true(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", "path": "/tmp/x"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert len(result.requires) == 1
        req = result.requires[0]
        assert isinstance(req, FileExistsRequirement)
        assert req.path == "/tmp/x"
        assert req.present is True

    def test_file_exists_predicate_accepts_present_false(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", "path": "/tmp/x", '
            '"present": false}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        req = result.requires[0]
        assert isinstance(req, FileExistsRequirement)
        assert req.present is False

    def test_env_var_set_predicate_parses(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "env_var_set", "name": "ANTHROPIC_API_KEY"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.requires == (
            EnvVarSetRequirement(name="ANTHROPIC_API_KEY"),
        )

    def test_multiple_predicates_preserve_order(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": ['
            '{"type": "command_grader", "name": "full-suite"}, '
            '{"type": "file_exists", "path": "/tmp/x"}, '
            '{"type": "env_var_set", "name": "FOO"}'
            "]}"
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.requires == (
            CommandGraderRequirement(name="full-suite"),
            FileExistsRequirement(path="/tmp/x", present=True),
            EnvVarSetRequirement(name="FOO"),
        )

    def test_duplicate_predicate_entries_accepted_without_dedup(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": ['
            '{"type": "file_exists", "path": "/tmp/x"}, '
            '{"type": "file_exists", "path": "/tmp/x"}'
            "]}"
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert len(result.requires) == 2

    def test_blocked_envelope_missing_requires_is_malformed(self) -> None:
        output = _wrap('{"intent": "blocked", "reason": "no requires"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "requires" in result.reason

    def test_blocked_envelope_with_non_list_requires_is_malformed(self) -> None:
        output = _wrap('{"intent": "blocked", "requires": "full-suite"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "requires" in result.reason
        assert "list" in result.reason

    def test_blocked_envelope_with_empty_requires_list_is_malformed(self) -> None:
        output = _wrap('{"intent": "blocked", "requires": []}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "requires" in result.reason
        assert "at least one" in result.reason

    def test_requires_entry_must_be_object_not_string(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": ["full-suite"]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "requires" in result.reason

    def test_requires_entry_missing_type_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": [{"name": "x"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "type" in result.reason

    def test_requires_entry_with_unknown_type_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "manual_ack", "name": "operator"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "type" in result.reason
        assert "'manual_ack'" in result.reason

    def test_requires_entry_with_non_string_type_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": [{"type": 42}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "type" in result.reason

    def test_command_grader_missing_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": [{"type": "command_grader"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "command_grader" in result.reason
        assert "name" in result.reason

    def test_command_grader_non_string_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "command_grader", "name": 42}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "command_grader" in result.reason
        assert "name" in result.reason

    def test_command_grader_empty_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "command_grader", "name": ""}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "command_grader" in result.reason
        assert "non-empty" in result.reason

    def test_file_exists_missing_path_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": [{"type": "file_exists"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "file_exists" in result.reason
        assert "path" in result.reason

    def test_file_exists_non_string_path_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", "path": 42}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "file_exists" in result.reason
        assert "path" in result.reason

    def test_file_exists_empty_path_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", "path": ""}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "file_exists" in result.reason

    def test_file_exists_non_bool_present_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "file_exists", "path": "/tmp/x", '
            '"present": "yes"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "file_exists" in result.reason
        assert "present" in result.reason

    def test_env_var_set_missing_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", "requires": [{"type": "env_var_set"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "env_var_set" in result.reason
        assert "name" in result.reason

    def test_env_var_set_non_string_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "env_var_set", "name": 42}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "env_var_set" in result.reason
        assert "name" in result.reason

    def test_env_var_set_empty_name_is_malformed(self) -> None:
        output = _wrap(
            '{"intent": "blocked", '
            '"requires": [{"type": "env_var_set", "name": ""}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert "env_var_set" in result.reason
        assert "non-empty" in result.reason

    def test_offending_snippet_present_on_requires_rejection(self) -> None:
        output = _wrap('{"intent": "blocked", "requires": "not-a-list"}')
        result = parse_envelope(output)
        assert isinstance(result, MalformedEnvelope)
        assert result.offending is not None
        assert "not-a-list" in result.offending

    def test_non_blocked_intent_with_stray_requires_still_parses_empty(
        self,
    ) -> None:
        # Non-blocked intents do not consult `requires` at all -- even when the
        # payload contains a malformed `requires` field, it is treated as an
        # ignored extra field (mirrors the existing "extra fields accepted"
        # contract). ValidEnvelope.requires is empty.
        output = _wrap(
            '{"intent": "continue", "requires": "this is syntactically broken"}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.CONTINUE
        assert result.requires == ()

    def test_non_blocked_intent_with_broken_requires_list_still_parses(
        self,
    ) -> None:
        output = _wrap(
            '{"intent": "verify", '
            '"requires": [{"type": "totally_bogus_predicate"}]}'
        )
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.VERIFY
        assert result.requires == ()
