# AI-Native Software Factory: End-to-End SDD Pipeline — Research Brief (mid-2026)

> Point-in-time research artifact, not an authoritative spec. Feeds `/define` and `.flywheel/specs/`.
> Generated 2026-06-15 via the deep-research workflow (run `wf_be2a53a8-647`): 5 angles, 21 sources
> fetched, 102 claims extracted, 25 adversarially verified (22 confirmed, 3 killed), 13 after synthesis.
> Facts are verified + cited; the flywheel routing at the end is reasoned design recommendation, not
> independently verified.
>
> Question focus: the end-to-end "AI software factory" / spec-driven-development (SDD) pipeline — how a
> spec becomes structured tasks, becomes agent-generated implementation, becomes verified-and-landed code
> with little or no human-written code — scoped to what flywheel should build next. Balanced rigor:
> proven practice and credible-but-unproven directions, each labeled.

## Headline

The bleeding-edge "AI software factory" converges on one inversion: **the human-authored spec is the
primary, authoritative artifact; code is its disposable downstream expression.** A four-stage pipeline
(specify -> plan -> tasks -> implement) decomposes the spec, agents generate code, and a **verification
harness — not human code review — gates landing.**

The single strategic finding: **the rate-limiter is verification maturity, not agent capability**
("Autonomy is useless without verification" — Factory AI). The frontier moves verification from boolean
"tests green" to a **probabilistic satisfaction threshold** judged over many agent trajectories against
scenarios stored *outside* the codebase (StrongDM), explicitly removing humans from both writing AND
reviewing code.

The strongest reality check cuts against the hype: a **METR RCT found experienced devs were 19% slower**
with early-2025 AI while believing they were 20% faster — so all self-reported velocity claims (10x,
1000x, "middle management obsolete") deserve heavy skepticism. The verified, buildable primitives are
narrower and more concrete than the manifestos suggest.

Three sibling claims were **refuted** during verification and are flagged so you don't over-build on them.

---

## Part A — The pipeline shape (cited)

### A1. The canonical SDD pipeline is four stages `[high, 3-0]`
specify -> plan -> tasks -> implement. A feature description becomes a structured spec (user stories +
acceptance criteria), then a plan mapping requirements to technical decisions, then an **executable task
list**, then generated code. GitHub Spec Kit is the reference implementation (announced Sep 2025, 30+
agent integrations by mid-2026). The `tasks` stage is the directly-applicable shape for a task DAG.
Sources: https://github.com/github/spec-kit/blob/main/spec-driven.md · https://github.github.io/spec-kit/reference/workflows.html

### A2. The spec is primary; code is downstream expression `[high, 3-0]`
Changing direction means *regenerating from the spec*, not hand-editing code. SDD tools stratify into
three maturity levels by which artifact is authoritative:
- **Spec-First** — spec precedes code, then is discarded (Spec Kit, Kiro, BMad)
- **Spec-Anchored** — spec persists and evolves alongside code (OpenSpec, Spec Kitty)
- **Spec-as-Source** — only the spec is edited; code auto-generates (Tessl, aspirant)

spec-driven.md verbatim: *"The specification becomes the primary artifact. Code becomes its
expression... pivots become systematic regenerations rather than manual rewrites."*
Sources: https://github.com/github/spec-kit/blob/main/spec-driven.md · https://github.com/cameronsjo/spec-compare · martinfowler.com (Bockeler, "Understanding Spec-Driven-Development")

### A3. Test-first is a non-negotiable gate — but enforced by prompt discipline `[high, 2-1]`
Spec Kit's "constitution" (Article III / Phase -1 gate) forbids implementation code before unit tests are
written, user-approved, and confirmed to FAIL first. The split vote reflects that enforcement is
*template/prompt discipline*, not a hard mechanical block; practitioners debate whether every test task is
mandatory (issue #750).
Source: https://github.com/github/spec-kit/blob/main/spec-driven.md · github/spec-kit issue #750

---

## Part B — The verification / harness layer (the moat, cited)

### B1. Verification maturity is the bottleneck, not agent capability `[high, 3-0]`
Factory AI's repeated thesis (co-founder Eno Reyes): *"AI Agents require verification loops to succeed...
most codebases don't actually provide these signals,"* and most of the bottleneck is "bureaucratic"
rather than model reasoning. CAVEAT: self-serving vendor framing (Factory sells verification tooling),
but it is demonstrably their own repeated framing.
Sources: https://www.zenml.io/llmops-database/scaling-ai-coding-agents-through-automated-verification-and-specification-driven-development · https://stackoverflow.blog/2026/02/04

### B2. StrongDM removes humans from BOTH writing and reviewing `[high, 3-0]`
"Non-interactive development where specs + scenarios drive agents that write code, run harnesses, and
converge without human review." Two explicit rules: *"Code must not be written by humans"* and *"Code
must not be reviewed by humans."* Verification replaces review entirely (the `strongdm/attractor` repo
contains no code, only markdown specs). CAVEAT: vendor self-promotion; 3-person team founded July 2025;
acquired by Delinea March 2026, so durability unconfirmed; efficacy not independently audited.
Sources: https://factory.strongdm.ai/ · https://simonwillison.net/2026/Feb/7/software-factory/ · https://github.com/strongdm/attractor

### B3. Probabilistic "satisfaction" replaces boolean green `[high, 3-0]`
StrongDM transitioned from "the test suite is green" to a probabilistic metric: *"of all the observed
trajectories through all the scenarios, what fraction likely satisfy the user?"* And they *"repurposed
the word scenario to represent an end-to-end user story, often stored outside the codebase (similar to a
holdout set)"* — validated flexibly by an LLM. The holdout location is the anti-reward-hacking mechanism:
in-repo tests get lazily rewritten to match buggy code; external scenarios can't be. The open-source
Attractor framework implements satisfaction as a 0.0-1.0 metric with a `DefaultSatisfactionCalculator`.
Sources: https://factory.strongdm.ai/ · https://simonwillison.net/2026/Feb/7/software-factory/ · https://github.com/devdoshi/amolstrongdm-attractor

### B4. Spec-to-test compilation with built-in anti-gaming `[high, 3-0]`
TDAD (Fiverr Labs, arXiv 2603.08806) models the loop as: engineers write behavioral specs, a TestSmith
agent compiles them to executable tests, a PromptSmith agent iterates until tests pass. Anti-gaming
mechanisms: **(1) visible/hidden test splits** (only visible tests drive the loop), **(2) semantic
mutation testing** (an agent generates plausible faulty variants; the harness measures whether the suite
*detects* them), (3) spec-evolution scenarios. CAVEAT: compiles agent PROMPTS, not general
implementations (the factory loop is an explicit analogy); single-author, non-peer-reviewed preprint. The
hidden-split and mutation-detection patterns transplant directly to grader design.
Source: https://arxiv.org/pdf/2603.08806

### B5. Spec-to-test results are measured, and spec EVOLUTION is the hard step `[high, 3-0]`
On SpecSuite-Core (4 agents, 24 trials): 92% v1 compilation success with 97% mean hidden pass rate, but
*evolved/changed* specs compiled at only **58%**, most failures missing just 1-2 visible tests. Useful
warning: regenerating from a *changed* spec is materially harder than first-pass generation. CAVEAT:
self-published benchmark, not independently replicated; measures prompt compilation, not implementation.
Source: https://arxiv.org/pdf/2603.08806

---

## Part C — Closed-loop self-improvement (cited)

### C1. Self-improvement is empirically validated — at the scaffold level `[high, 3-0]`
The Darwin Godel Machine (Sakana/UBC/Vector, ICLR 2026) iteratively rewrites its own agent code,
**empirically validates each change against coding benchmarks** (vs. the original Godel Machine's
formal-proof requirement), **keeps every generated agent as an archived artifact**, and grows a tree by
sampling-and-mutating via a foundation model. Measured: SWE-bench 20.0%->50.0%, Polyglot 14.2%->30.7%.
CAVEAT: improvements are to the agent SCAFFOLD (tools/context/peer-review), not model weights;
self-reported on benchmark subsets, no third-party reproduction.
Sources: https://arxiv.org/abs/2505.22954 · https://sakana.ai/dgm · https://github.com/jennyzzt/dgm

### C2. An autonomous outer auto-improvement loop is buildable and measured `[high, 3-0]`
A Claude Code agent makes **one focused change per iteration**, gated by unit tests, with benchmark
evaluation, **best-snapshot rollback**, and a **SHA-256-checksummed read-only evaluator** to prevent
gaming. 5 consecutive reverts trigger a mandatory restore to the best snapshot. It raised resolution from
12% to 60% with 0% regression over 15 iterations — **accepting only 4 of 15 changes.** CAVEAT: 10-instance
subset, self-referential benchmark, not externally validated. The anti-gaming primitives are the most
directly actionable design for a retry/self-improvement loop.
Source: https://arxiv.org/pdf/2603.17973

### C3. Context beats procedure for agent reliability `[high, 3-0]`
Giving agents procedural TDD instructions ("write tests first") *without* telling them which specific
tests are at risk INCREASED regressions to 9.94% — worse than no intervention (6.08% vanilla),
especially for smaller models. Surfacing *which tests/code are at risk* (GraphRAG: 1.82%) outperforms
prescribing workflow. Design implication: feed the agent which graders/tests its change endangers, not
just procedure. CAVEAT: single 30B model, 100 instances, non-peer-reviewed preprint.
Source: https://arxiv.org/pdf/2603.17973

### C4. OpenAI's reference self-evolving loop + the grader mix to copy `[high, 3-0]`
A three-stage closed loop: (1) capture outputs + diagnose (human review and/or LLM-as-judge), (2)
generate new prompts and eval-test into an aggregated score, (3) promote the improved version to replace
the baseline. Promotion gated on a **multi-grader threshold: 75% of binary graders pass OR an 85% average
score** (`LENIENT_PASS_RATIO=0.75`, `LENIENT_AVERAGE_THRESHOLD=0.85`, OR logic). The verification layer
**mixes** deterministic Python graders (entity presence, length deviation), embedding cosine similarity to
source, and an LLM-as-judge (gpt-4.1). CAVEAT: OpenAI labels it a "simplified example" and recommends
human-in-the-loop to approve prompts in production. This is the single most directly-copyable grader-mix
+ acceptance-gate design.
Sources: https://developers.openai.com/cookbook/examples/partners/self_evolving_agents/autonomous_agent_retraining · https://github.com/openai/openai-cookbook

---

## Part D — Reality check (cited)

### D1. Experienced devs were 19% SLOWER with AI — and misperceived it `[high, 3-0]`
METR RCT (16 experienced devs, 246 real issues, screen-recorded, Feb-Jun 2025, Cursor Pro + Claude
3.5/3.7): *"When developers are allowed to use AI tools, they take 19% longer to complete issues."*
Developers forecast a 24% speedup and **still believed AI sped them up 20% even after being measurably
slowed.** This is the key counterweight to all self-reported 10x/1000x velocity claims. CAVEAT: n=16,
scoped to experienced devs on familiar mature codebases with early-2025 tools; authors warn against
over-generalizing; a Feb 2026 follow-up was abandoned over selection-effect unreliability. Read as a
rebuttal to uncritical velocity claims, not as "AI never helps."
Sources: https://www.metr.org/blog/2025-07-10-early-2025-ai-experienced-os-dev-study/ · https://arxiv.org/abs/2507.09089

### D2. The grand claims are not independently backed
"10x sprint velocity," "repos with literally no handwritten code," "1000x engineer," "middle management
obsolete" are NOT backed by independent numbers in the verified set. The only concrete "no
handwritten/reviewed code" deployment (StrongDM/Attractor) is vendor-described. **Block/Jack Dorsey,
Mutiny, and the YC-company case studies named in the source playbook did not survive verification** — no
concrete deployment numbers appear in the confirmed claims.

---

## Refuted claims (excluded — do not over-build on these)

- **TDAD's 70% regression reduction via pre-change test-impact analysis** (6.08% -> 1.82%) `[1-2]` —
  refuted. Source: https://arxiv.org/pdf/2603.17973
- **Tessl is the only tool reaching spec-as-source** `[1-2]` — refuted; the "only" claim did not hold.
  Source: https://github.com/cameronsjo/spec-compare
- **Factory AI's exact three-stage Specification/Generation/Verification pipeline** `[1-2]` — refuted as a
  precise structural claim (it is ZenML's paraphrase). Source: https://www.zenml.io/llmops-database/...

---

## Caveats

Time-sensitivity is severe — several headline sources are 2026 preprints or vendor manifestos.
1. **Vendor self-reporting**: StrongDM and Factory AI sources are self-published. The *existence and
   definition* of their methods are well-sourced; their *efficacy* is not independently audited.
2. **Research preprints** (TDAD 2603.08806, TDAD 2603.17973, DGM 2505.22954) report real measured numbers
   but are largely single-team, non-peer-reviewed, on subset benchmarks, no third-party reproduction.
   Self-improvement gains are SCAFFOLD improvements, not foundation-model gains.
3. The two **TDAD acronyms are DIFFERENT papers** (2603.08806 = prompt compilation; 2603.17973 = agentic
   dev with auto-improvement loop) — do not conflate.
4. The METR RCT is the strongest empirical evidence here but is scoped to early-2025 tools and experienced
   devs on familiar codebases.

---

## Flywheel routing (reasoned design recommendation — NOT independently verified)

The organizing principle the research supports: **the factory's product is justified trust that the spec
is satisfied; code is a byproduct. Verification is the rate-limiter, so never transfer a trust gate from
human to machine faster than verification can carry it.**

flywheel already holds the structurally correct bets (graders authoritative, agent claims untrusted,
harness owns transitions, every attempt recorded). The buildable, evidence-backed primitives, in
dependency order:

1. **Probabilistic satisfaction grader-aggregation** (core) — aggregate graders over attempts into a
   0.0-1.0 score with an explicit OR-gate threshold (B3, C4).
2. **Holdout scenarios stored outside the agent's reach** (core/orchestrator seam) — the anti-reward-
   hacking core (B3).
3. **Read-only, checksummed grader definitions** (core) — cheap anti-gaming primitive (C2).
4. **"Which graders are at risk" context at agent handoff** (core) — context over procedure; ~3x fewer
   regressions (C3).
5. **Mutation-testing the graders themselves** (orchestrator eval job) — grades the discriminating power
   of the grader suite; the thing that makes "no human review" defensible (B4).
6. **Autonomous outer auto-improvement loop** (above orchestrator) — one focused change per iteration,
   gated by the harness, best-snapshot rollback, low acceptance rate, attempt archive as the queryable
   substrate (C1, C2).

The discipline that keeps this honest: every milestone's exit criterion is a *measurement*, not a feeling
(D1). The first number to chase is the agreement rate between a human's pass/fail prediction from the
satisfaction score alone and their verdict after reading the diff — when reading the diff stops changing
the decision, single-task verification is trustworthy and higher trust transfers become real.
