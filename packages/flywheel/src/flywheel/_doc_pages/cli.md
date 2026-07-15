# CLI and operator console

The `flywheel` / `fw` surface is the top-of-stack product shell: the operator's single entry point for driving the loop day to day. It is a thin verb router over the layers below it (core, orchestrator, worktree) plus a Textual operator console and a `--json` snapshot mode. This doc is the canonical reference for every verb, flag, key binding, and slash command.

## At a glance

| Surface | Invocation | Use |
|---|---|---|
| Operator console (TUI) | bare `fw` on a TTY | Watch active runs, drill into a session, steer, supervise the worker/autopilot |
| JSON snapshot | `fw --json` (or any non-TTY stdout) | One machine-readable frame for scripts |
| Read verbs | `fw status` / `live` / `history` / `show` / `validate` | Inspect state, runs, and task definitions |
| Steering verbs | `fw say` / `interrupt` / `approve` / `reject` | Enqueue one control command against a live run |
| Daemons | `fw worker` / `autopilot` / `triage` | Drain the queue (worker), keep it full (autopilot), or triage the intake board (triage) |
| Audit | `fw audit RUN_ID` | Stream the totally-ordered telemetry for one run |
| Setup | `fw init` | Scaffold `.flywheel/` and a `flywheel.toml` work policy |

`flywheel` and `fw` are byte-identical console scripts on one implementation (`flywheel._cli:main`, `packages/flywheel/pyproject.toml:23-25`). Use either; this doc writes `fw` for brevity.

## Entry points and the verb router

The router (`flywheel._cli.main`, `_cli.py:105`) dispatches by the first argv token:

- No args open the Textual console on a TTY, or print one `--json` snapshot when stdout is not a TTY (`_cli.py:117-118`).
- `-h` / `--help` print the verb list and exit 0 (`_cli.py:122-124`).
- A leading-`-` first token (`fw --json`, `fw --db ...`) is treated as a console flag and handed to the TUI (`_cli.py:128-129`).
- Otherwise the first token is the verb and the remaining argv is forwarded verbatim to the delegated implementation.

`fw <verb> --help` forwards `--help` to that verb's own parser, so each verb's specific flags come from the delegated parser, not from the router (`_cli.py:99-101`).

**`run`, `next`, and bare `orchestrate` are intentionally not routed.** Spec 00021 FR-4 keeps them as `python -m` plumbing; `worker --once` is the only blessed headless single-task drain (`_cli.py:21-23`). An unknown verb prints `fw: unknown command: <verb>` to stderr and exits 2 (`_cli.py:153-155`).

### Resolution precedence

Every delegated verb resolves its inputs the same way: **explicit flag wins, then `flywheel.toml` policy, then a built-in default.**

| Input | Explicit flag | Policy key | Built-in default | Anchor |
|---|---|---|---|---|
| Policy | `--policy FILE` | auto-detect `flywheel.toml` in cwd | none (`.flywheel/` defaults) | `_workflow.py:779` |
| DB path | `--db PATH` | `[paths] db` | `.flywheel/flywheel.sqlite` | `_workflow.py:822` |
| Work source | `--tasks-dir DIR` (forces directory source) | `[source]` (directory or github) | `.flywheel/tasks` | `_workflow.py:807` |

The Postgres DSN is never stored in `flywheel.toml`. It is read only from the environment: `FLYWHEEL_PG_DSN`, falling back to `DATABASE_URL` (`_store_factory.py:33-35`). See [configuration.md](configuration.md) for the full policy schema.

## Read verbs

Delegated to `flywheel_orchestrator._workflow.main`. On a load/policy/work-source error these print `error: ...` to stderr and exit 2 (`_workflow.py:3527-3532`).

| Verb | What it does | Key flags |
|---|---|---|
| `status` | One line per active task: `phase/task_id state [-- error]`. INTERRUPTED rows show `blocked_on:`; AWAITING_APPROVAL rows show `awaiting_on: <instruction>`; a DONE run that never landed shows `stranded: <park_kind> -- <detail>` (the work finished but the strategy parked it -- uncommitted tree, divergent base, or a failed `[submit] verify`). `--json` emits a per-task array, each entry carrying `stranded: {park_kind, detail}` when parked (omitted otherwise). | `--json`, `--rollup`, `--tasks-dir`, `--policy`, `--db` |
| `status --rollup` | Phase-grouped, **evidence-derived** rollup: each task's status is computed from grader receipts (verified vs accepted vs blocked/failed/not-started), never self-reported. | `--rollup`, `--json` |
| `live` | One line per in-flight run (RUNNING / VALIDATING / AWAITING_APPROVAL): status, `attempt=N iter=K`, `tokens=/cost=/turns=`, `age=` / `idle=` (marked `STALE` past 90s idle), last action. `--watch N` clears and refreshes every N seconds (Ctrl-C exits). | `--watch SECONDS`, `--policy`, `--db` (no `--tasks-dir`) |
| `history` | Finished runs (done / failed / failed_validation), one line per task, newest first; retries fold to one row `runs=N`. | `--status S` (repeatable), `--phase`, `--limit N`, `--json`, `--tasks-dir`, `--policy`, `--db` |
| `show ID` | One run in full: lifecycle, per-attempt outcome/iter/turns/tokens/cost, grader receipts (pass/FAIL), final agent output, related runs. `ID` accepts a run_id or a task id (latest run). | positional `run_or_task_id`, `--json`, `--tasks-dir`, `--policy`, `--db` |
| `validate` | Statically validate the active listing without running anything: per-task, each command grader must shell-parse (`bash -n`); pairwise, two tasks' derived file surfaces (context.relevant paths + command-grader run paths) must not overlap unless they share a `conflict_keys` entry, list the path in a top-level `overlap_ok` array, or are chained via `prerequisites`. Never checks whether a derived path exists on disk (an output path is still a surface). Exits non-zero naming each offending task (and, for an overlap, both task ids and every shared path), else `All N active task(s) valid.` Lints DONE-but-still-listed tasks too -- archiving, not status, is the operator's lever. Directory sources only. | `--tasks-dir`, `--policy` (no `--db`) |
| `archive` | Move active `<phase>` dirs whose tasks are all DONE into `archive/`, printing each moved dir. Directory sources only. A loop-path / phase-verify gate applies when `repo_root` is threaded (see [loop.md](loop.md)). | `--tasks-dir`, `--policy`, `--db` |
| `recover` | Finalize lifecycles stranded in running/validating to interrupted; prints transitioned run_ids, else `(no stranded lifecycles)`. | `--task-id`, `--policy`, `--db` |
| `resolve ID` | Deliberately abandon a strand: record an operator-attributed `stop-resolved` marker keyed to the task id, carrying `--reason` verbatim, so the next `archive` sweep archives the otherwise-landed phase. The only non-probe path to clearing a strand (never manual store SQL, never a task-file tombstone). Refuses (exit 1) when the task has no unresolved stop event; empty `--reason` exits 2. | positional `task_id`, required `--reason TEXT`, `--policy`, `--db` |
| `recheck-blocked` | Re-evaluate blocked (INTERRUPTED with persisted `requires`) lifecycles; when all predicates are satisfied, transition interrupted to ready. Default scans all. | `--run-id`, `--dry-run`, `--tasks-dir`, `--policy`, `--db` |

`status --rollup` is evidence-derived from grader receipts, not from any agent claim; see [work-sources.md](work-sources.md) and [orchestration.md](orchestration.md) for how items and phases map onto it. `recheck-blocked --dry-run` reports without transitioning (emits `harness.recheck_attempted`, never `harness.unblocked`).

## Steering verbs

Delegated to `flywheel_core.workflow.main`. Each enqueues exactly one row into the `control_commands` table keyed by `run_id` (`_enqueue_control_command`, `workflow.py:~1103-1165`): it validates the run exists, prints `enqueued #<id> kind=<k> run_id=<id>`, and warns when the run is not in-flight.

**Agent claims are untrusted; the harness owns every lifecycle transition.** These verbs are permitted only in the matching lifecycle state — the store records the command, the harness applies it. See [task-lifecycle.md](task-lifecycle.md).

| Verb | What it does | Allowed state | Key args |
|---|---|---|---|
| `say RUN_ID MSG` | Inject one operator message as a user turn into the live session. The task definition is not mutated. Empty message exits 2. | RUNNING / VALIDATING | positional `run_id`, `message`; `--db` |
| `interrupt RUN_ID` | Drive the live run to INTERRUPTED via in-band finalization; records `harness.control_command_applied`. | RUNNING / VALIDATING | positional `run_id`; `--db` |
| `approve RUN_ID` | Approve a parked manual gate: writes a `passed=True` manual receipt, then re-parks on the next gate or transitions to DONE. | AWAITING_APPROVAL | positional `run_id`; `--db` |
| `reject RUN_ID` | Reject a parked manual gate: writes a `passed=False` receipt, transitions to FAILED_VALIDATION. `--feedback` flows into the next attempt's reviewer-feedback section (absent renders `(no feedback provided)`). | AWAITING_APPROVAL | positional `run_id`; `--feedback TEXT`; `--db` |

The shell's `say` is a surface rename of core's `steer` verb; the underlying core CLI is unchanged (`_cli.py:138-140`). Core's `set_model` verb exists but is not routed on the product shell.

## Daemons and audit

### `worker [--once]`

The blessed headless drain, delegated in-process to `flywheel_worktree.worker.main` (`worker.py:2335`). It drives tasks under active phase dirs, each in its own git worktree, fast-forward-merging on done and parking on failure. `--once` runs a single drain cycle; without it the worker is a daemon.

| Flag | Controls |
|---|---|
| `--once` | single drain cycle, no daemon loop |
| `--tasks-dir`, `--db`, `--policy` | standard resolution inputs |
| `--sandbox-root` | worktree root (overrides `[paths] sandbox_root`); accepts a path or the `@cache`/`@sibling` tokens (see [configuration.md](configuration.md)) |
| `--model` | agent model (overrides `[agent] model`) |
| `--max-turns`, `--max-retries` | per-run ceilings |
| `--worker-id` | identity stamped on claims |
| `--lease-seconds` | claim lease window (default 300) |
| `--reconcile-seconds` | steering-bridge cadence: re-list the work source and interrupt in-flight runs whose item vanished (default 15; 0 disables) |
| `--poll-interval` | idle wait between drain cycles |
| `--heartbeat` | progress line every N seconds (0 disables) |
| `--worktree-retention-days` | parked-worktree retention |

### `autopilot [--once]`

Keeps the work queue full with verifiable, tier-prioritized tasks. Delegated to `flywheel_orchestrator._autopilot_run.main` (`_autopilot_run.py:209`). **Neverending daemon by default**; `--once` runs one refill pass and exits. The daemon loop never terminates on an idle cycle — it exits only on SIGTERM/SIGINT (`run_daemon_loop`, `_autopilot_run.py:136`).

| Flag | Controls |
|---|---|
| `--once` | one refill pass then exit |
| `--tasks-dir`, `--model` | standard inputs |
| `--target-depth N` | fill the queue to depth N (overrides `[autopilot]` config; default 5) |
| `--interval SECONDS` | daemon cycle interval (overrides config; default 300.0) |

Landing strategy and scoring weights come from policy (`autopilot_landing` default `merge`, `autopilot_weights`). See [autopilot.md](autopilot.md) for the tier and scoring model.

### `triage [--once]`

Triages the GitHub intake board: authors a fail-first spec+grader for each candidate issue, then labels well-specified issues ready and under-specified ones needs-detail, each with a receipt. Delegated to `flywheel_orchestrator._triage_run.main`. **Neverending label-polling daemon by default**; `--once` runs one triage pass and exits 0. The daemon loop never terminates on an idle cycle — an empty board writes nothing and the next cycle is scheduled; it exits only on SIGTERM/SIGINT (`run_daemon_loop`).

The policy is resolved and validated **before** the repo root or any `gh` call: a malformed `[triage]` value prints to stderr and exits 2 having issued no GitHub write. Triage requires `[source] kind = "github"` with a `repo`; any other source exits 2. The board labels and cadence come from the optional `[triage]` table (`intake_label` default `flywheel`, `ready_label` default `flywheel:ready`, `needs_detail_label` default `flywheel:needs-detail`, `interval_seconds` default `300.0`, `max_per_pass` uncapped); see [configuration.md](configuration.md).

| Flag | Controls |
|---|---|
| `--once` | one triage pass then exit |
| `--interval SECONDS` | daemon cycle interval (overrides `[triage] interval_seconds`; default 300.0) |
| `--model` | agent model for the authoring session (overrides `[agent] model`) |

### `audit RUN_ID`

Streams the totally-ordered audit records for one run, delegated to `flywheel_core.audit._cli.main` (`_cli.py:478`). **Redaction is ON by default** — `--raw` disables it only for authorized forensics.

| Flag | Controls |
|---|---|
| positional `run_id` | the run to stream |
| `--db`, `--logs-root` | store and log roots |
| `--json` | NDJSON output |
| `--follow` | tail until terminal status (Ctrl-C exits 130) |
| `--poll-interval` | tail cadence |
| `--redact` | explicit opt-in (already default) |
| `--redact-policy NAME\|module:factory`, `--redact-salt SALT` | redaction tuning |
| `--raw` | disable redaction (authorized forensics) |
| `--dry-run` | coverage report only |

## The operator console (TUI)

Entry: `flywheel._tui.main` (`_tui.py:75`). Bare `fw` on a TTY opens it; `fw --json` or any non-TTY stdout prints one `DashboardSnapshot` and exits 0 with no ANSI.

The console fails fast on a missing store before launching: a missing sqlite file exits 2 with the remedy `run 'fw init' ...`; a Postgres backend with no resolvable DSN env exits 2 (`_tui.py:86-97`).

### Console flags

| Flag | Controls |
|---|---|
| `--db PATH` | store path (default policy `[paths] db` else `.flywheel/flywheel.sqlite`) |
| `--policy FILE` | work-policy file (default `flywheel.toml` if present) |
| `--tasks-dir DIR` | tasks root for summary counts and `/archive` |
| `--json` | print one JSON snapshot and exit (implied off a non-TTY) |
| `--poll-interval FLOAT` | dashboard poll cadence (default 1.0s) |
| `--model ID` | model forwarded to the spawned worker (overrides `[agent] model`) |
| `--no-worker` | do not spawn a supervised worker on launch (status bar shows `worker: none`) |

On launch the console opens the store, constructs a `WorkerSupervisor` and spawns it unless `--no-worker`, and constructs an `AutopilotSupervisor` that is **never auto-spawned** — autopilot starts only via `/autopilot start` because it writes to the base branch unattended (decision D-6, `_tui.py:367-544`). On exit both supervisors detach by default: the child keeps running so a SIGINT on the console never silently kills it.

### Dashboard

`flywheel._dashboard.DashboardApp` (`_dashboard.py:170`) polls a `DashboardSnapshot` roughly once a second. Layout: a summary header (`active= queued= done= failed= tokens= cost= runtime=`), a worker/autopilot status bar, a table of active runs (task / status / attempt-iter / age / tokens / cost / last action), an empty-state line, a last-error status bar, and a persistent input bar. Rows that leave the active set linger dimmed for 30 seconds before dropping; a poll failure leaves the last good frame plus a warning rather than crashing.

The input bar live-filters rows by case-insensitive substring; a leading `/` switches it to slash-command mode.

Worker status-bar vocabulary: `worker: supervised pid=N` / `detached (this console did not spawn it)` / `none -- type '/worker start' to spawn one` / `dead ... (exit=N)` / `error: <reason>`. Autopilot status has no `detached` state (it writes no lease): `autopilot: supervised pid=N` / `none ...` / `dead ...` / `error ...`.

### Session screen

`flywheel._session_screen.SessionScreen` (`_session_screen.py:166`), opened by Enter on a dashboard row. It renders one run's merged audit transcript chat-style via a `TranscriptTailer` (~250ms poll) and tail-follows until you scroll up (follow pauses, new-activity indicator) or the run reaches a terminal status (banner pins the state, steering disabled).

The compose box doubles as input: plain text is a `say`; a leading `/` is a slash command. A submit-time status re-check enforces the lifecycle rule — say/interrupt only in RUNNING/VALIDATING, approve/reject only in AWAITING_APPROVAL; outside the set it shows an inline notice and leaves the store untouched (`_session_screen.py:923`).

### History screen

`flywheel._history_screen.HistoryScreen` (`_history_screen.py:86`), opened via `h` or `/history`. One row per task, newest-finished first — same grouping as `fw history` (columns: phase / task / status / finished / runs / tokens / cost). Enter drills into the run's session screen; `r` reloads. No polling timer — finished runs do not change.

### Quit-handoff prompt

`flywheel._quit_prompt.QuitPromptScreen` (`_quit_prompt.py:41`) is shown only when the console owns a supervised worker (`request_quit` short-circuits otherwise). A dismiss with no value is treated as cancel — the child is never silently killed.

| Key | Action |
|---|---|
| `enter` | detach (worker keeps running, console exits) — the default |
| `s` | stop gracefully (SIGTERM; in-flight run finalizes to interrupted) |
| `escape` | cancel quit |

### Key bindings

| Screen | Key | Action |
|---|---|---|
| Dashboard | `up` / `down` | move row selection |
| Dashboard | `enter` | open the selected run's session view |
| Dashboard | `h` | open the finished-run history view |
| Dashboard | `ctrl+i` | focus the input bar (filter / slash commands) |
| Dashboard | `?` | toggle the help footer |
| Dashboard | `q` / `ctrl+c` | quit (routes through the quit prompt) |
| Session | `escape` | back to dashboard |
| Session | `end` | resume tail-follow |
| Session | `home` / `pageup` / `up` | pause follow and scroll |
| Session | `pagedown` / `down` | scroll |
| Session | `ctrl+x` | interrupt the run |
| Session | `ctrl+y` | approve the parked gate |
| Session | `ctrl+r` | reject the parked gate (compose value is the feedback) |
| History | `escape` | back |
| History | `enter` | drill into the run's session |
| History | `r` | reload |

### Worker and autopilot supervisors

`WorkerSupervisor` (`_worker_supervisor.py:191`) spawns `python -m flywheel_worktree.worker --db <path> [--tasks-dir ..] [--model ..]` with `start_new_session=True`, so a console Ctrl-C never reaches the child; only `stop()` (SIGTERM + 10s wait) signals it. It reads `task_claims` to detect liveness: a live lease this console did not spawn shows as DETACHED. Spawn logs land under `.flywheel/logs/worker/`.

`AutopilotSupervisor` (`_autopilot_supervisor.py:89`) spawns `python -m flywheel_orchestrator._autopilot_run [--tasks-dir ..] [--model ..]` without `--once` (the neverending daemon). It writes no lease, so it has no DETACHED state. Logs land under `.flywheel/logs/autopilot/`. Same spawn/detach/stop shape, independent of the worker.

## Slash commands

`flywheel._slash` is the shared command vocabulary for the dashboard and session screens — an alternate input surface, not a new channel: each slash verb reuses the exact store-mediated semantics of its CLI or key twin.

| Command | Action | Notes |
|---|---|---|
| `/help` | List the slash commands | |
| `/status` | Selected/viewed run's status (or the summary aggregate on the dashboard with no selection) | |
| `/approve` | Approve the parked manual gate | AWAITING_APPROVAL only |
| `/reject [feedback]` | Reject the parked manual gate | AWAITING_APPROVAL only |
| `/interrupt` | Interrupt the run | RUNNING / VALIDATING only |
| `/archive` | Archive completed phases | directory sources only; degrades to a notice otherwise |
| `/history` | Open the finished-run listing | dashboard only |
| `/worker start\|stop` | Spawn / gracefully stop the supervised worker | `start` no-ops to DETACHED when a live lease exists; `stop` only signals a child this console owns |
| `/autopilot start\|stop` | Spawn / stop the supervised autopilot daemon | spawns the neverending daemon (no `--once`); idempotent start |
| `/quit` | Exit the console | routes through the quit prompt |

An unknown verb leaves the typed line populated for editing rather than erroring (`_slash.py:112`).

## `init`

Scaffolds `.flywheel/` and a `flywheel.toml` work policy (`_cmd_init`, `_workflow.py:2991`). When a Dockerfile or Containerfile sits at the repo root, it also appends `.flywheel/` to `.dockerignore` (`_ensure_dockerignore_covers_flywheel`, `_workflow.py:2028`) — docker build contexts do not read `.gitignore`. Idempotent: existing files are left untouched and reported.

**Git preflight is a hard gate.** Before any file is written, `init` refuses (exit 2, nothing scaffolded) when the cwd is not a git repo or HEAD is detached (`_workflow.py:2876`). The state `init` produces must be one the worker's own preconditions accept.

| Flag | Effect |
|---|---|
| `--store {sqlite,postgres}` | pre-answer `[store] backend` (default sqlite) |
| `--pg-schema NAME` | `[store] schema` (postgres only; identifier-validated) |
| `--provision` | after the postgres preflight passes, create the schema and run bootstrap DDL now (postgres + resolvable DSN only) |
| `--allow-unverified` | scaffold even when the postgres preflight reports a blocking issue (downgrades blocks to warnings) |
| `--source {directory,github}` | pre-answer `[source] kind` (default directory) |
| `--repo OWNER/NAME` | github repo (default parsed from the origin remote) |
| `--label NAME` | github issue label (default `flywheel`) |
| `--skills` / `--no-skills` | install / skip the Claude Code skills. Interactive default yes; non-interactive default no |
| `--defaults` | accept every default without prompting (a non-TTY stdin implies this) |

Answers are collected fully before any write, so Ctrl-C/EOF mid-prompt leaves no partial `flywheel.toml`. The rendered policy carries `[source]`, `[store]`, `[paths]`, an auto-detected `[submit] base`, and commented placeholders — **no credential ever appears** (the postgres DSN lives only in env). After writing, `init` reports whether `ANTHROPIC_API_KEY` is set or `~/.claude/.credentials.json` exists (non-blocking).

For the resulting `flywheel.toml` schema see [configuration.md](configuration.md); for the installed `fw-*` skills and the spec/plan/verify/execute pipeline they drive see [workflow.md](workflow.md).

## The `--json` snapshot

`fw --json` (or any non-TTY stdout) prints one `DashboardSnapshot` — the same shape the Textual dashboard renders (`flywheel._snapshot`). The key set is stable; scripts piping `fw --json` depend on it.

```json
{
  "summary": {
    "active_workers": 0,
    "task_counts": {},
    "tokens_total": 0,
    "cost_usd_total": 0.0,
    "runtime_seconds": 0.0
  },
  "rows": [
    {
      "run_id": "...",
      "task_id": "...",
      "status": "running",
      "attempt": 1,
      "iteration": 0,
      "age_seconds": 0.0,
      "idle_seconds": 0.0,
      "tokens": 0,
      "cost_usd": 0.0,
      "turns": 0,
      "iterations_completed": 0,
      "last_kind": "...",
      "last_detail": "...",
      "awaiting_instruction": null
    }
  ]
}
```

A work source that raises degrades the summary's task counts to empty rather than crashing the frame (`_snapshot.py:97`).
