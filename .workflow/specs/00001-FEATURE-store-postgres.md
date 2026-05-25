# Feature: PostgresStore backend

## Summary

Add `PostgresStore` as a third store implementation alongside `InMemoryStore` and
`SqliteStore`, satisfying every Protocol in `flywheel.store_protocols` against a
real Postgres database. Ships behind an optional `flywheel[postgres]` extra and
uses psycopg v3 with an internal connection pool so multiple workers across one
or more machines can share durable run state.

## Background

The current durable backend (`SqliteStore`) is a single-file, single-writer
store. Once multiple workers or services need to share lifecycle, attempt,
event, and grader-result history across machines, SQLite stops being a fit.
`PostgresStore` covers that gap without compromising the existing protocol
abstraction or in-memory test substrate. Adding it also stresses the store
Protocol surface against a second real engine, surfacing any latent
SQLite-isms.

## Scope

### In Scope

- A new `flywheel.store_postgres` module exporting `PostgresStore`.
- A new canonical schema file `docs/persistence-schema-postgres.sql` (Postgres
  dialect: JSONB, TIMESTAMPTZ, BIGSERIAL, BOOLEAN, FKs on by default).
- Schema bootstrap on construction, idempotent via `CREATE TABLE IF NOT EXISTS`.
- Internal connection pool via `psycopg_pool.ConnectionPool`, sized by
  constructor arguments.
- Optional schema-name argument so multiple flywheel deployments can share one
  database under separate schemas.
- Optimistic concurrency via `WHERE version = :expected` (matching SQLite).
- Append-only enforcement on `grader_results` via `BEFORE UPDATE`/`BEFORE DELETE`
  triggers (matching SQLite).
- Parametrized inclusion in `tests/test_store_contract.py` as a third backend
  alongside memory and sqlite, gated by Docker/testcontainers availability.
- A `tests/test_store_postgres.py` for Postgres-specific properties
  (JSONB round-trip, trigger enforcement at the raw SQL level, FK enforcement,
  pool behavior, schema-name isolation).
- Optional dependency declaration: `flywheel[postgres]` extra including `psycopg`
  and `psycopg_pool`; `testcontainers` added to the dev group.
- Re-export `PostgresStore` from `flywheel.__init__`.

### Out of Scope

- Migration tooling (Alembic, etc.). The schema is single-file, bootstrap-only;
  schema evolution is a later concern.
- Async API surface. PostgresStore is sync to match the existing Protocol shape
  used by SqliteStore and InMemoryStore.
- Removing the SQLite backend. SqliteStore remains the default durable option.
- Roadmap-doc updates. (`docs/roadmap.md` is being deprecated; this feature is
  tracked exclusively under `.workflow/specs/`.)
- Read replicas, sharding, or multi-DB routing.
- Operator CLI for bootstrap/migration.

## Requirements

### Functional Requirements

1. **FR-1: Protocol parity.** `PostgresStore` implements
   `LifecycleStore`, `AttemptStore`, `EventStore`, `GraderResultStore`, and
   `AgentSessionStore` from `flywheel.store_protocols`.
   - Acceptance: existing shared contract tests in
     `tests/test_store_contract.py` pass with `PostgresStore` parametrized as a
     third backend.

2. **FR-2: Optional dependency.** Installing flywheel without the `postgres`
   extra does not pull `psycopg` or `psycopg_pool`. Importing `PostgresStore`
   without the extra raises a clear `ImportError` naming the missing extra.
   - Acceptance: a test asserts the import-time guard message; pyproject.toml
     declares the extra under `[project.optional-dependencies]`.

3. **FR-3: Self-bootstrapping schema.** Constructing a `PostgresStore` against
   an empty database creates every table, index, and trigger defined in
   `docs/persistence-schema-postgres.sql`. Constructing against an already-
   bootstrapped database is a no-op (idempotent).
   - Acceptance: dedicated tests assert table set, trigger set, and that a
     second construction over the same DB does not error and does not duplicate
     objects.

4. **FR-4: Postgres-native types.** `*_json` columns use `JSONB`; timestamp
   columns use `TIMESTAMPTZ` (UTC); autoincrement ids use `BIGSERIAL`;
   `grader_results.passed` is `BOOLEAN`. The store correctly round-trips
   `dataclass <-> DB` for every record type without losing precision or
   timezone.
   - Acceptance: tests assert column types via `information_schema.columns` and
     round-trip values through every store method.

5. **FR-5: Optimistic concurrency.** `update_lifecycle` raises
   `OptimisticConcurrencyError` when `expected_version` does not match the
   stored version, and `LifecycleNotFoundError` when no row exists for
   `run_id`. The error carries `run_id`, `expected_version`, `actual_version`.
   - Acceptance: existing contract tests for these errors pass; a Postgres-
     specific test verifies concurrent conflicting updates via two pool
     connections.

6. **FR-6: Append-only `grader_results`.** Triggers reject any `UPDATE` or
   `DELETE` against `grader_results` with a clear error, even via raw SQL that
   bypasses the protocol surface.
   - Acceptance: a Postgres-specific test issues raw `UPDATE`/`DELETE` and
     asserts the trigger fires.

7. **FR-7: Foreign keys enforced.** `attempts(run_id)` references
   `lifecycles(run_id)`; `events(run_id)` references `lifecycles(run_id)`;
   `grader_results(run_id, attempt_number)` references
   `attempts(run_id, number)`. Inserts that violate FKs fail.
   - Acceptance: a test inserts a child row without its parent and asserts the
     FK violation.

8. **FR-8: Connection pool.** `PostgresStore(dsn, pool_min=N, pool_max=M)`
   constructs an internal `psycopg_pool.ConnectionPool`. Calls acquire and
   return connections to the pool; the store owns the pool's lifecycle.
   `store.close()` closes the pool.
   - Acceptance: tests verify pool sizing, parallel reads from multiple
     connections, and that `close()` releases all pool resources.

9. **FR-9: Schema-name isolation.** `PostgresStore(dsn, schema="flywheel")`
   creates all tables/triggers under the named schema. Two stores against the
   same DB under different schema names do not collide.
   - Acceptance: a test creates two stores under distinct schema names, writes
     a lifecycle to each with the same `run_id`, and asserts both reads
     succeed independently.

10. **FR-10: Public API surface.** `PostgresStore` is importable from
    `flywheel` (re-exported from `__init__`) alongside the other store classes.
    - Acceptance: `from flywheel import PostgresStore` works when the extra is
      installed.

### Non-Functional Requirements

- **Performance**: no explicit benchmark target. The pool must support
  ≥ `pool_max` concurrent attempt/event writes without serialization on a
  single shared lock. JSONB column access should be no slower than the SQLite
  TEXT-blob equivalent under typical contract-test workloads.
- **Security**: connection credentials supplied via DSN are the caller's
  concern; the store does not log the DSN. No SQL is constructed via string
  concatenation of caller input — everything goes through parameterized
  queries.
- **UX**: the import-time error when `psycopg` is missing must name the extra
  to install (e.g., `pip install 'flywheel[postgres]'` or
  `uv add 'flywheel[postgres]'`).

## Behavior Specification

### Happy Path

1. Caller installs flywheel with the extra: `uv add 'flywheel[postgres]'`.
2. Caller constructs `PostgresStore("postgresql://user@host/db", pool_min=1,
   pool_max=10, schema="public")`.
3. The store opens a `psycopg_pool.ConnectionPool`, acquires a connection, runs
   `docs/persistence-schema-postgres.sql` (with the schema name substituted),
   and returns the constructed instance.
4. Caller uses the same methods as `SqliteStore`/`InMemoryStore`. All writes
   commit; all reads observe committed writes.
5. Caller invokes `store.close()` at shutdown; the pool is drained and closed.

### Error Handling

| Error Condition                                         | Expected Behavior                                                                                                                                        |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `psycopg` not installed                                 | `import flywheel.store_postgres` raises `ImportError` naming the `postgres` extra.                                                                       |
| DSN unreachable / auth failure                          | Constructor raises the underlying `psycopg.OperationalError` unchanged; the message is not swallowed.                                                    |
| `create_lifecycle` for an existing `run_id`             | Raises `LifecycleAlreadyExistsError(run_id)`.                                                                                                            |
| `update_lifecycle` with mismatched `expected_version`   | Raises `OptimisticConcurrencyError(run_id, expected_version, actual_version)`.                                                                           |
| `update_lifecycle` for an unknown `run_id`              | Raises `LifecycleNotFoundError(run_id)`.                                                                                                                 |
| FK violation (attempt without parent lifecycle)         | Raises the underlying `psycopg.errors.ForeignKeyViolation`.                                                                                              |
| Raw `UPDATE`/`DELETE` on `grader_results`               | Trigger raises a Postgres exception; bubbles up unchanged.                                                                                               |
| Pool exhaustion                                         | `psycopg_pool.PoolTimeout` bubbles up; the store does not silently retry.                                                                                |
| Schema bootstrap on a DB where the role lacks DDL perms | The underlying `psycopg.errors.InsufficientPrivilege` bubbles up at construction; documented in the docstring so operators set up roles ahead of time.   |

### Edge Cases

| Case                                                       | Expected Behavior                                                                                                                                              |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Reopening an already-bootstrapped database                 | No DDL errors; all tables/triggers/indexes already exist (idempotent via `IF NOT EXISTS`).                                                                     |
| Two `PostgresStore` instances against the same DB+schema   | Both work; concurrency on lifecycles is mediated by the version column.                                                                                        |
| Two `PostgresStore` instances against the same DB, different schemas | Fully isolated table sets; no cross-talk.                                                                                                            |
| Concurrent updates to the same lifecycle from two pool connections | One succeeds, the other raises `OptimisticConcurrencyError`.                                                                                         |
| Reading an attempt/event/grader-result whose parent row is missing | Cannot occur: FKs prevent the orphaned insert in the first place.                                                                                    |
| Timezone-naive `datetime` written via dataclass field      | Stored as UTC `TIMESTAMPTZ`; read back as timezone-aware UTC. Caller responsibility to pass aware datetimes (matches existing SqliteStore practice).           |
| `claude_session_store` rows for a non-existent lifecycle   | Allowed: matches SQLite schema, which has no FK from `claude_session_store` to `lifecycles`.                                                                   |
| Empty `:memory:`-style ephemeral DB requested              | Not supported on Postgres; documented. Use `InMemoryStore` for ephemeral testing.                                                                              |
| Docker not available in the test environment               | All Postgres-backed tests skip with a clear reason; memory + sqlite contract suite still passes.                                                               |
| Pool closed mid-operation                                  | Pending operation fails with `psycopg_pool` error; new operations after `close()` raise.                                                                       |

## Technical Context

### Affected Apps

- `flywheel` (root and only package): new module + schema file + tests + extra.

### Integration Points

- **psycopg v3 + psycopg_pool**: synchronous driver and connection pool.
- **testcontainers-python**: dev-only dependency providing a Postgres container
  for the contract suite.
- **Existing `flywheel.store_protocols`**: PostgresStore must satisfy every
  Protocol unchanged.
- **Existing `tests/test_store_contract.py`**: extend its `_STORE_FACTORIES`
  map with a `postgres` entry whose factory yields a `PostgresStore` against a
  per-test schema in a session-scoped container.

### Relevant Existing Code

- `src/flywheel/store_protocols.py`: target Protocol surface.
- `src/flywheel/store_sqlite.py`: structural template (init, bootstrap,
  optimistic-concurrency pattern, trigger setup, row-to-dataclass converters).
- `src/flywheel/store_memory.py`: shape-of-API reference for defensive copies.
- `docs/persistence-schema.sql`: source-of-truth column list and table
  semantics; the Postgres file is its dialect-translated sibling.
- `tests/test_store_contract.py`: parametrized contract suite.
- `tests/test_store_sqlite.py`: pattern for backend-specific tests (trigger,
  schema bootstrap, FK enforcement).
- `src/flywheel/__init__.py`: where the public re-export goes.

## Decisions Log

| Decision                                        | Choice                                                                                       | Rationale                                                                                                              |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Motivation                                      | Shared/remote durable store                                                                  | Multiple machines/services need to read/write the same flywheel run history.                                           |
| Dependency packaging                            | Optional extra `flywheel[postgres]`                                                          | SQLite users should not pay the psycopg install cost; PostgresStore is a real shipped backend, not dev-only.           |
| Driver                                          | psycopg v3 (sync)                                                                            | Modern, maintained, sync API matches the existing SqliteStore Protocol shape; first-party pool via psycopg_pool.       |
| Schema definition                               | New canonical `docs/persistence-schema-postgres.sql`                                         | Hand-authored Postgres dialect mirrors the SQLite file structure; avoids fragile runtime translation.                  |
| Connection model                                | DSN + internal `psycopg_pool.ConnectionPool`                                                 | Multiple workers concurrent; the store owns its pool for predictability.                                               |
| Optimistic concurrency                          | `WHERE version = :expected` (mirror SQLite)                                                  | Preserves the Lifecycle.version semantics already locked into the Protocol contract.                                   |
| `grader_results` append-only                    | `BEFORE UPDATE`/`BEFORE DELETE` triggers                                                     | Matches SqliteStore enforcement; defends even against callers who bypass the protocol surface.                         |
| Type mapping                                    | Postgres-native: JSONB, TIMESTAMPTZ, BIGSERIAL, BOOLEAN                                      | Idiomatic Postgres, queryable for operators, no loss vs SQLite TEXT-stored values.                                     |
| Test infrastructure                             | `testcontainers-python`, skip when Docker absent                                             | Real Postgres in the loop without forcing every contributor to manage a service container.                             |
| Contract test placement                         | Parametrize as third backend in existing `tests/test_store_contract.py`                      | Directly asserts protocol parity across memory + sqlite + postgres.                                                    |
| Public API                                      | `PostgresStore(dsn, *, pool_min=1, pool_max=10, schema='public')`                            | Minimum constructor signature that exposes the deployment knobs (pool sizing, multi-tenant schema).                    |
| Bootstrap policy                                | Always bootstrap, idempotent via `IF NOT EXISTS`                                             | Mirrors SqliteStore exactly; one mental model across backends.                                                         |
| Tracking                                        | Spec-only under `.workflow/specs/`; no `docs/roadmap.md` entry                               | Roadmap doc is being deprecated; new work goes through specs.                                                          |

## Open Questions

- Roadmap deprecation cleanup (CLAUDE.md references, removal of
  `docs/roadmap.md` and `tasks/roadmap-NN/` directories) is a separate
  initiative outside this spec.
- Schema-name substitution mechanics: whether the `docs/persistence-schema-
  postgres.sql` file uses a `${schema}` placeholder rewritten at bootstrap, or
  the store issues `SET search_path` and the file uses unqualified names.
  Implementer's call during build.

## Next Steps

Run `/task 00001-FEATURE-store-postgres` to generate implementation task
definitions from this spec.
