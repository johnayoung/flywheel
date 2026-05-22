"""Behavioural tests for :mod:`flywheel.prompt`.

Covers determinism, content surface (goal / context / graders /
lifecycle), envelope-contract fidelity, and lifecycle-state reflection
between a first-attempt prompt and a retry prompt.
"""

from datetime import datetime, timezone

from flywheel import (
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
from flywheel.envelope import CLOSING_FENCE, Intent, OPENING_FENCE


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
        prerequisites=["setup-http-client"],
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
