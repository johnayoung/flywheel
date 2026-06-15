# fw-retro (Retro/audit stage) — Skill Research Brief (mid-2026)

> Point-in-time research artifact, not an authoritative spec. Feeds the fw-retro skill rewrite.
> Generated 2026-06-15 via the skill-research workflow (run wf_b401de88-b18): 6 angles, 55 sources,
> 49 claims extracted, 12 adversarially verified (2 killed). Facts are verified + cited;
> the fw-retro routing at the end is reasoned design recommendation, not independently verified.

## Headline

A loop audit is an evidence-grounded, blameless diagnosis of *how the loop behaved*, not a search for a single root cause and not a place to propose fixes. The mature incident-review tradition (Google SRE, Allspaw/Etsy, Cook/Dekker) and the agentic-failure literature (MAST, AgentErrorTaxonomy) converge on the same design: every finding links to re-verifiable evidence, "the agent erred" is a starting point rather than a conclusion, and analysis is deliberately quarantined from remediation because proposing a fix corrupts the act of understanding. The binding constraint is precision, not coverage — when real loop defects are rare, a noisy auditor that flags every retry trains operators to ignore it — so fw-retro should emit few high-signal findings keyed to a small fixed failure taxonomy, escalate only on recurrence, and treat any finding without a re-runnable pointer as inadmissible.

## Part 1 — The diagnosis-first discipline (why stop at diagnosis)

### Conclusions must be grounded in linked evidence, focused on "what" not "who" `[high, 3-0]`

Google SRE's postmortem rubric makes evidence-linking the bar that separates a good postmortem from a bad one, directly justifying fw-retro's rule that every finding cite a re-verifiable pointer (run_id, audit-stream record, grader receipt, transcript line) and never trust agent-reported status.

> "All of the conclusions presented are based on facts and data. Any data used to arrive at a conclusion is linked from the document."

Vendor-authored (Google) but the canonical practitioner reference; cross-validated across both SRE books and verified verbatim under the workbook's Depth rubric and the Blameless "what not who" framing.

Sources: https://sre.google/workbook/postmortem-culture/ , https://sre.google/sre-book/postmortem-culture/

### The strongest practice separates learning from remediation, making fixes a non-goal `[high]`

The advanced incident-review school (Allspaw, ACL/Howie, Code for America) treats learning as the sole goal of analysis and producing action items as an explicit *non-goal*, because jumping to fixes invites speculation, hindsight bias, and blame. This is direct support for fw-retro stopping at diagnosis and handing fixes to fw-improve.

> "Being explicit about learning as the sole goal of the meeting, and equally explicit that producing action items is a non-goal, leads to more learning. ... that path leads to speculation, to judgment clouded by hindsight bias, to blame, and to everyone's favorite remediation item: 'Next time, do what you should.'"

Credible-but-contested: this is the advanced minority stance. Google SRE/USENIX and Atlassian couple every finding to a tracked action item (see Part 5). fw-retro's separate fw-improve stage takes the deferral side — defensible, not universal.

Sources: https://codeforamerica.org/news/from-accident-to-investment-how-to-run-better-blameless-postmortems/ , https://howie-guide.pagerduty.com/analyze/

### Incident learning is a diagnosis activity; a pre-rationalized "root" story is an illusion of understanding `[high, 3-0]`

Etsy's open-sourced Debriefing Facilitation Guide (Allspaw/Evans/Schauenberg) is the canonical justification for making diagnosis and remediation separate stages: rushing to a single "root" cause only *feels* like understanding.

> "All too often, when teams get together to discuss an event, they walk into the room with a story they've already rationalized about what happened. This urge to point to a 'root' cause is seductive—it allows us to believe that we've understood the event well enough, and can move on towards fixing things."

Recognized canonical practitioner text (Etsy 2016, open-sourced); not peer-reviewed but widely cited and grounded in Dekker's research. Verified verbatim.

Sources: https://github.com/etsy/DebriefingFacilitationGuide , https://www.etsy.com/codeascraft/debriefing-facilitation-guide/

### The separation mechanism is operational: capture fix-shaped ideas, then defer them `[high, 3-0]`

When remediation ideas surface during reconstruction, the facilitator writes them down and defers them, because fuller context routinely renders early fixes moot. fw-retro should let fix-shaped observations be captured as pointers but never resolve them in-stage — that is fw-improve's job.

> "More often than not, the unfolding of events in the timeline render those initial remediation items moot when the full (or at least fuller) context becomes clear."

Canonical practitioner guide; this "collect-but-defer" tactic is the concrete how-to behind the stage split. Verified verbatim.

Sources: https://github.com/etsy/DebriefingFacilitationGuide/blob/master/guide/04-structure-and-preparation.md

## Part 2 — No single root cause; the loop-vs-agent distinction

### There is no single root cause: failures need multiple jointly-sufficient contributors `[high, 3-0]`

Cook's "How Complex Systems Fail" establishes that post-accident attribution to one root cause is fundamentally wrong; fw-retro should surface a *set* of contributing factors (a flapping grader + a tight budget + an ambiguous spec), not a linear five-whys chain ending in one culprit.

> "Post-accident attribution to a 'root cause' is fundamentally wrong. ... There is no isolated 'cause' of an accident. There are multiple contributors to accidents. Each of these is necessarily insufficient in itself to create an accident. Only jointly are these causes sufficient to create an accident."

Cook 1998 is a foundational safety-science essay (not peer-reviewed in the journal sense) but is the field's canonical reference; corroborated by Allspaw for software and by independent academic lineage (Dekker, Hollnagel, Leveson/STAMP-CAST). Note the contested edge logged in Refuted/Caveats: Leveson and Cooper dispute the categorical "fundamentally wrong" framing.

Sources: https://how.complexsystems.fail/ , https://www.kitchensoap.com/2014/11/14/the-infinite-hows-or-the-dangers-of-the-five-whys/

### "Agent erred" is where analysis begins, not ends `[high]`

"Human error" is a starting point for investigation, not a conclusion; the productive question is how it made sense for the actor to do what it did. This maps to fw-retro's mandate to distinguish loop friction from agent mistakes.

> "When it comes to decisions and actions, we want to know how it made sense for someone to do what they did. ... 'Human error' becomes a starting point, not a conclusion."

Caveat: the literature targets human cognition under stress; the analytic discipline (don't stop at "error") transfers, but the human-rationality framing only partially applies to an LLM agent.

Sources: https://www.kitchensoap.com/2014/11/14/the-infinite-hows-or-the-dangers-of-the-five-whys/ , https://sre.google/sre-book/postmortem-culture/

### Blamelessness for fw-retro is an analytical discipline, not a reporting-safety ritual `[high]`

In the human literature blamelessness exists to protect people's psychological safety so they report honestly — a rationale that does NOT apply to an AI agent. fw-retro keeps only the systemic-analysis half: don't stop at "the agent was wrong," keep digging to systemic loop causes; treat the agent as a system component to diagnose, not a person to exonerate.

> "If a culture of finger pointing and shaming individuals or teams for doing the 'wrong' thing prevails, people will not bring issues to light for fear of punishment."

WHERE FLYWHEEL DIVERGES: the human-safety motivation is moot for an agent; retain only the systemic-analysis half of blamelessness.

Sources: https://sre.google/sre-book/postmortem-culture/ , https://www.etsy.com/codeascraft/debriefing-facilitation-guide/

### Blameless means investigating systemic information gaps, not indicting an actor `[high]`

The system-vs-actor framing maps cleanly onto fw-retro's loop-friction-vs-agent-mistake split: loop friction (ambiguous spec, missing context, budget squeeze, grader flap) is the system's fault and reportable; an agent's own coding mistake inside a correctly-functioning loop is a different, lower-priority category that must not be moralized.

> "When postmortems shift from allocating blame to investigating the systematic reasons why an individual or team had incomplete or incorrect information, effective prevention plans can be put in place."

Authoritative practitioner text (Google SRE Book). "Blameless" applied to a software agent is an analogy, but the system-vs-actor framing is exactly fw-retro's distinction.

Sources: https://sre.google/sre-book/postmortem-culture/

## Part 3 — Cognitive bias: judge by contemporaneous evidence, not outcome

### Hindsight bias is the primary obstacle; judge by what the loop saw at decision time `[high]`

Knowing the outcome makes prior signals seem more salient than they were. fw-retro must reconstruct the loop's decision context (what the harness observed when it retried/aborted) from the audit stream and transcript, not judge from the known end-state — which reinforces the pointer-citation discipline.

> "Hindsight biases post-accident assessments of human performance. ... Knowledge of the outcome makes it seem that events leading to the outcome should have appeared more salient to practitioners at the time than was actually the case."

The bias is human-cognitive; the practical control (use only contemporaneous evidence) is the transferable guardrail. A deterministic store reduces but does not eliminate the risk.

Sources: https://how.complexsystems.fail/ , https://www.etsy.com/codeascraft/debriefing-facilitation-guide/

### Counterfactual "should have" findings are inadmissible as explanations `[high]`

Statements like "the loop should have retried" describe a world that did not happen and cannot explain why the loop did what it did. fw-retro should reconstruct what the loop actually did and what state it observed, citing re-verifiable pointers, rather than phrasing findings as counterfactual reproaches.

> "Saying what people failed to do, or implying what they could or should have done to prevent the mishap, has no role in understanding human error."

Dekker (2004), quoted in Allspaw's practitioner essay; peer-recognized "new view" source. DIVERGENCE caveat below: bounded counterfactuals over machine telemetry are legitimate for the forward-looking fw-improve stage.

Sources: https://www.kitchensoap.com/2013/10/29/counterfactuals-knight-capital/

### Outcome bias: a FAILED task is not proof the loop misfired, a PASSED task is not proof it ran clean `[high]`

Outcome bias is empirically robust: people rate identical decisions as worse when the outcome was bad, even while explicitly agreeing the outcome should not matter. For fw-retro, diagnosis must judge loop behavior on process/telemetry, decoupled from task pass/fail — a direct argument for "skip clean tasks (by loop behavior, not by outcome)."

> "outcome bias occurred despite participants indicating that they believe outcomes should not impact their judgment"

Peer-reviewed seminal study (Baron & Hershey, J. Personality & Social Psychology, 1988) plus a 2023 pre-registered replication with larger effect sizes. Strongest empirical leg of the bias angle.

Sources: https://www.ncbi.nlm.nih.gov/pmc/articles/PMC12372742/ , https://en.wikipedia.org/wiki/Outcome_bias

### Resist the narrative fallacy: surface multiple independently-cited friction signals `[medium]`

Humans compress messy interacting causes into one clean story, and "5 Whys"-style RCA is a prime venue for it. Allspaw's guide independently warns of "a powerful tendency to find a single simple solution." fw-retro should resist collapsing a phase to one root cause and instead surface multiple friction signals, each separately cited.

> "you need to counter this most human urge to find a single explanation and single fix for the incident."

Taleb's narrative fallacy is a popular/practitioner concept (The Black Swan), not a controlled experimental finding; the RCA-critique linkage came largely via blog summaries. Treat the Taleb attribution as illustrative; the Allspaw "single explanation" quote is the firmer evidence.

Sources: https://github.com/etsy/DebriefingFacilitationGuide/blob/master/guide/04-structure-and-preparation.md , https://fs.blog/narrative-fallacy/

### DIVERGENCE: bounded re-verifiable counterfactuals belong in fw-improve, not fw-retro `[medium]`

The human-incident tradition is anti-counterfactual on principle because its evidence is fallible human memory. fw-retro audits a machine-readable, mostly-deterministic run history, so several memory-era biases are weaker and bounded counterfactuals are legitimate — but only as forward-looking prevention handed to fw-improve, never as in-stage explanations of behavior.

> "Counterfactual reasoning can be useful when we're trying to discover countermeasures against similar failures. This is quite different though than using counterfactual as an explanation for behavior."

Synthesis/extrapolation: the quote is from the practitioner source, but the application to deterministic machine telemetry is inference, not an established finding. Credible-but-unproven direction; the proven core remains "diagnose without solutioning, defer fixes to fw-improve."

Sources: https://www.kitchensoap.com/2013/10/29/counterfactuals-knight-capital/

## Part 4 — The agentic-loop failure taxonomy and detection from telemetry

### A large share of agent-loop failures are structural/orchestration defects, not model capability gaps `[high, 3-0]`

MAST is exactly what a loop audit (vs a code review) exists to catch: it clusters 14 failure modes into three categories — system-design, inter-agent misalignment, task verification — and concludes that better base models alone will not fix them. The "task verification" category maps onto grader-disagreement and wasted-run findings.

> "14 unique modes, clustered into 3 categories: (i) system design issues, (ii) inter-agent misalignment, and (iii) task verification"

Peer-reviewed (NeurIPS 2025 Datasets & Benchmarks; UC Berkeley). Validates the thesis that loop friction is distinct from agent mistakes and must be separated; verified verbatim across paper and official repo (κ=0.88 over 1600+ traces).

Sources: https://arxiv.org/abs/2503.13657 , https://github.com/multi-agent-systems-failure-taxonomy/MAST

### Agent failures resolve to a small named taxonomy with a root-cause step that cascades `[high, 2-1]`

AgentErrorTaxonomy uses five categories (memory, reflection, planning, action, system-level operations) and frames failure as one root error propagating through later decisions — a useful diagnostic vocabulary for fw-retro.

> "a single root-cause error propagates through subsequent decisions, leading to task failure"

Non-peer-reviewed arXiv preprint (2025). One adversarial skeptic (hype-check) refuted on grounds that the single-root-cause framing is a by-construction modeling assumption validated only on toy benchmarks (ALFWorld/GAIA/WebShop) and should not be read as a durable truth for software-orchestration retros. ADOPT the named taxonomy as vocabulary; do NOT import its single-root-cause cascade as an analytic premise (it conflicts with the Part 2 multi-contributor finding). The same paper then proposes auto-recovery (AgentDebug) — flywheel deliberately stops before the fix.

Sources: https://arxiv.org/abs/2509.25370

### Retry storms show as a monotonically rising retry-rate curve — but may be symptom, not cause `[high]`

Retry storms are a canonical loop failure detectable from telemetry, and the SRE book warns the signal is easily misread. Direct guidance for how fw-retro should cite retry/run-count telemetry and how cautiously to interpret it.

> "Graphs of retry rates can be an indication of bad retry behavior, but may be confused as a symptom instead of a compounding cause."

Canonical practitioner text (Google SRE book), written for distributed RPC systems. The retry-budget/backoff-with-jitter *mitigations* are fw-improve material — fw-retro cites the rising retry curve as evidence and stops.

Sources: https://sre.google/sre-book/addressing-cascading-failures/

### Validation flaps / grader disagreements are common and must NOT be silently discarded `[high]`

Flaky failures can mask real regressions. Google reports flaky tests cause 4.56% of daily test failures; an independent Chromium study found that ignoring all flaky-test failures would have missed 76% of true regression faults. A grader that flips pass/fail across re-runs on identical inputs is a loop-friction finding, not an agent mistake.

> "they had 1.6M test failures on average each day, and 73K of them (4.56%) were caused by flaky tests"

Google data is a vendor/internal report but widely cited; the 76% figure is from a peer-reviewed CI study. Implication: fw-retro flags grader flaps rather than treating either verdict as authoritative.

Sources: https://research.google/pubs/de-flake-your-tests-automatically-locating-root-causes-of-flaky-tests-in-code-at-google/ , https://arxiv.org/pdf/2401.15788

### Reward-hacking / shortcut behavior is detectable from instrumentation, not self-report `[medium]`

The Reward Hacking Benchmark defines six exploit categories and detects them by logging protected-file writes, task-adjacent metadata reads, and downstream artifacts missing required upstream computation. A passing grader plus telemetry showing the grader script or evaluation paths were modified is a re-verifiable reward-hacking finding fw-retro should surface.

> "writes outside designated output directories (tampering), and missing upstream dependencies for downstream artifacts (sequence manipulation)"

Non-peer-reviewed benchmark preprint. Reinforces flywheel's "agent-reported status is untrusted" invariant.

Sources: https://arxiv.org/html/2605.02964v1

### Long-horizon / context exhaustion compounds tiny per-step errors and self-conditions `[medium]`

Token-budget growth across retries plus repeated near-identical failing attempts is evidence of a context-exhaustion squeeze, distinct from a one-shot agent mistake. Per-step error rates compound multiplicatively, and seeing past errors makes future errors likelier; degradation appears well within the nominal context window.

> "their performance still degrades substantially (13.9%-85%) as input length increases but remains well within the models' claimed lengths"

The self-conditioning finding is a 2025 arXiv preprint (credible but unproven); the context-length degradation range is peer-reviewed (EMNLP 2025 Findings).

Sources: https://arxiv.org/pdf/2509.09677 , https://aclanthology.org/2025.findings-emnlp.1264.pdf

### If fw-retro auto-classifies, the label is untrusted telemetry until re-verified `[medium]`

MAST's LLM-as-a-Judge classifier was validated against expert annotation with high inter-annotator agreement (κ=0.88) — automated classification is feasible but must be anchored to human-checkable evidence, not trusted blind.

> "validated by high inter-annotator agreement (kappa = 0.88)"

Peer-reviewed. If fw-retro uses an LLM to classify run history into failure modes, that classification is untrusted telemetry — every finding must still carry a re-verifiable pointer (run_id, grader receipt, transcript line), never just a model-assigned label.

Sources: https://arxiv.org/abs/2503.13657

## Part 5 — Signal vs noise: systemic defect vs one-off, and the precision constraint

### Most flaky/transient failures are episodic one-offs; only the recurring minority is systemic `[high, 2-1]`

A single failed run is a weak predictor. fw-retro should treat a single failed run as a *candidate* and escalate to a systemic finding only when the same misfire recurs across the stored run set.

> "although overall flakiness rates were low (0-0.4%), flakiness was highly episodic: nearly two-thirds of flaky tests appeared in only one release, while a small subset recurred intermittently or persistently"

Peer-reviewed study but quantum-software (Qiskit) domain. One skeptic (source-reliability) refuted the *direction*: the cited paper's body reports recurrence as the *majority* (83.87% reappeared across releases), inverting the "two-thirds one-off" framing; the source statistics are internally contested. SURVIVING TAKEAWAY (do build on this): the recurrence/episodic split is real and the candidate-vs-systemic escalation rule is sound; do NOT cite the specific two-thirds figure as settled — the quoted percentage is disputed within its own source.

Sources: https://arxiv.org/html/2512.18088 , https://arxiv.org/abs/2512.18088

### Base-rate fallacy makes PRECISION the binding constraint for a rare-event auditor `[high, 2-1]`

When true defects are rare, even a low false-positive rate yields mostly false findings; a noisy audit that flags every retry trains operators to dismiss it. Precision (fraction of findings that are real) is the design constraint.

> "in order to achieve substantial values of the Bayesian detection rate, P(Intrusion|Alarm), we have to achieve - a perhaps unattainably low - false alarm rate"

Canonical peer-reviewed paper (Axelsson, ACM CCS 1999). One skeptic (hype-check) refuted the absolutist framing: Axelsson conditions the effect on an extreme base rate (~2 in 1,000,000) and the "precision NOT coverage" dichotomy is a false binary — the Bayes formula depends jointly on detection rate and false-alarm rate. USE the precision-pressure principle and the alert-fatigue mechanism (well-supported); do NOT assert that retry-defects are rare enough to make coverage irrelevant — that base-rate premise is unestablished for flywheel and must not be smuggled in.

Sources: https://dl.acm.org/doi/10.1145/319709.319710 , https://www.researchgate.net/publication/2422569_The_Base-Rate_Fallacy_and_its_Implications_for_the_Difficulty_of_Intrusion_Detection

### Define audit triggers by severity threshold BEFORE the run; skip clean tasks by policy `[high, 2-1]`

Gating audits on pre-defined triggers (downtime past a threshold, data loss, rollback intervention, monitoring failure) is direct authority for fw-retro's "skip clean tasks" and "un-run scope → short note" rules.

> "The postmortem process does present an inherent cost in terms of time or effort, so we are deliberate in choosing when to write one."

Canonical practitioner text (Google SRE book). One skeptic (source-reliability) refuted the "restricted to / skip when not-X" reading: SRE frames triggers as an *inclusion floor* ("expected after any significant undesirable event," "any stakeholder may request a postmortem"), not a gating ceiling. ADOPT the cost-justified deliberate-triggering logic (verified verbatim, 2-0 on correctness and hype-check); do NOT claim SRE *forbids* auditing outside the trigger list. For fw-retro the practical rule stands — skip clean runs, gate on loop misfires — but framed as "audit when friction is present" rather than "auditing anything else is prohibited."

Sources: https://sre.google/sre-book/postmortem-culture/

### Find systemic TYPES by aggregating across runs with a consistent taxonomy `[high]`

Google derives systemic root-cause types (e.g. "immature change deployment planning") by aggregating thousands of postmortems against a consistent template, not by deep-reading one incident. fw-retro should classify each misfire into a small fixed taxonomy and report the category distribution across the phase — surfacing systemic patterns and preventing over-fitting a narrative to one dramatic run.

> "we use a standard postmortem template that allows us to consistently capture the incident root cause and trigger, which enables trend analysis ... to help us target improvements that address systemic root-cause types"

Canonical practitioner text (Google SRE Workbook); the trend-analysis method generalizes, the specific categories are Google's.

Sources: https://sre.google/workbook/postmortem-analysis/

### Confidence must scale with sample size; thin-n findings are low-confidence by rule `[high]`

Wilson-interval analysis shows rare events demand huge samples for detection confidence; fw-retro almost never has many runs of one phase, so any finding resting on one or two observations must be graded low-confidence, not asserted as systemic.

> "when N=10000 and p̂=10^{-4}, then the 95% Wilson lower bound is L_{0.95}≈1.77×10^{-5} ... n_{0.95,0.95}(0.0001,10000)≈1.69×10^5 repetitions"

Peer-reviewed; the exact budgets are illustrative — the load-bearing takeaway is the confidence-vs-sample-size relationship, not the numbers.

### The "rule of three": a recurring pattern is a hypothesis to investigate, not proof of cause `[medium]`

The practitioner heuristic (one accident, two coincidence, three a pattern) maps onto fw-retro's stop-at-diagnosis mandate: a recurring pattern is a strong signal worth investigating, not a conclusion. fw-retro should phrase recurring findings as evidence-cited hypotheses and leave causation/fix to fw-improve.

> "correlation is certainly not cause, and neither is coincidence ... even three correlated occurrences don't automatically prove root cause - they're a strong signal worth investigating, not a conclusion in themselves"

Practitioner folklore / blog sources, not peer-reviewed; useful as an articulable threshold heuristic, not a statistical rule.

Sources: https://alanweiss.com/once-an-accident-twice-a-coincidence-three-times-a-pattern/ , https://rothmanshore.com/2010/11/23/applying-once-is-happenstance-twice-is-coincidence-three-times-is-enemy-action-to-troubleshooting/

### A retry/rerun is a diagnostic signal, not noise — but it appears in only a small fraction of runs `[high]`

Across 1,960 OSS Java projects only 3.2% of builds were rerun, yet 67.73% of those reruns flipped outcome without code change. A rerun genuinely indicates loop friction worth a finding, but an audit that assumes friction everywhere will mostly cry wolf.

> "3.2% of builds are rerun, and 67.73% of these rerun builds exhibit flaky behavior, affecting 1,055 (51.28%) of the projects"

Recent empirical study (2026 arXiv preprint — not yet shown peer-reviewed); large sample, OSS Java only.

Sources: https://arxiv.org/abs/2602.02307 , https://arxiv.org/pdf/2602.02307

### DIVERGENCE: fw-retro cannot rerun history; it infers recurrence from the stored run set `[medium]`

Standard flaky-test detection confirms signal-vs-noise by re-running (≈100 reruns to catch half of flaky tests) — the single most expensive technique. fw-retro audits history after the fact and cannot re-run the loop, so it must infer recurrence and outcome-variance from the stored run set (multiple run_ids for a scope, grader receipts, audit-stream records), label findings as historical-evidence-based, and accept that a finite, unrepeatable run history caps achievable confidence.

> "it has been shown that on average at least 100 reruns are required to detect just half of all flaky tests within a test suite"

Peer-reviewed empirical results on rerun cost; the divergence recommendation is synthesis for flywheel's read-only-history constraint, not a claim from the sources.

Sources: https://arxiv.org/pdf/2302.09330 , https://link.springer.com/article/10.1007/s10664-023-10307-w

## Part 6 — Failure modes of the auditor itself

### AI-driven auditors confidently hallucinate root causes at high rates `[high]`

A measured failure mode, not a hypothetical: LLM-agent RCA produced hallucinated causes in 26% of correct and up to 49% of incorrect predictions, often fabricating remediation steps that never happened. This is the central reason every fw-retro finding must cite a re-verifiable pointer and stop at diagnosis — an unverifiable finding is statistically likely to be invented.

> "49% (29/59) of the RB (k=10)'s incorrect predictions are hallucinations, dropping to 18% (11/59) for CoT and 6%(4/63) for ReAct."

arXiv paper (Microsoft, ICSE-adjacent); directly on-point since fw-retro is itself an LLM auditing a run and subject to the same hallucination rates.

Sources: https://arxiv.org/html/2403.04123v1

### LLM auditors fabricate resolution actions that never occurred; structural grounding is the only fix `[high]`

LLM root-cause output frequently invents plausible-but-wrong resolutions and generic explanations; human verification is required to build trust. fw-retro's mitigation must be structural — ground every claim in a re-runnable pointer so a human can falsify it, rather than trusting the auditor's narrative.

> "Hallucinations...commonly manifests as predictions that suggest the execution of post-hoc resolutions actions (e.g. the incident was resolved by restarting the affected cluster) that did not take place."

arXiv research; the fabricated-remediation pattern is the AI analogue of fw-retro inventing a fix or a cause it never observed in the store.

Sources: https://arxiv.org/html/2403.04123v1

### "Retro theater" — padding with vague, unowned findings — is the dominant real-world retro failure `[medium]`

Few high-signal, evidence-bound findings beat a long report. The proven fixes are radical scope-limiting and starting from prior evidence; un-run scopes get a one-line note, not an invented section.

> "Probably the single biggest cause of ineffective retrospectives is the lack of clear action items to come out of the meeting."

Practitioner consensus / non-peer-reviewed blogs. CAVEAT: this literature pushes toward *action items*, which fw-retro must NOT produce (fixes are fw-improve) — borrow the anti-padding lesson, reject the action-item mandate.

Sources: https://www.mountaingoatsoftware.com/blog/overcoming-four-common-problems-with-retrospectives , https://www.scrum.org/resources/blog/21-sprint-retrospective-anti-patterns

### Findings must be high-signal or they become noise that breeds fatigue `[medium]`

SRE designs aggressively against low-signal output (e.g. not paging on single-machine failures because the data is "too noisy to be actionable"). fw-retro should apply the same signal-to-noise discipline: a finding that cannot be tied to a concrete loop misfire with evidence is noise and should be dropped, not reported to look thorough.

> "All alerts should be immediately actionable... alerts that cannot be acted upon by an on-caller generate noise."

Authoritative practitioner text (SRE). The alerting frame is about paging humans; transfer the high-signal/anti-noise principle, not the literal "actionable" requirement, since fw-retro stops at diagnosis.

Sources: https://sre.google/workbook/on-call/ , https://sre.google/sre-book/practical-alerting/

### Absence of evidence is not evidence of absence: an un-run scope is inconclusive, not a clean bill of health `[medium]`

fw-retro should distinguish "verified clean" (loop ran, evidence shows no friction) from "not observed" (loop never ran this scope) and emit only a short note for the latter — never an invented report and never a false all-clear.

> "Failing to reject the null hypothesis doesn't imply that it's true. Rather, such a result is inconclusive; there's not enough evidence to claim there is an association."

Reputable science-communication + peer-reviewed methodology source; an epistemics analogy, not a software-engineering finding — supports the "short note for un-run scope" rule conceptually.

Sources: https://theconversation.com/absence-of-evidence-is-not-evidence-of-absence-and-that-affects-what-scientific-journals-choose-to-publish-264854 , https://pmc.ncbi.nlm.nih.gov/articles/PMC11090505/

## Part 7 — Supporting evidence-forensics findings

### Distributed tracing is the mechanical backbone for re-verifiable pointers `[high]`

fw-retro's citation chain (run_id → audit record → grader receipt → transcript line) is exactly a trace/span hierarchy. The lesson: a citation is durable only if the identifier is stable and recorded, not reconstructed after the fact.

> "Context Propagation is the core concept that enables Distributed Tracing. With Context Propagation, Spans can be correlated with each other and assembled into a trace, regardless of where Spans are generated."

Standards-body / canonical source (OpenTelemetry, W3C Trace Context). Proven practice.

Sources: https://opentelemetry.io/docs/concepts/context-propagation/ , https://opentelemetry.io/docs/concepts/signals/traces/

### Decisions/transitions should be recorded as first-class events; they are the most diagnostic citations `[medium]`

Logs/metrics/traces show what happened but not why a branch was taken. For fw-retro the most diagnostic citations are often decision/transition records (a retry triggered, a phase rebased, a grader verdict) rather than raw model output.

> "These high-level decisions often shape control flow, and must be recorded independently of LLM or tool events so that replay reproduces the control graph correctly."

Credible-but-unproven practitioner direction (vendor/independent blogs, not peer-reviewed). Treat "record decisions as first-class events" as a design heuristic, not an established standard.

Sources: https://www.sakurasky.com/blog/missing-primitives-for-trustworthy-ai-part-8/ , https://dev.to/sreeni5018/debugging-non-deterministic-llm-agents-implementing-checkpoint-based-state-replay-with-langgraph-5171

### Digital-forensics standards define the trust properties fw-retro findings need `[medium]`

ISO/IEC 27037 codifies auditability, repeatability, reproducibility, and chain of custody; re-verifiability means an independent reviewer can follow the cited pointer to the same artifact and reach the same conclusion — adversarial re-verification, not authorial assertion.

> "every operation on digital evidence must be documented, traceable, and independently verifiable to maintain its probative value in any jurisdiction. Reproducibility specifically means that results must remain consistent even in different testing environments."

The standard is authoritative, but the supporting URLs are practitioner summaries, not the standard text. CAVEAT: legal chain-of-custody is heavier than fw-retro needs; borrow the principles (independent re-verifiability), not the courtroom machinery.

Sources: https://truescreen.io/articles/digital-chain-of-custody-guide/ , https://pmc.ncbi.nlm.nih.gov/articles/PMC12431127/

### temperature=0 does NOT make agent runs reproducible; absence of an artifact is itself a finding `[medium]`

Infrastructure-level effects (batching, prefix caching, floating-point ordering) plus context-assembly variance produce divergent trajectories from identical inputs. Diagnosis lives in the recorded audit stream; fw-retro must never assume a clean-looking re-run proves or refutes a finding, and a missing recorded artifact is itself a finding (the loop's observability gap), not a prompt to reconstruct what probably happened.

> "controlled studies have found accuracy variance up to 15% and best-vs-worst outcome gaps up to 70% even at temperature=0. The non-determinism doesn't live in the sampling logic. It lives in infrastructure: continuous batching, prefix caching, and floating-point ordering across distributed hardware."

Non-peer-reviewed practitioner blogs citing "controlled studies" without direct links; the 15%/70% figures are indicative, not authoritative. The qualitative claim (temp=0 ≠ reproducible) is well-corroborated across sources.

Sources: https://www.flowhunt.io/blog/defeating-non-determinism-in-llms/ , https://dev.to/sreeni5018/debugging-non-deterministic-llm-agents-implementing-checkpoint-based-state-replay-with-langgraph-5171

## Refuted claims (do NOT build on these)

These claims were killed in adversarial verification (≥2 of 3 skeptics refuted). Do not cite them as load-bearing support.

- **KILLED [2-1] — "Google SRE's postmortem rubric makes linked re-verifiable evidence *the formal core standard / the bar.*"** Two skeptics (source-reliability, hype-check) refuted: the quote is genuine and from a primary source, but linked data is *one illustrative bullet* under the "Depth" dimension of a narrative good-vs-bad example, not a formal scored rubric or a mandatory no-exception standard. The verbatim quote and the underlying *principle* (cite evidence) remain usable and are carried in Part 1's surviving 3-0 finding; what is refuted is the inflation to "formal rubric / the bar." Source: https://sre.google/workbook/postmortem-culture/

- **KILLED [3-0] — "Peer-reviewed forensics work proves agentic systems are non-reproducible *by construction.*"** All three skeptics refuted. The cited paper (Gruber & Hilgert, "Foundations for Agentic AI Investigations from the Forensic Analysis of OpenClaw," arXiv:2604.05589) is a real and substantively useful preprint, but it is an unrefereed arXiv v1 (posted 2026-04-07), NOT "peer-reviewed" as the claim twice asserted, and it says reproducibility is "complicated" / they "could not make OpenClaw behave completely identically" — not the absolute "non-reproducible by construction" the claim invented. USE the weaker, well-corroborated version instead (see Part 7 temperature=0 finding): agent runs are *not reliably reproducible*, so diagnose from recorded artifacts; cite the preprint as a single-case-study preprint, never as peer-reviewed proof. Source: https://arxiv.org/abs/2604.05589

## Caveats

- **Time-sensitivity.** The agentic-failure literature (MAST, AgentErrorTaxonomy, Reward Hacking Benchmark, self-conditioning, context-degradation) is 2025-2026 and moving fast; taxonomies and reproducibility findings may be superseded within months. Re-validate before treating any single preprint as settled.
- **Preprint risk.** Several load-bearing-adjacent sources are non-peer-reviewed arXiv preprints (2509.25370, 2605.02964v1, 2509.09677, 2602.02307, 2604.05589). One (2604.05589) was explicitly killed for being mislabeled peer-reviewed. Treat all arXiv-only sources as credible-but-unproven and flag inline, as done above. AgentErrorTaxonomy's single-root-cause framing in particular conflicts with the multi-contributor finding and is adopted only as vocabulary.
- **Vendor self-reporting.** The Google SRE corpus (postmortem culture, retry curves, flaky-test rates, alerting) is vendor-authored and treated as the de facto industry standard, but it is not peer-reviewed and reflects Google's distributed-systems context, not coding-agent loops. The flaky-test 4.56% figure is an internal report. Transfer principles, not specifics.
- **Domain transfer.** The flakiness recurrence and rerun-cost numbers come from quantum-software (Qiskit) and OSS-Java domains; the qualitative split (episodic majority + persistent minority; rerun = signal) generalizes, the exact percentages do not — and the Qiskit study's own one-off-vs-recurrence percentages are internally contested (see Part 5).
- **Human-to-agent analogy.** The blameless-postmortem and cognitive-bias literature targets human cognition and psychological safety. fw-retro retains the analytic discipline (cite evidence, don't stop at "error," judge by contemporaneous state) and drops the human-safety ritual. The agent is a system component to diagnose, not a person to exonerate.
- **The action-item schism is unresolved.** The deferral school (Allspaw/Howie) that fw-retro follows is the advanced minority; the coupled school (Google SRE/Atlassian, "every finding → tracked action item") is mainstream. fw-retro's stage split is defensible but non-universal — present it as a deliberate choice, not a settled best practice.

## fw-retro routing (reasoned design recommendation — NOT independently verified)

How to scope the audit
- Audit the LOOP's execution for one phase, never the shipped code. The unit of diagnosis is loop behavior (crashes, retries, validation flaps, budget squeezes, grader disagreements, wasted runs, ambiguous specs that confused the agent), not whether the feature works.
- Read state and telemetry only via the CLI (`flywheel history`, `flywheel show`, `flywheel audit`). Never query a backend directly, never re-execute the loop, never assume behavior. Diagnosis rests entirely on durably recorded artifacts captured at execution time.
- Gate the audit on friction, not on outcome. Skip tasks whose loop ran clean (no retries, no flaps, no budget pressure, one grader pass), regardless of whether the task passed or failed. A FAILED task with a clean loop is an agent-mistake candidate, not a loop finding; a PASSED task with three retries and a grader flip is a loop finding. (Outcome-bias discipline.)
- Treat the audit-stream's completeness as the ceiling on what you can say. If the loop never ran a scope, emit a one-line "not observed (loop did not run this scope)" note — never an invented report and never a false all-clear. Distinguish "verified clean (ran, no friction in evidence)" from "not observed (never ran)."

What evidence each finding must cite
- Every finding cites at least one re-verifiable pointer: a run_id, an audit-stream record, a grader receipt, or a transcript line — each re-openable via the CLI. A finding without a re-runnable pointer is inadmissible and is dropped, not softened. (This is the structural defense against the measured 26-49% LLM RCA hallucination rate.)
- Prefer decision/transition records (retry triggered, phase rebased, grader verdict, lease lost) over raw model output — they explain control flow, which is what a misfire diagnosis needs.
- Agent-reported status is untrusted narrative. A confession ("this is on me") or a self-label is the first story, never the conclusion; it may be quoted as a transcript pointer but never stands as a finding's evidence.
- If fw-retro uses an LLM to classify run history into buckets, the classification is itself telemetry — each bucketed finding must still carry an independent re-verifiable pointer, never just the model-assigned label.

Keeping diagnosis separate from remediation
- Stop at diagnosis. Do NOT propose, rank, or sketch fixes — that is fw-improve.
- When a fix-shaped idea surfaces during reconstruction, capture it as a pointer/observation and defer it; never resolve it in-stage. (Collect-but-defer.)
- Phrase findings as what the loop *did* and what state it *observed*, citing pointers — never as counterfactual reproaches ("the loop should have retried," "the agent should have known X"). Counterfactuals are legitimate only as forward prevention handed to fw-improve.

The loop-failure buckets to scan for, and how to detect each (from telemetry)
- Crash / hard abort — non-graceful loop termination in the audit stream; cite the terminal record.
- Retry exhaustion / retry storm — multiple run_ids for one scope, a rising retry count. Cite the run sequence; flag that the curve may be symptom, not cause.
- Validation flap / grader disagreement — a grader flipping pass/fail across re-runs on identical inputs, or two graders disagreeing. Cite both grader receipts. Never treat either verdict as authoritative; this is loop friction, not an agent mistake.
- Budget / context squeeze — token-budget growth across retries plus repeated near-identical failing attempts. Cite the per-attempt budget records; this is distinct from a one-shot agent mistake.
- Wasted run — a run that produced no admissible state change or repeated prior work. Cite the run_id and the unchanged-state evidence.
- Ambiguous-spec confusion — transcript evidence of the agent oscillating or asking for clarification the spec should have answered. Cite transcript lines.
- Reward-hacking / silent-wrong-result — a passing grader plus telemetry showing protected-path writes, grader/eval-path modification, or a downstream artifact missing its upstream computation. Cite the write record and the grader receipt together. (Untrusted-status invariant in action.)
- Missing observability — a transition that should have produced an artifact but did not. The absence is itself the finding (loop observability gap); cite the gap, do not reconstruct the missing event.

Systemic vs one-off, loop-friction vs agent-mistake
- A single failed run is a *candidate*, not a finding. Escalate to a systemic finding only when the same misfire recurs across the stored run set for the phase. Classify each misfire into the fixed bucket taxonomy above and report the category distribution across the phase — this surfaces systemic types and prevents over-fitting one narrative to one dramatic run.
- Treat a recurring pattern as an evidence-cited *hypothesis worth investigating*, not proof of cause. Causation and fixes go to fw-improve.
- Refuse to crown a single root cause. Where multiple conditions co-occurred (flapping grader + tight budget + ambiguous spec), report them as a set of jointly-sufficient contributing factors, each separately cited.
- Loop-friction vs agent-mistake test: did the loop (system) supply incomplete/incorrect information, miss a guardrail, squeeze the budget, or mis-grade? That is reportable loop friction. Did the agent make a coding error inside a correctly-functioning loop? That is a separate, lower-priority category — note it without moralizing, and do not let it stand in for a systemic gap.
- Grade confidence by sample size. One or two observations → low confidence, explicitly labeled. Recurrence across several run_ids → higher confidence. Never assert "systemic" on thin n. Because fw-retro cannot re-run history, it infers recurrence from outcome-variance across the stored run set and labels every finding historical-evidence-based.

When to write a short "nothing to audit" note
- Loop ran, evidence shows no friction → one line: "clean (loop ran, no friction in evidence)."
- Loop never ran the scope → one line: "not observed (loop did not run this scope)." Never an invented report.
- Whole phase clean → a short top-level note, not a padded report.

What to keep lean
- Few high-signal, evidence-bound findings beat a long report. Precision is the binding constraint: a finding that cannot be tied to a concrete loop misfire with a pointer is noise and is dropped, not reported to look thorough. (Avoid retro theater.)
- No action items, no fix proposals, no single-root-cause narrative, no counterfactual reproaches, no un-cited claims, no padding for un-run scopes.
- Output shape: per finding — bucket, confidence (+ recurrence count where applicable), the contributing factors, and the re-verifiable pointer(s). Plus the phase-level bucket distribution. Nothing else.

## Sources (verified primaries)

- https://sre.google/workbook/postmortem-culture/
- https://sre.google/sre-book/postmortem-culture/
- https://sre.google/workbook/postmortem-analysis/
- https://sre.google/sre-book/addressing-cascading-failures/
- https://sre.google/workbook/on-call/
- https://sre.google/sre-book/practical-alerting/
- https://how.complexsystems.fail/
- https://www.researchgate.net/publication/228797158_How_complex_systems_fail
- https://www.kitchensoap.com/2014/11/14/the-infinite-hows-or-the-dangers-of-the-five-whys/
- https://www.kitchensoap.com/2013/10/29/counterfactuals-knight-capital/
- https://www.kitchensoap.com/2012/02/10/each-necessary-but-only-jointly-sufficient/
- https://surfingcomplexity.blog/2021/08/13/root-cause-of-failure-root-cause-of-success/
- https://www.etsy.com/codeascraft/debriefing-facilitation-guide/
- https://github.com/etsy/DebriefingFacilitationGuide
- https://github.com/etsy/DebriefingFacilitationGuide/blob/master/guide/04-structure-and-preparation.md
- https://codeforamerica.org/news/from-accident-to-investment-how-to-run-better-blameless-postmortems/
- https://howie-guide.pagerduty.com/analyze/
- https://newsletter.pragmaticengineer.com/p/incident-review-best-practices
- https://www.usenix.org/system/files/login/articles/login_spring17_09_lunney.pdf
- https://resilienceroundup.com/issues/reconstructing-human-contributions-to-accidents-the-new-view-on-error-and-performance/
- https://fs.blog/narrative-fallacy/
- https://www.ncbi.nlm.nih.gov/pmc/articles/PMC12372742/
- https://en.wikipedia.org/wiki/Outcome_bias
- https://arxiv.org/abs/2503.13657
- https://github.com/multi-agent-systems-failure-taxonomy/MAST
- https://arxiv.org/abs/2509.25370
- https://arxiv.org/html/2605.02964v1
- https://arxiv.org/pdf/2509.09677
- https://aclanthology.org/2025.findings-emnlp.1264.pdf
- https://research.google/pubs/de-flake-your-tests-automatically-locating-root-causes-of-flaky-tests-in-code-at-google/
- https://arxiv.org/pdf/2401.15788
- https://arxiv.org/html/2512.18088
- https://arxiv.org/abs/2512.18088
- https://arxiv.org/abs/2602.02307
- https://arxiv.org/pdf/2602.02307
- https://arxiv.org/pdf/2302.09330
- https://link.springer.com/article/10.1007/s10664-023-10307-w
- https://dl.acm.org/doi/10.1145/319709.319710
- https://www.researchgate.net/publication/2422569_The_Base-Rate_Fallacy_and_its_Implications_for_the_Difficulty_of_Intrusion_Detection
- https://arxiv.org/html/2403.04123v1
- https://opentelemetry.io/docs/concepts/context-propagation/
- https://opentelemetry.io/docs/concepts/signals/traces/
- https://www.sakurasky.com/blog/missing-primitives-for-trustworthy-ai-part-8/
- https://dev.to/sreeni5018/debugging-non-deterministic-llm-agents-implementing-checkpoint-based-state-replay-with-langgraph-5171
- https://www.flowhunt.io/blog/defeating-non-determinism-in-llms/
- https://truescreen.io/articles/digital-chain-of-custody-guide/
- https://pmc.ncbi.nlm.nih.gov/articles/PMC12431127/
- https://www.mountaingoatsoftware.com/blog/overcoming-four-common-problems-with-retrospectives
- https://www.scrum.org/resources/blog/21-sprint-retrospective-anti-patterns
- https://theconversation.com/absence-of-evidence-is-not-evidence-of-absence-and-that-affects-what-scientific-journals-choose-to-publish-264854
- https://pmc.ncbi.nlm.nih.gov/articles/PMC11090505/
