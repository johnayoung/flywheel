"""Held-out oracle for spec 00044 G3 — Docker lifecycle primitives (pure parts).

RED until G3 lands. Covers the deterministic argv builders, the bind-mount flag,
the bounded output tail, the best-effort cleanup registry, and the package's
SDK-free guarantee. The live-daemon behaviors are exercised separately in
``test_docker_live.py``. Do not weaken assertions.
"""

from __future__ import annotations

import ast
import pathlib

import flywheel_container
from flywheel_container._docker import (
    VolumeMount,
    _BoundedTail,
    _flush_cleanup_registry,
    build_exec_argv,
    build_run_argv,
    register_container_cleanup,
)


# --- SDK-free guarantee (00036 §6) -----------------------------------------


def test_package_never_imports_claude_agent_sdk() -> None:
    pkg_dir = pathlib.Path(flywheel_container.__file__).parent
    offenders: list[str] = []
    for path in pkg_dir.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                names = [a.name for a in node.names]
            elif isinstance(node, ast.ImportFrom):
                names = [node.module or ""]
            else:
                continue
            if any(n.split(".")[0] == "claude_agent_sdk" for n in names):
                offenders.append(path.name)
    assert not offenders, f"flywheel_container imports the agent SDK in: {offenders}"


# --- argv builders ----------------------------------------------------------


def test_build_run_argv_minimal() -> None:
    argv = build_run_argv("box", "img")
    assert argv == ["docker", "run", "-d", "--name", "box", "img"]


def test_build_run_argv_full_flag_order() -> None:
    argv = build_run_argv(
        "box",
        "img",
        env={"ANTHROPIC_API_KEY": "x"},
        mounts=[VolumeMount(host_path="/h", sandbox_path="/s")],
        workdir="/s",
        user="1000:1000",
        network=["netA", "netB"],
        groups=["docker", 999],
        devices=["/dev/kvm"],
        cpus=1.5,
    )
    assert argv == [
        "docker", "run", "-d", "--name", "box",
        "-e", "ANTHROPIC_API_KEY=x",
        "-v", "/h:/s",
        "-w", "/s",
        "--user", "1000:1000",
        "--network", "netA",
        "--network", "netB",
        "--group-add", "docker",
        "--group-add", "999",
        "--device", "/dev/kvm",
        "--cpus", "1.5",
        "img",
    ]


def test_build_run_argv_appends_command_override() -> None:
    argv = build_run_argv("box", "alpine", command=["sleep", "infinity"])
    assert argv == [
        "docker", "run", "-d", "--name", "box", "alpine", "sleep", "infinity",
    ]


def test_build_exec_argv_buffered_vs_stdin() -> None:
    assert build_exec_argv("box", "ls") == [
        "docker", "exec", "box", "sh", "-c", "ls",
    ]
    assert build_exec_argv("box", "claude -p -", interactive=True, cwd="/s") == [
        "docker", "exec", "-i", "-w", "/s", "box", "sh", "-c", "claude -p -",
    ]


def test_volume_mount_flag_readonly() -> None:
    assert VolumeMount(host_path="/h", sandbox_path="/s").to_flag() == "/h:/s"
    assert (
        VolumeMount(host_path="/h", sandbox_path="/s", readonly=True).to_flag()
        == "/h:/s:ro"
    )


# --- bounded tail -----------------------------------------------------------


def test_bounded_tail_retains_only_the_last_chars() -> None:
    tail = _BoundedTail(5)
    tail.push("abcdef")  # -> "bcdef" (last 5 of "abcdef")
    tail.push("gh")  # -> "defgh" (last 5 of "bcdefgh")
    assert str(tail) == "defgh"


def test_bounded_tail_joins_lines_with_separator() -> None:
    tail = _BoundedTail(100, sep="\n")
    tail.push("one")
    tail.push("two")
    assert str(tail) == "one\ntwo"


# --- cleanup registry (injected remover, no docker) -------------------------


def test_cleanup_registry_flushes_registered_then_clears(monkeypatch) -> None:
    removed: list[str] = []
    monkeypatch.setattr(
        "flywheel_container._docker._remover", lambda name: removed.append(name)
    )
    register_container_cleanup("c1")
    register_container_cleanup("c2")
    _flush_cleanup_registry()
    assert sorted(removed) == ["c1", "c2"]
    # A second flush removes nothing (the set was cleared).
    removed.clear()
    _flush_cleanup_registry()
    assert removed == []


def test_unregister_drops_from_registry(monkeypatch) -> None:
    removed: list[str] = []
    monkeypatch.setattr(
        "flywheel_container._docker._remover", lambda name: removed.append(name)
    )
    unregister = register_container_cleanup("c3")
    unregister()
    _flush_cleanup_registry()
    assert removed == []
