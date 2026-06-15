"""The registry of work-source backends.

Names the work-source kinds an operator can select in ``[source] kind`` and
binds each to a builder in :mod:`flywheel_orchestrator._policy` (which owns
``WorkPolicy`` and the adapter imports, so the builders live there to avoid a
circular import; this module depends only on the core registry).

Both shipped kinds are always importable, so neither carries an ``extra`` --
``github`` drives the ``gh`` CLI rather than a Python dependency. The value of
the seam is forward-looking: project-management adapters (Jira, Linear, a
queue) register under the ``flywheel.work_sources`` entry-point group once
third-party discovery is enabled.
"""

from __future__ import annotations

from flywheel_core._registry import PluginSpec, Registry

SOURCES = Registry("work source", "flywheel.work_sources")

SOURCES.register(
    PluginSpec(
        name="directory",
        target="flywheel_orchestrator._policy:build_directory_source",
        summary="local directory of task JSON files (default)",
    )
)
SOURCES.register(
    PluginSpec(
        name="github",
        target="flywheel_orchestrator._policy:build_github_source",
        summary="labeled GitHub issues via the gh CLI",
    )
)

__all__ = ["SOURCES"]
