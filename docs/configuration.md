# Configuration

Flywheel reads three TOML files and one environment variable; the repo needs none of them. The eight keys below are the whole configuration surface: every other former setting is deleted, a built-in default, or a verb flag ([fates table](#where-every-old-setting-went)).

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
| `store` | `"sqlite"` or a `postgres://` / `postgresql://` DSN | `"sqlite"` (`projects/<id>/flywheel.sqlite`) | the store backend; one Postgres DSN serves every project, and choosing Postgres is what permits cross-machine claims ([team-mode.md](team-mode.md)) |
| `transcript_retention_days` | int >= 1 | `90` | transcript body retention |
| `verify` | string (shell) | unset (no gate) | the standing submit gate (re-run under the merge lock before every landing), the phase-exit gate, AND the default grader of a tracker item (GitHub issue/CI/review) that carries none; `""` disables a lower layer's command |
| `protected_paths` | list of globs | `[]` | paths a finished branch may not touch (`**` crosses dirs; `*`, `?` do not); a match parks the work |
| `land` | `"merge"` or `"pr"` | `"merge"` | the submit strategy, for task work and autopilot work alike |
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

## Verb flags

Distinct features that a config key would not fit are flags on the verb that uses them:

| Flag | Verb | Selects |
|------|------|---------|
| `--source directory\|github\|github_ci\|github_review` | `fw worker` | the work source (default `directory`: the project's `tasks/`); [work-sources.md](work-sources.md) |
| `--repo OWNER/NAME` | `fw worker` (github* sources), `fw triage` (required) | the GitHub repo |
| `--label L` | `fw worker --source github`, `fw triage` | the intake label (default `flywheel`) |
| `--require-triage-receipt` | `fw worker --source github` | run only issues a triage pass marked ready (team mode) |
| `--sandbox worktree\|container` | `fw worker` | the execution backend; [container-backend.md](container-backend.md) |
| `--image IMAGE` | `fw worker --sandbox container` | the container image (required there) |
| `--agent claude-code\|codex` | `fw worker`, `fw autopilot` | the agent adapter; [agent-harness.md](agent-harness.md) |

A pool (`--concurrency > 1`) hands every flag to its members. Existing path flags (`--db`, `--tasks-dir`, `--sandbox-root`) still name paths explicitly.

## Legacy layout

A repo's `flywheel.toml` and `.flywheel/` are **not read**. When either is present, every verb prints one notice that it is ignored and can be deleted, and changes nothing in the repo tree. There is no `--policy` flag. State lives under `$FLYWHEEL_HOME/projects/<id>/`; see [cli.md](cli.md#projects-and-the-home).

## Where every old setting went

Fate: **key** (one of the eight), **flag** (above), **fixed** (a location under `projects/<id>/`), **default** (built in, `crates/flywheel-orchestrator/src/policy.rs`), **deleted** (with its feature).

| Old setting | Fate |
|-------------|------|
| `--policy FILE`, `flywheel.toml` loading | deleted |
| `[source] kind` | flag `--source` (default `directory`) |
| `[source] tasks_dir` | fixed `projects/<id>/tasks` (`--tasks-dir` overrides per verb) |
| `[source] repo`, `label` | flags `--repo`, `--label` (default `flywheel`) |
| `[source] require_triage_receipt` | flag `--require-triage-receipt` |
| `[source] done_action` | default `comment` |
| `[source] failure_filter` | default `failure` |
| `[[defaults.graders]]` | key `verify` (one command grader) |
| `[paths] db` | fixed `projects/<id>/flywheel.sqlite` (`--db` overrides) |
| `[paths] sandbox_root` (+ `@cache`, `@sibling`) | fixed `projects/<id>/worktrees` (`--sandbox-root PATH` overrides); tokens deleted |
| `[agent] model` | key `model` |
| `[agent] id` | flag `--agent` |
| `[agent] transport` | deleted (`cli` was the only value) |
| `[store] backend` | key `store` |
| `[store] schema` | deleted (Postgres is partitioned by `project_id`, schema `public`) |
| `[execution] mode` | deleted: a postgres `store` enables cross-machine claims |
| `[execution] capabilities` | default empty (a task requiring a capability is not runnable) |
| `[worker] concurrency` | key `concurrency` |
| `[worker] checkpoint_nudge_seconds` | default 300 |
| `[worker] session_pause_ceiling_seconds` | default 21600 |
| `[submit] strategy` | key `land` (`merge`/`pr`); `phase` deleted with the phase integration-branch strategy |
| `[submit] verify` | key `verify` |
| `[submit] protected_paths` | key `protected_paths` |
| `[submit] remote` | default `origin` |
| `[submit] base` | default: the checked-out branch |
| `[submit] pr_base` | default: the landing base |
| `[submit] recovery_agent_max_turns`, `_max_wall_seconds` | default 30, 900 |
| `[[submit.tiers]]` | deleted with risk-tiered landing |
| `[phase] verify` | key `verify` |
| `[held_out] root` | fixed `projects/<id>/held-out`; the gate activates when it holds registrations |
| `[autopilot] target_depth`, `interval_seconds` | default 5, 300 (`--target-depth`, `--interval` override) |
| `[autopilot] landing` | key `land` |
| `[autopilot] authoring_concurrency`, `discovery_cache_ttl_seconds` | default 3, 21600 |
| `[autopilot.weights]` | default (docs/autopilot.md) |
| `[triage] intake_label` | flag `fw triage --label` (default `flywheel`) |
| `[triage] ready_label`, `needs_detail_label` | default `flywheel:ready`, `flywheel:needs-detail` |
| `[triage] interval_seconds`, `max_per_pass` | default 300 (`--interval` overrides), uncapped |
| `[deadlines]` | default (agent iteration 3600s, rubric judge 600s, command grader 900s, docker 120s, autopilot agent 1800s) |
| `[sandbox] preset` | deleted (`fast` behavior is the default) |
| `[sandbox] backend` | flag `--sandbox` |
| `[sandbox] setup` | deleted with the setup hook |
| `[sandbox] permission_mode` | default `bypassPermissions` |
| `[sandbox.exec]`, `[sandbox.capabilities]`, `[sandbox.network]`, `[sandbox.env]`, `[sandbox.limits]` | default (no exec isolation, all skills, MCP from the repo's `.mcp.json`, network allow, inherit the environment, 500 turns / 1 retry / 300s lease) |
| `[sandbox.retention]` | default `on_done = destroy`, `on_failure = park` (`--worktree-retention-days` sweeps) |
| `[sandbox.container] image` | flag `--image` |
| `[sandbox.container]` other keys | default (auth detected: `oauth` via `CLAUDE_CODE_OAUTH_TOKEN`, else a `claude login` session, else `ANTHROPIC_API_KEY`; model = `model`; exec timeout 1800s) |
| `fw init --store/--pg-schema/--source/--repo/--label/--skills/--defaults/--agent` | deleted (init writes nothing into the repo) |

## Environment variables

Secrets live in the environment or your user `config.toml`, never in a committed file.

| Variable | Used for | Notes |
|----------|----------|-------|
| `FLYWHEEL_HOME` | the flywheel home | default `~/.flywheel` |
| `FLYWHEEL_PG_DSN` | Postgres DSN | sets the effective `store` (source `env`, above personal/repo/user); an empty value is unset |
| `FLYWHEEL_DB` | SQLite store path fallback | the audit stream's `--db` fallback outside the shell's resolution (`crates/flywheel-core/src/audit.rs`) |
| `ANTHROPIC_API_KEY` | raw API-key agent auth | `crates/flywheel-container/src/auth.rs` |
| `CLAUDE_CODE_OAUTH_TOKEN` | container backend auth | preferred by the detected auth mode |
| `CLAUDE_CONFIG_DIR` | where `fw init` installs the fw-* skills | default `~/.claude` |
| `PGSSLMODE`, `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY` | Postgres TLS defaults | used only when the DSN omits the keyword |

Postgres TLS follows libpq: `sslmode` defaults to `prefer`; `verify-ca`/`verify-full` need `sslrootcert` (a PEM file, `~/.postgresql/root.crt`, or `system`). Example: `postgresql://u:p@db.example.com/app?sslmode=verify-full&sslrootcert=system`.

## What `fw init` does

Writes nothing into the repo tree (beyond the `.git/config` project id): resolves the project, creates `$FLYWHEEL_HOME/config.toml` (every key commented) when missing, installs the fw-* skills under `$CLAUDE_CONFIG_DIR/skills` (`--no-skills` skips), runs the Postgres preflight when `store` is a DSN (`--provision`, `--allow-unverified`), detects a verify command (`Cargo.toml` -> `cargo test`, `package.json` test script -> `npm test`, pytest config -> `pytest`, `go.mod` -> `go test ./...`, Makefile `test:` -> `make test`), and prints a suggested `.flywheel.toml` to commit and `fw trust`.
