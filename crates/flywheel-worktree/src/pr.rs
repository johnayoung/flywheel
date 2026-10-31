//! Pull-request landing strategy: push the task branch, open a PR.
//!
//! Rung two of the trust ladder (`docs/strategy.md`): nothing merges
//! locally -- review and CI own the merge. Provisioning is identical to the
//! merge strategy ([`GitWorktreeSubmitter`], which this wraps); only the
//! landing differs. On `done` the task branch is pushed to the remote
//! (`--force-with-lease`, covering rebase-on-retry and a remote branch deleted
//! after its PR merged) and a pull request is
//! opened -- or its body refreshed if one is already open for the branch --
//! with the run's grader receipts rendered in the body, so reviewers see how
//! "done" was decided before trusting it. Failed/interrupted work parks
//! exactly as the merge strategy does; the protected-path gate applies the
//! same (defense in depth even with a human reviewing).
//!
//! Selected via `flywheel.toml`:
//!
//! ```toml
//! [submit]
//! strategy = "pr"        # default "merge"
//! remote = "origin"      # push target (default shown)
//! # pr_base = "main"     # PR base branch (default: the worker's base)
//! ```
//!
//! PR creation shells out to the `gh` CLI through the same runner seam the
//! GitHub work source uses ([`GhRunner`]); tests inject a scripted runner.
//!
//! The second half of this module is the phase-branch completion layer
//! (spec 00079, criterion 3): [`PhasePrPublisher`] syncs a completed phase's
//! integration branch with the true base, pushes it, and ensures exactly one
//! aggregate PR whose body carries every task's receipts and held-out
//! verdict. It plugs into the archive sweep's `phase_completion` seam
//! (`crate::phase::ArchiveOptions`).
//!
//! Port of the Python `flywheel_worktree.pr` module. Deviations: the pr
//! strategy records the same `uncommitted-work` park witness the merge
//! strategy does (the Python only logged it), and provenance trailers are
//! stamped over the task's own commits (`<landing base>..<branch>`, the same
//! range the commit count and protected-path gate judge) rather than
//! `pr_base..branch`, which rewrote commits outside the task when `pr_base`
//! differed from the worker's base.

use crate::phase::phase_branch;
use crate::submit_registry::{
    PR_STRATEGY, SubmitBuildRequest, SubmitRegistryError, SubmitStrategyRegistry,
    SubmitStrategySpec,
};
use crate::submitter::{
    GitError, GitOutput, GitWorktreeSubmitter, Logger, SubmitError, build_merge_submitter, git,
};
use flywheel_core::events::{
    DomainEventPayload, LANDING_STRATEGY_PR, Landed, LandingParked, PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_PUSH_FAILED, PARK_KIND_SUBMIT_ERROR, PARK_KIND_UNCOMMITTED_WORK,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{
    AttemptStore, DomainEventStore, GraderResultStore, LifecycleFilter, LifecycleStore, StoreError,
};
use flywheel_core::task::Task;
use flywheel_orchestrator::github::{GhRunner, ProcessGhRunner, run_gh};
use flywheel_orchestrator::sources::GraderReceipt;
use flywheel_orchestrator::strategy::{
    LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    SubmitStrategy,
};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// The longest PR title the strategy emits; longer titles are cut to
/// `TITLE_MAX - 1` characters plus an ellipsis.
pub const TITLE_MAX: usize = 90;

/// A `gh` invocation the PR strategy expected to succeed did not. Carries
/// the branded `gh <argv> failed (exit N): <detail>` message [`run_gh`]
/// produces.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GhError(pub String);

/// Run `gh <argv>` through `runner` and return stdout.
fn gh(runner: &dyn GhRunner, argv: &[&str]) -> Result<String, GhError> {
    run_gh(runner, argv).map_err(|err| GhError(err.to_string()))
}

/// The PR body: goal, run identity, and the grader receipts table.
///
/// The receipts are the harness's verdict, not the agent's claim -- the body
/// says so explicitly because that distinction is the whole reason a
/// reviewer can trust the green. Byte-identical to the Python rendering.
pub fn render_pr_body(req: &SubmitRequest) -> String {
    let mut lines: Vec<String> = vec![
        "## Task".into(),
        String::new(),
        req.task.goal.clone(),
        String::new(),
        "## Run".into(),
        String::new(),
        format!("- task: `{}`", req.task_id),
        format!("- run: `{}`", req.run_id),
        format!("- terminal status: `{}`", req.status),
        String::new(),
        "## Grader receipts".into(),
        String::new(),
    ];
    push_receipt_table(&mut lines, &req.receipts);
    lines.push(String::new());
    lines.push("---".into());
    lines.push(
        "Opened by flywheel. Receipts are the harness's verdict from the run's final attempt; \
         agent claims are never authoritative."
            .into(),
    );
    lines.join("\n")
}

/// The receipts table (or the unavailable line when there are none).
fn push_receipt_table(lines: &mut Vec<String>, receipts: &[GraderReceipt]) {
    if receipts.is_empty() {
        lines.push("(receipt projection unavailable for this run)".into());
        return;
    }
    lines.push("| # | type | name | verdict |".into());
    lines.push("| - | ---- | ---- | ------- |".into());
    for r in receipts {
        lines.push(format!(
            "| {} | {} | {} | {} |",
            r.ordinal,
            r.grader_type,
            r.name.as_deref().unwrap_or("-"),
            if r.passed { "pass" } else { "FAIL" }
        ));
    }
}

/// Cut `title` to [`TITLE_MAX`] characters, ending in an ellipsis when it
/// was longer (character-counted, as the Python slice was).
pub fn truncate_title(title: String) -> String {
    if title.chars().count() <= TITLE_MAX {
        return title;
    }
    let mut cut: String = title.chars().take(TITLE_MAX - 1).collect();
    cut.push('\u{2026}');
    cut
}

/// Push `branch` to `remote` (as `refs/heads/<branch>`) under a lease.
///
/// The bare `--force-with-lease` expects the remote branch to sit where our
/// remote-tracking ref says it does: that permits rewriting our own earlier
/// push (rebase-on-retry) while refusing to clobber a branch someone else
/// moved. When the remote branch has since been deleted -- GitHub
/// auto-deletes a merged PR's head branch -- the surviving tracking ref is
/// stale and the lease refuses every retry as `stale info`, forever. Nothing
/// can be clobbered on a ref that does not exist, so a refused push probes
/// the remote and, only when the branch is absent, retries with a lease that
/// expects it absent (still atomic: a concurrent creator wins). Any other
/// refusal is returned unchanged.
fn push_with_lease(repo_root: &Path, remote: &str, branch: &str) -> Result<GitOutput, GitError> {
    let refspec = format!("{branch}:{branch}");
    let push = git(repo_root, &["push", "--force-with-lease", remote, &refspec])?;
    if push.ok() {
        return Ok(push);
    }
    let remote_ref = format!("refs/heads/{branch}");
    // `--exit-code` exits 2 exactly when the remote answered and no ref
    // matched; an unreachable remote exits 128 and keeps the first refusal.
    let probe = git(
        repo_root,
        &["ls-remote", "--exit-code", remote, &remote_ref],
    )?;
    if probe.code != Some(2) {
        return Ok(push);
    }
    let lease = format!("--force-with-lease={remote_ref}:");
    git(repo_root, &["push", &lease, remote, &refspec])
}

/// Open a PR for `branch` onto `pr_base`, or refresh the body of the one
/// already open for it; returns the PR URL.
fn ensure_pr(
    runner: &dyn GhRunner,
    branch: &str,
    pr_base: &str,
    title: &str,
    body: &str,
) -> Result<String, GhError> {
    let existing = gh(
        runner,
        &[
            "pr", "list", "--head", branch, "--base", pr_base, "--state", "open", "--json", "url",
            "--jq", ".[0].url",
        ],
    )?;
    let existing = existing.trim();
    if !existing.is_empty() {
        gh(runner, &["pr", "edit", existing, "--body", body])?;
        return Ok(existing.to_owned());
    }
    let url = gh(
        runner,
        &[
            "pr", "create", "--head", branch, "--base", pr_base, "--title", title, "--body", body,
        ],
    )?;
    Ok(url.trim().to_owned())
}

// --- the pr strategy ---------------------------------------------------------------------------

/// Worktree provisioning from the merge strategy; landing via PR.
#[derive(Clone)]
pub struct GitPullRequestSubmitter {
    inner: GitWorktreeSubmitter,
    /// The push target (`[submit] remote`).
    pub remote: String,
    /// The PR base branch (`[submit] pr_base`, default the worker's base).
    pub pr_base: String,
    gh: Arc<dyn GhRunner>,
}

impl fmt::Debug for GitPullRequestSubmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitPullRequestSubmitter")
            .field("inner", &self.inner)
            .field("remote", &self.remote)
            .field("pr_base", &self.pr_base)
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for GitPullRequestSubmitter {
    type Target = GitWorktreeSubmitter;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl GitPullRequestSubmitter {
    /// Wrap `inner`: push to `origin`, open PRs onto the worker's base, and
    /// drive the real `gh` binary. `inner` keeps its configured landing base
    /// (the merge strategy's), which is the base a task's commits, protected
    /// paths, and trailers are judged against.
    pub fn new(inner: GitWorktreeSubmitter) -> Self {
        let pr_base = inner.phase_base.clone();
        Self {
            inner,
            remote: "origin".to_owned(),
            pr_base,
            gh: Arc::new(ProcessGhRunner),
        }
    }

    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
        self
    }

    pub fn with_pr_base(mut self, pr_base: impl Into<String>) -> Self {
        self.pr_base = pr_base.into();
        self
    }

    pub fn with_gh(mut self, gh: Arc<dyn GhRunner>) -> Self {
        self.gh = gh;
        self
    }

    pub fn inner(&self) -> &GitWorktreeSubmitter {
        &self.inner
    }

    fn log(&self, message: impl AsRef<str>) {
        (self.inner.log)(message.as_ref());
    }

    /// Record the park witness and return the park outcome; the worktree
    /// and branch are left exactly as they are.
    fn park(&self, run_id: &str, park_kind: &str, detail: String) -> SubmitOutcome {
        self.inner.record_landing_park(
            run_id,
            &LandingParked {
                park_kind: park_kind.to_owned(),
                detail: detail.clone(),
                ..LandingParked::default()
            },
        );
        SubmitOutcome::parked(park_kind, detail)
    }

    /// Dispose a worktree whose branch has just landed as a PR, per
    /// `on_done`: the remote branch + PR now hold the work, so the local
    /// copies are done. `cleanup` force-deletes the branch: it is
    /// deliberately unmerged locally -- the push already succeeded, so the
    /// commits are safe remotely. `on_done = "preserve"` keeps the local
    /// worktree+branch for inspection instead.
    fn teardown_on_done(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        if self.inner.on_done == "preserve" {
            self.log(format!(
                "on_done=preserve; keeping landed worktree at {} for inspection",
                worktree.display()
            ));
            return Ok(());
        }
        self.inner.cleanup(worktree, branch)
    }

    /// The PR landing path. Every leaf returns an outcome; an `Err` is the
    /// Python "raise" that [`SubmitStrategy::submit`] contains.
    pub fn submit_inner(&self, req: &SubmitRequest) -> Result<SubmitOutcome, SubmitError> {
        if req.status != Status::Done {
            // Identical park semantics to the merge strategy: the inherited
            // ladder parks (or destroys) a non-DONE worktree before any
            // landing logic.
            return self.inner.submit_inner(req);
        }
        let task_id = &req.task_id;
        let phase = self.inner.phase_of(req.task_file.as_deref());
        let worktree = self.inner.worktree(task_id);
        let branch = self.inner.branch(task_id, &phase);
        let base = self.inner.landing_base_for(&phase)?;
        let wt = worktree.display();

        let porcelain = git(&worktree, &["status", "--porcelain"])?;
        if !porcelain.trimmed().is_empty() {
            self.log(format!(
                "DONE with uncommitted changes on {branch}; parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_UNCOMMITTED_WORK,
                format!("DONE with an uncommitted tree on {branch}; worktree preserved at {wt}"),
            ));
        }

        let commit_count = self.inner.commit_count(&branch, &base)?;
        if commit_count == 0 {
            let reason =
                format!("{task_id} reached DONE with no commits beyond {base}; nothing to submit");
            self.log(&reason);
            self.inner.cleanup(&worktree, &branch)?;
            return Ok(SubmitOutcome::no_op(reason));
        }

        let violations = self.inner.protected_violations(&branch, &base)?;
        if !violations.is_empty() {
            let joined = violations.join(", ");
            self.log(format!(
                "{task_id} touches protected path(s) {joined}; refusing to open a PR, parking \
                 worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_PROTECTED_PATHS,
                format!(
                    "{branch} touches protected path(s) {joined}; refusing to open a PR, \
                     worktree preserved at {wt}"
                ),
            ));
        }

        // Stamp harness-authoritative provenance onto the task's own commits
        // before the push, so the branch on the remote never lacks trailers
        // while a PR references it. Message-only, tree byte-identical,
        // forged `Flywheel-*` trailers replaced -- the same shared engine
        // and vocabulary the merge path uses (spec 00078, criterion 2). This
        // precedes both the push and the PR create/edit.
        self.inner
            .stamp_trailers(req, &worktree, &branch, &base, &phase)?;

        let push = push_with_lease(&self.inner.repo_root, &self.remote, &branch)?;
        if !push.ok() {
            let stderr = push.stderr.trim();
            self.log(format!(
                "push of {branch} to {} failed ({stderr}); parking worktree at {wt}",
                self.remote
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_PUSH_FAILED,
                format!(
                    "push of {branch} to {} failed: {stderr}; worktree preserved at {wt}",
                    self.remote
                ),
            ));
        }

        let url = match self.ensure_pr(&branch, req) {
            Ok(url) => url,
            Err(exc) => {
                // A failed PR open is the same class of land-suppression as
                // a failed push: the branch is pushed but the PR never
                // opened, so the work has not landed. Record the reason and
                // leave the worktree parked.
                self.log(format!(
                    "opening a PR for {branch} failed ({exc}); parking worktree at {wt}"
                ));
                return Ok(self.park(
                    &req.run_id,
                    PARK_KIND_PUSH_FAILED,
                    format!("opening a PR for {branch} failed: {exc}; worktree preserved at {wt}"),
                ));
            }
        };
        self.log(format!(
            "Landed {branch} as PR {url} ({commit_count} commit(s)); merge is review/CI's call"
        ));
        // The land completed (branch pushed, PR open): record the landed
        // reference -- the PR identifier -- before tearing down the local
        // copies.
        self.inner.record_landing(
            &req.run_id,
            &Landed {
                strategy: LANDING_STRATEGY_PR.to_owned(),
                landed_ref: url.clone(),
                ..Landed::default()
            },
        );
        self.teardown_on_done(&worktree, &branch)?;
        Ok(SubmitOutcome::landed(LANDING_STRATEGY_PR, url))
    }

    /// Open a PR for `branch`, or refresh the body of the open one.
    fn ensure_pr(&self, branch: &str, req: &SubmitRequest) -> Result<String, GhError> {
        let title = truncate_title(format!("{}: {}", req.task_id, req.task.goal));
        let body = render_pr_body(req);
        ensure_pr(self.gh.as_ref(), branch, &self.pr_base, &title, &body)
    }
}

impl SubmitStrategy for GitPullRequestSubmitter {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        self.inner.prepare_sandbox(request)
    }

    /// Push a DONE task's branch and open/refresh its PR, or park the
    /// worktree. Never fails: any git failure is logged, witnessed as a
    /// `submit-error` park, and the worktree left parked for inspection.
    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        match self.submit_inner(request) {
            Ok(outcome) => outcome,
            Err(err) => {
                let worktree = self.inner.worktree(&request.task_id);
                self.log(format!(
                    "submit error for {} ({}: {err}); worktree left parked at {}",
                    request.task_id,
                    err.kind(),
                    worktree.display()
                ));
                self.park(
                    &request.run_id,
                    PARK_KIND_SUBMIT_ERROR,
                    format!(
                        "submit raised {}: {err}; worktree left parked at {}",
                        err.kind(),
                        worktree.display()
                    ),
                )
            }
        }
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        self.inner.is_landable(request)
    }
}

/// Build the PR backend (the registry's `pr` target) from the shared
/// builder request: the merge builder's provisioning, with the remote and
/// PR base read off `policy` (`[submit] remote` / `pr_base`; a `None`
/// policy takes `origin` and the worker's base). Logs the resolved landing
/// target so an operator can see the strategy that armed.
pub fn build_pr_submitter(request: &SubmitBuildRequest) -> GitPullRequestSubmitter {
    let inner = build_merge_submitter(request);
    let (remote, pr_base) = match request.policy.as_deref() {
        Some(policy) => (
            policy.submit_remote.clone(),
            policy
                .submit_pr_base
                .clone()
                .unwrap_or_else(|| request.phase_base.clone()),
        ),
        None => ("origin".to_owned(), request.phase_base.clone()),
    };
    let submitter = GitPullRequestSubmitter::new(inner)
        .with_remote(remote)
        .with_pr_base(pr_base);
    (request.log)(&format!(
        "landing strategy: pr (remote={} base={})",
        submitter.remote, submitter.pr_base
    ));
    submitter
}

/// Register the `pr` strategy on `registry`; a second registration is a
/// [`SubmitRegistryError::Duplicate`].
pub fn register_pr_strategy(
    registry: &mut SubmitStrategyRegistry,
) -> Result<(), SubmitRegistryError> {
    registry.register(SubmitStrategySpec::new(
        PR_STRATEGY,
        "push the task branch to [submit] remote and open a pull request onto pr_base",
        Arc::new(|_registry, request| {
            Ok(Arc::new(build_pr_submitter(request)) as Arc<dyn SubmitStrategy>)
        }),
    ))
}

// --- phase-branch completion (spec 00079, criterion 3) -----------------------------------------
//
// The per-task PR strategy above lands one task per PR. Under `[submit]
// strategy = "phase"` tasks instead land continuously onto a per-phase
// integration branch, and the review unit becomes the whole phase: at phase
// completion the worker pushes that branch and opens exactly one PR whose
// body aggregates every task's receipts and held-out verdict. That
// completion layer lives here (it reuses this module's `gh` runner seam and
// receipt rendering) and is injected into the archive sweep as its
// `phase_completion` seam.

/// The store surface the phase publisher reads: each task's latest run, its
/// final-attempt receipts, and its held-out verdict. Every core store
/// satisfies it.
pub trait PhaseEvidenceStore:
    LifecycleStore + DomainEventStore + AttemptStore + GraderResultStore + Send + Sync
{
}

impl<T: LifecycleStore + DomainEventStore + AttemptStore + GraderResultStore + Send + Sync + ?Sized>
    PhaseEvidenceStore for T
{
}

/// One task's aggregated, store-backed evidence for the phase PR body.
///
/// `receipts` is the harness's verdict from the run's final attempt (empty
/// when the run never reached grading); `held_out_outcome` is the run's
/// `HeldOutGateEvaluated` wire value (`None` when no gate evaluation was
/// recorded). Both are read from the store, never re-derived from agent
/// output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTaskSection {
    pub task_id: String,
    pub goal: String,
    pub receipts: Vec<GraderReceipt>,
    pub held_out_outcome: Option<String>,
}

/// The human-readable label of a held-out verdict wire value. `NO_GATE` is
/// deliberately kept visually apart from `PASS` -- a task that registered
/// no held-out grader is not the same as one that passed a gate
/// (`docs/held-out-gate.md`); an unrecorded evaluation is distinct again.
pub fn held_out_label(outcome: Option<&str>) -> String {
    match outcome {
        None => "not recorded".to_owned(),
        Some("no_gate") => "NO_GATE".to_owned(),
        Some("pass") => "PASS".to_owned(),
        Some("fail") => "FAIL".to_owned(),
        Some(other) => other.to_owned(),
    }
}

/// Project the run's final-attempt grader receipts (the harness's
/// verdicts): the last attempt's `grader_results` rows flattened into
/// [`GraderReceipt`] values. A run that never reached grading yields an
/// empty list.
pub fn final_receipts(
    store: &dyn PhaseEvidenceStore,
    run_id: &str,
) -> Result<Vec<GraderReceipt>, StoreError> {
    let attempts = store.list_attempts(run_id)?;
    let Some(last) = attempts.iter().max_by_key(|a| a.number) else {
        return Ok(Vec::new());
    };
    Ok(store
        .list_grader_results(run_id, last.number)?
        .into_iter()
        .map(|record| GraderReceipt {
            ordinal: record.ordinal,
            grader_type: record.grader_type,
            name: record.grader_name,
            passed: record.passed,
        })
        .collect())
}

/// The run's most recent held-out gate outcome wire value, or `None` when
/// the gate recorded no evaluation for the run (distinct from a recorded
/// `no_gate`).
pub fn latest_held_out_outcome(
    store: &dyn PhaseEvidenceStore,
    run_id: &str,
) -> Result<Option<String>, StoreError> {
    Ok(store
        .list_domain_events(run_id)?
        .into_iter()
        .rev()
        .find_map(|event| match event.payload {
            DomainEventPayload::HeldOutGateEvaluated(evaluated) => Some(evaluated.outcome),
            _ => None,
        }))
}

/// Aggregate one task's store-backed evidence for the phase PR body.
///
/// Resolves the task's most recent run (`list_lifecycles` is ordered
/// newest-first) and projects its final receipts and held-out verdict. A
/// task with no recorded run yields an empty section -- rendered as the
/// receipt-projection-unavailable line rather than omitted.
pub fn collect_phase_task_receipts(
    store: &dyn PhaseEvidenceStore,
    task: &Task,
) -> Result<PhaseTaskSection, StoreError> {
    let lifecycles = store.list_lifecycles(&LifecycleFilter::default().with_task_id(&task.id))?;
    let Some(latest) = lifecycles.first() else {
        return Ok(PhaseTaskSection {
            task_id: task.id.clone(),
            goal: task.goal.clone(),
            receipts: Vec::new(),
            held_out_outcome: None,
        });
    };
    Ok(PhaseTaskSection {
        task_id: task.id.clone(),
        goal: task.goal.clone(),
        receipts: final_receipts(store, &latest.run_id)?,
        held_out_outcome: latest_held_out_outcome(store, &latest.run_id)?,
    })
}

/// Render one task's section: goal, held-out verdict, receipts table.
fn push_phase_task_section(lines: &mut Vec<String>, section: &PhaseTaskSection) {
    lines.push(format!("## Task `{}`", section.task_id));
    lines.push(String::new());
    lines.push(section.goal.clone());
    lines.push(String::new());
    lines.push(format!(
        "- held-out gate: `{}`",
        held_out_label(section.held_out_outcome.as_deref())
    ));
    lines.push(String::new());
    lines.push("### Grader receipts".into());
    lines.push(String::new());
    push_receipt_table(lines, &section.receipts);
    lines.push(String::new());
}

/// The phase PR body: one section per task, receipts + held-out verdict.
///
/// The receipts are the harness's verdicts (never the agent's claim) and
/// the held-out outcome is rendered faithfully -- `NO_GATE` distinct from
/// `PASS` -- so a reviewer sees, per task, exactly how "done" was decided
/// before trusting the aggregate. Byte-identical to the Python rendering.
pub fn render_phase_pr_body(phase: &str, sections: &[PhaseTaskSection]) -> String {
    let mut lines: Vec<String> = vec![
        format!("## Phase `{phase}`"),
        String::new(),
        format!(
            "Lands the phase integration branch `{}` onto the true base. Every task's grader \
             receipts and held-out gate verdict are aggregated below; receipts are the \
             harness's verdicts from each run's final attempt, never re-derived from agent \
             output.",
            phase_branch(phase)
        ),
        String::new(),
    ];
    for section in sections {
        push_phase_task_section(&mut lines, section);
    }
    lines.push("---".into());
    lines.push(
        "Opened by flywheel. Receipts are the harness's verdicts; agent claims are never \
         authoritative. Merge is review/CI's call -- the loop opened and refreshes this PR but \
         never merges it."
            .into(),
    );
    lines.join("\n")
}

/// What stops a phase publication outright (the Python `except Exception`
/// in the completion callable): the sweep is never unwound, the failure is
/// logged and the phase left active.
#[derive(Debug, thiserror::Error)]
pub enum PhasePublishError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot create a scratch checkout: {0}")]
    Scratch(#[source] std::io::Error),
}

/// A pre-PR gate's verdict: the publication continues, or stops with an
/// operator-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gate {
    Passed,
    Refused(String),
}

/// Push a completed phase's branch and ensure exactly one aggregate PR.
///
/// Injected by the worker as the `phase_completion` seam of
/// `crate::phase::archive_completed_phases` under `[submit] strategy =
/// "phase"`. When the sweep hands over a phase whose tasks are all DONE and
/// landed and whose loop-path gate passed, this:
///
/// 1. syncs the phase branch with the true base -- one merge of the base
///    INTO the phase branch, because a phase PR that conflicts with its base
///    runs ZERO GitHub CI (GitHub cannot build the test merge) and stalls
///    silently; a base already contained is a no-op, and a conflicting
///    merge aborts loudly and opens no PR;
/// 2. evaluates `[phase] verify` against a checkout of the phase-branch
///    tree (spec 00079, D-6) -- never the operator's checkout, which does
///    not hold the phase's work; after the sync this is the exact merged
///    tree the PR's CI will test -- opening no PR on a non-zero exit;
/// 3. pushes the phase branch to the remote (`--force-with-lease`);
/// 4. opens exactly one PR onto the true base -- or refreshes the body of
///    the one already open for the branch -- with every task's grader
///    receipts and held-out verdict aggregated in the body.
///
/// The worker pushes and opens/refreshes only; it never merges the phase
/// PR (the phase archives later, once the PR merges -- the dependent task's
/// surface). Every failure is logged and surfaced, never raised: a
/// phase-completion attempt must never unwind the archive sweep.
pub struct PhasePrPublisher {
    store: Arc<dyn PhaseEvidenceStore>,
    pub repo_root: PathBuf,
    pub remote: String,
    pub pr_base: String,
    /// `[phase] verify`, run against the phase-branch tree; `None` => no
    /// gate.
    pub phase_verify: Option<String>,
    log: Logger,
    gh: Arc<dyn GhRunner>,
}

impl fmt::Debug for PhasePrPublisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhasePrPublisher")
            .field("repo_root", &self.repo_root)
            .field("remote", &self.remote)
            .field("pr_base", &self.pr_base)
            .field("phase_verify", &self.phase_verify)
            .finish_non_exhaustive()
    }
}

impl PhasePrPublisher {
    /// A publisher pushing to `remote` and opening PRs onto `pr_base`
    /// (the true base), driving the real `gh` binary, with no
    /// `[phase] verify` gate.
    pub fn new(
        store: Arc<dyn PhaseEvidenceStore>,
        repo_root: impl Into<PathBuf>,
        remote: impl Into<String>,
        pr_base: impl Into<String>,
        log: Logger,
    ) -> Self {
        Self {
            store,
            repo_root: repo_root.into(),
            remote: remote.into(),
            pr_base: pr_base.into(),
            phase_verify: None,
            log,
            gh: Arc::new(ProcessGhRunner),
        }
    }

    pub fn with_phase_verify(mut self, command: Option<String>) -> Self {
        self.phase_verify = command;
        self
    }

    pub fn with_gh(mut self, gh: Arc<dyn GhRunner>) -> Self {
        self.gh = gh;
        self
    }

    /// This publisher as the archive sweep's `phase_completion` callable.
    pub fn completion(&self) -> impl Fn(&Path, &[Task]) + '_ {
        move |phase_dir, tasks| self.publish(phase_dir, tasks)
    }

    fn log(&self, message: impl AsRef<str>) {
        (self.log)(message.as_ref());
    }

    /// Publish the completed phase at `phase_dir`. Never fails: an
    /// unexpected failure (git could not spawn, the store errored) is
    /// logged and the phase left active with its PR not (re)opened.
    pub fn publish(&self, phase_dir: &Path, tasks: &[Task]) {
        let phase = phase_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let branch = phase_branch(&phase);
        if let Err(exc) = self.publish_inner(&phase, &branch, tasks) {
            self.log(format!(
                "phase completion for '{phase}' raised ({exc}); the phase is left active and its \
                 PR not (re)opened"
            ));
        }
    }

    fn publish_inner(
        &self,
        phase: &str,
        branch: &str,
        tasks: &[Task],
    ) -> Result<(), PhasePublishError> {
        if let Gate::Refused(detail) = self.sync_with_true_base(branch)? {
            self.log(format!(
                "pre-PR sync of {branch} with {}/{} failed ({detail}); opening no PR -- a \
                 conflicting phase PR runs zero CI and stalls silently -- phase '{phase}' left \
                 active; merge {} into {branch} by hand to unblock",
                self.remote, self.pr_base, self.pr_base
            ));
            return Ok(());
        }
        if let Some(command) = &self.phase_verify
            && let Gate::Refused(detail) = self.verify_phase_tree(branch, command)?
        {
            self.log(format!(
                "[phase] verify failed against the {branch} tree ({detail}); opening no PR, \
                 phase '{phase}' left active"
            ));
            return Ok(());
        }
        let push = push_with_lease(&self.repo_root, &self.remote, branch)?;
        if !push.ok() {
            self.log(format!(
                "push of {branch} to {} failed ({}); opening no PR, phase '{phase}' left active",
                self.remote,
                push.stderr.trim()
            ));
            return Ok(());
        }
        let sections = tasks
            .iter()
            .map(|task| collect_phase_task_receipts(self.store.as_ref(), task))
            .collect::<Result<Vec<_>, _>>()?;
        let body = render_phase_pr_body(phase, &sections);
        let title = truncate_title(format!("Phase {phase}: {} task(s)", sections.len()));
        match ensure_pr(self.gh.as_ref(), branch, &self.pr_base, &title, &body) {
            Ok(url) => self.log(format!(
                "Phase '{phase}': pushed {branch} and ensured PR {url} ({} task(s)); merge is \
                 review/CI's call",
                sections.len()
            )),
            Err(exc) => self.log(format!(
                "opening/refreshing the phase PR for {branch} failed ({exc}); the branch is \
                 pushed but no PR is open, phase '{phase}' left active"
            )),
        }
        Ok(())
    }

    /// Merge the true base into `branch` once, before any PR opens.
    ///
    /// GitHub builds a PR's check runs against the test merge of head and
    /// base; when they conflict there is no test merge, so a conflicting
    /// phase PR runs ZERO CI and sits silently un-mergeable -- the stall is
    /// invisible unless someone opens the PR page. Syncing here makes the
    /// PR mergeable up front (and lets `[phase] verify` judge the exact
    /// merged tree CI will test) or fails LOUD at the seam where the
    /// operator is already watching.
    ///
    /// The base is the ref the PR actually merges into: `pr_base` fetched
    /// from `remote`, falling back to the local `pr_base` ref when the fetch
    /// cannot resolve it (offline runs, local-only bases). A base already
    /// contained in the branch is a no-op -- no merge commit churn, no
    /// forced CI re-run -- so repeated sweep passes stay idempotent. The
    /// merge itself runs in a disposable worktree ON the branch (phase
    /// branches are never checked out, so the branch ref is free); a
    /// conflict aborts the merge, leaves the branch exactly as it was, and
    /// reports the conflicted paths.
    fn sync_with_true_base(&self, branch: &str) -> Result<Gate, PhasePublishError> {
        let fetch = git(&self.repo_root, &["fetch", &self.remote, &self.pr_base])?;
        let base_ref = if fetch.ok() {
            "FETCH_HEAD".to_owned()
        } else {
            let probe = git(
                &self.repo_root,
                &["rev-parse", "--verify", "--quiet", &self.pr_base],
            )?;
            if !probe.ok() {
                return Ok(Gate::Refused(format!(
                    "cannot resolve the true base '{}' locally or on '{}' ({})",
                    self.pr_base,
                    self.remote,
                    fetch.stderr.trim()
                )));
            }
            self.pr_base.clone()
        };
        let base_sha = git(&self.repo_root, &["rev-parse", &base_ref])?
            .trimmed()
            .to_owned();
        let contained = git(
            &self.repo_root,
            &["merge-base", "--is-ancestor", &base_sha, branch],
        )?;
        if contained.ok() {
            return Ok(Gate::Passed);
        }
        let scratch = Scratch::new("flywheel-phase-sync-", &self.repo_root)?;
        let checkout = scratch.checkout.to_string_lossy().into_owned();
        let add = git(&self.repo_root, &["worktree", "add", &checkout, branch])?;
        if !add.ok() {
            return Ok(Gate::Refused(refusal_detail(
                &add.stderr,
                &format!("could not check out {branch}"),
            )));
        }
        let message = format!(
            "merge: sync {branch} with {} before the phase PR",
            self.pr_base
        );
        let merge = git(
            &scratch.checkout,
            &["merge", "--no-edit", "-m", &message, &base_sha],
        )?;
        if !merge.ok() {
            let conflicted: Vec<String> = git(
                &scratch.checkout,
                &["diff", "--name-only", "--diff-filter=U"],
            )?
            .stdout
            .split_whitespace()
            .map(str::to_owned)
            .collect();
            git(&scratch.checkout, &["merge", "--abort"])?;
            let what = if conflicted.is_empty() {
                refusal_detail(&merge.stderr, merge.stdout.trim())
            } else {
                format!("conflicts in {}", conflicted.join(", "))
            };
            return Ok(Gate::Refused(format!(
                "merging {} ({}): {what}",
                self.pr_base,
                base_sha.get(..12).unwrap_or(&base_sha)
            )));
        }
        Ok(Gate::Passed)
    }

    /// Run `[phase] verify` against a detached checkout of `branch`.
    ///
    /// A dedicated worktree of the phase branch is the tree the gate must
    /// judge (D-6): the operator's checkout does not contain the phase's
    /// landed work. The worktree is always removed afterwards, pass or
    /// fail.
    fn verify_phase_tree(&self, branch: &str, command: &str) -> Result<Gate, PhasePublishError> {
        let scratch = Scratch::new("flywheel-phase-verify-", &self.repo_root)?;
        let checkout = scratch.checkout.to_string_lossy().into_owned();
        let add = git(
            &self.repo_root,
            &["worktree", "add", "--detach", &checkout, branch],
        )?;
        if !add.ok() {
            return Ok(Gate::Refused(refusal_detail(
                &add.stderr,
                &format!("could not check out {branch}"),
            )));
        }
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&scratch.checkout)
            .stdin(Stdio::null())
            .output();
        match output {
            Err(err) => Ok(Gate::Refused(format!("could not spawn {command:?}: {err}"))),
            Ok(out) if !out.status.success() => {
                let tail = String::from_utf8_lossy(if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                });
                let tail = tail.trim();
                if !tail.is_empty() {
                    self.log(format!(
                        "[phase] verify {command:?} output on the {branch} tree: {tail}"
                    ));
                }
                Ok(Gate::Refused(format!(
                    "exit {}",
                    out.status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |c| c.to_string())
                )))
            }
            Ok(_) => Ok(Gate::Passed),
        }
    }
}

/// `stderr` trimmed, else `fallback` -- the Python `a.strip() or b`.
fn refusal_detail(stderr: &str, fallback: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A disposable scratch worktree location under the system temp dir,
/// deregistered from `repo_root` and deleted on drop (pass or fail).
struct Scratch {
    parent: PathBuf,
    checkout: PathBuf,
    repo_root: PathBuf,
}

impl Scratch {
    fn new(prefix: &str, repo_root: &Path) -> Result<Self, PhasePublishError> {
        let parent = std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&parent).map_err(PhasePublishError::Scratch)?;
        Ok(Self {
            checkout: parent.join("tree"),
            parent,
            repo_root: repo_root.to_path_buf(),
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let checkout = self.checkout.to_string_lossy().into_owned();
        let _ = git(
            &self.repo_root,
            &["worktree", "remove", "--force", &checkout],
        );
        let _ = fs::remove_dir_all(&self.parent);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Real-git-with-a-bare-remote fixtures and the scripted `gh` runner,
    //! shared with the tier-routing tests.

    use super::*;
    use crate::submitter::test_support::{git_ok, init_repo};
    use flywheel_core::task::GraderType;
    use flywheel_orchestrator::github::GhOutput;
    use flywheel_orchestrator::sources::WorkSourceError;
    use std::sync::Mutex;

    /// The canned `pr create` URL.
    pub const CREATED_URL: &str = "https://example.test/pr/7";

    pub type GhResponder = Box<dyn Fn(&[String]) -> GhOutput + Send + Sync>;

    /// Recording `gh` runner: `pr list` returns the configured existing URL
    /// (or nothing), `pr create` returns [`CREATED_URL`], everything else
    /// returns an empty success. A custom responder overrides all of that.
    pub struct ScriptedGh {
        calls: Mutex<Vec<Vec<String>>>,
        responder: GhResponder,
    }

    impl ScriptedGh {
        pub fn new(existing_url: &str) -> Arc<Self> {
            let existing = existing_url.to_owned();
            Self::with_responder(Box::new(move |argv: &[String]| {
                if argv.len() >= 2 && argv[0] == "pr" && argv[1] == "list" {
                    GhOutput::ok(existing.clone())
                } else if argv.len() >= 2 && argv[0] == "pr" && argv[1] == "create" {
                    GhOutput::ok(format!("{CREATED_URL}\n"))
                } else {
                    GhOutput::ok("")
                }
            }))
        }

        pub fn with_responder(responder: GhResponder) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responder,
            })
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        /// `"pr list"`, `"pr create"`, ... per recorded call, in order.
        pub fn commands(&self) -> Vec<String> {
            self.calls()
                .iter()
                .map(|c| c.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
                .collect()
        }

        /// The first recorded call starting with `verb` (`"create"`, `"edit"`).
        pub fn call(&self, verb: &str) -> Vec<String> {
            self.calls()
                .into_iter()
                .find(|c| c.len() >= 2 && c[0] == "pr" && c[1] == verb)
                .unwrap_or_else(|| panic!("no pr {verb} call recorded"))
        }
    }

    impl GhRunner for ScriptedGh {
        fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            Ok((self.responder)(argv))
        }
    }

    /// The value following `flag` in `argv`.
    pub fn arg_after(argv: &[String], flag: &str) -> String {
        let idx = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"));
        argv[idx + 1].clone()
    }

    /// Init a work repo plus a bare `origin` next to it; returns the remote.
    pub fn init_repo_with_remote(path: &Path) -> PathBuf {
        init_repo(path);
        let remote = path.parent().unwrap().join("origin.git");
        git_ok(
            path.parent().unwrap(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git_ok(path, &["remote", "add", "origin", remote.to_str().unwrap()]);
        remote
    }

    pub fn remote_branch_exists(remote: &Path, branch: &str) -> bool {
        git(
            remote,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .unwrap()
        .ok()
    }

    /// The `valueonly` render of `key` on `rev` in `repo` -- `""` when the
    /// rev does not resolve (no push happened).
    pub fn trailer_at(repo: &Path, rev: &str, key: &str) -> String {
        git(
            repo,
            &[
                "show",
                "-s",
                &format!("--format=%(trailers:key={key},valueonly)"),
                rev,
            ],
        )
        .map(|out| {
            if out.ok() {
                out.trimmed().to_owned()
            } else {
                String::new()
            }
        })
        .unwrap_or_default()
    }

    pub fn receipt(ordinal: u32, name: Option<&str>, passed: bool) -> GraderReceipt {
        GraderReceipt {
            ordinal,
            grader_type: GraderType::Command,
            name: name.map(str::to_owned),
            passed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::phase::{ArchiveOptions, archive_completed_phases};
    use crate::submit_registry::{MERGE_STRATEGY, PHASE_STRATEGY, builtin_registry};
    use crate::submitter::test_support::{
        branch_exists, commit, git_ok, landed, ledger, parked, quiet, rev, sandbox_req, submit_req,
        submitter, task_file, trailer_value,
    };
    use chrono::Utc;
    use flywheel_core::events::{DomainEvent, HeldOutGateEvaluated};
    use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome};
    use flywheel_core::store::GraderResultRecord;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, Grader, GraderType};
    use flywheel_orchestrator::github::GhOutput;
    use flywheel_orchestrator::policy::WorkPolicy;
    use std::sync::Mutex;

    const BRANCH: &str = "flywheel/01-phase/t1";

    fn repo_with_remote() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = init_repo_with_remote(&repo);
        (dir, repo, remote)
    }

    fn pr_submitter(repo: &Path, gh: Arc<ScriptedGh>) -> GitPullRequestSubmitter {
        GitPullRequestSubmitter::new(submitter(repo, ledger())).with_gh(gh)
    }

    fn capture_log() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Logger = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        (log, lines)
    }

    /// A DONE request for `t1` carrying one passing receipt.
    fn done_req(tf: &Path, sandbox: &Path) -> SubmitRequest {
        submit_req(tf, "t1", sandbox, Status::Done, &["true"]).with_receipts(vec![receipt(
            0,
            Some("tests"),
            true,
        )])
    }

    fn prepare_with_commit(
        s: &GitPullRequestSubmitter,
        repo: &Path,
        file: &str,
        message: &str,
    ) -> (PathBuf, PathBuf) {
        let tf = task_file(repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        commit(&wt, file, "x", message);
        (tf, wt)
    }

    // --- body rendering ----------------------------------------------------------------

    #[test]
    fn render_pr_body_is_byte_identical_to_the_python() {
        let task = Task::new(
            "Goal for t1.",
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id("t1");
        let req = SubmitRequest::new(task, "run-1", Status::Done, "/sb").with_receipts(vec![
            receipt(0, Some("tests"), true),
            GraderReceipt {
                ordinal: 1,
                grader_type: GraderType::Transcript,
                name: None,
                passed: false,
            },
        ]);
        assert_eq!(
            render_pr_body(&req),
            "## Task\n\nGoal for t1.\n\n## Run\n\n- task: `t1`\n- run: `run-1`\n- terminal \
             status: `done`\n\n## Grader receipts\n\n| # | type | name | verdict |\n| - | ---- \
             | ---- | ------- |\n| 0 | command | tests | pass |\n| 1 | transcript | - | FAIL \
             |\n\n---\nOpened by flywheel. Receipts are the harness's verdict from the run's \
             final attempt; agent claims are never authoritative."
        );
    }

    #[test]
    fn render_pr_body_without_receipts() {
        let task = Task::new(
            "G.",
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id("t1");
        let req = SubmitRequest::new(task, "run-9", Status::Done, "/sb");
        let body = render_pr_body(&req);
        assert_eq!(
            body,
            "## Task\n\nG.\n\n## Run\n\n- task: `t1`\n- run: `run-9`\n- terminal status: \
             `done`\n\n## Grader receipts\n\n(receipt projection unavailable for this \
             run)\n\n---\nOpened by flywheel. Receipts are the harness's verdict from the \
             run's final attempt; agent claims are never authoritative."
        );
        assert!(body.contains("receipt projection unavailable"));
        assert!(body.contains("agent claims are never authoritative"));
    }

    #[test]
    fn titles_are_cut_at_ninety_characters_with_an_ellipsis() {
        let short = "t1: short".to_owned();
        assert_eq!(truncate_title(short.clone()), short);
        let exact: String = "x".repeat(TITLE_MAX);
        assert_eq!(truncate_title(exact.clone()), exact);
        let long = format!("t1: {}", "x".repeat(200));
        let cut = truncate_title(long);
        assert_eq!(cut.chars().count(), TITLE_MAX);
        assert_eq!(cut, format!("t1: {}\u{2026}", "x".repeat(85)));
        // Character-counted, not byte-counted.
        let wide = "\u{00e9}".repeat(100);
        assert_eq!(truncate_title(wide).chars().count(), TITLE_MAX);
    }

    // --- landing -------------------------------------------------------------------------

    #[test]
    fn done_pushes_branch_and_opens_pr() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        let (log, lines) = capture_log();
        let s = GitPullRequestSubmitter::new(
            submitter(&repo, Arc::clone(&store)).with_verify_command("false"),
        )
        .with_gh(gh.clone());
        let s = GitPullRequestSubmitter {
            inner: {
                let mut inner = s.inner.clone();
                inner.log = log;
                inner
            },
            ..s
        };
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        let base_before = rev(&repo, "main");

        let outcome = s.submit(&done_req(&tf, &wt));

        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_PR, CREATED_URL)
        );
        // Branch landed on the remote, PR opened, nothing merged locally; the
        // [submit] verify standing gate is not in the PR path (CI owns it).
        assert!(remote_branch_exists(&remote, BRANCH));
        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(!repo.join("feature.txt").exists());
        // Local worktree + branch cleaned: the remote + PR hold the work now.
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, BRANCH));

        let create = gh.call("create");
        let body = arg_after(&create, "--body");
        assert!(body.contains("Goal for t1."));
        assert!(body.contains("run-1"));
        assert!(body.contains("| 0 | command | tests | pass |"));
        assert_eq!(arg_after(&create, "--title"), "t1: Goal for t1.");
        assert_eq!(arg_after(&create, "--base"), "main");
        assert_eq!(arg_after(&create, "--head"), BRANCH);
        let list = gh.call("list");
        assert_eq!(
            list,
            [
                "pr", "list", "--head", BRANCH, "--base", "main", "--state", "open", "--json",
                "url", "--jq", ".[0].url"
            ]
        );

        // The landed witness carries the PR identifier under the pr strategy.
        let landed = landed(&store);
        assert_eq!(landed.len(), 1);
        assert_eq!(landed[0].strategy, LANDING_STRATEGY_PR);
        assert_eq!(landed[0].landed_ref, CREATED_URL);
        assert_eq!(landed[0].rung, "");
        assert!(parked(&store).is_empty());
        assert!(
            lines.lock().unwrap().iter().any(|l| l
                == &format!(
                    "Landed {BRANCH} as PR {CREATED_URL} (1 commit(s)); merge is review/CI's call"
                )),
            "{:?}",
            lines.lock().unwrap()
        );
    }

    #[test]
    fn done_with_open_pr_refreshes_body() {
        let (_dir, repo, _remote) = repo_with_remote();
        let gh = ScriptedGh::new("https://example.test/pr/3\n");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");

        let outcome = s.submit(&done_req(&tf, &wt));

        // Re-landing a branch with an open PR edits it; never a duplicate.
        assert_eq!(gh.commands(), ["pr list", "pr edit"]);
        let edit = gh.call("edit");
        assert_eq!(edit[2], "https://example.test/pr/3");
        assert_eq!(edit[3], "--body");
        assert!(edit[4].contains("| 0 | command | tests | pass |"));
        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_PR, "https://example.test/pr/3")
        );
    }

    #[test]
    fn failed_parks_without_push() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");

        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Failed, &["true"]));

        // Identical park semantics to the merge strategy: forensics stay local.
        assert!(matches!(outcome, SubmitOutcome::NoOp { .. }));
        assert!(wt.exists());
        assert!(!remote_branch_exists(&remote, BRANCH));
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn failed_with_on_failure_destroy_removes_the_worktree() {
        let (_dir, repo, _remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = GitPullRequestSubmitter::new(
            submitter(&repo, ledger()).with_retention("destroy", "destroy"),
        )
        .with_gh(gh);
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Failed, &["true"]));
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, BRANCH));
    }

    #[test]
    fn protected_path_refuses_pr_and_records_park() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        let s = GitPullRequestSubmitter::new(
            submitter(&repo, Arc::clone(&store)).with_protected_paths(["conftest.py"]),
        )
        .with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "conftest.py", "edit conftest");

        let outcome = s.submit(&done_req(&tf, &wt));

        // Defense in depth: even with a human reviewing, tampered
        // verification surface never even reaches the PR stage.
        assert!(wt.exists());
        assert!(!remote_branch_exists(&remote, BRANCH));
        assert!(gh.calls().is_empty());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PROTECTED_PATHS);
        assert!(parks[0].detail.contains("conftest.py"));
        assert!(parks[0].detail.contains("refusing to open a PR"));
        assert_eq!(
            outcome,
            SubmitOutcome::parked(PARK_KIND_PROTECTED_PATHS, parks[0].detail.clone())
        );
        // No trailer stamp ran either: the branch tip is untouched.
        assert_eq!(trailer_value(&repo, BRANCH, "Flywheel-Run"), "");
    }

    #[test]
    fn push_failure_parks_and_records_park() {
        let (_dir, repo, _remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        // A remote that does not exist makes the real push fail.
        let s = GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store)))
            .with_remote("nonexistent")
            .with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");

        let outcome = s.submit(&done_req(&tf, &wt));

        assert!(wt.exists());
        assert!(gh.calls().is_empty());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PUSH_FAILED);
        assert!(parks[0].detail.contains("nonexistent"));
        assert!(
            parks[0]
                .detail
                .starts_with(&format!("push of {BRANCH} to nonexistent failed: "))
        );
        assert!(
            matches!(outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_PUSH_FAILED)
        );
        assert!(landed(&store).is_empty());
    }

    /// The sha `branch` points at on the bare `remote`.
    fn remote_rev(remote: &Path, branch: &str) -> String {
        rev(remote, &format!("refs/heads/{branch}"))
    }

    #[test]
    fn rerun_lands_after_the_remote_branch_was_deleted() {
        // GitHub auto-deletes a merged PR's head branch. The local
        // remote-tracking ref from the first push survives, so a bare
        // `--force-with-lease` on the re-run compares against a ref the
        // remote no longer has and is refused as `stale info` -- every time.
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        let s =
            GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store))).with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        assert_eq!(
            s.submit(&done_req(&tf, &wt)),
            SubmitOutcome::landed(LANDING_STRATEGY_PR, CREATED_URL)
        );
        git_ok(&remote, &["branch", "-D", BRANCH]);
        assert!(!remote_branch_exists(&remote, BRANCH));

        let (tf, wt) = prepare_with_commit(&s, &repo, "again.txt", "feat again");
        let tree = rev(&wt, "HEAD^{tree}");
        let outcome = s.submit(&done_req(&tf, &wt));

        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_PR, CREATED_URL)
        );
        // Stamping is message-only: the pushed tip carries the re-run's tree.
        assert_eq!(rev(&remote, &format!("refs/heads/{BRANCH}^{{tree}}")), tree);
        assert!(parked(&store).is_empty(), "{:?}", parked(&store));
        assert_eq!(landed(&store).len(), 2);
        assert!(!wt.exists());
    }

    #[test]
    fn push_still_refuses_to_clobber_a_branch_pushed_from_elsewhere() {
        // The lease's job: a remote branch that moved since our last push
        // (another clone pushed to it) is never overwritten, even though the
        // re-run's history is unrelated to it.
        let (dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        let s =
            GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store))).with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        assert_eq!(
            s.submit(&done_req(&tf, &wt)),
            SubmitOutcome::landed(LANDING_STRATEGY_PR, CREATED_URL)
        );
        let other = dir.path().join("other");
        git_ok(
            dir.path(),
            &[
                "clone",
                "-q",
                "-b",
                BRANCH,
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        commit(&other, "theirs.txt", "y", "their fixup");
        git_ok(&other, &["push", "-q", "origin", BRANCH]);
        let theirs = remote_rev(&remote, BRANCH);

        let (tf, wt) = prepare_with_commit(&s, &repo, "again.txt", "feat again");
        let outcome = s.submit(&done_req(&tf, &wt));

        assert!(
            matches!(outcome, SubmitOutcome::Parked { ref park_kind, .. } if park_kind == PARK_KIND_PUSH_FAILED),
            "{outcome:?}"
        );
        assert_eq!(remote_rev(&remote, BRANCH), theirs);
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert!(
            parks[0].detail.contains("stale info"),
            "{}",
            parks[0].detail
        );
        assert_eq!(landed(&store).len(), 1);
    }

    #[test]
    fn gh_failure_after_push_parks_as_push_failed() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::with_responder(Box::new(|argv: &[String]| {
            if argv[1] == "create" {
                GhOutput {
                    stdout: String::new(),
                    stderr: "GraphQL: Resource not accessible\n".into(),
                    exit_code: Some(1),
                }
            } else {
                GhOutput::ok("")
            }
        }));
        let store = ledger();
        let (log, lines) = capture_log();
        let mut inner = submitter(&repo, Arc::clone(&store));
        inner.log = log;
        let s = GitPullRequestSubmitter::new(inner).with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");

        let outcome = s.submit(&done_req(&tf, &wt));

        // The branch is pushed but the PR never opened: the work has not
        // landed, the worktree stays parked, and the reason is witnessed.
        assert!(remote_branch_exists(&remote, BRANCH));
        assert!(wt.exists());
        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PUSH_FAILED);
        assert!(
            parks[0].detail.starts_with(&format!(
                "opening a PR for {BRANCH} failed: gh pr create --head {BRANCH} --base main \
                 --title"
            )),
            "{}",
            parks[0].detail
        );
        assert!(
            parks[0].detail.contains(
                "failed (exit 1): GraphQL: Resource not accessible; worktree preserved at"
            ),
            "{}",
            parks[0].detail
        );
        assert!(landed(&store).is_empty());
        assert!(outcome.is_parked());
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with(&format!("opening a PR for {BRANCH} failed (gh pr create")))
        );
    }

    #[test]
    fn uncommitted_changes_park_like_the_merge_strategy() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let store = ledger();
        let s =
            GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store))).with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

        let outcome = s.submit(&done_req(&tf, &wt));

        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_UNCOMMITTED_WORK)
        );
        assert!(wt.exists());
        assert!(!remote_branch_exists(&remote, BRANCH));
        assert!(gh.calls().is_empty());
        assert_eq!(parked(&store)[0].park_kind, PARK_KIND_UNCOMMITTED_WORK);
    }

    #[test]
    fn zero_commit_done_cleans_up_without_a_pr() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();

        let outcome = s.submit(&done_req(&tf, &wt));

        assert_eq!(
            outcome,
            SubmitOutcome::no_op("t1 reached DONE with no commits beyond main; nothing to submit")
        );
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, BRANCH));
        assert!(!remote_branch_exists(&remote, BRANCH));
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn on_done_preserve_keeps_the_local_worktree_after_the_pr() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = GitPullRequestSubmitter::new(
            submitter(&repo, ledger()).with_retention("preserve", "park"),
        )
        .with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        assert!(s.submit(&done_req(&tf, &wt)).is_landed());
        assert!(remote_branch_exists(&remote, BRANCH));
        assert!(wt.exists());
        assert!(branch_exists(&repo, BRANCH));
    }

    #[test]
    fn git_failure_is_contained_as_a_submit_error_park() {
        let (_dir, repo, _remote) = repo_with_remote();
        let store = ledger();
        let s = GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store)))
            .with_gh(ScriptedGh::new(""));
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        // Deleting the worktree directory out from under the strategy makes
        // the first git call fail; the error must be contained.
        fs::remove_dir_all(&wt).unwrap();
        let outcome = s.submit(&done_req(&tf, &wt));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_SUBMIT_ERROR)
        );
        assert_eq!(parked(&store)[0].park_kind, PARK_KIND_SUBMIT_ERROR);
        assert!(
            parked(&store)[0]
                .detail
                .starts_with("submit raised GitError: ")
        );
    }

    #[test]
    fn is_landable_delegates_to_the_worktree_probe() {
        let (_dir, repo, _remote) = repo_with_remote();
        let s = pr_submitter(&repo, ScriptedGh::new(""));
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        assert!(!s.is_landable(&done_req(&tf, &wt)).landable);
        commit(&wt, "feature.txt", "x", "feat");
        assert!(s.is_landable(&done_req(&tf, &wt)).landable);
        assert!(format!("{s:?}").contains("pr_base: \"main\""));
    }

    // --- trailers (spec 00078, criterion 2) -------------------------------------------------

    /// A recording `gh` that snapshots the remote branch tip's
    /// `Flywheel-Run` trailer at every `pr create`/`pr edit`, proving the
    /// stamped push preceded the PR call.
    fn snapshotting_gh(
        remote: &Path,
        existing_url: &str,
    ) -> (Arc<ScriptedGh>, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let remote = remote.to_path_buf();
        let existing = existing_url.to_owned();
        let gh =
            ScriptedGh::with_responder(Box::new(move |argv: &[String]| match argv[1].as_str() {
                "list" => GhOutput::ok(existing.clone()),
                "create" | "edit" => {
                    sink.lock().unwrap().push(trailer_at(
                        &remote,
                        &format!("refs/heads/{BRANCH}"),
                        "Flywheel-Run",
                    ));
                    if argv[1] == "create" {
                        GhOutput::ok(format!("{CREATED_URL}\n"))
                    } else {
                        GhOutput::ok("")
                    }
                }
                _ => GhOutput::ok(""),
            }));
        (gh, seen)
    }

    fn remote_range(remote: &Path, base_sha: &str) -> Vec<String> {
        git_ok(
            remote,
            &[
                "rev-list",
                "--reverse",
                &format!("{base_sha}..refs/heads/{BRANCH}"),
            ],
        )
        .split_whitespace()
        .map(str::to_owned)
        .collect()
    }

    fn assert_remote_stamped(remote: &Path, sha: &str) {
        for (key, value) in [
            ("Flywheel-Task", "t1"),
            ("Flywheel-Run", "run-1"),
            ("Flywheel-Phase", "01-phase"),
        ] {
            assert_eq!(trailer_at(remote, sha, key), value, "{key} on {sha}");
            let body = git_ok(remote, &["show", "-s", "--format=%B", sha]);
            let prefix = format!("{}:", key.to_lowercase());
            assert_eq!(
                body.lines()
                    .filter(|l| l.trim().to_lowercase().starts_with(&prefix))
                    .count(),
                1,
                "{key} count on {sha}"
            );
        }
    }

    #[test]
    fn create_flow_stamps_every_pushed_commit_before_the_pr_call() {
        let (_dir, repo, remote) = repo_with_remote();
        let (gh, seen) = snapshotting_gh(&remote, "");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        // A scripted agent that commits with NO trailers on two commits:
        // only mechanical stamping can put the three keys on both.
        commit(&wt, "a.txt", "1", "feat: a");
        commit(&wt, "b.txt", "2", "feat: b");
        let base_before = rev(&repo, "main");

        assert!(s.submit(&done_req(&tf, &wt)).is_landed());

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        let shas = remote_range(&remote, &base_before);
        assert_eq!(shas.len(), 2);
        for sha in &shas {
            assert_remote_stamped(&remote, sha);
        }
        assert_eq!(
            git_ok(&remote, &["show", "-s", "--format=%s", &shas[0]]),
            "feat: a"
        );
        // The push preceded the PR create: the remote branch tip already
        // carried the real run trailer by the time gh was invoked.
        assert_eq!(*seen.lock().unwrap(), vec!["run-1".to_owned()]);
    }

    #[test]
    fn refresh_flow_stamps_every_pushed_commit() {
        let (_dir, repo, remote) = repo_with_remote();
        let (gh, seen) = snapshotting_gh(&remote, "https://example.test/pr/3\n");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let (tf, wt) = prepare_with_commit(&s, &repo, "a.txt", "feat: a");
        let base_before = rev(&repo, "main");

        assert!(s.submit(&done_req(&tf, &wt)).is_landed());

        assert_eq!(gh.commands(), ["pr list", "pr edit"]);
        let shas = remote_range(&remote, &base_before);
        assert_eq!(shas.len(), 1);
        assert_remote_stamped(&remote, &shas[0]);
        assert_eq!(*seen.lock().unwrap(), vec!["run-1".to_owned()]);
    }

    #[test]
    fn forged_trailers_are_replaced_on_the_pushed_branch() {
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = pr_submitter(&repo, gh);
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        fs::write(wt.join("a.txt"), "1").unwrap();
        git_ok(&wt, &["add", "-A"]);
        git_ok(
            &wt,
            &[
                "commit",
                "-m",
                "feat: launder provenance",
                "-m",
                "Flywheel-Run: forged-123\nFlywheel-Task: forged-task\nFlywheel-Phase: forged-phase",
            ],
        );
        let base_before = rev(&repo, "main");

        assert!(s.submit(&done_req(&tf, &wt)).is_landed());

        let shas = remote_range(&remote, &base_before);
        assert_eq!(shas.len(), 1);
        assert_remote_stamped(&remote, &shas[0]);
        for forged in ["forged-123", "forged-task", "forged-phase"] {
            assert_eq!(
                git_ok(
                    &remote,
                    &[
                        "log",
                        &format!("--grep={forged}"),
                        "--format=%H",
                        &format!("{base_before}..refs/heads/{BRANCH}"),
                    ]
                ),
                ""
            );
        }
    }

    #[test]
    fn stamping_is_message_only_trees_identical() {
        let (_dir, repo, remote) = repo_with_remote();
        let s = pr_submitter(&repo, ScriptedGh::new(""));
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        commit(&wt, "a.txt", "1", "feat: a");
        commit(&wt, "b.txt", "2", "feat: b");
        commit(&wt, "c.txt", "3", "feat: c");
        let pre_shas: Vec<String> = git_ok(&wt, &["rev-list", "--reverse", "main..HEAD"])
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let pre_trees: Vec<String> = pre_shas
            .iter()
            .map(|c| git_ok(&wt, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();
        let base_before = rev(&repo, "main");

        assert!(s.submit(&done_req(&tf, &wt)).is_landed());

        let post_shas = remote_range(&remote, &base_before);
        let post_trees: Vec<String> = post_shas
            .iter()
            .map(|c| git_ok(&remote, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();
        assert_eq!(post_trees, pre_trees);
        assert_eq!(post_trees.len(), 3);
    }

    #[test]
    fn stamp_covers_only_the_tasks_own_commits_when_pr_base_lags() {
        // The PR base lags the worker's base by one commit: the stamp must
        // rewrite the task's commit only, never the base commit the PR
        // also carries (rewriting it would push a divergent copy of base
        // history under the task's provenance).
        let (_dir, repo, remote) = repo_with_remote();
        git_ok(&repo, &["branch", "release", "main"]);
        let release_tip = rev(&repo, "release");
        let advance = commit(&repo, "base.txt", "b", "chore: base advance");
        let gh = ScriptedGh::new("");
        let s = pr_submitter(&repo, Arc::clone(&gh)).with_pr_base("release");
        let (tf, wt) = prepare_with_commit(&s, &repo, "a.txt", "feat: a");

        assert!(s.submit(&done_req(&tf, &wt)).is_landed());

        let shas = git_ok(
            &remote,
            &[
                "rev-list",
                "--reverse",
                &format!("{release_tip}..refs/heads/{BRANCH}"),
            ],
        )
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(shas.len(), 2);
        assert_eq!(shas[0], advance, "the base commit is pushed unchanged");
        assert_eq!(trailer_at(&remote, &shas[0], "Flywheel-Run"), "");
        assert_remote_stamped(&remote, &shas[1]);
        assert_eq!(arg_after(&gh.call("create"), "--base"), "release");
    }

    // --- registry ----------------------------------------------------------------------------

    fn build_request(tmp: &Path) -> (SubmitBuildRequest, Arc<Mutex<Vec<String>>>) {
        let (log, lines) = capture_log();
        let req = SubmitBuildRequest::new(
            tmp.join("repo"),
            tmp.join("tasks"),
            tmp.join("worktrees"),
            "main",
            tmp.join(".merge.lock"),
            log,
        );
        (req, lines)
    }

    #[test]
    fn pr_registers_and_builds_from_policy_knobs() {
        let mut registry = builtin_registry();
        register_pr_strategy(&mut registry).unwrap();
        assert_eq!(
            registry.names(),
            vec![MERGE_STRATEGY, PHASE_STRATEGY, PR_STRATEGY]
        );
        assert!(matches!(
            register_pr_strategy(&mut registry),
            Err(SubmitRegistryError::Duplicate { .. })
        ));
        let tmp = tempfile::tempdir().unwrap();
        let (req, lines) = build_request(tmp.path());
        let policy = WorkPolicy {
            submit_remote: "upstream".into(),
            submit_pr_base: Some("develop".into()),
            ..WorkPolicy::default()
        };
        let req = req.with_policy(Arc::new(policy));
        let built = build_pr_submitter(&req);
        assert_eq!(built.remote, "upstream");
        assert_eq!(built.pr_base, "develop");
        assert_eq!(built.phase_base, "main");
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            ["landing strategy: pr (remote=upstream base=develop)"]
        );
        let _strategy = registry.build(PR_STRATEGY, &req).unwrap();
        assert_eq!(lines.lock().unwrap().len(), 2);
    }

    #[test]
    fn pr_builder_defaults_without_a_policy_or_pr_base() {
        let tmp = tempfile::tempdir().unwrap();
        let (req, lines) = build_request(tmp.path());
        let built = build_pr_submitter(&req);
        assert_eq!(built.remote, "origin");
        assert_eq!(built.pr_base, "main");
        let with_policy = build_pr_submitter(&req.with_policy(Arc::new(WorkPolicy::default())));
        assert_eq!(with_policy.pr_base, "main");
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "landing strategy: pr (remote=origin base=main)",
                "landing strategy: pr (remote=origin base=main)"
            ]
        );
    }

    // --- phase PR body (pure) ------------------------------------------------------------------

    #[test]
    fn render_phase_body_is_byte_identical_to_the_python() {
        let sections = vec![
            PhaseTaskSection {
                task_id: "t1".into(),
                goal: "Goal for t1.".into(),
                receipts: vec![receipt(0, Some("tests"), true)],
                held_out_outcome: Some("pass".into()),
            },
            PhaseTaskSection {
                task_id: "t2".into(),
                goal: "Goal for t2.".into(),
                receipts: Vec::new(),
                held_out_outcome: Some("no_gate".into()),
            },
            PhaseTaskSection {
                task_id: "t3".into(),
                goal: "G3.".into(),
                receipts: vec![receipt(0, None, false)],
                held_out_outcome: None,
            },
            PhaseTaskSection {
                task_id: "t4".into(),
                goal: "G4.".into(),
                receipts: Vec::new(),
                held_out_outcome: Some("weird".into()),
            },
        ];
        let body = render_phase_pr_body("01-foo", &sections);
        assert_eq!(
            body,
            "## Phase `01-foo`\n\nLands the phase integration branch `flywheel/phase/01-foo` \
             onto the true base. Every task's grader receipts and held-out gate verdict are \
             aggregated below; receipts are the harness's verdicts from each run's final \
             attempt, never re-derived from agent output.\n\n## Task `t1`\n\nGoal for \
             t1.\n\n- held-out gate: `PASS`\n\n### Grader receipts\n\n| # | type | name | \
             verdict |\n| - | ---- | ---- | ------- |\n| 0 | command | tests | pass |\n\n## \
             Task `t2`\n\nGoal for t2.\n\n- held-out gate: `NO_GATE`\n\n### Grader \
             receipts\n\n(receipt projection unavailable for this run)\n\n## Task \
             `t3`\n\nG3.\n\n- held-out gate: `not recorded`\n\n### Grader receipts\n\n| # | \
             type | name | verdict |\n| - | ---- | ---- | ------- |\n| 0 | command | - | FAIL \
             |\n\n## Task `t4`\n\nG4.\n\n- held-out gate: `weird`\n\n### Grader \
             receipts\n\n(receipt projection unavailable for this run)\n\n---\nOpened by \
             flywheel. Receipts are the harness's verdicts; agent claims are never \
             authoritative. Merge is review/CI's call -- the loop opened and refreshes this PR \
             but never merges it."
        );
        assert_eq!(
            render_phase_pr_body("02-empty", &[]),
            "## Phase `02-empty`\n\nLands the phase integration branch \
             `flywheel/phase/02-empty` onto the true base. Every task's grader receipts and \
             held-out gate verdict are aggregated below; receipts are the harness's verdicts \
             from each run's final attempt, never re-derived from agent output.\n\n---\nOpened \
             by flywheel. Receipts are the harness's verdicts; agent claims are never \
             authoritative. Merge is review/CI's call -- the loop opened and refreshes this PR \
             but never merges it."
        );
        assert_eq!(held_out_label(Some("fail")), "FAIL");
    }

    // --- phase completion (end-to-end through the archive sweep) ---------------------------

    struct PhaseFixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        remote: PathBuf,
        tasks_dir: PathBuf,
        store: Arc<InMemoryStore>,
    }

    const PHASE: &str = "01-foo";

    fn now() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    /// Seed a DONE lifecycle with optional final-attempt receipts + verdict.
    fn seed_done_task(
        store: &InMemoryStore,
        task_id: &str,
        run_id: &str,
        receipts: Option<&[(Option<&str>, bool)]>,
        held_out: Option<&str>,
    ) {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        for st in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::Done,
        ] {
            lc.transition_to(st, "", Some(now())).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        if let Some(receipts) = receipts {
            let mut attempt = Attempt::new(1, now(), run_id);
            attempt.ended_at = Some(now());
            attempt.outcome = Some(Outcome::Succeeded);
            attempt.iterations_completed = 1;
            attempt.turns = 1;
            store.save_attempt(run_id, &attempt, None).unwrap();
            for (ordinal, (name, passed)) in receipts.iter().enumerate() {
                let mut spec = flywheel_core::store::JsonObject::new();
                spec.insert("type".into(), "command".into());
                spec.insert("run".into(), "true".into());
                store
                    .append_grader_result(&GraderResultRecord {
                        run_id: run_id.into(),
                        attempt_number: 1,
                        ordinal: ordinal as u32,
                        grader_type: GraderType::Command,
                        grader_spec: spec,
                        passed: *passed,
                        duration_ms: 1,
                        payload: flywheel_core::store::JsonObject::new(),
                        ts: now(),
                        grader_name: name.map(str::to_owned),
                        id: None,
                    })
                    .unwrap();
            }
        }
        if let Some(outcome) = held_out {
            let loaded = store.load_lifecycle(run_id).unwrap().unwrap();
            let event = DomainEvent::new(
                run_id,
                now(),
                DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                    outcome: outcome.into(),
                    ..HeldOutGateEvaluated::default()
                }),
            );
            store.append_domain_event(&event, loaded.version).unwrap();
        }
    }

    /// Land `files` onto `flywheel/phase/<phase>`; leave HEAD back on main.
    /// After this the phase's work lives ONLY on the phase branch, so a gate
    /// run against the operator checkout (main) would not see it.
    fn make_phase_branch(repo: &Path, phase: &str, files: &[(&str, &str)]) {
        let branch = phase_branch(phase);
        git_ok(repo, &["checkout", "-q", "-b", &branch]);
        for (name, content) in files {
            fs::write(repo.join(name), content).unwrap();
        }
        git_ok(repo, &["add", "-A"]);
        git_ok(repo, &["commit", "-q", "-m", &format!("land {phase}")]);
        git_ok(repo, &["checkout", "-q", "main"]);
    }

    /// A repo with a completed phase `01-foo`: two DONE tasks, a phase
    /// branch holding their work, and a store seeded for both.
    fn complete_phase_fixture(phase_files: &[(&str, &str)]) -> PhaseFixture {
        let (dir, repo, remote) = repo_with_remote();
        task_file(&repo, PHASE, "t1", "true");
        task_file(&repo, PHASE, "t2", "true");
        // Commit the active task files onto main (the operator checkout)
        // BEFORE branching, so switching back to main after building the
        // phase branch keeps them in place.
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-q", "-m", "seed active tasks"]);
        make_phase_branch(&repo, PHASE, phase_files);
        let store = Arc::new(InMemoryStore::new());
        seed_done_task(
            &store,
            "t1",
            "run-t1",
            Some(&[(Some("tests"), true)]),
            Some("pass"),
        );
        seed_done_task(&store, "t2", "run-t2", None, Some("no_gate"));
        let tasks_dir = repo.join(".flywheel").join("tasks");
        PhaseFixture {
            _dir: dir,
            repo,
            remote,
            tasks_dir,
            store,
        }
    }

    impl PhaseFixture {
        fn publisher(&self, gh: Arc<ScriptedGh>, log: Logger) -> PhasePrPublisher {
            PhasePrPublisher::new(
                Arc::clone(&self.store) as Arc<dyn PhaseEvidenceStore>,
                &self.repo,
                "origin",
                "main",
                log,
            )
            .with_gh(gh)
        }

        /// One archive sweep with the publisher wired as the completion
        /// seam; returns the moved phases and the captured log.
        fn sweep(&self, publisher: &PhasePrPublisher) -> (Vec<PathBuf>, Vec<String>) {
            let (log, lines) = capture_log();
            let completion = publisher.completion();
            let options = ArchiveOptions {
                repo_root: Some(&self.repo),
                log: Some(&log),
                true_base: Some("main"),
                phase_completion: Some(&completion),
                ..ArchiveOptions::default()
            };
            let moved =
                archive_completed_phases(&self.tasks_dir, self.store.as_ref(), &options).unwrap();
            let logged = lines.lock().unwrap().clone();
            (moved, logged)
        }

        fn phase_active(&self) -> bool {
            self.tasks_dir.join("active").join(PHASE).is_dir()
                && !self.tasks_dir.join("archive").join(PHASE).exists()
        }
    }

    fn phase_gh(existing_url: &str) -> Arc<ScriptedGh> {
        let existing = existing_url.to_owned();
        ScriptedGh::with_responder(Box::new(move |argv: &[String]| match argv[1].as_str() {
            "list" => GhOutput::ok(existing.clone()),
            "create" => GhOutput::ok("https://example.test/phase-pr/1\n"),
            _ => GhOutput::ok(""),
        }))
    }

    #[test]
    fn completion_pushes_branch_and_opens_one_pr() {
        let f = complete_phase_fixture(&[("a.txt", "1"), ("b.txt", "2")]);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let publisher = f.publisher(Arc::clone(&gh), log);

        let (moved, sweep_log) = f.sweep(&publisher);

        assert!(moved.is_empty());
        assert!(sweep_log.is_empty(), "{sweep_log:?}");
        // Exactly one PR: list then create, never a duplicate create.
        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        let create = gh.call("create");
        assert_eq!(arg_after(&create, "--head"), phase_branch(PHASE));
        assert_eq!(arg_after(&create, "--base"), "main");
        assert_eq!(arg_after(&create, "--title"), "Phase 01-foo: 2 task(s)");
        let body = arg_after(&create, "--body");
        assert!(body.contains("## Task `t1`"));
        assert!(body.contains("## Task `t2`"));
        assert!(body.contains("| 0 | command | tests | pass |"));
        assert!(body.contains("held-out gate: `PASS`"));
        assert!(body.contains("held-out gate: `NO_GATE`"));
        assert!(body.contains("(receipt projection unavailable for this run)"));
        // The branch is on the remote; the phase stays active (archives on
        // merge).
        assert!(remote_branch_exists(&f.remote, &phase_branch(PHASE)));
        assert!(f.phase_active());
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "Phase '01-foo': pushed flywheel/phase/01-foo and ensured PR \
                 https://example.test/phase-pr/1 (2 task(s)); merge is review/CI's call"
            ]
        );
        assert!(format!("{publisher:?}").contains("pr_base: \"main\""));
    }

    #[test]
    fn second_sweep_edits_rather_than_duplicates() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("https://example.test/phase-pr/9\n");
        let publisher = f.publisher(Arc::clone(&gh), quiet());

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr edit"]);
        let edit = gh.call("edit");
        assert_eq!(edit[2], "https://example.test/phase-pr/9");
        assert!(f.phase_active());
    }

    #[test]
    fn phase_verify_runs_against_phase_branch_tree() {
        // `a.txt` exists only on the phase branch, never on the operator
        // checkout (main). A gate asserting its presence passes ONLY when
        // evaluated against the phase-branch tree -- the whole point of D-6.
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        assert!(!f.repo.join("a.txt").exists());
        let gh = phase_gh("");
        let publisher = f
            .publisher(Arc::clone(&gh), quiet())
            .with_phase_verify(Some("test -f a.txt".into()));

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        assert!(remote_branch_exists(&f.remote, &phase_branch(PHASE)));
        // The scratch checkout was deregistered afterwards.
        assert!(!git_ok(&f.repo, &["worktree", "list"]).contains("flywheel-phase-verify-"));
    }

    #[test]
    fn red_phase_verify_opens_no_pr() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let publisher = f
            .publisher(Arc::clone(&gh), log)
            .with_phase_verify(Some("echo nope >&2; test -f does-not-exist.txt".into()));

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        assert!(!remote_branch_exists(&f.remote, &phase_branch(PHASE)));
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        assert!(
            logged.iter().any(|m| m
                == "[phase] verify failed against the flywheel/phase/01-foo tree (exit 1); \
                    opening no PR, phase '01-foo' left active"),
            "{logged:?}"
        );
        assert!(
            logged
                .iter()
                .any(|m| m.contains("output on the") && m.ends_with("nope"))
        );
    }

    #[test]
    fn push_failure_opens_no_pr() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let mut publisher = f.publisher(Arc::clone(&gh), log);
        publisher.remote = "nonexistent".into();

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        // The fetch from the bad remote fails, the local base resolves, and
        // the push then fails loudly.
        assert!(
            logged.iter().any(|m| m
                .starts_with("push of flywheel/phase/01-foo to nonexistent failed (")
                && m.ends_with("); opening no PR, phase '01-foo' left active")),
            "{logged:?}"
        );
    }

    #[test]
    fn republish_pushes_after_the_remote_phase_branch_was_deleted() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("");
        let publisher = f.publisher(Arc::clone(&gh), quiet());
        f.sweep(&publisher);
        let branch = phase_branch(PHASE);
        assert!(remote_branch_exists(&f.remote, &branch));
        git_ok(&f.remote, &["branch", "-D", &branch]);

        let (log, lines) = capture_log();
        let publisher = f.publisher(Arc::clone(&gh), log);
        f.sweep(&publisher);

        assert!(remote_branch_exists(&f.remote, &branch));
        assert_eq!(
            rev(&f.remote, &format!("refs/heads/{branch}")),
            rev(&f.repo, &branch)
        );
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "Phase '01-foo': pushed flywheel/phase/01-foo and ensured PR \
                 https://example.test/phase-pr/1 (2 task(s)); merge is review/CI's call"
            ]
        );
    }

    #[test]
    fn gh_failure_leaves_the_branch_pushed_and_the_phase_active() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = ScriptedGh::with_responder(Box::new(|_argv: &[String]| GhOutput {
            stdout: String::new(),
            stderr: "not logged in".into(),
            exit_code: Some(4),
        }));
        let (log, lines) = capture_log();
        let publisher = f.publisher(Arc::clone(&gh), log);

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list"]);
        assert!(remote_branch_exists(&f.remote, &phase_branch(PHASE)));
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        assert!(
            logged.iter().any(|m| m.starts_with(
                "opening/refreshing the phase PR for flywheel/phase/01-foo failed (gh pr list"
            ) && m.ends_with(
                "failed (exit 4): not logged in); the branch is pushed but no PR is open, \
                 phase '01-foo' left active"
            )),
            "{logged:?}"
        );
    }

    #[test]
    fn publisher_without_a_run_renders_the_unavailable_section() {
        // A task with no lifecycle at all is rendered, not dropped.
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let task = Task::new("Goal for t9.", Vec::new()).with_id("t9");
        let section = collect_phase_task_receipts(f.store.as_ref(), &task).unwrap();
        assert_eq!(
            section,
            PhaseTaskSection {
                task_id: "t9".into(),
                goal: "Goal for t9.".into(),
                receipts: Vec::new(),
                held_out_outcome: None,
            }
        );
        let t1 = Task::new("Goal for t1.", Vec::new()).with_id("t1");
        let t1_section = collect_phase_task_receipts(f.store.as_ref(), &t1).unwrap();
        assert_eq!(t1_section.receipts, vec![receipt(0, Some("tests"), true)]);
        assert_eq!(t1_section.held_out_outcome.as_deref(), Some("pass"));
    }

    // --- pre-PR phase-branch sync -----------------------------------------------------------

    #[test]
    fn sync_merges_advanced_base_into_phase_branch_before_pr() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let branch = phase_branch(PHASE);
        // Non-conflicting base advance, visible on the remote (the ref the
        // PR actually merges into).
        commit(&f.repo, "extra.txt", "mainline\n", "feat: base advanced");
        git_ok(&f.repo, &["push", "-q", "origin", "main:main"]);
        let gh = phase_gh("");
        let publisher = f.publisher(Arc::clone(&gh), quiet());

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        let tree = git_ok(&f.repo, &["ls-tree", "-r", "--name-only", &branch]);
        assert!(tree.contains("extra.txt"));
        let subject = git_ok(&f.repo, &["log", "-1", "--format=%s", &branch]);
        assert_eq!(
            subject,
            format!("merge: sync {branch} with main before the phase PR")
        );
        let remote_tree = git_ok(&f.remote, &["ls-tree", "-r", "--name-only", &branch]);
        assert!(remote_tree.contains("extra.txt"));
        // The scratch checkout is gone and the operator's checkout untouched.
        assert!(!git_ok(&f.repo, &["worktree", "list"]).contains("flywheel-phase-sync-"));
        assert_eq!(
            git_ok(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
    }

    #[test]
    fn sync_prefers_the_remote_base_over_a_stale_local_one() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let branch = phase_branch(PHASE);
        commit(
            &f.repo,
            "remote-only.txt",
            "remote\n",
            "feat: remote-only advance",
        );
        git_ok(&f.repo, &["push", "-q", "origin", "main:main"]);
        git_ok(&f.repo, &["reset", "-q", "--hard", "HEAD~1"]); // local main goes stale
        let gh = phase_gh("");
        let publisher = f.publisher(Arc::clone(&gh), quiet());

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        let tree = git_ok(&f.repo, &["ls-tree", "-r", "--name-only", &branch]);
        assert!(tree.contains("remote-only.txt"));
    }

    #[test]
    fn conflicting_base_aborts_sync_loudly_and_opens_no_pr() {
        let f = complete_phase_fixture(&[("a.txt", "phase version\n")]);
        let branch = phase_branch(PHASE);
        commit(
            &f.repo,
            "a.txt",
            "conflicting mainline version\n",
            "feat: conflicting base advance",
        );
        git_ok(&f.repo, &["push", "-q", "origin", "main:main"]);
        let head_before = rev(&f.repo, &branch);
        let base_sha = rev(&f.repo, "main");
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let publisher = f.publisher(Arc::clone(&gh), log);

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        assert!(!remote_branch_exists(&f.remote, &branch));
        assert_eq!(rev(&f.repo, &branch), head_before);
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        assert_eq!(
            logged,
            [format!(
                "pre-PR sync of {branch} with origin/main failed (merging main ({}): conflicts \
                 in a.txt); opening no PR -- a conflicting phase PR runs zero CI and stalls \
                 silently -- phase '01-foo' left active; merge main into {branch} by hand to \
                 unblock",
                &base_sha[..12]
            )]
        );
        assert!(!git_ok(&f.repo, &["worktree", "list"]).contains("flywheel-phase-sync-"));
    }

    #[test]
    fn contained_base_syncs_as_a_no_op() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let branch = phase_branch(PHASE);
        let head_before = rev(&f.repo, &branch);
        let gh = phase_gh("");
        let publisher = f.publisher(Arc::clone(&gh), quiet());

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        assert_eq!(rev(&f.repo, &branch), head_before);
    }

    #[test]
    fn unresolvable_base_refuses_before_any_git_mutation() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let mut publisher = f.publisher(Arc::clone(&gh), log);
        publisher.pr_base = "no-such-base".into();

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        let logged = lines.lock().unwrap().clone();
        assert!(
            logged.iter().any(|m| m.starts_with(
                "pre-PR sync of flywheel/phase/01-foo with origin/no-such-base failed (cannot \
                 resolve the true base 'no-such-base' locally or on 'origin' ("
            )),
            "{logged:?}"
        );
    }

    #[test]
    fn publish_contains_a_git_spawn_failure() {
        // An unspawnable git (an empty repo root path) is the Python
        // "raised" branch: logged, never unwound into the sweep.
        let store: Arc<dyn PhaseEvidenceStore> = Arc::new(InMemoryStore::new());
        let (log, lines) = capture_log();
        let tmp = tempfile::tempdir().unwrap();
        let publisher =
            PhasePrPublisher::new(store, tmp.path().join("absent"), "origin", "main", log)
                .with_gh(phase_gh(""));
        publisher.publish(Path::new("/tasks/active/03-bar"), &[]);
        let logged = lines.lock().unwrap().clone();
        // git -C <absent> fails rather than failing to spawn, so the sync
        // refuses on the unresolvable base; either way nothing is raised.
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].contains("03-bar"));
    }

    // --- mutation-survey pins (gh refresh, titles, evidence, true base, sync, verify) -------

    #[test]
    fn a_failed_pr_body_refresh_parks_instead_of_landing() {
        // The open PR's body carries the receipts reviewers trust; a refresh
        // that failed leaves the previous run's verdict on display, so the
        // land is suppressed exactly like a failed create.
        let (_dir, repo, remote) = repo_with_remote();
        let gh = ScriptedGh::with_responder(Box::new(|argv: &[String]| match argv[1].as_str() {
            "list" => GhOutput::ok("https://example.test/pr/3\n"),
            "edit" => GhOutput {
                stdout: String::new(),
                stderr: "HTTP 502: Bad Gateway\n".into(),
                exit_code: Some(1),
            },
            _ => GhOutput::ok(""),
        }));
        let store = ledger();
        let s =
            GitPullRequestSubmitter::new(submitter(&repo, Arc::clone(&store))).with_gh(gh.clone());
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");

        let outcome = s.submit(&done_req(&tf, &wt));

        assert_eq!(gh.commands(), ["pr list", "pr edit"]);
        assert!(remote_branch_exists(&remote, BRANCH));
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PUSH_FAILED);
        assert!(
            parks[0].detail.starts_with(&format!(
                "opening a PR for {BRANCH} failed: gh pr edit https://example.test/pr/3 --body"
            )),
            "{}",
            parks[0].detail
        );
        assert!(
            parks[0]
                .detail
                .contains("failed (exit 1): HTTP 502: Bad Gateway; worktree preserved at"),
            "{}",
            parks[0].detail
        );
        assert!(landed(&store).is_empty());
        assert_eq!(
            outcome,
            SubmitOutcome::parked(PARK_KIND_PUSH_FAILED, parks[0].detail.clone())
        );
    }

    #[test]
    fn a_long_goal_is_truncated_in_the_pr_title() {
        let (_dir, repo, _remote) = repo_with_remote();
        let gh = ScriptedGh::new("");
        let s = pr_submitter(&repo, Arc::clone(&gh));
        let (tf, wt) = prepare_with_commit(&s, &repo, "feature.txt", "feat");
        let mut req = done_req(&tf, &wt);
        req.task.goal = "g".repeat(200);

        assert!(s.submit(&req).is_landed());

        let title = arg_after(&gh.call("create"), "--title");
        assert_eq!(title, format!("t1: {}\u{2026}", "g".repeat(85)));
        assert_eq!(title.chars().count(), TITLE_MAX);
    }

    /// Record one command-grader result for `run_id` attempt `number`.
    fn seed_attempt(store: &InMemoryStore, run_id: &str, number: u32, name: &str, passed: bool) {
        let mut attempt = Attempt::new(number, now(), run_id);
        attempt.ended_at = Some(now());
        attempt.outcome = Some(if passed {
            Outcome::Succeeded
        } else {
            Outcome::ValidationFailed
        });
        attempt.iterations_completed = 1;
        attempt.turns = 1;
        store.save_attempt(run_id, &attempt, None).unwrap();
        let mut spec = flywheel_core::store::JsonObject::new();
        spec.insert("type".into(), "command".into());
        spec.insert("run".into(), "true".into());
        store
            .append_grader_result(&GraderResultRecord {
                run_id: run_id.into(),
                attempt_number: number,
                ordinal: 0,
                grader_type: GraderType::Command,
                grader_spec: spec,
                passed,
                duration_ms: 1,
                payload: flywheel_core::store::JsonObject::new(),
                ts: now(),
                grader_name: Some(name.to_owned()),
                id: None,
            })
            .unwrap();
    }

    fn seed_held_out(store: &InMemoryStore, run_id: &str, outcome: &str) {
        let version = store.load_lifecycle(run_id).unwrap().unwrap().version;
        let event = DomainEvent::new(
            run_id,
            now(),
            DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                outcome: outcome.into(),
                ..HeldOutGateEvaluated::default()
            }),
        );
        store.append_domain_event(&event, version).unwrap();
    }

    #[test]
    fn phase_evidence_is_the_latest_runs_final_attempt_and_latest_verdict() {
        // A retried task: an older run, then a newer run whose first attempt
        // failed its grader and whose held-out gate first failed, then
        // passed. The body must show what decided DONE -- the newest run's
        // final attempt and most recent gate verdict.
        let store = InMemoryStore::new();
        seed_done_task(
            &store,
            "t1",
            "run-old",
            Some(&[(Some("stale"), false)]),
            Some("fail"),
        );
        seed_done_task(&store, "t1", "run-new", None, None);
        seed_attempt(&store, "run-new", 1, "tests", false);
        seed_attempt(&store, "run-new", 2, "tests", true);
        seed_held_out(&store, "run-new", "fail");
        seed_held_out(&store, "run-new", "pass");
        let task = Task::new("Goal for t1.", Vec::new()).with_id("t1");

        let section = collect_phase_task_receipts(&store, &task).unwrap();

        assert_eq!(section.receipts, vec![receipt(0, Some("tests"), true)]);
        assert_eq!(section.held_out_outcome.as_deref(), Some("pass"));
    }

    #[test]
    fn the_phase_pr_targets_the_configured_true_base() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        git_ok(&f.repo, &["branch", "trunk", "main"]);
        let gh = phase_gh("");
        let mut publisher = f.publisher(Arc::clone(&gh), quiet());
        publisher.pr_base = "trunk".into();

        f.sweep(&publisher);

        assert_eq!(gh.commands(), ["pr list", "pr create"]);
        assert_eq!(arg_after(&gh.call("list"), "--base"), "trunk");
        assert_eq!(arg_after(&gh.call("create"), "--base"), "trunk");
    }

    #[test]
    fn a_phase_branch_checked_out_elsewhere_refuses_the_sync_with_gits_reason() {
        // An operator inspecting the phase branch in their own worktree
        // blocks the scratch checkout; the refusal must be the logged gate
        // path carrying git's own reason, with nothing pushed or opened.
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let branch = phase_branch(PHASE);
        commit(&f.repo, "extra.txt", "mainline\n", "feat: base advanced");
        let held = f.repo.parent().unwrap().join("held");
        git_ok(
            &f.repo,
            &["worktree", "add", held.to_str().unwrap(), &branch],
        );
        let head_before = rev(&f.repo, &branch);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let publisher = f.publisher(Arc::clone(&gh), log);

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        assert!(!remote_branch_exists(&f.remote, &branch));
        assert_eq!(rev(&f.repo, &branch), head_before);
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(
            logged[0].starts_with(&format!(
                "pre-PR sync of {branch} with origin/main failed (fatal: "
            )) && logged[0].contains("already")
                && !logged[0].contains("could not check out"),
            "{logged:?}"
        );
    }

    #[test]
    fn a_phase_verify_killed_by_a_signal_opens_no_pr() {
        let f = complete_phase_fixture(&[("a.txt", "1")]);
        let gh = phase_gh("");
        let (log, lines) = capture_log();
        let publisher = f
            .publisher(Arc::clone(&gh), log)
            .with_phase_verify(Some("kill -9 $$".into()));

        f.sweep(&publisher);

        assert!(gh.calls().is_empty());
        assert!(!remote_branch_exists(&f.remote, &phase_branch(PHASE)));
        assert!(f.phase_active());
        let logged = lines.lock().unwrap().clone();
        assert_eq!(
            logged,
            [
                "[phase] verify failed against the flywheel/phase/01-foo tree (exit signal); \
              opening no PR, phase '01-foo' left active"
            ]
        );
    }

    #[test]
    fn scratch_deregisters_and_deletes_its_checkout_on_drop() {
        let (_dir, repo, _remote) = repo_with_remote();
        let scratch = Scratch::new("flywheel-scratch-test-", &repo).unwrap();
        let parent = scratch.parent.clone();
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                scratch.checkout.to_str().unwrap(),
                "main",
            ],
        );
        assert!(scratch.checkout.join(".git").exists());

        drop(scratch);

        assert!(!parent.exists());
        assert!(!git_ok(&repo, &["worktree", "list"]).contains("flywheel-scratch-test-"));
    }
}
