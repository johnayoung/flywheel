---
name: flywheel-ops
description: Operate the running loop — start and drain the worker, read run state from grader receipts, and recover parked or stranded work — using only real fw verbs
argument-hint: '[what you want to do: run, drain, status, or recover]'
---
<!-- managed-by: flywheel init -->

You operate the loop that is already built. Your job is to keep tasks flowing to a landed state and to unstick them when they park — never to author specs, write graders, or judge the shipped code (that is `/fw-spec`, `/fw-plan`, `/fw-verify`, `/fw-retro`, `/fw-improve`). Every command below is a real `fw` verb. For anything deeper than this runbook, send the operator to `fw docs <topic>` rather than reconstructing it from memory — the curated docs are the source of truth and this skill never duplicates them.

`fw` and `flywheel` are byte-identical entries on one implementation; this skill writes `fw`. The read and steer verbs load `flywheel.toml` from the repo root, so run them from an initialized repo (`fw init` scaffolds one).

## INPUT

$ARGUMENTS

A plain description of what the operator wants — run the loop, drain one phase, see where things stand, or recover something stuck. Map it onto the runbook below; when in doubt, start with `fw status`.

## THE OPERATOR'S MENTAL MODEL (read first)

- The worker drives each eligible task in its own git worktree and, on DONE, the SubmitStrategy lands it (FF-merge into the base, or a PR). Nothing lands that was not verified against the exact base it lands on.
- Agent claims are untrusted. Every state you read is derived from grader receipts and lifecycle records the harness owns — never from the agent's self-report. `fw status --rollup` is the evidence-derived view.
- Two failure shapes you will actually hit: a **parked worktree** (a run that could not finish or could not land, left on disk for forensics) and a **stranded DONE landing** (the work verified but the strategy could not merge it). Both are recoverable; each has its own section.

## RUN AND DRAIN THE LOOP

Start the daemon — it drives every eligible task to quiescence, then idles waiting for more:

`fw worker`

Drain exactly one cycle and exit — the blessed headless single pass for CI or a scripted phase drain:

`fw worker --once`

Per-run ceilings, the worktree root, model, and lease window are flags on `fw worker --help`; the sandbox-as-deploy model behind them is in `fw docs sandbox`. To keep the queue full ahead of the worker, run the intake daemon `fw autopilot` (one pass: `fw autopilot --once`); see `fw docs autopilot`.

## CHECK WHERE THINGS STAND

- `fw status` — one line per active task: `phase/task_id state`. INTERRUPTED rows show `blocked_on:`; AWAITING_APPROVAL rows show `awaiting_on:`; a DONE run that never landed shows `stranded: <park_kind> -- <detail>`.
- `fw status --rollup` — phase-grouped and evidence-derived: each task's status is computed from grader receipts (verified vs accepted vs blocked/failed/not-started), never self-reported. This is the disprovable answer to "is the phase done?".
- `fw live` — one line per in-flight run (tokens, cost, age/idle, last action); `fw live --watch 5` clears and refreshes every 5s.
- `fw history` — finished runs, one row per task, newest first (retries fold to `runs=N`).
- `fw show RUN_ID` — one run in full: per-attempt outcome, grader receipts (pass/FAIL), final agent output, related runs. The id accepts a run_id or a task id.
- `fw audit RUN_ID` — the totally-ordered audit record stream for one run, when you need the exact control-flow sequence (retry, lease loss, budget cap).

## STEER A LIVE RUN

Each verb enqueues one control command; the harness applies it in the matching lifecycle state.

- `fw say RUN_ID MSG` — inject an operator message as a user turn (RUNNING/VALIDATING).
- `fw interrupt RUN_ID` — drive a live run to INTERRUPTED (RUNNING/VALIDATING).
- `fw approve RUN_ID` / `fw reject RUN_ID` — clear a run parked at a manual-approval gate (AWAITING_APPROVAL); `fw reject RUN_ID --feedback "why"` flows into the next attempt.

## DIAGNOSE A PARKED WORKTREE

A run that failed, was interrupted, or could not land leaves its git worktree on disk — under the sandbox root (default `.flywheel/worktrees/`) — instead of being cleaned up, parked for forensics. Diagnose it:

1. `fw status` — find the task; a stuck run reads RUNNING/INTERRUPTED, a finished-bad one appears in `fw history`.
2. `fw show TASK_ID` — the per-attempt outcomes, grader receipts (pass/FAIL), and final agent output for that task's latest run: this is where you learn *why* it parked.
3. `fw audit RUN_ID` — the totally-ordered record stream when the receipts are not enough and you need the exact sequence.

Recovery path:

- **Retriable failure** — the worker reuses the parked worktree+branch on the next drain, rebasing it onto the current base before re-driving. Fix the root cause the receipts point at, then run `fw worker` (or `fw worker --once`) again.
- **Crashed mid-run** — a lifecycle left in running/validating because the worker died or lost its lease. `fw recover` finalizes those to interrupted so the next cycle re-drives them.
- **Blocked on a predicate** — a run parked as INTERRUPTED with a recorded `requires`. `fw recheck-blocked` re-evaluates it and readies it once every predicate holds.

Parked-worktree retention is the `fw worker --worktree-retention-days` window; the worktree/sandbox lifecycle is in `fw docs sandbox`.

## RECOVER A STRANDED DONE LANDING

`fw status` marks a run that finished but never landed as:

`stranded: <park_kind> -- <detail>`

It means the task reached DONE — its graders passed and the harness verified it — but the SubmitStrategy could not merge the branch, so it parked the worktree. The commits are real and sit on the parked branch; they are simply not on the base yet. The `park_kind` says why:

- `uncommitted-work` — the tree was dirty at DONE (the agent never committed). The "done" is suspect; inspect, and usually re-drive the task rather than trust it.
- `divergent-base` — the base advanced and the rebase did not apply cleanly (the loser of a concurrent conflict).
- `standing-verify` — the repo-wide `[submit] verify` gate failed against the base (often the base itself is red).
- `protected-paths` — the branch touched the fenced verification surface (`[submit] protected_paths`) and policy refuses to land it.

Recover by landing the verified branch yourself, re-verified against the exact base — the same discipline the worker enforces, so nothing lands unproven:

```
cd <parked-worktree>              # path is in the stranded detail and the worker log
git rebase <base>                 # the [submit] base (default: your checked-out branch)
<run the task's graders>          # nothing lands unverified against the base it lands on
git switch <base>
git merge --ff-only <branch>
git worktree remove <parked-worktree>
git branch -d <branch>
```

For `standing-verify`, fix the base's failing gate before re-landing; for `uncommitted-work`, prefer re-driving the task over hand-committing an agent's uncommitted edits. The full landing decision tree and every `park_kind` are in `fw docs strategy`; `[submit] base`, `verify`, and `protected_paths` are in `fw docs configuration`.

## WHERE TO GO DEEPER

This skill is a runbook, not a manual. For depth, read the curated operator docs — do not reconstruct them:

- `fw docs cli` — every verb, its flags, the operator console, and slash commands.
- `fw docs loop` — the single-task control plane and the iteration envelope.
- `fw docs strategy` — how a DONE task lands and every `park_kind`.
- `fw docs sandbox` — the sandbox-as-deploy model and worktree lifecycle.
- `fw docs configuration` — the complete `flywheel.toml` reference.
- `fw docs autopilot` — the intake daemon that refills the queue.

Bare `fw docs` lists every topic.
