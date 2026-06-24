"""Live-daemon oracle for spec 00044 G3 — real container lifecycle.

Skipped when Docker is unavailable. Exercises the round trip against the real
daemon: start a detached container, exec a streamed command, write through a
bind mount as the host uid, then tear it down. SC-4/SC-5 of 00044.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from flywheel_container._docker import (
    DockerError,
    ExecResult,
    VolumeMount,
    exec_in_container,
    remove_container,
    start_container,
)

_IMAGE = "alpine:latest"


def _docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        return (
            subprocess.run(
                ["docker", "info"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            ).returncode
            == 0
        )
    except (OSError, subprocess.SubprocessError):
        return False


pytestmark = pytest.mark.skipif(
    not _docker_available(), reason="docker daemon not available"
)


@pytest.fixture()
def container() -> Iterator[str]:
    name = f"flywheel-test-{uuid.uuid4().hex[:12]}"
    # Generic base image kept alive via an explicit command override (a real
    # backend image bakes `sleep infinity` as its entrypoint instead).
    start_container(name, _IMAGE, command=["sleep", "infinity"])
    try:
        yield name
    finally:
        remove_container(name)


def test_exec_streams_lines_and_returns_exit_code(container: str) -> None:
    lines: list[str] = []
    result: ExecResult = exec_in_container(
        container,
        "printf 'a\\nb\\nc\\n'",
        on_line=lines.append,
    )
    assert result.exit_code == 0
    assert lines == ["a", "b", "c"]
    assert result.stdout == "a\nb\nc"


def test_exec_nonzero_exit_is_reported(container: str) -> None:
    result = exec_in_container(container, "exit 7")
    assert result.exit_code == 7


def test_exec_pipes_stdin(container: str) -> None:
    result = exec_in_container(container, "cat", stdin="piped-prompt")
    assert result.stdout == "piped-prompt"


def test_exec_timeout_bounds_a_silent_hang(container: str) -> None:
    # A command that holds stdout open and produces nothing must still be
    # killed at the timeout — the kill-timer, not proc.wait(), enforces it.
    import time

    start = time.monotonic()
    with pytest.raises(DockerError, match="timed out"):
        exec_in_container(container, "sleep 30", timeout=2.0)
    assert time.monotonic() - start < 15  # killed promptly, not after 30s


def test_bind_mount_uid_alignment(tmp_path: Path) -> None:
    # A file the container user writes into the bind mount must land owned by
    # the host uid — the #1 bind-mount gotcha. Run --user as the host uid:gid.
    host_uid = os.getuid()
    host_gid = os.getgid()
    work = tmp_path / "work"
    work.mkdir()
    name = f"flywheel-test-{uuid.uuid4().hex[:12]}"
    start_container(
        name,
        _IMAGE,
        mounts=[VolumeMount(host_path=str(work), sandbox_path="/work")],
        workdir="/work",
        user=f"{host_uid}:{host_gid}",
        command=["sleep", "infinity"],
    )
    try:
        result = exec_in_container(name, "echo written > /work/out.txt")
        assert result.exit_code == 0
        produced = work / "out.txt"
        assert produced.exists()
        assert produced.read_text().strip() == "written"
        # Owned by the host user — readable back by the orchestrator.
        assert produced.stat().st_uid == host_uid
    finally:
        remove_container(name)
