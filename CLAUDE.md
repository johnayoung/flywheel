# Flywheel

Worktree-based parallel agent orchestrator for AI-driven coding tasks.

**Status:** mid-rewrite. The old DAG scheduler + 11-state orchestration engine has been stripped. What remains are the domain primitives (task schema, lifecycle state machine, git worktree plumbing, merge, store) awaiting a new orchestration layer.

## Build & Test

```bash
go build ./...                          # build all packages
go test ./...                           # run all tests
go test -race -coverprofile=coverage.out ./...  # with race detection + coverage
go vet ./...                            # static analysis
```

No CLI entry point exists in this state; `cmd/` is empty.

## Architecture

- **Task** (`internal/task/`): Immutable work definitions (JSON). Never modified at runtime.
- **Lifecycle** (`internal/lifecycle/`): Mutable execution state. State machine with explicit transitions in `machine.go`.
- **Worktree** (`internal/worktree/`): Git worktree lifecycle management.
- **Merge** (`internal/merge/`): Merger interface + sequential file-locked git merge.
- **Store** (`internal/store/`): Persistence interface + JSONL backend.

Key design: tasks (what to do) and lifecycles (execution state) are separate records, coordinated through a formal state machine.

## Conventions

- Conventional commits: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`
- All packages under `internal/` -- nothing is exported as a library
- Interfaces defined in their own files, implementations in subdirectories
- Tests live alongside source files (`*_test.go`)
