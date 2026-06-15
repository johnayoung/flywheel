# fw-improve (Improve/propose stage) — Skill Research Brief (mid-2026)

> Point-in-time research artifact, not an authoritative spec. Feeds the fw-improve skill rewrite.
> Generated 2026-06-15 via the skill-research workflow (run wf_b0b061cb-96b): 6 angles, 60 sources,
> 46 claims extracted, 12 adversarially verified (9 killed). Facts are verified + cited;
> the fw-improve routing at the end is reasoned design recommendation, not independently verified.

## Headline

The single most defensible borrow from the incident-review and product-prioritization canon is the **verifiable end state**: a well-formed action names a checkable change in the system, not a fix or a behavioral nudge — and the surviving evidence for this is Google's SRE Workbook, which states it almost verbatim. Around that spine, three other claims survived adversarial review intact: rank by **Cost of Delay ÷ job size** (Reinertsen/WSJF), not by how alarming a finding looks; allow an explicit, reasoned **accept/do-not-fix** because perfect reliability is the wrong target; and treat the LLM's own **verbosity/sycophancy reward bias** as a real force that mechanically pushes toward padded proposal lists. Crucially, several attractive-sounding claims were *killed* in verification — most notably "Confidence is a routing signal," the JTBD "jobs are stable" license, the OKR "exact rule" analogy, and the "30% completion rate" statistic — so fw-improve must be built on the survivors, not the slogans. The net design is lean by construction: borrow well-formedness, clustering, and economic ranking; reject owner-assignment, deadlines, composite scores, and any fabricated number.

## Part 1 — What makes a proposal well-formed

### A well-formed action names a verifiable end state, not a vague verb or a behavioral fix `[high, 3-0]`

The surviving, fully-corroborated borrow. Google's SRE Workbook gives action items an owner, a tracking number, a priority, and — the part that transfers cleanly to fw-improve — a *verifiable end state*. Vague verbs ("improve", "make better") and people-changing fixes ("train humans not to run unsafe commands") are explicitly the bad pattern; a good action names a system change with an observable success condition.

> "All action items have both an owner and a tracking number ... All action items are assigned a priority level ... The action items have a verifiable end state (e.g., 'Add an alert when more than X% of our machines have been taken away from us')."

This is proven practice. **Inline caveat:** the source frames these as characteristics of a good postmortem, not as four "mandatory properties," and owner/tracking-number/priority are *completion-ownership* concerns fw-improve must not adopt. Only the verifiable-end-state criterion transfers. Note also (this is what killed a sibling claim, see Refuted) that the SRE canonical *good* example — "Add an alert when more than X% ..." — is itself an implementation that merely has a checkable completion state. So fw-improve's stronger rule "outcome, *never* an implementation" is a flywheel design choice, not something Google's text licenses.

Sources: https://sre.google/workbook/postmortem-culture/ ; https://static.googleusercontent.com/media/sre.google/en//static/pdf/login_spring17_09_lunney.pdf

### "Too many action items are as bad as none"; cluster into few root-cause-addressing actions `[high]`

The canon explicitly favors quality over quantity and warns against enumerating symptoms. Lueder/Beyer's Google taxonomy sorts actions into buckets (Prevent / Mitigate / Detect / Fix / Improve / Investigate) and advises securing at least one root-cause-addressing action rather than many shallow ones; GitLab operationalizes a four-bucket variant as labels.

> "Too many action items are as bad as none at all ... 'Mitigate future incidents' and 'Prevent future incidents' are your most likely source of actions that address the root cause. Be sure to get at least one of these."

Proven practice. Directly underwrites "cluster related findings into one proposal; do not enumerate symptoms." **Caveat:** the "at least one of these" framing is a *floor* that does not apply to fw-improve, where zero proposals is a correct result for a clean retro.

Sources: https://www.usenix.org/conference/srecon17americas/program/presentation/lueder ; https://www.atlassian.com/incident-management/handbook/postmortems ; https://docs.gitlab.com/operations/incident_management/

### SMART's "A" originally meant Assignable; only Specific + Measurable transfer `[high]`

SMART originated with George T. Doran (*Management Review*, Nov 1981); its original "A" meant **Assignable** — "specify who will do it" — with M as a progress indicator and T as "specify when."

> "Specific: target a specific area for improvement. Measurable: quantify, or at least suggest, an indicator of progress. Assignable: specify who will do it. Realistic: state what results can realistically be achieved given available resources. Time-related: specify when the result can be achieved."

Primary origin (Doran 1981) confirmed; Wikipedia/ProjectSmart are accurate secondaries. **Caveat for fw-improve:** of SMART's five, only Specific and Measurable apply to a proposal. Assignable, Realistic (resource estimate), and Time-related are operator/downstream concerns and must be left out of the proposal — this is a deliberate divergence from the school that assigns an owner and deadline at proposal time.

Sources: https://en.wikipedia.org/wiki/SMART_criteria ; https://www.projectsmart.co.uk/smart-goals/brief-history-of-smart-goals.php

## Part 2 — How to rank without faking a number

### Rank by Cost of Delay ÷ job size, not by raw severity `[high, 3-0]`

Reinertsen's WSJF establishes that the correct ranking input is Cost of Delay (value lost per unit of delay) divided by job size — not how alarming a finding looks. The shortest-job tiebreak (equal cost-of-delay → do the cheaper fix first) is the literal arithmetic and traces to classic SPT/WSPT scheduling optimality. This is the formal basis for ranking by "cost-of-inaction vs fix-cost."

> "If you only quantify one thing, quantify the Cost of Delay."

All three verifiers confirmed the formula, the attribution to Reinertsen's *Principles of Product Development Flow* (2009), and the tiebreak. **Inline caveat:** WSJF/SAFe is a widely adopted practitioner framework, not peer-reviewed; the verbatim quote lives on SAFe's commercial page (the adapter), while the underlying queueing economics are Reinertsen's. Optimality is rigorous only for a single constrained flow and degrades to an approximation under parallel capacity.

Sources: https://framework.scaledagile.com/wsjf ; https://blackswanfarming.com/wsjf-weighted-shortest-job-first/ ; https://en.wikipedia.org/wiki/Cost_of_delay

### Decompose the gut-call into named factors; do not multiply them into one score `[high]`

The durable, transferable idea behind RICE is *decomposition*, not arithmetic: split one fuzzy "impact" estimate into named factors a team can each put a number on with evidence — or admit it cannot.

> "Sean built it on Intercom's growth team to split that single fuzzy estimate into four smaller ones - reach, impact, confidence, effort - each of which a team can put a number on with some data behind it, or admit they can't."

Adopt the decomposition (recurrence, blast radius, cost-of-inaction, fix-cost as separately-argued factors); reject the composite score. Note: the *Confidence-as-routing-signal* extension of this idea was killed in verification — see Refuted.

Sources: https://www.intercom.com/blog/rice-simple-prioritization-for-product-managers/ ; https://www.productplan.com/glossary/rice-scoring-model

### Rough estimates are economically sufficient: flat-bottomed optima `[high]`

Reinertsen's Imperfection Principle (E7) and U-curve Principle (E6) license ranking with rough estimates: economic trade-offs have flat-bottomed optima, so a defensible ordering with rationale beats chasing precise scores.

> "The U-curve Principle: Important trade-offs are likely to have U-curve optimizations. E7: The Imperfection Principle: Even imperfect answers improve decision-making."

**Inline caveat:** the popular "10-20% off the optimum costs ~2-3%" gloss is a practitioner illustration of the U-curve, not a measured constant — treat as directional. This is the antidote to over-investing in scoring machinery.

Sources: https://agility-at-scale.com/principles/product-economics/ ; https://www.amazon.com/Principles-Product-Development-Flow-Generation/dp/1935401009

### "Score theater": multiplied gut numbers look rigorous but are gameable and brittle `[medium]`

Practitioners name the exact failure mode fw-improve must avoid: composite scores produce math that looks rigorous but is manipulable to justify a predetermined answer, and a small confidence shift can flip the top of a ranking.

> "a RICE score with '80%' confidence based on vibes is fiction ... 'score theater' - beautiful spreadsheets, poor decisions."

**Inline caveat:** sources are practitioner blogs (non-peer-reviewed), some vendor-affiliated; the critique is consistent across many independent ones but each individually is opinion. This is the core argument for emitting **no composite numeric score** — a ranked list with a written leverage rationale per item instead.

Sources: https://www.rock.so/blog/rice-scoring ; https://dovetail.com/product-development/rice-scoring-model/

### Toil's recurrence/blast-radius test gives domain-fit, checkable ranking signals `[high]`

Google SRE's toil definition supplies concrete, retro-citable tests far better than a 1-10 "impact" guess: does the loop stay in the same state after the manual intervention; does the work scale linearly with run count/repo growth; is it interrupt-driven and reactive.

> "If your service remains in the same state after you have finished a task, the task was probably toil. ... Work that scales up linearly with service size, traffic volume, or user count."

**Inline caveat:** recognized canonical practitioner text (Google SRE book), not peer-reviewed; widely treated as authoritative.

Sources: https://sre.google/sre-book/eliminating-toil/

## Part 3 — Outcomes, not implementations

### Outcome = a change in behavior, severed from "making stuff" `[high]`

Seiden/Gothelf (Lean UX) give the sharpest operational definition and explicitly cut it from implementation.

> "An outcome is a change in human behavior that drives business results. Outcomes have nothing to do with making stuff."

A proposal should read like this: assert the changed behavior of the loop, not the artifact that produces it. **Inline caveat:** quote sourced from book summaries / a Mind the Product interview (book paywalled); the definition is verbatim-standard and widely cited, low misquote risk.

Sources: https://medium.com/design-bootcamp/outcomes-over-outputs-key-points-of-joshua-seidens-book-reading-e8cf5e2b7a9d ; https://www.mindtheproduct.com/outcomes-over-outputs-josh-seiden-on-the-product-experience/

### Output is done when shipped; an outcome isn't done until it has the intended impact `[high]`

Fowler frames the split as a completion-criteria difference and rebuts the "outcomes are hard to measure" objection.

> "even if it's tricky to determine how a team's work affects outcome, the fact that people are instead thinking about outcomes and how to improve them is worth more than any effort to compare teams' proficiency in producing the wrong things."

**Inline caveat:** canonical practitioner (Fowler bliki), opinion piece, not peer-reviewed; he *concedes* outcome measurement is genuinely harder than output measurement. The operational consequence for fw-improve: tie each outcome to the retro's concrete evidence so it stays checkable. (Note: the OKR and JTBD framings of this same idea were *killed* — see Refuted. The principle survives; two of its canonical "proofs" do not.)

Sources: https://www.martinfowler.com/bliki/OutcomeOverOutput.html

### Some work is legitimately output-shaped: the fw-plan exception `[medium]`

Pure outcome-orthodoxy is not universal. Well-understood, repeatable, low-novelty work (bug fixes, maintenance, compliance) is better framed as a known output/solution.

> "While focusing on outcomes can be immensely beneficial, it's not always the most appropriate method. There are situations where concentrating on outputs is more fitting, such as routine maintenance tasks, bug fixes, or other well-defined, repeatable tasks."

**Inline caveat:** LeadDev is a credible practitioner outlet, not peer-reviewed; SVPG's "outcomes-are-hard"/high-integrity-commitment exception corroborates the boundary. This justifies the **two-tier handoff** (fw-spec when discovery is needed; fw-plan when the fix is known and may be more solution-shaped while still naming the outcome it restores) rather than a single dogmatic "outcomes only" rule.

Sources: https://leaddev.com/velocity/focus-outcomes-over-outputs ; https://www.svpg.com/outcomes-are-hard/

### Outcome statements are gameable; guard them with cited evidence `[medium]`

Even a leading outcome advocate (Torres) flags the failure mode.

> "Outputs disguised as outcomes. Traction metrics masquerading as product outcomes. Sentiment metrics without direction."

**Inline caveat:** Producttalk is credible practitioner work (Torres), not peer-reviewed. The anti-gaming countermeasure: require each proposed outcome to trace to a cited retro finding (run_id / audit record / grader receipt / transcript line). An unverifiable outcome is as bad as an enumerated symptom; both should be rejected.

Sources: https://www.producttalk.org/defining-product-outcomes/

## Part 4 — When to accept and do nothing

### Perfect reliability is the wrong target; do not fix below tolerance `[high, 3-0]`

The economic license for accept/do-not-fix. Past a point, more reliability is *worse* for users (cost, slowed delivery), and each reliability increment may cost ~100x more.

> "increasing reliability is worse for a service (and its users) rather than better!"

All three verifiers confirmed this against the primary Google SRE book (CC-licensed, canonical, not vendor marketing). **Inline caveat:** the source hedges "*may* cost 100x more"; some practitioners cite 10x/nine. The core principle — there is a tolerance below which fixing is net-negative — is durable and survived clean.

Sources: https://sre.google/sre-book/embracing-risk/

### Toil is acceptable in small doses; accept when fix-cost exceeds recurring cost `[high]`

> "toil isn't always and invariably bad, it's fine in small doses."

The SRE Workbook also supplies the accept-vs-fix gate: weigh the cost of responding to the toil versus not, confirm time saved is at least proportional to time invested, and "don't try to design the perfect system ... automate a few high-priority items first." This directly backs "accept — do not fix" and "few or zero proposals is correct." Anchor acceptance to recurrence and blast radius.

Sources: https://sre.google/workbook/eliminating-toil/

### A do-not-fix decision must be an explicit, reasoned record `[high]`

Closure should be a deliberate act: the action is done, or it was explicitly deprioritized with a documented rationale. Quiet rotting in a backlog is the invalid state.

> "Either the actions are done, or they've been explicitly deprioritised with a reason. 'We decided not to do this because the risk is low and the effort is high' is a completely valid outcome."

**Inline caveat:** non-peer-reviewed (incident.io vendor blog), but the principle is widely echoed across SRE and incident-review practice. fw-improve should require a *cited reason* for every "accept", mirroring the "with a reason" clause. NASA's residual-risk-acceptance practice ("organizations should formally document residual risk acceptance decisions," NASA Risk Management Handbook) sets the same principle in a higher-stakes domain.

Sources: https://incident.io/blog/why-post-mortem-action-items-die ; https://www.nasa.gov/wp-content/uploads/2023/08/nasa-risk-mgmt-handbook.pdf

### Satisficing: propose above an aspiration bar, then stop `[high]`

Simon's bounded-rationality satisficing — accept the first option that clears a predefined threshold — is the formal argument against enumerating every option.

> "find one that meets or exceeds a predefined threshold, your aspiration level."

Peer-reviewed (Stanford Encyclopedia of Philosophy). For fw-improve: propose only what clears the leverage bar, then stop; do not rank-order the long tail.

Sources: https://plato.stanford.edu/entries/bounded-rationality/

## Part 5 — Failure modes fw-improve must resist

### The LLM itself is biased toward padded, verbose, agreeable lists `[high]`

Because fw-improve is LLM-driven, it inherits a documented reward bias toward verbosity and sycophancy: preference-trained models favor longer, agreeable outputs independent of substance, and policy optimization amplifies this.

> "Reward models... exhibit a tendency to prefer verbose outputs, even when verbosity undermines informativeness or coherence... even a small fraction (less than 1%) of 'biased' training pairs where longer outputs are preferred is sufficient to cause a strong length bias."

**Inline caveat:** peer-reviewed/preprint ML literature (arXiv); the bias is measured at *training time* in reward models — its expression at inference in a skill prompt is a credible-but-not-directly-measured extrapolation. The design consequence: an explicit anti-padding instruction and an affirmative "few or zero proposals is correct" license are *necessary countermeasures*, not nice-to-haves, because the model's default incentive is to over-produce.

Sources: https://arxiv.org/html/2411.15287v1 ; https://arxiv.org/html/2505.12843 ; https://www.emergentmind.com/topics/verbosity-bias-in-preference-models

### The "bad postmortem" is an undifferentiated, equal-priority, vaguely-worded list `[high]`

> "All of the action items have been tagged with an equal priority. There's no way to determine which action to tackle first."

The canonical anti-pattern: equal priority on everything plus vague verbs. **Inline caveat:** vendor-authored (Google SRE), but this is the canonical practitioner reference; the USENIX ;login: article is the peer-recognized print version. fw-improve must rank by leverage and state observable outcomes, never emit a flat list.

Sources: https://sre.google/workbook/postmortem-culture/ ; https://www.usenix.org/system/files/login/articles/login_spring17_09_lunney.pdf

### Hindsight bias and counterfactuals feel analytical but describe an alternate history `[high]`

> "Hindsight biases post-accident assessments of human performance. Knowledge of the outcome makes it seem that events leading to the outcome should have appeared more salient to practitioners at the time than was actually the case."

Counterfactual "if only X" reasoning describes a history that didn't occur and steers remedies toward the one incident and toward blaming a person — the mechanism behind over-fitting a whole program to one incident. **Inline caveat:** Cook's *How Complex Systems Fail* is a canonical position paper, not a peer-reviewed experiment; and counterfactuals remain useful *as forward-looking candidate changes* (Nygård's "parking lot") — the trap is treating them as causes. fw-improve must convert any cited finding into a forward-looking outcome, not a "would-have-prevented" fix. (Note: the stronger "single root cause is fundamentally wrong → therefore always cluster into ONE proposal" claim was *killed* — see Refuted. The descriptive hindsight finding survives; the prescriptive cluster-mandate does not.)

Sources: https://how.complexsystems.fail/ ; https://www.michaelnygard.com/blog/2021/06/counterfactuals-are-not-causality/ ; https://psychsafety.com/counterfactuals/

### Peer-reviewed RCA: investigators settle for weak administrative fixes and convenient causes `[high]`

> "The available evidence points to the endemic tendency of investigators to settle for administrative and perhaps 'weaker' solutions (such as reminders) rather than those that address the latent causes."

Peer-reviewed (Peerally et al., *BMJ Quality & Safety*, 2016; healthcare domain, generalizes). This is the recommendation-theater and blame-driven-fix risk: a proposal must trace to cited evidence and state a system outcome, resisting the convenient or self-serving fix.

Sources: https://pmc.ncbi.nlm.nih.gov/articles/PMC5530340/

### Change introduces new forms of failure; fix-cost includes new blast radius `[medium]`

> "Change introduces new forms of failure... novel technologies intended to eliminate known problems often introduce new pathways to large scale, catastrophic failures."

**Inline caveat:** Cook's canonical text; the application to fix-cost weighting is an inference not stated in the source. Implication: a "fix" is itself a wager with its own blast radius — prefer routing scoped-but-uncertain items to fw-spec (discovery) over fw-plan when the change itself is risky.

Sources: https://how.complexsystems.fail/

## Part 6 — Evidence-to-action traceability

### Traceability must be purposed and cost-effective — the license to propose nothing `[high]`

CoEST's Grand Challenges (Gotel, Cleland-Huang et al.) define traceability quality goals including **purposed** and **cost-effective**: links must serve a defined use and their benefit must exceed their cost.

> "the Grand Challenges centered around the quality goals of traceability... included the need for traceability to be purposed, cost-effective, configurable, trusted, scalable, portable, valued, and ultimately ubiquitous."

Recognized academic source. **Inline caveat:** the verbatim goal list was relayed via search summary of the arXiv/Springer texts; the goals themselves are stable and widely cited. Note the field's *end* goal is ubiquitous/every-requirement traceability — fw-improve must **diverge** from that completeness ideal toward selectivity: a proposal whose fix-cost exceeds its leverage is traceability waste by the field's own standard, which is the academic license for "do not invent work to fill the doc."

Sources: https://arxiv.org/abs/1710.03129 ; https://link.springer.com/chapter/10.1007/978-1-4471-2239-5_16

### Record rejected alternatives so a null result is itself auditable `[high]`

FDA CAPA practice requires the investigation record to document not just the final root cause but the alternatives evaluated and rejected, with justification.

> "Your investigation record should document... the evidence reviewed, any alternative root causes that were evaluated and rejected, and the final root cause determination with supporting justification."

**Inline caveat:** regulatory practice (21 CFR 820.100 / QMSR); wording relayed via compliance-vendor summaries, not the regulation verbatim. The "rejected alternatives" discipline is well-established across CAPA/8D literature. Adopt it to make "accept — do not fix" auditable: record which findings were clustered, which candidate actions were considered, and why none was proposed — so a null result is traceable, not a silent gap.

Sources: https://regwatchdaily.com/blog/capa-procedure-fda-21-cfr-820-complete-guide ; https://quality.eleapsoftware.com/21-cfr-part-820-complete-guide-to-medical-device-qms-compliance/

### Diverge from the RTM/CAPA completeness mandate: be selective and outcome-gated `[medium]`

The dominant RTM/CAPA tradition pushes traceability toward mandatory, complete, ubiquitous coverage and treats a missing link as a defect.

> "an RTM helps ensure that all regulatory requirements are captured, implemented, and validated, providing evidence to obtain necessary certifications and approval and to pass audits."

**Inline caveat:** this is a synthesis/divergence call, not a single named practice — grounded in the contrast between compliance-completeness framing and CoEST's cost-effectiveness goals. fw-improve adopts the traceability *discipline* (every action cites evidence) but rejects the *completeness mandate* (every finding must yield an action). "No proposal" is a valid, recorded result.

Sources: https://www.perforce.com/resources/alm/requirements-traceability-matrix ; https://www.parasoft.com/learning-center/iso-26262/requirements-traceability/

## Refuted claims (do NOT build on these)

- **"RICE's Confidence is a routing signal, not a ranking penalty" — KILLED [0-3].** All three verifiers refuted it. The load-bearing quote ("signals the team needs more research before committing; it does not by itself kill the idea") appears in *neither* cited source — it traces to a Tempo guide. Mechanically, RICE Confidence is a multiplier in (R×I×C)/Effort that directly cuts a project's score — i.e., a literal ranking penalty, the opposite of what the claim asserted. **Do not** justify "low confidence → route to fw-spec" by appeal to RICE. (The routing idea may still be sound design — but it has no support from this source, so build it as a flywheel choice, not a cited borrow.) Sources cited (and contradicted): https://www.intercom.com/blog/rice-simple-prioritization-for-product-managers/ ; https://easyretro.io/tools/rice-calculator/

- **"'Verifiable end state' IS exactly 'observable OUTCOME, never an implementation'" — KILLED [1-2].** The SRE criterion's own canonical good example ("Add an alert when more than X% of machines are taken away") is itself an *implementation* that merely has a checkable completion state. The "never an implementation" prohibition is conflated from a separate criterion, and the "single most decisive borrow" superlative is a ranking the source never makes. **Do not** claim Google's text mandates "outcome, never implementation." Source: https://sre.google/workbook/postmortem-culture/

- **"JTBD/ODI establishes jobs are stable and solution-independent" — KILLED [0-3].** Rests solely on Strategyn (Ulwick's own ODI-marketing page); stability is an unvalidated, *contested* premise — a rival JTBD school (Klement) explicitly rejects functional-job stability, and even proponents concede stability is an artifact of choosing the abstraction level. **Do not** cite JTBD as the theoretical license to defer the fix. Source: https://strategyn.com/jobs-to-be-done/

- **"OKRs codify the exact rule fw-improve needs (outcome not activity)" — KILLED [1-2].** re:Work is Google's vendor-promotional guide for an empirically thin framework; its outcome-vs-activity axis is *different* from fw-improve's change-vs-implementation distinction (a proposed change is itself an "activity/output" in OKR terms). A loose analogy dressed as "the exact rule." **Do not** present OKRs as canonical proof of the outcome rule. Source: https://rework.withgoogle.com/intl/en/guides/set-goals-with-okrs

- **"The Google example postmortem maps every action item to a specific root cause; type ∈ {prevent/mitigate/process}" — KILLED [0-3].** The example table has only Action Item/Type/Owner/Bug columns with *no* root-cause linkage; 5 of 9 items show "n/a" for the bug/tracking number; the real Type taxonomy is broader (mitigate/prevent/process/other, plus detect/investigate/repair in the workbook). The "operational template" of cause→action mapping is fabricated. **Do not** require proposals to mirror a non-existent SRE mapping table. Sources: https://sre.google/sre-book/example-postmortem/ ; https://sre.google/workbook/postmortem-culture/

- **"Traceability is formally defined as bidirectional, requirement→test→defect / defect→test→requirement" — KILLED [0-3].** The IEEE "reason for existing" phrase is genuine, but the bidirectional clause in the quote appears in neither cited source — it is Perforce vendor-marketing wording, and the cited Jama page (also marketing) defines forward/backward differently. The categorical "an action with no link *fails the definition of traceability itself*" is an overstatement of a descriptive vocabulary entry. **Do not** cite this exact bidirectional definition as IEEE-authoritative. (The weaker principle — proposals should link back to evidence and forward to an outcome — survives via the *purposed/cost-effective* CoEST finding above.) Sources: https://en.wikipedia.org/wiki/Requirements_traceability ; https://www.jamasoftware.com/requirements-management-guide/requirements-traceability/traceability-matrix-101/

- **"~30% industry completion rate for postmortem action items; bloat is a named root cause" — KILLED [0-3].** The "30%" quote appears in neither cited source (it traces to an uncited DEV.to blog); the incident.io source contains no completion-rate statistic and names *different* root causes; "<40% in 90 days" rests on a single pseudonymous Medium post citing unnamed "studies." Stitched-together folklore presented as industry-wide empirical data. **Do not** cite a completion-rate number to justify keeping proposals few. (The qualitative case — bloat erodes trust, fewer/sharper wins — survives via the SRE "too many is as bad as none" finding.) Sources cited (and contradicted): https://incident.io/blog/why-do-post-mortem-action-items-fail-how-to-make-incident-follow-ups-actually-get-done ; https://medium.com/@coding_with_tech/your-incident-postmortem-process-is-probably-making-your-team-worse-heres-the-data-3092c9005ad2

- **"Single root cause is fundamentally wrong → therefore cluster findings into ONE systemic proposal" — KILLED [1-2].** Cook's descriptive "no isolated cause" (point 7) is quoted accurately, but it does *not* license the prescriptive remediation rule; the same paper (point 15) warns that bundled remedies "increase the coupling and complexity of the system," and mainstream safety science (Reason's defense-in-depth) prescribes *multiple* independent barriers. "Cluster-don't-enumerate" is a contested design preference, not a settled safety-science finding. **Do not** justify clustering as safety-science-mandated. (Clustering survives as a flywheel design choice and via the SRE "too many action items" finding — just not via Cook.) Source: https://how.complexsystems.fail/

- **"Act only when a single cause crosses ~20% of error budget; rank by recurrence and blast radius" — KILLED [0-3].** The SRE 20% thresholds gate *secondary* actions (a mandatory postmortem, a P0 planning item), while the quoted "halt all changes" action triggers on *total* error-budget exhaustion, not a single cause at 20%. "Rank by recurrence and blast radius" appears nowhere in the source. The claim fuses two distinct clauses and grafts on an unsupported heuristic. **Do not** cite a 20%-of-budget rule as an fw-improve gate. Source: https://sre.google/workbook/error-budget-policy/

## Caveats

- **Time-sensitivity.** The LLM verbosity/sycophancy findings (Part 5) are 2024-2025 preprints describing a fast-moving area; reward-model behavior may shift with newer alignment techniques. The prioritization-framework critiques are stable but fashion-driven (WSJF/RICE adoption waxes and wanes). Re-validate the ML claims before any future rewrite.
- **Vendor self-reporting.** A large share of the surviving evidence is vendor- or consultancy-authored: Google SRE (sre.google), incident.io, Atlassian, Intercom, SAFe/Scaled Agile, Strategyn, Perforce, Jama, compliance vendors. These are practitioner canon, not independent research; several *killed* claims died precisely because a vendor page was treated as authoritative or a quote was misattributed to one. Treat every vendor quote as "best-practice assertion," not measured fact.
- **Preprint and non-peer-reviewed risk.** The CoEST Grand Challenges (arXiv) and the verbosity-bias papers (arXiv) are the only academic-leaning sources; one is a preprint family. The peer-reviewed anchors are narrow: Peerally et al. (BMJ Q&S, healthcare RCA) and Simon's satisficing (SEP). Most "canon" here (Google SRE books, Cook, Reinertsen, Fowler, Cagan) is recognized practitioner literature, not experiment.
- **Several quotes were relayed via intermediaries.** Where flagged inline (SMART origin, CoEST goal list, CAPA wording, Cagan's roadmap quote behind an HTTP 403), the substance is corroborated but exact wording fidelity is medium. The Refuted section shows the cost of not checking: multiple killed claims had quotes that existed nowhere in their cited URLs.

## fw-improve routing (reasoned design recommendation — NOT independently verified)

The following translates the *surviving* findings into concrete guidance. It is opinionated design, not a verified fact. The existing skill at `.claude/commands/propose-improvements.md` already implements most of this; the deltas below are where the evidence sharpens or corrects it.

**Every proposal traces to a cited finding — make the link load-bearing, not decorative.** Each proposal must quote the retro's evidence pointer *verbatim* (run_id / audit record / grader receipt / transcript `file:line` / `COUNT(*) → N`). A proposal whose evidence pointer is not re-verifiable is ineligible — skip it and note the retro was thin there. This is the *purposed/cost-effective* CoEST discipline (Part 6), not the killed "bidirectional IEEE definition" — cite the surviving source if you cite one at all. The link runs both ways: back to the cited finding, forward to a checkable outcome.

**Cluster related findings into one proposal — as a discipline, not a safety-science law.** Collapse symptoms of one root cause into a single proposal; do not emit one fix per symptom. Ground this in the *surviving* basis — SRE's "too many action items are as bad as none" (Part 1) — **not** the killed Cook "single root cause is wrong → cluster" claim. And honor Cook's actual warning (point 15): a clustered "mega-fix" can increase coupling. So cluster the *findings* into one proposal, but if the clustered fix is large or raises blast radius, route it to fw-spec for discovery rather than presenting it as one ready change.

**Rank by leverage as an ordering with a written rationale — never a composite number.** Leverage = (recurrence × blast radius × cost-of-inaction) ÷ fix-cost, expressed as a *rank order and a one-line rationale per item*, never a fabricated score. This is the WSJF Cost-of-Delay-÷-job-size survivor (Part 2) plus the "score theater" warning. Use Reinertsen's flat-bottomed-optima license to stop refining the ranking once the order is defensible. Operationalize recurrence/blast-radius with SRE's toil tests (does the loop stay in the same state after the manual fix; does cost scale with run count; is it interrupt-driven) — concrete and citable, not a 1-10 guess. Equal cost-of-delay → cheaper fix first.

**State the outcome, not the implementation — and stop there.** Each proposal asserts an observable change in how the loop runs ("an `AWAITING_APPROVAL` lifecycle is exercised end-to-end at least once"; "first-attempt pass rate on class-X tasks recovers"; "this grader stops passing on unverified output") and defers the *how* to the handoff. Use the Seiden definition (outcome = a change in behavior, nothing to do with making stuff) and Fowler's completion-criteria split. The moment the proposal names functions, columns, or signatures, it has crossed into fw-spec/fw-plan territory — cut it. Note the honest boundary (Part 3, fw-plan exception): for a well-understood, repeatable fix, the outcome may be more solution-shaped, and that is legitimate — it routes to fw-plan. Guard every outcome against Torres's "outputs disguised as outcomes" by requiring it to be checkable against the *same* cited evidence the finding carries.

**Confidence drives the handoff target — as a flywheel choice, explicitly not as a RICE borrow.** Route low-confidence / discovery-needed findings to **fw-spec**; route high-confidence, scoped, well-understood fixes to **fw-plan**; route one-offs and inherent costs to **accept**. Do *not* cite RICE Confidence for this — that claim was killed (in RICE, Confidence is a score penalty, and the supporting quote was fabricated). State the routing rule as fw-improve's own design, justified by the fw-plan output-vs-outcome boundary (Part 3) and Cook's "change introduces new failure" (riskier changes → discovery first).

**Decide accept/do-not-fix deliberately, with a cited reason, and record the rejected alternatives.** "Accept — do not fix" is a first-class proposal, blessed by the SRE economics (perfect reliability is the wrong target; toil is fine in small doses; fix only when time saved is at least proportional to time invested) and the incident.io closure discipline ("explicitly deprioritised with a reason ... is a completely valid outcome"). Require a *cited reason* for every accept, anchored to recurrence and blast radius. Borrow the CAPA "rejected alternatives" discipline: briefly record which findings were clustered and which candidate actions were considered and dropped — so a null result is auditable, not a silent gap. Gate one-off accepts on *low* blast radius (a one-off with large blast radius is not automatically acceptable).

**The operator owns prioritization — propose, rank, hand off; never assign.** Deliberately reject owner-assignment, deadlines, priority labels, and burndown tracking as out of scope (the school that owns remediation through completion is where fw-improve diverges — Part 1). fw-improve's "single next step" is the *handoff target* (fw-spec / fw-plan / accept), not a human assignee. Present the ranked list, let the operator pick what to advance (multi-select), and advance only what they pick; "advance none" is valid and the analysis is still recorded. This is the SMART divergence (only Specific + Measurable transfer; Assignable/Realistic/Time-related are downstream).

**Write a short "no action proposed" note when the retro is clean — and resist the model's pull to pad.** A clean retro yields a 1-2 sentence note naming what it found, then stop. This is licensed by satisficing (propose above the bar, then stop), SRE's deliberate-cost gating, and the WSJF "no realizable value → no cost of delay" logic. Critically, treat the LLM verbosity/sycophancy bias (Part 5) as a real force: the model's default incentive is to over-produce, so the skill needs an *explicit* anti-padding instruction and an *affirmative* "few or zero proposals is a correct result" license — not as politeness, but as a mechanical countermeasure. Do not enumerate symptoms to fill the doc.

**Keep it lean — what to cut.** No composite numeric scores (score theater). No owner fields, deadlines, SLAs, or priority floors ("at least one P0/P1" contradicts "zero is correct"). No mandatory cause→action mapping table (that template was the killed SRE claim). No invented findings, no "it would be nice if," no counterfactual "would-have-prevented" fixes (hindsight bias). No completeness mandate (every finding need not yield an action — the RTM/CAPA divergence). The whole stage is: read cited retros → cluster → rank by leverage with written rationale → state outcomes → hand off or accept-with-reason → let the operator choose. It writes no specs, no tasks, no code.

## Sources (verified primaries)

- https://sre.google/workbook/postmortem-culture/
- https://sre.google/sre-book/postmortem-culture/
- https://sre.google/sre-book/example-postmortem/
- https://sre.google/sre-book/embracing-risk/
- https://sre.google/sre-book/eliminating-toil/
- https://sre.google/workbook/eliminating-toil/
- https://sre.google/workbook/error-budget-policy/
- https://static.googleusercontent.com/media/sre.google/en//static/pdf/login_spring17_09_lunney.pdf
- https://www.usenix.org/conference/srecon17americas/program/presentation/lueder
- https://www.usenix.org/system/files/login/articles/login_spring17_09_lunney.pdf
- https://www.atlassian.com/incident-management/handbook/postmortems
- https://docs.gitlab.com/operations/incident_management/
- https://incident.io/blog/why-post-mortem-action-items-die
- https://postmortems.pagerduty.com/culture/accountability/
- https://en.wikipedia.org/wiki/SMART_criteria
- https://www.projectsmart.co.uk/smart-goals/brief-history-of-smart-goals.php
- https://framework.scaledagile.com/wsjf
- https://blackswanfarming.com/wsjf-weighted-shortest-job-first/
- https://en.wikipedia.org/wiki/Cost_of_delay
- https://www.intercom.com/blog/rice-simple-prioritization-for-product-managers/
- https://www.productplan.com/glossary/rice-scoring-model
- https://agility-at-scale.com/principles/product-economics/
- https://www.amazon.com/Principles-Product-Development-Flow-Generation/dp/1935401009
- https://www.rock.so/blog/rice-scoring
- https://dovetail.com/product-development/rice-scoring-model/
- https://www.martinfowler.com/bliki/OutcomeOverOutput.html
- https://www.mindtheproduct.com/outcomes-over-outputs-josh-seiden-on-the-product-experience/
- https://medium.com/design-bootcamp/outcomes-over-outputs-key-points-of-joshua-seidens-book-reading-e8cf5e2b7a9d
- https://leaddev.com/velocity/focus-outcomes-over-outputs
- https://www.svpg.com/outcomes-are-hard/
- https://www.producttalk.org/defining-product-outcomes/
- https://plato.stanford.edu/entries/bounded-rationality/
- https://www.nasa.gov/wp-content/uploads/2023/08/nasa-risk-mgmt-handbook.pdf
- https://how.complexsystems.fail/
- https://www.michaelnygard.com/blog/2021/06/counterfactuals-are-not-causality/
- https://psychsafety.com/counterfactuals/
- https://pmc.ncbi.nlm.nih.gov/articles/PMC5530340/
- https://arxiv.org/abs/1710.03129
- https://link.springer.com/chapter/10.1007/978-1-4471-2239-5_16
- https://regwatchdaily.com/blog/capa-procedure-fda-21-cfr-820-complete-guide
- https://quality.eleapsoftware.com/21-cfr-part-820-complete-guide-to-medical-device-qms-compliance/
- https://arxiv.org/html/2411.15287v1
- https://arxiv.org/html/2505.12843
- https://www.emergentmind.com/topics/verbosity-bias-in-preference-models
