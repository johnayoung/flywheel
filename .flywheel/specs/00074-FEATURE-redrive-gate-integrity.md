# Feature: Redrive gate integrity

## Outcome
No landing re-attempt can bypass the held-out gate: a re-driven landing completes
only after a fresh gate evaluation passes against the exact content landing, and
every grader-decided park (held-out-gate, standing-verify, post-rebase
re-verification) carries the deciding check's output excerpt in its record.

## Background
On 2026-07-07 a gate-FAILED run FF-merged to main anyway: the landing re-driver
re-attempts the strategy submit without re-evaluating the held-out gate, making
gate verdicts advisory once a park is redriven (finding 1,
`.flywheel/audits/00073-run-decision-record-verify.md`, execution addendum).
Separately, standing-verify parks carry a fixed message rather than the failing
check's output (finding 2), so those blocks are not diagnosable from the store.
The tacit bar: the gate's authority must survive every path to a landing, not
just the first attempt, and a park must name why in the check's own words.

## Scope
### In scope
- Fresh held-out gate evaluation on every landing re-attempt, with the same
  fail-closed semantics as the first attempt.
- Re-evaluation outcomes recorded like first evaluations (verdict records with
  receipts; FAIL re-parks; failures count toward the redrive bound and route to
  human review at the bound).
- Deciding-check output excerpts on standing-verify parks and on parks caused by
  failing post-rebase re-verification of the task's own graders.

### Out of scope
- Changing what the gate checks, oracle registration, or gate configuration.
- Parks not decided by a failing check (uncommitted-work, protected-paths,
  push-failed, submit-error, rebase-conflict): their existing detail stands.
- Retroactive records for past landings; the human-review queue mechanics.
- PR-strategy-specific landing flows beyond keeping their tests green.

### Must not regress
- Ungated redrives (no held-out source configured) land exactly as today.
- Redrive bound semantics, the landability probe's drop of already-landed runs,
  and human-review routing at the bound.
- First-attempt landing behavior: gate decisions and records unchanged.
- Spec 00073's shipped behavior: verdict records, Landed events, LandingRedriven
  pairing, decision surfaces in show/audit, excerpt bound and redaction.

## Success Criteria

1. When a gate-parked run's landing is re-attempted while its held-out oracle
   still fails, the landing shall not complete. [command | held-out]
   verify: end-to-end fixture with a persistently failing registration: after the
   redrive, the target branch head is unchanged and a fresh gate-decided park is
   retrievable for the run.
   defends against: the confirmed bypass — re-attempting submit without any gate
   evaluation lands the blocked change.

2. When a gate-parked run's landing is re-attempted and a fresh gate evaluation
   passes against the content landing, the landing shall complete.
   [command | held-out]
   verify: stateful oracle (fails on first evaluation, passes on the second):
   after the redrive, the landed reference is reachable on the target branch.
   defends against: overcorrecting into a dead end where a cleared gate park can
   never land.

3. When a landing re-attempt evaluates the gate, that evaluation shall leave its
   own retrievable verdict record whose receipts carry the re-evaluation's grader
   output, distinct from the prior evaluation's. [command | held-out]
   verify: the stateful oracle prints a different nonce on its second run; the
   run's record contains a second verdict record whose output excerpt contains
   the second-run nonce.
   defends against: landing without re-evaluating while stamping a synthetic pass
   record copied from the first evaluation — the second nonce cannot be
   pre-computed.

4. When a landing whose original park was not gate-decided is re-attempted while
   the gate now fails, the landing shall not complete. [command | held-out]
   verify: standing-verify-parked run whose gate oracle passed at first landing
   and fails at redrive time: head unchanged, gate-decided park retrievable.
   defends against: scoping the fix to held-out-gate parks only — a stale gate
   pass outliving a content-changing rebase.

5. If a landing re-attempt's gate evaluation cannot produce a verdict, then the
   landing shall not complete. [command | visible]
   verify: registration whose oracle is unexecutable at redrive time: no land, a
   defensive park is recorded.
   defends against: treating an errored evaluation as a pass on the retry path
   (fail-open).

6. While no held-out gate is configured, a re-attempted landing shall land
   exactly as today. [command | visible]
   verify: the existing redriver suite passes unmodified; an ungated park-and-
   redrive fixture lands with no gate record.
   defends against: gating redrives that have no gate, breaking the redriver for
   every non-gated project.

7. When a landing re-attempt's gate evaluation fails, the failure shall count
   toward the redrive bound, with routing to human review at the bound.
   [command | visible]
   verify: bound=N with a persistently failing oracle: exactly N re-attempts are
   made, then the run is routed to the human-review queue and no further attempt
   occurs.
   defends against: an infinite redrive loop against a failing gate, or dodging
   the bound by not counting gate failures as attempts.

8. When a landing parks on the standing build invariant, the park record shall
   carry an excerpt of the failing check's output. [command | held-out]
   verify: a [submit] verify command that prints a nonce and fails: the park
   retrievable for the run contains the nonce, not only the fixed message.
   defends against: keeping the fixed-message detail, or synthesizing an
   "output" field from the exit code.

9. When a landing parks because post-rebase re-verification of the task's
   graders fails, the park record shall carry the deciding grader's output
   excerpt. [command | held-out]
   verify: a grader that passes in-run and fails post-rebase printing a nonce:
   the resulting park record contains that nonce.
   defends against: leaving re-verification receipts in memory only, so the park
   stays undiagnosable from the store.

10. Output excerpts introduced by this feature shall be bounded and redacted
    identically to existing decision-record excerpts. [command | visible]
    verify: oversized-output fixture persists a capped tail retaining the final
    content; a secret-shaped token renders redacted by default.
    defends against: unbounded ledger growth or a redaction bypass entering via
    the new fields.

Verification-surface criteria (this feature changes the gate and redrive
machinery — the checks that decide what lands):

11. The existing verification suite passes after the change, with no gate,
    redriver, landing, or 00073-phase test deleted, skipped, or weakened; any
    relaxed check is named with an equal-or-greater replacement.
    [command | held-out] (verification-surface)
    verify: scripts/check.sh exits 0; the diff over packages/*/tests/ shows no
    removed or skipped assertion in those suites without a named replacement.
    defends against: making the redrive gate "pass" by deleting the redriver or
    gate tests that pin the bound and fail-closed behavior.

12. For first-attempt landings, gate decisions and their records shall be
    unchanged. [command | held-out] (verification-surface)
    verify: existing held-out gate suites pass with assertions unmodified; the
    00073 held-out oracles for gate verdicts, parks, landings, attempts, and
    durability remain green against the merged result.
    defends against: fixing the redrive path by rerouting or weakening the
    first-attempt gate path.

13. New behavior is proven by held-out checks the implementing agent did not
    author against its own known inputs. [command | held-out]
    (verification-surface)
    verify: held-out registrations for this spec's tasks exist and gate the
    landings, asserting stateful-oracle nonce content; the prior redrive oracle
    whose declared contract this feature supersedes (redrive does not re-run the
    gate) is re-authored to the new semantics as its equal-or-greater
    replacement before this spec's terminal task lands.
    defends against: the agent writing the only test that grades its own gate
    wiring.

## Decomposition Hint (for /fw-plan)
- Layer redrive-gate (orchestrator): satisfies #1-#7; the landing re-attempt path
  gains a gate evaluation with first-attempt bookkeeping.
- Layer park-output (worktree submitter): satisfies #8, #9, #10; independent of
  the redrive layer and parallel-eligible with it.
Shared invariants: the gate-receipt/excerpt payload shape shipped by spec 00073
(reuse, do not fork a second shape); the redrive entry point must be able to
reach the configured held-out source (wiring several callers assert against —
update them together).

## Decisions Log

### D-1: Every landing re-attempt re-evaluates the gate  (Status: Accepted)
- Context: the bypass fired live; also a rebase can change the content landing
  after a non-gate park, making any earlier gate pass stale. |
  Decision: a fresh gate evaluation guards every re-attempted landing, not only
  redrives of held-out-gate parks.
- Rejected: gate-parks-only (leaves the stale-pass hole open); gate-on-probe
  (the probe is read-only by design). | Consequences: each re-attempt pays one
  oracle run; bounded by the existing redrive bound.

### D-2: Re-evaluations use first-evaluation bookkeeping  (Status: Accepted)
- Context: spec 00073 D-1 records every evaluation; a separate re-evaluation
  bookkeeping path would fork the record vocabulary. |
  Decision: verdict records with receipts, FAIL re-parks, failures count toward
  the redrive bound, human-review routing at the bound.
- Rejected: a bypass-only counter or unrecorded re-evaluations. | Consequences:
  gate-failing redrives consume bound attempts by design.

### D-3: Grader-decided park output breadth and format  (Status: Accepted)
- Context: finding 2; post-rebase re-verification receipts are in-memory only. |
  Decision: standing-verify parks and re-verification-decided parks carry the
  deciding check's output excerpt, reusing 00073's shape (8 KiB tail stored raw,
  render-time redaction).
- Rejected: all park kinds (push-failed and friends already carry their cause
  and are not check-decided); a new excerpt shape (forks the vocabulary). |
  Consequences: rebase-conflict divergent-base parks keep today's detail.

### D-4: Interview skipped; defaults recorded as decisions  (Status: Accepted)
- Context: the operator pre-authorized proceeding without blocking; both
  findings carry live evidence in audits/00073-run-decision-record-verify.md. |
  Decision: author from that evidence; the recommended default was taken at each
  fork and recorded here.
- Rejected: a blocking interview round. | Consequences: any wrong default
  surfaces at fw-verify realizability or execution and routes back as a
  superseding decision, not a spec edit.

## Open Questions
None.

## Next Steps
Run `/fw-plan 00074-FEATURE-redrive-gate-integrity` to compile these criteria
into flywheel tasks and graders.
