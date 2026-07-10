"""The ``flywheel docs`` / ``fw docs`` verb: serve the curated operator docs.

The curated topics ship as package data under
:mod:`flywheel._doc_pages` -- byte-identical copies embedded from the
canonical ``docs/*.md`` at build time. Serving reads them through
:func:`importlib.resources.files`, so a topic prints identically from any
working directory and never resolves a path outside the installed package.

:data:`TOPICS` is the single source of truth for the curated set. The bare
listing, the per-topic serving path, and the parity test
(``tests/test_docs_parity.py``) all consume this one constant -- there is no
second copy of the topic names anywhere. Adding or renaming a curated doc is
a one-line edit here (plus the matching page under ``_doc_pages/``).

Docs are knowledge, not policy: this verb loads no ``flywheel.toml`` and
touches no store, so it works in a bare directory with nothing scaffolded.
"""

from __future__ import annotations

import sys
from collections.abc import Mapping, Sequence
from importlib.resources import files
from importlib.resources.abc import Traversable

_PROG = "fw"

# The curated topic set: the ONE importable constant. Keys are the topic
# names (which are also the ``_doc_pages/<topic>.md`` basenames and the
# canonical ``docs/<topic>.md`` basenames); values are the one-line
# descriptions shown in the bare listing, condensed from ``docs/README.md``.
# Insertion order is the listing order.
TOPICS: Mapping[str, str] = {
    "cli": "every verb, the interactive operator console, slash commands, and init.",
    "configuration": "the complete flywheel.toml reference.",
    "task-schema": "the Task / Grader / Context shape and validation rules.",
    "loop": "the single-task control plane: the iteration envelope, state detection, harness behavior.",
    "workflow": "how flywheel develops itself: the spec-driven authoring pipeline and the runtime loop.",
    "strategy": "the SubmitStrategy / SandboxHandle landing seam and the shipped strategies.",
    "sandbox": "the sandbox-as-deploy model and the complete [sandbox.*] reference.",
    "held-out-gate": "the execute-time held-out landing gate that defends against reward-hacking.",
    "autopilot": "the autopilot intake daemon: tier model, scoring, CLI, and console activation.",
    "team-mode": "the multi-operator runbook: GitHub App identity, rulesets, and the merge queue as the distributed merge lock.",
}

# The package-data directory holding the embedded ``<topic>.md`` pages. A
# plain data subdirectory of the ``flywheel`` package (no ``__init__``),
# mirroring how ``flywheel_core`` ships ``_schema/*.sql``.
_PAGES_DIR = "_doc_pages"


def _page(topic: str) -> Traversable:
    """The package-data resource for one curated topic's markdown page."""
    return files("flywheel").joinpath(_PAGES_DIR, f"{topic}.md")


def render_topic(topic: str) -> str:
    """Return the byte-for-byte content of one curated doc from package data.

    ``topic`` must be a key of :data:`TOPICS`; callers are responsible for
    validating membership first (the verb does so and reports an unknown
    topic on stderr).
    """
    return _page(topic).read_text(encoding="utf-8")


def render_listing() -> str:
    """Build the bare-listing text: one ``<topic>  <description>`` line each.

    Both the topic names and their descriptions come from :data:`TOPICS`, so
    the listing can never name a topic the serving path cannot serve.
    """
    width = max(len(topic) for topic in TOPICS) + 2
    lines = ["Curated operator docs. Read one with: flywheel docs <topic>", ""]
    lines.extend(f"{topic:<{width}}{description}" for topic, description in TOPICS.items())
    return "\n".join(lines) + "\n"


def main(argv: Sequence[str] | None = None) -> int:
    """Serve the curated operator docs.

    * ``flywheel docs`` (bare) prints the listing and exits 0.
    * ``flywheel docs <topic>`` writes that doc verbatim to stdout, exits 0.
    * An unknown topic prints an error naming it to stderr (nothing to
      stdout) and exits 2.
    """
    args = list(sys.argv[1:] if argv is None else argv)

    if not args:
        sys.stdout.write(render_listing())
        return 0

    topic = args[0]
    if topic in ("-h", "--help"):
        sys.stdout.write(render_listing())
        return 0

    if topic not in TOPICS:
        print(f"{_PROG}: unknown docs topic: {topic}", file=sys.stderr)
        print(f"  see '{_PROG} docs' for the topic list", file=sys.stderr)
        return 2

    sys.stdout.write(render_topic(topic))
    return 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
