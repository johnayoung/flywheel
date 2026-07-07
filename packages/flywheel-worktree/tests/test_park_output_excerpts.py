"""Behavior: a landing parked by the standing build invariant (``[submit]
verify``, spec 00064) or by a failing post-rebase re-verification carries the
deciding check's *output excerpt* in its ``LandingParked`` record, on both the
merge and PR land paths (spec 00074, criteria 8-10, decision D-3).

The excerpt reuses the 00073 shape: a :class:`GateGraderReceipt` per executed
check with a raw output tail capped at
:data:`~flywheel_core.events.GATE_EXCERPT_MAX_BYTES`, stored raw and redacted
only at render time. The discriminator throughout: a park that carried only the
existing fixed ``detail`` message would record *no* receipts, so every test here
asserts on the real check output the check emitted -- content a fixed message
could not contain.

Real git runs against a tmp repo; the recording ledger captures the appended
domain events. The standing gate and re-verify are merge-strategy machinery;
the PR strategy inherits the same ``_record_landing_park`` recorder, so the last
test pins that a park recorded through the PR subclass carries its receipts too.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_core.event_serde import event_kind, event_payload
from flywheel_core.events import (
    GATE_EXCERPT_MAX_BYTES,
    DomainEvent,
    GateGraderReceipt,
    LandingParked,
)
from flywheel_core.redaction import default_policy
from flywheel_core.store_protocols import EventRecord
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree import worker
from flywheel_worktree.pr import GitPullRequestSubmitter

# A github-token-shaped secret and its default-policy placeholder (mirrors the
# orchestrator's redaction fixtures).
_SECRET = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"
_REDACTED_TOKEN = "[REDACTED:github_token]"


# --- git / fixture helpers ----------------------------------------------------


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
    _git(path, "config", "user.email", "park-test@example.com")
    _git(path, "config", "user.name", "park test")
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


class _RecordingLedger:
    """Minimal LandingLedger stub: a non-None lifecycle and a captured event
    list, enough to read back the ``LandingParked`` witness and its receipts."""

    def __init__(self) -> None:
        self.events: list[DomainEvent] = []

    class _Lifecycle:
        version = 0

    def load_lifecycle(self, run_id: str) -> "_RecordingLedger._Lifecycle":
        return self._Lifecycle()

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> "_RecordingLedger._Lifecycle":
        self.events.append(event)
        return self._Lifecycle()

    def parks(self) -> list[LandingParked]:
        return [e for e in self.events if isinstance(e, LandingParked)]


def _submitter(
    repo: Path,
    *,
    verify_command: str | None = None,
    store: _RecordingLedger | None = None,
    protected_paths: tuple[str, ...] = (),
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
        protected_paths=protected_paths,
        store=store,  # type: ignore[arg-type]
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
    tf: Path, task_id: str, sandbox: Path, *, grader_run: str = "true"
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=grader_run)],
        ),
        run_id="run-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def _prepare_with_commit(
    s: worker.GitWorktreeSubmitter, tf: Path, task_id: str, *, filename: str
) -> Path:
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=task_id, task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, filename, "x", f"feat: {filename}")
    return wt


def _advance_base(repo: Path, *, filename: str) -> None:
    """Advance ``main`` out-of-band so a branch forked earlier can no longer
    fast-forward and must take the rebase + re-verify path."""
    (repo / filename).write_text("advanced\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", f"advance base with {filename}")


# --- standing-verify parks carry the check's output -------------------------


def test_standing_verify_clean_ff_park_carries_check_output(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    # The nonce lives in a file the check reads, never in the command string,
    # so a hit proves the *executed output* was captured, not the command echoed.
    s = _submitter(
        repo, verify_command="cat check_output.txt; exit 1", store=ledger
    )
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    nonce = "STANDING_CLEAN_FF_NONCE_5F3"
    _commit(wt, "check_output.txt", nonce, "add standing probe")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt))

    # Parked, base untouched -- and the park record carries the check's output.
    assert _rev(repo, "main") == base_before
    parks = ledger.parks()
    assert len(parks) == 1
    assert parks[0].park_kind == "standing-verify"
    assert len(parks[0].receipts) == 1
    receipt = parks[0].receipts[0]
    assert receipt.passed is False
    assert nonce in receipt.output_excerpt


def test_standing_verify_base_advanced_park_carries_check_output(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(
        repo, verify_command="cat check_output.txt; exit 1", store=ledger
    )
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    nonce = "STANDING_REBASE_NONCE_A17"
    _commit(wt, "check_output.txt", nonce, "add standing probe")
    _advance_base(repo, filename="peer.txt")  # forces FF -> rebase path
    base_advanced = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt))

    # Rebase cleanly, then the post-rebase standing gate refuses and parks with
    # the deciding check's output.
    assert _rev(repo, "main") == base_advanced
    parks = ledger.parks()
    assert len(parks) == 1
    assert parks[0].park_kind == "standing-verify"
    assert nonce in parks[0].receipts[0].output_excerpt


def test_reverify_failure_park_records_and_carries_grader_output(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    # Standing gate unset so the *re-verify* is the sole decider on this path.
    s = _submitter(repo, verify_command=None, store=ledger)
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    nonce = "REVERIFY_GRADER_NONCE_C42"
    _commit(wt, "reverify_output.txt", nonce, "add reverify probe")
    _advance_base(repo, filename="peer.txt")
    base_advanced = _rev(repo, "main")

    # The grader emits the nonce and then fails iff the peer's file is present:
    # it would have passed on the branch's original tree, but the rebase brings
    # peer.txt in, so re-verification against the advanced base fails.
    s.submit(
        _submit_req(
            tf,
            "t1",
            wt,
            grader_run="cat reverify_output.txt; test ! -f peer.txt",
        )
    )

    # A failing re-verify now records a park (previously it parked silently) and
    # carries the deciding grader's output.
    assert _rev(repo, "main") == base_advanced
    assert wt.exists()
    parks = ledger.parks()
    assert len(parks) == 1
    assert parks[0].park_kind == "divergent-base"
    assert len(parks[0].receipts) == 1
    assert nonce in parks[0].receipts[0].output_excerpt


# --- bounding + redaction ----------------------------------------------------


def test_standing_verify_park_output_truncated_to_bound_retains_tail(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(
        repo, verify_command="cat big_output.txt; exit 1", store=ledger
    )
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    tail_nonce = "TRUNCATION_TAIL_NONCE_7A9"
    # Output far exceeds the bound; the tail nonce sits at the very end.
    big = "A" * (GATE_EXCERPT_MAX_BYTES + 1000) + tail_nonce
    _commit(wt, "big_output.txt", big, "add oversize probe")

    s.submit(_submit_req(tf, "t1", wt))

    parks = ledger.parks()
    assert len(parks) == 1
    excerpt = parks[0].receipts[0].output_excerpt
    # Bounded to the cap, yet the *final* content survives the truncation.
    assert len(excerpt.encode("utf-8")) <= GATE_EXCERPT_MAX_BYTES
    assert excerpt.endswith(tail_nonce)
    # The leading padding was dropped -- this is a tail, not a head.
    assert excerpt.count("A") < GATE_EXCERPT_MAX_BYTES + 1000


def test_stored_excerpt_is_raw_and_redacts_at_render(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(repo, verify_command="cat leak.txt; exit 1", store=ledger)
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    _commit(wt, "leak.txt", f"build log with {_SECRET} leaked", "add secret")

    s.submit(_submit_req(tf, "t1", wt))

    parks = ledger.parks()
    assert len(parks) == 1
    event = parks[0]
    # Stored raw: the excerpt persisted verbatim, redaction is not a persist-time
    # concern (spec 00074, D-3 / spec 00073, D-2).
    assert _SECRET in event.receipts[0].output_excerpt

    # Rendered through the standard read-seam redactor, the secret is scrubbed by
    # default while the surrounding output survives.
    record = EventRecord(
        run_id=event.run_id,
        ts=event.ts,
        kind=event_kind(event),
        payload=event_payload(event),
    )
    redacted = default_policy().redact(record)
    excerpt = redacted.payload["receipts"][0]["output_excerpt"]
    assert _SECRET not in excerpt
    assert _REDACTED_TOKEN in excerpt
    assert "build log with" in excerpt
    # Nowhere in the whole rendered payload does the raw secret leak.
    assert _SECRET not in json.dumps(redacted.payload)


# --- non-check parks are unchanged (empty receipts) -------------------------


def test_non_check_park_carries_no_receipts(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(repo, store=ledger, protected_paths=("conftest.py",))
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    _commit(wt, "conftest.py", "tampered", "edit conftest")

    s.submit(_submit_req(tf, "t1", wt))

    # A park no check decided carries its cause in park_kind/detail alone; the
    # receipts stay empty, byte-for-byte the pre-00074 shape.
    parks = ledger.parks()
    assert len(parks) == 1
    assert parks[0].park_kind == "protected-paths"
    assert parks[0].receipts == ()


# --- PR land path: the inherited recorder carries receipts too ---------------


def _pr_submitter(
    repo: Path, store: _RecordingLedger
) -> GitPullRequestSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return GitPullRequestSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        gh=lambda _argv: "",
        store=store,  # type: ignore[arg-type]
    )


def test_pr_submitter_park_carries_receipts_through_inherited_recorder(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _pr_submitter(repo, ledger)

    # The PR strategy inherits GitWorktreeSubmitter._record_landing_park, so a
    # grader-decided park recorded on the PR land path carries the deciding
    # check's output through the same shape as the merge path.
    receipt = GateGraderReceipt(
        grader_name="cat check_output.txt; exit 1",
        passed=False,
        output_excerpt="PR_PATH_CHECK_OUTPUT_NONCE_9B1",
    )
    s._record_landing_park(
        "run-1",
        park_kind="standing-verify",
        detail="standing build invariant failed on the PR land path",
        receipts=(receipt,),
    )

    parks = ledger.parks()
    assert len(parks) == 1
    assert parks[0].park_kind == "standing-verify"
    assert parks[0].receipts == (receipt,)
