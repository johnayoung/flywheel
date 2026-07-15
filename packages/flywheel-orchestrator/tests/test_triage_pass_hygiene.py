"""Triage-pass hygiene: write only where issue state demands it.

Grades the pass-loop's selection and write discipline (spec 00082 criteria 7,
9, 12 and Decision D-5): a *ready* issue whose content hash drifted from its
receipt is re-triaged within the pass and never left carrying its stale
receipt; an immediate second pass over unchanged state records zero GitHub
writes; and each pass processes at most a configured cap of candidates, in
issue-number order, surfacing the deferred remainder.

Every external effect is an injectable seam, so the pass runs hermetically: a
label-aware scripted ``gh`` runner (returning a distinct issue list per label
query and capturing the write-set), a scripted authoring invoker, and a
scripted grader executor -- no network, no SDK, no real subprocess.
"""

from __future__ import annotations

import asyncio
import json

import pytest

from flywheel_orchestrator._triage import (
    DECISION_NEEDS_DETAIL,
    DECISION_READY,
    TriagePass,
)
from flywheel_orchestrator._triage_receipt import RECEIPT_KEY, content_hash

REPO = "octo/widgets"
BASE_SHA = "basesha0000000000000000000000000000000000"


# --- seams ------------------------------------------------------------------


class _LabelGh:
    """Scripted gh runner returning a distinct issue list per label query.

    ``by_label`` maps a label to the issues an ``issue list --label <L>``
    returns; an unlisted label returns an empty board. Every call is recorded in
    ``calls`` so the write-set (edits + comments) can be asserted -- the
    idempotency proof is that a settled pass records none.
    """

    def __init__(self, by_label: dict[str, list[dict]]) -> None:
        self._by_label = {k: json.dumps(v) for k, v in by_label.items()}
        self.calls: list[list[str]] = []

    def __call__(self, argv) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["issue", "list"]:
            label = argv[argv.index("--label") + 1]
            return self._by_label.get(label, "[]")
        return ""

    def edits(self) -> list[list[str]]:
        return [a for a in self.calls if a[:2] == ["issue", "edit"]]

    def comments(self) -> list[list[str]]:
        return [a for a in self.calls if a[:2] == ["issue", "comment"]]

    def writes(self) -> list[list[str]]:
        return [
            a
            for a in self.calls
            if a[:2] in (["issue", "edit"], ["issue", "comment"])
        ]


class _FakeExecutor:
    """Scripted grader executor: records commands, returns a fixed exit code."""

    def __init__(self, exit_code: int = 1) -> None:
        self.exit_code = exit_code
        self.commands: list[str] = []

    def __call__(self, command: str) -> int:
        self.commands.append(command)
        return self.exit_code


def _scripted_invoker(response: str):
    async def _invoke(prompt: str) -> str:
        return response

    return _invoke


def _routing_invoker(mapping: dict[str, str]):
    async def _invoke(prompt: str) -> str:
        for needle, response in mapping.items():
            if needle in prompt:
                return response
        raise AssertionError(f"no scripted response for prompt: {prompt[:80]}")

    return _invoke


def _boom_invoker():
    async def _invoke(prompt: str) -> str:
        raise AssertionError("authoring agent must not run for settled state")

    return _invoke


def _boom_executor():
    def _execute(command: str) -> int:
        raise AssertionError("executor must not run for settled state")

    return _execute


# --- fixtures ---------------------------------------------------------------


def _fenced(obj: dict) -> str:
    return "```json\n" + json.dumps(obj) + "\n```"


def _issue(
    number: int,
    *,
    title: str = "Fix the widget",
    body: str = "Widget drops retries under load.",
) -> dict:
    return {
        "number": number,
        "title": title,
        "body": body,
        "url": f"https://github.com/octo/widgets/issues/{number}",
    }


def _ready_response(command: str = "uv run pytest tests/widget") -> str:
    return _fenced(
        {
            "authoritative_grader": command,
            "graders": [{"type": "command", "run": command}],
            "goal": "Widget retries dropped requests under load.",
        }
    )


def _stale_receipt(command: str = "old-cmd") -> dict:
    """A well-formed but stale receipt: fail-first, yet a non-matching hash."""
    return {
        "command": command,
        "exit_code": 1,
        "base_sha": "OLDBASE",
        "content_hash": "deadbeefdeadbeef",  # does not match current content
    }


def _body_with_receipt(prose: str, receipt: dict) -> str:
    """Human prose followed by a ``flywheel`` block carrying ``receipt``."""
    spec = {
        "graders": [{"type": "command", "run": receipt["command"]}],
        RECEIPT_KEY: receipt,
    }
    block = "```flywheel\n" + json.dumps(spec, indent=2) + "\n```"
    return f"{prose}\n\n{block}"


def _pass(gh, executor, invoker, **kwargs) -> TriagePass:
    defaults = {
        "repo": REPO,
        "intake_label": "intake",
        "ready_label": "ready",
        "needs_detail_label": "needs-detail",
        "base_sha": BASE_SHA,
        "runner": gh,
        "executor": executor,
        "invoker": invoker,
    }
    defaults.update(kwargs)
    return TriagePass(**defaults)


def _body_of(argv: list[str]) -> str:
    return argv[argv.index("--body") + 1]


def _label_after(argv: list[str], flag: str) -> str:
    return argv[argv.index(flag) + 1]


def _extract_block(body: str) -> dict:
    lines = body.splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.strip() == "```flywheel")
    end = next(
        i for i in range(start + 1, len(lines)) if lines[i].strip() == "```"
    )
    return json.loads("\n".join(lines[start + 1 : end]))


# --- drift: a drifted ready issue is re-triaged within the pass -------------


def test_drifted_ready_issue_is_retriaged_and_reminted_ready() -> None:
    title, prose = "Fix the widget", "Widget drops retries under load."
    drifted = _issue(7, title=title, body=_body_with_receipt(prose, _stale_receipt()))
    gh = _LabelGh({"ready": [drifted]})  # no intake issues on the board
    ex = _FakeExecutor(exit_code=1)
    result = asyncio.run(
        _pass(gh, ex, _scripted_invoker(_ready_response("new-cmd"))).run()
    )

    (ready,) = result.ready
    assert ready.number == 7
    assert ready.decision == DECISION_READY
    assert ready.receipt is not None
    assert ready.receipt.command == "new-cmd"
    assert ready.receipt.base_sha == BASE_SHA  # fresh base, not OLDBASE
    assert ready.receipt.content_hash == content_hash(title, prose)

    # The re-triage executed the fresh candidate against the base.
    assert ex.commands == ["new-cmd"]

    # Exactly one edit refreshing the block in place; no comment.
    (edit,) = gh.edits()
    assert gh.comments() == []
    body = _body_of(edit)
    assert body.count("```flywheel") == 1  # stale block replaced, not doubled
    assert "deadbeefdeadbeef" not in body  # the stale receipt is gone
    assert "OLDBASE" not in body
    block = _extract_block(body)
    assert block[RECEIPT_KEY]["content_hash"] == content_hash(title, prose)
    assert block[RECEIPT_KEY]["base_sha"] == BASE_SHA
    # It stayed ready; the intake label was never touched.
    assert _label_after(edit, "--add-label") == "ready"
    assert "--remove-label" not in edit


def test_drift_retriage_to_needs_detail_strips_the_stale_receipt() -> None:
    title, prose = "Fix the widget", "Widget drops retries."
    drifted = _issue(8, title=title, body=_body_with_receipt(prose, _stale_receipt()))
    gh = _LabelGh({"ready": [drifted]})
    ex = _FakeExecutor(exit_code=0)  # candidate grader passes -> vacuous
    result = asyncio.run(
        _pass(gh, ex, _scripted_invoker(_ready_response("true"))).run()
    )

    assert result.ready == ()
    (nd,) = result.needs_detail
    assert nd.number == 8
    assert nd.decision == DECISION_NEEDS_DETAIL

    # The stale block/receipt is stripped from the body on demotion.
    (edit,) = gh.edits()
    body = _body_of(edit)
    assert "```flywheel" not in body
    assert "deadbeefdeadbeef" not in body
    assert prose in body  # human text preserved
    assert _label_after(edit, "--add-label") == "needs-detail"
    assert _label_after(edit, "--remove-label") == "ready"

    # Exactly one comment, naming the vacuity.
    (comment,) = gh.comments()
    text = _label_after(comment, "--body")
    assert "vacuous" in text.lower()


def test_drift_retriage_cannot_compile_strips_block_and_names_gaps() -> None:
    title, prose = "Make it better", "please improve retries"
    drifted = _issue(9, title=title, body=_body_with_receipt(prose, _stale_receipt()))
    gh = _LabelGh({"ready": [drifted]})
    ex = _FakeExecutor(exit_code=1)
    gaps = ["which module owns the retry logic -- src/widget.py or src/net.py"]
    response = _fenced({"authoritative_grader": "", "missing_information": gaps})
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(response)).run())

    assert result.ready == ()
    (nd,) = result.needs_detail
    assert nd.missing_information == tuple(gaps)
    # No candidate grader -> the executor was never consulted.
    assert ex.commands == []

    (edit,) = gh.edits()
    body = _body_of(edit)
    assert "```flywheel" not in body  # stale block stripped
    assert "deadbeefdeadbeef" not in body
    (comment,) = gh.comments()
    assert gaps[0] in _label_after(comment, "--body")


# --- idempotency: an unchanged second pass writes nothing -------------------


def test_second_pass_over_unchanged_state_records_zero_writes() -> None:
    title, prose = "Fix the widget", "Widget drops retries under load."

    # Pass 1: an intake issue is triaged to ready; capture the reminted body.
    gh1 = _LabelGh({"intake": [_issue(12, title=title, body=prose)]})
    asyncio.run(
        _pass(
            gh1, _FakeExecutor(exit_code=1), _scripted_invoker(_ready_response())
        ).run()
    )
    (edit1,) = gh1.edits()
    ready_body = _body_of(edit1)

    # Pass 2: that issue now carries the ready label and a matching receipt; a
    # needs-detail issue sits under a label the pass never queries. The boom
    # seams prove the pass re-triages nothing -- it never drives an agent.
    gh2 = _LabelGh(
        {
            "intake": [],
            "ready": [_issue(12, title=title, body=ready_body)],
            "needs-detail": [_issue(3, title="Vague", body="please improve")],
        }
    )
    result = asyncio.run(_pass(gh2, _boom_executor(), _boom_invoker()).run())

    assert result.outcomes == ()
    assert result.deferred == 0
    assert gh2.writes() == []  # zero GitHub writes over unchanged state


def test_needs_detail_issue_is_not_recommented_on_later_passes() -> None:
    # A needs-detail issue carries neither the intake nor the ready label, so a
    # later pass never queries it and cannot re-comment on it.
    gh = _LabelGh(
        {
            "intake": [],
            "ready": [],
            "needs-detail": [
                _issue(4, title="Vague", body="please improve retries")
            ],
        }
    )
    result = asyncio.run(_pass(gh, _boom_executor(), _boom_invoker()).run())
    assert result.outcomes == ()
    assert gh.writes() == []


# --- the per-pass cap -------------------------------------------------------


def test_per_pass_cap_processes_cap_issues_and_defers_the_remainder() -> None:
    issues = [
        _issue(n, title=f"Fix {n}", body=f"widget {n} drops retries")
        for n in (1, 2, 3, 4)
    ]
    gh = _LabelGh({"intake": issues})
    logs: list[str] = []
    result = asyncio.run(
        _pass(
            gh,
            _FakeExecutor(exit_code=1),
            _scripted_invoker(_ready_response()),
            per_pass_cap=2,
            log=logs.append,
        ).run()
    )

    # Exactly the first two issues (by number) were processed.
    assert [o.number for o in result.outcomes] == [1, 2]
    edited = [e[2] for e in gh.edits()]
    assert edited == ["1", "2"]
    # The remainder is untouched this pass -- no edit or comment references it.
    assert "3" not in edited and "4" not in edited
    assert gh.comments() == []
    # The deferred count is surfaced on the result and through the log seam.
    assert result.deferred == 2
    assert any("deferring 2" in line for line in logs)


def test_drift_on_a_ready_issue_counts_against_the_per_pass_cap() -> None:
    intake = [
        _issue(1, title="Alpha", body="alpha drops work"),
        _issue(3, title="Gamma", body="gamma drops work"),
    ]
    drifted = _issue(
        2, title="Beta", body=_body_with_receipt("beta drifted", _stale_receipt())
    )
    gh = _LabelGh({"intake": intake, "ready": [drifted]})
    logs: list[str] = []
    invoker = _routing_invoker(
        {
            "Alpha": _ready_response("cmd-a"),
            "Beta": _ready_response("cmd-b"),
            "Gamma": _ready_response("cmd-c"),
        }
    )
    result = asyncio.run(
        _pass(
            gh,
            _FakeExecutor(exit_code=1),
            invoker,
            per_pass_cap=2,
            log=logs.append,
        ).run()
    )

    # Backlog order by number is [1 intake, 2 ready-drift, 3 intake]; the cap of
    # 2 takes issues 1 and 2, deferring the intake issue 3.
    assert [o.number for o in result.outcomes] == [1, 2]
    edited = [e[2] for e in gh.edits()]
    assert edited == ["1", "2"]
    assert "3" not in edited  # the intake issue past the cap is deferred
    assert result.deferred == 1
    assert any("deferring 1" in line for line in logs)

    # Issue 2 was the drifted ready issue -- re-triaged, so it consumed a slot.
    beta = next(o for o in result.outcomes if o.number == 2)
    assert beta.decision == DECISION_READY
    assert beta.receipt is not None
    assert beta.receipt.command == "cmd-b"


def test_per_pass_cap_must_be_positive() -> None:
    with pytest.raises(ValueError):
        _pass(
            _LabelGh({}),
            _FakeExecutor(),
            _scripted_invoker(_ready_response()),
            per_pass_cap=0,
        )
