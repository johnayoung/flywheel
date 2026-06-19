# Vision

## The problem

AI coding agents such as Claude Code and Codex can complete real software tasks autonomously, but running them directly creates a black box. When an agent gets confused, it can loop, consume context, burn budget, and produce little of value without giving a reliable signal about what is happening. There is usually no durable execution record, no consistent lifecycle, and no meaningful operational visibility. In practice, you either babysit the agent or accept weak control over a system that can spend real time and money. 

## What we're building

We are building a production-grade orchestration loop for AI coding agents.

The agent is the brain. The loop is the control plane around it.

Its job is to take a structured task, run the agent against that task, observe what happens, verify any claim of completion, record the full execution history, and expose clear intervention points for humans and higher-level systems. It does not solve the coding problem itself. It does not decompose large goals into subtasks. It does not replace planning systems, queues, or product logic. It owns the execution lifecycle of a single task and makes that lifecycle legible, testable, and controllable. 

The loop is responsible for:

* invoking the agent
* recording lifecycle transitions
* validating agent-reported iteration status
* verifying completion claims
* preserving attempt history and failure context
* emitting structured events for telemetry and downstream control

Higher-level systems may still decide which tasks to run, what budgets to apply, when to escalate, and how to coordinate many tasks at once. But for one task at a time, the loop is the execution controller. 

## North star (forward bets)

> A forward-looking thesis (2026-06-19), held loosely and meant to be falsified. The rest of this document describes, in the present tense, what the loop *is*. This section describes where it is *heading* and why. Revisit and rewrite it whenever a bet is confirmed or broken.

As agent-driven generation gets cheap and abundant, the scarce thing stops being *writing code* and becomes *agreeing on what to build and proving it is correct*. Four falsifiable bets, each something a reasonable expert could reject:

1. **Line-by-line human review stops being the merge gate.** With many agent threads per engineer, the gate moves to spec + acceptance evidence. *Falsified if* review time per change stays flat over the next 18-36 months.
2. **The durable asset becomes the spec + graders, not the code.** Teams regenerate modules from intent + acceptance rather than hand-patch them. *Falsified if* patching hand-written code stays dominant.
3. **Surface area per engineer explodes, so cross-thread coordination becomes the dominant cost.** *Falsified if* shipped distinct surfaces per engineer stays flat.
4. **Status decouples from self-report, or it dies.** Once a human touches little of the work, only evidence-backed status survives. *Falsified if* self-reported cards remain how agent work is tracked.

**The binding constraint these imply:** a single, verifiable statement of intent that simultaneously binds agents (`Task` + graders) and informs humans (legible status). Flywheel already owns half of it — `Task`/`Grader` is machine-checkable acceptance. The missing half is the human-legible projection over it.

**What this commits flywheel to — and what it does not.** Flywheel stays an execution engine and surfaces the intent + acceptance graph as a first-class artifact with an evidence-derived read projection. It is the system of record for *verified build-conformance* ("this is built and meets its stated acceptance") and is explicit that it does **not** verify *value* ("is this the right thing to build"). Prioritization and product judgment stay human and stay elsewhere; flywheel owns the binding between stated intent and verified reality. This is deliberately **not** a planning/roadmap layer: a roadmap is a *view* of the Task DAG, never a separate surface to drag cards on.

**The honest limit.** Graders cover mechanical correctness, not desirability. As nodes climb toward stakeholder altitude, the grader-backed share of status falls — so the claim is scoped to conformance + structural rollup, never "this is valuable." Overclaim past that line and the thesis breaks.

**First disprovable step (shipped):** `flywheel status --rollup` — a phase-grouped projection whose every node status is *derived* from grader receipts (`verified` vs `accepted` vs blocked / failed / not-started), never operator-set. The test: does an evidence-only rollup get trusted and used in place of manual status reporting? If it gets overridden or explained around, the gap is value-judgment, not verification — and the upward direction is wrong.

## Core principles

### Task-based

The unit of work is a [task](task-schema.md): a structured definition containing a goal, graders, and optional briefing context. The loop operates on one task at a time.

The original task definition is treated as immutable once created. If execution reveals new constraints, clarifications, or human directives, those are recorded in lifecycle data rather than mutating the task itself. This preserves a clean separation between the definition of work and the history of execution.

### Observable

Observability is a first-class property of the loop.

Every meaningful event in execution is emitted as structured data: lifecycle transitions, agent invocations, iteration envelopes, verification results, interruptions, protocol errors, and infrastructure failures. This creates a durable execution record that can power logs, dashboards, alerts, automation, and postmortems.

The loop is not merely a reporter. It is the controller for a single task’s execution lifecycle. Business policy such as queueing, prioritization, retry budgets, and escalation thresholds can live above it, but the loop still owns the state transitions and local operational behavior required to run safely.

#### Audit stream

Persistence follows the data taxonomy split (`docs/data-taxonomy.md`): the relational store holds state — lifecycles, attempts (with rolled-up token/iteration aggregates), the domain-event ledger, grader receipts — while telemetry (every SDK message the agent emits, every `harness.*` telemetry event, and a `domain.*` mirror of each ledger append) streams to one append-only JSONL file per run at `<logs root>/runs/<run_id>.jsonl` (default logs root `.flywheel/logs`). File write order is the canonical observability ordering; the ledger row stays authoritative for state. Telemetry loss is non-fatal by design — a sink failure is recorded once and the run continues — while ledger and lifecycle write failures keep strict, fatal semantics.

The run file is exposed through `flywheel_core.audit.stream(run_id, store=..., logs_root=..., follow=...)` as the programmatic API and `python -m flywheel_core.audit` as the operator CLI. The reader tails the file (cursor = byte offset + line count; a missing file reads as empty; a partial trailing line is withheld until complete) and reconstructs the same record shapes consumers always saw; the lifecycle row remains the terminal-status oracle that ends a live follow. Replay and live tailing share one iterator; this is the canonical inspection surface for "what did the agent actually do?".

The run files are sensitive-by-default: payloads are captured verbatim with no truncation, so they may contain prompts, tool inputs, and tool outputs in the clear (they are covered by `.flywheel/.gitignore`). A read-time redaction layer (`flywheel_core.redaction.Redactor`) sits on top of the stream and is applied by `flywheel_core.audit.stream(redactor=...)`, `subscribe`, `attach_logger`, and the `python -m flywheel_core.audit` CLI (`--redact-policy`, `--raw`, `--dry-run`). Redaction is best-effort — unmatched secrets can still pass through, and the run file remains the unredacted, sensitive-by-default source of record for telemetry.

Write-time redaction at the sink seam is a designed-for follow-up that reuses the same `Redactor` abstraction — not built in this feature. It trades forensic completeness for no-cleartext-at-rest and is the consumer's deliberate opt-in; the MVP ships the read-time wiring only. Retention, rotation, and archival of run files are operator-owned: flywheel emits them and never deletes them.

### Claim-based signaling

The agent is instructed to report its status honestly, but the loop does not trust agent output as authoritative control input.

Agent-reported status is treated as a claim. A claim of `completed` does not end execution; it starts verification. A claim of `blocked` does not mutate state directly; it is interpreted by the harness and recorded appropriately. Prompt design can improve signal quality, but correctness is enforced by protocol handling, verification, and lifecycle control rather than by trusting the prompt alone. 

### Context-aware

Context is a finite resource with diminishing returns. As it accumulates, agent reliability often degrades. That degradation is not perfectly measurable, but its risk can be monitored through operational signals.

The loop treats context pressure as a first-class concern and tracks signals such as:

* context window utilization over time
* context growth rate by iteration and attempt
* threshold crossings such as 50%, 75%, and 90%
* cumulative token growth across a task run

These signals are surfaced through the same observability pipeline as other loop events. They are not treated as proof of semantic degradation; they are risk indicators that can inform future intervention, compaction, reset, or halt decisions. 

### Programmatic

The loop is a Python library.

It is designed to be embedded into larger systems such as CI pipelines, task queues, orchestration services, and review workflows. The core loop ships no operator UI of its own; rich operational surfaces — the `flywheel`/`fw` shell and operator console — are built above it. 

### Brain-agnostic

The loop operates against an interface rather than a single concrete coding agent.

The first implementation targets Claude Code, but the architecture is intended to support additional backends over time, including Codex and future agents. That portability is a goal, not an assumption. Different agents may expose different tool models, memory behavior, iteration boundaries, and failure modes, so adapter depth may vary. The loop abstracts the lifecycle, protocol handling, and verification flow while allowing agent-specific backends to implement the details. 

## What it is not

It is not a task queue. It does not prioritize, schedule, or coordinate many tasks. A higher-level system does that and hands individual tasks to the loop.

It is not a planner. It does not decompose broad goals into subtasks or choose the implementation strategy for the agent. The brain does that internally, and broader orchestration systems may do that externally.

It is not a UI. Operational surfaces are intentionally minimal — richer interfaces can consume the structured event stream and present more advanced views. 

## The loop

The loop drives a task through its [lifecycle](task-lifecycle.md): a state machine for controlling execution, verification, retries, and terminal outcomes for a single agent-driven task.

```text
pending -> ready -> running -> validating -> done
                      |           |
                      |           +-> validation_failed -> ready
                      |           +-> validation_failed -> failed
                      |
                      +-> blocked -> ready
                      +-> interrupted -> ready
                      +-> failed
```

The brain executes. The loop controls execution, records the lifecycle, and verifies claims.

A task typically begins in `pending`, becomes `ready` when it can be executed, moves to `running` when the agent is invoked, and transitions to `validating` only after the agent claims completion. If verification succeeds, the task reaches `done`. If verification fails, the loop enters `validation_failed`, which is a factual outcome — verification disproved the completion claim — not a retry decision. The controller then applies policy to decide whether the task returns to `ready` for another attempt or terminates as `failed`. Agent-declared inability to proceed can move the task into `blocked`. External pause or stop signals move it into `interrupted`. Terminal states produce structured outcome records. 

Every attempt is recorded with timing, outcome, agent output, and associated error context. Sequential failures are tracked explicitly so that higher-level systems can decide whether to continue, pause, or terminate. The data model must clearly distinguish iteration-level, attempt-level, and run-level counters to avoid ambiguity in implementation and telemetry. 

Completion is a claim that gets tested, not a promise that gets trusted. The agent saying “I’m done” is the start of the verification process, not the end of the loop. 

## Iteration signaling

At the end of every iteration, the agent emits a structured envelope. The envelope replaces fragile string matching in free-form stdout and serves a dual role: a control input the harness reads to decide what to do next, and an observability event recorded for telemetry.

The harness treats the envelope as untrusted protocol input. It validates the envelope before acting on it and must handle malformed, missing, duplicate, partial, or contradictory cases as first-class outcomes. The agent never directly mutates lifecycle state — the harness translates iteration signals into lifecycle transitions.

The envelope schema, the agent-reported status enum, and the per-status harness behavior live in [loop.md](loop.md).

## Graders

When the agent claims `completed`, the loop does not trust the claim. It runs the task's `graders`, defined in the [task schema](task-schema.md). A `validation_failed` outcome records that verification disproved the completion claim. It does not, by itself, decide what happens next. The controller applies policy to decide whether to retry, escalate, or terminate. That distinction matters: verification outcome and retry policy are not the same thing.

Each grader is a typed object: `command`, `rubric`, `manual`, or `transcript`. All graders must pass for the task to reach `done`. The harness runs them cost-cheapest-first so deterministic failures abort before any LLM or human cost is incurred. Graders are optional: a task with no graders is a deliberately unverified run that reaches `done` on the agent's own claim — the right tool for exploration, the wrong one the moment "done" must mean something checkable.

### `command`

Deterministic shell checks — tests, build, lint, typecheck, custom scripts, filesystem state checks. Pass = exit 0.

If any command grader fails, the validation result is recorded and the failure output is attached as context. For MVP, command failures are retryable by default: the task transitions back to `ready` so the agent can address the failure on the next attempt.

### `transcript`

Path-level constraints such as `max_turns`, `max_total_tokens`, and `max_wall_seconds`. The harness also enforces these as hard limits during the run, not only at grade time — a runaway loop is aborted as soon as the limit is crossed, before grading begins. For MVP, transcript-grader failures are treated the same as command failures: retryable.

### `rubric`

A separate LLM call, distinct from the working agent, evaluates natural-language assertions against the original goal, the diff, and other execution artifacts. Useful for catching the class of failure where deterministic checks pass but the implementation still does not match intent. It should be treated as semantic review assistance, not as a perfectly reliable judge, especially for broad or context-heavy tasks.

For MVP, a rubric failure auto-retries by default (`retry_on_fail=True` on each `RubricGrader`): the verifier's assessment is persisted as a grader receipt and surfaced as a `# Reviewer feedback` section in the next attempt's prompt, so the working agent can correct course on its own. Operators opt into the original pause-for-review behavior on a per-grader basis by setting `retry_on_fail=False`, which routes the lifecycle to `interrupted` instead.

### `manual`

For checks where automated verification is insufficient, the loop pauses and surfaces a summary, relevant artifacts, and change context for human approval. This is the escape hatch for work that is risky, ambiguous, or too dependent on product and architectural judgment to verify safely in a fully automated way.

The harness executes manual gates as a first-class lifecycle state. When the automated graders all pass, the attempt is finalized `succeeded` and the lifecycle parks at `awaiting_approval` instead of reaching `done`. An operator resolves each gate via `flywheel approve` or `flywheel reject [--feedback TEXT]`; approve advances to the next gate or `done`, while reject writes a `passed=false` receipt and routes through `failed_validation` to a retry that carries the operator's feedback into the next attempt's prompt (or to `failed` once the retry budget is exhausted).

### Execution order

Within an attempt, graders run in cost order:

1. `command`
2. `transcript`
3. `rubric`
4. `manual`

Within a type, list order is respected. The first failure inside a type skips the remainder of that type and all later types. A failed grader records a `validation_failed` outcome; what happens next is a separate policy decision. For MVP, `command`, `transcript`, and `rubric` failures retry automatically (rubric carries the auto-retry default per-grader via `retry_on_fail=True`; flipping it to `false` opts a specific rubric back into the original pause-for-review behavior). `manual` gates park the lifecycle at `awaiting_approval` and wait for an operator's `approve` or `reject`; a reject then follows the same retry-with-feedback arm as a failing rubric.

Future versions may add richer validation-failure categories, smarter retry policy, stuck detection, and structured repair hints. For MVP, the policy stays intentionally narrow.

## Intervention points

### Blocking

Sometimes the agent cannot proceed without external input. That is not the same thing as an infrastructure failure and not the same thing as an external interruption.

When the agent reports `blocked`, the loop records the reason, transitions the task into a blocked state, and exposes that state to operators or consuming systems. When the required input is provided, the task can return to `ready` and continue without losing prior execution history. 

### Interruption

External systems can halt a running task. This moves the task into `interrupted`.

Interruption is an exogenous pause: a human or system stops the run, independent of whether the agent believed it could continue. The loop records the interruption, preserves execution history as configured, and allows the task to resume through `ready` when released. 

## Failure classes

Not all failures mean the same thing, and the loop should record them distinctly even if some share lifecycle states.

### Task failure

The agent could not complete the task successfully.

### Validation failure

A completion claim was disproved by deterministic checks, semantic verification, or human review.

### Protocol failure

The agent emitted malformed, missing, duplicate, truncated, or contradictory iteration envelopes.

### Infrastructure failure

The agent subprocess, SDK layer, verifier infrastructure, storage system, or other execution dependency failed.

These distinctions matter for telemetry, retry policy, escalation, and operator understanding. Even if the externally visible lifecycle remains compact, the internal outcome model should preserve these categories. 

## Where this fits

The single-task loop is flywheel's foundation, shipped as the embeddable `flywheel-core` package. It implements the [lifecycle](task-lifecycle.md): run the agent, validate protocol signals, verify completion claims, manage retries, and preserve execution history. The [task schema](task-schema.md) and [lifecycle](task-lifecycle.md) are the source of truth — everything in the loop exists to move a task through those states correctly, observably, and reliably.

Its first agent backend targets the [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python), wired in as an optional extra (`flywheel-core[claude]`) behind a single lazy boundary, so the data and lifecycle surface need no SDK at all.

The building blocks that depend on a correct, trustworthy loop are built on top of it: multi-task orchestration over a prerequisite DAG (`flywheel-orchestrator`), the git-worktree landing strategies and worker daemon (`flywheel-worktree`), the `flywheel`/`fw` operator shell, and the spec-driven authoring skills installed by `flywheel init --skills`. Task decomposition and richer intervention policy continue to build on the same foundation. 

## Glossary

| Term                 | Layer         | Definition                                                                                                                                                                                                                                         |
| -------------------- | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Task**             | Schema        | The unit of work: a structured definition with goal, graders, and optional briefing context. The original task definition is immutable once created. Execution-time clarifications and directives live in lifecycle records rather than mutating the task. |
| **Lifecycle**        | State         | The mutable execution record for a task. It tracks status, attempts, timestamps, outcomes, and associated errors. One lifecycle exists per task execution.                                                                                         |
| **Lifecycle status** | State         | The system-controlled state of a task’s execution, such as `pending`, `ready`, `running`, `validating`, `blocked`, `interrupted`, `done`, or `failed`. Transitions are governed by the harness, not by the agent.                                  |
| **Envelope**         | Signal        | The structured payload emitted by the agent at the end of each iteration. Treated as both protocol input and observability data, not as a trusted command. Schema lives in [loop.md](loop.md).                                                     |
| **Iteration status** | Signal        | The agent-reported status carried inside an envelope. Represents the agent's claim about progress and never directly changes lifecycle state. Defined values live in [loop.md](loop.md).                                                          |
| **Iteration**        | Execution     | A single invocation of the agent within an active execution path. The agent runs, produces output, and emits an envelope.                                                                                                                          |
| **Attempt**          | State         | A recorded unit of execution with start and end times, outcome, output, and associated errors. An attempt may span one or more iterations and ends when the harness leaves the active execution path.                                              |
| **Run**              | State         | A logical grouping of attempts identified by `run_id`. If run-level counters are tracked, they should be defined separately from iteration- and attempt-level counters to avoid ambiguity in telemetry and policy.                                 |
| **Harness**          | Control       | The loop controller that invokes the agent, validates envelopes, owns lifecycle transitions, triggers verification, and manages retries, pauses, and termination.                                                                                  |
| **Verification**     | Control       | The process of testing a completion claim. It runs during `validating` and executes the task's graders — `command`, `transcript`, `rubric`, and `manual` — in cost order.                                                                            |
| **Brain**            | Execution     | The AI coding agent that performs the actual work, such as Claude Code or Codex. The loop is designed to support different brains through a backend interface.                                                                                     |
| **Agent contract**   | Signal        | The prompt- and protocol-level expectation that the agent emits valid envelopes and reports its state honestly. The system still treats those reports as untrusted claims.                                                                         |
| **Context pressure** | Observability | The operational risk that reliability degrades as context accumulates. The loop monitors proxies such as utilization and growth rate, but does not treat them as a complete measurement of semantic degradation.                                   |