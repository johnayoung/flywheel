"""Held-out oracle for spec 00039 SC-4 (limit threading, increment D of 00036).

RED until ``sandbox-limit-threading`` lands. ``policy.sandbox.limits.max_cost_usd``
decomposes into the primitive the harness consumes via
``_sandbox_limit_primitives`` (the limits mirror of ``_sandbox_agent_primitives``,
which feeds capabilities), defaulting to ``0.0`` (unenforced) for an absent
section or a ``None`` policy. Do not weaken or delete assertions.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

from flywheel_orchestrator._orchestrate import _sandbox_limit_primitives
from flywheel_orchestrator._policy import load_policy


def _policy(tmp_path: Path, body: str):
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(p)


def test_cost_ceiling_decomposes_from_policy(tmp_path: Path) -> None:
    pol = _policy(tmp_path, "[sandbox.limits]\nmax_cost_usd = 0.5\n")
    assert _sandbox_limit_primitives(pol)["max_cost_usd"] == 0.5


def test_absent_limits_default_to_unenforced(tmp_path: Path) -> None:
    pol = _policy(tmp_path, "")
    assert _sandbox_limit_primitives(pol)["max_cost_usd"] == 0.0


def test_none_policy_decomposes_to_unenforced() -> None:
    # Library callers pass no policy; the fast default is an unenforced ceiling.
    assert _sandbox_limit_primitives(None)["max_cost_usd"] == 0.0
