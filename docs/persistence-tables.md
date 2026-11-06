# Persistence Tables

Reference for the flywheel store: the core run tables (schema_version **13**) and the orchestrator's coordination tables (orchestrator_schema_version **10**). Canonical DDL: `crates/flywheel-core/schema/persistence-schema.sql` (Postgres mirror: `persistence-schema-postgres.sql` alongside); orchestrator DDL: `crates/flywheel-orchestrator/src/claims.rs` (SQLite) and `claims_postgres.rs` (Postgres). An empty table means "nothing exercised it on this database," not "deprecated."

Postgres `TEXT`/`JSONB` cannot hold U+0000: the Postgres store writes NULs in free-form run data (agent output, errors, blocked-requires, agent context, event/grader/control-command payloads) as U+FFFD; SQLite keeps them verbatim.

Flywheel core owns the lifecycle of a **single task**. The catalog therefore records only what defines and verifies one task; cross-task concerns (the dependency DAG) live in the orchestration layer built on top, not here.

## Partitioning rule

**Every table except the two schema-version sentinels has a `project_id TEXT NOT NULL` column, and every query reads and writes only the opener's `project_id`.** One SQLite file or one Postgres DSN can therefore hold many projects (no schema-per-project).

- `project_id` leads the primary key of every naturally keyed table (`tasks`, `task_versions`, `lifecycles`, `attempts`, `task_claims`, `work_items`, `work_item_dependencies`, `graph_snapshot_items`).
- Surrogate-id ledgers (`events`, `grader_results`, `control_commands`, `source_syncs`, `orchestrator_events`, `orchestrator_stop_events`, `graph_snapshots`, `orchestrator_session_pauses`) keep their `id` primary key; `project_id` leads every unique constraint and hot lookup index on them.
- Foreign keys are composite and carry `project_id`, so a row can only reference rows of its own project.
- In flywheel-core the column is an opaque partition key supplied by whoever opens the store (`SqliteStoreOptions::partition`, `PostgresStoreOptions::partition`; default `default`); core never resolves projects. The shell passes the bound project id (config `project_id` override, else the repo's `.git/config` id); invocations that skip project resolution (`--db` with `--tasks-dir`) use `default`.

## Core tables

| Table              | Key | Purpose |
| ------------------ | --- | ------- |
| `tasks`            | `(project_id, id)` | Logical task identity. The FK anchor every task reference points at. Carries no definition. |
| `task_versions`    | `(project_id, task_id, content_hash)` | Immutable, content-addressed definitions. `content_hash` covers goal + graders + tags + context. FK `(project_id, task_id)` -> `tasks`. |
| `lifecycles`       | `(project_id, run_id)` | The single mutable row per run. `version` drives optimistic concurrency. FK `(project_id, task_id)` -> `tasks`; `task_content_hash` pins the exact `task_versions` row the run ran. |
| `attempts`         | `(project_id, run_id, number)` | Per-execution history under a run with outcome + agent identity (`agent_context_json`). FK -> `lifecycles`. |
| `events`           | `id`; unique `(project_id, run_id, sequence)` | Totally-ordered per-run domain-event ledger; lifecycle state is folded from these rows. `sequence` is per-run, strictly monotonic, allocated inside the append transaction. |
| `grader_results`   | `id`; index `(project_id, run_id, attempt_number, ordinal)` | Append-only receipt log, one row per grader execution. Snapshots the grader spec as it ran. FK -> `attempts`. |
| `control_commands` | `id`; index `(project_id, run_id, id)` where pending | Operator steering queue for one live run. CLI enqueues; the in-run watcher claim-once-applies. **Empty until live operator interaction.** |
| `schema_version`   | `id = 1` | Sentinel `(version, id)`; no `project_id` (it versions the whole database). |

## Orchestrator tables

| Table | Key | Purpose |
| --- | --- | --- |
| `task_claims` | `(project_id, task_id)` | Multi-worker lease (transient coordination). |
| `work_items` / `work_item_dependencies` | `(project_id, task_id)` / `(project_id, task_id, prerequisite_task_id)` | The persisted work graph and its prerequisite edges. |
| `source_syncs`, `graph_snapshots`, `graph_snapshot_items` | `id` / `id` / `(project_id, snapshot_id, task_id)` | Work-source sync runs and point-in-time graph snapshots. |
| `orchestrator_events`, `orchestrator_stop_events`, `orchestrator_session_pauses` | `id` | Claim ledger, stop/strand ledger, rate-limit pauses. |
| `orchestrator_schema_version` | `id = 1` | Sentinel `(version, id)`; no `project_id`. |

## Schema versions and upgrade

- A store is checked before any DDL runs: a brand-new store is created at the current version; **a store behind it is refused with an error naming `fw db upgrade`** (no implicit migration on open); a store ahead of it is refused as written by a newer flywheel.
- `fw db upgrade` migrates in place, one transaction per half, from any supported older version (core v11+, orchestrator v1+): missing columns are added, every table lacking `project_id` is re-keyed with existing rows assigned the store's bound project id, the sentinel is re-created version-first, and the version row is set current. Each step inspects the live layout, so re-running it (or running it on a store already partitioned whose version row is behind) only fixes the version. See [cli.md](cli.md#the-store).

## Notes

- **Every durable task reference is a foreign key.** A run can never point at a task the catalog has never heard of; the identity is auto-registered on the seed path, so the FK holds without forcing a save order.
- **Recovering "the exact task a run executed"** is `lifecycles.task_content_hash` -> `task_versions` (`load_task_for_run`). The harness saves the definition before seeding the run, so the pinned version is always present.
- **`prerequisites` is not persisted by core.** The inter-task dependency DAG is an orchestration-layer concept, kept in the orchestrator tables above.
- **Multi-worker mutual exclusion lives outside core.** `task_claims` belongs to the `flywheel-orchestrator` crate's own store, which shares the database but owns its tables. The single-task loop has no notion of competing workers.
- **Telemetry is not in the DB.** The verbatim agent message stream and `harness.*` telemetry go to per-run append-only JSONL files under `logs/runs/` beside the store (`$FLYWHEEL_HOME/projects/<id>/logs/runs/` by default) via the telemetry sink (see [vision.md](vision.md), "Audit stream"). The relational store holds state only.
- Store contents are sensitive-by-default: payloads are persisted verbatim and unredacted. Treat the file as confidential.
