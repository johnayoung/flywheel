import io
import json
from pathlib import Path

import pytest

from flywheel import (
    Task,
    TaskLoadError,
    load_task_directory,
    load_task_file,
    load_tasks_jsonl,
)


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
    assert task.graders[0].type == "command"


def test_load_task_file_handles_fully_briefed_task(tmp_path: Path) -> None:
    p = tmp_path / "full.json"
    p.write_text(json.dumps(_fully_briefed()))
    task = load_task_file(p)
    assert task.prerequisites == ["setup"]
    assert task.context.relevant == ["src/foo.py"]
    assert task.context.notes == "see ADR"
    assert [g.type for g in task.graders] == [
        "command",
        "rubric",
        "transcript",
        "manual",
    ]


def test_load_task_file_loads_real_repo_task() -> None:
    repo_task = Path(__file__).resolve().parents[1] / "tasks" / "roadmap-01" / "roadmap-01-task-dataclass.json"
    task = load_task_file(repo_task)
    assert task.id == "roadmap-01-task-dataclass"


def test_load_task_file_malformed_json_names_path(tmp_path: Path) -> None:
    p = tmp_path / "bad.json"
    p.write_text("{not valid json")
    with pytest.raises(TaskLoadError) as exc:
        load_task_file(p)
    assert str(p) in str(exc.value)
    assert "invalid JSON" in str(exc.value)


def test_load_task_file_schema_violation_names_path(tmp_path: Path) -> None:
    p = tmp_path / "bad-schema.json"
    p.write_text(json.dumps({"goal": "no graders", "graders": []}))
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
    bad = json.dumps({"goal": "x", "graders": []})
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


# ---------- coexistence with direct construction ----------


def test_direct_task_construction_remains_unchanged() -> None:
    # No loader involvement; identical to roadmap-01-task-dataclass behavior.
    from flywheel import Context, Grader

    task = Task(
        goal="Direct.",
        graders=[Grader(type="command", run="true")],
        context=Context(relevant=["src/foo.py"]),
    )
    task.validate()
    assert task.context.relevant == ["src/foo.py"]
