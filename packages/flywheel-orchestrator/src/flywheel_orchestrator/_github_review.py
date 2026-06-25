"""GitHub PR review-thread work source — a reviewer's request becomes work.

A third GitHub adapter, distinct from :mod:`flywheel_orchestrator._github`
(labeled issues) and :mod:`flywheel_orchestrator._github_ci` (failed CI runs).
This one lists a repo's open pull requests' UNRESOLVED review threads through
the same injectable :data:`GhRunner` seam — but via ``gh api graphql`` because
the ``isResolved`` thread state the candidate filter depends on is GraphQL-only
(spec 00053 D-2) — and compiles each unresolved thread into a :class:`WorkItem`
the orchestrator drives an agent to address. The three adapters never mix:
issues, CI failures, and review threads have different listing, keying, and
write-back, so each lives behind its own ``[source] kind`` selector (D-1).

Thread -> Task compilation:

* ``id`` is ``prc-<digest(thread_node_id)>`` — STABLE for a given review
  thread across polls and across appended replies (D-3), so one thread is ONE
  work item, not a fresh one every time a reviewer comments. It is NOT keyed on
  a comment id or comment count (those advance on every reply, which would
  re-queue the same concern endlessly).
* ``graders`` come solely from the operator's policy ``[defaults.graders]`` —
  NEVER from the thread's ``isResolved`` state and never compiled from the
  review comment text (D-4). A review comment is subjective and not itself a
  check, so a thread that resolves to ZERO graders is **not runnable**: it is
  skipped and logged rather than run ungated or graded by a fabricated rubric.
  Resolution status is read ONLY to select candidate threads; it is never the
  verdict.
* ``source_version`` digests the thread's comment bodies + ``isResolved`` +
  ``isOutdated`` + latest comment timestamp, so a new reply changes the
  change-token even though the item id is stable (criterion #4).
* ``source_url`` locates the thread; ``source_kind`` marks the review
  provenance; ``source_ref`` encodes ``{repo}#{pr}#{thread_id}`` so the report
  path can recover both the PR (to post on) and the thread (to reference) — D-6.

Malformed ``gh`` output (invalid JSON, a non-object payload, a thread missing
its node id, or a non-zero ``gh`` exit) raises :class:`WorkSourceError` rather
than returning an empty list — a parse break or auth failure must never
masquerade as "every thread is resolved" (criterion #6).

Outbound: :meth:`GithubReviewWorkSource.report` posts the run's grader receipts
as a PR comment via ``gh pr comment`` (the PR parsed back out of
``source_ref``). It issues NO resolve/reply mutation — the harness never
resolves a thread, so the exact state that must stay untrusted cannot be
flipped by the report path (D-5).
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Callable, Sequence
from typing import Any

from flywheel_core.task import Context, Grader, Task, ValidationError

from flywheel_orchestrator._github import (
    GhRunner,
    _default_runner,
    _format_report_body,
)
from flywheel_orchestrator._sources import (
    WorkItem,
    WorkReport,
    WorkSourceError,
)

#: One bounded page per axis. Full multi-page pagination is the deferred
#: issue-hardening thread's job (D-2); here a truncated page is LOGGED, never
#: read as "no work".
_PR_PAGE = 50
_THREAD_PAGE = 50
_COMMENT_PAGE = 50

#: The single GraphQL query: open PRs and their review threads, with each
#: thread's comments (body, author, timestamp, url). ``pageInfo.hasNextPage``
#: on every connection lets the adapter detect — and log — truncation.
_GRAPHQL_QUERY = f"""
query($owner: String!, $name: String!) {{
  repository(owner: $owner, name: $name) {{
    pullRequests(
      states: OPEN
      first: {_PR_PAGE}
      orderBy: {{field: UPDATED_AT, direction: DESC}}
    ) {{
      nodes {{
        number
        reviewThreads(first: {_THREAD_PAGE}) {{
          nodes {{
            id
            isResolved
            isOutdated
            comments(first: {_COMMENT_PAGE}) {{
              nodes {{
                body
                createdAt
                url
                author {{ login }}
              }}
              pageInfo {{ hasNextPage }}
            }}
          }}
          pageInfo {{ hasNextPage }}
        }}
      }}
      pageInfo {{ hasNextPage }}
    }}
  }}
}}
""".strip()


def _review_item_id(thread_node_id: str) -> str:
    """Stable item id for a review thread, keyed on its GraphQL node id (D-3).

    A digest keeps the id well-formed regardless of the node id's characters
    and stable across polls and appended replies, so one thread is one item.
    """
    digest = hashlib.sha256(thread_node_id.encode("utf-8")).hexdigest()
    return f"prc-{digest[:16]}"


def _require_object(value: Any, what: str) -> dict[str, Any]:
    """Read a value that must be a JSON object or raise ``WorkSourceError``.

    A missing or wrong-typed node in the GraphQL response is malformed output,
    not "no work" — surfacing it keeps a parse break from reading as a repo
    with no unresolved threads (criterion #6).
    """
    if not isinstance(value, dict):
        raise WorkSourceError(
            f"gh api graphql payload {what} is "
            f"{type(value).__name__}, expected an object"
        )
    return value


def _nodes(connection: Any, what: str) -> list[Any]:
    """Read a GraphQL connection's ``nodes`` list or raise ``WorkSourceError``."""
    obj = _require_object(connection, what)
    nodes = obj.get("nodes")
    if not isinstance(nodes, list):
        raise WorkSourceError(
            f"gh api graphql payload {what}.nodes is "
            f"{type(nodes).__name__}, expected a list"
        )
    return nodes


def _truncated(connection: dict[str, Any]) -> bool:
    """Whether a GraphQL connection has an unread next page."""
    page = connection.get("pageInfo")
    return isinstance(page, dict) and bool(page.get("hasNextPage"))


class GithubReviewWorkSource:
    """Unresolved PR review threads in one GitHub repo, compiled to tasks.

    ``runner`` is the ``gh`` subprocess seam — production uses the real CLI,
    tests inject a callable returning canned stdout. ``log`` (when provided)
    receives one line per skipped thread (no default grader policy) and one
    line when a listing page truncates, so non-runnable threads and dropped
    work are visible rather than silently absent.
    """

    #: Provenance ``source_kind`` for emitted items and ``source_syncs`` rows.
    source_kind = "github_review"

    def __init__(
        self,
        *,
        repo: str,
        default_graders: Sequence[Grader] = (),
        runner: GhRunner | None = None,
        log: Callable[[str], None] | None = None,
    ) -> None:
        if not repo or "/" not in repo:
            raise ValueError(
                "repo must be a non-empty 'owner/repo' string"
            )
        self.repo = repo
        self.default_graders = tuple(default_graders)
        self._run = runner if runner is not None else _default_runner
        self._log = log

    @property
    def source_name(self) -> str:
        """The source's locus for ``source_syncs`` (D-6): the ``owner/repo``."""
        return self.repo

    # -- inbound --------------------------------------------------------

    def list_work(self) -> list[WorkItem]:
        owner, _, name = self.repo.partition("/")
        stdout = self._run(
            [
                "api",
                "graphql",
                "-f",
                f"query={_GRAPHQL_QUERY}",
                "-F",
                f"owner={owner}",
                "-F",
                f"name={name}",
            ]
        )
        try:
            payload = json.loads(stdout or "{}")
        except json.JSONDecodeError as exc:
            raise WorkSourceError(
                f"gh api graphql returned invalid JSON: {exc}"
            ) from exc

        data = _require_object(payload, "root")
        repository = _require_object(data.get("data"), "data").get("repository")
        repository = _require_object(repository, "data.repository")
        pull_requests = _require_object(
            repository.get("pullRequests"), "data.repository.pullRequests"
        )
        pr_nodes = _nodes(pull_requests, "pullRequests")
        if _truncated(pull_requests):
            self._note_truncation("open pull requests")

        items: list[WorkItem] = []
        for pr in pr_nodes:
            pr_obj = _require_object(pr, "pullRequest")
            number = pr_obj.get("number")
            if not isinstance(number, int):
                raise WorkSourceError(
                    f"gh api graphql pull request missing integer "
                    f"'number': {pr_obj!r}"
                )
            review_threads = _require_object(
                pr_obj.get("reviewThreads"),
                f"pullRequest(#{number}).reviewThreads",
            )
            if _truncated(review_threads):
                self._note_truncation(f"review threads on PR #{number}")
            for thread in _nodes(
                review_threads, f"pullRequest(#{number}).reviewThreads"
            ):
                item = self._compile_thread(thread, pr_number=number)
                if item is not None:
                    items.append(item)
        return items

    def _compile_thread(
        self, thread: Any, *, pr_number: int
    ) -> WorkItem | None:
        thread_obj = _require_object(thread, "reviewThread")
        node_id = thread_obj.get("id")
        if not isinstance(node_id, str) or not node_id:
            raise WorkSourceError(
                f"gh api graphql review thread missing string 'id': "
                f"{thread_obj!r}"
            )

        # Resolution state is the candidate FILTER only, never the verdict
        # (D-4): skip resolved threads, but do not derive a grade from it.
        if bool(thread_obj.get("isResolved")):
            return None

        is_outdated = bool(thread_obj.get("isOutdated"))
        comments = _nodes(
            _require_object(
                thread_obj.get("comments"), "reviewThread.comments"
            ),
            "reviewThread.comments",
        )

        bodies: list[str] = []
        authors: list[str] = []
        timestamps: list[str] = []
        thread_url = ""
        for comment in comments:
            comment_obj = _require_object(comment, "reviewThread.comment")
            body = str(comment_obj.get("body") or "")
            author_obj = comment_obj.get("author")
            login = (
                str(author_obj.get("login") or "")
                if isinstance(author_obj, dict)
                else ""
            ) or "(ghost)"
            bodies.append(body)
            authors.append(login)
            timestamps.append(str(comment_obj.get("createdAt") or ""))
            if not thread_url:
                thread_url = str(comment_obj.get("url") or "")

        item_id = _review_item_id(node_id)

        graders = list(self.default_graders)
        if not graders:
            if self._log is not None:
                self._log(
                    f"[github_review] skipping {item_id} (thread on PR "
                    f"#{pr_number}): no default grader policy -- not runnable"
                )
            return None

        notes_lines = [
            f"Unresolved review thread on pull request #{pr_number} "
            f"in {self.repo}"
            + (" (the diff it annotates is outdated)." if is_outdated else "."),
            "",
            "Reviewer thread (most recent last):",
        ]
        for login, body in zip(authors, bodies):
            notes_lines.append(f"{login}: {body}")
        context = Context(
            references=[thread_url] if thread_url else [],
            notes="\n".join(notes_lines),
        )

        goal = (
            f"Address the unresolved review thread on pull request "
            f"#{pr_number} in {self.repo}."
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

        latest_ts = max(timestamps) if timestamps else ""
        version_seed = "\x00".join(
            [*bodies, str(thread_obj.get("isResolved")), str(is_outdated), latest_ts]
        )
        return WorkItem(
            task=task,
            source_ref=f"{self.repo}#{pr_number}#{node_id}",
            source_kind=self.source_kind,
            source_version=hashlib.sha256(
                version_seed.encode("utf-8")
            ).hexdigest(),
            source_url=thread_url,
        )

    def _note_truncation(self, what: str) -> None:
        if self._log is not None:
            self._log(
                f"[github_review] {what} listing truncated at one page; "
                f"some threads were not read this pass"
            )

    # -- outbound -------------------------------------------------------

    def report(self, report: WorkReport) -> None:
        """Post the run's grader receipts as a PR comment (D-5).

        The PR number is parsed back out of the item's ``source_ref``
        (``owner/repo#<pr>#<thread_id>``); GraphQL node ids contain no ``#``,
        so the delimiter is unambiguous. No resolve/reply mutation is ever
        issued — the harness never flips a thread's resolution state, which is
        the one signal that must stay untrusted.
        """
        repo, pr_number, _ = _parse_review_source_ref(report.source_ref)
        body = _format_report_body(report)
        self._run(
            [
                "pr",
                "comment",
                pr_number,
                "--repo",
                repo,
                "--body",
                body,
            ]
        )


def _parse_review_source_ref(source_ref: str) -> tuple[str, str, str]:
    """``owner/repo#<pr>#<thread_id>`` -> ``(repo, pr, thread_id)`` (D-6)."""
    parts = source_ref.split("#")
    if len(parts) != 3 or not parts[0] or not parts[1].isdigit() or not parts[2]:
        raise WorkSourceError(
            f"malformed github_review source_ref {source_ref!r}; "
            f"expected 'owner/repo#<pr_number>#<thread_node_id>'"
        )
    return parts[0], parts[1], parts[2]


__all__ = ["GithubReviewWorkSource"]
