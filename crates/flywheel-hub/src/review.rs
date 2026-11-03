//! Review from anywhere: a session's git work -- its branch against its
//! base (commits, changed files with +/- counts, per-file diffs) and its
//! working tree (staged, unstaged, untracked) -- and guarded actions on
//! it: commit all, push the branch, open a pull request (`gh`), rebase
//! the branch onto its base, and fast-forward the base onto the branch in
//! the main checkout.
//!
//! Every git and gh call goes through [`runner::run`] (argv only, no
//! terminal, capped output, a deadline, bounded concurrency). Blocking:
//! call off the async executor.
//!
//! Actions are two steps. A dry run returns a [`Plan`]: the exact commands
//! (and the directory each runs in), what they will do, why they cannot
//! run now if so, and a fingerprint of the state it was planned against.
//! Running needs that fingerprint back; when the repository moved since
//! (a commit, an edit, another push) the plan is stale and nothing runs,
//! so what runs is what was confirmed.

pub mod runner;
mod session;

pub use session::{Action, ActionReport, AgentRef, AskKind, AskPlan, SessionReview};

use crate::git::WorktreeRef;
use runner::{Output, RunError, Spec};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Commits listed (newest first); more are counted, not listed.
pub const MAX_COMMITS: usize = 200;
/// Files listed per set (committed, staged, unstaged, untracked).
pub const MAX_FILES: usize = 1000;
/// Diff text read per file (the rest is cut and said so).
pub const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// Diff lines served per page ("load more" asks for the next).
pub const DIFF_PAGE_LINES: usize = 2000;
/// Output kept per command for an action's report.
const RAN_OUTPUT_BYTES: usize = 8 * 1024;
/// stdout read by the read-only git calls.
const READ_LIMIT: usize = 4 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Commit hooks and push to a slow remote can take a while; still bounded.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);
const REBASE_TIMEOUT: Duration = Duration::from_secs(120);
const MERGE_TIMEOUT: Duration = Duration::from_secs(120);
const GH_TIMEOUT: Duration = Duration::from_secs(60);
const GH_STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest commit message / PR title / PR body accepted.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_TITLE_CHARS: usize = 256;

/// The programs review runs (names searched on `PATH`, or paths).
#[derive(Debug, Clone)]
pub struct Tools {
    pub git: String,
    pub gh: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            git: "git".into(),
            gh: "gh".into(),
        }
    }
}

/// What to review: the work tree, its base, and (for a worktree session)
/// the worktree record.
#[derive(Debug, Clone)]
pub struct Target {
    /// The work tree's top directory.
    pub dir: PathBuf,
    /// What the branch is compared against: a worktree's base, a base the
    /// caller named, else the branch's upstream (resolved by [`read`]).
    pub base: Option<String>,
    pub base_source: BaseSource,
    pub worktree: Option<WorktreeRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    /// The base the session's worktree was cut from.
    Worktree,
    /// Named by the caller (`?base=`).
    Given,
    /// The branch's upstream (no worktree, no base named).
    Upstream,
    /// Nothing to compare against.
    None,
}

/// Why an action did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// The request itself is wrong (400).
    Invalid(String),
    /// The repository's state refuses it, it is stale, or a command failed
    /// (409).
    Conflict(String),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Conflict(m) => f.write_str(m),
        }
    }
}

impl From<RunError> for ActionError {
    fn from(e: RunError) -> Self {
        Self::Conflict(e.to_string())
    }
}

// --- the review ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    /// Author date, ISO 8601.
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// `added`, `modified`, `deleted`, `renamed`, `copied`, `type`
    /// (file type changed), `unmerged`.
    pub status: String,
    /// Lines added / removed; `None` for a binary file.
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
    pub binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileSet {
    pub files: Vec<FileChange>,
    pub insertions: u64,
    pub deletions: u64,
    /// More files changed than [`MAX_FILES`]; `files` lists the first.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaseView {
    pub name: String,
    pub source: BaseSource,
    /// What `name` resolves to now (`None`: it no longer does).
    pub sha: Option<String>,
    /// Where the branch left it: the diff of committed work starts here.
    pub merge_base: Option<String>,
    /// `name` is a local branch (merging into it is possible).
    pub is_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamView {
    /// `origin/feat`.
    pub name: String,
    pub remote: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Remote {
    pub name: String,
    /// The push URL, any credential in it masked.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Untracked {
    pub path: String,
}

/// A work tree's review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Review {
    pub dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRef>,
    /// The checked-out branch (`None`: detached HEAD).
    pub branch: Option<String>,
    /// HEAD's commit (`None`: no commits yet).
    pub head: Option<String>,
    pub base: Option<BaseView>,
    /// Commits on HEAD not in the base, and the other way round.
    pub ahead: u32,
    pub behind: u32,
    pub upstream: Option<UpstreamView>,
    pub remotes: Vec<Remote>,
    /// HEAD's commits the base lacks, newest first (at most
    /// [`MAX_COMMITS`]; `ahead` counts them all).
    pub commits: Vec<Commit>,
    /// Committed changes: the base's merge base to HEAD.
    pub committed: FileSet,
    pub staged: FileSet,
    pub unstaged: FileSet,
    pub untracked: Vec<Untracked>,
    pub untracked_truncated: bool,
    /// Paths with unresolved conflicts.
    pub conflicts: Vec<String>,
    /// An operation stopped half-way: `rebase`, `merge`, `cherry-pick`,
    /// `revert`.
    pub operation: Option<String>,
    /// The branch a stopped rebase is rebasing (HEAD is detached
    /// meanwhile).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebasing: Option<String>,
}

impl Review {
    pub fn clean(&self) -> bool {
        self.staged.files.is_empty()
            && self.unstaged.files.is_empty()
            && self.untracked.is_empty()
            && self.conflicts.is_empty()
    }
}

/// A git call in `dir`.
struct Git<'a> {
    program: &'a str,
    dir: &'a Path,
}

impl<'a> Git<'a> {
    fn new(tools: &'a Tools, dir: &'a Path) -> Self {
        Self {
            program: &tools.git,
            dir,
        }
    }

    fn spec(&self, args: &[&str], limit: usize, timeout: Duration) -> Spec<'a> {
        Spec {
            program: self.program,
            dir: self.dir,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            stdin: None,
            timeout,
            limit,
        }
    }

    /// Run a read-only git command (no optional locks), any exit.
    fn raw(&self, args: &[&str], limit: usize) -> Result<Output, String> {
        let mut full = vec!["--no-optional-locks"];
        full.extend_from_slice(args);
        runner::run(&self.spec(&full, limit, READ_TIMEOUT)).map_err(|e| e.to_string())
    }

    /// stdout of a read-only command that must succeed (trailing newline
    /// trimmed).
    fn out(&self, args: &[&str]) -> Result<String, String> {
        let output = self.raw(args, READ_LIMIT)?;
        if !output.success() {
            return Err(output.error_text());
        }
        Ok(output.stdout_text().trim_end().to_owned())
    }

    /// stdout of a read-only command, `None` when it fails.
    fn maybe(&self, args: &[&str]) -> Option<String> {
        self.out(args).ok().filter(|s| !s.is_empty())
    }
}

/// The top directory of the work tree `dir` is in; `None` outside git.
pub fn toplevel(tools: &Tools, dir: &Path) -> Option<PathBuf> {
    Git::new(tools, dir)
        .maybe(&["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
}

/// Whether `base` names a commit in the repository `dir` is in.
pub fn resolves(tools: &Tools, dir: &Path, base: &str) -> bool {
    Git::new(tools, dir)
        .maybe(&["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .is_some()
}

/// Mask the credential in a remote URL (`https://user:token@host/...`).
fn mask_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((auth, host)) = rest.split_once('@')
        && !auth.contains('/')
    {
        return format!("{scheme}://***@{host}");
    }
    url.to_owned()
}

/// The host a remote URL names (`github.com` from `git@github.com:o/r` or
/// `https://github.com/o/r`).
fn url_host(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        // scp-like `user@host:path`.
        None if !url.starts_with('/') && url.contains(':') => url,
        None => return None,
    };
    let host = rest.split(['/', ':']).next()?;
    let host = host.rsplit('@').next()?;
    (!host.is_empty()).then(|| host.to_owned())
}

fn status_word(code: &str) -> &'static str {
    match code.chars().next() {
        Some('A') => "added",
        Some('D') => "deleted",
        Some('R') => "renamed",
        Some('C') => "copied",
        Some('T') => "type",
        Some('U') => "unmerged",
        _ => "modified",
    }
}

/// Parse `git diff --numstat -z` and `--name-status -z` of one range.
fn parse_changes(numstat: &[u8], name_status: &[u8]) -> Vec<FileChange> {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    // name-status: STATUS \0 path \0, or STATUS \0 old \0 new \0 (R/C).
    let mut statuses: Vec<(String, Option<String>, String)> = Vec::new();
    let mut tokens = name_status.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(code) = tokens.next() {
        let code = text(code);
        if code.starts_with(['R', 'C']) {
            let (Some(old), Some(new)) = (tokens.next(), tokens.next()) else {
                break;
            };
            statuses.push((code, Some(text(old)), text(new)));
        } else {
            let Some(path) = tokens.next() else { break };
            statuses.push((code, None, text(path)));
        }
    }
    // numstat: ADD \t DEL \t path \0, or ADD \t DEL \t \0 old \0 new \0.
    let mut counts: HashMap<String, (Option<u32>, Option<u32>)> = HashMap::new();
    let mut tokens = numstat.split(|b| *b == 0);
    while let Some(head) = tokens.next() {
        if head.is_empty() {
            continue;
        }
        let head = text(head);
        let mut parts = head.splitn(3, '\t');
        let (Some(add), Some(del), path) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = match path {
            Some(p) if !p.is_empty() => p.to_owned(),
            _ => {
                let _old = tokens.next();
                match tokens.next() {
                    Some(new) => text(new),
                    None => break,
                }
            }
        };
        counts.insert(path, (add.parse().ok(), del.parse().ok()));
    }
    statuses
        .into_iter()
        .map(|(code, old_path, path)| {
            let (insertions, deletions) = counts.get(&path).copied().unwrap_or((None, None));
            FileChange {
                binary: insertions.is_none() && deletions.is_none() && counts.contains_key(&path),
                status: status_word(&code).to_owned(),
                path,
                old_path,
                insertions,
                deletions,
            }
        })
        .collect()
}

const DIFF_FLAGS: &[&str] = &["--no-color", "--no-ext-diff", "--no-textconv", "-M"];

/// The changes of `range` (`["--cached"]`, `[]`, `[from, to]`).
fn file_set(git: &Git<'_>, range: &[&str]) -> Result<FileSet, String> {
    let run = |what: &str| -> Result<Vec<u8>, String> {
        let mut args = vec!["diff", what, "-z"];
        args.extend_from_slice(DIFF_FLAGS);
        args.extend_from_slice(range);
        let output = git.raw(&args, READ_LIMIT)?;
        if !output.success() {
            return Err(output.error_text());
        }
        Ok(output.stdout)
    };
    let mut files = parse_changes(&run("--numstat")?, &run("--name-status")?);
    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    Ok(FileSet {
        insertions: files
            .iter()
            .filter_map(|f| f.insertions)
            .map(u64::from)
            .sum(),
        deletions: files
            .iter()
            .filter_map(|f| f.deletions)
            .map(u64::from)
            .sum(),
        files,
        truncated,
    })
}

fn nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| String::from_utf8_lossy(t).into_owned())
        .collect()
}

/// The operation stopped half-way in the work tree whose git dir is
/// `git_dir`.
fn operation(git_dir: &Path) -> Option<String> {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        Some("rebase".into())
    } else if git_dir.join("MERGE_HEAD").exists() {
        Some("merge".into())
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        Some("cherry-pick".into())
    } else if git_dir.join("REVERT_HEAD").exists() {
        Some("revert".into())
    } else {
        None
    }
}

/// The branch a rebase stopped in the work tree whose git dir is
/// `git_dir` is rebasing.
fn rebasing(git_dir: &Path) -> Option<String> {
    ["rebase-merge", "rebase-apply"].iter().find_map(|d| {
        let name = std::fs::read_to_string(git_dir.join(d).join("head-name")).ok()?;
        let name = name.trim();
        Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_owned())
            .filter(|n| !n.is_empty() && n != "detached HEAD")
    })
}

/// `git rev-list --left-right --count a...b` as (left, right).
fn left_right(git: &Git<'_>, range: &str) -> (u32, u32) {
    git.maybe(&["rev-list", "--left-right", "--count", range])
        .map(|counts| {
            let mut parts = counts.split_whitespace();
            (
                parts.next().and_then(|n| n.parse().ok()).unwrap_or(0),
                parts.next().and_then(|n| n.parse().ok()).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0))
}

fn remotes(git: &Git<'_>) -> Vec<Remote> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in git.maybe(&["remote", "-v"]).unwrap_or_default().lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), Some("(push)")) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if seen.insert(name.to_owned()) {
            out.push(Remote {
                name: name.to_owned(),
                url: mask_url(url),
            });
        }
    }
    out
}

fn upstream(git: &Git<'_>) -> Option<UpstreamView> {
    let name = git.maybe(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ])?;
    let branch = git.maybe(&["symbolic-ref", "-q", "--short", "HEAD"])?;
    let remote = git
        .maybe(&["config", "--get", &format!("branch.{branch}.remote")])
        .unwrap_or_else(|| name.split('/').next().unwrap_or_default().to_owned());
    let (behind, ahead) = left_right(git, "@{upstream}...HEAD");
    Some(UpstreamView {
        name,
        remote,
        ahead,
        behind,
    })
}

/// Read the review of `target`.
pub fn read(tools: &Tools, target: &Target) -> Result<Review, String> {
    let git = Git::new(tools, &target.dir);
    let head = git.maybe(&["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    let branch = git.maybe(&["symbolic-ref", "-q", "--short", "HEAD"]);
    let git_dir = git
        .out(&["rev-parse", "--absolute-git-dir"])
        .map(PathBuf::from)
        .map_err(|e| format!("{} is not a git work tree: {e}", target.dir.display()))?;
    let upstream = upstream(&git);
    let base_name = match (&target.base, target.base_source) {
        (Some(base), _) => Some((base.clone(), target.base_source)),
        (None, _) => upstream
            .as_ref()
            .map(|u| (u.name.clone(), BaseSource::Upstream)),
    };
    let mut review = Review {
        dir: target.dir.clone(),
        worktree: target.worktree.clone(),
        branch,
        head: head.clone(),
        base: None,
        ahead: 0,
        behind: 0,
        upstream,
        remotes: remotes(&git),
        commits: Vec::new(),
        committed: FileSet::default(),
        staged: FileSet::default(),
        unstaged: FileSet::default(),
        untracked: Vec::new(),
        untracked_truncated: false,
        conflicts: Vec::new(),
        operation: operation(&git_dir),
        rebasing: rebasing(&git_dir),
    };
    if let Some((name, source)) = base_name {
        let sha = git.maybe(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]);
        let merge_base = match (&sha, &head) {
            (Some(sha), Some(_)) => git.maybe(&["merge-base", sha, "HEAD"]),
            _ => None,
        };
        let is_branch = git
            .raw(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{name}"),
                ],
                64,
            )
            .is_ok_and(|o| o.success());
        if let (Some(sha), Some(_)) = (&sha, &head) {
            let (behind, ahead) = left_right(&git, &format!("{sha}...HEAD"));
            review.ahead = ahead;
            review.behind = behind;
            let log = git.raw(
                &[
                    "log",
                    "--no-color",
                    "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s%x1e",
                    &format!("-n{}", MAX_COMMITS),
                    "HEAD",
                    &format!("^{sha}"),
                ],
                READ_LIMIT,
            )?;
            review.commits = String::from_utf8_lossy(&log.stdout)
                .split('\x1e')
                .filter_map(|record| {
                    let mut f = record.trim_start_matches('\n').split('\x1f');
                    Some(Commit {
                        sha: f.next().filter(|s| !s.is_empty())?.to_owned(),
                        short: f.next()?.to_owned(),
                        author: f.next()?.to_owned(),
                        date: f.next()?.to_owned(),
                        subject: f.next().unwrap_or_default().to_owned(),
                    })
                })
                .collect();
        }
        if let Some(from) = &merge_base {
            review.committed = file_set(&git, &[from, "HEAD"])?;
        }
        review.base = Some(BaseView {
            name,
            source,
            sha,
            merge_base,
            is_branch,
        });
    }
    review.staged = file_set(&git, &["--cached"])?;
    review.unstaged = file_set(&git, &[])?;
    let untracked = git.raw(
        &["ls-files", "-z", "--others", "--exclude-standard"],
        READ_LIMIT,
    )?;
    let mut untracked = nul_list(&untracked.stdout);
    review.untracked_truncated = untracked.len() > MAX_FILES;
    untracked.truncate(MAX_FILES);
    review.untracked = untracked
        .into_iter()
        .map(|path| Untracked { path })
        .collect();
    let conflicts = git.raw(
        &["diff", "--name-only", "-z", "--diff-filter=U"],
        READ_LIMIT,
    )?;
    review.conflicts = nul_list(&conflicts.stdout);
    // An unmerged path shows in both sets; it is listed once, as a
    // conflict.
    let conflicted: HashSet<&String> = review.conflicts.iter().collect();
    review
        .unstaged
        .files
        .retain(|f| !conflicted.contains(&f.path));
    review
        .staged
        .files
        .retain(|f| !conflicted.contains(&f.path));
    Ok(review)
}

// --- per-file diffs ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The branch's commits (merge base to HEAD).
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

impl Scope {
    fn word(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "committed" => Ok(Self::Committed),
            "staged" => Ok(Self::Staged),
            "unstaged" => Ok(Self::Unstaged),
            "untracked" => Ok(Self::Untracked),
            other => Err(format!(
                "scope must be committed, staged, unstaged, or untracked, not {other:?}"
            )),
        }
    }
}

/// One page of a file's unified diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffPage {
    pub path: String,
    pub scope: Scope,
    pub binary: bool,
    /// Patch lines (headers included), from `offset`.
    pub lines: Vec<String>,
    pub offset: usize,
    /// The next page's offset; `None` on the last.
    pub next: Option<usize>,
    pub total_lines: usize,
    /// The diff was longer than [`MAX_DIFF_BYTES`] and was cut there.
    pub truncated: bool,
}

/// A path as the review listed it: relative, inside the work tree.
fn check_path(raw: &str) -> Result<&str, String> {
    let bad = raw.is_empty()
        || raw.contains('\0')
        || Path::new(raw)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if bad {
        return Err(format!("{raw:?} is not a path inside the work tree"));
    }
    Ok(raw)
}

/// Page `offset` of the diff of `path` (`old_path`: a rename's source) in
/// `scope`.
pub fn diff(
    tools: &Tools,
    target: &Target,
    scope: Scope,
    path: &str,
    old_path: Option<&str>,
    offset: usize,
) -> Result<DiffPage, String> {
    let path = check_path(path)?;
    let old_path = old_path
        .filter(|p| !p.is_empty())
        .map(check_path)
        .transpose()?;
    let git = Git::new(tools, &target.dir);
    let literal = |p: &str| format!(":(literal){p}");
    let range: Vec<String> = match scope {
        Scope::Committed => vec![read_base_only(&git, target)?, "HEAD".into()],
        Scope::Staged => vec!["--cached".into()],
        Scope::Unstaged | Scope::Untracked => Vec::new(),
    };
    let mut args: Vec<String> = vec!["diff".into()];
    args.extend(DIFF_FLAGS.iter().map(|f| (*f).to_owned()));
    let any_exit = scope == Scope::Untracked;
    let path = if scope == Scope::Untracked {
        // The file must be untracked and in the tree (not through a
        // symlinked directory to elsewhere).
        let full = target.dir.join(path);
        let parent_ok = full
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .zip(target.dir.canonicalize().ok())
            .is_some_and(|(p, top)| p.starts_with(top));
        let listed = git
            .raw(
                &[
                    "ls-files",
                    "-z",
                    "--others",
                    "--exclude-standard",
                    "--",
                    &literal(path),
                ],
                64 * 1024,
            )
            .is_ok_and(|o| nul_list(&o.stdout).iter().any(|p| p == path));
        if !parent_ok || full.symlink_metadata().is_err() || !listed {
            return Err(format!("{path} is not an untracked file here"));
        }
        args.retain(|a| a != "-M");
        args.extend([
            "--no-index".into(),
            "--".into(),
            "/dev/null".into(),
            path.into(),
        ]);
        path.to_owned()
    } else {
        // The path must be one the scope changes; a rename is diffed as
        // one (asked by either name), as the file list shows it.
        let mut list: Vec<String> = vec!["diff".into(), "--name-status".into(), "-z".into()];
        list.extend(DIFF_FLAGS.iter().map(|f| (*f).to_owned()));
        list.extend(range.iter().cloned());
        let list: Vec<&str> = list.iter().map(String::as_str).collect();
        let listed = git.raw(&list, READ_LIMIT)?;
        if !listed.success() {
            return Err(listed.error_text());
        }
        let changes = parse_changes(&[], &listed.stdout);
        let change = changes
            .iter()
            .find(|c| c.path == path && old_path.is_none_or(|o| c.old_path.as_deref() == Some(o)))
            .or_else(|| {
                changes
                    .iter()
                    .find(|c| old_path.is_none() && c.old_path.as_deref() == Some(path))
            })
            .ok_or_else(|| format!("{path} has no {} change", scope.word()))?;
        args.extend(range);
        args.push("--".into());
        args.push(literal(&change.path));
        if let Some(old) = &change.old_path {
            args.push(literal(old));
        }
        change.path.clone()
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git.raw(&refs, MAX_DIFF_BYTES)?;
    // --no-index exits 1 when the files differ.
    let failed = if any_exit {
        !matches!(output.code, Some(0 | 1))
    } else {
        !output.success()
    };
    if failed {
        return Err(output.error_text());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut all: Vec<&str> = text.split('\n').collect();
    if all.last() == Some(&"") {
        all.pop();
    }
    if output.truncated {
        // A line cut half-way is dropped.
        all.pop();
    }
    let binary = all
        .iter()
        .take(12)
        .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"));
    let total_lines = all.len();
    let offset = offset.min(total_lines);
    let end = (offset + DIFF_PAGE_LINES).min(total_lines);
    Ok(DiffPage {
        path,
        scope,
        binary,
        lines: all[offset..end].iter().map(|l| (*l).to_owned()).collect(),
        offset,
        next: (end < total_lines).then_some(end),
        total_lines,
        truncated: output.truncated,
    })
}

/// The merge base committed work is diffed from.
fn read_base_only(git: &Git<'_>, target: &Target) -> Result<String, String> {
    let base = match &target.base {
        Some(base) => base.clone(),
        None => git
            .maybe(&[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ])
            .ok_or("there is no base to compare against: name one")?,
    };
    let sha = git
        .maybe(&["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .ok_or_else(|| format!("base {base} names no commit"))?;
    git.maybe(&["merge-base", &sha, "HEAD"])
        .ok_or_else(|| format!("HEAD and {base} share no history"))
}

// --- plans ---------------------------------------------------------------

/// One command an action runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Planned {
    /// Where it runs.
    pub dir: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// How it reads in a shell (quoted), with what it reads on stdin
    /// named after `<`.
    pub display: String,
    #[serde(skip)]
    stdin: Option<Vec<u8>>,
    #[serde(skip)]
    timeout: Duration,
}

impl Planned {
    fn new(program: &str, dir: &Path, args: Vec<String>, timeout: Duration) -> Self {
        let dir_text = dir.to_string_lossy().into_owned();
        let name = program_name(program);
        let shown = if name == "git" {
            let mut with_dir = vec!["-C".to_owned(), dir_text];
            with_dir.extend(args.iter().cloned());
            runner::display("git", &with_dir)
        } else {
            format!(
                "(cd {} && {})",
                runner::quote(&dir_text),
                runner::display(name, &args)
            )
        };
        Self {
            dir: dir.to_path_buf(),
            program: program.to_owned(),
            args,
            display: shown,
            stdin: None,
            timeout,
        }
    }

    fn with_stdin(mut self, bytes: Vec<u8>, what: &str) -> Self {
        self.display = format!("{} < {what}", self.display);
        self.stdin = Some(bytes);
        self
    }
}

/// `gh` for showing, whatever path it was resolved at.
fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

/// What an action would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub action: &'static str,
    /// One line: what happens.
    pub summary: String,
    /// What it touches: files committed, commits pushed or merged.
    pub details: Vec<String>,
    /// Worth knowing, not stopping it.
    pub warnings: Vec<String>,
    /// Run in order; the first failure stops the rest.
    pub commands: Vec<Planned>,
    /// Hand back to run it: the state it was planned against.
    pub fingerprint: String,
    /// Why it cannot run now (`commands` show what it would run).
    pub refusal: Option<String>,
    /// Merge: the branch is not a fast-forward of the base; rebase first.
    pub needs_rebase: bool,
    /// Pull request: the title and body offered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<PrDefaults>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrDefaults {
    pub title: String,
    pub body: String,
    pub base: String,
}

/// One command an action ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ran {
    pub display: String,
    pub code: Option<i32>,
    /// stdout then stderr, trimmed (at most 8 KiB).
    pub output: String,
}

/// What an action did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub action: &'static str,
    /// It did what was planned (a rebase stopped on conflicts did not).
    pub ok: bool,
    pub summary: String,
    pub ran: Vec<Ran>,
    /// A rebase stopped on these; it is left in progress.
    pub conflicts: Vec<String>,
    /// The pull request opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HEAD of the directory acted on, afterwards.
    pub head: Option<String>,
}

fn fingerprint(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Everything that makes a work tree's state: HEAD, branch, index, every
/// change (untracked files included) and what each changed file holds
/// now, so an edit after the review -- even one keeping the same line
/// counts -- makes the plan stale.
fn tree_state(git: &Git<'_>) -> Result<String, String> {
    let status = git.raw(
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
        READ_LIMIT,
    )?;
    if !status.success() {
        return Err(status.error_text());
    }
    if status.truncated {
        return Err("too many changes to plan against (git status is cut)".into());
    }
    let mut state = String::from_utf8_lossy(&status.stdout).into_owned();
    for path in status_paths(&status.stdout) {
        state.push('\u{1}');
        state.push_str(&path);
        state.push('\u{2}');
        state.push_str(&content_hash(&git.dir.join(&path)));
    }
    Ok(state)
}

/// The work-tree paths `git status --porcelain=v2 -z` names (changed,
/// unmerged, untracked).
fn status_paths(status: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut tokens = status.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(entry) = tokens.next() {
        let entry = String::from_utf8_lossy(entry);
        // Fields before the path: 1 -> 8, 2 -> 9 (then the source path as
        // its own token), u -> 10, ? -> 1.
        let (fields, renamed) = match entry.as_bytes().first() {
            Some(b'1') => (8, false),
            Some(b'2') => (9, true),
            Some(b'u') => (10, false),
            Some(b'?') => (1, false),
            _ => continue,
        };
        if let Some(path) = entry.splitn(fields + 1, ' ').nth(fields) {
            paths.push(path.to_owned());
        }
        if renamed {
            tokens.next();
        }
    }
    paths
}

/// Files larger than this are known by size and modification time, not
/// read through.
const HASH_FILE_LIMIT: u64 = 64 * 1024 * 1024;

/// What a file holds: a SHA-256 of its bytes (a symlink: of its target;
/// a huge file: its size and modification time), or why it has none
/// (gone, a directory such as a submodule).
fn content_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let Ok(meta) = path.symlink_metadata() else {
        return "gone".into();
    };
    let mut digest = Sha256::new();
    if meta.file_type().is_symlink() {
        match std::fs::read_link(path) {
            Ok(to) => digest.update(to.to_string_lossy().as_bytes()),
            Err(_) => return "unreadable link".into(),
        }
    } else if meta.is_file() && meta.len() > HASH_FILE_LIMIT {
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        return format!("large {} {modified}", meta.len());
    } else if meta.is_file() {
        let Ok(mut file) = std::fs::File::open(path) else {
            return format!("unreadable {}", meta.len());
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => digest.update(&buf[..n]),
                Err(_) => return format!("unreadable {}", meta.len()),
            }
        }
    } else {
        return "not a file".into();
    }
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn change_line(prefix: &str, f: &FileChange) -> String {
    let counts = match (f.insertions, f.deletions) {
        (Some(a), Some(d)) => format!(" +{a} -{d}"),
        _ if f.binary => " binary".to_owned(),
        _ => String::new(),
    };
    match &f.old_path {
        Some(old) => format!("{prefix}{} {old} -> {}{counts}", f.status, f.path),
        None => format!("{prefix}{} {}{counts}", f.status, f.path),
    }
}

/// Plan committing every change (staged, unstaged, untracked) with
/// `message`.
pub fn plan_commit(tools: &Tools, target: &Target, message: &str) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let state = tree_state(&git)?;
    let mut refusal = None;
    if let Some(op) = &review.operation {
        refusal = Some(format!(
            "a {op} is in progress in {}; finish or abort it in the terminal first",
            target.dir.display()
        ));
    } else if !review.conflicts.is_empty() {
        refusal = Some(format!(
            "{} unresolved: {}",
            plural(review.conflicts.len(), "conflict"),
            review.conflicts.join(", ")
        ));
    } else if review.clean() {
        refusal = Some("nothing to commit: the work tree is clean".into());
    } else if message.trim().is_empty() {
        refusal = Some("write a commit message".into());
    }
    let mut details = Vec::new();
    for f in &review.staged.files {
        details.push(change_line("staged ", f));
    }
    for f in &review.unstaged.files {
        details.push(change_line("", f));
    }
    for u in &review.untracked {
        details.push(format!("new {}", u.path));
    }
    // A file both staged and changed again since is one file.
    let files = review
        .staged
        .files
        .iter()
        .chain(&review.unstaged.files)
        .map(|f| f.path.as_str())
        .chain(review.untracked.iter().map(|u| u.path.as_str()))
        .collect::<HashSet<_>>()
        .len();
    let branch = review
        .branch
        .clone()
        .unwrap_or_else(|| "a detached HEAD".into());
    let mut warnings = Vec::new();
    if review.branch.is_none() {
        warnings.push("HEAD is detached: the commit is on no branch".into());
    }
    if review.untracked_truncated || review.unstaged.truncated || review.staged.truncated {
        warnings.push(format!(
            "more than {MAX_FILES} files change; the list is cut"
        ));
    }
    let dir = &target.dir;
    let add = Planned::new(
        &tools.git,
        dir,
        vec!["add".into(), "-A".into()],
        COMMIT_TIMEOUT,
    );
    let commit = Planned::new(
        &tools.git,
        dir,
        vec!["commit".into(), "-F".into(), "-".into()],
        COMMIT_TIMEOUT,
    )
    .with_stdin(message.as_bytes().to_vec(), "the message");
    Ok(Plan {
        action: "commit",
        summary: format!(
            "Stage all {} and commit them on {branch}",
            plural(files, "file")
        ),
        details,
        warnings,
        commands: vec![add, commit],
        fingerprint: fingerprint(&["commit", &state, message]),
        refusal,
        needs_rebase: false,
        defaults: None,
    })
}

/// Where a branch is pushed.
struct PushTo {
    remote: String,
    /// Always the same-named branch there (`refs/heads/<branch>`): a
    /// branch tracking another name (`origin/main`) is never pushed onto
    /// it.
    dest: String,
    /// The branch tracks exactly `remote`/`dest` already.
    tracked: bool,
    /// `-u`: it tracks nothing yet, or another branch on the same remote.
    set_upstream: bool,
    /// What it tracks now when that is not `remote`/`dest`.
    tracking: Option<String>,
}

/// Which remote, and which branch on it, `branch` is pushed to.
fn push_destination(
    git: &Git<'_>,
    branch: &str,
    remotes: &[Remote],
    wanted: Option<&str>,
) -> Result<PushTo, String> {
    let names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
    if names.is_empty() {
        return Err("the repository has no remotes: add one (git remote add)".into());
    }
    let config = |key: &str| git.maybe(&["config", "--get", key]);
    let upstream_remote = config(&format!("branch.{branch}.remote"));
    let upstream_merge = config(&format!("branch.{branch}.merge"));
    let remote = match wanted.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) if names.contains(&w) => w.to_owned(),
        Some(w) => {
            return Err(format!(
                "no remote named {w:?} (remotes: {})",
                names.join(", ")
            ));
        }
        None => config(&format!("branch.{branch}.pushRemote"))
            .or_else(|| config("remote.pushDefault"))
            .or_else(|| upstream_remote.clone())
            .filter(|r| names.contains(&r.as_str()))
            .or_else(|| names.contains(&"origin").then(|| "origin".to_owned()))
            .or_else(|| (names.len() == 1).then(|| names[0].to_owned()))
            .ok_or_else(|| format!("pick a remote: {}", names.join(", ")))?,
    };
    let dest = format!("refs/heads/{branch}");
    let same_remote = upstream_remote.as_deref() == Some(remote.as_str());
    let tracked = same_remote && upstream_merge.as_deref() == Some(dest.as_str());
    let tracking = match (&upstream_remote, &upstream_merge) {
        (Some(r), Some(m)) if !tracked => Some(format!(
            "{r}/{}",
            m.strip_prefix("refs/heads/").unwrap_or(m)
        )),
        _ => None,
    };
    Ok(PushTo {
        set_upstream: !tracked && (upstream_remote.is_none() || same_remote),
        remote,
        dest,
        tracked,
        tracking,
    })
}

/// Plan pushing the branch (never forced) to the same-named branch on
/// its push remote, else its upstream's, else `origin`, else the only
/// remote; `-u` when it tracks nothing there yet.
pub fn plan_push(tools: &Tools, target: &Target, remote: Option<&str>) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let head = review.head.clone().unwrap_or_default();
    let mut refusal = None;
    let mut commands = Vec::new();
    let mut details = Vec::new();
    let mut summary = "Push the branch".to_owned();
    let mut chosen = String::new();
    match &review.branch {
        None => refusal = Some("HEAD is detached: there is no branch to push".into()),
        Some(_) if review.head.is_none() => {
            refusal = Some("the branch has no commits yet".into());
        }
        Some(branch) => match push_destination(&git, branch, &review.remotes, remote) {
            Err(why) => refusal = Some(why),
            Ok(to) => {
                let name = &to.remote;
                let mut args = vec!["push".to_owned()];
                if to.set_upstream {
                    args.push("-u".into());
                }
                args.push(name.clone());
                args.push(format!("refs/heads/{branch}:{}", to.dest));
                commands.push(Planned::new(&tools.git, &target.dir, args, PUSH_TIMEOUT));
                let url = review
                    .remotes
                    .iter()
                    .find(|r| r.name == *name)
                    .map(|r| r.url.clone())
                    .unwrap_or_default();
                summary = format!("Push {branch} to {name}/{branch} ({url})");
                // What the remote has of it, as last fetched.
                let there = format!("{name}/{branch}");
                let known = git
                    .maybe(&[
                        "rev-parse",
                        "--verify",
                        "-q",
                        &format!("refs/remotes/{there}^{{commit}}"),
                    ])
                    .is_some();
                if known {
                    let (behind, ahead) = left_right(&git, &format!("refs/remotes/{there}...HEAD"));
                    if ahead == 0 && behind == 0 {
                        refusal = Some(format!("{branch} is up to date with {there}"));
                    } else {
                        details.push(format!(
                            "{} ahead of {there}",
                            plural(ahead as usize, "commit")
                        ));
                    }
                    if behind > 0 {
                        details.push(format!(
                            "{there} has {} this branch lacks: the push is refused unless you pull or rebase first (it is never forced)",
                            plural(behind as usize, "commit")
                        ));
                    }
                } else {
                    details.push(format!("creates {there}"));
                }
                match (&to.tracking, to.set_upstream) {
                    (_, false) if to.tracked => {}
                    (Some(old), true) => details.push(format!(
                        "sets {there} as the upstream of {branch} (it tracks {old} now, which is not pushed to)"
                    )),
                    (None, true) => {
                        details.push(format!("sets {there} as the upstream of {branch}"));
                    }
                    (Some(old), false) => {
                        details.push(format!("{branch} keeps tracking {old}"));
                    }
                    (None, false) => {}
                }
                chosen = format!("{name}\u{1}{}\u{1}{}", to.dest, to.set_upstream);
            }
        },
    }
    let mut warnings = Vec::new();
    if !review.clean() {
        warnings.push("uncommitted changes are not pushed (commit them first)".into());
    }
    Ok(Plan {
        action: "push",
        summary,
        details,
        warnings,
        commands,
        fingerprint: fingerprint(&["push", &head, &chosen]),
        refusal,
        needs_rebase: false,
        defaults: None,
    })
}

/// Whether `gh` is installed and signed in for `host`: `(installed,
/// authenticated, why not)`.
pub fn gh_status(tools: &Tools, dir: &Path, host: Option<&str>) -> (bool, bool, Option<String>) {
    let Some(program) = runner::find_program(&tools.gh) else {
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
    let program = runner::find_program(&tools.gh)
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
        summary: format!(
            "Open a pull request of {} into {}{}",
            branch.clone().unwrap_or_default(),
            base.clone().unwrap_or_else(|| "?".into()),
            host.map(|h| format!(" on {h}")).unwrap_or_default()
        ),
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
        commands: vec![command],
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
    })
}

/// The main checkout of the repository `dir` belongs to.
fn main_checkout(git: &Git<'_>) -> Result<PathBuf, String> {
    let list = git.out(&["worktree", "list", "--porcelain"])?;
    let mut blocks = list.split("\n\n");
    let first = blocks.next().unwrap_or_default();
    if first.lines().any(|l| l == "bare") {
        return Err(
            "the repository's main checkout is bare: there is no checkout to merge in".into(),
        );
    }
    first
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| "git listed no main checkout".into())
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
    })
}

/// Plan fast-forwarding the base onto a worktree's branch in the main
/// checkout (`git merge --ff-only`): refused when the main checkout is
/// not on the base, has uncommitted changes, or the branch is not a
/// fast-forward of the base (`needs_rebase`).
pub fn plan_merge(tools: &Tools, target: &Target) -> Result<Plan, String> {
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
    let command = Planned::new(
        &tools.git,
        &main,
        vec![
            "merge".into(),
            "--ff-only".into(),
            format!("refs/heads/{branch}"),
        ],
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
            &branch_sha,
            &main_head,
            main_branch.as_deref().unwrap_or_default(),
            &String::from_utf8_lossy(&main_status),
        ]),
        refusal,
        needs_rebase,
        defaults: None,
    })
}

fn refused(action: &'static str, why: &str) -> Plan {
    Plan {
        action,
        summary: why.to_owned(),
        details: Vec::new(),
        warnings: Vec::new(),
        commands: Vec::new(),
        fingerprint: fingerprint(&[action, "refused"]),
        refusal: Some(why.to_owned()),
        needs_rebase: false,
        defaults: None,
    }
}

// --- running a plan ------------------------------------------------------

/// Main checkouts (git common dirs) an action is running in: one at a
/// time per repository.
static BUSY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

struct RepoLock(PathBuf);

impl RepoLock {
    fn take(tools: &Tools, dir: &Path) -> Result<Self, ActionError> {
        let key = Git::new(tools, dir)
            .maybe(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.to_path_buf());
        let key = key.canonicalize().unwrap_or(key);
        let mut busy = BUSY.lock().unwrap_or_else(PoisonError::into_inner);
        if busy.contains(&key) {
            return Err(ActionError::Conflict(
                "another review action is running in this repository; wait for it".into(),
            ));
        }
        busy.push(key.clone());
        Ok(Self(key))
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        BUSY.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k| k != &self.0);
    }
}

/// Check a confirmed plan before running it.
fn confirmed(plan: &Plan, confirm: &str) -> Result<(), ActionError> {
    if let Some(why) = &plan.refusal {
        return Err(ActionError::Conflict(why.clone()));
    }
    if confirm.trim().is_empty() {
        return Err(ActionError::Invalid(
            "confirm the plan: pass the fingerprint a dry run returned".into(),
        ));
    }
    if plan.fingerprint != confirm.trim() {
        return Err(ActionError::Conflict(
            "the repository changed since this was planned: review it again and confirm the new plan".into(),
        ));
    }
    Ok(())
}

fn ran(command: &Planned, output: &Output) -> Ran {
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let err = output.stderr.trim();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err);
    }
    if text.len() > RAN_OUTPUT_BYTES {
        let mut cut = RAN_OUTPUT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[cut]");
    }
    Ran {
        display: command.display.clone(),
        code: output.code,
        output: text,
    }
}

fn exec(command: &Planned) -> Result<Output, RunError> {
    runner::run(&Spec {
        program: &command.program,
        dir: &command.dir,
        args: command.args.clone(),
        stdin: command.stdin.clone(),
        timeout: command.timeout,
        limit: READ_LIMIT,
    })
}

/// Run `plan`'s commands in order, stopping at the first failure (an
/// error naming the command and what it said).
fn run_plan(plan: &Plan) -> Result<Vec<(Ran, Output)>, ActionError> {
    let mut done = Vec::new();
    for command in &plan.commands {
        let output = exec(command)?;
        let record = ran(command, &output);
        if !output.success() {
            let how = if output.timed_out {
                "timed out"
            } else {
                "failed"
            };
            let before = if done.is_empty() {
                String::new()
            } else {
                format!(
                    " (after {})",
                    done.iter()
                        .map(|(r, _): &(Ran, Output)| r.display.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(ActionError::Conflict(format!(
                "{} {how}{before}: {}",
                command.display,
                output.error_text()
            )));
        }
        done.push((record, output));
    }
    Ok(done)
}

fn head_of(tools: &Tools, dir: &Path) -> Option<String> {
    Git::new(tools, dir).maybe(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
}

/// Planning itself failed (git could not read the repository).
fn unplannable(e: String) -> ActionError {
    ActionError::Conflict(e)
}

/// Commit every change with `message`, as confirmed.
pub fn commit(
    tools: &Tools,
    target: &Target,
    message: &str,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    check_message(message)?;
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_commit(tools, target, message).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan)?;
    let head = head_of(tools, &target.dir);
    let short = head.as_deref().map_or("?", |h| &h[..h.len().min(10)]);
    // What the commit holds, as git recorded it.
    let files = Git::new(tools, &target.dir)
        .raw(
            &[
                "diff-tree",
                "--no-commit-id",
                "--root",
                "-r",
                "--name-only",
                "-z",
                "HEAD",
            ],
            READ_LIMIT,
        )
        .map(|o| nul_list(&o.stdout).len())
        .unwrap_or_default();
    Ok(Outcome {
        action: "commit",
        ok: true,
        summary: format!("Committed {} as {short}", plural(files, "file")),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head,
    })
}

pub fn check_message(message: &str) -> Result<(), ActionError> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ActionError::Invalid(format!(
            "the message is longer than {} KiB",
            MAX_MESSAGE_BYTES / 1024
        )));
    }
    if message.contains('\0') {
        return Err(ActionError::Invalid("the message holds a NUL byte".into()));
    }
    Ok(())
}

/// Push the branch, as confirmed.
pub fn push(
    tools: &Tools,
    target: &Target,
    remote: Option<&str>,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_push(tools, target, remote).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan)?;
    Ok(Outcome {
        action: "push",
        ok: true,
        summary: plan.summary.replacen("Push ", "Pushed ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &target.dir),
    })
}

/// Open the pull request, as confirmed: its URL.
pub fn pr(
    tools: &Tools,
    target: &Target,
    request: &PrRequest,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    if let Some(title) = &request.title
        && (title.chars().count() > MAX_TITLE_CHARS || title.contains(['\n', '\r', '\0']))
    {
        return Err(ActionError::Invalid(format!(
            "the title must be one line of at most {MAX_TITLE_CHARS} characters"
        )));
    }
    if let Some(body) = &request.body {
        check_message(body)?;
    }
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_pr(tools, target, request).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan)?;
    let url = done.last().and_then(|(_, o)| {
        o.stdout_text()
            .lines()
            .rev()
            .find(|l| l.trim().starts_with("https://") || l.trim().starts_with("http://"))
            .map(|l| l.trim().to_owned())
    });
    Ok(Outcome {
        action: "pr",
        ok: true,
        summary: match &url {
            Some(url) => format!("Opened {url}"),
            None => "Opened the pull request".into(),
        },
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url,
        head: head_of(tools, &target.dir),
    })
}

/// Rebase onto the base, as confirmed. A conflict is an outcome (not an
/// error): the rebase is left stopped, `conflicts` names the files.
pub fn rebase(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_rebase(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let command = &plan.commands[0];
    let output = exec(command)?;
    let record = ran(command, &output);
    let head = head_of(tools, &target.dir);
    if output.success() {
        return Ok(Outcome {
            action: "rebase",
            ok: true,
            summary: plan.summary.replacen("Replay ", "Replayed ", 1),
            ran: vec![record],
            conflicts: Vec::new(),
            url: None,
            head,
        });
    }
    let git = Git::new(tools, &target.dir);
    let git_dir = git
        .maybe(&["rev-parse", "--absolute-git-dir"])
        .map(PathBuf::from);
    let stopped = git_dir.as_deref().and_then(operation).as_deref() == Some("rebase");
    if !stopped {
        return Err(ActionError::Conflict(format!(
            "{} failed: {}",
            command.display,
            output.error_text()
        )));
    }
    let conflicts = git
        .raw(
            &["diff", "--name-only", "-z", "--diff-filter=U"],
            READ_LIMIT,
        )
        .map(|o| nul_list(&o.stdout))
        .unwrap_or_default();
    Ok(Outcome {
        action: "rebase",
        ok: false,
        summary: if conflicts.is_empty() {
            "The rebase stopped (left in progress): see its output".into()
        } else {
            format!(
                "The rebase stopped on {} in {} (left in progress): resolve, git add, git rebase --continue",
                plural(conflicts.len(), "conflict"),
                conflicts.join(", ")
            )
        },
        ran: vec![record],
        conflicts,
        url: None,
        head,
    })
}

/// Abort the stopped rebase, as confirmed.
pub fn abort(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_abort(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan)?;
    Ok(Outcome {
        action: "abort",
        ok: true,
        summary: plan.summary.replacen("Abort ", "Aborted ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &target.dir),
    })
}

/// Fast-forward the base in the main checkout, as confirmed.
pub fn merge(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_merge(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan)?;
    let main = plan.commands[0].dir.clone();
    Ok(Outcome {
        action: "merge",
        ok: true,
        summary: plan.summary.replacen("Fast-forward ", "Fast-forwarded ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &main),
    })
}

#[cfg(test)]
mod tests;
