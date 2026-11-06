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
//! The pr strategy records the same `uncommitted-work` park witness the
//! merge strategy does, and provenance trailers are stamped over the task's
//! own commits (`<landing base>..<branch>`, the same range the commit count
//! and protected-path gate judge) rather than `pr_base..branch`, which would
//! rewrite commits outside the task when `pr_base` differs from the
//! worker's base.

use crate::submit_registry::{
    PR_STRATEGY, SubmitBuildRequest, SubmitRegistryError, SubmitStrategyRegistry,
    SubmitStrategySpec,
};
use crate::submitter::{
    GitError, GitOutput, GitWorktreeSubmitter, SubmitError, build_merge_submitter, git,
};
use flywheel_core::events::{
    LANDING_STRATEGY_PR, Landed, LandingParked, PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_PUSH_FAILED, PARK_KIND_SUBMIT_ERROR, PARK_KIND_UNCOMMITTED_WORK,
};
use flywheel_core::lifecycle::Status;
use flywheel_orchestrator::github::{GhRunner, ProcessGhRunner, run_gh};
use flywheel_orchestrator::sources::GraderReceipt;
use flywheel_orchestrator::strategy::{
    LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    SubmitStrategy,
};
use std::fmt;
use std::path::Path;
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
/// reviewer can trust the green. Pinned byte for byte by the tests.
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
/// was longer (character-counted).
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
    /// failure that [`SubmitStrategy::submit`] contains.
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

#[cfg(test)]
pub(crate) mod test_support {
    //! Real-git-with-a-bare-remote fixtures and the scripted `gh` runner.

    use super::*;
    use std::path::PathBuf;
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
    use crate::submitter::Logger;
    use flywheel_core::task::Task;
    use std::fs;
    use std::path::PathBuf;
    
    use crate::submit_registry::{MERGE_STRATEGY, builtin_registry};
    use crate::submitter::test_support::{
        branch_exists, commit, git_ok, landed, ledger, parked, rev, sandbox_req, submit_req,
        submitter, task_file, trailer_value,
    };
    
    
    
    
    
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
    fn render_pr_body_matches_golden() {
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
        // A clone carries no repo-local identity; set one so the commit does
        // not depend on the host's global git config (CI runners have none).
        git_ok(
            &other,
            &["config", "user.email", "landing-test@example.com"],
        );
        git_ok(&other, &["config", "user.name", "landing test"]);
        git_ok(&other, &["config", "commit.gpgsign", "false"]);
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
            vec![MERGE_STRATEGY, PR_STRATEGY]
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

    // --- mutation-survey pins (gh refresh, titles) --------------------------------------------

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

}
