import pytest

from flywheel.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    DuplicateEnvelope,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
    parse_envelope,
)


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


class TestValidEnvelopes:
    def test_minimal_envelope_with_only_intent(self) -> None:
        output = _wrap('{"intent": "continue"}')
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.CONTINUE
        assert result.reason is None
        assert result.kind == "valid"

    def test_envelope_with_intent_and_reason(self) -> None:
        output = _wrap('{"intent": "blocked", "reason": "needs API key"}')
        result = parse_envelope(output)
        assert isinstance(result, ValidEnvelope)
        assert result.intent is Intent.BLOCKED
        assert result.reason == "needs API key"

    @pytest.mark.parametrize(
        "intent_value, expected",
        [
            ("verify", Intent.VERIFY),
            ("blocked", Intent.BLOCKED),
            ("continue", Intent.CONTINUE),
            ("abort", Intent.ABORT),
        ],
    )
    def test_each_closed_enum_intent_parses(
        self, intent_value: str, expected: Intent
    ) -> None:
        output = _wrap(f'{{"intent": "{intent_value}"}}')
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
        output = _wrap('{"intent": "blocked", "reason": 123}')
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
        output = "\n".join(
            _wrap(f'{{"intent": "{intent}"}}')
            for intent in ("continue", "verify", "blocked")
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
