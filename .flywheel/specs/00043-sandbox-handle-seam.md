# 00043 — SandboxHandle isolation seam (increment F of 00036)

Status: spec. Increment F of [00036](00036-sandbox-deploy-model.md): widen the
`SandboxProvider` return from a bare `Path` to a `SandboxHandle` so a
non-worktree backend can describe *how a run attaches to its sandbox*, not just
where it lives. The architectural prerequisite for the container backend (G).

## Why

Today `prepare_sandbox` returns a `Path` and the orchestrator runs the agent in
the worker process with that path as CWD. A container backend cannot express
itself through a `Path`: it must inject environment (the container's `PATH`, a
forwarded socket) and run the agent *inside* the container (`docker exec`), not
in the worker. `SandboxHandle` is the seam that carries those two extensions;
the worktree backend keeps returning a `Path` and is adapted with empty
contributions, so nothing changes for it.

## Scope (decided)

- **`SandboxHandle` frozen dataclass** (`_strategy.py`, peer of
  `SandboxRequest`/`SubmitRequest`): `path: Path`,
  `env_contribution: Mapping[str, str] = {}`,
  `invoke_wrapper: Callable[[InvokeFunc], InvokeFunc] | None = None`.
- **`SandboxProvider` / `SubmitStrategy.prepare_sandbox` return widened** to
  `Path | SandboxHandle`. `_as_handle` adapts a bare `Path` to an
  empty-contribution handle.
- **Orchestrator applies the handle** (`_apply_handle`): `env_contribution`
  merges onto the policy-resolved `agent_env` (handle wins on collision);
  `invoke_wrapper` wraps the run's `InvokeFunc`. An empty handle returns the
  drive args unchanged — byte-identical to today.
- **Only the two fields G needs.** No `permission_mode`/capability hints on the
  handle — capabilities already flow from `[sandbox.capabilities]` policy
  (increment B); duplicating them on the handle would be a second source of
  truth with no consumer. (00036 §6 floated permission hints; dropped per the
  repo's "no seam without a consumer" lesson — see 00040/00041.)
- **No `teardown()` here.** Deferred to G, where the container's stop/rm is its
  consumer (00041 made the same call for retention).

## Success criteria (each lowers to a grader)

**SC-1 — `_as_handle` adapts a bare `Path`.** A `Path` becomes
`SandboxHandle(path=..., env_contribution={}, invoke_wrapper=None)`; an existing
handle passes through unchanged. *Grader:* `test_sandbox_handle.py`.

**SC-2 — empty handle is identity.** `_apply_handle` of an empty-contribution
handle returns the same `sandbox_primitives` and the same invoke object —
proving back-compat. *Grader:* same file.

**SC-3 — `env_contribution` merges, handle wins.** The handle's env overlays the
policy `agent_env` on collision; the input dict is not mutated. *Grader:* same
file.

**SC-4 — `invoke_wrapper` wraps the invoker.** When set, the effective invoke is
`invoke_wrapper(base_invoke)`; absent a base invoke it raises. *Grader:* same
file.

**SC-5 — back-compat end to end.** The default and worktree `Path`-returning
providers still drive runs identically; the full orchestrator suite stays green.
*Grader:* full suite.

## Out of scope

The container backend and `teardown()` (G); per-task capability overrides on the
handle.

## Task

- `sandbox-handle-seam` (orchestrator) — SC-1..5. `SandboxHandle` + `_as_handle`
  in `_strategy.py`; `resolve_sandbox` returns a handle; `_apply_handle` folds
  contributions into the two drive sites; `SandboxHandle` exported.

## Anchor files

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_strategy.py` —
  `SandboxHandle`, `_as_handle`, widened `SandboxProvider`/`SubmitStrategy`.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  `resolve_sandbox` (returns handle), `_apply_handle`, the two
  `_drive_or_relinquish` call sites.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/__init__.py` —
  export `SandboxHandle`.
