"""Held-out oracle for spec 00040 SC-2 (env decomposition, increment C).

RED until ``sandbox-env-decomposition`` lands. ``_sandbox_agent_primitives``
returns an ``agent_env`` key resolving ``[sandbox.env]``: declared ``pass`` names
forwarded from ``os.environ`` (present-only) merged with inline ``set`` literals
(literals win), or ``{}`` when both are empty (the ``fast`` default and ``None``
policy). Do not weaken or delete assertions.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest

from flywheel_orchestrator._orchestrate import _sandbox_agent_primitives
from flywheel_orchestrator._policy import load_policy


def _policy(tmp_path: Path, body: str):
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(p)


def test_fast_resolves_to_empty_agent_env(tmp_path: Path) -> None:
    prims = _sandbox_agent_primitives(_policy(tmp_path, ""))
    assert prims["agent_env"] == {}


def test_none_policy_resolves_to_empty_agent_env() -> None:
    assert _sandbox_agent_primitives(None)["agent_env"] == {}


def test_set_literals_are_injected(tmp_path: Path) -> None:
    pol = _policy(tmp_path, '[sandbox.env]\nset = { FOO = "bar" }\n')
    assert _sandbox_agent_primitives(pol)["agent_env"] == {"FOO": "bar"}


def test_pass_forwards_present_env_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("PRESENT_TOKEN", "sekret")
    monkeypatch.delenv("ABSENT_TOKEN", raising=False)
    pol = _policy(
        tmp_path,
        '[sandbox.env]\npass = ["PRESENT_TOKEN", "ABSENT_TOKEN"]\n',
    )
    env = _sandbox_agent_primitives(pol)["agent_env"]
    assert env == {"PRESENT_TOKEN": "sekret"}  # absent name dropped, not blanked


def test_set_literal_wins_over_forwarded_value(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FOO", "from-environ")
    pol = _policy(
        tmp_path,
        '[sandbox.env]\npass = ["FOO"]\nset = { FOO = "from-literal" }\n',
    )
    assert _sandbox_agent_primitives(pol)["agent_env"]["FOO"] == "from-literal"
