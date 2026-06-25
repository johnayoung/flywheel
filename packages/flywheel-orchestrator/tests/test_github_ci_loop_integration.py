"""Loop-integration tests for the GitHub CI work source (spec 00052, layer 2).

Layer 1 (``test_github_ci_source.py``) owns the adapter in isolation: listing,
compilation, keying, provenance, and the unit-level ``report`` shape. This file
owns the adapter *driven through the orchestrator* and *synced through storage*,
proving the four loop-integration criteria:

* #3 -- a CI item reaches DONE only when the operator's graders pass when run
  OUT-OF-BAND by the harness; the source's grade path issues no ``gh`` call that
  reads check/run status as the verdict (D-2).
* #4 -- the headline anti-hack: a committed "fix" that makes CI green by
  disabling the failing CI step (not the code) leaves the out-of-band graders
  failing, so the item does NOT reach DONE.
* #6 -- a SUCCESSFUL list pass that omits a now-fixed item marks it disappeared
  through the existing ``sync_work_source`` path; a FAILED ``list_work`` raises
  ``WorkSourceError`` and marks NOTHING disappeared (D-6, the 00048 posture).
* #8 -- on a terminal outcome the source writes the grader receipts back to the
  run's COMMIT (never an issue mutation) via the injected runner (D-5).

The ``gh`` binary is never invoked: a fake runner returns the canned failed-run
listing and records every argv, the same injectable seam the unit tests use. The
orchestrate loop, the SQLite store, and the command graders are all real, so the
DONE / FAILED verdicts are authentic out-of-band grades.
"""

from __future__ import annotations

import asyncio
import io
import json
from datetime import datetime, timezone
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
from flywheel_orchestrator import (
    GithubCiWorkSource,
    SqliteClaimStore,
    orchestrate,
    sync_work_source,
)

REPO = "octo/widgets"


# --- fakes / helpers --------------------------------------------------------


class _RecordingGh:
    """Scripted gh runner: records every argv and serves the listing.

    ``run list`` calls return the canned failed-run JSON; every other call
    (the ``api`` write-back) returns empty stdout. Recording every argv lets a
    test assert the source NEVER issues a check/run-status read as the grade.
    """

    def __init__(self, runs: list[dict]) -> None:
        self.runs_json = json.dumps(runs)
        self.calls: list[list[str]] = []

    def __call__(self, argv) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["run", "list"]:
            return self.runs_json
        return ""


def _run_payload(
    *,
    workflow: str = "CI",
    branch: str = "main",
    head_sha: str = "abc123",
    conclusion: str = "failure",
    url: str = "https://github.com/octo/widgets/actions/runs/1",
    created_at: str = "2026-06-25T00:00:00Z",
) -> dict:
    return {
        "workflowName": workflow,
        "headBranch": branch,
        "headSha": head_sha,
        "conclusion": conclusion,
        "url": url,
        "databaseId": 1,
        "createdAt": created_at,
        "displayTitle": "Fix the widget",
        "event": "push",
    }


def _ci_source(gh: _RecordingGh, grader_run: str) -> GithubCiWorkSource:
    return GithubCiWorkSource(
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


def _drive(source: GithubCiWorkSource, tmp_path: Path):
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


# --- #3: out-of-band grade is the verdict -----------------------------------


def test_ci_item_reaches_done_on_out_of_band_grader_pass(
    tmp_path: Path,
) -> None:
    # The operator's default grader (run out-of-band by the harness) passes ->
    # the failing-CI item reaches DONE. The grade is the grader, never the
    # GitHub check status.
    gh = _RecordingGh([_run_payload()])
    report = _drive(_ci_source(gh, "true"), tmp_path)

    (run,) = report.runs
    assert run.task_id.startswith("ci-")
    assert run.status is Status.DONE


def test_ci_item_not_done_when_out_of_band_grader_fails(
    tmp_path: Path,
) -> None:
    # The agent reports intent=verify (claims DONE), but the operator's
    # out-of-band grader fails -> NOT DONE. The agent's self-report is never
    # the verdict (D-2 / "agent claims are untrusted").
    gh = _RecordingGh([_run_payload()])
    report = _drive(_ci_source(gh, "false"), tmp_path)

    (run,) = report.runs
    assert run.status is not Status.DONE
    assert run.status is Status.FAILED


def test_grade_path_issues_no_check_status_gh_call(tmp_path: Path) -> None:
    # Across an entire driven lifecycle, the source's only gh calls are the
    # inbound listing (``run list``) and the outbound receipt (``api ...
    # comments``). It NEVER reads a run's conclusion / check status as the
    # verdict (no ``run view`` / ``run watch`` / ``check-runs``) and never
    # triggers a re-run (no ``run rerun``) -- the grade stays out-of-band.
    gh = _RecordingGh([_run_payload()])
    report = _drive(_ci_source(gh, "true"), tmp_path)

    assert report.runs[0].status is Status.DONE
    assert gh.calls, "the source must have talked to gh at least once"
    for argv in gh.calls:
        is_listing = argv[:2] == ["run", "list"]
        is_writeback = argv[0] == "api" and any(
            a.endswith("/comments") for a in argv
        )
        assert is_listing or is_writeback, (
            f"unexpected gh call in the grade path: {argv!r}"
        )
        # No call reads a specific run's status as the verdict or forces a
        # re-run the agent could influence.
        assert "rerun" not in argv
        assert ["run", "view"] != argv[:2]
        assert ["run", "watch"] != argv[:2]
        assert not any("check-runs" in a for a in argv)


# --- #4: disabling the failing CI step is not a fix -------------------------


def test_disabling_ci_step_does_not_reach_done_when_code_unfixed(
    tmp_path: Path,
) -> None:
    # The cheapest CI-fix hack: make the build green by disabling the failing
    # CI step instead of fixing the code. The operator's grader is the test
    # suite (modeled as a check on a code marker), independent of the workflow
    # file. We simulate the hack by creating a "ci step disabled" marker but
    # leaving the code marker absent: the out-of-band grader still fails, so
    # the item does NOT reach DONE.
    code_fixed = tmp_path / "code_fixed"
    ci_step_disabled = tmp_path / "ci_step_disabled"
    ci_step_disabled.write_text("workflow edited to skip the failing step")
    assert not code_fixed.exists()

    gh = _RecordingGh([_run_payload()])
    report = _drive(_ci_source(gh, f"test -f {code_fixed}"), tmp_path)

    (run,) = report.runs
    assert run.status is not Status.DONE, (
        "disabling the CI step must not reach DONE while the code is unfixed"
    )
    assert run.status is Status.FAILED


def test_actually_fixing_the_code_reaches_done(tmp_path: Path) -> None:
    # The discriminating positive of the anti-hack: when the code is genuinely
    # fixed (the marker the out-of-band grader checks exists), the same grader
    # passes and the item reaches DONE -- so the FAILED above is the grade
    # tracking the code, not a grader that can never pass.
    code_fixed = tmp_path / "code_fixed"
    code_fixed.write_text("the real bug is fixed")

    gh = _RecordingGh([_run_payload()])
    report = _drive(_ci_source(gh, f"test -f {code_fixed}"), tmp_path)

    (run,) = report.runs
    assert run.status is Status.DONE


# --- #6: disappearance / failed-listing posture via sync_work_source --------


def _sync(source: GithubCiWorkSource, store: SqliteClaimStore):
    return sync_work_source(
        source,
        store,
        source_kind=source.source_kind,
        source_name=source.source_name,
        now=datetime.now(timezone.utc),
    )


def test_now_passing_ci_item_marked_disappeared_via_sync(
    tmp_path: Path,
) -> None:
    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        # First pass observes the failing run: the item is catalogued and NOT
        # marked disappeared.
        first = _sync(
            _ci_source(_RecordingGh([_run_payload()]), "true"), store
        )
        assert first.status == "ok"
        assert first.observed_count == 1
        (record,) = store.list_work_items()
        ci_id = record.task_id
        assert record.disappeared_at is None

        # Second pass: the job now passes, so the failing-run listing is empty.
        # A SUCCESSFUL empty pass marks the previously-seen item disappeared.
        second = _sync(_ci_source(_RecordingGh([]), "true"), store)
        assert second.status == "ok"
        assert second.observed_count == 0
        gone = store.load_work_item(ci_id)
        assert gone is not None
        assert gone.disappeared_at is not None
    finally:
        store.close()


def test_failed_listing_marks_nothing_disappeared_via_sync(
    tmp_path: Path,
) -> None:
    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        # Seed the catalog with a successful observation.
        _sync(_ci_source(_RecordingGh([_run_payload()]), "true"), store)
        (record,) = store.list_work_items()
        ci_id = record.task_id
        assert record.disappeared_at is None

        # A transient gh listing failure (malformed output -> WorkSourceError)
        # must NOT be read as "all CI work vanished": the sync settles 'error'
        # and marks NOTHING disappeared (the 00048 anti-regression, D-6).
        def _malformed(argv) -> str:
            return "not json at all"

        bad = GithubCiWorkSource(
            repo=REPO,
            default_graders=(CommandGrader(run="true"),),
            runner=_malformed,
        )
        settled = _sync(bad, store)
        assert settled.status == "error"
        assert settled.error
        still_there = store.load_work_item(ci_id)
        assert still_there is not None
        assert still_there.disappeared_at is None
    finally:
        store.close()


# --- #8: terminal-outcome receipt write-back through the driven loop ---------


def test_report_writes_receipts_to_commit_not_issue_on_done(
    tmp_path: Path,
) -> None:
    # Driving a CI item to a terminal outcome projects the grader receipts back
    # onto the run's COMMIT via the injected runner -- never an issue close or
    # comment (a CI run is not an issue; D-5).
    gh = _RecordingGh([_run_payload(head_sha="deadbeef")])
    report = _drive(_ci_source(gh, "true"), tmp_path)
    assert report.runs[0].status is Status.DONE

    writebacks = [c for c in gh.calls if c and c[0] == "api"]
    assert len(writebacks) == 1
    argv = writebacks[0]
    assert "repos/octo/widgets/commits/deadbeef/comments" in argv
    body = argv[argv.index("-f") + 1]
    assert body.startswith("body=")
    assert f"`{report.runs[0].run_id}`" in body
    # The CI write-back never touches an issue object.
    assert not any(c[:1] == ["issue"] for c in gh.calls)
