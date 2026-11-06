# Configuration

Flywheel reads three TOML files and one environment variable; the repo needs none of them. Every key below is everything a user can set -- each other knob is a built-in default.

| File | Who writes it | Keys |
|------|---------------|------|
| `$FLYWHEEL_HOME/config.toml` (default `~/.flywheel`) | you, once per machine (**user**) | all eight |
| `<repo top-level>/.flywheel.toml` | the team, committed (**repo**); applies only after `fw trust` | `verify`, `protected_paths`, `land`, `project_id` |
| `$FLYWHEEL_HOME/projects/<id>/config.toml` | you, per project, never in the repo (**personal**) | all eight |

**Precedence:** CLI flag > env > personal > repo > user > built-in default. `fw config [--json]` prints every key with the layer it came from.

**Strict:** any other key -- including every old `flywheel.toml` section (`[sandbox]`, `[submit]`, `[source]`, ...) -- is an error naming it, exit non-zero. The loader is `crates/flywheel-orchestrator/src/config.rs`.

## Keys

| Key | Type | Default | Maps onto |
|-----|------|---------|-----------|
| `model` | string | unset (the agent CLI's own default; `null` in `fw config`) | the agent model (`--model` wins) |
| `concurrency` | int >= 1 | `1` | worker pool size (`--concurrency` wins) |
| `store` | `"sqlite"` or a `postgres://` / `postgresql://` DSN | `"sqlite"` (`projects/<id>/flywheel.sqlite`) | the store backend; one Postgres DSN serves every project |
| `transcript_retention_days` | int >= 1 | `90` | transcript body retention |
| `verify` | string (shell) | unset (no gate) | BOTH the standing submit gate (re-run under the merge lock before every landing) and the phase-exit gate; `""` disables a lower layer's command |
| `protected_paths` | list of globs | `[]` | paths a finished branch may not touch (`**` crosses dirs; `*`, `?` do not); a match parks the work |
| `land` | `"merge"` or `"pr"` | `"merge"` | the submit strategy |
| `project_id` | UUID | the id in the repo's `.git/config` | which `projects/<id>/` dir (and Postgres partition) the repo uses -- a committed value lets a team share one |

Env: `FLYWHEEL_PG_DSN` sets `store` (source `env`). Flags: `--model`, `--concurrency` (source `flag`; `fw config` accepts both to preview).

```toml
# ~/.flywheel/config.toml
model = "claude-opus-4-8"
concurrency = 2
store = "postgres://fw:secret@db.internal:5432/flywheel"

# <repo>/.flywheel.toml (committed)
verify = "scripts/check.sh"
protected_paths = [".github/**", ".flywheel.toml"]
land = "pr"
```

`fw config --json` (pinned shape): an object with exactly the eight keys, each `{"value": <value or null>, "source": "default|user|repo|personal|env|flag"}`. A DSN's password prints as `***`.

## The repo file and trust

`.flywheel.toml` is committed, so it can make every clone run `verify`. It is honored only after approval:

- **Credentials are refused at load**, naming the key and never echoing the value: a key named `store`, any key whose name contains `dsn`, `password`, `passwd`, `token`, or `secret` (case-insensitive, at any depth), a url/uri-named key whose value holds `scheme://user@...`, or any string value holding `scheme://user:password@...`.
- **Trust** lives in `$FLYWHEEL_HOME/trust.json`, keyed by (the LOCAL project id from `.git/config`, sha256 of the file's bytes). Editing the file revokes it; the file's own `project_id` never picks its trust key. Code: `crates/flywheel-orchestrator/src/trust.rs`.
- `fw trust` approves the current bytes (showing the file and what it runs when stdin is a terminal); `fw trust --revoke` drops every approval of the project.
- **Untrusted, executing verbs** (`worker`, `autopilot`, `archive`, the console, hub-launched runs): stdin not a TTY -> refuse, exit non-zero, print `cd <repo> && fw trust`; stdin a TTY -> show the file, prompt once `[y/N]`, record on yes.
- **Untrusted, read-only verbs** (`status`, `config`, `history`, `show`, ...): one warning line, then run as if the file were absent.
- The hub's Flywheel panel shows the untrusted state and the exact command.

## Legacy layout

A repo's `flywheel.toml` and `.flywheel/` are **not read**. When either is present, every verb prints one notice that it is ignored and can be deleted, and changes nothing in the repo tree. State lives under `$FLYWHEEL_HOME/projects/<id>/`; see [cli.md](cli.md#projects-and-the-home).

Every old `WorkPolicy` setting without a key above runs at its built-in default (`WorkPolicy::default`, `crates/flywheel-orchestrator/src/policy.rs`): `directory` source over `projects/<id>/tasks`, worktrees under `projects/<id>/worktrees`, no default graders, `[execution] mode = "local"`, `[worker]` nudge 300s / pause ceiling 6h, `[submit]` remote `origin` / no `base` / no tiers / recovery rung 30 turns, 900s, no `[held_out]` gate, `[sandbox]` preset `fast` on the `worktree` backend, `[deadlines]` defaults, `[autopilot]` depth 5 / 300s / weights default, `[triage]` default labels.

**`--policy FILE` (legacy, removed later):** `status`, `live`, `history`, `show`, `archive`, `recover`, `resolve`, `recheck-blocked`, `validate`, `worker`, `autopilot`, and the console still accept an explicit old-format policy file, loaded as-is (no effective config, no trust); `--db` / `--tasks-dir` still name paths directly. The reference below documents that format only for `--policy`.

## Legacy policy file reference (`--policy FILE` only)

Precedence inside a `--policy` file: an explicit CLI flag wins over the file, the file over the built-in default. Values are strict (a wrong type or enum fails naming the file and key); unknown keys and sections are ignored. `[source]` is the only required table. Postgres DSNs come from the environment, never the file.

### `[source]` (required)

Where work comes from. `kind` selects the `WorkSource` backend; the remaining keys are per-kind. See [work-sources.md](work-sources.md) for each kind's listing/grading behavior.

| Key | Type | Default | Applies to | Controls |
|-----|------|---------|------------|----------|
| `kind` | enum `directory`/`github`/`github_ci`/`github_review` | (required) | all | which `WorkSource` backend builds |
| `tasks_dir` | path | `.flywheel/tasks` | `directory` | root of `active/<phase>/*.json` task files |
| `repo` | `owner/name` | (required) | `github`/`github_ci`/`github_review` | the GitHub repo the `gh` CLI targets |
| `label` | str | (required) | `github` | only issues carrying this label are listed |
| `done_action` | enum `comment`/`close` | `comment` | `github` | post the outcome as a comment, or close the issue |
| `failure_filter` | str | `failure` | `github_ci` | `gh run --status` filter for which CI runs become work |

`github_review` lists unresolved PR review threads; its grade is the policy's `[[defaults.graders]]` run out-of-band, never the thread's `isResolved` state.

### `[paths]` (optional)

Where runtime state lives. When a key is unset the CLI falls back to its built-in default.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `db` | path | `.flywheel/flywheel.sqlite` | SQLite store location |
| `sandbox_root` | path or token | `.flywheel/worktrees` | root under which each task's worktree/sandbox is created |

`sandbox_root` resolution (`resolve_sandbox_root`, `policy.rs`): a relative path anchors at the repo root (never the process cwd); an absolute path is used verbatim; two tokens opt into out-of-tree layouts:

- `@cache` — `<cache-base>/flywheel/<repo-name>-<id>/worktrees`, where `<cache-base>` is the first writable of `$XDG_CACHE_HOME`, `~/.cache`, and the platform tmpdir, and `<id>` keys the repo by the realpath of its git common dir (clones never collide; a linked worktree maps to its main repo).
- `@sibling` — `<repo-parent>/<repo-name>.worktrees`; refused at startup (`PolicyError`) when the parent directory is read-only (CI mounts).

Both `flywheel worker` and `flywheel orchestrate` honor the key; `--sandbox-root` overrides it. When the resolved root differs from the legacy `.flywheel/worktrees`, the worker's retention sweep also covers the legacy directory while it exists, so worktrees created before a relocation still age out. An out-of-tree root stays out of docker build contexts by construction; for the default nested root, `flywheel init` appends `.flywheel/` to `.dockerignore` when a Dockerfile is present (docker does not read `.gitignore`).

### `[agent]` (optional)

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `model` | str (opaque) | unset | the model id passed verbatim to the agent CLI |
| `id` | str (opaque) | unset | the multi-agent opt-in: routes every agent invocation (worker, rubric judge, recovery summarizer, conflict resolver, autopilot) through the `flywheel-agents` runtime (`"claude-code"` and `"codex"` ship); unset means `"claude-code"` |
| `transport` | str | unset (`"cli"`) | adapter transport selection; `"cli"` is the only transport either adapter ships (`"sdk"` is rejected); requires `id` |

Values are opaque: flywheel maintains no allowlist. Model resolution precedence is `--model` flag > `[agent] model` > the agent CLI's own default (`resolve_model`, `crates/flywheel-worktree/src/daemon.rs`). An empty or whitespace-only string raises `PolicyError`, as does `transport` without `id`. An unknown `id` fails at run time with the registered adapter set in the message. See [agent-harness.md](agent-harness.md) for the multi-agent architecture.

### `[[defaults.graders]]` (optional)

Default grader policy, parsed as the standard `Grader` array via `load_graders` (`crates/flywheel-core/src/loaders.rs`). See [task-schema.md](task-schema.md) for the `Grader` shape.

**Meaningful only for tracker sources** (`github`/`github_ci`/`github_review`): applied to a work item that declares no graders of its own. Directory task files always carry their own graders (the schema requires at least one), so the default is inert for `directory`. A tracker item with no graders and no default policy is not runnable and never reaches the scheduler.

```toml
[[defaults.graders]]
type = "command"
run = "cargo test --workspace"
```

### `[store]` (optional)

Persistence backend. An absent section means `sqlite`.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `backend` | enum `sqlite`/`postgres` | `sqlite` | persistence backend |
| `schema` | str (postgres only) | unset | Postgres schema name |

The Postgres DSN is never configured here; it comes from the environment (see below).

### `[execution]` (optional)

Distribution and capability matching. See [orchestration.md](orchestration.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `mode` | enum `local`/`distributed` | `local` | distribution posture |
| `capabilities` | list of str | `[]` | this worker's advertised capability set |

**`mode = "distributed"` requires `store.backend = "postgres"` or `load_policy` raises**. That postgres requirement is `mode`'s *only* runtime effect: `execution_mode` is a pure load-time validation assertion — it is never read by any scheduler/claim/lease code path, so it does not itself change how work is scheduled, claimed, or leased. `capabilities` is the *worker's* advertised set: the scheduler offers this worker only items whose `required_capabilities` is a subset of it. This is distinct from `[sandbox.capabilities]`, which is the *agent's* tool/skill/MCP surface inside the sandbox.

### `[worker]` (optional)

Worker pool size, checkpoint-nudge threshold, and session-pause ceiling for a single `flywheel worker` invocation.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `concurrency` | int (>=1 once resolved) | `1` | how many tasks one `flywheel worker` invocation drives concurrently |
| `checkpoint_nudge_seconds` | finite float (>= 0; negative/`inf`/`nan` rejected) | `300.0` | remaining wall time to a task's `AGENT_ITERATION` deadline at which the harness nudges the agent to commit work-in-progress, when its branch has gained no new commit; `0` disables |
| `session_pause_ceiling_seconds` | finite float (>= 0; `inf`/`nan` rejected) | `21600.0` | ceiling (seconds) clamping a session-limit-driven pool-wide claim pause; `0` disables pausing entirely |

`--concurrency` overrides this value per-run (flag wins). An absent `[worker]` table means `concurrency = 1` — single serial worker, today's behavior byte-for-byte. A resolved value below `1` (e.g. `--concurrency 0`, a negative, or a non-integer flag) is a hard error: the worker exits non-zero naming the setting and claims no task (`resolve_concurrency`, `crates/flywheel-worktree/src/daemon.rs`). The `< 1` range is *not* checked at load time because the flag can override the config, so a config of `0` with `--concurrency 3` is valid.

`checkpoint_nudge_seconds` is **default-on**: an absent `[worker]` table still nudges (300s), because the retro loss came from this behavior not existing. The worktree worker binds a git progress probe per run — the nudge fires at most once, only when the sandbox branch has no new commit as the deadline nears, and never moves the deadline (the iteration is still cancelled at its ceiling). The threshold is measured against the resolved `AGENT_ITERATION` ceiling, so `[deadlines]` operator overrides flow through automatically. `0` disables the nudge. The container backend does not wire the probe (nudge dormant there).

`session_pause_ceiling_seconds` is **default-on** (21600.0 = 6h): when any run surfaces a structural session-limit reset (the harness's `HarnessOutcome.session_limit_reset`, never parsed from an error string), the orchestrator records a pause behind the shared claim store with `pause_until = min(reset, now + ceiling)`. While that horizon is in the future, **every** driver against the same claim store (in-memory, SQLite, or Postgres) declines fresh and resume claims pool-wide — bookkeeping (lease sweeps, reconcile, redrives, snapshots) is unaffected — and claiming resumes on its own once it passes. The ceiling bounds how long one derived reset can hold the pool off claiming, so a far-future reset never yields an unbounded pause; a reset already in the past never records a pause. Each pause is a queryable ledger row plus a stream log line naming `pause_until` and its cause. `0` disables pausing entirely (today's behavior byte-for-byte).

### `[submit]` (optional)

How DONE work leaves the loop. An absent table means historical merge landing. See [strategy.md](strategy.md) for the shipped strategies and the protected-paths contract.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `strategy` | enum `merge`/`pr`/`phase` | `merge` | how DONE work lands; routed via the `SUBMIT_STRATEGIES` registry; ignored for routing when `[[submit.tiers]]` is configured |
| `protected_paths` | list of glob | `[]` | repo-relative whole-path globs (`**` crosses dirs; `*`, `?` do not) a finished branch may not touch; a match refuses landing and parks the work |
| `remote` | str | `origin` | `pr` strategy push target |
| `pr_base` | str | unset (worker base branch) | `pr` strategy PR base branch |
| `base` | str | unset (checked-out branch) | explicit landing/phase-base branch |
| `verify` | str (shell) | unset (no gate) | standing build invariant re-run under the merge lock against the exact tree about to become the base, on every land path; a non-zero exit refuses landing and parks the work (`park_kind="standing-verify"`) |
| `recovery_agent_max_turns` | int (>= 0) | `30` | turn ceiling for the bounded conflict-resolution session on the `merge` strategy's fallback rung; `0` disables the rung (a merge conflict parks exactly as before) |
| `recovery_agent_max_wall_seconds` | finite float (> 0; `inf`/`nan` rejected) | `900.0` | wall-clock ceiling for that same session; the session is cancelled and the run parks preserved when it is exceeded |

`protected_paths` is honored by both strategies; it protects the verification surface — grader configs, CI, harness state — from being rewritten by the work it judges.

`verify` is the "trunk must always build" gate (spec 00064): repo-wide and independent of the task's own (often crate-scoped) command graders, it catches a *semantic merge skew* where two independently-valid changes union into a tree that does not build. It runs serialized under the merge lock — a slow command bottlenecks landings — and inherits the resolved `[sandbox.env]`. Example: `verify = "cargo build --workspace --tests"` or `verify = "scripts/check.sh"`.

`recovery_agent_max_turns` / `recovery_agent_max_wall_seconds` arm the `merge` strategy's last recovery rung (spec 00077): when FF, rebase, and the merge-fallback all hit a textual conflict, a single bounded agent session resolves the conflict markers and stages the result. The resolved tree lands only after re-running the task's command graders, `[submit] verify`, and the declared held-out gate — the same out-of-band bar every other land clears; any failure parks the run preserved with the session's turn/wall usage recorded on the ledger. A land from this rung records `rung="agent-resolved"`. Both bounds are wrong-type / out-of-range validated at load (a bad value raises `PolicyError` naming the key) so a typo never silently disables or unbounds the rung. Setting `recovery_agent_max_turns = 0` keeps the historical merge-conflict park.

#### `[[submit.tiers]]` (optional, spec 00080)

Risk-tiered landing: route each DONE task by the highest tier its changed files classify at, instead of one repo-global strategy. Array of tables, each with exactly `tier` and `paths`:

```toml
[[submit.tiers]]
tier = 0                      # lands via merge, direct
paths = ["docs/**", "*.md"]

[[submit.tiers]]
tier = 2                      # lands via pr, human approval
paths = ["scripts/**"]
```

- Tiers: `0` = merge (direct), `1` = phase integration branch, `2` = PR. Highest tier of any touched file wins; a file matching no rule defaults to tier `1`.
- `paths` use `protected_paths` glob semantics (whole-path match; `**` crosses dirs, `*`, `?` do not) over the branch's merge-base-scoped diff.
- Classification is worker-side only; the policy file itself always classifies tier `2`, so a branch editing these rules cannot cheapen its own route.
- `protected_paths` outranks every tier (a protected file parks; tier 2 is a landing route, protection is a refusal).
- Each decision is recorded on the run's ledger (`LandingRouted`: per-file tiers, winning tier, strategy).
- Absent: landing is byte-identical to `[submit] strategy`.

### `[phase]` (optional)

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `verify` | str (shell) | unset (no gate) | command run against the merged phase base once every task in a phase has landed; a non-zero exit leaves the phase active |

Consumed by the phase-archive gate (`ArchiveOptions::phase_verify`, `crates/flywheel-worktree/src/phase.rs`) and the phase-PR publisher (`crates/flywheel-worktree/src/pr.rs`). Unset means today's archival behavior with no gate.

### `[held_out]` (optional)

Execute-time held-out landing gate. See [held-out-gate.md](held-out-gate.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `root` | path | unset (no gate) | directory of operator-declared held-out grader registrations (one `<task_id>.json` per gated task) the gate reads |

**There is deliberately no default `root`.** Unset means the worker builds no held-out source and landing is byte-identical to today; a default would silently activate gating on upgrade. A relative `root` resolves against the repo root (`build_held_out_source`, `crates/flywheel-worktree/src/daemon.rs`). A set `root` must be an existing directory: the worker refuses to start if it is missing, and the gate fails closed if it disappears mid-run. Exception: the conventional `.flywheel/verification/held-out` (git-ignored, so absent from a fresh clone) is created empty at worker start when missing.

### `[autopilot]` and `[autopilot.weights]` (optional)

Intake-daemon cadence and the score-axis weights. See [autopilot.md](autopilot.md) for the tier hierarchy and weighted scoring model.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `target_depth` | int (>0) | `5` | how deep autopilot fills the queue |
| `landing` | enum `merge`/`pr` | `merge` | landing posture for autopilot-authored work |
| `interval_seconds` | finite float (>0; `inf`/`nan` rejected) | `300.0` | seconds between daemon cycles |
| `authoring_concurrency` | int (>0) | `3` | authoring sessions run at once per pass; `1` = serial. Emission order and the per-pass write cap are unchanged |
| `discovery_cache_ttl_seconds` | float (>=0) | `21600.0` | lifetime of cached discovery verdicts (keyed on HEAD + working tree); `0` disables the cache |

`[autopilot.weights]` overrides individual score axes; each unset weight keeps the engine default (`AutopilotWeights`, `policy.rs`).

| Key | Type | Default |
|-----|------|---------|
| `tier` | float | `10.0` |
| `urgency` | float | `3.0` |
| `importance` | float | `3.0` |
| `unblock` | float | `2.0` |
| `effort` | float | `1.0` |
| `interrupt_base` | float | `10000.0` |

The autopilot CLI flags `--target-depth`/`--interval`/`--model` override the corresponding `[autopilot]` keys (`resolve_runtime`, `crates/flywheel-orchestrator/src/autopilot_run.rs`).

### `[triage]` (optional)

Board labels and cadence for the `flywheel triage` intake daemon (`fw triage`). The intake label defaults to the github source's own default label (`flywheel`), so the board triage reads is the board the worker drains. A malformed value exits 2 before the engine is built or any `gh` call is issued.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `intake_label` | string | `flywheel` | label whose issues are triage candidates |
| `ready_label` | string | `flywheel:ready` | label applied to a well-specified (fail-first) issue |
| `needs_detail_label` | string | `flywheel:needs-detail` | label applied to an under-specified issue |
| `interval_seconds` | finite float (>0; `inf`/`nan` rejected) | `300.0` | seconds between daemon cycles |
| `max_per_pass` | int (>0) | unset (uncapped) | candidates processed per pass; the remainder defers to a later pass |

The triage CLI flags `--interval`/`--model` override the corresponding `[triage]`/`[agent]` keys.

### `[deadlines]` (optional)

Default-on, per-class wall-clock ceilings (seconds) for the five external-call classes flywheel issues (spec 00066, `crates/flywheel-core/src/deadline_config.rs`). **Every class ships a finite default; set a key to `0` to opt that class out (unbounded).** TOML has no null, so `0` is the on-disk spelling of the unbounded opt-out — an omitted key keeps its default, leaving a pre-existing file byte-identical.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `agent_iteration_seconds` | float (`0` = unbounded) | `3600` | working-agent iteration ceiling (worker harness) |
| `rubric_judge_seconds` | float (`0` = unbounded) | `600` | rubric-judge stream ceiling (worker harness) |
| `command_grader_seconds` | float (`0` = unbounded) | `900` | command-grader ceiling (worker harness) |
| `docker_management_seconds` | float (`0` = unbounded) | `120` | docker management-call ceiling (container backend) |
| `autopilot_agent_seconds` | float (`0` = unbounded) | `1800` | discovery/authoring agent ceiling (autopilot daemon) |

Consumed only by those three layers; a negative, non-finite, or out-of-range (past ~1.8e19) value resolves to the same unbounded opt-out as `0`. Every other seconds-valued knob in this file rejects a value outside the `Duration` range (`inf`, `nan`, past ~1.8e19) at load. A task's own `budgets` object (see [task-schema.md](task-schema.md)) overrides `agent_iteration_seconds` and `rubric_judge_seconds` for that task alone — the heavyweight tail declares its ceiling per task instead of the repo loosening a class-wide one.

### `[sandbox.*]`

The `[sandbox]` table configures provisioning and the agent's execution environment. The top-level flat keys are below; the full per-subtable reference (`exec`, `capabilities`, `network`, `env`, `limits`, `retention`, `container`) and the named presets live in [sandbox.md](sandbox.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `setup` | str (shell) | unset (bare sandbox) | command run inside every newly created sandbox before the agent enters (deps, codegen); reused parked sandboxes skip it |
| `preset` | enum `fast`/`balanced`/`hardened` | `fast` | named code-owned baseline; per-key overrides from subtables merge on top |
| `backend` | enum `worktree`/`container` | `worktree` | execution backend; `container` lazy-wraps the submit strategy |
| `permission_mode` | str | `bypassPermissions` | agent permission mode: `bypassPermissions` runs unattended, `plan` is plan-only, anything else is supervised (`permission_policy_for`, `crates/flywheel-core/src/workflow.rs`) |

`setup` and `backend` are consumed by the worker (`crates/flywheel-worktree/src/daemon.rs`); `backend` selects the container wrap in `wrap_for_backend`. Several subtable behaviors are enforced only under `backend = "container"` — see [sandbox.md](sandbox.md) for which.

`[sandbox.limits]` carries one attempt-budget key (full table in [sandbox.md](sandbox.md)):

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `rubric_judge_max_turns` | int (>0) | `32` | per-judge-call turn budget for the rubric judge; an absent key keeps the harness default of 32 |

## Environment variables

Secrets live in the environment or your user `config.toml`, never in a committed file.

| Variable | Used for | Notes |
|----------|----------|-------|
| `FLYWHEEL_HOME` | the flywheel home | default `~/.flywheel` |
| `FLYWHEEL_PG_DSN` | Postgres DSN | sets the effective `store` (source `env`, above personal/repo/user); under a legacy `--policy` it is the primary DSN (`resolve_postgres_dsn`, `crates/flywheel-orchestrator/src/store_factory.rs`). An empty value is treated as unset |
| `DATABASE_URL` | Postgres DSN (fallback) | legacy `--policy` postgres only, when `FLYWHEEL_PG_DSN` is unset |
| `FLYWHEEL_DB` | SQLite store path fallback | fallback for the audit stream's `--db` when invoked without the shell's policy resolution (`resolve_db_path`, `crates/flywheel-core/src/audit.rs`); empty value treated as unset |
| `ANTHROPIC_API_KEY` | raw API-key auth for the agent | `crates/flywheel-container/src/auth.rs`; in `api_key` container auth mode the token is read from this name |
| `PGSSLMODE`, `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY` | Postgres TLS defaults | used only when the DSN omits the matching keyword |

Postgres TLS follows libpq: `sslmode` defaults to `prefer`; `verify-ca`/`verify-full` need `sslrootcert` (a PEM file, `~/.postgresql/root.crt`, or `system` for the platform store, which forces `verify-full`). Example: `postgresql://u:p@db.example.com/app?sslmode=verify-full&sslrootcert=system`.

### What `flywheel init` scaffolds (legacy)

`flywheel init` writes only a thin subset of the surface (`render_init_policy`, `crates/flywheel-orchestrator/src/workflow/init.rs`). It is idempotent and never overwrites a non-managed file. See [cli.md](cli.md) for the full init flag list.

Rendered:

- `[source]` — `directory` or `github` only. `github_ci`/`github_review` must be hand-edited in.
- `[paths]` — `db` and `sandbox_root`.
- `[store]` — from `--store`/`--pg-schema`.
- `[submit]` — `protected_paths = [".github/**", "flywheel.toml"]` active; `base` (suggesting the detected branch, else `"main"`), `verify`, `strategy`, `remote`, and `pr_base` as commented placeholders.
- Commented placeholders for `[[defaults.graders]]`, `[agent] model`, `[sandbox] setup`/`backend`/`preset`/`permission_mode`, `[phase] verify`, `[held_out] root`, `[autopilot]` and `[autopilot.weights]`, `[worker] concurrency`, and `[execution] mode`/`capabilities`. The `[sandbox.*]` subtables are listed in a comment only.

**Not rendered by init — must be hand-written:** `[triage]`, `[deadlines]`, every `[sandbox.*]` subtable (`exec`/`capabilities`/`network`/`env`/`limits`/`retention`/`container`), and `[source] failure_filter`.

The commented `[agent] model` placeholder shows one example model id; do not treat any printed id as canonical — the value is opaque and projects vary.
