"""Risk-tiered landing: classify a DONE branch's diff and route its landing.

Spec 00080. With ``[[submit.tiers]]`` configured, the landing strategy applied
to each DONE task is a mechanical function of its diff, not a repo-global
constant: the changed files are classified at submit time against
operator-declared path rules -- tier 0 lands direct (``merge``), tier 1 lands
on the phase integration branch (``phase``, spec 00079), tier 2 goes to a PR
requiring human approval (``pr``). The highest tier of any touched file wins
and an unmatched file defaults to tier 1 (D-3), so a newly invented path is
never a silent direct-merge lane.

Classification is deliberately agent-external: :func:`classify_changed_files`
consumes the :class:`~flywheel_orchestrator.SubmitTierRule` rules the WORKER
process loaded from the operator's checkout, never any copy in the task branch
or worktree -- an agent that rewrites its worktree's ``flywheel.toml`` to
declare everything tier 0 changes nothing, and the policy-file path itself
always classifies at the highest tier (criterion 4). ``protected_paths``
remains the distinct, absolute refuse-list: every routed strategy re-evaluates
it first, so a protected file parks regardless of its computed tier (D-1,
criterion 5).

This module imports :mod:`flywheel_worktree.worker` (like
:mod:`flywheel_worktree.pr` does); the worker imports it lazily at the
dispatch site, so the module cycle stays broken exactly as the submit-strategy
registry does it.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

from flywheel_core.events import LandingRouted
from flywheel_orchestrator import (
    DEFAULT_POLICY_FILENAME,
    SUBMIT_TIER_DEFAULT,
    HeldOutGraderSource,
    SubmitRequest,
    SubmitTierRule,
    WorkPolicy,
)

from flywheel_worktree._submit_registry import SUBMIT_STRATEGIES
from flywheel_worktree.worker import (
    GitError,
    GitWorktreeSubmitter,
    LandingLedger,
    Logger,
    _git,
    phase_of_task_file,
)

#: Tier-to-strategy routing (spec 00080): the registry name each winning tier
#: lands through. The vocabulary is closed -- policy validation pins tiers to
#: this domain -- and the mapping is deliberately not configurable: loosening
#: a route is a spec change, not a knob.
TIER_ROUTE_STRATEGIES: Mapping[int, str] = {0: "merge", 1: "phase", 2: "pr"}

_HIGHEST_TIER: int = max(TIER_ROUTE_STRATEGIES)


@dataclass(frozen=True)
class TierDecision:
    """One classified diff: every changed file's tier and the winning route.

    ``per_file_tiers`` maps each changed file (repo-relative, the same
    merge-base-scoped diff the protected-path gate reads) to its computed
    tier; ``winning_tier`` is the highest of those; ``strategy`` is the
    :data:`TIER_ROUTE_STRATEGIES` registry name the decision routes to.
    """

    per_file_tiers: Mapping[str, int]
    winning_tier: int
    strategy: str


def classify_changed_files(
    files: Sequence[str],
    rules: Sequence[SubmitTierRule],
    *,
    policy_file: str = DEFAULT_POLICY_FILENAME,
) -> TierDecision:
    """Classify ``files`` against ``rules``; highest tier wins (spec 00080).

    Per file: the highest tier of any rule whose glob matches
    (``PurePath.full_match`` semantics, ``**`` crosses directories -- the
    ``protected_paths`` vocabulary); a file matching no rule classifies at
    :data:`~flywheel_orchestrator.SUBMIT_TIER_DEFAULT` (tier 1, D-3). The
    ``policy_file`` path -- the source of the rules themselves -- always
    classifies at the highest tier regardless of what any rule says about it
    (criterion 4): a diff that edits the rules can never use those edits to
    cheapen its own route. ``files`` must be non-empty: an empty diff has no
    tier and the caller routes it to the plain merge no-op path instead.
    """
    if not files:
        raise ValueError("cannot classify an empty changed-file set")
    per_file: dict[str, int] = {}
    for path in files:
        if path == policy_file:
            per_file[path] = _HIGHEST_TIER
            continue
        matched = [
            rule.tier
            for rule in rules
            if any(
                PurePosixPath(path).full_match(pattern)
                for pattern in rule.paths
            )
        ]
        per_file[path] = max(matched) if matched else SUBMIT_TIER_DEFAULT
    winning = max(per_file.values())
    return TierDecision(
        per_file_tiers=per_file,
        winning_tier=winning,
        strategy=TIER_ROUTE_STRATEGIES[winning],
    )


class TierRoutingSubmitter(GitWorktreeSubmitter):
    """Provision like the merge strategy; route each landing by its tier.

    Provisioning, landability, and every non-landing concern inherit from
    :class:`~flywheel_worktree.worker.GitWorktreeSubmitter` unchanged --
    worktrees always fork from the true base, so a diff classifies against
    the base the operator actually reviews against. Only ``_submit`` differs:
    the branch's merge-base-scoped changed files are classified against the
    worker-loaded tier rules, the decision is recorded on the run's ledger as
    a :class:`~flywheel_core.events.LandingRouted` audit witness (before the
    routed strategy runs, so a subsequent park still leaves the decision
    queryable), and the whole submit request is delegated to the winning
    tier's strategy: ``merge`` runs this class's inherited landing, ``phase``
    and ``pr`` run the registry-built delegates. Each delegate re-runs its own
    full verify ladder including the protected-path refusal, so protection
    outranks every tier by construction (criterion 5). A diff that cannot be
    derived fails closed: the raised error parks the run as ``submit-error``
    via the inherited ``submit`` wrapper.

    A tier-1 (phase) route lands a true-base-forked branch onto the phase
    integration branch; when that branch has already advanced, the delegate's
    rebase-once-then-reverify rung carries it, so nothing lands unverified
    against the exact ref it lands on.
    """

    def __init__(
        self,
        *,
        repo_root: Path,
        tasks_dir: Path,
        worktrees_dir: Path,
        phase_base: str,
        lock_path: Path,
        log: Logger,
        tier_rules: Sequence[SubmitTierRule],
        delegates: Mapping[str, GitWorktreeSubmitter],
        policy_file: str = DEFAULT_POLICY_FILENAME,
        protected_paths: Sequence[str] = (),
        setup_command: str | None = None,
        on_done: str = "destroy",
        on_failure: str = "park",
        store: LandingLedger | None = None,
        grader_env: Mapping[str, str] | None = None,
        verify_command: str | None = None,
        held_out_source: HeldOutGraderSource | None = None,
        recovery_agent_max_turns: int = 0,
        recovery_agent_max_wall_seconds: float = 900.0,
        recovery_agent_model: str | None = None,
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
            verify_command=verify_command,
            held_out_source=held_out_source,
            recovery_agent_max_turns=recovery_agent_max_turns,
            recovery_agent_max_wall_seconds=recovery_agent_max_wall_seconds,
            recovery_agent_model=recovery_agent_model,
        )
        self.tier_rules = tuple(tier_rules)
        self.delegates = dict(delegates)
        self.policy_file = policy_file

    def _changed_files(self, branch: str) -> list[str]:
        """Repo-relative paths the branch touches vs the true base.

        The same merge-base-scoped diff (``base...branch``) the protected-path
        gate reads, so classification and protection judge the identical file
        set. Fail closed: an underivable diff raises (the ``submit`` wrapper
        parks the run as ``submit-error``) rather than guessing a route.
        """
        res = _git(
            self.repo_root,
            "diff",
            "--name-only",
            f"{self.phase_base}...{branch}",
        )
        if res.returncode != 0:
            raise GitError(
                f"tier classification cannot derive the changed files of "
                f"{branch}: {res.stderr.strip()}"
            )
        return [path for path in res.stdout.splitlines() if path]

    def _submit(self, req: SubmitRequest) -> None:
        task_id = req.task_id
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        branch = self._branch(task_id, phase)
        changed = self._changed_files(branch)
        if not changed:
            # Nothing to classify: an empty diff is the merge path's
            # no-op/cleanup case, identical to the un-tiered behavior.
            super()._submit(req)
            return
        decision = classify_changed_files(
            changed, self.tier_rules, policy_file=self.policy_file
        )
        self._record_landing_routed(req.run_id, decision)
        self.log(
            f"tier routing {task_id}: {len(changed)} changed file(s), "
            f"winning tier {decision.winning_tier} -> "
            f"{decision.strategy} strategy"
        )
        if decision.strategy in self.delegates:
            self.delegates[decision.strategy]._submit(req)
            return
        super()._submit(req)

    def _record_landing_routed(
        self, run_id: str, decision: TierDecision
    ) -> None:
        """Append the queryable ``LANDING_ROUTED`` audit witness (criterion 7).

        Recorded at classification time -- before the routed strategy runs --
        so the decision outlives a subsequent park. Best-effort exactly like
        :meth:`~flywheel_worktree.worker.GitWorktreeSubmitter._record_landing`:
        a missing store handle or any store error is logged, never raised, and
        the append is guarded by the same disk/inode preflight.
        """
        if self.store is None:
            return

        store = self.store

        def _append() -> None:
            lifecycle = store.load_lifecycle(run_id)
            if lifecycle is None:
                self.log(
                    f"cannot record landing-routed event: no lifecycle for "
                    f"{run_id}"
                )
                return
            store.append_domain_event(
                LandingRouted(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    winning_tier=decision.winning_tier,
                    strategy=decision.strategy,
                    per_file_tiers=dict(decision.per_file_tiers),
                ),
                expected_version=lifecycle.version,
            )

        try:
            self.disk_preflight.guard(self.repo_root, _append, run_id=run_id)
        except Exception as exc:  # noqa: BLE001 - must not escape submit
            self.log(
                f"failed to record landing-routed event for {run_id} "
                f"({type(exc).__name__}: {exc})"
            )


def build_tiered_submitter(
    policy: WorkPolicy,
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
) -> TierRoutingSubmitter:
    """Build the tier-routing submitter for a policy with ``[[submit.tiers]]``.

    Takes the same shared builder arguments the submit-strategy registry
    dispatches on. The ``phase`` and ``pr`` delegates are resolved through
    :data:`~flywheel_worktree._submit_registry.SUBMIT_STRATEGIES` with those
    identical arguments (criterion: routing goes through the existing
    registry), so each routed landing runs the exact strategy an operator
    would have gotten by configuring it repo-globally; the merge route is the
    router's own inherited landing, built with the same ``[submit]`` knobs as
    :func:`~flywheel_worktree.worker.build_merge_submitter`. ``policy`` is
    non-optional: the worker selects this builder only when the loaded policy
    carries tier rules.
    """
    delegates: dict[str, GitWorktreeSubmitter] = {}
    for name in sorted(set(TIER_ROUTE_STRATEGIES.values()) - {"merge"}):
        delegates[name] = SUBMIT_STRATEGIES.resolve(name)(
            policy,
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
    submitter = TierRoutingSubmitter(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
        tier_rules=policy.submit_tiers,
        delegates=delegates,
        protected_paths=protected_paths,
        setup_command=setup_command,
        on_done=on_done,
        on_failure=on_failure,
        store=store,
        grader_env=grader_env,
        verify_command=policy.submit_verify,
        held_out_source=held_out_source,
        recovery_agent_max_turns=policy.submit_recovery_agent_max_turns,
        recovery_agent_max_wall_seconds=(
            policy.submit_recovery_agent_max_wall_seconds
        ),
        recovery_agent_model=policy.model,
    )
    log(
        f"landing strategy: tiered ({len(policy.submit_tiers)} rule(s); "
        f"0=merge 1=phase 2=pr, unmatched=tier {SUBMIT_TIER_DEFAULT})"
    )
    return submitter
