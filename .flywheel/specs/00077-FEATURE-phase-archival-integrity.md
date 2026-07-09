# Feature: Phase archival integrity

## Outcome
A phase archives only when every task's verified work is landed, not merely DONE; an unlanded strand blocks archival, stays visible in `flywheel status` regardless of the active listing, and is resolved only by git-truth (a landability probe) or explicit operator action — never by the act of archiving.

## Background
On 2026-07-09 the adopting repo (infrared) lost a landing silently: an all-green run parked `divergent-base`, exhausted the spec-00069 re-drive bound, and 197 milliseconds later the archive sweep filed the phase and stamped the strand `stop-resolved` — the verified commit became reachable from no base and was found only by manual `git cherry` archaeology (stop-event rows ids 14/15 in infrared's postgres store; operator-attested, not locally re-verifiable). The two local defects are verified on main: the archive predicate consults lifecycle DONE alone, and archival is treated by design as "the verified resolution act" for surviving stop rows. The tacit bar from the interview: resolution must carry attribution — a strand is cleared by proof (ancestry) or by a person, and the audit trail must say which. Complementary to spec 00076: 00076 makes strands recoverable, this spec makes them impossible to hide while unrecovered; either alone leaves the failure mode open.

## Scope
### In scope
- The archive sweep's eligibility predicate (landed, not merely DONE) and its blocking report.
- Stop-event resolution semantics for landing-strand park kinds (`divergent-base`, `uncommitted-work`, `standing-verify`, `protected-paths`): probe-attributed and operator-attributed resolution only.
- An operator-facing CLI resolution verb for deliberately abandoning a strand.
- A store-backed stranded surface in `flywheel status` that outlives the active listing.
### Out of scope
- Recovering or landing stranded branches (spec 00076 owns the recovery rungs).
- Changes to the spec-00069 re-drive bound, witness pairing, or human-review queue mechanics.
- New TUI screens; the strand surface is the existing status stranded view with its source changed.
- Recovery of the specific lost infrared commit (an adopting-repo action, already noted in the proposals addendum).
### Must not regress
- A phase with any non-DONE task stays active (the existing half of the predicate).
- The loop-path archive gate (`phase_verify`, spec 00035) still gates archival.
- `.loop-base` materialization on archive.
- Existing stranded-status rendering for runs whose task files are still in the active listing.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When the archive sweep evaluates a phase containing a DONE task that has no landing receipt and whose recorded work is not an ancestor of the landing base, the sweep shall leave the phase in the active listing and report the blocking task id. [command | held-out]
   verify: harness repo with a two-task phase — one landed, one DONE-but-parked `divergent-base` with its branch head not an ancestor of the base; after the sweep the phase directory remains under `active/` and the sweep's log names the blocking task.
   defends against: the current defect — archiving on all-DONE alone; and blocking silently, which just moves the invisibility one surface over.

2. When every DONE task in a phase is landed — a landing receipt exists, or the landability probe confirms its work is an ancestor of the landing base — the sweep shall archive the phase. [command | held-out]
   verify: same harness with all branches landed archives the phase in one sweep, receipts present for some tasks and ancestry-only (hand-landed, no receipt) for at least one.
   defends against: over-blocking — satisfying criterion 1 with a predicate that never archives anything.

3. If the archive sweep runs while a landing-strand stop event is unresolved and its work has not landed, then that stop event shall remain unresolved after the sweep. [command | held-out]
   verify: the blocked-phase harness from criterion 1; after repeated sweeps the stop event carries no resolution marker.
   defends against: the id-15 failure verbatim — archival stamping a fresh, terminal strand `stop-resolved` and erasing its last trace.

4. When the landability probe confirms a stranded run's work is reachable from the landing base, the sweep shall mark that strand resolved with a probe-attributed marker and proceed to archive the otherwise-landed phase in the same sweep. [command | held-out]
   verify: hand-land the stranded branch in the harness, run one sweep; the stop event carries a probe-attributed resolution (distinct from operator attribution) and the phase moves to `archive/` in that sweep.
   defends against: a probe that always answers "landed" (paired with criterion 3's non-ancestor case, which must stay unresolved); attribution defends against machine resolutions masquerading as operator decisions in the audit trail.

5. When an operator resolves a strand through the CLI resolution verb with a reason, the next sweep shall archive the otherwise-landed phase and the stop event shall carry an operator-attributed resolution with that reason. [command | visible]
   verify: blocked-phase harness; invoke the verb with a reason; the stop event records operator attribution plus the reason text and the next sweep archives the phase.
   defends against: the abandon path collapsing back to manual store SQL — the toil class the control-plane phase just eliminated for approvals.

6. While a stranded run's stop event is unresolved, `flywheel status` shall list the strand with its park kind even when the run's task file is absent from the active listing. [command | held-out]
   verify: record a strand, remove its task file from `active/` (simulating archival or manual moves), run status; the strand renders with its park kind, and only a resolution marker clears it from the view.
   defends against: satisfying "strand visible" by keeping the active-listing filter — the exact surface defect that made visibility an accident of phase composition.

7. If a DONE task's landing state cannot be determined — no landing receipt, and neither its branch nor its recorded head resolves for the ancestry probe — then the sweep shall leave the phase active and surface that task as a strand with an indeterminate-landing marker. [command | visible]
   verify: harness with a DONE run whose branch is deleted and which has no landing receipt; the sweep leaves the phase active and the status surface names the task with the indeterminate marker.
   defends against: treating "cannot check" as "landed" — the cheapest way to make a blocked phase archive; retention-destroyed evidence must fail closed.

8. (verification-surface) When this feature's changes land, the repo's full gate shall pass with every existing archival and stranded-surface test intact or replaced by a named equal-or-stronger check. [command | held-out]
   verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing assertion covering the archive sweep, the loop-path gate, stop-event recording, or stranded-status rendering without a named replacement.
   defends against: passing the new predicate by deleting the tests that pin the old sweep's correct halves.

Verification surface: changed — the archive sweep and stranded surface are part of the loop's own bookkeeping machinery. The existing suite must still pass; any relaxed, removed, or skipped check must be named with an equal-or-greater replacement (a removed assertion with none is a blocking defect); new behavior is proven by out-of-band checks the implementing agent did not author against its own known inputs (criteria 1-4, 6 held-out).

## Decomposition Hint (for /fw-plan)
- Archival predicate and blocking report (sweep layer): satisfies #1, #2, #7.
- Stop-event resolution semantics (remove archival stamping; probe resolution with attribution): satisfies #3, #4; shares the landed-predicate with the sweep layer.
- Operator resolution verb: satisfies #5; depends on the attribution/marker shape landing first.
- Store-backed stranded surface: satisfies #6; independent of the verb, shares the resolution-marker vocabulary.
Shared invariants: the resolution-marker attribution vocabulary (probe vs operator; archival is never an attributor) and the landed-predicate (receipt or ancestry) — the sweep, the status surface, and the verb all consume both; dependent slices update together.

## Decisions Log

### D-1: Landed means receipt or ancestry, decided per task at sweep time  (Status: Accepted)
- Context: the store records landing receipts for machine lands, but hand-landed branches have none, and cleanly landed branches may be deleted afterward. | Decision: a DONE task counts landed when a landing receipt exists, or when the landability probe confirms its work is an ancestor of the landing base.
- Rejected: DONE-only (the defect being fixed); receipt-only (hand-landed work blocks forever); probe-only (deleted branches after clean lands read indeterminate). | Consequences: the probe runs at sweep time against the current landing base; a receipt is a fast path, ancestry is the truth.

### D-2: Probe-confirmed landings auto-resolve and archive in the same sweep  (Status: Accepted)
- Context: hand-landed strands would otherwise need a second manual ack. | Decision: the probe stamps a probe-attributed resolution and the sweep archives immediately when the phase is otherwise landed.
- Rejected: archive-next-sweep (latency without audit gain — attribution already preserves the trail); operator-ack-always (re-institutionalizes the manual toil this spec removes). | Consequences: a wrong probe would both resolve and archive in one pass, which is why criterion 3 pins the non-ancestor case and the probe grade is held-out.

### D-3: Abandonment is an operator-facing CLI verb writing an operator-attributed marker  (Status: Accepted)
- Context: some strands are deliberately never landed; today the only path is manual store surgery. | Decision: an `fw`-level resolution verb records an operator-attributed resolution with a reason, which unblocks archival.
- Rejected: store-level edits (manual SQL, the P1 toil class); task-file tombstones (runtime state written into the work source, against the data taxonomy). | Consequences: a new operator verb to document and test; the verb is the only non-probe path to resolution.

### D-4: Indeterminate landing state fails closed  (Status: Accepted)
- Context: retention can destroy a parked branch/worktree before landing state is confirmed. | Decision: a DONE task whose landing cannot be determined blocks archival and surfaces as indeterminate; it never counts as landed.
- Rejected: fail-open (silently blesses destroyed evidence — a second silent-loss path). | Consequences: phases can block on genuinely lost evidence until an operator resolves them via D-3's verb; that loudness is the point.

## Open Questions
None.

## Next Steps
Run `/fw-plan 00077-FEATURE-phase-archival-integrity` to compile these criteria into flywheel tasks and graders.
