# Persistence Tables

Reference for `.workflow/flywheel.sqlite` (schema_version **7**). Canonical DDL: `src/flywheel/_schema/persistence-schema.sql` (Postgres mirror alongside). An empty table means "nothing exercised it on this database," not "deprecated."

Tasks are modeled in three tiers so every task reference in the store is a checkable foreign key, not a free-floating string:

| Table                | Purpose                                                                                                                                                                                          | Status                         |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------ |
| `tasks`              | Logical task identity (one row per `id`). The FK anchor every task reference points at. Carries no definition; a row may exist before the task is defined (declared as a prerequisite first).      | Active                         |
| `task_versions`      | Immutable, content-addressed definitions keyed `(task_id, content_hash)`. `content_hash` covers the *executed* definition only — goal + graders + context. FK `task_id` → `tasks(id)`.            | Active                         |
| `task_tags`          | Mutable labels on a task `(task_id, tag)`. Not in the content hash; last-write-wins on save. FK → `tasks(id)`; `idx_task_tags_tag` serves "every task labelled X."                               | Active                         |
| `task_prerequisites` | Mutable DAG edges `(task_id, prereq_task_id)`. Not in the content hash. Both endpoints FK → `tasks(id)`; `idx_task_prerequisites_prereq` serves the dependents lookup a parallel scheduler walks. | Active                         |
| `lifecycles`         | The single mutable row per run (`run_id`). `version` drives optimistic concurrency. `task_id` FK → `tasks(id)`; `task_content_hash` pins the exact `task_versions` row the run executed.           | Active                         |
| `attempts`           | Per-execution history under a run (`run_id`, `number`) with outcome + agent identity (`agent_context_json`).                                                                                     | Active                         |
| `events`             | Totally-ordered per-run audit log. `category` splits state-bearing `domain` events (folded into state) from `telemetry`.                                                                          | Active                         |
| `grader_results`     | Append-only receipt log, one row per grader execution. Snapshots the grader spec as it ran.                                                                                                     | Active                         |
| `sdk_messages`       | Verbatim agent SDK message stream per iteration. Largest table by row count.                                                                                                                    | Active                         |
| `run_sequence`       | Per-run monotonic counter shared by `events` + `sdk_messages` so both interleave into one ordered stream.                                                                                       | Active                         |
| `schema_version`     | Single sentinel row (`id=1`) pinning on-disk schema version; stores refuse a mismatched DB on open.                                                                                              | Active                         |
| `control_commands`   | Operator steering queue (interrupt / inject message / set model). CLI enqueues; the worker's watcher claim-once-applies. **Empty until live operator interaction.**                              | Active (empty by circumstance) |
| `task_claims`        | Multi-worker mutual exclusion: a worker holds a leased row while running a task and releases it on completion. **Empty when no worker is running** — claims are transient, not a log.             | Active (empty by circumstance) |

## Notes

- **Every durable task reference is a foreign key.** `lifecycles.task_id`, `task_versions.task_id`, `task_tags.task_id`, and both ends of `task_prerequisites` foreign-key `tasks(id)`. A run, tag, or DAG edge can never point at a task the catalog has never heard of. The identity is auto-registered on the seed path (and for not-yet-defined prerequisites), so the FK holds without forcing a save order.
- **Recovering "the exact task a run executed"** is `lifecycles.task_content_hash` → `task_versions(task_id, content_hash)` (`load_task_for_run`). The harness saves the definition before seeding the run, so the pinned version is always present.
- **Tags and prerequisites are out of the content hash on purpose.** They are mutable orchestration metadata — grouping/filtering labels and dependency edges that harnesses layered on flywheel rewire to build parallelizable task DAGs. Retagging or rewiring the DAG never forks the definition a run pinned; editing goal/graders/context does. `load_task` reattaches the task's *current* tags and prerequisites to the pinned definition.
- **`task_claims.task_id` is the one task reference with no FK — deliberately.** A claim is taken before the task definition is recorded (the orchestrator leases during selection, then `run_task` saves it) and deleted on completion: transient coordination state, not audit history, so it has no catalog row to anchor to and nothing reads it as history.
- Store contents are sensitive-by-default: payloads are persisted verbatim and unredacted. Treat the file as confidential.
- No in-place migration. A pre-v7 database is rejected with a "must be re-created" error.
