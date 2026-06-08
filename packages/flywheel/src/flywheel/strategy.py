import re
from dataclasses import dataclass, field
from typing import Protocol, runtime_checkable

from flywheel.task import Task


@dataclass
class StrategyContext:
    task_id: str = ""
    ref: str = ""
    metadata: dict[str, str] = field(default_factory=dict)


@dataclass
class StrategyResult:
    ref: str = ""
    submitted: bool = False
    metadata: dict[str, str] = field(default_factory=dict)


@runtime_checkable
class Strategy(Protocol):
    def setup(self, task: Task) -> StrategyContext: ...
    def submit(self, ctx: StrategyContext) -> StrategyResult: ...
    def cleanup(self, ctx: StrategyContext) -> None: ...


class NoOpStrategy:
    """Default Strategy that performs no git, filesystem, or network work."""

    def setup(self, task: Task) -> StrategyContext:
        return StrategyContext(task_id=task.id)

    def submit(self, ctx: StrategyContext) -> StrategyResult:
        return StrategyResult(ref=ctx.ref)

    def cleanup(self, ctx: StrategyContext) -> None:
        _ = ctx
        return None


_SLUG_FALLBACK = "task"
_CONVENTIONAL_PREFIX = re.compile(r"^[a-zA-Z]+(?:\([^)]+\))?!?:\s*")


def derive_slug(commit: str) -> str:
    """Extract a branch-name slug from a (conventional) commit message.

    Total: empty, whitespace-only, or otherwise unparsable input yields a
    stable fallback rather than raising.
    """
    if not isinstance(commit, str):
        return _SLUG_FALLBACK
    text = commit.strip()
    if not text:
        return _SLUG_FALLBACK

    first_line = text.split("\n", 1)[0].strip()
    if not first_line:
        return _SLUG_FALLBACK

    body = _CONVENTIONAL_PREFIX.sub("", first_line, count=1).strip()
    if not body:
        return _SLUG_FALLBACK

    slug_chars: list[str] = []
    for ch in body.lower():
        slug_chars.append(ch if ch.isalnum() else "-")
    slug = "".join(slug_chars)
    while "--" in slug:
        slug = slug.replace("--", "-")
    slug = slug.strip("-")
    return slug or _SLUG_FALLBACK
