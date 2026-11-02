//! Git for hub sessions: the sibling worktrees agent sessions can run in
//! (`<repo-parent>/<repo>.worktrees/<task>` on branch `<prefix><task>`,
//! never nested inside the repo), and the branch / dirty / ahead badges
//! sessions show.
//!
//! Every function here runs `git` (blocking): call off the async
//! executor. Badges are read through [`GitWatch`], a cache a background
//! thread refreshes (on agent status reports and on a slow timer), so
//! listing sessions never runs git.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// Longest task name (it is a directory and a branch name).
pub const MAX_TASK_CHARS: usize = 64;
/// How often the badges of running sessions are re-read without a nudge.
pub const SLOW_REFRESH: Duration = Duration::from_secs(20);
/// A nudged directory read this recently is not read again.
const NUDGE_DEBOUNCE: Duration = Duration::from_millis(750);
/// At most this many local branches are offered as bases.
const MAX_BRANCHES: usize = 200;

/// A session's worktree: where it runs, the workspace folder (repo) it
/// was cut from, and the branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRef {
    /// The workspace folder (a git work tree) the worktree belongs to.
    pub repo: PathBuf,
    /// The worktree's directory (canonical).
    pub path: PathBuf,
    pub branch: String,
    /// What the branch was cut from: a branch name, or a commit when the
    /// repo's HEAD was detached. Ahead/behind and "unmerged" count against
    /// it.
    pub base: String,
}

/// What [`plan`] resolved for a new worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreePlan {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

/// A directory's git state, as badges show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitStatus {
    /// The checked-out branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Tracked files with changes (staged or not), conflicts included.
    pub dirty: u32,
    /// Untracked files (ignored ones not counted).
    pub untracked: u32,
    /// Commits on HEAD not in `against`, and the other way round.
    pub ahead: u32,
    pub behind: u32,
    /// What ahead/behind count against: a worktree's base, else the
    /// branch's upstream; `None` (counts 0) when there is neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<String>,
}

/// Whether a worktree can be removed without losing work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovalCheck {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    /// The directory is gone already (only git's record is left).
    pub missing: bool,
    pub dirty: u32,
    pub untracked: u32,
    /// Commits on the branch that its base does not have.
    pub unmerged: u32,
}

impl RemovalCheck {
    /// Why removing it would lose work (`None`: it would not).
    pub fn refusal(&self) -> Option<String> {
        let mut why = Vec::new();
        if self.dirty > 0 {
            why.push(plural(self.dirty, "uncommitted change"));
        }
        if self.untracked > 0 {
            why.push(plural(self.untracked, "untracked file"));
        }
        if self.unmerged > 0 {
            why.push(format!(
                "{} not in {}",
                plural(self.unmerged, "commit"),
                self.base
            ));
        }
        (!why.is_empty()).then(|| {
            format!(
                "worktree {} has {}; commit and merge them first, or force the removal (uncommitted work is lost; the branch {} is kept)",
                self.path.display(),
                why.join(" and "),
                self.branch
            )
        })
    }
}

/// What [`remove`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Removed {
    pub path: PathBuf,
    pub branch: String,
    /// The branch was deleted too (it had nothing its base lacks); else it
    /// is kept so no commit is lost.
    pub branch_deleted: bool,
}

fn plural(n: u32, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Run `git -C dir args...`: its trimmed stdout, or its stderr as the
/// error. Never prompts (no terminal, no credential helpers asking).
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches("fatal: ")
                    .trim_start_matches("error: ")
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        Err(if message.is_empty() {
            format!("git {} failed ({})", args.join(" "), output.status)
        } else {
            message
        })
    }
}

/// Whether `git -C dir args...` succeeds (its output discarded).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Check a task name: it becomes a directory and (after the prefix) a
/// branch, so letters, digits, `.`, `_`, `-` only, starting with a letter
/// or digit, at most [`MAX_TASK_CHARS`].
pub fn validate_task(raw: &str) -> Result<String, String> {
    let task = raw.trim();
    if task.is_empty() {
        return Err("a worktree needs a task name".into());
    }
    if task.chars().count() > MAX_TASK_CHARS {
        return Err(format!(
            "task name is longer than {MAX_TASK_CHARS} characters"
        ));
    }
    if !task
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "task name {task:?} may only hold letters, digits, '.', '_' and '-' (it names a directory and a branch)"
        ));
    }
    if !task.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "task name {task:?} must start with a letter or digit"
        ));
    }
    if task.contains("..") || task.ends_with('.') || task.ends_with(".lock") {
        return Err(format!("task name {task:?} is not a valid branch name"));
    }
    Ok(task.to_owned())
}

/// Check a branch prefix (`fw hub serve --worktree-branch-prefix`): empty,
/// or ref-name characters ending in `/` or `-` (`agent/`, `wt-`).
pub fn validate_prefix(raw: &str) -> Result<String, String> {
    let prefix = raw.trim();
    if prefix.is_empty() {
        return Ok(String::new());
    }
    let ok_chars = prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    let ok_shape = prefix.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !prefix.contains("..")
        && !prefix.contains("//")
        && !prefix.contains("./")
        && !prefix.contains("/.");
    if !ok_chars || !ok_shape {
        return Err(format!(
            "branch prefix {prefix:?} is not a valid ref prefix"
        ));
    }
    Ok(prefix.to_owned())
}

/// The main checkout of the repo `repo` belongs to (also for a linked
/// worktree), as `(parent dir, repo name)`: worktrees go to
/// `<parent>/<name>.worktrees/`. A bare main repo (`x.git`) counts as `x`.
fn main_checkout(repo: &Path) -> Result<(PathBuf, String), String> {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|e| format!("{} is not a git repository: {e}", repo.display()))?;
    let common = PathBuf::from(common);
    let common = common.canonicalize().unwrap_or(common);
    let (root, name) = if common.file_name().is_some_and(|n| n == ".git") {
        let root = common
            .parent()
            .ok_or_else(|| format!("git dir {} has no parent", common.display()))?
            .to_path_buf();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("repo {} has no name", root.display()))?;
        (root, name)
    } else {
        let name = common
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("git dir {} has no name", common.display()))?;
        let name = name.strip_suffix(".git").unwrap_or(&name).to_owned();
        (common.clone(), name)
    };
    let parent = root
        .parent()
        .ok_or_else(|| format!("repo {} has no parent directory", root.display()))?
        .to_path_buf();
    Ok((parent, name))
}

/// The directory sibling worktrees of `repo` go in.
pub fn worktrees_dir(repo: &Path) -> Result<PathBuf, String> {
    let (parent, name) = main_checkout(repo)?;
    Ok(parent.join(format!("{name}.worktrees")))
}

/// Resolve a new worktree for `task` in `repo` without creating it: the
/// path (sibling of the main checkout, never inside a repo), branch
/// `<prefix><task>` (must not exist yet), and base (`base`, which must
/// name a commit, else the repo's current branch, else its HEAD commit).
pub fn plan(
    repo: &Path,
    task: &str,
    prefix: &str,
    base: Option<&str>,
) -> Result<WorktreePlan, String> {
    let task = validate_task(task)?;
    let dir = worktrees_dir(repo)?;
    let path = dir.join(&task);
    let repo_root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let (main_parent, main_name) = main_checkout(repo)?;
    let main_root = main_parent.join(&main_name);
    // By construction a sibling; checked anyway, since a worktree inside
    // a repo shows up in its status and its tooling.
    if path.starts_with(&repo_root) || path.starts_with(&main_root) {
        return Err(format!(
            "worktree {} would be inside the repo; refusing",
            path.display()
        ));
    }
    if path.exists() {
        return Err(format!(
            "{} already exists; pick another task name",
            path.display()
        ));
    }
    let branch = format!("{prefix}{task}");
    if !git_ok(repo, &["check-ref-format", "--branch", &branch]) {
        return Err(format!("{branch:?} is not a valid branch name"));
    }
    if git_ok(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    ) {
        return Err(format!(
            "branch {branch} already exists; pick another task name"
        ));
    }
    let base = match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(base) => {
            if base.starts_with('-') {
                return Err(format!("base {base:?} is not a branch or commit"));
            }
            git(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .map_err(|_| {
                format!(
                    "base {base:?} names no branch or commit in {}",
                    repo.display()
                )
            })?;
            base.to_owned()
        }
        None => current_base(repo)?,
    };
    Ok(WorktreePlan { path, branch, base })
}

/// The repo's current branch, else (detached) its HEAD commit.
fn current_base(repo: &Path) -> Result<String, String> {
    if let Ok(branch) = git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        && !branch.is_empty()
    {
        // An unborn branch (no commits yet) cannot be cut from.
        if !git_ok(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]) {
            return Err(format!(
                "{} has no commits yet; commit something before cutting a worktree",
                repo.display()
            ));
        }
        return Ok(branch);
    }
    git(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .map_err(|_| format!("{} has no commits yet", repo.display()))
}

/// Create the worktree [`plan`] resolved, on a new branch cut from its
/// base.
pub fn create(repo: &Path, plan: &WorktreePlan) -> Result<WorktreeRef, String> {
    if let Some(dir) = plan.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let path = plan.path.to_string_lossy().into_owned();
    git(
        repo,
        &["worktree", "add", "-b", &plan.branch, &path, &plan.base],
    )
    .map_err(|e| format!("git worktree add failed: {e}"))?;
    let path = plan
        .path
        .canonicalize()
        .unwrap_or_else(|_| plan.path.clone());
    Ok(WorktreeRef {
        repo: repo.to_path_buf(),
        path,
        branch: plan.branch.clone(),
        base: plan.base.clone(),
    })
}

/// Local branch names of `repo` (at most [`MAX_BRANCHES`]), for picking
/// a base.
pub fn branches(repo: &Path) -> Vec<String> {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "--sort=-committerdate",
            "refs/heads",
        ],
    )
    .map(|out| {
        out.lines()
            .filter(|l| !l.is_empty())
            .take(MAX_BRANCHES)
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// Read `dir`'s git state: branch, changed and untracked files, and
/// ahead/behind against `base` when given (else the upstream).
pub fn status(dir: &Path, base: Option<&str>) -> Result<GitStatus, String> {
    let out = git(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
    )?;
    let mut status = GitStatus {
        branch: None,
        dirty: 0,
        untracked: 0,
        ahead: 0,
        behind: 0,
        against: None,
    };
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = (head != "(detached)").then(|| head.to_owned());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            if base.is_none() {
                status.against = Some(upstream.to_owned());
            }
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            if base.is_none() {
                for part in ab.split_whitespace() {
                    if let Some(n) = part.strip_prefix('+') {
                        status.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix('-') {
                        status.behind = n.parse().unwrap_or(0);
                    }
                }
            }
        } else if line.starts_with("? ") {
            status.untracked += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            status.dirty += 1;
        }
    }
    if let Some(base) = base {
        // A base deleted since (or a repo without it) just counts nothing.
        if let Ok(counts) = git(
            dir,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ],
        ) {
            let mut parts = counts.split_whitespace();
            status.behind = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            status.ahead = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            status.against = Some(base.to_owned());
        }
    }
    Ok(status)
}

/// Commits on `branch` that `base` lacks; when `base` no longer resolves,
/// those no other local branch has.
fn unmerged(repo: &Path, branch: &str, base: &str) -> Result<u32, String> {
    let branch_ref = format!("refs/heads/{branch}");
    if !git_ok(repo, &["show-ref", "--verify", "--quiet", &branch_ref]) {
        return Ok(0);
    }
    let out = if git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ],
    ) {
        git(
            repo,
            &["rev-list", "--count", &format!("{base}..{branch_ref}")],
        )?
    } else {
        git(
            repo,
            &[
                "rev-list",
                "--count",
                &branch_ref,
                "--not",
                &format!("--exclude={branch_ref}"),
                "--branches",
            ],
        )?
    };
    Ok(out.trim().parse().unwrap_or(0))
}

/// What removing `worktree` would lose (fresh git calls).
pub fn removal_check(worktree: &WorktreeRef) -> Result<RemovalCheck, String> {
    let missing = !worktree.path.is_dir();
    let (dirty, untracked) = if missing {
        (0, 0)
    } else {
        let s = status(&worktree.path, None)?;
        (s.dirty, s.untracked)
    };
    let git_dir = if worktree.repo.is_dir() {
        &worktree.repo
    } else {
        &worktree.path
    };
    Ok(RemovalCheck {
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        base: worktree.base.clone(),
        missing,
        dirty,
        untracked,
        unmerged: unmerged(git_dir, &worktree.branch, &worktree.base)?,
    })
}

/// Remove `worktree` (refused when it holds work its base lacks, unless
/// `force`) and its branch when the branch has nothing its base lacks.
pub fn remove(worktree: &WorktreeRef, force: bool) -> Result<Removed, String> {
    let check = removal_check(worktree)?;
    if !force && let Some(why) = check.refusal() {
        return Err(why);
    }
    let repo = &worktree.repo;
    if !repo.is_dir() {
        return Err(format!(
            "repo {} is gone; remove {} by hand",
            repo.display(),
            worktree.path.display()
        ));
    }
    if check.missing {
        git(repo, &["worktree", "prune"])?;
    } else {
        let path = worktree.path.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        git(repo, &args).map_err(|e| format!("git worktree remove failed: {e}"))?;
    }
    let branch_deleted = check.unmerged == 0
        && git_ok(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", worktree.branch),
            ],
        )
        && git_ok(repo, &["branch", "-D", "--", &worktree.branch]);
    Ok(Removed {
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_deleted,
    })
}

// --- cached badges ------------------------------------------------------------

/// What a badge is read for: a directory and (for a worktree) its base.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BadgeKey {
    pub dir: PathBuf,
    pub base: Option<String>,
}

struct Badge {
    /// `None`: not a git work tree (or git failed).
    status: Option<GitStatus>,
    at: Instant,
}

/// Session badges, read by a background thread so listing never runs
/// git. [`GitWatch::get`] answers from the cache only.
#[derive(Default)]
pub struct GitWatch {
    cache: Mutex<HashMap<BadgeKey, Badge>>,
    wake: Mutex<Option<mpsc::Sender<Option<BadgeKey>>>>,
}

impl GitWatch {
    /// The cached badge for `key` (`None` until first read, or not git).
    pub fn get(&self, key: &BadgeKey) -> Option<GitStatus> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .and_then(|b| b.status.clone())
    }

    /// Ask the thread to re-read `key` soon (coalesced and debounced).
    pub fn nudge(&self, key: BadgeKey) {
        if let Some(wake) = &*self.wake.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = wake.send(Some(key));
        }
    }

    /// A handle that nudges like [`GitWatch::nudge`] (for a delayed one).
    pub fn sender(&self) -> Option<mpsc::Sender<Option<BadgeKey>>> {
        self.wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stop the thread (it exits at its next wake).
    pub fn stop(&self) {
        self.wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Read `key` now; whether the badge changed.
    fn read(&self, key: &BadgeKey) -> bool {
        let status = if key.dir.join(".git").exists() {
            status(&key.dir, key.base.as_deref()).ok()
        } else {
            None
        };
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = cache.get(key).is_none_or(|b| b.status != status);
        cache.insert(
            key.clone(),
            Badge {
                status,
                at: Instant::now(),
            },
        );
        changed
    }

    fn fresh(&self, key: &BadgeKey) -> bool {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .is_some_and(|b| b.at.elapsed() < NUDGE_DEBOUNCE)
    }

    fn retain(&self, keep: &HashSet<BadgeKey>) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k, _| keep.contains(k));
    }
}

/// What the badge thread needs from its owner.
pub trait BadgeSource: Send + Sync + 'static {
    fn git_watch(&self) -> &GitWatch;
    /// Every session's badge key, and whether its process runs (only
    /// running ones are re-read on the slow timer).
    fn badge_keys(&self) -> Vec<(BadgeKey, bool)>;
    /// Badges changed: tell clients to re-list.
    fn badges_changed(&self);
}

/// Start the badge thread: it reads nudged keys at once (a burst
/// coalesced) and every running session's every [`SLOW_REFRESH`] (and
/// keys never read yet whenever it wakes). Ends when the owner is dropped
/// or [`GitWatch::stop`] is called.
pub fn spawn_watcher<T: BadgeSource>(owner: &Arc<T>) {
    let (wake, woken) = mpsc::channel::<Option<BadgeKey>>();
    *owner
        .git_watch()
        .wake
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(wake);
    let weak: Weak<T> = Arc::downgrade(owner);
    let _ = std::thread::Builder::new()
        .name("hub-git".into())
        .spawn(move || {
            let mut last_sweep = Instant::now();
            // A first sweep at once, so badges show soon after a start.
            let mut sweep = true;
            loop {
                let mut nudged: Vec<BadgeKey> = Vec::new();
                if !sweep {
                    let wait = SLOW_REFRESH.saturating_sub(last_sweep.elapsed());
                    match woken.recv_timeout(wait) {
                        Ok(key) => nudged.extend(key),
                        Err(mpsc::RecvTimeoutError::Timeout) => sweep = true,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    loop {
                        match woken.try_recv() {
                            Ok(key) => nudged.extend(key),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }
                }
                let Some(owner) = weak.upgrade() else {
                    break;
                };
                let watch = owner.git_watch();
                let keys = owner.badge_keys();
                let known: HashSet<BadgeKey> = keys.iter().map(|(k, _)| k.clone()).collect();
                let mut todo: Vec<BadgeKey> = Vec::new();
                for key in nudged {
                    if known.contains(&key) && !watch.fresh(&key) && !todo.contains(&key) {
                        todo.push(key);
                    }
                }
                {
                    let cache = watch.cache.lock().unwrap_or_else(PoisonError::into_inner);
                    for (key, running) in &keys {
                        let unread = !cache.contains_key(key);
                        if (unread || (sweep && *running)) && !todo.contains(key) {
                            todo.push(key.clone());
                        }
                    }
                }
                if sweep {
                    watch.retain(&known);
                    last_sweep = Instant::now();
                    sweep = false;
                }
                let mut changed = false;
                for key in &todo {
                    changed |= watch.read(key);
                }
                if changed {
                    owner.badges_changed();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// `<tmp>/code/app` with one commit on `main`.
    fn repo(tmp: &Path) -> PathBuf {
        let repo = tmp.join("code").join("app");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        run(&repo, &["add", "README"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        repo.canonicalize().unwrap()
    }

    #[test]
    fn task_names_are_directory_and_branch_safe() {
        assert_eq!(validate_task(" fix-login ").unwrap(), "fix-login");
        assert_eq!(validate_task("v1.2_rc").unwrap(), "v1.2_rc");
        for bad in [
            "", "a/b", "-x", ".x", "a..b", "x.lock", "x.", "a b", "../up", "é",
        ] {
            assert!(validate_task(bad).is_err(), "{bad:?}");
        }
        assert!(validate_task(&"a".repeat(MAX_TASK_CHARS + 1)).is_err());
        assert_eq!(validate_prefix("agent/").unwrap(), "agent/");
        assert_eq!(validate_prefix("").unwrap(), "");
        for bad in ["/x", "a..b", "a//b", "a b", "-x"] {
            assert!(validate_prefix(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn worktrees_are_siblings_of_the_main_checkout_even_from_a_linked_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());
        let plan = plan(&repo, "fix-login", "", None).unwrap();
        let code = repo.parent().unwrap();
        assert_eq!(plan.path, code.join("app.worktrees").join("fix-login"));
        assert_eq!(plan.branch, "fix-login");
        assert_eq!(plan.base, "main");
        let wt = create(&repo, &plan).unwrap();
        assert!(wt.path.join("README").is_file());
        assert!(!wt.path.starts_with(&repo));
        // Cutting from inside the linked worktree still lands beside the
        // main checkout, never in `app.worktrees/fix-login.worktrees`.
        let nested = super::plan(&wt.path, "second", "agent/", None).unwrap();
        assert_eq!(nested.path, code.join("app.worktrees").join("second"));
        assert_eq!(nested.branch, "agent/second");
        assert_eq!(nested.base, "fix-login");
        // Taken names are refused before git is asked.
        let err = super::plan(&repo, "fix-login", "", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        std::fs::create_dir_all(code.join("app.worktrees").join("other")).unwrap();
        let err = super::plan(&repo, "other", "", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        run(&repo, &["branch", "taken"]);
        let err = super::plan(&repo, "taken", "", None).unwrap_err();
        assert!(err.contains("branch taken already exists"), "{err}");
        let err = super::plan(&repo, "x", "", Some("nope")).unwrap_err();
        assert!(err.contains("names no branch"), "{err}");
        let err = super::plan(&repo, "x", "", Some("--force")).unwrap_err();
        assert!(err.contains("not a branch"), "{err}");
    }

    #[test]
    fn a_repo_without_commits_cannot_be_cut_from() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("empty");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        let err = plan(&repo, "x", "", None).unwrap_err();
        assert!(err.contains("no commits"), "{err}");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(plan(&plain, "x", "", None).is_err());
    }

    #[test]
    fn status_counts_changes_and_ahead_behind_against_the_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());
        let wt = create(&repo, &plan(&repo, "feat", "", Some("main")).unwrap()).unwrap();
        let s = status(&wt.path, Some(&wt.base)).unwrap();
        assert_eq!(s.branch.as_deref(), Some("feat"));
        assert_eq!((s.dirty, s.untracked, s.ahead, s.behind), (0, 0, 0, 0));
        assert_eq!(s.against.as_deref(), Some("main"));
        std::fs::write(wt.path.join("README"), "changed\n").unwrap();
        std::fs::write(wt.path.join("new.txt"), "x\n").unwrap();
        run(&wt.path, &["add", "new.txt"]);
        run(&wt.path, &["commit", "-q", "-m", "one"]);
        std::fs::write(wt.path.join("loose"), "x\n").unwrap();
        std::fs::write(repo.join("main-only"), "x\n").unwrap();
        run(&repo, &["add", "main-only"]);
        run(&repo, &["commit", "-q", "-m", "main moves"]);
        let s = status(&wt.path, Some("main")).unwrap();
        assert_eq!((s.dirty, s.untracked, s.ahead, s.behind), (1, 1, 1, 1));
        // No base and no upstream: nothing to count against.
        let s = status(&repo, None).unwrap();
        assert_eq!((s.ahead, s.behind, s.against), (0, 0, None));
    }

    #[test]
    fn removal_refuses_lost_work_unless_forced_and_keeps_unmerged_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());

        // Clean and merged: worktree and branch both go.
        let clean = create(&repo, &plan(&repo, "clean", "", None).unwrap()).unwrap();
        assert!(removal_check(&clean).unwrap().refusal().is_none());
        let removed = remove(&clean, false).unwrap();
        assert!(removed.branch_deleted);
        assert!(!clean.path.exists());
        assert!(!git_ok(
            &repo,
            &["show-ref", "--verify", "--quiet", "refs/heads/clean"]
        ));

        // Dirty: refused with what would be lost.
        let dirty = create(&repo, &plan(&repo, "dirty", "", None).unwrap()).unwrap();
        std::fs::write(dirty.path.join("README"), "edit\n").unwrap();
        std::fs::write(dirty.path.join("scratch"), "x\n").unwrap();
        let err = remove(&dirty, false).unwrap_err();
        assert!(
            err.contains("1 uncommitted change and 1 untracked file"),
            "{err}"
        );
        assert!(dirty.path.exists());
        let forced = remove(&dirty, true).unwrap();
        assert!(!dirty.path.exists());
        assert!(
            forced.branch_deleted,
            "no commits beyond main: nothing lost"
        );

        // Unmerged commits: refused; forced, the branch survives.
        let ahead = create(&repo, &plan(&repo, "ahead", "", None).unwrap()).unwrap();
        std::fs::write(ahead.path.join("work"), "x\n").unwrap();
        run(&ahead.path, &["add", "work"]);
        run(&ahead.path, &["commit", "-q", "-m", "work"]);
        let check = removal_check(&ahead).unwrap();
        assert_eq!(check.unmerged, 1);
        let err = remove(&ahead, false).unwrap_err();
        assert!(err.contains("1 commit not in main"), "{err}");
        let forced = remove(&ahead, true).unwrap();
        assert!(!forced.branch_deleted);
        assert!(git_ok(
            &repo,
            &["show-ref", "--verify", "--quiet", "refs/heads/ahead"]
        ));

        // Merged later: removable without force.
        let merged = create(&repo, &plan(&repo, "merged", "", None).unwrap()).unwrap();
        std::fs::write(merged.path.join("m"), "x\n").unwrap();
        run(&merged.path, &["add", "m"]);
        run(&merged.path, &["commit", "-q", "-m", "m"]);
        run(&repo, &["merge", "-q", "--ff-only", "merged"]);
        assert!(removal_check(&merged).unwrap().refusal().is_none());
        assert!(remove(&merged, false).unwrap().branch_deleted);

        // A directory deleted by hand: only git's record is pruned.
        let gone = create(&repo, &plan(&repo, "gone", "", None).unwrap()).unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        let check = removal_check(&gone).unwrap();
        assert!(check.missing);
        remove(&gone, false).unwrap();
        let list = git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(!list.contains("gone"), "{list}");
    }

    #[test]
    fn branches_lists_local_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = repo(tmp.path());
        run(&repo, &["branch", "dev"]);
        let mut names = branches(&repo);
        names.sort();
        assert_eq!(names, ["dev", "main"]);
    }
}
