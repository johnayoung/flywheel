# 00036 — The Sandbox-as-Deploy model

Status: design doc (pre-`fw-spec`). No implementation. This document defines the
conceptual model, the named config schema, and an honest enforcement map for
making the agent execution environment a first-class, configurable surface. It
is the input to `fw-spec` (which lowers the open questions into ungameable
success criteria) and then `fw-plan` (which compiles tasks).

## Why

Flywheel runs every task in an isolated git worktree, lets the agent run with the
operator's full ambient power (env, `$HOME`, gcloud/gh/Anthropic creds, MCP,
skills), and lands only verified work back through a `SubmitStrategy`. That
execution *environment* is not currently a designed thing. It is one of three:

- **Hardcoded** — `permission_mode="bypassPermissions"`, `skills="all"`,
  worktree-only, at every `ClaudeAgentOptions` site.
- **Accidental** — the agent inherits the operator's entire `os.environ` plus
  `$HOME`; project/user MCP servers load *only* as a side effect of `skills="all"`
  defaulting `setting_sources=["user","project"]` in the SDK.
- **Scattered** — resource limits live as CLI flags; config is split across
  `[sandbox]` / `[submit]` / `[paths]` / `[agent]`.

The goal: make this a **flexibility spectrum** from *startup-fast* (barebones
isolation, full capability, rapid completion) to *hardened* (locked-down,
security-conscious). This matches how incumbents structure agent sandboxing —
Codex (`sandbox_mode` × `approval_policy` × network, plus named permission
profiles), OpenHands (pluggable runtime tiers Local → Docker → microVM). The
flywheel-specific twist: there is **no human-approval axis**; graders + the
Landing trust ladder already do that job.

## 1. Thesis: a sandbox is a *deploy* of a task

Flywheel already behaves like a 12-factor PaaS — fresh process, provisioned
working dir, attached backing services, disposal — it just hardcodes what
12-factor externalizes. The work is to lift those decisions into config, the way
`[submit]` and `[sandbox] setup` already were, and to **name the parts** (no
umbrella brand; the shared prefix is the literal word `sandbox`, already in the
schema).

One spine, eight ribs. Spine = the existing lifecycle: `prepare_sandbox`
(provision) → invoker builds `ClaudeAgentOptions` (run) → `submit` (land) →
teardown. Today the middle step is a constant; the eight aspects are the knobs it
should read.

### 12-factor mapping

| Factor | Owns | Gap it closes |
| --- | --- | --- |
| **III Config** | Capabilities, Network, Limits, declaration half of Credentials | Lifts `skills="all"` and CLI-only limits into `flywheel.toml`; secrets named in config, valued from env |
| **IV Backing services** | MCP servers, model endpoint, store, network allow-hosts — attached by reference | MCP stops leaking in accidentally; becomes explicit named attachment like `store.backend` |
| **V Build / release / run** | The preset mechanism | preset (build, code-owned) + repo overrides (release, frozen at load) + invoker reads it (run); release is immutable |
| **VI Processes / stateless** | Isolation backend, Provisioning | Adds a backend dimension to the worktree provider; sandbox holds no authoritative state (store + git refs do) |
| **IX Disposability** | Teardown/retention, wall-clock limit | Makes ephemeral-vs-preserve a dial; adds an enforced wall-clock ceiling so a hung run disposes itself |
| **X Dev/prod parity** | The preset spectrum itself | `fast` = dev-loop deploy, `hardened` = prod/CI deploy; one-line `preset =` change, not a fork |
| **XI Logs as streams** | Forensics, cost capture | Already satisfied (telemetry JSONL, `total_cost_usd` captured); budget aspect closes the loop by *enforcing* off the same stream |

**Landing (`[submit]`) is deliberately not derived from a factor.** It is
flywheel's structural substitute for the human-approval axis incumbents bake in
(graders + protected-paths + submit-time rebase-and-reverify replace "ask me
before X"). It stays its own top-level section; presets may *reference* it but
never absorb it.

Factors not load-bearing here: **I Codebase** (each sandbox is a deploy of the
same codebase at a base commit — framing only), **II Dependencies** (the `setup`
command is the explicit dependency declaration — already exists), **VII Port
binding** (N/A until a backend exposes preview services), **VIII Concurrency**
(worker/lease layer, a separate axis — see below), **XII Admin processes**
(`init`/`--provision`/retention sweep already run as one-offs).

## 2. The eight named aspects (→ config keys)

| # | Aspect | Config home | Current state |
| --- | --- | --- | --- |
| 1 | **Isolation** (backend + permission mode) | `[sandbox] backend`, `[sandbox.exec]` | Hardcoded: worktree-only, `bypassPermissions`. Seam already backend-agnostic (`SandboxProvider` returns a `Path`). |
| 2 | **Capabilities** (skills/tools/MCP) | `[sandbox.capabilities]`, `[sandbox.capabilities.mcp]` | Accidental: `skills="all"` is the only reason MCP/settings load. |
| 3 | **Network** | `[sandbox.network]` | Absent; unrestricted. |
| 4 | **Landing** | `[submit]` (unchanged) | Mature; the trust ladder. |
| 5 | **Credentials/secrets** | `[sandbox.env]` (names) + environment (values) | Accidental/leaky: full `os.environ` + `$HOME` inherited. |
| 6 | **Resource/budget limits** | `[sandbox.limits]` | Partial, CLI-only; cost captured but never enforced; no token/wall-clock ceiling. |
| 7 | **Provisioning/setup** | `[sandbox] setup` (unchanged shape) | Already first-class. |
| 8 | **Teardown/retention** | `[sandbox.retention]` | Built but hardcoded (DONE→destroy, fail→park, 7d sweep). |

Two things that *look* like axes but are not, stated explicitly so the model is
unambiguous:

- **Autonomy / approval** — replaced by graders + Landing. Do not model
  separately.
- **Concurrency** — worker count, leases, the merge flock. Aggregate blast
  radius (how many sandboxes are loose at once), set at the worker/lease layer,
  not a per-task sandbox property.

## 3. Config schema — sub-tables under `[sandbox]`

**Decision: nest under `[sandbox]`, not flat top-level sections.** `[sandbox]`
already means "the agent execution environment" (`setup` lives there); six of
eight aspects are properties of it. Flat `[network]`/`[limits]` would scatter one
concept and collide with generic words. `[submit]` stays top-level — landing is a
*peer* that acts after the sandbox is gone, not a property of it. Forward-compat
is free: the existing "unknown keys ignored" rule (`_policy.py`) means every
current `flywheel.toml` resolves to today's behavior.

```toml
[sandbox]
setup   = "uv sync"          # existing, unchanged
preset  = "fast"             # "fast" | "balanced" | "hardened"  (default fast = today)
backend = "worktree"         # isolation backend (only impl today; reserves container/plain-dir)

[sandbox.exec]               # maps to SDK ClaudeAgentOptions.sandbox (bash command isolation)
enabled    = false
auto_allow = true

[sandbox.capabilities]
skills          = "all"               # "all" | [str] | "none"
allowed_tools   = []                  # -> allowed_tools
denied_tools    = []                  # -> disallowed_tools
setting_sources = ["user", "project"] # the thing skills="all" silently forces today; now explicit

[sandbox.capabilities.mcp]
servers = []                 # named servers to attach (factor IV)
strict  = false              # -> --strict-mcp-config: ONLY named servers

[sandbox.network]
policy             = "allow" # "allow" | "deny"
allow_hosts        = []      # host allowlist when deny
allow_unix_sockets = []      # e.g. ssh-agent socket

[sandbox.env]                # factor III: names only; values from environment
pass         = ["ANTHROPIC_API_KEY"]
set          = { TERM = "dumb" }   # only inline NON-secret literals allowed
inherit_home = false

[sandbox.limits]
max_turns          = 500
max_retries        = 1
lease_seconds      = 300
wall_clock_seconds = 0       # 0 = unlimited (today); >0 = hard timeout (new)
max_cost_usd       = 0.0     # 0 = unenforced (today); >0 = ceiling off total_cost_usd
max_tokens         = 0       # 0 = unenforced

[sandbox.retention]
on_done    = "destroy"       # "destroy" | "preserve"
on_failure = "park"          # "park" | "destroy"
sweep_days = 7
```

Every key defaults to today's behavior. `WorkPolicy` (frozen dataclass) gains
**one** nested `sandbox: SandboxPolicy` field (holding frozen sub-dataclasses),
not twenty flat ones, so override composition stays localized. Validators mirror
the existing `_optional_*` strict/forward-compat pattern (empty strings rejected,
typos fail fast, unknown keys ignored).

## 4. Preset mechanism (fast → hardened)

Named presets are **code-owned frozen `SandboxPolicy` constants** (versioned,
testable; idiomatic to flywheel's registry pattern — `SOURCES`,
`SUBMIT_STRATEGIES`, store factory). Repo sub-tables are a **sparse per-key
override layer** merged onto the chosen preset.

Resolution (build → release → run, factor V): preset baseline → per-key overrides
(an absent key inside a present table keeps the preset value) → CLI flags last
(preserves the existing "CLI wins" contract) → freeze. **List-replace** semantics
(a declared `allow_hosts` replaces the preset's, does not append) — matches how
`protected_paths` already behaves and avoids "why is this host allowed?"
archaeology.

| Aspect | `fast` (= today) | `balanced` | `hardened` |
| --- | --- | --- | --- |
| Isolation | worktree, exec off, `bypassPermissions` | worktree, exec on | worktree (container when impl'd), exec on, stricter perms |
| Capabilities | `skills=all`, no allow/deny | `skills=all`, curated `denied_tools` | `allowed_tools` allowlist, `mcp.strict=true` |
| Network | allow | allow | deny, empty allow_hosts |
| Credentials | **full passthrough (today)** | default-deny + declared `pass` | default-deny, no `$HOME` |
| Limits | turns=500, no ceilings | wall-clock + generous cost cap | tight cost/token/wall-clock |
| Retention | done→destroy, fail→park, 7d | same | same, shorter sweep |
| Landing | not overridden | not overridden | not overridden |

Opinionated calls: (a) **`fast` ≡ today exactly** so adopting the feature is a
no-op until someone opts up; (b) **no preset overrides `[submit]`** — landing is
the repo's trust decision, orthogonal to the isolation tier; (c) the
capability-preserving env default is the most consequential call — see §7.1.

Override example:

```toml
[sandbox]
preset = "hardened"          # lock everything down ...
[sandbox.network]
allow_hosts = ["api.github.com", "pypi.org"]   # ... but punch two holes
[sandbox.limits]
max_cost_usd = 5.0           # raise the hardened ceiling for this repo
```

## 5. Factor-III secrets model

Config **declares which env vars by NAME**; values come from the operator's
environment at run time; undeclared = denied. This generalizes a precedent the
codebase already endorses — `FLYWHEEL_PG_DSN` / `DATABASE_URL` /
`ANTHROPIC_API_KEY` resolve from `os.environ`, explicitly "never in the policy
file" (`_store_factory.py`, `_workflow.py:2168`).

- **Declaration:** `[sandbox.env] pass = ["NAME"]` is a name allowlist — the file
  holds names only, safe to commit. `[sandbox.env] set` holds only literal
  non-secret values (`TERM`, `LANG`, a feature flag); the validator warns (not
  errors) on values matching common secret shapes.
- **Resolution (run time):** at invoker construction,
  `env = {n: os.environ[n] for n in pass if n in os.environ} | set`. A
  declared-but-absent var is dropped with a one-line diagnostic, never
  blank-substituted (mirrors `resolve_postgres_dsn`'s "empty counts as unset" so a
  typo cannot smuggle a blank value).
- **flywheel's own backing-service creds stay environment-attached** (factor IV)
  and are *not* re-declared in `[sandbox.env]` — they are consumed by flywheel
  itself (store factory, SDK auth), not forwarded into the agent. Clean boundary:
  flywheel's creds vs. the agent's forwarded env.
- **Logs (factor XI):** every `pass` name seeds the existing `EnvValueRedactor`
  (`redaction.py:221`, already seeded with `ANTHROPIC_API_KEY`) so any forwarded
  secret the agent echoes is scrubbed from the transcript.

## 6. Enforcement / threading map (so the design is honest)

Every knob follows the proven **model spine**: `flywheel.toml` → `WorkPolicy` →
`orchestrate` → `run_task_object` → `_make_claude_code_invoke` →
`ClaudeAgentOptions` (`workflow.py:447-456`). Knobs must reach the option sites as
**plain primitives** (str / tuple / dict) — never SDK types — to preserve the
optional-SDK boundary (`import flywheel_core` must work without the extra).

- **SDK-existing (just new `WorkPolicy` fields + pass-through):**
  permission_mode, allowed_tools, disallowed_tools, skills, mcp_servers,
  setting_sources, env, max_turns. Set only at the 4 existing `ClaudeAgentOptions`
  sites (`workflow.py:449`, `grader_rubric.py:368`,
  `recovery_summarizer.py:291`, `examples/hello`).
- **NEW-harness (cost / token / wall-clock ceilings):** hook in the iteration
  loop immediately after the existing per-iteration rollup
  (`harness.py:3184-3198`), not a wrapper around invoke (the loop already has the
  accumulated attempt totals; a wrapper sees only one iteration). A breach reuses
  the cap-reached shape (`_handle_loop_guard_thrash` `harness.py:2068`) plus a
  distinct `harness.ceiling_breach` telemetry event `{kind, limit, observed}`. New
  fields land on `HarnessConfig` (`harness.py:311`) — never on `Task`/`Lifecycle`
  (purity).
- **NEW-seam (Isolation pluggability + Teardown):** extend the provider return
  from a bare `Path` to a frozen `SandboxHandle` (`path`, `env_contribution`,
  `permission_mode`/capability hints, optional `invoke_wrapper`), back-compat by
  adapting a bare `Path`. Add an optional `teardown()` to `SubmitStrategy`, called
  by `orchestrate` after `submit()`, carrying retention mode (formalizing the
  park/discard logic currently buried in `submit`). A container backend is a
  **new `SubmitStrategy`** registered in the worktree/orchestrator layer (or a
  sibling `flywheel-container` package) — it never imports `claude_agent_sdk` and
  never touches core purity.
- **NEW-provider (Network, true env allowlist):** cannot be enforced in-process.
  With worktree/plain-dir backends, `network.policy="deny"` is **advisory only**;
  real enforcement requires a container backend's network namespace. The schema
  ships, but the doc must not imply the harness can firewall.

Purity guardrails: all config dataclasses live in
`flywheel-orchestrator/_policy.py` (above the core line, already non-pure). Never
pass `ClaudeAgentOptions` or SDK types through `HarnessConfig` / `orchestrate` /
`run_task_object` signatures (would force an SDK import into modules
`flywheel_core/__init__` imports). Secret values never enter `WorkPolicy` /
`Task` / telemetry / the SDK message stream.

## 7. Resolved positions and open questions

### Resolved (opinionated)

1. **`fast` preserves today's full env passthrough — the single most
   consequential call.** The ambient-capability behavior (gcloud/gh/MCP "just
   work") is the thing this whole effort exists to celebrate, so `fast` must equal
   today exactly. Default-deny secret scoping is the **`balanced`/`hardened`**
   behavior, not the baseline. (This diverges from a "secure-by-default" reading
   in favor of back-compat and the valued capability.)
2. Presets are **code-owned constants**, not a forkable `presets.toml`.
3. Container backend is **reserved** (named follow-on), not in this design's
   scope; the `SandboxHandle` seam is shaped so it slots in later.
4. Network ships as **schema + advisory** under worktree; real teeth gated behind
   a non-worktree backend.

### Open (for `fw-spec` to interview into gradeable criteria, or accept)

1. **Budget accounting granularity & retry semantics** (biggest open decision):
   recommend per-run-cumulative + terminal `FAILED` (non-retryable) for
   cost/wall-clock, vs per-attempt + retryable for tokens. A cost ceiling that
   routes into the retry arm would immediately re-breach if the budget is
   per-run.
2. Does `permission_mode` default stay `bypassPermissions`? (Recommend yes, to
   preserve behavior; opt in to stricter.)
3. `mcp.servers` resolution source — SDK discovery via `setting_sources` only,
   or a future `[sandbox.mcp.<name>]` table (which reintroduces "where do MCP
   secrets live?" → must route through `[sandbox.env]`).
4. Container backend home — `flywheel-worktree` vs a new `flywheel-container`
   workspace package (dependency-arrow-wise, a sibling consumer of the
   orchestrator is cleaner than overloading the git-worktree package).

## 8. Acceptance (this is a design doc; "done" = the doc)

The doc is complete when it:

- Names all **8 aspects** as concrete config keys, each with current-state and a
  default that equals today.
- Maps each to its **12-factor** principle.
- Presents the **full TOML schema** with the back-compat guarantee: a current
  `flywheel.toml` resolves to byte-identical `fast` behavior (confirmable by
  walking each default against the verified current value).
- Defines **3 presets** plus the override-composition grammar.
- States the **factor-III secrets model** with the env-name-allowlist mechanic.
- Includes the **honest enforcement map** distinguishing SDK-existing /
  new-harness / new-seam / new-provider, with the network "advisory-only-under-
  worktree" caveat called out.
- Records the **open questions**, each marked gradeable-later or accepted.

Follow-on (not this task): `fw-spec` to interview the open questions into
ungameable success criteria, then `fw-plan` to compile the implementation tasks.

## 9. Implementation roadmap

This design is too broad for one spec. It decomposes into **seven dependency-
ordered increments**, each its own `fw-spec` → `fw-plan` session and each
independently shippable. The keystone is the foundation; everything else fans out
from it.

```
              ┌─ B Capabilities ──┐
A Foundation ─┼─ C Secrets ───────┼─ (ship in any order after A)
 (keystone)   ├─ D Budget ────────┤
              └─ E Retention ─────┘
                      │
                      └─ F Isolation seam ── G Container backend + network teeth
```

| # | Increment | Scope | Mechanism class | Depends on | Behavior change? |
| --- | --- | --- | --- | --- | --- |
| **A** | **Sandbox config foundation + presets** | `SandboxPolicy` nested dataclass, `[sandbox.*]` TOML parsing + `_optional_*` validators, the 3 code-owned preset constants, override-composition, threading the resolved policy through the spine as plain primitives. Wires nothing to the SDK yet — proves the surface exists and resolves. | Config | — | **No** (all defaults = `fast` = today) |
| **B** | **Capabilities + permission surface** | Wire `skills` / `allowed_tools` / `disallowed_tools` / `setting_sources` / `mcp` / `permission_mode` into the 4 `ClaudeAgentOptions` sites. De-accidentalize MCP/settings. Ship `[sandbox.network]` schema as **advisory** (no teeth yet). | SDK-existing plumbing | A | No at `fast` |
| **C** | **Credentials / secrets scoping** | `[sandbox.env]` name-allowlist resolver, default-deny `env=` construction, `EnvValueRedactor` seeding. | SDK-existing (`env`) + resolver | A | Only at `balanced`/`hardened` |
| **D** | **Budget ceilings** | cost / token / wall-clock ceilings in the harness loop; lift turns/retries/lease into policy. | NEW-harness | A | No until a ceiling is set |
| **E** | **Teardown / retention** | optional `teardown()` hook on `SubmitStrategy`, `orchestrate` calls it, worker reads `[sandbox.retention]`. | NEW-seam (small) | A | No at defaults |
| **F** | **Isolation pluggability seam** | extend provider return to `SandboxHandle` (path + env_contribution + permission hints + optional `invoke_wrapper`), back-compat `Path` adapter. | NEW-seam (architectural) | A (E co-lands cleanly) | No |
| **G** | **Container backend + real network enforcement** | new `SubmitStrategy` (own package), netns/firewall for `[sandbox.network]` teeth. | NEW-provider (major) | F | Opt-in backend |

### Recommended order

1. **A first, alone.** It is the keystone and ships with zero behavior change —
   the golden acceptance test is "a current `flywheel.toml` produces a
   byte-identical run." Low risk, unblocks all six others.
2. **Then the MVP vertical slice: B + C.** Together these make the spectrum
   *demonstrable* — flip `preset = "hardened"` and visibly get a tool-allowlisted,
   MCP-strict, env-scoped run versus `fast`'s wide-open one. This is the proof the
   whole effort is real, and the first thing to demo.
3. **D and E** in parallel with / after B+C — independent, self-contained
   (D lives in the harness, E in the seam+worker).
4. **F**, then **G** — the architectural + future major effort, gated behind F.

Each increment updates `flywheel init`'s commented `[sandbox.*]` block and the
docs for its own keys (cross-cutting, no separate workstream).

### Decision gates (resolve in each increment's `fw-spec`, before coding)

- **B:** keep `permission_mode=bypassPermissions` as the default? (recommend yes)
- **C:** confirm `fast` = full passthrough, default-deny only at
  `balanced`/`hardened` (resolved in §7.1).
- **D:** per-run-cumulative + terminal `FAILED` vs per-attempt + retryable
  (§7-open-1) — must settle before the ceiling code.
- **F/G:** container backend home — `flywheel-worktree` vs new
  `flywheel-container` package (§7-open-4).

## Anchor files (references only — no edits)

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `WorkPolicy` dataclass + `_optional_*` validators (add nested `SandboxPolicy`,
  preset resolution).
- `packages/flywheel-core/src/flywheel_core/workflow.py:447-456` — the
  `ClaudeAgentOptions` site and `_make_claude_code_invoke`; the spine every knob
  threads through.
- `packages/flywheel-core/src/flywheel_core/harness.py:311,2068,3184-3198` —
  `HarnessConfig`, the cap-reached AGENT_ERROR shape, the per-iteration
  cost/turn rollup where ceilings hook.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_strategy.py:97` —
  `SandboxRequest`/`SubmitRequest`/`SandboxProvider`/`SubmitStrategy`, the seam to
  extend with `SandboxHandle` + `teardown`.
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py:379,429,688,811` —
  the reference provider (`prepare_sandbox`), `_run_setup`, `_cleanup` teardown,
  `retention_sweep`.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_store_factory.py` —
  the env-secret precedent (`resolve_postgres_dsn`) the `[sandbox.env]` resolver
  mirrors.
