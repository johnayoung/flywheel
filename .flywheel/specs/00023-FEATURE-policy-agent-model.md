# Feature: Policy-configured agent model

## Summary

Add an optional `[agent] model` setting to `flywheel.toml` that selects the Claude model the worker agent runs tasks with, plus a `--model` CLI override on the worker verbs. Today no model is configured anywhere in the worker path; every run falls through to the Claude Code installation default.

## Background

The worker stack already threads a `model` parameter end to end — `flywheel_worktree.worker --model` -> orchestrator `run` -> core workflow -> `ClaudeAgentOptions(model=...)` — but nothing feeds it: the `flywheel worker` supervisor spawns the worker with only `--db`/`--tasks-dir`, so the flag's `None` default always wins. Operators currently cannot pick a model without invoking `python -m flywheel_worktree.worker` by hand.

A per-task `model` field on the core `Task` schema was considered and deferred; the repo-owned policy file is the right starting point (one committed setting, no schema or content-hash implications).

## Scope

### In Scope

- New optional `[agent]` table in `flywheel.toml` with a `model` key (string).
- `WorkPolicy` carries the parsed value; `load_policy` validates it.
- The `fw` / `flywheel worker` verbs gain a `--model` flag; precedence: CLI flag > `flywheel.toml` > Claude Code default (matches the file's documented "CLI flags always override" contract).
- The worker supervisor spawn argv passes the resolved model to `flywheel_worktree.worker --model`.
- `flywheel init` scaffold mentions the `[agent]` table (commented out, like `[[defaults.graders]]`).

### Out of Scope

- Per-task `model` field on the core `Task` schema (deferred; possible later layer that would override the policy value).
- Rubric judge and recovery summarizer models — `HarnessConfig.rubric_judge_model`, per-grader `judge_model`, and `summarizer_model` keep their existing knobs, unaffected by `[agent] model`.
- Model allowlist or alias validation — the SDK is the authority on valid model ids.
- Changes to the live `flywheel set-model RUN_ID` control command (it already overrides everything mid-run and continues to).

## Requirements

### Functional Requirements

1. **FR-1**: `load_policy` parses an optional `[agent]` table; `agent.model`, when present, must be a non-empty string, exposed as `WorkPolicy.model` (default `None`).
   - Acceptance: unit tests — file without `[agent]` yields `model=None`; with `model = "claude-opus-4-8"` yields that string; `[agent]` as non-table, `model` as non-string, or empty/whitespace string raise `PolicyError` naming file and key.
2. **FR-2**: `fw` / `flywheel worker` accept `--model`; the effective model resolves CLI flag > policy > `None`.
   - Acceptance: unit test on the resolution helper covering all three tiers.
3. **FR-3**: The worker supervisor spawn argv includes `--model <effective>` when an effective model is resolved, and omits the flag entirely when none is.
   - Acceptance: test on the spawn-argv builder (mirrors the existing `--tasks-dir` conditional-arg tests).
4. **FR-4**: With a model resolved from policy, a real worker run invokes the SDK with that model and records it in `Attempt.agent_context.model_id` (replacing the `"claude-code-default"` placeholder for that run).
   - Acceptance: existing scripted-invoker worker test extended to assert the model value reaches the invocation options and the attempt record.
5. **FR-5**: `flywheel init` writes the `[agent]` table into the scaffolded `flywheel.toml` as a commented example.
   - Acceptance: scaffold test asserts the commented `[agent]`/`model` lines are present and the file still parses as a valid policy.

### Non-Functional Requirements

- **Performance**: No specific requirement.
- **Security**: Standard practices; the value is passed to the SDK verbatim, never shell-interpolated.
- **UX**: `PolicyError` messages identify file and key, matching existing policy validation style.

## Behavior Specification

### Happy Path

1. Operator adds `[agent]` / `model = "claude-opus-4-8"` to `flywheel.toml` and commits it.
2. `uv run flywheel worker` loads the effective policy, resolves the model, and spawns the worker with `--model claude-opus-4-8`.
3. Every task the worker claims runs with that model; each attempt's `agent_context.model_id` records it.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| `agent.model` is not a string (or is empty/whitespace) | `PolicyError` at load: `<path>: agent.model must be a non-empty string` — worker refuses to start, same as other policy errors |
| `[agent]` present but not a table | `PolicyError`: `<path>: [agent] must be a table` |
| Model id unknown to the SDK | No load-time validation; the SDK rejects it at session start and the run fails with the SDK's error surfaced in the attempt record — same behavior as a bad `--model` today |
| No `[agent]` table and no `--model` flag | `model=None` end to end; SDK uses the Claude Code default (current behavior, unchanged) |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Both `--model` and policy set | CLI flag wins |
| `flywheel set-model RUN_ID` issued mid-run | Control command still overrides the session model regardless of source (unchanged) |
| Policy has `[agent]` table with no `model` key | Treated as absent; `model=None` |
| Unknown keys under `[agent]` | Follow the existing policy posture for unknown keys (ignored today); do not add new strictness in this feature |

## Technical Context

### Affected Apps

- `flywheel-orchestrator`: `_policy.py` — `[agent]` parsing, `WorkPolicy.model` field, validation.
- `flywheel`: `_tui.py` — `--model` flag and resolution helper; `_worker_supervisor.py` — spawn argv; `_cli.py` — `init` scaffold content.
- `flywheel-worktree`: no changes expected — `worker.py --model` already exists and threads through.
- `flywheel-core`: no changes expected — `workflow.py` already accepts `model` and records `agent_context.model_id`.

### Integration Points

- `flywheel_orchestrator._policy.load_policy` / `WorkPolicy` — new field.
- `flywheel._worker_supervisor` spawn argv builder — conditional `--model` arg (pattern: existing `--tasks-dir` handling at `_worker_supervisor.py:162-171`).
- `flywheel_core.workflow` `ClaudeAgentOptions(model=...)` at `workflow.py:508-515`; attempt recording at `workflow.py:776`.

### Relevant Existing Code

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py`: policy parsing, `PolicyError` conventions, `[paths]`/`[defaults]` precedent.
- `packages/flywheel/src/flywheel/_tui.py`: `load_effective_policy` use and the explicit-flag -> policy -> default resolution pattern (`_resolve_tasks_dir_for_worker`).
- `packages/flywheel/src/flywheel/_worker_supervisor.py:162`: spawn argv builder to extend.
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py:848`: existing `--model` flag (the sink).

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Where the setting lives | `flywheel.toml`, not the `Task` schema | User pivot: start with one committed repo-level setting; avoids core schema change and content-hash churn. Per-task override deferred. |
| TOML shape | New `[agent]` table, `model` key | Keeps `[defaults]` meaning work-item defaults (graders); gives future agent-runtime knobs a namespaced home. |
| CLI override | `--model` on `fw`/`flywheel worker`, flag > policy > default | Matches the policy file's documented "CLI flags always override" contract. |
| Coverage | Worker agent only | Judges and summarizer keep separate knobs; cheap-judge/expensive-worker splits stay possible. |
| Validation | Non-empty string at load; id validity left to the SDK | No allowlist to rot across model releases; aliases like `opus` keep working. |
| Loop-path coverage | Not required | No new Status/Outcome, SQL column, Grader variant, store-protocol method, or control command — no Trigger Set signal trips. |

## Open Questions

None.

## Next Steps

Run `/task 00023-FEATURE-policy-agent-model` to generate implementation tasks from this spec.
