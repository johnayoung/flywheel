# Roadmap

Path to parity with the prior Go implementation. Each task says what, not how — implementation is the agent's call. Order is dependency-driven; each item can be done with only prior items complete.

1. **Task schema + loaders.** _Done._ `Task` dataclass with `validate()`; `load_task_file` / `load_task_directory` / `load_tasks_jsonl` exported from `flywheel`. Spec: [task-schema.md](task-schema.md).
2. **Lifecycle state machine.** _Done._ `Lifecycle`, `Attempt`, status enum, transition rules, retry eligibility, consecutive-failed-runs counter. Spec: [task-lifecycle.md](task-lifecycle.md).
3. **Store interfaces + in-memory impl.** _Done._ Persistence contracts for lifecycle / attempt / event / grader-result / agent-session, with an in-memory implementation for tests.
4. **SQLite store.** Same contracts, backed by [persistence-schema.sql](persistence-schema.sql). WAL, foreign keys on, optimistic concurrency on `version`.
5. **Iteration envelope parser.** Extract and validate `<!-- LOOP_STATUS -->` JSON; treat malformed / missing / duplicate / truncated as first-class outcomes. Spec: [loop.md](loop.md).
6. **Claude invoker.** Thin adapter over `claude-agent-sdk` that drives one iteration and returns transcript, envelope, and SDK signals.
7. **Prompt builder.** Per-iteration prompt from task + lifecycle state + envelope contract.
8. **Command grader runner.** Run `command` graders in list order; persist each execution to `grader_results` with a `grader_spec_json` snapshot and a structured `payload_json` per the schema contract.
9. **Transcript grader enforcement.** Apply `transcript` graders (`max_turns`, `max_total_tokens`, `max_wall_seconds`) as hard limits during the run and as graders at validation time; persist to `grader_results` with observed-vs-breached payload.
10. **Harness.** Wire it together: invoke → envelope → intent → run graders on-completed → record attempt (with `agent_context`) → apply retry policy → transition. Owns all lifecycle transitions and per-attempt artifact dirs. Spec: [loop.md](loop.md).
11. **Hello example.** End-to-end smoke: one trivial task, SQLite store, run harness, stream events to stdout.
12. **Strategy stub.** `Protocol` from [strategy.md](strategy.md) with a no-op default; concrete branch/PR impls deferred.
