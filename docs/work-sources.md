# Work sources

A work source is the orchestrator's project-agnostic boundary with whatever owns the work — a directory of task JSON, an issue tracker, a CI feed, a review queue. The orchestrator schedules `WorkItem` values and does not care where they live. [workflow.md](workflow.md) introduces this seam narratively; this is the reference.

The contract is two methods (`flywheel_orchestrator._sources.WorkSource`, `_sources.py:129`):

```python
@runtime_checkable
class WorkSource(Protocol):
    def list_work(self) -> Sequence[WorkItem]: ...
    def report(self, report: WorkReport) -> None: ...
```

It is a structural `Protocol` — any object with these two methods (plus the duck-typed `source_kind`/`source_name`, below) qualifies. No base class to inherit.

## The seam

### Inbound: `list_work()`

Enumerate the current candidate items, each already compiled to a validated [core `Task`](task-schema.md) plus orchestration-layer `prerequisites` and an opaque `source_ref`. Called once per scheduling pass, so an implementation should be cheap-ish and deterministic in order — selection ties break by enumeration order.

**Anything that cannot compile to a `Task` with at least one grader never reaches the scheduler.** A tracker item with no graders and no default-grader policy is the adapter's problem to skip and surface; it is filtered before scheduling, not failed mid-run.

**A parse or transport break must raise, never return `[]`.** Adapter-internal failures (a tracker API error, an unreadable directory, a non-zero `gh` exit) surface as `WorkSourceError` (`_sources.py:42`) — a `RuntimeError` subclass — or `flywheel_core.loaders.TaskLoadError`. An empty list reads downstream as "all that work disappeared" and would let the reconciler interrupt every live run (see [Steering](#steering)); raising keeps the failure contained.

### Outbound: `report(WorkReport)`

Receives the terminal outcome of one driven run, for the work owner. Called after the consumer `submit` step (see [strategy.md](strategy.md)), while the task lease is still held — so two workers never report the same task concurrently.

**Best-effort delivery: a raising `report` is contained by the orchestrator and never unwinds the scheduling loop.** The store remains flywheel's authoritative record; the report is a projection for the owner (close a ticket, post grader receipts).

**Ticket writes go through `report`, never through the agent.** The "agent claims are untrusted" invariant extends to ticket transitions: the harness owns every write-back.

### Duck-typed provenance (not in the Protocol)

Every shipped adapter additionally exposes a class attribute `source_kind` (the provenance tag) and a `source_name` property (the source's locus — a `tasks_dir` path or `owner/repo`). `sync_work_source` reads both off the adapter to stamp the `source_syncs` row (`_orchestrate.py:303`). They are not part of the `WorkSource` Protocol, so a new adapter must remember to provide both or syncing fails.

## Data shapes

`WorkItem` (`_sources.py:52`) is the schedulable unit — frozen, `kw_only`:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `task` | `Task` | required | the compiled, validated core Task — the only thing that crosses into flywheel |
| `prerequisites` | `tuple[str, ...]` | `()` | orchestration-layer DAG edges (task ids that must reach DONE first); core `Task` has no such field |
| `source_ref` | `str` | required | adapter's opaque handle; round-trips into `WorkReport`, persisted as the lifecycle's `source` label |
| `local_path` | `Path \| None` | `None` | set only by file-backed sources; non-file sources leave `None` |
| `source_kind` | `str \| None` | `None` | provenance tag (`"directory"`, `"github_issue"`, `"github_ci"`, `"github_review"`) |
| `source_version` | `str \| None` | `None` | stable content hash that changes when the underlying definition is edited |
| `source_url` | `str \| None` | `None` | a locator (file path, issue/run/thread URL) |
| `priority` | `int` | `0` | scheduling metadata; core flywheel ignores it |
| `required_capabilities` | `frozenset[str]` | `frozenset()` | scheduler offers a worker only items whose set is a subset of its `[execution] capabilities` |
| `conflict_keys` | `frozenset[str]` | `frozenset()` | scheduling metadata |

All provenance and scheduling fields are optional, so direct `WorkItem` construction without them still compiles. File-backed sources read `priority`/`required_capabilities`/`conflict_keys` from the task file's top-level JSON (`_read_scheduling_metadata`, `_sources.py:178`); a JSON `bool` is rejected as a priority.

`WorkReport` (`_sources.py:109`) is the terminal outcome handed to `report`: `task_id`, `source_ref`, `run_id`, `status` (a `flywheel_core.lifecycle.Status` — `DONE` on verified completion, `FAILED`/`FAILED_VALIDATION` on exhausted budgets, `INTERRUPTED`/`AWAITING_APPROVAL` when parked), `error`, and `graders` (a tuple of `GraderReceipt`). `GraderReceipt` (`_sources.py:94`) is a flattened store receipt — `ordinal`, `grader_type`, `name`, `passed` — rendered into report bodies.

## Shipped sources

Four kinds ship, all resolved through the registry by their `[source] kind` (see [configuration.md](configuration.md)). The three GitHub kinds drive the `gh` CLI subprocess — no Python dependency, no `pip` extra — so `gh` must be on PATH and authenticated (`gh auth`); flywheel does no auth itself and fails closed on any non-zero `gh` exit.

| Kind | Lists | Task id | Graded by | Reports back |
|---|---|---|---|---|
| `directory` | `active/<phase>/*.json` task files | task file's `id` | the task's own graders (always present) | no-op |
| `github` | labeled open issues | `gh-<number>` | issue's `flywheel` block, else `[[defaults.graders]]` | issue comment, or close |
| `github_ci` | failed CI runs | `ci-<hash(workflow,branch)>` | `[[defaults.graders]]` only | commit comment |
| `github_review` | unresolved PR review threads | `prc-<hash(thread)>` | `[[defaults.graders]]` only | PR comment |

**Tracker sources need a grader policy.** `[[defaults.graders]]` is meaningful only for tracker kinds — directory tasks always carry their own graders, so the policy is a no-op there. A tracker item that resolves zero graders is not runnable: it is skipped and logged, never scheduled.

### `directory`

The reference adapter (`DirectoryWorkSource`, `_sources.py:249`). Walks the historical `<tasks_dir>/active/<phase>/<task>.json` layout in sorted phase-then-filename order, emitting one `WorkItem` per active task file with `local_path` set, `source_kind="directory"`, and `source_version = task_digest(task)`. Directory tasks always carry graders (the [task schema](task-schema.md) requires them), so `[[defaults.graders]]` does not apply.

`report()` is intentionally a no-op (`_sources.py:296`): the store is already the durable local record, and phase archiving is a separate directory-shaped flow driven by the worker (`archive_completed_phases`). `local_path` is what the worktree submitter reads for phase-from-directory branch naming.

```toml
[source]
kind = "directory"               # default kind
tasks_dir = ".flywheel/tasks"    # optional; defaults to DEFAULT_TASKS_DIR
```

### `github` (labeled issues)

`GithubWorkSource` (`_github.py:198`) maps labeled open issues to tasks. Listing runs `gh issue list --repo <repo> --label <label> --state open --json number,title,body,url --limit 200`; issues sort by number ascending.

Per issue (`_github.py:279`):

- `id` = `gh-<number>` (referenceable from other issues' `prerequisites`).
- `goal` = the issue title by default.
- **Embedded spec block:** an issue may carry a fenced block whose info string is exactly ` ```flywheel `. The block is JSON; every key optional — `goal`, `graders`, `context`, `tags`, `prerequisites` — and any present key overrides the default. A **present-but-invalid** block raises `WorkSourceError` (the author meant it; silent fallback would mask the error).
- `graders` come from the spec block if present, else `[[defaults.graders]]`. Zero graders from either source means the issue is skipped and logged.
- `context.notes` falls back to the issue body, and the issue URL is appended to `context.references`, so the agent sees the full ticket text.
- `source_ref` = `<repo>#<number>`; `source_kind="github_issue"` (note: the registry kind is `github`, the provenance tag is `github_issue`).

`report()` (`_github.py:343`) builds a body (run-id/status line, optional error, a markdown table of grader receipts). On `DONE` with `done_action = "close"` it runs `gh issue close <n> --comment <body>`; otherwise `gh issue comment <n> --body <body>`.

```toml
[source]
kind = "github"
repo = "owner/name"          # required
label = "flywheel"           # required
done_action = "comment"      # or "close"; default "comment"

[[defaults.graders]]         # fallback for issues with no spec-block graders
type = "command"
run = "uv run pytest"
```

### `github_ci` (failed CI runs)

`GithubCiWorkSource` (`_github_ci.py:102`) turns a red build into a graded task. Listing runs `gh run list --repo <repo> --status <failure_filter> --json ... --limit 200`. Runs dedupe to one item per `(workflow, branch)`, keeping the most recent — a persistently-broken branch is one work item, not several.

Per item (`_github_ci.py:203`):

- `id` = `ci-<sha256(workflow, branch)[:16]>` — **stable across polls**, deliberately not keyed on the per-run `databaseId` (which advances as the head moves, producing unbounded duplicates).
- `goal` = `Fix the failing CI workflow '<workflow>' on branch '<branch>'.`; `context.notes` locates the failure (workflow, branch, head sha, conclusion).
- `graders` = `[[defaults.graders]]` only (a CI run carries no spec block). Zero graders means the item is skipped and logged, so a default grader policy is required in practice.
- `source_ref` = `<repo>@<head_sha>`; `source_kind="github_ci"`; `source_version = sha256(head_sha, conclusion)` so a failure moving to a new commit is detected even though the id is stable.

**The "fixed" verdict is the harness running the operator's default graders out of band — never the GitHub check status, never a re-run** (spec 00052). The source issues no `gh` call that reads run status as the verdict. This forecloses the cheapest CI-fix hack: disabling or editing the failing workflow turns the build green, but the out-of-band graders still fail.

`report()` (`_github_ci.py:262`) posts the grader receipts as a commit comment via `gh api --method POST repos/<repo>/commits/<head_sha>/comments` — never an issue mutation, never a check-status flip. The audit trail lands on the commit the failure was listed against.

```toml
[source]
kind = "github_ci"
repo = "owner/name"          # required
failure_filter = "failure"   # optional; the gh run --status value; default "failure"

[[defaults.graders]]         # required in practice: a CI item with zero graders is skipped
type = "command"
run = "uv run pytest"
```

### `github_review` (unresolved PR review threads)

`GithubReviewWorkSource` (`_github_review.py:155`) turns a reviewer's request into work. Listing runs one bounded `gh api graphql` query (`isResolved` is GraphQL-only) fetching open PRs, their review threads, and each thread's comments. Unlike the issue and CI sources, it carries no `done_action`/`failure_filter` knob.

Per thread (`_github_review.py:252`):

- **Resolution is the candidate filter only, never the verdict.** A resolved thread is skipped (`if isResolved: return None`), but no grade is derived from resolution state.
- `id` = `prc-<sha256(thread_node_id)[:16]>` — stable across polls and across appended replies, deliberately not keyed on comment id or count.
- `goal` = `Address the unresolved review thread on pull request #<n> in <repo>.`; `context.notes` carries the full thread (every comment body with its author, most-recent last).
- `graders` = `[[defaults.graders]]` only — **never the thread's `isResolved` state, never compiled from the review-comment text** (spec 00053). A review comment is subjective and not a check. Zero graders means the thread is skipped and logged.
- `source_ref` = `<repo>#<pr>#<thread_node_id>`; `source_kind="github_review"`; `source_version` changes when a reply is appended though the id is stable.

**The grade is the operator's `[[defaults.graders]]` run out of band — a deliberate anti-gaming decision.** `report()` (`_github_review.py:355`) posts receipts as a PR comment via `gh pr comment <pr> --body <body>` and issues no resolve or in-thread-reply mutation: the harness never flips a thread's resolution state, the one signal that must stay untrusted. A PR comment (not an in-thread reply, which is API-adjacent to `resolveReviewThread`) makes accidental resolution impossible by construction.

```toml
[source]
kind = "github_review"
repo = "owner/name"          # required

[[defaults.graders]]         # required in practice: a thread with zero graders is skipped
type = "command"
run = "uv run pytest"
```

## Listing-truncation transparency

GitHub listings are capped at one page (200 for the REST issue/CI feeds, 50 per axis for the GraphQL review query); the cap is not configurable. When a listing is truncated, a shared emitter `emit_truncation_warning` (`_github.py:90`) warns so silent under-coverage never reads as "nothing to do":

```text
[<source>] <unit> listing truncated at one page; some items were not read this pass
```

The REST sources trigger it when a returned page size equals the 200 cap (a conservative full-page heuristic that may false-positive at exactly 200). The review source triggers it per axis off the real GraphQL `pageInfo.hasNextPage`. The warning is a pure side channel: the returned work sequence is never dropped, trimmed, or reordered — overflow simply re-drains on the next scheduling pass.

**Partial: the warning sink is not wired to an operator log at runtime.** Every adapter defaults `log=None`, and the `_policy.py` builders construct each source without passing `log=`. Truncation warnings and grader-less "skipping ... not runnable" lines are therefore emitted only when a caller injects a `log` callable (the tests do). In production today both signals go nowhere — a board exceeding 200 items drains silently with no operator-visible warning. The dropped items still re-drain next pass; the gap is observability, not correctness.

## Steering

A reconciler re-lists the source on an interval and reconciles in-flight runs against the new listing (`reconcile_live_runs`, `_orchestrate.py:174`). An in-flight run whose item has vanished — a closed issue, a pulled label, a deleted task file — is interrupted: the reconciler enqueues an `interrupt` control command, and the run parks `INTERRUPTED` with its sandbox preserved. Restore the item and it resumes on the next pass.

**A listing failure never interrupts anything.** A failed `list_work()` raises (it never returns `[]`), so the reconciler treats it as "could not observe," not "all work disappeared" — no live run is touched.

The interval is `reconcile_seconds`:

| Surface | Default |
|---|---|
| `flywheel worker` / daemon (`--reconcile-seconds`) | `15.0` |
| `orchestrate(...)` library call | `None` (off) |
| `0` (any surface) | disables the reconciler |

See [orchestration.md](orchestration.md) for how the reconciler runs alongside the scheduling loop and [cli.md](cli.md) for the `worker` flags.

## Registry and extensibility

Sources resolve through the `SOURCES` registry (`_source_registry.py:20`), the same "pick a named backend" mechanism used for stores and submit strategies. `build_work_source(policy)` calls `SOURCES.resolve(policy.source_kind)` to import and build the per-kind adapter on demand. An explicit `--tasks-dir` always selects the directory source (historical behavior); otherwise the policy's `[source] kind` decides.

A third-party adapter advertises a `flywheel.work_sources` entry point. On first access the registry scans `importlib.metadata.entry_points(group="flywheel.work_sources")` and registers any adapter not already built in, so a Jira, Linear, or queue source is selectable with no fork and no flywheel code change. **Built-ins win a name collision** — an installed plugin can extend the choices but never shadow a shipped kind. A discovered spec declares no extra, so a missing dependency in the plugin's own module surfaces as its raw `ImportError`.

A listing pass is persisted by `sync_work_source` (`_orchestrate.py:285`): it opens a `source_syncs` row, and on success upserts every observed item, replaces dependency edges, and marks previously-seen-but-now-absent items disappeared. **On a failed `list_work()` it finishes the row `status='error'` and marks nothing disappeared** — the load-bearing invariant a raising source exists to protect: a tracker hiccup is never read as task disappearance.

### Writing an adapter

A conforming adapter provides:

- `list_work()` returning a `Sequence[WorkItem]`, each carrying a validated `Task` with at least one grader; raise `WorkSourceError` on any enumerate/compile failure, never return `[]`.
- `report(WorkReport)` performing the write-back; it may raise (the orchestrator contains it), but should be idempotent against repeated delivery.
- A `source_kind` class attribute and a `source_name` property (required by `sync_work_source`, not by the Protocol).

Direct `WorkItem`/`WorkReport` construction is the API; the provenance and scheduling fields are optional conveniences adapters populate.
