"""Tests for the pure autopilot scoring engine (spec 00058, task autopilot-scoring).

Grades acceptance criteria #3 (legible, recomputable score breakdown), #4
(absolute preemptive override), and #5 (weighted scheduling within the 4-11
band, not strict tier order). The engine is pure, so these tests need no agent,
no I/O, and no clock.
"""

from __future__ import annotations

from flywheel_orchestrator._autopilot import (
    DEFAULT_WEIGHTS,
    TIER_WEIGHTS,
    Finding,
    ScoreWeights,
    Tier,
    recompute_final,
    score_finding,
    select_findings,
    sequence_findings,
)


def _finding(
    fid: str,
    tier: Tier,
    *,
    urgency: int = 0,
    importance: int = 0,
    blocks: int = 0,
    effort: int = 0,
    ready: bool = True,
) -> Finding:
    return Finding(
        id=fid,
        tier=tier,
        title=f"finding {fid}",
        urgency=urgency,
        importance=importance,
        blocks=blocks,
        effort=effort,
        ready=ready,
    )


# --- Tier model -------------------------------------------------------------


def test_tier_preemptive_boundary() -> None:
    assert all(Tier(t).is_preemptive for t in (1, 2, 3))
    assert all(not Tier(t).is_preemptive for t in range(4, 12))
    # The enum covers exactly tiers 1-11.
    assert [t.value for t in Tier] == list(range(1, 12))


# --- Criterion #3: legible, recomputable breakdown --------------------------


def test_breakdown_records_all_five_components_and_recomputes() -> None:
    f = _finding(
        "a", Tier.TECH_DEBT, urgency=4, importance=5, blocks=2, effort=3
    )
    bd = score_finding(f)
    # All five components present and equal to the finding's axes.
    assert bd.tier_weight == TIER_WEIGHTS[Tier.TECH_DEBT]
    assert bd.urgency == 4
    assert bd.importance == 5
    assert bd.blocks == 2
    assert bd.effort == 3
    # The final recomputes from the components under the documented formula.
    assert recompute_final(bd) == bd.final
    # Spelled out: TierWeight*w_tier + Urgency*w_urg + Importance*w_imp
    #              + Blocks*w_unblock - Effort*w_effort.
    expected = (
        TIER_WEIGHTS[Tier.TECH_DEBT] * DEFAULT_WEIGHTS.tier
        + 4 * DEFAULT_WEIGHTS.urgency
        + 5 * DEFAULT_WEIGHTS.importance
        + 2 * DEFAULT_WEIGHTS.unblock
        - 3 * DEFAULT_WEIGHTS.effort
    )
    assert bd.final == expected


def test_flipping_any_component_changes_the_final() -> None:
    base = score_finding(
        _finding("a", Tier.TECH_DEBT, urgency=1, importance=1, blocks=1, effort=1)
    )
    # Each of the four scheduled axes must move the final — a final that
    # ignores a component is a wrong implementation.
    assert score_finding(
        _finding("a", Tier.TECH_DEBT, urgency=2, importance=1, blocks=1, effort=1)
    ).final != base.final
    assert score_finding(
        _finding("a", Tier.TECH_DEBT, urgency=1, importance=2, blocks=1, effort=1)
    ).final != base.final
    assert score_finding(
        _finding("a", Tier.TECH_DEBT, urgency=1, importance=1, blocks=2, effort=1)
    ).final != base.final
    assert score_finding(
        _finding("a", Tier.TECH_DEBT, urgency=1, importance=1, blocks=1, effort=2)
    ).final != base.final


def test_effort_lowers_the_score() -> None:
    cheap = score_finding(_finding("cheap", Tier.TECH_DEBT, effort=0))
    pricey = score_finding(_finding("pricey", Tier.TECH_DEBT, effort=5))
    assert cheap.final > pricey.final


def test_preemptive_breakdown_recomputes_under_interrupt_branch() -> None:
    bd = score_finding(_finding("p", Tier.BROKEN_BUILD, urgency=7))
    assert bd.preemptive is True
    assert bd.final == DEFAULT_WEIGHTS.interrupt_base + 7 * DEFAULT_WEIGHTS.urgency
    assert recompute_final(bd) == bd.final


# --- Criterion #4: absolute preemptive override -----------------------------


def test_ready_tier_1_3_precede_all_scheduled_regardless_of_score() -> None:
    # One ready Tier-3 broken build (low axes) plus several high-scoring
    # Tier-5/Tier-8 findings.
    findings = [
        _finding("debt", Tier.TECH_DEBT, urgency=10, importance=10, blocks=10),
        _finding("feature", Tier.CORE_FEATURE_WORK, urgency=10, importance=10),
        _finding("build", Tier.BROKEN_BUILD, urgency=0),
    ]
    seq = sequence_findings(findings)
    assert seq[0].finding.id == "build"
    # Every preemptive finding precedes every scheduled one.
    first_scheduled = next(
        i for i, s in enumerate(seq) if not s.breakdown.preemptive
    )
    assert all(seq[i].breakdown.preemptive for i in range(first_scheduled))
    assert all(
        not seq[i].breakdown.preemptive
        for i in range(first_scheduled, len(seq))
    )


def test_two_ready_preemptive_both_precede_scheduled_stable() -> None:
    findings = [
        _finding("build-a", Tier.BROKEN_BUILD, urgency=1),
        _finding("feature", Tier.CORE_FEATURE_WORK, urgency=10, importance=10),
        _finding("build-b", Tier.BROKEN_BUILD, urgency=1),
    ]
    seq = sequence_findings(findings)
    ids = [s.finding.id for s in seq]
    # Both broken builds precede the feature; equal-urgency ties keep input
    # order (stable), never random.
    assert ids.index("build-a") < ids.index("feature")
    assert ids.index("build-b") < ids.index("feature")
    assert ids.index("build-a") < ids.index("build-b")


def test_non_ready_preemptive_does_not_float() -> None:
    # A Tier-2 finding that is not ready falls into the scheduled band.
    findings = [
        _finding("risk", Tier.IMMINENT_SEVERE_RISK, urgency=0, ready=False),
        _finding("build", Tier.BROKEN_BUILD, urgency=0, ready=True),
    ]
    seq = sequence_findings(findings)
    assert seq[0].finding.id == "build"
    risk = next(s for s in seq if s.finding.id == "risk")
    assert risk.breakdown.preemptive is False


# --- Criterion #5: weighted scheduling, not strict tier order ---------------


def test_higher_scoring_lower_tier_precedes_lower_scoring_higher_tier() -> None:
    # A Tier-8 with strong axes must beat a Tier-5 with weak axes.
    tier8 = _finding(
        "debt", Tier.TECH_DEBT, urgency=10, importance=10, blocks=10
    )
    tier5 = _finding("feature", Tier.CORE_FEATURE_WORK, urgency=0, effort=10)
    assert score_finding(tier8).final > score_finding(tier5).final
    seq = sequence_findings([tier5, tier8])
    assert seq[0].finding.id == "debt"


def test_scheduled_band_orders_by_score_not_tier() -> None:
    seq = sequence_findings(
        [
            _finding("t5", Tier.CORE_FEATURE_WORK, urgency=0, effort=10),
            _finding("t8", Tier.TECH_DEBT, urgency=10, importance=10, blocks=10),
        ]
    )
    assert [s.finding.id for s in seq] == ["t8", "t5"]


# --- Selection to depth -----------------------------------------------------


def test_select_findings_fills_to_slots_in_sequence_order() -> None:
    findings = [
        _finding("t5", Tier.CORE_FEATURE_WORK, urgency=1),
        _finding("build", Tier.BROKEN_BUILD, urgency=0),
        _finding("t8", Tier.TECH_DEBT, urgency=10, importance=10),
    ]
    chosen = select_findings(findings, slots=2)
    assert [s.finding.id for s in chosen] == ["build", "t8"]


def test_select_findings_zero_or_negative_slots_selects_nothing() -> None:
    findings = [_finding("build", Tier.BROKEN_BUILD)]
    assert select_findings(findings, slots=0) == []
    assert select_findings(findings, slots=-3) == []


def test_select_findings_caps_at_available() -> None:
    findings = [_finding("build", Tier.BROKEN_BUILD)]
    assert len(select_findings(findings, slots=5)) == 1


# --- Edge cases -------------------------------------------------------------


def test_empty_finding_set_yields_empty_sequence() -> None:
    assert sequence_findings([]) == []
    assert select_findings([], slots=4) == []


def test_weight_override_changes_ordering() -> None:
    # With effort heavily penalized, a cheap lower-value item can overtake an
    # expensive higher-value one — proving weights feed the engine.
    findings = [
        _finding("pricey", Tier.TECH_DEBT, importance=10, effort=10),
        _finding("cheap", Tier.TECH_DEBT, importance=6, effort=0),
    ]
    default_seq = sequence_findings(findings)
    assert default_seq[0].finding.id == "pricey"
    heavy_effort = ScoreWeights(effort=5.0)
    overridden = sequence_findings(findings, heavy_effort)
    assert overridden[0].finding.id == "cheap"
