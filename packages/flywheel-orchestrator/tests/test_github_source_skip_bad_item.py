"""The github-issues source skips one uncompilable issue, not the board.

A single malformed payload among otherwise valid issues must be counted +
logged (so it is recoverable and never reads as "no work") while every
other valid item still returns. This is distinct from the loud abort that
fires when a listing has *no* compilable item at all (covered in
``test_github_source.py``), and from the existing skip-and-log for issues
that are merely not-runnable (no graders).
"""

from __future__ import annotations

import json

import pytest

from flywheel_core.task import CommandGrader
from flywheel_orchestrator import GithubWorkSource, WorkSourceError


class _FakeGh:
    """Scripted gh runner: returns the canned stdout for ``issue list``."""

    def __init__(self, stdout: str) -> None:
        self.stdout = stdout

    def __call__(self, argv) -> str:
        return self.stdout


def _issue(number, *, title: str = "Fix it", body: str = "", url: str = "") -> dict:
    return {"number": number, "title": title, "body": body, "url": url}


def _source(gh: _FakeGh, **kwargs) -> GithubWorkSource:
    defaults = {
        "repo": "octo/widgets",
        "label": "flywheel",
        "runner": gh,
        "default_graders": (CommandGrader(run="true"),),
    }
    defaults.update(kwargs)
    return GithubWorkSource(**defaults)


_BAD_JSON_BODY = "```flywheel\n{not valid json}\n```"


def test_missing_number_issue_is_skipped_rest_return() -> None:
    # Three valid issues and one payload with no integer 'number'.
    payload = json.dumps(
        [
            _issue(5, body="Please."),
            {"title": "no number here", "body": "Please.", "url": ""},
            _issue(20, body="Please."),
        ]
    )
    lines: list[str] = []
    source = _source(_FakeGh(payload), log=lines.append)

    items = source.list_work()

    # Exactly the valid items survive, in issue-number order.
    assert [i.task.id for i in items] == ["gh-5", "gh-20"]
    # The skip is recorded, not silently swallowed.
    skip_lines = [ln for ln in lines if "skipping malformed issue" in ln]
    assert len(skip_lines) == 1
    # The offending payload is identifiable in the log line.
    assert "number" in skip_lines[0]


def test_invalid_spec_block_issue_is_skipped_rest_return() -> None:
    # Two valid issues straddling one issue with an invalid spec block.
    payload = json.dumps(
        [
            _issue(5, body="Please."),
            _issue(9, body=_BAD_JSON_BODY),
            _issue(20, body="Please."),
        ]
    )
    lines: list[str] = []
    source = _source(_FakeGh(payload), log=lines.append)

    items = source.list_work()

    assert [i.task.id for i in items] == ["gh-5", "gh-20"]
    skip_lines = [ln for ln in lines if "skipping malformed issue" in ln]
    assert len(skip_lines) == 1
    # Recoverable: the offending source_ref appears in the skip line.
    assert "octo/widgets#9" in skip_lines[0]


def test_one_bad_item_never_reads_as_empty() -> None:
    # The board has work; a single bad ticket must not collapse it to [].
    payload = json.dumps(
        [
            _issue(5, body="Please."),
            _issue(9, body=_BAD_JSON_BODY),
        ]
    )
    items = _source(_FakeGh(payload)).list_work()

    assert [i.task.id for i in items] == ["gh-5"]
    assert items != []


def test_skip_works_without_a_log_sink() -> None:
    # log=None is a valid configuration; skipping must not require a logger.
    payload = json.dumps(
        [
            _issue(5, body="Please."),
            _issue(9, body=_BAD_JSON_BODY),
            _issue(20, body="Please."),
        ]
    )
    source = _source(_FakeGh(payload), log=None)

    items = source.list_work()

    assert [i.task.id for i in items] == ["gh-5", "gh-20"]


def test_all_bad_listing_still_aborts_loudly() -> None:
    # No compilable item + a malformed payload is a hard error, not "no work".
    payload = json.dumps([_issue(9, body=_BAD_JSON_BODY)])

    with pytest.raises(WorkSourceError, match="octo/widgets#9.*invalid JSON"):
        _source(_FakeGh(payload)).list_work()
