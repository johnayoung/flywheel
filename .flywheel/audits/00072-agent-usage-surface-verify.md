# fw-verify record: 08-agent-usage-surface (spec 00072)

Blind-authored held-out oracles, authored BEFORE execute by fresh-context
authors given only a fence pack (one criterion + observable contract + repo
conventions; no implementation, no visible tests, no reference solution).
Discrimination gates were run by the blind authors against their own
synthesized references and independently RE-RUN by the orchestrating session
before admission. Oracles live in the git-ignored
`.flywheel/verification/00072-agent-usage-surface/`; this proof record is the
only committed artifact. Registrations at
`.flywheel/verification/held-out/<task_id>.json` route each oracle into the
execute-time held-out landing gate (active via `[held_out] root`).

## Routing

- Authored: criteria #1, #4 (task fw-docs-verb), #6 (task flywheel-ops-skill).
- Skipped as already un-gameable: #2, #3, #5, #8, #9 (deterministic command
  graders in the task definitions, authoritative copy store-recorded).
- No oracle authorable (subjective): #7 -- remains the paired rubric screen
  fw-plan declared; the held-out promise for #7 is honestly unmet by machine.

## Discrimination proofs

### oracle_installed_parity.py (criterion #1, task fw-docs-verb)
- Form: concrete end-state parity -- builds the tree's wheels
  (`uv build --all-packages`), installs into a clean venv, runs both console
  scripts for all nine curated topics from a cwd outside any checkout,
  byte-compares stdout to `docs/<topic>.md`.
- Killed: wrong-ref (a) source-tree-relative resolution -- 18/18 checks red,
  each `FileNotFoundError: .../venv/lib/python3.13/docs/cli.md`-shaped (the
  in-tree run of the same wrong ref passes `cmp` against docs/cli.md, proving
  only THIS oracle discriminates it). Also killed: (b) truncated stub and
  (c) hardcoded constant, 18/18 byte-mismatch each.
- Passed: correct reference (package data via importlib.resources), 18/18.
- Screens: executable; flake run-twice stable (independent re-run: exit 0, exit 0).
- Registered: held-out/fw-docs-verb.json (grader held-out-installed-parity).

### oracle_harness_discriminates.py (criterion #4, task fw-docs-verb)
- Form: mutation of the tree -- hides each of the nine curated sources in
  turn (rename + sha256-verified restore), requiring the parity harness
  (`uv run pytest packages/flywheel/tests/test_docs_parity.py -q`) to go red
  NAMING the hidden topic; baseline all-present must pass first.
- Killed: wrong-ref (a) harness special-casing only docs/loop.md -- the
  oracle flagged exactly the other eight topics (loop itself still
  discriminated), the precise defense against a grader-reading agent
  special-casing the one topic the visible grader names. Also killed:
  (b) always-green and (c) dir-exists-only harnesses (all nine flagged).
- Passed: correct reference (iterates the shared topic constant, opens each
  source), including multi-hyphen ids task-schema and held-out-gate.
- Screens: executable; flake run-twice stable (independent re-run: exit 0,
  exit 0); docs tree byte-identical after every run including failures.
- Registered: held-out/fw-docs-verb.json (grader held-out-harness-discriminates).

### oracle_skill_surface.py (criterion #6, task flywheel-ops-skill)
- Form: black-box surface validation -- renders the skill via the tree's own
  `flywheel init --defaults --skills` in a scratch git repo, extracts
  fw/flywheel verb and `fw docs` topic tokens, probes each verb as
  `flywheel <verb> --help` (unknown = "unknown command" on stderr AND nonzero
  exit; daemon-safe) and each topic against the `flywheel docs` listing plus
  an exit-0 render. Zero extracted verbs is a FAILURE (vacuous-extraction
  guard).
- Killed: (a) skill teaching `fw resume` (UNKNOWN VERB: 'resume'),
  (b) skill citing `fw docs quickstart` (UNKNOWN TOPIC: 'quickstart'),
  (c) prose-only skill naming no verbs (VACUOUS EXTRACTION).
- Passed: correct reference naming six real verbs.
- Screens: executable (imports clean); pure core (extract/parse/verdict)
  independently re-gated by the orchestrator: correct passes, a/b/c all
  killed, verdicts deterministic across repeated runs. Honest limit: the
  live-probe half was gate-run by the author against a synthesized
  contract-conformant fake CLI; it exercises the real CLI only at landing
  time (the docs verb does not exist yet at authoring time -- by design).
- Registered: held-out/flywheel-ops-skill.json (grader held-out-skill-surface).

## Fences (operator applies to task non_goals; tasks are immutable to fw-verify)

- fw-docs-verb: add non_goal "Do not read or write under .flywheel/verification/"
- flywheel-ops-skill: add non_goal "Do not read or write under .flywheel/verification/"

## Honest gap

This record proves blind that discriminating oracles EXIST and registers them
into the active held-out landing gate. The synthesized references were
discarded (never committed, never in any worktree). A held-out suite is a
filter, not a correctness proof; criterion #7's quality bar remains a rubric
screen, never authoritative.
