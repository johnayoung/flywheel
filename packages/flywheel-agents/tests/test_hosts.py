"""Real-subprocess tests for the execution hosts.

``LocalHost`` is exercised with ``sys.executable -c`` scripts (streaming,
stderr tail bounds, environment policy, stdin delivery, exit codes, group
kill). ``DockerExecHost`` is exercised without Docker: ``/bin/echo`` stands in
for the docker client, so a real spawn prints the assembled argv back.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path

import pytest

from flywheel_agents import AgentExit, DockerExecHost, LocalHost, ProcessPlan


def _plan(
    code: str,
    *,
    environment: dict[str, str] | None = None,
    denied_environment: tuple[str, ...] = (),
    stdin_payload: str | None = None,
) -> ProcessPlan:
    return ProcessPlan(
        argv=(sys.executable, "-c", code),
        environment=environment if environment is not None else {},
        denied_environment=denied_environment,
        stdin_payload=stdin_payload,
    )


async def _run_to_exit(plan: ProcessPlan) -> tuple[list[str], AgentExit]:
    process = await LocalHost().spawn(plan)
    lines = [line.rstrip("\n") async for line in process.stdout_lines()]
    return lines, await process.wait()


def test_stdout_lines_stream_in_order() -> None:
    code = "for i in range(5): print(f'line-{i}')"
    lines, exit_info = asyncio.run(_run_to_exit(_plan(code)))
    assert lines == [f"line-{i}" for i in range(5)]
    assert exit_info.returncode == 0


def test_stderr_tail_captured_and_bounded() -> None:
    code = (
        "import sys\n"
        "sys.stderr.write('a' * 70000)\n"
        "sys.stderr.write('ZTAIL-END')\n"
    )
    _, exit_info = asyncio.run(_run_to_exit(_plan(code)))
    assert exit_info.stderr_tail is not None
    assert len(exit_info.stderr_tail.encode("utf-8")) <= 65536
    assert exit_info.stderr_tail.endswith("ZTAIL-END")


def test_environment_merged_and_denied_popped(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("FLYWHEEL_TEST_BASE", "base-value")
    monkeypatch.setenv("FLYWHEEL_TEST_DENIED", "secret-value")
    code = (
        "import json, os\n"
        "print(json.dumps({\n"
        "    'base': os.environ.get('FLYWHEEL_TEST_BASE'),\n"
        "    'extra': os.environ.get('FLYWHEEL_TEST_EXTRA'),\n"
        "    'denied': os.environ.get('FLYWHEEL_TEST_DENIED'),\n"
        "}))\n"
    )
    plan = _plan(
        code,
        environment={
            "FLYWHEEL_TEST_EXTRA": "extra-value",
            "FLYWHEEL_TEST_BASE": "merged-over",
        },
        denied_environment=("FLYWHEEL_TEST_DENIED",),
    )
    lines, exit_info = asyncio.run(_run_to_exit(plan))
    assert exit_info.returncode == 0
    assert json.loads(lines[0]) == {
        "base": "merged-over",
        "extra": "extra-value",
        "denied": None,
    }


def test_stdin_payload_delivered_then_eof() -> None:
    code = "import sys\nprint('got:' + sys.stdin.read())"
    plan = _plan(code, stdin_payload="hello prompt")
    lines, exit_info = asyncio.run(_run_to_exit(plan))
    assert lines == ["got:hello prompt"]
    assert exit_info.returncode == 0


def test_exit_code_propagates_through_wait() -> None:
    lines, exit_info = asyncio.run(_run_to_exit(_plan("raise SystemExit(7)")))
    assert lines == []
    assert exit_info.returncode == 7
    assert exit_info.duration_seconds is not None


_KILL_SCRIPT = (
    "import subprocess, sys, time\n"
    "child = subprocess.Popen(\n"
    "    [sys.executable, '-c', 'import time; time.sleep(300)'])\n"
    "print(child.pid, flush=True)\n"
    "time.sleep(300)\n"
)


async def _spawn_and_kill() -> tuple[int, AgentExit, float]:
    process = await LocalHost().spawn(_plan(_KILL_SCRIPT))
    lines = process.stdout_lines()
    grandchild_pid = int((await anext(lines)).strip())
    started = time.monotonic()
    await process.kill()
    exit_info = await process.wait()
    return grandchild_pid, exit_info, time.monotonic() - started


def _process_dead(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    try:
        with open(f"/proc/{pid}/stat") as handle:
            stat = handle.read()
        return stat.rsplit(")", 1)[1].split()[0] == "Z"
    except OSError:
        return True


def test_kill_terminates_process_group() -> None:
    grandchild_pid, exit_info, elapsed = asyncio.run(_spawn_and_kill())
    assert elapsed < 10.0
    assert exit_info.returncode not in (0, None)
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline and not _process_dead(grandchild_pid):
        time.sleep(0.05)
    assert _process_dead(grandchild_pid)


async def _spawn_echo(
    host: DockerExecHost, plan: ProcessPlan
) -> tuple[list[str], AgentExit]:
    process = await host.spawn(plan)
    lines = [line.rstrip("\n") async for line in process.stdout_lines()]
    return lines, await process.wait()


def test_docker_exec_host_argv_assembly() -> None:
    host = DockerExecHost(container_name="cont-1", docker_argv=("/bin/echo",))
    plan = ProcessPlan(
        argv=("claude", "--print"),
        cwd=Path("/inside/workdir"),
        environment={"K": "V", "Z": "9"},
        stdin_payload="prompt-on-stdin",
    )
    lines, exit_info = asyncio.run(_spawn_echo(host, plan))
    assert exit_info.returncode == 0
    assert lines == [
        "exec -i -e K=V -e Z=9 -w /inside/workdir cont-1 claude --print"
    ]


def test_docker_exec_host_omits_stdin_flag_without_payload() -> None:
    host = DockerExecHost(container_name="cont-2", docker_argv=("/bin/echo",))
    plan = ProcessPlan(argv=("claude", "--version"))
    lines, exit_info = asyncio.run(_spawn_echo(host, plan))
    assert exit_info.returncode == 0
    assert lines == ["exec cont-2 claude --version"]
