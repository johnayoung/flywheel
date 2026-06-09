# Workflow

How flywheel develops itself. Every feature since the Postgres store has gone through this pipeline: a spec is defined, decomposed into task JSONs, executed by flywheel's own worker loop, then audited for loop friction that feeds the next round of features. The pipeline lives half in `.claude/commands/` (prompt commands run by an operator in Claude Code) and half in `.workflow/` (artifacts and runtime state). This doc records the current state and rationale as the baseline for promoting pieces of it into first-class flywheel features.

## Pipeline at a glance

| Stage     | Command                    | Input                       | Output                                                           |
| --------- | -------------------------- | --------------------------- | ---------------------------------------------------------------- |
| Define    | `/define`                  | vague feature idea          | spec: `.workflow/specs/NNNNN-FEATURE-<name>.md`                  |
| Decompose | `/task`                    | spec reference or free text | task JSONs: `.workflow/tasks/active/NN-<phase>/<id>.json`        |
| Execute   | `flywheel-worktree` daemon | active phase dirs           | commits on main, lifecycles in the store, archived phase         |
| Audit     | `/audit-phase`             | store + logs for a phase    | findings: `.workflow/audits/<phase>.md`                          |
| Propose   | `/propose-improvements`    | one or more audits          | ranked proposals, each handed to `/define`, `/task`, or "accept" |

The loop closes: proposals become specs become phases become audits. Two further commands review the *work* rather than the loop (`/review-phase`, `/arewedone`); they live at user level (`~/.claude/commands/`), not in this repo.

### Stage contracts

- **`/define`** is a requirements interview: it asks `AskUserQuestion` rounds until scope, behavior, edge cases, and integration points are unambiguous, then emits a spec with a Decisions Log. It also runs the loop-path trigger check (below) and records the result in the spec.
- **`/task`** converts a spec into one JSON file per task per `docs/task-schema.md`. Discipline it enforces: one-sentence `goal`, at least one grader, `prerequisites` as the only ordering mechanism, no procedure prescription, a constraint telling the agent to commit before `intent=verify`. It always presents the proposal before writing files.
- **`/audit-phase`** audits *flywheel*, not the shipped code. Every finding cites a `run_id`, `events.id`, grader row, or log file:line, and stops at diagnosis — proposing fixes is explicitly forbidden. A phase the loop never ran gets a short "nothing to audit" note, not an invented report.
- **`/propose-improvements`** is the action half: every proposal must trace to a cited audit finding, states an observable outcome (never an implementation), and ends in a handoff. "Accept — do not fix" is a valid proposal. The operator picks what advances via `AskUserQuestion`.

## Artifact layout

```
.claude/commands/           define.md, task.md, audit-phase.md, propose-improvements.md
.workflow/
  specs/                    NNNNN-FEATURE-<name>.md (sequential, zero-padded)
  tasks/
    active/NN-<phase>/      one JSON per task; the worker consumes these
      .loop-base            HEAD SHA when the phase was first seen (committed)
      loop-path-exempt.md   optional gate opt-out (phase/author/reason front-matter)
    archive/NN-<phase>/     moved here by the archive gate when all tasks are DONE
  audits/<phase>.md         committed audit findings
  proposals/<phase>.md      committed proposal docs
  flywheel.sqlite           runtime store (gitignored; lifecycles, attempts, events, grader_results)
  worktrees/                per-task git worktrees (runtime)
  .merge.lock               serializes FF-merges into main
logs/worker/                per-run worker logs
```

Phases are plain directories: the `NN-` prefix controls walk order, there is no phase metadata, and cross-task ordering lives only in each task's `prerequisites`.

## The runtime loop

The daemon is `flywheel-worktree` (`packages/flywheel-worktree/src/flywheel_worktree/worker.py`). Each cycle:

1. Commit any untracked task JSONs under `tasks/active/`.
2. Record a `.loop-base` SHA for any phase that lacks one.
3. `orchestrate()` — drive every eligible task to quiescence (`packages/flywheel-orchestrator/`): a task is eligible when its state is FRESH/RETRYABLE/INTERRUPTED and every prerequisite is DONE; each task runs in its own git worktree branched off main and FF-merges back on DONE under the merge lock.
4. Write per-run logs to `logs/worker/`.
5. `archive_completed_phases()` — move fully-DONE phases to `archive/`, subject to the gate below.

Default paths are code, not convention: `DEFAULT_TASKS_DIR = .workflow/tasks` and `DEFAULT_LOG_DIR = logs/worker` (`flywheel_orchestrator/_workflow.py:50`), `DEFAULT_DB_PATH = .workflow/flywheel.sqlite` (`flywheel/workflow.py:94`). All are overridable via CLI flags.

### The in-loop-verification gate

`archive_completed_phases` diffs the phase against its `.loop-base` and scans for five watched signals (`flywheel/loop_path_marker.py`): a new `Status`/`Outcome`/transition rule, a new schema column or table, a new `Grader` variant, a new store-Protocol method with dispatch, a new control-command verb. If any trips, the phase cannot archive without either a DONE task tagged `in-loop-verification` (a test that drives the real `orchestrate` loop with a scripted invoker) or a committed `loop-path-exempt.md` opt-out. `/define` flags the trigger at spec time, `/task` emits the tagged slot, and `/audit-phase` re-derives the signals after archive to catch slips and contradicted opt-outs.

## Why it works this way

Each rule exists because an audit caught its absence:

- **Discovery before tasks.** Under-specified tasks are the dominant source of loop waste — the agent burns context discovering what the operator already knew. `/define` forces ambiguity to zero before any task exists; `context.relevant` in task JSONs is the single biggest lever on context burn.
- **Evidence and action are separate commands.** `/audit-phase` may not propose; `/propose-improvements` may not invent findings. Splitting them prevents audits from padding into feature backlogs and keeps every proposal traceable to a cited row.
- **The gate exists because unit tests lied.** Phase 08 shipped a schema change whose graders passed while the live store was never migrated; phase 17 built the manual-approval gate without ever producing an `AWAITING_APPROVAL` lifecycle in-loop; phase 19 merged code a stale worker process never executed. The gate makes "the real loop ran the new path" a mechanical archive precondition instead of a hope.
- **Enumerate dependents of shared invariants.** A task that changes a shape other graders assert against must update those tests in the same commit, or the next task inherits a red suite (phase 02 audit).
- **One task per file, phases as bare directories.** The worker iterates files; keeping phase semantics out of the schema keeps `Task` tight and ordering explicit.

## Work sources (project-agnostic boundary)

The orchestrator does not consume `.workflow/tasks/` directly anymore — it consumes a `WorkSource` (`packages/flywheel-orchestrator/src/flywheel_orchestrator/_sources.py`):

- **Inbound** — `list_work()` returns `WorkItem`s, each a validated core `Task` plus `prerequisites` and an opaque `source_ref`. Anything that cannot compile to a Task with at least one grader never reaches the scheduler.
- **Outbound** — `report(WorkReport)` receives each driven run's terminal status, run id, and final grader receipts after the consumer `submit` step, still under the task lease. Delivery is best-effort; a raising report never unwinds the loop. Ticket writes go through this path, never through the agent.

Adapters shipped today:

- `DirectoryWorkSource` — the historical `.workflow/tasks/active/<phase>/*.json` layout; `report` is a no-op (the store is the local record; phase archiving stays a separate directory flow).
- `GithubWorkSource` (`_github.py`) — labeled open issues via the `gh` CLI. `gh-<number>` task ids; an optional fenced ` ```flywheel ` JSON block in the body overrides goal/graders/context/tags/prerequisites; issues without graders fall back to the policy's default graders or are skipped. Outcomes post back as comments (or close the issue when `done_action = "close"`).

`flywheel.toml` at the repo root selects the source per project (`_policy.py`):

```toml
[source]
kind = "github"            # or "directory"
repo = "owner/name"
label = "flywheel"
done_action = "close"      # or "comment"

[[defaults.graders]]
type = "command"
run = "uv run pytest"
```

`flywheel-orchestrate next|status|orchestrate|recheck-blocked` auto-detect `flywheel.toml` (override with `--policy`; an explicit `--tasks-dir` always wins and selects the directory source).

## Code vs. convention

What would need promotion for another codebase to use this workflow:

| Piece                                                               | Lives in                                              | Status          |
| ------------------------------------------------------------------- | ----------------------------------------------------- | --------------- |
| Task selection, claims/leases, `orchestrate`                        | `flywheel-orchestrator`                               | shipped code    |
| `WorkSource` seam, directory + GitHub adapters, `flywheel.toml`     | `flywheel-orchestrator` (`_sources`, `_github`, `_policy`) | shipped code    |
| Archive gate, loop-path signals, `.loop-base`, opt-out parsing      | `flywheel-orchestrator` + `flywheel.loop_path_marker` | shipped code    |
| Worktree-per-task submit strategy, daemon                           | `flywheel-worktree`                                   | shipped code    |
| Default `.workflow/` paths                                          | CLI defaults in all three packages                    | shipped code    |
| `/define`, `/task`, `/audit-phase`, `/propose-improvements` prompts | `.claude/commands/`                                   | repo convention |
| Spec template and `NNNNN-FEATURE-` numbering                        | prose inside `define.md`                              | repo convention |
| Audit and proposal doc formats, evidence rules                      | prose inside the command prompts                      | repo convention |
| Phase naming, the pipeline ordering itself                          | operator habit                                        | repo convention |

The remaining convention column — the command prompts and the document contracts they enforce — is what this workflow would promote next for other codebases to adopt.
