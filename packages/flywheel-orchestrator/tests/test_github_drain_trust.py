"""The GitHub issues source's opt-in triage-trust rule (drain side).

With ``require_triage_receipt`` enabled, the source schedules a ready-labeled
issue only when its ``flywheel`` spec block carries a triage receipt whose
``content_hash`` matches the issue's current human content and whose
``exit_code`` is fail-first. Every other ready issue is skipped loudly through
the log seam and produces no ``WorkItem`` -- and the ``[[defaults.graders]]``
fallback deliberately does NOT rescue it. With the rule off (the default), the
source is byte-identical to today.

The ``gh`` subprocess is the injectable seam (the same scripted-runner pattern
as ``test_github_source.py``): a canned stdout in, the recorded argv out; the
real ``gh`` binary is never invoked. The end of the file also pins the
DONE-under-``done_action=close`` write-back end-to-end.
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
from flywheel_orchestrator._triage_receipt import RECEIPT_KEY, content_hash


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


def _source(gh: _FakeGh, *, require: bool = True, **kwargs) -> GithubWorkSource:
    defaults = {
        "repo": "octo/widgets",
        "label": "flywheel",
        "runner": gh,
        "require_triage_receipt": require,
    }
    defaults.update(kwargs)
    return GithubWorkSource(**defaults)


def _block(spec: dict) -> str:
    return "```flywheel\n" + json.dumps(spec) + "\n```"


def _receipt(
    title: str,
    prose: str,
    *,
    exit_code: int = 1,
    base_sha: str = "abc123",
) -> dict:
    """A fail-first receipt whose content_hash matches ``title``/``prose``."""
    return {
        "command": "uv run pytest tests/widget",
        "exit_code": exit_code,
        "base_sha": base_sha,
        "content_hash": content_hash(title, prose),
    }


def _triaged_body(
    title: str,
    prose: str,
    *,
    receipt: dict | None = "auto",  # type: ignore[assignment]
    graders: list | None = None,
) -> str:
    """An issue body: human prose followed by a ```flywheel block.

    ``receipt="auto"`` embeds a valid fail-first receipt matching the prose;
    pass an explicit dict to override it or ``None`` to embed no receipt.
    """
    spec: dict = {}
    if receipt == "auto":
        spec[RECEIPT_KEY] = _receipt(title, prose)
    elif receipt is not None:
        spec[RECEIPT_KEY] = receipt
    if graders is not None:
        spec["graders"] = graders
    return prose + "\n\n" + _block(spec)


# --- trust rule OFF: byte-identical to today --------------------------------


def test_trust_off_issue_without_block_falls_back_to_defaults() -> None:
    gh = _FakeGh(json.dumps([_issue(3, title="Add caching", body="Please.")]))
    source = _source(
        gh, require=False, default_graders=(CommandGrader(run="true"),)
    )

    (item,) = source.list_work()

    assert item.task.id == "gh-3"
    assert item.task.goal == "Add caching"
    assert [g.run for g in item.task.graders] == ["true"]  # type: ignore[union-attr]


def test_trust_off_ignores_receipt_entirely() -> None:
    # No receipt in the block, trust off: still schedules on the block graders.
    body = _triaged_body(
        "Fix the widget",
        "Widget drops retries.",
        receipt=None,
        graders=[{"type": "command", "run": "uv run pytest tests/widget"}],
    )
    gh = _FakeGh(json.dumps([_issue(6, body=body)]))
    source = _source(gh, require=False)

    (item,) = source.list_work()

    assert item.task.id == "gh-6"
    assert item.task.graders[0].run == "uv run pytest tests/widget"  # type: ignore[union-attr]


# --- trust rule ON: the happy path ------------------------------------------


def test_valid_block_and_matching_receipt_yields_workitem() -> None:
    title = "Fix the widget"
    prose = "Widget drops retries under load."
    body = _triaged_body(
        title,
        prose,
        graders=[{"type": "command", "run": "uv run pytest tests/widget"}],
    )
    gh = _FakeGh(json.dumps([_issue(12, title=title, body=body)]))
    lines: list[str] = []
    source = _source(gh, log=lines.append)

    (item,) = source.list_work()

    assert item.task.id == "gh-12"
    assert item.task.graders[0].run == "uv run pytest tests/widget"  # type: ignore[union-attr]
    assert item.source_ref == "octo/widgets#12"
    assert lines == []  # a scheduled issue is not a skip


def test_valid_receipt_without_block_graders_uses_defaults() -> None:
    # Once the trust gate passes, normal grader resolution applies: an issue
    # whose block carries only a receipt falls back to [[defaults.graders]].
    title = "Fix the widget"
    body = _triaged_body(title, "Widget drops retries.")
    gh = _FakeGh(json.dumps([_issue(8, title=title, body=body)]))
    source = _source(gh, default_graders=(CommandGrader(run="true"),))

    (item,) = source.list_work()

    assert item.task.id == "gh-8"
    assert [g.run for g in item.task.graders] == ["true"]  # type: ignore[union-attr]


# --- trust rule ON: the loud-skip failure modes -----------------------------


def test_missing_block_skipped_even_with_defaults() -> None:
    gh = _FakeGh(json.dumps([_issue(4, title="Vague wish", body="Please.")]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    # The fallback must not fire in trust mode.
    assert source.list_work() == []
    assert len(lines) == 1
    assert "octo/widgets#4" in lines[0]
    assert "trust mode" in lines[0]


def test_missing_receipt_skipped_even_with_defaults() -> None:
    body = _triaged_body(
        "Fix the widget",
        "Widget drops retries.",
        receipt=None,
        graders=[{"type": "command", "run": "uv run pytest tests/widget"}],
    )
    gh = _FakeGh(json.dumps([_issue(5, body=body)]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    assert source.list_work() == []
    assert len(lines) == 1
    assert "octo/widgets#5" in lines[0]
    assert "receipt" in lines[0]


def test_stale_receipt_after_body_edit_skipped() -> None:
    title = "Fix the widget"
    original = "Widget drops retries."
    # A receipt minted against the ORIGINAL prose, embedded under EDITED prose.
    receipt = _receipt(title, original)
    edited = original + "\n\nEDIT: also handle request timeouts."
    body = edited + "\n\n" + _block({RECEIPT_KEY: receipt})
    gh = _FakeGh(json.dumps([_issue(7, title=title, body=body)]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    assert source.list_work() == []
    assert len(lines) == 1
    assert "stale" in lines[0]


def test_title_edit_also_staleness_skips() -> None:
    # The hash covers the title too: a title edit after triage goes stale.
    prose = "Widget drops retries."
    receipt = _receipt("Fix the widget", prose)
    body = prose + "\n\n" + _block({RECEIPT_KEY: receipt})
    gh = _FakeGh(json.dumps([_issue(7, title="Fix the widget NOW", body=body)]))
    source = _source(gh, default_graders=(CommandGrader(run="true"),))

    assert source.list_work() == []


def test_passing_receipt_is_not_fail_first_and_is_skipped() -> None:
    title = "Fix the widget"
    prose = "Widget drops retries."
    # Fields parse and the hash matches, but exit_code 0 is not fail-first.
    receipt = _receipt(title, prose, exit_code=0)
    body = prose + "\n\n" + _block({RECEIPT_KEY: receipt})
    gh = _FakeGh(json.dumps([_issue(9, title=title, body=body)]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    assert source.list_work() == []
    assert len(lines) == 1
    assert "fail-first" in lines[0]


def test_malformed_receipt_is_skipped() -> None:
    title = "Fix the widget"
    prose = "Widget drops retries."
    # content_hash present but exit_code wrong-typed -> parse fails.
    receipt = {
        "command": "uv run pytest",
        "exit_code": "nonzero",
        "base_sha": "abc123",
        "content_hash": content_hash(title, prose),
    }
    body = prose + "\n\n" + _block({RECEIPT_KEY: receipt})
    gh = _FakeGh(json.dumps([_issue(10, title=title, body=body)]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    assert source.list_work() == []
    assert len(lines) == 1
    assert "malformed" in lines[0]


def test_trust_skip_does_not_require_a_log_sink() -> None:
    # log=None is valid: skipping must never depend on a logger being wired.
    gh = _FakeGh(json.dumps([_issue(4, title="Vague wish", body="Please.")]))
    source = _source(gh, default_graders=(CommandGrader(run="true"),), log=None)

    assert source.list_work() == []


def test_present_but_invalid_block_still_raises_in_trust_mode() -> None:
    # Trust mode does not change present-but-invalid spec-block semantics: an
    # unparseable block is a hard error, not a quiet trust skip.
    gh = _FakeGh(json.dumps([_issue(9, body="```flywheel\n{not json}\n```")]))

    with pytest.raises(WorkSourceError, match="octo/widgets#9.*invalid JSON"):
        _source(gh).list_work()


def test_trust_mode_schedules_only_the_receipted_issue() -> None:
    # A mixed board: one properly triaged issue and one bare ready issue.
    title = "Fix the widget"
    prose = "Widget drops retries."
    good = _issue(
        2,
        title=title,
        body=_triaged_body(
            title,
            prose,
            graders=[{"type": "command", "run": "uv run pytest"}],
        ),
    )
    bare = _issue(11, title="Vague wish", body="Please.")
    gh = _FakeGh(json.dumps([good, bare]))
    lines: list[str] = []
    source = _source(
        gh, default_graders=(CommandGrader(run="true"),), log=lines.append
    )

    items = source.list_work()

    assert [i.task.id for i in items] == ["gh-2"]
    assert len(lines) == 1
    assert "octo/widgets#11" in lines[0]


# --- outbound: DONE under done_action=close, end-to-end ----------------------


def test_done_under_close_closes_issue_with_grader_receipt_body() -> None:
    gh = _FakeGh("")
    source = _source(gh, done_action="close")
    receipts = (
        GraderReceipt(
            ordinal=0, grader_type="command", name="tests", passed=True
        ),
        GraderReceipt(
            ordinal=1, grader_type="command", name=None, passed=False
        ),
    )
    report = WorkReport(
        task_id="gh-12",
        source_ref="octo/widgets#12",
        run_id="run-abc",
        status=Status.DONE,
        error="",
        graders=receipts,
    )

    source.report(report)

    (argv,) = gh.calls
    assert argv[:3] == ["issue", "close", "12"]
    assert "--comment" in argv
    body = argv[argv.index("--comment") + 1]
    assert "run `run-abc`" in body
    assert "`done`" in body
    assert "| 0 | tests | command | yes |" in body
    assert "| 1 | - | command | no |" in body
