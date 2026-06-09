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

import json
import subprocess
from collections.abc import Callable, Sequence
from typing import Any

from flywheel.lifecycle import Status
from flywheel.loaders import TaskLoadError, load_graders
from flywheel.task import Context, Grader, Task, ValidationError

from flywheel_orchestrator._sources import (
    WorkItem,
    WorkReport,
    WorkSourceError,
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
    """

    def __init__(
        self,
        *,
        repo: str,
        label: str,
        default_graders: Sequence[Grader] = (),
        done_action: str = "comment",
        runner: GhRunner | None = None,
        log: Callable[[str], None] | None = None,
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
        self._run = runner if runner is not None else _default_runner
        self._log = log

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
        items: list[WorkItem] = []
        for issue in sorted(
            (i for i in issues if isinstance(i, dict)),
            key=lambda i: int(i.get("number", 0)),
        ):
            item = self._compile_issue(issue)
            if item is not None:
                items.append(item)
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

        spec = _extract_spec_block(body, source=source_ref) or {}

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


__all__ = ["GhRunner", "GithubWorkSource"]
