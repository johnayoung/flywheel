"""Execution hosts: argv construction and process spawning are separate axes.

Adapters produce a :class:`ProcessPlan`; hosts execute it. This is what lets
one adapter serve both a local worktree (``LocalHost``) and a bind-mounted
container worktree (``DockerExecHost``) without duplicating stream logic.

Commands are always argv arrays — no shell strings. ``plan.environment`` is
the *extra* environment contributed to the agent process: ``LocalHost`` merges
it over ``os.environ`` (matching how the agent SDK and the current CLI paths
behave); ``DockerExecHost`` passes it as ``docker exec -e`` flags only, the
container image supplying the base environment.
"""

from __future__ import annotations

import asyncio
import os
import signal
import time
from collections.abc import AsyncIterator, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol

from flywheel_agents.errors import AgentProtocolError, AgentStartupError
from flywheel_agents.models import AgentExit

# Stream-json lines carry whole file edits inside tool_use inputs; 64 MiB is a
# framing ceiling, not a typical size. Overrun is an AgentProtocolError.
_LINE_LIMIT_BYTES = 2**26
# Bounded stderr retention, mirroring the container backend's 64 KiB tail.
_STDERR_TAIL_BYTES = 64 * 1024


@dataclass(frozen=True, slots=True, kw_only=True)
class ProcessPlan:
    argv: tuple[str, ...]
    cwd: Path | None = None
    environment: Mapping[str, str] = field(default_factory=dict)
    stdin_payload: str | None = None
    # Variables that must NOT reach the agent (account-only auth policy).
    # LocalHost pops them from the merged environment. DockerExecHost cannot
    # unset container-image env; in-container auth hygiene stays with the
    # container lifecycle owner (flywheel-container's auth guard).
    denied_environment: tuple[str, ...] = ()


class RunningProcess(Protocol):
    """A spawned plan: line-oriented stdout, bounded stderr tail, kill, wait."""

    def stdout_lines(self) -> AsyncIterator[str]: ...

    async def kill(self, *, grace_seconds: float = 5.0) -> None: ...

    async def wait(self) -> AgentExit: ...


class ExecutionHost(Protocol):
    async def spawn(self, plan: ProcessPlan) -> RunningProcess: ...


class _SubprocessHandle:
    """Shared RunningProcess implementation over asyncio subprocesses."""

    def __init__(
        self,
        proc: asyncio.subprocess.Process,
        *,
        stdin_payload: str | None,
    ) -> None:
        self._proc = proc
        self._started_at = time.monotonic()
        self._stderr_tail = bytearray()
        self._stderr_task = asyncio.ensure_future(self._drain_stderr())
        self._stdin_task = (
            asyncio.ensure_future(self._write_stdin(stdin_payload))
            if stdin_payload is not None
            else None
        )

    async def _write_stdin(self, payload: str) -> None:
        stdin = self._proc.stdin
        if stdin is None:
            return
        try:
            stdin.write(payload.encode("utf-8"))
            await stdin.drain()
        except (BrokenPipeError, ConnectionResetError):
            pass  # agent exited before consuming the prompt; wait() reports it
        finally:
            try:
                stdin.close()
            except (BrokenPipeError, ConnectionResetError):
                pass

    async def _drain_stderr(self) -> None:
        stderr = self._proc.stderr
        if stderr is None:
            return
        while True:
            chunk = await stderr.read(8192)
            if not chunk:
                return
            self._stderr_tail.extend(chunk)
            if len(self._stderr_tail) > _STDERR_TAIL_BYTES:
                del self._stderr_tail[: len(self._stderr_tail) - _STDERR_TAIL_BYTES]

    async def stdout_lines(self) -> AsyncIterator[str]:
        stdout = self._proc.stdout
        if stdout is None:
            return
        while True:
            try:
                line = await stdout.readline()
            except ValueError as exc:  # LimitOverrunError folds into ValueError
                raise AgentProtocolError(
                    f"agent stdout line exceeded {_LINE_LIMIT_BYTES} bytes"
                ) from exc
            if not line:
                return
            yield line.decode("utf-8", errors="replace")

    async def kill(self, *, grace_seconds: float = 5.0) -> None:
        if self._proc.returncode is not None:
            return
        try:
            pgid = os.getpgid(self._proc.pid)
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            return
        try:
            async with asyncio.timeout(grace_seconds):
                await self._proc.wait()
                return
        except TimeoutError:
            pass
        try:
            os.killpg(pgid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass

    async def wait(self) -> AgentExit:
        returncode = await self._proc.wait()
        if self._stdin_task is not None:
            await self._stdin_task
        await self._stderr_task
        stderr_tail = (
            bytes(self._stderr_tail).decode("utf-8", errors="replace")
            if self._stderr_tail
            else None
        )
        return AgentExit(
            returncode=returncode,
            stderr_tail=stderr_tail,
            duration_seconds=time.monotonic() - self._started_at,
        )


async def _spawn(
    argv: tuple[str, ...],
    *,
    cwd: Path | None,
    env: Mapping[str, str] | None,
    stdin_payload: str | None,
) -> _SubprocessHandle:
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            cwd=str(cwd) if cwd is not None else None,
            env=dict(env) if env is not None else None,
            stdin=asyncio.subprocess.PIPE
            if stdin_payload is not None
            else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            start_new_session=True,
            limit=_LINE_LIMIT_BYTES,
        )
    except (OSError, ValueError) as exc:
        raise AgentStartupError(f"failed to spawn {argv[0]!r}: {exc}") from exc
    return _SubprocessHandle(proc, stdin_payload=stdin_payload)


@dataclass(frozen=True, slots=True)
class LocalHost:
    """Spawns the plan as a local process group.

    The child environment is ``os.environ`` merged with ``plan.environment``
    (extras win) — the same inheritance the agent SDK applies, so vendor CLIs
    keep their ``HOME``/``PATH``-anchored auth. ``kill`` signals the whole
    process group (``start_new_session=True`` makes the child its leader).
    """

    async def spawn(self, plan: ProcessPlan) -> RunningProcess:
        env = {**os.environ, **plan.environment}
        for name in plan.denied_environment:
            env.pop(name, None)
        return await _spawn(
            plan.argv,
            cwd=plan.cwd,
            env=env,
            stdin_payload=plan.stdin_payload,
        )


@dataclass(frozen=True, slots=True)
class DockerExecHost:
    """Executes the plan inside an already-running container via ``docker exec``.

    Container provisioning, teardown, networking, and auth mounts stay owned
    by the caller (flywheel-container); this host only executes plans.

    ``kill`` terminates the local docker client, which does **not** reap the
    in-container process — that is Docker's documented exec behavior. The
    container lifecycle owner reaps survivors at teardown (stop/rm), exactly
    as the container backend does today.
    """

    container_name: str
    docker_argv: tuple[str, ...] = ("docker",)

    async def spawn(self, plan: ProcessPlan) -> RunningProcess:
        argv: list[str] = [*self.docker_argv, "exec"]
        if plan.stdin_payload is not None:
            argv.append("-i")
        for key, value in plan.environment.items():
            argv.extend(["-e", f"{key}={value}"])
        if plan.cwd is not None:
            argv.extend(["-w", str(plan.cwd)])
        argv.append(self.container_name)
        argv.extend(plan.argv)
        return await _spawn(
            tuple(argv),
            cwd=None,
            env=None,  # the docker *client* inherits the parent environment
            stdin_payload=plan.stdin_payload,
        )
