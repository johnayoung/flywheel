//! Dry runs that bring a branch into its base: a pull request (`gh`),
//! a rebase (and its abort), and a fast-forward merge in the main checkout.

use super::model::{Review, left_right, nul_list, operation, read, url_host};
use super::plan::{Plan, Planned, PrDefaults, fingerprint, push_destination, refused, tree_state};
use super::runner;
use super::runner::Spec;
use super::suspects::{Suspect, suspects};
use super::{
    BaseSource, GH_STATUS_TIMEOUT, GH_TIMEOUT, Git, MERGE_TIMEOUT, READ_LIMIT, REBASE_TIMEOUT,
    Target, Tools,
};
use crate::util::plural;
use std::path::{Path, PathBuf};

/// Whether `gh` is installed and signed in for `host`: `(installed,
/// authenticated, why not)`.
pub fn gh_status(tools: &Tools, dir: &Path, host: Option<&str>) -> (bool, bool, Option<String>) {
    let Some(program) = crate::agent::which(&tools.gh) else {
        return (
            false,
            false,
            Some("gh (GitHub CLI) is not installed: https://cli.github.com".into()),
        );
    };
    let program = program.to_string_lossy().into_owned();
    let mut args = vec!["auth".to_owned(), "status".to_owned()];
    if let Some(host) = host {
        args.push("--hostname".into());
        args.push(host.to_owned());
    }
    let spec = Spec {
        program: &program,
        dir,
        args,
        stdin: None,
        timeout: GH_STATUS_TIMEOUT,
        limit: 64 * 1024,
    };
    match runner::run(&spec) {
        Ok(out) if out.success() => (true, true, None),
        Ok(out) => (
            true,
            false,
            Some(format!(
                "gh is not signed in{}: run `gh auth login` ({})",
                host.map(|h| format!(" to {h}")).unwrap_or_default(),
                out.error_text()
            )),
        ),
        Err(e) => (true, false, Some(e.to_string())),
    }
}

/// The pull request's base: given, else the worktree's base, else the
/// remote's default branch.
fn pr_base(git: &Git<'_>, review: &Review, remote: &str, given: Option<&str>) -> Option<String> {
    if let Some(base) = given.map(str::trim).filter(|b| !b.is_empty()) {
        return Some(base.to_owned());
    }
    if let Some(base) = review
        .base
        .as_ref()
        .filter(|b| b.is_branch && b.source != BaseSource::Upstream)
    {
        return Some(base.name.clone());
    }
    let head = git.maybe(&[
        "symbolic-ref",
        "-q",
        "--short",
        &format!("refs/remotes/{remote}/HEAD"),
    ])?;
    head.strip_prefix(&format!("{remote}/")).map(str::to_owned)
}

/// The title and body offered for a pull request of `review`'s commits.
fn pr_defaults(review: &Review, base: &str) -> PrDefaults {
    let branch = review.branch.clone().unwrap_or_default();
    let title = match review.commits.as_slice() {
        [only] => only.subject.clone(),
        _ => {
            let leaf = branch.rsplit('/').next().unwrap_or(&branch);
            leaf.replace(['-', '_'], " ")
        }
    };
    let mut body = String::new();
    if !review.commits.is_empty() {
        body.push_str("## Commits\n\n");
        for c in review.commits.iter().rev() {
            body.push_str(&format!("- {} ({})\n", c.subject, c.short));
        }
    }
    PrDefaults {
        title,
        body,
        base: base.to_owned(),
    }
}

/// What a pull request is opened with.
#[derive(Debug, Clone, Default)]
pub struct PrRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub draft: bool,
}

/// Plan opening a pull request with `gh pr create` from the pushed
/// branch (nothing is pushed, let alone forced).
pub fn plan_pr(tools: &Tools, target: &Target, request: &PrRequest) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let mut refusal: Option<String> = None;
    let branch = review.branch.clone();
    // The remote the branch is pushed to, and the same-named branch there
    // (what the pull request is opened from).
    let remote = branch
        .as_deref()
        .and_then(|b| push_destination(&git, b, &review.remotes, None).ok())
        .map(|to| to.remote)
        .or_else(|| review.remotes.first().map(|r| r.name.clone()))
        .unwrap_or_default();
    let pushed = branch.as_deref().and_then(|b| {
        let there = format!("{remote}/{b}");
        git.maybe(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/remotes/{there}^{{commit}}"),
        ])?;
        let (behind, ahead) = left_right(&git, &format!("refs/remotes/{there}...HEAD"));
        Some((there, ahead, behind))
    });
    let host = review
        .remotes
        .iter()
        .find(|r| r.name == remote)
        .and_then(|r| url_host(&r.url));
    let base = pr_base(&git, &review, &remote, request.base.as_deref());
    let defaults = pr_defaults(&review, base.as_deref().unwrap_or_default());
    let title = request
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| defaults.title.clone());
    let body = request
        .body
        .clone()
        .unwrap_or_else(|| defaults.body.clone());
    match (&branch, &pushed, &base) {
        (None, _, _) => refusal = Some("HEAD is detached: a pull request needs a branch".into()),
        (Some(_), None, _) if review.remotes.is_empty() => {
            refusal = Some(
                "the repository has no remotes: add one (git remote add), push, then open the \
                 pull request"
                    .into(),
            );
        }
        (Some(_), _, Some(base)) if base.starts_with('-') || base.contains(char::is_whitespace) => {
            refusal = Some(format!("base {base:?} is not a branch name"));
        }
        (Some(b), None, _) => {
            refusal = Some(format!(
                "{b} is not pushed to {remote} yet: push it first (the pull request is opened from the pushed branch)"
            ));
        }
        (Some(_), Some((there, ahead, behind)), _) if *ahead > 0 || *behind > 0 => {
            refusal = Some(format!(
                "the branch and {there} differ ({ahead} ahead, {behind} behind): push first so the pull request shows what you reviewed"
            ));
        }
        (Some(_), Some(_), None) => {
            refusal = Some("name the base branch the pull request merges into".into());
        }
        (Some(b), Some(_), Some(base)) if base == b => {
            refusal = Some(format!(
                "the pull request would merge {b} into itself: name another base"
            ));
        }
        _ => {}
    }
    if refusal.is_none() && title.trim().is_empty() {
        refusal = Some("write a title".into());
    }
    if refusal.is_none() && host.is_none() {
        refusal = Some(format!(
            "the remote {remote} is not on a host (a local path): pull requests need a GitHub remote"
        ));
    }
    if refusal.is_none() {
        let (installed, authed, why) = gh_status(tools, &target.dir, host.as_deref());
        if !installed || !authed {
            refusal = why;
        }
    }
    let program = crate::agent::which(&tools.gh)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| tools.gh.clone());
    let mut args = vec![
        "pr".to_owned(),
        "create".into(),
        "--base".into(),
        base.clone().unwrap_or_default(),
        "--head".into(),
        branch.clone().unwrap_or_default(),
        "--title".into(),
        title.clone(),
        "--body-file".into(),
        "-".into(),
    ];
    if request.draft {
        args.push("--draft".into());
    }
    let command = Planned::new(&program, &target.dir, args, GH_TIMEOUT)
        .with_stdin(body.clone().into_bytes(), "the body");
    let head = review.head.clone().unwrap_or_default();
    let there = pushed.as_ref().map(|p| p.0.clone()).unwrap_or_default();
    Ok(Plan {
        action: "pr",
        summary: match &base {
            Some(base) => format!(
                "Open a pull request of {} into {base}{}",
                branch.clone().unwrap_or_default(),
                host.map(|h| format!(" on {h}")).unwrap_or_default()
            ),
            None => format!(
                "Open a pull request of {} (no base branch to merge into yet)",
                branch.clone().unwrap_or_default()
            ),
        },
        details: review
            .commits
            .iter()
            .map(|c| format!("{} {}", c.short, c.subject))
            .collect(),
        warnings: if review.clean() {
            Vec::new()
        } else {
            vec!["uncommitted changes are not in the pull request".into()]
        },
        // No base: no command to show (never `--base ''`).
        commands: if base.is_some() {
            vec![command]
        } else {
            Vec::new()
        },
        // Everything that shapes the command: what runs is what was
        // confirmed.
        fingerprint: fingerprint(&[
            "pr",
            &head,
            &there,
            base.as_deref().unwrap_or_default(),
            &title,
            &body,
            if request.draft { "draft" } else { "ready" },
        ]),
        refusal,
        needs_rebase: false,
        defaults: Some(defaults),
        contents: Vec::new(),
        suspects: Vec::new(),
        excluded: Vec::new(),
        files: None,
    })
}

/// The main checkout of the repository `dir` belongs to.
fn main_checkout(git: &Git<'_>) -> Result<PathBuf, String> {
    match crate::git::main_worktree(git.dir)? {
        (_, true) => {
            Err("the repository's main checkout is bare: there is no checkout to merge in".into())
        }
        (main, false) => Ok(main),
    }
}

/// Plan rebasing a worktree's branch onto its base, in the worktree. A
/// conflict stops it there (left in progress for the agent or you).
pub fn plan_rebase(tools: &Tools, target: &Target) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let state = tree_state(&git)?;
    let base = review.base.clone();
    let mut refusal = None;
    if target.worktree.is_none() {
        refusal = Some("rebasing onto the base is for worktree sessions".into());
    } else if let Some(op) = &review.operation {
        refusal = Some(format!("a {op} is already in progress; finish it first"));
    } else if review.branch.is_none() {
        refusal = Some("HEAD is detached".into());
    } else if !review.staged.files.is_empty() || !review.unstaged.files.is_empty() {
        refusal = Some("the worktree has uncommitted changes: commit them first".into());
    } else if base.as_ref().is_none_or(|b| b.sha.is_none()) {
        refusal = Some("the base no longer resolves".into());
    } else if review.behind == 0 {
        refusal = Some(format!(
            "{} already has everything in {}: nothing to rebase",
            review.branch.clone().unwrap_or_default(),
            base.as_ref().map(|b| b.name.as_str()).unwrap_or_default()
        ));
    }
    let base_name = base.as_ref().map(|b| b.name.clone()).unwrap_or_default();
    let command = Planned::new(
        &tools.git,
        &target.dir,
        vec!["rebase".into(), base_name.clone()],
        REBASE_TIMEOUT,
    );
    let branch = review.branch.clone().unwrap_or_default();
    Ok(Plan {
        action: "rebase",
        summary: format!(
            "Replay {}'s {} onto {} ({} new there), in the worktree",
            branch,
            plural(review.ahead as usize, "commit"),
            base_name,
            review.behind
        ),
        details: vec![
            "a conflict stops the rebase where it is: resolve the files, git add them, git rebase --continue (or ask the agent to)".into(),
            "rewrites the branch's commits: a branch already pushed then needs a forced push, which the hub never does".into(),
        ],
        warnings: if review.upstream.is_some() {
            vec![format!(
                "{branch} is already pushed: after the rebase it differs from {}",
                review.upstream.as_ref().map(|u| u.name.as_str()).unwrap_or_default()
            )]
        } else {
            Vec::new()
        },
        commands: vec![command],
        fingerprint: fingerprint(&[
            "rebase",
            &state,
            base.as_ref().and_then(|b| b.sha.as_deref()).unwrap_or_default(),
        ]),
        refusal,
        needs_rebase: false,
        defaults: None,
        contents: Vec::new(),
        suspects: Vec::new(),
        excluded: Vec::new(),
        files: None,
    })
}

/// Plan aborting a stopped rebase: the branch goes back to where it was
/// before it.
pub fn plan_abort(tools: &Tools, target: &Target) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let state = tree_state(&git)?;
    let refusal = (review.operation.as_deref() != Some("rebase"))
        .then(|| format!("no rebase is stopped in {}", target.dir.display()));
    let branch = review
        .rebasing
        .clone()
        .unwrap_or_else(|| "the branch".into());
    let mut details = vec![format!(
        "{branch} goes back to the commit it was on before the rebase"
    )];
    if !review.conflicts.is_empty() || !review.staged.files.is_empty() {
        details
            .push("conflict resolutions and staged changes made during the rebase are lost".into());
    }
    Ok(Plan {
        action: "abort",
        summary: format!("Abort the rebase of {branch} in {}", target.dir.display()),
        details,
        warnings: Vec::new(),
        commands: vec![Planned::new(
            &tools.git,
            &target.dir,
            vec!["rebase".into(), "--abort".into()],
            REBASE_TIMEOUT,
        )],
        fingerprint: fingerprint(&["abort", &state]),
        refusal,
        needs_rebase: false,
        defaults: None,
        contents: Vec::new(),
        suspects: Vec::new(),
        excluded: Vec::new(),
        files: None,
    })
}

/// Plan fast-forwarding the base onto a worktree's branch in the main
/// checkout (`git merge --ff-only`): refused when the main checkout is
/// not on the base, has uncommitted changes, or the branch is not a
/// fast-forward of the base (`needs_rebase`).
/// What a merge would land that looks like a secret, an agent's personal
/// settings, or build output: the files the branch adds (since it left
/// the base), judged as commit all judges them.
fn landed_suspects(git: &Git<'_>, dir: &Path, base: &str, head: &str) -> Vec<Suspect> {
    let Ok(out) = git.raw(
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
            &format!("refs/heads/{base}...{head}"),
        ],
        READ_LIMIT,
    ) else {
        return Vec::new();
    };
    if !out.success() {
        return Vec::new();
    }
    let added = nul_list(&out.stdout);
    let paths: Vec<&str> = added.iter().map(String::as_str).collect();
    suspects(dir, &paths, None)
}

/// Plan fast-forwarding the base to a worktree session's branch.
/// `include_suspects`: land what looks like a secret, personal settings,
/// or build output anyway (else refused, naming it).
pub fn plan_merge(tools: &Tools, target: &Target, include_suspects: bool) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let Some(worktree) = &target.worktree else {
        return Ok(refused(
            "merge",
            "merging into the base is for worktree sessions",
        ));
    };
    let review = read(tools, target)?;
    let branch = worktree.branch.clone();
    let base = worktree.base.clone();
    let main = main_checkout(&git)?;
    let main_git = Git::new(tools, &main);
    let mut refusal = None;
    let mut needs_rebase = false;
    let branch_sha = git
        .maybe(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}^{{commit}}"),
        ])
        .unwrap_or_default();
    let main_head = main_git
        .maybe(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .unwrap_or_default();
    let main_branch = main_git.maybe(&["symbolic-ref", "-q", "--short", "HEAD"]);
    let main_status = main_git
        .raw(
            &["status", "--porcelain=v2", "-z", "--untracked-files=no"],
            READ_LIMIT,
        )?
        .stdout;
    let main_dirty = nul_list(&main_status)
        .iter()
        .filter(|e| e.starts_with(['1', '2', 'u']))
        .count();
    let main_dir = main_git
        .maybe(&["rev-parse", "--absolute-git-dir"])
        .map(PathBuf::from);
    let base_is_branch = review.base.as_ref().is_some_and(|b| b.is_branch);
    let (ahead, behind) = (review.ahead, review.behind);
    if branch_sha.is_empty() {
        refusal = Some(format!("branch {branch} no longer exists"));
    } else if main == target.dir {
        refusal = Some("this session runs in the main checkout itself".into());
    } else if !base_is_branch {
        refusal = Some(format!(
            "the base {base} is a commit, not a branch: there is no branch to merge into"
        ));
    } else if main_branch.as_deref() != Some(base.as_str()) {
        refusal = Some(format!(
            "the main checkout {} is on {}, not {base}: check out {base} there first",
            main.display(),
            main_branch.as_deref().unwrap_or("a detached HEAD")
        ));
    } else if let Some(op) = main_dir.as_deref().and_then(operation) {
        refusal = Some(format!(
            "a {op} is in progress in the main checkout {}",
            main.display()
        ));
    } else if main_dirty > 0 {
        refusal = Some(format!(
            "the main checkout {} has {}: commit or stash them there first",
            main.display(),
            plural(main_dirty, "uncommitted change")
        ));
    } else if ahead == 0 {
        refusal = Some(format!(
            "{branch} has nothing {base} lacks: nothing to merge"
        ));
    } else if behind > 0
        || !git
            .raw(
                &[
                    "merge-base",
                    "--is-ancestor",
                    &format!("refs/heads/{base}"),
                    &branch_sha,
                ],
                64,
            )
            .is_ok_and(|o| o.success())
    {
        needs_rebase = true;
        refusal = Some(format!(
            "{base} has {} {branch} lacks, so it cannot fast-forward: rebase {branch} onto {base} first",
            plural(behind as usize, "commit")
        ));
    }
    let mut warnings = Vec::new();
    if !review.clean() {
        warnings.push(
            "the worktree has uncommitted changes or untracked files: they are not merged (commit them first)"
                .to_owned(),
        );
    }
    let suspects = if branch_sha.is_empty() || !base_is_branch {
        Vec::new()
    } else {
        landed_suspects(&git, &target.dir, &base, &branch_sha)
    };
    let held: Vec<&str> = suspects
        .iter()
        .filter(|s| s.excluded)
        .map(|s| s.path.as_str())
        .collect();
    if !held.is_empty() {
        let what = format!(
            "{branch} adds {} that look{} like a secret, an agent's personal settings, or build output: {}",
            plural(held.len(), "path"),
            if held.len() == 1 { "s" } else { "" },
            held.join(", ")
        );
        if include_suspects {
            warnings.push(format!("{what} (they land anyway, as asked)"));
        } else if refusal.is_none() {
            refusal = Some(format!(
                "{what}. Remove them from the branch (git rm --cached, commit), or land them anyway (--include-suspects; in the hub, \"Land them anyway\")"
            ));
        }
    }
    let command = Planned::new(
        &tools.git,
        &main,
        // The commit planned, not the branch: a commit landing on it after
        // the plan is not merged unreviewed.
        vec!["merge".into(), "--ff-only".into(), branch_sha.clone()],
        MERGE_TIMEOUT,
    );
    Ok(Plan {
        action: "merge",
        summary: format!(
            "Fast-forward {base} to {branch} ({}) in the main checkout {}",
            plural(ahead as usize, "commit"),
            main.display()
        ),
        details: review
            .commits
            .iter()
            .map(|c| format!("{} {}", c.short, c.subject))
            .collect(),
        warnings,
        commands: vec![command],
        fingerprint: fingerprint(&[
            "merge",
            if include_suspects {
                "include-suspects"
            } else {
                ""
            },
            &branch_sha,
            &main_head,
            main_branch.as_deref().unwrap_or_default(),
            &String::from_utf8_lossy(&main_status),
        ]),
        refusal,
        needs_rebase,
        defaults: None,
        contents: Vec::new(),
        suspects,
        excluded: Vec::new(),
        files: None,
    })
}
