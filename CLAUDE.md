# Flywheel

Worktree-based parallel agent orchestrator for AI-driven coding tasks.

## Build & Test

```bash
go build ./...                          # build all packages
go test ./...                           # run all tests
go test -race -coverprofile=coverage.out ./...  # with race detection + coverage
go vet ./...                            # static analysis
```

Entry point: `cmd/flywheel/main.go`

## Architecture

- **Task** (`internal/task/`): Immutable work definitions (JSON). Never modified at runtime.
- **Lifecycle** (`internal/lifecycle/`): Mutable execution state. 11-state machine with explicit transitions in `machine.go`.
- **Store** (`internal/store/`): Interface for persistence. JSONL file implementation in `store/jsonl/`.
- **DAG** (`internal/dag/`): Dependency graph built from task prerequisites. Readiness re-evaluated after each merge.
- **Engine** (`internal/engine/`): Orchestration loop. Workers claim ready tasks, execute via agent, validate, review, merge.
- **Agent** (`internal/agent/`): Interface for code execution backends. Claude Code subprocess implementation in `agent/claudecode/`.

Key design: tasks (what to do) and lifecycles (execution state) are separate records. The engine coordinates them through the state machine.

## Conventions

- Conventional commits: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`
- All packages under `internal/` -- nothing is exported as a library
- Interfaces defined in their own files, implementations in subdirectories
- Tests live alongside source files (`*_test.go`)
