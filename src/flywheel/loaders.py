from __future__ import annotations

import json
import os
from pathlib import Path
from typing import IO, Any

from flywheel.task import (
    CommandGrader,
    Context,
    Grader,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
    ValidationError,
)


class TaskLoadError(ValueError):
    """Raised when a task source cannot be parsed or fails validation.

    The message always identifies the offending source (file path, or
    ``<path>:<line>`` for JSONL) so callers see actionable errors, not raw
    parser tracebacks.
    """


def load_task_file(path: str | os.PathLike[str]) -> Task:
    """Build a validated ``Task`` from a single JSON file."""
    file_path = Path(path)
    try:
        text = file_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise TaskLoadError(f"{file_path}: cannot read file: {exc}") from exc
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise TaskLoadError(f"{file_path}: invalid JSON: {exc}") from exc
    return _task_from_dict(data, str(file_path))


def load_task_directory(path: str | os.PathLike[str]) -> list[Task]:
    """Build validated ``Task`` instances from every ``.json`` file in ``path``.

    Non-``.json`` entries and subdirectories are ignored. An empty (or
    JSON-less) directory returns an empty list.
    """
    dir_path = Path(path)
    if not dir_path.is_dir():
        raise TaskLoadError(f"{dir_path}: not a directory")
    tasks: list[Task] = []
    for entry in sorted(dir_path.iterdir()):
        if entry.is_file() and entry.suffix == ".json":
            tasks.append(load_task_file(entry))
    return tasks


def load_tasks_jsonl(source: str | os.PathLike[str] | IO[str]) -> list[Task]:
    """Build validated ``Task`` instances from a JSONL stream or path.

    Blank lines and lines whose first non-whitespace character is ``#`` are
    skipped. Trailing newlines are tolerated. Malformed lines raise
    ``TaskLoadError`` citing the source label and 1-based line number.
    """
    if hasattr(source, "read"):
        text = source.read()  # type: ignore[union-attr]
        label = getattr(source, "name", "<stream>")
    else:
        file_path = Path(source)  # type: ignore[arg-type]
        try:
            text = file_path.read_text(encoding="utf-8")
        except OSError as exc:
            raise TaskLoadError(f"{file_path}: cannot read file: {exc}") from exc
        label = str(file_path)

    tasks: list[Task] = []
    for lineno, raw in enumerate(text.splitlines(), start=1):
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        try:
            data = json.loads(stripped)
        except json.JSONDecodeError as exc:
            raise TaskLoadError(
                f"{label}:{lineno}: invalid JSON: {exc}"
            ) from exc
        tasks.append(_task_from_dict(data, f"{label}:{lineno}"))
    return tasks


def _build_grader(entry: dict[str, Any], source: str, idx: int) -> Grader:
    grader_type = entry.get("type")
    name = entry.get("name")
    try:
        if grader_type == "command":
            return CommandGrader(run=entry.get("run", ""), name=name)
        if grader_type == "rubric":
            return RubricGrader(
                assertions=entry.get("assertions") or [],
                rubric=entry.get("rubric"),
                name=name,
                judge_model=entry.get("judge_model"),
                retry_on_fail=entry.get("retry_on_fail", True),
            )
        if grader_type == "manual":
            return ManualGrader(
                instruction=entry.get("instruction", ""),
                name=name,
            )
        if grader_type == "transcript":
            return TranscriptGrader(
                max_turns=entry.get("max_turns"),
                max_total_tokens=entry.get("max_total_tokens"),
                max_wall_seconds=entry.get("max_wall_seconds"),
                name=name,
            )
    except ValidationError as exc:
        raise TaskLoadError(f"{source}: graders[{idx}] {exc}") from exc
    except TypeError as exc:
        raise TaskLoadError(
            f"{source}: graders[{idx}] cannot construct grader: {exc}"
        ) from exc

    raise TaskLoadError(
        f"{source}: graders[{idx}] has unknown type {grader_type!r}; "
        f"expected one of ('command', 'rubric', 'manual', 'transcript')"
    )


def _task_from_dict(data: Any, source: str) -> Task:
    if not isinstance(data, dict):
        raise TaskLoadError(
            f"{source}: expected JSON object, got {type(data).__name__}"
        )

    context_data = data.get("context") or {}
    if not isinstance(context_data, dict):
        raise TaskLoadError(
            f"{source}: 'context' must be an object, got {type(context_data).__name__}"
        )
    context = Context(
        relevant=list(context_data.get("relevant") or []),
        references=list(context_data.get("references") or []),
        constraints=list(context_data.get("constraints") or []),
        non_goals=list(context_data.get("non_goals") or []),
        edge_cases=list(context_data.get("edge_cases") or []),
        notes=context_data.get("notes") or "",
    )

    raw_graders = data.get("graders", [])
    if not isinstance(raw_graders, list):
        raise TaskLoadError(
            f"{source}: 'graders' must be a list, got {type(raw_graders).__name__}"
        )
    graders: list[Grader] = []
    for idx, entry in enumerate(raw_graders):
        if not isinstance(entry, dict):
            raise TaskLoadError(
                f"{source}: graders[{idx}] must be an object, "
                f"got {type(entry).__name__}"
            )
        graders.append(_build_grader(entry, source, idx))

    kwargs: dict[str, Any] = {
        "goal": data.get("goal", ""),
        "graders": graders,
        "context": context,
    }
    if "id" in data:
        kwargs["id"] = data["id"]
    if "prerequisites" in data:
        kwargs["prerequisites"] = list(data["prerequisites"])
    if "tags" in data:
        kwargs["tags"] = list(data["tags"])

    try:
        task = Task(**kwargs)
    except TypeError as exc:
        raise TaskLoadError(f"{source}: cannot construct Task: {exc}") from exc

    try:
        task.validate()
    except ValidationError as exc:
        raise TaskLoadError(f"{source}: {exc}") from exc

    return task
