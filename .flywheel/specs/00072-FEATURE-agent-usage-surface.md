# Feature: Agent usage surface (`fw docs` + flywheel-ops skill + init breadcrumb)

## Outcome
An agent (or human) in any repo where flywheel is installed can retrieve the authoritative, version-matched flywheel documentation from the CLI and load an installed skill that teaches operating the loop — without access to the flywheel source checkout, without an MCP server, and at zero context cost until invoked.

## Background
Consumer repos get flywheel's binary but none of its knowledge: `docs/` lives only in the flywheel checkout, so agents in adopting repos (observed in the infrared adoption) fly on `--help` output alone. Separately, operator sessions demonstrably forget the installed fw-* skills exist. The tacit bar from the interview: knowledge must be version-matched to the installed flywheel (no drift between what an agent reads and what the binary does) and must never require the operator's CLAUDE.md to be edited by tooling.

## Scope
### In scope
- A `fw docs` verb serving a curated set of operator docs shipped inside the installed package.
- A `flywheel-ops` skill, rendered by `flywheel init --skills` alongside the existing five, teaching loop operation and recovery.
- A copy-pasteable CLAUDE.md pointer line printed by init (suggestion only).
### Out of scope
- Any MCP server or MCP resource surface (D-1).
- Serving `docs/research/` or repo-internal specs/audits.
- Automatic editing of any consumer file beyond what init already writes (CLAUDE.md is untouchable, D-3).
- Worker-agent (in-loop) prompt changes; this feature targets operator-side sessions.
### Must not regress
- The five existing fw-* skills still render with unchanged names and managed-by headers.
- Every existing `fw`/`flywheel` verb resolves exactly as before; `scripts/check.sh` stays green.
- `flywheel init` idempotency: re-runs still never clobber existing files.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

The **curated topic set** (shared invariant, D-4): `cli`, `configuration`,
`task-schema`, `workflow`, `loop`, `strategy`, `sandbox`, `autopilot`,
`held-out-gate` — each sourced from the identically-named `docs/<topic>.md`.

1. When `fw docs <topic>` runs for any curated topic from any working directory — including one outside every flywheel checkout — stdout is the complete canonical document for the installed version and the exit code is 0. [command | held-out]
   verify: in an empty temp dir, for every curated topic, `fw docs <topic>` exits 0 and stdout is byte-identical to the flywheel repo's `docs/<topic>.md` at the built commit (parity harness authored out-of-band).
   defends against: printing a stub/summary, or resolving repo-relative paths so the verb only works inside the flywheel checkout.

2. When `fw docs` runs with no argument, stdout lists exactly the curated topics, one per line with a one-line description, and exits 0. [command | visible]
   verify: the listed topic set equals the curated set; every listed topic then passes criterion 1's per-topic check.
   defends against: a display list hardcoded separately from the served set, so listing and serving silently drift.

3. If `fw docs <unknown-topic>` runs, the exit code is non-zero and stderr names the unknown topic and how to list valid ones; stdout is empty. [command | visible]
   verify: `fw docs no-such-topic` exits non-zero, stderr contains `no-such-topic`, stdout is empty.
   defends against: silent exit-0 empty output that a caller or agent mistakes for a real document.

4. If a curated topic's source document is missing or renamed in `docs/` at test time, the test suite shall fail naming that topic. [command | held-out]
   verify: the parity harness iterates the curated list against the docs tree and fails loudly on any absent source.
   defends against: the embedded copies going stale against deleted/renamed docs while the suite stays green.

5. When `flywheel init --skills` runs, `.claude/skills/flywheel-ops/SKILL.md` is installed alongside the five existing fw-* skills, carrying the managed-by header. [command | visible]
   verify: in a fresh temp repo, init --skills produces all six SKILL.md files; flywheel-ops contains the `managed-by: flywheel init` marker.
   defends against: the skill existing only in the flywheel repo instead of shipping to every consumer.

6. The installed flywheel-ops skill shall reference only CLI surface that exists: every `fw`/`flywheel` verb and every `fw docs` topic it names resolves against the shipped verb router and the curated topic set. [command | held-out]
   verify: extract verb and topic tokens from the rendered SKILL.md; assert each verb is in the router's dispatch set and each named topic is curated.
   defends against: teaching agents hallucinated or stale commands — the exact failure this feature exists to prevent.

7. The flywheel-ops skill equips an agent to operate the loop end to end: driving `worker`, reading `status`, `status --rollup`, `live`, `history`, and `show`, and diagnosing then recovering a parked worktree and a stranded landing. [rubric, paired with #6 | held-out]
   verify: an LLM judge asserts an agent given only this skill plus the CLI could run a phase and correctly recover a parked worktree and a stranded DONE run; the paired command check requires anchors for each named verb plus `parked`, `stranded`, and `fw docs`.
   defends against: a skeleton file that name-drops verbs without operational guidance; token-stuffing the rubric is closed by the paired #6 and the anchor grep.

8. When init completes (interactive or `--defaults`), its output includes a copy-pasteable one-line CLAUDE.md pointer naming the flywheel-ops skill and `fw docs`. [command | visible]
   verify: run init in a temp repo; stdout contains one suggestion line referencing both `flywheel-ops` and `fw docs`.
   defends against: the discovery gap staying open silently — the operator always at least sees the line.

9. If init runs in a repo with or without a root CLAUDE.md, it shall neither create nor modify that file. [command | visible]
   verify: temp repo without CLAUDE.md → init → file still absent; temp repo with a CLAUDE.md of known hash → init → hash unchanged.
   defends against: tooling silently editing operator-owned instruction files (explicitly rejected in D-3).

Verification surface: unchanged.

## Decomposition Hint (for /fw-plan)
- Layer docs-and-verb: curated docs shipped as package data + `fw docs` serve/list/error paths: satisfies #1, #2, #3, #4.
- Layer skill: `flywheel-ops` template registered with the managed skill set: satisfies #5, #6, #7; depends on docs-and-verb (it references `fw docs` topics).
- Layer init-output: the printed CLAUDE.md suggestion and the no-touch guarantee: satisfies #8, #9; depends on skill (the line names it).
Shared invariant: the curated topic set — one source of truth consumed by the verb's list, the parity harness, and the skill's references. A slice that hardcodes its own copy breaks #2/#4/#6 together.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: CLI verb + skill, not an MCP server  (Status: Accepted)
- Context: agents in consumer repos need flywheel knowledge; MCP was the candidate. | Decision: ship knowledge via a `fw docs` verb and an installed skill.
- Rejected: MCP server (duplicates a local CLI as tool schemas, adds server lifecycle, burns context on schemas, and the `[sandbox.capabilities.mcp]` knob is known-broken); CLAUDE.md-embedded documentation (always-in-context cost). | Consequences: knowledge is pull-based; a future distributed-fleet steering surface may still justify an MCP facade over the store, as a separate feature.

### D-2: Docs single-sourced from `docs/`, embedded at build, parity-enforced  (Status: Accepted)
- Context: served docs could be hand-curated prose or copies of `docs/`. | Decision: the canonical `docs/*.md` files are the only source; the package embeds them; a test enforces byte parity.
- Rejected: separately-authored CLI help prose (guaranteed drift from the authoritative docs). | Consequences: docs edits are automatically the served content; parity failures block the gate.

### D-3: Breadcrumb is a printed suggestion; init never touches CLAUDE.md  (Status: Accepted)
- Context: the discovery gap ("sessions forget the skills exist") wants an always-in-context line; auto-append was the turnkey option. | Decision: init prints one copy-pasteable line; the operator owns the paste.
- Rejected: idempotent auto-append (tooling editing operator instruction files); interactive prompt (more code paths, same trust question). | Consequences: discovery still depends on the operator pasting the line; if the forget-failure recurs in practice, a superseding decision can revisit auto-append.

### D-4: Curated operator topic subset, not all of `docs/`  (Status: Accepted)
- Context: ship everything vs a curated operator surface. | Decision: the nine-topic curated set named in Success Criteria.
- Rejected: all top-level docs (leaner surface preferred by the author). | Consequences: new docs require a deliberate curation add; criterion #4 protects the delete/rename direction, additions remain a judgment call.

## Open Questions (accepted gaps)
None.

## Next Steps
Run `/fw-plan 00072-FEATURE-agent-usage-surface` to compile these criteria into flywheel tasks and graders.
