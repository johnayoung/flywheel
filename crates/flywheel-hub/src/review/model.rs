//! The read model: a work tree's review (branch against base, commits,
//! files, working tree, remotes) and a file's diff, one page at a time.

use super::{
    BaseSource, DIFF_PAGE_LINES, Git, MAX_COMMITS, MAX_DIFF_BYTES, MAX_FILES, READ_LIMIT, Target,
    Tools,
};
use crate::git::WorktreeRef;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

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

/// Mask the credential in a remote URL (`https://user:token@host/...`).
pub(super) fn mask_url(url: &str) -> String {
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
pub(super) fn url_host(url: &str) -> Option<String> {
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

pub(super) fn nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| String::from_utf8_lossy(t).into_owned())
        .collect()
}

/// The operation stopped half-way in the work tree whose git dir is
/// `git_dir`.
pub(super) fn operation(git_dir: &Path) -> Option<String> {
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
pub(super) fn left_right(git: &Git<'_>, range: &str) -> (u32, u32) {
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

/// The work tree's own state: what is staged, unstaged, untracked, and
/// conflicted (independent of the base).
struct WorkTreeState {
    staged: FileSet,
    unstaged: FileSet,
    untracked: Vec<String>,
    conflicts: Vec<String>,
}

/// Read [`WorkTreeState`]: its four reads run at once (each is a
/// read-only git command, `--no-optional-locks`).
fn work_tree_state(git: &Git<'_>) -> Result<WorkTreeState, String> {
    std::thread::scope(|scope| {
        let staged = scope.spawn(|| file_set(git, &["--cached"]));
        let unstaged = scope.spawn(|| file_set(git, &[]));
        let untracked = scope.spawn(|| {
            git.raw(
                &["ls-files", "-z", "--others", "--exclude-standard"],
                READ_LIMIT,
            )
            .map(|out| nul_list(&out.stdout))
        });
        let conflicts = git
            .raw(
                &["diff", "--name-only", "-z", "--diff-filter=U"],
                READ_LIMIT,
            )
            .map(|out| nul_list(&out.stdout));
        Ok(WorkTreeState {
            staged: join(staged)?,
            unstaged: join(unstaged)?,
            untracked: join(untracked)?,
            conflicts: conflicts?,
        })
    })
}

/// Read the review of `target`. The work tree's state is read alongside
/// the branch and base (independent git commands, run at once: a review
/// is a dozen git processes, and its latency their sum otherwise).
pub fn read(tools: &Tools, target: &Target) -> Result<Review, String> {
    let git = Git::new(tools, &target.dir);
    let (review, work_tree) = std::thread::scope(|scope| {
        let work_tree = scope.spawn(|| work_tree_state(&git));
        let review = read_branch(&git, target);
        (review, join(work_tree))
    });
    let mut review = review?;
    let work_tree = work_tree?;
    review.staged = work_tree.staged;
    review.unstaged = work_tree.unstaged;
    let mut untracked = work_tree.untracked;
    review.untracked_truncated = untracked.len() > MAX_FILES;
    untracked.truncate(MAX_FILES);
    review.untracked = untracked
        .into_iter()
        .map(|path| Untracked { path })
        .collect();
    review.conflicts = work_tree.conflicts;
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

/// A scoped git read's result (a panic in it is an error, not a crash).
fn join<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>) -> Result<T, String> {
    handle
        .join()
        .unwrap_or_else(|_| Err("a git read panicked".to_owned()))
}

/// The review's branch half: HEAD, branch, upstream, remotes, and the
/// base with its commits and committed changes (work-tree sets empty).
fn read_branch(git: &Git<'_>, target: &Target) -> Result<Review, String> {
    let head = git.maybe(&["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    let branch = git.maybe(&["symbolic-ref", "-q", "--short", "HEAD"]);
    let git_dir = git
        .out(&["rev-parse", "--absolute-git-dir"])
        .map(PathBuf::from)
        .map_err(|e| format!("{} is not a git work tree: {e}", target.dir.display()))?;
    let upstream = upstream(git);
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
        remotes: remotes(git),
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
            let (behind, ahead) = left_right(git, &format!("{sha}...HEAD"));
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
            review.committed = file_set(git, &[from, "HEAD"])?;
        }
        review.base = Some(BaseView {
            name,
            source,
            sha,
            merge_base,
            is_branch,
        });
    }
    Ok(review)
}

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
