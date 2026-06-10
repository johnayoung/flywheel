"""Behavioural tests for :mod:`flywheel_core.prompt`.

Covers determinism, content surface (goal / context / graders /
lifecycle), envelope-contract fidelity, and lifecycle-state reflection
between a first-attempt prompt and a retry prompt.
"""

from datetime import datetime, timezone

from flywheel_core import (
    Attempt,
    CommandGrader,
    Context,
    IterationInputs,
    Lifecycle,
    ManualGrader,
    Outcome,
    RubricGrader,
    Status,
    Task,
    TranscriptGrader,
    build_iteration_prompt,
)
from flywheel_core.envelope import CLOSING_FENCE, Intent, OPENING_FENCE
from flywheel_core.prompt import ManualFinding, RubricFindings


def _minimal_task() -> Task:
    return Task(
        goal="Add exponential backoff to the HTTP client.",
        graders=[CommandGrader(run="uv run pytest tests/http", name="tests")],
        id="add-retry-logic",
    )


def _briefed_task() -> Task:
    return Task(
        id="add-retry-logic",
        goal="HTTP client retries 5xx and timeout failures with exponential backoff.",
        context=Context(
            relevant=[
                "src/flywheel/http/client.py",
                "src/flywheel/http/config.py",
            ],
            references=["src/flywheel/db/retry.py — mirror this structure"],
            constraints=["Use stdlib + existing deps; no new packages"],
            non_goals=["Don't touch tests outside tests/http"],
            edge_cases=["Respect Retry-After when the server provides it"],
            notes="Backoff base delay is 100ms. See ADR-014 for the curve.",
        ),
        graders=[
            CommandGrader(run="uv run pytest tests/http", name="tests"),
            RubricGrader(
                assertions=[
                    "Retries on 5xx and timeout errors only",
                    "Respects max_retries config (default 3)",
                ],
                name="semantics",
            ),
            TranscriptGrader(max_turns=20, name="budget"),
            ManualGrader(
                instruction="Confirm jitter algorithm fits upstream rate limits",
                name="ops-sign-off",
            ),
        ],
    )


def _fixed_now() -> datetime:
    return datetime(2026, 5, 22, 12, 0, 0, tzinfo=timezone.utc)


def test_builder_is_deterministic_for_identical_inputs() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-abc")
    inputs = IterationInputs(max_retries=3)

    first = build_iteration_prompt(task, lifecycle, inputs)
    second = build_iteration_prompt(task, lifecycle, inputs)

    assert first == second
    assert isinstance(first, str)
    assert len(first) > 0


def test_builder_surfaces_goal_verbatim() -> None:
    task = _minimal_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=2)
    )

    assert task.goal in prompt


def test_builder_renders_every_context_field_when_present() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )

    assert "src/flywheel/http/client.py" in prompt
    assert "mirror this structure" in prompt
    assert "no new packages" in prompt
    assert "Don't touch tests outside tests/http" in prompt
    assert "Respect Retry-After" in prompt
    assert "ADR-014" in prompt


def test_builder_omits_context_section_when_all_fields_are_empty() -> None:
    task = _minimal_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=2)
    )

    assert "# Context" not in prompt
    assert task.goal in prompt
    assert OPENING_FENCE in prompt


def test_builder_does_not_invent_context_the_task_lacks() -> None:
    task = Task(
        id="bare",
        goal="Do a thing.",
        graders=[CommandGrader(run="true")],
    )
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=1)
    )

    for forbidden in ("Relevant files", "References", "Constraints",
                      "Non-goals", "Edge cases", "Notes"):
        assert forbidden not in prompt, (
            f"builder hallucinated context heading {forbidden!r}"
        )


def test_envelope_contract_is_explained_with_fences_and_closed_intent_enum() -> None:
    task = _minimal_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=1)
    )

    assert OPENING_FENCE in prompt
    assert CLOSING_FENCE in prompt
    for intent in Intent:
        assert f'"{intent.value}"' in prompt, (
            f"intent {intent.value!r} missing from prompt envelope spec"
        )
    assert "closed enum" in prompt.lower()
    assert "required" in prompt.lower()


def test_graders_section_surfaces_every_grader_type() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )

    assert "uv run pytest tests/http" in prompt
    assert "Retries on 5xx and timeout errors only" in prompt
    assert "max_turns=20" in prompt
    assert "Confirm jitter algorithm fits upstream rate limits" in prompt
    assert "operator approval required" in prompt
    assert "LLM-judged" in prompt


def test_first_attempt_prompt_differs_from_retry_prompt_for_same_task() -> None:
    task = _minimal_task()
    inputs = IterationInputs(max_retries=3)

    fresh = Lifecycle(task_id=task.id, run_id="run-1")
    fresh_prompt = build_iteration_prompt(task, fresh, inputs)

    retried = Lifecycle(
        task_id=task.id,
        run_id="run-1",
        status=Status.READY,
        retries=2,
        error="",
        attempts=[
            Attempt(
                number=1,
                started_at=_fixed_now(),
                ended_at=_fixed_now(),
                run_id="run-1",
                outcome=Outcome.VALIDATION_FAILED,
                error="pytest failed: tests/http/test_retry.py::test_backoff",
            ),
            Attempt(
                number=2,
                started_at=_fixed_now(),
                ended_at=_fixed_now(),
                run_id="run-1",
                outcome=Outcome.VALIDATION_FAILED,
                error="pytest failed: jitter clipped above ceiling",
            ),
        ],
    )
    retry_prompt = build_iteration_prompt(task, retried, inputs)

    assert fresh_prompt != retry_prompt
    assert "first iteration" in fresh_prompt
    assert "Prior attempts" in retry_prompt
    assert "Retries used: 2 / 3" in retry_prompt
    assert "Retries used: 0 / 3" in fresh_prompt
    assert "jitter clipped above ceiling" in retry_prompt
    assert "Do not blindly repeat" in retry_prompt


def test_last_error_on_lifecycle_surfaces_when_set() -> None:
    task = _minimal_task()
    lifecycle = Lifecycle(
        task_id=task.id,
        run_id="run-1",
        status=Status.FAILED_VALIDATION,
        retries=1,
        error="grader command exited 1",
    )

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )

    assert "Last error: grader command exited 1" in prompt
    assert "Status: failed_validation" in prompt


def test_retry_budget_is_visible_in_prompt() -> None:
    task = _minimal_task()
    inputs = IterationInputs(max_retries=5)
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1", retries=2)

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "Retries used: 2 / 5" in prompt


def test_long_notes_are_not_silently_truncated() -> None:
    long_notes = "Sentence one. " * 500
    task = Task(
        id="long-notes",
        goal="Do a thing.",
        graders=[CommandGrader(run="true")],
        context=Context(notes=long_notes),
    )
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=1)
    )

    assert long_notes in prompt


def test_builder_handles_command_grader_without_explicit_name() -> None:
    task = Task(
        id="unnamed",
        goal="Do a thing.",
        graders=[CommandGrader(run="uv run pytest")],
    )
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=1)
    )

    assert "uv run pytest" in prompt
    assert "command `command`" in prompt


def test_attempt_with_no_outcome_renders_as_pending() -> None:
    task = _minimal_task()
    lifecycle = Lifecycle(
        task_id=task.id,
        run_id="run-1",
        attempts=[
            Attempt(
                number=1,
                started_at=_fixed_now(),
                run_id="run-1",
            )
        ],
    )

    prompt = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )

    assert "outcome=pending" in prompt


def test_iteration_inputs_is_frozen() -> None:
    inputs = IterationInputs(max_retries=3)
    try:
        inputs.max_retries = 5  # type: ignore[misc]
    except Exception:
        return
    raise AssertionError("IterationInputs should be frozen")


def test_iteration_inputs_prior_rubric_findings_defaults_to_empty_tuple() -> None:
    inputs = IterationInputs(max_retries=2)

    assert inputs.prior_rubric_findings == ()


def test_empty_prior_rubric_findings_omits_reviewer_feedback_section() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(max_retries=3)

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "# Reviewer feedback" not in prompt


def test_empty_prior_rubric_findings_byte_identical_to_default_inputs() -> None:
    """Pin: omitting the new field must not perturb the first-attempt prompt."""

    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    without_field = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )
    with_empty_tuple = build_iteration_prompt(
        task,
        lifecycle,
        IterationInputs(max_retries=3, prior_rubric_findings=()),
    )

    assert without_field == with_empty_tuple


def test_single_rubric_finding_renders_grader_attempt_and_summary() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="Backoff base delay missing; only the ceiling is enforced.",
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "# Reviewer feedback" in prompt
    assert "## attempt #1" in prompt
    assert (
        "- rubric `semantics`: Backoff base delay missing; "
        "only the ceiling is enforced."
    ) in prompt


def test_multiple_findings_same_attempt_render_in_tuple_order() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="first",
            ),
            RubricFindings(
                grader_name="aaa-alpha-first-alphabetically",
                attempt_number=1,
                summary="second",
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    # Single attempt heading.
    assert prompt.count("## attempt #1") == 1
    # Tuple order is preserved — NOT re-sorted by grader_name.
    first_idx = prompt.index("- rubric `semantics`: first")
    second_idx = prompt.index(
        "- rubric `aaa-alpha-first-alphabetically`: second"
    )
    assert first_idx < second_idx


def test_findings_across_attempts_render_in_ascending_attempt_order() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    # Intentionally supply attempt #2's findings before attempt #1's to
    # confirm the renderer sorts by attempt_number rather than tuple index.
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=2,
                summary="second-attempt finding",
            ),
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="first-attempt finding",
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    one_idx = prompt.index("## attempt #1")
    two_idx = prompt.index("## attempt #2")
    assert one_idx < two_idx
    first_bullet = prompt.index("first-attempt finding")
    second_bullet = prompt.index("second-attempt finding")
    assert first_bullet < second_bullet
    # Each attempt has its own heading.
    assert prompt.count("## attempt #1") == 1
    assert prompt.count("## attempt #2") == 1


def test_empty_summary_finding_renders_placeholder() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="",
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "- rubric `semantics`: (no summary provided)" in prompt


def test_reviewer_feedback_section_is_byte_deterministic() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="first",
            ),
            RubricFindings(
                grader_name="semantics",
                attempt_number=2,
                summary="",
            ),
        ),
    )

    first = build_iteration_prompt(task, lifecycle, inputs)
    second = build_iteration_prompt(task, lifecycle, inputs)

    assert first == second


def test_reviewer_feedback_appears_between_context_and_verification() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics",
                attempt_number=1,
                summary="something to review",
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    context_idx = prompt.index("# Context")
    feedback_idx = prompt.index("# Reviewer feedback")
    verification_idx = prompt.index("# Verification")
    assert context_idx < feedback_idx < verification_idx


def test_rubric_findings_is_frozen() -> None:
    finding = RubricFindings(
        grader_name="semantics", attempt_number=1, summary="x"
    )
    try:
        finding.summary = "y"  # type: ignore[misc]
    except Exception:
        return
    raise AssertionError("RubricFindings should be frozen")


def test_iteration_inputs_prior_manual_findings_defaults_to_empty_tuple() -> None:
    inputs = IterationInputs(max_retries=2)

    assert inputs.prior_manual_findings == ()


def test_manual_finding_renders_in_reviewer_feedback_with_operator_label() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_manual_findings=(
            ManualFinding(
                grader_name="confirm-migration",
                attempt_number=1,
                summary=(
                    "The migration drops a column still read by the "
                    "billing service. Gate it behind a feature flag first."
                ),
                ordinal=3,
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "# Reviewer feedback" in prompt
    assert "## attempt #1" in prompt
    # The operator label distinguishes a manual rejection from a rubric
    # verdict so the agent does not confuse the two.
    assert (
        "- manual `confirm-migration` (operator): The migration drops a "
        "column still read by the billing service. Gate it behind a "
        "feature flag first."
    ) in prompt
    # Without rubric findings the section still appears — the trigger is
    # any non-empty reviewer-finding tuple, not specifically rubric.
    assert "- rubric" not in prompt


def test_empty_manual_findings_omits_reviewer_feedback_section() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_manual_findings=(),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    assert "# Reviewer feedback" not in prompt


def test_empty_rubric_and_manual_findings_byte_identical_to_default_inputs() -> None:
    """Pin: an explicit empty tuple for either field must not perturb the
    first-attempt prompt (so a no-op harness sweep is byte-stable)."""

    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")

    without_field = build_iteration_prompt(
        task, lifecycle, IterationInputs(max_retries=3)
    )
    with_empty_tuples = build_iteration_prompt(
        task,
        lifecycle,
        IterationInputs(
            max_retries=3,
            prior_rubric_findings=(),
            prior_manual_findings=(),
        ),
    )

    assert without_field == with_empty_tuples


def test_mixed_rubric_and_manual_findings_render_in_attempt_then_ordinal_order() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    # Supply findings in deliberately scrambled order — across attempts
    # AND within each attempt by ordinal — to prove the renderer sorts
    # by (attempt_number, ordinal), not by tuple position.
    inputs = IterationInputs(
        max_retries=3,
        prior_rubric_findings=(
            RubricFindings(
                grader_name="semantics-attempt-2",
                attempt_number=2,
                summary="attempt-2 rubric",
                ordinal=1,
            ),
            RubricFindings(
                grader_name="semantics-attempt-1",
                attempt_number=1,
                summary="attempt-1 rubric",
                ordinal=1,
            ),
        ),
        prior_manual_findings=(
            ManualFinding(
                grader_name="confirm-attempt-2",
                attempt_number=2,
                summary="attempt-2 manual",
                ordinal=3,
            ),
            ManualFinding(
                grader_name="confirm-attempt-1",
                attempt_number=1,
                summary="attempt-1 manual",
                ordinal=3,
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    # Attempt headings are emitted in ascending attempt_number order.
    attempt_1_idx = prompt.index("## attempt #1")
    attempt_2_idx = prompt.index("## attempt #2")
    assert attempt_1_idx < attempt_2_idx

    # Within attempt #1: rubric (ordinal 1) precedes manual (ordinal 3).
    attempt_1_rubric_idx = prompt.index("- rubric `semantics-attempt-1`")
    attempt_1_manual_idx = prompt.index(
        "- manual `confirm-attempt-1` (operator):"
    )
    assert attempt_1_idx < attempt_1_rubric_idx < attempt_1_manual_idx
    assert attempt_1_manual_idx < attempt_2_idx

    # Within attempt #2: same (attempt_number, ordinal) ordering applies.
    attempt_2_rubric_idx = prompt.index("- rubric `semantics-attempt-2`")
    attempt_2_manual_idx = prompt.index(
        "- manual `confirm-attempt-2` (operator):"
    )
    assert attempt_2_idx < attempt_2_rubric_idx < attempt_2_manual_idx


def test_manual_finding_with_empty_summary_renders_no_feedback_placeholder() -> None:
    task = _briefed_task()
    lifecycle = Lifecycle(task_id=task.id, run_id="run-1")
    inputs = IterationInputs(
        max_retries=3,
        prior_manual_findings=(
            ManualFinding(
                grader_name="confirm-migration",
                attempt_number=1,
                summary="",
                ordinal=3,
            ),
        ),
    )

    prompt = build_iteration_prompt(task, lifecycle, inputs)

    # The reject-with-absent-feedback path substitutes this placeholder
    # at the resolver so the gate name still appears in the prompt; the
    # renderer falls back to the same placeholder when handed an empty
    # summary directly (defense in depth).
    assert (
        "- manual `confirm-migration` (operator): (no feedback provided)"
    ) in prompt


def test_manual_finding_is_frozen() -> None:
    finding = ManualFinding(
        grader_name="confirm-migration", attempt_number=1, summary="x"
    )
    try:
        finding.summary = "y"  # type: ignore[misc]
    except Exception:
        return
    raise AssertionError("ManualFinding should be frozen")
