# Feature: Per-run decision record

## Outcome
Every decision the loop takes on a run — attempts, grader verdicts, held-out gate
results with their output, parks, redrives, landings — is retrievable per-run
through the CLI's run-scoped JSON views (`flywheel audit <run-id> --json` stream,
run detail via `flywheel show ... --json`), with a window and grader history that
faithfully cover all attempts.

## Background
Retro 08-agent-usage-surface (F1) showed three gated landings whose CLI-retrievable
streams all terminate at `attempt_finalized`: the gate verdicts, one park, one
redrive, and three merges left no retrievable record, so a real block-and-redrive
had to be demoted to un-citable context because its only witness was ephemeral
worker stdout. The tacit bar: forensics on trust decisions must work from the store,
days later, with no live process — a record that requires stdout capture at the
moment of failure does not count as retrievable.

## Scope
### In scope
- Held-out gate verdict records for every evaluation (PASS, FAIL, NO_GATE) with
  per-grader outcome and bounded redacted output excerpts.
- Park records retrievable per-run, carrying the deciding grader's output.
- Redrive records paired with their result.
- Positive landing records (all submit strategies) carrying the landed reference.
- Per-run retrieval of every attempt's grader verdicts, not only the last attempt's.
- Decision retrievability that survives loss of the run's telemetry file.

### Out of scope
- Intra-attempt telemetry cadence (P2 of proposals/08 — accepted, do not fix).
- Retroactive backfill: runs completed before this ships gain no records.
- Retention/pruning policy for the events ledger.
- Any change to gate, park, or redrive semantics (what blocks, passes, or retries).
- The orchestrator claim-lease ledger and the live/status UI beyond must-not-regress.

### Must not regress
- Held-out gate blocking and fail-closed behavior (existing gate tests pass with
  their assertions unmodified).
- `flywheel status` parked/stranded surfacing and the human-review queue.
- Audit redaction-on-by-default and its flag contract.
- The multi-attempt `started_at` window fix
  (`test_history_started_at_survives_retry_stamp_overwrite`).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When the held-out landing gate evaluates a landing, the per-run CLI record shall
   contain that evaluation's verdict (pass, fail, or no-gate) with each executed
   held-out grader's identity, outcome, and an output excerpt. [command | held-out]
   verify: end-to-end gated landing whose oracle prints a nonce and fails: run-scoped
   JSON contains a gate-verdict record whose grader entry includes the nonce text and
   the landing is blocked; a run with no registration yields a no-gate verdict record;
   a passing gate yields a pass record with per-grader entries.
   defends against: emitting a synthetic verdict without running the gate, or
   persisting a verdict with empty output — the nonce cannot be pre-computed, and
   no-gate records make "gate never ran" distinguishable from "not surfaced".

2. When a landing is parked for a grader-decided reason, the per-run CLI record shall
   contain the park with its kind and the deciding grader's output excerpt.
   [command | held-out]
   verify: forced held-out FAIL with nonce-bearing oracle output: the retrieved park
   record contains the nonce text, not merely a grader label or exit code.
   defends against: duplicating the existing reason string (exit codes + labels) into
   a field named "output".

3. When a run's change lands (any submit strategy), the per-run CLI record shall
   contain a landing record carrying the landed reference. [command | held-out]
   verify: merge-strategy landing: the record's commit sha equals the commit reachable
   on the target branch (cross-checked via git, out-of-band); PR-strategy landing: the
   record carries the PR identifier.
   defends against: stamping "landed" with a fabricated or absent reference — the sha
   must match git state the recorder does not control.

4. If a landing attempt does not complete (parked or errored), then the per-run CLI
   record shall contain no landing-success record for that attempt. [command | visible]
   verify: parked-landing fixture: run-scoped JSON contains the park and zero landing
   records for that landing attempt.
   defends against: writing the landing record at submit-start regardless of outcome.

5. When a parked landing is redriven, the per-run CLI record shall contain the redrive
   paired with its result (landed, re-parked, or routed to human review).
   [command | held-out]
   verify: park -> redrive cycle fixture: run-scoped JSON shows the redrive record
   followed by its outcome record (a landing record, a new park, or a human-review
   routing record).
   defends against: recording "redriven" without re-attempting the land — the required
   outcome pairing needs a real result witness.

6. While a run has multiple attempts, the per-run CLI record shall include every
   attempt's grader verdicts keyed to their attempt. [command | held-out]
   verify: two-attempt run where attempt 1's grader outcomes differ from attempt 2's:
   run-scoped JSON contains both sets, each keyed to its attempt number.
   defends against: rendering only the last attempt's receipts, or duplicating them
   across attempts — the differing outcomes make either fake detectable.

7. The per-run record's window and totals shall cover all attempts: `started_at` is
   no later than the first attempt's start and `tokens_total` equals the sum over all
   attempts. [command | visible]
   verify: multi-attempt fixture asserts both equalities;
   `test_history_started_at_survives_retry_stamp_overwrite` passes unmodified.
   defends against: re-introducing the retry stamp-overwrite that collapsed the window
   to the last attempt while totals still summed all attempts.

8. If a run's telemetry file is absent, then the run's landing decisions (gate
   verdicts, parks, redrives, landings) shall remain retrievable per-run through the
   CLI. [command | held-out]
   verify: fixture deletes the run's JSONL after completion: run-detail JSON still
   returns the decision records, exit 0.
   defends against: satisfying the audit-stream check with telemetry-only mirror lines
   that have no authoritative store record behind them.

9. When the audit stream of a run whose landing was gate-decided is retrieved, it
   shall contain landing-stage record kinds rather than terminating at attempt
   finalization. [command | visible]
   verify: F1's re-verification shape — `flywheel audit <run-id> --json` kind
   extraction on a gated, landed run yields at least one landing-stage kind sequenced
   after `attempt_finalized`.
   defends against: leaving the stream view blind so decisions exist only in a
   different verb than the one retros extract from (paired with #8 for authority).

10. If persisted grader output contains a secret-shaped value, then default CLI
    retrieval shall render it redacted. [command | visible]
    verify: oracle output containing a token matching the audit redaction patterns:
    default retrieval shows the redacted form; the existing raw flag reveals it.
    defends against: bypassing the established redaction path because the output
    arrives via the ledger instead of the telemetry stream.

11. If a held-out grader emits output exceeding the per-grader excerpt bound, then the
    persisted excerpt shall be truncated to the bound while retaining the final
    content. [command | visible]
    verify: oversized-output fixture with a trailing nonce: stored excerpt is within
    the bound and still contains the nonce.
    defends against: capping at zero bytes (breaks #1's nonce check) or persisting
    unbounded output (breaks the bound assertion).

Verification-surface criteria (this feature touches the held-out gate execution path
— the machinery that decides landings):

12. The existing verification suite passes after the change, with no gate, park,
    redrive, or window test deleted, skipped, or weakened; any relaxed check is named
    with an equal-or-greater replacement. [command | held-out] (verification-surface)
    verify: `scripts/check.sh` exits 0; diff over `packages/*/tests/` shows no removed
    or skipped assertion in the gate/park/redrive/history test files without a named
    replacement.
    defends against: making new records "pass" by deleting the tests that pin gate
    blocking and fail-closed behavior.

13. For identical gate inputs, the landing decision (blocked, landed, fail-closed) is
    unchanged by recording. [command | held-out] (verification-surface)
    verify: existing held-out gate and landing tests pass with their decision
    assertions unmodified; the #1 fixture confirms the failing gate still blocks.
    defends against: satisfying retrievability by rerouting the gate through a
    recording path that swallows or alters verdicts.

14. New retrievability behavior is proven by held-out checks the implementing agent
    did not author against its own known inputs. [command | held-out]
    (verification-surface)
    verify: the held-out registrations for this feature's tasks exist and gate the
    landings (authored via /fw-verify), asserting nonce content the agent cannot
    pre-compute.
    defends against: the agent writing the only test that grades its own recording
    path to pass.

## Decomposition Hint (for /fw-plan)
- Layer decision-ledger vocabulary: the record shapes for gate verdicts, landings,
  redrives, and output-bearing parks; satisfies the record-shape halves of #1, #2,
  #3, #5.
- Layer emission (gate, submit, redrive paths): satisfies #1-#5, #10, #11; depends
  on the ledger layer.
- Layer retrieval (run-scoped CLI views): satisfies #6, #8, #9 and the retrieval
  halves of #1-#5; #7 rides here as regression pinning; depends on the ledger layer.

Shared invariants multiple layers assert against: the landing-stage record-kind
vocabulary; the bounded-excerpt payload shape (bound value, tail semantics); the
redaction rules shared with the audit surface. Name them once so dependent tasks
update together.

## Decisions Log

### D-1: Record every gate evaluation, including NO_GATE  (Status: Accepted)
- Context: F1 could not distinguish "gate passed" from "gate never ran". |
  Decision: PASS, FAIL, and NO_GATE all leave a verdict record.
- Rejected: FAIL-only (status quo shape; passed landings stay receipt-less);
  PASS+FAIL only (ungated landings stay ambiguous). | Consequences: one extra
  record per landing attempt; negligible volume.

### D-2: Bounded redacted tail excerpt per grader  (Status: Accepted)
- Context: park detail today is exit codes + labels; full output is unbounded. |
  Decision: persist a capped tail of stdout/stderr per executed grader, default
  bound 8 KiB, passed through the audit redaction rules.
- Rejected: full output (chatty oracles grow the ledger without bound); exit-code
  only (rejects the proposal's core complaint). | Consequences: very long failure
  output loses its head; the tail carries the deciding assertion in practice.

### D-3: Positive landing record for every landing, all strategies  (Status: Accepted)
- Context: no merged/landed event exists anywhere; clean lands print to stdout only. |
  Decision: merge-FF, PR, and container landings each record a landing event with
  the landed reference.
- Rejected: gated-landings-only (ungated runs still end at attempt_finalized). |
  Consequences: the ledger gains a terminal record per land; redrive outcomes
  become provable (#5).

### D-4: Multi-attempt fidelity extends to all attempts' grader verdicts  (Status: Accepted)
- Context: the F2 window bug is already fixed on main with a regression test; the
  run detail still surfaces only the last attempt's grader receipts. |
  Decision: per-run record includes every attempt's grader verdicts; the window
  fix is pinned as must-not-regress (#7).
- Rejected: must-not-regress only (earlier attempts' verdicts stay invisible);
  per-attempt token decomposition (no evidenced need). | Consequences: run detail
  grows with attempt count.

### D-5: Landing decisions are authoritative ledger state  (Status: Accepted)
- Context: docs/data-taxonomy.md — the events ledger is the system of record;
  telemetry is lossy-acceptable. Landing decisions today are appended post-run and
  never mirrored to the telemetry file. |
  Decision: the authoritative decision record lives in the store; the audit stream
  view is a projection of it; retrievability must survive telemetry loss (#8).
- Rejected: telemetry-only mirroring (a lost JSONL loses trust decisions; violates
  the taxonomy). | Consequences: the audit verb's contract widens beyond pure
  telemetry (#9) or gains a store-backed projection — mechanism left to /fw-plan.

## Open Questions
None.

## Next Steps
Run `/fw-plan 00073-FEATURE-run-decision-record` to compile these criteria into
flywheel tasks and graders.
