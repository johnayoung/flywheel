"""Autopilot: the unattended intake half of the loop (spec 00058).

flywheel already *executes* graded tasks autonomously. Autopilot is the loop
that *authors* them: it discovers what a codebase needs (one relevance agent
per tier), sequences the findings by the ``docs/autopilot.md`` tier model, and
compiles each selected finding into a grader-bearing :class:`~flywheel_core.task.Task`
the existing worker then drains and lands.

This module owns the phase's shared, pure invariants — the data model every
later layer imports and **never redefines or extends**:

* :class:`Tier` — the 11 tiers of ``docs/autopilot.md`` with the preemptive
  (``<= 3``) vs scheduled (``>= 4``) boundary.
* :class:`Finding` — one concrete unit of work a tier agent surfaced, carrying
  its identity, tier, the five score axes (urgency, importance, blocks, effort
  — plus tier), a ``ready`` status (the ``Status == ready`` gate the preemptive
  override keys on), and the descriptive fields (title/detail/evidence) the
  authoring layer compiles from.
* :class:`ScoreBreakdown` — the legible, recorded score: the five components
  plus the final, so a recommendation is auditable rather than an opaque number.

The scoring engine here is **pure**: no file I/O, no agent calls, no
subprocess, no clock/random. Default weights are module constants; the
``[autopilot]`` override lands in the loop layer (autopilot-loop), which passes
a :class:`ScoreWeights` into these functions — this module never reads config.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from enum import IntEnum
from pathlib import Path, PurePosixPath
from typing import TYPE_CHECKING, Any, Literal

from flywheel_core.deadline import run_with_deadline
from flywheel_core.deadline_config import (
    DEFAULT_AUTOPILOT_AGENT_SECONDS,
    DeadlineClass,
    DeadlineConfig,
)
from flywheel_core.invoker import (
    IterationResult,
    ToolResultObservation,
    invoke_iteration,
)
from flywheel_core.lifecycle import Status
from flywheel_core.loaders import TaskLoadError, load_task_data, serialize_task
from flywheel_core.task import CommandGrader, Task

if TYPE_CHECKING:
    from flywheel_core._sdk import AgentDefinition, ClaudeAgentOptions
    from flywheel_core.store_postgres import PostgresStore
    from flywheel_core.store_sqlite import SqliteStore

# --- Tier model (docs/autopilot.md) -----------------------------------------


class Tier(IntEnum):
    """The 11 autopilot tiers, in priority order (1 highest).

    Tiers 1-3 are *preemptive interrupts*: while any such finding is ``ready``
    it floats above everything below. Tiers 4-11 are *scheduled* by weighted
    score, never by strict tier order. :attr:`is_preemptive` is the single
    boundary every layer keys on, so the ``<= 3`` rule lives in exactly one
    place.
    """

    PRODUCTION_DOWN = 1
    IMMINENT_SEVERE_RISK = 2
    BROKEN_BUILD = 3
    COMMITTED_DELIVERABLES = 4
    CORE_FEATURE_WORK = 5
    TEST_COVERAGE = 6
    NON_CRITICAL_BUGS = 7
    TECH_DEBT = 8
    OBSERVABILITY_TOOLING = 9
    DOCUMENTATION = 10
    POLISH = 11

    @property
    def is_preemptive(self) -> bool:
        """True for tiers 1-3 (the preemptive interrupt band)."""
        return self.value <= PREEMPTIVE_MAX_TIER


#: The inclusive upper bound of the preemptive band; tiers ``<= 3`` preempt.
PREEMPTIVE_MAX_TIER: int = 3


#: Per-tier base weight (``TierWeight[Tier]`` in the docs formula). A higher
#: tier (lower number) carries more weight; the scheduled band uses tiers
#: 4-11 (weights 8..1). Defined for all 11 tiers so a non-ready preemptive
#: finding still scores under the scheduled branch with a sensible weight.
TIER_WEIGHTS: dict[Tier, int] = {tier: 12 - tier.value for tier in Tier}


# --- Score weights (module constants; overridable by the loop layer) --------

#: ``INTERRUPT_BASE`` from the docs formula — large enough that any ready
#: preemptive finding's ``INTERRUPT_BASE + Urgency`` dominates every possible
#: scheduled score, so the preemptive override is absolute by arithmetic as
#: well as by partition.
INTERRUPT_BASE: float = 10_000.0

W_TIER: float = 10.0
W_URGENCY: float = 3.0
W_IMPORTANCE: float = 3.0
W_UNBLOCK: float = 2.0
W_EFFORT: float = 1.0


@dataclass(frozen=True, kw_only=True)
class ScoreWeights:
    """The tunable weights of the ``docs/autopilot.md`` scheduled-score formula.

    Defaults are the module constants (the shipped weights). The loop layer
    builds one of these from the ``[autopilot]`` table to override them; this
    module never reads config, so the override is always passed in explicitly.
    """

    tier: float = W_TIER
    urgency: float = W_URGENCY
    importance: float = W_IMPORTANCE
    unblock: float = W_UNBLOCK
    effort: float = W_EFFORT
    interrupt_base: float = INTERRUPT_BASE


DEFAULT_WEIGHTS: ScoreWeights = ScoreWeights()


# --- Findings ---------------------------------------------------------------


@dataclass(frozen=True, kw_only=True)
class Finding:
    """One concrete unit of work a tier relevance agent surfaced.

    Carries an identity, its tier, the five score axes, and a ``ready`` status
    (the ``Status == ready`` gate the preemptive override keys on), plus the
    descriptive fields the authoring layer compiles a task from
    (``title``/``detail``/``evidence``). Downstream layers (discovery,
    authoring, loop) import this shape and add no field to it.

    ``urgency``/``importance``/``blocks``/``effort`` are agent-supplied
    estimates; the *final score* is computed here from them and is never an
    agent-reported number.
    """

    id: str
    tier: Tier
    title: str
    detail: str = ""
    evidence: tuple[str, ...] = ()
    urgency: int = 0
    importance: int = 0
    blocks: int = 0
    effort: int = 0
    ready: bool = True


# --- Score breakdown (legible, recorded) ------------------------------------


@dataclass(frozen=True, kw_only=True)
class ScoreBreakdown:
    """The legible score recorded on every sequenced finding.

    Records the five components (``tier_weight``, ``urgency``, ``importance``,
    ``blocks``, ``effort``) plus the ``final`` so the recommendation is
    auditable: a test recomputes the documented formula from the recorded
    components and asserts it equals ``final``. ``preemptive`` records which
    formula branch produced ``final`` (the ``INTERRUPT_BASE + Urgency``
    interrupt branch, or the weighted scheduled branch), so the recompute is
    unambiguous.
    """

    tier: Tier
    tier_weight: int
    urgency: int
    importance: int
    blocks: int
    effort: int
    final: float
    preemptive: bool


@dataclass(frozen=True, kw_only=True)
class ScoredFinding:
    """A :class:`Finding` paired with the :class:`ScoreBreakdown` it earned."""

    finding: Finding
    breakdown: ScoreBreakdown


def score_finding(
    finding: Finding, weights: ScoreWeights = DEFAULT_WEIGHTS
) -> ScoreBreakdown:
    """Compute the :class:`ScoreBreakdown` for one finding.

    Implements the ``docs/autopilot.md`` formula verbatim:

    * a ready Tier 1-3 finding scores ``INTERRUPT_BASE + Urgency`` (preemptive
      branch — it floats above everything);
    * everything else scores
      ``TierWeight*w_tier + Urgency*w_urg + Importance*w_imp
      + BlocksCount*w_unblock - Effort*w_effort`` (scheduled branch).
    """
    tier_weight = TIER_WEIGHTS[finding.tier]
    preemptive = finding.tier.is_preemptive and finding.ready
    final = _compute_final(
        tier_weight=tier_weight,
        urgency=finding.urgency,
        importance=finding.importance,
        blocks=finding.blocks,
        effort=finding.effort,
        preemptive=preemptive,
        weights=weights,
    )
    return ScoreBreakdown(
        tier=finding.tier,
        tier_weight=tier_weight,
        urgency=finding.urgency,
        importance=finding.importance,
        blocks=finding.blocks,
        effort=finding.effort,
        final=final,
        preemptive=preemptive,
    )


def _compute_final(
    *,
    tier_weight: int,
    urgency: int,
    importance: int,
    blocks: int,
    effort: int,
    preemptive: bool,
    weights: ScoreWeights,
) -> float:
    """The two-branch scoring formula, applied to raw components."""
    if preemptive:
        return weights.interrupt_base + urgency * weights.urgency
    return (
        tier_weight * weights.tier
        + urgency * weights.urgency
        + importance * weights.importance
        + blocks * weights.unblock
        - effort * weights.effort
    )


def recompute_final(
    breakdown: ScoreBreakdown, weights: ScoreWeights = DEFAULT_WEIGHTS
) -> float:
    """Re-evaluate the documented formula from a breakdown's own components.

    The audit primitive behind criterion #3: feeding a recorded breakdown's
    five components (and its ``preemptive`` branch flag) back through the
    formula must reproduce its ``final``. A breakdown whose ``final`` ignores
    a component fails this round-trip.
    """
    return _compute_final(
        tier_weight=breakdown.tier_weight,
        urgency=breakdown.urgency,
        importance=breakdown.importance,
        blocks=breakdown.blocks,
        effort=breakdown.effort,
        preemptive=breakdown.preemptive,
        weights=weights,
    )


def sequence_findings(
    findings: list[Finding], weights: ScoreWeights = DEFAULT_WEIGHTS
) -> list[ScoredFinding]:
    """Sequence findings by the tier model: preemptive first, then weighted.

    Every ready Tier 1-3 finding precedes every Tier 4-11 finding regardless
    of the scheduled scores (the absolute preemptive override). Within each
    band, order is by descending final score; ties break deterministically by
    input order (Python's stable sort), never randomly. An empty input yields
    an empty sequence.
    """
    scored = [
        ScoredFinding(finding=f, breakdown=score_finding(f, weights))
        for f in findings
    ]
    preemptive = [s for s in scored if s.breakdown.preemptive]
    scheduled = [s for s in scored if not s.breakdown.preemptive]
    preemptive.sort(key=lambda s: s.breakdown.final, reverse=True)
    scheduled.sort(key=lambda s: s.breakdown.final, reverse=True)
    return preemptive + scheduled


def select_findings(
    findings: list[Finding],
    *,
    slots: int,
    weights: ScoreWeights = DEFAULT_WEIGHTS,
) -> list[ScoredFinding]:
    """Select the top ``slots`` findings in sequenced order (fill-to-depth).

    The loop layer computes ``slots`` as ``target_depth - current_depth`` and
    asks for that many; this returns the highest-priority findings up to that
    count (fewer when the input has fewer). A non-positive ``slots`` selects
    nothing.
    """
    if slots <= 0:
        return []
    return sequence_findings(findings, weights)[:slots]


# --- Relevance verdict (the per-tier discovery output) ----------------------


@dataclass(frozen=True, kw_only=True)
class TierVerdict:
    """One tier's relevance verdict plus the findings it surfaced.

    The structured output of the discovery fan-out (autopilot-discovery): each
    of the 11 tiers produces exactly one of these. A tier judged not relevant
    to the codebase carries ``relevant = False``, a ``reason``, and zero
    ``findings``; a relevant tier may still surface zero findings (distinct
    from not-relevant).
    """

    tier: Tier
    relevant: bool
    reason: str
    findings: tuple[Finding, ...] = ()


# --- Discovery: per-tier relevance fan-out (autopilot-discovery) -------------
#
# One agent invocation per tier. Each agent reads the repo, decides whether its
# tier is even relevant to *this* codebase (D-1: relevance is judged per-repo by
# an agent, never by coded detectors), and surfaces zero or more concrete
# findings with evidence. Agent access goes through an injectable seam built on
# ``flywheel_core.invoker.invoke_iteration`` so a test scripts the fan-out
# offline; the real driver resolves the SDK lazily through ``flywheel_core._sdk``.


#: The tier's remit handed to its relevance agent: the class name and the
#: one-line description from ``docs/autopilot.md``. The agent judges relevance
#: and surfaces findings against exactly this remit.
TIER_DESCRIPTIONS: dict[Tier, tuple[str, str]] = {
    Tier.PRODUCTION_DOWN: (
        "Production down / active harm",
        "Outages, in-progress data loss, active breach, payments failing. "
        "Nothing below advances while open.",
    ),
    Tier.IMMINENT_SEVERE_RISK: (
        "Imminent severe risk",
        "Actively/trivially exploitable vuln, spreading corruption, "
        "hard-deadline compliance violation, dependency about to break prod.",
    ),
    Tier.BROKEN_BUILD: (
        "Broken build / blocked pipeline",
        "CI red, main can't deploy, team blocked from shipping. Blocks "
        "everyone's forward motion.",
    ),
    Tier.COMMITTED_DELIVERABLES: (
        "Committed deliverables at risk",
        "Work with external commitments (customer deadlines, contracts, "
        "dependent teams) about to slip.",
    ),
    Tier.CORE_FEATURE_WORK: (
        "Core feature work",
        "The roadmap -- new functionality delivering primary value. Default "
        "steady state.",
    ),
    Tier.TEST_COVERAGE: (
        "Test coverage (shipped/in-flight)",
        "Tests for code that exists or is being written. Untested code "
        "becomes tomorrow's Tier 1.",
    ),
    Tier.NON_CRITICAL_BUGS: (
        "Non-critical bugs",
        "Known defects, not blocking or severe. System functions.",
    ),
    Tier.TECH_DEBT: (
        "Tech debt / refactoring",
        "Cleanup that improves velocity and reduces risk.",
    ),
    Tier.OBSERVABILITY_TOOLING: (
        "Observability & tooling",
        "Logging, metrics, dashboards, dev ergonomics. Compounding "
        "dividends, rarely urgent.",
    ),
    Tier.DOCUMENTATION: (
        "Documentation",
        "READMEs, API docs, runbooks, onboarding. Valuable, almost never "
        "time-critical.",
    ),
    Tier.POLISH: (
        "Polish / nice-to-have",
        "Cosmetic tweaks, minor optimizations, \"wouldn't it be nice.\"",
    ),
}


#: The injectable agent seam: a coroutine that takes a prompt and returns the
#: agent's response text. The default driver wraps
#: ``flywheel_core.invoker.invoke_iteration`` (SDK behind the lazy
#: ``flywheel_core._sdk`` boundary); tests inject a scripted coroutine returning
#: canned per-tier JSON so the fan-out is deterministic and offline.
AutopilotInvoker = Callable[[str], Awaitable[str]]

#: The injectable invoke-iteration seam the production builders drive. The
#: default is the real :func:`flywheel_core.invoker.invoke_iteration` (SDK behind
#: the lazy ``flywheel_core._sdk`` boundary); tests inject a never-terminating
#: stub to prove the wall-clock deadline fires even while the stream produces.
InvokeIterationFn = Callable[..., Awaitable[IterationResult]]

#: Default per-tier turn budget for the real SDK-backed invoker.
DEFAULT_DISCOVERY_MAX_TURNS: int = 60


async def _drive_agent_iteration(
    *,
    prompt: str,
    options: ClaudeAgentOptions,
    deadline_seconds: float | None,
    invoke: InvokeIterationFn,
) -> IterationResult:
    """Drive one autopilot agent iteration under the wall-clock deadline.

    Bounds the agent call -- the ``async for`` over the SDK stream inside
    :func:`flywheel_core.invoker.invoke_iteration` -- with the shared deadline
    primitive (spec 00066 criterion #6, D-2/D-3). ``max_turns`` is a turn budget,
    not a time budget: a stream that streams forever without ever spending a turn
    would otherwise run unbounded. The bound is total wall-clock elapsed; on
    timeout the call is cancelled and
    :class:`~flywheel_core.deadline.DeadlineExceeded` propagates so the cycle
    surfaces a distinguishable timeout outcome rather than parking the daemon.
    ``None`` (the operator per-class opt-out) restores the unbounded await.
    """
    call = invoke(prompt=prompt, options=options)
    if deadline_seconds is None:
        return await call
    return await run_with_deadline(call, deadline_seconds)


def build_repo_invoker(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_DISCOVERY_MAX_TURNS,
    deadline_seconds: float | None = DEFAULT_AUTOPILOT_AGENT_SECONDS,
    invoke: InvokeIterationFn = invoke_iteration,
) -> AutopilotInvoker:
    """Build the production agent seam: a Claude session rooted in ``repo_root``.

    The agent gets read access to the repo so it can judge tier relevance and
    surface findings. The SDK is resolved lazily inside the returned coroutine,
    so importing this module never requires the ``claude`` extra (the seam is
    only exercised when an unscripted autopilot run actually drives an agent).

    ``deadline_seconds`` is the wall-clock ceiling the agent call runs under
    (spec 00066 criterion #6); it defaults to the resolved-config autopilot
    ceiling so the call is bounded by default. ``invoke`` is the injectable
    invoke-iteration seam (tests substitute a never-terminating stub).
    """

    async def _invoke(prompt: str) -> str:
        from flywheel_core._sdk import ClaudeAgentOptions

        options = ClaudeAgentOptions(
            cwd=str(repo_root),
            add_dirs=[str(repo_root)],
            permission_mode="bypassPermissions",
            max_turns=max_turns,
            model=model,
        )
        result = await _drive_agent_iteration(
            prompt=prompt,
            options=options,
            deadline_seconds=deadline_seconds,
            invoke=invoke,
        )
        return result.transcript

    return _invoke


def tier_prompt(tier: Tier, repo_root: Path) -> str:
    """Build the relevance-and-findings prompt for one tier.

    The prompt names the tier (so a scripted invoker can route on ``TIER: N``),
    states its remit, and pins the structured-JSON output contract the fan-out
    parses. It explicitly authorizes a *not relevant* verdict so a tier that
    does not apply to this repo contributes nothing rather than inventing work.
    """
    name, description = TIER_DESCRIPTIONS[tier]
    return (
        f"You are the relevance agent for autopilot TIER: {tier.value} "
        f"({name}).\n\n"
        f"Tier remit: {description}\n\n"
        f"Read the repository at {repo_root} and decide whether THIS tier is "
        f"relevant to THIS codebase. Relevance is per-codebase: e.g. "
        f"\"production down\" is meaningless for a pure library. If the tier "
        f"does not apply, say so -- do not invent work to fill it.\n\n"
        f"If the tier is relevant, surface zero or more concrete findings, "
        f"each backed by evidence you actually observed in the repo. For each "
        f"finding estimate, on a 0-10 scale: urgency (how fast cost grows if "
        f"untouched), importance (value/risk-reduction if done), blocks (how "
        f"many other efforts it unblocks), effort (cost to do). Set ready=true "
        f"when the work can start now.\n\n"
        f"Respond with exactly one fenced JSON block:\n"
        f"```json\n"
        f"{{\n"
        f'  "relevant": true,\n'
        f'  "reason": "one sentence on why this tier does or does not apply",\n'
        f'  "findings": [\n'
        f'    {{"id": "short-slug", "title": "...", "detail": "...", '
        f'"evidence": ["path:line or observation"], "urgency": 0, '
        f'"importance": 0, "blocks": 0, "effort": 0, "ready": true}}\n'
        f"  ]\n"
        f"}}\n"
        f"```\n"
        f"A not-relevant verdict carries relevant=false, a reason, and an "
        f"empty findings list."
    )


def _extract_json_object(text: str) -> dict[str, Any]:
    """Extract the structured JSON object from an agent response.

    Prefers a ```` ```json ```` fenced block (the contract in
    :func:`tier_prompt`); falls back to the outermost ``{...}`` span so a model
    that omitted the fence still parses. Raises :class:`ValueError` when no
    JSON object is present so the caller records a parse failure rather than a
    silent empty verdict.
    """
    fence = "```json"
    start = text.find(fence)
    if start != -1:
        body_start = start + len(fence)
        end = text.find("```", body_start)
        if end != -1:
            candidate = text[body_start:end].strip()
            data = json.loads(candidate)
            if not isinstance(data, dict):
                raise ValueError("fenced JSON is not an object")
            return data
    # Fallback: the outermost brace span.
    first = text.find("{")
    last = text.rfind("}")
    if first == -1 or last == -1 or last <= first:
        raise ValueError("no JSON object found in agent response")
    data = json.loads(text[first : last + 1])
    if not isinstance(data, dict):
        raise ValueError("response JSON is not an object")
    return data


def _coerce_int(value: Any) -> int:
    """Coerce an agent-supplied score axis to a non-negative int, default 0."""
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        return max(0, value)
    if isinstance(value, float):
        return max(0, int(value))
    return 0


def _parse_findings(tier: Tier, raw: Any) -> tuple[Finding, ...]:
    """Build :class:`Finding` values for one tier from the agent's list.

    Each finding is stamped with ``tier`` here -- the agent never assigns its
    own tier, so a finding cannot land in a tier its relevance agent was not
    asked about. A malformed entry (not an object, or missing a title) is
    skipped rather than aborting the tier.
    """
    if not isinstance(raw, list):
        return ()
    findings: list[Finding] = []
    for idx, entry in enumerate(raw):
        if not isinstance(entry, dict):
            continue
        title = entry.get("title")
        if not isinstance(title, str) or not title.strip():
            continue
        raw_id = entry.get("id")
        slug = raw_id if isinstance(raw_id, str) and raw_id.strip() else str(idx)
        evidence = entry.get("evidence")
        evidence_tuple = (
            tuple(str(e) for e in evidence)
            if isinstance(evidence, list)
            else ()
        )
        detail = entry.get("detail")
        ready = entry.get("ready")
        findings.append(
            Finding(
                id=f"t{tier.value}-{slug}",
                tier=tier,
                title=title.strip(),
                detail=detail if isinstance(detail, str) else "",
                evidence=evidence_tuple,
                urgency=_coerce_int(entry.get("urgency")),
                importance=_coerce_int(entry.get("importance")),
                blocks=_coerce_int(entry.get("blocks")),
                effort=_coerce_int(entry.get("effort")),
                ready=ready if isinstance(ready, bool) else True,
            )
        )
    return tuple(findings)


def parse_tier_verdict(tier: Tier, text: str) -> TierVerdict:
    """Parse one tier agent's response into a :class:`TierVerdict`.

    A not-relevant verdict always carries zero findings: even if the agent
    contradicts itself and lists findings under ``relevant=false``, they are
    dropped here so a not-relevant tier can never contribute work (criterion
    #2). A response with no parseable JSON yields a not-relevant verdict with
    the parse error as its reason -- the fan-out records the failure rather
    than crashing.
    """
    try:
        data = _extract_json_object(text)
    except (ValueError, json.JSONDecodeError) as exc:
        return TierVerdict(
            tier=tier,
            relevant=False,
            reason=f"unparseable discovery response: {exc}",
            findings=(),
        )
    return _verdict_from_data(tier, data)


def _verdict_from_data(tier: Tier, data: dict[str, Any]) -> TierVerdict:
    """Build a :class:`TierVerdict` from an already-parsed verdict object.

    Shared by :func:`parse_tier_verdict` (one tier's own JSON) and the
    aggregated-final-message fallback (:func:`_parse_aggregated_verdicts`), so
    the relevance/reason/findings rules -- a not-relevant verdict always drops
    any findings the agent listed -- live in exactly one place.
    """
    relevant = bool(data.get("relevant"))
    reason_raw = data.get("reason")
    reason = reason_raw.strip() if isinstance(reason_raw, str) else ""
    if not relevant:
        return TierVerdict(
            tier=tier,
            relevant=False,
            reason=reason or "tier judged not relevant to this codebase",
            findings=(),
        )
    return TierVerdict(
        tier=tier,
        relevant=True,
        reason=reason or "tier judged relevant to this codebase",
        findings=_parse_findings(tier, data.get("findings")),
    )


async def discover_tier(
    tier: Tier, *, repo_root: Path, invoker: AutopilotInvoker
) -> TierVerdict:
    """Run one tier's relevance agent and parse its verdict.

    Any exception from the agent is contained and returned as a not-relevant
    verdict carrying the error reason, so one failing tier never aborts the
    fan-out (best-effort, mirroring the orchestrator's report posture).
    """
    try:
        text = await invoker(tier_prompt(tier, repo_root))
    except Exception as exc:  # noqa: BLE001 - best-effort fan-out per tier.
        return TierVerdict(
            tier=tier,
            relevant=False,
            reason=f"discovery agent error: {type(exc).__name__}: {exc}",
            findings=(),
        )
    return parse_tier_verdict(tier, text)


async def run_discovery(
    *,
    repo_root: Path,
    invoker: AutopilotInvoker | None = None,
    session_runner: DiscoverySessionRunner | None = None,
    model: str | None = None,
    deadline_seconds: float | None = DEFAULT_AUTOPILOT_AGENT_SECONDS,
) -> list[TierVerdict]:
    """Run discovery and return all 11 verdicts, in tier order.

    Two paths, one observable contract (exactly one verdict per tier, 1-11):

    * **Production (no scripted ``invoker``)** runs the single-session
      tier-subagent discovery (:func:`run_single_session_discovery`) -- ONE
      ``claude`` session whose ``options.agents`` registers all 11 tiers,
      collapsing the old 11-session fan-out into one process and one MCP boot
      (spec 00059). ``session_runner`` defaults to the SDK-backed runner; a test
      injects a scripted one to exercise the production route with no SDK.
    * **Tests (a scripted ``invoker`` is injected)** keep the per-tier fan-out
      unchanged: one ``discover_tier`` call per tier through the scripted seam,
      concurrent and best-effort.

    Both paths are best-effort: a tier whose agent raises or returns malformed
    output still yields a (not-relevant, error-reason) verdict, so the run
    always returns 11.
    """
    if invoker is None:
        return await run_single_session_discovery(
            repo_root=repo_root,
            session_runner=session_runner,
            model=model,
            deadline_seconds=deadline_seconds,
        )
    verdicts = await asyncio.gather(
        *(discover_tier(tier, repo_root=repo_root, invoker=invoker) for tier in Tier)
    )
    return list(verdicts)


# --- Single-session discovery: tier subagents in one session (spec 00059) -----
#
# The per-tier session fan-out above opens one ``claude`` process (and one MCP
# boot) per tier. Single-session discovery collapses that to ONE session whose
# ``ClaudeAgentOptions.agents`` registers all 11 tiers as subagents; the parent
# dispatches each via the built-in ``Agent`` (a.k.a. ``Task``) tool, and every
# verdict is read back from the subagent's tool-RESULT block in the streamed
# messages -- never from the parent's prose, which the SDK lets it re-summarize
# (D-2). A tier whose result is missing, errors, or fails to parse still yields
# a (not relevant, error-reason) verdict, so the run always returns 11.

#: The built-in subagent-dispatch tool. ``Task`` was renamed ``Agent`` in
#: Claude Code v2.1.63; both names appear in the stream, so both are accepted on
#: read and both are carried in ``allowed_tools`` on dispatch.
SUBAGENT_TOOL_NAMES: frozenset[str] = frozenset({"Agent", "Task"})

#: Read-only repo tools handed to each tier subagent: enough to judge relevance
#: by reading the repo, nothing that can mutate it or drive a browser.
SUBAGENT_READONLY_TOOLS: tuple[str, ...] = ("Read", "Grep", "Glob")

#: The ``AgentDefinition.effort`` levels the SDK accepts (camelCase enum).
DiscoveryEffort = Literal["low", "medium", "high", "xhigh", "max"]

#: A tier subagent is a relevance triage, not a deep task: a low effort and a
#: tight turn budget cap its per-tier cost (D-3).
DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS: int = 8
DEFAULT_DISCOVERY_SUBAGENT_EFFORT: DiscoveryEffort = "low"


#: The single-session seam: drive one discovery session for ``prompt`` and
#: return its drained :class:`IterationResult`. The default builds one
#: ``ClaudeSDKClient``-backed session (SDK behind ``flywheel_core._sdk``); tests
#: inject a coroutine returning a canned result with per-subagent tool-result
#: blocks, so the collector is exercised with no SDK and no live model.
DiscoverySessionRunner = Callable[[str], Awaitable[IterationResult]]


def tier_agent_key(tier: Tier) -> str:
    """The ``options.agents`` key (and ``subagent_type``) for one tier."""
    return f"tier-{tier.value}"


def _tier_from_agent_key(key: Any) -> Tier | None:
    """Map a dispatched ``subagent_type`` back to its :class:`Tier`.

    Returns ``None`` for any value that is not one of the 11 ``tier-N`` keys, so
    a stray tool call the parent makes (or a malformed dispatch) is ignored
    rather than mis-attributed to a tier.
    """
    if not isinstance(key, str) or not key.startswith("tier-"):
        return None
    try:
        value = int(key[len("tier-") :])
    except ValueError:
        return None
    try:
        return Tier(value)
    except ValueError:
        return None


def build_tier_agents(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
    effort: DiscoveryEffort = DEFAULT_DISCOVERY_SUBAGENT_EFFORT,
) -> dict[str, AgentDefinition]:
    """Build the 11 tier-relevance subagent definitions, keyed by tier.

    Each subagent carries the tier's relevance-and-findings remit as its system
    prompt, read-only repo tools, ``mcpServers=[]`` (no redundant
    ``playwright``/``proto`` boots, D-3), and a cheap budget. The SDK is
    resolved lazily so importing this module never requires the ``claude``
    extra.
    """
    from flywheel_core._sdk import AgentDefinition

    agents: dict[str, AgentDefinition] = {}
    for tier in Tier:
        name = TIER_DESCRIPTIONS[tier][0]
        agents[tier_agent_key(tier)] = AgentDefinition(
            description=f"Autopilot relevance triage for tier {tier.value} ({name}).",
            prompt=tier_prompt(tier, repo_root),
            tools=list(SUBAGENT_READONLY_TOOLS),
            mcpServers=[],
            model=model,
            maxTurns=max_turns,
            effort=effort,
            permissionMode="bypassPermissions",
        )
    return agents


def orchestrator_prompt(repo_root: Path) -> str:
    """Build the deterministic dispatcher prompt for the parent session.

    The parent is a dispatcher, not a judge: it must fan out every one of the 11
    tier subagents exactly once via the ``Agent`` tool and must NOT pre-judge,
    skip, or merge any tier. It also emits an aggregated final JSON purely as the
    fallback the collector reads only when it cannot key a tool-result to a tier
    (spec 00059 D-2 supersession); the per-tier tool-results remain the trusted
    source.
    """
    keys = ", ".join(tier_agent_key(tier) for tier in Tier)
    return (
        "You are autopilot's discovery dispatcher for the repository at "
        f"{repo_root}.\n\n"
        f"Eleven tier-relevance subagents are registered: {keys}.\n\n"
        "Use the Agent tool (a.k.a. Task) to dispatch EVERY one of these 11 "
        "subagents exactly once, each with subagent_type set to its key. Tell "
        "each subagent to read the repository, judge whether its tier is "
        "relevant to THIS codebase, and return its verdict as the fenced JSON "
        "block its own instructions define. Dispatch all 11 regardless of your "
        "own opinion -- do not skip, merge, or pre-judge any tier. The per-tier "
        "tool-results are read directly, so you need not restate them.\n\n"
        "After all 11 subagents return, emit exactly one final fenced JSON block "
        "aggregating every verdict as a safety net:\n"
        "```json\n"
        '{"verdicts": [{"tier": 1, "relevant": false, "reason": "...", '
        '"findings": []}]}\n'
        "```"
    )


def build_single_session_options(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_DISCOVERY_MAX_TURNS,
    subagent_max_turns: int = DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
    subagent_effort: DiscoveryEffort = DEFAULT_DISCOVERY_SUBAGENT_EFFORT,
) -> ClaudeAgentOptions:
    """Build the ONE session's options with all 11 tiers as subagents.

    The parent carries the ``Agent``/``Task`` dispatch tool plus the read-only
    repo tools in ``allowed_tools``; the 11 tier subagents are registered under
    ``agents``. The SDK is resolved lazily through ``flywheel_core._sdk``.
    """
    from flywheel_core._sdk import ClaudeAgentOptions

    return ClaudeAgentOptions(
        cwd=str(repo_root),
        add_dirs=[str(repo_root)],
        permission_mode="bypassPermissions",
        max_turns=max_turns,
        model=model,
        allowed_tools=[*sorted(SUBAGENT_TOOL_NAMES), *SUBAGENT_READONLY_TOOLS],
        agents=build_tier_agents(
            repo_root,
            model=model,
            max_turns=subagent_max_turns,
            effort=subagent_effort,
        ),
    )


def build_single_session_runner(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_DISCOVERY_MAX_TURNS,
    subagent_max_turns: int = DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
    subagent_effort: DiscoveryEffort = DEFAULT_DISCOVERY_SUBAGENT_EFFORT,
    deadline_seconds: float | None = DEFAULT_AUTOPILOT_AGENT_SECONDS,
    invoke: InvokeIterationFn = invoke_iteration,
) -> DiscoverySessionRunner:
    """Build the production single-session runner rooted at ``repo_root``.

    The returned coroutine drives one agent session (one ``claude`` process,
    one MCP boot) whose options register the 11 tier subagents, and returns the
    drained :class:`IterationResult` so the collector can read each verdict from
    its subagent tool-result block. The SDK is resolved lazily inside the
    coroutine.

    ``deadline_seconds`` is the wall-clock ceiling the discovery session runs
    under (spec 00066 criterion #6); it defaults to the resolved-config
    autopilot ceiling so the session is bounded by default. ``invoke`` is the
    injectable invoke-iteration seam (tests substitute a never-terminating
    stub).
    """

    async def _run(prompt: str) -> IterationResult:
        options = build_single_session_options(
            repo_root,
            model=model,
            max_turns=max_turns,
            subagent_max_turns=subagent_max_turns,
            subagent_effort=subagent_effort,
        )
        return await _drive_agent_iteration(
            prompt=prompt,
            options=options,
            deadline_seconds=deadline_seconds,
            invoke=invoke,
        )

    return _run


def _tool_result_text(observation: ToolResultObservation | None) -> str | None:
    """Extract the subagent's response text from its tool-result block.

    A subagent returns its final message as the ``Agent`` tool result, which the
    stream surfaces either as a plain string or as a list of content-block dicts
    (``{"type": "text", "text": ...}``). Returns ``None`` when the observation is
    absent, errored, or carries no text, so the caller records a missing-result
    verdict rather than parsing an empty string.
    """
    if observation is None or observation.is_error:
        return None
    content = observation.content
    if content is None:
        return None
    if isinstance(content, str):
        return content or None
    parts: list[str] = []
    for block in content:
        if isinstance(block, dict):
            text = block.get("text")
            if isinstance(text, str):
                parts.append(text)
    return "".join(parts) if parts else None


def _parse_aggregated_verdicts(text: str) -> dict[Tier, TierVerdict]:
    """Parse the orchestrator's aggregated final JSON into per-tier verdicts.

    The D-2 fallback: when a subagent tool-result cannot be keyed to its tier,
    the parent's final ``{"verdicts": [...]}`` block is parsed once. Returns the
    verdicts it could read, keyed by tier; an absent or malformed block yields an
    empty mapping so the caller falls through to the error verdict.
    """
    try:
        data = _extract_json_object(text)
    except (ValueError, json.JSONDecodeError):
        return {}
    raw = data.get("verdicts")
    if not isinstance(raw, list):
        return {}
    out: dict[Tier, TierVerdict] = {}
    for entry in raw:
        if not isinstance(entry, dict):
            continue
        tier_value = entry.get("tier")
        if isinstance(tier_value, bool) or not isinstance(tier_value, int):
            continue
        try:
            tier = Tier(tier_value)
        except ValueError:
            continue
        out[tier] = _verdict_from_data(tier, entry)
    return out


#: The collector source: which path produced the 11 verdicts. ``tool_result``
#: when every filled tier came from its subagent tool-result; ``aggregated``
#: when the final-JSON fallback supplied them; ``mixed`` when both contributed;
#: ``error_fallback`` when nothing keyed and no aggregate parsed.
COLLECT_SOURCE_TOOL_RESULT: str = "tool_result"
COLLECT_SOURCE_AGGREGATED: str = "aggregated"
COLLECT_SOURCE_MIXED: str = "mixed"
COLLECT_SOURCE_ERROR: str = "error_fallback"


@dataclass(frozen=True, kw_only=True)
class DiscoveryCollection:
    """The 11 collected verdicts plus a record of which path produced them.

    ``verdicts`` is exactly one :class:`TierVerdict` per tier in tier order.
    ``source`` records whether the trusted per-subagent tool-results, the
    aggregated-final-JSON fallback, or a mix produced them (spec 00059: "record
    which path was used").
    """

    verdicts: tuple[TierVerdict, ...]
    source: str


def collect_tier_verdicts(result: IterationResult) -> DiscoveryCollection:
    """Collect 11 tier verdicts from one discovery session's drained result.

    Reads each verdict from its subagent ``Agent``/``Task`` tool-RESULT block
    (keyed to its tier by ``subagent_type``), never from the parent's prose
    (D-2). A tier with a present-but-unparseable/errored result yields a
    (not relevant, error-reason) verdict; a tier with NO keyed tool-result is
    filled from the parent's aggregated final JSON, and if that too is absent it
    yields a (not relevant) error verdict. Always returns exactly 11 verdicts in
    tier order.
    """
    from_tool: dict[Tier, TierVerdict] = {}
    for interaction in result.signals.tool_interactions:
        if interaction.tool_name not in SUBAGENT_TOOL_NAMES:
            continue
        tier = _tier_from_agent_key(interaction.tool_input.get("subagent_type"))
        if tier is None:
            continue
        text = _tool_result_text(interaction.result)
        if text is None:
            from_tool[tier] = TierVerdict(
                tier=tier,
                relevant=False,
                reason="subagent tool-result missing, errored, or empty",
                findings=(),
            )
            continue
        from_tool[tier] = parse_tier_verdict(tier, text)

    missing = [tier for tier in Tier if tier not in from_tool]
    aggregated = (
        _parse_aggregated_verdicts(result.transcript) if missing else {}
    )

    verdicts: list[TierVerdict] = []
    used_tool = used_aggregated = used_error = False
    for tier in Tier:
        if tier in from_tool:
            verdicts.append(from_tool[tier])
            used_tool = True
        elif tier in aggregated:
            verdicts.append(aggregated[tier])
            used_aggregated = True
        else:
            verdicts.append(
                TierVerdict(
                    tier=tier,
                    relevant=False,
                    reason=(
                        "no discovery result for this tier: subagent was not "
                        "dispatched and no aggregated verdict was emitted"
                    ),
                    findings=(),
                )
            )
            used_error = True

    return DiscoveryCollection(
        verdicts=tuple(verdicts),
        source=_collect_source(used_tool, used_aggregated, used_error),
    )


def _collect_source(
    used_tool: bool, used_aggregated: bool, used_error: bool
) -> str:
    """Summarize which path(s) produced the collected verdicts."""
    if used_tool and not used_aggregated and not used_error:
        return COLLECT_SOURCE_TOOL_RESULT
    if used_aggregated and not used_tool and not used_error:
        return COLLECT_SOURCE_AGGREGATED
    if not used_tool and not used_aggregated:
        return COLLECT_SOURCE_ERROR
    return COLLECT_SOURCE_MIXED


async def run_single_session_discovery(
    *,
    repo_root: Path,
    session_runner: DiscoverySessionRunner | None = None,
    model: str | None = None,
    deadline_seconds: float | None = DEFAULT_AUTOPILOT_AGENT_SECONDS,
) -> list[TierVerdict]:
    """Run discovery as ONE session of tier subagents; return all 11 verdicts.

    Drives a single session (default: the production ``ClaudeSDKClient``-backed
    runner rooted at ``repo_root``; tests inject a scripted runner) whose
    ``options.agents`` registers the 11 tiers, then collects each verdict from
    its subagent tool-result block. Returns exactly one verdict per tier (1-11)
    in tier order -- a tier whose result is missing or malformed still yields a
    (not relevant, error-reason) verdict, mirroring :func:`run_discovery`.
    """
    runner = (
        session_runner
        if session_runner is not None
        else build_single_session_runner(
            repo_root, model=model, deadline_seconds=deadline_seconds
        )
    )
    result = await runner(orchestrator_prompt(repo_root))
    return list(collect_tier_verdicts(result).verdicts)


# --- Authoring: compile a finding into grader-bearing tasks (autopilot-authoring)
#
# The integrity crux of the phase. Each selected finding is compiled -- entirely
# by an agent following the fw-spec / fw-plan contracts headlessly -- into one or
# more validated flywheel tasks. The authoritative grader on every emitted task
# must be either a pre-existing committed repo check or a registered held-out
# oracle, never a check the same task's own diff creates (criterion #8: the
# "entirely by agents" path must not emit self-attestation wearing a grader's
# clothes). A finding that cannot be lowered to such a task is DROPPED with a
# recorded reason, never written as a goal-only or grader-less stub (criterion #1).

#: The authoritative grader points at a check already committed in the repo.
GRADER_SOURCE_REPO_COMMAND: str = "repo_command"
#: The authoritative grader is a registered held-out oracle (out of the agent's
#: reach), declared by an absolute path outside the agent's worktree.
GRADER_SOURCE_HELD_OUT: str = "held_out_oracle"
_GRADER_SOURCES: frozenset[str] = frozenset(
    {GRADER_SOURCE_REPO_COMMAND, GRADER_SOURCE_HELD_OUT}
)

#: Default turn budget for the real SDK-backed authoring invoker.
DEFAULT_AUTHORING_MAX_TURNS: int = 120

#: Commit-before-done discipline that ``/fw-plan`` bakes into hand-authored
#: tasks (fw-plan template). The autopilot authoring prompt does not, so every
#: emitted task gets it injected deterministically -- a task that finishes with
#: uncommitted work is verified green against a dirty tree yet silently fails to
#: land (the worker parks it). Phrasing mirrors the fw-plan example constraint.
COMMIT_BEFORE_DONE_CONSTRAINT: str = (
    "Commit the change with a clear message before reporting done"
)


@dataclass(frozen=True, kw_only=True)
class EmittedTask:
    """One validated, grader-bearing task autopilot authored from a finding.

    ``task`` is the compiled core :class:`~flywheel_core.task.Task` (it loaded
    through the authoritative validator and carries at least one grader).
    ``authoritative_grader`` is the run string of the out-of-band command grader
    the work lands on; ``grader_source`` records whether it is a pre-existing
    repo check or a held-out oracle; ``grader_target`` is the pre-existing check
    it resolves to. ``creates_files`` are the files the task's own diff is
    expected to create -- recorded so the self-attestation guard can prove the
    authoritative grader does not target any of them. ``assumptions`` are the
    ambiguities the authoring agent resolved itself (D-2), recorded with the
    emitted task rather than edited into a spec.
    """

    finding: Finding
    task: Task
    authoritative_grader: str
    grader_source: str
    grader_target: str
    creates_files: tuple[str, ...] = ()
    assumptions: tuple[str, ...] = ()
    held_out_oracle_path: str | None = None
    prerequisites: tuple[str, ...] = ()


@dataclass(frozen=True, kw_only=True)
class DroppedFinding:
    """A finding autopilot could not compile to a grader-bearing task.

    Recorded with a ``reason`` instead of emitting a vaguer or grader-less task
    (criterion #1 / D-2). The loop persists the reason so a clean repo or an
    ungradeable finding is auditable rather than silently filler-filled.
    """

    finding: Finding
    reason: str


@dataclass(frozen=True, kw_only=True)
class AuthoringResult:
    """The outcome of compiling one or more findings: emitted tasks + drops."""

    emitted: tuple[EmittedTask, ...] = ()
    dropped: tuple[DroppedFinding, ...] = ()


def authoring_prompt(finding: Finding, repo_root: Path) -> str:
    """Build the headless authoring prompt for one selected finding.

    Carries the load-bearing integrity rule: the authoritative grader must be a
    pre-existing committed check (or a held-out oracle), never a check this
    task's own diff creates. Ambiguity is resolved as a recorded assumption, not
    by emitting a vaguer task (D-2).
    """
    tier_name = TIER_DESCRIPTIONS[finding.tier][0]
    evidence = "\n".join(f"  - {e}" for e in finding.evidence) or "  (none)"
    return (
        f"You are autopilot's headless authoring agent. Compile the finding "
        f"below into one or more flywheel tasks following the fw-spec and "
        f"fw-plan contracts -- a one-sentence goal and the strongest "
        f"out-of-band grader you can express.\n\n"
        f"Repository: {repo_root}\n"
        f"Finding (Tier {finding.tier.value}, {tier_name}): {finding.title}\n"
        f"Detail: {finding.detail}\n"
        f"Evidence:\n{evidence}\n\n"
        f"HARD INTEGRITY RULES:\n"
        f"- Every emitted task must carry at least one grader.\n"
        f"- The authoritative grader MUST be either (a) a command that invokes "
        f"a check already committed in this repo (a test/lint/build command "
        f"that exists BEFORE this task runs), or (b) a registered held-out "
        f"oracle by absolute path. It must NEVER be a brand-new check this "
        f"task's own diff creates -- that is self-attestation.\n"
        f"- Resolve any ambiguity a human would be asked about as a recorded "
        f"assumption; never emit a vaguer task to dodge a question.\n"
        f"- If you cannot express an out-of-band grader for this finding, do "
        f"not invent one: set \"dropped\" to a one-sentence reason and emit no "
        f"task.\n\n"
        f"Respond with exactly one fenced JSON block:\n"
        f"```json\n"
        f"{{\n"
        f'  "tasks": [\n'
        f"    {{\n"
        f'      "task": {{"id": "kebab-id", "goal": "one sentence", '
        f'"graders": [{{"type": "command", "run": "<existing repo check>"}}], '
        f'"tags": [], "context": {{}}}},\n'
        f'      "authoritative_grader": "<run string matching a command grader '
        f'above>",\n'
        f'      "grader_source": "{GRADER_SOURCE_REPO_COMMAND}",\n'
        f'      "grader_target": "<repo-relative path to the pre-existing check '
        f'the grader runs>",\n'
        f'      "creates_files": ["<paths this task\'s diff will create>"],\n'
        f'      "assumptions": ["<ambiguity you resolved yourself>"]\n'
        f"    }}\n"
        f"  ],\n"
        f'  "dropped": ""\n'
        f"}}\n"
        f"```"
    )


def _ensure_commit_before_done(task: Task) -> None:
    """Inject the commit-before-done constraint on an authored task (in place).

    Idempotent: a no-op when the task already carries a constraint mentioning a
    commit, so an agent that authored its own commit discipline is not
    duplicated. Constraints are advisory prompt text (the agent sees them under
    ``## Constraints``); the hard enforcement is the landable-change gate.
    """
    constraints = task.context.constraints
    if any("commit" in c.lower() for c in constraints):
        return
    constraints.append(COMMIT_BEFORE_DONE_CONSTRAINT)


def _validate_task_entry(
    finding: Finding, entry: Any, repo_root: Path
) -> tuple[EmittedTask | None, str | None]:
    """Validate one authored-task entry; return ``(emitted, drop_reason)``.

    Exactly one of the two is non-``None``. The checks are the deterministic
    enforcement of criteria #1 and #8 -- a self-authored or grader-less entry
    is dropped, never emitted.
    """
    if not isinstance(entry, dict):
        return None, "authored entry is not an object"
    task_data = entry.get("task")
    if not isinstance(task_data, dict):
        return None, "authored entry has no task object"
    try:
        task = load_task_data(task_data, source=f"autopilot:{finding.id}")
    except TaskLoadError as exc:
        return None, f"authored task failed validation: {exc}"
    if not task.graders:
        return None, "authored task carries no grader (criterion #1)"
    _ensure_commit_before_done(task)

    authoritative = entry.get("authoritative_grader")
    if not isinstance(authoritative, str) or not authoritative.strip():
        return None, "authored task names no authoritative grader"
    command_runs = {
        g.run for g in task.graders if isinstance(g, CommandGrader)
    }
    if authoritative not in command_runs:
        return (
            None,
            "authoritative grader is not a command grader on the emitted task",
        )

    source = entry.get("grader_source")
    if source not in _GRADER_SOURCES:
        return None, f"unknown grader_source {source!r}"
    target = entry.get("grader_target")
    if not isinstance(target, str) or not target.strip():
        return None, "authoritative grader names no pre-existing target"

    raw_creates = entry.get("creates_files")
    creates = (
        tuple(str(c) for c in raw_creates if isinstance(c, str))
        if isinstance(raw_creates, list)
        else ()
    )

    # Self-attestation guard (criterion #8): the authoritative grader must not
    # name -- nor resolve to -- a file this task's own diff creates.
    for created in creates:
        if created and created in authoritative:
            return (
                None,
                "authoritative grader names a file this task's own diff "
                "creates (self-attestation)",
            )
        if created and created == target:
            return (
                None,
                "authoritative grader target is a file this task's own diff "
                "creates (self-attestation)",
            )

    held_out_oracle_path: str | None = None
    if source == GRADER_SOURCE_REPO_COMMAND:
        # The grader must point at a check that pre-exists in the repo -- proof
        # it is out-of-band, not created by the run it grades.
        if not (repo_root / target).exists():
            return (
                None,
                "authoritative grader target does not pre-exist in the repo "
                "(not an out-of-band check)",
            )
    else:  # GRADER_SOURCE_HELD_OUT
        oracle = Path(target)
        if not oracle.is_absolute() or not oracle.exists():
            return (
                None,
                "held-out oracle target is not an existing absolute path",
            )
        held_out_oracle_path = str(oracle)

    raw_assumptions = entry.get("assumptions")
    assumptions = (
        tuple(str(a) for a in raw_assumptions if isinstance(a, str))
        if isinstance(raw_assumptions, list)
        else ()
    )

    # ``prerequisites`` is an orchestration-layer edge core drops on load, so
    # read it straight from the authored task source (mirroring the directory
    # work source) and carry it on the emitted task for emission to preserve.
    raw_prereqs = task_data.get("prerequisites")
    prerequisites = (
        tuple(str(p) for p in raw_prereqs if isinstance(p, str))
        if isinstance(raw_prereqs, list)
        else ()
    )

    return (
        EmittedTask(
            finding=finding,
            task=task,
            authoritative_grader=authoritative,
            grader_source=source,
            grader_target=target,
            creates_files=creates,
            assumptions=assumptions,
            held_out_oracle_path=held_out_oracle_path,
            prerequisites=prerequisites,
        ),
        None,
    )


async def author_finding(
    finding: Finding, *, repo_root: Path, invoker: AutopilotInvoker
) -> AuthoringResult:
    """Compile one finding into emitted tasks (or a recorded drop).

    Drives the authoring agent through the injectable seam, parses its
    structured output, and validates every authored task against criteria #1
    and #8. A finding whose agent call raises, whose response is unparseable, or
    that yields no valid grader-bearing task is dropped with a recorded reason
    rather than written.
    """
    try:
        text = await invoker(authoring_prompt(finding, repo_root))
    except Exception as exc:  # noqa: BLE001 - best-effort per finding.
        return AuthoringResult(
            dropped=(
                DroppedFinding(
                    finding=finding,
                    reason=f"authoring agent error: {type(exc).__name__}: {exc}",
                ),
            )
        )
    try:
        data = _extract_json_object(text)
    except (ValueError, json.JSONDecodeError) as exc:
        return AuthoringResult(
            dropped=(
                DroppedFinding(
                    finding=finding,
                    reason=f"unparseable authoring response: {exc}",
                ),
            )
        )

    raw_tasks = data.get("tasks")
    entries = raw_tasks if isinstance(raw_tasks, list) else []
    emitted: list[EmittedTask] = []
    drop_reasons: list[str] = []
    for entry in entries:
        task, reason = _validate_task_entry(finding, entry, repo_root)
        if task is not None:
            emitted.append(task)
        elif reason is not None:
            drop_reasons.append(reason)

    if emitted:
        return AuthoringResult(emitted=tuple(emitted))

    # No valid task: prefer the agent's own drop reason, else the first
    # validation failure, else a generic note.
    agent_drop = data.get("dropped")
    reason = (
        agent_drop.strip()
        if isinstance(agent_drop, str) and agent_drop.strip()
        else (drop_reasons[0] if drop_reasons else "no grader-bearing task authored")
    )
    return AuthoringResult(
        dropped=(DroppedFinding(finding=finding, reason=reason),)
    )


async def author_findings(
    findings: list[Finding], *, repo_root: Path, invoker: AutopilotInvoker
) -> AuthoringResult:
    """Author every finding in order; aggregate emitted tasks and drops."""
    emitted: list[EmittedTask] = []
    dropped: list[DroppedFinding] = []
    for finding in findings:
        result = await author_finding(
            finding, repo_root=repo_root, invoker=invoker
        )
        emitted.extend(result.emitted)
        dropped.extend(result.dropped)
    return AuthoringResult(emitted=tuple(emitted), dropped=tuple(dropped))


# --- The single refill pass: compose discovery -> score -> author -> emit ----
#
# autopilot-loop's orchestration: one pass that fills the work source up to the
# target queue depth from actionable findings. Structured as a callable the
# daemon (autopilot-daemon) invokes in a loop -- no infinite loop here. On a
# clean repo (no actionable finding) it writes zero tasks and returns cleanly
# (D-5): the single pass under `--once` exits 0.

#: Default target queue depth when the [autopilot] table omits one.
DEFAULT_TARGET_DEPTH: int = 5
#: Default landing posture (FF-merge autonomy is the shipped default, D-3).
DEFAULT_LANDING: str = "merge"
#: The phase directory autopilot emits authored tasks into.
AUTOPILOT_PHASE: str = "autopilot"


@dataclass(frozen=True, kw_only=True)
class AutopilotPassResult:
    """The recorded outcome of one refill pass, for logs and the daemon.

    ``emitted_paths`` are the task files written into the work source this pass;
    ``emitted`` the corresponding authored tasks; ``dropped`` the findings that
    could not be compiled. ``relevant_tiers`` / ``not_relevant_tiers`` record
    the per-tier relevance verdicts so a clean repo is auditable. ``reason``
    summarizes why the pass emitted what it did.
    """

    emitted_paths: tuple[Path, ...] = ()
    emitted: tuple[EmittedTask, ...] = ()
    dropped: tuple[DroppedFinding, ...] = ()
    queue_depth_before: int = 0
    target_depth: int = 0
    relevant_tiers: tuple[Tier, ...] = ()
    not_relevant_tiers: tuple[Tier, ...] = ()
    landing: str = DEFAULT_LANDING
    reason: str = ""

    @property
    def emitted_count(self) -> int:
        return len(self.emitted_paths)


#: Build-manifest / lockfile basenames that make poor conflict keys: many
#: legitimately-distinct tasks land against the same manifest, so serializing on
#: it would collapse unrelated work into one lane. Conflict keys want the
#: specific source file a task contends for, not the shared package manifest.
_MANIFEST_BASENAMES: frozenset[str] = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "go.mod",
        "go.sum",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    }
)


def _conflict_key_for_path(raw: str) -> str | None:
    """Normalize a repo-relative path into a stable conflict-key token, or
    ``None`` when it is too coarse to be a useful claim-time conflict resource.

    Returns ``None`` for a path that is empty, absolute, parent-escaping,
    directory-like (no file suffix -- e.g. ``crates/``), or a shared build
    manifest / lockfile (see :data:`_MANIFEST_BASENAMES`). A specific source
    file (``crates/infrared-feed/src/tycho.rs``) returns its normalized posix
    form so two tasks scoped to it serialize.
    """
    s = raw.strip()
    if not s:
        return None
    p = PurePosixPath(s)
    if p.is_absolute() or any(part == ".." for part in p.parts):
        return None
    if p.suffix == "":  # directory-like (or extensionless): too coarse
        return None
    if p.name in _MANIFEST_BASENAMES:
        return None
    return p.as_posix()


def _conflict_keys_for(emitted: EmittedTask) -> list[str]:
    """Claim-time conflict keys for an emitted task: the repo source files it
    contends for, so two autopilot tasks scoped to the same file serialize at
    claim time instead of racing -- the dedupe-swarm failure mode (spec 00061
    Gap 3, spec 00064 P2).

    Derived deterministically from ``grader_target`` (the file the task's
    authoritative check resolves to -- the signal that collapsed the 13-task
    ``tycho.rs`` swarm) and ``creates_files``, never from an agent-reported
    value. Coarse targets (build manifests, directories) are dropped so
    unrelated work that merely shares a manifest is not over-serialized. Empty
    when neither yields a specific source file.
    """
    keys: set[str] = set()
    target = _conflict_key_for_path(emitted.grader_target)
    if target is not None:
        keys.add(target)
    for created in emitted.creates_files:
        norm = _conflict_key_for_path(created)
        if norm is not None:
            keys.add(norm)
    return sorted(keys)


def _emitted_task_file(
    emitted: EmittedTask, breakdown: ScoreBreakdown
) -> dict[str, Any]:
    """Serialize an emitted task to the directory work source's file shape.

    The body is the core task (``serialize_task``) plus the orchestration-layer
    keys the directory source reads from top-level JSON: ``priority`` (derived
    from the recorded final score so the scheduler orders autopilot work),
    ``prerequisites``, and ``conflict_keys`` (the source files the task contends
    for, so overlapping tasks serialize at claim time -- omitted when empty).
    The full :class:`ScoreBreakdown` is recorded under an
    ``autopilot`` key (ignored by the core loader) so the recommendation stays
    inspectable after the run -- the legible-score requirement (#3) persisted.
    """
    body = serialize_task(emitted.task)
    body["priority"] = int(round(breakdown.final))
    if emitted.prerequisites:
        body["prerequisites"] = list(emitted.prerequisites)
    conflict_keys = _conflict_keys_for(emitted)
    if conflict_keys:
        body["conflict_keys"] = conflict_keys
    body["autopilot"] = {
        "finding_id": emitted.finding.id,
        "tier": breakdown.tier.value,
        "tier_weight": breakdown.tier_weight,
        "urgency": breakdown.urgency,
        "importance": breakdown.importance,
        "blocks": breakdown.blocks,
        "effort": breakdown.effort,
        "final": breakdown.final,
        "preemptive": breakdown.preemptive,
        "authoritative_grader": emitted.authoritative_grader,
        "grader_source": emitted.grader_source,
        "grader_target": emitted.grader_target,
        "assumptions": list(emitted.assumptions),
    }
    return body


def emit_emitted_task(
    emitted: EmittedTask,
    breakdown: ScoreBreakdown,
    *,
    tasks_dir: Path,
    phase: str = AUTOPILOT_PHASE,
) -> Path | None:
    """Write one emitted task into the directory work source; return its path.

    Lands the file under ``<tasks_dir>/active/<phase>/<task_id>.json`` so the
    existing worker drains it (the directory adapter lists it on its next
    pass). Returns ``None`` without overwriting when a file for that task id
    already exists, so re-running autopilot does not duplicate or clobber
    in-flight work.
    """
    phase_dir = tasks_dir / "active" / phase
    phase_dir.mkdir(parents=True, exist_ok=True)
    target = phase_dir / f"{emitted.task.id}.json"
    if target.exists():
        return None
    target.write_text(
        json.dumps(_emitted_task_file(emitted, breakdown), indent=2) + "\n",
        encoding="utf-8",
    )
    return target


async def run_refill_pass(
    *,
    tasks_dir: Path,
    repo_root: Path,
    target_depth: int = DEFAULT_TARGET_DEPTH,
    discovery_invoker: AutopilotInvoker | None = None,
    authoring_invoker: AutopilotInvoker | None = None,
    weights: ScoreWeights = DEFAULT_WEIGHTS,
    landing: str = DEFAULT_LANDING,
    model: str | None = None,
    queue_depth: Callable[[Path], int] | None = None,
    deadlines: DeadlineConfig | None = None,
) -> AutopilotPassResult:
    """Run one refill pass: discovery -> score/select -> author -> emit.

    While the live queue depth is below ``target_depth`` and actionable
    findings remain, emits authored tasks until the target is met or the
    actionable findings are exhausted. When discovery yields nothing actionable
    the pass writes ZERO tasks and returns cleanly -- never raising, never
    emitting filler (D-5). It does not loop on an interval; the daemon
    (autopilot-daemon) calls this once per cycle.

    ``queue_depth`` measures the current work-source depth; it defaults to the
    directory adapter's listing count. The agent seams default to the real
    SDK-backed invokers rooted at ``repo_root``; tests inject scripted ones.
    """
    # Default-on, operator-overridable wall-clock ceiling for the discovery and
    # authoring agent calls (spec 00066 criterion #6, D-1/D-3): resolve it once
    # and feed it to both the authoring invoker and the discovery session so a
    # stalled SDK stream is cancelled rather than parking the daemon.
    resolved_deadlines = deadlines if deadlines is not None else DeadlineConfig()
    autopilot_ceiling = resolved_deadlines.for_class(DeadlineClass.AUTOPILOT_AGENT)

    depth_fn = queue_depth if queue_depth is not None else _directory_queue_depth
    current = depth_fn(tasks_dir)
    slots = target_depth - current
    if slots <= 0:
        return AutopilotPassResult(
            queue_depth_before=current,
            target_depth=target_depth,
            landing=landing,
            reason=(
                f"queue depth {current} already at or above target "
                f"{target_depth}; nothing emitted"
            ),
        )

    author = (
        authoring_invoker
        if authoring_invoker is not None
        else build_repo_invoker(
            repo_root,
            model=model,
            max_turns=DEFAULT_AUTHORING_MAX_TURNS,
            deadline_seconds=autopilot_ceiling,
        )
    )

    # Discovery routes itself: a scripted ``discovery_invoker`` (tests) drives
    # the per-tier fan-out; ``None`` (production) drives the single-session
    # tier-subagent path. Either way ``run_discovery`` returns exactly 11
    # verdicts.
    verdicts = await run_discovery(
        repo_root=repo_root,
        invoker=discovery_invoker,
        model=model,
        deadline_seconds=autopilot_ceiling,
    )
    relevant_tiers = tuple(v.tier for v in verdicts if v.relevant)
    not_relevant_tiers = tuple(v.tier for v in verdicts if not v.relevant)
    findings = [f for v in verdicts if v.relevant for f in v.findings]

    sequenced = sequence_findings(findings, weights)
    if not sequenced:
        return AutopilotPassResult(
            queue_depth_before=current,
            target_depth=target_depth,
            relevant_tiers=relevant_tiers,
            not_relevant_tiers=not_relevant_tiers,
            landing=landing,
            reason="no actionable findings this cycle; idling without emitting",
        )

    emitted: list[EmittedTask] = []
    emitted_paths: list[Path] = []
    dropped: list[DroppedFinding] = []
    breakdown_by_finding = {s.finding.id: s.breakdown for s in sequenced}

    # Author the top-sequenced findings until the queue reaches the target or
    # the actionable findings run out; only enough to fill the depth-to-target.
    for scored in sequenced:
        if len(emitted_paths) >= slots:
            break
        result = await author_finding(
            scored.finding, repo_root=repo_root, invoker=author
        )
        dropped.extend(result.dropped)
        for et in result.emitted:
            breakdown = breakdown_by_finding[et.finding.id]
            path = emit_emitted_task(et, breakdown, tasks_dir=tasks_dir)
            if path is not None:
                emitted.append(et)
                emitted_paths.append(path)

    reason = (
        f"emitted {len(emitted_paths)} task(s) to fill queue from {current} "
        f"toward target {target_depth}"
        if emitted_paths
        else "no grader-bearing task could be authored this cycle"
    )
    return AutopilotPassResult(
        emitted_paths=tuple(emitted_paths),
        emitted=tuple(emitted),
        dropped=tuple(dropped),
        queue_depth_before=current,
        target_depth=target_depth,
        relevant_tiers=relevant_tiers,
        not_relevant_tiers=not_relevant_tiers,
        landing=landing,
        reason=reason,
    )


def _directory_queue_depth(tasks_dir: Path) -> int:
    """Current work-source depth: the directory adapter's active-item count."""
    from flywheel_orchestrator._sources import DirectoryWorkSource

    return len(DirectoryWorkSource(tasks_dir).list_work())


#: The lifecycle statuses that mean a task is finished work, not queued work
#: (the two states with no outgoing transition in ``_VALID_EDGES``).
_TERMINAL_STATUSES: tuple[Status, ...] = (Status.DONE, Status.FAILED)


def actionable_queue_depth(
    tasks_dir: Path, store: SqliteStore | PostgresStore
) -> int:
    """Active task files whose task has no terminal lifecycle (spec 00062).

    The refill decision needs *actionable* depth -- work the worker can still
    drive -- not the raw active-file count :func:`_directory_queue_depth`
    returns. A finished task's JSON lingers under ``active/<phase>/`` until its
    WHOLE phase archives (archival is all-or-nothing per phase, gated on every
    task reaching ``DONE``), so counting terminal tasks pins depth at target and
    suppresses refill; worse, a single terminally-``FAILED`` task -- now
    reachable via the spec-00061 landable-change gate -- never archives and
    would wedge intake permanently. Excluding ``DONE``/``FAILED`` tasks
    decouples "is the queue full?" from "is the phase fully archived?".

    Resilient by construction: a per-task store read that fails counts the task
    as actionable (conservative -- never under-reports depth) so a transient
    store hiccup can never crash a daemon cycle or spuriously over-emit.
    """
    from flywheel_orchestrator._sources import load_active_tasks

    depth = 0
    for _path, task in load_active_tasks(tasks_dir):
        try:
            terminal = store.list_lifecycles(
                statuses=_TERMINAL_STATUSES, task_id=task.id
            )
        except Exception:  # noqa: BLE001 - depth must never crash a cycle
            terminal = []
        if not terminal:
            depth += 1
    return depth


__all__ = [
    "AUTOPILOT_PHASE",
    "COLLECT_SOURCE_AGGREGATED",
    "COLLECT_SOURCE_ERROR",
    "COLLECT_SOURCE_MIXED",
    "COLLECT_SOURCE_TOOL_RESULT",
    "DEFAULT_AUTHORING_MAX_TURNS",
    "DEFAULT_DISCOVERY_MAX_TURNS",
    "DEFAULT_DISCOVERY_SUBAGENT_EFFORT",
    "DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS",
    "DEFAULT_LANDING",
    "DEFAULT_TARGET_DEPTH",
    "DEFAULT_WEIGHTS",
    "INTERRUPT_BASE",
    "PREEMPTIVE_MAX_TIER",
    "GRADER_SOURCE_HELD_OUT",
    "GRADER_SOURCE_REPO_COMMAND",
    "SUBAGENT_READONLY_TOOLS",
    "SUBAGENT_TOOL_NAMES",
    "TIER_DESCRIPTIONS",
    "TIER_WEIGHTS",
    "W_EFFORT",
    "W_IMPORTANCE",
    "W_TIER",
    "W_UNBLOCK",
    "W_URGENCY",
    "AuthoringResult",
    "AutopilotInvoker",
    "AutopilotPassResult",
    "DiscoveryCollection",
    "DiscoverySessionRunner",
    "DroppedFinding",
    "EmittedTask",
    "Finding",
    "ScoreBreakdown",
    "ScoreWeights",
    "ScoredFinding",
    "Tier",
    "TierVerdict",
    "author_finding",
    "author_findings",
    "authoring_prompt",
    "build_repo_invoker",
    "build_single_session_options",
    "build_single_session_runner",
    "build_tier_agents",
    "actionable_queue_depth",
    "collect_tier_verdicts",
    "discover_tier",
    "emit_emitted_task",
    "orchestrator_prompt",
    "parse_tier_verdict",
    "recompute_final",
    "run_discovery",
    "run_refill_pass",
    "run_single_session_discovery",
    "tier_agent_key",
    "score_finding",
    "select_findings",
    "sequence_findings",
    "tier_prompt",
]
