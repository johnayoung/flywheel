"""Tests for the claude-code fault classifier and installation probe.

``classify_claude_fault`` is pure; ``probe_claude_code`` runs a real
``--version`` subprocess against ``sys.executable`` with a monkeypatched
``Path.home`` so no real credentials are ever inspected.
"""

from __future__ import annotations

import asyncio
import shutil
import sys
from pathlib import Path

import pytest

from flywheel_agents import (
    AgentProbeResult,
    AssuranceLevel,
    AuthenticationKind,
    FaultEvidence,
)
from flywheel_agents.claude_code._faults import classify_claude_fault
from flywheel_agents.claude_code._probe import probe_claude_code

_OAUTH_ENV = "CLAUDE_CODE_OAUTH_TOKEN"
_API_KEY_ENV = "ANTHROPIC_API_KEY"


def test_pipe_epoch_refusal_carries_reset_instant() -> None:
    fault = classify_claude_fault(
        FaultEvidence(final_text="Claude AI usage limit reached|1751990400")
    )
    assert fault is not None
    assert fault.kind == "session_limit"
    assert fault.resets_at_epoch == 1751990400.0


def test_marker_without_epoch_has_no_reset() -> None:
    fault = classify_claude_fault(
        FaultEvidence(final_text="Claude AI usage limit reached -- resets 6pm")
    )
    assert fault is not None
    assert fault.kind == "session_limit"
    assert fault.resets_at_epoch is None


def test_match_in_stderr_with_empty_final_text() -> None:
    fault = classify_claude_fault(
        FaultEvidence(
            final_text="",
            stderr="Claude AI usage limit reached|1751990400",
        )
    )
    assert fault is not None
    assert fault.kind == "session_limit"
    assert fault.resets_at_epoch == 1751990400.0


def test_no_match_returns_none() -> None:
    evidence = FaultEvidence(
        final_text="all changes applied", stderr="warning: slow network"
    )
    assert classify_claude_fault(evidence) is None


def _isolate(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """Point HOME at tmp_path and clear both auth environment variables."""
    monkeypatch.delenv(_OAUTH_ENV, raising=False)
    monkeypatch.delenv(_API_KEY_ENV, raising=False)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)


def _probe() -> AgentProbeResult:
    return asyncio.run(probe_claude_code())


def test_probe_not_installed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: None)
    result = _probe()
    assert result.installed is False
    assert result.executable_path is None
    assert any("not found" in warning for warning in result.warnings)


def test_probe_installation_only(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    result = _probe()
    assert result.installed is True
    assert result.executable_path == Path(sys.executable)
    assert result.version is not None  # real `python --version` run
    assert result.authentication_kind is AuthenticationKind.INSTALLATION_ONLY
    assert result.authentication_assurance is AssuranceLevel.BEST_EFFORT
    assert result.config_paths == ()


def test_probe_oauth_token_means_account_session(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    monkeypatch.setenv(_OAUTH_ENV, "sk-test-oauth-secret")
    result = _probe()
    assert result.authentication_kind is AuthenticationKind.ACCOUNT_SESSION
    assert result.authentication_assurance is AssuranceLevel.STRONG_INDICATION


def test_probe_api_key_only(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    monkeypatch.setenv(_API_KEY_ENV, "sk-test-api-secret")
    result = _probe()
    assert result.authentication_kind is AuthenticationKind.API_KEY
    assert result.authentication_assurance is AssuranceLevel.STRONG_INDICATION


def _probe_mixed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> AgentProbeResult:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    monkeypatch.setenv(_OAUTH_ENV, "sk-test-oauth-secret")
    monkeypatch.setenv(_API_KEY_ENV, "sk-test-api-secret")
    claude_dir = tmp_path / ".claude"
    claude_dir.mkdir()
    (claude_dir / ".credentials.json").write_text("{}")
    return _probe()


def test_probe_mixed_with_strand_warning(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    result = _probe_mixed(monkeypatch, tmp_path)
    assert result.authentication_kind is AuthenticationKind.MIXED
    assert any("strands the subscription" in w for w in result.warnings)
    assert result.config_paths == (tmp_path / ".claude",)
    assert len(result.authentication_evidence) == 3


def test_probe_evidence_never_contains_secret_values(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    result = _probe_mixed(monkeypatch, tmp_path)
    for text in (*result.authentication_evidence, *result.warnings):
        assert "sk-test" not in text
