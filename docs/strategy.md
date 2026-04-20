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

```go
type Strategy interface {
    Setup(ctx context.Context, t *task.Task) (*Context, error)
    Submit(ctx context.Context, sctx *Context) (*Result, error)
    Cleanup(ctx context.Context, sctx *Context) error
}
```

**Setup** is called before the agent runs. It prepares the working environment — creates a branch, sets up a worktree, configures env vars. Returns a `Context` the loop threads through subsequent calls.

**Submit** is called as a side-effect of the lifecycle reaching `done`. It commits changes, pushes, creates PRs — whatever the strategy defines as "submission." Returns a `Result` with a ref (commit hash, PR URL, etc.).

**Cleanup** is called on teardown regardless of outcome. Removes worktrees, deletes temporary branches, releases resources.

## Utilities

`DeriveSlug(commit string) (string, error)` — extracts a branch-name slug from a conventional commit message. Lives in the strategy package as a helper for implementations that use conventional commits for branch naming.

## Status

The interface is defined. Implementation is deferred — the loop is the current focus. First concrete strategy will likely be a simple "branch + commit + push" flow for single-task execution.
