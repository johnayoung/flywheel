"""GitHub CI-failure work source — a red build becomes a graded task.

A second GitHub adapter, distinct from :mod:`flywheel_orchestrator._github`
(labeled issues). This one lists a repo's FAILED continuous-integration runs
through the ``gh`` CLI (reusing the same injectable :data:`GhRunner` seam) and
compiles each into a :class:`WorkItem` the orchestrator drives to green. The
two adapters never mix: issues and CI failures have different listing, keying,
and write-back, so they live behind separate ``[source] kind`` selectors
(spec 00052 decision D-1).

Run -> Task compilation:

* ``id`` is ``ci-<digest(workflow, branch)>`` — STABLE for a given
  (workflow, branch) pair across polls, so a persistently-broken branch is
  ONE work item, not a new one every poll/push (D-3). It is NOT keyed on the
  per-run ``databaseId`` (that advances as the head moves, which would queue
  an unbounded stream of duplicates).
* ``graders`` come solely from the operator's policy ``[defaults.graders]``.
  A CI run carries no embedded spec block, so a run that resolves to ZERO
  graders is **not runnable**: it is skipped and logged rather than run
  ungated or graded by a fabricated check the operator never declared (D-4).
  The authoritative "fixed" signal is the harness running those default
  graders out-of-band — never the GitHub check status, never a re-run (D-2).
* ``source_version`` tracks the failing head sha + conclusion, so a failure
  that moves to a new commit is detected even though the item id is stable.
* ``source_url`` locates the run; ``source_kind`` marks the CI provenance.

A single malformed run row (a non-object entry, a missing identity string, a
definition that fails validation) is **skipped, counted, and logged** rather
than aborting the whole listing: one uncompilable run must not hide every
other failing run. The skip is recorded in :attr:`last_skipped_count` and one
``log`` line names the offending run. But a parse break must never masquerade
as "CI is green": if the page held rows yet *nothing* compiled because rows
were dropped, ``list_work`` re-raises :class:`WorkSourceError` instead of
returning an empty list.

Outbound: :meth:`GithubCiWorkSource.report` posts the run's grader receipts
back as a comment on the failing commit (never an issue mutation, never a
check-status flip), so a terminal outcome leaves an audit trail on GitHub
(D-5).
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Callable, Sequence
from typing import Any

from flywheel_core.task import Context, Grader, Task, ValidationError

from flywheel_orchestrator._claims import (
    STOP_SOURCE_TRUNCATION,
    STOP_ZERO_GRADER_DROP,
)
from flywheel_orchestrator._github import (
    GhRunner,
    _default_runner,
    _format_report_body,
    emit_truncation_warning,
)
from flywheel_orchestrator._sources import (
    StopEventSink,
    WorkItem,
    WorkReport,
    WorkSourceError,
)

#: gh ``run list`` fields the adapter reads. ``workflowName`` + ``headBranch``
#: form the stable key; ``headSha`` + ``conclusion`` form ``source_version``;
#: ``createdAt`` orders runs so dedup keeps the most recent per key.
_LIST_FIELDS = (
    "databaseId,workflowName,headBranch,headSha,conclusion,"
    "url,displayTitle,event,createdAt"
)

# gh defaults to 20; one page of 200 keeps a realistic backlog in one call.
_LIST_LIMIT = "200"

# The default failure filter: gh's ``--status failure`` selects runs whose
# conclusion is a hard failure. Operators may widen it (e.g. ``timed_out``).
_DEFAULT_FAILURE_FILTER = "failure"


def _ci_item_id(workflow: str, branch: str) -> str:
    """Stable item id for a (workflow, branch) pair (D-3).

    A digest keeps the id well-formed regardless of the characters a
    workflow name or branch ref carries, and stable across polls so a
    persistent failure is one item, not a new id each pass.
    """
    digest = hashlib.sha256(
        f"{workflow}\x00{branch}".encode("utf-8")
    ).hexdigest()
    return f"ci-{digest[:16]}"


def _required_str(run: dict[str, Any], key: str) -> str:
    """Read a required non-empty string field or raise ``WorkSourceError``.

    A run row missing the identity fields the adapter keys on is malformed
    output, not "no work" — surfacing it keeps a parse break from reading as
    a green repo (criterion #9).
    """
    value = run.get(key)
    if not isinstance(value, str) or not value:
        raise WorkSourceError(
            f"gh run list payload missing string {key!r}: {run!r}"
        )
    return value


class GithubCiWorkSource:
    """Failed CI runs in one GitHub repo, compiled to flywheel tasks.

    ``runner`` is the ``gh`` subprocess seam — production uses the real CLI,
    tests inject a callable returning canned stdout. ``failure_filter`` is the
    ``gh run list --status`` value (default ``"failure"``); an operator may
    widen it to capture other failing conclusions. ``log`` (when provided)
    receives one line per skipped run so non-runnable failures are visible.

    A run row that cannot be compiled into a valid :class:`WorkItem` (a
    non-object entry, a missing identity string, a definition that fails
    validation) is skipped rather than aborting the listing: it is counted in
    :attr:`last_skipped_count` and named in one ``log`` line, mirroring
    :class:`~flywheel_orchestrator._sources.DirectoryWorkSource`. The count is
    what lets a reconciler tell a dropped item ("investigate it") from an empty
    source ("no work") -- the two must never look alike.
    """

    #: Provenance ``source_kind`` for emitted items and ``source_syncs`` rows.
    source_kind = "github_ci"

    def __init__(
        self,
        *,
        repo: str,
        default_graders: Sequence[Grader] = (),
        failure_filter: str = _DEFAULT_FAILURE_FILTER,
        runner: GhRunner | None = None,
        log: Callable[[str], None] | None = None,
        stop_sink: StopEventSink | None = None,
    ) -> None:
        if not repo:
            raise ValueError("repo must be a non-empty 'owner/repo' string")
        if not failure_filter:
            raise ValueError("failure_filter must be a non-empty string")
        self.repo = repo
        self.default_graders = tuple(default_graders)
        self.failure_filter = failure_filter
        self._run = runner if runner is not None else _default_runner
        self._log = log
        self._stop_sink = stop_sink
        #: Number of run rows the most recent :meth:`list_work` skipped
        #: because they could not be compiled into a valid item (malformed
        #: output, missing identity field, failed validation). A reconciler
        #: reads this to distinguish "dropped a bad row" from "no work" --
        #: both can yield a short or empty item list, but only a skip is
        #: non-zero. The no-grader readiness gate (D-4) is a policy decision,
        #: not a parse break, so it is logged but not counted here.
        self.last_skipped_count = 0

    @property
    def source_name(self) -> str:
        """The source's locus for ``source_syncs`` (D-6): the ``owner/repo``."""
        return self.repo

    # -- inbound --------------------------------------------------------

    def list_work(self) -> list[WorkItem]:
        stdout = self._run(
            [
                "run",
                "list",
                "--repo",
                self.repo,
                "--status",
                self.failure_filter,
                "--json",
                _LIST_FIELDS,
                "--limit",
                _LIST_LIMIT,
            ]
        )
        try:
            runs = json.loads(stdout or "[]")
        except json.JSONDecodeError as exc:
            raise WorkSourceError(
                f"gh run list returned invalid JSON: {exc}"
            ) from exc
        if not isinstance(runs, list):
            raise WorkSourceError(
                f"gh run list returned {type(runs).__name__}, expected a list"
            )

        # A page filled to the cap may have dropped overflow runs: gh's REST
        # ``run list`` exposes no ``hasNextPage``, so a full page is the only
        # available truncation evidence (D-4). Measured on the raw page, before
        # the (workflow, branch) dedup collapses it.
        if len(runs) == int(_LIST_LIMIT):
            emit_truncation_warning(
                self._log, source="github_ci", what="run"
            )
            if self._stop_sink is not None:
                self._stop_sink(
                    STOP_SOURCE_TRUNCATION,
                    self.source_name,
                    f"run listing truncated at one page ({_LIST_LIMIT} "
                    f"runs); some runs were not read this pass",
                )

        # One uncompilable run row is skipped (counted + logged), not fatal:
        # it must not hide every other failing run. ``skipped`` feeds
        # ``last_skipped_count``; ``first_skip_error`` is held so an
        # all-dropped page re-raises rather than reading as "CI is green".
        skipped = 0
        first_skip_error: WorkSourceError | None = None

        def _skip(error: WorkSourceError) -> None:
            nonlocal skipped, first_skip_error
            skipped += 1
            if first_skip_error is None:
                first_skip_error = error
            if self._log is not None:
                self._log(
                    f"[github_ci] skipping malformed run -- {error}"
                )

        # Dedup to one item per (workflow, branch), keeping the most recent
        # run (D-3): multiple failed runs of the same workflow on the same
        # branch are one persistent failure, not several work items.
        latest: dict[tuple[str, str], dict[str, Any]] = {}
        for run in runs:
            if not isinstance(run, dict):
                _skip(
                    WorkSourceError(
                        f"gh run list entry is {type(run).__name__}, "
                        f"expected an object: {run!r}"
                    )
                )
                continue
            try:
                workflow = _required_str(run, "workflowName")
                branch = _required_str(run, "headBranch")
                _required_str(run, "headSha")
            except WorkSourceError as exc:
                _skip(exc)
                continue
            key = (workflow, branch)
            prior = latest.get(key)
            if prior is None or str(run.get("createdAt") or "") >= str(
                prior.get("createdAt") or ""
            ):
                latest[key] = run

        items: list[WorkItem] = []
        for (workflow, branch), run in sorted(latest.items()):
            try:
                item = self._compile_run(run, workflow=workflow, branch=branch)
            except WorkSourceError as exc:
                _skip(exc)
                continue
            if item is not None:
                items.append(item)

        self.last_skipped_count = skipped

        # A parse break must never masquerade as "CI is green": if the page
        # held rows yet nothing compiled because rows were dropped, surface
        # the first skip's error rather than returning an empty list.
        if not items and first_skip_error is not None:
            raise first_skip_error
        return items

    def _compile_run(
        self, run: dict[str, Any], *, workflow: str, branch: str
    ) -> WorkItem | None:
        head_sha = _required_str(run, "headSha")
        conclusion = str(run.get("conclusion") or "")
        url = str(run.get("url") or "")
        display_title = str(run.get("displayTitle") or "")
        event = str(run.get("event") or "")

        item_id = _ci_item_id(workflow, branch)

        graders = list(self.default_graders)
        if not graders:
            if self._log is not None:
                self._log(
                    f"[github_ci] skipping {item_id} ({workflow} on "
                    f"{branch}): no default grader policy -- not runnable"
                )
            if self._stop_sink is not None:
                self._stop_sink(
                    STOP_ZERO_GRADER_DROP,
                    self.source_name,
                    f"{item_id} ({workflow} on {branch}): no default grader "
                    f"policy -- not runnable",
                )
            return None

        notes_lines = [
            f"CI workflow {workflow!r} is failing on branch {branch!r}.",
            f"Failing head: {head_sha} (conclusion: {conclusion or 'unknown'}).",
        ]
        if display_title:
            notes_lines.append(f"Run title: {display_title}")
        if event:
            notes_lines.append(f"Triggering event: {event}")
        context = Context(
            references=[url] if url else [],
            notes="\n".join(notes_lines),
        )

        goal = (
            f"Fix the failing CI workflow {workflow!r} on branch {branch!r}."
        )
        task = Task(
            goal=goal,
            graders=graders,
            id=item_id,
            context=context,
        )
        try:
            task.validate()
        except ValidationError as exc:
            raise WorkSourceError(f"{item_id}: {exc}") from exc

        return WorkItem(
            task=task,
            source_ref=f"{self.repo}@{head_sha}",
            source_kind=self.source_kind,
            source_version=hashlib.sha256(
                f"{head_sha}\x00{conclusion}".encode("utf-8")
            ).hexdigest(),
            source_url=url,
        )

    # -- outbound -------------------------------------------------------

    def report(self, report: WorkReport) -> None:
        """Post the run's grader receipts as a comment on the failing commit.

        A CI run is not an issue to close (D-5): the audit trail goes onto the
        commit the failure was listed against, parsed back out of the item's
        ``source_ref`` (``owner/repo@<sha>``). No issue is mutated and no check
        status is flipped — the grade stays the harness's out-of-band graders.
        """
        repo, sep, head_sha = report.source_ref.rpartition("@")
        if not sep or not repo or not head_sha:
            raise WorkSourceError(
                f"malformed github_ci source_ref {report.source_ref!r}; "
                f"expected 'owner/repo@<sha>'"
            )
        body = _format_report_body(report)
        self._run(
            [
                "api",
                "--method",
                "POST",
                f"repos/{repo}/commits/{head_sha}/comments",
                "-f",
                f"body={body}",
            ]
        )


__all__ = ["GithubCiWorkSource"]
