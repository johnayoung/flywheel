# Persistence Tables

Reference for the flywheel core store (schema_version **9**). Canonical DDL: `packages/flywheel/src/flywheel/_schema/persistence-schema.sql` (Postgres mirror alongside). An empty table means "nothing exercised it on this database," not "deprecated."

Flywheel core owns the lifecycle of a **single task**. The catalog therefore records only what defines and verifies one task; cross-task concerns (the dependency DAG) live in the orchestration layer built on top, not here.

| Table              | Purpose                                                                                                                                                                          | Status                         |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------ |
| `tasks`            | Logical task identity (one row per `id`). The FK anchor every task reference points at. Carries no definition.                                                                   | Active                         |
| `task_versions`    | Immutable, content-addressed definitions keyed `(task_id, content_hash)`. `content_hash` covers the whole definition — goal + graders + tags + context. FK `task_id` → `tasks(id)`. | Active                         |
| `lifecycles`       | The single mutable row per run (`run_id`). `version` drives optimistic concurrency. `task_id` FK → `tasks(id)`; `task_content_hash` pins the exact `task_versions` row the run ran. | Active                         |
| `attempts`         | Per-execution history under a run (`run_id`, `number`) with outcome + agent identity (`agent_context_json`).                                                                     | Active                         |
| `events`           | Totally-ordered per-run audit log. `category` splits state-bearing `domain` events (folded into state) from `telemetry`.                                                          | Active                         |
| `grader_results`   | Append-only receipt log, one row per grader execution. Snapshots the grader spec as it ran.                                                                                     | Active                         |
| `sdk_messages`     | Verbatim agent SDK message stream per iteration. Largest table by row count.                                                                                                    | Active                         |
| `run_sequence`     | Per-run monotonic counter shared by `events` + `sdk_messages` so both interleave into one ordered stream.                                                                       | Active                         |
| `schema_version`   | Single sentinel row (`id=1`) pinning on-disk schema version; stores refuse a mismatched DB on open.                                                                              | Active                         |
| `control_commands` | Operator steering queue for one live run (interrupt / inject message / set model). CLI enqueues; the in-run watcher claim-once-applies. **Empty until live operator interaction.** | Active (empty by circumstance) |

## Notes

- **Every durable task reference is a foreign key.** `lifecycles.task_id` and `task_versions.task_id` foreign-key `tasks(id)`; a run can never point at a task the catalog has never heard of. The identity is auto-registered on the seed path, so the FK holds without forcing a save order.
- **Recovering "the exact task a run executed"** is `lifecycles.task_content_hash` → `task_versions(task_id, content_hash)` (`load_task_for_run`). The harness saves the definition before seeding the run, so the pinned version is always present.
- **The content hash covers the full definition** — goal, graders, tags, context. Editing any of them mints a new immutable version; a run pins exactly the version it executed, so historical truth survives later edits.
- **`prerequisites` is not persisted by core.** The inter-task dependency DAG is an orchestration-layer concept; flywheel core records only the single-task definition. The orchestration layer keeps its own scheduling state.
- **Multi-worker mutual exclusion lives outside core.** The `task_claims` lease is transient coordination, not audit history, and only the orchestration layer uses it — so it lives in `flywheel_orchestrator`'s own store (its own `task_claims` + `orchestrator_schema_version` tables), which can share this database file but owns its own tables. The single-task loop has no notion of competing workers.
- Store contents are sensitive-by-default: payloads are persisted verbatim and unredacted. Treat the file as confidential.
- No in-place migration. A pre-v9 database is rejected with a "must be re-created" error.
