"""Tests for risk-tiered landing (spec 00080, criteria 1-5, 7).

Real git against a tmp repo with a local bare remote (pushes are real); the
``gh`` CLI is a recording fake. The discriminators throughout:

* a tier-0 land advances the true base directly and creates no
  ``flywheel/phase/*`` branch and no PR (criterion 1);
* a diff with any tier-2 file routes to a PR -- highest tier wins -- and
  neither the true base nor any phase branch advances (criterion 2);
* an unmatched file classifies at the default tier 1 and lands on the phase
  integration branch, never the base (criterion 3);
* a branch that edits the policy file classifies at the highest tier from the
  WORKER-loaded rules, no matter what the worktree copy declares
  (criterion 4);
* a protected path parks under the existing ``protected-paths`` kind
  regardless of its computed tier -- protection is refusal, not routing
  (criterion 5);
* every routed landing leaves a queryable ``LandingRouted`` witness carrying
  the per-file tiers, winning tier, and chosen strategy (criterion 7).

Criterion 6 (no ``[[submit.tiers]]`` => byte-identical single-strategy
landing) is pinned by the untouched pre-existing strategy suites plus the
worker dispatch selecting the registry path when ``policy.submit_tiers`` is
empty.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from pathlib import Path

import pytest

from flywheel_core import CommandGrader, Grader, Status, Task
from flywheel_core.events import DomainEvent, LandingParked, LandingRouted
from flywheel_orchestrator import (
    SandboxRequest,
    SubmitRequest,
    SubmitTierRule,
    load_policy,
)
from flywheel_worktree import worker
from flywheel_worktree.pr import GitPullRequestSubmitter
from flywheel_worktree.tiering import (
    TIER_ROUTE_STRATEGIES,
    TierRoutingSubmitter,
    build_tiered_submitter,
    classify_changed_files,
)


# --- git helpers (mirror test_phase_strategy_landing.py) ---------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo_with_remote(path: Path) -> Path:
    """Init a work repo plus a bare ``origin`` next to it."""
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "tier-test@example.com")
    _git(path, "config", "user.name", "tier test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    remote = path.parent / "origin.git"
    subprocess.run(
        ["git", "init", "--bare", str(remote)],
        capture_output=True,
        check=True,
    )
    _git(path, "remote", "add", "origin", str(remote))
    return remote


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _branch_exists(repo: Path, branch: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--verify", branch],
            capture_output=True,
        ).returncode
        == 0
    )


def _commit_files(worktree: Path, files: dict[str, str], message: str) -> None:
    for name, body in files.items():
        target = worktree / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


class _FakeGh:
    """Recording ``gh`` runner: ``pr list`` returns nothing, ``pr create``
    returns a canned URL."""

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv: Sequence[str]) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["pr", "create"]:
            return "https://example.test/pr/7\n"
        return ""

    def commands(self) -> list[str]:
        return [" ".join(c[:2]) for c in self.calls]


class _RecordingLedger:
    """Minimal LandingLedger stub capturing every appended domain event."""

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


def _router(
    repo: Path,
    rules: Sequence[SubmitTierRule],
    *,
    gh: _FakeGh | None = None,
    protected_paths: tuple[str, ...] = (),
    store: _RecordingLedger | None = None,
) -> TierRoutingSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    shared = dict(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        protected_paths=protected_paths,
    )
    delegates: dict[str, worker.GitWorktreeSubmitter] = {
        "phase": worker.PhaseBranchSubmitter(
            store=store,  # type: ignore[arg-type]
            **shared,  # type: ignore[arg-type]
        ),
        "pr": GitPullRequestSubmitter(
            gh=gh or _FakeGh(),
            store=store,  # type: ignore[arg-type]
            **shared,  # type: ignore[arg-type]
        ),
    }
    return TierRoutingSubmitter(
        tier_rules=rules,
        delegates=delegates,
        store=store,  # type: ignore[arg-type]
        **shared,  # type: ignore[arg-type]
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
    tf: Path,
    task_id: str,
    sandbox: Path,
    *,
    run_id: str,
    graders: list[Grader] | None = None,
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=(
                graders
                if graders is not None
                else [CommandGrader(run="true")]
            ),
        ),
        run_id=run_id,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _land(
    s: TierRoutingSubmitter,
    repo: Path,
    phase: str,
    task_id: str,
    files: dict[str, str],
    *,
    run_id: str,
) -> None:
    tf = _task_file(repo, phase, task_id)
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=task_id, task_file=tf, run_id=None, mode="fresh")
    )
    _commit_files(wt, files, f"feat: {task_id}")
    s.submit(_submit_req(tf, task_id, wt, run_id=run_id))


# --- the pure classifier ------------------------------------------------------


def test_classify_highest_tier_wins_per_file_and_per_diff() -> None:
    rules = (
        SubmitTierRule(tier=0, paths=("docs/**",)),
        SubmitTierRule(tier=2, paths=("docs/security/**",)),
    )
    decision = classify_changed_files(
        ["docs/a.md", "docs/security/threat.md"], rules
    )
    # Per file: the security doc matches both rules; the higher tier wins.
    assert decision.per_file_tiers == {
        "docs/a.md": 0,
        "docs/security/threat.md": 2,
    }
    # Per diff: the highest file tier wins the route.
    assert decision.winning_tier == 2
    assert decision.strategy == "pr"


def test_classify_unmatched_file_defaults_to_tier_1() -> None:
    rules = (SubmitTierRule(tier=0, paths=("docs/**",)),)
    decision = classify_changed_files(["src/new_lane.py"], rules)
    assert decision.per_file_tiers == {"src/new_lane.py": 1}
    assert decision.winning_tier == 1
    assert decision.strategy == "phase"


def test_classify_policy_file_forced_to_highest_tier() -> None:
    # Even a rule that names the policy file tier 0 cannot cheapen it: the
    # rules' own source always classifies at the highest tier (criterion 4).
    rules = (SubmitTierRule(tier=0, paths=("**",)),)
    decision = classify_changed_files(["flywheel.toml", "docs/a.md"], rules)
    assert decision.per_file_tiers["flywheel.toml"] == max(
        TIER_ROUTE_STRATEGIES
    )
    assert decision.per_file_tiers["docs/a.md"] == 0
    assert decision.strategy == "pr"


def test_classify_empty_file_set_raises() -> None:
    with pytest.raises(ValueError, match="empty"):
        classify_changed_files([], ())


# --- criterion 1: tier 0 lands direct; no phase branch, no PR ----------------


def test_tier0_diff_lands_direct_on_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    s = _router(
        repo, (SubmitTierRule(tier=0, paths=("docs/**",)),), gh=gh
    )

    _land(s, repo, "01-phase", "t-docs", {"docs/a.md": "text"}, run_id="r1")

    tree = _git(repo, "ls-tree", "-r", "--name-only", "main").splitlines()
    assert "docs/a.md" in tree
    assert not _branch_exists(repo, "flywheel/phase/01-phase")
    assert gh.calls == []


# --- criterion 2: any tier-2 file wins; base and phase branch untouched ------


def test_tier2_file_among_tier0_routes_to_pr(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    rules = (
        SubmitTierRule(tier=0, paths=("docs/**",)),
        SubmitTierRule(tier=2, paths=("scripts/**",)),
    )
    s = _router(repo, rules, gh=gh)
    base_before = _rev(repo, "main")

    _land(
        s,
        repo,
        "01-phase",
        "t-mixed",
        {"docs/a.md": "text", "docs/b.md": "text", "scripts/gate.sh": "x"},
        run_id="r1",
    )

    # Highest tier won: a PR was opened; nothing landed locally.
    assert "pr create" in gh.commands()
    assert _rev(repo, "main") == base_before
    assert not _branch_exists(repo, "flywheel/phase/01-phase")


# --- criterion 3: unmatched defaults to tier 1 (phase branch) ----------------


def test_unmatched_diff_lands_on_phase_branch_not_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    s = _router(repo, (SubmitTierRule(tier=0, paths=("docs/**",)),))
    base_before = _rev(repo, "main")

    _land(
        s, repo, "01-phase", "t-new", {"src/new_lane.py": "pass"}, run_id="r1"
    )

    integration = "flywheel/phase/01-phase"
    assert _branch_exists(repo, integration)
    assert _rev(repo, "main") == base_before
    tree = _git(repo, "ls-tree", "-r", "--name-only", integration).splitlines()
    assert "src/new_lane.py" in tree


def test_second_unmatched_task_stacks_via_rebase_rung(tmp_path: Path) -> None:
    # Tier-routed worktrees fork from the true base, so the second task's
    # branch does not contain the advanced integration branch; the phase
    # delegate's rebase-once-then-reverify rung carries it.
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    s = _router(repo, (SubmitTierRule(tier=0, paths=("docs/**",)),))

    _land(s, repo, "01-phase", "t-one", {"src/one.py": "pass"}, run_id="r1")
    _land(s, repo, "01-phase", "t-two", {"src/two.py": "pass"}, run_id="r2")

    integration = "flywheel/phase/01-phase"
    tree = _git(repo, "ls-tree", "-r", "--name-only", integration).splitlines()
    assert "src/one.py" in tree
    assert "src/two.py" in tree


# --- criterion 4: the policy file classifies worker-side, at the top tier ----


def test_policy_file_edit_routes_to_pr_despite_worktree_rules(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    # The WORKER-loaded rules declare everything tier 0 -- the cheapest
    # possible configuration -- and the branch additionally rewrites its
    # worktree policy copy to say the same. Neither cheapens the route: the
    # policy file itself always classifies at the highest tier.
    s = _router(repo, (SubmitTierRule(tier=0, paths=("**",)),), gh=gh)
    base_before = _rev(repo, "main")

    _land(
        s,
        repo,
        "01-phase",
        "t-rules",
        {
            "flywheel.toml": (
                "[[submit.tiers]]\ntier = 0\npaths = [\"**\"]\n"
            ),
            "docs/a.md": "text",
        },
        run_id="r1",
    )

    assert "pr create" in gh.commands()
    assert _rev(repo, "main") == base_before
    assert not _branch_exists(repo, "flywheel/phase/01-phase")


# --- criterion 5: protected paths park regardless of tier --------------------


def test_protected_path_parks_regardless_of_tier(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    store = _RecordingLedger()
    # The same path is both tier 2 and protected: protection is refusal, not
    # routing -- the routed strategy's protected gate parks before any push.
    s = _router(
        repo,
        (SubmitTierRule(tier=2, paths=("ci/**",)),),
        gh=gh,
        protected_paths=("ci/**",),
        store=store,
    )
    base_before = _rev(repo, "main")

    _land(s, repo, "01-phase", "t-ci", {"ci/gate.yml": "x"}, run_id="r1")

    parks = [e for e in store.events if isinstance(e, LandingParked)]
    assert [p.park_kind for p in parks] == ["protected-paths"]
    assert "pr create" not in gh.commands()
    assert _rev(repo, "main") == base_before
    assert not _branch_exists(repo, "flywheel/phase/01-phase")
    # The routing decision itself is still recorded (criterion 7): the park
    # is diagnosable together with the route that led to it.
    routed = [e for e in store.events if isinstance(e, LandingRouted)]
    assert [r.strategy for r in routed] == ["pr"]


# --- criterion 7: the decision is recorded on the run's ledger ---------------


def test_landing_routed_event_recorded_per_tier(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    store = _RecordingLedger()
    rules = (
        SubmitTierRule(tier=0, paths=("docs/**",)),
        SubmitTierRule(tier=2, paths=("scripts/**",)),
    )
    s = _router(repo, rules, gh=gh, store=store)

    _land(s, repo, "01-phase", "t0", {"docs/a.md": "text"}, run_id="r0")
    _land(s, repo, "01-phase", "t1", {"src/x.py": "pass"}, run_id="r1")
    _land(s, repo, "01-phase", "t2", {"scripts/y.sh": "x"}, run_id="r2")

    routed = [e for e in store.events if isinstance(e, LandingRouted)]
    by_run = {e.run_id: e for e in routed}
    assert by_run["r0"].winning_tier == 0
    assert by_run["r0"].strategy == "merge"
    assert by_run["r0"].per_file_tiers == {"docs/a.md": 0}
    assert by_run["r1"].winning_tier == 1
    assert by_run["r1"].strategy == "phase"
    assert by_run["r2"].winning_tier == 2
    assert by_run["r2"].strategy == "pr"


# --- the builder resolves delegates through the registry ---------------------


def test_build_tiered_submitter_from_policy(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    policy_file = tmp_path / "flywheel.toml"
    policy_file.write_text(
        "\n".join(
            [
                "[source]",
                'kind = "directory"',
                "[[submit.tiers]]",
                "tier = 0",
                'paths = ["docs/**"]',
            ]
        )
    )
    policy = load_policy(policy_file)
    logs: list[str] = []
    s = build_tiered_submitter(
        policy,
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=repo / ".flywheel" / "worktrees",
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=logs.append,
        protected_paths=(),
        setup_command=None,
    )
    assert isinstance(s, TierRoutingSubmitter)
    assert s.tier_rules == policy.submit_tiers
    assert isinstance(s.delegates["phase"], worker.PhaseBranchSubmitter)
    assert isinstance(s.delegates["pr"], GitPullRequestSubmitter)
    assert any("tiered" in line for line in logs)
