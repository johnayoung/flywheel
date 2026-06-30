"""Default-on, operator-overridable wall-clock deadline ceilings (spec 00066).

Each of the five external-call classes flywheel issues -- the working agent
iteration, the rubric judge stream, command graders, docker *management* calls,
and the autopilot discovery/authoring agent -- resolves a finite, non-null
wall-clock ceiling here with no operator override required (D-1: default-on,
generous, overridable). An operator may raise/lower the ceiling per class, or
opt a class out (unbounded) by overriding with ``0`` / ``None``.

This module only *resolves* ceilings; it applies none of them. Each dependent
call-site layer (harness, container, orchestrator) reads the resolved value and
feeds it to the shared :mod:`flywheel_core.deadline` primitive. The resolved
value is ``float | None`` where a positive float is the ceiling in seconds and
``None`` is the explicit unbounded opt-out.

The module lives outside the pure ``task``/``lifecycle`` core and carries no
agent-SDK dependency: ``import flywheel_core`` works without the ``claude``
extra.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from typing import Final


class DeadlineClass(str, Enum):
    """The five external-call classes governed by a wall-clock deadline.

    Each member names one call site layer that adopts the shared deadline
    primitive; the resolved ceiling for a class is read via
    :meth:`DeadlineConfig.for_class`.
    """

    AGENT_ITERATION = "agent_iteration"
    RUBRIC_JUDGE = "rubric_judge"
    COMMAND_GRADER = "command_grader"
    DOCKER_MANAGEMENT = "docker_management"
    AUTOPILOT_AGENT = "autopilot_agent"


# Generous, finite default ceilings (seconds). Magnitudes are chosen to clear a
# legitimately slow call of each class with headroom -- the requirement is
# finite + default-on, not a "correct" number (spec 00066 non-goal). They align
# with existing bounds where one exists (docker `start_container` is 120s; the
# container `exec_timeout` default is 1800s).
DEFAULT_AGENT_ITERATION_SECONDS: Final[float] = 1800.0
DEFAULT_RUBRIC_JUDGE_SECONDS: Final[float] = 600.0
DEFAULT_COMMAND_GRADER_SECONDS: Final[float] = 900.0
DEFAULT_DOCKER_MANAGEMENT_SECONDS: Final[float] = 120.0
DEFAULT_AUTOPILOT_AGENT_SECONDS: Final[float] = 1800.0

DEFAULT_DEADLINE_SECONDS: Final[Mapping[DeadlineClass, float]] = {
    DeadlineClass.AGENT_ITERATION: DEFAULT_AGENT_ITERATION_SECONDS,
    DeadlineClass.RUBRIC_JUDGE: DEFAULT_RUBRIC_JUDGE_SECONDS,
    DeadlineClass.COMMAND_GRADER: DEFAULT_COMMAND_GRADER_SECONDS,
    DeadlineClass.DOCKER_MANAGEMENT: DEFAULT_DOCKER_MANAGEMENT_SECONDS,
    DeadlineClass.AUTOPILOT_AGENT: DEFAULT_AUTOPILOT_AGENT_SECONDS,
}

# TOML ``[deadlines]`` key names, one per class. An omitted key keeps the
# default; a key set to ``0`` opts the class out (unbounded). TOML cannot
# express ``None``, so ``0`` is the on-disk spelling of the unbounded opt-out.
_TABLE_KEYS: Final[Mapping[DeadlineClass, str]] = {
    DeadlineClass.AGENT_ITERATION: "agent_iteration_seconds",
    DeadlineClass.RUBRIC_JUDGE: "rubric_judge_seconds",
    DeadlineClass.COMMAND_GRADER: "command_grader_seconds",
    DeadlineClass.DOCKER_MANAGEMENT: "docker_management_seconds",
    DeadlineClass.AUTOPILOT_AGENT: "autopilot_agent_seconds",
}


def _normalize_override(value: float | int | None) -> float | None:
    """Map a raw per-class override to a resolved ceiling.

    ``None`` and any value ``<= 0`` are the unbounded opt-out and resolve to
    ``None``; a positive value resolves to itself as a ``float``. This is the
    single place that distinguishes "operator opted this class out" from
    "operator gave no override" -- the latter never reaches here (the caller
    keys on presence and uses the default instead).
    """

    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError(
            f"deadline override must be a number or None, got {value!r}"
        )
    numeric = float(value)
    if numeric <= 0:
        return None
    return numeric


@dataclass(frozen=True, kw_only=True)
class DeadlineConfig:
    """Resolved wall-clock ceilings for the five external-call classes.

    Each field is ``float | None``: a positive float is the ceiling in seconds,
    ``None`` is the explicit unbounded opt-out. Built via
    :func:`resolve_deadlines`; the default-constructed instance carries the
    finite, non-null defaults for every class (default-on).
    """

    agent_iteration_seconds: float | None = DEFAULT_AGENT_ITERATION_SECONDS
    rubric_judge_seconds: float | None = DEFAULT_RUBRIC_JUDGE_SECONDS
    command_grader_seconds: float | None = DEFAULT_COMMAND_GRADER_SECONDS
    docker_management_seconds: float | None = DEFAULT_DOCKER_MANAGEMENT_SECONDS
    autopilot_agent_seconds: float | None = DEFAULT_AUTOPILOT_AGENT_SECONDS

    def for_class(self, deadline_class: DeadlineClass) -> float | None:
        """Return the resolved ceiling for ``deadline_class``.

        ``None`` means unbounded (the operator opted this class out); a
        positive float is the ceiling in seconds.
        """

        if deadline_class is DeadlineClass.AGENT_ITERATION:
            return self.agent_iteration_seconds
        if deadline_class is DeadlineClass.RUBRIC_JUDGE:
            return self.rubric_judge_seconds
        if deadline_class is DeadlineClass.COMMAND_GRADER:
            return self.command_grader_seconds
        if deadline_class is DeadlineClass.DOCKER_MANAGEMENT:
            return self.docker_management_seconds
        if deadline_class is DeadlineClass.AUTOPILOT_AGENT:
            return self.autopilot_agent_seconds
        raise ValueError(f"unknown deadline class {deadline_class!r}")


def resolve_deadlines(
    overrides: Mapping[DeadlineClass, float | int | None] | None = None,
) -> DeadlineConfig:
    """Resolve the five ceilings, applying per-class overrides over defaults.

    A class absent from ``overrides`` keeps its finite, non-null default
    (default-on). A class present with a positive value takes that value
    exactly; present with ``0`` / a negative number / ``None`` resolves to the
    unbounded opt-out (``None``). Presence -- not value -- distinguishes "kept
    the default" from "opted out", so an explicit unbounded override never
    snaps back to the default.
    """

    overrides = overrides or {}
    resolved: dict[DeadlineClass, float | None] = {}
    for deadline_class in DeadlineClass:
        if deadline_class in overrides:
            resolved[deadline_class] = _normalize_override(
                overrides[deadline_class]
            )
        else:
            resolved[deadline_class] = DEFAULT_DEADLINE_SECONDS[deadline_class]
    return DeadlineConfig(
        agent_iteration_seconds=resolved[DeadlineClass.AGENT_ITERATION],
        rubric_judge_seconds=resolved[DeadlineClass.RUBRIC_JUDGE],
        command_grader_seconds=resolved[DeadlineClass.COMMAND_GRADER],
        docker_management_seconds=resolved[DeadlineClass.DOCKER_MANAGEMENT],
        autopilot_agent_seconds=resolved[DeadlineClass.AUTOPILOT_AGENT],
    )


def deadline_config_from_mapping(
    table: Mapping[str, object] | None,
) -> DeadlineConfig:
    """Resolve a :class:`DeadlineConfig` from a TOML ``[deadlines]`` table.

    Each recognized key (see :data:`_TABLE_KEYS`) must be a number; an omitted
    key keeps the default and a key set to ``0`` opts the class out (TOML has no
    ``None``, so ``0`` is the on-disk unbounded spelling). Unknown keys are
    ignored so a future key does not hard-fail an older reader. A non-numeric
    value is a configuration error.
    """

    table = table or {}
    overrides: dict[DeadlineClass, float | int | None] = {}
    for deadline_class, key in _TABLE_KEYS.items():
        if key not in table:
            continue
        value = table[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"deadlines.{key} must be a number, got {value!r}")
        overrides[deadline_class] = value
    return resolve_deadlines(overrides)


__all__ = [
    "DEFAULT_AGENT_ITERATION_SECONDS",
    "DEFAULT_AUTOPILOT_AGENT_SECONDS",
    "DEFAULT_COMMAND_GRADER_SECONDS",
    "DEFAULT_DEADLINE_SECONDS",
    "DEFAULT_DOCKER_MANAGEMENT_SECONDS",
    "DEFAULT_RUBRIC_JUDGE_SECONDS",
    "DeadlineClass",
    "DeadlineConfig",
    "deadline_config_from_mapping",
    "resolve_deadlines",
]
