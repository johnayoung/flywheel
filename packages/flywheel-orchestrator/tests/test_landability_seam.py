"""Tests for the optional landability seam on ``SubmitStrategy`` (spec 00061,
layer predicate-seam).

The seam is the default-bearing half of the landable-change gate: a git-aware
strategy implements :data:`LandabilityProbe.is_landable` and answers for itself,
while a strategy with no notion of a diff (research/config, non-git) implements
nothing and is treated as always-landable. :func:`probe_landability` is the
helper the orchestrator gate (a later layer) will call to get that default for
free, so non-code task classes opt out of the gate entirely (criterion #3).
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import (
    LANDABLE,
    LandabilityProbe,
    LandabilityVerdict,
    SubmitRequest,
    probe_landability,
)


def _submit_req() -> SubmitRequest:
    return SubmitRequest(
        task_id="t1",
        task_file=Path("active/01-phase/t1.json"),
        task=Task(id="t1", goal="Goal.", graders=[CommandGrader(run="true")]),
        run_id="run-1",
        status=Status.DONE,
        sandbox=Path("/tmp/sandbox/t1"),
    )


class _NoDiffStrategy:
    """A strategy with no notion of a diff: it supplies no ``is_landable``."""

    def prepare_sandbox(self, request: object, /) -> Path:
        return Path("/tmp/sandbox")

    def submit(self, request: object, /) -> None:
        return None


class _GitishStrategy:
    """A strategy that supplies the optional predicate."""

    def __init__(self, verdict: LandabilityVerdict) -> None:
        self._verdict = verdict
        self.calls: list[SubmitRequest] = []

    def prepare_sandbox(self, request: object, /) -> Path:
        return Path("/tmp/sandbox")

    def submit(self, request: object, /) -> None:
        return None

    def is_landable(self, request: SubmitRequest, /) -> LandabilityVerdict:
        self.calls.append(request)
        return self._verdict


def test_landable_default_constant() -> None:
    assert LANDABLE == LandabilityVerdict(landable=True)
    assert LANDABLE.landable is True
    assert LANDABLE.reason == ""


def test_verdict_carries_reason() -> None:
    v = LandabilityVerdict(landable=False, reason="no commits")
    assert v.landable is False
    assert v.reason == "no commits"


def test_no_diff_strategy_defaults_to_landable() -> None:
    # A strategy with no is_landable inherits the always-landable default, so a
    # non-code task class is unaffected by the gate.
    strategy = _NoDiffStrategy()
    assert not isinstance(strategy, LandabilityProbe)

    verdict = probe_landability(strategy, _submit_req())

    assert verdict is LANDABLE
    assert verdict.landable is True


def test_probe_calls_supplied_predicate() -> None:
    expected = LandabilityVerdict(landable=False, reason="uncommitted tree")
    strategy = _GitishStrategy(expected)
    assert isinstance(strategy, LandabilityProbe)

    req = _submit_req()
    verdict = probe_landability(strategy, req)

    assert verdict == expected
    assert strategy.calls == [req]  # the helper routed to the strategy


def test_probe_passes_through_landable_predicate() -> None:
    strategy = _GitishStrategy(LandabilityVerdict(landable=True))

    verdict = probe_landability(strategy, _submit_req())

    assert verdict.landable is True
