"""Installation and authentication probe for Claude Code.

Evidence-based, never definitive about billing: an OAuth token or credentials
file is a strong indication of account-session auth; an API key indicates
API-backed auth; both present is mixed (the CLI's own precedence prefers the
API key, which is exactly the silent-subscription-strand the container
backend's auth guard exists to catch). Evidence strings carry names and
paths only — never secret values.
"""

from __future__ import annotations

import asyncio
import os
import shutil
from pathlib import Path

from flywheel_agents.models import (
    AgentProbeResult,
    AssuranceLevel,
    AuthenticationKind,
)

_OAUTH_TOKEN_ENV = "CLAUDE_CODE_OAUTH_TOKEN"
_API_KEY_ENV = "ANTHROPIC_API_KEY"
_VERSION_TIMEOUT_SECONDS = 10.0


async def _read_version(executable: str) -> str | None:
    try:
        proc = await asyncio.create_subprocess_exec(
            executable,
            "--version",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
            stdin=asyncio.subprocess.DEVNULL,
        )
        async with asyncio.timeout(_VERSION_TIMEOUT_SECONDS):
            stdout, _ = await proc.communicate()
    except (OSError, TimeoutError):
        return None
    if proc.returncode != 0:
        return None
    text = stdout.decode("utf-8", errors="replace").strip()
    return text or None


async def probe_claude_code() -> AgentProbeResult:
    executable = shutil.which("claude")
    home = Path.home()
    credentials = home / ".claude" / ".credentials.json"
    config_dir = home / ".claude"

    evidence: list[str] = []
    warnings: list[str] = []
    has_account = False
    has_api_key = False

    if os.environ.get(_OAUTH_TOKEN_ENV):
        has_account = True
        evidence.append(f"{_OAUTH_TOKEN_ENV} is set")
    if credentials.is_file():
        has_account = True
        evidence.append(f"{credentials} exists")
    if os.environ.get(_API_KEY_ENV):
        has_api_key = True
        evidence.append(f"{_API_KEY_ENV} is set")

    if executable is None:
        return AgentProbeResult(
            installed=False,
            authentication_kind=AuthenticationKind.UNKNOWN,
            authentication_assurance=AssuranceLevel.UNKNOWN,
            authentication_evidence=tuple(evidence),
            warnings=("claude executable not found on PATH",),
        )

    version = await _read_version(executable)
    if version is None:
        warnings.append("claude --version failed; version unknown")

    if has_account and has_api_key:
        kind = AuthenticationKind.MIXED
        warnings.append(
            f"both account credentials and {_API_KEY_ENV} present; the CLI "
            "prefers the API key, which strands the subscription"
        )
    elif has_account:
        kind = AuthenticationKind.ACCOUNT_SESSION
    elif has_api_key:
        kind = AuthenticationKind.API_KEY
    else:
        kind = AuthenticationKind.INSTALLATION_ONLY

    return AgentProbeResult(
        installed=True,
        executable_path=Path(executable),
        version=version,
        authentication_kind=kind,
        authentication_assurance=AssuranceLevel.STRONG_INDICATION
        if kind is not AuthenticationKind.INSTALLATION_ONLY
        else AssuranceLevel.BEST_EFFORT,
        authentication_evidence=tuple(evidence),
        config_paths=(config_dir,) if config_dir.is_dir() else (),
        warnings=tuple(warnings),
    )
