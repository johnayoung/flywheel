# Flywheel Task Decomposition Prompt

Paste this into Claude Code from inside the target repo. Replace `<SPEC_FILE>` with your spec's filename.

---

I need you to decompose the specification in `docs/<SPEC_FILE>.md` into a set of Flywheel task definitions that will be executed by parallel AI coding agents. Read the spec thoroughly first, then produce JSON task files in `.flywheel/tasks/`, one file per task.

**Naming:**

- `id` must be domain-specific and self-describing. Prefer `mcf-core-types` over `module-scaffold`, `http-handler-logging` over `add-logging`. If someone reads the ID in isolation six months from now, they should know what it built.
- Filename: `<wave><slot>-<id>.json` where `wave` is the two-digit execution wave (`01`, `02`, ...) and `slot` is a letter disambiguating parallel tasks in the same wave (`a`, `b`, `c`, ...). Example: `02a-mcf-public-api.json`. This keeps alphabetical directory listing aligned with execution order.

**Task schema** (see https://github.com/johnayoung/flywheel for full reference):

```json
{
  "id": "kebab-case-id",
  "description": "One sentence: what this task accomplishes",
  "category": "feat|fix|refactor|test|docs|chore",
  "priority": 1,
  "prerequisites": ["other-task-id"],
  "commit": "feat: conventional commit message",
  "steps": ["concrete step 1", "concrete step 2"],
  "acceptance_criteria": ["verifiable condition", "go build ./... succeeds", "go test ./... passes"]
}
```

**Decomposition rules:**

1. **Maximize parallelism.** Each agent runs in an isolated git worktree branched from `main`. Tasks with no `prerequisites` run simultaneously. Only add a prerequisite when task B *genuinely cannot start* without task A's merged code (e.g., B imports a type A defines). Do NOT chain tasks linearly "for safety" -- that defeats the entire purpose of Flywheel.

2. **Leaf tasks first.** Foundational work (data models, interfaces, package scaffolding, config structs) should have empty `prerequisites` so they fan out in wave 1. Integration and wiring tasks depend on the leaves.

3. **Right-sized tasks -- hard caps.** A task is too big if *any* of these are true:
   - More than ~8 numbered `steps`.
   - Expected diff exceeds ~200 lines of implementation (tests excluded).
   - More than one non-trivial algorithmic concern (e.g., "implement pricing AND pivot AND tree updates" = three tasks).
   - Acceptance criteria cannot be verified without the entire deliverable existing.

   When any cap trips, **split the task into vertical slices** that each compile, test, and merge independently. For a multi-phase algorithm, slice by phase (initialization, phase 1, phase 2, finalization) -- each phase exposes an internal API the next phase consumes, and each ships with its own test proving the phase's invariant. Do not slice by "write the function, then write the test in a later task" -- that eliminates the feedback loop.

4. **Every task is self-testing.** The task is not complete until a test in the same commit demonstrates the behavior the task claims to deliver. `steps` must include writing the test; `acceptance_criteria` must include that test passing. "A later task will test this" is never acceptable -- it defers failure to merge time, where debugging costs 10x more. For pure-data tasks (types, config structs), a compile-time assertion or a trivial round-trip test is sufficient.

5. **No file-level conflicts between parallel tasks.** If two tasks without a prerequisite relationship would both edit the same file, either (a) add a prerequisite, or (b) split the file's changes differently. Flywheel resolves conflicts but avoiding them is cheaper.

6. **Self-contained `steps`.** The agent sees only the task JSON and the repo -- not the spec. Each step must be concrete and actionable without external context. Reference specific file paths, function names, and types. If a design decision from the spec matters, restate it inline.

7. **Verifiable `acceptance_criteria`.** Every criterion must be checkable by running a command or reading the diff. Always include `go build ./...` and, where relevant, `go test ./...`. Avoid vague criteria like "code is clean" or "works correctly."

8. **Conventional commits.** The `commit` field must start with `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, or `chore:` and match the `category`.

**Before writing files, output:**

- A dependency graph (ASCII or bulleted tree) showing the waves of parallel execution.
- A short justification for any prerequisite edge -- why B cannot start before A merges.

Then stop and wait for me to approve the plan before generating the JSON files. If the spec is ambiguous on any point that affects task boundaries, ask me rather than guessing.
