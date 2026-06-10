"""Tests for the WorkSource seam: the directory reference adapter, the
source-agnostic status rows, and orchestrate() driving a non-file source
end-to-end (with outcome reports flowing back).

Mirrors test_orchestrator.py's approach: real lifecycles through a
file-backed SQLite store, the agent stubbed by a scripted ``invoke``,
command graders running real subprocesses so DONE/FAILED are authentic.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

import pytest
from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator import (
    DirectoryWorkSource,
    WorkItem,
    WorkReport,
    orchestrate,
    select_next_task,
    status_rows_for_items,
)


# --- fixtures / helpers -----------------------------------------------------


def _write_task(
    phase: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    grader_run: str = "true",
) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    return _invoke


def _item(
    task_id: str,
    *,
    grader_run: str = "true",
    prerequisites: tuple[str, ...] = (),
) -> WorkItem:
    return WorkItem(
        task=Task(
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=grader_run)],
            id=task_id,
        ),
        prerequisites=prerequisites,
        source_ref=f"mem://{task_id}",
    )


class _MemorySource:
    """In-memory WorkSource: a fixed item list plus a report inbox."""

    def __init__(self, items: list[WorkItem]) -> None:
        self.items = items
        self.reports: list[WorkReport] = []

    def list_work(self) -> list[WorkItem]:
        return list(self.items)

    def report(self, report: WorkReport) -> None:
        self.reports.append(report)


class _RaisingReportSource(_MemorySource):
    def report(self, report: WorkReport) -> None:
        super().report(report)
        raise RuntimeError("tracker unreachable")


def _run(source, tmp_path: Path, **kwargs):
    return asyncio.run(
        orchestrate(
            source=source,
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            **kwargs,
        )
    )


# --- DirectoryWorkSource ----------------------------------------------------


def test_directory_source_lists_tasks_in_walk_order_with_prereqs(
    tmp_path: Path,
) -> None:
    tasks_dir = tmp_path / "tasks"
    _write_task(tasks_dir / "active" / "02-late", "z-second")
    _write_task(
        tasks_dir / "active" / "01-early",
        "a-first",
        prerequisites=["z-second"],
    )

    items = DirectoryWorkSource(tasks_dir).list_work()

    assert [i.task.id for i in items] == ["a-first", "z-second"]
    assert items[0].prerequisites == ("z-second",)
    assert items[1].prerequisites == ()
    # source_ref is the file path string; local_path is the same as a Path.
    assert items[0].source_ref == str(
        tasks_dir / "active" / "01-early" / "a-first.json"
    )
    assert items[0].local_path == Path(items[0].source_ref)


def test_directory_source_report_is_noop(tmp_path: Path) -> None:
    source = DirectoryWorkSource(tmp_path / "tasks")
    source.report(
        WorkReport(
            task_id="x",
            source_ref="x.json",
            run_id="run-x",
            status=Status.DONE,
            error="",
            graders=(),
        )
    )  # must not raise


# --- source-agnostic rows / selection ---------------------------------------


def test_status_rows_and_selection_work_without_files(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        items = [
            _item("dependent", prerequisites=("base",)),
            _item("base"),
        ]
        rows = status_rows_for_items(items, store)

        assert [r.task.id for r in rows] == ["dependent", "base"]
        assert all(r.task_file == Path() for r in rows)
        assert rows[0].source_ref == "mem://dependent"
        assert rows[0].prerequisites == ("base",)

        # base has no prerequisites and is FRESH -> selected first.
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "base"
    finally:
        store.close()


# --- orchestrate over a non-file source -------------------------------------


def test_orchestrate_drives_memory_source_and_reports(tmp_path: Path) -> None:
    source = _MemorySource([_item("only-task")])

    report = _run(source, tmp_path)

    assert [r.task_id for r in report.runs] == ["only-task"]
    assert report.runs[0].status is Status.DONE

    # The outcome was projected back to the source with grader receipts.
    assert len(source.reports) == 1
    delivered = source.reports[0]
    assert delivered.task_id == "only-task"
    assert delivered.source_ref == "mem://only-task"
    assert delivered.status is Status.DONE
    assert delivered.run_id == report.runs[0].run_id
    assert [g.passed for g in delivered.graders] == [True]
    assert delivered.graders[0].grader_type == "command"


def test_orchestrate_memory_source_respects_prerequisites(
    tmp_path: Path,
) -> None:
    source = _MemorySource(
        [
            _item("dependent", prerequisites=("base",)),
            _item("base"),
        ]
    )

    report = _run(source, tmp_path)

    assert [r.task_id for r in report.runs] == ["base", "dependent"]
    assert all(r.status is Status.DONE for r in report.runs)


def test_orchestrate_reports_failed_runs_with_receipts(
    tmp_path: Path,
) -> None:
    source = _MemorySource([_item("doomed", grader_run="false")])

    report = _run(source, tmp_path)

    assert report.runs[0].status is not Status.DONE
    delivered = source.reports[0]
    assert delivered.status is not Status.DONE
    assert [g.passed for g in delivered.graders] == [False]


def test_report_failure_is_contained(tmp_path: Path) -> None:
    source = _RaisingReportSource([_item("a"), _item("b")])
    stream = io.StringIO()

    report = asyncio.run(
        orchestrate(
            source=source,
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=stream,
        )
    )

    # Both tasks ran to DONE despite every report raising.
    assert sorted(r.task_id for r in report.runs) == ["a", "b"]
    assert all(r.status is Status.DONE for r in report.runs)
    assert len(source.reports) == 2
    assert "work-source report failed" in stream.getvalue()


def test_orchestrate_requires_exactly_one_of_tasks_dir_and_source(
    tmp_path: Path,
) -> None:
    with pytest.raises(ValueError, match="either tasks_dir or source"):
        asyncio.run(
            orchestrate(
                db_path=tmp_path / "db.sqlite",
                sandbox_root=tmp_path / "sb",
            )
        )
    with pytest.raises(ValueError, match="not both"):
        asyncio.run(
            orchestrate(
                tasks_dir=tmp_path / "tasks",
                source=_MemorySource([]),
                db_path=tmp_path / "db.sqlite",
                sandbox_root=tmp_path / "sb",
            )
        )


def test_directory_backed_orchestrate_unchanged(tmp_path: Path) -> None:
    """The historical tasks_dir entry point still works end-to-end."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "legacy-task")

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )

    assert [r.task_id for r in report.runs] == ["legacy-task"]
    assert report.runs[0].status is Status.DONE
