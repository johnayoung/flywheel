"""Per-iteration prompt builder.

Composes the agent prompt for a single iteration from a :class:`Task`, the
current :class:`Lifecycle` state, and an :class:`IterationInputs` bundle
that carries per-run parameters (currently the configured retry ceiling)
that are not on the Task or Lifecycle themselves.

Deterministic: the same ``(task, lifecycle, iteration_inputs)`` triple
produces byte-identical output — no timestamps, no PIDs, no env-var
injection, no IO. The harness owns timing and delivery; this module owns
shape.

The prompt surfaces:

- The task's ``goal`` and provided ``context`` fields.
- The task's ``graders`` — including non-``command`` types so the agent
  knows non-deterministic verification will follow.
- The lifecycle's current status, retry budget, last error, and full
  attempts history so the agent does not re-attempt already-failed
  approaches blindly.
- The iteration envelope contract from ``docs/loop.md`` — the closed-enum
  ``intent`` (``verify``/``blocked``/``continue``/``abort``), the
  ``<!-- LOOP_STATUS -->`` fencing, and the mandatory ``requires`` predicate
  array that an ``intent=blocked`` envelope must carry (with the steer to use
  ``abort`` for an unsatisfiable/moot task that has nothing to re-check).

Pure module: no IO, no SDK imports, no logging, no template-file reads.
"""

from dataclasses import dataclass

from flywheel_core.envelope import CLOSING_FENCE, Intent, OPENING_FENCE
from flywheel_core.lifecycle import Lifecycle
from flywheel_core.task import (
    CommandGrader,
    Grader,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
)


@dataclass(frozen=True, kw_only=True)
class RubricFindings:
    """A single rubric finding carried forward into the next iteration.

    Pure data: produced by the harness (which queries the grader-results
    store) and consumed by :func:`build_iteration_prompt` to render the
    ``# Reviewer feedback`` section. The dataclass is frozen so the prompt
    builder cannot mutate it and remains deterministic.

    ``ordinal`` mirrors the grader's index in ``task.graders`` so the
    renderer can deterministically interleave rubric and manual findings
    by ``(attempt_number, ordinal)``. Defaults to ``0`` so existing
    callers that only build rubric tuples in their preferred order
    continue to work — Python's stable sort then preserves that order.
    """

    grader_name: str
    attempt_number: int
    summary: str
    ordinal: int = 0


@dataclass(frozen=True, kw_only=True)
class ManualFinding:
    """A single manual-grader rejection carried forward into the next
    iteration.

    Pure data sibling of :class:`RubricFindings`: built by the harness
    from failing ``grader_type='manual'`` receipts (operator rejections,
    per spec 00016 FR-6) and rendered in the ``# Reviewer feedback``
    section with an operator-distinguishing label so the agent can tell
    a human "no" apart from a rubric verdict.

    ``ordinal`` is the grader's index in ``task.graders`` (matching the
    :class:`flywheel_core.grader_manual.ManualGate.ordinal` the resolver
    persisted to the receipt) and feeds the renderer's
    ``(attempt_number, ordinal)`` ordering when rubric and manual
    findings appear on the same attempt.
    """

    grader_name: str
    attempt_number: int
    summary: str
    ordinal: int = 0


@dataclass(frozen=True, kw_only=True)
class RecoveryHandoff:
    """Structured handoff summary from a prior attempt that was terminated
    by the harness's context-recovery policy (spec 00018 FR-3).

    Pure data: produced from a fresh-context summarizer call when the
    working attempt approaches the agent's context-window capacity, then
    consumed by :func:`build_iteration_prompt` to render the
    ``# Recovery handoff`` section on the recovery attempt's prompt so
    the resuming agent rebuilds situational context from the summary
    rather than re-discovering it.

    The four fields mirror the spec's structured-handoff shape exactly
    (work done, work remaining, key decisions, suggested next step); the
    dataclass is frozen so the prompt builder cannot mutate it and
    remains byte-deterministic.
    """

    work_done: str
    work_remaining: str
    key_decisions: str
    suggested_next_step: str


@dataclass(frozen=True, kw_only=True)
class IterationInputs:
    """Per-iteration parameters not carried by the Task or Lifecycle.

    Kept as a dataclass so future fields (grader-result snapshots, prior
    envelope intent, harness-supplied tool budgets) can be added without
    changing the ``build_iteration_prompt`` signature.
    """

    max_retries: int
    prior_rubric_findings: tuple[RubricFindings, ...] = ()
    prior_manual_findings: tuple[ManualFinding, ...] = ()
    recovery_handoff: RecoveryHandoff | None = None


def build_iteration_prompt(
    task: Task,
    lifecycle: Lifecycle,
    iteration_inputs: IterationInputs,
) -> str:
    """Render the iteration prompt as a single string.

    The output is byte-identical for byte-identical inputs. Sections are
    emitted in a fixed order, list-typed context fields are walked in
    insertion order, and the envelope contract block is built from
    enum-iteration order rather than set iteration.
    """

    sections: list[str] = [_section_goal(task)]

    context_section = _section_context(task)
    if context_section is not None:
        sections.append(context_section)

    if iteration_inputs.recovery_handoff is not None:
        sections.append(
            _section_recovery_handoff(iteration_inputs.recovery_handoff)
        )

    if (
        iteration_inputs.prior_rubric_findings
        or iteration_inputs.prior_manual_findings
    ):
        sections.append(
            _section_reviewer_feedback(
                iteration_inputs.prior_rubric_findings,
                iteration_inputs.prior_manual_findings,
            )
        )

    sections.append(_section_graders(task))
    sections.append(_section_lifecycle(lifecycle, iteration_inputs))
    sections.append(_section_envelope_contract())

    return "\n\n".join(sections) + "\n"


def _section_goal(task: Task) -> str:
    return f"# Goal\n\n{task.goal}"


def _section_context(task: Task) -> str | None:
    ctx = task.context
    blocks: list[str] = []
    if ctx.relevant:
        blocks.append(_bulleted("Relevant files", ctx.relevant))
    if ctx.references:
        blocks.append(_bulleted("References", ctx.references))
    if ctx.constraints:
        blocks.append(_bulleted("Constraints", ctx.constraints))
    if ctx.non_goals:
        blocks.append(_bulleted("Non-goals", ctx.non_goals))
    if ctx.edge_cases:
        blocks.append(_bulleted("Edge cases", ctx.edge_cases))
    if ctx.notes.strip():
        blocks.append(f"## Notes\n\n{ctx.notes}")
    if not blocks:
        return None
    return "# Context\n\n" + "\n\n".join(blocks)


def _bulleted(heading: str, items: list[str]) -> str:
    bullets = "\n".join(f"- {item}" for item in items)
    return f"## {heading}\n\n{bullets}"


def _section_recovery_handoff(handoff: RecoveryHandoff) -> str:
    """Render the ``# Recovery handoff`` section from a structured summary
    produced by the harness's context-recovery summarizer (spec 00018
    FR-3).

    The section appears on a recovery attempt scheduled after the prior
    attempt approached the agent's context-window capacity. Four fixed
    subsections — work done, work remaining, key decisions, suggested
    next step — let the resuming agent rebuild situational context on a
    fresh window without re-running the prior attempt's discovery.

    Field order and headings are fixed for byte-determinism. An empty
    field renders as the ``"(none recorded)"`` placeholder so a missing
    subsection is explicit in the prompt instead of collapsing into
    adjacent text — the agent can see that a slot was deliberately left
    blank rather than guess what the summarizer intended.
    """
    placeholder = "(none recorded)"
    return (
        "# Recovery handoff\n\n"
        "A prior attempt approached the agent's context-window capacity "
        "and was finalized by the harness. The structured summary below "
        "is your starting point on a fresh context — continue from here "
        "rather than re-running the prior attempt's discovery.\n\n"
        f"## Work done\n\n{handoff.work_done or placeholder}\n\n"
        f"## Work remaining\n\n{handoff.work_remaining or placeholder}\n\n"
        f"## Key decisions\n\n{handoff.key_decisions or placeholder}\n\n"
        "## Suggested next step\n\n"
        f"{handoff.suggested_next_step or placeholder}"
    )


def _section_reviewer_feedback(
    rubric_findings: tuple[RubricFindings, ...],
    manual_findings: tuple[ManualFinding, ...],
) -> str:
    """Render the ``# Reviewer feedback`` section from rubric and manual
    findings.

    Findings are grouped under ``## attempt #N`` subheadings in ascending
    ``attempt_number`` order; within each attempt, bullets are emitted in
    ascending ``ordinal`` order so a mixed list of rubric + manual
    findings sorts deterministically by ``(attempt_number, ordinal)``.
    Python's sort is stable, so when two findings share an ordinal (the
    default ``0`` for rubric-only callers that don't set it) the renderer
    preserves the order in which they were supplied. This keeps the
    pre-existing harness behavior — "the harness controls the tuple it
    supplies, and the renderer only walks it" — byte-identical for
    rubric-only inputs while letting mixed inputs interleave by the
    grader-list position the harness already knows.

    Rubric findings render as ``- rubric `name`: <summary>``; manual
    findings render as ``- manual `name` (operator): <feedback>`` so
    the agent can tell a rubric verdict from an operator rejection.
    Manual findings substitute the documented
    ``"(no feedback provided)"`` placeholder for an empty summary
    (matching the resolver's substitution for an absent ``feedback``
    payload); rubric findings keep their ``"(no summary provided)"``
    placeholder for symmetry.
    """
    grouped: dict[int, list[RubricFindings | ManualFinding]] = {}
    for finding in (*rubric_findings, *manual_findings):
        grouped.setdefault(finding.attempt_number, []).append(finding)

    lines: list[str] = ["# Reviewer feedback"]
    for attempt_number in sorted(grouped):
        lines.append("")
        lines.append(f"## attempt #{attempt_number}")
        lines.append("")
        for finding in sorted(grouped[attempt_number], key=lambda f: f.ordinal):
            if isinstance(finding, ManualFinding):
                summary = finding.summary or "(no feedback provided)"
                lines.append(
                    f"- manual `{finding.grader_name}` (operator): {summary}"
                )
            else:
                summary = finding.summary or "(no summary provided)"
                lines.append(f"- rubric `{finding.grader_name}`: {summary}")
    return "\n".join(lines)


def _section_graders(task: Task) -> str:
    lines: list[str] = [
        "# Verification",
        "",
        "When you emit `intent=verify`, the harness runs these graders in",
        "list order. All must pass for the task to reach `done`.",
        "",
    ]
    for index, grader in enumerate(task.graders, start=1):
        lines.append(_grader_line(index, grader))
    return "\n".join(lines)


def _grader_line(index: int, grader: Grader) -> str:
    if isinstance(grader, CommandGrader):
        label = grader.name or "command"
        return f"{index}. command `{label}`: `{grader.run}`"
    if isinstance(grader, RubricGrader):
        label = grader.name or "rubric"
        assertions = "\n".join(f"   - {a}" for a in grader.assertions)
        return (
            f"{index}. rubric `{label}` (LLM-judged, agent cannot self-verify):"
            f"\n{assertions}"
        )
    if isinstance(grader, ManualGrader):
        label = grader.name or "manual"
        return (
            f"{index}. manual `{label}` (operator approval required): "
            f"{grader.instruction}"
        )
    if isinstance(grader, TranscriptGrader):
        label = grader.name or "transcript"
        caps: list[str] = []
        if grader.max_turns is not None:
            caps.append(f"max_turns={grader.max_turns}")
        if grader.max_total_tokens is not None:
            caps.append(f"max_total_tokens={grader.max_total_tokens}")
        if grader.max_wall_seconds is not None:
            caps.append(f"max_wall_seconds={grader.max_wall_seconds}")
        return (
            f"{index}. transcript `{label}` (hard run-time caps): "
            f"{', '.join(caps)}"
        )
    raise TypeError(f"unsupported grader type: {type(grader).__name__}")


def _section_lifecycle(
    lifecycle: Lifecycle, iteration_inputs: IterationInputs
) -> str:
    lines: list[str] = [
        "# Lifecycle state",
        "",
        f"Status: {lifecycle.status.value}",
        f"Run id: {lifecycle.run_id}",
        f"Retries used: {lifecycle.retries} / {iteration_inputs.max_retries}",
    ]
    if lifecycle.error:
        lines.append(f"Last error: {lifecycle.error}")

    if lifecycle.attempts:
        lines.append("")
        lines.append("## Prior attempts")
        lines.append("")
        for attempt in lifecycle.attempts:
            outcome = (
                attempt.outcome.value if attempt.outcome is not None else "pending"
            )
            entry = (
                f"- attempt #{attempt.number} "
                f"run={attempt.run_id} outcome={outcome}"
            )
            if attempt.error:
                entry += f" error={attempt.error}"
            lines.append(entry)
        lines.append("")
        lines.append(
            "Do not blindly repeat approaches that already failed above."
        )
    else:
        lines.append("")
        lines.append(
            "No prior attempts on this task — this is the first iteration."
        )

    return "\n".join(lines)


def _section_envelope_contract() -> str:
    intent_values = [member.value for member in Intent]
    intent_list = " | ".join(f'"{value}"' for value in intent_values)
    intent_bullets = "\n".join(
        f"- `{value}`: {_INTENT_DESCRIPTIONS[value]}" for value in intent_values
    )
    return (
        "# Iteration envelope (required)\n\n"
        "Every iteration MUST end with exactly one envelope, fenced like so:\n\n"
        f"{OPENING_FENCE}\n"
        '{"intent": ' + intent_list + ', "reason": "..."}\n'
        f"{CLOSING_FENCE}\n\n"
        "`intent` is a closed enum — only these four values are accepted:\n\n"
        f"{intent_bullets}\n\n"
        "## `blocked` requires a `requires` array\n\n"
        "An `intent=blocked` envelope MUST additionally carry a non-empty "
        "`requires` array naming the machine-checkable predicate(s) that would "
        "unblock it; the harness persists them and auto-resumes the lifecycle "
        "once they all hold. Use `blocked` ONLY when such a predicate exists. "
        "The three recognized shapes (and only these) are:\n\n"
        '- `{"type": "command_grader", "name": "<a command grader on this '
        'task>"}`\n'
        '- `{"type": "file_exists", "path": "<repo-relative path>", '
        '"present": true}`\n'
        '- `{"type": "env_var_set", "name": "<ENV_VAR>"}`\n\n'
        "Example:\n\n"
        f"{OPENING_FENCE}\n"
        '{"intent": "blocked", "reason": "needs the integration database up", '
        '"requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}\n'
        f"{CLOSING_FENCE}\n\n"
        "If you cannot proceed and there is NOTHING for the harness to "
        "re-check — the task's premise is false or already satisfied, the work "
        "is already on the base, or no correct change is possible — emit "
        "`abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` "
        "envelope with no `requires` array is a protocol failure that discards "
        "your reasoning.\n\n"
        "Missing, malformed, duplicated, or truncated envelopes are protocol "
        "failures and may end the run."
    )


_INTENT_DESCRIPTIONS: dict[str, str] = {
    Intent.VERIFY.value: (
        "work claims to be complete; the harness runs the graders above. "
        "Emit only when you believe every grader will pass."
    ),
    Intent.BLOCKED.value: (
        "you cannot proceed until a concrete, RE-CHECKABLE external "
        "prerequisite is met (a named command grader passing, a file "
        "appearing, an env var being set). This intent REQUIRES a non-empty "
        "`requires` array (see below) and pauses the lifecycle until the "
        "harness re-checks those predicates. Do NOT use `blocked` for a task "
        "you simply cannot complete — use `abort`."
    ),
    Intent.CONTINUE.value: (
        "more iterations needed; carry context forward."
    ),
    Intent.ABORT.value: (
        "you cannot complete the task and there is nothing for the harness to "
        "re-check — e.g. the task's premise is false or already satisfied, the "
        "work is already on the base, or no correct change is possible without "
        "fabricating one. The harness records your `reason` and stops (no "
        "retry). Put your full diagnosis in `reason`."
    ),
}
