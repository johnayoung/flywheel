# Feature: Distributable install path (run flywheel from outside this checkout)

## Outcome
A stranger can take built artifacts of this workspace (wheels and/or an sdist),
install `flywheel` into a Python environment that is NOT this checkout and has no
access to the workspace source tree, and the `flywheel` / `fw` console scripts run
against an arbitrary cwd git repo — scaffolding `.flywheel/` and reaching first
store-open without ImportError, missing-distribution, or missing-package-data
failures. The only supported "run flywheel" path today is `uv run flywheel ...`
from inside this clone; after this feature an installed-from-build invocation works.

## Background
Distribution is the one thing blocking adoption that the *design* already accounts
for: path resolution is cwd/git-toplevel relative (`_repo_root` =
`git rev-parse --show-toplevel`; `INIT_ROOT = Path(".flywheel")`;
`flywheel.toml` read from the working dir), so the binary is meant to be pointed at
a foreign repo. What blocks it is purely packaging: the four packages declare
inter-package dependencies with NO version constraint, resolvable only through the
root `[tool.uv.sources] { workspace = true }` table — and that table is a uv
workspace-local convenience that is NOT emitted into a built wheel's METADATA. A
wheel of `flywheel` therefore carries `Requires-Dist: flywheel-orchestrator` with
no version and no findable source, so a resolver in a foreign environment cannot
complete the install graph from the artifacts alone.

The tacit, load-bearing trap a literal agent will miss: flywheel ships **non-`.py`
package data that is loaded at runtime via `importlib.resources.files(...)`** —
`flywheel_core/_schema/persistence-schema.sql` and `-postgres.sql` (read the
instant any store opens) and `flywheel_orchestrator/_skill_templates/*.md` (read by
`init`). A wheel can "build successfully" and even import cleanly while silently
omitting these data files; the failure only surfaces when the installed binary
actually opens a store or scaffolds skills against a foreign repo. "It built" and
"it imports" are both gameable proxies for "it runs end-to-end from the artifact."
The authoritative grade must drive an installed binary against a real cwd repo, in
an environment with no workspace source on the path.

## Scope
### In scope
- Inter-package version constraints in `packages/*/pyproject.toml` such that the
  install graph resolves from built artifact metadata alone, without the workspace
  source table.
- Built artifacts (wheels and/or sdist for every workspace package an install of
  `flywheel` transitively needs) that install into a foreign environment.
- The runtime package-data files (`_schema/*.sql`, `_skill_templates/*.md`,
  `py.typed`) being present and loadable inside the installed distributions.
- The `flywheel` and `fw` console scripts being created by the install and
  executable from a working directory that is not this checkout.
- A build/publish helper or documented build invocation that produces the
  installable artifact set.

### Out of scope
- Publishing to PyPI or any index (spec 00021 parks this; an internal
  `uv tool` / local-wheel / sdist path satisfies this spec).
- Behavioral correctness of `worker`, landing, base-branch, auth, or sandbox setup
  (B2/B3/B4/B5, M-series) — those are separate specs; this one only proves the
  install + reach-first-store-open path works from a built artifact.
- Plugin / entry-point discovery (M3), Postgres read-path (M1).
- Any change to cwd/git-toplevel path-resolution logic — it already targets the cwd
  repo and must stay that way.

### Must not regress
- `uv sync` from inside this checkout still installs all four workspace packages and
  `uv run flywheel ...` still works (the dev workflow in CLAUDE.md/README).
- `import flywheel_core` still succeeds with NO agent SDK installed (the SDK stays
  an optional extra `flywheel-core[claude]`; nothing here makes `claude-agent-sdk`
  a hard dependency of core).
- The one-way dependency arrow is unchanged: `flywheel-core` declares no dependency
  on any downstream package.
- `uv run pytest` (the four package test dirs) still collects and passes.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When `flywheel` is installed from built artifacts into a Python environment that
   contains NO workspace source and NO `[tool.uv.sources]` table, the install
   completes and creates the console scripts. [command | held-out]
   verify (held-out, run outside the checkout): build the artifact set, then in a
   throwaway venv created OUTSIDE this repo install only the built `flywheel`
   artifact with `--no-index --find-links <dist-dir>` (or `uv pip install` against
   the dist dir with no source/workspace on the path), and assert exit 0 AND that
   `command -v flywheel` and `command -v fw` both resolve to that venv's bin.
   defends against: relying on the workspace source table to resolve deps — a bare
   `Requires-Dist: flywheel-orchestrator` with no version and `--no-index` makes the
   resolver fail unless real version constraints + artifacts exist; forecloses
   "passes only because uv sync re-read the workspace."

2. When the installed `flywheel` binary is run from an arbitrary git repo that is
   NOT this checkout, it scaffolds the `.flywheel/` layout in that cwd repo without
   ImportError or missing-distribution error. [command | held-out]
   verify (held-out): in the foreign venv from #1, `cd` into a fresh `git init`
   throwaway repo (outside this checkout), run the installed `flywheel init`
   non-interactively, assert exit 0 AND that `.flywheel/` and `flywheel.toml` now
   exist in that throwaway repo's cwd (`test -d .flywheel && test -f flywheel.toml`).
   defends against: a binary that only works with this repo on `sys.path`, or
   path-resolution that silently anchors to the install location instead of cwd;
   running it from a foreign cwd forces real cwd-relative resolution.

3. When `init` runs from the installed binary, every skill template it materializes
   is sourced from package data bundled in the installed distribution. [command | held-out]
   verify (held-out): after #2's `init` (run with skills enabled), assert exit 0 AND
   that at least one expected `SKILL.md` was written under the throwaway repo's
   `.claude/skills/` (e.g. `test -f .claude/skills/fw-spec/SKILL.md`), proving
   `importlib.resources.files("flywheel_orchestrator") / "_skill_templates"` resolved
   inside the installed wheel.
   defends against: a wheel built with the `.md` template data files omitted — they
   are non-`.py` and silently dropped by a naive build, "builds + imports" green
   while `init` throws `FileNotFoundError`/`KeyError` at template read.

4. When a SQLite store is opened by the installed binary against the foreign cwd
   repo, the bundled schema SQL is read and the store bootstraps. [command | held-out]
   verify (held-out): in the foreign venv, against the throwaway repo, drive an
   installed-binary path that opens the default `.flywheel/flywheel.sqlite` store
   (e.g. `flywheel status` / `flywheel history` against the scaffolded repo) and
   assert exit 0 AND that `.flywheel/flywheel.sqlite` exists and is a valid SQLite
   DB whose schema includes the expected tables (e.g.
   `python -c "import sqlite3,sys; ...; assert {'lifecycles' present}"` — name the
   real table set at plan time from persistence-schema.sql).
   defends against: a wheel that omitted `_schema/persistence-schema.sql`; the loader
   does `files("flywheel_core") / "_schema" / "persistence-schema.sql"` at store-open
   and a missing data file throws only when a store is actually opened — not at
   import.

5. If a built `flywheel` wheel's dependency metadata is inspected, then every
   inter-flywheel `Requires-Dist` carries a non-empty version constraint (not a bare
   unversioned name). [command | held-out]
   verify (held-out): build the wheel, unzip its `*.dist-info/METADATA`, and assert
   that each `Requires-Dist:` line naming `flywheel-core` / `flywheel-orchestrator` /
   `flywheel-worktree` includes a version specifier (e.g. grep that those lines match
   a `[<>=~!]=?` constraint and fail if any such line is just the bare name).
   defends against: "deps resolve in CI because the workspace is present" — pins the
   end-state to the artifact metadata itself, foreclosing reintroduction of the
   workspace-only resolution that B1 names.

6. When `import flywheel_core` runs in an environment WITHOUT `claude-agent-sdk`
   installed, it succeeds. [command | held-out]
   verify (held-out): in a venv that installed `flywheel-core` WITHOUT the `[claude]`
   extra (and with no `claude_agent_sdk` importable), run
   `python -c "import flywheel_core"` and assert exit 0.
   defends against: making the SDK a hard dependency to "simplify" packaging —
   directly pins the optional-extra invariant from CLAUDE.md.

7. The `flywheel-core` distribution declares no dependency on any downstream
   flywheel package. [command | held-out]
   verify (held-out): build `flywheel-core`, read its METADATA, assert NO
   `Requires-Dist` names `flywheel-orchestrator`, `flywheel-worktree`, or `flywheel`.
   defends against: a cyclic/back-pointing dependency edge sneaking in while wiring
   version constraints, violating the one-way arrow.

8. When `uv sync` is run from inside this checkout after the change, all four
   workspace packages install and `uv run flywheel --help` exits 0.
   [command | visible]
   verify: from the repo root, `uv sync` exit 0, then `uv run flywheel --help`
   exit 0.
   defends against: satisfying the foreign-install path by breaking the in-repo dev
   workflow (e.g. pinning exact versions that the workspace source table then can't
   satisfy on a dirty version bump).

Verification surface: this feature changes the build/packaging machinery (the
artifact + dependency-metadata path that `/fw-plan` will turn into the install
grader, and that every other consumer install inherits). The inherited
Definition-of-Done below applies.

9. (verification-surface) The existing verification suite still passes after the
   change. [command | held-out]
   verify (held-out): from the repo root after the change, `uv run pytest` over the
   four package test dirs exits 0 with collected > 0.
   defends against: making the build/metadata change pass by deleting or weakening
   tests that constrained packaging; a green build with zero collected tests.

10. (verification-surface) If any existing purity/packaging test is relaxed,
    removed, or skipped, then a named replacement check of equal-or-greater strength
    exists. [manual | held-out]
    verify (held-out): operator diffs the test changes; for every removed/relaxed
    assertion (notably `test_task_module_purity.py` /
    `test_lifecycle_module_purity.py` and any test asserting the dependency arrow),
    a named replacement of equal-or-greater strength is present, else reject.
    defends against: dropping the core-purity or one-way-arrow guard under cover of a
    packaging refactor with no equal replacement — a blocking defect.

## Decomposition Hint (for /fw-plan)
Splits along packaging layers; chain with prerequisites bottom-up so no slice
inherits an unbuildable workspace.

- Layer A: inter-package version constraints in `packages/*/pyproject.toml` — add
  real version specifiers to the inter-flywheel `Requires-Dist` edges (keeping the
  workspace source table working in-repo) and keep core dependency-free downstream.
  Satisfies #5, #7; supports #1.
- Layer B: build artifacts + package-data inclusion — produce wheels/sdist for the
  packages an install of `flywheel` needs, ensuring `_schema/*.sql`,
  `_skill_templates/*.md`, and `py.typed` ship inside them. Satisfies #3, #4;
  depends on Layer A.
- Layer C: foreign-environment install + run — a build/publish helper or documented
  build invocation, plus the held-out install-and-run-against-a-foreign-cwd checks.
  Satisfies #1, #2; depends on Layers A and B.
- Layer D: regression + invariant guard — in-repo `uv sync` still works, core
  imports without the SDK, suite still green. Satisfies #6, #8, #9, #10; depends on
  all above.

Shared invariants multiple layers assert against:
- The four package versions are currently all `0.1.0`; the version-constraint scheme
  in Layer A and the artifacts in Layer B must agree on the SAME constraint form so
  the foreign install graph resolves. Name the exact constraint style (e.g.
  `==0.1.0` vs `>=0.1.0`) once, in Layer A, and have B/C inherit it.
- The set of runtime package-data globs (`_schema/*.sql`, `_skill_templates/*.md`,
  `py.typed`) is the same set asserted present in #3/#4 and configured in Layer B —
  enumerate it once so a file added later is not silently left out of the wheel.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Internal/local-artifact distribution satisfies this spec; no public index publish  (Status: Accepted)
- Context: spec 00021 explicitly parks publishing; the audit (B1, sequencing item 2)
  asks only that "a stranger can install it," naming "even an internal `uv tool`/wheel
  path." | Decision: the observable end-state is install-from-built-artifact into a
  foreign environment (wheels/sdist via `--find-links`/local dir), NOT upload to
  PyPI. Criteria #1-#4 grade a local-artifact install with `--no-index`.
- Rejected: requiring a PyPI publish — out of scope per 00021 and gold-plating the
  audit did not ask for; requiring `uv tool install` specifically — that is one
  acceptable mechanism, but pinning the mechanism would grade the path, so criteria
  grade the artifact + the run, not the install verb.
- Consequences: the grade depends on a reproducible local build step; the foreign
  install must use `--no-index` (or an equivalent no-workspace path) or criterion #1
  is gameable by silently re-reading the workspace.

### D-2: The authoritative grade drives an installed binary against a foreign cwd repo, not "build/import succeeds"  (Status: Accepted)
- Context: the load-bearing failure mode is non-`.py` package data
  (`_schema/*.sql`, `_skill_templates/*.md`) loaded via `importlib.resources` at
  store-open / init time; a wheel can build and import while omitting them. | Decision:
  the highest-stakes criteria (#2, #3, #4) are held-out checks that run the installed
  console script from a throwaway git repo OUTSIDE this checkout and assert real
  filesystem end-states (`.flywheel/`, a `SKILL.md`, a bootstrapped SQLite schema).
- Rejected: grading on `uv build` exit 0 (proves nothing about data files or foreign
  resolution); grading on `python -c "import flywheel"` (import does not read the data
  files); a rubric over the pyproject diff (machine-decidable, so rubric is a defect).
- Consequences: the grader needs an ephemeral venv + throwaway git repo created
  outside the workspace; more setup, but it is the only check that cannot be faked by
  a workspace-present environment.

### D-3: Inter-package version constraints must resolve from artifact metadata alone  (Status: Accepted)
- Context: B1 — deps are unversioned, resolvable only via the workspace source table,
  which is not in wheel METADATA. | Decision: every inter-flywheel `Requires-Dist`
  gets a real version constraint (criterion #5 asserts it in built METADATA), while
  the in-repo `[tool.uv.sources] workspace=true` table stays so `uv sync` keeps
  working (criterion #8). Core stays free of any downstream dependency (#7).
- Rejected: vendoring all packages into one mega-wheel (loses the workspace layout
  and the one-way arrow's enforceability); leaving deps unversioned and shipping a
  custom resolver shim (reinvents a resolver, fragile).
- Consequences: a version bump must update the constraint scheme consistently across
  packages, or the foreign install graph stops resolving — flagged as a shared
  invariant in the Decomposition Hint.

### D-4: Deterministic command + one manual gate; no rubric  (Status: Accepted)
- Context: every end-state here (files present, exit codes, METADATA lines, schema
  tables) is machine-decidable. | Decision: all correctness criteria are `command`;
  the only non-command criterion is #10 (a `manual` operator gate on
  removed/relaxed verification-surface assertions, which needs human judgment about
  "equal-or-greater strength"). No `rubric` criterion.
- Rejected: a rubric over "is it nicely distributable" — gameable by a superficial
  token and machine-decidable anyway.
- Consequences: #10 carries a human checkpoint; acceptable because it guards the
  verification surface and a command cannot judge replacement-strength equivalence.

### D-5: Reconciliation (2026-06-17) — confirmed independent; no shared code surface with 00026-00028, 00030-00032  (Status: Accepted)
- Context: the cross-spec reconcile checked whether this spec's edits collide with the
  other six. 00026/00028 add fields to the `WorkPolicy` dataclass in
  `_policy.py`; 00030/00031/00032 touch `store_protocols`/`store_sqlite`/`store_postgres`/
  `persistence-schema.sql`; 00027 touches `worker.py`.
- Decision: this spec touches ONLY `packages/*/pyproject.toml` (inter-package version
  constraints + wheel build/package-data metadata), the root `[tool.uv.sources]` workspace
  table interplay, and a build/publish helper — none of which any other spec in the batch
  edits. NO shared-invariant (SI) label is required; this spec carries no cross-spec
  prerequisite edge and is a prerequisite of none. It may land in any order relative to the
  other six. The one indirect coupling worth recording: this spec's package-data inclusion
  set (`_schema/*.sql`, `_skill_templates/*.md`, `py.typed`) must remain the source of truth;
  if 00030/00031/00032 ever ADD a `.sql` schema file (they do not — SI-9 holds schema at v12,
  no new file), Layer B's enumerated glob set would need to include it. Since the store specs
  add no new data file, no edge is needed.
- Rejected: assigning a defensive SI label "just in case" (no actual shared surface exists, so
  a label would be noise); making 00029 depend on the store specs (their no-new-schema-file
  decision means the package-data set is unchanged).
- Consequences: 00029 is a free-standing node in the cross-spec DAG.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader except #10, which is an
intentional `manual` verification-surface gate (judging replacement-strength
equivalence is not machine-decidable). No un-gradeable criterion remains.

## Next Steps
Run `/fw-plan 00029-FEATURE-distributable-install` to compile these criteria into
flywheel tasks and graders.
