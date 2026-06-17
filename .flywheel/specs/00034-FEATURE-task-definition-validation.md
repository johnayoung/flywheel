# Feature: Task-definition validation (grader lint)

## Outcome
A task whose definition is statically broken — a command grader that cannot parse as a shell command, or that references a repo-relative filesystem path that does not exist — is reported as invalid before any worker runs it, naming the task and the specific defect, and is not dispatched. A `flywheel validate` verb makes the same check runnable on demand and exits non-zero when any active task is invalid.

## Background
In a real batch, two graders were impossible the moment they were authored and nobody knew until a worker burned a cycle on them: one ran `uv build --package A --package B` (a flag the tool rejects), and one ran `uv run pytest .flywheel/holdout/00029-distributable-install/` against a directory that did not exist (exit 4, "file or directory not found"). The grader is the spine of a task; an un-runnable grader is a silent hole in the verification surface that masquerades as "not done yet." The tacit expectation: "if I wrote a grader, the harness can at least *run* it." This spec makes that checkable at define/schedule time. It deliberately does NOT promise to catch a grader that parses and runs but is semantically wrong for its tool (the `--package` case) — that is undecidable without a trustworthy oracle and is named out of scope.

## Scope
### In scope
- A static validation pass over a task's graders that flags, without executing them: (a) a `command` grader whose `run` is empty or does not parse as a shell command; (b) a `command` grader whose `run` references a repo-relative filesystem path that does not exist on disk.
- A `flywheel validate` (and `fw validate`) verb that runs the pass over every active task and exits non-zero, naming each invalid task and its defect(s), when any task is invalid; exit 0 when all are valid.
- Schedule-time enforcement: the orchestrator does not dispatch a task that fails static validation; it surfaces the defect (the same report) instead of running its agent.

### Out of scope
- Semantic correctness of a grader command that parses and runs (wrong tool flags, wrong assertion logic, a command that exits 2 on a usage error) — undecidable statically; explicitly NOT promised (the `uv build --package A --package B` class).
- Executing graders ("dry-run") to judge them — a grader is expected to fail against the base before the feature exists, so a run cannot distinguish "correctly failing" from "broken." No execution.
- Validating `transcript`/`rubric`/`manual` graders' content beyond their existing schema rules (only `command` graders gain the new shell/path checks).
- Re-deriving the `Task`/`Grader` schema rules already enforced at construction/load (those stay; this adds checks on top).

### Must not regress
- A task with only valid graders validates clean and dispatches/runs exactly as today.
- The existing `Task`/`Grader` schema validation (unknown grader type, missing required field) still rejects malformed definitions at construction/load.
- `flywheel_core.task` stays pure (no filesystem/JSON/IO); any path-existence check lives in a loader/validation module that may touch the filesystem, never in the pure task module.
- The full existing suite still passes.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type, visibility, the exact check, and the gaming move it forecloses.

1. When a `command` grader's `run` references a repo-relative path token that does not exist on disk, validation shall report the owning task invalid, naming the task id and the missing path. [command | held-out]
   verify: held-out pytest — construct a task with a `command` grader `run` = `uv run pytest .flywheel/holdout/does-not-exist/ -q`; call the validation pass with a repo_root under which that path is absent; assert the result is non-empty, marks that task invalid, and the defect text contains the task id and the missing path. A path token is a `run` argument that contains `/`, does not start with `-`, has no `://`, and (with a trailing `/` and any trailing `/*` glob segment stripped) names a location under repo_root.
   defends against: returning "valid" for everything (the report is empty); a check that only inspects the grader `type`/shape and never looks at the `run` body.

2. When a `command` grader's `run` is empty or does not parse as a shell command, validation shall report the owning task invalid. [command | held-out]
   verify: held-out pytest — a grader `run` of `"for f in"` (an unterminated shell construct, `bash -n` exit non-zero) and a grader `run` of `""` each cause the task to be reported invalid with a parse/empty defect; a well-formed multi-token `run` does not.
   defends against: treating any non-empty string as valid; a substring/regex check that a crafted-but-unparseable command slips past.

3. When every grader in a task is valid (parseable commands, all referenced paths exist), validation shall report that task valid and the orchestrator shall dispatch it normally. [command | held-out]
   verify: held-out pytest — a task whose `command` graders parse and whose referenced paths all exist validates with an empty defect list; and the schedule path that consults validation dispatches it (does not skip it). Chosen `run` strings reference paths that DO exist so a blanket "flag everything" implementation fails.
   defends against: a validator that reports every task invalid (or every task valid) regardless of content — the mixed valid/invalid corpus separates the two.

4. When `flywheel validate` runs over an active task set containing at least one statically-invalid task, the verb shall exit non-zero and name each invalid task; when all tasks are valid it shall exit 0. [command | visible]
   verify: pytest driving the CLI verb over a temp tasks tree — one invalid + one valid task yields a non-zero exit whose output names the invalid task; an all-valid tree yields exit 0.
   defends against: a verb that always exits 0 (prints warnings but never fails) — the non-zero-on-invalid assertion catches it.

### Verification-surface (Definition of Done)
5. The existing suite still runs and still passes after the change, with no fewer tests collected in the touched packages than before, and the `flywheel_core.task` purity guard still passes. [command | held-out] (verification-surface)
   verify: `uv run pytest` exits 0; `uv run pytest packages/flywheel-core/tests/test_task_module_purity.py` exits 0; collected count in touched packages is >= the pre-change baseline.
   defends against: making validation pass by weakening the schema/purity tests, or by dragging filesystem IO into the pure task module.

Verification surface: the existing suite still passes and `flywheel_core.task` stays pure (criterion 5); new behavior is proven by held-out pytest (criteria 1-3) that feeds the validator crafted task definitions whose valid/invalid split the agent cannot precompute from a single known input.

## Decomposition Hint (for /fw-plan)
Splits along two layers; chain with a prerequisite.
- Validation layer (a new `validate_task(task, *, repo_root) -> list[TaskDefect]` in the loader/validation module — NOT in pure `flywheel_core.task`; uses `bash -n` for shell-parse and filesystem existence for path tokens): satisfies #1, #2, #3-validator-half. Pure-task purity invariant: the path/shell checks live where filesystem IO is allowed.
- Surface layer (the `flywheel validate`/`fw validate` verb + the schedule-time dispatch consult so an invalid task is not run): satisfies #3-dispatch-half, #4; depends on the validation layer.
Shared invariant: the defect record shape (`task_id` + human-readable `detail`) returned by `validate_task` — the verb and the orchestrator both render it; name it so both consume one shape.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Static validation only — never execute graders to judge them  (Status: Accepted)
- Context: the strongest catch would be "run the grader and see if it works," but a grader is expected to FAIL against the base before its feature exists, so a run cannot distinguish "correctly failing" from "broken," and a usage-error exit (2) is indistinguishable from a real failure.
- Decision: validate statically — schema (existing), shell-parseability (`bash -n`), and path-existence of repo-relative tokens. Do not execute graders.
- Rejected: dry-running graders against the base (false signal, as above); a tool-aware semantic checker (open-ended, per-tool, brittle). Consequences: a grader that parses and runs but is wrong for its tool (the `uv build --package A --package B` case) is NOT caught — named explicitly out of scope so the limitation is honest, not a silent gap.

### D-2: Path-token heuristic is conservative and explicit  (Status: Accepted)
- Context: extracting "the paths a shell command references" is undecidable in general; over-eager flagging would false-positive on flags and literals.
- Decision: a token is a path reference only if it contains `/`, does not start with `-`, contains no `://`, and resolves to an intended repo-relative location (trailing `/` and a trailing `/*` glob stripped). Only such tokens are existence-checked. URLs, flags, and bare words are never flagged.
- Rejected: full shell-AST path extraction (over-engineered, brittle); flagging any token with a dot (false-positives on `module.submodule`). Consequences: a path passed through a variable or constructed at runtime is not checked — acceptable; the common authored-literal case (the 00029 defect) is caught.

### D-3: Refuse-to-dispatch reuses the existing skip-and-report path  (Status: Accepted)
- Context: an invalid task must not run, but one bad task must not halt the fleet.
- Decision: schedule-time enforcement skips the invalid task and surfaces its defect, mirroring how an unprovisionable task is skipped today; the `validate` verb is the operator's on-demand form of the same check.
- Rejected: hard-failing the whole worker on one invalid task (starves peers). Consequences: an invalid task simply never runs until fixed; `flywheel validate` is how an operator finds it before launching.

## Open Questions (accepted gaps)
None. Criteria 1-4 lower to `command` graders. The semantic-tool-error class is not an un-gradeable criterion — it is explicitly out of scope (D-1), not a dropped requirement.

## Next Steps
Run `/fw-plan 00034-FEATURE-task-definition-validation` to compile these criteria into flywheel tasks and graders.
