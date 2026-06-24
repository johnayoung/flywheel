"""Held-out oracle for spec 00045 SC-1 — [sandbox.container] schema.

RED until container-config-schema lands. The container backend's config parses
into a frozen SandboxContainer on SandboxPolicy; backend="container" without an
image is a PolicyError; an unknown auth mode is a PolicyError. Do not weaken.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest

from flywheel_orchestrator._policy import PolicyError, load_policy


def _policy(tmp_path: Path, body: str):
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(p)


def test_absent_container_table_defaults(tmp_path: Path) -> None:
    c = _policy(tmp_path, "").sandbox.container
    assert c.image == ""
    assert c.model == ""
    assert c.auth == "oauth"
    assert c.auth_env == ""
    assert c.exec_timeout == 1800
    assert c.egress_network == ""


def test_worktree_backend_needs_no_image(tmp_path: Path) -> None:
    pol = _policy(tmp_path, '[sandbox]\nbackend = "worktree"\n')
    assert pol.sandbox.backend == "worktree"


def test_container_backend_requires_image(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="image is required"):
        _policy(tmp_path, '[sandbox]\nbackend = "container"\n')


def test_container_backend_with_image_parses(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "flywheel-agent:latest"
        model = "claude-sonnet-4-6"
        auth = "oauth"
        exec_timeout = 900
        egress_network = "egress-proxy"
        """,
    )
    assert pol.sandbox.backend == "container"
    c = pol.sandbox.container
    assert c.image == "flywheel-agent:latest"
    assert c.model == "claude-sonnet-4-6"
    assert c.exec_timeout == 900
    assert c.egress_network == "egress-proxy"


def test_unknown_auth_mode_rejected(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="sandbox.container.auth"):
        _policy(
            tmp_path,
            """
            [sandbox]
            backend = "container"
            [sandbox.container]
            image = "img"
            auth = "magic"
            """,
        )


def test_all_auth_modes_accepted(tmp_path: Path) -> None:
    for mode in ("oauth", "session", "api_key", "none"):
        pol = _policy(
            tmp_path,
            f"""
            [sandbox]
            backend = "container"
            [sandbox.container]
            image = "img"
            auth = "{mode}"
            """,
        )
        assert pol.sandbox.container.auth == mode
