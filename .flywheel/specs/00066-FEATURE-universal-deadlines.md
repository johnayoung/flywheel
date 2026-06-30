# Feature: Universal Deadlines (no unbounded wait)

## Outcome
Every external call flywheel makes — the working agent iteration, the rubric judge
stream, command graders, every docker management invocation, and the autopilot
discovery/authoring agent calls — is governed by a default-on, operator-overridable
wall-clock deadline. A call that exceeds its ceiling returns or raises a timeout
outcome within roughly `ceiling + epsilon` instead of blocking forever, so no single
external call can park a worker or the autopilot daemon indefinitely. With no
operator override present, the effective ceiling for each of these call classes is a
finite, non-null value (default-on), not unbounded.

## Background
Flywheel has five places where it issues an external call with no wall-clock bound, so
"the call never returns" is currently "the daemon is wedged forever while still holding
its lease." The hang watchdog ships gated OFF (`loop_guard.hang_timeout_seconds`
defaults to `None`) and even when enabled only fires on inter-message silence, not on a
steadily-streaming-but-never-terminating agent; the rubric judge stream and the docker
teardown calls sit entirely outside any watchdog; command graders already accept a
per-grader timeout the harness never passes; and the autopilot agent calls are bounded
by a turn budget, not a wall-clock one. The tacit requirement the literal reading misses:
"bounded" here means a real wall-clock ceiling that fires even while bytes are still
arriving — an idle/silence timeout does not satisfy it, and a turn budget does not
satisfy it. The natural seam is one reusable deadline primitive every call site adopts
together, so the behavior is uniform and the defaults live in one place. This phase
assumes Phase 1 (containment) has landed: a timeout-induced cancellation/exception is
already caught and turned into a contained, retryable failure rather than crashing the
loop, so these criteria grade only that the deadline fires and surfaces a timeout
outcome — not the downstream retry policy.

## Scope
### In scope
- A single reusable wall-clock deadline primitive in `flywheel-core` that bounds an
  awaitable/`async for` stream and raises a distinguishable timeout signal when the
  ceiling is exceeded, regardless of whether the stream is still producing output.
- Default-on, finite, operator-overridable ceilings for: the working agent iteration,
  the rubric judge invocation, command graders (the harness actually passes a default
  per-grader timeout through), every docker *management* call (image/network inspect,
  network create, container stop/rm, the atexit force-remove), and the autopilot
  discovery/authoring agent calls.
- A timeout, when it fires, surfaces as a distinguishable timeout outcome/exception
  (not a hang, not a silent return) within roughly `ceiling + epsilon`.

### Out of scope
- The retry / escalation / re-queue policy after a timeout fires (that is Phase 1
  containment + later phases). These criteria stop at "the deadline fires and surfaces a
  timeout outcome."
- Changing the already-bounded paths: `start_container` (`timeout=120.0`) and docker
  `exec` (governed by `exec_timeout`) already have bounds; do not regress them.
- The inter-message silence/idle watchdog behavior already shipped — the new wall-clock
  bound is additive, not a replacement, and the existing silence detector must keep
  working.
- Tuning the numeric ceiling values to any "right" number; the requirement is finite +
  default-on + overridable, not a specific magnitude.

### Must not regress
- `import flywheel_core` must continue to work without the agent SDK extra installed
  (the deadline primitive and its core call sites must not import the SDK at module top).
- `flywheel_core.task` and `flywheel_core.lifecycle` stay pure (no file/JSON/io); the
  deadline primitive must not be placed in those modules.
- The existing verification suite (`scripts/check.sh`) still passes; no grader/test/CI
  assertion is weakened or deleted.
- An override of `0` / unbounded remains expressible (operator can opt out per call
  class), but the *default* with no override is finite.

## Success Criteria
Each criterion is atomic, grades an observable end-state, carries its grader type,
visibility, the exact check, and the gaming move it forecloses. `/fw-plan` lowers each
to a command / transcript / rubric / manual grader.

1. When the deadline primitive wraps an awaitable or async-stream that never completes
   and exceeds the configured ceiling, the primitive raises its distinguishable timeout
   signal within roughly `ceiling + epsilon`, even while the wrapped stream is still
   yielding items. [command | held-out]
   verify: `uv run pytest -k deadline_primitive` — a test drives the primitive with (a)
   a coroutine that sleeps past a tiny ceiling and (b) an `async for` over a generator
   that yields forever without stopping; both raise the timeout signal in bounded wall
   time. A still-streaming generator must NOT escape the deadline.
   defends against: implementing an idle/silence timer that only fires when output stops
   (the existing watchdog's weakness) and calling it a wall-clock deadline; or asserting
   only the sleep case and letting a steadily-streaming stream run forever.

2. When the harness drives a working-agent iteration with no operator override present,
   the iteration runs under a finite wall-clock ceiling, so an iteration whose invoker
   never returns is cancelled and surfaces a timeout-classified failure rather than
   awaiting forever. [command | held-out]
   verify: `uv run pytest -k harness_agent_deadline` — a test injects an invoker that
   never returns under the *default* config (no override) and asserts the harness
   produces a timeout-classified outcome in bounded wall time; a second assertion reads
   the default-resolved config and asserts the agent-iteration ceiling is non-null.
   defends against: leaving `hang_timeout_seconds` default `None` so the bare
   `await invoker(request)` path stays unbounded; or wiring the bound but defaulting it
   off so the test only passes when an override is set.

3. When the harness runs the rubric judge for a grader and no operator override is
   present, the judge stream runs under a finite wall-clock ceiling, so a judge whose
   stream never terminates is cancelled and surfaces a judge-infra timeout outcome
   rather than stalling VALIDATING forever. [command | held-out]
   verify: `uv run pytest -k rubric_judge_deadline` — a test substitutes a judge-invoke
   whose `async for` never ends, runs it under default config, and asserts a
   timeout-classified judge-infra failure outcome in bounded wall time.
   defends against: bounding only the working agent (which already had a watchdog) and
   leaving the judge stream — which sits outside `_drive_iterations` — unbounded; or
   re-using the silence watchdog that a streaming judge would slip past.

4. When the harness runs command graders with no operator override present, each command
   grader runs under a finite default per-grader wall-clock timeout, so a grader command
   that hangs is killed and recorded with a `timeout` termination rather than blocking
   validation forever. [command | held-out]
   verify: `uv run pytest -k command_grader_default_timeout` — a test runs a command
   grader whose `run` blocks (e.g. `sleep`-style) under the default harness config (no
   explicit per-grader timeout argument) and asserts the result is a failure with
   `payload["termination"] == "timeout"` in bounded wall time.
   defends against: relying on the existing `per_grader_timeout_seconds` parameter while
   the harness still calls `run_command_graders` without it (the current
   `harness.py:3422` gap), so the default path stays unbounded; or only testing the
   parameter in isolation, never through the harness's default call.

5. When any docker *management* call (image/network inspect, network create, container
   stop/rm, the atexit force-remove) is issued with no operator override present, the
   call runs under a finite default wall-clock timeout, so a hung `dockerd` surfaces a
   docker timeout error within roughly `ceiling + epsilon` rather than wedging the
   caller while it holds the lease. [command | held-out]
   verify: `uv run pytest -k docker_management_deadline` — a test points the docker
   primitives at a stub `docker` invocation that hangs, calls each management primitive
   (`image_exists`, `check_image_uid`, `network_exists`, `ensure_internal_network`,
   `remove_container`, `force_remove_container_sync`) under default config, and asserts
   each raises a docker timeout error in bounded wall time. The already-bounded
   `start_container` / exec paths must keep their bounds (assert they are unchanged).
   defends against: bounding `_run_docker`'s signature default but leaving the teardown
   callers (`remove_container`, `force_remove_container_sync`) passing nothing so they
   stay `None`; or fixing only one call site and leaving the others unbounded.

6. When the autopilot discovery or authoring agent call is issued with no operator
   override present, the agent stream runs under a finite wall-clock ceiling, so a
   stalled SDK stream is cancelled and surfaces a timeout outcome rather than parking the
   daemon, and the resolved default ceiling is non-null. [command | held-out]
   verify: `uv run pytest -k autopilot_agent_deadline` — a test injects an
   invoke-iteration whose underlying stream never yields a terminal message, runs a
   discovery/authoring cycle under default config, and asserts the call is cancelled with
   a timeout-classified outcome in bounded wall time; a second assertion asserts the
   default-resolved autopilot agent ceiling is non-null.
   defends against: treating `max_turns` (a turn budget) as the bound and leaving the
   `async for` with no `asyncio.wait_for`, so a stream that streams forever without ever
   spending a "turn" runs unbounded; or only bounding the cycle's `asyncio.run` without
   bounding the agent call inside it.

7. While the default `flywheel.toml` (no deadline overrides) is in effect, the resolved
   configuration yields a finite, non-null ceiling for each of the five call classes
   (agent iteration, rubric judge, command grader, docker management, autopilot agent),
   and an explicit operator override changes the resolved ceiling for that class.
   [command | held-out]
   verify: `uv run pytest -k deadline_config_defaults` — a test resolves config with no
   overrides and asserts each of the five ceilings is non-null and finite; then resolves
   config with an explicit override per class and asserts the resolved value reflects the
   override (including the unbounded/`0` opt-out remaining expressible).
   defends against: hard-coding the ceilings so they cannot be overridden (violates
   default-on-but-overridable), or shipping a default of `None`/unbounded for any class
   so "default-on" is faked by requiring the operator to set it.

8. The existing verification suite still passes after the change, and the agent SDK
   stays an optional extra: `import flywheel_core` succeeds with the SDK absent, and
   `flywheel_core.task` / `flywheel_core.lifecycle` remain pure. [command | held-out]
   (verification-surface)
   verify: `scripts/check.sh` is green (ruff -> pyright -> pytest), and the existing
   purity/optional-extra suites still pass —
   `uv run pytest -k "module_purity or sdk_optional or import_without"` — none weakened
   or deleted.
   defends against: placing the deadline primitive in `task`/`lifecycle` (breaking
   purity), adding a top-level SDK import to satisfy a deadline path, or making a
   criterion pass by relaxing an existing assertion.

Verification surface: this feature touches command-grader execution, which `/fw-plan`
turns into graders, and adds new tests. The existing suite still passes (criterion #8);
no check is relaxed, removed, or skipped; new behavior is proven by held-out command
checks the implementing agent did not author against its own known inputs (the timeout
tests assert a real bounded-wall-time outcome against a never-returning stub, which a
no-op cannot fake).

## Decomposition Hint (for /fw-plan)
This splits along the call-site layers, all consuming one shared invariant.

- Layer **deadline primitive** (`flywheel-core`, non-pure module — NOT `task`/`lifecycle`):
  satisfies #1. The single reusable wall-clock seam that bounds an awaitable / `async for`
  and raises a distinguishable timeout signal even while the stream is producing. Every
  other layer depends on this.
- Layer **deadline config / defaults**: satisfies #7; depends on the primitive. Resolves
  the five default-on, finite, overridable ceilings from config.
- Layer **agent-iteration deadline** (harness): satisfies #2; depends on primitive +
  config.
- Layer **rubric-judge deadline** (harness `_validate` -> rubric path): satisfies #3;
  depends on primitive + config.
- Layer **command-grader default timeout** (harness passes the default through to
  `run_command_graders`): satisfies #4; depends on config.
- Layer **docker-management deadline** (`flywheel-container`): satisfies #5; depends on
  config (its own default ceiling resolved consistently with the shared primitive's
  contract).
- Layer **autopilot-agent deadline** (`flywheel-orchestrator` invoker / cycle):
  satisfies #6; depends on primitive + config.

**Shared invariant:** the wall-clock deadline primitive (its public signature and its
distinguishable timeout signal) plus the resolved-config ceiling accessor are consumed
by every call-site layer. Name and land the primitive + config layer first so each
dependent layer adopts the same seam; a holdout (#8 / `scripts/check.sh`) composes the
dependents so no call site is left on a divergent ad-hoc timeout. Do NOT split the
primitive's definition across layers — one definition, many consumers.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Deadlines are default-on with generous, operator-overridable ceilings  (Status: Accepted)
- Context: every external call can currently hang forever; a knob that defaults off
  (today's `hang_timeout_seconds = None`) is no protection because nobody enables it.
  | Decision: each of the five call classes resolves a finite, non-null ceiling with no
  override required; an operator may raise/lower it or opt out (unbounded/`0`) per class.
- Rejected: default-off knobs (status quo — protection nobody turns on); a single global
  hard-coded timeout (not overridable, and one magnitude cannot fit a fast docker
  inspect and a long agent iteration). | Consequences: a too-tight default could kill a
  legitimately slow call — hence "generous" ceilings and per-class overridability; the
  cost is one more resolved-config surface to document.

### D-2: A wall-clock deadline, not an idle/silence timeout, is the bound  (Status: Accepted)
- Context: the shipped watchdog only fires on inter-message silence, so a steadily
  streaming-but-never-terminating agent or judge slips past it. | Decision: the bound is
  total wall-clock elapsed since the call started; it fires even while bytes are still
  arriving.
- Rejected: reusing the silence watchdog (provably gameable by a chatty infinite stream).
  | Consequences: the new bound is additive to, not a replacement for, the existing
  silence detector — both run.

### D-3: One reusable primitive, adopted by all call sites  (Status: Accepted)
- Context: five call sites, five ad-hoc timeouts would drift and re-introduce gaps.
  | Decision: a single wall-clock deadline primitive in core is the seam every site
  consumes. | Rejected: per-site bespoke `asyncio.wait_for` calls (drift, duplicated
  classification, no single place for the default contract). | Consequences: the
  primitive's signature and timeout signal are a shared invariant; dependents must adopt
  it together (enforced by ordering the primitive/config tasks first).

### D-4: Criteria stop at "the deadline fires," not at the retry policy  (Status: Accepted)
- Context: Phase 1 containment (assumed landed) already turns a timeout exception into a
  contained, retryable failure. | Decision: these criteria grade only that the deadline
  fires and surfaces a distinguishable timeout outcome within `ceiling + epsilon`; the
  retry/escalation behavior is out of scope here. | Rejected: re-grading containment
  (duplicates Phase 1). | Consequences: tasks here assume Phase-1 containment code
  exists; see Open Questions for the cross-phase prerequisite.

## Open Questions (accepted gaps)
- Cross-phase prerequisite: this phase assumes Phase 1 (containment) has landed so that a
  timeout-induced cancellation/exception is already caught and surfaced as a contained,
  retryable failure rather than crashing the loop. Phase 1's spec (`00065`) is authored
  by a sibling subagent and is not yet on disk at authoring time. If Phase 1's
  containment seam names a specific exception/classification the timeout signal must map
  to, the timeout primitive should raise into that seam; tasks here grade only that a
  *distinguishable timeout outcome* is surfaced, leaving the exact classification name to
  be reconciled at execution time via lifecycle records. This is the only accepted gap;
  it does not leave any criterion un-gradeable (each grades bounded wall-clock behavior
  independent of the downstream classification string).

## Next Steps
Run `/fw-plan 00066-FEATURE-universal-deadlines` to compile these criteria into flywheel
tasks and graders. Tasks stage under `.flywheel/tasks/staged/02-universal-deadlines/`
and must not activate until Phase 1 lands.
