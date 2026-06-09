# flywheel-orchestrator

The multi-task layer built on [`flywheel`](../flywheel). Flywheel runs **one**
task; this drives **many**: it decides which task runs next over a prerequisite
DAG, coordinates several workers over a shared store (claims + leases), and
drives each chosen task through `flywheel.run_task`.

Depends on `flywheel`; `flywheel` never depends on this.

## Install

```bash
uv add flywheel-orchestrator
```

## Quickstart

```bash
flywheel-orchestrate init               # scaffold .flywheel/ + flywheel.toml
# drop one JSON file per task into .flywheel/tasks/active/<phase>/
flywheel-orchestrate orchestrate        # drive everything eligible to done
```

`init` is idempotent and never overwrites existing files. The generated
`flywheel.toml` keeps all runtime state (store, sandboxes) under
`.flywheel/`, gitignored.

## CLI

```bash
flywheel-orchestrate init               # scaffold .flywheel/ + work policy
flywheel-orchestrate next               # path to the next eligible task
flywheel-orchestrate orchestrate        # drain every eligible task to quiescence
flywheel-orchestrate status             # state of every active task
flywheel-orchestrate live               # one line per in-flight run
flywheel-orchestrate recover            # finalize stranded lifecycles
```

## Work sources

Work comes in through a `WorkSource` — anything that can enumerate items
compiled to flywheel `Task`s and receive outcome reports back:

```python
from flywheel_orchestrator import DirectoryWorkSource, GithubWorkSource, orchestrate

await orchestrate(source=GithubWorkSource(repo="owner/name", label="flywheel"),
                  db_path=db, sandbox_root=root)
```

- `DirectoryWorkSource` — the `.flywheel/tasks/active/<phase>/*.json` layout
  (what `tasks_dir=` wraps; the historical default).
- `GithubWorkSource` — labeled open issues via the `gh` CLI. An optional
  fenced ` ```flywheel ` JSON block in the issue body supplies
  goal/graders/context/prerequisites; issues without graders use the policy's
  default graders or are skipped. Outcomes post back as comments (or close
  the issue).

A repo-root `flywheel.toml` selects the source per project and declares
default graders; the CLI auto-detects it (`--policy` overrides, an explicit
`--tasks-dir` always wins):

```toml
[source]
kind = "github"            # or "directory"
repo = "owner/name"
label = "flywheel"

[paths]                    # optional; CLI flags still win
db = ".flywheel/flywheel.sqlite"
sandbox_root = ".flywheel/sandboxes"

[[defaults.graders]]
type = "command"
run = "uv run pytest"
```

After each driven run the orchestrator calls `source.report(WorkReport)` with
the terminal status and the final attempt's grader receipts — under the task
lease, after `submit`, best-effort (a raising report never unwinds the loop).

## Steering bridge

The work source is also the steering wheel: while runs are in flight, a
reconciler re-lists the source every `reconcile_seconds` and enqueues an
`interrupt` control command for any run whose item is no longer listed —
close the GitHub issue, pull its label, or delete the task JSON, and the
live session is interrupted (lifecycle parks as `INTERRUPTED`, sandbox
preserved; restore the item and the run resumes). A listing failure never
interrupts anything. Off by default for library callers
(`reconcile_seconds=None`); the CLI and the worktree daemon default to 15s
(`--reconcile-seconds`, 0 disables).

## The submit seam

`orchestrate()` provisions a sandbox and acts on each task's terminal status
through two injectable callbacks, so no consumer-specific code (git, merges)
enters the loop:

```python
from flywheel_orchestrator import orchestrate, SandboxRequest, SubmitRequest

await orchestrate(
    tasks_dir=tasks_dir, db_path=db, sandbox_root=root,
    prepare_sandbox=my_prepare,   # SandboxRequest -> Path
    submit=my_submit,             # SubmitRequest -> None
)
```

`flywheel-worktree` is a worked example of that seam. Bring your own for a
different VCS/sandbox/merge strategy.

## Persistence

Owns its scheduling state (`task_claims`) in its **own** store
(`SqliteClaimStore` / `PostgresClaimStore` / `InMemoryClaimStore`), which can
share a backend with flywheel's store but manages only its own tables.
