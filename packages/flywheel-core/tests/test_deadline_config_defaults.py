"""Behavioral coverage for :mod:`flywheel_core.deadline_config`.

Cross-references criterion #7 of
``.flywheel/specs/00066-FEATURE-universal-deadlines.md``: with the default
``flywheel.toml`` (no deadline overrides) the resolved configuration yields a
finite, non-null ceiling for each of the five external-call classes, and an
explicit operator override changes the resolved ceiling for that class --
including the ``0`` / unbounded opt-out remaining expressible.
"""

import math

import pytest

from flywheel_core.deadline_config import (
    DEFAULT_DEADLINE_SECONDS,
    DeadlineClass,
    DeadlineConfig,
    deadline_config_from_mapping,
    resolve_deadlines,
)

# The five call classes the spec enumerates; the suite asserts the resolver
# covers exactly these, so a future class cannot be added without coverage.
_ALL_CLASSES = (
    DeadlineClass.AGENT_ITERATION,
    DeadlineClass.RUBRIC_JUDGE,
    DeadlineClass.COMMAND_GRADER,
    DeadlineClass.DOCKER_MANAGEMENT,
    DeadlineClass.AUTOPILOT_AGENT,
)


def _is_finite_positive(value: float | None) -> bool:
    return (
        value is not None
        and isinstance(value, float)
        and math.isfinite(value)
        and value > 0
    )


# --- default-on: no override yields finite, non-null ceilings ---


def test_enum_covers_exactly_the_five_call_classes() -> None:
    assert tuple(DeadlineClass) == _ALL_CLASSES
    assert set(DEFAULT_DEADLINE_SECONDS) == set(_ALL_CLASSES)


def test_default_config_yields_finite_non_null_ceiling_for_every_class() -> None:
    config = resolve_deadlines()
    for deadline_class in _ALL_CLASSES:
        ceiling = config.for_class(deadline_class)
        assert _is_finite_positive(ceiling), (
            f"{deadline_class} resolved to a non-finite/null default: {ceiling}"
        )


def test_default_constructed_dataclass_is_finite_for_every_class() -> None:
    config = DeadlineConfig()
    for deadline_class in _ALL_CLASSES:
        assert _is_finite_positive(config.for_class(deadline_class))


def test_empty_override_mapping_matches_no_override() -> None:
    assert resolve_deadlines({}) == resolve_deadlines()
    assert resolve_deadlines(None) == resolve_deadlines()


def test_resolved_defaults_match_the_published_default_table() -> None:
    config = resolve_deadlines()
    for deadline_class in _ALL_CLASSES:
        assert (
            config.for_class(deadline_class)
            == DEFAULT_DEADLINE_SECONDS[deadline_class]
        )


# --- explicit override changes the resolved ceiling ---


@pytest.mark.parametrize("deadline_class", _ALL_CLASSES)
def test_explicit_override_is_reflected_exactly(
    deadline_class: DeadlineClass,
) -> None:
    magnitude = 42.5
    config = resolve_deadlines({deadline_class: magnitude})
    assert config.for_class(deadline_class) == magnitude
    # Other classes keep their finite defaults -- the override is per-class.
    for other in _ALL_CLASSES:
        if other is deadline_class:
            continue
        assert (
            config.for_class(other) == DEFAULT_DEADLINE_SECONDS[other]
        )


def test_integer_override_is_coerced_to_float() -> None:
    config = resolve_deadlines({DeadlineClass.COMMAND_GRADER: 30})
    resolved = config.for_class(DeadlineClass.COMMAND_GRADER)
    assert resolved == 30.0
    assert isinstance(resolved, float)


def test_overriding_all_five_changes_every_resolved_value() -> None:
    overrides: dict[DeadlineClass, float | int | None] = {
        DeadlineClass.AGENT_ITERATION: 1.0,
        DeadlineClass.RUBRIC_JUDGE: 2.0,
        DeadlineClass.COMMAND_GRADER: 3.0,
        DeadlineClass.DOCKER_MANAGEMENT: 4.0,
        DeadlineClass.AUTOPILOT_AGENT: 5.0,
    }
    config = resolve_deadlines(overrides)
    for deadline_class, expected in overrides.items():
        assert config.for_class(deadline_class) == expected


# --- the unbounded / 0 opt-out remains expressible per class ---


@pytest.mark.parametrize("deadline_class", _ALL_CLASSES)
def test_zero_override_opts_the_class_out(
    deadline_class: DeadlineClass,
) -> None:
    config = resolve_deadlines({deadline_class: 0})
    assert config.for_class(deadline_class) is None


@pytest.mark.parametrize("deadline_class", _ALL_CLASSES)
def test_none_override_opts_the_class_out(
    deadline_class: DeadlineClass,
) -> None:
    config = resolve_deadlines({deadline_class: None})
    assert config.for_class(deadline_class) is None


@pytest.mark.parametrize("deadline_class", _ALL_CLASSES)
def test_opt_out_does_not_snap_back_to_the_default(
    deadline_class: DeadlineClass,
) -> None:
    """An explicit 0/None opt-out must resolve to unbounded, not the default.

    This is the load-bearing distinction: presence-of-key, not value, decides
    "kept the default" vs "opted out", so the opt-out cannot collapse into the
    finite default.
    """

    opted_out = resolve_deadlines({deadline_class: 0})
    assert opted_out.for_class(deadline_class) is None
    assert (
        opted_out.for_class(deadline_class)
        != DEFAULT_DEADLINE_SECONDS[deadline_class]
    )


def test_negative_override_is_treated_as_opt_out() -> None:
    config = resolve_deadlines({DeadlineClass.DOCKER_MANAGEMENT: -1})
    assert config.for_class(DeadlineClass.DOCKER_MANAGEMENT) is None


# --- TOML mapping path ---


def test_mapping_path_with_no_overrides_matches_defaults() -> None:
    assert deadline_config_from_mapping({}) == resolve_deadlines()
    assert deadline_config_from_mapping(None) == resolve_deadlines()


def test_mapping_path_applies_per_class_overrides() -> None:
    config = deadline_config_from_mapping(
        {
            "agent_iteration_seconds": 120,
            "command_grader_seconds": 0,  # opt-out spelled as 0 on disk
        }
    )
    assert config.for_class(DeadlineClass.AGENT_ITERATION) == 120.0
    assert config.for_class(DeadlineClass.COMMAND_GRADER) is None
    # Untouched classes keep their finite defaults.
    assert (
        config.for_class(DeadlineClass.RUBRIC_JUDGE)
        == DEFAULT_DEADLINE_SECONDS[DeadlineClass.RUBRIC_JUDGE]
    )


def test_mapping_path_ignores_unknown_keys() -> None:
    config = deadline_config_from_mapping({"not_a_class_seconds": 7})
    assert config == resolve_deadlines()


def test_mapping_path_rejects_non_numeric_value() -> None:
    with pytest.raises(ValueError, match="agent_iteration_seconds"):
        deadline_config_from_mapping({"agent_iteration_seconds": "soon"})


def test_mapping_path_rejects_boolean_value() -> None:
    # bool is an int subclass; a TOML ``true`` must not be read as ``1``.
    with pytest.raises(ValueError):
        deadline_config_from_mapping({"rubric_judge_seconds": True})


# --- accessor totality ---


def test_for_class_rejects_non_member() -> None:
    config = resolve_deadlines()
    with pytest.raises(ValueError):
        config.for_class("agent_iteration")  # type: ignore[arg-type]
