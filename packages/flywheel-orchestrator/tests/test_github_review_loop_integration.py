"""Loop-integration tests for the GitHub review-thread source (spec 00053).

Layer 1 (``test_github_review_source.py``) owns the adapter in isolation:
listing unresolved threads, compilation, keying, provenance, and the unit-level
``report`` shape. This file owns the adapter *driven through the orchestrator*
and *synced through storage*, proving the loop-integration criteria the wiring
task is responsible for:

* #5 -- the headline anti-hack: an UNRESOLVED review thread reaches DONE only
  when the operator's default graders pass when run OUT-OF-BAND by the harness.
  The compiled Task carries no grader derived from ``isResolved``; an unresolved
  thread whose default graders pass still reaches DONE, and the harness issues
  no resolve mutation anywhere on the drive/landing path (D-4, D-5).

The ``gh`` binary is never invoked: a fake runner returns the canned GraphQL
listing of unresolved threads and records every argv, the same injectable seam
the unit tests use. The orchestrate loop, the SQLite store, and the command
graders are all real, so the DONE / FAILED verdicts are authentic out-of-band
grades.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.task import CommandGrader
from flywheel_orchestrator import GithubReviewWorkSource, orchestrate

REPO = "octo/widgets"


# --- fakes / helpers --------------------------------------------------------


class _RecordingGh:
    """Scripted gh runner: records every argv and serves the listing.

    ``api graphql`` calls return the canned unresolved-thread listing; every
    other call (the outbound ``pr comment`` receipt) returns empty stdout.
    Recording every argv lets a test assert the source NEVER issues a
    resolve mutation (``resolveReviewThread``) on the drive/landing path.
    """

    def __init__(self, listing: str) -> None:
        self.listing = listing
        self.calls: list[list[str]] = []

    def __call__(self, argv) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["api", "graphql"]:
            return self.listing
        return ""


def _comment(
    *,
    body: str = "please add a test",
    login: str = "reviewer",
    created_at: str = "2026-06-25T00:00:00Z",
    url: str = "https://github.com/octo/widgets/pull/7#discussion_r1",
) -> dict:
    return {
        "body": body,
        "createdAt": created_at,
        "url": url,
        "author": {"login": login},
    }


def _unresolved_listing(*, pr_number: int = 7, node_id: str = "RT_open") -> str:
    """One open PR with one UNRESOLVED review thread (isResolved=False)."""
    return json.dumps(
        {
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": [
                            {
                                "number": pr_number,
                                "reviewThreads": {
                                    "nodes": [
                                        {
                                            "id": node_id,
                                            "isResolved": False,
                                            "isOutdated": False,
                                            "comments": {
                                                "nodes": [_comment()],
                                                "pageInfo": {
                                                    "hasNextPage": False
                                                },
                                            },
                                        }
                                    ],
                                    "pageInfo": {"hasNextPage": False},
                                },
                            }
                        ],
                        "pageInfo": {"hasNextPage": False},
                    }
                }
            }
        }
    )


def _review_source(gh: _RecordingGh, grader_run: str) -> GithubReviewWorkSource:
    return GithubReviewWorkSource(
        repo=REPO,
        default_graders=(CommandGrader(run=grader_run),),
        runner=gh,
    )


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _verify_result() -> IterationResult:
    """The agent CLAIMS done (intent=verify). The harness, not this claim,
    decides the outcome by running the operator's graders out-of-band."""
    return IterationResult(
        transcript="ok",
        messages=(
            AssistantMessage(
                content=[TextBlock(text="done")],
                model="claude-test",
                stop_reason="end_turn",
                session_id="sess",
                usage=None,
            ),
            ResultMessage(
                subtype="success",
                duration_ms=1,
                duration_api_ms=1,
                is_error=False,
                num_turns=1,
                session_id="sess",
                stop_reason="end_turn",
                total_cost_usd=0.0,
                usage=None,
            ),
        ),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    return _invoke


def _drive(source: GithubReviewWorkSource, tmp_path: Path):
    return asyncio.run(
        orchestrate(
            source=source,
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )


# --- #5: out-of-band grade is the verdict, never thread resolution ----------


def test_unresolved_thread_reaches_done_on_out_of_band_grader_pass(
    tmp_path: Path,
) -> None:
    # The operator's default grader (run out-of-band by the harness) passes ->
    # the UNRESOLVED review-thread item reaches DONE. The grade is the grader,
    # never the thread's isResolved state.
    gh = _RecordingGh(_unresolved_listing())
    report = _drive(_review_source(gh, "true"), tmp_path)

    (run,) = report.runs
    assert run.task_id.startswith("prc-")
    assert run.status is Status.DONE


def test_unresolved_thread_not_done_when_out_of_band_grader_fails(
    tmp_path: Path,
) -> None:
    # The agent reports intent=verify (claims DONE), but the operator's
    # out-of-band grader fails -> NOT DONE. Neither the agent's self-report nor
    # the thread's unresolved state is the verdict (D-4 / agent claims are
    # untrusted).
    gh = _RecordingGh(_unresolved_listing())
    report = _drive(_review_source(gh, "false"), tmp_path)

    (run,) = report.runs
    assert run.status is not Status.DONE
    assert run.status is Status.FAILED


def test_unresolved_thread_driven_to_done_issues_no_resolve_mutation(
    tmp_path: Path,
) -> None:
    # The headline anti-hack: an unresolved thread whose default graders pass is
    # driven all the way to DONE, and across the entire driven lifecycle the
    # source's only gh calls are the inbound GraphQL listing and the outbound
    # PR-comment receipt. It NEVER issues a resolveReviewThread mutation -- the
    # one signal that must stay untrusted is never flipped (D-4, D-5).
    gh = _RecordingGh(_unresolved_listing())
    report = _drive(_review_source(gh, "true"), tmp_path)

    assert report.runs[0].status is Status.DONE
    assert gh.calls, "the source must have talked to gh at least once"
    for argv in gh.calls:
        is_listing = argv[:2] == ["api", "graphql"]
        is_writeback = argv[:2] == ["pr", "comment"]
        assert is_listing or is_writeback, (
            f"unexpected gh call on the drive path: {argv!r}"
        )
        # No call resolves a thread, by GraphQL mutation or REST.
        assert not any("resolveReviewThread" in a for a in argv)
        assert not any("unresolveReviewThread" in a for a in argv)
