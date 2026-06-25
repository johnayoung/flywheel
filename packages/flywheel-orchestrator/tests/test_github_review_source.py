"""Tests for the GitHub PR review-thread work source.

Same injectable-``gh`` seam as :mod:`test_github_ci_source`: a fake runner
records argv and returns canned stdout, so the real ``gh`` binary is never
invoked. Covers listing unresolved threads only (criterion #1), the
default-grader readiness gate (#2), stable per-thread-node-id keying across
replies (#3), a moving ``source_version`` on a new reply (#4), defensive
parsing of malformed output (#6), the no-resolve PR-comment report (#7), and
the full-thread context (#10).
"""

from __future__ import annotations

import json

import pytest

from flywheel_core import Status
from flywheel_core.task import CommandGrader
from flywheel_orchestrator import (
    GithubReviewWorkSource,
    GraderReceipt,
    WorkReport,
    WorkSourceError,
)


class _FakeGh:
    """Scripted gh runner: records argv, returns the canned stdout."""

    def __init__(self, stdout: str = "{}") -> None:
        self.stdout = stdout
        self.calls: list[list[str]] = []

    def __call__(self, argv) -> str:
        self.calls.append(list(argv))
        return self.stdout


def _comment(
    *,
    body: str = "please add a test",
    login: str = "reviewer",
    created_at: str = "2026-06-25T00:00:00Z",
    url: str = "https://github.com/octo/widgets/pull/7#discussion_r1",
) -> dict:
    return {
        "body": body,
        "createdAt": created_at,
        "url": url,
        "author": {"login": login},
    }


def _thread(
    *,
    node_id: str = "RT_thread1",
    is_resolved: bool = False,
    is_outdated: bool = False,
    comments: list[dict] | None = None,
) -> dict:
    return {
        "id": node_id,
        "isResolved": is_resolved,
        "isOutdated": is_outdated,
        "comments": {
            "nodes": comments if comments is not None else [_comment()],
            "pageInfo": {"hasNextPage": False},
        },
    }


def _payload(
    *,
    pr_number: int = 7,
    threads: list[dict] | None = None,
    prs: list[dict] | None = None,
    pr_truncated: bool = False,
    thread_truncated: bool = False,
) -> str:
    if prs is None:
        prs = [
            {
                "number": pr_number,
                "reviewThreads": {
                    "nodes": threads if threads is not None else [_thread()],
                    "pageInfo": {"hasNextPage": thread_truncated},
                },
            }
        ]
    return json.dumps(
        {
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": prs,
                        "pageInfo": {"hasNextPage": pr_truncated},
                    }
                }
            }
        }
    )


def _source(gh: _FakeGh, **kwargs) -> GithubReviewWorkSource:
    defaults = {
        "repo": "octo/widgets",
        "default_graders": (CommandGrader(run="uv run pytest"),),
        "runner": gh,
    }
    defaults.update(kwargs)
    return GithubReviewWorkSource(**defaults)


# --- inbound: list_work -----------------------------------------------------


def test_list_work_queries_graphql() -> None:
    gh = _FakeGh(_payload(threads=[]))
    _source(gh).list_work()

    (argv,) = gh.calls
    assert argv[:2] == ["api", "graphql"]
    joined = " ".join(argv)
    assert "owner=octo" in joined
    assert "name=widgets" in joined
    # isResolved is GraphQL-only (D-2): the query must read it.
    assert "isResolved" in joined


def test_only_unresolved_threads_become_items() -> None:
    gh = _FakeGh(
        _payload(
            threads=[
                _thread(node_id="RT_open", is_resolved=False),
                _thread(node_id="RT_done", is_resolved=True),
            ]
        )
    )
    items = _source(gh).list_work()

    assert len(items) == 1
    assert items[0].task.id.startswith("prc-")


def test_default_graders_become_the_task_graders() -> None:
    grader = CommandGrader(run="uv run pytest")
    gh = _FakeGh(_payload())
    (item,) = _source(gh, default_graders=(grader,)).list_work()

    assert item.task.graders == [grader]


def test_thread_without_default_graders_is_skipped_and_logged() -> None:
    gh = _FakeGh(_payload())
    lines: list[str] = []
    source = _source(gh, default_graders=(), log=lines.append)

    assert source.list_work() == []
    assert len(lines) == 1
    assert "not runnable" in lines[0]


def test_stable_id_across_polls_including_after_reply() -> None:
    # Same thread node id, second poll appends a reply: same item id (D-3),
    # but the change-token (source_version) moves with the new comment (#4).
    first = _FakeGh(_payload(threads=[_thread(node_id="RT_x")]))
    second = _FakeGh(
        _payload(
            threads=[
                _thread(
                    node_id="RT_x",
                    comments=[
                        _comment(),
                        _comment(
                            body="still not addressed",
                            created_at="2026-06-25T01:00:00Z",
                        ),
                    ],
                )
            ]
        )
    )

    (item1,) = _source(first).list_work()
    (item2,) = _source(second).list_work()

    assert item1.task.id == item2.task.id
    assert item1.source_version != item2.source_version


def test_different_thread_node_id_gives_distinct_ids() -> None:
    gh = _FakeGh(
        _payload(
            threads=[
                _thread(node_id="RT_a"),
                _thread(node_id="RT_b"),
            ]
        )
    )
    ids = {i.task.id for i in _source(gh).list_work()}
    assert len(ids) == 2


def test_context_carries_full_thread_and_url() -> None:
    gh = _FakeGh(
        _payload(
            threads=[
                _thread(
                    comments=[
                        _comment(body="please add a test", login="alice"),
                        _comment(body="and rename it", login="bob"),
                    ]
                )
            ]
        )
    )
    (item,) = _source(gh).list_work()

    notes = item.task.context.notes
    assert "please add a test" in notes
    assert "and rename it" in notes
    assert "alice" in notes
    assert "bob" in notes
    assert (
        "https://github.com/octo/widgets/pull/7#discussion_r1"
        in item.task.context.references
    )


def test_provenance_stamps() -> None:
    gh = _FakeGh(_payload(pr_number=42, threads=[_thread(node_id="RT_z")]))
    (item,) = _source(gh).list_work()

    assert item.source_kind == "github_review"
    assert item.source_url is not None
    assert item.source_url.startswith("https://github.com/")
    assert item.source_version  # non-empty digest
    assert item.source_ref == "octo/widgets#42#RT_z"


def test_truncated_pages_are_logged() -> None:
    gh = _FakeGh(_payload(pr_truncated=True, thread_truncated=True))
    lines: list[str] = []
    items = _source(gh, log=lines.append).list_work()

    assert len(items) == 1
    assert any("truncated" in line for line in lines)


def test_invalid_list_payload_raises() -> None:
    gh = _FakeGh("not json at all")
    with pytest.raises(WorkSourceError, match="invalid JSON"):
        _source(gh).list_work()


def test_non_object_payload_raises() -> None:
    gh = _FakeGh(json.dumps([1, 2, 3]))
    with pytest.raises(WorkSourceError, match="expected an object"):
        _source(gh).list_work()


def test_thread_missing_node_id_raises() -> None:
    bad = _thread()
    del bad["id"]
    gh = _FakeGh(_payload(threads=[bad]))
    with pytest.raises(WorkSourceError, match="missing string 'id'"):
        _source(gh).list_work()


# --- outbound: report -------------------------------------------------------


def test_report_comments_on_pr_and_never_resolves() -> None:
    gh = _FakeGh("")
    receipts = (
        GraderReceipt(
            ordinal=0, grader_type="command", name="tests", passed=True
        ),
    )
    report = WorkReport(
        task_id="prc-x",
        source_ref="octo/widgets#7#RT_x",
        run_id="run-abc",
        status=Status.DONE,
        error="",
        graders=receipts,
    )
    _source(gh).report(report)

    (argv,) = gh.calls
    assert argv[:2] == ["pr", "comment"]
    assert "7" in argv
    assert ["--repo", "octo/widgets"] == argv[argv.index("--repo") :][:2]
    body = argv[argv.index("--body") + 1]
    assert "run `run-abc`" in body
    # The harness never resolves a thread (D-5).
    assert not any("resolveReviewThread" in arg for arg in argv)


def test_report_malformed_source_ref_raises() -> None:
    gh = _FakeGh("")
    report = WorkReport(
        task_id="prc-x",
        source_ref="octo/widgets#7",
        run_id="r",
        status=Status.DONE,
        error="",
        graders=(),
    )
    with pytest.raises(
        WorkSourceError, match="malformed github_review source_ref"
    ):
        _source(gh).report(report)
