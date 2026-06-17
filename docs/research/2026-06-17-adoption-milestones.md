# Adoption Milestones — Spec/Plan/Verify Pipeline Output

**Generated** 2026-06-17 by the SDD pipeline (`/fw-spec` → reconcile → `/fw-plan` → reconcile → `/fw-verify`). **Status: DRAFT for author review.** Every milestone groups the seven specs that turn the 2026-06-17 adoption-readiness audit findings into runnable tasks with held-out oracles. All paths are absolute; file:line references point at the live tree at authoring time.

Milestone map (audit finding → spec):
- **M1 — Local adoption viability**: B3→00026, B2→00027, B4+B5+G7→00028
- **M2 — Distribution**: B1→00029
- **M3 — Observability & Postgres parity**: Q1→00030, Q2+Q3→00031, M1→00032

Totals: **7 specs · 18 tasks · 71 criteria (61 held-out) · 24 held-out oracles admitted · 4 held-out behaviors blocked on an undeclared seam (00026 c5, 00027 c4/c5: store-injection point; 00026 c2/c3/c4: drivable base-resolution entrypoint).** Updated by the sharpening pass (round 2, 2026-06-17) — see the dated note below.

---

## M1 — Local adoption viability

### Spec 00026 — Safe Landing Target
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00026-FEATURE-safe-landing-target.md`
- Criteria: **8 total, 7 held-out** (7/8). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/26-safe-landing-target/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `land-config-submit-base` | — | `submit-base-config-behavior` (cmd/visible, c6); `policy-and-init-seam-holdout` (rung-4 composition over `_INIT_POLICY_TAIL`+`load_policy`) |
| `land-resolve-base-from-policy` | `land-config-submit-base` | `detached-head-lands-on-configured-base-held-out` (c3, **manual upstream**); `refuse-checked-out-base-held-out` (c2, **manual upstream**); `fetch-fresh-base-before-land-held-out` (c4, **manual upstream**); `must-not-regress-gate-holdout` (c8); `no-tests-dropped-holdout` (c7, baseline 411) |
| `land-ref-update-and-queryable-park` | `land-resolve-base-from-policy`, **`commit-dirty-tree-park-classification` (cross-spec, 00027)** | `operator-tree-byte-unchanged-held-out` (held-out c1, load-bearing); `divergent-base-park-event-held-out` (c5, **manual — store-injection seam undeclared**); plus the two regression holdouts |

Held-out grader paths now follow the 00027 convention (`uv run pytest .flywheel/holdout/00026-safe-landing-target/ -k <kw>`). The c2/c3/c4 grader keywords were re-pointed off stale `packages/flywheel-worktree/tests/test_worker.py` references onto the holdout dir; c5's keyword was renamed `divergent_base_records_queryable_outcome → divergent_base_records_park_event` and its brief re-pointed from the superseded D-4 lifecycle-status language to the D-6 `LANDING_PARKED` domain event (`park_kind="divergent-base"`, non-empty `detail`, run stays terminal `DONE`, no transition, schema stays v12).

**Held-out oracles authored (1 admitted; c5 routed AUTHOR but returned under-specified; c2/c3/c4 manual upstream):**
- c1 `land-ref-update-and-queryable-park` → `/home/johnayoung/code/flywheel/.flywheel/holdout/00026-safe-landing-target/test_landing_does_not_touch_operator_worktree.py`. Killed 4/4 wrong references: `merge_ff_in_repo_root`, `merge_then_checkout_base`, `reset_base_to_tip_in_tree` on the Half-1 operator-HEAD assertion; `skip_merge_entirely` on the Half-2 base-advanced assertion (both halves jointly load-bearing). Flake screen pass/pass/pass. **Grader targets the holdout path, not `packages/flywheel-worktree/tests/test_worker.py`.** This is the canonical pattern the c2/c3/c4/c5 graders must follow (build a real `GitWorktreeSubmitter` over `repo_root`, `prepare_sandbox`, play a commit, drive `submit`/landing, assert git ref/worktree/store end-state).
- c5 `divergent-base-park-event` — routed **AUTHOR** (the held-out behavior is sound: drive the real `GitWorktreeSubmitter` against a real store, construct a post-rebase-FF-impossible divergence, `submit(status=DONE)`, then assert `list_domain_events(run_id)` has exactly one `LANDING_PARKED` with `park_kind=="divergent-base"` + non-empty `detail`, lifecycle still `DONE`, worktree+branch preserved). **Returned UNDER-SPECIFIED on authoring:** the contract does not declare HOW the real `SqliteStore` is connected to the submitter — the only wiring fact every assertion depends on. The declared kw-only `GitWorktreeSubmitter` constructor (`repo_root, tasks_dir, worktrees_dir, phase_base, lock_path, log`) names no `store` parameter; neither `SandboxRequest` nor `SubmitRequest` carries a store. *(Action: pin the store-injection point — a `store=` kwarg, a setter/attribute, or a derivable path — on the worktree submit seam; then re-run `/fw-verify` for c5. Same gap as 00027 c4/c5.)*
- c2/c3/c4 `land-resolve-base-from-policy` — routed **MANUAL (under-specified upstream)**. The end-states are pinnable (configured-base SHA unchanged; refusal observable; detached-HEAD survives at its original SHA; a remote-only commit is an ancestor only if a fresh fetch fed the landing), but the contract names the c1 oracle as the drive reference and c1 passes a **literal** `phase_base` and runs **no** base resolution/fetch. The only resolution path today is the forbidden `_phase_base()` inside `main()`; there is no public, drivable policy-driven base-resolution seam. *(Action: name the observable base-resolution entrypoint and land `WorkPolicy.submit_base` (the `land-config-submit-base` prereq) before authoring blind. The "inverse fence" gap `/fw-verify` surfaced.)*

### Spec 00027 — Worktree Commit Integrity
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00027-FEATURE-worktree-commit-integrity.md`
- Criteria: **9 total, 8 held-out** (8/9). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/27-worktree-commit-integrity/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `commit-worktree-identity` | — | `commit-identity-held-out` (c1); `deterministic-identity-held-out` (c2); `worktree-suite-intact` (regression) |
| `commit-dirty-tree-park-classification` | `commit-worktree-identity` | `uncommitted-done-preserved-held-out` (c3); `uncommitted-park-event-on-ledger-held-out` (c4, **manual — store-injection seam undeclared**); `clean-noop-no-park-event-held-out` (c5, **manual — same**); `landing-parked-event-core-roundtrip` (cmd, NEW); `submit-never-raises` (cmd/visible c7); `worktree-suite-intact` |
| `commit-scrubbed-host-merge-and-suite` | `commit-worktree-identity`, `commit-dirty-tree-park-classification` | `clean-commit-ff-merges-held-out` (c6, composition); `worktree-suite-intact-and-floor` (c8); `full-suite-green`; `verification-surface-merge-gate` (manual c9, now confirms c4/c5 via `store.list_domain_events`) |

**Held-out oracles authored (4 admitted; c4/c5 routed AUTHOR but returned under-specified):**
- c1 → `.../holdout/00027-worktree-commit-integrity/test_commit_identity.py`. Killed `no_identity_established` (= current real impl, exit 128) and `identity_only_in_worker_subprocess_env`; `identity_written_to_global_config_path` tolerated as behaviorally-equivalent under the scrubbed-HOME env (documented).
- c2 → `.../test_deterministic_identity.py`. Killed per-worktree uuid + per-provisioning timestamp on `quad1 == quad2`; empty/unset on git commit exit 128.
- c3 → `.../test_uncommitted_done_preserved.py`. Killed `log_then_cleanup`, `remove_dir_keep_branch`, `dirty_done_as_clean_noop` on `wt.exists()`; the faithful `delete_branch_keep_dir_detach` on the show-ref branch-ref assertion (independent discriminating power).
- c6 → `.../test_clean_commit_ff_merges.py`. Killed no-identity (exit 128), ff-merge-skipped (delta 0), merge-without-cleanup (worktree survives), squash (exact-delta N=2).
- c4 `uncommitted-park-event-on-ledger` and c5 `clean-noop-no-park-event` — both routed **AUTHOR** (fully-pinned observable contracts: a dirty-tree DONE submit yields exactly one `landing_parked` event with `park_kind=="uncommitted-work"` AND lifecycle status still `DONE`; a clean zero-commit DONE submit removes worktree+branch AND appends ZERO `landing_parked` events; both read only through `store.list_domain_events(run_id)`, never git markers/refs/notes or stderr). **Both returned UNDER-SPECIFIED on authoring:** the contract's documented store-injection point — `GitWorktreeSubmitter(store=<real store>)` — does not exist. The real `GitWorktreeSubmitter.__init__` is kw-only (`repo_root, tasks_dir, worktrees_dir, phase_base, lock_path, log, protected_paths, setup_command`) with no `store` param and no member matching `store`; `SubmitRequest`/`SandboxRequest` carry no store either. Because the grader must read the result via the SAME store the submitter writes to, and the contract forbids guessing another wiring shape, neither c4 nor c5 can be pinned blind (c5's two git-state assertions alone do not discriminate the "cleans up but ALSO parks" bug — only the store-ledger clause does). *(Action: add a kw-only `store=` injection point (or equivalent named seam) to the worktree submit surface, then re-run `/fw-verify` for c4/c5. Identical root gap to 00026 c5.)*
- **NEW command grader `landing-parked-event-core-roundtrip`** on `commit-dirty-tree-park-classification`: `uv run pytest packages/flywheel-core/tests/test_events.py packages/flywheel-core/tests/test_events_module_purity.py packages/flywheel-core/tests/test_store_contract.py` — exercises the new `LANDING_PARKED` kind apply/identity-fold + `event_serde` round-trip + cross-backend `append_domain_event`/`list_domain_events` contract on the v12 opaque-JSON ledger. This gates the SI-12 core change (owned by 00027) independently of the still-blocked store-injection oracle.

### Spec 00028 — Onboarding Preflight & Auth
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00028-FEATURE-onboarding-preflight-and-auth.md`
- Criteria: **11 total, 8 held-out** (8/11). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/28-onboarding-preflight-auth/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `onboard-init-git-preflight` | — | `visible-message-and-happy-path` (c2,c4); `held-out-preflight-refusal` (c1,c3); `seam-holdout-init-policy-suites` (composition) |
| `onboard-sandbox-scaffold` | `onboard-init-git-preflight` | `held-out-sandbox-render-roundtrip-and-never-clobber` (c5,c6,c8); `seam-holdout-sandbox-default-and-parse`; `seam-holdout-init-policy-suites` |
| `onboard-auth-docs` | `onboard-init-git-preflight` | `visible-readme-auth-reference` (c7a grep); `held-out-next-steps-auth` (c7b); `seam-holdout-init-suite` |

**Held-out oracles authored (6 admitted):** all under `.../holdout/00028-onboarding-preflight-and-auth/`
- c1 `test_git_preflight.py` — killed `warn_and_continue_writes_file`, `refuse_after_write` (file-absent clause), `no_preflight_today`; non-git input proven via `GIT_CEILING_DIRECTORIES`.
- c3 `test_detached_head.py` — killed `wrong_git_only`, `wrong_opaque` (message clause), `wrong_writes_first` (file clause), `wrong_defers`.
- c5 `test_sandbox_section_rendered.py` — killed `no_sandbox`, `header_only_no_setup`, `setup_empty_value`, `setup_other_command`.
- c6 `test_sandbox_setup_uncomment_parses.py` — round-trip `sandbox_setup == "uv sync"`; killed `wrong_value`/`wrong_key`/`miscased`.
- c7 `test_next_steps_auth.py` — killed `omit_auth`, `auth_in_readme_only`, `vague_auth`, `auth_on_stderr` (stdout pin load-bearing).
- c8 `test_existing_policy_untouched.py` — byte-identity + stdout-report halves both load-bearing; killed all 4.

---

## M2 — Distribution

### Spec 00029 — Distributable Install
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00029-FEATURE-distributable-install.md`
- Criteria: **10 total, 8 held-out** (8/10). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/29-distributable-install/`
- **Independent: no cross-spec edges** (touches only `packages/*/pyproject.toml`, workspace source-table, build helper — surfaces no other spec edits).

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `dist-version-constraints` | — | `downstream-metadata-is-versioned` (c5); `core-declares-no-downstream-dep` (c7); `in-repo-uv-sync-and-help` (c8); `core-imports-without-sdk` (c6, existing `test_sdk_optional.py`); `full-suite` (c9); `verification-surface-review` (manual c10) |
| `dist-foreign-install` | `dist-version-constraints` | `foreign-install-and-run-held-out` (c1-c4, out-of-band); `build-helper-produces-artifact-set`; `full-suite`; `verification-surface-review` (manual c10) |

**Held-out oracles authored: 0.** All criteria are structural/state checks already graded un-gameably by command (METADATA scans, exit codes, filesystem/schema-presence) — VERIFY routed c1-c4 and c5-c9 to SKIP and c10 to MANUAL. No `.flywheel/holdout/00029-*` dir exists (correct). The c1-c4 "foreign install" command grader (`uv run pytest .flywheel/holdout/00029-distributable-install/ -q`) referenced in the plan is a **command end-state** check the implementing agent realizes; it is not a `/fw-verify`-authored behavior oracle.

---

## M3 — Observability & Postgres parity

### Spec 00030 — Cross-Task Query Surface
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00030-FEATURE-cross-task-query-surface.md`
- Criteria: **12 total, 11 held-out** (11/12). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/30-cross-task-query-surface/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `query-list-lifecycles-backends` | — | `behavior-held-out` (c1,c2,c4,c5,c9); `store-contract-parity` (c4 composition); `store-protocols-purity`; `full-suite`; `relaxed-check-review` (manual) |
| `query-migrate-orchestrator-reads` | `query-list-lifecycles-backends`, **`agg-worker-id-live-surface` (cross-spec, 00031)** | `no-raw-lifecycle-select-held-out` (c7); `orchestrator-history-workflow-seam` (c8); `full-suite`; `relaxed-check-review` (manual) |

**Held-out oracles authored (6 admitted — c3 now closed by D-7):** all under `.../holdout/00030-cross-task-query-surface/`
- c1 `test_list_lifecycles_status_filter.py` — killed `ignores_filter_returns_all`, `hardcoded_status_echo`, `returns_only_first_match`, `returns_complement_set`.
- c2 `test_list_lifecycles_no_filter_full_fold.py` — killed `active_rows_only` (membership), `stub_rows_empty_attempts` (model-equivalence), `first_row_only`, `drops_zero_attempt_runs`.
- c3 `test_list_lifecycles_task_id_order.py` — **NEW, admitted.** D-7 (2026-06-17) recast c3 from the unauthorable injected-`updated_at` forced tie into a metamorphic-determinism check: two identical `list_lifecycles(task_id=...)` calls must return a byte-identical `run_id` sequence (relation a); a later store-stamped `update_lifecycle` write sorts first under `(updated_at DESC, run_id DESC)` (relation b, recency from store clock); the `run_id DESC` tiebreak (relation c) is asserted **opportunistically** — only when two rows read back with equal store-assigned `updated_at`, falling through to (b) otherwise so it never flakes on a strict-monotonic clock. CORRECT passes; five wrong refs die under a tie-producing clock; flake screen pass/pass on both clock regimes; real `InMemoryStore` lacks `list_lifecycles` so the runner fails pre-implementation (non-vacuous). Picked up automatically by the dir-level `behavior-held-out` grader (whole dir now collects 19, was 11).
- c4 `test_list_lifecycles_backend_parity.py` — Postgres reachable, all three legs ran; killed `postgres_not_implemented` (M1), `postgres_status_filter_case_divergent`, `sqlite_drops_attempts_in_fold`, `memory_missing_row_off_by_one_filter`.
- c5 `test_list_lifecycles_store_only_no_worksource.py` — store-only enumeration, no WorkSource; killed 4/4.
- c7 `test_orchestrator_lifecycle_select.py` — table-scoped source scan; killed no-op (un-migrated `FROM lifecycles`) in both `_history.py` and `_workflow.py`; tolerated the D-4 `FROM attempts` read and commented-out blocks. **Operator note:** oracle also flags `_workflow.py:1303` (`_list_blocked_lifecycles`), a lifecycle SELECT not in the task's migration notes — the implementing agent must migrate it too.

### Spec 00031 — Spend & Worker Aggregates
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00031-FEATURE-spend-and-worker-aggregates.md`
- Criteria: **11 total, 7 held-out** (7/11). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/31-spend-worker-aggregates/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `agg-spend-summary` | — | `held-out-spend-contract` (c1,c2,c4,c11); `store-contract-seam-holdout` (c4); `spend-backends-and-purity` (c3); `full-suite-green` |
| `agg-list-claims` | — | `held-out-claim-enumeration` (c5,c11); `claims-contract-seam-holdout` (c6); `full-suite-green` |
| `agg-worker-id-live-surface` | — | `held-out-worker-id-surface` (c7,c8,c11); `live-and-status-seam-holdout` (c9); `full-suite-green` |

**Held-out oracles authored (5 admitted):** all under `.../holdout/00031-spend-and-worker-aggregates/`
- c1 `test_spend_cross_run_sum.py` — Postgres ran (Docker reachable); killed `single_run_only`, `latest_run_only`, `hardcoded_constant`, `cache_fields_swapped` (per-field cache assertion).
- c2 `test_spend_window.py` — killed `ignores_window`, `upper_inclusive`, `lower_exclusive`, `empty_falls_back` (half-open `[since,until)` on `attempts.last_activity_at`).
- c5 `test_list_claims_pairs.py` — set-equality over `(task_id, worker_id)`; killed `only_first`, `worker_blank`, `scrambled` (rotated pairs), `count_recon`.
- c7 `test_live_snapshot_worker_id.py` — killed `worker_id_never_populated`, `worker_id_hardcoded_sentinel`, `worker_id_coerced_to_empty`; `worker_id_from_wrong_lifecycle` tolerated as equivalent under single-row seed.
- c8 `test_status_json_worker_id.py` — key-name-agnostic; killed `human_text_only`, `omitted`, `malformed` (parse guard), `hardcoded` (literal-id assert).

### Spec 00032 — Postgres Read-Path Parity
- Path: `/home/johnayoung/code/flywheel/.flywheel/specs/00032-FEATURE-postgres-read-path-parity.md`
- Criteria: **10 total, 10 held-out** (10/10). Verification-surface touched.
- Phase dir: `.flywheel/tasks/active/32-postgres-read-path-parity/`

| Task | Prereqs | Graders (rung) |
|---|---|---|
| `pg-read-path-port` | **`query-migrate-orchestrator-reads` (00030), `agg-spend-summary` (00031), `agg-list-claims` (00031), `agg-worker-id-live-surface` (00031)** — all HARD | `history-parity-holdout` (c1); `live-rows-parity-holdout` (c2); `latest-lifecycle-parity-holdout` (c3); `optional-extra-import-boundary` (c7); `read-path-suites-stay-green` |
| `pg-factory-seam` | `pg-read-path-port` | `seam-returns-usable-postgres-holdout` (c4); `no-dsn-fail-fast-preserved` (c6, existing); `factory-suite-green-with-replacement-assertion` (c9) |
| `pg-status-verb-end-to-end` | `pg-factory-seam` | `status-verb-postgres-end-to-end-holdout` (c5); `full-suite-composition-holdout` (c8) |

**Held-out oracles authored (5 admitted — c1 now closed by D-6):** all under `.../holdout/00032-postgres-read-path-parity/`
- c1 `test_history_parity.py` — **NOW ADMITTED.** D-6 (2026-06-17) bound the history read's terminal-run enumeration to the canonical SI-3 seam `list_lifecycles(statuses=TERMINAL_STATUSES)` (reusing the existing `(DONE, FAILED, FAILED_VALIDATION)` tuple in `_history.py`), replacing the SQLite-only `store._connection` SELECT at `_history.py:272` (`_select_lifecycles`). That made a correct cross-backend reference writable: the oracle is now a pure metamorphic parity check (no golden literals) — seed identical write-protocol state including ≥1 non-terminal lifecycle on both backends, then assert (i) the returned terminal `run_id` set equals exactly the seeded terminal set on BOTH backends (non-terminal absent on each) AND (ii) field-for-field-equal `HistoryRun` rows (`run_id, task_id, status, attempts, tokens_total, cost_usd_total, turns_total`) across backends. Postgres reachable; CORRECT passes (flake screen pass/pass); killed WRONG-A (dropped `TERMINAL_STATUSES` filter — non-terminal leaks), WRONG-B (truncated enumeration), WRONG-D (the exact live `store._connection` bug — `AttributeError` on the Postgres arm). WRONG-C (identically-zeroed rollups on both backends) tolerated as an equivalent mutant by design — the test is literal-free per the spec's defends-against. The shipped file run via its real grader (`-k "history and parity"`) currently FAILS the un-ported impl with `'PostgresStore' object has no attribute '_connection'` and will pass once the read is ported.
- c2 `test_live_rows_parity.py` — killed `sqlite_only_private_connection` (primary, AttributeError on PG arm), `present_but_empty_rollups`, `wrong_breadcrumb_first_attempt`; `returns_terminal_runs_too` tolerated as equivalent under the single DONE seed (stronger leak-all variant killed).
- c3 `test_latest_lifecycle_parity.py` — killed the shipped private-`_connection` impl on the real Postgres arm; correct ref green on both backends.
- c4 `test_factory_seam_postgres.py` — killed `refuses_with_widened_message` (no-raise guard), `returns_closed_store` (`PoolClosed` on usability read), `returns_none_for_postgres` (isinstance gate), `falls_back_to_sqlite`.
- c5 `test_status_verb_postgres.py` — killed `still-refuses`, `uncaught-bubble` (unhandled AttributeError), `swallow-and-force-zero` (no-traceback assertion).

---

## SHARED-INVARIANT REGISTER

| SI | Surface | Canonical decision | Specs |
|---|---|---|---|
| SI-1 | `[submit] base` key + `WorkPolicy.submit_base` | Key `[submit] base` → `submit_base` (default `None`; empty/non-string → PolicyError); mirrors `pr_base`/`submit_pr_base`. Owned by 00026; 00028 must NOT emit/read it. | 00026, 00028 |
| SI-2 | `[sandbox] setup` + `WorkPolicy.sandbox_setup` | Key `[sandbox] setup` → `_optional_sandbox_setup` (`_policy.py:387`) → `sandbox_setup` (default None); commented block, example `setup = "uv sync"`. 00028's sole `_INIT_POLICY_TAIL` addition. | 00028 |
| SI-3 | `LifecycleStore.list_lifecycles` | `list_lifecycles(self, *, statuses: Collection[Status] \| None = None, task_id: str \| None = None) -> list[Lifecycle]`; fully-folded objects. Owned by 00030; implemented verbatim on PostgresStore by 00032. | 00030, 00032 |
| SI-4 | `summarize_spend` + `SpendSummary` | `summarize_spend(self, *, since=None, until=None) -> SpendSummary`; frozen dataclass (4 token sums + `total_cost_usd`); half-open `[since,until)` on `attempts.last_activity_at`. Owned by 00031. | 00031, 00032 |
| SI-5 | `list_lifecycles` ordering | `(updated_at DESC, run_id DESC)`, deterministic; every backend identical (latest-for-task depends on first element). | 00030, 00032 |
| SI-6 | `worker.py _submit` / landing path | 00027 owns HEAD (dirty-tree/no-op classification `worker.py:439-457` + identity); 00026 owns TAIL (base resolution/fetch, `_ff_merge`/`_phase_base`, divergent-base park). 00027 lands first; 00026 takes a prereq edge. | 00026, 00027 |
| SI-7 | Park-disposition surface (REDEFINED 2026-06-17, D-6) | Was: two DISTINCT namespaces (00027 git-observable marker; 00026 lifecycle status/reason). Now superseded — BOTH park types unify on ONE store ledger event, the landing-parked `DomainEvent` kind (see SI-12). 00027's D-2 (git marker) and 00026's D-4 (reuse lifecycle status) are both SUPERSEDED by their respective D-6 entries. No git marker; no new `Status`. | 00026, 00027 |
| SI-8 | `_INIT_POLICY_TAIL` + detached-HEAD consistency | 00026 appends `[submit] base`, 00028 appends `[sandbox]` — distinct tables, non-overlapping, no edge, second-to-touch rebases. 00028 init detached-HEAD refusal matches worker DEFAULT precondition; 00026 relaxes detached tolerance only WHEN base configured (a state init never writes). No contradiction. | 00026, 00028 |
| SI-9 | `persistence-schema.sql` / `CURRENT_SCHEMA_VERSION` | Stays **12**. 00030/00031 land ZERO schema changes; 00032 authors no migration. Any future bump = ONE coordinated migration across all stores + mirror. | 00030, 00031, 00032 |
| SI-10 | `ClaimStore.list_claims` | `list_claims(self) -> list[TaskClaim]`; only currently-held claims; each with `task_id, worker_id, claimed_at, lease_expires_at, version`. Stays out of core. Owned by 00031. | 00031 |
| SI-11 | `worker_id` on `LiveRunRow` + `status --json` | Unset → null/omitted, never a sentinel. 00031 ADDS `LiveRunRow.worker_id` + selects it; 00032 inherits and asserts Postgres parity. | 00031, 00032 |
| SI-12 | landing-parked `DomainEvent` kind (`flywheel_core.events`) | `DomainEventKind.LANDING_PARKED` (wire tag `landing_parked`); frozen `LandingParked(_DomainEventBase)` with `park_kind: str` + `detail: str = ""`; folded by the identity branch of `events.apply` (no lifecycle mutation, advances `version` only); round-tripped by `event_serde.py`; persisted as opaque JSON (`kind` + `payload_json`) on the existing `events` ledger (NO migration — SI-9, `schema_version` stays 12); appended via `append_domain_event`, read via `list_domain_events(run_id)`. `park_kind` vocab: `uncommitted-work` (00027) and `divergent-base` (00026). OWNED + landed by 00027 (its SI-7); CONSUMED by 00026 (`park_kind == "divergent-base"`) — 00026 adds NO new core event type. Supersedes the old git-marker SI-7. | 00026, 00027 |
| — | `store_protocols` module purity | New signatures must not drag json/pathlib/io/sqlite into the pure type module (`test_store_protocols_module_purity.py`). `SpendSummary` is a plain frozen dataclass. | 00030, 00031, 00032 |
| — | `flywheel_core.events` / `event_serde` purity | The `LandingParked` addition (SI-12) keeps `events.py` pure — a frozen dataclass with primitive fields, no json/pathlib/io (`test_events_module_purity.py`); serialization stays in `event_serde.py`. The identity-fold branch already admits `Unblocked`/`RetryScheduled`/`GraderEvaluated`/`CommandApplied`; `LandingParked` joins it. | 00026, 00027 |

---

## CONSOLIDATED CROSS-SPEC PREREQUISITE DAG

Acyclic (verified by DFS-coloring + Kahn). **Topological order = the exact order `flywheel worker` picks tasks up** (19 nodes; includes `99-smoke-readme` not in this batch — the 18 batch tasks below):

```
 1. agg-list-claims                       (00031, no prereqs)
 2. agg-spend-summary                     (00031, no prereqs)
 3. agg-worker-id-live-surface            (00031, no prereqs)
 4. commit-worktree-identity              (00027, no prereqs)
 5. dist-version-constraints              (00029, no prereqs)
 6. land-config-submit-base               (00026, no prereqs)
 7. onboard-init-git-preflight            (00028, no prereqs)
 8. query-list-lifecycles-backends        (00030, no prereqs)
 9. commit-dirty-tree-park-classification (00027 ← commit-worktree-identity)
10. dist-foreign-install                  (00029 ← dist-version-constraints)
11. land-resolve-base-from-policy         (00026 ← land-config-submit-base)
12. onboard-auth-docs                     (00028 ← onboard-init-git-preflight)
13. onboard-sandbox-scaffold              (00028 ← onboard-init-git-preflight)
14. query-migrate-orchestrator-reads      (00030 ← query-list-lifecycles-backends, agg-worker-id-live-surface[X])
15. commit-scrubbed-host-merge-and-suite  (00027 ← commit-worktree-identity, commit-dirty-tree-park-classification)
16. land-ref-update-and-queryable-park    (00026 ← land-resolve-base-from-policy, commit-dirty-tree-park-classification[X])
17. pg-read-path-port                     (00032 ← query-migrate-orchestrator-reads[X], agg-spend-summary[X], agg-list-claims[X], agg-worker-id-live-surface[X])
18. pg-factory-seam                       (00032 ← pg-read-path-port)
19. pg-status-verb-end-to-end             (00032 ← pg-factory-seam)
```

`[X]` = cross-spec edge. The six cross-spec edges wired by the plan reconciler:

| From | To | Type | Reason |
|---|---|---|---|
| `land-ref-update-and-queryable-park` | `commit-dirty-tree-park-classification` | order | SI-6: 00026 `_submit` TAIL lands on 00027's already-split, green classification gate |
| `query-migrate-orchestrator-reads` | `agg-worker-id-live-surface` | order | 3-way `collect_live_rows` overlap: 00031 adds `worker_id` to the raw SELECT first; 00030 migrate preserves it by reading folded `Lifecycle.worker_id` (`lifecycle.py:133` default `''` → None per SI-11) |
| `pg-read-path-port` | `query-migrate-orchestrator-reads` | HARD | Postgres port sits on already-migrated `list_lifecycles` call sites, not raw SQL (SI-3) |
| `pg-read-path-port` | `agg-spend-summary` | HARD | implements `summarize_spend` (SI-4) on PostgresStore |
| `pg-read-path-port` | `agg-list-claims` | HARD | implements `list_claims` (SI-10) on PG claim store |
| `pg-read-path-port` | `agg-worker-id-live-surface` | HARD | inherits `LiveRunRow.worker_id` (SI-11), asserts Postgres parity |

Register-decided **non-edges** (kept unwired by design): 00026/00028 `_INIT_POLICY_TAIL` (distinct tables, second-to-touch rebases — SI-1/2/8); 00029 free-standing chain.

---

## RECONCILIATION CHANGELOG

### Spec-level
- **SI-1**: pinned `[submit] base → submit_base` (default None); c6 now asserts the attribute name + default. Unnamed-key collision with 00028's `[sandbox]` resolved (distinct tables).
- **SI-6**: split `worker.py _submit` ownership — 00027 HEAD (`439-457`+identity) lands first, 00026 TAIL takes a prereq edge instead of conflict-merging the same function.
- **SI-7**: originally declared 00026 divergent-base store-outcome and 00027 uncommitted marker as DISTINCT surfaces (no shared namespace). **Superseded 2026-06-17 by the D-6 re-spec (both specs):** the two park types UNIFY on ONE store ledger event — the landing-parked `DomainEvent` kind, now tracked as **SI-12** (owned by 00027, consumed by 00026). 00027 D-2 (git marker) and 00026 D-4 (lifecycle status vocab) are both superseded; no git marker, no new `Status`.
- **SI-8**: resolved the apparent detached-HEAD contradiction (00028 init refuses vs 00026 worker tolerates) — 00026 relaxes tolerance only when base is configured, a state `init --defaults` never writes.
- **SI-3/4/5/9/10/11**: pinned the full canonical store-protocol signatures, `(updated_at DESC, run_id DESC)` ordering, the no-schema-bump (v12) stance, and `worker_id` null/absent semantics across 00030/00031/00032. 00026 base-resolution layer hint added: `_phase_base(repo_root)` runs at `worker.py:1017` BEFORE `load_effective_policy()` at `1022` — slice must reorder.
- All seven specs carry a **D-5 (Reconciliation 2026-06-17)** decision-log entry. No dropped audit findings (B1-B5, G7, M1, Q1-Q3 map 1:1; M2-M7 except M1 and Q4-Q6 explicitly parked).

### Plan-level
- Wired SI-6/D-5 edge (00026→00027) onto `land-ref-update-and-queryable-park`.
- Wired four HARD edges (00032→00030 + 00032×3→00031) onto `pg-read-path-port` (chose `query-migrate-orchestrator-reads` as the 00030 anchor so the PG port sits on migrated call sites).
- **Discovered + resolved a previously-unflagged 3-way `collect_live_rows`/`_workflow.py` overlap** (00030 migrate + 00031 worker_id add + 00032 retype): ordered `agg-worker-id-live-surface` before `query-migrate-orchestrator-reads`; pinned the `Lifecycle.worker_id '' → None` preservation requirement.
- Fixed two stale references in the 00026 worker tasks claiming held-out tests live under `.flywheel/holdout/00026-*` when graders correctly run them from `packages/flywheel-worktree/tests/test_worker.py` per the authoritative spec. *(Note: `/fw-verify` subsequently authored the c1 oracle INTO the holdout dir and re-pointed its grader there — see VERIFY summary.)*
- No tasks folded or split; all shared-file overlaps resolved by ordering, not merging. Full 19-task graph verified acyclic; all JSON parses.

---

## Sharpening pass (round 2, 2026-06-17)

A second `/fw-verify`-driven pass re-specced, reconciled, re-planned, and re-verified the four criteria that round 1 left under-specified. The central call was an **operator park-disposition decision** that unified two previously-distinct surfaces.

**Unified store-domain-event park decision (D-6, both 00026 + 00027 → SI-12).** Worker landing-park outcomes are now recorded by appending ONE new store domain event — `DomainEventKind.LANDING_PARKED` (wire tag `landing_parked`; frozen `LandingParked(_DomainEventBase)` with `park_kind: str` + `detail: str = ""`) — queryable through `list_domain_events(run_id)`. A `park_kind` discriminator separates the two park types on the same surface: `uncommitted-work` (00027) vs `divergent-base` (00026). There is **no new lifecycle `Status` and no transition** — the run stays terminal `DONE`, the fold is the identity branch of `events.apply` (advancing `version` only). The event persists as opaque JSON (`kind` + `payload_json`) on the existing `events` ledger, so **no schema migration** — `CURRENT_SCHEMA_VERSION` / `schema_version` stays **12** (SI-9). The kind+dataclass is OWNED + landed by 00027 (its redefined SI-7); 00026 CONSUMES the IDENTICAL contract with `park_kind="divergent-base"` and adds no new core event type. This supersedes 00027's D-2 (git-observable marker) and 00026's D-4 (reuse lifecycle status vocabulary), both retained unedited and marked Superseded.

**What changed:**
- **SI-7 redefined / SI-12 added** to the register: the park surface is the unified `LANDING_PARKED` `DomainEvent`, not a git marker or lifecycle status. A new `flywheel_core.events`/`event_serde` purity register row records that `LandingParked` keeps `events.py` pure (frozen primitive-field dataclass, joins the existing identity-fold branch; `test_events_module_purity.py`).
- **Two gaps RESOLVED.** 00032 c1 (history parity) — D-6 bound the terminal-run enumeration to the SI-3 `list_lifecycles(statuses=TERMINAL_STATUSES)` seam (off the SQLite-only `_history.py:272` `store._connection` SELECT); oracle authored + admitted. 00030 c3 (ordering) — D-7 recast the unauthorable injected-`updated_at` tie into a metamorphic-determinism + recency + opportunistic-tiebreak check; oracle authored + admitted (D-7 because the immutable reconciliation entry already holds D-6 in 00030).
- **Two gaps sharpened to a single named missing seam each.** 00027 c4/c5 + 00026 c5 are now AUTHOR-routed against the pinned `LANDING_PARKED` contract but blocked solely on the absence of a `store=` injection point on `GitWorktreeSubmitter` (verified: kw-only ctor, no `store` param/member). 00026 c2/c3/c4 are blocked solely on the absence of a drivable policy base-resolution entrypoint (the only path is the forbidden `_phase_base()` inside `main()`).
- **Task-side deltas applied** (re-plan): 00026 c2/c3/c4/c5 graders re-pointed onto `.flywheel/holdout/00026-safe-landing-target/` with D-6 keyword renames; 00027 c4/c5 keywords renamed (`uncommitted_park_event_on_ledger`, `clean_noop_no_park_event`) + a NEW command grader `landing-parked-event-core-roundtrip` gating the SI-12 core change; 00030 added `test_list_lifecycles_task_id_order.py` (dir now collects 19); 00032 `history-parity-holdout` strengthened to assert the exact terminal-run-id set on both backends. **DAG unchanged and acyclic** (the event-kind ownership rides the pre-existing SI-6 00026→00027 edge — no new edge); all 7 affected task JSONs re-parse clean; held-out files verified on disk.

---

## VERIFY ROUTING SUMMARY

71 criteria across the batch. Routing tallies:

| Outcome | Count | Meaning |
|---|---|---|
| **AUTHORED + admitted** | 24 | held-out oracle written, discriminates, flake-stable, fenced (added 00030 c3 via D-7, 00032 c1 via D-6) |
| **SKIP** | ~31 | structural/state/exit-code/metadata checks already graded un-gameably by command, or visible (agent-authored), or meta-criteria satisfied by oracle existence |
| **MANUAL / under-specified upstream** | 7 | subjective merge-gate / relaxed-check review (Weyuker class: 00027 c9, 00029 c10, 00030 relaxed-check) OR a blocked held-out behavior whose drive/wiring seam is undeclared: **00026 c2/c3/c4** (no drivable policy base-resolution entrypoint) and **00026 c5 + 00027 c4/c5** (no `store=` injection point on `GitWorktreeSubmitter`). Both gaps are one root each. |

Net change from the sharpening pass: 00030 c3 and 00032 c1 moved out of MANUAL/UNDER-SPECIFIED into AUTHORED+admitted (D-7 recast, D-6 SI-3 enumeration seam). 00026 c2/c3/c4/c5 and 00027 c4/c5 sharpened to crisp residuals — their oracles are now AUTHOR-ROUTED with a single named missing seam each, no longer vague. No criterion is left "UNDER-SPECIFIED, contract gap unknown."

**Operator task-edits to apply** (each admitted oracle needs its grader + non_goals fence on the owning task; many are *already present* per VERIFY — apply only where the task JSON lacks them):

| Spec/criterion | Owning task | Command grader to ensure present | non_goals fence |
|---|---|---|---|
| 00026 c1 | `land-ref-update-and-queryable-park` | `uv run pytest .flywheel/holdout/00026-safe-landing-target/ -k landing_does_not_touch_operator_worktree` | `Do not read, modify, or weaken tests under .flywheel/holdout/00026-safe-landing-target/` |
| 00026 c2/c3/c4 | `land-resolve-base-from-policy` | re-pointed onto `.flywheel/holdout/00026-safe-landing-target/ -k <refuse_land_into_checked_out_base \| detached_head_lands_on_configured_base \| landing_fetches_base_before_merge>` (oracles **not yet authored** — blocked on the base-resolution entrypoint gap) | same dir |
| 00026 c5 | `land-ref-update-and-queryable-park` | `... -k divergent_base_records_park_event` (oracle **not yet authored** — blocked on store-injection gap; brief re-pointed to the D-6 `LANDING_PARKED` event) | same dir |
| 00027 c4/c5 | `commit-dirty-tree-park-classification` | `... -k uncommitted_park_event_on_ledger` / `-k clean_noop_no_park_event` (renamed from `uncommitted_park_reason_queryable`/`clean_noop_cleaned_up`; oracles **not yet authored** — blocked on store-injection gap); **plus the NEW core grader** `uv run pytest packages/flywheel-core/tests/test_events.py packages/flywheel-core/tests/test_events_module_purity.py packages/flywheel-core/tests/test_store_contract.py` | same dir |
| 00027 c1/c2 | `commit-worktree-identity` | `uv run pytest .flywheel/holdout/00027-worktree-commit-integrity/ -k commit_identity` / `-k deterministic_identity` | `...under .flywheel/holdout/00027-worktree-commit-integrity/` |
| 00027 c3 | `commit-dirty-tree-park-classification` | `... -k uncommitted_done_preserved` | same dir |
| 00027 c6 | `commit-scrubbed-host-merge-and-suite` | `... -k clean_commit_ff_merges` | same dir |
| 00028 c1/c3 | `onboard-init-git-preflight` | `uv run pytest .flywheel/holdout/00028-onboarding-preflight-and-auth -k 'git_preflight or detached_head' -q` *(already present)* | *(already fenced)* |
| 00028 c5/c6/c8 | `onboard-sandbox-scaffold` | dir-level `-k 'sandbox_section_rendered or sandbox_setup_uncomment_parses or existing_policy_untouched'` *(present)* | *(present)* |
| 00028 c7 | `onboard-auth-docs` | `... -k next_steps_auth -q` *(present)* | *(present)* |
| 00030 c1/c2/c3/c4/c5 | `query-list-lifecycles-backends` | dir-level `behavior-held-out`: `uv run pytest .flywheel/holdout/00030-cross-task-query-surface/ -q` (now collects 19 incl. the NEW `test_list_lifecycles_task_id_order.py` for c3 per D-7) | `...under .flywheel/holdout/00030-cross-task-query-surface/` |
| 00030 c9 | `query-list-lifecycles-backends` | `... test_list_lifecycles_reopen_v12_no_recreate.py -q` | same dir |
| 00030 c7 | `query-migrate-orchestrator-reads` | `... test_orchestrator_lifecycle_select.py -q -k orchestrator_lifecycle_select` *(present)* | *(present)* |
| 00031 c1/c2 | `agg-spend-summary` | dir-level `held-out-spend-contract` *(present, subsumes both files)* | *(present)* |
| 00031 c5 | `agg-list-claims` | `... test_list_claims_pairs.py -q` | `...under .flywheel/holdout/00031-spend-and-worker-aggregates/...` |
| 00031 c7/c8 | `agg-worker-id-live-surface` | `... test_live_snapshot_worker_id.py` / `test_status_json_worker_id.py -q` | same dir |
| 00032 c1 | `pg-read-path-port` | `uv run pytest .flywheel/holdout/00032-postgres-read-path-parity -k "history and parity"` *(present; D-6 strengthened to assert exact terminal-run-id set on both backends)* | *(present, phase-wide fence)* |
| 00032 c2/c3 | `pg-read-path-port` | `... -k "live_rows and parity"` / `-k "latest_lifecycle and parity"` *(present)* | *(present, phase-wide fence)* |
| 00032 c4 | `pg-factory-seam` | `... test_factory_seam_postgres.py -k "postgres and not refus"` | `...under .flywheel/holdout/00032-postgres-read-path-parity/` |
| 00032 c5 | `pg-status-verb-end-to-end` | `... -k "status and postgres"` *(present)* | *(present)* |

---

## READY TO RUN

- **7 specs, 18 batch tasks, all on disk and JSON-valid.** On-disk prerequisites match the reconciled DAG exactly (verified). The 7 affected task JSONs (00026×3, 00027×2, 00030×1, 00032×1) re-parse clean after the sharpening pass.
- **24 held-out oracle files admitted** under `.flywheel/holdout/000{26,27,28,30,31,32}-*` (added 00030 `test_list_lifecycles_task_id_order.py` and the now-admitted 00032 `test_history_parity.py`); 00029 has none by design. **4 held-out behaviors remain un-authored**, each blocked on one named missing seam: 00026 c5 + 00027 c4/c5 on a `store=` injection point for `GitWorktreeSubmitter`; 00026 c2/c3/c4 on a drivable policy base-resolution entrypoint.
- **DAG is acyclic; topological order above is what the worker will follow.** First wave (no prereqs): `agg-list-claims`, `agg-spend-summary`, `agg-worker-id-live-surface`, `commit-worktree-identity`, `dist-version-constraints`, `land-config-submit-base`, `onboard-init-git-preflight`, `query-list-lifecycles-backends`.
- Postgres-backed oracles (00030 c4, 00031 c1, 00032 c2-c5) **require a reachable Docker test container**; they skip cleanly otherwise but then grade nothing for the Postgres arm.
- The baseline test-count floor for 00026 regression holdouts is **411** (`packages/flywheel-worktree` + `packages/flywheel-orchestrator`, `--collect-only`).

---

## OPEN QUESTIONS / OPERATOR ACTIONS

**Status of the four gaps after the sharpening pass (round 2, 2026-06-17):** gaps 1 and 2 are RESOLVED; gaps 3 and 4 collapsed from "vague/undeclared contract" into two precise residual seams.

1. **RESOLVED — 00032 c1 (history parity).** D-6 bound the terminal-run enumeration to the SI-3 seam `list_lifecycles(statuses=TERMINAL_STATUSES)` (reusing `_history.py`'s `(DONE, FAILED, FAILED_VALIDATION)` tuple), replacing the SQLite-only `store._connection` SELECT at `_history.py:272`. A correct literal-free cross-backend reference is now writable; the oracle was authored, admitted (killed 3 genuine wrong refs incl. the live `_connection` `AttributeError` on Postgres), and is on disk at `.flywheel/holdout/00032-postgres-read-path-parity/test_history_parity.py`.
2. **RESOLVED — 00030 c3 (ordering).** D-7 recast c3 off the unauthorable injected-`updated_at` forced tie into a metamorphic-determinism + recency + opportunistic-`run_id`-DESC-tiebreak check that needs no caller-settable timestamp (`updated_at` is store-clock-set; not a `Lifecycle` field). Oracle authored and admitted at `.flywheel/holdout/00030-cross-task-query-surface/test_list_lifecycles_task_id_order.py` (numbered D-7 because the immutable 2026-06-17 reconciliation already holds D-6 in 00030).

**Remaining residual gaps (precise, one named missing seam each — NOT vague):**

3. **00027 c4/c5 + 00026 c5 — undeclared store-injection point on the worktree submit seam.** Under D-6/SI-12 the park outcome is recorded as a `LANDING_PARKED` `DomainEvent` read back via `store.list_domain_events(run_id)` — so the held-out oracle must construct a `GitWorktreeSubmitter` connected to the SAME real store it later queries. The real `GitWorktreeSubmitter.__init__` is kw-only (`repo_root, tasks_dir, worktrees_dir, phase_base, lock_path, log, protected_paths, setup_command`) with **no `store` parameter, no `**kwargs`, and no member matching `store`**; `SubmitRequest`/`SandboxRequest` carry no store. The criteria's "read only via the store ledger" clause means the git-state assertions alone do not discriminate. *(Action: add a kw-only `store=` injection point (or an equivalent named setter/attribute/threaded arg) to the worktree submit surface and name it in the briefs, then re-run `/fw-verify` for 00027 c4/c5 and 00026 c5. The new core grader `landing-parked-event-core-roundtrip` already gates the SI-12 `flywheel_core.events`/`event_serde` change independently.)*
4. **00026 c2/c3/c4 — no drivable policy base-resolution entrypoint.** The end-states are pinnable, but the contract names the c1 oracle as the drive reference and c1 passes a literal `phase_base` and performs no base resolution/fetch. The only resolution path today is the forbidden `_phase_base()` inside `main()`; there is no public seam (headless `main()` convention, `run_once(base=...)`, or builder) that accepts a policy-resolved base, and `WorkPolicy.submit_base` (the `land-config-submit-base` prereq) must exist. *(Action: name the observable startup/base-resolution entrypoint and land `WorkPolicy.submit_base`, then re-run `/fw-verify` for 00026 c2/c3/c4. The "inverse fence" defect — the spec forbids asserting an internal signature but names no public one.)*

**Phasing decision (operator/scheduler):** the DAG permits the 00026/00027 worker pair and the 00030/00031/00032 store chain to be separate phases or co-phased; if co-phased the prereq edges must be honored within the phase and second-to-touch tasks rebase + re-run render/parity assertions.

**Operator note for the 00030 c7 implementer:** the source-scan oracle correctly flags `_workflow.py:1303` (`_list_blocked_lifecycles`) — a lifecycle SELECT not listed in the task's migration notes. The agent must migrate that site too for the holdout to pass.

**Fencing caveat:** all holdout fences are by convention (in-repo, outside pytest `testpaths` so a bare `uv run pytest` skips them). True tamper-resistance requires out-of-worktree grading by the orchestrator — out of scope for this batch.
