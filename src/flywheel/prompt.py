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
- The iteration envelope contract from ``docs/loop.md`` verbatim — the
  closed-enum ``intent`` (``verify``/``blocked``/``continue``/``abort``)
  and the ``<!-- LOOP_STATUS -->`` fencing.

Pure module: no IO, no SDK imports, no logging, no template-file reads.
"""

from dataclasses import dataclass

from flywheel.envelope import CLOSING_FENCE, Intent, OPENING_FENCE
from flywheel.lifecycle import Lifecycle
from flywheel.task import (
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
    """

    grader_name: str
    attempt_number: int
    summary: str


@dataclass(frozen=True, kw_only=True)
class IterationInputs:
    """Per-iteration parameters not carried by the Task or Lifecycle.

    Kept as a dataclass so future fields (grader-result snapshots, prior
    envelope intent, harness-supplied tool budgets) can be added without
    changing the ``build_iteration_prompt`` signature.
    """

    max_retries: int
    prior_rubric_findings: tuple[RubricFindings, ...] = ()


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

    if iteration_inputs.prior_rubric_findings:
        sections.append(
            _section_reviewer_feedback(iteration_inputs.prior_rubric_findings)
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


def _section_reviewer_feedback(findings: tuple[RubricFindings, ...]) -> str:
    """Render the ``# Reviewer feedback`` section from rubric findings.

    Findings are grouped under ``## attempt #N`` subheadings in ascending
    ``attempt_number`` order; within each attempt, bullets appear in the
    same order they were supplied in the tuple — they are NOT re-sorted by
    grader name. This ordering is the module's deterministic-output
    contract: the harness controls the tuple it supplies, and the renderer
    only walks it.
    """
    grouped: dict[int, list[RubricFindings]] = {}
    for finding in findings:
        grouped.setdefault(finding.attempt_number, []).append(finding)

    lines: list[str] = ["# Reviewer feedback"]
    for attempt_number in sorted(grouped):
        lines.append("")
        lines.append(f"## attempt #{attempt_number}")
        lines.append("")
        for finding in grouped[attempt_number]:
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
        "Missing, malformed, duplicated, or truncated envelopes are protocol "
        "failures and may end the run."
    )


_INTENT_DESCRIPTIONS: dict[str, str] = {
    Intent.VERIFY.value: (
        "work claims to be complete; harness runs graders."
    ),
    Intent.BLOCKED.value: (
        "external input is needed; pauses the loop until intervention."
    ),
    Intent.CONTINUE.value: (
        "more iterations needed; carry context forward."
    ),
    Intent.ABORT.value: (
        "give up; harness records the failure and stops."
    ),
}
