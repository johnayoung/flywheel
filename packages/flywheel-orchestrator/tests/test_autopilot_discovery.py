"""Tests for the autopilot tier discovery fan-out (spec 00058, autopilot-discovery).

Grades acceptance criterion #2: one relevance verdict per tier (1-11); a tier
judged not-relevant to the codebase contributes zero findings and records its
not-relevant reason. The fan-out drives agents through an injectable seam, so a
scripted invoker makes the whole test deterministic and offline -- no live
model, no network.
"""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path

from flywheel_orchestrator._autopilot import (
    Finding,
    Tier,
    parse_tier_verdict,
    run_discovery,
)

# A library-shaped fixture repo: the production/deploy tiers do not apply.
# Each tier's scripted result is keyed by tier value.
_NOT_RELEVANT_TIERS = {1, 2, 3, 4}


def _library_invoker_response(tier_value: int) -> str:
    """Canned per-tier JSON for a pure-library repo.

    Production/imminent-risk/broken-build/committed-deliverables tiers are
    marked not-relevant; the rest surface a finding.
    """
    if tier_value in _NOT_RELEVANT_TIERS:
        body = {
            "relevant": False,
            "reason": f"tier {tier_value} does not apply to a pure library",
            "findings": [],
        }
    else:
        body = {
            "relevant": True,
            "reason": f"tier {tier_value} applies",
            "findings": [
                {
                    "id": "f1",
                    "title": f"finding for tier {tier_value}",
                    "detail": "detail",
                    "evidence": ["src/lib.py:10"],
                    "urgency": 3,
                    "importance": 4,
                    "blocks": 0,
                    "effort": 2,
                    "ready": True,
                }
            ],
        }
    return f"Here is my assessment.\n```json\n{json.dumps(body)}\n```\n"


def _make_scripted_invoker():
    """A scripted invoker that routes on the ``TIER: N`` marker in the prompt."""
    seen: list[int] = []

    async def _invoke(prompt: str) -> str:
        match = re.search(r"TIER: (\d+)", prompt)
        assert match is not None, "prompt must name its tier"
        tier_value = int(match.group(1))
        seen.append(tier_value)
        return _library_invoker_response(tier_value)

    return _invoke, seen


def test_exactly_one_verdict_per_tier() -> None:
    invoker, seen = _make_scripted_invoker()
    verdicts = asyncio.run(
        run_discovery(repo_root=Path("/repo"), invoker=invoker)
    )
    # Exactly one verdict per tier, all 11 tiers, in tier order.
    assert [v.tier for v in verdicts] == list(Tier)
    assert sorted(seen) == list(range(1, 12))


def test_not_relevant_tiers_contribute_zero_findings_with_reason() -> None:
    invoker, _ = _make_scripted_invoker()
    verdicts = asyncio.run(
        run_discovery(repo_root=Path("/repo"), invoker=invoker)
    )
    by_tier = {v.tier.value: v for v in verdicts}
    for tier_value in _NOT_RELEVANT_TIERS:
        verdict = by_tier[tier_value]
        assert verdict.relevant is False
        assert verdict.findings == ()
        assert verdict.reason  # a recorded not-relevant reason


def test_relevant_tiers_surface_findings_stamped_with_their_tier() -> None:
    invoker, _ = _make_scripted_invoker()
    verdicts = asyncio.run(
        run_discovery(repo_root=Path("/repo"), invoker=invoker)
    )
    for verdict in verdicts:
        if verdict.relevant:
            assert len(verdict.findings) == 1
            # The fan-out stamps each finding with the tier it asked about;
            # the agent never assigns its own tier.
            assert all(f.tier == verdict.tier for f in verdict.findings)


def test_not_relevant_verdict_drops_any_findings_the_agent_lists() -> None:
    # Even if an agent contradicts itself (relevant=false but lists findings),
    # the parser drops them so a not-relevant tier can never contribute work.
    contradictory = json.dumps(
        {
            "relevant": False,
            "reason": "does not apply",
            "findings": [{"id": "x", "title": "sneaky", "urgency": 9}],
        }
    )
    verdict = parse_tier_verdict(
        Tier.PRODUCTION_DOWN, f"```json\n{contradictory}\n```"
    )
    assert verdict.relevant is False
    assert verdict.findings == ()


def test_relevant_with_no_findings_is_distinct_from_not_relevant() -> None:
    empty_relevant = json.dumps(
        {"relevant": True, "reason": "applies but nothing actionable now",
         "findings": []}
    )
    verdict = parse_tier_verdict(
        Tier.CORE_FEATURE_WORK, f"```json\n{empty_relevant}\n```"
    )
    assert verdict.relevant is True
    assert verdict.findings == ()


def test_one_raising_tier_does_not_abort_the_fan_out() -> None:
    async def _invoke(prompt: str) -> str:
        match = re.search(r"TIER: (\d+)", prompt)
        assert match is not None
        tier_value = int(match.group(1))
        if tier_value == 5:
            raise RuntimeError("agent exploded on tier 5")
        return _library_invoker_response(tier_value)

    verdicts = asyncio.run(run_discovery(repo_root=Path("/repo"), invoker=_invoke))
    # Still exactly 11 verdicts; the raising tier is a not-relevant error verdict.
    assert len(verdicts) == 11
    by_tier = {v.tier.value: v for v in verdicts}
    assert by_tier[5].relevant is False
    assert "agent error" in by_tier[5].reason
    # Other tiers still returned their real verdicts.
    assert by_tier[6].relevant is True


def test_unparseable_response_yields_not_relevant_error_verdict() -> None:
    verdict = parse_tier_verdict(Tier.TECH_DEBT, "no json here at all")
    assert verdict.relevant is False
    assert verdict.findings == ()
    assert "unparseable" in verdict.reason


def test_findings_carry_the_full_score_axes() -> None:
    body = json.dumps(
        {
            "relevant": True,
            "reason": "applies",
            "findings": [
                {
                    "id": "slug",
                    "title": "real finding",
                    "urgency": 7,
                    "importance": 8,
                    "blocks": 2,
                    "effort": 1,
                    "ready": False,
                }
            ],
        }
    )
    verdict = parse_tier_verdict(Tier.TEST_COVERAGE, f"```json\n{body}\n```")
    assert len(verdict.findings) == 1
    f = verdict.findings[0]
    assert isinstance(f, Finding)
    assert (f.urgency, f.importance, f.blocks, f.effort) == (7, 8, 2, 1)
    assert f.ready is False
    assert f.id == "t6-slug"  # stamped with tier value + slug
