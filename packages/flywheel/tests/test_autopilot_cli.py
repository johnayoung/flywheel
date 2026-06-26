"""Tests for the ``flywheel autopilot`` verb routing (spec 00058, autopilot-loop).

The product shell must register an ``autopilot`` verb that delegates to the
orchestrator's autopilot entry (mirroring how ``worker`` is wired), and the
``--once`` flag must reach the single-pass runner.
"""

from __future__ import annotations

import pytest

import flywheel._cli as cli
from flywheel import main


def test_autopilot_verb_in_help(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["--help"]) == 0
    out = capsys.readouterr().out
    assert "autopilot" in out


def test_autopilot_help_forwards_to_the_autopilot_parser(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw autopilot --help`` reaches the autopilot parser (exit 0)."""
    with pytest.raises(SystemExit) as excinfo:
        main(["autopilot", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    # The autopilot parser's own description mentions its neverending nature
    # and the --once single pass.
    assert "neverending" in out
    assert "--once" in out


def test_autopilot_verb_routes_to_the_orchestrator_entry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The verb dispatches to the autopilot entry, not the worker."""
    captured: dict[str, object] = {}

    def _fake_autopilot(argv: object) -> int:
        captured["argv"] = argv
        return 0

    def _fail_worker(argv: object) -> int:  # pragma: no cover - must not run
        raise AssertionError("autopilot must not route to the worker")

    monkeypatch.setattr(cli, "_autopilot_main", _fake_autopilot)
    monkeypatch.setattr(cli, "_worker_main", _fail_worker)

    assert main(["autopilot", "--once"]) == 0
    assert captured["argv"] == ["--once"]
