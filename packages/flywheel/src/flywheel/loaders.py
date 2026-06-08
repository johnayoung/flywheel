from __future__ import annotations

import hashlib
import json
import os
from collections.abc import Mapping
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
    if "tags" in data:
        kwargs["tags"] = list(data["tags"])
    # ``prerequisites`` is an orchestration-layer concept (the inter-task DAG),
    # not part of a single task's definition — flywheel core ignores it here.
    # Consumers that schedule across tasks parse it from the task source
    # themselves (see flywheel_orchestrator).

    try:
        task = Task(**kwargs)
    except TypeError as exc:
        raise TaskLoadError(f"{source}: cannot construct Task: {exc}") from exc

    try:
        task.validate()
    except ValidationError as exc:
        raise TaskLoadError(f"{source}: {exc}") from exc

    return task


# --- Serialization (Task -> dict / digest) ----------------------------------
#
# The inverse of ``_task_from_dict``. Lives here, not in ``flywheel.task``,
# because that module is purity-enforced (no json/io). Concrete stores call
# these to persist a Task and to content-address it for the ``tasks`` table.


def _grader_to_dict(grader: Grader) -> dict[str, Any]:
    """Project a grader back into the JSON object ``_build_grader`` consumes.

    Optional fields that are ``None`` are omitted so the output matches the
    on-disk file shape; ``serialize_task`` -> ``deserialize_task`` round-trips
    to an equal :class:`Task`.
    """
    if isinstance(grader, CommandGrader):
        spec: dict[str, Any] = {"type": "command", "run": grader.run}
        if grader.name is not None:
            spec["name"] = grader.name
        return spec
    if isinstance(grader, RubricGrader):
        spec = {
            "type": "rubric",
            "assertions": list(grader.assertions),
            "retry_on_fail": grader.retry_on_fail,
        }
        if grader.rubric is not None:
            spec["rubric"] = grader.rubric
        if grader.name is not None:
            spec["name"] = grader.name
        if grader.judge_model is not None:
            spec["judge_model"] = grader.judge_model
        return spec
    if isinstance(grader, ManualGrader):
        spec = {"type": "manual", "instruction": grader.instruction}
        if grader.name is not None:
            spec["name"] = grader.name
        return spec
    if isinstance(grader, TranscriptGrader):
        spec = {"type": "transcript"}
        if grader.max_turns is not None:
            spec["max_turns"] = grader.max_turns
        if grader.max_total_tokens is not None:
            spec["max_total_tokens"] = grader.max_total_tokens
        if grader.max_wall_seconds is not None:
            spec["max_wall_seconds"] = grader.max_wall_seconds
        if grader.name is not None:
            spec["name"] = grader.name
        return spec
    raise TypeError(f"cannot serialize unknown grader {type(grader)!r}")


def _context_to_dict(context: Context) -> dict[str, Any]:
    return {
        "relevant": list(context.relevant),
        "references": list(context.references),
        "constraints": list(context.constraints),
        "non_goals": list(context.non_goals),
        "edge_cases": list(context.edge_cases),
        "notes": context.notes,
    }


def serialize_task(task: Task) -> dict[str, Any]:
    """Return the JSON-compatible dict form of ``task``.

    Exact inverse of :func:`_task_from_dict`: feeding the result to
    :func:`deserialize_task` reconstructs an equal :class:`Task`.
    """
    return {
        "id": task.id,
        "goal": task.goal,
        "graders": [_grader_to_dict(g) for g in task.graders],
        "tags": list(task.tags),
        "context": _context_to_dict(task.context),
    }


def deserialize_task(data: Mapping[str, Any]) -> Task:
    """Reconstruct a validated :class:`Task` from its serialized dict form."""
    return _task_from_dict(dict(data), "<serialized task>")


def task_digest(task: Task) -> str:
    """Content hash of a task's definition: goal, graders, tags, context.

    Two tasks with the same goal/graders/tags/context share a digest
    regardless of ``id``. Editing the goal, a grader, a tag, or the context
    yields a new digest (a new immutable version a run can pin). ``id`` is
    excluded so identity does not shape the version. ``prerequisites`` is
    excluded because it is not a flywheel-core concept at all — the dependency
    DAG belongs to the orchestration layer built on top of flywheel, not to
    the single-task definition this hash addresses. Used as the version
    discriminator in the content-addressed ``task_versions`` store, so a run
    can pin the exact definition it executed.
    """
    definition = {
        "goal": task.goal,
        "graders": [_grader_to_dict(g) for g in task.graders],
        "tags": list(task.tags),
        "context": _context_to_dict(task.context),
    }
    canonical = json.dumps(definition, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()
