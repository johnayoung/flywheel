# 00040 — Sandbox env injection (increment C of 00036)

Status: spec. Increment C of [00036](00036-sandbox-deploy-model.md): make the
`[sandbox.env]` declaration **functional** — resolve it into the agent's env and
inject it through `build_agent_options`. The resolution layer the container
backend (G) consumes.

## Why (and the honest scope)

The 00036 design assumed default-deny works by *setting* `ClaudeAgentOptions.env`
to a clean dict. It does not: the SDK builds the subprocess env as
`{**os.environ, ..., **options.env}` (`subprocess_cli.py:430`) — `options.env`
only *adds/overrides*, never *removes*. So on the worktree backend:

- **`set` literals inject cleanly** (additive) — the real value here.
- **`pass` forwarding is a no-op** (the var is already inherited) — but it's the
  declaration the container backend (G) will use to forward from a *clean* env.
- **True default-deny scrubbing is impossible** on worktree — it needs G's clean
  container env (decided with the operator).
- **Read-path secret redaction** (`build_default_redactor`, `_session.py:1281`)
  is best-effort and folds into G alongside the real scrub, not here.

So C delivers the env *resolution + injection* (G-foundational), not the scrub.
It is **non-breaking**: every current preset has an empty `[sandbox.env]`, which
resolves to no injection — byte-identical to today.

## Success criteria (each lowers to a grader)

**SC-1 — build_agent_options injects a resolved env.** `build_agent_options`
gains `agent_env: Mapping[str, str] | None = None`; when truthy it sets
`ClaudeAgentOptions.env` to it, when `None`/empty it leaves `env` at the SDK
default (`{}`), so the `fast`/no-env construction is byte-identical.
*Grader:* `test_sandbox_env_builder.py`.

**SC-2 — the decomposition resolves `[sandbox.env]`.**
`_sandbox_agent_primitives` returns an `agent_env` key:
`{name: os.environ[name] for name in env.passthrough if name in os.environ}`
merged with `env.set_values` (literals win), or `{}` when both are empty (the
`fast` default / a `None` policy). Threaded into `build_agent_options`.
*Grader:* `test_sandbox_env_decomposition.py` (with a monkeypatched environ).

**SC-3 — back-compat.** Every preset (all have empty `[sandbox.env]`) resolves to
`agent_env == {}` so `options.env` is unchanged; the full suite and the A/B/D
oracles stay green. *Grader:* full suite.

## Out of scope (folds into G)

Read-path redaction seeding from `pass` names; true default-deny scrubbing of
ambient credentials (needs a clean container env); `inherit_home` enforcement
(meaningless on the always-inherited worktree env). These land in the container
backend (G), where the env starts clean and `pass` becomes load-bearing.

## Tasks

- `sandbox-env-builder` (core) — SC-1. `build_agent_options` `agent_env` param.
- `sandbox-env-decomposition` (orchestrator, prereq `sandbox-env-builder`) —
  SC-2. `_sandbox_agent_primitives` resolves + threads `agent_env`.

## Anchor files

- `packages/flywheel-core/src/flywheel_core/workflow.py` — `build_agent_options`
  (add `agent_env`, set `ClaudeAgentOptions.env`); `_make_claude_code_invoke` /
  `run_task_object` thread it.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  `_sandbox_agent_primitives` (~line 418) adds the `agent_env` key.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `SandboxEnv` (`passthrough`, `set_values`, `inherit_home`; ~line 156), parsed.
