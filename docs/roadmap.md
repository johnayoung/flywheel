# Roadmap

Path to parity with the prior Go implementation. Each task says what, not how — implementation is the agent's call. Order is dependency-driven; each item can be done with only prior items complete.

1. **Task schema + loaders.** `Task` dataclass with `validate()`; loaders for single JSON file, directory of JSONs, and JSONL. Spec: [task-schema.md](task-schema.md).
2. **Lifecycle state machine.** `Lifecycle`, `Attempt`, status enum, transition rules, retry eligibility, consecutive-failed-runs counter. Spec: [task-lifecycle.md](task-lifecycle.md).
3. **Store interfaces + in-memory impl.** Persistence contracts for lifecycle / attempt / event / verification / agent-session, with an in-memory implementation for tests.
4. **SQLite store.** Same contracts, backed by [persistence-schema.sql](persistence-schema.sql). WAL, foreign keys on, optimistic concurrency on `version`.
5. **Iteration envelope parser.** Extract and validate `<!-- LOOP_STATUS -->` JSON; treat malformed / missing / duplicate / truncated as first-class outcomes. Spec: [loop.md](loop.md).
6. **Claude invoker.** Thin adapter over `claude-agent-sdk` that drives one iteration and returns transcript, envelope, and SDK signals.
7. **Prompt builder.** Per-iteration prompt from task + lifecycle state + envelope contract.
8. **Tier-1 verifier.** Run `acceptance_criteria.commands` in order; capture exit/stdout/stderr/duration; persist as verification rows.
9. **Harness.** Wire it together: invoke → envelope → intent → verify-on-completed → record attempt → apply retry policy → transition. Owns all lifecycle transitions and per-attempt artifact dirs. Spec: [loop.md](loop.md).
10. **Hello example.** End-to-end smoke: one trivial task, SQLite store, run harness, stream events to stdout.
11. **Strategy stub.** `Protocol` from [strategy.md](strategy.md) with a no-op default; concrete branch/PR impls deferred.
