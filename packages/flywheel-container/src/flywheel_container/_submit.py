"""The container submit strategy (spec 00044 G5, rebased on flywheel-agents).

Assembles the G3 lifecycle primitives onto the ``SandboxHandle`` seam.
``ContainerSubmitStrategy`` *composes* an inner landing strategy (e.g. the
git-worktree merge submitter) rather than depending on it: the inner strategy
provisions the worktree and lands the result host-side (unchanged — the
worktree is bind-mounted, so the agent's edits are already on the host), while
this layer starts the container, runs the agent CLI inside it, and tears the
container down.

The agent invocation rides the flywheel-agents claude-code CLI transport
(``docs/agent-harness.md`` section 15.4): the per-run invoke is built by
:func:`flywheel_core.agents_invoke.make_agents_invoke` over a
:class:`flywheel_agents.DockerExecHost`, replacing the historical local
stream-json fold. Every normalized event reaches
``InvocationRequest.on_message``, so the harness hang watchdog and per-message
telemetry are live on this path.

SDK-free: the agent runs as its own CLI inside the image; this module never
imports ``claude_agent_sdk`` (the flywheel-agents CLI transport shells out to
the vendor executable).
"""

from __future__ import annotations

import functools
import os
import uuid
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from flywheel_agents import DockerExecHost
from flywheel_core import InvokeFunc
from flywheel_core.agents_invoke import make_agents_invoke
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
    DEFAULT_MANAGEMENT_TIMEOUT,
    DEFAULT_WORKDIR,
    ExecResult,
    VolumeMount,
)
from flywheel_container._network import DEFAULT_INTERNAL_NETWORK, resolve_network


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
        model: str,
        dangerously_skip_permissions: bool = True,
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
        management_timeout: float | None = DEFAULT_MANAGEMENT_TIMEOUT,
        preflight: bool = True,
        runtime: ContainerRuntime | None = None,
        invoke_factory: Callable[[str], InvokeFunc] | None = None,
    ) -> None:
        self._inner = inner
        self._image = image
        # The model is deployment config — the host SDK invoker is bypassed
        # entirely, so it cannot be inferred from there.
        # ``dangerously_skip_permissions`` defaults True (mirroring the SDK
        # path's ``permission_mode="bypassPermissions"``): a headless --print
        # run has no TTY to approve tool use, so without it the CLI blocks
        # forever the first time the agent writes a file or runs a command.
        self._model = model
        self._dangerously_skip_permissions = dangerously_skip_permissions
        # Injectable per-run invoke builder (container name -> InvokeFunc), so
        # tests can swap the docker hop for a LocalHost while exercising the
        # real flywheel-agents bridge. Defaults to the DockerExecHost builder.
        self._invoke_factory = invoke_factory
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
        self._management_timeout = management_timeout
        self._preflight = preflight
        # The default runtime binds the resolved docker-management ceiling into
        # every management call the strategy issues -- container teardown, the
        # internal-network ensure, and the atexit force-remove backstop -- so an
        # operator [deadlines] override reaches them all, not just the inline
        # check_image_uid preflight below. An injected runtime (tests) is used
        # as-is; its call sites stay single-arg.
        if runtime is not None:
            self._runtime = runtime
        else:
            self._runtime = ContainerRuntime(
                remove=functools.partial(
                    _docker.remove_container, timeout=management_timeout
                ),
                ensure_internal_network=functools.partial(
                    _docker.ensure_internal_network, timeout=management_timeout
                ),
                register_cleanup=functools.partial(
                    _docker.register_container_cleanup, timeout=management_timeout
                ),
            )

    def prepare_sandbox(self, request: SandboxRequest) -> SandboxHandle:
        inner_result = self._inner.prepare_sandbox(request)
        inner_handle = (
            inner_result
            if isinstance(inner_result, SandboxHandle)
            else SandboxHandle(path=inner_result)
        )
        worktree = inner_handle.path

        if self._preflight:
            _docker.check_image_uid(
                self._image, self._uid, timeout=self._management_timeout
            )

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

    def _default_invoke_factory(self, container_name: str) -> InvokeFunc:
        """The per-run invoke: the claude-code CLI transport executed inside
        the container via ``DockerExecHost``.

        ``docker exec`` runs the agent as direct argv (no ``sh -c``) with the
        prompt piped on stdin, in the same in-container workdir the exec always
        targeted. ``exec_timeout`` becomes the runtime's wall-clock ceiling: a
        run that exceeds it folds into a structured failure with
        ``error_type="timeout"``; a nonzero agent exit folds into
        ``error_type="agent_exit"``.
        """
        return make_agents_invoke(
            agent_id="claude-code",
            working_directory=Path(self._workdir),
            model=self._model,
            permission_policy=(
                "auto" if self._dangerously_skip_permissions else "supervised"
            ),
            timeout_seconds=self._exec_timeout,
            host=DockerExecHost(container_name=container_name),
        )

    def _make_invoke_wrapper(
        self, container_name: str
    ) -> Callable[[InvokeFunc | None], InvokeFunc]:
        invoke_factory = (
            self._invoke_factory
            if self._invoke_factory is not None
            else self._default_invoke_factory
        )

        def wrapper(_base_invoke: InvokeFunc | None) -> InvokeFunc:
            # The host SDK invoker is replaced wholesale: the agent runs in the
            # container, not the worker process. The base invoke (``None`` in
            # normal operation) is intentionally ignored.
            return invoke_factory(container_name)

        return wrapper
