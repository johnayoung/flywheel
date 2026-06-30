"""github_review skips ONE uncompilable thread node, keeps the rest.

A single *identifiable* review-thread node (its GraphQL node id is in hand)
that cannot be compiled into a valid work item is dropped one at a time —
counted and logged by node id — while every other valid unresolved thread on
the page still returns. This is the per-node case: a malformed individual
thread NODE, as distinct from a structurally-broken top-level envelope (a
thread with no node id at all), which still aborts the whole listing so a parse
break never masquerades as "no unresolved threads".
"""

from __future__ import annotations

import json

import pytest

from flywheel_core.task import CommandGrader
from flywheel_orchestrator import (
    GithubReviewWorkSource,
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


def _good_thread(node_id: str) -> dict:
    """A well-formed unresolved thread that compiles to a work item."""
    return {
        "id": node_id,
        "isResolved": False,
        "isOutdated": False,
        "comments": {
            "nodes": [_comment()],
            "pageInfo": {"hasNextPage": False},
        },
    }


def _bad_thread(node_id: str = "RT_bad") -> dict:
    """Identifiable (has a node id) but uncompilable: comments is not an object.

    The node id is present, so the adapter can NAME this node when it skips it;
    the malformed ``comments`` connection is what makes it fail to compile.
    """
    return {
        "id": node_id,
        "isResolved": False,
        "isOutdated": False,
        "comments": "not-an-object",
    }


def _payload(threads: list[dict], *, pr_number: int = 7) -> str:
    return json.dumps(
        {
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": [
                            {
                                "number": pr_number,
                                "reviewThreads": {
                                    "nodes": threads,
                                    "pageInfo": {"hasNextPage": False},
                                },
                            }
                        ],
                        "pageInfo": {"hasNextPage": False},
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


def test_one_bad_node_is_skipped_and_the_valid_threads_still_return() -> None:
    # Two valid threads sandwich one malformed-but-identifiable node.
    gh = _FakeGh(
        _payload(
            [
                _good_thread("RT_a"),
                _bad_thread("RT_bad"),
                _good_thread("RT_b"),
            ]
        )
    )
    lines: list[str] = []
    items = _source(gh, log=lines.append).list_work()

    # The result is EXACTLY the valid items — an empty/aborted listing fails.
    assert len(items) == 2
    ids = {i.source_ref for i in items}
    assert ids == {"octo/widgets#7#RT_a", "octo/widgets#7#RT_b"}
    # The bad node never made it through.
    assert all("RT_bad" not in i.source_ref for i in items)


def test_the_skip_is_recorded_naming_the_bad_node() -> None:
    gh = _FakeGh(_payload([_good_thread("RT_a"), _bad_thread("RT_bad")]))
    lines: list[str] = []
    _source(gh, log=lines.append).list_work()

    # A log line names the offending thread node, and the count is recorded.
    skip_lines = [ln for ln in lines if "skipping malformed review thread" in ln]
    assert skip_lines, "the bad node must be logged, not silently swallowed"
    assert any("RT_bad" in ln for ln in skip_lines)
    assert any(
        "skipped 1 malformed review thread node" in ln for ln in lines
    )


def test_skip_does_not_crash_when_no_log_is_configured() -> None:
    gh = _FakeGh(_payload([_good_thread("RT_a"), _bad_thread("RT_bad")]))
    items = _source(gh).list_work()  # no log= -> counts only, must not raise

    assert [i.source_ref for i in items] == ["octo/widgets#7#RT_a"]


def test_all_nodes_bad_yields_empty_but_does_not_raise() -> None:
    # Every node is identifiable-but-uncompilable: each is skipped one at a
    # time. The empty result here is honest (no node compiled), NOT an abort.
    gh = _FakeGh(_payload([_bad_thread("RT_x"), _bad_thread("RT_y")]))
    lines: list[str] = []
    items = _source(gh, log=lines.append).list_work()

    assert items == []
    assert any("skipped 2 malformed review thread node" in ln for ln in lines)


def test_missing_node_id_is_an_identity_break_and_still_aborts() -> None:
    # A thread with no node id cannot be named or keyed: that is the top-level
    # envelope/identity case, which must abort rather than skip — one bad
    # *identifiable* node is skipped, an unnameable one is not.
    no_id = _good_thread("RT_a")
    del no_id["id"]
    gh = _FakeGh(_payload([no_id, _good_thread("RT_b")]))
    with pytest.raises(WorkSourceError, match="missing string 'id'"):
        _source(gh).list_work()
