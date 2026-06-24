"""The container submit strategy (spec 00044 G5).

Assembles the G3 lifecycle primitives and the G4 stream adapter onto the
``SandboxHandle`` seam. ``ContainerSubmitStrategy`` *composes* an inner landing
strategy (e.g. the git-worktree merge submitter) rather than depending on it:
the inner strategy provisions the worktree and lands the result host-side
(unchanged — the worktree is bind-mounted, so the agent's edits are already on
the host), while this layer starts the container, runs the agent CLI inside it,
and tears the container down.

SDK-free: the agent runs as its own CLI inside the image; this module imports
only plain ``flywheel_core`` types and the orchestrator seam.
"""

from __future__ import annotations

import asyncio
import os
import shlex
import uuid
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from flywheel_core import (
    InvocationFailure,
    InvocationRequest,
    InvokeFunc,
    IterationResult,
)
from flywheel_orchestrator import (
    SandboxHandle,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
)

from flywheel_container import _docker
from flywheel_container._auth import ClaudeAuth
from flywheel_container._docker import (
    DEFAULT_AGENT_HOME,
    DEFAULT_WORKDIR,
    ExecResult,
    VolumeMount,
)
from flywheel_container._network import DEFAULT_INTERNAL_NETWORK, resolve_network
from flywheel_container._stream import iteration_result_from_stream, parse_stream_json


@dataclass(frozen=True)
class ClaudeCliAgent:
    """Builds the in-container Claude CLI invocation for one iteration.

    ``--print --output-format stream-json`` drives the headless JSONL stream the
    G4 adapter parses; the prompt is delivered on stdin (``-p -``) to dodge the
    Linux per-arg ``ARG_MAX`` limit. The model is deployment config — the host
    SDK invoker is bypassed entirely, so it cannot be inferred from there.

    ``dangerously_skip_permissions`` defaults ``True`` (mirroring the SDK path's
    ``permission_mode="bypassPermissions"``): a headless ``--print`` run has no
    TTY to approve tool use, so without it the CLI blocks forever the first time
    the agent writes a file or runs a command.
    """

    model: str
    dangerously_skip_permissions: bool = True
    extra_flags: tuple[str, ...] = ()

    def build_command(self, prompt: str) -> tuple[str, str]:
        parts: list[str] = []
        if self.dangerously_skip_permissions:
            parts.append("--dangerously-skip-permissions")
        parts.extend(self.extra_flags)
        flags = "".join(f" {flag}" for flag in parts)
        command = (
            "claude --print --verbose --output-format stream-json "
            f"--model {shlex.quote(self.model)}{flags} -p -"
        )
        return command, prompt


@dataclass(frozen=True)
class ContainerRuntime:
    """Injectable seam over the Docker primitives, so the strategy is
    unit-testable without a live daemon (tests pass fakes)."""

    start: Callable[..., str] = _docker.start_container
    exec_command: Callable[..., ExecResult] = _docker.exec_in_container
    remove: Callable[[str], None] = _docker.remove_container
    register_cleanup: Callable[[str], Callable[[], None]] = (
        _docker.register_container_cleanup
    )
    ensure_internal_network: Callable[[str], None] = _docker.ensure_internal_network


class ContainerSubmitStrategy:
    """Run the task agent inside a Docker container (a :class:`SubmitStrategy`).

    ``inner`` provisions the worktree and lands the terminal status; this
    strategy bind-mounts that worktree into a container, runs the agent CLI in
    it, and disposes the container via the handle's ``teardown``.
    """

    def __init__(
        self,
        inner: SubmitStrategy,
        *,
        image: str,
        agent: ClaudeCliAgent,
        container_uid: int | None = None,
        container_gid: int | None = None,
        network: str | Sequence[str] | None = None,
        network_policy: str | None = None,
        allow_hosts: Sequence[str] = (),
        egress_network: str | None = None,
        internal_network: str = DEFAULT_INTERNAL_NETWORK,
        mounts: Sequence[VolumeMount] = (),
        env: Mapping[str, str] | None = None,
        auth: ClaudeAuth | None = None,
        cpus: float | None = None,
        workdir: str = DEFAULT_WORKDIR,
        exec_timeout: float | None = None,
        preflight: bool = True,
        runtime: ContainerRuntime | None = None,
    ) -> None:
        self._inner = inner
        self._image = image
        self._agent = agent
        self._uid = container_uid if container_uid is not None else os.getuid()
        self._gid = container_gid if container_gid is not None else os.getgid()
        self._network = network
        self._network_policy = network_policy
        self._allow_hosts = tuple(allow_hosts)
        self._egress_network = egress_network
        self._internal_network = internal_network
        self._user_mounts = tuple(mounts)
        self._env = dict(env or {})
        # The agent CLI resolves its config/cache/credentials under $HOME
        # (~/.claude). A `docker run --user <uid>` numeric override leaves HOME
        # unset (or "/"), which the agent cannot write to — it stalls or fails.
        # Always point HOME at the agent home unless the operator set it.
        self._env.setdefault("HOME", DEFAULT_AGENT_HOME)
        if auth is not None:
            # Auth env/mounts are validated against the operator-supplied env
            # (e.g. a subscription mode rejects a stray ANTHROPIC_API_KEY) and
            # then folded in, so the container starts already authenticated.
            auth_env, auth_mounts = auth.resolve(self._env)
            self._env.update(auth_env)
            self._user_mounts = (*self._user_mounts, *auth_mounts)
        self._cpus = cpus
        self._workdir = workdir
        self._exec_timeout = exec_timeout
        self._preflight = preflight
        self._runtime = runtime or ContainerRuntime()

    def prepare_sandbox(self, request: SandboxRequest) -> SandboxHandle:
        inner_result = self._inner.prepare_sandbox(request)
        inner_handle = (
            inner_result
            if isinstance(inner_result, SandboxHandle)
            else SandboxHandle(path=inner_result)
        )
        worktree = inner_handle.path

        if self._preflight:
            _docker.check_image_uid(self._image, self._uid)

        name = f"flywheel-{request.task_id}-{uuid.uuid4().hex[:8]}"
        mounts = (
            VolumeMount(host_path=str(worktree), sandbox_path=self._workdir),
            *self._user_mounts,
        )
        self._runtime.start(
            name,
            self._image,
            env=self._env,
            mounts=mounts,
            workdir=self._workdir,
            user=f"{self._uid}:{self._gid}",
            network=self._resolve_network(),
            cpus=self._cpus,
        )
        unregister = self._runtime.register_cleanup(name)

        def _teardown() -> None:
            try:
                self._runtime.remove(name)
            finally:
                unregister()

        return SandboxHandle(
            path=worktree,
            env_contribution=inner_handle.env_contribution,
            invoke_wrapper=self._make_invoke_wrapper(name),
            teardown=_teardown,
        )

    def _resolve_network(self) -> str | Sequence[str] | None:
        """The container's ``--network``: ``[sandbox.network]`` policy when
        configured (G6), otherwise the raw ``network`` override."""
        if self._network_policy is None:
            return self._network
        resolved = resolve_network(
            policy=self._network_policy,
            allow_hosts=self._allow_hosts,
            egress_network=self._egress_network,
            internal_network=self._internal_network,
        )
        if resolved.ensure_internal and resolved.name is not None:
            self._runtime.ensure_internal_network(resolved.name)
        return resolved.name

    def submit(self, request: SubmitRequest) -> None:
        # Landing is host-side and backend-agnostic: the worktree the inner
        # strategy provisioned was bind-mounted, so the agent's edits are
        # already on it. Delegate the merge/park unchanged.
        self._inner.submit(request)

    def _make_invoke_wrapper(
        self, container_name: str
    ) -> Callable[[InvokeFunc], InvokeFunc]:
        agent = self._agent
        runtime = self._runtime
        workdir = self._workdir
        exec_timeout = self._exec_timeout

        def wrapper(_base_invoke: InvokeFunc | None) -> InvokeFunc:
            # The host SDK invoker is replaced wholesale: the agent runs in the
            # container, not the worker process. The base invoke (``None`` in
            # normal operation) is intentionally ignored.
            async def _invoke(request: InvocationRequest) -> IterationResult:
                command, stdin = agent.build_command(request.prompt)
                lines: list[str] = []
                result = await asyncio.to_thread(
                    runtime.exec_command,
                    container_name,
                    command,
                    stdin=stdin,
                    on_line=lines.append,
                    cwd=workdir,
                    timeout=exec_timeout,
                )
                outcome = parse_stream_json(lines)
                failure = None
                if result.exit_code != 0:
                    failure = InvocationFailure(
                        error_type="container_exec_error",
                        message=f"agent CLI exited {result.exit_code} in container",
                        exit_code=result.exit_code,
                        stderr=result.stderr,
                    )
                return iteration_result_from_stream(outcome, failure=failure)

            return _invoke

        return wrapper
