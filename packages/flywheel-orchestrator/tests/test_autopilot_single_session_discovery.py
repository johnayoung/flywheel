"""Tests for single-session tier-subagent discovery (spec 00059).

Discovery runs as ONE session whose ``options.agents`` registers the 11 tiers
as subagents; the parent dispatches each via the built-in ``Agent`` (a.k.a.
``Task``) tool, and every verdict is read back from the subagent's tool-RESULT
block -- never the parent's prose. These tests drive the collector and the
single-session function through injected, canned tool-result blocks so the whole
suite is deterministic and offline (no SDK import, no live model). The few
structural assertions over the real ``AgentDefinition`` / ``ClaudeAgentOptions``
shapes ``importorskip`` the SDK so the offline path always runs.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from flywheel_core.envelope import parse_envelope
from flywheel_core.invoker import (
    InvocationSignals,
    IterationResult,
    ToolInteraction,
    ToolResultObservation,
)

from flywheel_orchestrator._autopilot import (
    COLLECT_SOURCE_AGGREGATED,
    COLLECT_SOURCE_MIXED,
    COLLECT_SOURCE_TOOL_RESULT,
    SUBAGENT_TOOL_NAMES,
    Tier,
    build_single_session_options,
    build_tier_agents,
    collect_tier_verdicts,
    run_single_session_discovery,
    tier_agent_key,
)

# --- canned tool-result helpers (no SDK) ------------------------------------


def _verdict_json(tier_value: int, *, relevant: bool = True) -> str:
    """A fenced per-tier verdict JSON block as a subagent would return it."""
    if relevant:
        body = {
            "relevant": True,
            "reason": f"tier {tier_value} applies",
            "findings": [
                {
                    "id": "f1",
                    "title": f"finding for tier {tier_value}",
                    "evidence": ["src/lib.py:1"],
                    "urgency": 3,
                    "importance": 4,
                    "blocks": 0,
                    "effort": 2,
                    "ready": True,
                }
            ],
        }
    else:
        body = {
            "relevant": False,
            "reason": f"tier {tier_value} does not apply",
            "findings": [],
        }
    return f"Subagent summary.\n```json\n{json.dumps(body)}\n```\n"


def _agent_interaction(
    tier_value: int,
    *,
    tool_name: str = "Agent",
    text: str | None = None,
    is_error: bool | None = None,
    missing_result: bool = False,
) -> ToolInteraction:
    """Build one ``Agent``/``Task`` dispatch + its tool-result observation."""
    tool_use_id = f"tu-{tier_value}"
    if missing_result:
        result = None
    else:
        result = ToolResultObservation(
            tool_use_id=tool_use_id,
            is_error=is_error,
            content=text if text is not None else _verdict_json(tier_value),
        )
    return ToolInteraction(
        tool_use_id=tool_use_id,
        tool_name=tool_name,
        tool_input={
            "subagent_type": tier_agent_key(Tier(tier_value)),
            "prompt": f"evaluate tier {tier_value}",
        },
        result=result,
    )


def _result(
    interactions: list[ToolInteraction], *, transcript: str = ""
) -> IterationResult:
    """Wrap canned tool interactions in an IterationResult (SDK-free)."""
    signals = InvocationSignals(
        stop_reason=None,
        num_turns=None,
        total_cost_usd=None,
        result_is_error=None,
        result_subtype=None,
        api_error_status=None,
        session_id=None,
        tool_interactions=tuple(interactions),
    )
    return IterationResult(
        transcript=transcript,
        messages=(),
        envelope=parse_envelope(transcript),
        signals=signals,
    )


# --- collector: verdicts from tool-result blocks ----------------------------


def test_collects_eleven_verdicts_from_tool_results_in_tier_order() -> None:
    interactions = [_agent_interaction(t.value) for t in Tier]
    collection = collect_tier_verdicts(_result(interactions))
    assert [v.tier for v in collection.verdicts] == list(Tier)
    assert collection.source == COLLECT_SOURCE_TOOL_RESULT
    assert all(v.relevant for v in collection.verdicts)


def test_both_task_and_agent_tool_names_parse_identically() -> None:
    via_agent = collect_tier_verdicts(
        _result([_agent_interaction(t.value, tool_name="Agent") for t in Tier])
    )
    via_task = collect_tier_verdicts(
        _result([_agent_interaction(t.value, tool_name="Task") for t in Tier])
    )
    assert [
        (v.tier, v.relevant, v.reason) for v in via_agent.verdicts
    ] == [(v.tier, v.relevant, v.reason) for v in via_task.verdicts]
    assert "Agent" in SUBAGENT_TOOL_NAMES and "Task" in SUBAGENT_TOOL_NAMES


def test_tool_result_content_as_block_list_is_concatenated() -> None:
    # The subagent result can arrive as a list of content-block dicts.
    blocks = [
        {"type": "text", "text": "intro "},
        {"type": "text", "text": _verdict_json(5)},
    ]
    interaction = ToolInteraction(
        tool_use_id="tu-5",
        tool_name="Agent",
        tool_input={"subagent_type": tier_agent_key(Tier.CORE_FEATURE_WORK)},
        result=ToolResultObservation(
            tool_use_id="tu-5", is_error=None, content=blocks
        ),
    )
    collection = collect_tier_verdicts(_result([interaction]))
    by_tier = {v.tier: v for v in collection.verdicts}
    assert by_tier[Tier.CORE_FEATURE_WORK].relevant is True


def test_verdicts_come_from_tool_results_not_the_parents_prose() -> None:
    # Criterion 5: the parent's final prose contradicts / omits the per-tier
    # JSON, yet the parsed verdicts match the tool-results.
    interactions = [
        _agent_interaction(
            t.value,
            text=_verdict_json(t.value, relevant=(t.value % 2 == 0)),
        )
        for t in Tier
    ]
    misleading_prose = (
        "All tiers are irrelevant.\n```json\n"
        + json.dumps(
            {
                "verdicts": [
                    {"tier": t.value, "relevant": False, "reason": "lies",
                     "findings": []}
                    for t in Tier
                ]
            }
        )
        + "\n```\n"
    )
    collection = collect_tier_verdicts(
        _result(interactions, transcript=misleading_prose)
    )
    # Tool-results win: even tiers relevant, odd tiers not -- not the prose.
    by_tier = {v.tier.value: v for v in collection.verdicts}
    for tier_value, verdict in by_tier.items():
        assert verdict.relevant is (tier_value % 2 == 0)
    assert collection.source == COLLECT_SOURCE_TOOL_RESULT


def test_missing_tool_result_yields_error_verdict_still_eleven() -> None:
    interactions = [
        _agent_interaction(t.value, missing_result=True)
        if t is Tier.TECH_DEBT
        else _agent_interaction(t.value)
        for t in Tier
    ]
    collection = collect_tier_verdicts(_result(interactions))
    assert len(collection.verdicts) == 11
    by_tier = {v.tier: v for v in collection.verdicts}
    assert by_tier[Tier.TECH_DEBT].relevant is False
    assert by_tier[Tier.TECH_DEBT].findings == ()
    assert by_tier[Tier.TECH_DEBT].reason


def test_errored_tool_result_yields_not_relevant() -> None:
    interactions = [
        _agent_interaction(t.value, is_error=True, text="boom")
        if t is Tier.POLISH
        else _agent_interaction(t.value)
        for t in Tier
    ]
    collection = collect_tier_verdicts(_result(interactions))
    by_tier = {v.tier: v for v in collection.verdicts}
    assert by_tier[Tier.POLISH].relevant is False
    assert by_tier[Tier.POLISH].findings == ()


def test_malformed_tool_result_yields_not_relevant_error_verdict() -> None:
    interactions = [
        _agent_interaction(t.value, text="no json here at all")
        if t is Tier.DOCUMENTATION
        else _agent_interaction(t.value)
        for t in Tier
    ]
    collection = collect_tier_verdicts(_result(interactions))
    by_tier = {v.tier: v for v in collection.verdicts}
    assert by_tier[Tier.DOCUMENTATION].relevant is False
    assert "unparseable" in by_tier[Tier.DOCUMENTATION].reason


def test_each_tier_present_exactly_once_even_with_no_interactions() -> None:
    collection = collect_tier_verdicts(_result([]))
    assert [v.tier for v in collection.verdicts] == list(Tier)
    assert all(v.relevant is False for v in collection.verdicts)


def test_non_subagent_tool_calls_are_ignored() -> None:
    stray = ToolInteraction(
        tool_use_id="x",
        tool_name="Read",
        tool_input={"file_path": "/repo/README.md"},
        result=ToolResultObservation(
            tool_use_id="x", is_error=None, content="contents"
        ),
    )
    interactions = [stray, *[_agent_interaction(t.value) for t in Tier]]
    collection = collect_tier_verdicts(_result(interactions))
    assert len(collection.verdicts) == 11
    assert collection.source == COLLECT_SOURCE_TOOL_RESULT


# --- aggregated-final-JSON fallback (D-2 supersession) ----------------------


def test_aggregated_fallback_when_no_tool_results_key_to_tiers() -> None:
    aggregate = {
        "verdicts": [
            {
                "tier": t.value,
                "relevant": t.value >= 5,
                "reason": f"tier {t.value} aggregated",
                "findings": (
                    [
                        {"id": "g1", "title": f"agg finding {t.value}",
                         "urgency": 1}
                    ]
                    if t.value >= 5
                    else []
                ),
            }
            for t in Tier
        ]
    }
    transcript = "Final summary.\n```json\n" + json.dumps(aggregate) + "\n```\n"
    collection = collect_tier_verdicts(_result([], transcript=transcript))
    assert [v.tier for v in collection.verdicts] == list(Tier)
    assert collection.source == COLLECT_SOURCE_AGGREGATED
    by_tier = {v.tier.value: v for v in collection.verdicts}
    assert by_tier[1].relevant is False
    assert by_tier[5].relevant is True
    assert len(by_tier[5].findings) == 1


def test_partial_tool_results_filled_from_aggregate_is_mixed() -> None:
    # Tiers 1-3 dispatched via tool; the rest only appear in the aggregate.
    interactions = [_agent_interaction(t) for t in (1, 2, 3)]
    aggregate = {
        "verdicts": [
            {"tier": t.value, "relevant": False, "reason": "agg",
             "findings": []}
            for t in Tier
        ]
    }
    transcript = "```json\n" + json.dumps(aggregate) + "\n```"
    collection = collect_tier_verdicts(
        _result(interactions, transcript=transcript)
    )
    assert len(collection.verdicts) == 11
    assert collection.source == COLLECT_SOURCE_MIXED
    by_tier = {v.tier.value: v for v in collection.verdicts}
    # Tool-result tiers stay relevant; aggregate-filled tiers are not.
    assert by_tier[1].relevant is True
    assert by_tier[4].relevant is False


# --- run_single_session_discovery via injected runner (no SDK) --------------


def test_run_single_session_discovery_returns_eleven_in_order() -> None:
    captured: list[str] = []

    async def _runner(prompt: str) -> IterationResult:
        captured.append(prompt)
        return _result([_agent_interaction(t.value) for t in Tier])

    verdicts = asyncio.run(
        run_single_session_discovery(
            repo_root=Path("/repo"), session_runner=_runner
        )
    )
    assert [v.tier for v in verdicts] == list(Tier)
    assert len(verdicts) == 11
    # The runner was driven exactly once -- one session, not eleven.
    assert len(captured) == 1


# --- structural shape of the single session (SDK required) ------------------


def test_tier_agents_declare_no_mcp_servers() -> None:
    pytest.importorskip("claude_agent_sdk")
    agents = build_tier_agents(Path("/repo"))
    assert set(agents) == {tier_agent_key(t) for t in Tier}
    for definition in agents.values():
        # Edge case (criteria 1-2): a tier that booted an MCP server must fail.
        assert definition.mcpServers == []
        assert definition.maxTurns is not None and definition.maxTurns > 0


def test_single_session_options_carry_dispatch_tool_and_agents() -> None:
    pytest.importorskip("claude_agent_sdk")
    options = build_single_session_options(Path("/repo"))
    # The orchestrator must be allowed to dispatch subagents.
    assert SUBAGENT_TOOL_NAMES.issubset(set(options.allowed_tools or []))
    assert options.agents is not None
    assert set(options.agents) == {tier_agent_key(t) for t in Tier}
