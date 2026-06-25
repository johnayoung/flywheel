"""Tests for the GitHub CI-failure work source.

Same injectable-``gh`` seam as :mod:`test_github_source`: a fake runner
records argv and returns canned stdout, so the real ``gh`` binary is never
invoked. Covers listing failed runs (criterion #1), the default-grader
readiness gate (#2), stable per-(workflow, branch) keying (#5), provenance
stamps (#7), and defensive parsing of malformed output (#9).
"""

from __future__ import annotations

import json

import pytest

from flywheel_core import Status
from flywheel_core.task import CommandGrader
from flywheel_orchestrator import (
    GithubCiWorkSource,
    GraderReceipt,
    WorkReport,
    WorkSourceError,
)


class _FakeGh:
    """Scripted gh runner: records argv, returns the canned stdout."""

    def __init__(self, stdout: str = "[]") -> None:
        self.stdout = stdout
        self.calls: list[list[str]] = []

    def __call__(self, argv) -> str:
        self.calls.append(list(argv))
        return self.stdout


def _run(
    *,
    workflow: str = "CI",
    branch: str = "main",
    head_sha: str = "abc123",
    conclusion: str = "failure",
    url: str = "https://github.com/octo/widgets/actions/runs/1",
    database_id: int = 1,
    created_at: str = "2026-06-25T00:00:00Z",
    display_title: str = "Fix the widget",
    event: str = "push",
) -> dict:
    return {
        "workflowName": workflow,
        "headBranch": branch,
        "headSha": head_sha,
        "conclusion": conclusion,
        "url": url,
        "databaseId": database_id,
        "createdAt": created_at,
        "displayTitle": display_title,
        "event": event,
    }


def _source(gh: _FakeGh, **kwargs) -> GithubCiWorkSource:
    defaults = {
        "repo": "octo/widgets",
        "default_graders": (CommandGrader(run="uv run pytest"),),
        "runner": gh,
    }
    defaults.update(kwargs)
    return GithubCiWorkSource(**defaults)


# --- inbound: list_work -----------------------------------------------------


def test_list_work_queries_failed_runs() -> None:
    gh = _FakeGh(json.dumps([]))
    _source(gh).list_work()

    (argv,) = gh.calls
    assert argv[:2] == ["run", "list"]
    assert ["--repo", "octo/widgets"] == argv[argv.index("--repo") :][:2]
    # Defaults to failed runs, never passing ones (criterion #1).
    assert ["--status", "failure"] == argv[argv.index("--status") :][:2]


def test_failure_filter_is_configurable() -> None:
    gh = _FakeGh(json.dumps([]))
    _source(gh, failure_filter="timed_out").list_work()

    (argv,) = gh.calls
    assert ["--status", "timed_out"] == argv[argv.index("--status") :][:2]


def test_one_item_per_failed_run() -> None:
    gh = _FakeGh(
        json.dumps(
            [
                _run(workflow="CI", branch="main"),
                _run(workflow="Lint", branch="main"),
            ]
        )
    )
    items = _source(gh).list_work()

    assert len(items) == 2
    goals = {i.task.goal for i in items}
    assert any("'CI'" in g for g in goals)
    assert any("'Lint'" in g for g in goals)


def test_default_graders_become_the_task_graders() -> None:
    grader = CommandGrader(run="uv run pytest")
    gh = _FakeGh(json.dumps([_run()]))
    (item,) = _source(gh, default_graders=(grader,)).list_work()

    assert item.task.graders == [grader]


def test_run_without_default_graders_is_skipped_and_logged() -> None:
    gh = _FakeGh(json.dumps([_run(workflow="CI", branch="main")]))
    lines: list[str] = []
    source = _source(gh, default_graders=(), log=lines.append)

    assert source.list_work() == []
    assert len(lines) == 1
    assert "not runnable" in lines[0]


def test_stable_id_across_two_passes_for_same_failure() -> None:
    # First poll, then a re-poll after the head advanced: same (workflow,
    # branch) -> same item id (criterion #5), but source_version moves with
    # the failing head sha (criterion #7).
    first = _FakeGh(json.dumps([_run(head_sha="aaa", conclusion="failure")]))
    second = _FakeGh(json.dumps([_run(head_sha="bbb", conclusion="failure")]))

    (item1,) = _source(first).list_work()
    (item2,) = _source(second).list_work()

    assert item1.task.id == item2.task.id
    assert item1.source_version != item2.source_version


def test_different_workflow_or_branch_gives_distinct_ids() -> None:
    gh = _FakeGh(
        json.dumps(
            [
                _run(workflow="CI", branch="main"),
                _run(workflow="CI", branch="dev"),
            ]
        )
    )
    ids = {i.task.id for i in _source(gh).list_work()}
    assert len(ids) == 2


def test_duplicate_runs_collapse_to_one_item() -> None:
    # Two failed runs of the same workflow on the same branch are ONE
    # persistent failure (D-3), not two items; the most recent wins.
    gh = _FakeGh(
        json.dumps(
            [
                _run(head_sha="old", created_at="2026-06-24T00:00:00Z"),
                _run(head_sha="new", created_at="2026-06-25T00:00:00Z"),
            ]
        )
    )
    items = _source(gh).list_work()

    assert len(items) == 1
    # source_version reflects the most recent failing head.
    newest = _source(_FakeGh(json.dumps([_run(head_sha="new")]))).list_work()
    assert items[0].source_version == newest[0].source_version


def test_provenance_stamps() -> None:
    gh = _FakeGh(
        json.dumps(
            [_run(url="https://github.com/octo/widgets/actions/runs/42")]
        )
    )
    (item,) = _source(gh).list_work()

    assert item.source_kind == "github_ci"
    assert item.source_url == "https://github.com/octo/widgets/actions/runs/42"
    assert item.source_version  # non-empty digest


def test_invalid_list_payload_raises() -> None:
    gh = _FakeGh("not json at all")
    with pytest.raises(WorkSourceError, match="invalid JSON"):
        _source(gh).list_work()


def test_non_list_payload_raises() -> None:
    gh = _FakeGh(json.dumps({"unexpected": "object"}))
    with pytest.raises(WorkSourceError, match="expected a list"):
        _source(gh).list_work()


def test_run_missing_identity_field_raises() -> None:
    bad = _run()
    del bad["headSha"]
    gh = _FakeGh(json.dumps([bad]))
    with pytest.raises(WorkSourceError, match="headSha"):
        _source(gh).list_work()


# --- outbound: report -------------------------------------------------------


def test_report_comments_on_failing_commit() -> None:
    gh = _FakeGh("")
    receipts = (
        GraderReceipt(
            ordinal=0, grader_type="command", name="tests", passed=True
        ),
    )
    report = WorkReport(
        task_id="ci-x",
        source_ref="octo/widgets@abc123",
        run_id="run-abc",
        status=Status.DONE,
        error="",
        graders=receipts,
    )
    _source(gh).report(report)

    (argv,) = gh.calls
    assert argv[0] == "api"
    # The receipt lands on the failing commit, never an issue.
    assert "repos/octo/widgets/commits/abc123/comments" in argv
    assert "issue" not in argv
    body = argv[argv.index("-f") + 1]
    assert body.startswith("body=")
    assert "run `run-abc`" in body


def test_report_malformed_source_ref_raises() -> None:
    gh = _FakeGh("")
    report = WorkReport(
        task_id="ci-x",
        source_ref="no-at-sign",
        run_id="r",
        status=Status.DONE,
        error="",
        graders=(),
    )
    with pytest.raises(WorkSourceError, match="malformed github_ci source_ref"):
        _source(gh).report(report)
