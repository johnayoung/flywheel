"""Container (Docker) sandbox backend for flywheel-orchestrator.

Runs the task agent inside a Docker container against a bind-mounted worktree
(spec 00044). SDK-free: the agent runs as its own CLI inside the image, so this
package shells out to ``docker`` and never imports ``claude_agent_sdk``.

G3 surface (lifecycle primitives); the ``ContainerSubmitStrategy``, the
stream-json adapter, and ``[sandbox.network]`` enforcement land in G4–G6.
"""

from __future__ import annotations

from flywheel_container._docker import (
    DEFAULT_AGENT_HOME,
    DEFAULT_MAX_TAIL_CHARS,
    DEFAULT_WORKDIR,
    DockerError,
    ExecResult,
    VolumeMount,
    build_exec_argv,
    build_run_argv,
    check_image_uid,
    ensure_internal_network,
    exec_in_container,
    force_remove_container_sync,
    image_exists,
    network_exists,
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
from flywheel_container._stream import (
    StreamOutcome,
    iteration_result_from_stream,
    parse_stream_json,
)
from flywheel_container._submit import (
    ClaudeCliAgent,
    ContainerRuntime,
    ContainerSubmitStrategy,
)

__all__ = [
    "API_KEY_ENV",
    "ClaudeAuth",
    "ClaudeCliAgent",
    "ContainerRuntime",
    "ContainerSubmitStrategy",
    "DEFAULT_AGENT_HOME",
    "DEFAULT_INTERNAL_NETWORK",
    "OAUTH_TOKEN_ENV",
    "DEFAULT_MAX_TAIL_CHARS",
    "DEFAULT_WORKDIR",
    "DockerError",
    "ExecResult",
    "ResolvedNetwork",
    "StreamOutcome",
    "VolumeMount",
    "build_container_strategy",
    "build_exec_argv",
    "build_run_argv",
    "check_image_uid",
    "ensure_internal_network",
    "exec_in_container",
    "force_remove_container_sync",
    "image_exists",
    "iteration_result_from_stream",
    "network_exists",
    "parse_stream_json",
    "register_container_cleanup",
    "remove_container",
    "resolve_auth",
    "resolve_network",
    "start_container",
]
