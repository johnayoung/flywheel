# 00038 — Sandbox capability presets (increment B of 00036)

Status: spec. Increment B of [00036](00036-sandbox-deploy-model.md): make the
fast -> hardened spectrum **selectable and demonstrable** by defining the
`balanced` and `hardened` presets, and wire the one capability knob increment A
left unmapped (`[sandbox.exec]`, the SDK bash sandbox). Builds directly on the
`SandboxPolicy` surface and `build_agent_options` seam A landed.

## Why

A delivered the full capability *wiring* — `build_agent_options` already maps
skills/allowed_tools/denied_tools/setting_sources/mcp_servers/`strict_mcp_config`
— but only defined the `fast` preset, so nothing visibly differs from today. B
adds the two upper rungs of the trust ladder and the last capability lever
(bash-exec), turning "the spectrum exists in config" into "selecting `hardened`
produces a least-privilege agent."

## Scope (decided)

- **Presets restrict only the enforced capability dimension.** balanced/hardened
  change skills/tools/MCP/settings/exec — *not* env, limits, network, or
  retention, which stay at `fast` defaults (their enforcement lands in C/D/E/G;
  a preset never advertises a guarantee it cannot enforce).
- **permission_mode stays `bypassPermissions` in every preset.** flywheel's
  worker loop has no human approver, so a non-bypass mode would freeze the agent.
  Hardening is least-privilege *tool allowlisting*, not prompting.
- **`[sandbox.exec]` gets wired** into `build_agent_options` (the SDK
  `sandbox: SandboxSettings` option), the one capability knob A parsed but left
  inert.

## Preset definitions (the exact values to add to `_SANDBOX_PRESETS`)

```python
"balanced": SandboxPolicy(preset="balanced",
    capabilities=SandboxCapabilities(mcp_strict=True)),

"hardened": SandboxPolicy(preset="hardened",
    capabilities=SandboxCapabilities(
        allowed_tools=("Bash", "Edit", "Glob", "Grep", "Read", "Write"),
        setting_sources=("project",),
        mcp_strict=True),
    exec=SandboxExec(enabled=True)),
```

- **`balanced`** — full coding capability, but `mcp_strict=True` with no servers,
  so a coding agent stops loading the operator's personal MCP surface
  (Gmail/Calendar/Drive/playwright). Everything else == `fast`.
- **`hardened`** — least-privilege coding agent: a minimal tool allowlist, no MCP,
  project-only settings (drops user-level `~/.claude` config/CLAUDE.md), and the
  SDK bash sandbox on. The allowlist is a sensible default, overridable per-repo
  via `[sandbox.capabilities] allowed_tools`.

## Success criteria (each lowers to a grader)

**SC-1 — Presets resolve to the documented values.** `preset = "balanced"` and
`preset = "hardened"` resolve to the `SandboxPolicy` above; an unknown preset
still fails fast.
*Grader:* `test_sandbox_presets.py` asserts each preset's capability/exec values
and that deferred aspects (network/env/limits) stay at `fast`.

**SC-2 — Per-key override replaces, not appends.** `preset = "hardened"` plus
`[sandbox.capabilities] allowed_tools = ["Read"]` resolves `allowed_tools` to
exactly `("Read",)` while untouched keys (e.g. `mcp_strict`) keep the hardened
value. (Now testable for the first time — `hardened` has a non-empty list.)
*Grader:* `test_sandbox_presets.py` list-replace case.

**SC-3 — `[sandbox.exec]` is wired.** `build_agent_options(exec_enabled=True,
exec_auto_allow=...)` sets `ClaudeAgentOptions.sandbox` to a `SandboxSettings`
with `enabled=True` and `autoAllowBashIfSandboxed=<auto_allow>`; with exec
disabled (the `fast` default) `options.sandbox` stays `None` (byte-identical).
*Grader:* `test_sandbox_exec_wiring.py`.

**SC-4 — End-to-end composition.** A resolved `hardened` policy fed through
`build_agent_options` yields locked-down options: the allowlist set,
`strict_mcp_config=True`, `setting_sources=["project"]`, `sandbox.enabled=True`.
*Grader:* the seam-holdout test in `test_sandbox_presets.py` (composes the
orchestrator preset with the core builder; depends on SC-3).

**SC-5 — Back-compat preserved.** `fast` is unchanged; the full `uv run pytest`
suite is green; increment A's oracles and the purity/optional-SDK tests still
pass.
*Grader:* full suite + A's oracles + purity.

## Out of scope

env/limits/network/retention enforcement (C/D/E/G); the judge/recovery/example
option sites; `excludedCommands`/network sub-config of the bash sandbox; any
change to `permission_mode`.

## Open questions

- The `hardened` allowlist is a sensible default, not a proof — it is overridable
  per-repo. Enabling the bash sandbox may interfere with the agent's own
  in-sandbox `uv`/`git` (network/fs restricted); the authoritative command
  graders run out-of-band and are unaffected, so this degrades agent ergonomics
  at worst, not the grade. `excludedCommands` tuning is a later schema extension.

## Tasks

- `sandbox-exec-wiring` (core) — SC-3. Wire exec into `build_agent_options` +
  thread `exec_enabled`/`exec_auto_allow` exactly as the other capability
  primitives thread.
- `sandbox-capability-presets` (orchestrator, prereq `sandbox-exec-wiring`) —
  SC-1/2/4/5. Add the two presets + the end-to-end seam holdout.

## Anchor files

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `_SANDBOX_PRESETS` (add two entries), the `SandboxCapabilities`/`SandboxExec`
  dataclasses, `_optional_sandbox_policy` resolution (already does per-key
  override).
- `packages/flywheel-core/src/flywheel_core/workflow.py` — `build_agent_options`
  + `_make_claude_code_invoke` + `run_task_object` (add the exec primitives).
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  if the exec primitives thread through here, mirror the existing capability
  pass-through.
