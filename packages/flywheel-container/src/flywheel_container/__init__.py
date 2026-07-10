"""Container (Docker) sandbox backend for flywheel-orchestrator.

Runs the task agent inside a Docker container against a bind-mounted worktree
(spec 00044). SDK-free: the agent runs as its own CLI inside the image, so this
package shells out to ``docker`` and never imports ``claude_agent_sdk``. The
invocation itself rides the flywheel-agents claude-code CLI transport under
``DockerExecHost`` (``docs/agent-harness.md`` section 15.4).

Public surface: the Docker lifecycle primitives, the ``ClaudeAuth`` auth-mode
passthrough, ``ContainerSubmitStrategy`` (which composes an inner landing
strategy), and the ``[sandbox.network]`` policy resolver. Activated via
``[sandbox] backend = "container"``.
"""

from __future__ import annotations

from flywheel_container._docker import (
    DEFAULT_AGENT_HOME,
    DEFAULT_MANAGEMENT_TIMEOUT,
    DEFAULT_MAX_TAIL_CHARS,
    DEFAULT_WORKDIR,
    DockerError,
    DockerTimeoutError,
    ExecResult,
    OWNER_LABEL,
    OWNER_LABEL_SELECTOR,
    OWNER_LABEL_VALUE,
    VolumeMount,
    active_container_names,
    build_exec_argv,
    build_run_argv,
    check_image_uid,
    ensure_internal_network,
    exec_in_container,
    force_remove_container_sync,
    image_exists,
    list_owned_containers,
    network_exists,
    reap_orphan_containers,
    register_container_cleanup,
    remove_container,
    start_container,
)
from flywheel_container._auth import (
    API_KEY_ENV,
    OAUTH_TOKEN_ENV,
    ClaudeAuth,
)
from flywheel_container._config import build_container_strategy, resolve_auth
from flywheel_container._network import (
    DEFAULT_INTERNAL_NETWORK,
    ResolvedNetwork,
    resolve_network,
)
from flywheel_container._submit import (
    ContainerRuntime,
    ContainerSubmitStrategy,
)

__all__ = [
    "API_KEY_ENV",
    "ClaudeAuth",
    "ContainerRuntime",
    "ContainerSubmitStrategy",
    "DEFAULT_AGENT_HOME",
    "DEFAULT_INTERNAL_NETWORK",
    "OAUTH_TOKEN_ENV",
    "DEFAULT_MANAGEMENT_TIMEOUT",
    "DEFAULT_MAX_TAIL_CHARS",
    "DEFAULT_WORKDIR",
    "DockerError",
    "DockerTimeoutError",
    "ExecResult",
    "OWNER_LABEL",
    "OWNER_LABEL_SELECTOR",
    "OWNER_LABEL_VALUE",
    "ResolvedNetwork",
    "VolumeMount",
    "active_container_names",
    "build_container_strategy",
    "build_exec_argv",
    "build_run_argv",
    "check_image_uid",
    "ensure_internal_network",
    "exec_in_container",
    "force_remove_container_sync",
    "image_exists",
    "list_owned_containers",
    "network_exists",
    "reap_orphan_containers",
    "register_container_cleanup",
    "remove_container",
    "resolve_auth",
    "resolve_network",
    "start_container",
]
