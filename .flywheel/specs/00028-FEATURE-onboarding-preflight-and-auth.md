# Feature: Onboarding preflight, sandbox-setup scaffolding, and documented agent auth

## Outcome
A fresh adopter who clones the repo and runs the documented quickstart reaches a
first successful run instead of an opaque failure. `flywheel init` refuses up
front (non-zero exit, actionable message) when its runtime preconditions are
absent — the working directory is not a git repository, or HEAD is detached —
rather than letting the worker discover them later. The policy file `init`
writes carries a `[sandbox]` setup section (commented scaffold plus an emitted
hint) so an adopter is steered to install dependencies before graders run,
closing the deps-less-worktree "0 tests collected, passing" footgun. And the
onboarding surface (README + the `init` next-steps) names the agent-auth step
(`ANTHROPIC_API_KEY` or `claude login`) so the SDK does not fail opaquely on the
first `worker` run.

## Background
The seams are correct but the turnkey path assumes flywheel's own host: a git
identity already set, dependencies already installed, and SDK auth already
present. Three audit findings (B4, B5, G7 in
`docs/research/2026-06-17-adoption-readiness-audit.md`) each make a stranger's
first run fail without explaining why. Verified against source on 2026-06-17:
`flywheel init --defaults` exits 0 and writes `flywheel.toml` even in a directory
that is not a git repo (`_cmd_init`, `_workflow.py:2343`); the rendered policy's
tail (`_INIT_POLICY_TAIL`, `_workflow.py:1699`) emits `[paths]`, a commented
`[[defaults.graders]]`, and a commented `[agent]` block but no `[sandbox]`
section, while `sandbox_setup` defaults to `None` (`_policy.py:145`); and
README/`docs/workflow.md`/`init` next-steps contain zero references to
`ANTHROPIC_API_KEY` or `claude login` (only the audit file mentions them). The
tacit expectation a literal agent would miss: the empty-grader case is
*silent success*, not a crash — `uv run pytest` in a deps-less worktree collects
0 tests and exits 0, so a grader "passes" having verified nothing. That is the
highest-stakes outcome here, and the reason the sandbox criterion is graded
against an observable policy-file state, not against prose.

## Scope
### In scope
- `flywheel init` performs a git-repo preflight: when the working directory is
  not inside a git repository, it exits non-zero with an actionable message and
  writes no `flywheel.toml`.
- `flywheel init` performs a detached-HEAD preflight: when HEAD is detached
  (the worker's `_phase_base` precondition, `worker.py:885-892`), it exits
  non-zero with an actionable message and writes no `flywheel.toml`.
- The `flywheel.toml` rendered by `init` carries a `[sandbox]` section
  (a `setup` command scaffold) so an adopter has a one-edit path to a populated
  worktree before graders run.
- `flywheel init`'s next-steps output and the README onboarding surface name the
  agent-authentication step required before the first `worker` run.

### Out of scope
- Enforcing or running the sandbox setup command (the worker already reads
  `policy.sandbox_setup` at `worker.py:1041`); this spec scaffolds the config and
  guidance, it does not change worker execution.
- Setting or validating a git identity (`user.name`/`user.email`) — that is
  blocker B2, a separate spec.
- Configurable / protected base branch and refuse-to-land-onto-checked-out-branch
  (B3) and base-branch config — owned by spec 00026. This spec MUST NOT add a
  `[submit] base` key, a `submit_base` attribute, or any `[submit]` scaffolding to
  the rendered policy; per SI-1 that key is 00026's, and the two specs append to the
  same `_INIT_POLICY_TAIL` template without overwriting each other (00028 adds only
  the `[sandbox]` block).
- Verifying that the agent SDK credential is actually valid (a live auth probe);
  this spec documents the step and the failure, it does not call the SDK.
- Any GitHub-source-specific onboarding beyond what `init` already prints.

### Must not regress
- `init` idempotency and never-clobber: an existing `flywheel.toml` is left
  untouched and reported (`_cmd_init`, `_workflow.py:2380-2396`).
- The rendered `flywheel.toml` still loads cleanly via `load_policy` with all
  pre-existing defaults intact (`source_kind`, `tasks_dir`, `db_path`,
  `sandbox_root`, `store_backend`).
- A policy file that declares no `[sandbox]` section still loads with
  `sandbox_setup is None` (the historical bare-sandbox behavior;
  `_policy.py:145`, `_optional_sandbox_setup` at `_policy.py:387`).
- The full existing test suite (`uv run pytest`) still passes.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When `flywheel init` runs with a working directory that is not inside a git
   repository, the command shall exit non-zero and write no `flywheel.toml`.
   [command | held-out]
   verify: a held-out pytest in `packages/flywheel-orchestrator/tests/test_init.py`
   chdir's into a tmp dir that is NOT `git init`-ed (and whose parents are not
   git repos), invokes `main(["init", "--defaults"])`, and asserts the return
   value is non-zero AND `Path("flywheel.toml")` does not exist after the call.
   Run: `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k git_preflight`.
   defends against: satisfying "preflight" by printing a warning but still
   exiting 0 and still writing the policy file — the test asserts both the
   non-zero exit AND the absent file, so a warning-only no-op fails.

2. When `flywheel init` aborts because the working directory is not a git
   repository, the emitted error shall name the missing precondition in operator
   terms (the words "git" and "repository"/"repo"). [command | visible]
   verify: a pytest captures stderr (capsys) of the same non-git `init` run and
   asserts the message contains case-insensitive "git" and one of
   "repository"/"repo". Run: `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k git_preflight_message`.
   defends against: a bare non-zero exit with an opaque or empty message that
   passes criterion 1 but leaves the adopter as stuck as the worker would have.

3. When `flywheel init` runs while HEAD is detached, the command shall exit
   non-zero and write no `flywheel.toml`. [command | held-out]
   verify: a held-out pytest creates a git repo in tmp (`git init`, one commit so
   a SHA exists), detaches HEAD (`git checkout <sha>`), invokes
   `main(["init", "--defaults"])`, and asserts non-zero return AND no
   `flywheel.toml` written. The error message is asserted to contain
   case-insensitive "detached". Run: `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k detached_head`.
   defends against: gating only on the absence of `.git` (criterion 1) while
   leaving the worker's other documented precondition (`_phase_base` rejects
   `HEAD`/detached, `worker.py:885-892`) to fail later at worker start.
   reconcile note (SI-8): the detached-HEAD refusal matches the worker's DEFAULT
   precondition (no `submit_base` set), which is the state `init --defaults`
   produces. Spec 00026 makes the worker TOLERATE detached HEAD only when
   `[submit] base` is explicitly configured; `init --defaults` never writes that
   key, so the refusal stays correct for the path init creates. The two are not in
   conflict — see D-5.

4. When `flywheel init` runs inside a git repository on a normal (attached)
   branch, the command shall exit 0 and write a `flywheel.toml` (preflight does
   not regress the happy path). [command | visible]
   verify: existing-style pytest `git init`s a tmp repo with one commit on a
   branch, runs `main(["init", "--defaults"])`, asserts return 0 and
   `Path("flywheel.toml").is_file()`. Run: `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k init_happy_path`.
   defends against: implementing the preflight so strictly that it also blocks
   the supported case — e.g. rejecting a valid repo with a freshly created
   default branch — silently breaking every legitimate adopter.

5. The `flywheel.toml` rendered by `flywheel init` shall contain a `[sandbox]`
   section header that documents a `setup` command, so an adopter has a
   single-edit path to a populated worktree. [command | held-out]
   verify: a held-out pytest runs `init` in a valid tmp git repo, reads
   `flywheel.toml`, and asserts the text contains a line matching
   `^\s*#?\s*\[sandbox\]\s*$` AND a `setup`-key example line referencing the
   workspace install command `uv sync` (regex: `setup\s*=.*uv sync`). Run:
   `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k sandbox_section_rendered`.
   defends against: emitting a `[sandbox]` header with no `setup` example, or a
   `setup` value that installs nothing (the empty-grader footgun unchanged) — the
   regex requires the dependency-install command, not just the section header.

6. When the rendered policy's `[sandbox] setup` example is uncommented, the
   parsed policy shall expose that command as `sandbox_setup` (the scaffold is
   real config, not dead text). [command | held-out]
   verify: a held-out pytest runs `init`, reads `flywheel.toml`, strips the
   leading `# ` from the `[sandbox]` block and its `setup` line, writes it back,
   loads it via `load_policy`, and asserts `policy.sandbox_setup == "uv sync"`.
   Run: `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k sandbox_setup_uncomment_parses`.
   defends against: shipping a `[sandbox]` block whose key name or value is not
   what the parser actually reads (`_optional_sandbox_setup`, `_policy.py:387`),
   so the documented edit silently does nothing and the adopter is back in a
   bare worktree.

7. The README quickstart and the `flywheel init` next-steps output shall each
   name the agent-authentication step required before the first `worker` run.
   [command | held-out]
   verify: two held-out checks. (a) shell:
   `grep -qE 'ANTHROPIC_API_KEY|claude login' README.md`. (b) pytest: run
   `init` in a valid tmp git repo, capture stdout (capsys), assert it contains a
   case-insensitive reference to authenticating the agent matching
   `ANTHROPIC_API_KEY|claude login`. Run:
   `grep -qE 'ANTHROPIC_API_KEY|claude login' README.md && uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k next_steps_auth`.
   defends against: documenting auth in one place only (e.g. a docs file the
   quickstart never points to) so the adopter following the README still hits the
   opaque SDK failure; both the README and the post-init terminal output must
   carry it.

8. When `flywheel init` runs against an existing `flywheel.toml` non-interactively,
   the command shall leave that file byte-for-byte unchanged (the new preflight
   and sandbox scaffolding do not regress never-clobber). [command | held-out]
   verify: a held-out pytest writes a sentinel `flywheel.toml` in a valid tmp git
   repo, records its bytes, runs `main(["init"])` (non-interactive), and asserts
   the file's bytes are identical AND stdout reports it left untouched. Run:
   `uv run pytest packages/flywheel-orchestrator/tests/test_init.py -k existing_policy_untouched`.
   defends against: a preflight or sandbox-scaffold change that rewrites or
   appends to a tuned policy file on re-run, silently clobbering an adopter's
   configuration.

### Verification-surface (Definition-of-Done) — this feature changes onboarding/init machinery
This spec changes the init scaffolding and the documented onboarding path that
new adopters trust to reach a first run. The following are inherited, held-out
where possible, and every task touching the init/policy surface must satisfy them.

9. The full pre-existing test suite shall still pass after this change.
   [command | held-out] (verification-surface)
   verify: `uv run pytest` exits 0 with no test removed or skipped relative to
   the baseline; the new init/policy tests run alongside the existing
   `test_init.py` / `test_policy.py` suites.
   defends against: making a new criterion pass by deleting or weakening an
   existing init/policy assertion (e.g. the idempotency or
   `sandbox_setup is None` default tests) instead of adding behavior.

10. If any pre-existing init or policy test assertion is relaxed, removed, or
    skipped, then the change shall name it with its justification and a
    replacement check of equal-or-greater strength. [manual | held-out]
    (verification-surface)
    verify: operator diff review of `packages/flywheel-orchestrator/tests/` — a
    removed assertion with no named, equal-or-stronger replacement is a blocking
    defect.
    defends against: quietly dropping the historical "policy with no `[sandbox]`
    loads with `sandbox_setup is None`" assertion so a regression in the default
    sails through.

11. The behavior added by criteria 1, 3, 5, 6, 7, and 8 shall be proven by
    held-out checks the implementing agent did not author against its own known
    inputs. [command | held-out] (verification-surface)
    verify: the held-out graders for criteria 1, 3, 5, 6, 7, 8 are run by the
    harness out-of-band (not the visible suite the agent iterates against) and
    all exit 0.
    defends against: the agent writing a test that asserts whatever its
    implementation happens to print, so "the test passes" means "the agent wrote
    the test to pass" rather than "the preflight/scaffold/docs are correct."

Verification surface: the existing suite still passes (criterion 9); any relaxed
or removed init/policy check is named with an equal-or-greater replacement
(criterion 10); the new onboarding behavior is proven by out-of-band held-out
checks the agent did not author against its own known inputs (criterion 11).

## Decomposition Hint (for /fw-plan)
Splits along three slices that can land independently but share the `init` entry
point, so they must update together against one green suite:
- Layer init-preflight (orchestrator `_workflow.py` `_cmd_init`): satisfies
  #1, #2, #3, #4. Adds the git-repo + detached-HEAD gate before any file is
  written; reuses the worker's existing precondition shape (`_phase_base`,
  `worker.py:885-892`) so init and worker agree on what "ready" means.
- Layer sandbox-scaffold (orchestrator `_workflow.py` policy template
  `_INIT_POLICY_TAIL` + `_policy.py` parse path): satisfies #5, #6. The
  `[sandbox]` block init renders must round-trip through `load_policy`'s
  `_optional_sandbox_setup`.
- Layer onboarding-docs (README.md + `_print_init_next_steps`,
  `_workflow.py:2155`): satisfies #7. Pure text/print surface; depends on no
  other layer.
- Verification-surface DoD (#9, #10, #11) spans all layers.

Shared invariants multiple layers and the worker assert against:
- **SI-2** — the TOML key the sandbox setup command lives under is `[sandbox] setup`,
  read by `_optional_sandbox_setup` into `WorkPolicy.sandbox_setup`
  (`_policy.py:145,207,387`). The rendered scaffold (#5) and the parse assertion
  (#6) must use this exact name; the worker reads it at `worker.py:1041`. This is
  00028's sole addition to the shared `_INIT_POLICY_TAIL` template.
- **SI-1 (boundary)** — the init policy template `_INIT_POLICY_TAIL`
  (`_workflow.py:1699`) is shared with spec 00026, which adds a `[submit] base` key
  (`WorkPolicy.submit_base`). RECONCILED: both specs append commented blocks to the
  same template, neither rewrites the other's. 00028 owns the `[sandbox]` block;
  00026 owns the `[submit] base` line. There is no key-name collision (`sandbox.setup`
  vs `submit.base`), so the two can land in either order; if both land in one phase
  the second-to-touch `_INIT_POLICY_TAIL` rebases onto the first and re-runs its
  render assertions (00028 #5/#6 and 00026 #6).
- The detached-HEAD / git-repo definition init enforces (#1, #3) must match the
  worker's runtime preconditions (`_repo_root` and `_phase_base`,
  `worker.py:~875-892`) so init does not pass a state the worker then rejects.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Preflight is a hard gate (non-zero exit, no file written), not a warning  (Status: Accepted)
- Context: G7 says init checks neither git-repo nor non-detached-HEAD, so worker
  preconditions only fail at runtime (`worker.py:880-891`). The audit's intended
  end-state is "fails up front with actionable guidance." A warning that still
  writes `flywheel.toml` and exits 0 leaves the adopter exactly where the worker
  would have left them.
- Decision: When a precondition is absent, `init` exits non-zero and writes no
  `flywheel.toml` (criteria 1, 3). The next-steps and happy path are unchanged
  for valid repos (criterion 4).
- Rejected: warn-and-continue (fails the audit's "fails up front" intent and is
  gameable per criterion 1's defense); auto-`git init` (out of scope — silently
  mutating the adopter's filesystem is a worse surprise than a clear refusal).
- Consequences: a user running `init` in a non-repo or detached state must fix it
  first; this is the desired behavior. Tests that previously ran `init` in a bare
  tmp dir must now `git init` that dir first — that is the must-not-regress diff
  callout under criterion 10.

### D-2: Scaffold the sandbox setup command as a commented `[sandbox]` block whose example is `uv sync`  (Status: Accepted)
- Context: B5 — `[sandbox] setup` defaults to None, the template emits no
  `[sandbox]` section, and `uv run pytest` then runs in a deps-less worktree
  collecting 0 tests and "passing." The repo's own dependency-install command is
  `uv sync` (README quickstart, CLAUDE.md). The empty-grader footgun is the
  highest-stakes outcome in this spec.
- Decision: render a `[sandbox]` section in `_INIT_POLICY_TAIL` with a `setup`
  example of `uv sync` (criterion 5), mirroring the existing commented
  `[agent]`/`[[defaults.graders]]` style so the default stays bare (preserving
  the must-not-regress `sandbox_setup is None` default) while the one-edit path
  to a populated worktree is in front of the adopter and round-trips through the
  parser (criterion 6).
- Rejected: emitting an *uncommented* `setup = "uv sync"` by default (would change
  the historical bare-sandbox default and run a possibly-wrong command in every
  adopter's worktree — `uv sync` is correct for THIS repo, not universally;
  scaffolding keeps the adopter in control); prompting interactively for the
  setup command (larger init surface than the audit asks for; the commented
  scaffold is the minimal fix); a prose-only README note (not gradeable against
  policy state and easy to miss).
- Consequences: the default remains a bare sandbox unless the adopter uncomments,
  so the footgun is *disclosed and one edit away* rather than *eliminated by
  default*. This is the deliberate trade: do not silently run an install command
  the adopter did not choose. Enforcing/auto-running setup is out of scope.

### D-3: Auth is documented in two places — README and the init next-steps  (Status: Accepted)
- Context: B4 — zero references to `ANTHROPIC_API_KEY`/`claude login` anywhere in
  the onboarding surface; the quickstart goes `uv sync -> init -> worker --once`
  and hits an opaque SDK auth failure. The README is the cloned-repo entry point;
  the init next-steps are the last thing the adopter sees before running the
  worker.
- Decision: name the auth step in BOTH the README quickstart and
  `_print_init_next_steps` output (criterion 7), referencing
  `ANTHROPIC_API_KEY` or `claude login`. Documenting it in only one place leaves
  a path to the opaque failure (criterion 7's defense).
- Rejected: a live auth probe in init (out of scope — would couple init to the
  optional SDK extra, violating the lazy-SDK-boundary invariant; init must not
  hard-import the SDK); a standalone docs page only (the quickstart never points
  to it, so the README reader still fails).
- Consequences: two surfaces carry the same fact; a future auth change touches
  both. Acceptable: both are cheap text and both are graded.

### D-4: Held-out tests live in test_init.py and run via keyword-selected pytest  (Status: Accepted)
- Context: The repo's only deterministic grader vocabulary is pytest runs and
  shell state checks (`grep -q`, `test -f`); there is no typecheck CLI. Bare
  `uv run pytest` collects only the four package test dirs. Existing init tests
  already use `main(["init"])` under `monkeypatch.chdir(tmp_path)` with capsys.
- Decision: every command criterion lowers to either a `grep -qE` on a tracked
  file (README) or a keyword-selected pytest under
  `packages/flywheel-orchestrator/tests/test_init.py`, reusing the established
  `main([...])` + capsys + tmp-repo pattern. The README check is a shell
  `grep -q`, satisfying the cross-spec note that the docs criterion lowers to a
  grep command.
- Rejected: a typecheck grader (no such CLI is wired — the audit and repo facts
  forbid inventing one); manual gates for the machine-decidable criteria (a
  manual tier is weaker than the available command tier).
- Consequences: graders are runnable with the repo's real commands and bind to
  `test_init.py`; the verification surface is the init/policy test files.

### D-5: Reconciliation (2026-06-17) — no `_INIT_POLICY_TAIL` collision with 00026; detached-HEAD refusal is consistent with 00026's worker change  (Status: Accepted)
- Context: reconcile read 00028 against 00026 (shared `_INIT_POLICY_TAIL` + `WorkPolicy`)
  and surfaced two questions: (a) could the two specs' template additions or config
  keys collide, and (b) does 00028's "init refuses detached HEAD" (criterion 3)
  contradict 00026's "worker tolerates detached HEAD" (00026 criterion 3)?
- Decision: (a) No collision. 00028 adds ONLY the `[sandbox]` block + `sandbox.setup`
  key (SI-2); 00026 adds ONLY the `[submit] base` line + `submit_base` attribute
  (SI-1). Distinct TOML tables, distinct `WorkPolicy` fields. Both append to
  `_INIT_POLICY_TAIL` without rewriting the other; whichever lands second rebases and
  re-runs its render assertions. No prerequisite edge is required between 00026 and
  00028 — they are independent (SI-8). (b) No contradiction. 00026 only relaxes the
  worker's detached-HEAD precondition WHEN `[submit] base` is explicitly set;
  `init --defaults` never writes that key, so the state init produces is exactly the
  one the worker still rejects. The init refusal correctly mirrors the worker's
  default precondition; an adopter who later sets `[submit] base` opts into the
  detached-HEAD-tolerant worker path knowingly.
- Rejected: making 00028 a prerequisite of 00026 or vice-versa (no shared code region
  forces an order — only the same template file, which git-merges cleanly for
  non-overlapping appends); softening 00028's detached-HEAD refusal to "only when no
  base configured" (init at scaffold time has no base configured by construction, so
  the simpler unconditional refusal is correct and the only state init reaches).
- Consequences: `/fw-plan` for 00028 and 00026 may schedule in either order; if
  co-phased, the second `_INIT_POLICY_TAIL` editor re-runs both specs' render
  assertions after rebase (00026 #6, 00028 #5/#6).

## Open Questions (accepted gaps)
None. Every criterion lowers to a command grader (pytest or `grep -q`) except the
inherited verification-surface review (criterion 10), which is intrinsically a
manual diff-review gate paired with the held-out command suite (criteria 9, 11).

## Next Steps
Run `/fw-plan 00028-FEATURE-onboarding-preflight-and-auth` to compile these
criteria into flywheel tasks and graders.
