"""Tests for the ``flywheel triage`` verb routing (spec 00082, triage-verb-daemon).

The product shell must register a ``triage`` verb that delegates to the
orchestrator's triage entry (mirroring how ``worker`` and ``autopilot`` are
wired), and the ``--once`` flag must reach the single-pass runner.
"""

from __future__ import annotations

import pytest

import flywheel._cli as cli
from flywheel import main


def test_triage_verb_in_help(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["--help"]) == 0
    out = capsys.readouterr().out
    assert "triage" in out


def test_triage_help_forwards_to_the_triage_parser(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw triage --help`` reaches the triage parser (exit 0)."""
    with pytest.raises(SystemExit) as excinfo:
        main(["triage", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    # The triage parser's own description mentions its neverending nature and
    # the --once single pass.
    assert "neverending" in out
    assert "--once" in out


def test_triage_verb_routes_to_the_orchestrator_entry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The verb dispatches to the triage entry, not the worker or autopilot."""
    captured: dict[str, object] = {}

    def _fake_triage(argv: object) -> int:
        captured["argv"] = argv
        return 0

    def _fail_worker(argv: object) -> int:  # pragma: no cover - must not run
        raise AssertionError("triage must not route to the worker")

    def _fail_autopilot(argv: object) -> int:  # pragma: no cover - must not run
        raise AssertionError("triage must not route to autopilot")

    monkeypatch.setattr(cli, "_triage_main", _fake_triage)
    monkeypatch.setattr(cli, "_worker_main", _fail_worker)
    monkeypatch.setattr(cli, "_autopilot_main", _fail_autopilot)

    assert main(["triage", "--once"]) == 0
    assert captured["argv"] == ["--once"]


def test_triage_help_advertises_the_daemon_interval(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The daemon exposes an --interval knob (the between-cycle wait)."""
    with pytest.raises(SystemExit) as excinfo:
        main(["triage", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    assert "--interval" in out
