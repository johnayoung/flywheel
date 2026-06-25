# 00055 graph-snapshots — fw-verify audit

Blind held-out discrimination proofs for spec
`.flywheel/specs/00055-graph-snapshots.md`, run AFTER execute against the
shipped code on `main`. Mirrors the 00054 method: blind oracles authored from
the declared contract only, a synthesized-mutant discrimination gate, and a
real-grade pass against the shipped stores (InMemory + SQLite + live Postgres).

Verification scratch is git-ignored under `.flywheel/verification/00055/`
(oracles, `mutants_check.py`, `probes_real_grade.py`); only this audit is
durable. The permanent regression guard is the tasks' own committed tests in
`test_orchestrator_claims.py` / `test_orchestrator.py`, run by the command
graders and CI.

## Result

- **4 blind held-out oracles authored** (A/B/C/D), each by a fresh fenced
  subagent that read only its one criterion + the public contract, never the
  implementation. All ADMITTED.
- **Discrimination gate: 4/4 mutant kills, correct reference passes all 4**
  (`mutants_check.py`), flake-screened run-twice (identical verdicts).
- **Real-graded against shipped code**: all 4 oracles pass the durable
  `SqliteClaimStore`; real-grade probes pass for #10/#11; the live Postgres
  parity + migration tests pass this run (not skipped).
- Integrated `main` green: ruff 0, pyright 0, **2039 passed**.

## Routing

| Criterion | Route | Why |
| --- | --- | --- |
| #1/#2 materialized-state fidelity | AUTHOR oracle A | behavior; a constant-stamp passes a presence check |
| #3 internal completeness | AUTHOR oracle D | behavior; a partial write misrepresents the graph |
| #4 event-cursor correctness | AUTHOR oracle B | behavior; a stale/zero cursor breaks reconstruction |
| #5 append-only / no overwrite | AUTHOR oracle C | behavior; an upserting store is lossy |
| #7/#8 per-pass loop capture + evolving state | real-grade probe | loop wiring; covered by driven-orchestrate suite |
| #9 cross-backend parity | real-grade (live Postgres) | composition holdout; ran against a live DB this run |
| #10 v4->v5 non-destructive migration | real-grade probe + live Postgres test | drop-and-recreate discriminated |
| #11 empty-graph snapshot | real-grade probe | structural-ish; folded into D + probe |
| #6 read API | SKIP (un-gameable) | structural read-back already graded by /fw-plan |

## Blind oracles + discrimination proof

Each oracle exposes `verify(make_store)` (a zero-arg store factory). The gate
runs it against the real `InMemoryClaimStore` (must PASS) and one targeted
mutant subclass injecting exactly the gaming move (must be KILLED).

### A — state fidelity (#1/#2)
`.flywheel/verification/00055/A-state-fidelity/test_state_fidelity.py`
- form: per-item round-trip + cross-item inequality. Records two items differing
  in `ready`/`claim_holder`/`state`/provenance; asserts each field round-trips
  and X != Y on readiness, holder, state.
- mutant `ConstantStateStore` (stamps `ready=True, claim_holder=None,
  state="fresh"` on every item) -> **KILLED** ("X must round-trip to an equal
  GraphSnapshotItem").
- correct `InMemoryClaimStore` -> PASS.

### B — event-cursor correctness (#4)
`.flywheel/verification/00055/B-event-cursor/test_event_cursor.py`
- form: relation to the live ledger. Empty store -> cursor 0; after acquire+renew
  -> cursor == `list_events()[-1].id`; after a further renew -> strictly greater.
- mutant `ZeroCursorStore` (stamps `last_event_id=0` always) -> **KILLED**
  ("snapshot cursor must equal ledger max id (2), got 0").
- correct -> PASS.

### C — append-only / no overwrite (#5)
`.flywheel/verification/00055/C-append-only/test_append_only.py`
- form: history accumulation. Records 3 snapshots with differing item sets;
  asserts the stream has all 3 with strictly-increasing ids and that an earlier
  snapshot's items are unchanged after later ones are recorded.
- mutant `OverwriteSnapshotStore` (keeps only the latest snapshot) -> **KILLED**
  ("all three recorded snapshots must persist; got 1").
- correct -> PASS.

### D — internal completeness (#3)
`.flywheel/verification/00055/D-completeness/test_completeness.py`
- form: header-vs-rows-vs-input invariant over sizes 0/1/3/5. Asserts
  `item_count == len(rows) == n` and the read-back task-id set equals the input.
- mutant `DropItemStore` (writes the header item_count but drops one row) ->
  **KILLED** ("read back 0 item rows but 1 items were captured").
- correct -> PASS.

```
$ uv run python .flywheel/verification/00055/mutants_check.py
[A-state-fidelity] KILL ConstantStateStore: X must round-trip to an equal GraphSnapshotItem.
[B-event-cursor]   KILL ZeroCursorStore: snapshot cursor must equal ledger max id (2), got 0
[C-append-only]    KILL OverwriteSnapshotStore: all three recorded snapshots must persist; got 1 ...
[D-completeness]   KILL DropItemStore: read back 0 item rows but 1 items were captured ...
Killed 4/4 mutants; correct reference passed all 4.
```
Flake screen: run twice, identical verdicts (stable).

## Real-grade probes (against shipped code)

`.flywheel/verification/00055/probes_real_grade.py`:
- All 4 oracles re-run against the durable `SqliteClaimStore` -> PASS (the
  oracles pass the shipped durable backend, not only InMemory).
- **#11 empty-graph**: `record_graph_snapshot([])` yields a recorded 0-item
  snapshot with empty rows and cursor 0 -> PASS.
- **#10 migration (SQLite)**: a store rolled back to sentinel v4 with its snapshot
  tables dropped (a genuine pre-v5 on-disk shape) reopens under v5 with the
  claim, work_item, and event rows intact, an empty snapshot stream, and the
  sentinel converged to 5 -> PASS (discriminates a drop-and-recreate, which would
  wipe the rows).
- **#7/#8 loop**: the shipped suite carries the driven-orchestrate tests
  `test_each_scheduling_pass_records_a_graph_snapshot` and
  `test_successive_snapshots_track_the_graph_evolving`; both pass independently
  (`pytest -k snapshot` over `test_orchestrator.py`: 4 passed). The capture is
  wired at the top of each pass, after graph/states/readiness are built and
  before dispatch (`_orchestrate.py` ~L862), a pure read-and-record side channel.

Live-Postgres tests (DB reachable this run, NOT skipped):
- `test_sqlite_postgres_graph_snapshot_parity` -> PASS (**#9** cross-backend
  parity real-graded against a live DB).
- `test_v4_postgres_store_opens_under_v5_with_rows_and_empty_snapshots` -> PASS
  (**#10** Postgres migration real-graded).
- Full `pytest -k snapshot` over `test_orchestrator_claims.py`: 19 passed across
  memory/sqlite/postgres params.

## Honest limits

- The oracles are git-ignored scratch; they do NOT gate the agent's real run.
  The execute-time guard is the tasks' own committed tests + command graders
  (run green first-attempt; the loop task's attempt 1 was an `agent_error`
  protocol hiccup — "missing iteration envelope" — not a grader failure, and
  attempt 2 passed).
- Registration into an out-of-worktree held-out gate is deferred: `flywheel.toml`
  has no `[held_out] root` (same posture as 00053/00054). The discrimination is
  proven and recorded here; wiring it as an orchestrator-run gate is future work.
- #7/#8 are real-graded by the shipped driven-orchestrate suite, not a single
  blind discrimination oracle — synthesizing a loop mutant would require
  reimplementing `orchestrate`; the store-level fidelity that the loop depends on
  is discrimination-proven by oracle A.

## Verdict
00055 graph-snapshots: spec + plan + execute + fw-verify all on `main`. The
WorkGraph observability pair is complete — 00054's event ledger (deltas) plus
00055's snapshots (base state), composable via the store-stamped `last_event_id`
cursor for point-in-time reconstruction.
