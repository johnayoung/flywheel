# Flywheel Adoption Readiness Audit — orchestrator + worktree (2026-06-17)

> Point-in-time audit, not an authoritative spec. Feeds `/define`, `/fw-spec`, and `.flywheel/specs/`.
> Scope: what critical features are missing before *another local codebase* (not flywheel's own repo)
> can adopt flywheel to drive AI coding tasks. Lens: the two product goals — (1) built for the
> AI-native company, (2) everything is queryable.
> Method: four parallel deep-read passes (orchestrator, worktree, onboarding/config, queryability),
> each returning file:line evidence. The two headline blockers (worker never commits; lands onto the
> checked-out branch) and the distribution claim were spot-verified directly against source.

## Verdict

The seams are right. `WorkSource`, `SubmitStrategy`, `WorkPolicy`/`flywheel.toml`, and the store
protocol are clean, structural, and genuinely project-agnostic — an adopter *could* wire a custom
source/landing as a library call. But the **shipped, turnkey path is hardwired to flywheel's own repo
shape**: it can only be run from inside this checkout, lands onto whatever branch you have checked out,
and assumes the agent commits its own work on a host with a git identity already set. The second stated
goal — *everything is queryable* — holds only for a single known run, not across tasks.

The blockers below are what stop a stranger cold. The queryability gaps are what make the
"AI-native, everything-queryable" story untrue at the fleet level.

---

## Blockers — a new codebase hits these on day one

### B1. Not installable outside this checkout
`packages/flywheel/pyproject.toml` depends on `flywheel-orchestrator`/`flywheel-worktree` with no
versions, resolved only via `[tool.uv.sources] = { workspace = true }` (root `pyproject.toml:35-39`).
Nothing is published (spec 00021 parks it). The only way a stranger runs flywheel is to clone *this*
repo and `uv sync` — there is no `uv add flywheel` / `pip install`. Path resolution is correctly
cwd-relative, so the *design* intends dependency use; only distribution blocks it.
Type: missing feature.

### B2. The worker never commits, and never sets a git identity
`worker.py` runs only `worktree add`, `merge --ff-only`, `rev-parse` — verified, no `git add`/`commit`
anywhere. It relies entirely on the agent committing inside the sandbox. On a fresh host: an agent that
did real work but didn't commit gets its branch deleted as a no-op (`worker.py:448-457`); with no
`git config user.name/email` set, the agent's own `git commit` fails, so the loop produces empty
branches. Works here only because the dev's global git identity is set.
Type: hardcoded assumption + missing feature.

### B3. FF-merge lands onto the operator's checked-out branch, and dies on detached HEAD
`_phase_base` resolves the base as `rev-parse --abbrev-ref HEAD` (`worker.py:885-892`) and merges into
`repo_root` — the live checkout. Run the worker while sitting on `main` and task branches fast-forward
straight into your working branch. No configurable base, no protected-branch concept for the default
`merge` strategy, no `git fetch` (lands onto a stale local base). On a busy shared branch, a rebase
conflict parks the worktree forever with only a stderr line (`worker.py:478-502`).
Type: hardcoded assumption.

### B4. Agent authentication is undocumented
Zero references to `ANTHROPIC_API_KEY` or `claude login` in README, `docs/workflow.md`, vision, or
`init`'s next-steps. The quickstart goes `uv sync → init → worker --once` and the adopter hits an
opaque SDK auth failure with no guidance.
Type: missing documentation.

### B5. Bare sandboxes by default — the empty-grader footgun
`[sandbox] setup` defaults to `None` (`_policy.py:145`), the init template doesn't emit a `[sandbox]`
section, and graders like `uv run pytest` then run in a worktree with no deps installed — collecting
0 tests and "passing." This is the exact footgun spec 00021 cites as motivation, still live for new
adopters.
Type: hardcoded assumption + missing onboarding guidance.

---

## Major gaps

### M1. Postgres is advertised but non-functional
`init` prompts for, writes, and validates `[store] backend = "postgres"`, then *every* command path
refuses it — `open_sqlite_bound_store` builds the store and raises `StoreConfigError` if it's Postgres
(`_store_factory.py:142-168`). The multi-host adopter (the only reason to want Postgres) gets a config
that fails at runtime. Matters doubly for an AI-native company running a fleet.
Type: missing feature (postgres read-path port deferred).

### M2. `flywheel orchestrate` silently does nothing useful
The verb whose name implies the entry point never constructs a `SubmitStrategy` or reads
`[sandbox] setup`/`[submit]` — it runs tasks in bare sandboxes and discards the result
(`_orchestrate.py:450-453`). The only path that actually lands work is `flywheel worker`, in a
different package. An adopter reading the verb list picks the wrong door.
Type: missing wiring.

### M3. Third-party plugins can't register
Entry-point discovery is a `return None` stub (`_registry.py:173-183`); docstrings admit Jira/Linear/
queue sources and custom stores/strategies register "once discovery is enabled." A codebase whose tasks
live in Jira/Linear, or that needs a GitLab/patch landing, must fork or monkeypatch. Only
`directory`+`github` and `merge`+`pr` are reachable.
Type: missing feature (explicitly deferred).

### M4. No per-task env or secret isolation
The agent runs `bypassPermissions` with no `env=` passed (`flywheel_core/workflow.py:445-452`) — it and
every grader subprocess inherit the daemon's full environment (API keys, git creds). No way to give a
task project-scoped secrets without exporting them globally to the daemon. A single-trusted-host model,
not a sandbox — at odds with multi-tenant fleet use.
Type: hardcoded assumption.

### M5. Lands without submodule/LFS/monorepo handling
Bare `git worktree add` with no `submodule update --init`, no LFS smudge, no sparse-checkout
(`worker.py:322-327`). Any repo with submodules or LFS gets incomplete worktrees and spurious grader
results.
Type: missing feature.

### M6. GitHub-source loses phases, archiving; prereqs are directory-shaped
Phase grouping, `flywheel archive`, and the loop-path gate are all `.json`-path-derived
(`_history.py:124-179`); a tracker-source adopter loses them. Prereqs are read off the work item, not
persisted, and a typo'd prereq id silently makes a task never run (`_workflow.py:310-315`).
Type: hardcoded assumption.

### M7. Multi-worker lease correctness depends on NTP discipline
Lease expiry compares stored ISO time against each worker's own wall clock with no shared/monotonic
source (`_orchestrate.py:113-121`, `_claims.py:291`). A worker with a fast clock can steal a live
peer's lease. Documented landmine, not a guarded one.
Type: hardcoded assumption.

---

## Queryability gaps — "everything is queryable" breaks at the fleet level

The store protocol is **100% single-`run_id` keyed** (`store_protocols.py:259-502`). Every cross-task
question is answered by reaching past the protocol into private raw SQL (`store._connection.execute(...)`
with `# noqa: SLF001` at `_history.py:212,272,351,378`; `_workflow.py:167,889`).

- **Q1. Can't list tasks/runs from the store alone.** `status` needs the on-disk `WorkSource` to
  enumerate tasks (`_workflow.py:234,1227`). Hold only the database and "show me all tasks and their
  status" returns nothing. No `list_lifecycles`, no filter-by-status.
- **Q2. No cost/spend aggregate.** Token/cost rollups exist per-run but no verb or method sums spend
  across tasks or over time. For a company watching agent burn, a core miss.
- **Q3. Per-worker activity is unqueryable.** `ClaimStore` has only `load_claim(task_id)`, no
  `list_claims`. `worker_id` is *persisted* but never read or surfaced by `collect_live_rows`/`status`.
  "What is each worker doing" has no path.
- **Q4. The DAG isn't in the store.** Prerequisites are sourced from task files at runtime, never
  persisted (schema `:48-52`). "What's blocked on what" requires re-reading task files.
- **Q5. Telemetry is tail/replay only.** `audit.stream` has no filter; CLI `--kind`/`--attempt`/
  `--summary` are explicitly deferred (`audit/_cli.py:32-33`). "What did the agent do on attempt 3"
  means grepping one run's JSONL by hand. No cross-run telemetry query, no event-type index.
- **Q6. No external read contract.** External tools can open the SQLite/Postgres file, but any schema
  mismatch beyond one forward migration is refused (`StoreSchemaError`), and the store is unredacted —
  external SQL sees raw secrets.

---

## What's already solid (don't rebuild)

- The `WorkSource` / `SubmitStrategy` / `WorkPolicy` protocols and `flywheel.toml` schema are clean and
  project-agnostic — the seams are correct.
- `init` skill templates are genuinely reusable (fully policy-parameterized, no flywheel-repo
  hardcoding).
- SQLite schema bootstrap is automatic and idempotent on first open; `init` idempotency/never-clobber is
  solid.
- `merge` flock + leases + crash-recovery-on-startup are real (with the clock caveat M7).
- Per-run queryability and the live-snapshot `--json` contract are strong.

---

## Recommended sequencing

1. **Make the turnkey path survive a foreign repo** (cheap, highest leverage, unblocks the *first*
   successful run): configurable/protected base branch + refuse-to-land-onto-checked-out-branch (B3),
   commit enforcement + git identity setup (B2), documented auth step (B4), prompt toward
   `[sandbox] setup` (B5).
2. **Distribution** (B1) — even an internal `uv tool`/wheel path. Nothing matters until a stranger can
   install it.
3. **Queryability protocol gap** (Q1-Q3): cross-task `list/filter/aggregate` + spend + per-worker, lifted
   above raw `_connection` SQL. Load-bearing for goal 2; also unblocks Postgres (M1) and any external
   dashboard.
4. **Park the deeper bets** until a real adopter's repo demands them: plugin discovery (M3), env
   isolation (M4), submodule/LFS (M5).

## Out of scope (covered elsewhere)

The core-grader sandbox-isolation gap (research brief R1) and replay/fork (R4) are real but orthogonal
to adoption; see `docs/research/2026-06-15-bleeding-edge-routes.md`. Not re-derived here.
