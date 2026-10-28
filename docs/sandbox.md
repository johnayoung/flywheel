# Sandbox (the agent execution environment)

The sandbox is where and how the agent runs a single task: the working directory it gets, the tools and MCP surface it can reach, the network it can hit, the credentials it carries, the budget it may spend, and what happens to its workspace afterward. This page owns the `[sandbox.*]` config reference; [configuration.md](configuration.md) links here.

## The sandbox-as-deploy model

**A sandbox is a deploy of a task.** Flywheel already behaves like a 12-factor PaaS — a fresh process, a provisioned working directory, attached backing services, disposal at the end — but historically hardcoded what 12-factor externalizes. The `[sandbox.*]` config lifts those decisions into config without changing the `prepare_sandbox -> invoke -> submit -> teardown` lifecycle (the spine; see [strategy.md](strategy.md)).

**The flexibility spectrum is `fast -> balanced -> hardened`**: startup-fast and barebones with full capability, through to locked-down and least-privilege. The flywheel-specific twist versus incumbents (Codex, OpenHands): **there is no human-approval axis.** Graders plus the `[submit]` landing trust ladder ([strategy.md](strategy.md)) do the job an approval prompt would. Every preset keeps `permission_mode = "bypassPermissions"` — a worker loop has no human approver, and a non-bypass mode would freeze the agent the first time it writes a file. Hardening is least-privilege tool *allowlisting*, never prompting.

Two things that look like sandbox axes but deliberately are not: **autonomy/approval** (replaced by graders + landing) and **concurrency** (worker count, leases, merge flock — set at the worker layer, not here).

## The eight aspects

A deploy decomposes into eight named aspects, each with a config home.

| # | Aspect | Config home |
| --- | --- | --- |
| 1 | Isolation (backend + permission mode) | `[sandbox] backend`, `[sandbox.exec]` |
| 2 | Capabilities (skills/tools/MCP) | `[sandbox.capabilities]`, `[sandbox.capabilities.mcp]` |
| 3 | Network | `[sandbox.network]` |
| 4 | Landing | `[submit]` (top-level, not a sandbox property) |
| 5 | Credentials/secrets | `[sandbox.env]` (names) + the operator environment (values) |
| 6 | Resource/budget limits | `[sandbox.limits]` |
| 7 | Provisioning/setup | `[sandbox] setup` (pre-existing `WorkPolicy` field) |
| 8 | Teardown/retention | `[sandbox.retention]` |

Landing (aspect 4) is owned by `[submit]`, not `[sandbox.*]`. Provisioning (aspect 7) is the pre-existing `[sandbox] setup` command on `WorkPolicy`, not part of the resolved `SandboxPolicy`.

## Presets

Three code-owned presets (`sandbox_preset`, `crates/flywheel-orchestrator/src/policy.rs`). A preset is the *baseline*; sparse per-key overrides merge on top.

| Preset | Restricts | vs `fast` |
| --- | --- | --- |
| `fast` (default) | nothing | today's hardcoded behavior verbatim — a no-op until you opt up |
| `balanced` | `mcp_strict = true`, no servers | full coding capability, but the agent stops loading the operator's personal MCP surface (Gmail/Calendar/Drive/playwright) |
| `hardened` | minimal tool allowlist + project-only settings + bash exec isolation | least-privilege coding set, no MCP, drops user-level `~/.claude` config, bash isolation on |

`hardened`'s allowlist is `["Bash", "Edit", "Glob", "Grep", "Read", "Write"]` with `setting_sources = ["project"]`, `mcp_strict = true`, and `[sandbox.exec] enabled = true`.

**Presets restrict only the capability dimension** (skills/tools/MCP/settings/exec). They leave `env`, `limits`, `network`, and `retention` at their `fast` values — a preset never advertises a guarantee it cannot enforce. To tighten those, set their keys explicitly. Every preset keeps `permission_mode = "bypassPermissions"`.

## Resolution semantics

Resolution follows build -> release -> run (`optional_sandbox_policy`, `crates/flywheel-orchestrator/src/policy.rs`):

1. **Preset baseline** — the named preset's values. An unknown preset name fails fast (`preset '<x>' is not available`).
2. **Sparse per-key repo overrides** — keys present in `[sandbox.*]` tables override the preset; absent keys keep the preset value. Unknown *keys* under any `[sandbox.*]` table are ignored (forward-compat).
3. **CLI flags** last.
4. **Freeze** into a single immutable `SandboxPolicy` on `WorkPolicy.sandbox`.

**List-replace, never append.** A declared list (`allow_hosts`, `allowed_tools`, ...) *replaces* the preset's list wholesale; it does not extend it.

**`backend = "container"` requires `[sandbox.container] image`** — omitting it is a `PolicyError` at load.

## `[sandbox]` (top-level)

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `preset` | str | `"fast"` | baseline preset (`fast`/`balanced`/`hardened`) | load-time resolution |
| `backend` | str | `"worktree"` | `worktree` (in-process) or `container` (Docker) | worker backend select |
| `permission_mode` | str | `"bypassPermissions"` | agent permission mode: `bypassPermissions` -> `auto` (claude `--permission-mode bypassPermissions`), `plan` -> `plan`, anything else -> `supervised` | agent CLI flags |
| `setup` | str | (unset) | provisioning command (pre-existing `WorkPolicy.sandbox_setup`, not in `SandboxPolicy`) | worker |

### `[sandbox.exec]`

Bash command isolation, written into the `sandbox` key of the claude CLI's `--settings` JSON via the `sandbox_exec` adapter option (`SandboxExec`, `crates/flywheel-orchestrator/src/policy.rs`).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `enabled` | bool | `false` | enable the claude bash sandbox (on under `hardened`) | host agent CLI settings |
| `auto_allow` | bool | `true` | auto-allow sandboxed bash | host agent CLI settings |

### `[sandbox.capabilities]` and `[sandbox.capabilities.mcp]`

The skills/tools/MCP surface (`SandboxCapabilities`), translated into claude CLI flags by the claude-code adapter (`build_cli_plan`, `crates/flywheel-agents/src/claude_code/cli.rs`).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `skills` | str \| list | `"all"` | `"all"`/`"none"` or explicit skill names (`Skill` / `Skill(name)` allow rules) | host agent CLI flags |
| `allowed_tools` | list | `[]` | tool allowlist (list-replace), `--allowedTools` | host agent CLI flags |
| `denied_tools` | list | `[]` | tool denylist (list-replace), `--disallowedTools` | host agent CLI flags |
| `setting_sources` | list \| unset | unset | settings origins (`hardened` = `["project"]`); unset lets the adapter derive `user,project` when skills are enabled | host agent CLI flags |
| `mcp_servers` | list | `[]` | MCP server names (list-replace), each looked up in the sandbox's `.mcp.json` and passed inline via `--mcp-config`; an undeclared name fails the run | enforced |
| `mcp_strict` | bool | `false` | only load declared servers, `--strict-mcp-config` (on under `balanced`/`hardened`) | host agent CLI flags |

**Capabilities are enforced only on the host (worktree) path.** The container backend builds its agent configuration from model and permission policy alone, so these primitives do not reach the in-container agent — see the v1 gap below.

### `[sandbox.network]`

Network policy (`SandboxNetwork`).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `policy` | str | `"allow"` | `"allow"` (full egress) or `"deny"` | container backend only |
| `allow_hosts` | list | `[]` | egress allowlist under `deny` (list-replace) | container backend only |
| `allow_unix_sockets` | list | `[]` | forwarded unix sockets | container backend only |

**Network is advisory under the worktree backend (enforces nothing) and has real teeth only under `backend = "container"`.** Under the container backend, `policy = "deny"` with no `allow_hosts` attaches an internal Docker network (no gateway, no egress); `deny` with `allow_hosts` requires an operator-provisioned egress proxy. See [container-backend.md](container-backend.md) for the attachment-vs-proxy model.

### `[sandbox.env]`

Credential/secret name allowlist — the declaration half. Values live in the operator environment, never the policy file (`SandboxEnv`, resolved by `resolve_agent_env` in `crates/flywheel-orchestrator/src/policy.rs`).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `pass` | list | `[]` | names passed through from the operator env (only those present in the process environment) | host agent env |
| `set` | table | `{}` | inline literal values (win over `pass`) | host agent env |
| `inherit_home` | bool | `true` | inherit `HOME`/ambient env (`fast` inherits the full env) | host agent env |

**v1 gap: `pass`/`set` do not reach the in-container agent under `backend = "container"`.** They resolve into the host agent environment, which the container path does not use: the policy bridge builds the container config with an empty `env` (`container_config_from_policy`, `crates/flywheel-container/src/config.rs`). This is a documented limitation, not a silent drop — see [container-backend.md](container-backend.md). On the worktree backend, a true default-deny scrub is also not possible: `LocalHost` merges the plan's extra environment over the parent process environment (`crates/flywheel-agents/src/hosts.rs`), so `pass`/`set` only add or override, never remove.

### `[sandbox.limits]`

Resource/budget ceilings (`SandboxLimits`).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `max_cost_usd` | float | `0.0` | per-run cumulative cost ceiling | `HarnessConfig` (see below) |
| `max_tokens` | int | `0` | per-run cumulative token ceiling | `HarnessConfig` (see below) |
| `wall_clock_seconds` | int | `0` | per-run wall-clock ceiling | `HarnessConfig` (see below) |
| `max_turns` | int | `500` | (carried, not yet wired) mirrors today's hardcoded value | CLI/default path |
| `max_retries` | int | `1` | (carried, not yet wired) mirrors today's hardcoded value | CLI/default path |
| `lease_seconds` | int | `300` | (carried, not yet wired) mirrors today's hardcoded value | CLI/default path |

**Only `max_cost_usd`, `max_tokens`, and `wall_clock_seconds` are enforced** (lifted into `HarnessConfig`). `max_turns`/`max_retries`/`lease_seconds` are carried at their `fast` values to match today's behavior but keep their existing CLI/default path — they are not lifted into ceiling enforcement. `0` means unenforced for every ceiling.

### `[sandbox.retention]`

Teardown/disposal policy (`SandboxRetention`). Threaded through the worktree submitter.

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `on_done` | str | `"destroy"` | `destroy` or `preserve` a DONE worktree after merge | worktree submitter |
| `on_failure` | str | `"park"` | `park` (keep for forensics) or `destroy` a non-DONE worktree | worktree submitter |
| `sweep_days` | int | `7` | parked-worktree sweep age | CLI `--worktree-retention-days` (not policy-threaded) |

`sweep_days` is set via CLI, not threaded from policy.

### `[sandbox.container]`

Container backend config (`SandboxContainer`). Inert unless `backend = "container"`. Full operator quickstart in [container-backend.md](container-backend.md).

| Key | Type | Default | Controls | Enforced where |
| --- | --- | --- | --- | --- |
| `image` | str | `""` | agent image — **required** when `backend = "container"` | load-time `PolicyError` |
| `model` | str | `""` | agent model; falls back to the worker's resolved model | container strategy |
| `auth` | str | `"oauth"` | auth mode: `oauth` / `session` / `api_key` / `none` | container strategy |
| `auth_env` | str | `""` | override the env var name the token is read from | container strategy |
| `exec_timeout` | int | `1800` | `docker exec` watchdog seconds; `0` = unbounded (not recommended) | container strategy |
| `egress_network` | str | `""` | operator egress-proxy network for `allow_hosts` | container strategy |

## Budget ceilings

`max_cost_usd`, `max_tokens`, and `wall_clock_seconds` are enforced in the harness iteration loop via `HarnessConfig` (`crates/flywheel-core/src/harness/mod.rs`; breach handling in `finalize_budget_breach`, `harness/attempt.rs`):

- **Per-run cumulative.** Cost sums `total_cost_usd` across all attempts; tokens sum `Attempt.total_tokens`; wall-clock measures elapsed from the earliest attempt's `started_at`.
- **Checked after the per-iteration rollup, before grading** — a breach pre-empts the grade, so a run that blew its budget never gets to pass.
- **Terminal `Status.FAILED`, non-retryable** (the ABORT path, not the retryable THRASH path).
- **Order: cost, then tokens, then wall-clock.** The first breach ends the run.
- Each breach emits a `harness.budget_ceiling_breached` event with `payload["ceiling"]` in `{"cost_usd", "tokens", "wall_clock_seconds"}`, distinguishing a budget kill from an agent error in audit (see [data-taxonomy.md](data-taxonomy.md)).
- **`0` is unenforced** (the `fast` default) — byte-identical to today.

Container runs feed this path too: the folded `CompletedRun.usage` reaches `IterationResult.usage` on every backend, so `max_tokens` still trips.

## Backend

`backend = "worktree"` (the default) runs the agent CLI as a local process (flywheel-agents `LocalHost`) against a git worktree; `backend = "container"` runs the agent as its own CLI inside a Docker image against the bind-mounted worktree. The worker selects the backend automatically — `flywheel worker` (or `fw worker`), no driver script — once `[sandbox] backend = "container"` is set (see [cli.md](cli.md)). The container backend, its image contract, auth modes, network teeth, and v1 limitations are documented in [container-backend.md](container-backend.md).
