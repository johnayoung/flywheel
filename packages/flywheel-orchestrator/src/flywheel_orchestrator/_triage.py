"""Triage pass: compile intake-labeled issues to ready or needs-detail.

A triage pass reads every open issue carrying the intake label and ends each
one in exactly one terminal state:

* **ready** -- the issue body gains an appended ``flywheel`` spec block whose
  authoritative grader was *executed against the current base and observed to
  fail* (non-zero exit). The block carries a
  :class:`~flywheel_orchestrator._triage_receipt.TriageReceipt` recording that
  command, the observed exit code, the base SHA, and the content hash of the
  issue's human text, so the drain-side trust rule
  (:class:`~flywheel_orchestrator._github.GithubWorkSource` built with
  ``require_triage_receipt``) can schedule it. The ready label is added and the
  intake label removed.
* **needs-detail** -- the issue gains the needs-detail label and exactly one
  comment naming the specific missing information. No spec block is appended and
  no receipt is minted. This is the outcome when the authoring agent cannot
  derive an authoritative grader from the issue's stated intent, or when the
  candidate grader it derives exits 0 against the base (a vacuous check that
  proves nothing).

Every external effect is an injectable seam so a triage pass runs hermetically
in tests -- no network, no SDK, no real subprocess:

* ``runner`` -- the ``gh`` subprocess seam
  (:data:`~flywheel_orchestrator._github.GhRunner`), reused for both the issue
  read and every label / body / comment write.
* ``invoker`` -- the authoring-agent seam (:data:`TriageAuthoringInvoker`, the
  same shape as :data:`~flywheel_orchestrator._autopilot.AutopilotInvoker`). The
  production builder resolves the claude SDK lazily through
  ``flywheel_core._sdk`` (see :func:`build_triage_authoring_invoker`), so
  importing this module never requires the ``claude`` extra.
* ``executor`` -- the grader-command seam (:data:`TriageGraderExecutor`): given
  the authoritative command, run it against the current base and return its
  observed exit code. The ready flip is ordered strictly after this call and
  records the exit code the executor observed, never one the agent reported --
  agent claims are untrusted, so the receipt is unforgeable by construction.

The pass writes only where issue state demands it. Its backlog is the intake
issues (always first-time candidates) merged with the *ready* issues whose
receipt drifted from their current content (:func:`_ready_needs_retriage`); a
settled ready issue -- one still carrying a fail-first receipt matching its
content -- is not a candidate, so a pass over unchanged state records zero
GitHub writes. Re-triage of a drifted ready issue rewrites its body so no stale
receipt survives -- refreshed in place when it stays ready, block-stripped when
it demotes to needs-detail. Each pass processes at most ``per_pass_cap``
candidates in issue-number order and surfaces the deferred remainder through the
``log`` seam and :attr:`TriagePassResult.deferred`.

Non-goals: there is no CLI verb, no daemon loop, and no ``[triage]`` policy
binding here -- the repo, labels, base SHA, and per-pass cap arrive as explicit
constructor arguments.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Awaitable, Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from flywheel_orchestrator._autopilot import build_repo_invoker
from flywheel_orchestrator._github import GhRunner
from flywheel_orchestrator._triage_receipt import (
    RECEIPT_KEY,
    TriageReceipt,
    content_hash,
    is_fail_first,
    parse_receipt,
    strip_spec_block,
)

#: The authoring-agent seam: a coroutine taking a prompt and returning the
#: agent's response text. Structurally identical to
#: :data:`~flywheel_orchestrator._autopilot.AutopilotInvoker`; the production
#: builder resolves the SDK lazily so importing this module never requires it.
TriageAuthoringInvoker = Callable[[str], Awaitable[str]]

#: The grader-command seam: given the authoritative command string, run it
#: against the current base and return its observed exit code. This is the sole
#: authority for the fail-first proof -- the ready flip reads its return value,
#: never an agent-reported status.
TriageGraderExecutor = Callable[[str], int]

#: Terminal triage decisions.
DECISION_READY = "ready"
DECISION_NEEDS_DETAIL = "needs_detail"

#: Backlog-item kinds: a first-time intake issue vs. a drifted ready issue that
#: must be re-triaged. Both consume a per-pass cap slot.
_KIND_INTAKE = "intake"
_KIND_READY_DRIFT = "ready_drift"

#: Default turn budget for the real SDK-backed authoring invoker (mirrors
#: :data:`~flywheel_orchestrator._autopilot.DEFAULT_AUTHORING_MAX_TURNS`).
DEFAULT_TRIAGE_MAX_TURNS: int = 120

_SPEC_FENCE_OPEN = "```flywheel"
_SPEC_FENCE_CLOSE = "```"
_LIST_FIELDS = "number,title,body,url"
_LIST_LIMIT = "200"


class TriageError(Exception):
    """A triage pass could not read the board (malformed ``gh`` output)."""


@dataclass(frozen=True, kw_only=True)
class TriagePlan:
    """A compilable triage plan the authoring agent derived from an issue.

    ``authoritative_grader`` is the single fail-first command; ``graders`` are
    the grader dicts to embed in the spec block (the authoritative command is
    guaranteed to appear among them). ``goal``/``context``/``tags``/
    ``prerequisites`` are optional spec-block fields carried verbatim.
    """

    authoritative_grader: str
    graders: tuple[dict[str, Any], ...] = ()
    goal: str | None = None
    context: dict[str, Any] | None = None
    tags: tuple[str, ...] = ()
    prerequisites: tuple[str, ...] = ()


@dataclass(frozen=True, kw_only=True)
class CannotCompile:
    """The authoring agent could not derive an authoritative grader.

    ``missing_information`` names the specific facts the issue would need to
    become compilable -- one entry per gap, phrased for THIS issue.
    """

    missing_information: tuple[str, ...] = ()


@dataclass(frozen=True, kw_only=True)
class TriageOutcome:
    """The terminal triage decision for one issue."""

    number: int
    source_ref: str
    decision: str
    authoritative_command: str | None = None
    exit_code: int | None = None
    receipt: TriageReceipt | None = None
    comment: str | None = None
    missing_information: tuple[str, ...] = ()


@dataclass(frozen=True, kw_only=True)
class TriagePassResult:
    """The outcomes of one triage pass, in board order.

    ``deferred`` counts the work candidates cut past this pass's ``per_pass_cap``
    -- issues left untouched this pass (0 when uncapped or within the cap).
    """

    outcomes: tuple[TriageOutcome, ...] = ()
    deferred: int = 0

    @property
    def ready(self) -> tuple[TriageOutcome, ...]:
        return tuple(
            o for o in self.outcomes if o.decision == DECISION_READY
        )

    @property
    def needs_detail(self) -> tuple[TriageOutcome, ...]:
        return tuple(
            o for o in self.outcomes if o.decision == DECISION_NEEDS_DETAIL
        )


def triage_authoring_prompt(*, title: str, body: str) -> str:
    """Build the headless triage-authoring prompt for one issue.

    The prompt instructs the agent to derive the authoritative grader from the
    intent the human states in the issue -- a command that fails today *because*
    that stated outcome does not yet hold -- not merely any check that happens
    to fail on the current base. When the issue lacks the information to write
    such a grader, the agent must name the specific gaps for THIS issue rather
    than emit a vague check.
    """
    human = strip_spec_block(body) or "(the issue body is empty)"
    return (
        "You are flywheel's issue-triage authoring agent. A human filed the "
        "GitHub issue below. Decide whether its stated intent can be turned "
        "into a single, concrete, fail-first grader: a command that FAILS "
        "today precisely because the outcome the issue asks for does not yet "
        "hold, and would PASS once that outcome is delivered.\n\n"
        f"Issue title: {title}\n"
        f"Issue body (human-authored):\n{human}\n\n"
        "HARD RULES:\n"
        "- Derive the authoritative grader from the intent the human states "
        "in the issue above. It must encode THAT outcome. Do NOT reach for "
        "some unrelated check merely because it happens to fail on the "
        "current base -- a grader that fails for a reason the issue did not "
        "ask about is wrong, even though it is red today.\n"
        "- The grader must be a single shell command runnable from the repo "
        "root (a test invocation, a lint/build command, an assertion "
        "script).\n"
        "- If the issue does not carry enough information to write such a "
        "grader -- the desired behavior, the affected module, or the "
        "acceptance signal is missing or ambiguous -- do NOT invent one. "
        "Instead list the SPECIFIC facts you would need, each naming the "
        "exact gap for THIS issue (e.g. \"which module owns the retry "
        "logic\", not \"add more detail\").\n\n"
        "Respond with exactly one fenced JSON block:\n"
        "```json\n"
        "{\n"
        '  "authoritative_grader": "<one shell command that fails today for '
        'the reason the issue states, or empty string if you cannot write '
        'one>",\n'
        '  "graders": [{"type": "command", "run": "<same or additional '
        'commands>"}],\n'
        '  "goal": "<one sentence naming the outcome the grader proves>",\n'
        '  "context": {"relevant": ["<path>"]},\n'
        '  "missing_information": ["<specific fact this issue is missing>"]\n'
        "}\n"
        "```\n"
        "Populate \"missing_information\" (and leave \"authoritative_grader\" "
        "empty) when and only when the issue cannot be compiled; otherwise "
        "leave \"missing_information\" empty."
    )


_NEEDS_DETAIL_PREAMBLE = (
    "flywheel triage could not compile this issue into a runnable, fail-first "
    "check from its current text. To move it to ready, please add the "
    "following specific detail:"
)


def _needs_detail_comment(missing_information: Sequence[str]) -> str:
    """The needs-detail comment naming the specific missing facts.

    Enumerates the gaps the authoring agent named for THIS issue. When the
    agent signalled cannot-compile without naming any gap, the load-bearing
    facts every issue needs are requested instead of an empty ask.
    """
    gaps = [g.strip() for g in missing_information if g and g.strip()]
    if not gaps:
        gaps = [
            "the concrete behavior or outcome this issue should produce",
            "the module or file the change affects",
            "the signal that proves the work is done (a test or command)",
        ]
    bullets = "\n".join(f"- {g}" for g in gaps)
    return f"{_NEEDS_DETAIL_PREAMBLE}\n\n{bullets}"


def _vacuity_comment(command: str) -> str:
    """The needs-detail comment for a candidate grader that passes on the base.

    Names the exact command triage derived and explains that, because it is
    already green against the current base, it is a vacuous check that proves
    nothing is broken -- so there is no fail-first work to schedule.
    """
    return (
        "flywheel triage derived the check below from this issue, but it "
        "already passes against the current base -- a vacuous check that "
        "proves nothing is broken:\n\n"
        f"    {command}\n\n"
        "A check that is green today cannot be the fail-first proof for new "
        "work. Please sharpen the issue so the intended gap is expressed by a "
        "check that fails now -- name the exact behavior that is missing or "
        "the input that is mishandled -- and it will be re-triaged."
    )


def _extract_json_object(text: str) -> dict[str, Any]:
    """Extract the structured JSON object from an agent response.

    Prefers a ```` ```json ```` fenced block (the contract in
    :func:`triage_authoring_prompt`); falls back to the outermost ``{...}`` span
    so a model that omitted the fence still parses. Raises :class:`ValueError`
    when no JSON object is present. Mirrors the autopilot extractor.
    """
    fence = "```json"
    start = text.find(fence)
    if start != -1:
        body_start = start + len(fence)
        end = text.find("```", body_start)
        if end != -1:
            data = json.loads(text[body_start:end].strip())
            if not isinstance(data, dict):
                raise ValueError("fenced JSON is not an object")
            return data
    first = text.find("{")
    last = text.rfind("}")
    if first == -1 or last == -1 or last <= first:
        raise ValueError("no JSON object found in agent response")
    data = json.loads(text[first : last + 1])
    if not isinstance(data, dict):
        raise ValueError("response JSON is not an object")
    return data


def parse_authoring_response(text: str) -> TriagePlan | CannotCompile:
    """Parse the authoring agent's response into a plan or a cannot-compile.

    A response whose ``authoritative_grader`` is absent or empty is a
    cannot-compile (its ``missing_information`` names the gaps). An unparseable
    response is treated as cannot-compile with no named gaps -- the pass still
    routes the issue to needs-detail rather than stranding it.
    """
    try:
        data = _extract_json_object(text)
    except (ValueError, json.JSONDecodeError):
        return CannotCompile()

    authoritative = data.get("authoritative_grader")
    if not isinstance(authoritative, str) or not authoritative.strip():
        raw_missing = data.get("missing_information")
        missing = (
            tuple(
                str(m)
                for m in raw_missing
                if isinstance(m, str) and m.strip()
            )
            if isinstance(raw_missing, list)
            else ()
        )
        return CannotCompile(missing_information=missing)

    raw_graders = data.get("graders")
    graders = (
        tuple(g for g in raw_graders if isinstance(g, dict))
        if isinstance(raw_graders, list)
        else ()
    )
    goal = data.get("goal")
    context = data.get("context")
    raw_tags = data.get("tags")
    tags = (
        tuple(str(t) for t in raw_tags) if isinstance(raw_tags, list) else ()
    )
    raw_prereqs = data.get("prerequisites")
    prerequisites = (
        tuple(str(p) for p in raw_prereqs)
        if isinstance(raw_prereqs, list)
        else ()
    )
    return TriagePlan(
        authoritative_grader=authoritative.strip(),
        graders=graders,
        goal=goal.strip() if isinstance(goal, str) and goal.strip() else None,
        context=context if isinstance(context, dict) else None,
        tags=tags,
        prerequisites=prerequisites,
    )


def _ensure_authoritative_grader(plan: TriagePlan) -> list[dict[str, Any]]:
    """The spec-block grader list, guaranteed to carry the authoritative run.

    The authoritative command must be a command grader on the resulting task so
    the drain schedules a run that executes it. If the agent's grader list
    already includes it, it is used verbatim; otherwise it is prepended.
    """
    graders = [dict(g) for g in plan.graders]
    runs = {g.get("run") for g in graders if g.get("type") == "command"}
    if plan.authoritative_grader not in runs:
        graders.insert(
            0, {"type": "command", "run": plan.authoritative_grader}
        )
    return graders


def _embed_spec_block(
    body: str, plan: TriagePlan, receipt: TriageReceipt
) -> str:
    """Append the machine spec block, preserving the human text verbatim.

    Any pre-existing ``flywheel`` block is stripped so the drain reads exactly
    one block -- the fresh one carrying this pass's receipt. The human prose is
    preserved (only its boundary whitespace is normalized, matching how
    :func:`content_hash` reads it), so the appended body hashes identically to
    the receipt's ``content_hash``.
    """
    human = strip_spec_block(body)
    spec: dict[str, Any] = {}
    if plan.goal:
        spec["goal"] = plan.goal
    spec["graders"] = _ensure_authoritative_grader(plan)
    if plan.context:
        spec["context"] = plan.context
    if plan.tags:
        spec["tags"] = list(plan.tags)
    if plan.prerequisites:
        spec["prerequisites"] = list(plan.prerequisites)
    spec[RECEIPT_KEY] = {
        "command": receipt.command,
        "exit_code": receipt.exit_code,
        "base_sha": receipt.base_sha,
        "content_hash": receipt.content_hash,
    }
    block = (
        _SPEC_FENCE_OPEN
        + "\n"
        + json.dumps(spec, indent=2)
        + "\n"
        + _SPEC_FENCE_CLOSE
    )
    return f"{human}\n\n{block}" if human else block


def _issue_sort_key(issue: dict[str, Any]) -> int:
    """Sort key by issue number, tolerant of malformed payloads."""
    number = issue.get("number")
    return number if isinstance(number, int) else 0


def _spec_block_of(body: str) -> dict[str, Any] | None:
    """The issue body's ``flywheel`` spec block as a dict, or ``None``.

    A tolerant reader for the triage side: unlike the drain's strict
    ``_extract_spec_block`` (which raises on a malformed block an author meant),
    a missing, unterminated, non-JSON, or non-object block all yield ``None``.
    The re-triage caller treats every "no trustworthy block" case uniformly as
    drift, so a raise here would be wrong -- the point is to *fix* such an issue.
    """
    lines = body.splitlines()
    open_idx: int | None = None
    for idx, line in enumerate(lines):
        if line.strip() == _SPEC_FENCE_OPEN:
            open_idx = idx
            break
    if open_idx is None:
        return None
    block: list[str] = []
    for line in lines[open_idx + 1 :]:
        if line.strip() == _SPEC_FENCE_CLOSE:
            try:
                data = json.loads("\n".join(block))
            except (json.JSONDecodeError, RecursionError):
                return None
            return data if isinstance(data, dict) else None
        block.append(line)
    return None


def _ready_needs_retriage(title: str, body: str) -> bool:
    """Whether a ready-labeled issue must be re-triaged this pass.

    A ready issue is *settled* -- left untouched, so an idempotent pass writes
    nothing -- exactly when its spec block carries a fail-first triage receipt
    whose ``content_hash`` still matches the issue's current human content. Drift
    (a human edited the title/body after triage), a missing or malformed
    receipt, or a non-fail-first receipt all mean the receipt no longer attests
    the current content, so the issue is re-triaged. The receipt schema and the
    content-hash definition are imported from
    :mod:`flywheel_orchestrator._triage_receipt`, never redefined here.
    """
    receipt = parse_receipt((_spec_block_of(body) or {}).get(RECEIPT_KEY))
    if receipt is None or not is_fail_first(receipt):
        return True
    return receipt.content_hash != content_hash(title, body)


class TriagePass:
    """One triage pass over a repo's open intake-labeled issues.

    ``runner`` is the ``gh`` subprocess seam; ``invoker`` drives the authoring
    agent; ``executor`` runs the authoritative command against the current base
    (``base_sha``). All three are injected so a pass runs hermetically in tests.
    ``intake_label``/``ready_label``/``needs_detail_label`` are the board labels
    the pass reads and transitions between -- no policy binding, they arrive as
    explicit arguments. ``per_pass_cap`` bounds how many work candidates a single
    pass processes (``None`` is unbounded); the remainder is deferred untouched
    and surfaced through ``log`` and :attr:`TriagePassResult.deferred`.
    """

    def __init__(
        self,
        *,
        repo: str,
        intake_label: str,
        ready_label: str,
        needs_detail_label: str,
        base_sha: str,
        invoker: TriageAuthoringInvoker,
        executor: TriageGraderExecutor,
        runner: GhRunner,
        per_pass_cap: int | None = None,
        log: Callable[[str], None] | None = None,
    ) -> None:
        if not base_sha:
            raise ValueError("base_sha must be a non-empty commit SHA")
        if per_pass_cap is not None and per_pass_cap < 1:
            raise ValueError("per_pass_cap must be a positive integer or None")
        self.repo = repo
        self.intake_label = intake_label
        self.ready_label = ready_label
        self.needs_detail_label = needs_detail_label
        self.base_sha = base_sha
        self._invoke = invoker
        self._execute = executor
        self._run = runner
        self._per_pass_cap = per_pass_cap
        self._log = log

    async def run(self) -> TriagePassResult:
        """Triage the board with write discipline; return the terminal outcomes.

        The backlog is the intake issues (first-time candidates) merged with the
        drifted ready issues, deduplicated and ordered by issue number. Settled
        ready issues are not candidates, so a pass over unchanged state writes
        nothing. Candidates are processed up to ``per_pass_cap``; the remainder
        is deferred untouched, counted, and logged.
        """
        backlog = self._backlog(
            self._list_issues(self.intake_label),
            self._list_issues(self.ready_label),
        )
        selected = backlog
        deferred = 0
        cap = self._per_pass_cap
        if cap is not None and len(backlog) > cap:
            selected = backlog[:cap]
            deferred = len(backlog) - cap
            self._log_line(
                f"[triage] per-pass cap {cap} reached: processing {cap} of "
                f"{len(backlog)} candidates, deferring {deferred} to a later "
                f"pass"
            )
        outcomes: list[TriageOutcome] = []
        for kind, issue in selected:
            if kind == _KIND_READY_DRIFT:
                outcome = await self._retriage_ready(issue)
            else:
                outcome = await self._triage_issue(issue)
            if outcome is not None:
                outcomes.append(outcome)
        return TriagePassResult(outcomes=tuple(outcomes), deferred=deferred)

    def _backlog(
        self,
        intake: list[dict[str, Any]],
        ready: list[dict[str, Any]],
    ) -> list[tuple[str, dict[str, Any]]]:
        """The ordered work backlog: intake issues plus drifted ready issues.

        Every intake issue is a first-time candidate; a ready issue enters only
        when :func:`_ready_needs_retriage` holds. A number is added at most once
        -- intake and ready are disjoint labels in reality, and the guard also
        keeps a runner that conflates the two (the test fakes) honest. The
        backlog is sorted by issue number so a drifted ready issue mid-board
        still consumes a per-pass cap slot.
        """
        items: list[tuple[str, dict[str, Any]]] = []
        seen: set[int] = set()
        for issue in intake:
            number = issue.get("number")
            if not isinstance(number, int):
                self._log_line(
                    f"[triage] skipping issue with non-integer number: {issue!r}"
                )
                continue
            if number in seen:
                continue
            seen.add(number)
            items.append((_KIND_INTAKE, issue))
        for issue in ready:
            number = issue.get("number")
            if not isinstance(number, int):
                self._log_line(
                    f"[triage] skipping issue with non-integer number: {issue!r}"
                )
                continue
            if number in seen:
                continue
            seen.add(number)
            title = str(issue.get("title") or "")
            body = str(issue.get("body") or "")
            if _ready_needs_retriage(title, body):
                items.append((_KIND_READY_DRIFT, issue))
        items.sort(key=lambda item: _issue_sort_key(item[1]))
        return items

    def _list_issues(self, label: str) -> list[dict[str, Any]]:
        stdout = self._run(
            [
                "issue",
                "list",
                "--repo",
                self.repo,
                "--label",
                label,
                "--state",
                "open",
                "--json",
                _LIST_FIELDS,
                "--limit",
                _LIST_LIMIT,
            ]
        )
        try:
            issues = json.loads(stdout or "[]")
        except json.JSONDecodeError as exc:
            raise TriageError(
                f"gh issue list returned invalid JSON: {exc}"
            ) from exc
        if not isinstance(issues, list):
            raise TriageError(
                f"gh issue list returned {type(issues).__name__}, "
                f"expected a list"
            )
        return sorted(
            (i for i in issues if isinstance(i, dict)),
            key=_issue_sort_key,
        )

    async def _triage_issue(
        self, issue: dict[str, Any]
    ) -> TriageOutcome | None:
        number = issue.get("number")
        if not isinstance(number, int):
            self._log_line(
                f"[triage] skipping issue with non-integer number: {issue!r}"
            )
            return None
        source_ref = f"{self.repo}#{number}"
        title = str(issue.get("title") or "")
        body = str(issue.get("body") or "")

        response = await self._invoke(
            triage_authoring_prompt(title=title, body=body)
        )
        plan = parse_authoring_response(response)

        if isinstance(plan, CannotCompile):
            comment = _needs_detail_comment(plan.missing_information)
            self._apply_needs_detail(number, comment)
            return TriageOutcome(
                number=number,
                source_ref=source_ref,
                decision=DECISION_NEEDS_DETAIL,
                comment=comment,
                missing_information=plan.missing_information,
            )

        # A candidate grader exists. The ready flip is gated on the executor's
        # observed exit code, read here BEFORE any GitHub write -- the receipt
        # can only record what the executor saw, never what the agent claimed.
        exit_code = self._execute(plan.authoritative_grader)
        if exit_code == 0:
            comment = _vacuity_comment(plan.authoritative_grader)
            self._apply_needs_detail(number, comment)
            return TriageOutcome(
                number=number,
                source_ref=source_ref,
                decision=DECISION_NEEDS_DETAIL,
                authoritative_command=plan.authoritative_grader,
                exit_code=0,
                comment=comment,
            )

        receipt = TriageReceipt(
            command=plan.authoritative_grader,
            exit_code=exit_code,
            base_sha=self.base_sha,
            content_hash=content_hash(title, body),
        )
        new_body = _embed_spec_block(body, plan, receipt)
        self._apply_ready(number, new_body)
        return TriageOutcome(
            number=number,
            source_ref=source_ref,
            decision=DECISION_READY,
            authoritative_command=plan.authoritative_grader,
            exit_code=exit_code,
            receipt=receipt,
        )

    async def _retriage_ready(
        self, issue: dict[str, Any]
    ) -> TriageOutcome | None:
        """Re-triage a ready issue whose receipt drifted from its content.

        Mirrors :meth:`_triage_issue`, but its writes guarantee no stale receipt
        survives (the invariant a drifted issue would otherwise violate): a
        still-fail-first re-triage refreshes the spec block in place
        (:meth:`_refresh_ready`, which strips the old block before appending the
        new one); a vacuous or uncompilable re-triage strips the stale block and
        demotes the issue to needs-detail (:meth:`_demote_to_needs_detail`). The
        exit code is the executor's, never the agent's -- the receipt stays
        unforgeable.
        """
        number = issue.get("number")
        if not isinstance(number, int):
            self._log_line(
                f"[triage] skipping issue with non-integer number: {issue!r}"
            )
            return None
        source_ref = f"{self.repo}#{number}"
        title = str(issue.get("title") or "")
        body = str(issue.get("body") or "")

        response = await self._invoke(
            triage_authoring_prompt(title=title, body=body)
        )
        plan = parse_authoring_response(response)

        if isinstance(plan, CannotCompile):
            comment = _needs_detail_comment(plan.missing_information)
            self._demote_to_needs_detail(number, body, comment)
            return TriageOutcome(
                number=number,
                source_ref=source_ref,
                decision=DECISION_NEEDS_DETAIL,
                comment=comment,
                missing_information=plan.missing_information,
            )

        exit_code = self._execute(plan.authoritative_grader)
        if exit_code == 0:
            comment = _vacuity_comment(plan.authoritative_grader)
            self._demote_to_needs_detail(number, body, comment)
            return TriageOutcome(
                number=number,
                source_ref=source_ref,
                decision=DECISION_NEEDS_DETAIL,
                authoritative_command=plan.authoritative_grader,
                exit_code=0,
                comment=comment,
            )

        receipt = TriageReceipt(
            command=plan.authoritative_grader,
            exit_code=exit_code,
            base_sha=self.base_sha,
            content_hash=content_hash(title, body),
        )
        new_body = _embed_spec_block(body, plan, receipt)
        self._refresh_ready(number, new_body)
        return TriageOutcome(
            number=number,
            source_ref=source_ref,
            decision=DECISION_READY,
            authoritative_command=plan.authoritative_grader,
            exit_code=exit_code,
            receipt=receipt,
        )

    def _apply_ready(self, number: int, new_body: str) -> None:
        self._run(
            [
                "issue",
                "edit",
                str(number),
                "--repo",
                self.repo,
                "--body",
                new_body,
                "--add-label",
                self.ready_label,
                "--remove-label",
                self.intake_label,
            ]
        )

    def _refresh_ready(self, number: int, new_body: str) -> None:
        """Refresh a still-ready issue's body with the fresh spec block/receipt.

        ``_embed_spec_block`` strips the drifted block before appending the new
        one, so no stale receipt survives. The issue is already ready-labeled;
        re-adding the label keeps the write idempotent and touches no other
        label (unlike :meth:`_apply_ready`, there is no intake label to remove).
        """
        self._run(
            [
                "issue",
                "edit",
                str(number),
                "--repo",
                self.repo,
                "--body",
                new_body,
                "--add-label",
                self.ready_label,
            ]
        )

    def _demote_to_needs_detail(
        self, number: int, body: str, comment: str
    ) -> None:
        """Demote a drifted ready issue to needs-detail, stripping the stale block.

        The body is rewritten with its ``flywheel`` spec block removed so the
        stale receipt does not survive the demotion; the ready label is dropped,
        the needs-detail label added, and exactly one comment left.
        """
        self._run(
            [
                "issue",
                "edit",
                str(number),
                "--repo",
                self.repo,
                "--body",
                strip_spec_block(body),
                "--add-label",
                self.needs_detail_label,
                "--remove-label",
                self.ready_label,
            ]
        )
        self._run(
            [
                "issue",
                "comment",
                str(number),
                "--repo",
                self.repo,
                "--body",
                comment,
            ]
        )

    def _apply_needs_detail(self, number: int, comment: str) -> None:
        self._run(
            [
                "issue",
                "edit",
                str(number),
                "--repo",
                self.repo,
                "--add-label",
                self.needs_detail_label,
                "--remove-label",
                self.intake_label,
            ]
        )
        self._run(
            [
                "issue",
                "comment",
                str(number),
                "--repo",
                self.repo,
                "--body",
                comment,
            ]
        )

    def _log_line(self, line: str) -> None:
        if self._log is not None:
            self._log(line)


def build_triage_authoring_invoker(
    repo_root: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_TRIAGE_MAX_TURNS,
) -> TriageAuthoringInvoker:
    """The production authoring seam: a claude session rooted in ``repo_root``.

    Delegates to :func:`~flywheel_orchestrator._autopilot.build_repo_invoker`, so
    the lazy ``flywheel_core._sdk`` boundary and wall-clock deadline discipline
    are shared verbatim: importing this module never requires the ``claude``
    extra, and the SDK is resolved only when an unscripted pass drives an agent.
    """
    return build_repo_invoker(repo_root, model=model, max_turns=max_turns)


def build_grader_executor(repo_root: Path) -> TriageGraderExecutor:
    """The production grader-command seam: run the command against the base.

    Executes the authoritative command with ``shell=True`` rooted in
    ``repo_root`` (the checkout at the current base), mirroring how command
    graders run, and returns the observed exit code.
    """

    def _execute(command: str) -> int:
        proc = subprocess.run(
            command,
            shell=True,
            cwd=str(repo_root),
            capture_output=True,
            text=True,
            check=False,
        )
        return proc.returncode

    return _execute


def resolve_base_sha(repo_root: Path) -> str:
    """Resolve the current base commit SHA of ``repo_root`` for the receipt."""
    proc = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


__all__ = [
    "DECISION_NEEDS_DETAIL",
    "DECISION_READY",
    "DEFAULT_TRIAGE_MAX_TURNS",
    "CannotCompile",
    "TriageAuthoringInvoker",
    "TriageError",
    "TriageGraderExecutor",
    "TriageOutcome",
    "TriagePass",
    "TriagePassResult",
    "TriagePlan",
    "build_grader_executor",
    "build_triage_authoring_invoker",
    "parse_authoring_response",
    "resolve_base_sha",
    "triage_authoring_prompt",
]
