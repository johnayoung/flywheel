"""Cross-path re-driver invariants (spec 00069, criteria #12/#13/#14).

The individual layer suites each pin one re-drive path in isolation
(``test_redriver_landing`` / ``_escalation`` / ``_no_progress`` / ``_prereq`` /
``_human_gates`` / ``_lease_sweep``). This suite grades the three properties
that only hold *across* every path at once -- the ones a per-layer test can never
prove on its own:

* #13 -- **boundedness of every automatic path.** For a cause that never clears,
  each of the landing re-driver, the retry-escalation, the no-progress back-off,
  and the dangling-prerequisite re-driver makes exactly its configured bound of
  automatic attempts and then produces a queue entry -- never a bound+1 attempt.
  This forecloses the phase's central anti-goal: the re-driver itself becoming
  the new infinite spin.

* #12 -- **one queue, every kind.** A single scenario that routes one unit of
  each kind (#4 landing, #6 escalation, #8 prereq, #9 no-progress, #10 awaiting,
  #11 abort) is listable from ONE ``list_human_review_queue`` read, each entry
  carrying a distinct machine-readable reason, with every routed row living on
  the existing ``orchestrator_stop_events`` ledger and no new silo table created.

* #14 -- **transition discipline.** An AST audit of the re-drive entry points in
  ``_orchestrate.py`` proves none of them forge a lifecycle transition (no
  ``transition_to``, no direct ``create_lifecycle`` / ``update_lifecycle`` write)
  or synthesize an agent envelope (no ``ValidEnvelope`` construction); every path
  requests re-eligibility only through the sanctioned claim / finalize / ledger
  APIs, exactly as ``_recover_claimable_stranded`` does. Paired with the core
  purity holdouts (run as a separate grader), this is the whole of criterion #14.

Nothing here forges lifecycle state: every routed row is produced by driving the
real re-driver entry points over genuine harness/strategy-produced lifecycles.
"""

from __future__ import annotations

import ast
import asyncio
import json
import sqlite3
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    CommandGrader,
    HarnessConfig,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    ManualGrader,
    Status,
    Task,
    ValidEnvelope,
    run_task,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.workflow import run_task_object

import flywheel_orchestrator._orchestrate as orch_mod
from flywheel_orchestrator import (
    DEFAULT_ESCALATION_BOUND,
    REASON_ABORTED,
    REASON_AWAITING_APPROVAL,
    REASON_NO_PROGRESS,
    REASON_PREREQUISITE_MISSING,
    REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    EscalationRequest,
    GraphValidationIssue,
    HumanGateRequest,
    NoProgressObservation,
    SandboxRequest,
    SqliteClaimStore,
    SubmitRequest,
    redrive_exhausted_retries,
    redrive_human_gates,
    redrive_missing_prerequisites,
    redrive_no_progress,
    redrive_parked_landings,
)
from flywheel_worktree import worker

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)

# The landing park cause used throughout: a failed ``[submit] verify`` standing
# invariant. Its park_kind is folded into the human-review reason vocabulary.
_STANDING_VERIFY = "standing-verify"


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


# --- git helpers (mirror test_redriver_landing.py) --------------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


def _seed_done(store: SqliteStore, task_id: str, run_id: str) -> None:
    """A lifecycle finalized ``DONE`` -- the terminal state a parked-unlanded run
    sits in; ``_record_landing_park`` loads it to stamp the park version."""
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=_BASE)
    lc.transition_to(Status.RUNNING, now=_BASE)
    lc.transition_to(Status.VALIDATING, now=_BASE)
    lc.transition_to(Status.DONE, now=_BASE)
    store.create_lifecycle(lc)


def _submitter(
    repo: Path, *, verify_command: str | None, store: SqliteStore
) -> worker.GitWorktreeSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        verify_command=verify_command,
        store=store,
    )


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )
    return tf


def _submit_req(
    tf: Path, task_id: str, run_id: str, sandbox: Path
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id=run_id,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _prepare_with_commit(
    s: worker.GitWorktreeSubmitter, tf: Path, task_id: str, *, filename: str
) -> Path:
    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id=task_id, task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, filename, "x", f"feat: {filename}")
    return wt


def _park_never_clearing_landing(
    repo: Path,
    control: SqliteStore,
    *,
    task_id: str,
    run_id: str,
    filename: str,
) -> tuple[worker.GitWorktreeSubmitter, SubmitRequest]:
    """Seed a DONE run and park it on a standing-verify failure that never
    clears (``verify_command`` stays ``false``)."""
    _seed_done(control, task_id, run_id)
    s = _submitter(repo, verify_command="false", store=control)
    tf = _task_file(repo, "01-phase", task_id)
    wt = _prepare_with_commit(s, tf, task_id, filename=filename)
    req = _submit_req(tf, task_id, run_id, wt)
    s.submit(req)  # original land-suppression -> park #1
    return s, req


# --- scripted-agent helpers (mirror the layer suites) -----------------------


def _signals(cost: float = 0.0) -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=cost,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
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
    )


def _object_result(intent: Intent) -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(),
        failure=None,
    )


def _always(intent: Intent):
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _object_result(intent)

    return _invoke


def _failing_task(task_id: str) -> Task:
    # A verify envelope drives validation; a ``false`` grader always fails it,
    # so the harness's retry walk spends the whole budget -> terminal FAILED.
    return Task(id=task_id, goal=f"Goal for {task_id}.", graders=[CommandGrader(run="false")])


def _drive_object_run(
    task: Task, *, db: Path, sandbox: Path, run_id: str, intent: Intent = Intent.VERIFY
) -> None:
    asyncio.run(
        run_task_object(
            task,
            db_path=db,
            sandbox=sandbox,
            run_id=run_id,
            max_retries=1,
            max_turns=4,
            invoke=_always(intent),
        )
    )


def _escalation_drive(db: Path, tmp_path: Path, escalated_run: str):
    async def _drive(request: EscalationRequest, model: str | None) -> str | None:
        await run_task_object(
            request.task,
            db_path=db,
            sandbox=tmp_path / f"sb-{escalated_run}",
            run_id=escalated_run,
            max_retries=1,
            max_turns=4,
            invoke=_always(Intent.VERIFY),
        )
        return escalated_run

    return _drive


# --- run_task (single-lifecycle) helpers for human gates --------------------


def _iter(intent: Intent, cost: float = 0.0) -> IterationResult:
    return IterationResult(
        transcript="",
        messages=(),
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(cost),
        failure=None,
    )


def _scripted(results: list[IterationResult]):
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return results.pop(0)

    return _invoke


def _make_awaiting(store: SqliteStore, *, task_id: str, run_id: str) -> None:
    # A passing command grader then a manual gate parks AWAITING_APPROVAL.
    lifecycle = Lifecycle(task_id=task_id, run_id=run_id)
    asyncio.run(
        run_task(
            Task(
                goal="g",
                graders=[
                    CommandGrader(run="true"),
                    ManualGrader(instruction="Confirm.", name="operator-confirm"),
                ],
            ),
            lifecycle,
            store,
            invoke=_scripted([_iter(Intent.VERIFY, 0.01)]),
            config=HarnessConfig(max_retries=1),
        )
    )


def _make_abort(store: SqliteStore, *, task_id: str, run_id: str) -> None:
    lifecycle = Lifecycle(task_id=task_id, run_id=run_id)
    asyncio.run(
        run_task(
            Task(goal="g", graders=[CommandGrader(run="true")]),
            lifecycle,
            store,
            invoke=_scripted([_iter(Intent.ABORT)]),
            config=HarnessConfig(max_retries=1),
        )
    )


# --- #13: every automatic re-drive path is bounded --------------------------


def test_landing_redrive_is_bounded(tmp_path: Path) -> None:
    """A landing whose standing invariant never clears makes exactly ``bound``
    automatic land attempts, then a queue entry -- and a further pass makes no
    bound+1 attempt (criterion #13, the landing path)."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        bound = 2
        s, req = _park_never_clearing_landing(
            repo, control, task_id="t1", run_id="run-1", filename="feature.txt"
        )

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=bound,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )
        # Exactly ``bound`` automatic attempts, then routed; base untouched.
        assert [o.result for o in outcomes] == ["queued"]
        assert outcomes[0].attempts == bound
        assert _rev(repo, "main") == base_before
        assert len(claims.list_human_review_queue()) == 1

        # A second pass makes NO further attempt (no bound+1) and does not
        # re-queue: the already-queued guard short-circuits.
        again = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=bound,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )
        assert [o.result for o in again] == ["queued"]
        assert again[0].attempts == 0
        assert len(claims.list_human_review_queue()) == 1
    finally:
        control.close()
        claims.close()


def test_escalation_is_bounded_to_a_single_attempt(tmp_path: Path) -> None:
    """A never-passing task escalates exactly the configured bound of times
    (one), then is routed -- a further pass makes no second escalation
    (criterion #13, the escalation path)."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        assert DEFAULT_ESCALATION_BOUND == 1  # the configured bound is finite
        task = _failing_task("t1")
        _drive_object_run(task, db=db, sandbox=tmp_path / "sb-1", run_id="run-1")

        drive = _escalation_drive(db, tmp_path, "run-esc-1")
        first = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[EscalationRequest(task_id="t1", task=task, run_id="run-1")],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert [o.result for o in first] == ["escalated"]
        assert first[0].escalations == DEFAULT_ESCALATION_BOUND

        # The escalated run also exhausted -> routed once with the reason token.
        second = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[
                    EscalationRequest(task_id="t1", task=task, run_id="run-esc-1")
                ],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert [o.result for o in second] == ["queued"]
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION

        # A third pass over the queued task makes no further escalation.
        third = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[
                    EscalationRequest(task_id="t1", task=task, run_id="run-esc-1")
                ],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert [o.result for o in third] == ["queued"]
        assert third[0].escalations == DEFAULT_ESCALATION_BOUND
        assert len(claims.list_human_review_queue()) == 1
    finally:
        control.close()
        claims.close()


def test_no_progress_backoff_is_bounded(tmp_path: Path) -> None:
    """A unit that never progresses records exactly ``bound`` witnesses and
    exactly one queue entry -- no infinite re-attempt spin (criterion #13, the
    no-progress path)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 3
        results = [
            redrive_no_progress(
                claims,
                observations=[NoProgressObservation(unit_id="U", progressed=False)],
                bound=bound,
                now=_frozen(_BASE),
            )[0].result
            for _ in range(bound)
        ]
        # Exactly ``bound`` automatic cycles, the last of which routes.
        assert results == ["waiting"] * (bound - 1) + ["queued"]
        assert len(claims.list_human_review_queue()) == 1

        # Well past the bound: no further witnessing, no re-queue.
        for _ in range(3):
            redrive_no_progress(
                claims,
                observations=[NoProgressObservation(unit_id="U", progressed=False)],
                bound=bound,
                now=_frozen(_BASE),
            )
        assert len(claims.list_human_review_queue()) == 1
    finally:
        claims.close()


def test_prereq_redrive_is_bounded(tmp_path: Path) -> None:
    """A prerequisite that never appears costs exactly ``bound`` dangling
    witnesses and exactly one queue entry -- no infinite ineligible spin
    (criterion #13, the prerequisite path)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 3
        results = [
            redrive_missing_prerequisites(
                claims,
                issues=[GraphValidationIssue(referencing_id="B", missing_id="A")],
                bound=bound,
                now=_frozen(_BASE),
            )[0].result
            for _ in range(bound)
        ]
        assert results == ["waiting"] * (bound - 1) + ["queued"]
        assert len(claims.list_human_review_queue()) == 1

        # Well past the bound: still exactly one queue entry.
        for _ in range(3):
            redrive_missing_prerequisites(
                claims,
                issues=[GraphValidationIssue(referencing_id="B", missing_id="A")],
                bound=bound,
                now=_frozen(_BASE),
            )
        assert len(claims.list_human_review_queue()) == 1
    finally:
        claims.close()


# --- #12: one queue read returns one unit of every routed kind --------------


def test_all_routed_kinds_listable_from_one_queue_read(tmp_path: Path) -> None:
    """A single scenario routes one unit of each kind (#4/#6/#8/#9/#10/#11);
    ONE ``list_human_review_queue`` read returns all of them, each with a
    distinct machine-readable reason, over the existing stop-event ledger with
    no new silo table (criterion #12)."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        # #4 -- a landing whose standing invariant never clears.
        s, req = _park_never_clearing_landing(
            repo, control, task_id="land", run_id="run-land", filename="feature.txt"
        )
        redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=2,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # #6 -- retries exhausted after the single escalation.
        esc_task = _failing_task("esc")
        _drive_object_run(
            esc_task, db=db, sandbox=tmp_path / "sb-esc", run_id="run-esc"
        )
        drive = _escalation_drive(db, tmp_path, "run-esc-1")
        asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[
                    EscalationRequest(task_id="esc", task=esc_task, run_id="run-esc")
                ],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[
                    EscalationRequest(task_id="esc", task=esc_task, run_id="run-esc-1")
                ],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )

        # #10 -- an intentional AWAITING_APPROVAL gate.
        _make_awaiting(control, task_id="gated", run_id="run-gate")
        redrive_human_gates(
            control,
            claims,
            requests=[HumanGateRequest(task_id="gated", run_id="run-gate")],
            now=_frozen(_BASE),
        )

        # #11 -- an intentional abort.
        _make_abort(control, task_id="aborted", run_id="run-abort")
        redrive_human_gates(
            control,
            claims,
            requests=[HumanGateRequest(task_id="aborted", run_id="run-abort")],
            now=_frozen(_BASE),
        )

        # #8 -- a prerequisite that stays missing past the bound.
        for _ in range(2):
            redrive_missing_prerequisites(
                claims,
                issues=[GraphValidationIssue(referencing_id="B", missing_id="A")],
                bound=2,
                now=_frozen(_BASE),
            )

        # #9 -- a unit that makes no progress past the bound.
        for _ in range(2):
            redrive_no_progress(
                claims,
                observations=[NoProgressObservation(unit_id="U", progressed=False)],
                bound=2,
                now=_frozen(_BASE),
            )

        # ONE read returns one unit of every routed kind, distinct reasons.
        queue = claims.list_human_review_queue()
        by_task = {e.task_id: e for e in queue}
        assert by_task["land"].reason == _STANDING_VERIFY
        assert by_task["esc"].reason == REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION
        assert by_task["gated"].reason == REASON_AWAITING_APPROVAL
        assert by_task["aborted"].reason == REASON_ABORTED
        assert by_task["B"].reason == REASON_PREREQUISITE_MISSING
        assert by_task["U"].reason == REASON_NO_PROGRESS

        reasons = [e.reason for e in queue]
        assert len(queue) == 6
        assert len(set(reasons)) == 6  # every routed reason is distinct
        assert set(reasons) == {
            _STANDING_VERIFY,
            REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
            REASON_AWAITING_APPROVAL,
            REASON_ABORTED,
            REASON_PREREQUISITE_MISSING,
            REASON_NO_PROGRESS,
        }
        # Every entry carries task identity and a human-readable detail.
        assert all(e.task_id and e.detail for e in queue)
    finally:
        control.close()
        claims.close()

    # The queue is a routed read over the existing ledger, not a new silo: no
    # dedicated queue table exists, and every routed row lives on
    # ``orchestrator_stop_events`` (criterion #12).
    conn = sqlite3.connect(db)
    try:
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        for forbidden in (
            "human_review_queue",
            "review_queue",
            "redriver_queue",
            "human_review",
        ):
            assert forbidden not in tables
        assert "orchestrator_stop_events" in tables
        (count,) = conn.execute(
            "SELECT COUNT(*) FROM orchestrator_stop_events WHERE kind IN "
            "(?, ?, ?, ?, ?, ?)",
            (
                _STANDING_VERIFY,
                REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
                REASON_AWAITING_APPROVAL,
                REASON_ABORTED,
                REASON_PREREQUISITE_MISSING,
                REASON_NO_PROGRESS,
            ),
        ).fetchone()
        assert count == 6
    finally:
        conn.close()


# --- #14: the re-drive paths never forge a lifecycle transition -------------

# The sanctioned re-eligibility surface: a re-driver may only append to the
# claim/stop-event ledger or finalize through the harness's own path. It may
# NEVER write a lifecycle status directly or synthesize an agent envelope.
_REDRIVE_ENTRY_POINTS = frozenset(
    {
        "sweep_expired_leases",
        "redrive_parked_landings",
        "redrive_exhausted_retries",
        "redrive_missing_prerequisites",
        "redrive_no_progress",
        "redrive_human_gates",
    }
)
# Call-attribute names that would forge a transition / status write.
_FORBIDDEN_ATTRS = frozenset(
    {"transition_to", "create_lifecycle", "update_lifecycle", "save_lifecycle"}
)
# Constructors that would synthesize an agent claim / envelope.
_FORBIDDEN_NAMES = frozenset({"ValidEnvelope"})
# At least one of these sanctioned surfaces must carry each path's routing: the
# ledger append (``record_*``), the harness's own finalize, or a claim API
# (acquire / batch-reap). ``sweep_expired_leases`` delegates its finalize to the
# canonical ``_recover_claimable_stranded`` helper and reaps via the claim
# store's ``sweep_expired_claims`` -- both sanctioned, neither a status forge.
_SANCTIONED_ATTRS = frozenset(
    {
        "record_human_review",
        "record_stop_event",
        "finalize_stranded_lifecycle",
        "acquire_claim",
        "sweep_expired_claims",
    }
)


def _redrive_function_nodes() -> dict[str, ast.AST]:
    source = Path(orch_mod.__file__).read_text()
    tree = ast.parse(source)
    found: dict[str, ast.AST] = {}
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.name in _REDRIVE_ENTRY_POINTS:
                found[node.name] = node
    return found


def _called_attrs(fn: ast.AST) -> set[str]:
    attrs: set[str] = set()
    for node in ast.walk(fn):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            attrs.add(node.func.attr)
    return attrs


def _constructed_names(fn: ast.AST) -> set[str]:
    names: set[str] = set()
    for node in ast.walk(fn):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            names.add(node.func.id)
    return names


def test_every_redrive_entry_point_is_present_for_audit() -> None:
    # The audit is only meaningful if every named re-drive path actually exists
    # in the module -- a renamed/removed path must fail loudly here, never pass
    # by simply not being found.
    found = _redrive_function_nodes()
    assert set(found) == _REDRIVE_ENTRY_POINTS


def test_redrive_paths_never_forge_a_transition_or_envelope() -> None:
    """AST audit (criterion #14): no re-drive entry point calls a lifecycle
    transition / status write or constructs an agent envelope; each routes only
    through the sanctioned claim / finalize / ledger APIs, as
    ``_recover_claimable_stranded`` does."""
    for name, fn in _redrive_function_nodes().items():
        called = _called_attrs(fn)
        constructed = _constructed_names(fn)
        forbidden_calls = called & _FORBIDDEN_ATTRS
        forbidden_ctors = constructed & _FORBIDDEN_NAMES
        assert not forbidden_calls, (
            f"{name} forges a lifecycle transition via {sorted(forbidden_calls)}"
        )
        assert not forbidden_ctors, (
            f"{name} synthesizes an agent envelope via {sorted(forbidden_ctors)}"
        )
        # Positively: the path routes through a sanctioned surface.
        assert called & _SANCTIONED_ATTRS, (
            f"{name} routes through no sanctioned claim/finalize/ledger API"
        )
