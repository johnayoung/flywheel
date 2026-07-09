"""The [deadlines] autopilot ceiling threads from flywheel.toml to the seam.

Spec 00066 shipped the wall-clock ceiling *application* inside
``run_refill_pass`` (it consumes ``deadlines`` and derives the
``AUTOPILOT_AGENT`` ceiling). This oracle grades the missing wire: the
policy-resolved :class:`DeadlineConfig` -- the one an operator sets via
``[deadlines] autopilot_agent_seconds`` in ``flywheel.toml`` -- must actually
reach ``run_refill_pass``, threaded through ``run_single_pass`` and the daemon
entry ``main``. Before the wire existed, the seam always received the built-in
default and any operator override was silently dropped.

These tests capture the ``DeadlineConfig`` handed to ``run_refill_pass`` and
assert it equals the policy-resolved one, covering the finite-override, the
``0`` opt-out (unbounded), and the ``None``-policy (byte-identical default)
cases. No live model and no real agent stream: the seam is monkeypatched to a
recording stand-in.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

from flywheel_core.deadline_config import (
    DEFAULT_AUTOPILOT_AGENT_SECONDS,
    DeadlineClass,
    DeadlineConfig,
)

from flywheel_orchestrator import _autopilot_run
from flywheel_orchestrator._autopilot import DEFAULT_WEIGHTS, AutopilotPassResult
from flywheel_orchestrator._autopilot_run import run_single_pass
from flywheel_orchestrator._policy import load_policy


def _policy(tmp_path: Path, body: str):
    """Write a minimal ``flywheel.toml`` (plus ``body``) and load its policy."""
    path = tmp_path / "flywheel.toml"
    path.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(path)


def _capture_refill_seam(monkeypatch) -> dict[str, object]:
    """Replace ``run_refill_pass`` with a recorder of the ``deadlines`` kwarg.

    Returns the mutable dict the recorder writes into; ``captured["deadlines"]``
    holds whatever the caller forwarded to the seam.
    """
    captured: dict[str, object] = {}

    async def _recording_refill(**kwargs: object) -> AutopilotPassResult:
        captured["deadlines"] = kwargs.get("deadlines")
        captured["called"] = True
        return AutopilotPassResult(reason="captured")

    monkeypatch.setattr(_autopilot_run, "run_refill_pass", _recording_refill)
    return captured


def test_run_single_pass_forwards_finite_override_to_seam(
    tmp_path: Path, monkeypatch
) -> None:
    # A finite [deadlines] override resolves to a DeadlineConfig whose
    # AUTOPILOT_AGENT ceiling is the operator's value; run_single_pass forwards
    # that exact config to the run_refill_pass seam, not the built-in default.
    policy = _policy(tmp_path, "[deadlines]\nautopilot_agent_seconds = 0.05\n")
    captured = _capture_refill_seam(monkeypatch)

    run_single_pass(
        repo_root=tmp_path,
        tasks_dir=tmp_path / "tasks",
        target_depth=5,
        landing="merge",
        weights=DEFAULT_WEIGHTS,
        model=None,
        deadlines=policy.deadlines,
    )

    seam = captured["deadlines"]
    assert isinstance(seam, DeadlineConfig)
    assert seam == policy.deadlines
    assert seam.for_class(DeadlineClass.AUTOPILOT_AGENT) == 0.05
    # It is NOT the built-in default -- the override actually took effect.
    assert (
        seam.for_class(DeadlineClass.AUTOPILOT_AGENT)
        != DEFAULT_AUTOPILOT_AGENT_SECONDS
    )


def test_run_single_pass_forwards_zero_opt_out_to_seam(
    tmp_path: Path, monkeypatch
) -> None:
    # Edge case: autopilot_agent_seconds = 0 opts the class out (unbounded).
    # The seam must receive the resolved None ceiling, never the 1800s default.
    policy = _policy(tmp_path, "[deadlines]\nautopilot_agent_seconds = 0\n")
    assert policy.deadlines.for_class(DeadlineClass.AUTOPILOT_AGENT) is None

    captured = _capture_refill_seam(monkeypatch)
    run_single_pass(
        repo_root=tmp_path,
        tasks_dir=tmp_path / "tasks",
        target_depth=5,
        landing="merge",
        weights=DEFAULT_WEIGHTS,
        model=None,
        deadlines=policy.deadlines,
    )

    seam = captured["deadlines"]
    assert isinstance(seam, DeadlineConfig)
    assert seam == policy.deadlines
    assert seam.for_class(DeadlineClass.AUTOPILOT_AGENT) is None


def test_run_single_pass_none_deadlines_keeps_defaults_byte_identical(
    tmp_path: Path, monkeypatch
) -> None:
    # A None policy (library caller / no flywheel.toml) forwards None unchanged,
    # so run_refill_pass falls back to a default DeadlineConfig -- byte-identical
    # to today's built-in ceilings, not a silently different value.
    captured = _capture_refill_seam(monkeypatch)
    run_single_pass(
        repo_root=tmp_path,
        tasks_dir=tmp_path / "tasks",
        target_depth=5,
        landing="merge",
        weights=DEFAULT_WEIGHTS,
        model=None,
        deadlines=None,
    )

    # None reaches the seam; run_refill_pass maps it to DeadlineConfig(), whose
    # AUTOPILOT_AGENT ceiling is the finite default.
    assert captured["called"] is True
    assert captured["deadlines"] is None
    assert (
        DeadlineConfig().for_class(DeadlineClass.AUTOPILOT_AGENT)
        == DEFAULT_AUTOPILOT_AGENT_SECONDS
    )


def test_main_threads_toml_deadline_override_end_to_end(
    tmp_path: Path, monkeypatch
) -> None:
    # End-to-end: `flywheel autopilot --once` loads flywheel.toml from the
    # working directory and threads its policy-resolved [deadlines] ceiling all
    # the way to the run_refill_pass seam. Proves main -> run_single_pass ->
    # run_refill_pass carry the operator override, not the default.
    _policy(tmp_path, "[deadlines]\nautopilot_agent_seconds = 0.05\n")
    expected = load_policy(tmp_path / "flywheel.toml").deadlines

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(_autopilot_run, "_repo_root", lambda: tmp_path)
    # Queue-depth backing is orthogonal to deadline threading; keep the run
    # hermetic (no store backend) by opting out of the counter.
    monkeypatch.setattr(
        _autopilot_run, "_build_queue_depth", lambda *a, **k: None
    )
    captured = _capture_refill_seam(monkeypatch)

    rc = _autopilot_run.main(["--once"])

    assert rc == 0
    seam = captured["deadlines"]
    assert isinstance(seam, DeadlineConfig)
    assert seam == expected
    assert seam.for_class(DeadlineClass.AUTOPILOT_AGENT) == 0.05


def test_main_no_policy_forwards_none_deadlines(
    tmp_path: Path, monkeypatch
) -> None:
    # No flywheel.toml in the working directory -> no policy -> the seam receives
    # None, keeping the byte-identical default ceilings (not a fabricated one).
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(_autopilot_run, "_repo_root", lambda: tmp_path)
    monkeypatch.setattr(
        _autopilot_run, "_build_queue_depth", lambda *a, **k: None
    )
    captured = _capture_refill_seam(monkeypatch)

    rc = _autopilot_run.main(["--once"])

    assert rc == 0
    assert captured["called"] is True
    assert captured["deadlines"] is None
