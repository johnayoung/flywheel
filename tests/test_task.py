import pytest

from flywheel import Context, Grader, Task, ValidationError
from flywheel.task import KNOWN_GRADER_TYPES


def test_construct_task_directly_and_validate() -> None:
    task = Task(
        goal="Expose a Task dataclass.",
        graders=[Grader(type="command", run="uv run pytest")],
    )
    task.validate()


def test_default_id_is_uuid_derived_and_has_no_whitespace() -> None:
    t1 = Task(goal="g", graders=[Grader(type="command", run="x")])
    t2 = Task(goal="g", graders=[Grader(type="command", run="x")])
    assert t1.id != t2.id
    assert not any(ch.isspace() for ch in t1.id)


def test_context_defaults_to_empty() -> None:
    task = Task(goal="g", graders=[Grader(type="command", run="x")])
    assert task.context == Context()
    assert task.tags == []
    assert task.prerequisites == []


def test_full_construction_passes_validate() -> None:
    task = Task(
        id="add-retry",
        goal="Retry on 5xx.",
        prerequisites=["setup-client"],
        tags=["http"],
        context=Context(
            relevant=["src/flywheel/http/client.py"],
            constraints=["No new packages"],
            edge_cases=["Respect Retry-After"],
            notes="See ADR-12",
        ),
        graders=[
            Grader(type="command", run="uv run pytest", name="tests"),
            Grader(type="rubric", assertions=["Retries on 5xx only"]),
            Grader(type="manual", instruction="Confirm jitter shape"),
            Grader(type="transcript", max_turns=20),
        ],
    )
    task.validate()


def test_validate_rejects_missing_goal() -> None:
    task = Task(goal="", graders=[Grader(type="command", run="x")])
    with pytest.raises(ValidationError, match="goal"):
        task.validate()


def test_validate_rejects_whitespace_only_goal() -> None:
    task = Task(goal="   \n  ", graders=[Grader(type="command", run="x")])
    with pytest.raises(ValidationError, match="goal"):
        task.validate()


def test_validate_rejects_empty_graders() -> None:
    task = Task(goal="g", graders=[])
    with pytest.raises(ValidationError, match="graders"):
        task.validate()


def test_validate_rejects_self_referential_prerequisites() -> None:
    task = Task(
        id="t1",
        goal="g",
        graders=[Grader(type="command", run="x")],
        prerequisites=["t1"],
    )
    with pytest.raises(ValidationError, match="prerequisites"):
        task.validate()


def test_validate_rejects_whitespace_in_id() -> None:
    task = Task(id="bad id", goal="g", graders=[Grader(type="command", run="x")])
    with pytest.raises(ValidationError, match="whitespace"):
        task.validate()


def test_validate_rejects_transcript_with_no_limits() -> None:
    task = Task(goal="g", graders=[Grader(type="transcript")])
    with pytest.raises(ValidationError, match="transcript"):
        task.validate()


def test_validate_accepts_transcript_with_any_single_limit() -> None:
    for kwargs in (
        {"max_turns": 5},
        {"max_total_tokens": 1000},
        {"max_wall_seconds": 30.0},
    ):
        task = Task(goal="g", graders=[Grader(type="transcript", **kwargs)])
        task.validate()


def test_validate_rejects_unknown_grader_type() -> None:
    task = Task(goal="g", graders=[Grader(type="bogus", run="x")])
    with pytest.raises(ValidationError, match="unknown type"):
        task.validate()


def test_validate_rejects_command_grader_without_run() -> None:
    task = Task(goal="g", graders=[Grader(type="command")])
    with pytest.raises(ValidationError, match="command"):
        task.validate()


def test_validate_rejects_rubric_grader_without_assertions() -> None:
    task = Task(goal="g", graders=[Grader(type="rubric")])
    with pytest.raises(ValidationError, match="rubric"):
        task.validate()


def test_validate_rejects_rubric_grader_with_empty_assertions() -> None:
    task = Task(goal="g", graders=[Grader(type="rubric", assertions=[])])
    with pytest.raises(ValidationError, match="rubric"):
        task.validate()


def test_validate_rejects_manual_grader_without_instruction() -> None:
    task = Task(goal="g", graders=[Grader(type="manual")])
    with pytest.raises(ValidationError, match="manual"):
        task.validate()


def test_known_grader_types_match_schema() -> None:
    assert set(KNOWN_GRADER_TYPES) == {"command", "rubric", "manual", "transcript"}
