//! The git plumbing the eval layer runs: a thin, checked wrapper over the
//! `git` CLI (the same tool the worker and landing use), never a library,
//! so what eval does to a repo is what an operator could type.

use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
#[error("git {args} (in {dir}): {detail}")]
pub struct GitError {
    pub args: String,
    pub dir: String,
    pub detail: String,
}

/// A git invocation with the environment eval wants: no pager, no prompts,
/// and none of the inherited `GIT_*` variables that would point it at
/// another repository.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (key, _) in std::env::vars_os() {
        if let Some(key) = key.to_str()
            && key.starts_with("GIT_")
            && key != "GIT_EXEC_PATH"
        {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null());
    cmd
}

fn fail(dir: &Path, args: &[&str], detail: String) -> GitError {
    GitError {
        args: args.join(" "),
        dir: dir.display().to_string(),
        detail,
    }
}

/// Run git in `dir`; its trimmed stdout, or an error carrying its stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = command(dir, args)
        .output()
        .map_err(|e| fail(dir, args, e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(fail(
            dir,
            args,
            if stderr.is_empty() {
                format!("exit {}", out.status)
            } else {
                stderr
            },
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether git in `dir` exits zero (stderr discarded).
pub fn ok(dir: &Path, args: &[&str]) -> bool {
    command(dir, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The full sha `rev` names in `repo`.
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String, GitError> {
    run(
        repo,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
}

/// Whether `ancestor` is an ancestor of (or equal to) `of`.
pub fn is_ancestor(repo: &Path, ancestor: &str, of: &str) -> bool {
    ok(repo, &["merge-base", "--is-ancestor", ancestor, of])
}

/// Point `name` at `sha` (creating or moving it).
pub fn update_ref(repo: &Path, name: &str, sha: &str) -> Result<(), GitError> {
    run(repo, &["update-ref", name, sha]).map(|_| ())
}

/// The branch `repo`'s work lands on: its remote's default branch, else the
/// branch checked out, else `main`.
pub fn default_branch(repo: &Path) -> String {
    run(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|r| r.strip_prefix("origin/").map(str::to_owned))
    .or_else(|| run(repo, &["symbolic-ref", "--short", "HEAD"]).ok())
    .filter(|b| !b.is_empty())
    .unwrap_or_else(|| "main".to_owned())
}

/// Point `branch` and its `origin/` twin at `sha`, so whatever measures
/// "the fork point with the base branch" finds `sha`.
pub fn pin_branch(repo: &Path, branch: &str, sha: &str) -> Result<(), GitError> {
    update_ref(repo, &format!("refs/heads/{branch}"), sha)?;
    update_ref(repo, &format!("refs/remotes/origin/{branch}"), sha)
}

/// Whether `sha` is a 40-hex commit id.
pub fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}
