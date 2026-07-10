"""Claude Code adapter (CLI stream-json + optional SDK transport)."""

from flywheel_agents.claude_code.adapter import ClaudeCodeAdapter
from flywheel_agents.claude_code._cli import (
    ClaudeStreamNormalizer,
    build_cli_plan,
)

__all__ = ["ClaudeCodeAdapter", "ClaudeStreamNormalizer", "build_cli_plan"]
