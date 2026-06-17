# Feature: Worktree commit integrity and deterministic git identity

## Outcome
On a host with no preset git identity (no global/system `user.name`/`user.email`), a task that
runs in a worktree and makes a file change ends with that change captured as a commit on its task
branch — the in-sandbox `git commit` succeeds because the worker established a deterministic
identity for the worktree. When a finished (`done`) run leaves real work uncommitted (a dirty tree
at submit time), the worker preserves the worktree and branch and appends a distinguishable,
queryable landing-parked domain event to the store ledger (per D-6, superseding the earlier
git-marker choice), instead of treating it as a clean no-op and discarding the branch. A
genuine no-op (clean tree, zero commits beyond base) is still cleaned up and appends no such event.
The two cases are externally distinguishable via `list_domain_events(run_id)`, without reading
worker stderr.

## Background
`worker.py` runs only `worktree add`, `merge --ff-only`, `rev-parse` — it never runs `git add`/
`git commit` and never sets a git identity (verified: no `git config user.name/email` and no
`GIT_AUTHOR_*`/`GIT_COMMITTER_*` anywhere in the package). It relies entirely on the agent
committing inside the sandbox, which works in this repo only because the developer's global git
identity happens to be set. On a fresh host the agent's own `git commit` fails for lack of an
identity, so the loop produces empty branches; and `submit` treats `commit_count == 0` as a
legitimate no-op (`worker.py:447-457`) and deletes the branch — silently discarding work an agent
performed but could not commit. The dirty-tree-at-`done` case already parks (`worker.py:439-445`)
but only as an stderr line, so an operator holding the repo cannot tell a parked-with-uncommitted-
work failure from a clean no-op without scraping logs. The tacit requirement a literal agent would
miss: "work present but uncommitted" is a *recoverable failure to surface*, not a no-op to swallow,
and the identity must be the worker's responsibility because the host may have none.

This stays inside the CLAUDE.md invariant "the worker never commits to the operator's branch":
commit integrity here is the *agent's in-sandbox commit* on its own task branch, the *worktree's
git identity*, and *dirty-tree detection at submit*. The worker still never authors a commit on the
base/operator branch — landing remains FF-merge of branches the agent committed.

## Scope
### In scope
- The worker establishes a deterministic git identity reachable by an in-sandbox `git commit`, so a
  commit in a freshly provisioned worktree succeeds even when no global/system git identity exists.
- At submit of a `done` run, the worker distinguishes "dirty tree / uncommitted work present" from
  "clean tree with zero commits beyond base" and preserves (parks) the worktree and branch in the
  uncommitted case rather than deleting them.
- The parked-uncommitted outcome is recorded as a distinguishable, externally queryable reason — a
  landing-parked domain event on the store ledger (D-6) with `park_kind=uncommitted-work`, queryable
  via `list_domain_events(run_id)`, without reading worker stderr or git state.
- The clean zero-commit no-op continues to be cleaned up (branch + worktree removed).

### Out of scope
- The worker authoring any commit on the base/operator branch (forbidden by CLAUDE.md; landing
  stays FF-merge of agent-committed branches).
- Configurable / protected base branch and detached-HEAD landing behavior (audit B3 — a separate
  spec).
- Distribution/installability (B1), agent auth docs (B4), bare-sandbox `[sandbox] setup` prompting
  (B5), Postgres read path (M1), cross-task queryability protocol (Q1-Q6).
- Submodule/LFS/sparse-checkout worktree completeness (M5).
- Changing the FF-merge / rebase / re-verify landing semantics themselves (only the no-op-vs-
  uncommitted classification at the head of `_submit` is in scope).

### Must not regress
- A clean `done` run with commits beyond base still FF-merges into the base and is cleaned up.
- A clean `done` run with zero commits beyond base is still cleaned up as a no-op (branch + worktree
  removed).
- A non-`done` terminal status (failed/interrupted) still parks the worktree for forensics.
- `submit` still never raises into `orchestrate` (a git failure is caught and the worktree left
  parked).
- The submit-time rebase + re-verify gate and protected-path refusal are unchanged.
- The worker still creates no commit on the base/operator branch.
- The existing `flywheel-worktree` test suite still passes.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type, visibility,
the exact check, and the gaming move it forecloses. `/fw-plan` lowers each one to a command /
transcript / rubric / manual grader.

1. When a task runs in a freshly provisioned worktree on a host with NO global or system git
   identity configured, a `git commit` of a working-tree change inside that worktree exits zero and
   produces a commit. [command | held-out]
   verify: held-out pytest in `packages/flywheel-worktree/tests/` that builds a repo whose worktree
   provisioning runs with `HOME`/`XDG_CONFIG_HOME` pointed at empty dirs and `GIT_CONFIG_NOSYSTEM=1`
   (no global/system identity resolvable), provisions a worktree via the worker's prepare path,
   writes a file, runs `git add -A && git commit -m x` inside the worktree with the same scrubbed
   environment, and asserts the commit exit code is 0 and `git -C <worktree> rev-list --count HEAD`
   increased by 1; run with `uv run pytest packages/flywheel-worktree/tests/<file> -k commit_identity`.
   defends against: the agent (or a narrow fix) hard-relying on the dev's ambient `~/.gitconfig` —
   the scrubbed environment removes the only thing that makes a commit succeed today, so a fix that
   does not set an identity the worktree can see fails the check; also forecloses "set identity only
   in the test's repo config" since the test scrubs that path.

2. While the worker provisions worktrees with the identity it establishes, the established
   committer/author name and email are deterministic and identical across two independently
   provisioned worktrees in the same repo (not derived from a random or per-run value). [command |
   held-out]
   verify: held-out pytest that, under the same scrubbed-identity environment as #1, provisions two
   worktrees, makes one commit in each, and asserts `git -C <wt> log -1 --format=%an|%ae|%cn|%ce` is
   byte-identical between the two worktrees and non-empty; `uv run pytest
   packages/flywheel-worktree/tests/<file> -k deterministic_identity`.
   defends against: faking #1 by injecting a random/UUID or timestamp identity per worktree (passes
   "a commit exists" but is non-deterministic and unattributable) — the equality assertion across
   two worktrees forecloses a per-invocation value.

3. When a `done` run is submitted with a dirty worktree (an uncommitted modified or untracked file
   present at submit time), the task branch and worktree both still exist after `submit` returns.
   [command | held-out]
   verify: held-out pytest that provisions a worktree, writes an uncommitted file, calls `submit`
   with `Status.DONE`, then asserts the worktree directory exists and `git -C <repo> show-ref
   --verify --quiet refs/heads/flywheel/<phase>/<task-id>` exits 0; `uv run pytest
   packages/flywheel-worktree/tests/<file> -k uncommitted_done_preserved`.
   defends against: a fix that "surfaces" the failure by logging but still calls `_cleanup`/`branch
   -D` (discarding the recoverable work) — the ref-exists assertion forecloses deleting the branch.

4. When a `done` run is submitted with a dirty worktree (uncommitted modified or untracked file at
   submit time), the worker appends the landing-parked domain event (SI-7) for that run with its
   `park_kind` field equal to the uncommitted-work value, queryable via the store API
   `list_domain_events(run_id)`. [command | held-out]
   verify: held-out pytest that, against a real store, triggers the dirty-tree `done` submit, then
   calls `store.list_domain_events(run_id)` and asserts exactly one event whose kind tag is the
   landing-parked discriminator (`DomainEventKind.LANDING_PARKED`, wire value `landing_parked`) and
   whose `park_kind` payload field equals the uncommitted-work value (`uncommitted-work`), and that
   the run's lifecycle `status` is still `Status.DONE` (no transition was performed); the grader reads
   only the store API, never git markers/refs/notes and never captured stderr; `uv run pytest
   packages/flywheel-worktree/tests/<file> -k uncommitted_park_event_on_ledger`.
   defends against: satisfying #3 by parking with no machine-readable distinction from a no-op
   (operator still can't tell why); the cheaper fakes of writing the reason only to stderr or a git
   marker (the grader reads neither — only `list_domain_events`); and minting a lifecycle status from
   the consumer (the `status == DONE` assertion forecloses a transition, holding D-1/D-6).

5. When a `done` run is submitted with a CLEAN worktree and zero commits beyond base, the worker
   removes the branch and worktree (the no-op disposition is unchanged) and appends NO landing-parked
   domain event for that run. [command | held-out]
   verify: held-out pytest that provisions a worktree, makes no commit, calls `submit` with
   `Status.DONE`, then asserts the worktree directory is gone, the task branch ref is absent
   (`git -C <repo> show-ref --verify --quiet refs/heads/flywheel/<phase>/<task-id>` exits non-zero),
   and `store.list_domain_events(run_id)` contains NO event of kind `landing_parked`; `uv run pytest
   packages/flywheel-worktree/tests/<file> -k clean_noop_no_park_event`.
   defends against: "fixing" #3/#4 by parking everything (never cleaning up) so the worker leaks a
   worktree per no-op task — the cleanup + zero-landing_parked-events assertions force the two cases
   to stay distinct on the same queryable surface c4 reads.

6. When a `done` run is submitted with a CLEAN worktree carrying one or more commits beyond base on
   a fresh, identity-less host, the branch FF-merges into the base and the worktree+branch are
   cleaned up. [command | held-out]
   verify: held-out pytest (under the scrubbed-identity environment of #1) that provisions a
   worktree, makes a real commit via the worker-established identity, calls `submit` with
   `Status.DONE`, and asserts `git -C <repo> rev-list --count <base>..HEAD` advanced by the commit
   count, the worktree is gone, and the task branch ref is absent; `uv run pytest
   packages/flywheel-worktree/tests/<file> -k clean_commit_ff_merges`.
   defends against: an identity fix that lets commits happen but breaks the existing FF-merge/cleanup
   happy path (regression masked because no test exercises commit + merge on a scrubbed host).

7. If `submit` encounters a git failure during the dirty-tree classification or park-reason write,
   then it does not raise into `orchestrate` and the worktree is left present for inspection.
   [command | visible]
   verify: pytest that injects a failing `git` (monkeypatched to non-zero on the relevant call) for
   a `done` submit and asserts `submit` returns without raising and the worktree directory still
   exists; `uv run pytest packages/flywheel-worktree/tests/<file> -k submit_never_raises`.
   defends against: the new park-reason write path throwing and unwinding the orchestrator (which
   would abandon peer tasks under the same cycle) — the no-raise assertion pins the invariant.

8. The existing `flywheel-worktree` verification suite still runs and still passes after the change.
   [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-worktree/tests` collects more than zero tests and exits
   0; the count of collected tests is greater than or equal to the pre-change count.
   defends against: making the new criteria pass by deleting or weakening the existing park/no-op/
   merge tests (e.g. removing `test_submit_zero_commit_done_cleans_up` or
   `test_submit_uncommitted_done_parks`) instead of preserving their guarantees.

9. Any existing check this feature relaxes or removes is named explicitly with an equal-or-greater
   replacement, and new behavior is proven by a held-out check the implementing agent did not author
   against its own known inputs. [manual | held-out] (verification-surface)
   verify: operator confirms at the merge gate that no assertion in `test_submit_zero_commit_done_
   cleans_up` or `test_submit_uncommitted_done_parks` was deleted without a named, equal-or-stronger
   replacement among criteria #3-#6, and that the identity/park-reason checks (#1, #2, #4) are
   graded by held-out tests run out-of-band, not by tests the agent wrote against its own fixture.
   defends against: a removed assertion with no named replacement (a blocking defect), and the agent
   authoring the only test that judges its own identity/park-reason work.

Verification surface: this feature touches the landing/submit machinery AND `flywheel_core.events`
(per D-6 it adds the `LANDING_PARKED` domain-event kind + `event_serde` round-trip — SI-7), and adds
new graded behavior (in-sandbox commit success, dirty-tree classification, the landing-parked ledger
event). Therefore: the existing suite still passes (#8) and now spans `flywheel-core` too — the
`events.py` purity test (`test_events_module_purity.py`) and the event-serde round-trip tests must
stay green, and `schema_version` must remain 12 (the new kind is additive opaque JSON, no migration);
any relaxed or removed check is named with an equal-or-greater replacement and a removed assertion
with none is a blocking defect (#9); and the highest-stakes new behaviors (#1, #2, #3, #4, #6) are
proven by held-out command checks the implementing agent did not author against its own known inputs
(the scrubbed-identity environment and the store-queryable-only grader — reading `list_domain_events`,
not git state or stderr — are specified here, not left to the agent's own fixtures).

## Decomposition Hint (for /fw-plan)
Splits along the two independent failure modes, both inside `worker.py`'s submit/provision surface;
they share one worker test module, so size them so the second inherits a green suite from the first.
- Layer A — deterministic git identity (worktree provisioning / prepare path): satisfies #1, #2,
  and the fresh-host half of #6. The seam: the worker must make a committer identity reachable by an
  in-sandbox `git commit` without a global/system config; the agent process and any in-worktree git
  call must see it.
- Layer B — dirty-tree classification + landing-parked domain event (submit path, head of `_submit`,
  plus a new `flywheel_core.events` kind): satisfies #3, #4, #5, #7; depends on the no-op-vs-
  uncommitted branch staying coherent with Layer A's commits. Layer B must NOT delete the branch in
  the uncommitted case, must keep the clean no-op cleanup, and on the dirty-tree path appends the
  landing-parked domain event (SI-7) to the store ledger with `park_kind=uncommitted-work`. This
  layer's blast radius spans `flywheel-core` (the new event kind + serde round-trip) and
  `flywheel-worktree` (the append at submit) — size it accordingly; the core change lands the SI-7
  contract 00026 will consume.
- Layer C — regression + verification-surface: satisfies #6 (merge half), #8, #9; depends on A and
  B. Asserts the FF-merge/cleanup happy path survives on a scrubbed host and the existing suite is
  intact.

Shared invariants multiple layers assert against (name them so dependent tasks update together):
- **SI-7** (REDEFINED 2026-06-17 per D-6 — was a git-observable marker, now a domain-event kind) —
  the landing-parked `DomainEvent` kind and its `park_kind` vocabulary, a `flywheel_core.events`
  addition OWNED here. Concretely: a new `DomainEventKind.LANDING_PARKED` (wire value `landing_parked`)
  + a frozen `LandingParked(_DomainEventBase)` dataclass carrying a `park_kind: str` discriminator
  (and an optional human `detail: str`), added to the `DomainEvent` union and folded by the identity
  branch of `events.apply` (no lifecycle-state mutation); the `event_serde.py`
  `event_payload`/`event_from_record` round-trip; and persistence on the existing opaque-JSON `events`
  ledger (no schema bump — SI-9 holds). The `park_kind` vocabulary is `uncommitted-work` (this spec)
  and `divergent-base` (00026). #3, #4, #5 all assert presence/absence of the SAME event kind on the
  SAME store surface (`list_domain_events(run_id)`); it must be one named constant, not two. This is
  the SAME surface 00026's divergent-base outcome (SI-6) uses — by design, both park types unify on
  one ledger event kind discriminated by `park_kind`; they share the kind and MUST agree on the wire
  contract, but are distinguished by the `park_kind` value, not by separate namespaces. (This
  supersedes the prior SI-7, which kept 00027's marker on git state distinct from 00026's store
  outcome.)
- **SI-6** — the worker `_submit` seam: this spec owns the HEAD (classification + identity), spec
  00026 owns the TAIL; 00027 lands first as 00026's prerequisite (D-5).
- The scrubbed-identity test environment (empty `HOME`/`XDG_CONFIG_HOME`, `GIT_CONFIG_NOSYSTEM=1`) —
  #1, #2, #6 share it; it must be a single shared fixture so all three exercise the identical
  no-ambient-identity condition.
- The worker-established committer/author name+email constants — #1, #2, #6 depend on them being
  fixed and deterministic.

Cross-spec coupling (RECONCILED 2026-06-17 — see D-5): this spec shares `worker.py`'s `_submit`/
landing path with spec 00026. **00027 lands first; 00026 depends on it (SI-6).** 00027 owns the HEAD
of `_submit` — the porcelain dirty-tree-vs-no-op classification at `worker.py:439-457` and the
worktree provisioning/prepare path (identity). It does NOT change the FF-merge (`_ff_merge`
`worker.py:592-595`), rebase, re-verify, protected-path logic, base resolution (`_phase_base`
`worker.py:885-892`), or the divergent-base park (`worker.py:478-502`) — those are 00026's TAIL.
Per D-6 (2026-06-17, superseding D-2): the uncommitted-work park this spec writes is the landing-parked
domain event on the store ledger (SI-7), NOT a git-observable marker. 00026's divergent-base park
reuses the SAME event kind with `park_kind=divergent-base`; the two park types unify on one ledger
event discriminated by `park_kind`, so 00027 OWNS and lands the kind (SI-7) and 00026 CONSUMES it.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Commit integrity = agent's in-sandbox commit + worktree identity + dirty-tree detection, NOT worker-authored base commits  (Status: Accepted)
- Context: CLAUDE.md states "the worker never commits to the operator's branch." The audit's B2
  "worker never commits" could be misread as "make the worker commit." | Decision: scope the fix to
  (a) establishing a git identity the agent's in-sandbox commit can use, (b) detecting a dirty tree
  at submit, (c) preserving + surfacing the uncommitted case. The worker authors no commit on the
  base; landing stays FF-merge of branches the agent committed.
- Rejected: having the worker run `git add`/`git commit` on the agent's behalf — would have the
  worker author work-bearing commits, blurring the trust boundary (agent claims are untrusted; the
  worker would be fabricating the artifact it then lands). Rejected for violating the invariant and
  the trust model. | Consequences: an agent that genuinely never writes a commit and leaves a dirty
  tree is surfaced as a recoverable park, not auto-committed; recovery (commit + retry) stays the
  agent's job on resume.

### D-2: Surface the uncommitted-work failure as git-observable state, not a lifecycle mutation  (Status: Accepted)
- Context: the audit asks for a "recoverable, queryable failure." The worker's submit runs in the
  worker process; the lifecycle/store transitions are owned by the harness (agent claims are
  untrusted, the harness owns transitions), and the run already finalized `done` before submit. |
  Decision: the worker records the uncommitted-work reason on a git-observable surface tied to the
  preserved branch/worktree (a marker file or a `refs/flywheel/...` note — the implementing layer
  picks one named constant), queryable from the repo alone. The worker does not rewrite the
  authoritative lifecycle status.
- Rejected: mutating the lifecycle row to a failed status from submit — submit is not a lifecycle
  authority and runs after finalization; co-opting it would put transition logic in the consumer,
  across the orchestrator/core line. | Consequences: "queryable" is satisfied at the git/worktree
  grain (the surface this spec actually changes); a richer store-level cross-task query is the
  separate queryability program (Q1-Q6), explicitly out of scope.

### D-3: Identity must be reachable WITHOUT a global/system gitconfig  (Status: Accepted)
- Context: the failure only manifests on a fresh host with no ambient identity; the current suite
  passes only because the dev's `~/.gitconfig` is set. A test that sets repo-local identity in its
  own fixture would not reproduce the bug. | Decision: the authoritative held-out checks (#1, #2,
  #6) run under a scrubbed environment (empty `HOME`/`XDG_CONFIG_HOME`, `GIT_CONFIG_NOSYSTEM=1`) so
  the only identity present is the one the worker establishes.
- Rejected: trusting `uv run pytest` as-is (ambient identity present) to prove the fix — it cannot
  fail for the audited reason, so it cannot certify the fix. | Consequences: tests must construct
  and pass a scrubbed environment; this is the cost of making the check actually bind to the
  audited end-state.

### D-4: No-op classification keyed on tree-dirtiness + commit count, surfaced distinctly  (Status: Accepted)
- Context: today `commit_count == 0` is the no-op path and the dirty-tree check parks but only logs;
  the two outcomes are not externally distinguishable. | Decision: dirty tree at `done` ->
  preserve + write the uncommitted-work marker (recoverable failure); clean tree with zero commits
  -> clean no-op (remove branch+worktree, no marker). The two are distinguishable from git state
  alone.
- Rejected: collapsing both into "always park" (leaks a worktree per genuine no-op) or "always
  clean up" (current behavior, discards recoverable work). | Consequences: a clean no-op still
  self-cleans; only the work-present case is preserved.

### D-5: Reconciliation (2026-06-17) — 00027 lands before 00026 on the shared `_submit` seam; marker surface is SI-7  (Status: Accepted)
- Context: 00027 and 00026 both edit `worker.py`'s `_submit`. The pre-reconcile coupling note left the
  order undeclared. Both also touch park-disposition surfaces, risking a collision on one marker/ref
  namespace.
- Decision: **00027 is a prerequisite of 00026** (SI-6). 00027 rewrites the HEAD of `_submit` (the
  `git status --porcelain` dirty-tree check and the `commit_count == 0` no-op branch at
  `worker.py:439-457`) and the worktree-provisioning identity; 00026 then rewrites the TAIL (base
  resolution/fetch, `_ff_merge`, the divergent-base park). Landing 00027 first means 00026 inherits a
  green, already-split classification gate instead of racing the same function. The uncommitted-work
  marker (SI-7) and 00026's store-queryable divergent-base outcome (SI-6) are explicitly distinct
  surfaces: 00027 writes a git-observable marker on the preserved worktree; 00026 records a lifecycle
  status/reason in the store. They must not share a marker/ref namespace.
- Rejected: ordering 00026 first (its TAIL changes assume the pre-00027 no-op semantics and would
  conflict-merge with 00027's HEAD rewrite); routing 00027's uncommitted-work reason through the same
  store-outcome surface 00026 adds (would couple two independent failure modes and put lifecycle
  mutation in the consumer, against D-2 and the orchestrator/core line).
- Consequences: `/fw-plan` for 00026 emits a cross-spec prerequisite on 00027's `_submit`-classification
  task; this spec carries no new prerequisite (it is the upstream of the pair).

### D-6: Park outcomes are a store domain-event append, NOT a git-observable marker — supersedes D-2  (Status: Accepted, 2026-06-17)
- Context (operator decision 2026-06-17): "parked at submit" must be a single, unified, queryable
  outcome across BOTH park types — 00027's uncommitted-work park and 00026's divergent-base park —
  so an operator (and a future cross-task query) sees one timeline of landing failures rather than a
  git marker for one and a store row for the other. Reading `flywheel_core.events` /
  `event_serde.py` and the SQLite/Postgres `events` ledger (`append_domain_event` /
  `list_domain_events`, `store_protocols.py:357-364`) confirmed the ledger stores each event as
  opaque JSON (`events.payload_json TEXT` keyed by a `kind TEXT` discriminator), and that the
  identity-fold branch of `events.apply` already admits audit-witness events that record a fact
  without mutating lifecycle state.
- Decision: the worker records both park outcomes by appending a NEW domain event (the landing-parked
  kind, SI-12 / SI-7) to the store ledger via `append_domain_event`, queryable through
  `list_domain_events(run_id)`. A `park_kind` field on that event discriminates `uncommitted-work`
  (00027) from `divergent-base` (00026). NO new lifecycle `Status` is added; the run stays terminal
  `DONE`; the worker performs NO lifecycle transition — it appends a single audit-witness domain
  event whose fold is the identity. Because the ledger is generic opaque-JSON storage, the new kind
  needs no migration and `schema_version` stays at 12 (SI-9 holds). `flywheel_core.events` stays pure
  (a frozen dataclass with primitive fields, no json/pathlib/io); `event_serde.py` gains the
  payload/round-trip; `_project_domain_event` is unchanged (identity fold, no read-model projection).
- Supersedes: **D-2** (git-observable marker file / `refs/flywheel/...` note). D-2's text is retained
  unedited and is now superseded — the uncommitted-work reason is NOT written to a git surface and is
  NOT read from git state; it lives on the store ledger. (00026's D-4, "reuse existing lifecycle
  status vocabulary," is superseded by the matching D-6 entry in 00026: no Status is reused or added;
  the outcome is a domain event.)
- Rejected: a git-observable marker per D-2 (forces an operator to hold the repo and scrape two
  different surfaces for two park types; not unifiable with 00026's store-side outcome); a new
  lifecycle `Status`/`Outcome` (the run already finalized `DONE` before submit, the harness owns
  transitions, and `DONE` is terminal — minting a status from the consumer crosses the
  orchestrator/core line, exactly D-2's original rejection); a schema migration to add a typed column
  (unnecessary — the ledger is opaque JSON, so the kind is additive).
- Consequences: "queryable" is now satisfied at the store grain via `list_domain_events(run_id)`
  rather than the git grain, so the held-out checks for c4/c5 read the store API, never git markers or
  stderr. The new core change (events.py + event_serde round-trip + ledger persistence) expands this
  spec's blast radius beyond `flywheel-worktree` into `flywheel-core`; the verification surface grows
  accordingly (see SI-7). 00026 consumes the IDENTICAL event contract with `park_kind=divergent-base`.

## Open Questions (accepted gaps)
None. Every criterion lowers to a command grader except #9 (manual, paired with the held-out command
checks #1-#6 and #8), which encodes the verification-surface Definition-of-Done that an operator
must confirm at the merge gate. The D-6-rewritten #4 and #5 lower cleanly to held-out pytest command
graders that assert against the store API `list_domain_events(run_id)` (event kind `landing_parked`,
`park_kind` field) — no git-state or stderr scraping, so the grade is deterministic and out-of-band.

## Next Steps
Run `/fw-plan 00027-FEATURE-worktree-commit-integrity` to compile these criteria into flywheel tasks
and graders.
