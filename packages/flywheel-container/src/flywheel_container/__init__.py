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
    exec_in_container,
    force_remove_container_sync,
    image_exists,
    register_container_cleanup,
    remove_container,
    start_container,
)

__all__ = [
    "DEFAULT_AGENT_HOME",
    "DEFAULT_MAX_TAIL_CHARS",
    "DEFAULT_WORKDIR",
    "DockerError",
    "ExecResult",
    "VolumeMount",
    "build_exec_argv",
    "build_run_argv",
    "check_image_uid",
    "exec_in_container",
    "force_remove_container_sync",
    "image_exists",
    "register_container_cleanup",
    "remove_container",
    "start_container",
]
