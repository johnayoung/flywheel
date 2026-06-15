"""The single lazy boundary to the optional ``claude-agent-sdk`` dependency.

flywheel-core's *data* — ``Task``, ``Lifecycle``, graders, envelopes, the
store — needs no agent SDK. Only *driving* an agent does. So the SDK is an
optional extra (``flywheel-core[claude]``) and this module is the one place
the rest of core reaches it:

* For **annotations**, every module imports the real SDK names under
  ``if TYPE_CHECKING:`` — zero runtime cost, full type-checker fidelity.
* For **runtime objects** (constructing ``ClaudeAgentOptions``, calling
  ``query``, ``isinstance`` against the message classes), the agent-driving
  functions do a local ``from flywheel_core._sdk import ...``. Module
  ``__getattr__`` imports the SDK on first such access and raises a
  branded, actionable error if the extra is not installed.

The net effect: ``import flywheel_core`` succeeds with or without the SDK;
the failure only surfaces the moment something actually tries to drive an
agent, pointing straight at the install command that fixes it.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from flywheel_core._registry import MissingExtraError

if TYPE_CHECKING:
    # Re-exported for annotation use elsewhere via ``from
    # flywheel_core._sdk import X`` guarded by the importer's own
    # TYPE_CHECKING block. Never executed at runtime.
    from claude_agent_sdk import (
        AssistantMessage as AssistantMessage,
    )
    from claude_agent_sdk import (
        ClaudeAgentOptions as ClaudeAgentOptions,
    )
    from claude_agent_sdk import (
        ClaudeSDKClient as ClaudeSDKClient,
    )
    from claude_agent_sdk import (
        ClaudeSDKError as ClaudeSDKError,
    )
    from claude_agent_sdk import (
        ContextUsageResponse as ContextUsageResponse,
    )
    from claude_agent_sdk import (
        HookEventMessage as HookEventMessage,
    )
    from claude_agent_sdk import (
        Message as Message,
    )
    from claude_agent_sdk import (
        ProcessError as ProcessError,
    )
    from claude_agent_sdk import (
        RateLimitEvent as RateLimitEvent,
    )
    from claude_agent_sdk import (
        ResultMessage as ResultMessage,
    )
    from claude_agent_sdk import (
        TextBlock as TextBlock,
    )
    from claude_agent_sdk import (
        ToolResultBlock as ToolResultBlock,
    )
    from claude_agent_sdk import (
        ToolUseBlock as ToolUseBlock,
    )
    from claude_agent_sdk import (
        UserMessage as UserMessage,
    )
    from claude_agent_sdk import (
        query as query,
    )


_INSTALL_HINT = (
    "driving an agent requires the optional 'claude-agent-sdk' dependency. "
    "Install it with:  pip install 'flywheel-core[claude]'  (or, for the "
    "product shell, 'flywheel' already bundles it)."
)


class MissingAgentSDKError(MissingExtraError):
    """Raised when an agent-driving path needs ``claude-agent-sdk`` but the
    optional extra is not installed.

    A member of the shared :class:`~flywheel_core._registry.MissingExtraError`
    family (so ``except MissingExtraError`` catches every missing-optional
    failure uniformly), but it keeps its own, more specific hint: the SDK is
    on the ``claude`` extra of *core*, and the product ``flywheel`` dist
    already bundles it.
    """

    def __init__(self, message: str = _INSTALL_HINT) -> None:
        self.extra = "claude"
        # Bypass MissingExtraError's generic message format; the SDK hint
        # above is the established, more actionable one.
        ModuleNotFoundError.__init__(self, message)

# The SDK symbols this boundary resolves on demand. Anything else (notably
# the dunders the import system probes, e.g. ``__path__``) must raise a
# plain AttributeError so ``__getattr__`` never triggers an SDK load — and
# never masks a real "no such attribute" as a missing-SDK error.
_SDK_EXPORTS: frozenset[str] = frozenset(
    {
        "AssistantMessage",
        "ClaudeAgentOptions",
        "ClaudeSDKClient",
        "ClaudeSDKError",
        "ContextUsageResponse",
        "HookEventMessage",
        "Message",
        "ProcessError",
        "RateLimitEvent",
        "ResultMessage",
        "TextBlock",
        "ToolResultBlock",
        "ToolUseBlock",
        "UserMessage",
        "query",
    }
)


def load_sdk() -> Any:
    """Import and return the ``claude_agent_sdk`` module, or raise
    :class:`MissingAgentSDKError` with the install hint."""
    try:
        import claude_agent_sdk
    except ModuleNotFoundError as exc:  # pragma: no cover - exercised in subprocess
        if exc.name not in (None, "claude_agent_sdk"):
            raise
        raise MissingAgentSDKError(_INSTALL_HINT) from exc
    return claude_agent_sdk


def __getattr__(name: str) -> Any:
    """Resolve an SDK symbol on first access, importing the SDK lazily.

    Enables ``from flywheel_core._sdk import AssistantMessage`` (and
    ``query``, ``ClaudeAgentOptions``, ...) inside agent-driving functions:
    the import fires here, the branded error fires here, and the resolved
    object is the genuine SDK class/function so ``isinstance`` and
    construction behave exactly as a direct import would.
    """
    if name not in _SDK_EXPORTS:
        raise AttributeError(
            f"module {__name__!r} has no attribute {name!r}"
        )
    sdk = load_sdk()
    try:
        return getattr(sdk, name)
    except AttributeError as exc:
        raise AttributeError(
            f"claude_agent_sdk has no attribute {name!r}"
        ) from exc
