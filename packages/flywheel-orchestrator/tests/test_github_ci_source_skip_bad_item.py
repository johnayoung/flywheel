"""github_ci skips one uncompilable run row instead of aborting the listing.

A single malformed run (a non-object entry, a row missing a required identity
string, a definition that fails validation) must not hide every *other*
failing run: it is skipped, counted in ``last_skipped_count``, and named in one
``log`` line, while every valid run still surfaces. The one exception is a page
where rows were dropped yet *nothing* compiled -- that re-raises, because a
parse break must never read as "CI is green" (the existing
``test_run_missing_identity_field_raises`` pins that single-bad-row case).
"""

from __future__ import annotations

import json

import pytest

from flywheel_core.task import CommandGrader
from flywheel_orchestrator import GithubCiWorkSource, WorkSourceError


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


def test_one_bad_row_is_skipped_and_the_valid_runs_still_surface() -> None:
    # Two valid runs and one row missing the headSha identity string. The bad
    # row is dropped; the result is exactly the two valid items, never an
    # empty or aborted listing (the edge case).
    bad = _run(workflow="Broken", branch="main")
    del bad["headSha"]
    gh = _FakeGh(
        json.dumps(
            [
                _run(workflow="CI", branch="main"),
                bad,
                _run(workflow="Lint", branch="main"),
            ]
        )
    )
    lines: list[str] = []
    source = _source(gh, log=lines.append)

    items = source.list_work()

    # Exactly the valid items survive -- an empty/aborted listing fails here.
    assert len(items) == 2
    goals = {i.task.goal for i in items}
    assert any("'CI'" in g for g in goals)
    assert any("'Lint'" in g for g in goals)
    assert not any("'Broken'" in g for g in goals)


def test_the_skip_is_recorded_as_a_count() -> None:
    bad = _run(workflow="Broken", branch="main")
    del bad["headSha"]
    gh = _FakeGh(json.dumps([_run(workflow="CI", branch="main"), bad]))
    source = _source(gh)

    source.list_work()

    assert source.last_skipped_count == 1


def test_the_skip_is_logged_naming_the_bad_run() -> None:
    bad = _run(workflow="Broken", branch="dev")
    del bad["headSha"]
    gh = _FakeGh(json.dumps([_run(workflow="CI", branch="main"), bad]))
    lines: list[str] = []

    _source(gh, log=lines.append).list_work()

    skip_lines = [ln for ln in lines if "skipping malformed run" in ln]
    assert len(skip_lines) == 1
    # The line names the offending field so the drop is investigable.
    assert "headSha" in skip_lines[0]
    assert "[github_ci]" in skip_lines[0]


def test_non_object_row_is_skipped_not_fatal() -> None:
    # A non-object entry cannot be compiled either; it is dropped, counted,
    # and the valid run still surfaces.
    gh = _FakeGh(json.dumps([_run(workflow="CI", branch="main"), "not-an-object"]))
    source = _source(gh)

    items = source.list_work()

    assert len(items) == 1
    assert source.last_skipped_count == 1


def test_count_resets_to_zero_on_a_clean_pass() -> None:
    bad = _run(workflow="Broken", branch="main")
    del bad["headSha"]
    source = _source(_FakeGh(json.dumps([_run(workflow="CI"), bad])))
    source.list_work()
    assert source.last_skipped_count == 1

    # A subsequent clean listing must not carry the prior skip forward.
    clean = _FakeGh(json.dumps([_run(workflow="CI", branch="main")]))
    source._run = clean
    source.list_work()
    assert source.last_skipped_count == 0


def test_all_rows_dropped_reraises_so_a_parse_break_is_not_green() -> None:
    # If the page held rows yet nothing compiled because every row was bad,
    # returning an empty list would read as "CI is green". Re-raise instead.
    bad = _run()
    del bad["headSha"]
    gh = _FakeGh(json.dumps([bad]))

    with pytest.raises(WorkSourceError, match="headSha"):
        _source(gh).list_work()


def test_no_grader_skip_is_not_counted_as_a_parse_break() -> None:
    # A run with no default grader policy is "not runnable" (D-4), a policy
    # decision -- not malformed output. It yields an empty list without
    # raising and without incrementing the malformed-skip count.
    gh = _FakeGh(json.dumps([_run(workflow="CI", branch="main")]))
    source = _source(gh, default_graders=())

    assert source.list_work() == []
    assert source.last_skipped_count == 0
