<p align="center">
  <strong>Flywheel</strong><br>
  Parallel agent orchestrator for AI-driven coding tasks
</p>

---

## Status

Flywheel is mid-rewrite. The previous build (DAG scheduler + 11-state orchestration engine + Claude-Code-only agent backend) has been stripped back to its domain primitives: task schema, lifecycle state machine, git worktree plumbing, merge, and the persistence layer. A new orchestration architecture is being designed.

This branch does not ship a runnable binary. If you are looking for the last runnable release, see the [Releases](https://github.com/johnayoung/flywheel/releases) page.

## What's here

```
internal/
  task/        -- task schema + validation
  lifecycle/   -- state machine + attempt history
  worktree/    -- git worktree lifecycle
  merge/       -- file-locked sequential git merge
  store/       -- TaskStore interface + JSONL backend
```

## Build & Test

```
go build ./...
go test ./...
go vet ./...
```

## License

[MIT](LICENSE)
