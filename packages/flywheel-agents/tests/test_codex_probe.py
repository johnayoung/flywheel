"""Tests for the codex installation and authentication probe.

``probe_codex`` runs a real ``--version`` subprocess against
``sys.executable`` with a monkeypatched ``Path.home`` so no real credentials
are ever inspected.
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
)
from flywheel_agents.codex._probe import probe_codex

_API_KEY_ENV = "OPENAI_API_KEY"


def _isolate(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """Point HOME at tmp_path and clear the API key environment variable."""
    monkeypatch.delenv(_API_KEY_ENV, raising=False)
    monkeypatch.setattr(Path, "home", lambda: tmp_path)


def _probe() -> AgentProbeResult:
    return asyncio.run(probe_codex())


def _write_auth_file(tmp_path: Path) -> Path:
    codex_dir = tmp_path / ".codex"
    codex_dir.mkdir()
    (codex_dir / "auth.json").write_text('{"tokens": "sk-test-account-secret"}')
    return codex_dir


def test_probe_not_installed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: None)
    result = _probe()
    assert result.installed is False
    assert result.executable_path is None
    assert result.authentication_kind is AuthenticationKind.UNKNOWN
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


def test_probe_api_key_only(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    monkeypatch.setenv(_API_KEY_ENV, "sk-test-api-secret")
    result = _probe()
    assert result.authentication_kind is AuthenticationKind.API_KEY
    assert result.authentication_assurance is AssuranceLevel.STRONG_INDICATION


def test_probe_auth_json_means_account_session(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    codex_dir = _write_auth_file(tmp_path)
    result = _probe()
    assert result.authentication_kind is AuthenticationKind.ACCOUNT_SESSION
    assert result.authentication_assurance is AssuranceLevel.STRONG_INDICATION
    assert result.config_paths == (codex_dir,)


def _probe_mixed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> AgentProbeResult:
    _isolate(monkeypatch, tmp_path)
    monkeypatch.setattr(shutil, "which", lambda name: sys.executable)
    monkeypatch.setenv(_API_KEY_ENV, "sk-test-api-secret")
    _write_auth_file(tmp_path)
    return _probe()


def test_probe_mixed_with_shadow_warning(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    result = _probe_mixed(monkeypatch, tmp_path)
    assert result.authentication_kind is AuthenticationKind.MIXED
    assert any("shadow" in w for w in result.warnings)
    assert result.config_paths == (tmp_path / ".codex",)
    assert len(result.authentication_evidence) == 2


def test_probe_evidence_never_contains_secret_values(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    result = _probe_mixed(monkeypatch, tmp_path)
    for text in (*result.authentication_evidence, *result.warnings):
        assert "sk-test" not in text
