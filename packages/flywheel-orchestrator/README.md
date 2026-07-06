# flywheel-orchestrator

The multi-task layer built on [`flywheel-core`](../flywheel-core). Flywheel runs
**one** task; this drives **many**: it decides which task runs next over a
prerequisite DAG, coordinates several workers over a shared store (claims +
leases), and drives each chosen task through `flywheel.run_task`.

Depends on `flywheel-core`; `flywheel-core` never depends on this. Library
only; verbs are exposed through the unified product shell (`flywheel`
subcommands route here).

## Install

```bash
uv add flywheel-orchestrator
```

## Quickstart

```bash
flywheel init                   # scaffold .flywheel/ + flywheel.toml
# drop one JSON file per task into .flywheel/tasks/active/<phase>/
flywheel worker                 # drive every eligible task to quiescence
```

`init` is idempotent and never overwrites existing files. The generated
`flywheel.toml` keeps all runtime state (store, sandboxes) under
`.flywheel/`, gitignored.

## CLI

The product shell ships verbs that delegate here in-process; module-level
plumbing remains available as `python -m flywheel_orchestrator._workflow`:

```bash
flywheel init                   # scaffold .flywheel/ + work policy
flywheel status                 # state of every active task
flywheel status --rollup        # phase-grouped, evidence-derived (not self-reported) rollup
flywheel live                   # one line per in-flight run
flywheel history                # finished runs, newest first
flywheel show ID                # one run in full (run_id or task id)
flywheel validate               # statically lint active tasks' graders
flywheel archive                # move fully-DONE phases to archive/
flywheel recover                # finalize stranded lifecycles
flywheel recheck-blocked        # re-evaluate blocked lifecycles' requires
```

`history`, `show`, and `validate` are product-shell verbs routed here
in-process (`flywheel/_cli.py:46-58`), as is `status --rollup`. The autopilot
intake daemon (`flywheel autopilot [--once]`, keeps the queue full) lives on the
product shell too, delegating to `flywheel_orchestrator._autopilot_run`. `next`
and bare `orchestrate` remain `python -m flywheel_orchestrator._workflow`
plumbing; the blessed headless drain is `flywheel worker [--once]`. See
[../../docs/cli.md](../../docs/cli.md) for the full verb reference and
[../../docs/orchestration.md](../../docs/orchestration.md) for scheduling,
claims, and the orchestrator store.

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
- `GithubWorkSource` (`kind = "github"`) — labeled open issues via the `gh`
  CLI. An optional fenced ` ```flywheel ` JSON block in the issue body supplies
  goal/graders/context/prerequisites; issues without graders use the policy's
  default graders or are skipped. Outcomes post back as comments (or close
  the issue).
- `GithubCiWorkSource` (`kind = "github_ci"`) — failed GitHub CI runs via the
  `gh` CLI; each red `(workflow, branch)` becomes one graded task, verified by
  the policy's `[defaults.graders]` run out-of-band (never the check status).
- `GithubReviewWorkSource` (`kind = "github_review"`) — unresolved PR review
  threads via `gh api graphql`; resolution is only a candidate filter, the grade
  is `[defaults.graders]` out-of-band.

See [../../docs/work-sources.md](../../docs/work-sources.md) for the full
per-kind reference (config keys, item ids, write-back, and the entry-point
plugin path for custom sources).

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
sandbox_root = ".flywheel/worktrees"

[[defaults.graders]]
type = "command"
run = "uv run pytest"

[submit]                   # optional; how DONE work lands (worktree consumer)
strategy = "merge"         # or "pr"
protected_paths = [".github/**", "flywheel.toml"]

[sandbox]                  # optional; runs in each fresh worktree
setup = "uv sync"
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
through the `SubmitStrategy` seam — one object bundling `prepare_sandbox` and
`submit` — so no consumer-specific code (git, merges) enters the loop:

```python
from flywheel_orchestrator import orchestrate, SubmitStrategy

await orchestrate(
    tasks_dir=tasks_dir, db_path=db, sandbox_root=root,
    strategy=my_strategy,         # any object with prepare_sandbox + submit
)
```

`SubmitStrategy` is a runtime-checkable protocol; the standalone
`prepare_sandbox=` / `submit=` callables remain accepted as the primitive
form. The `flywheel-worktree` package ships two worked examples (FF-merge and
pull-request landing). Bring your own for a different VCS/sandbox/merge flow.

## Persistence

Owns its scheduling state (`task_claims`) in its **own** store
(`SqliteClaimStore` / `PostgresClaimStore` / `InMemoryClaimStore`), which can
share a backend with flywheel's store but manages only its own tables.
