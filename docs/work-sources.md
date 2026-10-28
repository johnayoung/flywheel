# Work sources

A work source is the orchestrator's project-agnostic boundary with whatever owns the work — a directory of task JSON, an issue tracker, a CI feed, a review queue. The orchestrator schedules `WorkItem` values and does not care where they live. [workflow.md](workflow.md) introduces this seam narratively; this is the reference.

The contract is the `WorkSource` trait (`crates/flywheel-orchestrator/src/sources.rs`):

```rust
pub trait WorkSource: Send + Sync {
    fn source_kind(&self) -> &str;
    fn source_name(&self) -> String;
    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError>;
    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError>;
}
```

Any type implementing it qualifies; the orchestrator holds it as `Box<dyn WorkSource>`.

## The seam

### Inbound: `list_work()`

Enumerate the current candidate items, each already compiled to a validated [core `Task`](task-schema.md) plus orchestration-layer `prerequisites` and an opaque `source_ref`. Called once per scheduling pass, so an implementation should be cheap-ish and deterministic in order — selection ties break by enumeration order.

**Anything that cannot compile to a `Task` with at least one grader never reaches the scheduler.** A tracker item with no graders and no default-grader policy is the adapter's problem to skip and surface; it is filtered before scheduling, not failed mid-run.

**A parse or transport break must return an error, never `Ok(vec![])`.** Adapter-internal failures (a tracker API error, an unreadable directory, a non-zero `gh` exit) surface as `WorkSourceError` — `Source` (adapter message), `TaskLoad` (a wrapped core `TaskLoadError`), or `Io` (a filesystem failure naming the path). An empty list reads downstream as "all that work disappeared" and would let the reconciler interrupt every live run (see [Steering](#steering)); erroring keeps the failure contained.

### Outbound: `report(WorkReport)`

Receives the terminal outcome of one driven run, for the work owner. Called after the consumer `submit` step (see [strategy.md](strategy.md)), while the task lease is still held — so two workers never report the same task concurrently.

**Best-effort delivery: a failing `report` is contained by the orchestrator and never unwinds the scheduling loop.** The store remains flywheel's authoritative record; the report is a projection for the owner (close a ticket, post grader receipts).

**Ticket writes go through `report`, never through the agent.** The "agent claims are untrusted" invariant extends to ticket transitions: the harness owns every write-back.

### Provenance: `source_kind` / `source_name`

`source_kind` (the provenance tag) and `source_name` (the source's locus — a `tasks_dir` path or `owner/repo`) are required trait methods. `sync_work_source` (`crates/flywheel-orchestrator/src/orchestrate/sweep.rs`) reads both to stamp the `source_syncs` row, so an adapter missing either fails at compile time, not at sync time.

## Data shapes

`WorkItem` (`crates/flywheel-orchestrator/src/sources.rs`) is the schedulable unit:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `task` | `Task` | required | the compiled, validated core Task — the only thing that crosses into flywheel |
| `prerequisites` | `Vec<String>` | empty | orchestration-layer DAG edges (task ids that must reach DONE first); core `Task` has no such field |
| `source_ref` | `String` | required | adapter's opaque handle; round-trips into `WorkReport`, persisted as the lifecycle's `source` label |
| `local_path` | `Option<PathBuf>` | `None` | set only by file-backed sources; non-file sources leave `None` |
| `phase` | `Option<String>` | `None` | the `active/<phase>` directory of a file-backed item; `None` for non-file sources |
| `source_kind` | `Option<String>` | `None` | provenance tag (`"directory"`, `"github_issue"`, `"github_ci"`, `"github_review"`) |
| `source_version` | `Option<String>` | `None` | stable content hash that changes when the underlying definition is edited |
| `source_url` | `Option<String>` | `None` | a locator (file path, issue/run/thread URL) |
| `priority` | `i64` | `0` | scheduling metadata; core flywheel ignores it |
| `required_capabilities` | `BTreeSet<String>` | empty | scheduler offers a worker only items whose set is a subset of its `[execution] capabilities` |
| `conflict_keys` | `BTreeSet<String>` | empty | scheduling metadata |

`WorkItem::new(task, source_ref)` sets every optional field to its default; `with_prerequisites`/`with_priority`/`with_required_capabilities`/`with_conflict_keys` fill the rest. File-backed sources read `priority`/`required_capabilities`/`conflict_keys` from the task file's top-level JSON (`scheduling_metadata_from_value`); a JSON boolean or non-integral number is never a priority (it falls back to `0`).

`WorkReport` is the terminal outcome handed to `report`: `task_id`, `source_ref`, `run_id`, `status` (a core lifecycle `Status` — `Done` on verified completion, `Failed`/`FailedValidation` on exhausted budgets, `Interrupted`/`AwaitingApproval` when parked), `error`, and `graders` (a `Vec<GraderReceipt>`). `GraderReceipt` is a flattened store receipt — `ordinal`, `grader_type`, `name`, `passed` — rendered into report bodies.

## Shipped sources

Four kinds ship, all resolved through the registry by their `[source] kind` (see [configuration.md](configuration.md)). The three GitHub kinds drive the `gh` CLI as a subprocess (no extra crate dependency), so `gh` must be on PATH and authenticated (`gh auth`); flywheel does no auth itself and fails closed on any non-zero `gh` exit.

| Kind | Lists | Task id | Graded by | Reports back |
|---|---|---|---|---|
| `directory` | `active/<phase>/*.json` task files | task file's `id` | the task's own graders (always present) | no-op |
| `github` | labeled open issues | `gh-<number>` | issue's `flywheel` block, else `[[defaults.graders]]` | issue comment, or close |
| `github_ci` | failed CI runs | `ci-<hash(workflow,branch)>` | `[[defaults.graders]]` only | commit comment |
| `github_review` | unresolved PR review threads | `prc-<hash(thread)>` | `[[defaults.graders]]` only | PR comment |

**Tracker sources need a grader policy.** `[[defaults.graders]]` is meaningful only for tracker kinds — directory tasks always carry their own graders, so the policy is a no-op there. A tracker item that resolves zero graders is not runnable: it is skipped and logged, never scheduled.

### `directory`

The reference adapter (`DirectoryWorkSource`, `crates/flywheel-orchestrator/src/sources.rs`). Walks the historical `<tasks_dir>/active/<phase>/<task>.json` layout in sorted phase-then-filename order, emitting one `WorkItem` per active task file with `local_path` set, `source_kind="directory"`, and `source_version = task_digest(task)`. An unloadable task file (malformed JSON, a definition failing validation) is skipped, not fatal: it is counted in `last_skipped_count` and logged to the `log` sink when one is attached. Directory tasks always carry graders (the [task schema](task-schema.md) requires them), so `[[defaults.graders]]` does not apply.

`report()` is intentionally a no-op: the store is already the durable local record, and phase archiving is a separate directory-shaped flow driven by the worker (`archive_completed_phases`, `crates/flywheel-worktree/src/phase.rs`). `local_path` is what the worktree submitter reads for phase-from-directory branch naming.

```toml
[source]
kind = "directory"               # default kind
tasks_dir = ".flywheel/tasks"    # optional; defaults to DEFAULT_TASKS_DIR
```

### `github` (labeled issues)

`GithubWorkSource` (`crates/flywheel-orchestrator/src/github.rs`) maps labeled open issues to tasks. Listing runs `gh issue list --repo <repo> --label <label> --state open --json number,title,body,url --limit 200`; issues sort by number ascending.

Per issue (`compile_issue`):

- `id` = `gh-<number>` (referenceable from other issues' `prerequisites`).
- `goal` = the issue title by default.
- **Embedded spec block:** an issue may carry a fenced block whose info string is exactly ` ```flywheel `. The block is JSON; every key optional — `goal`, `graders`, `context`, `tags`, `prerequisites` — and any present key overrides the default. A **present-but-invalid** block fails that issue with `WorkSourceError` (the author meant it; silent fallback would mask the error): `list_work` logs and skips the malformed issue, and returns the error only when no issue on the board compiled, so a fully broken board never reads as empty.
- `graders` come from the spec block if present, else `[[defaults.graders]]`. Zero graders from either source means the issue is skipped and logged.
- `context.notes` falls back to the issue body, and the issue URL is appended to `context.references`, so the agent sees the full ticket text.
- `source_ref` = `<repo>#<number>`; `source_kind="github_issue"` (note: the registry kind is `github`, the provenance tag is `github_issue`).

`report()` builds a body (run-id/status line, optional error, a markdown table of grader receipts). On `DONE` with `done_action = "close"` it runs `gh issue close <n> --comment <body>`; otherwise `gh issue comment <n> --body <body>`.

```toml
[source]
kind = "github"
repo = "owner/name"          # required
label = "flywheel"           # required
done_action = "comment"      # or "close"; default "comment"

[[defaults.graders]]         # fallback for issues with no spec-block graders
type = "command"
run = "cargo test"
```

### `github_ci` (failed CI runs)

`GithubCiWorkSource` (`crates/flywheel-orchestrator/src/github_ci.rs`) turns a red build into a graded task. Listing runs `gh run list --repo <repo> --status <failure_filter> --json ... --limit 200`. Runs dedupe to one item per `(workflow, branch)`, keeping the most recent — a persistently-broken branch is one work item, not several.

Per item (`compile_run`):

- `id` = `ci-<sha256(workflow, branch)[:16]>` — **stable across polls**, deliberately not keyed on the per-run `databaseId` (which advances as the head moves, producing unbounded duplicates).
- `goal` = `Fix the failing CI workflow '<workflow>' on branch '<branch>'.`; `context.notes` locates the failure (workflow, branch, head sha, conclusion).
- `graders` = `[[defaults.graders]]` only (a CI run carries no spec block). Zero graders means the item is skipped and logged, so a default grader policy is required in practice.
- `source_ref` = `<repo>@<head_sha>`; `source_kind="github_ci"`; `source_version = sha256(head_sha, conclusion)` so a failure moving to a new commit is detected even though the id is stable.

**The "fixed" verdict is the harness running the operator's default graders out of band — never the GitHub check status, never a re-run** (spec 00052). The source issues no `gh` call that reads run status as the verdict. This forecloses the cheapest CI-fix hack: disabling or editing the failing workflow turns the build green, but the out-of-band graders still fail.

`report()` posts the grader receipts as a commit comment via `gh api --method POST repos/<repo>/commits/<head_sha>/comments` — never an issue mutation, never a check-status flip. The audit trail lands on the commit the failure was listed against.

```toml
[source]
kind = "github_ci"
repo = "owner/name"          # required
failure_filter = "failure"   # optional; the gh run --status value; default "failure"

[[defaults.graders]]         # required in practice: a CI item with zero graders is skipped
type = "command"
run = "cargo test"
```

### `github_review` (unresolved PR review threads)

`GithubReviewWorkSource` (`crates/flywheel-orchestrator/src/github_review.rs`) turns a reviewer's request into work. Listing runs one bounded `gh api graphql` query (`isResolved` is GraphQL-only) fetching open PRs, their review threads, and each thread's comments. Unlike the issue and CI sources, it carries no `done_action`/`failure_filter` knob.

Per thread (`compile_identified_thread`):

- **Resolution is the candidate filter only, never the verdict.** A resolved thread is skipped (no item is emitted), but no grade is derived from resolution state.
- `id` = `prc-<sha256(thread_node_id)[:16]>` — stable across polls and across appended replies, deliberately not keyed on comment id or count.
- `goal` = `Address the unresolved review thread on pull request #<n> in <repo>.`; `context.notes` carries the full thread (every comment body with its author, most-recent last).
- `graders` = `[[defaults.graders]]` only — **never the thread's `isResolved` state, never compiled from the review-comment text** (spec 00053). A review comment is subjective and not a check. Zero graders means the thread is skipped and logged.
- `source_ref` = `<repo>#<pr>#<thread_node_id>`; `source_kind="github_review"`; `source_version` changes when a reply is appended though the id is stable.

**The grade is the operator's `[[defaults.graders]]` run out of band — a deliberate anti-gaming decision.** `report()` posts receipts as a PR comment via `gh pr comment <pr> --body <body>` and issues no resolve or in-thread-reply mutation: the harness never flips a thread's resolution state, the one signal that must stay untrusted. A PR comment (not an in-thread reply, which is API-adjacent to `resolveReviewThread`) makes accidental resolution impossible by construction.

```toml
[source]
kind = "github_review"
repo = "owner/name"          # required

[[defaults.graders]]         # required in practice: a thread with zero graders is skipped
type = "command"
run = "cargo test"
```

## Listing-truncation transparency

GitHub listings are capped at one page (200 for the REST issue/CI feeds, 50 per axis for the GraphQL review query); the cap is not configurable. When a listing is truncated, a shared emitter `emit_truncation_warning` (`crates/flywheel-orchestrator/src/github.rs`) warns so silent under-coverage never reads as "nothing to do":

```text
[<source>] <unit> listing truncated at one page; some items were not read this pass
```

The REST sources trigger it when a returned page size equals the 200 cap (a conservative full-page heuristic that may false-positive at exactly 200). The review source triggers it per axis off the real GraphQL `pageInfo.hasNextPage`. The warning is a pure side channel: the returned work sequence is never dropped, trimmed, or reordered — overflow simply re-drains on the next scheduling pass.

**Partial: the warning sink is not wired to an operator log at runtime.** Every adapter defaults to no `log` sink (and no stop-event sink), and `build_work_source` (`crates/flywheel-orchestrator/src/workflow/mod.rs`) builds each source from a `SourceBuildContext` carrying neither. Truncation warnings and grader-less "skipping ... not runnable" lines are therefore emitted only when a caller attaches a `LogSink` (the tests do). In production today both signals go nowhere — a board exceeding 200 items drains silently with no operator-visible warning. The dropped items still re-drain next pass; the gap is observability, not correctness.

## Steering

A reconciler re-lists the source on an interval and reconciles in-flight runs against the new listing (`reconcile_live_runs`, `crates/flywheel-orchestrator/src/orchestrate/sweep.rs`). An in-flight run whose item has vanished — a closed issue, a pulled label, a deleted task file — is interrupted: the reconciler enqueues an `interrupt` control command, and the run parks `INTERRUPTED` with its sandbox preserved. Restore the item and it resumes on the next pass.

**A listing failure never interrupts anything.** A failed `list_work()` returns an error (never an empty list), so the reconciler treats it as "could not observe," not "all work disappeared" — no live run is touched.

The interval is `reconcile_seconds`:

| Surface | Default |
|---|---|
| `flywheel worker` / daemon (`--reconcile-seconds`) | `15.0` |
| `orchestrate` library call (`OrchestrateRequest.reconcile_seconds`) | `None` (off) |
| `0` (any surface) | disables the reconciler |

See [orchestration.md](orchestration.md) for how the reconciler runs alongside the scheduling loop and [cli.md](cli.md) for the `worker` flags.

## Registry and extensibility

Sources resolve through a `SourceRegistry` (`crates/flywheel-orchestrator/src/source_registry.rs`), the same compile-time "pick a named backend" mechanism used for stores and submit strategies. `default_registry()` registers `directory`; `register_github_sources` (`crates/flywheel-orchestrator/src/github.rs`) adds `github`, `github_ci`, and `github_review`; `default_source_registry()` (`crates/flywheel-orchestrator/src/workflow/mod.rs`) is the shipped combination. `build_work_source(policy, registry)` builds a `SourceBuildContext` (the `[source]` table plus `[[defaults.graders]]`) and calls `registry.build(policy.source_kind, ...)`; an unknown kind fails with `SourceRegistryError::Unknown`, listing the registered names. An explicit `--tasks-dir` always selects the directory source (historical behavior); otherwise the policy's `[source] kind` decides.

**There is no runtime plugin discovery.** The registry is a plain table built in code; adding a kind means compiling it in. A new adapter implements `WorkSource` plus a `SourceFactory` (`Fn(&SourceBuildContext) -> Result<Box<dyn WorkSource>, SourceRegistryError>`) and is added with `SourceRegistry::register(name, summary, factory)` (re-registering a name replaces it in place) or `register_if_absent` (**built-ins win a name collision** — an extension can add choices but never shadow a shipped kind). The shipped `flywheel` binary uses `default_source_registry()`, so a Jira, Linear, or queue source requires a code change and rebuild.

A listing pass is persisted by `sync_work_source` (`crates/flywheel-orchestrator/src/orchestrate/sweep.rs`): it opens a `source_syncs` row, and on success upserts every observed item, replaces dependency edges, and marks previously-seen-but-now-absent items disappeared. **On a failed `list_work()` it finishes the row `status='error'` and marks nothing disappeared** — the load-bearing invariant an erroring source exists to protect: a tracker hiccup is never read as task disappearance.

### Writing an adapter

A conforming adapter implements `WorkSource`:

- `list_work()` returning `Vec<WorkItem>`, each carrying a validated `Task` with at least one grader; return `WorkSourceError` on any enumerate/compile failure, never an empty list.
- `report(&WorkReport)` performing the write-back; it may return an error (the orchestrator contains it), but should be idempotent against repeated delivery.
- `source_kind()` and `source_name()` (stamped by `sync_work_source` onto `source_syncs`).

Register it as described above.

Direct `WorkItem`/`WorkReport` construction is the API; the provenance and scheduling fields are optional conveniences adapters populate.
