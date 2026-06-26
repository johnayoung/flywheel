# Configuration (flywheel.toml)

A single `flywheel.toml` at the repo root is the consumer repo's versioned contract with the orchestrator: where work comes from, what "runnable" means by default, and how finished work lands. Switching a project between a task directory and an issue tracker is a committed config change, not a flywheel code change.

One module owns the whole surface: `flywheel_orchestrator._policy.load_policy` (`_policy.py:405`) parses the file with stdlib `tomllib` into a frozen `WorkPolicy` (`_policy.py:324`). Every key is validated there, every default lives there.

## Precedence and validation

- **Precedence**: an explicit CLI flag wins over the file, the file wins over the built-in default. The read commands (`status`, `live`, `worker`, `autopilot`, ...) auto-detect `flywheel.toml` in the cwd (`_workflow.py:783`); `--policy` overrides the path, and an explicit `--tasks-dir`/`--db`/`--sandbox-root`/`--model` always beats the file.
- **Strict on values, lenient on keys**: a wrong-typed or out-of-enum value fails fast with a `PolicyError` that names the offending file and key — a typo never silently degrades behavior. Unknown *keys* under a known table, and unknown *section* tables, are ignored for forward-compatibility (`_optional_*` helpers).
- **`[source]` is the only required table.** Its absence raises `PolicyError` "missing required `[source]` table" (`_policy.py:417`). Every other section is optional; an absent section yields a back-compat default so a pre-existing file keeps loading unchanged.
- **Credentials and DSNs never live in the file.** Postgres DSNs, API tokens, and OAuth tokens are read from named environment variables only (see [Environment variables](#environment-variables)). Config carries env var *names*, never values.

## Sections at a glance

| Section | Required | Purpose | Deep dive |
|---------|----------|---------|-----------|
| `[source]` | yes | where work comes from (kind + per-kind settings) | [work-sources.md](work-sources.md) |
| `[paths]` | no | where runtime state lives (db, sandbox root) | this doc |
| `[agent]` | no | agent model id | this doc |
| `[[defaults.graders]]` | no | default graders for tracker work items | [task-schema.md](task-schema.md) |
| `[store]` | no | persistence backend (sqlite/postgres) | this doc |
| `[execution]` | no | mode (local/distributed) + advertised capabilities | [orchestration.md](orchestration.md) |
| `[submit]` | no | how DONE work lands (strategy, protected paths) | [strategy.md](strategy.md) |
| `[phase]` | no | phase-exit verify gate | this doc |
| `[held_out]` | no | execute-time held-out landing gate | [held-out-gate.md](held-out-gate.md) |
| `[autopilot]` | no | intake-daemon cadence + scoring weights | [autopilot.md](autopilot.md) |
| `[sandbox.*]` | no | provisioning + the agent's execution environment | [sandbox.md](sandbox.md) |

## `[source]` (required)

Where work comes from. `kind` selects the `WorkSource` backend; the remaining keys are per-kind. See [work-sources.md](work-sources.md) for each kind's listing/grading behavior.

| Key | Type | Default | Applies to | Controls |
|-----|------|---------|------------|----------|
| `kind` | enum `directory`/`github`/`github_ci`/`github_review` | (required) | all | which `WorkSource` backend builds |
| `tasks_dir` | path | `.flywheel/tasks` | `directory` | root of `active/<phase>/*.json` task files |
| `repo` | `owner/name` | (required) | `github`/`github_ci`/`github_review` | the GitHub repo the `gh` CLI targets |
| `label` | str | (required) | `github` | only issues carrying this label are listed |
| `done_action` | enum `comment`/`close` | `comment` | `github` | post the outcome as a comment, or close the issue |
| `failure_filter` | str | `failure` | `github_ci` | `gh run --status` filter for which CI runs become work |

`github_review` lists unresolved PR review threads; its grade is the policy's `[[defaults.graders]]` run out-of-band, never the thread's `isResolved` state (`_policy.py:1352`).

## `[paths]` (optional)

Where runtime state lives. When a key is unset the CLI falls back to its built-in default (`_policy.py:653`).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `db` | path | `.flywheel/flywheel.sqlite` | SQLite store location |
| `sandbox_root` | path | `.flywheel/worktrees` | root under which each task's worktree/sandbox is created |

Any non-empty string is accepted. Note: the `init`-rendered template and the `_policy.py` docstring example write `.flywheel/sandboxes` for `sandbox_root` while the committed repo file uses `.flywheel/worktrees` — the difference is cosmetic.

## `[agent]` (optional)

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `model` | str (opaque) | unset | the model id passed verbatim to the SDK |

The value is opaque: flywheel maintains no allowlist. Model resolution precedence is `--model` flag > `[agent] model` > SDK/Claude Code default (`worker.py:1238`). An empty or whitespace-only string raises `PolicyError`.

## `[[defaults.graders]]` (optional)

Default grader policy, parsed as the standard `Grader` array via `flywheel_core.loaders.load_graders` (`_policy.py:426`). See [task-schema.md](task-schema.md) for the `Grader` shape.

**Meaningful only for tracker sources** (`github`/`github_ci`/`github_review`): applied to a work item that declares no graders of its own. Directory task files always carry their own graders (the schema requires at least one), so the default is inert for `directory`. A tracker item with no graders and no default policy is not runnable and never reaches the scheduler.

```toml
[[defaults.graders]]
type = "command"
run = "uv run pytest"
```

## `[store]` (optional)

Persistence backend. An absent section means `sqlite` (`_policy.py:688`).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `backend` | enum `sqlite`/`postgres` | `sqlite` | persistence backend |
| `schema` | str (postgres only) | unset | Postgres schema name |

The Postgres DSN is never configured here; it comes from the environment (see below).

## `[execution]` (optional)

Distribution and capability matching. See [orchestration.md](orchestration.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `mode` | enum `local`/`distributed` | `local` | distribution posture |
| `capabilities` | list of str | `[]` | this worker's advertised capability set |

**`mode = "distributed"` requires `store.backend = "postgres"` or `load_policy` raises** (`_policy.py:460`). That postgres requirement is `mode`'s *only* runtime effect: `execution_mode` is a pure load-time validation assertion (`_policy.py:348`) — it is never read by any scheduler/claim/lease code path, so it does not itself change how work is scheduled, claimed, or leased. `capabilities` is the *worker's* advertised set: the scheduler offers this worker only items whose `required_capabilities` is a subset of it. This is distinct from `[sandbox.capabilities]`, which is the *agent's* tool/skill/MCP surface inside the sandbox.

## `[submit]` (optional)

How DONE work leaves the loop. An absent table means historical merge landing. See [strategy.md](strategy.md) for the two shipped strategies and the protected-paths contract.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `strategy` | enum `merge`/`pr` | `merge` | how DONE work lands; routed via the `SUBMIT_STRATEGIES` registry |
| `protected_paths` | list of glob | `[]` | repo-relative globs (`PurePath.full_match`, `**` crosses dirs) a finished branch may not touch; a match refuses landing and parks the work |
| `remote` | str | `origin` | `pr` strategy push target |
| `pr_base` | str | unset (worker base branch) | `pr` strategy PR base branch |
| `base` | str | unset (checked-out branch) | explicit landing/phase-base branch |

`protected_paths` is honored by both strategies (`worker.py:598`); it protects the verification surface — grader configs, CI, harness state — from being rewritten by the work it judges.

## `[phase]` (optional)

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `verify` | str (shell) | unset (no gate) | command run against the merged phase base once every task in a phase has landed; a non-zero exit leaves the phase active |

Consumed at `worker.py:833`. Unset means today's archival behavior with no gate.

## `[held_out]` (optional)

Execute-time held-out landing gate. See [held-out-gate.md](held-out-gate.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `root` | path | unset (no gate) | directory of operator-declared held-out grader registrations (one `<task_id>.json` per gated task) the gate reads |

**There is deliberately no default `root`.** Unset means the worker builds no held-out source and landing is byte-identical to today (`_policy.py:885`); a default would silently activate gating on upgrade. A relative `root` resolves against the repo root (`worker.py:1376`).

## `[autopilot]` and `[autopilot.weights]` (optional)

Intake-daemon cadence and the score-axis weights. See [autopilot.md](autopilot.md) for the tier hierarchy and weighted scoring model.

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `target_depth` | int (>0) | `5` | how deep autopilot fills the queue |
| `landing` | enum `merge`/`pr` | `merge` | landing posture for autopilot-authored work |
| `interval_seconds` | float (>0) | `300.0` | seconds between daemon cycles |

`[autopilot.weights]` overrides individual score axes; each unset weight keeps the engine default (`ScoreWeights`, `_autopilot.py:100`).

| Key | Type | Default |
|-----|------|---------|
| `tier` | float | `10.0` |
| `urgency` | float | `3.0` |
| `importance` | float | `3.0` |
| `unblock` | float | `2.0` |
| `effort` | float | `1.0` |
| `interrupt_base` | float | `10000.0` |

The autopilot CLI flags `--target-depth`/`--interval`/`--model` override the corresponding `[autopilot]` keys (`_autopilot_run.py:65`).

## `[sandbox.*]`

The `[sandbox]` table configures provisioning and the agent's execution environment. The top-level flat keys are below; the full per-subtable reference (`exec`, `capabilities`, `network`, `env`, `limits`, `retention`, `container`) and the named presets live in [sandbox.md](sandbox.md).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `setup` | str (shell) | unset (bare sandbox) | command run inside every newly created sandbox before the agent enters (deps, codegen); reused parked sandboxes skip it |
| `preset` | enum `fast`/`balanced`/`hardened` | `fast` | named code-owned baseline; per-key overrides from subtables merge on top |
| `backend` | enum `worktree`/`container` | `worktree` | execution backend; `container` lazy-wraps the submit strategy |
| `permission_mode` | str | `bypassPermissions` | SDK permission mode |

`setup` is consumed at `worker.py:1414`; `backend` selects the container wrap in `maybe_wrap_for_backend` (`worker.py:1303`). Several subtable behaviors are enforced only under `backend = "container"` — see [sandbox.md](sandbox.md) for which.

## Environment variables

Secrets and connection strings live in the environment, never in `flywheel.toml`.

| Variable | Used for | Notes |
|----------|----------|-------|
| `FLYWHEEL_PG_DSN` | Postgres DSN (primary) | `_store_factory.py:33`; wins over `DATABASE_URL`. An empty value is treated as unset so a stray `export FLYWHEEL_PG_DSN=` cannot select an empty DSN |
| `DATABASE_URL` | Postgres DSN (fallback) | `_store_factory.py:35`; used only when `FLYWHEEL_PG_DSN` is unset |
| `FLYWHEEL_DB` | SQLite store path fallback | fallback for `--db` (audit CLI, `flywheel_core/audit/_cli.py:119`); empty value treated as unset |
| `ANTHROPIC_API_KEY` | raw API-key auth for the agent | `_auth.py:29`; in `api_key` container auth mode the token is read from this name |

## What `flywheel init` scaffolds

`flywheel init` writes only a thin subset of the surface (`_render_init_policy`, `_workflow.py:2043`). It is idempotent and never overwrites a non-managed file. See [cli.md](cli.md) for the full init flag list.

Rendered:

- `[source]` — `directory` or `github` only. `github_ci`/`github_review` must be hand-edited in.
- `[paths]` — `db` and `sandbox_root`.
- `[store]` — from `--store`/`--pg-schema`.
- `[submit]` — `base = "<current branch>"` when a branch is detected, else a commented placeholder.
- Commented placeholders for `[[defaults.graders]]`, `[agent] model`, `[sandbox] setup`, and `[phase] verify`.

**Not rendered by init — must be hand-written:** `[execution]`, `[held_out]`, `[autopilot]` and `[autopilot.weights]`, every `[sandbox.*]` subtable (`exec`/`capabilities`/`network`/`env`/`limits`/`retention`/`container`), `[sandbox] preset`/`backend`/`permission_mode`, `[submit] strategy`/`protected_paths`/`remote`/`pr_base`, and `[source] failure_filter`.

The commented `[agent] model` placeholder shows one example model id; do not treat any printed id as canonical — the value is opaque and projects vary.
