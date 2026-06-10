import pytest

from flywheel_core import (
    CommandGrader,
    Context,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
    ValidationError,
)
from flywheel_core.task import KNOWN_GRADER_TYPES


def test_construct_task_directly_and_validate() -> None:
    task = Task(
        goal="Expose a Task dataclass.",
        graders=[CommandGrader(run="uv run pytest")],
    )
    task.validate()


def test_default_id_is_uuid_derived_and_has_no_whitespace() -> None:
    t1 = Task(goal="g", graders=[CommandGrader(run="x")])
    t2 = Task(goal="g", graders=[CommandGrader(run="x")])
    assert t1.id != t2.id
    assert not any(ch.isspace() for ch in t1.id)


def test_context_defaults_to_empty() -> None:
    task = Task(goal="g", graders=[CommandGrader(run="x")])
    assert task.context == Context()
    assert task.tags == []


def test_full_construction_passes_validate() -> None:
    task = Task(
        id="add-retry",
        goal="Retry on 5xx.",
        tags=["http"],
        context=Context(
            relevant=["src/flywheel/http/client.py"],
            constraints=["No new packages"],
            edge_cases=["Respect Retry-After"],
            notes="See ADR-12",
        ),
        graders=[
            CommandGrader(run="uv run pytest", name="tests"),
            RubricGrader(assertions=["Retries on 5xx only"]),
            ManualGrader(instruction="Confirm jitter shape"),
            TranscriptGrader(max_turns=20),
        ],
    )
    task.validate()


def test_validate_rejects_missing_goal() -> None:
    task = Task(goal="", graders=[CommandGrader(run="x")])
    with pytest.raises(ValidationError, match="goal"):
        task.validate()


def test_validate_rejects_whitespace_only_goal() -> None:
    task = Task(goal="   \n  ", graders=[CommandGrader(run="x")])
    with pytest.raises(ValidationError, match="goal"):
        task.validate()


def test_validate_accepts_empty_graders() -> None:
    # Graders are optional: a graderless task is an unverified run that
    # records DONE on the agent's own claim. The harness verify path
    # tolerates an empty list (all-pass is vacuously true).
    task = Task(goal="g", graders=[])
    task.validate()


def test_validate_rejects_non_list_graders() -> None:
    task = Task(goal="g", graders="nope")  # type: ignore[arg-type]
    with pytest.raises(ValidationError, match="graders"):
        task.validate()


def test_validate_rejects_whitespace_in_id() -> None:
    task = Task(id="bad id", goal="g", graders=[CommandGrader(run="x")])
    with pytest.raises(ValidationError, match="whitespace"):
        task.validate()


def test_transcript_grader_rejects_construction_with_no_limits() -> None:
    with pytest.raises(ValidationError, match="transcript"):
        TranscriptGrader()


def test_transcript_grader_accepts_any_single_limit() -> None:
    for kwargs in (
        {"max_turns": 5},
        {"max_total_tokens": 1000},
        {"max_wall_seconds": 30.0},
    ):
        task = Task(goal="g", graders=[TranscriptGrader(**kwargs)])
        task.validate()


def test_command_grader_rejects_construction_without_run() -> None:
    with pytest.raises(TypeError):
        CommandGrader()  # type: ignore[call-arg]


def test_command_grader_rejects_empty_run() -> None:
    with pytest.raises(ValidationError, match="command"):
        CommandGrader(run="")


def test_rubric_grader_rejects_construction_without_assertions() -> None:
    with pytest.raises(TypeError):
        RubricGrader()  # type: ignore[call-arg]


def test_rubric_grader_rejects_empty_assertions() -> None:
    with pytest.raises(ValidationError, match="rubric"):
        RubricGrader(assertions=[])


def test_rubric_grader_defaults_judge_model_and_retry_on_fail() -> None:
    grader = RubricGrader(assertions=["does the thing"])
    assert grader.judge_model is None
    assert grader.retry_on_fail is True


def test_rubric_grader_round_trips_judge_model_and_retry_on_fail() -> None:
    grader = RubricGrader(
        assertions=["does the thing"],
        judge_model="claude-haiku-4-5",
        retry_on_fail=False,
    )
    assert grader.judge_model == "claude-haiku-4-5"
    assert grader.retry_on_fail is False


def test_rubric_grader_rejects_non_bool_retry_on_fail() -> None:
    with pytest.raises(ValidationError, match="retry_on_fail"):
        RubricGrader(assertions=["x"], retry_on_fail="false")  # type: ignore[arg-type]


def test_rubric_grader_rejects_non_string_judge_model() -> None:
    with pytest.raises(ValidationError, match="judge_model"):
        RubricGrader(assertions=["x"], judge_model=123)  # type: ignore[arg-type]


def test_manual_grader_rejects_construction_without_instruction() -> None:
    with pytest.raises(TypeError):
        ManualGrader()  # type: ignore[call-arg]


def test_manual_grader_rejects_empty_instruction() -> None:
    with pytest.raises(ValidationError, match="manual"):
        ManualGrader(instruction="")


def test_grader_variants_carry_correct_type_tag() -> None:
    assert CommandGrader(run="x").type == "command"
    assert RubricGrader(assertions=["a"]).type == "rubric"
    assert ManualGrader(instruction="i").type == "manual"
    assert TranscriptGrader(max_turns=1).type == "transcript"


def test_known_grader_types_match_schema() -> None:
    assert set(KNOWN_GRADER_TYPES) == {"command", "rubric", "manual", "transcript"}
