"""The triage pass: intake issue -> fail-first ready spec, or needs-detail.

Grades the write-side engine (spec 00082 criteria 1-4, 13): a pass ends each
open intake-labeled issue either *ready* -- carrying an appended spec block
whose authoritative grader was executed against the current base and observed
to fail, with the receipt recorded -- or *needs-detail* with one comment naming
the specific missing information.

Every external effect is an injectable seam, so the pass runs hermetically: a
scripted ``gh`` runner, a scripted authoring invoker, and a scripted grader
executor, with no network, no SDK, and no real subprocess. The end-to-end
tests feed a freshly-triaged issue back through the drain-side trust rule
(:class:`GithubWorkSource` with ``require_triage_receipt``) to prove the receipt
is well-formed and unforgeable by construction.
"""

from __future__ import annotations

import asyncio
import json

from flywheel_orchestrator import GithubWorkSource
from flywheel_orchestrator._triage import (
    DECISION_NEEDS_DETAIL,
    DECISION_READY,
    CannotCompile,
    TriagePass,
    TriagePlan,
    parse_authoring_response,
    triage_authoring_prompt,
)
from flywheel_orchestrator._triage_receipt import RECEIPT_KEY, content_hash

REPO = "octo/widgets"
BASE_SHA = "basesha0000000000000000000000000000000000"


# --- seams ------------------------------------------------------------------


class _FakeGh:
    """Scripted gh runner: records argv, returns canned issue-list stdout."""

    def __init__(self, issues=None, *, events=None) -> None:
        self._issues_json = json.dumps(issues or [])
        self.calls: list[list[str]] = []
        self._events = events

    def __call__(self, argv) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["issue", "list"]:
            return self._issues_json
        if self._events is not None:
            self._events.append(("gh", argv[1]))
        return ""

    def edits(self) -> list[list[str]]:
        return [a for a in self.calls if a[:2] == ["issue", "edit"]]

    def comments(self) -> list[list[str]]:
        return [a for a in self.calls if a[:2] == ["issue", "comment"]]


class _FakeExecutor:
    """Scripted grader executor: records commands, returns a fixed exit code."""

    def __init__(self, exit_code: int = 1, *, events=None) -> None:
        self.exit_code = exit_code
        self.commands: list[str] = []
        self._events = events

    def __call__(self, command: str) -> int:
        self.commands.append(command)
        if self._events is not None:
            self._events.append(("exec", command))
        return self.exit_code


def _scripted_invoker(response: str, *, events=None):
    async def _invoke(prompt: str) -> str:
        if events is not None:
            events.append(("invoke", prompt))
        return response

    return _invoke


def _routing_invoker(mapping: dict[str, str]):
    async def _invoke(prompt: str) -> str:
        for needle, response in mapping.items():
            if needle in prompt:
                return response
        raise AssertionError(f"no scripted response for prompt: {prompt[:80]}")

    return _invoke


# --- fixtures ---------------------------------------------------------------


def _fenced(obj: dict) -> str:
    return "```json\n" + json.dumps(obj) + "\n```"


def _issue(
    number: int,
    *,
    title: str = "Fix the widget",
    body: str = "Widget drops retries under load.",
    url: str = "https://github.com/octo/widgets/issues/1",
) -> dict:
    return {"number": number, "title": title, "body": body, "url": url}


def _ready_response(command: str = "uv run pytest tests/widget") -> str:
    return _fenced(
        {
            "authoritative_grader": command,
            "graders": [{"type": "command", "run": command}],
            "goal": "Widget retries dropped requests under load.",
        }
    )


def _pass(gh: _FakeGh, executor: _FakeExecutor, invoker, **kwargs) -> TriagePass:
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


def _body_of(edit: list[str]) -> str:
    return edit[edit.index("--body") + 1]


def _label_after(argv: list[str], flag: str) -> str:
    return argv[argv.index(flag) + 1]


# --- ready: the fail-first happy path ---------------------------------------


def test_fail_first_grader_flips_issue_to_ready() -> None:
    title, prose = "Fix the widget", "Widget drops retries under load."
    gh = _FakeGh([_issue(12, title=title, body=prose)])
    ex = _FakeExecutor(exit_code=1)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(_ready_response())).run())

    (ready,) = result.ready
    assert result.needs_detail == ()
    assert ready.number == 12
    assert ready.decision == DECISION_READY
    assert ready.receipt is not None
    assert ready.receipt.command == "uv run pytest tests/widget"
    assert ready.receipt.exit_code == 1
    assert ready.receipt.base_sha == BASE_SHA
    assert ready.receipt.content_hash == content_hash(title, prose)

    # The executor ran the authoritative command exactly once.
    assert ex.commands == ["uv run pytest tests/widget"]

    # One gh edit: appends the block, adds ready, removes intake. No comment.
    (edit,) = gh.edits()
    assert edit[:3] == ["issue", "edit", "12"]
    assert _label_after(edit, "--add-label") == "ready"
    assert _label_after(edit, "--remove-label") == "intake"
    body = _body_of(edit)
    assert prose in body  # human text preserved verbatim
    assert "```flywheel" in body
    assert RECEIPT_KEY in body
    assert gh.comments() == []


def test_ready_block_carries_the_authoritative_command_as_a_grader() -> None:
    title, prose = "Fix the widget", "Widget drops retries."
    gh = _FakeGh([_issue(3, title=title, body=prose)])
    ex = _FakeExecutor(exit_code=2)
    asyncio.run(_pass(gh, ex, _scripted_invoker(_ready_response("cmd-x"))).run())

    body = _body_of(gh.edits()[0])
    block = _extract_block(body)
    runs = [g["run"] for g in block["graders"] if g["type"] == "command"]
    assert "cmd-x" in runs
    assert block[RECEIPT_KEY]["command"] == "cmd-x"


# --- ready: the receipt is unforgeable by construction ----------------------


def test_ready_flip_is_ordered_after_executor_and_uses_observed_exit_code() -> None:
    """The receipt records the executor's observed exit code, never the agent's.

    The scripted agent forges a passing ``exit_code`` in its response; the pass
    must ignore it and record the executor's observed 7, and the ready-flip
    write must come strictly after the executor call.
    """
    title, prose = "Fix the widget", "Widget drops retries."
    forged = _fenced(
        {
            "authoritative_grader": "cmd",
            "graders": [{"type": "command", "run": "cmd"}],
            "exit_code": 0,  # a forged pass claim -- must be ignored
        }
    )
    events: list[tuple[str, str]] = []
    gh = _FakeGh([_issue(5, title=title, body=prose)], events=events)
    ex = _FakeExecutor(exit_code=7, events=events)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(forged, events=events)).run())

    (ready,) = result.ready
    assert ready.receipt is not None
    assert ready.receipt.exit_code == 7  # observed, not the forged 0

    # Ordering: the executor call precedes the gh write.
    kinds = [k for k, _ in events]
    assert kinds.index("exec") < kinds.index("gh")

    # The embedded receipt carries the observed 7, not the forged 0.
    block = _extract_block(_body_of(gh.edits()[0]))
    assert block[RECEIPT_KEY]["exit_code"] == 7


# --- needs-detail: vacuous check (grader passes on the base) ----------------


def test_grader_passing_on_base_routes_to_needs_detail_as_vacuous() -> None:
    gh = _FakeGh([_issue(9)])
    ex = _FakeExecutor(exit_code=0)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(_ready_response("true"))).run())

    assert result.ready == ()
    (nd,) = result.needs_detail
    assert nd.decision == DECISION_NEEDS_DETAIL
    assert nd.exit_code == 0
    assert nd.receipt is None  # no receipt is minted for a vacuous check

    # needs-detail label added, intake removed, and NO spec block appended.
    (edit,) = gh.edits()
    assert _label_after(edit, "--add-label") == "needs-detail"
    assert _label_after(edit, "--remove-label") == "intake"
    assert "--body" not in edit

    # Exactly one comment; it flags vacuity and names the exact command.
    (comment,) = gh.comments()
    text = _label_after(comment, "--body")
    assert "vacuous" in text.lower()
    assert "true" in text


# --- needs-detail: the agent cannot compile a grader ------------------------


def test_cannot_compile_routes_to_needs_detail_naming_the_specific_gaps() -> None:
    gaps = [
        "which module owns the retry logic -- src/widget.py or src/net.py",
        "the expected retry count and backoff policy",
    ]
    response = _fenced({"authoritative_grader": "", "missing_information": gaps})
    gh = _FakeGh([_issue(4, title="Make it better", body="please improve retries")])
    ex = _FakeExecutor(exit_code=1)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(response)).run())

    assert result.ready == ()
    (nd,) = result.needs_detail
    assert nd.missing_information == tuple(gaps)

    # No candidate grader -> the executor was never consulted.
    assert ex.commands == []

    # Exactly one comment, naming BOTH specific gaps for this issue.
    (comment,) = gh.comments()
    text = _label_after(comment, "--body")
    for gap in gaps:
        assert gap in text

    (edit,) = gh.edits()
    assert _label_after(edit, "--add-label") == "needs-detail"
    assert _label_after(edit, "--remove-label") == "intake"
    assert "--body" not in edit  # body untouched -- no block appended


def test_cannot_compile_issue_yields_no_workitem_from_the_drain() -> None:
    # A needs-detail issue keeps its original (block-less) body, so the
    # drain-side trust rule schedules nothing from it.
    response = _fenced(
        {"authoritative_grader": "", "missing_information": ["name the module"]}
    )
    issue = _issue(4, title="Make it better", body="please improve retries")
    gh = _FakeGh([dict(issue)])
    asyncio.run(_pass(gh, _FakeExecutor(), _scripted_invoker(response)).run())

    drain = _FakeGh([dict(issue)])
    source = GithubWorkSource(
        repo=REPO, label="ready", runner=drain, require_triage_receipt=True
    )
    assert source.list_work() == []


# --- pre-existing spec block still requires the fail-first proof -------------


def _issue_with_block(number: int, title: str, prose: str, block_spec: dict) -> dict:
    block = "```flywheel\n" + json.dumps(block_spec) + "\n```"
    return _issue(number, title=title, body=prose + "\n\n" + block)


def test_human_block_does_not_short_circuit_when_grader_passes_on_base() -> None:
    # The issue already carries a hand-written, fail-first-looking receipt whose
    # hash matches. It must NOT be trusted: the executor governs, and a passing
    # candidate routes to needs-detail regardless of the pre-existing block.
    title, prose = "Fix the widget", "Widget drops retries."
    forged_receipt = {
        "command": "old",
        "exit_code": 1,
        "base_sha": "OLDBASE",
        "content_hash": content_hash(title, prose),
    }
    issue = _issue_with_block(7, title, prose, {RECEIPT_KEY: forged_receipt})
    gh = _FakeGh([issue])
    ex = _FakeExecutor(exit_code=0)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(_ready_response("true"))).run())

    assert result.ready == ()
    (nd,) = result.needs_detail
    assert nd.decision == DECISION_NEEDS_DETAIL


def test_human_block_is_replaced_by_a_fresh_receipt_when_grader_fails() -> None:
    title, prose = "Fix the widget", "Widget drops retries."
    stale_receipt = {
        "command": "old",
        "exit_code": 1,
        "base_sha": "OLDBASE",
        "content_hash": "deadbeef",
    }
    issue = _issue_with_block(8, title, prose, {RECEIPT_KEY: stale_receipt})
    gh = _FakeGh([issue])
    ex = _FakeExecutor(exit_code=1)
    result = asyncio.run(_pass(gh, ex, _scripted_invoker(_ready_response())).run())

    (ready,) = result.ready
    assert ready.receipt is not None
    assert ready.receipt.base_sha == BASE_SHA  # fresh, not the stale OLDBASE
    assert ready.receipt.command == "uv run pytest tests/widget"

    body = _body_of(gh.edits()[0])
    assert body.count("```flywheel") == 1  # exactly one block after replacement
    assert prose in body
    block = _extract_block(body)
    assert block[RECEIPT_KEY]["base_sha"] == BASE_SHA
    assert block[RECEIPT_KEY]["command"] == "uv run pytest tests/widget"


# --- end to end: a ready issue is schedulable by the drain ------------------


def test_ready_issue_is_scheduled_by_the_drain_trust_rule() -> None:
    title, prose = "Fix the widget", "Widget drops retries under load."
    gh = _FakeGh([_issue(12, title=title, body=prose)])
    asyncio.run(_pass(gh, _FakeExecutor(exit_code=1), _scripted_invoker(_ready_response())).run())
    new_body = _body_of(gh.edits()[0])

    # Feed the freshly-triaged issue back through the drain-side trust rule.
    drain = _FakeGh([_issue(12, title=title, body=new_body)])
    source = GithubWorkSource(
        repo=REPO, label="ready", runner=drain, require_triage_receipt=True
    )
    (item,) = source.list_work()
    assert item.task.id == "gh-12"
    assert item.task.graders[0].run == "uv run pytest tests/widget"  # type: ignore[union-attr]


# --- a whole pass partitions the board --------------------------------------


def test_pass_partitions_ready_and_needs_detail_across_the_board() -> None:
    ready = _fenced(
        {
            "authoritative_grader": "uv run pytest tests/alpha",
            "graders": [{"type": "command", "run": "uv run pytest tests/alpha"}],
        }
    )
    stuck = _fenced(
        {"authoritative_grader": "", "missing_information": ["name the module"]}
    )
    gh = _FakeGh(
        [
            _issue(1, title="Alpha needs a retry fix", body="alpha drops work"),
            _issue(2, title="Beta is vague", body="make beta nicer"),
        ]
    )
    ex = _FakeExecutor(exit_code=1)
    invoker = _routing_invoker({"Alpha": ready, "Beta": stuck})
    result = asyncio.run(_pass(gh, ex, invoker).run())

    assert [o.number for o in result.ready] == [1]
    assert [o.number for o in result.needs_detail] == [2]
    # Exactly one comment across the whole pass -- only the needs-detail issue.
    assert len(gh.comments()) == 1
    assert gh.comments()[0][:3] == ["issue", "comment", "2"]
    # The ready issue was never commented on, only edited.
    assert len(gh.edits()) == 2


# --- the authoring prompt and response parser (unit) ------------------------


def test_authoring_prompt_targets_stated_intent_and_asks_for_specific_gaps() -> None:
    prompt = triage_authoring_prompt(
        title="Fix the widget", body="Widget drops retries under load."
    )
    assert "Widget drops retries under load." in prompt
    assert "Fix the widget" in prompt
    low = prompt.lower()
    # Derives from the issue's stated intent, not any check that fails on base.
    assert "intent" in low
    assert "unrelated" in low
    assert "fails today" in low
    # Instructs the agent to name specific gaps for THIS issue.
    assert "specific" in low


def test_parse_authoring_response_variants() -> None:
    plan = parse_authoring_response(_ready_response("cmd"))
    assert isinstance(plan, TriagePlan)
    assert plan.authoritative_grader == "cmd"

    stuck = parse_authoring_response(
        _fenced({"authoritative_grader": "", "missing_information": ["a", "b"]})
    )
    assert isinstance(stuck, CannotCompile)
    assert stuck.missing_information == ("a", "b")

    # An unparseable response still routes to a cannot-compile (never a crash).
    assert isinstance(parse_authoring_response("not json at all"), CannotCompile)


# --- helpers ----------------------------------------------------------------


def _extract_block(body: str) -> dict:
    """Parse the ``flywheel`` fenced block embedded in an issue body."""
    lines = body.splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.strip() == "```flywheel")
    end = next(
        i for i in range(start + 1, len(lines)) if lines[i].strip() == "```"
    )
    return json.loads("\n".join(lines[start + 1 : end]))
