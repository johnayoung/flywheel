import io
import json
from pathlib import Path

import pytest

from flywheel import (
    CommandGrader,
    Context,
    ManualGrader,
    RubricGrader,
    Task,
    TaskLoadError,
    TranscriptGrader,
    load_task_directory,
    load_task_file,
    load_tasks_jsonl,
)
from flywheel.loaders import deserialize_task, serialize_task, task_digest


def _well_formed() -> dict:
    return {
        "id": "demo",
        "goal": "Demo goal.",
        "graders": [{"type": "command", "run": "true"}],
    }


def _fully_briefed() -> dict:
    return {
        "id": "full-demo",
        "goal": "Full demo.",
        "prerequisites": ["setup"],
        "tags": ["x"],
        "context": {
            "relevant": ["src/foo.py"],
            "references": ["src/bar.py"],
            "constraints": ["no new deps"],
            "non_goals": ["docs"],
            "edge_cases": ["empty input"],
            "notes": "see ADR",
        },
        "graders": [
            {"type": "command", "run": "true", "name": "tests"},
            {"type": "rubric", "assertions": ["does the thing"]},
            {"type": "transcript", "max_turns": 5},
            {"type": "manual", "instruction": "check it"},
        ],
    }


# ---------- load_task_file ----------


def test_load_task_file_returns_validated_task(tmp_path: Path) -> None:
    p = tmp_path / "t.json"
    p.write_text(json.dumps(_well_formed()))
    task = load_task_file(p)
    assert isinstance(task, Task)
    assert task.id == "demo"
    assert task.goal == "Demo goal."
    assert isinstance(task.graders[0], CommandGrader)
    assert task.graders[0].type == "command"


def test_load_task_file_dispatches_each_grader_variant(tmp_path: Path) -> None:
    p = tmp_path / "full.json"
    p.write_text(json.dumps(_fully_briefed()))
    task = load_task_file(p)
    assert isinstance(task.graders[0], CommandGrader)
    assert isinstance(task.graders[1], RubricGrader)
    assert isinstance(task.graders[2], TranscriptGrader)
    assert isinstance(task.graders[3], ManualGrader)
    assert task.graders[0].run == "true"
    assert task.graders[1].assertions == ["does the thing"]
    assert task.graders[2].max_turns == 5
    assert task.graders[3].instruction == "check it"


def test_load_task_file_unknown_grader_type_names_index(tmp_path: Path) -> None:
    payload = {**_well_formed(), "graders": [{"type": "bogus", "run": "x"}]}
    p = tmp_path / "bad-type.json"
    p.write_text(json.dumps(payload))
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    msg = str(exc.value)
    assert "graders[0]" in msg
    assert "unknown type" in msg


def test_load_task_file_grader_field_violation_names_index(tmp_path: Path) -> None:
    payload = {**_well_formed(), "graders": [{"type": "command"}]}
    p = tmp_path / "bad-grader.json"
    p.write_text(json.dumps(payload))
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    msg = str(exc.value)
    assert "graders[0]" in msg
    assert "command" in msg


def test_load_task_file_handles_fully_briefed_task(tmp_path: Path) -> None:
    p = tmp_path / "full.json"
    p.write_text(json.dumps(_fully_briefed()))
    task = load_task_file(p)
    # ``prerequisites`` in the file is ignored by core (orchestration-layer
    # concept); the rest of the briefed fields load.
    assert task.context.relevant == ["src/foo.py"]
    assert task.context.notes == "see ADR"
    assert [g.type for g in task.graders] == [
        "command",
        "rubric",
        "transcript",
        "manual",
    ]


def test_load_task_file_malformed_json_names_path(tmp_path: Path) -> None:
    p = tmp_path / "bad.json"
    p.write_text("{not valid json")
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    assert str(p) in str(exc.value)
    assert "invalid JSON" in str(exc.value)


def test_load_task_file_schema_violation_names_path(tmp_path: Path) -> None:
    p = tmp_path / "bad-schema.json"
    p.write_text(json.dumps({"goal": "bad graders", "graders": "nope"}))
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    assert str(p) in str(exc.value)
    assert "graders" in str(exc.value)


def test_load_task_file_missing_file(tmp_path: Path) -> None:
    p = tmp_path / "missing.json"
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    assert str(p) in str(exc.value)


def test_load_task_file_non_object_json(tmp_path: Path) -> None:
    p = tmp_path / "array.json"
    p.write_text("[1, 2, 3]")
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    assert "expected JSON object" in str(exc.value)
    assert str(p) in str(exc.value)


# ---------- load_task_directory ----------


def test_load_task_directory_returns_all_json_tasks(tmp_path: Path) -> None:
    (tmp_path / "a.json").write_text(json.dumps({**_well_formed(), "id": "a"}))
    (tmp_path / "b.json").write_text(json.dumps({**_well_formed(), "id": "b"}))
    tasks = load_task_directory(tmp_path)
    assert sorted(t.id for t in tasks) == ["a", "b"]


def test_load_task_directory_ignores_non_json_entries(tmp_path: Path) -> None:
    (tmp_path / "a.json").write_text(json.dumps({**_well_formed(), "id": "a"}))
    (tmp_path / "notes.txt").write_text("just text, not a task")
    (tmp_path / "README.md").write_text("# heading")
    (tmp_path / "subdir").mkdir()
    (tmp_path / "subdir" / "nested.json").write_text(
        json.dumps({**_well_formed(), "id": "nested"})
    )
    tasks = load_task_directory(tmp_path)
    assert [t.id for t in tasks] == ["a"]


def test_load_task_directory_empty_returns_empty_list(tmp_path: Path) -> None:
    assert load_task_directory(tmp_path) == []


def test_load_task_directory_malformed_member_names_offending_file(
    tmp_path: Path,
) -> None:
    (tmp_path / "good.json").write_text(json.dumps(_well_formed()))
    bad = tmp_path / "bad.json"
    bad.write_text("{not json")
    with pytest.raises(TaskLoadError) as exc:
        load_task_directory(tmp_path)
    assert str(bad) in str(exc.value)


def test_load_task_directory_rejects_non_directory(tmp_path: Path) -> None:
    file_path = tmp_path / "x.json"
    file_path.write_text(json.dumps(_well_formed()))
    with pytest.raises(TaskLoadError) as exc:
        load_task_directory(file_path)
    assert "not a directory" in str(exc.value)


# ---------- load_tasks_jsonl ----------


def test_load_tasks_jsonl_from_path_returns_all(tmp_path: Path) -> None:
    p = tmp_path / "stream.jsonl"
    lines = [
        json.dumps({**_well_formed(), "id": "a"}),
        json.dumps({**_well_formed(), "id": "b"}),
    ]
    p.write_text("\n".join(lines) + "\n")
    tasks = load_tasks_jsonl(p)
    assert [t.id for t in tasks] == ["a", "b"]


def test_load_tasks_jsonl_from_stream(tmp_path: Path) -> None:
    payload = (
        json.dumps({**_well_formed(), "id": "a"})
        + "\n"
        + json.dumps({**_well_formed(), "id": "b"})
        + "\n"
    )
    stream = io.StringIO(payload)
    tasks = load_tasks_jsonl(stream)
    assert [t.id for t in tasks] == ["a", "b"]


def test_load_tasks_jsonl_skips_blank_and_comment_lines(tmp_path: Path) -> None:
    p = tmp_path / "stream.jsonl"
    p.write_text(
        "\n"
        "# a leading comment\n"
        f"{json.dumps({**_well_formed(), 'id': 'a'})}\n"
        "\n"
        "   \n"
        "# another comment\n"
        f"{json.dumps({**_well_formed(), 'id': 'b'})}\n"
        "\n"
    )
    tasks = load_tasks_jsonl(p)
    assert [t.id for t in tasks] == ["a", "b"]


def test_load_tasks_jsonl_empty_returns_empty_list(tmp_path: Path) -> None:
    p = tmp_path / "empty.jsonl"
    p.write_text("")
    assert load_tasks_jsonl(p) == []


def test_load_tasks_jsonl_only_comments_returns_empty_list(tmp_path: Path) -> None:
    p = tmp_path / "comments.jsonl"
    p.write_text("# just\n# comments\n\n")
    assert load_tasks_jsonl(p) == []


def test_load_tasks_jsonl_malformed_line_names_line_number(tmp_path: Path) -> None:
    p = tmp_path / "bad.jsonl"
    p.write_text(
        json.dumps({**_well_formed(), "id": "a"}) + "\n"
        "{not json on line two\n"
        + json.dumps({**_well_formed(), "id": "c"}) + "\n"
    )
    with pytest.raises(TaskLoadError) as exc:
        load_tasks_jsonl(p)
    msg = str(exc.value)
    assert str(p) in msg
    assert ":2" in msg
    assert "invalid JSON" in msg


def test_load_tasks_jsonl_schema_violation_names_line_number(tmp_path: Path) -> None:
    p = tmp_path / "bad-schema.jsonl"
    bad = json.dumps({"goal": "x", "graders": "nope"})
    p.write_text(
        json.dumps({**_well_formed(), "id": "a"}) + "\n"
        + bad + "\n"
    )
    with pytest.raises(TaskLoadError) as exc:
        load_tasks_jsonl(p)
    msg = str(exc.value)
    assert ":2" in msg
    assert "graders" in msg


def test_load_tasks_jsonl_stream_without_name_uses_stream_label() -> None:
    stream = io.StringIO("{not json\n")
    with pytest.raises(TaskLoadError) as exc:
        load_tasks_jsonl(stream)
    assert ":1" in str(exc.value)


# ---------- rubric grader optional fields ----------


def test_load_task_file_parses_rubric_judge_model_and_retry_on_fail(
    tmp_path: Path,
) -> None:
    payload = {
        **_well_formed(),
        "graders": [
            {
                "type": "rubric",
                "assertions": ["does the thing"],
                "judge_model": "claude-haiku-4-5",
                "retry_on_fail": False,
            }
        ],
    }
    p = tmp_path / "rubric.json"
    p.write_text(json.dumps(payload))
    task = load_task_file(p)
    grader = task.graders[0]
    assert isinstance(grader, RubricGrader)
    assert grader.judge_model == "claude-haiku-4-5"
    assert grader.retry_on_fail is False


def test_load_task_file_rubric_defaults_when_fields_omitted(tmp_path: Path) -> None:
    payload = {
        **_well_formed(),
        "graders": [{"type": "rubric", "assertions": ["does the thing"]}],
    }
    p = tmp_path / "rubric-defaults.json"
    p.write_text(json.dumps(payload))
    task = load_task_file(p)
    grader = task.graders[0]
    assert isinstance(grader, RubricGrader)
    assert grader.judge_model is None
    assert grader.retry_on_fail is True


def test_load_task_file_rejects_retry_on_fail_string(tmp_path: Path) -> None:
    payload = {
        **_well_formed(),
        "graders": [
            {
                "type": "rubric",
                "assertions": ["does the thing"],
                "retry_on_fail": "false",
            }
        ],
    }
    p = tmp_path / "bad-retry.json"
    p.write_text(json.dumps(payload))
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    msg = str(exc.value)
    assert "graders[0]" in msg
    assert "retry_on_fail" in msg


# ---------- serialize_task / deserialize_task / task_digest ----------


def _minimal_task() -> Task:
    return Task(
        id="demo",
        goal="Demo goal.",
        graders=[CommandGrader(run="true")],
    )


def _briefed_task() -> Task:
    return deserialize_task(_fully_briefed())


def test_serialize_then_deserialize_round_trips_minimal() -> None:
    task = _minimal_task()
    assert deserialize_task(serialize_task(task)) == task


def test_serialize_then_deserialize_round_trips_fully_briefed() -> None:
    task = _briefed_task()
    restored = deserialize_task(serialize_task(task))
    assert restored == task
    # Every grader variant survives the round trip with its concrete type.
    assert [type(g) for g in restored.graders] == [
        CommandGrader,
        RubricGrader,
        TranscriptGrader,
        ManualGrader,
    ]


def test_serialize_task_matches_loader_input_shape() -> None:
    # The dict serialize_task emits is itself loadable, so it is the same
    # shape task files use.
    task = _briefed_task()
    assert deserialize_task(serialize_task(task)) == task


def test_task_digest_is_stable_across_reserialization() -> None:
    task = _briefed_task()
    assert task_digest(task) == task_digest(deserialize_task(serialize_task(task)))


def test_task_digest_ignores_id() -> None:
    a = Task(id="one", goal="g", graders=[CommandGrader(run="true")])
    b = Task(id="two", goal="g", graders=[CommandGrader(run="true")])
    assert task_digest(a) == task_digest(b)


def test_task_digest_changes_when_definition_changes() -> None:
    # The digest covers the definition: goal, graders, tags, context.
    base = Task(id="x", goal="g", graders=[CommandGrader(run="true")])
    digest = task_digest(base)
    assert task_digest(Task(id="x", goal="g2", graders=base.graders)) != digest
    assert (
        task_digest(Task(id="x", goal="g", graders=[CommandGrader(run="false")]))
        != digest
    )
    assert (
        task_digest(Task(id="x", goal="g", graders=base.graders, tags=["a"]))
        != digest
    )
    assert (
        task_digest(
            Task(
                id="x",
                goal="g",
                graders=base.graders,
                context=Context(notes="changed"),
            )
        )
        != digest
    )




def test_load_task_file_rejects_non_string_judge_model(tmp_path: Path) -> None:
    payload = {
        **_well_formed(),
        "graders": [
            {
                "type": "rubric",
                "assertions": ["does the thing"],
                "judge_model": 123,
            }
        ],
    }
    p = tmp_path / "bad-judge-model.json"
    p.write_text(json.dumps(payload))
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    msg = str(exc.value)
    assert "graders[0]" in msg
    assert "judge_model" in msg


# ---------- load_task_data / load_graders (non-file twins) ----------


def test_load_task_data_builds_validated_task() -> None:
    from flywheel import load_task_data

    task = load_task_data(
        {
            "id": "from-data",
            "goal": "Build from a decoded payload.",
            "graders": [{"type": "command", "run": "true"}],
        },
        source="api-payload",
    )
    assert task.id == "from-data"
    assert isinstance(task.graders[0], CommandGrader)


def test_load_task_data_errors_cite_the_source_label() -> None:
    from flywheel import load_task_data

    with pytest.raises(TaskLoadError) as exc:
        load_task_data({"goal": "", "graders": []}, source="issue#42")
    assert "issue#42" in str(exc.value)


def test_load_graders_applies_grader_validation() -> None:
    from flywheel import load_graders

    graders = load_graders(
        [
            {"type": "command", "run": "uv run pytest"},
            {"type": "transcript", "max_turns": 9},
        ],
        source="policy",
    )
    assert len(graders) == 2
    assert isinstance(graders[0], CommandGrader)

    with pytest.raises(TaskLoadError) as exc:
        load_graders([{"type": "vibes"}], source="policy")
    assert "policy" in str(exc.value)
    assert "unknown type" in str(exc.value)

    with pytest.raises(TaskLoadError) as exc2:
        load_graders({"type": "command"}, source="policy")
    assert "must be a list" in str(exc2.value)


# ---------- coexistence with direct construction ----------


def test_direct_task_construction_remains_unchanged() -> None:
    # No loader involvement; identical to roadmap-01-task-dataclass behavior.
    from flywheel import Context

    task = Task(
        goal="Direct.",
        graders=[CommandGrader(run="true")],
        context=Context(relevant=["src/foo.py"]),
    )
    task.validate()
    assert task.context.relevant == ["src/foo.py"]
