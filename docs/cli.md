# CLI and operator console

The `flywheel` / `fw` surface is the top-of-stack product shell: the operator's single entry point for driving the loop day to day. It is a thin verb router over the layers below it (core, orchestrator, worktree) plus a terminal (ratatui) operator console and a `--json` snapshot mode. This doc is the canonical reference for every verb, flag, key binding, and slash command.

## At a glance

| Surface | Invocation | Use |
|---|---|---|
| Operator console (TUI) | bare `fw` on a TTY | Watch active runs, drill into a session, steer, supervise the worker/autopilot |
| JSON snapshot | `fw --json` (or any non-TTY stdout) | One machine-readable frame for scripts |
| Read verbs | `fw status` / `live` / `history` / `show` / `validate` | Inspect state, runs, and task definitions |
| Steering verbs | `fw say` / `interrupt` / `approve` / `reject` | Enqueue one control command against a live run |
| Landing | `fw reland TASK_ID [--db PATH]` | Retry a parked task's landing on its branch's current head (see [`reland TASK_ID`](#read-verbs)) |
| Daemons | `fw worker` / `autopilot` / `triage` | Drain the queue (worker), keep it full (autopilot), or triage the intake board (triage) |
| Audit | `fw audit RUN_ID` / `fw audit --project [--json]` / `fw purge [--project]` | Stream one run's telemetry; list the project's audit rows (transcripts, autopilot decisions); delete transcript bodies (rows stay) |
| Setup | `fw init` | Set up this repo's project; writes nothing into the repo (see [`init`](#init)) |
| Hub | `fw hub` server: `start` / `stop` / `upgrade` / `status` / `logs` / `serve` / `install-service` / `doctor`; access: `open` / `url` / `token` / `tls` / `peer` / `machine` / `share`; sessions: `ls` / `new` / `send` / `read` / `wait` / `attach` / `kill` / `relaunch` / `explain` / `import` / `template` / `queue` / `broadcast` / `conversations` / `account`; output: `commands` / `search` / `activity` / `digest` / `record`; git: `review` / `merge` / `pr` / `handoff` / `ask-review` / `fanout` / `autopilot`; settings: `config` / `plugin` / `perms`; hooks only: `report` | Web workspace of server-owned agent terminals (see [hub.md](hub.md)) |
| One-shot | `fw exec [PROMPT]` | Run one agent call from any project: no tasks, store, worktree, or config |
| Projects | `fw project ls [--json]` / `prune` / `relink [PATH]` / `path [FLAG]` | The home's project registry and the cwd project's dirs (see [Projects and the home](#projects-and-the-home)) |
| Specs | `fw spec ls [--json]` / `export DIR [--force]` | The cwd project's central specs; copy them out (see [Specs](#specs)) |
| Config | `fw config [--json]` / `fw trust [--revoke]` | The effective config with sources; approve the repo's `.flywheel.toml` (see [Config and trust](#config-and-trust)) |
| Store | `fw db upgrade [--db PATH]` | Migrate the store in place to this build's schema (see [The store](#the-store)) |
| Self-update | `fw self-update [--dry-run] [--json]` | Build a clean, landed flywheel checkout and install the new `fw` / `flywheel` in place (see [`self-update`](#self-update)) |

`flywheel` and `fw` are byte-identical binaries on one implementation (`crates/flywheel/src/cli.rs`, two `[[bin]]` targets in `crates/flywheel/Cargo.toml`). Use either; this doc writes `fw` for brevity.

## Entry points and the verb router

The router (`route`, `crates/flywheel/src/cli.rs`) dispatches by the first argv token:

- No args open the console on a TTY, or print one `--json` snapshot when stdout is not a TTY.
- `-h` / `--help` print the verb list and exit 0.
- `-V` / `--version` print `fw <version>` and exit 0.
- A leading-`-` first token (`fw --json`, `fw --db ...`) is treated as a console flag and handed to the TUI.
- Otherwise the first token is the verb and the remaining argv is forwarded verbatim to the delegated implementation.

`fw <verb> --help` forwards `--help` to that verb's own parser, so each verb's specific flags come from the delegated parser, not from the router.

**Plumbing verbs are hidden.** `next`, bare `orchestrate`, and the single-task core verbs stay plumbing: they route (`flywheel next`, `flywheel orchestrate`, `flywheel workflow run|is-done|set-model|...`) but never appear in `fw --help`. `worker --once` is the only blessed headless single-task drain. An unknown verb prints `fw: unknown command: <verb>` to stderr and exits 2.

### Resolution precedence

Every delegated verb resolves its inputs the same way: **explicit flag wins, then the effective config ([Config and trust](#config-and-trust)), then the home project's built-in default.** Every default lives under `$FLYWHEEL_HOME/projects/<id>/` (see [Projects and the home](#projects-and-the-home)); a directory outside a git repo is refused unless explicit flags name every path the verb reads. A repo's legacy `flywheel.toml` / `.flywheel/` is never read: one stderr notice says it is ignored and can be deleted. There is no `--policy` flag.

| Input | Explicit flag | Built-in default | Anchor |
|---|---|---|---|
| Policy | (none) | the home project + effective config (re-anchors cwd at the worktree top level) | `load_effective_policy` |
| DB path | `--db PATH` | `$FLYWHEEL_HOME/projects/<id>/flywheel.sqlite` (or the `store` DSN) | `resolve_db_path` |
| Work source | `--tasks-dir DIR` (forces directory source) | `$FLYWHEEL_HOME/projects/<id>/tasks` | `resolve_work_source` |
| Worktree root | `--sandbox-root` (worker) | `$FLYWHEEL_HOME/projects/<id>/worktrees` | `resolve_sandbox_root` |

Run transcripts (`transcripts/<run_id>.jsonl`), logs (`logs/worker/`, `logs/autopilot/`), the merge lock (`.merge.lock`), and the held-out registry (`held-out/`) sit in the project dir (an explicit `--db` outside a project dir keeps run files at `<db dir>/logs/runs/`). Work-source, sandbox, and agent flags: [configuration.md](configuration.md#verb-flags).

Anchors are in `crates/flywheel-orchestrator/src/workflow/mod.rs`.

The Postgres DSN comes from `store` in your user/personal `config.toml` or `FLYWHEEL_PG_DSN`, never a committed file. See [configuration.md](configuration.md).

## Config and trust

| Verb | What it does |
|---|---|
| `config [--json]` | The effective config of the cwd project: `model`, `concurrency`, `store`, `transcript_retention_days`, `verify`, `protected_paths`, `land`, `project_id`, each with its source (`default`/`user`/`repo`/`personal`/`env`/`flag`). `--json`: `{key: {"value", "source"}}`, DSN password as `***`. `--model` / `--concurrency` preview a flag. Exit 2 on an invalid config file (the error names the key). |
| `trust` | Approve the cwd repo's `.flywheel.toml` as it is now (printed first when stdin is a terminal), keyed by the local project id and the file's sha256 in `$FLYWHEEL_HOME/trust.json`. Exit 1 when there is no file. |
| `trust --revoke` | Remove every approval of the cwd project. |

An unapproved repo file: `worker`, `autopilot`, `archive`, and the console refuse unattended (stdin not a TTY) with `cd <repo> && fw trust`, or prompt once `[y/N]` on a TTY; read-only verbs warn once and ignore it. Editing the file revokes approval.

## Projects and the home

Every byte flywheel writes lives under `$FLYWHEEL_HOME` (default `~/.flywheel`): `config.toml`, `trust.json`, `hub/` (hub state), and `projects/<id>/` (`meta.json`, `config.toml`, `flywheel.sqlite`, `specs/`, `tasks/`, `transcripts/`, `worktrees/`, `held-out/`, `logs/`). A pre-home hub state dir (`$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub`) is not read: the first `fw hub start`/`serve` prints the `mv` that moves it.

- A project is a git repo. Its id (UUID v4) is stored in the repo's common `.git/config` as `flywheel.projectid`, so every linked worktree maps to one project. The first store verb in a repo mints it; nothing else is written into the repo.
- `meta.json` records `id`, `path` (canonical main worktree), `git_common_dir`, `remote` (origin URL or null), `first_seen`, `last_seen`.
- A moved repo keeps its id (`meta.json` follows). A copy (the recorded repo still exists and carries the same id) mints a new id for the copy; the original is untouched.
- Outside a git repo, store verbs exit non-zero with an error naming git.

| Verb | What it does |
|---|---|
| `project ls [--json]` | Every project: `<id>  <path>` lines; `--json` prints `[{"id","path"}]`. |
| `project prune` | Remove project dirs whose recorded repo no longer exists or no longer carries that id; prints `removed <id> (<path>): <reason>` per dir. |
| `project relink [PATH]` | Point the cwd repo's id at an existing project dir (PATH: the dir or its id; bare: the single project recording this repo's path). Recovery after a re-clone or a lost key. |
| `project path [FLAG]` | Print the absolute dir of the cwd project (bound to a trusted `project_id`), creating it: `--root` (default), `--specs`, `--tasks`, `--logs`, `--worktrees`, `--held-out`. Skills and scripts use this instead of hard-coding paths: `ls "$(fw project path --specs)"`. Exit 2 on an unknown flag, 1 outside a git repo. |

### Specs

Specs live centrally in `$FLYWHEEL_HOME/projects/<id>/specs/` (with `archive/`), never in the repo; the `fw-*` skills read and write them through `fw project path --specs`.

| Verb | What it does |
|---|---|
| `spec ls [--json]` | Every spec file relative to the specs dir, sorted; `--json` prints `[{"path","archived"}]`. |
| `spec export DIR [--force]` | Copy the specs tree (including `archive/`) into DIR byte-for-byte, creating DIR (relative DIR: against the working dir). Prints `wrote <path>` per file and a summary; identical files are left alone. A differing existing file aborts before any write (exit 1) unless `--force`; DIR inside the specs dir is refused. |

## The store

Every table carries a `project_id` partition column, so one store (one Postgres DSN) holds many projects; each verb reads and writes only its own project's rows. The partition is the bound project id (the config `project_id` override, else the `.git/config` id). Invocations that skip project resolution (`--db` with `--tasks-dir`) use the fixed partition `default`.

**A store whose schema version is behind this build is refused, never migrated implicitly**: every store verb exits non-zero with an error naming `fw db upgrade`. A store written by a newer flywheel is refused outright. A brand-new store is created at the current version.

| Verb | What it does |
|---|---|
| `db upgrade` | Migrate the cwd project's store (sqlite under the project dir, or the configured Postgres DSN) in place: run tables and orchestrator tables, each in one transaction, from any supported older version; existing rows get the project id. Idempotent (a second run, or a store already partitioned whose version row is behind, only fixes the version). Exit 1 on a newer or unrecognized store. |
| `db upgrade --db PATH` | Upgrade that sqlite file (partition `default`). |

Schema versions: run store `schema_version` 15 (v15 added `attempts.stop_reason_json`), orchestrator store `orchestrator_schema_version` 10. Catalog: [persistence-tables.md](persistence-tables.md).

## `self-update`

`fw self-update [--from PATH] [--install-dir DIR] [--allow-unlanded] [--force] [--drain-timeout SECS] [--upgrade-postgres] [--dry-run] [--json]` builds a flywheel checkout with `cargo build --release --locked -p flywheel` and replaces `fw` and `flywheel` in place by atomic rename. It never runs `scripts/check.sh`: a landed commit is already verified. Component upgrades stay separate verbs: `fw db upgrade` (one store) and `fw hub upgrade` (hub handoff).

**Bootstrap:** a flywheel without this verb moves to one that has it by one manual `cargo install --path crates/flywheel`; every later move uses `fw self-update`.

| Flag | Meaning |
|---|---|
| `--from PATH` | The checkout to build. Default: the cwd repo when it is a flywheel checkout, else the checkout the last successful self-update recorded. |
| `--install-dir DIR` | Where both binaries are replaced. Default: the running executable's directory. Refused when it lacks `fw` or `flywheel`, is inside a cargo `target/` dir, or is not writable. |
| `--allow-unlanded` | Build a HEAD not reachable from the default branch. |
| `--force` | Update even when already current. |
| `--drain-timeout SECS` | Wait per drained process after SIGTERM (default 120). |
| `--upgrade-postgres` | Allow upgrading a Postgres store that is behind (no backup). |
| `--dry-run` | Build, then report every later step `planned`; writes nothing but the lock. |
| `--json` | JSON Lines on stdout (below). |

Refusals (exit 3, before `cargo` runs, nothing changed but the lock file): not a git work tree whose `crates/flywheel/Cargo.toml` declares package `flywheel` with bins `fw` and `flywheel`; a modified tracked or untracked non-ignored file; HEAD not reachable from the default branch (`refs/remotes/origin/HEAD`'s target, else `main`; local or `origin/` ref); another run holding `$FLYWHEEL_HOME/self-update/lock` (names its pid); a refused install dir. A failed build, or an artifact (`${CARGO_TARGET_DIR:-<checkout>/target}/release/{fw,flywheel}`) whose `--version` does not exit 0 printing `fw <version>`, also exits 3 with the install dir untouched.

**Already current:** HEAD equals the recorded commit and the installed `fw` / `flywheel` bytes equal the recorded sha256 (never the version string): outcome `current`, no `cargo`. State lives in `$FLYWHEEL_HOME/self-update/`: `lock`, `previous/{fw,flywheel}` (the pair the last update replaced), `state.json` (commit, checkout, install dir, version, sha256 of both).

Exit codes: `0` updated, current, or planned; `1` a step failed after changes began (reverted); `2` usage error; `3` refused before any change; `4` binaries and stores updated, hub not handed off.

Human output is one line per step in the fixed order `preflight build drain backup install db-upgrade hub restart`, then `manual:` / `not-stopped:` / `unidentified-claim:` lines, then `self-update: <outcome>`:

```
self-update: preflight ok -- /src/flywheel at 3f2a9c1d0b7e, install dir /home/me/.cargo/bin
self-update: build ok -- fw 0.6.1 at 3f2a9c1d0b7e
self-update: drain skipped
...
self-update: install ok -- fw and flywheel 0.6.1 (3f2a9c1d0b7e) in /home/me/.cargo/bin
...
self-update: updated
```

Status is `ok`, `skipped`, `failed`, `reverted`, or `planned`; a step a refused run never reached is `skipped`. This build performs preflight, build, and install; drain, backup, db-upgrade, hub, and restart report `skipped` (`planned` under `--dry-run`).

`--json` prints one `{"step","status","detail"}` object per step, then exactly one `{"summary":{...}}` with keys `outcome` (`updated` / `current` / `planned` / `refused` / `failed` / `updated-hub-pending`), `source` (`{kind:"checkout", path, commit}`), `from` / `to` (`{version, commit|null, sha256}`), `drained`, `not_stopped`, `unidentified_claims`, `backups`, `upgraded`, `hub`, `restarted`, `manual`. Build output goes to stderr, so stdout stays JSON Lines.

## Read verbs

Delegated to the orchestrator CLI (`main`, `crates/flywheel-orchestrator/src/workflow/mod.rs`). On a load/policy/work-source error these print `error: ...` to stderr and exit 2.

| Verb | What it does | Key flags |
|---|---|---|
| `status` | Leads with one `session-pause: <label> until <pause_until> (run <run_id>)` line per active pause (label as `fw pauses`: `@<name>`, `cli login`, `all accounts`; expired or cleared pauses print nothing), then one line per active task: `phase/task_id state [-- error]`. INTERRUPTED rows show `blocked_on:`; AWAITING_APPROVAL rows show `awaiting_on: <instruction>`; a DONE run that never landed shows `stranded: <park_kind> -- <detail>` (the work finished but the strategy parked it -- uncommitted tree, divergent base, or a failed `verify`). A dependent held on a DONE prerequisite whose landing parked unresolved or is still in flight shows `waiting_on_landing: <id>[, <id>]` (prerequisite order). `--json` emits a per-task array, each entry carrying `stranded: {park_kind, detail}` when parked and `unlanded_prerequisites: [ids]` when held (each omitted otherwise). | `--json`, `--rollup`, `--tasks-dir`, `--db` |
| `status --rollup` | Phase-grouped, **evidence-derived** rollup: each task's status is computed from grader receipts (verified vs accepted vs blocked/failed/not-started), never self-reported. A dependent's unlanded prerequisites join its `unsatisfied_prerequisites` (`blocked_by_prereq`, detail `waiting on landing: <ids>`). | `--rollup`, `--json` |
| `live` | One line per in-flight run (RUNNING / VALIDATING / AWAITING_APPROVAL): status, `attempt=N iter=K`, `tokens=/cost=/turns=`, `age=` / `idle=` (marked `STALE` past 90s idle), last action. `--watch N` clears and refreshes every N seconds (Ctrl-C exits). | `--watch SECONDS`, `--db` (no `--tasks-dir`) |
| `history` | Finished runs (done / failed / failed_validation), one line per task, newest first; retries fold to one row `runs=N`. | `--status S` (repeatable), `--phase`, `--limit N`, `--json`, `--tasks-dir`, `--db` |
| `show ID` | One run in full: lifecycle, per-attempt outcome/iter/turns/tokens/cost, grader receipts (pass/FAIL), final agent output, related runs. `--json` gives each attempt a `checks` array: every check of the pinned task in task order (`passed`/`failed`/`judge_faulted`/`not_run`, stored exit/termination/duration, redacted output tails, rubric summary), then that attempt's held-out gate results (name, verdict, redacted excerpt only). `ID` accepts a run_id or a task id (latest run). | positional `run_or_task_id`, `--json`, `--tasks-dir`, `--db` |
| `validate` | Statically validate the active listing without running anything: per-task, each command grader must shell-parse (`bash -n`); pairwise, two tasks' derived file surfaces (context.relevant paths + command-grader run paths) must not overlap unless they share a `conflict_keys` entry, list the path in a top-level `overlap_ok` array, or are chained via `prerequisites`. Never checks whether a derived path exists on disk (an output path is still a surface). Exits non-zero naming each offending task (and, for an overlap, both task ids and every shared path), else `All N active task(s) valid.` Lints DONE-but-still-listed tasks too -- archiving, not status, is the operator's lever. Directory sources only. | `--tasks-dir` (no `--db`) |
| `archive` | Move active `<phase>` dirs whose tasks are all DONE into `archive/`, printing each moved dir. Directory sources only. A loop-path / phase-verify gate applies when `repo_root` is threaded (see [loop.md](loop.md)). | `--tasks-dir`, `--db` |
| `recover` | Finalize lifecycles stranded in running/validating to interrupted; prints transitioned run_ids, else `(no stranded lifecycles)`. Finalizes only under the task's claim: a run whose claim a live worker holds is in flight, not stranded, and is skipped with a stderr `skipped <run_id>: ...` note (a lapsed lease is recovered). | `--task-id`, `--db` |
| `resolve ID` | Deliberately abandon a strand: record an operator-attributed `stop-resolved` marker keyed to the task id, carrying `--reason` verbatim, so the next `archive` sweep archives the otherwise-landed phase. The landing re-driver leaves a run alone once an attributed marker is at least as recent as its latest park, so an abandoned strand is never re-landed. A landing strand parked on the task's latest DONE run after the marker is a fresh strand: the re-driver resumes it, `status` re-surfaces it and archival probes the task again until it is resolved anew. The only non-probe path to clearing a strand (never manual store SQL, never a task-file tombstone). Clears either an unresolved stop event or a ledger-only landing strand (a parked DONE run `status` shows as `stranded:`); refuses (exit 1) when the task has neither; empty `--reason` exits 2. Resolving a prerequisite whose landing parked releases its waiting dependents: one extra line `releases: <id>[, <id>]` names those no longer held on any unlanded prerequisite (no line when none). | positional `task_id`, required `--reason TEXT`, `--db` |
| `pauses` / `pauses clear` | List session-limit pauses newest first (`--json`: `[{id, account, run_id, reset_at, pause_until, recorded_at, active, cleared}]`; `account` is the name, `""` for the default login, `null` for a pre-account pause that holds every account; `cleared` is `null` or `{at, via, reason}`). `pauses clear (ID... | --account NAME | --default | --all) --reason TEXT` appends a `via: cli` clear record (reason verbatim) to each matching active pause and prints `cleared <id>` per pause; the row is never deleted. `--default` = account `""`; only `--all` also clears `null` pauses. Empty reason exits 2, no active match exits 1; both record nothing. Unrelated to `fw autopilot --pause`, which holds the intake daemon. | `--json`; clear: positional `ID`, `--account`, `--default`, `--all`, required `--reason TEXT`; `--db` |
| `reland TASK_ID` | One operator-initiated landing attempt for the task's latest DONE run whose landing parked, on the task branch's **current** head (commits added after the park included), through the configured submit strategy: the held-out gate grades the branch head rebased onto the current base, then submit rebases, re-runs the task's command graders, runs `verify` and protected paths, rewrites the harness-owned `Flywheel-Task` / `Flywheel-Run` trailers (an operator-written `Flywheel-*` value never survives), under the merge lock and a fresh task claim. A pruned worktree is rebuilt from the surviving task branch. Exit 0: landed, prints the landed ref; records `landing_redriven{landed, initiator operator}` and a `stop-resolved` attributed `reland`. Exit 1: attempted and parked (prints `park_kind` and detail; the stop stays) or refused before attempting (prints why; nothing recorded, nothing claimed): no DONE run, never parked, already landed, an attributed `stop-resolved` newer than its latest park, the claim held by another worker (named), or the task branch is gone. Exit 2: usage. Loads config like `worker` (refuses an untrusted repo file unattended). Never re-arms the landing re-driver's bound. | positional `task_id`, `--db` |
| `recheck-blocked` | Re-evaluate blocked (INTERRUPTED with persisted `requires`) lifecycles; when all predicates are satisfied, transition interrupted to ready. Default scans all. | `--run-id`, `--dry-run`, `--tasks-dir`, `--db` |

`status --rollup` is evidence-derived from grader receipts, not from any agent claim; see [work-sources.md](work-sources.md) and [orchestration.md](orchestration.md) for how items and phases map onto it. `recheck-blocked --dry-run` reports without transitioning (emits `harness.recheck_attempted`, never `harness.unblocked`).

## Steering verbs

Delegated to the core CLI (`crates/flywheel-core/src/workflow.rs`). Each enqueues exactly one row into the `control_commands` table keyed by `run_id` (`enqueue_control_command`): it validates the run exists, prints `enqueued #<id> kind=<k> run_id=<id>`, and warns when the run is not in-flight. `approve` / `reject` instead bind to the gate parked right now (payload `awaiting_ordinal` + `attempt_number`) and refuse with exit 1, enqueuing nothing, when the run has no parked gate or when `--gate ORDINAL` / `--attempt N` (the `awaiting_ordinal` / `attempt_number` of `harness.awaiting_approval`) name a different gate than the one parked; without a pin the decision binds to whatever gate is parked when the command runs, and stderr prints the bound gate. Script or repeat decisions with `--gate`. The console pins every approve/reject to the gate it displayed; the resolver applies a decision only to the exact park it was bound to, and records every other claimed row (stale or unbound decisions, duplicates, `say`/`set_model` sent while parked) as `harness.control_command_not_applicable` with a `reason`.

**Agent claims are untrusted; the harness owns every lifecycle transition.** These verbs are permitted only in the matching lifecycle state — the store records the command, the harness applies it. See [task-lifecycle.md](task-lifecycle.md).

| Verb | What it does | Allowed state | Key args |
|---|---|---|---|
| `say RUN_ID MSG` | Inject one operator message as a user turn into the live session. The task definition is not mutated. Empty message exits 2. | RUNNING / VALIDATING | positional `run_id`, `message`; `--db` |
| `interrupt RUN_ID` | Drive the live run to INTERRUPTED via in-band finalization; records `harness.control_command_applied`. | RUNNING / VALIDATING | positional `run_id`; `--db` |
| `approve RUN_ID` | Approve the manual gate parked now: writes a `passed=True` manual receipt, then re-parks on the next gate (which needs its own `approve`) or transitions to DONE. `--gate` / `--attempt` pin the gate you reviewed; a re-run after that gate resolved is refused (exit 1) instead of approving the next one. | AWAITING_APPROVAL | positional `run_id`; `--gate ORDINAL`; `--attempt N`; `--db` |
| `reject RUN_ID` | Reject a parked manual gate: writes a `passed=False` receipt, transitions to FAILED_VALIDATION. `--feedback` flows into the next attempt's reviewer-feedback section (absent renders `(no feedback provided)`). | AWAITING_APPROVAL | positional `run_id`; `--feedback TEXT`; `--gate ORDINAL`; `--attempt N`; `--db` |

The shell's `say` is a surface rename of core's `steer` verb; the underlying core CLI is unchanged. Core's `set-model` verb exists but is not a product verb; it is reachable only as the hidden `flywheel workflow set-model`.

## `exec`

One agent call through the `flywheel-agents` runtime under a local host (`crates/flywheel/src/exec.rs`). **No config is loaded and nothing is written under `$FLYWHEEL_HOME`**: no task, store, or worktree. stdout carries only the result; diagnostics go to stderr.

```bash
fw exec --json --max-turns 1 --cwd /tmp/work \
  --system-prompt "Answer in one word." - -- --tools "" < prompt.txt
```

| Flag | Controls |
|---|---|
| positional `PROMPT` | the prompt; `-` or omitted reads stdin; empty exits 2 |
| `--agent ID` | adapter id (default `claude-code`; `codex`) |
| `--model M`, `--max-turns N` | model and turn ceiling |
| `--timeout SECS` | wall-clock ceiling; expiry kills the agent's process group |
| `--cwd DIR` | agent working directory (default the current dir; must exist) |
| `--system-prompt TEXT` | the adapter's `system_prompt` option |
| `--option KEY=VALUE` | one adapter option (repeatable); `VALUE` parsed as JSON, else a string |
| `--command ARG` | replace the agent executable (repeat for an argv prefix) |
| `--json` | print the `CompletedRun` (`final_text`, `stop`, `total_cost_usd`, `failure`, `fault`, ...) as one JSON line; default prints `final_text` |
| `-- ARGS...` | appended to the agent CLI argv (the `extra_cli_args` option) |

Setting `system_prompt` or `extra_cli_args` both by flag and by `--option` is a usage error. Tool permission prompts are denied (supervised policy); widen tools with agent flags after `--`.

Exit codes: **0** clean stop (terminal envelope seen, no `failure`/`fault`, `stop.is_error` false); **1** run failed, result still printed: `failure.error_type` is `timeout`, `cancelled` (SIGINT/SIGTERM, process group killed), `no_result` (agent exited without its result envelope), `agent_exit`, or the start-up error code (e.g. the agent CLI is missing); **2** usage error (unknown agent, bad flag/number/`--option`, missing `--cwd`, empty prompt), stdout empty.

## Daemons and audit

### `worker [--once] [--task ID ...] [--with-prereqs]`

The blessed headless drain, delegated in-process to the worktree daemon (`main`, `crates/flywheel-worktree/src/daemon.rs`). It drives the project's tasks (`projects/<id>/tasks/active/<phase>/`), each in its own git worktree under `projects/<id>/worktrees`, landing on done (`land`) and parking on failure. `--once` runs a single drain cycle; without it the worker is a daemon.

| Flag | Controls |
|---|---|
| `--once` | single drain cycle, no daemon loop |
| `--tasks-dir`, `--db` | standard resolution inputs |
| `--sandbox-root` | worktree root (default `projects/<id>/worktrees`; relative paths anchor at the repo root) |
| `--source`, `--repo`, `--label`, `--require-triage-receipt` | GitHub work sources ([configuration.md](configuration.md#verb-flags)) |
| `--sandbox container --image IMAGE` | the container backend ([container-backend.md](container-backend.md)) |
| `--agent ID` | agent adapter (`claude-code` default, `codex`) |
| `--account NAME` | run every Claude agent under that account: `$FLYWHEEL_HOME/hub/claude-accounts/NAME.token` (`fw hub account add`), re-read at each agent spawn, as `CLAUDE_CODE_OAUTH_TOKEN` with `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` removed. Exit 2 before any claim on an invalid name, a missing token file, `--sandbox container`, or `--agent codex`; pool members inherit it |
| `--concurrency N` | pool size (overrides `concurrency`; default 1) |
| `--model` | agent model (overrides `model`) |
| `--max-turns`, `--max-retries` | per-run ceilings |
| `--worker-id` | identity stamped on claims |
| `--lease-seconds` | claim lease window (default 300) |
| `--reconcile-seconds` | steering-bridge cadence: re-list the work source and interrupt in-flight runs whose item vanished (default 15; 0 disables) |
| `--poll-interval` | idle wait between drain cycles |
| `--heartbeat` | progress line every N seconds (0 disables) |
| `--worktree-retention-days` | parked-worktree retention |
| `--task ID` | run only this task (repeatable); see **Scoped runs** |
| `--with-prereqs` | with `--task`: also run the named tasks' not-done prerequisites (transitively), first |

**Scoped runs** (`--task`). Selection is restricted to the named tasks (`TaskScope`, `crates/flywheel-orchestrator/src/orchestrate/scope.rs`); claims, the held-out gate, retries, and landing are unchanged, and a task another worker holds is waited for, never stolen. Nothing outside the scope is recovered, resumed, or re-driven. Refused before anything is claimed (exit 2): an id neither listed nor done in the store, a task it cannot run (capabilities, invalid graders, a graph cycle), without `--with-prereqs`, a named task whose prerequisites are not done (each listed with its state), or a task held behind a done prerequisite whose `flywheel/phase/<phase>` has not reached the base (merge it first). The worker exits once every named task settles instead of idling, printing one line to stdout, e.g. `scope: 1/2 verified -- a verified; b failed (grader unit failed)`; pulled-in prerequisites count too (`...; prerequisite p verified`). Settled means verified (done, landing not parked, or a park the archive probe resolved as landed), failed (its latest run failed after this invocation started; an older failure is re-run), parked (landing parked or held-out gate blocked, awaiting approval, blocked on `requires`, its sandbox failed to prepare 3 times in a row, its definition went invalid, or a phase hold), or blocked (a pulled-in prerequisite did not verify). While waiting it logs `scope: waiting -- <id> pending (<why>)` to stderr once per change (e.g. `claimed by <worker>`, `interrupted; ...`). Exit 0 only when all named tasks verified, else 1 (also when stopped by a signal first). `--concurrency` is capped to the scope's size; pool members share the scope.

**Shutdown.** SIGTERM, SIGINT, and SIGHUP (a closing terminal/PTY) are all an orderly stop: the in-flight run finalizes to `interrupted`, its claim is released, and every agent/grader process group is terminated (SIGTERM, SIGKILL after 5s). With `--concurrency N > 1` the invocation is a pool supervisor whose members run in sessions of their own; it forwards the stop as SIGTERM to each member (SIGKILL after 10s, logged as `pool member <id> did not exit within 10s of SIGTERM`). A signal while the worker is still starting (loading the policy, opening stores) is recorded from its first instruction and acted on before the first claim: an orderly stop, exit 0, nothing claimed. Only one landing while the binary itself is still being loaded (the first few ms after exec) takes the default action, before anything exists to leave behind. A supervisor that dies without a handler (SIGKILL, OOM) closes each member's lifeline pipe (the internal `--supervisor-fd`), and the member stops exactly as on SIGTERM. A member that dies without a handler (SIGKILL, OOM, past the 10s window) has whatever it left in its session (agent/grader groups) SIGKILLed by the supervisor when it reaps it (Linux), and its claim lapses (`--lease-seconds`). A SIGKILLed single worker (`--concurrency 1`) has nothing above it: its claim lapses and its agent group keeps running, so stop workers with SIGTERM. SIGHUP inherited as ignored (`nohup fw worker ...`) stays ignored: the worker outlives its terminal.

### `autopilot [--once [--dry-run]]`

Keeps the work queue full with verifiable, tier-prioritized tasks. Delegated to `main` in `crates/flywheel-orchestrator/src/autopilot_run.rs`. **Neverending daemon by default**; `--once` runs one refill pass and exits. The daemon loop never terminates on an idle cycle — it exits only on SIGTERM/SIGINT (`run_daemon_loop`).

| Flag | Controls |
|---|---|
| `--once` | one refill pass then exit |
| `--tasks-dir`, `--model` | standard inputs |
| `--dry-run` | with `--once`: measure the queue against the target and log the decision; no agent, nothing emitted |
| `--agent ID` | agent adapter for discovery and authoring |
| `--target-depth N` | fill the queue to depth N (default 5) |
| `--interval SECONDS` | daemon cycle interval (default 300.0) |
| `--pause` / `--resume` | write / remove `<activity file>.paused` and exit: a running daemon holds between cycles (phase `paused`, still live) until resumed (not a session-limit pause; see `fw pauses`) |

Emitted work lands per the `land` config key; scoring weights are built in. See [autopilot.md](autopilot.md) for the tier and scoring model.

### `triage --repo OWNER/NAME [--label L] [--once]`

Triages the GitHub intake board: authors a fail-first spec+grader for each candidate issue, then labels well-specified issues ready and under-specified ones needs-detail, each with a receipt. Delegated to `main` in `crates/flywheel-orchestrator/src/triage_run.rs`. **Neverending label-polling daemon by default**; `--once` runs one triage pass and exits 0. The daemon loop never terminates on an idle cycle — an empty board writes nothing and the next cycle is scheduled; it exits only on SIGTERM/SIGINT (`run_daemon_loop`).

A candidate whose triage errors (agent, grader executor, or `gh` write) is logged as `#N failed (...)` and counted in the pass summary without blocking the remaining candidates; an issue it left unwritten is retried next pass. A pass fails only when every attempted candidate failed (or the board read fails). A full 200-issue listing page logs a truncation warning.

Flags are validated **before** the repo root or any `gh` call: a missing or malformed `--repo` exits 2 having issued no GitHub write. Labels: intake `--label` (default `flywheel`), ready `flywheel:ready`, needs-detail `flywheel:needs-detail`; interval 300s; no per-pass cap.

| Flag | Controls |
|---|---|
| `--repo OWNER/NAME` | the intake board's repo (required) |
| `--label L` | intake label (default `flywheel`) |
| `--once` | one triage pass then exit |
| `--interval SECONDS` | daemon cycle interval (default 300.0) |
| `--model` | agent model for the authoring session (overrides `model`) |

### `audit --project [--json]`

The cwd project's audit record, oldest first (newest last), from whichever store the project uses (sqlite or Postgres). Rows are append-only and kept forever (`crates/flywheel/src/ledger_verbs.rs`).

| `kind` | Fields (besides `kind`, `at`) | Written |
|---|---|---|
| `transcript` | `run_id`, `uri` (`file://` + absolute path), `sha256` (hex of the body bytes), `bytes` | when a run finalizes (the runner returned) |
| `autopilot.cycle` | `cycle`, `mode` (`once`/`daemon`), `outcome` (`ok`/`error`), `reason`, `emitted`, `dropped`, `relevant_tiers`, `not_relevant_tiers`, `prefiltered`, `discovery_cache`, `emitted_tasks`, `dropped_findings`; on error `error`, `error_type` | every autopilot pass |
| `autopilot.decision` | `cycle`, `decision` (`emitted`/`dropped`), `finding_id`, `tier`, `title`, `reason`; `task_id` when emitted | per emitted task / dropped finding |
| `autopilot.approve` / `autopilot.deny` | `run_id`, `task_id`, `gate`, `attempt`, `command_id`, `actor`, `with_feedback` | per hub gate decision on a task autopilot emitted |
| `hub.approve` / `hub.deny` | as above plus `subject` = `run_gate`; or (`hub.approve` only) `subject` = `permission`, `agent`, `file`, `rules`, `dangerous` | a hub gate decision on any other task; an agent tool permission allowed in the hub |
| `hub.session.start` | `session_id`, `session_name`, `session_kind` (`shell`/`claude`/`codex`), `workspace`, `cwd`, `worktree`, `branch`, `base`, `repo` | a hub session opened |
| `hub.session.end` | the session fields plus `exit_code`, `signal`, `hub_stopping` (hung up by `fw hub stop`), `summary` (the agent's last reply, one line, 280 chars) | its process exited |
| `hub.merge` / `hub.pr` | the session fields plus `branch`, `base`, `commit`, `url` (PR), `summary` | a review drawer merge / opened PR |
| `hub.review` | the session fields plus `action` (`commit`/`push`/`rebase`/`request`), `commit`; a request adds `reviewer_session_id`, `reviewer_kind`, `reused` | a review action, or a reviewer asked for |
| `hub.handoff` | the target session's fields plus `from`, `source`, `bytes`, `truncated` | text handed off into a session |
| `hub.fanout` | `fanout_id`, `name`, `workspace`, `base`, `base_sha`, `graded_at`, `leader`, `winner`, `counts`, `candidates` (`label`, `session_id`, `verdict`, `passed`, `branch`, `commit`, `graders_passed`, `graders_total`) | a verified fan-out finished grading |

`hub.*` rows land in the store of the project the touched folder (a session's worktree, else its start directory) is in. A folder in no project the home knows -- not a git repo, a repo no `fw` verb ran in, a copy of a project repo -- records nothing and is not made a project (no project dir, no `.git/config` write); a failed write is logged in the hub log, never fails the action (`crates/flywheel/src/hub_audit.rs`).

`--json` prints NDJSON, one object per row; `at` is RFC 3339. Plain output is one line per row.

### `purge [--project [--yes]]`

`fw purge` deletes every transcript body of the cwd project (`transcripts/`, plus any pre-ledger `logs/runs/`) and reports the count; the `transcript` rows stay. `fw purge --project` deletes the whole `$FLYWHEEL_HOME/projects/<id>/` dir (store file, tasks, specs, transcripts, worktrees) after a `[y/N]` prompt; unattended (stdin not a TTY) it exits 2 unless `--yes`. A Postgres store's rows survive it.

Retention: bodies older than `transcript_retention_days` (default 90; `0` keeps forever) are deleted at worker start and by `fw purge`; rows stay.

### `audit RUN_ID`

Streams the totally-ordered audit records for one run, routed by the shell (`audit_main`, `crates/flywheel/src/cli.rs`) onto the core audit stream (`run_audit`, `crates/flywheel-core/src/audit.rs`). **Redaction is ON by default** — `--raw` disables it only for authorized forensics.

| Flag | Controls |
|---|---|
| positional `run_id` | the run to stream |
| `--db`, `--logs-root` | store and log roots |
| `--json` | NDJSON output |
| `--follow` | tail until terminal status (Ctrl-C exits 130) |
| `--poll-interval` | tail cadence |
| `--redact` | explicit opt-in (already default) |
| `--redact-policy NAME`, `--redact-salt SALT` | redaction tuning; `NAME` is a built-in (`default`, `strict`; `strict` also blanks the inputs and results of Read/Grep/Bash/BashOutput/TaskOutput/NotebookRead, every MCP tool (`mcp__*`, codex `mcp:*`), and codex `command_execution`/`mcp_tool_call` items, including ones recorded raw; Glob and WebFetch are kept) |
| `--raw` | disable redaction (authorized forensics) |
| `--dry-run` | coverage report only |

## The operator console (TUI)

Entry: `main_with` (`crates/flywheel/src/tui.rs`). Bare `fw` on a TTY opens it; `fw --json` or any non-TTY stdout prints one `DashboardSnapshot` and exits 0 with no ANSI.

A home project's store is created on launch; an explicit `--db` naming a missing sqlite file exits 2 (remedy: `run 'fw worker' ...`).

### Console flags

| Flag | Controls |
|---|---|
| `--db PATH` | store path (default the home project's `flywheel.sqlite`) |
| `--tasks-dir DIR` | tasks root for summary counts and `/archive` |
| `--json` | print one JSON snapshot and exit (implied off a non-TTY) |
| `--poll-interval FLOAT` | dashboard poll cadence (default 1.0s) |
| `--model ID` | model forwarded to the spawned worker (overrides `model`) |
| `--no-worker` | do not spawn a supervised worker on launch (status bar shows `worker: none`) |

On launch the console opens the store, constructs a `WorkerSupervisor` and spawns it unless `--no-worker`, and constructs an `AutopilotSupervisor` that is **never auto-spawned** — autopilot starts only via `/autopilot start` because it writes to the base branch unattended (decision D-6). On exit both supervisors detach by default: the child keeps running so a SIGINT on the console never silently kills it.

### Dashboard

`DashboardApp` (`crates/flywheel/src/dashboard.rs`) polls a `DashboardSnapshot` roughly once a second. Layout: a summary header (`active= queued= done= failed= tokens= cost= runtime=`), a worker/autopilot status bar, one `session-pause:` line per active pause, a table of active runs (task / status / attempt-iter / age / tokens / cost / last action), an empty-state line, a last-error status bar, and a persistent input bar. Rows that leave the active set linger dimmed for 30 seconds before dropping; a poll failure leaves the last good frame plus a warning rather than crashing.

The input bar live-filters rows by case-insensitive substring; a leading `/` switches it to slash-command mode.

Worker status-bar vocabulary: `worker: supervised pid=N` / `detached (this console did not spawn it)` / `none -- type '/worker start' to spawn one` / `dead ... (exit=N)` / `dead (crash-loop budget exhausted) ...` / `error: <reason>`. Autopilot uses the same states; its `detached` comes from a live activity record (pid + freshness deadline) rather than a lease: `autopilot: supervised pid=N` / `detached pid=N ...` / `none ...` / `dead ...` / `error ...`.

### Session screen

`SessionScreen` (`crates/flywheel/src/session_screen.rs`), opened by Enter on a dashboard row. It renders one run's merged audit transcript chat-style via a `TranscriptTailer` (~250ms poll) and tail-follows until you scroll up (follow pauses, new-activity indicator) or the run reaches a terminal status (banner pins the state, steering disabled).

The compose box doubles as input: plain text is a `say`; a leading `/` is a slash command. A submit-time status re-check enforces the lifecycle rule — say/interrupt only in RUNNING/VALIDATING, approve/reject only in AWAITING_APPROVAL; outside the set it shows an inline notice and leaves the store untouched.

### History screen

`HistoryScreen` (`crates/flywheel/src/history_screen.rs`), opened via `h` or `/history`. One row per task, newest-finished first — same grouping as `fw history` (columns: phase / task / status / finished / runs / tokens / cost). Enter drills into the run's session screen; `r` reloads. No polling timer — finished runs do not change.

### Quit-handoff prompt

`QuitPrompt` (`crates/flywheel/src/quit_prompt.rs`) is shown only when the console owns a supervised worker and/or autopilot daemon (`request_quit` short-circuits otherwise). Only `s` signals a child; the child is never silently killed.

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

`WorkerSupervisor` (`crates/flywheel/src/worker_supervisor.rs`) spawns `<console binary> worker --db <path> [--tasks-dir ..] [--model ..]` in a new session (`setsid`), so a console Ctrl-C never reaches the child; only `stop` signals it (SIGTERM to the process group, 10s wait, then SIGKILL). It reads `task_claims` to detect liveness: a live lease this console did not spawn shows as DETACHED. Spawn logs land under the project's `logs/worker/`.

`AutopilotSupervisor` (`crates/flywheel/src/autopilot_supervisor.rs`) spawns `<console binary> autopilot [--tasks-dir ..] [--model ..] [--activity-file ..]` without `--once` (the neverending daemon). It writes no lease; liveness rides on the daemon's activity record (pid + `expires_at`), so a live daemon this console did not spawn is adopted as DETACHED and a stale record reads as not-live. Logs land under the project's `logs/autopilot/`. Same spawn/detach/stop shape, independent of the worker.

## Slash commands

`crates/flywheel/src/slash.rs` is the shared command vocabulary for the dashboard and session screens — an alternate input surface, not a new channel: each slash verb reuses the exact store-mediated semantics of its CLI or key twin.

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

An unknown verb leaves the typed line populated for editing rather than erroring.

## `init`

`fw init [--no-skills] [--provision] [--allow-unverified]` (`cmd_init`, `crates/flywheel-orchestrator/src/workflow/init.rs`). **Writes nothing into the repo tree** (beyond the `.git/config` project id). Outside a git repo: exit 2.

1. Resolves the project and prints `project: <id> (<dir>)`.
2. Creates `$FLYWHEEL_HOME/config.toml` (every key commented) when missing.
3. Postgres `store`: runs the preflight; a blocking issue exits 2 unless `--allow-unverified`; `--provision` creates the tables now.
4. Installs the fw-* skills under `$CLAUDE_CONFIG_DIR/skills` (default `~/.claude/skills`) unless `--no-skills`.
5. Reports agent auth (`ANTHROPIC_API_KEY` or `~/.claude/.credentials.json`; non-blocking).
6. Without a `.flywheel.toml`: detects a verify command and prints a suggested file to commit, then `fw trust`:

```toml
verify = "cargo test"
# protected_paths = [".github/**", ".flywheel.toml"]
# land = "merge"    # or "pr"
```

Detection: `Cargo.toml` -> `cargo test`, `package.json` test script -> `npm test`, pytest config -> `pytest`, `go.mod` -> `go test ./...`, Makefile `test:` -> `make test`. Skills and the pipeline they drive: [workflow.md](workflow.md).

## The `--json` snapshot

`fw --json` (or any non-TTY stdout) prints one `DashboardSnapshot` — the same shape the console dashboard renders (`crates/flywheel/src/snapshot.rs`). The key set is stable; scripts piping `fw --json` depend on it.

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

A work source that raises degrades the summary's task counts to empty rather than crashing the frame.
