# Feature: GitHub issue triage and drain

## Outcome
A human labels a GitHub issue with the intake label and walks away. Without further operator action the issue is agentically compiled into a task carried on the issue itself — with an authoritative grader proven to fail against the current base — drained by the worker through the existing `github` source, and closed with grader receipts. An issue too vague to compile surfaces as needs-detail with the specific missing information named, instead of being run vacuously against an already-green default grader.

## Background
The shipped `github` work source already drains labeled issues, but a vague issue with no spec block falls back to `[[defaults.graders]]` — typically an already-green suite — so the agent can "complete" it without addressing anything and the store records done+verified. The missing halves are an agentic triage stage that turns issue prose into a properly graded task, and a trust rule on the drain side so a ready label alone is never enough to schedule work. Race scope is a single flywheel deployment per repo: claims/leases and the reconciler already cover worker races; the races to close here are human/agent edits to the issue after triage and re-drain after completion.

## Scope
### In scope
- A triage engine (orchestrator layer) that lists intake-labeled open issues, agentically compiles each into a ` ```flywheel ` spec block written back onto the issue, proves the authoritative grader fails against the current base, records that receipt, and flips the label to ready or needs-detail.
- A `flywheel triage` verb: daemon by default, `--once` for a single pass, configured by a `[triage]` policy table (labels, interval, per-pass cap).
- A drain-side trust rule in the `github` source (opt-in config): a ready issue is schedulable only with a valid spec block whose triage receipt matches the issue's current content.
### Out of scope
- Cross-deployment claim markers on GitHub (assignee/label at claim time). Single deployment per repo is the supported topology.
- Changes to `github_ci` / `github_review` / `directory` sources, the claims/lease machinery, or reconciler semantics.
- Priority scoring among ready issues (existing issue-number order stands) and autopilot integration.
- Console supervisor integration for the triage daemon (operator starts it explicitly, like autopilot).
### Must not regress
- The `github` source with today's config (no trust knob) behaves byte-identically: label listing, spec-block parsing (present-but-invalid raises `WorkSourceError`), `[[defaults.graders]]` fallback, comment/close report.
- `list_work()` raises on transport failure, never returns `[]`; a listing failure never interrupts live runs.
- All GitHub ticket writes originate from the harness (triage engine or `report`), never from the coding agent's run.
- `scripts/check.sh` stays green.

## Success Criteria

1. When a triage pass observes an open issue bearing the intake label and no current triage receipt, the issue shall end the pass carrying a parseable ` ```flywheel ` block (goal plus at least one command grader plus a triage receipt) with the intake label replaced by the ready label. [command | held-out]
   verify: pytest against a faked `gh` transport: seed an intake-labeled issue, run one pass, assert the captured writes contain a block that the shipped spec-block parser accepts, a receipt, and the label flip.
   defends against: flipping ready without authoring anything (lazy queue-clearing).

2. When triage marks an issue ready, the recorded receipt shall contain the authoritative grader command, its observed non-zero exit code from a real execution against the current base, the base commit sha, and a hash of the issue's human content (title + body with the block stripped). [command | held-out]
   verify: pytest asserts the ready flip occurs only after a subprocess execution of the recorded command returned non-zero, and that all four receipt fields are present and consistent with the seeded base.
   defends against: stamping a receipt without running the check, or running it against a different tree than the one the proof claims.

3. If every candidate authoritative grader triage can author exits zero against the current base, then the issue shall be routed to needs-detail (never ready) with a comment stating the check was vacuous. [command | held-out]
   verify: pytest seeds an issue whose only viable check already passes; assert no ready flip, needs-detail label, and the comment.
   defends against: already-green suite graders enabling do-nothing completion reported as done+verified.

4. If triage cannot compile an issue into a fail-first gradeable task, then the issue shall end the pass with the needs-detail label and exactly one comment naming the specific missing information, and shall never appear in `list_work()`. [command | held-out]
   verify: pytest with an authoring stub returning cannot-compile: assert label, single comment with named gaps, and zero WorkItems from the source.
   defends against: emitting a stub task to clear the queue, or skipping silently so the issue rots unlabeled.

5. While the drain trust rule is enabled, a ready-labeled issue shall be scheduled only if it carries a valid spec block whose receipt's content hash matches the issue's current human content. [command | held-out]
   verify: pytest source test: ready issue with valid block and matching receipt yields a WorkItem; the same issue with the body edited after the receipt yields none.
   defends against: the fixing agent (host-authenticated `gh`) or a human weakening the grader in the block after the fail-first proof.

6. If a ready-labeled issue lacks a valid spec block or receipt while the trust rule is enabled, then the source shall skip it, surface the skip through its log seam, and never fall back to `[[defaults.graders]]`. [command | held-out]
   verify: pytest: ready label with no block yields no WorkItem and one surfaced skip line, even with `[[defaults.graders]]` configured.
   defends against: anyone flipping the ready label directly to smuggle ungraded work into the queue.

7. When a triage pass observes a ready issue whose content hash no longer matches its receipt, it shall re-triage the issue (new block and receipt, or needs-detail) within that pass. [command | held-out]
   verify: pytest: seed a ready issue with a stale receipt, run one pass, assert a fresh receipt matching the new content or the needs-detail transition.
   defends against: post-triage edits leaving the issue permanently unschedulable with no path back.

8. When a drained issue's run reaches DONE, the report shall close the issue with a grader-receipt comment, and the closed issue shall not appear in the next listing. [command | visible]
   verify: pytest: deliver a DONE WorkReport through the configured pipeline, assert `gh issue close` with receipts body in the captured writes and absence from a subsequent `list_work()`.
   defends against: completed issues re-draining forever under `done_action = "comment"`.

9. When a triage pass runs immediately again over unchanged issue state, the second pass shall issue zero GitHub writes. [command | held-out]
   verify: pytest: run two passes on the fake transport with no state change between them; assert the second pass's write-set is empty.
   defends against: comment spam and duplicate blocks that break the parser or bury the issue thread.

10. When invoked as `flywheel triage --once`, the verb shall run exactly one triage pass and exit 0; if the `[triage]` policy is malformed, it shall exit 2 before issuing any GitHub write. [command | visible]
    verify: pytest CLI test: `--once` on a valid policy exits 0 after one pass; a malformed policy exits 2 with zero captured `gh` writes.
    defends against: a config typo silently degrading into live writes against the wrong labels.

11. While running as a daemon, a pass that finds no intake-labeled or stale-ready issues shall issue zero GitHub writes and the daemon shall continue to the next interval. [command | held-out]
    verify: pytest: idle pass on the fake transport writes nothing and the loop schedules the next cycle rather than exiting.
    defends against: filler writes on idle repos, and crash-on-idle killing the unattended drain.

12. While a triage pass runs, it shall process at most the configured per-pass cap of issues, and shall surface how many intake candidates were deferred. [command | visible]
    verify: pytest: seed cap+2 intake issues, assert exactly cap are processed and the deferral count is surfaced.
    defends against: a 200-issue backlog burning unbounded agent spend in one unattended pass, and silent truncation reading as full coverage.

13. The authored goal and authoritative grader shall address the intent stated in the issue's human content, not an arbitrary check that merely fails on the base. [rubric, paired with #2 and #3 | held-out]
    verify: an LLM judge is given (issue human content, authored block) pairs from the test corpus and asserts the grader exercises the behavior the issue asks for.
    defends against: the residual fail-first hole — a check that is genuinely red on the base but orthogonal to the issue (fail-first proves non-vacuity, not relevance).

Verification surface: unchanged. This feature adds new modules, config, and tests; it does not modify the repo's existing test, lint, type, or CI machinery. Standard gate: `scripts/check.sh` green.

## Decomposition Hint (for /fw-plan)
- Layer drain-trust (`github` source + config knob): satisfies #5, #6, #8; must-not-regress pins for today's no-knob behavior.
- Layer triage-engine (orchestrator layer, agentic compile + fail-first run + GitHub write-back): satisfies #1, #2, #3, #4, #7, #9, #12, #13; depends on drain-trust only for the shared receipt schema.
- Layer cli-daemon (`flywheel triage` verb + `[triage]` policy table + loop): satisfies #10, #11, #12 config; depends on triage-engine.
Shared invariants both sides assert against — define once, both layers import:
- The receipt schema: `command`, `exit_code`, `base_sha`, `content_hash`.
- The content-hash definition: title + body with the ` ```flywheel ` block stripped (the receipt lives inside the block, so the hash must exclude it or it can never match).
- The three label names, sourced from the `[triage]` policy table.

## Decisions Log

### D-1: Drain through the shipped `github` source; triage writes the task back onto the issue  (Status: Accepted)
- Context: the agentic compile cannot live in `list_work()` (listing must stay cheap and deterministic; the reconciler re-lists every ~15s). | Decision: a separate triage stage authors the ` ```flywheel ` block onto the issue; the existing source drains the ready label unchanged except for the trust rule.
- Rejected: emitting local task JSON (task content invisible on GitHub, drifts from the issue); compile-at-claim (agentic cost in the claim path, no human review window). | Consequences: GitHub stays the single source of truth and humans can audit or edit the compiled task; the spec-block parser becomes a load-bearing contract.

### D-2: Single-deployment race scope  (Status: Accepted)
- Context: claims/leases already serialize workers within one deployment; the reconciler already interrupts runs whose issue vanishes. | Decision: no GitHub-side claim markers; the races closed here are post-triage content edits (#5, #7) and re-drain after done (#8).
- Rejected: claim-time label/assignee markers for multi-deployment topologies. | Consequences: two independent flywheel installs pointed at one repo can still double-run an issue; documented as unsupported.

### D-3: Label lifecycle intake -> ready | needs-detail; done closes the issue  (Status: Accepted)
- Context: picking needs an opt-in signal and a visible triage outcome. | Decision: humans apply the intake label; triage flips to ready or needs-detail; the drain is configured with `done_action = "close"`. Re-entry from needs-detail is a human re-applying the intake label after editing.
- Rejected: single-label with spec-block-only eligibility (vague issues sit invisibly unrunnable); auto-triage of all open issues (touches work nobody opted in). | Consequences: three labels to provision per repo; a closed issue is the terminal state, mirroring `gh-<number>` DONE in the store.

### D-4: Fail-first, proven, is the grader bar  (Status: Accepted)
- Context: triage-authored graders are agent-authored — the single biggest gaming vector is a vacuous always-green check. | Decision: the authoritative grader must be executed at triage time and observed to fail against the current base; the receipt binds command, exit, base sha, and content hash (#2, #3).
- Rejected: autopilot-parity self-attestation guard alone (an already-green suite still qualifies); per-issue manual approval (defeats the unattended drain). | Consequences: one grader execution per triaged issue; issues whose desired end-state cannot be expressed as a currently-failing check are pushed to needs-detail rather than run on faith.

### D-5: Content-hash binding closes the post-triage tamper window  (Status: Accepted)
- Context: worktree-backend agents inherit host `gh` auth, so the coding agent could edit the issue block or flip labels; humans also edit issues after triage. | Decision: the receipt hashes the issue's human content; the drain schedules only on a match (#5, #6) and triage re-processes mismatches (#7).
- Rejected: trusting the ready label alone; pinning graders in the local store instead (diverges from the issue as source of truth). | Consequences: any edit to a ready issue makes it unschedulable until re-triaged — a mid-run edit therefore interrupts the run via the existing vanish-from-listing reconciler semantics, which is the intended behavior when the definition changes under a run.

### D-6: Verb with daemon parity, operator-started  (Status: Accepted)
- Context: the end-state is "labeled issue drains without operator action", which needs a resident process. | Decision: `flywheel triage` mirrors worker/autopilot — daemon by default, `--once` for one pass, `[triage]` policy table, exit 2 on policy error.
- Rejected: one-shot only (drain not autonomous); folding into autopilot (couples issue intake to autopilot's base-branch trust posture and depth throttling). | Consequences: a third daemon to operate; console supervision deliberately deferred (out of scope).

### D-7: Block placement is an idempotent body append  (Status: Accepted)
- Context: the shipped parser reads the fenced block from the issue body. | Decision: triage appends/replaces the block in the body, preserving the human's text above it; the content hash excludes the block so the receipt can self-consistently live inside it.
- Rejected: comment-carried blocks (would require extending the parser and choosing among multiple comments). | Consequences: triage edits issue bodies it does not own — acceptable because the human text is preserved verbatim and every write is harness-originated.

## Open Questions
None. All thirteen criteria lower to command, rubric-paired, or CLI-exit checks.

## Next Steps
Run `/fw-plan 00082-FEATURE-github-issue-triage` to compile these criteria into flywheel tasks and graders.
