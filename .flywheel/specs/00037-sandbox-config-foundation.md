# 00037 — Sandbox config foundation (increment A of 00036)

Status: spec. The keystone increment of [00036](00036-sandbox-deploy-model.md):
build the `[sandbox.*]` config surface, the `fast` preset, the resolution +
threading spine, and live-wire it through the agent invocation **with zero
behavior change**. Every later increment (B-G) fans out from this.

The deliverable of this stage is *justified trust that the spine is correct and
inert* — not a feature. The valuable, irreducible work is the success criteria
below, written so an optimizing agent cannot satisfy the literal check while
breaking the guarantee.

## Why

The agent execution environment is hardcoded/accidental/scattered across four
`ClaudeAgentOptions` sites, `WorkPolicy`, and CLI flags (see 00036 §Why). Nothing
in 00036 can be configured until there is a config surface, a preset model, and a
threading spine that reaches the option sites. This increment builds exactly that
and proves it changes nothing — so B-G become pure additions.

## Scope (decided)

- **Live-wire `fast` end-to-end.** The four `ClaudeAgentOptions` sites read the
  resolved `SandboxPolicy`; `fast`'s values equal today's hardcoded ones; the
  golden test proves equivalence. No surface-only dead code.
- **`fast` preset only.** `balanced`/`hardened` are *not* defined here; selecting
  them fails fast. Later increments add preset values as each aspect's enforcement
  lands, so a preset never advertises a guarantee it cannot enforce.

## The `fast` baseline (= today, verbatim — the golden target)

`fast` must reproduce the current construction exactly. Verified current values:

| Knob | `fast` value | Source today |
| --- | --- | --- |
| `permission_mode` | `bypassPermissions` | hardcoded `workflow.py:452` |
| `skills` | `all` | hardcoded `workflow.py:453` |
| `allowed_tools` / `disallowed_tools` | unset (omit) | not set today |
| `setting_sources` | unset (omit — SDK derives `["user","project"]` from `skills="all"`) | not set today |
| `mcp.servers` / `mcp.strict` | none / false (omit `mcp_servers`) | not set today |
| `env` | full ambient passthrough (no explicit `env=`) | SDK inherits `os.environ` |
| `network` | allow (no enforcement) | absent today |
| `exec` | disabled | not set today |
| `limits` | turns=500, retries=1, lease=300, no cost/token/wall-clock ceiling | `DEFAULT_*`, CLI flags |
| `retention` | done=destroy, fail=park, sweep=7 | hardcoded `worker.py` |

The mapping policy→options must **omit** any field at its unset sentinel, so the
constructed options object is identical to today's (not merely effectively
equivalent).

## Success criteria (each lowers to a grader)

**SC-1 — Back-compat golden (headline).** For a `flywheel.toml` with no
`[sandbox.*]` sub-tables (and for one with `[sandbox] setup` only), the
`ClaudeAgentOptions` constructed at each of the four sites has an identical set of
set fields and values to the pre-change construction, and the full existing
`uv run pytest` suite passes unchanged.
*Grader:* a characterization test capturing the options for a default-config run
at all four sites and asserting field-set + values equal the documented `fast`
baseline; CI runs the existing suite.
*Ungameable angle:* asserts on the actual constructed options object at every
site, not "a test passes" — an agent cannot satisfy it by special-casing one site.

**SC-2 — Config surface parses.** Every key in the 00036 §3 schema
(`[sandbox] preset/backend/setup`, `[sandbox.exec]`, `[sandbox.capabilities]`
+ `.mcp`, `[sandbox.network]`, `[sandbox.env]`, `[sandbox.limits]`,
`[sandbox.retention]`) parses into a frozen `SandboxPolicy` on `WorkPolicy`.
*Grader:* a test loading a `flywheel.toml` that sets every key to a non-default
value and asserting each resolves to that value on the frozen dataclass.

**SC-3 — Strict validation + forward-compat.** A present-but-malformed value
fails fast with a `{path}: sandbox.<key> must be ...` message; an unknown key
under any `[sandbox.*]` table is ignored.
*Grader:* per-key tests that a bad value raises the policy error with the keyed
message; a test that a bogus key resolves cleanly to defaults. Mirrors the
existing `_optional_*` pattern in `_policy.py`.

**SC-4 — Preset resolution semantics.** `preset` defaults to `fast`; `fast` is
the only defined preset; any other preset name fails fast with
`preset '<x>' is not available`; a present sub-table key overrides the preset
per-key with **list-replace** (not append) semantics; CLI flags
(`--max-turns` etc.) win over both.
*Grader:* tests for default=fast; unknown-preset error; single-key override
leaving the rest at `fast`; `allow_hosts`/list keys replacing not appending;
CLI-over-policy precedence on a shared knob.

**SC-5 — Live-wire, no dead config.** Every value `SandboxPolicy` now owns is
*read* from the resolved policy at the option-construction sites; no site still
hardcodes a policy-owned value.
*Grader:* a test asserting the four sites construct from the threaded policy (e.g.
injecting a non-`fast` `skills`/`permission_mode` via a resolved policy changes
the constructed options) — proving the path is live, with SC-1 proving `fast`
through it equals today.

**SC-6 — Purity + optional-SDK preserved.** `test_task_module_purity` and
`test_lifecycle_module_purity` pass; `import flywheel_core` succeeds without the
`[claude]` extra; no SDK type appears in the signatures of `WorkPolicy`,
`HarnessConfig`, `orchestrate`, or `run_task_object` — the new values thread as
primitives (`str`/`tuple[str,...]`/`dict[str,str]`/`bool`/`int`/`float`).
*Grader:* the two purity tests; an import-without-extra test; a static check (or
test) that the threading params are primitives, not `ClaudeAgentOptions`.

**SC-7 — Network is parsed-but-inert.** `[sandbox.network] policy = "deny"`
parses and is carried on the policy but enforces nothing in this increment.
*Grader:* a test asserting a `deny` policy is accepted and that the run is
unaffected (no network gating), documenting the advisory status until G.

## Out of scope (explicit boundary — not graded by absence, fenced here)

Secret env-scoping (C), budget ceilings (D), the `teardown()` hook (E), the
`SandboxHandle` return type (F), real network enforcement + container backend (G),
and the `balanced`/`hardened` presets. Selecting a non-`fast` preset erroring
(SC-4) is the one place this boundary is positively gradeable.

## Open questions

None blocking. The two scoping decisions (live-wire `fast`; `fast`-only preset)
are resolved above. Downstream decisions (per-run vs per-attempt budgets,
`permission_mode` default, container home) belong to their own increments and do
not affect A.

## Anchor files

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `WorkPolicy` + `_optional_*` validators; add `SandboxPolicy` + sub-dataclasses,
  the `fast` constant, preset resolution.
- `packages/flywheel-core/src/flywheel_core/workflow.py:447-456` — the option
  site + `_make_claude_code_invoke`; the spine to thread the resolved policy
  through and the primary live-wire site.
- `packages/flywheel-core/src/flywheel_core/grader_rubric.py:368`,
  `recovery_summarizer.py:291`, `examples/hello/example.py:130` — the other three
  option sites SC-1/SC-5 cover.
- `packages/flywheel-core/tests/test_task_module_purity.py`,
  `test_lifecycle_module_purity.py` — SC-6 graders.
- `packages/flywheel-orchestrator/.../_store_factory.py` — the env-var precedent
  (`resolve_postgres_dsn`) for the later `[sandbox.env]` resolver (referenced, not
  built here).
