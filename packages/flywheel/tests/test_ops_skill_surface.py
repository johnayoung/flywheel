"""Seam holdout: the ``flywheel-ops`` skill may only name surface that exists.

The operability skill is a runbook of ``fw`` verbs and ``fw docs`` topics. A
hallucinated verb (``fw drain``) or a hallucinated doc topic (``fw docs
recovery``) would send an operator down a dead end, and prose drifts silently
as the CLI evolves. This test binds the rendered skill to the *live* surface:

* every ``fw``/``flywheel`` command token in the render must be a verb the
  router (:mod:`flywheel._cli`) actually dispatches, and
* every concrete ``fw docs <topic>`` must be a key of the curated topic set
  (:data:`flywheel._docs.TOPICS`).

Both sides are *imported*, never copied here: the router's own frozensets and
the docs module's own constant are the single sources of truth, so this test
cannot pass a stale duplicate. The ``flywheel`` dist depends on
``flywheel-orchestrator`` (where the template lives) and owns the router and
docs verb, so the product test is the one place that sees both sides.
"""

from __future__ import annotations

import re

import pytest

from flywheel import _cli
from flywheel._docs import TOPICS
from flywheel_orchestrator._skills import SkillRenderSettings, render_skill

_SKILL = "flywheel-ops"

# A ``fw docs <topic>`` argument is a real topic only when it looks like one
# (lowercase words and hyphens: ``loop``, ``held-out-gate``). Anything else in
# that slot is a placeholder the skill uses on purpose (``fw docs <topic>``),
# not a claim about a specific page, so it is not validated as a topic.
_TOPIC_TOKEN = re.compile(r"[a-z][a-z-]*\Z")
_INLINE_CODE = re.compile(r"`([^`\n]+)`")


def _router_verbs() -> frozenset[str]:
    """The set of verbs the router accepts on the surface, derived from the
    router's own dispatch constants (not a copied list). ``say`` is the
    surface name; core's internal ``steer`` alias is deliberately excluded
    because an operator never types it."""
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
    """Every command-shaped fragment in the render: inline code spans and the
    lines inside fenced code blocks. Prose mentions of the word 'flywheel'
    (outside backticks) are intentionally ignored -- only marked-up commands
    are held to the surface."""
    snippets = list(_INLINE_CODE.findall(text))
    in_fence = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            snippets.append(line)
    return snippets


def _referenced_surface(text: str) -> tuple[set[str], set[str]]:
    """Return ``(verbs, topics)`` the render actually invokes.

    A snippet is a command only when its first token is exactly ``fw`` or
    ``flywheel``. The verb is the second token (a leading-``-`` token is a
    top-level flag, e.g. ``fw --json``, not a verb). For ``fw docs <topic>``
    the third token is validated as a topic only when it is a concrete topic
    name, never a ``<placeholder>`` or a ``RUN_ID``-style argument.
    """
    verbs: set[str] = set()
    topics: set[str] = set()
    for snippet in _command_snippets(text):
        tokens = snippet.strip().split()
        if len(tokens) < 2 or tokens[0] not in ("fw", "flywheel"):
            continue
        verb = tokens[1]
        if verb.startswith("-"):
            continue
        verbs.add(verb)
        if verb == _cli._DOCS_VERB and len(tokens) >= 3:
            topic = tokens[2]
            if _TOPIC_TOKEN.match(topic):
                topics.add(topic)
    return verbs, topics


_SETTINGS = [
    pytest.param(SkillRenderSettings(), id="defaults"),
    pytest.param(
        SkillRenderSettings(
            source_kind="github",
            github_repo="octo/widgets",
            github_label="flywheel",
        ),
        id="github-source",
    ),
]


@pytest.mark.parametrize("settings", _SETTINGS)
def test_every_verb_token_is_a_real_router_verb(
    settings: SkillRenderSettings,
) -> None:
    text = render_skill(_SKILL, settings)
    verbs, _ = _referenced_surface(text)
    assert verbs, f"{_SKILL} references no fw/flywheel verb at all"
    unknown = verbs - _router_verbs()
    assert not unknown, (
        f"{_SKILL} references verb(s) the router does not dispatch: "
        f"{sorted(unknown)}. The skill conforms to the CLI, never the reverse."
    )


@pytest.mark.parametrize("settings", _SETTINGS)
def test_every_docs_topic_is_curated(settings: SkillRenderSettings) -> None:
    text = render_skill(_SKILL, settings)
    _, topics = _referenced_surface(text)
    assert topics, f"{_SKILL} names no concrete 'fw docs <topic>'"
    unknown = topics - set(TOPICS)
    assert not unknown, (
        f"{_SKILL} points at docs topic(s) that do not exist: "
        f"{sorted(unknown)}. Valid topics: {sorted(TOPICS)}."
    )


def test_placeholder_docs_pointer_is_not_treated_as_a_topic() -> None:
    """The skill must send operators to ``fw docs <topic>`` for depth. That
    literal placeholder is a pointer, not a page name, so the surface check
    must not mistake ``<topic>`` for a topic and reject it."""
    _, topics = _referenced_surface("guidance: `fw docs <topic>` for depth")
    assert topics == set()


def test_the_skill_actually_covers_the_operability_verbs() -> None:
    """Guard against a render that trivially passes by naming no real work:
    the runbook must reach the worker, the evidence-derived rollup, and a
    recovery verb -- the spine the operability screen grades."""
    verbs, topics = _referenced_surface(render_skill(_SKILL, SkillRenderSettings()))
    for expected in (_cli._WORKER_VERB, "status", "recover"):
        assert expected in verbs, f"{_SKILL} never invokes '{expected}'"
    assert topics, "runbook must route to at least one curated doc for depth"
