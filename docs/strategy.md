# Strategy

The Strategy interface owns everything between "the agent finished the work" and "the result is committed/merged/submitted." It is the pluggable boundary where git workflow lives — the loop calls into it, but does not know or care what it does internally.

## Responsibilities

- Branch creation and naming (e.g., deriving a slug from the task's category + description)
- Commit message generation
- Worktree setup and teardown
- Merge conflict detection and resolution
- Submission (push, PR creation, merge)
- Review gates (if applicable)

## Interface

```python
from typing import Protocol
from flywheel.task import Task

class Strategy(Protocol):
    def setup(self, task: Task) -> "StrategyContext": ...
    def submit(self, ctx: "StrategyContext") -> "StrategyResult": ...
    def cleanup(self, ctx: "StrategyContext") -> None: ...
```

**setup** is called before the agent runs. It prepares the working environment — creates a branch, sets up a worktree, configures env vars. Returns a `StrategyContext` the loop threads through subsequent calls.

**submit** is called as a side-effect of the lifecycle reaching `done`. It commits changes, pushes, creates PRs — whatever the strategy defines as "submission." Returns a `StrategyResult` with a ref (commit hash, PR URL, etc.).

**cleanup** is called on teardown regardless of outcome. Removes worktrees, deletes temporary branches, releases resources.

## Utilities

`derive_slug(commit: str) -> str` — extracts a branch-name slug from a conventional commit message. Lives in the strategy module as a helper for implementations that use conventional commits for branch naming.

## Status

Specified here; not yet implemented. First concrete strategy will likely be a simple "branch + commit + push" flow for single-task execution.
