# Held-out landing gate

The held-out landing gate grades an agent's committed result with operator-declared command graders the agent never saw, then blocks landing if they fail. It is the orchestrator's anti-reward-hacking defense for high-stakes work, shipped and active in this repo (specs 00050/00051/00057).

## The problem it solves

An optimizing agent that can see or edit the test that grades it can game it — hard-code the expected output, weaken an assertion, or special-case the checker's inputs. In-band graders (the `command` graders declared on a `Task`, run by the harness) are visible in the worktree the agent edits, so they are not a sufficient verdict for work where gaming the check is cheaper than doing the work.

The held-out gate adds a second, out-of-band judgment: an oracle the agent never sees, run by the orchestrator against the committed tree, that decides whether the work is allowed to land.

## What the gate does

At submit time, after the run finalizes and before the result lands, the gate runs the task's held-out graders against the committed tree and gates landing on the verdict. It runs in `_drive_under_lease` between `run_task_object` finalizing and `submit` being called, while the lease is still held (`_orchestrate.py:1340-1372`), so two workers never race the same task and a blocked land never reaches `submit`.

The gate is strategy-agnostic: `merge`, `pr`, and any future strategy all flow through this one drive point. A blocked gate simply suppresses the `submit` call — see [strategy.md](strategy.md). The sandbox is left parked for forensics; nothing merges or opens a PR.

### Verdict semantics

`evaluate_held_out_gate(task, source, *, committed_tree, run_id)` (`_held_out_gate.py:311`) returns a frozen `GateVerdict` whose `outcome` is one of three terminal states (`_held_out_gate.py:64`):

| Outcome | Meaning | Blocks landing |
| --- | --- | --- |
| `NO_GATE` | No held-out grader registered for this task. Lands byte-identically to an ungated repo. | No |
| `PASS` | Every registered held-out grader ran and exited zero. | No |
| `FAIL` | At least one grader exited non-zero, OR a registration exists but could not be loaded/executed. | Yes |

Four properties an operator must internalize:

- **Fail closed.** A registered grader that cannot be located, parsed, or executed yields `FAIL`, never a silent skip (`_held_out_gate.py:23-29`). The absence of a *parseable* check is never treated as the absence of a check.
- **`NO_GATE` is distinct from `FAIL`.** A task that never registered an oracle lands unchanged. Only tasks you explicitly opt in are gated.
- **The verdict is the grader exit code, never the agent's self-report.** The engine is not given the agent's terminal status. An agent reporting DONE while a held-out grader exits non-zero yields `FAIL` (`_held_out_gate.py:20-22`).
- **Command graders only.** A non-command held-out registration is a misconfiguration and fails closed (D-5). The committed tree is passed as `cwd` to every grader, so each observes the agent's committed changes.

`GateVerdict.reason` is an operator-readable summary (`_fail_reason`, `_held_out_gate.py:399`): how many of N graders ran, each failed grader's label, termination, and exit code, and how many never ran (the runner stops at the first failure).

## The no-commit fence

This is the load-bearing security property. **The held-out payload never materializes into an agent worktree.** The per-task registrations and the oracle scripts they invoke live under the git-ignored `.flywheel/verification/` tree. A fresh git worktree materializes only tracked files, so the agent cannot read or modify the oracle that judges it.

What is committed is the *pointer* (the `[held_out] root` key in `flywheel.toml`); what is git-ignored is the *payload* (the registrations and oracles under that root):

```bash
git check-ignore .flywheel/verification   # exit 0 — the payload tree is ignored
```

**Security: commit only the pointer, never the payload.** Committing a registration or oracle into the tracked repo defeats the fence — it would land in every agent worktree, exactly the test-visible-to-the-agent failure the gate exists to prevent. The orchestrator reads the payload out of band at gate time; it is never merged into the `Task` handed to the harness.

## Oracle registration shape

Each gated task gets one file under the held-out root, named by task id: `<root>/<task_id>.json`, read by `FilesystemHeldOutGraderSource` (`_held_out_gate.py:127`, the only shipped source). The body is either a bare JSON list of grader entries or an object with a `graders` key — the same per-entry shape as a `Task`'s `graders` field, validated through `flywheel_core.loaders.load_graders`.

A registration declares held-out `command` graders that invoke the oracle by its **absolute** operator path, run with the committed tree as `cwd`:

```json
{
  "graders": [
    {
      "type": "command",
      "run": "python3 /abs/path/.flywheel/verification/held-out/oracle_xyz.py",
      "name": "held-out-oracle:oracle_xyz.py"
    }
  ]
}
```

The absolute path is mandatory: under `cwd = committed tree`, a relative path would resolve inside the agent's worktree, where the oracle does not exist, so the gate would fail closed on every run.

Two library helpers build and write this shape (re-exported from `flywheel_orchestrator`):

| Helper | Anchor | Role |
| --- | --- | --- |
| `build_oracle_registration(oracle_path, *, name=None, interpreter=None)` | `_held_out_gate.py:206` | Builds the JSON body in memory. Raises `ValueError` on a relative `oracle_path`. Self-validates by round-tripping through `load_graders`. |
| `write_oracle_registration(root, task_id, oracle_path, *, name=None, interpreter=None)` | `_held_out_gate.py:271` | Writes the body to `<root>/<task_id>.json` (creates root, overwrites). Returns the path. |

`interpreter` (e.g. `"python3"`) prefixes the command for oracles that are not self-executable; omit it for an executable oracle. Both helpers refuse a `task_id` containing `/`, `\`, or `.`/`..` — the source treats such an id as unregistered (`NO_GATE`) and the writer raises `ValueError`, so no registration can read or write outside `root` via path traversal (`_held_out_gate.py:188-203`, `:297`).

## Activation

The gate is config-only — there is no CLI verb or flag. It activates when `flywheel.toml` declares a `[held_out] root`:

```toml
[held_out]
root = ".flywheel/verification/held-out"
```

**There is deliberately no default root.** A default would silently activate gating on upgrade (spec 00051 #2). When `[held_out] root` is absent, `WorkPolicy.held_out_root` is `None` (`_policy.py:468`) and the gate runs for no task. A typo never leaves the gate silently inert: a non-string or empty value raises `PolicyError` (`_policy.py:1116-1138`).

The worker resolves the root via `build_held_out_source(policy, repo_root)` (`worker.py:2221`): a relative root resolves against the repo root (`<repo_root>/<root>`, independent of the worker's cwd or any sandbox path); an absolute root is honored as written. When a source is built, `flywheel worker` logs `held-out gate active root=<...>` on startup and forwards the source into every `run_once(...)`. See [configuration.md](configuration.md) and [cli.md](cli.md).

This repo's committed config sets `root = ".flywheel/verification/held-out"`, so the gate is live here.

### How a blocked land is recorded

The verdict lands on the in-process `RunRecord`: `gate: GateOutcome | None` and `gate_reason: str` (`_orchestrate.py:410-411`). This makes a gate-blocked land distinguishable from a clean land and from an agent-run failure — **only a gate-blocked land carries `status == DONE` with `gate is GateOutcome.FAIL`**. The verdict is also logged to the run stream as `[orchestrate] <task_id>: held-out landing gate {BLOCKED|passed} (<reason>)` for any outcome other than `NO_GATE` (`_orchestrate.py:1215`).

The gate's verdict is recorded on the `RunRecord` only — it is not persisted into the lifecycle store and the held-out grader receipts are not added to the work-source report.

## Authoring tie-in: write the oracle before the work

The fence only holds if the agent never authors the test that grades it. The `/fw-verify` skill enforces this: it blind-authors a discriminating oracle into the git-ignored `.flywheel/verification/` scratch dir and records a discrimination proof, **before** the implementing agent runs. When `[held_out] root` is set, fw-verify then writes the sanctioned out-of-worktree registration (`<root>/<task_id>.json`) keyed by task id — a write to the git-ignored, out-of-worktree root only, explicitly not a commit into the tracked repo. The implementing task carries a `non_goals` line forbidding the agent from reading or writing under `.flywheel/verification/`.

fw-verify proves an oracle exists at authoring time; the execute-time held-out gate documented here is the complementary orchestrator half that grades the real run. See [workflow.md](workflow.md) for the full spec/plan/verify/execute/retro pipeline.
