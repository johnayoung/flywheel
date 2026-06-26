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
from pathlib import Path
from typing import Any

from flywheel_core.invoker import invoke_iteration
from flywheel_core.loaders import TaskLoadError, load_task_data
from flywheel_core.task import CommandGrader, Task

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

#: Default per-tier turn budget for the real SDK-backed invoker.
DEFAULT_DISCOVERY_MAX_TURNS: int = 60


def build_repo_invoker(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_DISCOVERY_MAX_TURNS,
) -> AutopilotInvoker:
    """Build the production agent seam: a Claude session rooted in ``repo_root``.

    The agent gets read access to the repo so it can judge tier relevance and
    surface findings. The SDK is resolved lazily inside the returned coroutine,
    so importing this module never requires the ``claude`` extra (the seam is
    only exercised when an unscripted autopilot run actually drives an agent).
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
        result = await invoke_iteration(prompt=prompt, options=options)
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
    model: str | None = None,
) -> list[TierVerdict]:
    """Fan out one relevance agent per tier and return all 11 verdicts.

    Returns exactly one verdict per tier (1-11) in tier order. ``invoker``
    defaults to the production SDK-backed seam rooted at ``repo_root``; tests
    pass a scripted coroutine. The fan-out is concurrent and best-effort: a
    tier whose agent raises still yields a (not-relevant, error-reason)
    verdict, so the run always returns 11 verdicts.
    """
    seam = invoker if invoker is not None else build_repo_invoker(
        repo_root, model=model
    )
    verdicts = await asyncio.gather(
        *(discover_tier(tier, repo_root=repo_root, invoker=seam) for tier in Tier)
    )
    return list(verdicts)


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


__all__ = [
    "DEFAULT_AUTHORING_MAX_TURNS",
    "DEFAULT_DISCOVERY_MAX_TURNS",
    "DEFAULT_WEIGHTS",
    "INTERRUPT_BASE",
    "PREEMPTIVE_MAX_TIER",
    "GRADER_SOURCE_HELD_OUT",
    "GRADER_SOURCE_REPO_COMMAND",
    "TIER_DESCRIPTIONS",
    "TIER_WEIGHTS",
    "W_EFFORT",
    "W_IMPORTANCE",
    "W_TIER",
    "W_UNBLOCK",
    "W_URGENCY",
    "AuthoringResult",
    "AutopilotInvoker",
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
    "discover_tier",
    "parse_tier_verdict",
    "recompute_final",
    "run_discovery",
    "score_finding",
    "select_findings",
    "sequence_findings",
    "tier_prompt",
]
