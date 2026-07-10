"""Bridge from resolved ``[sandbox.container]`` policy to a strategy (00045).

Keeps the policy→strategy mapping in flywheel-container (which owns
``ContainerSubmitStrategy`` / ``ClaudeAuth``), so the worker just decomposes the
policy into primitives and calls these. SDK-free.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence

from flywheel_orchestrator import SubmitStrategy

from flywheel_container._auth import API_KEY_ENV, OAUTH_TOKEN_ENV, ClaudeAuth
from flywheel_container._docker import DEFAULT_MANAGEMENT_TIMEOUT
from flywheel_container._submit import ContainerSubmitStrategy

_DEFAULT_TOKEN_ENV: dict[str, str] = {
    "oauth": OAUTH_TOKEN_ENV,
    "api_key": API_KEY_ENV,
}


def resolve_auth(
    mode: str,
    *,
    env: Mapping[str, str],
    token_env: str | None = None,
) -> ClaudeAuth | None:
    """Resolve a ``[sandbox.container] auth`` mode to a :class:`ClaudeAuth`.

    ``oauth``/``api_key`` read the token from ``env`` by name (``token_env`` or
    the mode's default var); an unset var is a clear error rather than a silent
    unauthenticated run. ``session`` mounts ``~/.claude``; ``none`` injects
    nothing (the operator pre-authenticated the image or passes env directly).
    """
    if mode == "none":
        return None
    if mode == "session":
        return ClaudeAuth.session()
    if mode in _DEFAULT_TOKEN_ENV:
        name = token_env or _DEFAULT_TOKEN_ENV[mode]
        token = env.get(name)
        if not token:
            raise ValueError(
                f"sandbox.container auth={mode!r} requires the {name} "
                f"environment variable to be set"
            )
        return (
            ClaudeAuth.oauth_token(token)
            if mode == "oauth"
            else ClaudeAuth.api_key(token)
        )
    raise ValueError(f"unknown container auth mode {mode!r}")


def build_container_strategy(
    inner: SubmitStrategy,
    *,
    image: str,
    model: str,
    exec_timeout: int | None = None,
    network_policy: str | None = None,
    allow_hosts: Sequence[str] = (),
    egress_network: str | None = None,
    auth: ClaudeAuth | None = None,
    env: Mapping[str, str] | None = None,
    management_timeout: float | None = DEFAULT_MANAGEMENT_TIMEOUT,
) -> ContainerSubmitStrategy:
    """Wrap ``inner`` in a :class:`ContainerSubmitStrategy` from primitives.

    ``exec_timeout`` of ``0`` means unbounded (mapped to ``None``).

    ``management_timeout`` is the wall-clock ceiling for docker *management*
    calls; it defaults to the module constant so library callers stay
    byte-identical, while the worktree wiring supplies the policy-resolved
    ``[deadlines] docker_management_seconds`` value.
    """
    return ContainerSubmitStrategy(
        inner,
        image=image,
        model=model,
        auth=auth,
        env=env,
        network_policy=network_policy,
        allow_hosts=allow_hosts,
        egress_network=egress_network or None,
        exec_timeout=exec_timeout or None,
        management_timeout=management_timeout,
    )
