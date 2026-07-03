"""Tests for the GitHub Issues work source.

The ``gh`` subprocess is the injectable seam (same pattern as the scripted
``invoke``): tests hand the source a runner returning canned stdout and
assert on the argv it was called with. Compilation, the readiness gate
(graders or skip), and outbound reporting are covered; the real ``gh``
binary is never invoked.
"""

from __future__ import annotations

import json

import pytest

from flywheel_core import Status
from flywheel_core.task import CommandGrader
from flywheel_orchestrator import (
    GithubWorkSource,
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


def _issue(
    number: int,
    *,
    title: str = "Fix the widget",
    body: str = "",
    url: str = "https://github.com/octo/widgets/issues/1",
) -> dict:
    return {"number": number, "title": title, "body": body, "url": url}


def _source(gh: _FakeGh, **kwargs) -> GithubWorkSource:
    defaults = {
        "repo": "octo/widgets",
        "label": "flywheel",
        "runner": gh,
    }
    defaults.update(kwargs)
    return GithubWorkSource(**defaults)


SPEC_BODY = """Some prose describing the bug.

```flywheel
{
  "goal": "Widget retries flaky fetches.",
  "graders": [{"type": "command", "run": "uv run pytest tests/widget"}],
  "context": {"relevant": ["src/widget.py"]},
  "tags": ["reliability"],
  "prerequisites": ["gh-7"]
}
```

More prose after the block.
"""


# --- inbound: list_work -----------------------------------------------------


def test_list_work_queries_open_labeled_issues() -> None:
    gh = _FakeGh(json.dumps([]))
    _source(gh).list_work()

    (argv,) = gh.calls
    assert argv[:2] == ["issue", "list"]
    assert ["--repo", "octo/widgets"] == argv[argv.index("--repo") :][:2]
    assert ["--label", "flywheel"] == argv[argv.index("--label") :][:2]
    assert ["--state", "open"] == argv[argv.index("--state") :][:2]


def test_spec_block_compiles_fully() -> None:
    gh = _FakeGh(json.dumps([_issue(12, body=SPEC_BODY)]))
    (item,) = _source(gh).list_work()

    assert item.task.id == "gh-12"
    assert item.task.goal == "Widget retries flaky fetches."
    assert item.source_ref == "octo/widgets#12"
    assert item.local_path is None
    assert item.prerequisites == ("gh-7",)
    assert item.task.tags == ["reliability"]
    assert isinstance(item.task.graders[0], CommandGrader)
    assert item.task.graders[0].run == "uv run pytest tests/widget"
    assert item.task.context.relevant == ["src/widget.py"]
    # Body text and issue URL are carried into the task context.
    assert "Some prose describing the bug." in item.task.context.notes
    assert (
        "https://github.com/octo/widgets/issues/1"
        in item.task.context.references
    )


def test_issue_without_spec_uses_title_and_default_graders() -> None:
    gh = _FakeGh(json.dumps([_issue(3, title="Add caching", body="Please.")]))
    source = _source(gh, default_graders=(CommandGrader(run="true"),))

    (item,) = source.list_work()

    assert item.task.id == "gh-3"
    assert item.task.goal == "Add caching"
    assert [g.run for g in item.task.graders] == ["true"]  # type: ignore[union-attr]
    assert item.task.context.notes == "Please."


def test_issue_without_any_graders_is_skipped_and_logged() -> None:
    gh = _FakeGh(json.dumps([_issue(4, title="Vague wish", body="")]))
    lines: list[str] = []
    source = _source(gh, log=lines.append)

    assert source.list_work() == []
    assert len(lines) == 1
    assert "octo/widgets#4" in lines[0]
    assert "not runnable" in lines[0]


def test_items_sort_by_issue_number() -> None:
    gh = _FakeGh(json.dumps([_issue(20), _issue(5)]))
    source = _source(gh, default_graders=(CommandGrader(run="true"),))

    items = source.list_work()

    assert [i.task.id for i in items] == ["gh-5", "gh-20"]


def test_invalid_spec_json_raises_instead_of_silent_fallback() -> None:
    body = "```flywheel\n{not json}\n```"
    gh = _FakeGh(json.dumps([_issue(9, body=body)]))

    with pytest.raises(WorkSourceError, match="octo/widgets#9.*invalid JSON"):
        _source(gh).list_work()


def test_deeply_nested_spec_block_raises_worksourceerror_not_recursion() -> None:
    # The spec block is the entire body of an attacker-authored fenced block.
    # A deeply-nested payload makes json.loads raise RecursionError (a
    # RuntimeError, not JSONDecodeError), which would otherwise escape past
    # list_work's per-item WorkSourceError guard and blackout the whole board.
    # It must surface as a WorkSourceError like any other invalid block.
    depth = 20000
    body = "```flywheel\n" + "[" * depth + "]" * depth + "\n```"
    gh = _FakeGh(json.dumps([_issue(9, body=body)]))

    with pytest.raises(WorkSourceError, match="octo/widgets#9.*too deeply"):
        _source(gh).list_work()


def test_unclosed_spec_fence_raises() -> None:
    body = "```flywheel\n{}"
    gh = _FakeGh(json.dumps([_issue(9, body=body)]))

    with pytest.raises(WorkSourceError, match="not closed"):
        _source(gh).list_work()


def test_invalid_list_payload_raises() -> None:
    gh = _FakeGh("not json at all")
    with pytest.raises(WorkSourceError, match="invalid JSON"):
        _source(gh).list_work()


def _full_page() -> str:
    return json.dumps([_issue(n, body="Please.") for n in range(1, 201)])


def test_full_page_emits_one_truncation_warning() -> None:
    gh = _FakeGh(_full_page())
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    items = source.list_work()

    assert len(items) == 200
    warnings = [ln for ln in lines if "truncated at one page" in ln]
    assert len(warnings) == 1
    assert "[github]" in warnings[0]
    assert "some items were not read this pass" in warnings[0]


def test_below_cap_emits_no_truncation_warning() -> None:
    gh = _FakeGh(json.dumps([_issue(n, body="Please.") for n in range(1, 200)]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    source.list_work()

    assert [ln for ln in lines if "truncated" in ln] == []


def test_truncation_warning_is_a_side_channel() -> None:
    page = _full_page()
    with_log_source = _source(
        _FakeGh(page),
        default_graders=(CommandGrader(run="true"),),
        log=[].append,
    )
    without_log_source = _source(
        _FakeGh(page), default_graders=(CommandGrader(run="true"),)
    )

    with_log = with_log_source.list_work()
    without_log = without_log_source.list_work()

    assert [i.task.id for i in with_log] == [i.task.id for i in without_log]


def test_full_page_with_log_none_does_not_crash() -> None:
    gh = _FakeGh(_full_page())
    source = _source(gh, default_graders=(CommandGrader(run="true"),))

    items = source.list_work()

    assert len(items) == 200


def test_full_page_records_one_source_truncation_on_sink() -> None:
    gh = _FakeGh(_full_page())
    events: list[tuple[str, str, str]] = []
    source = _source(
        gh,
        default_graders=(CommandGrader(run="true"),),
        stop_sink=lambda k, s, d: events.append((k, s, d)),
    )

    items = source.list_work()

    assert len(items) == 200  # sequence unchanged by the sink
    truncations = [e for e in events if e[0] == "source-truncation"]
    assert len(truncations) == 1
    _, subject, detail = truncations[0]
    assert subject == source.source_name
    assert "truncated at one page" in detail
    assert "some items were not read this pass" in detail


def test_below_cap_records_no_source_truncation_on_sink() -> None:
    gh = _FakeGh(json.dumps([_issue(n, body="Please.") for n in range(1, 200)]))
    events: list[tuple[str, str, str]] = []
    source = _source(
        gh,
        default_graders=(CommandGrader(run="true"),),
        stop_sink=lambda k, s, d: events.append((k, s, d)),
    )

    source.list_work()

    assert [e for e in events if e[0] == "source-truncation"] == []


def test_zero_grader_issue_records_zero_grader_drop_on_sink() -> None:
    gh = _FakeGh(json.dumps([_issue(4, title="Vague wish", body="")]))
    events: list[tuple[str, str, str]] = []
    source = _source(gh, stop_sink=lambda k, s, d: events.append((k, s, d)))

    assert source.list_work() == []  # still dropped from the sequence
    drops = [e for e in events if e[0] == "zero-grader-drop"]
    assert len(drops) == 1
    _, subject, detail = drops[0]
    assert subject == source.source_name
    assert "octo/widgets#4" in detail
    assert "not runnable" in detail


def test_stop_sink_is_a_side_channel() -> None:
    page = _full_page()
    with_sink = _source(
        _FakeGh(page),
        default_graders=(CommandGrader(run="true"),),
        stop_sink=lambda *_: None,
    ).list_work()
    without_sink = _source(
        _FakeGh(page), default_graders=(CommandGrader(run="true"),)
    ).list_work()

    assert [i.task.id for i in with_sink] == [i.task.id for i in without_sink]


def test_failed_listing_raises_and_never_returns_empty() -> None:
    def _failing(argv) -> str:
        raise WorkSourceError("gh issue list failed (exit 1): boom")

    source = GithubWorkSource(
        repo="octo/widgets", label="flywheel", runner=_failing
    )
    with pytest.raises(WorkSourceError, match="failed"):
        source.list_work()


# --- outbound: report -------------------------------------------------------


def _report(status: Status, *, graders: tuple = ()) -> WorkReport:
    return WorkReport(
        task_id="gh-12",
        source_ref="octo/widgets#12",
        run_id="run-abc",
        status=status,
        error="",
        graders=graders,
    )


def test_report_comments_with_receipts() -> None:
    gh = _FakeGh("")
    receipts = (
        GraderReceipt(ordinal=0, grader_type="command", name="tests", passed=True),
        GraderReceipt(ordinal=1, grader_type="command", name=None, passed=False),
    )

    _source(gh).report(_report(Status.FAILED_VALIDATION, graders=receipts))

    (argv,) = gh.calls
    assert argv[:3] == ["issue", "comment", "12"]
    body = argv[argv.index("--body") + 1]
    assert "run `run-abc`" in body
    assert "`failed_validation`" in body
    assert "| 0 | tests | command | yes |" in body
    assert "| 1 | - | command | no |" in body


def test_report_closes_on_done_when_policy_says_close() -> None:
    gh = _FakeGh("")
    source = _source(gh, done_action="close")

    source.report(_report(Status.DONE))

    (argv,) = gh.calls
    assert argv[:3] == ["issue", "close", "12"]
    assert "--comment" in argv


def test_report_comments_on_done_by_default() -> None:
    gh = _FakeGh("")
    _source(gh).report(_report(Status.DONE))

    (argv,) = gh.calls
    assert argv[:3] == ["issue", "comment", "12"]


def test_report_with_malformed_source_ref_raises() -> None:
    gh = _FakeGh("")
    report = WorkReport(
        task_id="x",
        source_ref="no-number-here",
        run_id="r",
        status=Status.DONE,
        error="",
        graders=(),
    )
    with pytest.raises(WorkSourceError, match="malformed github source_ref"):
        _source(gh).report(report)


def test_done_action_validated_at_construction() -> None:
    with pytest.raises(ValueError, match="done_action"):
        GithubWorkSource(repo="a/b", label="x", done_action="merge")
