# Workflow

This is the home of flywheel's **intent-authoring pipeline** — the write-half of the binding the [vision](vision.md) North Star describes (authoring intent an agent cannot game, then compiling it into `Task`/`Grader`). It ships as the `fw-*` skills `flywheel init --skills` installs into any repo; this doc is also the record of how flywheel runs that pipeline on itself.

How flywheel develops itself. Every feature since the Postgres store has gone through this pipeline: a spec is defined, decomposed into task JSONs, executed by flywheel's own worker loop, then audited for loop friction that feeds the next round of features. The pipeline lives half in the `fw-*` authoring skills (run by an operator in Claude Code) and half in `.flywheel/` (artifacts and runtime state; formerly `.workflow/`, cut over wholesale with history preserved). This doc records the current state and rationale. The authoring half has been promoted into shipped code — the `fw-spec`/`fw-plan`/`fw-verify`/`fw-retro`/`fw-improve` skills that `flywheel init --skills` installs into any repo; the original slash-command prompts they were promoted from have since been removed. The rest is the baseline for promoting further pieces.

## Pipeline at a glance

| Stage   | Skill                    | Input                       | Output                                                           |
| ------- | ------------------------ | --------------------------- | ---------------------------------------------------------------- |
| Spec    | `/fw-spec`               | vague feature idea          | spec with held-out, ungameable success criteria: `.flywheel/specs/NNNNN-FEATURE-<name>.md` |
| Plan    | `/fw-plan`               | spec reference or free text | task JSONs, each spined on a grader: `.flywheel/tasks/active/NN-<phase>/<id>.json` |
| Verify  | `/fw-verify` (optional)  | planned tasks for a phase   | blind-authored held-out oracles: discrimination proof in `.flywheel/audits/`, throwaway oracle in git-ignored `.flywheel/verification/`, optional `[held_out] root` registration. Runs between plan and execute |
| Execute | `flywheel worker` daemon | active phase dirs           | landed work (FF-merge into the base, or a PR), lifecycles in the store, archived phase |
| Retro   | `/fw-retro`              | store + logs for a phase    | cited loop-friction findings: `.flywheel/audits/<phase>.md`     |
| Improve | `/fw-improve`            | one or more retros          | ranked proposals, each handed to `/fw-spec`, `/fw-plan`, or "accept": `.flywheel/proposals/<phase>.md` |

The loop closes: proposals become specs become phases become retros. The five stages ship as the `fw-*` skills `flywheel init --skills` installs into any repo (flywheel's own included); the old `/define`, `/task`, `/audit-phase`, and `/propose-improvements` slash commands they were promoted from no longer exist. Two further commands review the *work* rather than the loop (`/review-phase`, `/arewedone`); they live at user level (`~/.claude/commands/`), not in this repo.

### Stage contracts

- **`/fw-spec`** is a requirements interview whose product is justified trust, not a feature description: it asks rationed `AskUserQuestion` rounds until it can author *ungameable* success criteria — each atomic, in EARS condition->response form, grading an observable end-state, and carrying a grader type, a `visible|held-out` flag, a `verify:` line (the exact check), and a `defends against:` line (the cheapest fake it forecloses). A criterion that cannot be lowered to a grader is a blocking defect. It runs a blocking gradeability gate and a verification-surface gate, then emits an immutable spec with a Decisions Log.
- **`/fw-plan`** compiles each spec criterion into one JSON task per `docs/task-schema.md`, grader first: it writes the strongest reward-hack-resistant grader the worker can run out-of-band before the `goal`. Discipline it enforces: one-sentence `goal` naming an observable behavior change, at least one grader, `prerequisites` as the only ordering mechanism, per-task graders scoped narrow (never the whole-repo gate), `context.relevant` pinned and the grading surface fenced in `context.non_goals`, a constraint telling the agent to commit before verify. It always presents the proposal before writing files.
- **`/fw-verify`** (optional, between plan and execute) blind-authors a held-out test *oracle* for each held-out behavior criterion, so the agent never writes the test that grades it. It proves the oracle DISCRIMINATES — kills a synthesized plausible-wrong reference and passes a correct one — and records that proof under `.flywheel/audits/`; the throwaway oracle lands only in git-ignored `.flywheel/verification/`, never in the committed tree. A criterion no discriminating oracle can be authored for routes to manual or back upstream to `/fw-spec` as under-specified.
- **`/fw-retro`** is a forensic audit of *the loop's execution*, not the shipped code. Every finding carries a re-verifiable pointer (a `run_id`, an audit record, a grader receipt, or a transcript line) or is dropped, and it stops at diagnosis — proposing fixes is explicitly forbidden. A phase the loop never ran gets a short "not observed" note, not an invented report.
- **`/fw-improve`** is the action half: every proposal traces to a cited retro finding, states an observable outcome (never an implementation), is ranked by leverage, and ends in a handoff to `/fw-spec`, `/fw-plan`, or "accept — do not fix". The operator picks what advances via `AskUserQuestion`.

## Artifact layout

```
.flywheel/
  specs/                    NNNNN-FEATURE-<name>.md (sequential, zero-padded)
  tasks/
    active/NN-<phase>/      one JSON per task; the worker consumes these
                            (base SHA for an active phase lives in the
                            refs/flywheel/loop-base/<phase> git ref; archiving
                            materializes it into a .loop-base dotfile)
      loop-path-exempt.md   optional gate opt-out (phase/author/reason front-matter)
    archive/NN-<phase>/     moved here by the archive gate when all tasks are DONE
  audits/<phase>.md         committed audit findings
  proposals/<phase>.md      committed proposal docs
  flywheel.sqlite           runtime store (gitignored; lifecycles, attempts, events, grader_results)
  worktrees/                per-task git worktrees (runtime)
  .merge.lock               serializes FF-merges into main
  logs/worker/              supervisor per-run worker logs
  logs/runs/                per-run agent transcripts
```

Phases are plain directories: the `NN-` prefix controls walk order, there is no phase metadata, and cross-task ordering lives only in each task's `prerequisites`.

## The runtime loop

The daemon is invoked as `flywheel worker` (the product shell delegates in-process to `packages/flywheel-worktree/src/flywheel_worktree/worker.py`). Each cycle:

1. Record a base SHA (the `refs/flywheel/loop-base/<phase>` ref) for any active phase that lacks one. The worker never creates commits on the operator's branch — landing work is the submit strategy's job, bookkeeping lives in the ref namespace, and versioning task JSONs is the operator's choice.
2. `orchestrate()` — drive every eligible task to quiescence (`packages/flywheel-orchestrator/`): a task is eligible when its state is FRESH/RETRYABLE/INTERRUPTED and every prerequisite is DONE; each task runs in its own git worktree branched off main and FF-merges back on DONE under the merge lock. If the base advanced under a finished task, the branch is rebased once and its command graders re-run against the rebased tree (still under the lock) before the merge; a red re-run parks the worktree instead of merging — nothing lands that was not verified against the exact base it lands on.
3. Write per-run logs to `.flywheel/logs/` (supervisor logs under `logs/worker/`, agent transcripts under `logs/runs/`).
4. `archive_completed_phases()` — move fully-DONE phases to `archive/`, subject to the gate below.

Default paths are code, not convention: `DEFAULT_TASKS_DIR = .flywheel/tasks` (`flywheel_orchestrator/_workflow.py:113`) and `DEFAULT_DB_PATH = .flywheel/flywheel.sqlite` (`flywheel_core/workflow.py:92`); the worker logs default to `.flywheel/logs/worker/` (supervisor) and `.flywheel/logs/runs/` (per-run agent transcripts). All are overridable via CLI flags.

### The in-loop-verification gate

`archive_completed_phases` diffs the phase against its recorded base (the loop-base ref while active; materialized into the archived dir as a `.loop-base` dotfile) and scans for five watched signals (`flywheel_core/loop_path_marker.py`): a new `Status`/`Outcome`/transition rule, a new schema column or table, a new `Grader` variant, a new store-Protocol method with dispatch, a new control-command verb. If any trips, the phase cannot archive without either a DONE task tagged `in-loop-verification` (a test that drives the real `orchestrate` loop with a scripted invoker) or a committed `loop-path-exempt.md` opt-out. The gate is enforced mechanically by the archive step (`flywheel_orchestrator/_workflow.py`), not by the authoring skills: the shipped `fw-*` skills are project-agnostic and carry no loop-path logic, so on flywheel's own repo the operator adds the tagged in-loop-verification task or the opt-out by hand.

## Why it works this way

Each rule exists because an audit caught its absence:

- **Discovery before tasks.** Under-specified tasks are the dominant source of loop waste — the agent burns context discovering what the operator already knew. `/fw-spec` forces ambiguity to zero and authors ungameable criteria before any task exists; `context.relevant` in task JSONs is the single biggest lever on context burn.
- **Evidence and action are separate skills.** `/fw-retro` may not propose; `/fw-improve` may not invent findings. Splitting them prevents audits from padding into feature backlogs and keeps every proposal traceable to a cited pointer.
- **The gate exists because unit tests lied.** Phase 08 shipped a schema change whose graders passed while the live store was never migrated; phase 17 built the manual-approval gate without ever producing an `AWAITING_APPROVAL` lifecycle in-loop; phase 19 merged code a stale worker process never executed. The gate makes "the real loop ran the new path" a mechanical archive precondition instead of a hope.
- **Enumerate dependents of shared invariants.** A task that changes a shape other graders assert against must update those tests in the same commit, or the next task inherits a red suite (phase 02 audit).
- **One task per file, phases as bare directories.** The worker iterates files; keeping phase semantics out of the schema keeps `Task` tight and ordering explicit.

## Work sources (project-agnostic boundary)

The orchestrator does not consume `.flywheel/tasks/` directly anymore — it consumes a `WorkSource` (`packages/flywheel-orchestrator/src/flywheel_orchestrator/_sources.py`):

- **Inbound** — `list_work()` returns `WorkItem`s, each a validated core `Task` plus `prerequisites` and an opaque `source_ref`. Anything that cannot compile to a Task with at least one grader never reaches the scheduler.
- **Outbound** — `report(WorkReport)` receives each driven run's terminal status, run id, and final grader receipts after the consumer `submit` step, still under the task lease. Delivery is best-effort; a raising report never unwinds the loop. Ticket writes go through this path, never through the agent.
- **Steering** — a reconciler re-lists the source every `--reconcile-seconds` (default 15, 0 disables) and enqueues an `interrupt` control command for any in-flight run whose item is no longer listed (closed issue, pulled label, deleted task file). A listing failure never interrupts anything. The run parks as `INTERRUPTED` with its sandbox preserved — restore the item and it resumes.

Four adapters ship today, registered in `_source_registry.py` (the registry is entry-point-extensible via the `flywheel.work_sources` group; built-ins win a name collision). The grade is always the policy's graders run out-of-band by the harness, never the tracker's own status — see [work-sources.md](work-sources.md) for the full per-kind reference.

| `[source] kind` | Inbound | Outbound | Grade |
| --------------- | ------- | -------- | ----- |
| `directory` | `.flywheel/tasks/active/<phase>/*.json` task files | `report` is a no-op (the store is the local record; phase archiving stays a separate directory flow) | task file's own graders |
| `github` | labeled open issues via `gh` (`gh-<number>` ids; optional fenced ` ```flywheel ` body block overrides goal/graders/context/tags/prerequisites) | comment, or close on DONE when `done_action = "close"` | issue block graders, else `[[defaults.graders]]`, else skipped |
| `github_ci` | failed CI runs via `gh run list --status <failure_filter>` (stable per-`(workflow, branch)` ids) | commit comment | `[[defaults.graders]]` only (out-of-band; never the check status) |
| `github_review` | unresolved PR review threads via `gh api graphql` (`isResolved` is a candidate filter, never the verdict) | PR comment (never a resolve/reply mutation) | `[[defaults.graders]]` only |

The GitHub kinds drive the `gh` CLI (no Python extra); `gh` must be on PATH and authed. `init` scaffolds only `directory`/`github` — `github_ci`/`github_review` are hand-edited into the file.

`flywheel.toml` at the repo root selects the source per project (`_policy.py`); see [configuration.md](configuration.md) for the complete key reference:

```toml
[source]
kind = "github"            # or "directory"
repo = "owner/name"
label = "flywheel"
done_action = "close"      # or "comment"

[[defaults.graders]]
type = "command"
run = "uv run pytest"

[submit]
# How DONE work lands: "merge" FF-merges into the worker's base (full
# autonomy, default); "pr" pushes the branch and opens a PR with grader
# receipts in the body (remote/pr_base configurable) — review owns the merge.
strategy = "merge"
# Glob patterns (** crosses directories) a finished task's branch may not
# touch; a match refuses the landing and parks the work. Protects the
# verification surface (grader configs, CI) from the work it judges.
protected_paths = [".github/**", "flywheel.toml"]

[sandbox]
# Shell command run inside every newly created worktree before the agent
# enters (deps install, codegen). Reused parked worktrees skip it.
setup = "uv sync"
```

`flywheel status|live|archive|recover|recheck-blocked` auto-detect `flywheel.toml` (override with `--policy`; an explicit `--tasks-dir`/`--db` flag always wins). The optional `[paths]` table pins the store db and sandbox root so an initialized repo never falls back to `.flywheel/` defaults. The bare `next` and `orchestrate` verbs are intentionally not exposed on the product shell -- use `flywheel worker [--once]` to drive a phase. See [cli.md](cli.md) for the full verb and console reference, and [autopilot.md](autopilot.md) for the intake daemon that authors and refills work ahead of the worker.

`flywheel init` scaffolds the self-contained local layout — `.flywheel/tasks/{active,archive}/`, a `.flywheel/.gitignore` for runtime state, and a repo-root `flywheel.toml` pointing everything at `.flywheel/`; when a Dockerfile is present it also appends `.flywheel/` to `.dockerignore`, since docker build contexts do not read `.gitignore`. Idempotent; never overwrites. `flywheel init --skills` additionally renders the five authoring-pipeline skills (`fw-spec`/`fw-plan`/`fw-verify`/`fw-retro`/`fw-improve`, in pipeline order — `SKILL_NAMES`, `_skills.py:41`) into `.claude/skills/`, parameterized by the work policy (`fw-plan` gets the task-directory or GitHub-issues delivery section to match the source). `/fw-verify` runs between plan and execute: it blind-authors the discriminating held-out test oracle for the planned tasks so the agent never writes the test that grades it — the write-half of the execute-time [held-out-gate.md](held-out-gate.md). This repo adopted the layout itself: the old hand-rolled `.workflow/` tree was migrated wholesale into `.flywheel/` (specs, audits, archived phases, the live store) and no longer exists.

## Code vs. convention

What it takes for another codebase to run this workflow — most of it now ships as code:

| Piece                                                               | Lives in                                              | Status          |
| ------------------------------------------------------------------- | ----------------------------------------------------- | --------------- |
| Task selection, claims/leases, `orchestrate`                        | `flywheel-orchestrator`                               | shipped code    |
| `WorkSource` seam, directory + GitHub adapters, `flywheel.toml`     | `flywheel-orchestrator` (`_sources`, `_github`, `_policy`) | shipped code    |
| Archive gate, loop-path signals, `.loop-base`, opt-out parsing      | `flywheel-orchestrator` + `flywheel_core.loop_path_marker` | shipped code    |
| Worktree-per-task submit strategy, daemon                           | `flywheel-worktree` package (library)                 | shipped code    |
| Default `.flywheel/` paths                                          | CLI defaults in all three packages                    | shipped code    |
| Authoring pipeline as installable skills (`fw-spec`/`fw-plan`/`fw-verify`/`fw-retro`/`fw-improve`) | `flywheel-orchestrator` (`_skills`, `_skill_templates/`); installed by `flywheel init --skills` | shipped code |
| Spec/task/audit/proposal doc formats, `NNNNN-FEATURE-` numbering, evidence rules | rendered inside those skill templates | shipped code |
| Phase naming, the pipeline ordering itself | operator habit | repo convention |

The command prompts and the document contracts they enforce have since been promoted: they ship as the `fw-*` skills `flywheel init --skills` installs into any repo, and the original slash-command prompts they were promoted from have been removed. What remains convention is the thin operator-habit layer — phase naming and the pipeline ordering itself.
