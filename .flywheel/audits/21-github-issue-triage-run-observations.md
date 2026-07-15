# Run observations: phase 21-github-issue-triage

Live orchestration log for the 00082 drain. Appended as the run progresses.
Categories: [feature] new-feature candidate, [bug] defect found, [error] run/infra
failure, [process] loop-behavior observation, [adjudication] held-out adapter action.

## 2026-07-15 pre-launch

- [process] Setup: 4-task chain (github-drain-trust -> triage-compile-pass ->
  triage-pass-hygiene -> triage-verb-daemon), worker daemon backgrounded,
  concurrency 3 but the chain serializes to 1. Held-out gate armed for the first
  three tasks; U2/U3 adapters are provisional binders expected to need gate-time
  re-pointing once the engine's real API exists (documented in
  00082-github-issue-triage-verify.md).
- [feature] Pre-existing gap that now matters more: work-source skip lines and
  truncation warnings are emitted only when a `log` callable is injected, and the
  production `_policy.py` builders pass `log=None` (documented in
  docs/work-sources.md "Partial"). Criterion 6's surfaced-skip requirement lands
  through the same seam -- until the operator log is wired at runtime, drain-trust
  skips are invisible in production. Candidate follow-up spec: wire work-source
  logs to the worker's stderr/operator console.

## 2026-07-15 check-in 1 (~16:20Z)

- [process] github-drain-trust landed VERIFIED in 1 run (~11 min wall,
  7.35M tokens, $3.18). Held-out U1 driver re-run against landed main:
  ORACLE-GREEN (7/7 blind tests pass on the real trust rule).
- [adjudication] All U1 bridging assumptions bound first-try: the
  require_triage_receipt kwarg, gh-<number> ids, and the receipt encoding --
  the landed RECEIPT_KEY is exactly the "triage_receipt" fallback guess.
  Landed API now frozen: content_hash(title, body) (sha256, block-stripped),
  parse_receipt(value) -> TriageReceipt | None, is_fail_first, strip_spec_block.
- [bug] Latent adapter bug found and fixed during the B4 re-point: the U2/U3
  name-vocabulary reader would have matched the real parse_receipt and called
  it with the whole body STRING (it takes the parsed block value), returning
  None for every receipt -> receipt_matches always False -> guaranteed false-RED
  at gate time. Direction was fail-closed (park, not pass), but it would have
  cost an adjudication cycle. Lesson for the fw-verify skill: vocabulary
  introspection over callables with guessed call shapes is the fragile half of
  the adapter pattern; exact-name re-pointing as soon as a producer task lands
  should be a standing step.
- [process] U2/U3 drivers still ORACLE-RED on main with only the expected B1
  engine-absent bind failures -- correct null state while triage-compile-pass
  is in flight.
- [process] Cost note: 7.35M tokens for a ~1-module + tests change is high;
  consistent with the known repeated-large-module-read pattern. Candidate
  retro input, not blocking.

## 2026-07-15 check-in 2 (~16:50Z) -- gate park adjudicated

- [process] triage-compile-pass DONE in 1 run (10.28M tokens, $5.13), visible
  graders 4/4, full check.sh green in-sandbox. Landing PARKED on the held-out
  gate (U2 driver exit 1), re-driver exhausted its 3 attempts and routed the
  run to the human-review queue (terminal, by design).
- [adjudication] Root cause: the provisional U2 adapter, not the
  implementation. Three mis-binds: (1) TriagePass.run() is async -- the
  vocabulary binder called it without awaiting, so the pass never executed and
  the oracle saw zero events; (2) the authoring seam is a prompt-shaped async
  invoker returning a fenced-JSON schema (authoritative_grader/graders/goal/
  missing_information), not the guessed author-dict; (3) the authoring prompt
  carries no issue number -- issues must be identified by title/body match.
  Rewrote both U2 and U3 adapters with exact bindings. U2 driver vs the parked
  committed tree: ORACLE-GREEN 6/6. U3 driver vs task 3's in-flight committed
  tree: ORACLE-GREEN 5/5. The oracles themselves were never edited.
- [process] Landing plan: task 3's worktree was cut with task 2's REBASED
  engine commit stacked (9292c52 atop main), so task 3's landing carries the
  whole stack; task 2's strand then self-clears via the landability probe
  ("branch merged, no commits beyond base -> drops out"). Task 2's diff thus
  lands under task 3's (now correctly bound) gate, with U2's green driver run
  against the parked tree as the independent evidence for criteria 1-4. Will
  re-run U1+U2 drivers against main after the stack lands as final proof.
- [feature] The landing re-driver consumed all 3 re-attempts within minutes of
  the first park -- far faster than any human (or orchestrating agent) could
  adjudicate a held-out-gate failure. Candidate improvements: a backoff or
  operator-ack pause between held-out-gate re-drive attempts, and/or a
  first-park notification hook, so adjudication can happen before the bound
  exhausts and the run goes terminal.
- [bug] Stale strand surface: github-drain-trust LANDED on main (1aba327) yet
  `flywheel status` still shows its historical standing-verify park line
  ("stranded: standing-verify ... worktree preserved") with no live worktree
  or branch behind it. The park record appears to outlive the strand it
  described. Related known gap: pre-attribution STOP_RESOLVED never clears
  status (archive-sweep memory). Verify at phase archive; candidate fix spec.
- [process] Also noteworthy: task 1's landing hit ONE standing-verify park
  before landing on a re-attempt -- transient scripts/check.sh failure at
  submit time, self-healed by the re-driver. Worth a later look at which gate
  flaked (possible test pollution under concurrent worker pool).

## 2026-07-15 check-in 3 (~17:10Z) -- operator manual land of the verified stack

- [error] The U3 gate race repeated before the fix could matter: task 3 went
  DONE and its landing burned all 3 re-drive attempts against the OLD broken
  adapter during the very adjudication window that was fixing it. Both task 2
  and task 3 landings were terminal-queued; main still held only task 1; task 4
  (running) would have landed the whole stack gated by nothing but its own
  graders -- a held-out GATE BYPASS BY STACKING.
- [adjudication] Operator manual land executed: main fast-forwarded
  1aba327 -> 45152a8 (engine 9292c52 + hygiene 45152a8; FF-only, no new
  commits). Verified against the exact landed tree: scripts/check.sh green
  (ruff/pyright/pytest) AND all three held-out drivers ORACLE-GREEN
  (U1 7/7, U2 6/6, U3 5/5). Rationale: both gates had already been run green
  by the operator against the exact trees; the review queue is terminal with
  no re-arm verb; waiting meant landing the same diff with LESS verification.
- [bug] Gate-bypass by stacking: when a landing parks terminal and a dependent
  was cut from the parked tip (the designed stacking behavior), the
  dependent's landing carries the parked diff under only the dependent's own
  gate -- the parked task's held-out registration never gates what actually
  lands. Needs a spec: either re-run the gates of every task whose commits
  ride in a landing, or block dependent dispatch while the prerequisite's
  landing is unresolved.
- [feature] No operator verb to re-arm a terminal-queued landing (resolve =
  abandon only). Candidate: `flywheel redrive TASK_ID` that clears the queue
  marker and re-arms the bounded re-driver -- the natural pair to the
  first-park notification proposed at check-in 2.
- [process] Post-land state to watch: tasks 2/3 strands should self-clear via
  the landability probe (branch merged); their preserved worktrees and the
  stale queue/park records remain for archive-time cleanup. Task 4's landing
  now rebases onto the landed main (its first two commits drop as
  already-applied).

## 2026-07-15 check-in 4 (~17:25Z) -- phase complete, run closed

- [process] triage-verb-daemon landed on main (a9b3153) through the normal
  path: its landing rebased onto the manually-landed main (the two stacked
  prerequisite commits dropped as already-applied) and passed its own graders
  including docs-parity. Phase archived; rollup empty; no in-flight runs.
- [process] Final verification on final main: scripts/check.sh green
  (ruff/pyright/pytest) and all three held-out oracles ORACLE-GREEN. The
  `flywheel triage` verb is live (daemon default, --once single pass).
- [process] Strand ledger after the dust settled: tasks 2/3 strands
  self-cleared via the landability probe exactly as designed once their
  commits reached main. One phantom record remains: github-drain-trust
  "[no active row] stranded: standing-verify" naming a worktree that no
  longer exists -- the stale-park-record bug logged at check-in 2.
- [process] Phase totals: 4/4 tasks verified and landed, 4 runs, ~$18.9,
  ~35.7M tokens (T1 $3.18 / T2 $5.13 / T3+T4 remainder), wall ~1h35m
  including two gate adjudications and one operator manual land.
- [process] Worker daemon SIGTERMed after phase completion; pool shut down
  cleanly (no orphans -- the group-kill fix holding).
