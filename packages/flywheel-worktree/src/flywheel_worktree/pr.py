"""Pull-request landing strategy: push the task branch, open a PR.

Rung two of the trust ladder (``docs/strategy.md``): nothing merges
locally — review and CI own the merge. Provisioning is identical to the
merge strategy (:class:`~flywheel_worktree.worker.GitWorktreeSubmitter`,
which this subclasses); only the landing differs. On ``done`` the task
branch is pushed to the remote (``--force-with-lease``, covering
rebase-on-retry) and a pull request is opened — or its body refreshed if
one is already open for the branch — with the run's grader receipts
rendered in the body, so reviewers see how "done" was decided before
trusting it. Failed/interrupted work parks exactly as the merge strategy
does; the protected-path gate applies the same (defense in depth even
with a human reviewing).

Selected via ``flywheel.toml``::

    [submit]
    strategy = "pr"        # default "merge"
    remote = "origin"      # push target (default shown)
    # pr_base = "main"     # PR base branch (default: the worker's base)

PR creation shells out to the ``gh`` CLI through the same runner seam the
GitHub work source uses (:data:`flywheel_orchestrator.GhRunner`); tests
inject a fake.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Mapping, Sequence

from flywheel_core import Status, Task
from flywheel_core.events import (
    LANDING_STRATEGY_PR,
    PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_PUSH_FAILED,
    HeldOutGateEvaluated,
)
from flywheel_orchestrator import (
    GhRunner,
    GraderReceipt,
    HeldOutGraderSource,
    SubmitRequest,
    WorkPolicy,
)

from flywheel_worktree.worker import (
    GitWorktreeSubmitter,
    LandingLedger,
    Logger,
    _git,
    phase_of_task_file,
)

if TYPE_CHECKING:
    from flywheel_core.store_postgres import PostgresStore
    from flywheel_core.store_sqlite import SqliteStore

_TITLE_MAX = 90


class GhError(RuntimeError):
    """A ``gh`` invocation the PR strategy expected to succeed did not."""


def _default_gh(argv: Sequence[str]) -> str:
    """Run ``gh <argv>`` and return stdout; raise :class:`GhError` on
    non-zero exit (contained by ``submit``'s never-raise wrapper)."""
    proc = subprocess.run(
        ["gh", *argv],
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or "(no output)"
        raise GhError(
            f"gh {' '.join(argv)} failed (exit {proc.returncode}): {detail}"
        )
    return proc.stdout


def render_pr_body(req: SubmitRequest) -> str:
    """The PR body: goal, run identity, and the grader receipts table.

    The receipts are the harness's verdict, not the agent's claim — the
    body says so explicitly because that distinction is the whole reason
    a reviewer can trust the green.
    """
    lines = [
        "## Task",
        "",
        req.task.goal,
        "",
        "## Run",
        "",
        f"- task: `{req.task_id}`",
        f"- run: `{req.run_id}`",
        f"- terminal status: `{req.status.value}`",
        "",
        "## Grader receipts",
        "",
    ]
    if req.receipts:
        lines += [
            "| # | type | name | verdict |",
            "| - | ---- | ---- | ------- |",
        ]
        lines += [
            f"| {r.ordinal} | {r.grader_type} | {r.name or '-'} | "
            f"{'pass' if r.passed else 'FAIL'} |"
            for r in req.receipts
        ]
    else:
        lines.append("(receipt projection unavailable for this run)")
    lines += [
        "",
        "---",
        "Opened by flywheel. Receipts are the harness's verdict from the "
        "run's final attempt; agent claims are never authoritative.",
    ]
    return "\n".join(lines)


def build_pr_submitter(
    policy: WorkPolicy | None,
    *,
    repo_root: Path,
    tasks_dir: Path,
    worktrees_dir: Path,
    phase_base: str,
    lock_path: Path,
    log: Logger,
    protected_paths: Sequence[str],
    setup_command: str | None,
    on_done: str = "destroy",
    on_failure: str = "park",
    store: LandingLedger | None = None,
    grader_env: Mapping[str, str] | None = None,
    held_out_source: HeldOutGraderSource | None = None,
) -> GitPullRequestSubmitter:
    """Build the PR backend (the registry's ``pr`` target).

    Reads the remote and PR base from ``policy`` and logs the resolved
    landing target, matching the shared builder signature the submit-strategy
    registry dispatches on. ``policy`` is non-``None`` here: the worker only
    selects ``pr`` from a policy that set it. ``grader_env`` flows to the
    inherited submit-time re-verification, matching the merge strategy.
    ``held_out_source`` rides the shared builder signature too; the PR strategy
    lands via review/CI rather than a local merge, so it holds no
    merge-fallback rung to gate, but accepting the argument keeps the registry
    dispatch uniform across strategies.
    """
    assert policy is not None  # selection of "pr" requires a policy
    submitter = GitPullRequestSubmitter(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
        protected_paths=protected_paths,
        setup_command=setup_command,
        on_done=on_done,
        on_failure=on_failure,
        remote=policy.submit_remote,
        pr_base=policy.submit_pr_base,
        store=store,
        grader_env=grader_env,
        held_out_source=held_out_source,
    )
    log(
        f"landing strategy: pr (remote={policy.submit_remote} "
        f"base={policy.submit_pr_base or phase_base})"
    )
    return submitter


class GitPullRequestSubmitter(GitWorktreeSubmitter):
    """Worktree provisioning from the merge strategy; landing via PR."""

    def __init__(
        self,
        *,
        repo_root: Path,
        tasks_dir: Path,
        worktrees_dir: Path,
        phase_base: str,
        lock_path: Path,
        log: Logger,
        protected_paths: Sequence[str] = (),
        setup_command: str | None = None,
        on_done: str = "destroy",
        on_failure: str = "park",
        remote: str = "origin",
        pr_base: str | None = None,
        gh: GhRunner | None = None,
        store: LandingLedger | None = None,
        grader_env: Mapping[str, str] | None = None,
        held_out_source: HeldOutGraderSource | None = None,
    ) -> None:
        super().__init__(
            repo_root=repo_root,
            tasks_dir=tasks_dir,
            worktrees_dir=worktrees_dir,
            phase_base=phase_base,
            lock_path=lock_path,
            log=log,
            protected_paths=protected_paths,
            setup_command=setup_command,
            on_done=on_done,
            on_failure=on_failure,
            store=store,
            grader_env=grader_env,
            held_out_source=held_out_source,
        )
        self.remote = remote
        self.pr_base = pr_base or phase_base
        self._gh = gh or _default_gh

    def _submit(self, req: SubmitRequest) -> None:
        task_id = req.task_id
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        worktree = self._worktree(task_id)
        branch = self._branch(task_id, phase)

        if req.status != Status.DONE:
            self._teardown_on_failure(worktree, branch, req.status)
            return

        porcelain = _git(worktree, "status", "--porcelain").stdout
        if porcelain.strip():
            self.log(
                f"DONE with uncommitted changes on {branch}; parking "
                f"worktree at {worktree}"
            )
            return

        commit_count = self._commit_count(branch)
        if commit_count == 0:
            self.log(
                f"{task_id} reached DONE with no commits beyond "
                f"{self.phase_base}; nothing to submit"
            )
            self._cleanup(worktree, branch)
            return

        violations = self._protected_violations(branch)
        if violations:
            self.log(
                f"{task_id} touches protected path(s) "
                f"{', '.join(violations)}; refusing to open a PR, parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_PROTECTED_PATHS,
                detail=(
                    f"{branch} touches protected path(s) "
                    f"{', '.join(violations)}; refusing to open a PR, worktree "
                    f"preserved at {worktree}"
                ),
            )
            return

        # Stamp harness-authoritative provenance onto the pushed range
        # (``pr_base..branch``) before the push, so the branch on the remote
        # never lacks trailers while a PR references it. Message-only, tree
        # byte-identical, forged ``Flywheel-*`` trailers replaced -- the same
        # shared engine and vocabulary the merge path uses (spec 00078,
        # criterion 2). This precedes both the push and the PR create/edit in
        # ``_ensure_pr``.
        self._stamp_trailers(req, worktree, branch, base=self.pr_base)

        push = _git(
            self.repo_root,
            "push",
            "--force-with-lease",
            self.remote,
            f"{branch}:{branch}",
        )
        if push.returncode != 0:
            self.log(
                f"push of {branch} to {self.remote} failed "
                f"({push.stderr.strip()}); parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_PUSH_FAILED,
                detail=(
                    f"push of {branch} to {self.remote} failed: "
                    f"{push.stderr.strip()}; worktree preserved at {worktree}"
                ),
            )
            return

        try:
            url = self._ensure_pr(branch, req)
        except GhError as exc:
            # A failed PR open is the same class of land-suppression as a failed
            # push: the branch is pushed but the PR never opened, so the work
            # has not landed. Record the reason and leave the worktree parked.
            self.log(
                f"opening a PR for {branch} failed ({exc}); parking worktree "
                f"at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_PUSH_FAILED,
                detail=(
                    f"opening a PR for {branch} failed: {exc}; worktree "
                    f"preserved at {worktree}"
                ),
            )
            return
        self.log(
            f"Landed {branch} as PR {url} ({commit_count} commit(s)); "
            f"merge is review/CI's call"
        )
        # The land completed (branch pushed, PR open): record the landed
        # reference -- the PR identifier -- before tearing down the local copies.
        self._record_landing(
            req.run_id, strategy=LANDING_STRATEGY_PR, landed_ref=url
        )
        # The remote branch + PR now hold the work; local copies are done.
        # -D, not -d (inside _cleanup): the branch is deliberately unmerged
        # locally — the push above already succeeded, so the commits are safe
        # remotely. ``on_done="preserve"`` keeps the local worktree+branch for
        # inspection instead.
        self._teardown_on_done(worktree, branch)

    def _ensure_pr(self, branch: str, req: SubmitRequest) -> str:
        """Open a PR for ``branch``, or refresh the body of the open one."""
        title = f"{req.task_id}: {req.task.goal}"
        if len(title) > _TITLE_MAX:
            title = title[: _TITLE_MAX - 1] + "…"
        body = render_pr_body(req)
        existing = self._gh(
            [
                "pr",
                "list",
                "--head",
                branch,
                "--base",
                self.pr_base,
                "--state",
                "open",
                "--json",
                "url",
                "--jq",
                ".[0].url",
            ]
        ).strip()
        if existing:
            self._gh(["pr", "edit", existing, "--body", body])
            return existing
        return self._gh(
            [
                "pr",
                "create",
                "--head",
                branch,
                "--base",
                self.pr_base,
                "--title",
                title,
                "--body",
                body,
            ]
        ).strip()


# --- phase-branch completion (spec 00079, criterion 3) ------------------------
#
# The per-task PR strategy above lands one task per PR. Under ``[submit]
# strategy = "phase"`` tasks instead land continuously onto a per-phase
# integration branch, and the review unit becomes the whole phase: at phase
# completion the worker pushes that branch and opens exactly one PR whose body
# aggregates every task's receipts and held-out verdict. That completion layer
# lives here (it reuses this module's ``gh`` runner seam and receipt rendering)
# and is injected into the archive sweep as its ``phase_completion`` callable.


def _phase_branch(phase: str) -> str:
    """The integration branch a phase's tasks land on (``flywheel/phase/<p>``)."""
    return f"flywheel/phase/{phase}"


# Held-out verdict wire values (``HeldOutGateEvaluated.outcome``) mapped to
# distinct, human-readable labels for the PR body. ``NO_GATE`` is deliberately
# kept visually apart from ``PASS`` -- a task that registered no held-out grader
# is not the same as one that passed a gate (docs/held-out-gate.md).
_HELD_OUT_LABELS: Mapping[str, str] = {
    "no_gate": "NO_GATE",
    "pass": "PASS",
    "fail": "FAIL",
}


@dataclass(frozen=True)
class PhaseTaskSection:
    """One task's aggregated, store-backed evidence for the phase PR body.

    ``receipts`` is the harness's verdict from the run's final attempt (empty
    when the run never reached grading); ``held_out_outcome`` is the run's
    ``HeldOutGateEvaluated`` wire value (``None`` when no gate evaluation was
    recorded). Both are read from the store, never re-derived from agent output.
    """

    task_id: str
    goal: str
    receipts: tuple[GraderReceipt, ...]
    held_out_outcome: str | None


def _final_receipts(
    store: SqliteStore | PostgresStore, run_id: str
) -> tuple[GraderReceipt, ...]:
    """Project the run's final-attempt grader receipts (the harness's verdicts).

    Mirrors the orchestrator's own work-report projection: the last attempt's
    ``grader_results`` rows flattened into :class:`GraderReceipt` values. A run
    that never reached grading yields an empty tuple.
    """
    attempts = store.list_attempts(run_id)
    if not attempts:
        return ()
    final_number = attempts[-1].number
    return tuple(
        GraderReceipt(
            ordinal=record.ordinal,
            grader_type=str(record.grader_type),
            name=record.grader_name,
            passed=record.passed,
        )
        for record in store.list_grader_results(run_id, final_number)
    )


def _latest_held_out_outcome(
    store: SqliteStore | PostgresStore, run_id: str
) -> str | None:
    """The run's most recent held-out gate outcome wire value, or ``None``.

    Reads the ``HeldOutGateEvaluated`` witnesses off the run's domain-event log
    and returns the last one's ``outcome`` -- the terminal verdict. ``None``
    when the gate recorded no evaluation for the run (distinct from a recorded
    ``no_gate``).
    """
    outcome: str | None = None
    for event in store.list_domain_events(run_id):
        if isinstance(event, HeldOutGateEvaluated):
            outcome = event.outcome
    return outcome


def collect_phase_task_receipts(
    store: SqliteStore | PostgresStore, task: Task
) -> PhaseTaskSection:
    """Aggregate one task's store-backed evidence for the phase PR body.

    Resolves the task's most recent run (``list_lifecycles(task_id=...)`` is
    ordered newest-first) and projects its final receipts and held-out verdict.
    A task with no recorded run yields an empty section -- rendered as the
    receipt-projection-unavailable line rather than omitted.
    """
    lifecycles = store.list_lifecycles(task_id=task.id)
    if not lifecycles:
        return PhaseTaskSection(
            task_id=task.id,
            goal=task.goal,
            receipts=(),
            held_out_outcome=None,
        )
    run_id = lifecycles[0].run_id
    return PhaseTaskSection(
        task_id=task.id,
        goal=task.goal,
        receipts=_final_receipts(store, run_id),
        held_out_outcome=_latest_held_out_outcome(store, run_id),
    )


def _render_phase_task_section(section: PhaseTaskSection) -> list[str]:
    """Render one task's section: goal, held-out verdict, receipts table."""
    if section.held_out_outcome is None:
        label = "not recorded"
    else:
        label = _HELD_OUT_LABELS.get(
            section.held_out_outcome, section.held_out_outcome
        )
    lines = [
        f"## Task `{section.task_id}`",
        "",
        section.goal,
        "",
        f"- held-out gate: `{label}`",
        "",
        "### Grader receipts",
        "",
    ]
    if section.receipts:
        lines += [
            "| # | type | name | verdict |",
            "| - | ---- | ---- | ------- |",
        ]
        lines += [
            f"| {r.ordinal} | {r.grader_type} | {r.name or '-'} | "
            f"{'pass' if r.passed else 'FAIL'} |"
            for r in section.receipts
        ]
    else:
        lines.append("(receipt projection unavailable for this run)")
    lines.append("")
    return lines


def render_phase_pr_body(
    phase: str, sections: Sequence[PhaseTaskSection]
) -> str:
    """The phase PR body: one section per task, receipts + held-out verdict.

    The receipts are the harness's verdicts (never the agent's claim) and the
    held-out outcome is rendered faithfully -- ``NO_GATE`` distinct from
    ``PASS`` -- so a reviewer sees, per task, exactly how "done" was decided
    before trusting the aggregate.
    """
    lines = [
        f"## Phase `{phase}`",
        "",
        f"Lands the phase integration branch `{_phase_branch(phase)}` onto the "
        f"true base. Every task's grader receipts and held-out gate verdict are "
        f"aggregated below; receipts are the harness's verdicts from each run's "
        f"final attempt, never re-derived from agent output.",
        "",
    ]
    for section in sections:
        lines += _render_phase_task_section(section)
    lines += [
        "---",
        "Opened by flywheel. Receipts are the harness's verdicts; agent claims "
        "are never authoritative. Merge is review/CI's call -- the loop opened "
        "and refreshes this PR but never merges it.",
    ]
    return "\n".join(lines)


class PhasePrPublisher:
    """Push a completed phase's branch and ensure exactly one aggregate PR.

    Injected by the worker as the ``phase_completion`` seam of
    :func:`~flywheel_orchestrator.archive_completed_phases` under ``[submit]
    strategy = "phase"``. When the sweep hands over a phase whose tasks are all
    DONE and landed and whose loop-path gate passed, this:

    1. syncs the phase branch with the true base -- one merge of the base
       INTO the phase branch, because a phase PR that conflicts with its base
       runs ZERO GitHub CI (GitHub cannot build the test merge) and stalls
       silently; a base already contained is a no-op, and a conflicting merge
       aborts loudly and opens no PR;
    2. evaluates ``[phase] verify`` against a checkout of the phase-branch tree
       (spec 00079, D-6) -- never the operator's checkout, which does not hold
       the phase's work; after the sync this is the exact merged tree the PR's
       CI will test -- opening no PR on a non-zero exit;
    3. pushes the phase branch to the remote (``--force-with-lease``);
    4. opens exactly one PR onto the true base -- or refreshes the body of the
       one already open for the branch -- with every task's grader receipts and
       held-out verdict aggregated in the body.

    The worker pushes and opens/refreshes only; it never merges the phase PR
    (the phase archives later, once the PR merges -- the dependent task's
    surface). Every failure is logged and surfaced, never raised: a
    phase-completion attempt must never unwind the archive sweep.
    """

    def __init__(
        self,
        *,
        store: SqliteStore | PostgresStore,
        repo_root: Path,
        tasks_dir: Path,
        remote: str,
        pr_base: str,
        phase_verify: str | None,
        log: Logger,
        gh: GhRunner | None = None,
    ) -> None:
        self.store = store
        self.repo_root = repo_root
        self.tasks_dir = tasks_dir
        self.remote = remote
        self.pr_base = pr_base
        self.phase_verify = phase_verify
        self.log = log
        self._gh = gh or _default_gh

    def __call__(self, phase_dir: Path, tasks: list[Task]) -> None:
        phase = phase_dir.name
        branch = _phase_branch(phase)
        try:
            self._publish(phase, branch, tasks)
        except Exception as exc:  # never unwind the sweep (submit discipline)
            self.log(
                f"phase completion for {phase!r} raised ({exc}); the phase is "
                f"left active and its PR not (re)opened"
            )

    def _publish(self, phase: str, branch: str, tasks: list[Task]) -> None:
        synced, detail = self._sync_with_true_base(branch)
        if not synced:
            self.log(
                f"pre-PR sync of {branch} with {self.remote}/{self.pr_base} "
                f"failed ({detail}); opening no PR -- a conflicting phase PR "
                f"runs zero CI and stalls silently -- phase {phase!r} left "
                f"active; merge {self.pr_base} into {branch} by hand to "
                f"unblock"
            )
            return
        if self.phase_verify is not None:
            ok, detail = self._verify_phase_tree(branch)
            if not ok:
                self.log(
                    f"[phase] verify failed against the {branch} tree "
                    f"({detail}); opening no PR, phase {phase!r} left active"
                )
                return
        push = _git(
            self.repo_root,
            "push",
            "--force-with-lease",
            self.remote,
            f"{branch}:{branch}",
        )
        if push.returncode != 0:
            self.log(
                f"push of {branch} to {self.remote} failed "
                f"({push.stderr.strip()}); opening no PR, phase {phase!r} "
                f"left active"
            )
            return
        sections = [collect_phase_task_receipts(self.store, t) for t in tasks]
        body = render_phase_pr_body(phase, sections)
        title = f"Phase {phase}: {len(sections)} task(s)"
        if len(title) > _TITLE_MAX:
            title = title[: _TITLE_MAX - 1] + "…"
        try:
            url = self._ensure_phase_pr(branch, title, body)
        except GhError as exc:
            self.log(
                f"opening/refreshing the phase PR for {branch} failed ({exc}); "
                f"the branch is pushed but no PR is open, phase {phase!r} "
                f"left active"
            )
            return
        self.log(
            f"Phase {phase!r}: pushed {branch} and ensured PR {url} "
            f"({len(sections)} task(s)); merge is review/CI's call"
        )

    def _sync_with_true_base(self, branch: str) -> tuple[bool, str]:
        """Merge the true base into ``branch`` once, before any PR opens.

        GitHub builds a PR's check runs against the test merge of head and
        base; when they conflict there is no test merge, so a conflicting
        phase PR runs ZERO CI and sits silently un-mergeable -- the stall is
        invisible unless someone opens the PR page. Syncing here makes the
        PR mergeable up front (and lets ``[phase] verify`` judge the exact
        merged tree CI will test) or fails LOUD at the seam where the
        operator is already watching.

        The base is the ref the PR actually merges into: ``pr_base`` fetched
        from ``remote``, falling back to the local ``pr_base`` ref when the
        fetch cannot resolve it (offline runs, local-only bases). A base
        already contained in the branch is a no-op -- no merge commit churn,
        no forced CI re-run -- so repeated sweep passes stay idempotent. The
        merge itself runs in a disposable worktree ON the branch (phase
        branches are never checked out, so the branch ref is free); a
        conflict aborts the merge, leaves the branch exactly as it was, and
        reports the conflicted paths.
        """
        fetch = _git(self.repo_root, "fetch", self.remote, self.pr_base)
        if fetch.returncode == 0:
            base_ref = "FETCH_HEAD"
        else:
            probe = _git(
                self.repo_root,
                "rev-parse",
                "--verify",
                "--quiet",
                self.pr_base,
            )
            if probe.returncode != 0:
                return False, (
                    f"cannot resolve the true base {self.pr_base!r} locally "
                    f"or on {self.remote!r} ({fetch.stderr.strip()})"
                )
            base_ref = self.pr_base
        base_sha = _git(self.repo_root, "rev-parse", base_ref).stdout.strip()
        contained = _git(
            self.repo_root, "merge-base", "--is-ancestor", base_sha, branch
        )
        if contained.returncode == 0:
            return True, ""
        parent = Path(tempfile.mkdtemp(prefix="flywheel-phase-sync-"))
        checkout = parent / "tree"
        try:
            add = _git(
                self.repo_root, "worktree", "add", str(checkout), branch
            )
            if add.returncode != 0:
                return False, (
                    add.stderr.strip() or f"could not check out {branch}"
                )
            merge = _git(
                checkout,
                "merge",
                "--no-edit",
                "-m",
                f"merge: sync {branch} with {self.pr_base} before the "
                f"phase PR",
                base_sha,
            )
            if merge.returncode != 0:
                conflicted = _git(
                    checkout, "diff", "--name-only", "--diff-filter=U"
                ).stdout.split()
                _git(checkout, "merge", "--abort")
                what = (
                    f"conflicts in {', '.join(conflicted)}"
                    if conflicted
                    else (merge.stderr.strip() or merge.stdout.strip())
                )
                return False, f"merging {self.pr_base} ({base_sha[:12]}): {what}"
            return True, ""
        finally:
            _git(
                self.repo_root,
                "worktree",
                "remove",
                "--force",
                str(checkout),
            )
            shutil.rmtree(parent, ignore_errors=True)

    def _verify_phase_tree(self, branch: str) -> tuple[bool, str]:
        """Run ``[phase] verify`` against a detached checkout of ``branch``.

        A dedicated worktree of the phase branch is the tree the gate must
        judge (D-6): the operator's checkout does not contain the phase's
        landed work. The worktree is always removed afterwards, pass or fail.
        """
        assert self.phase_verify is not None
        parent = Path(tempfile.mkdtemp(prefix="flywheel-phase-verify-"))
        checkout = parent / "tree"
        try:
            add = _git(
                self.repo_root,
                "worktree",
                "add",
                "--detach",
                str(checkout),
                branch,
            )
            if add.returncode != 0:
                return False, (
                    add.stderr.strip() or f"could not check out {branch}"
                )
            result = subprocess.run(
                self.phase_verify, shell=True, cwd=str(checkout)
            )
            if result.returncode != 0:
                return False, f"exit {result.returncode}"
            return True, ""
        finally:
            _git(
                self.repo_root,
                "worktree",
                "remove",
                "--force",
                str(checkout),
            )
            shutil.rmtree(parent, ignore_errors=True)

    def _ensure_phase_pr(self, branch: str, title: str, body: str) -> str:
        """Open a PR for the phase branch, or refresh the open one's body."""
        existing = self._gh(
            [
                "pr",
                "list",
                "--head",
                branch,
                "--base",
                self.pr_base,
                "--state",
                "open",
                "--json",
                "url",
                "--jq",
                ".[0].url",
            ]
        ).strip()
        if existing:
            self._gh(["pr", "edit", existing, "--body", body])
            return existing
        return self._gh(
            [
                "pr",
                "create",
                "--head",
                branch,
                "--base",
                self.pr_base,
                "--title",
                title,
                "--body",
                body,
            ]
        ).strip()
