"""How the in-container Claude CLI authenticates (spec 00044).

A first-class, validated alternative to hand-building the container ``env``:
prefer a Claude Pro/Max **subscription** session over an API key. Three modes,
each via a factory; never construct :class:`ClaudeAuth` directly.

- ``ClaudeAuth.oauth_token(t)`` — a long-lived subscription OAuth token from
  ``claude setup-token``, passed as ``CLAUDE_CODE_OAUTH_TOKEN``. No API key, no
  on-disk state, no refresh. The recommended container path.
- ``ClaudeAuth.session()`` — bind-mount the host's existing ``~/.claude``
  credentials (a prior ``claude /login``) read-write so OAuth refresh persists;
  sets ``HOME`` so the CLI finds them.
- ``ClaudeAuth.api_key(k)`` — a raw ``ANTHROPIC_API_KEY`` (not a subscription).

Guard: ``ANTHROPIC_API_KEY`` takes precedence over the subscription token in the
CLI's auth order, so a subscription mode refuses to coexist with one in the
container env — otherwise the key would silently win.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import dataclass

from flywheel_container._docker import DEFAULT_AGENT_HOME, VolumeMount

OAUTH_TOKEN_ENV = "CLAUDE_CODE_OAUTH_TOKEN"
API_KEY_ENV = "ANTHROPIC_API_KEY"


@dataclass(frozen=True)
class ClaudeAuth:
    """The env + mounts an auth mode contributes to the container.

    Construct via :meth:`oauth_token` / :meth:`session` / :meth:`api_key`; the
    fields are an implementation detail.
    """

    env: Mapping[str, str]
    mounts: tuple[VolumeMount, ...]
    # True for subscription modes (oauth/session): ANTHROPIC_API_KEY must be
    # absent from the container env or it would override the session.
    subscription: bool

    @classmethod
    def oauth_token(cls, token: str) -> ClaudeAuth:
        """Subscription OAuth token (`claude setup-token`) via env. Recommended."""
        if not token:
            raise ValueError("ClaudeAuth.oauth_token requires a non-empty token")
        return cls(env={OAUTH_TOKEN_ENV: token}, mounts=(), subscription=True)

    @classmethod
    def session(cls, claude_dir: str | None = None) -> ClaudeAuth:
        """Bind-mount the host's ``~/.claude`` (an existing ``claude /login``).

        Mounted read-write so OAuth refresh persists; ``HOME`` is set so the CLI
        resolves credentials there. The container user must be UID-matched to
        the host owner of the dir (the strategy's ``--user`` default).
        """
        host = os.path.expanduser(claude_dir or "~/.claude")
        sandbox = f"{DEFAULT_AGENT_HOME}/.claude"
        return cls(
            env={"HOME": DEFAULT_AGENT_HOME},
            mounts=(VolumeMount(host_path=host, sandbox_path=sandbox),),
            subscription=True,
        )

    @classmethod
    def api_key(cls, key: str) -> ClaudeAuth:
        """A raw ``ANTHROPIC_API_KEY`` (not a subscription session)."""
        if not key:
            raise ValueError("ClaudeAuth.api_key requires a non-empty key")
        return cls(env={API_KEY_ENV: key}, mounts=(), subscription=False)

    def resolve(
        self, base_env: Mapping[str, str]
    ) -> tuple[dict[str, str], tuple[VolumeMount, ...]]:
        """Validate against ``base_env`` and return the (env, mounts) to add.

        Raises if a subscription mode would be overridden by an
        ``ANTHROPIC_API_KEY`` already in the container env.
        """
        if self.subscription and API_KEY_ENV in base_env:
            raise ValueError(
                f"{API_KEY_ENV} is set in the container env alongside a "
                f"subscription auth; the CLI prefers the key over the "
                f"subscription, so the session would never be used. Remove "
                f"{API_KEY_ENV} to authenticate with the subscription."
            )
        return dict(self.env), self.mounts
