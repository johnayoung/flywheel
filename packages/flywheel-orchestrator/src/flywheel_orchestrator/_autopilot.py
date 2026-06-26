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

from dataclasses import dataclass
from enum import IntEnum

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


__all__ = [
    "DEFAULT_WEIGHTS",
    "INTERRUPT_BASE",
    "PREEMPTIVE_MAX_TIER",
    "TIER_WEIGHTS",
    "W_EFFORT",
    "W_IMPORTANCE",
    "W_TIER",
    "W_UNBLOCK",
    "W_URGENCY",
    "Finding",
    "ScoreBreakdown",
    "ScoreWeights",
    "ScoredFinding",
    "Tier",
    "TierVerdict",
    "recompute_final",
    "score_finding",
    "select_findings",
    "sequence_findings",
]
