# Feature: Unified Product CLI

## Summary
Collapse the four console scripts (`flywheel`, `flywheel-orchestrate`, `flywheel-worktree`, `flywheel-tui`) into one Claude Code-inspired product command: bare `flywheel` (alias `fw`) opens the operator console and ensures the engine is running; everything scriptable lives under `flywheel <subcommand>`. The core package's distribution renames to `flywheel-core` (import `flywheel` unchanged); a new top-of-stack `flywheel` product package absorbs the TUI and owns the command surface.

## Background
Running a phase today requires knowing which of three binaries does what — the empty-sandbox `orchestrate` footgun (graders collecting 0 tests in a bare directory) was hit in practice on 2026-06-10. The TUI (spec 00020) shipped as a fourth binary and is the natural seed of a product shell. Pre-publication is the last cheap moment for a wholesale rename; precedent: the `.workflow/` to `.flywheel/` cutover.

## Scope

### In Scope
- Rename core dist `flywheel` to `flywheel-core`; directory moves to `packages/flywheel-core`; import name `flywheel` is unchanged everywhere.
- New product package at `packages/flywheel` (dist `flywheel`, module `flywheel_cli`, console scripts `flywheel` and `fw`), absorbing the `flywheel-tui` package's code; depends on `flywheel-orchestrator` and `flywheel-worktree`.
- Bare `flywheel` in a TTY opens the console; `--json` or non-TTY stdout prints one snapshot and exits 0 (behavior carried over from `flywheel-tui`).
- Engine supervision: console launch detects a live worker via claim-lease heartbeats; if none, spawns and supervises a `flywheel-worktree`-daemon subprocess; quit prompts detach-or-stop (default detach); `--no-worker` skips spawning.
- Subcommand router: `init | worker [--once] | status [--json] | live [--watch] | say | interrupt | approve | reject | archive | recover | recheck-blocked | audit` — each delegating to the existing implementation, not reimplementing it.
- Full shell input in v1: persistent input bar on both screens; `/help /status /approve /reject [feedback] /interrupt /archive /worker start|stop /quit` act on the selected (dashboard) or viewed (session) run; plain text filters rows on the dashboard and is `say` on the session screen.
- Delete the `flywheel-orchestrate`, `flywheel-worktree`, and `flywheel-tui` console scripts and the `packages/flywheel-tui` package; `flywheel-worktree` remains as a library (daemon loop + `GitWorktreeSubmitter`) without a script.
- Workspace housekeeping: root `pyproject.toml` sources/dev-group/testpaths updated for the renamed and new packages.

### Out of Scope
- Publishing to PyPI and the publishable-dist-name question (`fw`, `flywheel`, `flywheel-cli` are all taken; `flywheel-cli` is flywheel.io's active product, which also ships an `fw` binary). Parked until publishing is real.
- Exposing `run` (single-task), `next`, or bare `orchestrate` as product subcommands — they stay `python -m` plumbing (`flywheel.workflow`, `flywheel_orchestrator`). `flywheel worker --once` is the only blessed headless drain.
- Typing a goal on the dashboard to create a task in the work source (future door, explicitly v2).
- Deprecation shims or transitional aliases for the deleted binaries (pre-1.0 wholesale cutover).
- Managing workers the console did not spawn: `/worker stop` applies to the supervised child only; detached/external workers are stopped via OS signals as today.
- Any daemon-manager machinery (pidfiles, systemd units, restart policies) beyond plain parent-child subprocess supervision.
- Any new `Status`/`Outcome`, schema, grader, store-protocol, or control-command additions.

## Requirements

### Functional Requirements
1. **FR-1**: `flywheel` (and `fw`, identically) with a TTY opens the console on the dashboard screen; `flywheel --json` or piped stdout prints one JSON snapshot (same shape as today's `flywheel-tui --json`) and exits 0 with no escape sequences.
   - Acceptance: both invocations against a store with active runs; TUI renders, JSON parses with the same fields.
2. **FR-2**: On console launch (without `--no-worker`), a live worker is detected via claim-lease heartbeats; when none is live, a worker subprocess (the `flywheel-worktree` daemon loop) is spawned and supervised, logging to `.flywheel/logs/worker/`; the status bar always shows worker state (supervised / detached / none).
   - Acceptance: launch with no worker -> tasks start executing without a second terminal; launch while a detached worker holds a live lease -> no spawn, state shows detached.
3. **FR-3**: Quitting with a supervised worker prompts once: Enter detaches (worker keeps running, console exits), `s` stops it gracefully (SIGTERM; in-flight lifecycles finalize to `interrupted` per existing semantics). Quit never prompts when the worker is detached, external, or already stopped.
   - Acceptance: pilot test drives both prompt branches; after detach the worker process survives console exit; after stop no worker process remains and the in-flight run is `interrupted`.
4. **FR-4**: `flywheel <verb>` for `init`, `worker [--once]`, `status [--json]`, `live [--watch N]`, `say`, `interrupt`, `approve`, `reject [--feedback]`, `archive`, `recover`, `recheck-blocked`, `audit` produces output and exit codes identical to the pre-cutover implementations they delegate to.
   - Acceptance: existing CLI tests retargeted at the new entry point pass unchanged in substance; `flywheel status --json` round-trips.
5. **FR-5**: Both screens carry a persistent input bar: `/`-prefixed commands from the v1 set execute against the selected/viewed run with the same store-mediated semantics as their subcommand twins; unknown `/command` shows an inline error; `/help` lists the set. Plain text filters dashboard rows; on the session screen it remains `say` (shipped behavior).
   - Acceptance: pilot tests cover one happy slash command per screen, the filter, and the unknown-command error.
6. **FR-6**: The repo installs exactly two console scripts, `flywheel` and `fw`; `flywheel-orchestrate`, `flywheel-worktree`, and `flywheel-tui` no longer exist; `packages/flywheel-tui` is deleted; `import flywheel` still resolves to core everywhere.
   - Acceptance: `uv sync` then `command -v` checks; grep shows no references to the dead script names outside archived specs/tasks; full suite green including purity tests.
7. **FR-7**: Store resolution for every surface is unchanged: explicit `--db`, else policy `[paths] db`, else `.flywheel/flywheel.sqlite`, via the public seams from spec 00020.
   - Acceptance: subcommands and console honor a non-default `[paths] db` in `flywheel.toml`.

### Non-Functional Requirements
- **Performance**: console polling and tailing unchanged from spec 00020 (~1s cadence, cursor-incremental); supervision adds no store traffic beyond the existing lease reads.
- **Security**: unchanged — task files remain executable code; the spawned worker inherits the console's environment exactly as a manually started one would.
- **UX**: keyboard-only; no emojis; the input bar must not steal keys from existing bindings (arrows, Enter, Escape, q, ?) when unfocused.

## Behavior Specification

### Happy Path
1. Operator runs `flywheel` in an initialized project; no worker is live.
2. Console opens; a supervised worker spawns; status bar shows `worker: supervised`.
3. Tasks dropped in `.flywheel/tasks/active/` get picked up; dashboard rows appear.
4. Operator types `/approve` with an AWAITING_APPROVAL row selected; the gate resolves via the existing control channel.
5. Operator quits; prompt offers detach (Enter) or stop (`s`); Enter leaves the worker draining the phase.

### Error Handling
| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| No store / uninitialized project | Console and store-needing subcommands exit with a message naming the resolved path and the `flywheel init` remedy |
| Worker spawn fails (bad env, missing dep) | Console still opens; status bar shows the spawn error; `/worker start` retries |
| Supervised worker dies mid-session (crash/OOM) | Status bar flips to `worker: dead` with a notice; `/worker start` respawns; next worker start runs the existing startup recovery sweep |
| Live lease exists but heartbeat is stale | Treated as no live worker (lease-lapse semantics already defined); spawn proceeds |
| Second console launched while first supervises | Second sees the live lease, does not spawn, shows `worker: detached` (it does not own the process) |
| Unknown slash command | Inline error naming `/help`; input preserved for editing |
| `--json` combined with TTY-only flags | Snapshot printed, exit 0, conflicting flags rejected with a clear message |

### Edge Cases
| Case | Expected Behavior |
| ---- | ----------------- |
| `fw` vs `flywheel` | Byte-identical behavior; one implementation, two script entries |
| Quit via SIGINT/SIGTERM instead of `/quit` | Same detach-by-default path; never silently kills the supervised worker without the prompt or an explicit flag |
| `flywheel worker` run manually while a console supervises | Leases keep them off the same task (existing multi-worker semantics); console state stays `supervised` for its own child |
| `--no-worker` with no live worker | Console opens with `worker: none`; dashboard shows queued tasks not starting; status bar hints `/worker start` |
| Slash command targeting a run that left the active set | Same not-steerable inline notice as spec 00020 steering |
| Filter text matching zero rows | Empty-state line with the filter shown; clearing restores rows |
| Core rename vs purity tests | `test_task_module_purity` / `test_lifecycle_module_purity` and all `import flywheel` sites pass unmodified (import name never changes) |

## Technical Context

### Affected Apps
- `packages/flywheel-core` (renamed dir + dist; was `packages/flywheel`): loses its console script; `workflow.py` verbs remain importable and runnable via `python -m flywheel.workflow`.
- `packages/flywheel` (new product; absorbs `packages/flywheel-tui`): module `flywheel_cli` — command router, console screens, input bar, worker supervision, snapshot mode.
- `packages/flywheel-orchestrator`: loses console script; keeps `python -m` plumbing; otherwise untouched (public seams from 00020 already exist).
- `packages/flywheel-worktree`: loses console script; daemon loop gets an importable entry the product calls (function, not shell-out) plus subprocess spawn support for supervision.
- Root `pyproject.toml`: workspace sources, dev group, pytest testpaths updated.

### Integration Points
- Worker liveness: `task_claims` lease reads (existing orchestrator store) — no new store surface.
- Steering and gates: existing `enqueue_command` verbs and `harness.control_command_applied/_failed` feedback (specs 00013/00020).
- Store resolution: public `load_effective_policy` / `resolve_db_path` seams (spec 00020, commit e88a867).
- Subprocess supervision: stdlib only; worker logs to the existing `.flywheel/logs/worker/` layout.

### Relevant Existing Code
- `packages/flywheel-tui/src/flywheel_tui/` — the console code the product package absorbs (`_cli.py`, `_dashboard.py`, `_session*.py`, `_snapshot.py`).
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py` — daemon loop + graceful-shutdown semantics the supervisor reuses.
- `packages/flywheel/src/flywheel/workflow.py` — core CLI verbs migrating up (say/interrupt/approve/reject/audit/run).
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py` — subcommand implementations the router delegates to (status/live/archive/recover/recheck-blocked/init).
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_claims.py` — lease/heartbeat reads for liveness detection.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Naming | Dist `flywheel-core` for core (import `flywheel` unchanged); dist `flywheel` for the product (module `flywheel_cli`) | Dist and import names are independent; zero import churn; purity tests untouched |
| Layout | Directories match dists: core moves to `packages/flywheel-core`, product lives at `packages/flywheel` | Directory tells you the dist; one-time `git mv` |
| TUI package | Absorbed into the product package | The console IS the product shell; avoids a thin-wrapper package |
| flywheel-worktree | Kept as a library, console script deleted | Remains the worked example of a consumer; product exposes it as `flywheel worker` |
| Engine on launch | Bare `flywheel` detects via lease heartbeats and spawns/supervises a worker when none is live | Kills the two-terminal dance; store-mediated control means supervised == detached |
| Quit behavior | Prompt, default detach; `s` stops gracefully | Never silently kills a paid agent run; explicit keypress to stop |
| Verb surface | Lean: no `run`/`next`/`orchestrate` subcommands; `worker --once` is the only headless drain | Removes the empty-sandbox footgun from the blessed surface |
| Shell input | Full input bar on both screens in v1 (slash commands + filter/say) | Without it v1 is just a rename; this is the Claude Code feel that motivated the refactor |
| Cutover | No shims; old scripts and `flywheel-tui` deleted outright | Pre-1.0, nothing external depends on the names; precedent: `.workflow/` cutover |
| PyPI name | Parked; `flywheel-core` free, `flywheel`/`fw`/`flywheel-cli` taken (flywheel-cli is flywheel.io, which ships an `fw` binary) | Not publishing now; workspace-internal dists collide with nothing |
| Sequencing | Phased on a quiescent store: (a) scaffold product package + router delegating to existing code, (b) absorb TUI + shell input, (c) worker supervision + quit semantics, (d) cutover -- core dir/dist rename, script deletions, `flywheel-tui` removal, workspace registration | The rename touches the package the loop runs on; it lands last and alone |
| Loop-path coverage | Not required | Renames, a router, and subprocess supervision: no new status, schema, grader, protocol method, or control verb -- no Trigger Set signal |

## Open Questions
None.

## Next Steps
Run `/task 00021-FEATURE-unified-cli` to generate implementation tasks from this spec.
