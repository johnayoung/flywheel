"""Seam holdout: ``docs/team-mode.md`` may only name surface that exists.

The team-mode runbook (spec 00081, criterion 4) instructs operators across a
real GitHub org; a hallucinated verb, config key, or doc topic would send
them down a dead end, and prose drifts silently as the surface evolves. This
test binds the served runbook to the *live* surface, the
``test_ops_skill_surface`` pattern:

* every ``fw``/``flywheel`` command token must be a verb the router
  (:mod:`flywheel._cli`) actually dispatches;
* every concrete ``fw docs <topic>`` must be a key of the curated topic set
  (:data:`flywheel._docs.TOPICS`);
* every ``[table] key`` config reference must be a parsed
  :class:`~flywheel_orchestrator.WorkPolicy` field;
* the strategies the runbook calls single-machine must be exactly the
  policy guard's :data:`~flywheel_orchestrator.LOCAL_LOCK_STRATEGIES` --
  the spec's shared invariant, asserted from the constant, never a copy.

The runbook is read through :func:`flywheel._docs.render_topic` (package
data); ``test_docs_parity.py`` pins that byte-identical to the canonical
``docs/team-mode.md``, so this test covers both copies.
"""

from __future__ import annotations

import dataclasses
import re

from flywheel import _cli
from flywheel._docs import TOPICS, render_topic
from flywheel_orchestrator import LOCAL_LOCK_STRATEGIES, WorkPolicy

_TOPIC = "team-mode"

_TOPIC_TOKEN = re.compile(r"[a-z][a-z-]*\Z")
_INLINE_CODE = re.compile(r"`([^`\n]+)`")

# A config reference is a backtick span shaped `[table] key` (the runbook's
# citation form) or a bare `[[submit.tiers]]` array-of-tables reference.
_CONFIG_REF = re.compile(r"\[(?P<table>[a-z_.]+)\]\s+(?P<key>[a-z_]+)\Z")
_TIERS_REF = re.compile(r"\[\[submit\.tiers\]\]\Z")

# ``[table] key`` -> WorkPolicy field, where the mechanical ``table_key``
# spelling does not hold. Kept minimal on purpose: a new special case means
# the policy field naming diverged from the config vocabulary.
_FIELD_EXCEPTIONS: dict[tuple[str, str], str] = {
    ("submit", "protected_paths"): "protected_paths",
}


def _router_verbs() -> frozenset[str]:
    """The verbs the router accepts, derived from its own dispatch constants."""
    return frozenset(
        set(_cli._ORCHESTRATOR_VERBS)
        | set(_cli._CORE_VERBS)
        | {
            _cli._SAY_VERB,
            _cli._WORKER_VERB,
            _cli._AUTOPILOT_VERB,
            _cli._AUDIT_VERB,
            _cli._DOCS_VERB,
        }
    )


def _command_snippets(text: str) -> list[str]:
    """Inline code spans plus fenced-block lines -- the command-shaped text."""
    snippets = list(_INLINE_CODE.findall(text))
    in_fence = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            snippets.append(line)
    return snippets


def test_every_named_verb_exists_on_the_router() -> None:
    text = render_topic(_TOPIC)
    verbs = _router_verbs()
    named: set[str] = set()
    for snippet in _command_snippets(text):
        tokens = snippet.strip().split()
        if len(tokens) < 2 or tokens[0] not in ("fw", "flywheel"):
            continue
        if tokens[1].startswith("-"):
            continue
        named.add(tokens[1])
    assert named, "the runbook must name at least one operator verb"
    unknown = named - verbs
    assert not unknown, f"runbook names verbs the router lacks: {sorted(unknown)}"


def test_every_named_docs_topic_is_curated() -> None:
    text = render_topic(_TOPIC)
    named: set[str] = set()
    for snippet in _command_snippets(text):
        tokens = snippet.strip().split()
        if (
            len(tokens) >= 3
            and tokens[0] in ("fw", "flywheel")
            and tokens[1] == _cli._DOCS_VERB
            and _TOPIC_TOKEN.match(tokens[2])
        ):
            named.add(tokens[2])
    unknown = named - set(TOPICS)
    assert not unknown, f"runbook names unknown docs topics: {sorted(unknown)}"


def test_every_named_config_key_is_a_policy_field() -> None:
    text = render_topic(_TOPIC)
    fields = {f.name for f in dataclasses.fields(WorkPolicy)}
    named: list[tuple[str, str]] = []
    tiers_named = False
    for span in _INLINE_CODE.findall(text):
        if _TIERS_REF.match(span.strip()):
            tiers_named = True
            continue
        match = _CONFIG_REF.match(span.strip())
        if match is None:
            continue
        named.append((match.group("table"), match.group("key")))
    assert named, "the runbook must cite its config keys as `[table] key`"
    for table, key in named:
        field = _FIELD_EXCEPTIONS.get(
            (table, key), f"{table.replace('.', '_')}_{key}"
        )
        assert field in fields, (
            f"runbook cites `[{table}] {key}` but WorkPolicy has no "
            f"{field!r} field"
        )
    assert tiers_named, "the runbook must reference [[submit.tiers]]"
    assert "submit_tiers" in fields


def test_single_machine_strategy_set_matches_the_guard() -> None:
    # The runbook's claim about which strategies serialize through the local
    # flock must be the guard's own constant -- the shared invariant of spec
    # 00081. Every guarded strategy must be named in the runbook, and the
    # runbook must route distributed landing at the strategy the guard allows.
    text = render_topic(_TOPIC)
    for strategy in LOCAL_LOCK_STRATEGIES:
        assert f"`{strategy}`" in text, (
            f"runbook never names the local-lock strategy {strategy!r}"
        )
    assert "pr" not in LOCAL_LOCK_STRATEGIES
    assert 'strategy = "pr"' in text
