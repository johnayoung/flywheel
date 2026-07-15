"""GitHub Issues work source — a tracker-backed :class:`WorkSource`.

Maps labeled GitHub issues to flywheel tasks through the ``gh`` CLI (no
extra dependencies; auth is whatever ``gh auth`` already holds). The board
is the inbound control plane; flywheel's store stays the authoritative
execution record, and outcomes are projected back as issue comments.

Issue -> Task compilation:

* ``id`` is ``gh-<number>`` (stable, referenceable from other issues'
  ``prerequisites``).
* ``goal`` is the issue title, unless overridden by a spec block.
* An issue MAY embed a fenced spec block whose info string is exactly
  ``flywheel``::

      ```flywheel
      {
        "goal": "One-sentence diff test.",
        "graders": [{"type": "command", "run": "uv run pytest"}],
        "context": {"relevant": ["src/thing.py"]},
        "tags": ["loop"],
        "prerequisites": ["gh-12"]
      }
      ```

  Every key is optional. ``graders`` falls back to the repo policy's
  default graders (see :mod:`flywheel_orchestrator._policy`); an issue
  with no graders from either source is **not runnable** and is skipped —
  the readiness gate stays mechanical without forcing every ticket author
  to write graders.
* The issue body is carried in ``context.notes`` (unless the spec block
  sets its own) and the issue URL in ``context.references``, so the agent
  sees the full ticket text.

Outbound: :meth:`GithubWorkSource.report` posts a comment with the run id,
terminal status, and the final attempt's grader receipts. When the run
reached ``DONE`` and the policy says ``done_action = "close"``, the issue
is closed with that comment instead. Ticket writes go through the harness's
reporting path — never through the agent — so "agent claims are untrusted"
extends to ticket transitions.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
from collections.abc import Callable, Sequence
from typing import Any

from flywheel_core.lifecycle import Status
from flywheel_core.loaders import TaskLoadError, load_graders
from flywheel_core.task import Context, Grader, Task, ValidationError

from flywheel_orchestrator._claims import (
    STOP_SOURCE_TRUNCATION,
    STOP_ZERO_GRADER_DROP,
)
from flywheel_orchestrator._sources import (
    StopEventSink,
    WorkItem,
    WorkReport,
    WorkSourceError,
)
from flywheel_orchestrator._triage_receipt import (
    RECEIPT_KEY,
    content_hash,
    is_fail_first,
    parse_receipt,
)

# argv after the ``gh`` executable -> stdout. The seam tests inject.
GhRunner = Callable[[Sequence[str]], str]

_SPEC_FENCE_OPEN = "```flywheel"

_SPEC_FENCE_CLOSE = "```"

_LIST_FIELDS = "number,title,body,url"

# gh defaults to 30; one page of 200 keeps a realistic board in one call.
_LIST_LIMIT = "200"


def _default_runner(argv: Sequence[str]) -> str:
    """Run ``gh <argv>`` and return stdout; raise on non-zero exit."""
    proc = subprocess.run(
        ["gh", *argv],
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or "(no output)"
        raise WorkSourceError(
            f"gh {' '.join(argv)} failed (exit {proc.returncode}): {detail}"
        )
    return proc.stdout


def emit_truncation_warning(
    log: Callable[[str], None] | None,
    *,
    source: str,
    what: str,
) -> None:
    """Emit the uniform one-page truncation warning through ``log``.

    The shared seam for every GitHub source: a bounded single-page listing
    that fills its cap signals dropped items rather than silently truncating
    (D-3). ``source`` names the source tag (e.g. ``github``), ``what`` names
    the listed unit. The wording is uniform (``items``, not source-specific
    nouns). A side channel only — a ``log=None`` caller is a no-op and the
    returned work sequence is never affected.
    """
    if log is None:
        return
    log(
        f"[{source}] {what} listing truncated at one page; "
        f"some items were not read this pass"
    )


def _extract_spec_block(body: str, *, source: str) -> dict[str, Any] | None:
    """Parse the issue body's ```` ```flywheel ```` fenced block, if any.

    Returns ``None`` when no block is present. A present-but-invalid block
    raises :class:`WorkSourceError` — a ticket author who wrote a spec
    block meant it, so silent fallback to defaults would mask their error.
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
            except json.JSONDecodeError as exc:
                raise WorkSourceError(
                    f"{source}: invalid JSON in flywheel spec block: {exc}"
                ) from exc
            except RecursionError as exc:
                # A deeply-nested spec block (fully attacker-authored issue
                # body) overflows the C JSON scanner's recursion. RecursionError
                # is a RuntimeError, not JSONDecodeError, so it would otherwise
                # escape past list_work's per-item WorkSourceError guard and
                # blackout the whole board. Report it as an invalid block.
                raise WorkSourceError(
                    f"{source}: flywheel spec block nests too deeply to parse"
                ) from exc
            if not isinstance(data, dict):
                raise WorkSourceError(
                    f"{source}: flywheel spec block must be a JSON object, "
                    f"got {type(data).__name__}"
                )
            return data
        block.append(line)
    raise WorkSourceError(
        f"{source}: flywheel spec block is not closed by a ``` fence"
    )


def _triage_skip_reason(
    spec: dict[str, Any] | None, *, title: str, body: str
) -> str | None:
    """Why the trust rule skips this issue, or ``None`` when it may schedule.

    Encapsulates the drain-side triage-trust gate (the *policy*; the receipt
    schema and content-hash *definition* live in
    :mod:`flywheel_orchestrator._triage_receipt`). Returns a human-readable
    reason string for every failure mode -- no spec block, no receipt in the
    block, a malformed or passing (not fail-first) receipt, or a receipt whose
    ``content_hash`` no longer matches the issue's current human content -- and
    ``None`` only when the block carries a fail-first receipt matching the
    current content hash.
    """
    if spec is None:
        return (
            "trust mode requires a flywheel spec block carrying a triage "
            "receipt, but the issue has none"
        )
    raw = spec.get(RECEIPT_KEY)
    if raw is None:
        return (
            "trust mode requires a triage receipt in the flywheel spec "
            "block, but none is present"
        )
    receipt = parse_receipt(raw)
    if receipt is None:
        return "the flywheel spec block's triage receipt is malformed"
    if not is_fail_first(receipt):
        return (
            "the triage receipt is not fail-first (exit_code 0); it records "
            "a command that already passed, so there is nothing to schedule"
        )
    if receipt.content_hash != content_hash(title, body):
        return (
            "the triage receipt is stale -- its content_hash no longer "
            "matches the issue's current title/body (edited after triage)"
        )
    return None


def _issue_sort_key(issue: dict[str, Any]) -> int:
    """Sort key by issue number, tolerant of malformed payloads.

    Sorting runs before per-item compilation, so a payload with a missing
    or non-integer ``number`` (which :meth:`_compile_issue` will reject and
    skip) must not crash the ordering. Such items sort as ``0``.
    """
    number = issue.get("number")
    return number if isinstance(number, int) else 0


def _context_from_spec(spec: dict[str, Any], *, source: str) -> Context:
    raw = spec.get("context") or {}
    if not isinstance(raw, dict):
        raise WorkSourceError(
            f"{source}: spec 'context' must be an object, "
            f"got {type(raw).__name__}"
        )
    return Context(
        relevant=[str(v) for v in raw.get("relevant") or []],
        references=[str(v) for v in raw.get("references") or []],
        constraints=[str(v) for v in raw.get("constraints") or []],
        non_goals=[str(v) for v in raw.get("non_goals") or []],
        edge_cases=[str(v) for v in raw.get("edge_cases") or []],
        notes=str(raw.get("notes") or ""),
    )


def _issue_number_from_ref(source_ref: str) -> str:
    """``owner/repo#123`` -> ``123`` (the shape this source itself emits)."""
    _, sep, number = source_ref.partition("#")
    if not sep or not number.isdigit():
        raise WorkSourceError(
            f"malformed github source_ref {source_ref!r}; "
            f"expected 'owner/repo#<number>'"
        )
    return number


def _format_report_body(report: WorkReport) -> str:
    lines = [
        f"flywheel: run `{report.run_id}` for task `{report.task_id}` "
        f"finished with status `{report.status.value}`.",
    ]
    if report.error:
        lines.append(f"\nError: {report.error}")
    if report.graders:
        lines.append("\n| ordinal | grader | type | passed |")
        lines.append("| ------- | ------ | ---- | ------ |")
        for receipt in report.graders:
            lines.append(
                f"| {receipt.ordinal} | {receipt.name or '-'} | "
                f"{receipt.grader_type} | "
                f"{'yes' if receipt.passed else 'no'} |"
            )
    else:
        lines.append("\nNo grader receipts were recorded for this run.")
    return "\n".join(lines)


class GithubWorkSource:
    """Labeled open issues in one GitHub repo, compiled to flywheel tasks.

    ``runner`` is the ``gh`` subprocess seam — production uses the real
    CLI, tests inject a callable returning canned stdout. ``log`` (when
    provided) receives one line per skipped issue so non-runnable tickets
    are visible rather than silently absent.

    ``require_triage_receipt`` is the opt-in trust rule (default off, so the
    source is byte-identical to today when absent). When on, an issue is
    scheduled only when its ``flywheel`` spec block carries a triage receipt
    (see :mod:`flywheel_orchestrator._triage_receipt`) whose ``content_hash``
    matches the issue's current human content and whose ``exit_code`` is
    fail-first (non-zero). Every issue that fails that gate — no block, no
    receipt, a malformed or passing receipt, or a hash gone stale after a
    title/body edit — is skipped loudly through ``log`` and produces no
    ``WorkItem``, and the ``[[defaults.graders]]`` fallback deliberately does
    NOT rescue it.
    """

    #: Provenance ``source_kind`` for emitted items and ``source_syncs`` rows.
    source_kind = "github_issue"

    def __init__(
        self,
        *,
        repo: str,
        label: str,
        default_graders: Sequence[Grader] = (),
        done_action: str = "comment",
        require_triage_receipt: bool = False,
        runner: GhRunner | None = None,
        log: Callable[[str], None] | None = None,
        stop_sink: StopEventSink | None = None,
    ) -> None:
        if done_action not in ("comment", "close"):
            raise ValueError(
                f"done_action must be 'comment' or 'close', "
                f"got {done_action!r}"
            )
        self.repo = repo
        self.label = label
        self.default_graders = tuple(default_graders)
        self.done_action = done_action
        self.require_triage_receipt = require_triage_receipt
        self._run = runner if runner is not None else _default_runner
        self._log = log
        self._stop_sink = stop_sink

    @property
    def source_name(self) -> str:
        """The source's locus for ``source_syncs`` (D-4): the ``owner/repo``."""
        return self.repo

    # -- inbound --------------------------------------------------------

    def list_work(self) -> list[WorkItem]:
        stdout = self._run(
            [
                "issue",
                "list",
                "--repo",
                self.repo,
                "--label",
                self.label,
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
            raise WorkSourceError(
                f"gh issue list returned invalid JSON: {exc}"
            ) from exc
        if not isinstance(issues, list):
            raise WorkSourceError(
                f"gh issue list returned {type(issues).__name__}, "
                f"expected a list"
            )
        if len(issues) == int(_LIST_LIMIT):
            emit_truncation_warning(self._log, source="github", what="issue")
            if self._stop_sink is not None:
                self._stop_sink(
                    STOP_SOURCE_TRUNCATION,
                    self.source_name,
                    f"issue listing truncated at one page ({_LIST_LIMIT} "
                    f"items); some items were not read this pass",
                )
        items: list[WorkItem] = []
        skipped: list[WorkSourceError] = []
        for issue in sorted(
            (i for i in issues if isinstance(i, dict)),
            key=_issue_sort_key,
        ):
            try:
                item = self._compile_issue(issue)
            except WorkSourceError as exc:
                # One uncompilable payload must not abort the whole board:
                # count + log the bad ticket (the message carries its
                # source_ref / payload so it is recoverable) and keep going.
                skipped.append(exc)
                if self._log is not None:
                    self._log(f"[github] skipping malformed issue: {exc}")
                continue
            if item is not None:
                items.append(item)
        # A listing that produced no compilable item yet hit malformed
        # payloads is a hard error, not "no work" -- preserve the loud abort
        # so a fully broken board never silently reads as empty.
        if not items and skipped:
            raise skipped[0]
        return items

    def _compile_issue(self, issue: dict[str, Any]) -> WorkItem | None:
        number = issue.get("number")
        if not isinstance(number, int):
            raise WorkSourceError(
                f"github issue payload missing integer 'number': {issue!r}"
            )
        source_ref = f"{self.repo}#{number}"
        title = str(issue.get("title") or "")
        body = str(issue.get("body") or "")
        url = str(issue.get("url") or "")

        # A present-but-invalid block still raises here (unchanged), even in
        # trust mode -- an author who wrote a block meant it.
        spec_block = _extract_spec_block(body, source=source_ref)

        if self.require_triage_receipt:
            reason = _triage_skip_reason(spec_block, title=title, body=body)
            if reason is not None:
                # Loud skip: surfaced through the log seam, no WorkItem, and
                # the [[defaults.graders]] fallback deliberately does not fire.
                if self._log is not None:
                    self._log(f"[github] skipping {source_ref}: {reason}")
                return None

        spec = spec_block or {}

        raw_graders = spec.get("graders")
        if raw_graders is not None:
            try:
                graders = load_graders(raw_graders, source=source_ref)
            except TaskLoadError as exc:
                raise WorkSourceError(str(exc)) from exc
        else:
            graders = list(self.default_graders)
        if not graders:
            if self._log is not None:
                self._log(
                    f"[github] skipping {source_ref}: no graders in the "
                    f"issue's flywheel block and no default grader policy "
                    f"-- not runnable"
                )
            if self._stop_sink is not None:
                self._stop_sink(
                    STOP_ZERO_GRADER_DROP,
                    self.source_name,
                    f"{source_ref}: no graders in the issue's flywheel block "
                    f"and no default grader policy -- not runnable",
                )
            return None

        context = _context_from_spec(spec, source=source_ref)
        if not context.notes and body:
            context.notes = body
        if url and url not in context.references:
            context.references.append(url)

        goal = str(spec.get("goal") or title).strip()
        prerequisites = tuple(
            str(p) for p in (spec.get("prerequisites") or [])
        )
        tags = [str(t) for t in (spec.get("tags") or [])]

        task = Task(
            goal=goal,
            graders=graders,
            id=f"gh-{number}",
            tags=tags,
            context=context,
        )
        try:
            task.validate()
        except ValidationError as exc:
            raise WorkSourceError(f"{source_ref}: {exc}") from exc
        return WorkItem(
            task=task,
            prerequisites=prerequisites,
            source_ref=source_ref,
            source_kind="github_issue",
            source_version=hashlib.sha256(body.encode("utf-8")).hexdigest(),
            source_url=url,
        )

    # -- outbound -------------------------------------------------------

    def report(self, report: WorkReport) -> None:
        number = _issue_number_from_ref(report.source_ref)
        body = _format_report_body(report)
        if report.status == Status.DONE and self.done_action == "close":
            self._run(
                [
                    "issue",
                    "close",
                    number,
                    "--repo",
                    self.repo,
                    "--comment",
                    body,
                ]
            )
            return
        self._run(
            [
                "issue",
                "comment",
                number,
                "--repo",
                self.repo,
                "--body",
                body,
            ]
        )


__all__ = ["GhRunner", "GithubWorkSource", "emit_truncation_warning"]
