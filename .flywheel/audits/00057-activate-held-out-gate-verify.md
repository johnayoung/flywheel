# fw-verify audit — 00057 activate-held-out-gate

Spec: `.flywheel/specs/00057-activate-held-out-gate.md` (6 criteria; #1-#5 held-out, #6 verification-surface DoD).
Tasks (both `done` first-attempt, merged to main, archived): `held-out-gate-activate-config` (3d30def), `held-out-gate-prove-end-to-end` (0497d7d).
Integrated main: ruff 0, pyright 0, **2058 passed** (+4 over 00056's 2054). The one full-suite failure (`test_two_workers_run_each_task_exactly_once`, `sqlite3.OperationalError: database is locked`) is a pre-existing two-worker SQLite-contention flake — passes in isolation, unrelated to this change (config + tests only; no source/core edit).

This spec turns the 00050/00051 execute-time held-out gate ON in flywheel's own repo: `flywheel.toml` now commits `[held_out] root = ".flywheel/verification/held-out"` (a git-ignored payload dir). No production code changed — the deliverable is the committed pointer plus proof tests that drive the REAL construction path (`build_held_out_source`). fw-verify therefore proves the ACTIVATION discrimination the committed graders + 00050/00051's already-verified machinery do not, by themselves, blind-prove.

## Routing

| Criterion | Route | Reason |
|---|---|---|
| #1 repo `flywheel.toml` parses to non-None `held_out_root`; root git-ignored | SKIP (un-gameable) | Config-state, graded out-of-band by the committed `activation-pointer` (`load_policy(...).held_out_root is not None`, fails on the un-pointed tree) + `payload-gitignored` (`git check-ignore -q <root>`) command graders. |
| #2 source resolves under `<repo_root>/<root>` via `build_held_out_source` | **AUTHOR** (Oracle A) | Behavior with an observable contract; cwd-vs-repo-root resolution is discriminable. |
| #3/#4 config-built gate blocks-fail / lands-pass / NO_GATE-unregistered | **AUTHOR** (Oracle B) | The headline activation behavior; the verdict matrix discriminates a gate that ignores exit codes / fails open / gates everything. |
| #5 registration + oracle absent from the agent worktree | SKIP (un-gameable) | Filesystem-state assertion graded by the committed proof test; the committed-pointer / git-ignored-payload property was already blind-verified in 00051 #7 (`.flywheel/verification/00057/` itself is git-ignored, confirmed). |
| #6 suites pass; no flywheel-core edit | SKIP (verification-surface DoD) | The three suite graders + a `git diff` over core; deterministic. |

Authored 2 blind held-out oracles (fresh subagents, fenced to the shipped public contract only; the orchestrator was contaminated by the committed proof tests and did not author). Oracles live git-ignored under `.flywheel/verification/00057/`.

## Oracle A — source resolution (#2)

- Path: `.flywheel/verification/00057/resolution/oracle.py` (`verify(build_source)` factory) + `mutants_check.py`.
- Form: property/resolution. Sets a RELATIVE `held_out_root` and builds the source from a process cwd != repo_root; writes the registration at `<repo_root>/<root>/<tid>.json` and a distinguishable decoy at `<cwd>/<root>/<tid>.json`; asserts `source.root == repo_root/<root>` and that `graders_for(tid)` reflects the repo-root registration, never the cwd decoy.
- Real-grade: shipped `worker.build_held_out_source` **PASSES** (both flake runs).
- Discrimination — **4/4 mutants KILLED**:
  - cwd-relative `FilesystemHeldOutGraderSource(root=Path(str(held_out_root)))` — killed (root resolves to cwd, not repo_root).
  - always-None `lambda p,r: None` — killed (None for a set `held_out_root`).
  - fixed-absolute constant `/tmp` root — killed (ignores repo_root).
  - wrong-subdir `r/"held-out"` — killed (repo-root-anchored but wrong dir; registrations would miss).
- Flake screen: run-twice, identical verdicts. No under-specification.

## Oracle B — gate verdict (#3/#4)

- Path: `.flywheel/verification/00057/gate-verdict/oracle.py` (`verify(evaluate_gate)` factory) + `mutants_check.py`.
- Form: verdict matrix. Builds the source via `build_held_out_source` from a `WorkPolicy` with `held_out_root` set; registers ONE out-of-tree oracle via `write_oracle_registration(interpreter=sys.executable)`; drives `evaluate_gate` with the SAME registration+oracle against a CORRECT committed tree and a plausible-WRONG one (only the committed file differs), plus an UNREGISTERED task id. The agent-facing Task's only visible grader is `true` (it would self-report DONE), so the verdict can only come from out-of-band grading of committed content.
- Real-grade: shipped `evaluate_held_out_gate` **PASSES** the full matrix — correct tree -> `PASS` (`passed`, not `blocks_landing`); wrong tree -> `FAIL` (`blocks_landing`); unregistered -> `NO_GATE` (does not block). Both flake runs.
- Discrimination — **3/3 mutants KILLED**:
  - always-PASS — killed on the wrong-tree arm (must FAIL; got PASS). Defends against a verdict computed but ignored at landing.
  - trust-status / never-block (always NO_GATE) — killed on the correct-tree arm (must PASS; got NO_GATE). Defends against trusting the agent's status / gating-off the registered task.
  - fail-OPEN (non-PASS coerced to PASS) — killed on the wrong-tree arm. Defends against losing the fail-closed posture.
- Flake screen: run-twice, identical verdicts.
- Contract note (recorded, not a defect): the shipped `GateVerdict` exposes `.passed` / `.blocks_landing` (the fence pack mis-named them `.is_pass` / `.should_block`); the blind oracle asserted real enum semantics and tolerated either spelling, so the admission stands.

## Honest limits

- Blind discrimination is proven on the CONSTRUCTION + VERDICT layer (does the committed config wire an active, repo-root-resolved gate whose verdict tracks committed content). The full worker-drive land/block-and-park is real-graded by the committed `test_held_out_gate_proof.py` (3/3 green: block-fail/land-pass, plausible-wrong-tree-fails, worktree-absence) driving through `build_held_out_source` + `orchestrate`, building on 00050's already-verified landing integration — not re-litigated as a separate blind end-to-end oracle (a landing mutant = reimplementing `orchestrate`; the construction/verdict oracles carry the discrimination the landing path depends on). Mirrors the 00055 store-blind / loop-real-grade split.
- #1/#5/#6 SKIP to deterministic committed command graders + `git check-ignore` + `git diff` — config/filesystem state, not behavior.
- The gate is now ACTIVE in this repo, but registering THESE tasks' oracles gates nothing (the tasks are archived; the gate keys per-task at execute time). The durable win is the activated, regression-locked channel: any FUTURE self-development task gains an out-of-band held-out gate by dropping a `<root>/<task_id>.json` registration before the worker runs it. The fw-verify registration step (00051 #8) is moot here — no live task to gate — so only the proof is recorded; no `<root>/<task_id>.json` is written for the archived tasks.

## Verification surface

DoD held: existing suites pass (2058, modulo the pre-existing flake); no `packages/flywheel-core/src/flywheel_core/` edit (the two task commits touch only `flywheel.toml` + test files). No grading assertion relaxed or removed.
