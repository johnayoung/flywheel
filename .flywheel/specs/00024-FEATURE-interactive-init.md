# Feature: Interactive init with store backend selection

## Summary
Turn `flywheel init` from a fixed scaffold into a proper init process: interactive prompts (with matching flags) for store backend and work source, plus the store-selection plumbing that makes the postgres choice real. Today every CLI verb hardcodes `SqliteStore`; this feature adds a config-driven store factory so `worker`, `status`, `live`, `fw`, and the rest honor the configured backend.

## Background
`init` currently writes a fixed `flywheel.toml` template and assumes sqlite, directory source, and default paths. `PostgresStore` exists in `flywheel_core.store_postgres` but is unreachable from the CLI — there is no factory or config key to select it. The user wants init to ask the questions that matter (store, source) while staying scriptable, and wants the design left open for future choices (multi-source, worker tuning, agent settings).

## Scope

### In Scope
- Interactive prompt flow when `init` runs on a TTY; flags pre-answer individual prompts; non-TTY or `--defaults` uses today's defaults (sqlite, directory source) with no prompts.
- Store backend choice: `sqlite` (default) or `postgres`, recorded as a new `[store]` section in `flywheel.toml`.
- Postgres DSN is never written to `flywheel.toml`. It is read from `FLYWHEEL_PG_DSN`, falling back to `DATABASE_URL` (12-factor). Init prints this contract.
- Optional postgres schema name (non-sensitive): prompted during the postgres path, stored as `[store] schema`.
- Postgres validation at init: if the DSN env var is set, test-connect and report; init completes either way. If unset, complete and print exactly which var to set.
- Work source choice: `directory` (default) or `github`; the github path prompts for repo (default auto-detected from `git remote get-url origin`), label (default `flywheel`), and done action (default `comment`).
- Store factory: a single construction point that reads `WorkPolicy` and returns `SqliteStore` or `PostgresStore`, used by every store-constructing call site in `flywheel-orchestrator`, `flywheel-worktree`, and `flywheel` (TUI).
- Reconfigure flow: when `flywheel.toml` exists and init is interactive, show current settings and offer to reconfigure; rewriting touches only the answered keys and preserves everything else (graders, model, paths). Non-interactive re-runs keep today's never-touch behavior.
- Backend-aware "store missing" handling in `fw` (the existing `db_path.exists()` check is sqlite-only).

### Out of Scope
- Multiple simultaneous work sources (directory AND github). Config stays a single `[source]` table; multi-source is a future spec including any config migration.
- Prompting for agent model or path overrides; these stay as commented template entries.
- A `[worker]` config section (heartbeat, retention, poll interval stay as code constants).
- Postgres pool sizing configuration (`min_size`/`max_size` stay at code defaults).
- Provisioning the postgres database itself; schema bootstrap remains `PostgresStore`'s job on first open.
- Auto-generating any in-loop verification fixture (not triggered; see Decisions Log).

## Requirements

### Functional Requirements
1. **FR-1**: `flywheel init` on a TTY prompts for store backend and work source; each prompt has a default (sqlite, directory) selectable by pressing enter.
   - Acceptance: running init interactively and accepting defaults produces a config equivalent to today's template plus `[store] backend = "sqlite"`.
2. **FR-2**: Flags pre-answer prompts: `--store {sqlite,postgres}`, `--pg-schema NAME`, `--source {directory,github}`, `--repo OWNER/NAME`, `--label LABEL`, `--defaults` (accept all defaults, never prompt). Any flag suppresses its prompt; `--defaults` suppresses all.
   - Acceptance: `flywheel init --store postgres --defaults` in a non-TTY shell writes `[store] backend = "postgres"` without prompting.
3. **FR-3**: Non-TTY stdin with no flags behaves exactly like `--defaults` (today's behavior preserved for scripts/CI).
   - Acceptance: existing `test_init.py` expectations hold when stdin is not a TTY.
4. **FR-4**: No DSN or other credential ever appears in `flywheel.toml` or in init's stdout. The postgres path prints the env var contract: `FLYWHEEL_PG_DSN`, fallback `DATABASE_URL`.
   - Acceptance: grep of the generated file and captured output for the DSN value is empty.
5. **FR-5**: When backend is postgres and the DSN env var is set at init time, init attempts a connection and prints success or the failure reason; init exits 0 in both cases (warning on failure). When unset, init exits 0 and prints which var to set.
   - Acceptance: init against a reachable test container reports success; against a bogus DSN reports the warning and still writes config.
6. **FR-6**: A store factory in `flywheel-orchestrator` builds the store from policy: backend `sqlite` (or absent `[store]`) yields `SqliteStore(db_path)`; backend `postgres` resolves the DSN from `FLYWHEEL_PG_DSN` then `DATABASE_URL` and yields `PostgresStore(dsn, schema=policy_schema)`. All CLI verbs and the TUI construct stores through it.
   - Acceptance: with `backend = "postgres"` and a live DSN env var, `flywheel status` operates against postgres; no remaining direct `SqliteStore(` construction in command paths (verified by test or grep).
7. **FR-7**: `WorkPolicy`/`load_policy` gain `store_backend` (default `"sqlite"`) and `store_schema` (default `None`); absent `[store]` section parses to sqlite, so every existing `flywheel.toml` keeps working unchanged.
   - Acceptance: policy loader tests for absent section, each backend value, and rejection of unknown backends.
8. **FR-8**: With backend postgres and neither env var set, any store-using command fails fast with a message naming both env vars. With the postgres extra not installed, the message includes the install command (`uv add 'flywheel[postgres]'`). Init's postgres path performs the same import check and prints the install command as a warning.
   - Acceptance: unit tests asserting both messages.
9. **FR-9**: Interactive init in a repo with an existing `flywheel.toml` shows the current backend/source and asks whether to reconfigure; declining leaves the file byte-identical; accepting rewrites only the keys the prompts answered, preserving all other keys and comments-bearing sections it does not own (at minimum: `[agent]`, `[[defaults.graders]]`, unanswered `[paths]` keys).
   - Acceptance: round-trip test with a hand-tuned `flywheel.toml` containing `[agent] model` shows the key surviving a reconfigure.
10. **FR-10**: GitHub source prompts default the repo from the `origin` remote when parseable, label to `flywheel`, done action to `comment`; values are validated like the existing policy loader (repo `owner/name`, done action in `{comment, close}`).
    - Acceptance: init in a repo with a GitHub origin pre-fills the repo prompt; final config loads via `load_policy` without error.
11. **FR-11**: `fw`'s missing-store detection is backend-aware: the `db_path.exists()` check applies only to sqlite; for postgres, the equivalent check is DSN env var presence (and the existing connection error path covers the rest).
    - Acceptance: `fw` with postgres backend and no env var exits 2 with the env-var message instead of the sqlite-path message.

### Non-Functional Requirements
- **Performance**: Init's optional connection test uses a short timeout (a few seconds) so a wrong DSN does not hang init.
- **Security**: Credentials live only in env vars; never persisted to repo files or echoed to output. DSN values must not appear in error messages verbatim (host/db name OK, password never).
- **UX**: Prompts are plain stdin/stdout (no new TUI dependency); every prompt shows its default; the whole default path is two enters. Scaffold output keeps the current `created:`/`exists:` reporting.

## Behavior Specification

### Happy Path (interactive, postgres)
1. User runs `flywheel init` in a fresh repo on a TTY.
2. Init scaffolds `.flywheel/tasks/{active,archive}` and `.flywheel/.gitignore` as today.
3. Prompt: store backend `[sqlite]/postgres` — user picks postgres.
4. Prompt: postgres schema name `[none]` — user enters `flywheel_ci` or accepts none.
5. Init checks the postgres extra is importable (warns with install command if not), resolves `FLYWHEEL_PG_DSN` then `DATABASE_URL`; if set, test-connects and reports; if unset, prints "set FLYWHEEL_PG_DSN (or DATABASE_URL) before running flywheel worker".
6. Prompt: work source `[directory]/github` — user accepts directory.
7. Init writes `flywheel.toml` with `[store] backend = "postgres"` (plus `schema` if given), `[source]`, `[paths]` (sandbox_root; `db` only meaningful for sqlite), and the existing commented `[defaults.graders]`/`[agent]` blocks.
8. Init prints next steps including the env var contract.

### Error Handling
| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Postgres chosen, extra not installed (init) | Warning with `uv add 'flywheel[postgres]'`; config still written; exit 0 |
| Postgres chosen, env var set, connection fails (init) | Warning with sanitized reason; config still written; exit 0 |
| Postgres backend, no env var (runtime, any verb) | Fail fast, exit non-zero, message names `FLYWHEEL_PG_DSN` and `DATABASE_URL` |
| Postgres backend, extra not installed (runtime) | Fail fast with install command |
| Unknown `[store] backend` value in toml | `PolicyError` from `load_policy`, consistent with existing validation style |
| Invalid repo format / done action at github prompts | Re-prompt with the validation message (interactive); flag values fail with the same message |
| `--repo`/`--label` given with `--source directory` (or implied directory) | Error: flags inconsistent, exit non-zero |

### Edge Cases
| Case | Expected Behavior |
| ---- | ----------------- |
| Non-TTY stdin, no flags | Identical to `--defaults`: today's scaffold + `[store] backend = "sqlite"`, no prompts |
| Existing `flywheel.toml`, interactive, user declines reconfigure | File byte-identical; scaffold dirs still ensured; exit 0 |
| Existing `flywheel.toml` predating `[store]` section | Loads as sqlite everywhere; reconfigure adds the section without disturbing other keys |
| `DATABASE_URL` set but pointing at a non-postgres scheme | Treated as a connection failure with a clear message (init: warning; runtime: error) |
| Both `FLYWHEEL_PG_DSN` and `DATABASE_URL` set | `FLYWHEEL_PG_DSN` wins, silently |
| Origin remote absent or not GitHub-shaped | Repo prompt has no default; user must type it |
| Ctrl-C / EOF mid-prompts | No partial `flywheel.toml` written; scaffold dirs already created are fine (idempotent) |

## Technical Context

### Affected Apps
- `flywheel-orchestrator`: `_workflow.py` (`_cmd_init`, init parser, all `SqliteStore(...)` call sites), `_policy.py` (`WorkPolicy`, `load_policy`), new store factory module.
- `flywheel-worktree`: worker store construction goes through the factory.
- `flywheel`: `_cli.py` (flag passthrough), `_tui.py` (backend-aware missing-store check, store construction).
- `flywheel-core`: no changes expected (`SqliteStore`/`PostgresStore` already exist).

### Integration Points
- `flywheel_core.store_postgres.PostgresStore(dsn, schema=...)` — constructed by the factory; import deferred so the extra stays optional.
- Env vars `FLYWHEEL_PG_DSN` / `DATABASE_URL` — runtime DSN resolution, also used by init validation.
- Existing root `conftest.py` Postgres test container — reuse for factory/init integration tests.

### Relevant Existing Code
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py:1563-1598` — current `_cmd_init`; `:1526-1561` templates; `:649-675` db path resolution.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py:70-216` — `WorkPolicy`, `load_policy`, validation style to extend.
- `packages/flywheel-core/src/flywheel_core/store_postgres.py:47-57` — extra-missing ImportError message to surface.
- `packages/flywheel/src/flywheel/_tui.py:247-257` — sqlite-only missing-store check to make backend-aware.
- `packages/flywheel-orchestrator/tests/test_init.py` — idempotency expectations to preserve.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Interaction model | Interactive on TTY + flags; non-TTY = defaults | Scriptable and CI-safe while giving new users a guided flow |
| Choices surfaced | Store backend, work source | Agent model and path overrides stay template comments; less prompt noise |
| Store plumbing | In scope, this spec | A postgres option without the factory is a lie |
| Multi-source | Out of scope; single `[source]` table unchanged | Orchestrator-level merging/ID semantics deserve their own spec |
| DSN storage | Env vars only (`FLYWHEEL_PG_DSN`, fallback `DATABASE_URL`); never in toml | 12-factor; toml is committed |
| Init validation | Test-connect if var set, warn-don't-block; print contract if unset | Never blocks environments where the DB comes up later |
| PG schema name | Optional `[store] schema` key, prompted in init | Non-sensitive; enables multi-deployment isolation |
| Config shape | New `[store]` section (`backend`, `schema`); absent section = sqlite | Backward compatible; `[paths] db` stays sqlite-only |
| Re-run behavior | Interactive detect + offer reconfigure, surgical key rewrite; non-interactive never touches | Preserves hand-tuned keys; keeps idempotency for scripts |
| GitHub prompt defaults | Repo from origin remote, label `flywheel`, done action `comment` | Sensible defaults reduce typing; same validation as loader |
| Loop-path coverage | Not required | Trigger set checked: no new Status/Outcome, no schema DDL, no Grader variant, no new store-protocol `def` (factory selects existing stores), no control command. No signal tripped. |

## Open Questions
None.

## Next Steps
Run `/task 00024-FEATURE-interactive-init` to generate implementation tasks from this spec.
