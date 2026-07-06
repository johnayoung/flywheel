"""Agent-driven ``flywheel init``: inspect the repo and propose the policy.

``flywheel init`` normally prompts (or takes defaults) for the handful of
choices it owns. ``flywheel init --agent`` automates the onboarding
*assessment* a human would otherwise do by hand: an agent reads the repo,
infers the toolchain, and proposes the verification/grader commands, the
autopilot cadence, the sandbox provisioning step, and any ``.gitignore``
additions. This module owns that single agent round-trip and the rendering
of its proposal into a valid ``flywheel.toml``.

The agent seam mirrors the one in :mod:`flywheel_orchestrator._autopilot`
exactly -- the injectable :data:`~flywheel_orchestrator._autopilot.AutopilotInvoker`
coroutine, the lazy SDK-backed :func:`~flywheel_orchestrator._autopilot.build_repo_invoker`
driver, and the fenced-JSON extraction. ALL real SDK access stays behind the
lazy ``flywheel_core._sdk`` boundary (reached only inside ``build_repo_invoker``'s
coroutine), so importing this module never requires the ``claude`` extra.

The internal entry point :func:`run_agent_init` takes an optional ``invoker``
so a test scripts the agent offline; the CLI handler injects the production
driver. A proposal that does not parse, or that renders a policy
:func:`~flywheel_orchestrator._policy.load_policy` rejects, raises
:class:`InitAgentError` and writes nothing -- init never lands a broken policy.
"""

from __future__ import annotations

import asyncio
import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from flywheel_orchestrator._autopilot import (
    AutopilotInvoker,
    _extract_json_object,
    build_repo_invoker,
)
from flywheel_orchestrator._policy import (
    DEFAULT_POLICY_FILENAME,
    WorkPolicy,
    load_policy,
)

#: Turn budget for the real SDK-backed init invoker. Init's assessment is a
#: single bounded inspection (read the repo, return one JSON proposal), so it
#: needs far fewer turns than the autopilot discovery/authoring fan-outs.
DEFAULT_INIT_MAX_TURNS: int = 40

#: The work-source kinds init's agent path is allowed to propose. Init only
#: renders the directory and github sources today (mirroring the interactive
#: ``--source`` choices); an unknown kind is rejected before rendering.
_PROPOSAL_SOURCE_KINDS: frozenset[str] = frozenset({"directory", "github"})


class InitAgentError(Exception):
    """The agent proposal could not be parsed or rendered into a valid policy.

    Raised before anything is written, so an init-agent run that fails leaves
    the working directory exactly as it was -- no half-written ``flywheel.toml``,
    no partial ``.gitignore``.
    """


@dataclass(frozen=True, kw_only=True)
class InitProposal:
    """The agent's parsed onboarding proposal.

    Every field is the validated, typed projection of the agent's fenced JSON
    (see :func:`parse_init_proposal`); the agent never writes a path or runs a
    command, it only *proposes* the strings init then renders and re-validates.
    ``default_graders`` are the verification commands every grader-less work
    item inherits; ``sandbox_setup`` is the provisioning command run in each
    fresh sandbox; ``target_depth``/``interval_seconds`` tune the autopilot
    intake daemon; ``gitignore_additions`` are lines appended to ``.gitignore``;
    ``notes`` is the agent's one-line rationale, echoed to the operator.
    """

    source_kind: str = "directory"
    default_graders: tuple[str, ...] = ()
    sandbox_setup: str | None = None
    target_depth: int | None = None
    interval_seconds: float | None = None
    gitignore_additions: tuple[str, ...] = ()
    notes: str = ""


def init_prompt(repo_root: Path) -> str:
    """Build the onboarding-assessment prompt for the init agent.

    Asks the agent to inspect the repo and return exactly one fenced JSON
    proposal in the shape :func:`parse_init_proposal` reads. The contract is
    pinned in the prompt so a scripted invoker can return the same shape, and
    the agent is told to derive commands from checks that ALREADY exist in the
    repo (a test/lint/build command), never to invent one.
    """
    return (
        f"You are the onboarding-assessment agent for `flywheel init`.\n\n"
        f"Inspect the repository at {repo_root}: detect its toolchain (build "
        f"system, test runner, linter), and propose the flywheel policy a "
        f"maintainer would write by hand.\n\n"
        f"Derive the default grader commands from checks that ALREADY exist in "
        f"this repo (the test/lint/build commands a contributor runs today). "
        f"Do not invent a command the repo cannot run. Propose a sandbox setup "
        f"command that warms a fresh checkout (dependency install/fetch) when "
        f"one applies, the autopilot intake cadence, and any build-output "
        f"directories that belong in .gitignore.\n\n"
        f"Respond with exactly one fenced JSON block:\n"
        f"```json\n"
        f"{{\n"
        f'  "source_kind": "directory",\n'
        f'  "default_graders": ["<existing test command>", '
        f'"<existing lint command>"],\n'
        f'  "sandbox_setup": "<dependency install/fetch command, or null>",\n'
        f'  "autopilot": {{"target_depth": 3, "interval_seconds": 600}},\n'
        f'  "gitignore_additions": ["<build-output dir>"],\n'
        f'  "notes": "one sentence describing the detected toolchain"\n'
        f"}}\n"
        f"```"
    )


def _coerce_str_tuple(value: Any) -> tuple[str, ...]:
    """Coerce an agent-supplied list into a tuple of non-empty strings."""
    if not isinstance(value, list):
        return ()
    out: list[str] = []
    for entry in value:
        if isinstance(entry, str) and entry.strip():
            out.append(entry.strip())
    return tuple(out)


def _coerce_optional_int(value: Any) -> int | None:
    """Coerce an agent-supplied positive int, else ``None`` (use the default)."""
    if isinstance(value, bool):
        return None
    if isinstance(value, int) and value > 0:
        return value
    if isinstance(value, float) and value > 0 and value.is_integer():
        return int(value)
    return None


def _coerce_optional_number(value: Any) -> float | None:
    """Coerce an agent-supplied positive number, else ``None`` (use default)."""
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)) and value > 0:
        return float(value)
    return None


def parse_init_proposal(text: str) -> InitProposal:
    """Parse the agent's response text into a typed :class:`InitProposal`.

    Pulls the fenced JSON object (via the shared autopilot extractor) and
    projects each field with defensive coercion -- a missing or wrong-typed
    field falls back to its default rather than crashing, so a slightly
    off-contract response still yields a usable proposal. Raises
    :class:`InitAgentError` only when no JSON object can be extracted at all
    (the unrecoverable case), so init fails loudly instead of writing a
    default-everything policy the operator did not ask for.
    """
    try:
        data = _extract_json_object(text)
    except (ValueError, json.JSONDecodeError) as exc:
        raise InitAgentError(
            f"could not parse an init proposal from the agent response: {exc}"
        ) from exc

    raw_kind = data.get("source_kind")
    source_kind = (
        raw_kind if isinstance(raw_kind, str) and raw_kind in _PROPOSAL_SOURCE_KINDS
        else "directory"
    )

    raw_setup = data.get("sandbox_setup")
    sandbox_setup = (
        raw_setup.strip()
        if isinstance(raw_setup, str) and raw_setup.strip()
        else None
    )

    autopilot = data.get("autopilot")
    target_depth: int | None = None
    interval_seconds: float | None = None
    if isinstance(autopilot, dict):
        target_depth = _coerce_optional_int(autopilot.get("target_depth"))
        interval_seconds = _coerce_optional_number(
            autopilot.get("interval_seconds")
        )

    raw_notes = data.get("notes")
    notes = raw_notes.strip() if isinstance(raw_notes, str) else ""

    return InitProposal(
        source_kind=source_kind,
        default_graders=_coerce_str_tuple(data.get("default_graders")),
        sandbox_setup=sandbox_setup,
        target_depth=target_depth,
        interval_seconds=interval_seconds,
        gitignore_additions=_coerce_str_tuple(data.get("gitignore_additions")),
        notes=notes,
    )


def _render_grader_block(run: str) -> str:
    """Render one ``[[defaults.graders]]`` array-of-tables entry.

    ``json.dumps`` quoting is valid for a TOML basic string, so an arbitrary
    command (quotes, backslashes) cannot break the rendered file.
    """
    return (
        "[[defaults.graders]]\n"
        'type = "command"\n'
        f"run = {json.dumps(run)}\n"
    )


def render_agent_policy(proposal: InitProposal, *, submit_base: str | None) -> str:
    """Render a complete ``flywheel.toml`` from an agent proposal.

    The layout mirrors the hand-written init scaffold (source -> store ->
    paths -> defaults.graders -> sandbox -> autopilot -> submit), but the
    grader/sandbox/autopilot sections are ACTIVE keys derived from the
    proposal rather than commented suggestions. Every string is quoted via
    ``json.dumps`` (valid TOML basic-string escaping), so an arbitrary command
    or branch name cannot break the file. The rendered text is re-validated by
    :func:`load_policy` in :func:`run_agent_init` before it is written.
    """
    parts: list[str] = [
        "# Flywheel work policy proposed by `flywheel init --agent`.\n"
        "# An agent inspected the repo and derived these settings; review and "
        "edit freely.\n"
        "# Committed with the repo; CLI flags always override.\n",
        "\n",
        '[source]\nkind = "directory"\ntasks_dir = ".flywheel/tasks"\n',
        "\n",
        '[store]\nbackend = "sqlite"\n',
        "\n",
        "[paths]\n"
        'db = ".flywheel/flywheel.sqlite"\n'
        'sandbox_root = ".flywheel/worktrees"\n',
    ]

    if proposal.default_graders:
        parts.append("\n")
        parts.append(
            "# Default graders for work items that declare none (the "
            "verification\n# commands the agent derived from this repo's "
            "existing checks).\n"
        )
        parts.append(
            "\n".join(
                _render_grader_block(run) for run in proposal.default_graders
            )
        )

    if proposal.sandbox_setup is not None:
        parts.append("\n")
        parts.append(
            "# Sandbox provisioning: runs (shell) inside every newly created "
            "sandbox\n# before the agent enters.\n"
        )
        parts.append(
            f"[sandbox]\nsetup = {json.dumps(proposal.sandbox_setup)}\n"
        )

    if proposal.target_depth is not None or proposal.interval_seconds is not None:
        parts.append("\n")
        parts.append(
            "# Autopilot intake daemon cadence (target queue depth and refill "
            "interval).\n"
        )
        parts.append("[autopilot]\n")
        if proposal.target_depth is not None:
            parts.append(f"target_depth = {proposal.target_depth}\n")
        if proposal.interval_seconds is not None:
            interval = proposal.interval_seconds
            rendered = (
                str(int(interval)) if float(interval).is_integer()
                else repr(interval)
            )
            parts.append(f"interval_seconds = {rendered}\n")

    parts.append("\n")
    suggestion = json.dumps(submit_base) if submit_base else '"main"'
    detected_note = (
        f" (detected current branch: {submit_base})" if submit_base else ""
    )
    parts.append(
        "# Landing policy. Leave base UNSET to FF-merge work in-tree onto your\n"
        f"# checked-out branch{detected_note}. Set base ONLY to a separate\n"
        "# integration branch you do NOT have checked out.\n"
        "[submit]\n"
        f"# base = {suggestion}\n"
    )
    return "".join(parts)


@dataclass(frozen=True, kw_only=True)
class InitAgentResult:
    """The outcome of an agent-driven init: the proposal and what it wrote.

    ``policy_path`` is the written ``flywheel.toml``; ``gitignore_added`` are
    the lines actually appended to ``.gitignore`` this run (empty when the
    proposal added none or they were already present). ``policy`` is the
    re-validated :class:`WorkPolicy` parsed from the written file -- proof the
    emitted policy loads.
    """

    proposal: InitProposal
    policy_path: Path
    policy: WorkPolicy
    gitignore_added: tuple[str, ...] = field(default=())


def _append_gitignore(additions: tuple[str, ...], *, root: Path) -> tuple[str, ...]:
    """Append never-before-seen ``additions`` to ``root/.gitignore``.

    Creates the file when absent. A line already present (exact match, after
    stripping trailing whitespace) is skipped so a second run never duplicates
    an entry. Returns the lines actually written this call.
    """
    if not additions:
        return ()
    gitignore = root / ".gitignore"
    existing_lines: set[str] = set()
    prior_text = ""
    if gitignore.is_file():
        prior_text = gitignore.read_text(encoding="utf-8")
        existing_lines = {
            line.rstrip() for line in prior_text.splitlines()
        }
    to_add = [a for a in additions if a.rstrip() not in existing_lines]
    if not to_add:
        return ()
    prefix = ""
    if prior_text and not prior_text.endswith("\n"):
        prefix = "\n"
    body = "\n".join(to_add) + "\n"
    with gitignore.open("a", encoding="utf-8") as handle:
        handle.write(prefix + body)
    return tuple(to_add)


def run_agent_init(
    *,
    repo_root: Path,
    submit_base: str | None = None,
    invoker: AutopilotInvoker | None = None,
    model: str | None = None,
    policy_path: Path | None = None,
) -> InitAgentResult:
    """Drive the agent, render the proposed policy, validate, and write it.

    The single internal entry point the CLI calls. ``invoker`` defaults to the
    production SDK-backed seam rooted at ``repo_root`` (built lazily, so the
    SDK is only touched when an unscripted run actually drives an agent); tests
    inject a scripted coroutine to run offline.

    The proposal is parsed, rendered, and -- critically -- re-validated through
    :func:`load_policy` BEFORE anything is written: an unparseable response or a
    proposal that renders an invalid policy raises :class:`InitAgentError` and
    leaves the working directory untouched. Only after validation passes does
    it write ``flywheel.toml`` and (optionally) append to ``.gitignore``.
    """
    seam = (
        invoker
        if invoker is not None
        else build_repo_invoker(
            repo_root, model=model, max_turns=DEFAULT_INIT_MAX_TURNS
        )
    )
    async def _drive() -> str:
        return await seam(init_prompt(repo_root))

    text = asyncio.run(_drive())
    proposal = parse_init_proposal(text)
    rendered = render_agent_policy(proposal, submit_base=submit_base)

    target = (
        policy_path
        if policy_path is not None
        else repo_root / DEFAULT_POLICY_FILENAME
    )
    # Validate the rendered policy in-memory (via a scratch file the loader can
    # read) before writing the real one, so a broken render never lands.
    scratch = target.parent / f".{target.name}.init-agent.tmp"
    scratch.write_text(rendered, encoding="utf-8")
    try:
        policy = load_policy(scratch)
    except Exception as exc:  # noqa: BLE001 - any load failure aborts the write.
        raise InitAgentError(
            f"the agent-proposed policy failed validation and was not "
            f"written: {exc}"
        ) from exc
    finally:
        scratch.unlink(missing_ok=True)

    target.write_text(rendered, encoding="utf-8")
    gitignore_added = _append_gitignore(
        proposal.gitignore_additions, root=repo_root
    )
    return InitAgentResult(
        proposal=proposal,
        policy_path=target,
        policy=policy,
        gitignore_added=gitignore_added,
    )


__all__ = [
    "DEFAULT_INIT_MAX_TURNS",
    "InitAgentError",
    "InitAgentResult",
    "InitProposal",
    "init_prompt",
    "parse_init_proposal",
    "render_agent_policy",
    "run_agent_init",
]
