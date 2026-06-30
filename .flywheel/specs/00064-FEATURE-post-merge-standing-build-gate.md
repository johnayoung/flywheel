# Feature: Post-merge standing build gate

## Outcome
Nothing lands on the base branch unless a repo-wide **standing build invariant**
passes against the *exact tree that will become the base*, on **every** land
path. A new `[submit] verify` command (e.g. `cargo build --workspace --tests`,
`uv run pytest`) is re-run under the merge lock against the to-be-landed worktree
immediately before the fast-forward — independent of the task's own (often
narrow, crate-scoped) command graders. The net effect: a *semantic merge skew*,
where two independently-valid changes union into a tree that does not build,
cannot reach `main`. This is the "Not Rocket Science Rule" (never advance trunk to
a state that fails its tests) applied in-process, the invariant every merge queue
(Bors, GitHub merge queue, Graphite, Aviator) is built on. Unset `verify` => no
gate, byte-identical to today.

The concrete failure this closes: on 2026-06-30 an overnight autopilot run landed
two byte-identical `#[cfg(test)] mod tests` blocks into `infrared-feed/src/tycho.rs`
(`error[E0428]`), breaking `main`'s compile, and a ~25-branch dedupe swarm then
raced for hours without fixing it. Each contributing task passed its own grader in
isolation; the union was never compiled.

## Background
Landing lives in `GitWorktreeSubmitter._submit` (`worker.py:545-648`), under a
cross-process `merge_lock` flock (`worker.py:558`) that already serializes all
base mutations. The existing gate has two paths:

- **Clean FF** (`worker.py:596`): when the branch already contains the current
  base, `_ff_merge` fast-forwards immediately — **with no verification**.
- **Rebase path** (`worker.py:604-648`): when the base advanced, the branch is
  rebased and `_reverify` (`worker.py:684-724`) re-runs **the task's own command
  graders** against the rebased tree before retrying FF.

Two gaps let the union through:

1. The clean-FF path runs no check at all.
2. `_reverify` runs only `task.graders` command graders (`worker.py:698-699`).
   A task whose grader was `cargo test -p infrared-feed` (one crate) or a
   structural check never compiles the *workspace*, so a cross-task union break
   isn't caught even on the rebase path.

The held-out gate (`_held_out_gate.py`) already supports "standing" graders ANDed
into a verdict, but it runs **pre-merge on the isolated committed sandbox tree**
via the orchestrator, not against the post-rebase/post-merge tree, and only when
`[held_out] root` is configured. It is the wrong tree at the wrong time for this.

Serialization (the merge-queue "land lock") and submit-time rebase+reverify
already exist — this feature adds the missing piece: a standing, repo-wide build
invariant evaluated against the exact landing tree on both paths.

## Decisions
- **D-1 — Config seam.** `[submit] verify = "<shell command>"`, parsed to
  `WorkPolicy.submit_verify: str | None` (absent => `None` => no gate). Mirrors
  the existing `[phase] verify` knob. Validated as a non-empty string.
- **D-2 — Where it runs.** Inside `_submit`, under `merge_lock`, in the worktree
  that holds the to-be-landed tree, immediately before each `_ff_merge`. The
  branch is the post-merge tree on the clean-FF path (it already contains base)
  and after rebase on the divergent path; an ancestry check
  (`git merge-base --is-ancestor base branch`) selects the path deterministically
  (the base cannot move under the lock).
- **D-3 — Independent of task graders.** The verify command is policy-level, not
  task-level; it runs even for tasks with zero command graders. It runs *in
  addition to* `_reverify`, never replacing it.
- **D-4 — Failure => park, surfaced.** A non-zero exit parks the worktree (base
  untouched) and records a `LandingParked` event with `park_kind="standing-verify"`
  and the captured failure tail. Never a silent strand; never raises into
  orchestrate.
- **D-5 — Same env as graders.** Runs with `self.grader_env` (resolved
  `[sandbox.env]`), `None` inheriting the worker environment — byte-identical to
  how `_reverify` runs graders, so the build shares the same cache/toolchain.
- **D-6 — Throughput tradeoff acknowledged.** The gate runs serialized under the
  merge lock, so a slow `verify` bottlenecks landings (the classic `m*n`
  merge-queue cost). For now serialize-first is correct; speculative/batch
  verification with bisection (Aviator/Mergify/Google TAP) is deferred (P-future).

## Acceptance criteria (Phase 1)
1. `verify` unset: landing behavior is byte-identical to today (existing
   `test_submit_*` suite stays green unchanged).
2. `verify` set and passing, clean FF: branch lands; base advances.
3. `verify` set and **failing**, clean FF: branch does **not** land; base
   unchanged; worktree parked; a `LandingParked` event with
   `park_kind="standing-verify"` is recorded.
4. `verify` set, base advanced (rebase path), verify passing: lands after rebase.
5. `verify` set, base advanced, verify **failing**: parks; base unchanged.
6. The standing verify runs even when the task declares zero command graders.
7. `submit()` never raises; a verify command that itself errors is a park, not a
   crash.

## Grader plan (Phase 1)
- `command`: `uv run pytest packages/flywheel-worktree/tests/test_submit_standing_verify.py`
  — new test module exercising criteria 2-7 with a temp git repo, a passing
  verify (`true`) and a failing verify (`false` / a script asserting a unioned
  break), on both the clean-FF and base-advanced paths.
- `command`: `uv run pytest packages/flywheel-orchestrator/tests -k policy` —
  `[submit] verify` parses to `WorkPolicy.submit_verify`; absent => `None`;
  empty/non-string => `PolicyError`.
- `command`: `scripts/check.sh` — full gate (ruff -> pyright -> pytest) green.

## Subsequent phases (same outcome family — closes the rest of the overnight retro)
- **P2 — Autopilot populates `conflict_keys`. (LANDED)** `_emitted_task_file`
  derives `conflict_keys` from each task's `grader_target` + `creates_files`,
  keeping only specific source files (build manifests and directories are dropped
  so unrelated work is not over-serialized), and stamps them on the emitted task
  JSON. The directory source reads them into `WorkItem.conflict_keys` and the
  existing claim-time `_conflicts` check serializes overlapping tasks. Collapses
  the dedupe swarm (13 tasks all targeting `tycho.rs`) into one in-flight task.
  Closes spec 00061 Gap 3.
- **P3 — Surface stranded landings.** `fw status` grows a "stranded landings"
  section listing parked/unlanded autopilot branches with their `LandingParked`
  `park_kind` + detail, so a divergent-base / standing-verify park is visible
  instead of accumulating as silent `flywheel/autopilot/*` branches.
- **P-future — Speculative batch landing.** Combine N queued branches, verify the
  union once, bisect on failure (the merge-queue throughput pattern) to recover the
  serialization cost D-6 introduces.
