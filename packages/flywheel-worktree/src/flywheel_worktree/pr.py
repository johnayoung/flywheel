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

import subprocess
from pathlib import Path
from typing import Sequence

from flywheel_core import Status
from flywheel_orchestrator import GhRunner, SubmitRequest, WorkPolicy

from flywheel_worktree.worker import (
    GitWorktreeSubmitter,
    Logger,
    _git,
    phase_of_task_file,
)

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
) -> GitPullRequestSubmitter:
    """Build the PR backend (the registry's ``pr`` target).

    Reads the remote and PR base from ``policy`` and logs the resolved
    landing target, matching the shared builder signature the submit-strategy
    registry dispatches on. ``policy`` is non-``None`` here: the worker only
    selects ``pr`` from a policy that set it.
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
        remote=policy.submit_remote,
        pr_base=policy.submit_pr_base,
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
        remote: str = "origin",
        pr_base: str | None = None,
        gh: GhRunner | None = None,
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
            self.log(
                f"Lifecycle {req.status.value}; worktree preserved at "
                f"{worktree}"
            )
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
                f"({push.stderr.strip()}); parking worktree at {worktree}"
            )
            return

        url = self._ensure_pr(branch, req)
        self.log(
            f"Landed {branch} as PR {url} ({commit_count} commit(s)); "
            f"merge is review/CI's call"
        )
        # The remote branch + PR now hold the work; local copies are done.
        # -D, not -d: the branch is deliberately unmerged locally — the
        # push above already succeeded, so the commits are safe remotely.
        _git(self.repo_root, "worktree", "remove", str(worktree))
        _git(self.repo_root, "branch", "-D", branch)

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
