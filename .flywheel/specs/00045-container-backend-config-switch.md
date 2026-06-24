# 00045 — `[sandbox] backend = "container"` config switch

Status: spec. The turnkey finish to increment G of
[00036](00036-sandbox-deploy-model.md): make the container backend selectable
from `flywheel.toml` so `uv run flywheel worker` runs tasks in containers with no
driver script. Today the container backend ([00044](00044-container-sandbox-backend.md))
is library-only — usable only via a hand-written `orchestrate(...)` driver
(`examples/container/run_e2e.py`).

## Why

`SandboxPolicy.backend` already parses `"worktree"`/`"container"` but the worker
ignores it: `flywheel_worktree.worker.main` always builds a `GitWorktreeSubmitter`
and passes it to `orchestrate(strategy=...)`. Wiring the switch means an operator
sets a few `[sandbox]` keys and gets containerized agent runs through the same
`flywheel worker` entry as any other backend.

## Design (decided)

**The container strategy *wraps* the worktree submitter.** The worktree backend
still provisions the git worktree and lands the merge host-side (the worktree is
bind-mounted, so the agent's edits are already on it). `backend = "container"`
wraps that inner submitter in a `ContainerSubmitStrategy` — it does not replace
the landing path. So the existing `submit_strategy` (merge/pr) keeps working
*inside* the container backend.

**Dependency arrow — `flywheel-container` is an optional extra.** The worker
lives in `flywheel-worktree` (depends on `flywheel-orchestrator`, not the
container package). The wrap is a **lazy import** inside the
`backend == "container"` branch, mirroring the optional-SDK pattern: importing
`flywheel_worktree.worker` never requires `flywheel-container`. `flywheel-worktree`
gains a `[container]` extra; the `flywheel` product pins it (turnkey out of box).
A missing package raises a clear "install flywheel-container" error.

**Secret-safe auth (mirrors `[sandbox.env]`).** The policy declares the auth
*mode* and the env var *name*; the token *value* is read from the environment at
worker build time, never from the file. `auth = "oauth"` (default) reads
`CLAUDE_CODE_OAUTH_TOKEN`; `"api_key"` reads `ANTHROPIC_API_KEY`; `"session"`
bind-mounts `~/.claude` (no env); `"none"` injects nothing. `auth_env` overrides
the var name. A declared-but-unset token env is a fast, clear error.

### Config schema

```toml
[sandbox]
backend = "container"          # "worktree" (default) | "container"

[sandbox.container]
image       = "flywheel-agent:latest"   # REQUIRED when backend = "container"
model       = "claude-sonnet-4-6"        # default: the resolved agent model
auth        = "oauth"                    # "oauth" | "session" | "api_key" | "none"
auth_env    = ""                         # override the token env var name
exec_timeout = 1800                      # seconds; 0 = unbounded (not recommended)
egress_network = ""                      # operator egress-proxy net for [sandbox.network] allow_hosts
```

`[sandbox.network]` (existing) drives the container's network: `policy="deny"`
with no `allow_hosts` → an `--internal` network; with `allow_hosts` →
`egress_network`. `[sandbox.limits]`/capabilities/retention keep their existing
roles (graders + retention are host-side; see Scope).

## Scope (decided)

- **Agent runs in the container; setup + command graders stay host-side.** The
  container is the agent execution environment; the harness still runs graders
  in-process against the host worktree (the worktree is bind-mounted). Running
  graders/setup *inside* the container is a separate concern (the known
  grade-outside-container gap), out of scope here.
- **Image build is the operator's** (`examples/container/Dockerfile`); the
  worker pre-flights image existence + UID match (already in `_docker`).
- `model` falls back to the worker's resolved agent model so a minimal
  `[sandbox.container] image = …` works.

## Success criteria (each → grader)

**SC-1 — schema.** `[sandbox.container]` parses into a frozen `SandboxContainer`
on `SandboxPolicy`; absent table = defaults; `backend = "container"` without an
`image` is a `PolicyError`; an unknown `auth` mode is a `PolicyError`. *Grader:*
`test_sandbox_container_policy.py`.

**SC-2 — auth resolution.** `resolve_auth(mode, env, token_env)` returns the
right `ClaudeAuth`: `oauth`/`api_key` read the token by name (missing → clear
error), `session` → a `~/.claude` mount, `none` → `None`. *Grader:*
`test_container_config.py`.

**SC-3 — strategy build.** `build_container_strategy(inner, …)` returns a
`ContainerSubmitStrategy` wrapping `inner` with the resolved image/model/auth/
network/exec_timeout. *Grader:* same file.

**SC-4 — worker wiring.** `maybe_wrap_for_backend(submitter, policy, env, log)`
returns a `ContainerSubmitStrategy` for `backend="container"` and the unchanged
`submitter` for `backend="worktree"`; a missing `flywheel-container` raises a
clear install error. *Grader:* `test_worker_backend_select.py`.

**SC-5 — back-compat.** Absent `[sandbox.container]` / `backend="worktree"` is
byte-identical; the full suite stays green. *Grader:* full suite.

## Tasks

- `container-config-schema` (orchestrator) — `SandboxContainer` + parse.
- `container-config-bridge` (flywheel-container) — `resolve_auth` +
  `build_container_strategy` (+ exports).
- `worker-backend-select` (flywheel-worktree) — `maybe_wrap_for_backend` lazy
  wrap in `worker.main`; `[container]` extra; product dep.

## Anchor files

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `SandboxContainer`, `_optional_sandbox_policy`, `SandboxPolicy.container`.
- `packages/flywheel-container/src/flywheel_container/_config.py` (new) —
  `resolve_auth`, `build_container_strategy`.
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py` — `main`
  (~1331 strategy build), new `maybe_wrap_for_backend`.
- `packages/flywheel-worktree/pyproject.toml` — `[container]` extra.
- `packages/flywheel/pyproject.toml` — add `flywheel-container`.
