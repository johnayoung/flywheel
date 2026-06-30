# Feature: Resource & Leak Hardening (no slow-burn stop)

## Outcome
Four currently-unbounded resources that a long-lived fleet accumulates until a
hard crash are each bounded by an active maintenance or preflight behavior, so
none can grow without limit into a stop. Observably: (1) the supervisor/run log
footprint stays under a configured bound across repeated starts/runs instead of
growing one new file per start forever; (2) a parked worktree older than the
retention window is reclaimed by a sweep tick that fires while the daemon keeps
running, not only at boot; (3) a labelled orphan container left by a previously
killed worker is reaped on the next worker start; (4) when free disk/inodes fall
below a threshold the worker records a queryable degraded-space state and does
not proceed into the authoritative store write that would otherwise crash with
ENOSPC. This is Phase 7 of the "autopilot never stops" reliability program: the
slow-burn leaks that eventually become Phase-1-class hard crashes
(disk-full -> store-write OSError).

## Background
Each finding is an unbounded resource with no reclaim path. Supervisor logs open
a new timestamped `autopilot-supervisor-<ts>.log` on every `start()`
(`_autopilot_supervisor.py:340-347`) with nothing rotating or unlinking, and the
per-run JSONL `.flywheel/logs/runs/<run_id>.jsonl` is never capped (the worker
explicitly leaves it operator-owned, `worker.py:1013-1016`); a long-lived fleet
fills the disk and the next authoritative store write raises ENOSPC. The
worktree retention sweep runs once at boot (`worker.py:2072`) and removes
worktrees older than the window (`:1019-1054`), so a long-running daemon with a
high strand rate accumulates parked worktrees for up to the window PLUS its whole
uptime. The container atexit force-remove backstop (`_docker.py:402-427`) never
runs on SIGKILL/OOM-kill, and container orphans sit outside git-lease
reconciliation, so the next run never reaps them. And telemetry writes are
already crash-swallowed (`harness.py:814`) but the authoritative store write is
not, so an ENOSPC/inode-exhaustion there is an immediate hard crash with no
warning. The tacit requirement a literal agent would miss: each fix must grade an
OBSERVABLE END-STATE of the resource being bounded — "the log directory's total
footprint stays under the bound after N rotations", "a too-old worktree is gone
after a sweep tick while the daemon is still up", "a labelled orphan present at
startup is absent after start", "below the threshold a degraded record exists AND
the crashing store write was not taken" — never "a rotate function exists" or
"the sweep was called". A layer-only / function-exists grader is explicitly a
defect here. Respect core purity: none of this rotation/disk/sweep/reap logic may
live in `flywheel_core.task` or `flywheel_core.lifecycle`; it belongs in the
telemetry/loaders/worker/container layers.

## Scope
### In scope
- Supervisor log growth is bounded: across repeated supervisor starts the
  supervisor log footprint (total bytes or file count under the supervisor log
  directory) is held under a configured ceiling by rotation/cap/retention, so it
  cannot grow one unbounded file per start.
- Per-run telemetry JSONL growth is bounded: the run-logs directory
  (`.flywheel/logs/runs/`) is held under a configured retention bound (count
  and/or total size) by an active reclaim behavior, so a long-lived fleet's run
  files cannot fill the disk.
- The worktree retention sweep runs on a bounded recurring schedule while the
  daemon is up (per-cycle or on an interval), not only at startup, so a parked
  worktree that ages past the window is reclaimed during the daemon's lifetime.
- Orphaned containers from a previously-killed worker are detected and reaped on
  the next worker start via a label/owner/name scan (covering the SIGKILL/OOM
  case the atexit backstop misses), bounding container accumulation.
- The worker performs a disk/inode preflight (a guarded low-space check) that,
  when free space/inodes are below a configured threshold, records a queryable
  degraded-space state and declines to proceed into the authoritative store write
  for that unit, instead of letting that write hard-crash on ENOSPC.

### Out of scope
- Tuning the numeric bounds (ceiling bytes, retention count, threshold size) to
  any "right" magnitude; the requirement is bounded + default-on + queryable, not
  a specific number.
- Recovering or re-driving the work that was declined under low space (Phases
  4-5) — this phase stops at "a queryable degraded record exists and the crashing
  write was not taken".
- Reclaiming containers a *live* worker still owns, or changing the existing
  atexit/normal-teardown path (`_docker.py` register/unregister), or git-lease
  reconciliation of worktrees — only the killed-worker orphan case and the
  recurring worktree sweep are added.
- General process supervision / respawn of crashed daemons (Phase 6).
- The deadline/timeout behavior of any management call (Phase 2).

### Must not regress
- `flywheel_core.task` and `flywheel_core.lifecycle` stay pure (no json / pathlib
  / io); none of the rotation/disk/sweep/reap logic is placed in them.
- `import flywheel_core` still works without the agent SDK extra; no new top-level
  SDK import is added to a module core imports.
- The harness stays the authority on lifecycle transitions; agent self-reports
  stay untrusted; the worker still never commits to the operator's branch.
- The existing retention-sweep removal semantics (remove worktrees older than the
  window and their branches, prune dangling entries first) are preserved — the
  change makes it RECUR, it does not weaken what one sweep does.
- The existing normal container teardown and atexit backstop keep working; the
  startup orphan reap is additive and must not remove a container a live worker
  owns.
- The full gate (`scripts/check.sh`: ruff -> pyright -> pytest) stays green; no
  existing test, assertion, lint, or typecheck is weakened or deleted.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each to a command / transcript / rubric / manual grader.

1. When the supervisor log directory has accumulated more supervisor log output
   than the configured ceiling across repeated starts, the supervisor shall hold
   the directory's footprint at or under that ceiling by reclaiming old logs.
   [command | held-out]
   verify: a new pytest under `packages/flywheel/tests/` drives the supervisor
   (or its log-open path) through more starts/writes than the configured bound
   admits and asserts the post-state footprint (total bytes and/or surviving file
   count under the supervisor log dir) is at or under the ceiling AND that at
   least the most-recent log content is retained (reclaim is of OLD logs, not
   all); plus `scripts/check.sh`.
   defends against: "rotating" by truncating/deleting everything (footprint zero
   but no logs survive) or by capping a single file's name while still opening a
   new unbounded file per start — the test asserts both an upper bound on
   footprint AND that recent content survives, so neither delete-all nor
   never-reclaim passes.

2. While the run-logs directory holds more retained run files than the configured
   retention bound, the worker's reclaim behavior shall reduce the directory to
   at or under that bound while preserving the most recent runs. [command | held-out]
   verify: a pytest seeds `.flywheel/logs/runs/` with more run JSONL files (with
   distinct mtimes) than the configured bound, invokes the run-log reclaim, and
   asserts the surviving set is exactly the most-recent N (older ones gone, newest
   present) and the directory is at or under the bound; plus `scripts/check.sh`.
   defends against: a no-op that leaves growth unbounded (the seam the worker
   today explicitly skips), or deleting the newest/in-progress run instead of the
   oldest — the test pins WHICH files survive (most-recent-N), not merely a count.

3. When a worktree under the worktrees directory has aged past the retention
   window while the daemon is running, a recurring sweep tick shall remove that
   worktree without requiring a restart. [command | held-out]
   verify: a pytest invokes the daemon's recurring-sweep tick (the per-cycle /
   interval hook, not the boot-only call) with a worktrees dir containing one
   worktree whose mtime is older than the window and one within it, advancing the
   clock the tick reads, and asserts the aged worktree is gone and the fresh one
   remains AFTER a tick that is not the startup sweep; plus `scripts/check.sh`.
   defends against: leaving the sweep boot-only (today) so the test would only
   pass by restarting — the test drives the recurring tick directly and asserts
   reclaim happens mid-run; and against a tick that nukes every worktree (the
   within-window one must survive).

4. While the daemon's main loop is running, the worktree sweep shall execute on a
   bounded recurring cadence rather than at most once per process lifetime.
   [command | held-out]
   verify: a pytest runs the daemon loop body across multiple cycles with a
   recording sweep stub and asserts the sweep was invoked on a recurring schedule
   (more than the single boot invocation across N cycles / an elapsed interval),
   not exactly once; plus `scripts/check.sh`.
   defends against: satisfying #3 by calling the sweep once at boot and never
   again (a single tick passes #3's "a tick reclaims" but leaves accumulation
   unbounded over uptime) — this criterion pins that the cadence RECURS while the
   loop runs.

5. When a worker starts and a flywheel-owned orphan container left by a prior
   (killed) worker is present, the worker shall reap that orphan during startup.
   [command | held-out]
   verify: a pytest invokes the worker's startup orphan-reap path against a stub
   docker layer reporting a flywheel-owned container (matched by the flywheel
   label/owner/name marker) as present, and asserts the reap path issues the
   removal for that container; a sibling assertion asserts a non-flywheel /
   unrelated container reported by the same stub is NOT removed; plus
   `scripts/check.sh`.
   defends against: reaping by a broad match that also removes containers the
   worker does not own (collateral damage) — the test pins that only the
   flywheel-owned marker is targeted; and against a no-op reap that the SIGKILL
   case (atexit never ran) leaves unhandled — the test asserts the removal is
   actually issued for the orphan.

6. When a flywheel container is created, it shall carry a flywheel-owner marker
   that the startup orphan-reap scan selects on. [command | held-out]
   verify: a pytest asserts the container-run argv builder emits a stable
   flywheel-owner marker (a `--label` / owner identifier) that the reap scan's
   selector matches, and that the reap scan's selector and the created marker are
   the SAME identifier (round-trip: a container built with the marker is selected
   by the scan; one without it is not); plus `scripts/check.sh`.
   defends against: the marker and the scan selector drifting apart so created
   containers are invisible to the reaper (the orphan scan finds nothing and #5
   silently never reaps a real orphan) — the test asserts producer and consumer
   agree on one identifier, not each in isolation.

7. If a disk/inode preflight finds free space or inodes below the configured
   threshold, then the worker shall record a queryable degraded-space state and
   shall not proceed into the authoritative store write for that unit. [command | held-out]
   verify: a pytest drives the worker's preflight with an injected free-space /
   inode probe returning a below-threshold value and a store whose authoritative
   write raises if called, and asserts (a) a degraded-space record is produced and
   is queryable through the worker's status/telemetry surface, AND (b) the
   authoritative store write was NOT invoked (no crash); a sibling assertion with
   an above-threshold probe asserts normal flow proceeds and the store write IS
   reached; plus `scripts/check.sh`.
   defends against: "handling" low space by swallowing it silently (no queryable
   record, so the operator cannot see the degraded state) or by still calling the
   crashing store write — the test asserts BOTH a surfaced/queryable record AND
   that the crashing write was skipped, and a printed success string cannot
   satisfy it because the check reads the classified degraded record and the
   not-called store write, not stdout.

8. The existing verification suite still passes after the change, and core stays
   pure and SDK-optional: `import flywheel_core` succeeds with the SDK absent and
   `flywheel_core.task` / `flywheel_core.lifecycle` remain pure (no json / pathlib
   / io). [command | held-out] (verification-surface)
   verify: `scripts/check.sh` is green (ruff -> pyright -> pytest), and the
   existing purity / optional-extra suites still pass —
   `uv run pytest -k "module_purity or sdk_optional or import_without"` — none
   weakened or deleted.
   defends against: placing rotation/disk/sweep/reap logic in `task`/`lifecycle`
   (breaking purity), adding a top-level SDK import to satisfy a path, or making a
   criterion pass by relaxing an existing assertion.

Verification surface: this feature adds new tests under `packages/*/tests/` and
hardens production maintenance paths; it does not relax, remove, or weaken any
existing test, assertion, lint, or typecheck. The full gate (`scripts/check.sh`)
must still pass after every task, and the existing retention-sweep, container,
and purity tests must stay green unchanged. Each new behavior is proven by an
out-of-band `command` grader (a pytest the harness runs outside the agent's turn)
that asserts a real bounded end-state against a seeded over-bound / below-threshold
/ orphan-present fixture — a no-op or function-exists stub cannot fake it (#8 is
the inherited DoD).

## Decomposition Hint (for /fw-plan)
Each finding is an independent resource bound at a distinct layer; they share no
runtime invariant EXCEPT the container marker (producer #6 -> consumer #5). Group
by the module a fix lives in, one observable end-state per task:
- Layer supervisor-log-bound (`flywheel/_autopilot_supervisor.py` log path):
  satisfies #1. Independent.
- Layer run-log-bound (`flywheel_core.telemetry_file` / a worker-side reclaim that
  drives it — NOT in `task`/`lifecycle`; the worker today explicitly owns this
  seam): satisfies #2. Independent. The reclaim/cap logic must live in the
  telemetry/loaders/worker layer to preserve core purity.
- Layer worktree-sweep-cadence (`flywheel_worktree.worker` loop + `retention_sweep`):
  satisfies #3 and #4 (one cohesive change: make the existing sweep recur on a
  bounded cadence and prove both that a tick reclaims and that the cadence
  recurs). One task, two graders.
- Layer container-orphan-reap (`flywheel_container._docker` marker + the worker's
  startup reap call): satisfies #5 and #6. #5 (the startup reap selects and
  removes flywheel-owned orphans) DEPENDS ON #6 (containers carry the owner marker
  the scan selects on) — declare the edge so the consumer is not graded against a
  marker that does not exist yet. The marker identifier is the shared invariant
  between producer and consumer; fold both into the dependent pair with a
  round-trip holdout (the seam) rather than grading each in isolation.
- Layer disk-preflight (`flywheel_worktree.worker` preflight + a degraded-space
  record on the worker's status/telemetry surface): satisfies #7. Independent.
- Inherited DoD: #8 (`scripts/check.sh` + purity/SDK-optional suites) is a
  standing criterion every task above inherits.
Shared invariant: the flywheel container owner marker (the `--label`/owner
identifier produced at container creation and consumed by the startup reap scan)
is the one cross-task invariant — #6 produces it, #5 consumes it; do NOT split its
definition across the two nodes.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Bound each resource by reclaiming the OLDEST, preserving the newest  (Status: Accepted)
- Context: the cheapest fake for "bounded footprint" is delete-everything (zero
  bytes) or truncate-on-every-write, which destroys the observability the logs
  exist for; the run JSONL and supervisor logs are forensic state. | Decision:
  each bound reclaims OLD output (rotation/retention by age/count) while
  preserving the most-recent run(s)/log(s); graders assert BOTH an upper bound on
  footprint AND that recent content survives.
- Rejected: delete-all-on-bound (no forensics survive — provably gameable);
  size-cap a single file's name while still opening a new unbounded file per start
  (the supervisor's current shape, just renamed). | Consequences: each bound task
  carries a "most-recent survives" obligation, slightly more than a blunt cap.

### D-2: The run-log reclaim moves OFF "operator-owned" into an active worker behavior  (Status: Accepted)
- Context: the worker today explicitly leaves `.flywheel/logs/runs/` for the
  operator to rotate (`worker.py:1013-1016`); on a long-lived unattended fleet
  nobody does, so it grows until ENOSPC. | Decision: the worker gains an active,
  default-on run-log retention behavior so growth is bounded without an operator
  configuring logrotate. The reclaim/cap logic lives in the telemetry/loaders/
  worker layer, never in `core.task`/`core.lifecycle` (purity). | Rejected:
  keeping it operator-owned (the status quo that produces the leak); putting the
  disk/file logic in core (breaks the purity invariant). | Consequences: the
  worker now owns a retention surface it previously disclaimed; #2 grades the
  active reclaim, and #8 guards purity.

### D-3: Orphan reap keys on a flywheel-owner marker the container carries  (Status: Accepted)
- Context: the atexit backstop never runs on SIGKILL/OOM, and container orphans
  are outside git-lease reconciliation, so a killed worker's container survives
  with no reclaim path; container names are `flywheel-<task>-<uuid>` but there is
  no label the scan can safely select on. | Decision: containers carry a stable
  flywheel-owner marker (a `--label`/owner identifier) at creation, and the next
  worker start scans for and reaps flywheel-owned orphans by that exact marker —
  covering the killed-worker case while never touching non-flywheel containers. |
  Rejected: reaping by a broad match (collateral damage to unrelated containers);
  relying only on atexit (misses the SIGKILL case entirely); reconciling
  containers through the git-lease path (wrong seam — containers are not git
  leases). | Consequences: marker (producer) and scan (consumer) must agree on one
  identifier — #6 produces, #5 consumes, with the edge #5 requires #6 and a
  round-trip holdout so they cannot drift.

### D-4: Disk preflight surfaces a queryable degraded state and declines, never crashes  (Status: Accepted)
- Context: telemetry writes are already crash-swallowed (`harness.py:814`) but the
  authoritative store write is not, so an ENOSPC/inode-exhaustion there is an
  immediate hard stop with no warning to the operator. | Decision: a guarded
  low-space preflight, when free space/inodes are below threshold, records a
  queryable degraded-space state (on the worker's status/telemetry surface) and
  declines to proceed into the authoritative store write for that unit, rather than
  letting that write raise. The degraded record is the operator-visible early
  warning the hard crash never gives. | Rejected: catching the store-write OSError
  after the fact (the crash already happened / state may be half-written);
  swallowing low-space silently (no operator signal — the leak still ends the
  fleet, just quietly). | Consequences: #7 grades BOTH the queryable record AND
  that the crashing store write was not taken; the threshold is configurable
  (magnitude out of scope).

### D-5: Sweep cadence recurs while the daemon runs, not just at boot  (Status: Accepted)
- Context: the retention sweep runs once at `worker.py:2072` startup; a daemon
  with a high strand rate accumulates parked worktrees for up to the window PLUS
  its whole uptime before the next restart reclaims them. | Decision: the existing
  sweep recurs on a bounded cadence (per-cycle or on an interval) inside the main
  loop, preserving exactly what one sweep does (remove worktrees older than the
  window and their branches, prune dangling entries first). | Rejected: leaving it
  boot-only (the status quo leak); changing what one sweep removes (out of scope —
  only the cadence changes). | Consequences: #3 grades a mid-run tick reclaiming an
  aged worktree; #4 grades that the cadence recurs (one tick is not enough).

## Open Questions (accepted gaps)
- Cross-phase prerequisite (accepted, non-blocking): this phase removes slow-burn
  leaks that, unbounded, eventually become Phase-1-class hard crashes (disk-full
  -> store-write OSError). It assumes the Phase-1 containment floor
  (`00065-FEATURE-containment-floor`) exists so that any residual contained error
  surfaced by the degraded-space decline path is handled as a contained, recorded
  failure rather than crashing the loop. Phase 1's spec is authored by a sibling
  subagent; the exact contained-error classification name is reconciled at
  execution time via lifecycle records. This does not leave any criterion
  un-gradeable — each grades a bounded resource end-state independent of the
  downstream classification string.
- No un-gradeable criterion remains. Every criterion lowers to a `command` grader
  (a pytest the worker runs out-of-band) against this repo's real verification
  surface (`scripts/check.sh`, `uv run pytest -k ...`).

## Next Steps
Run `/fw-plan 00071-FEATURE-resource-leak-hardening` to compile these criteria
into flywheel tasks and graders. Tasks stage under
`.flywheel/tasks/staged/07-resource-hardening/` and must not activate until the
prior phases land.
