//! The build sweep (`[build_sweep]` in `hub.toml`, `fw hub sweep`): the
//! `target/` dirs of worktrees nobody uses any more, deleted -- only
//! `target/`, never the worktree, a branch, or any other file, and never
//! in a repo's main checkout.
//!
//! Candidates are the linked worktrees (`git worktree list`) of the repos
//! the hub knows. One is eligible only when all hold:
//! 1. no hub session that has not exited works in it;
//! 2. no keep marker ([`KEEP_FILE`] at its root; an RFC 3339 first line
//!    is its expiry, and an expired one does not count);
//! 3. no process uses it ([`procs`]: cwd, root, exe, open or mapped
//!    files, a mount);
//! 4. its HEAD is in the repo's default branch and its `target/` is
//!    untouched for `merged_idle`, or untouched for `stale_idle`.
//!
//! Anything uncertain (an unreadable `/proc` entry, a git error, an
//! unreadable build dir or marker) keeps the worktree. 1-3 are checked
//! again right before each deletion. Oldest first: by the watermark
//! rule, on each filesystem past `high_watermark` until it is under
//! `low_watermark`; with `all`, every eligible one.

pub mod disk;
pub mod procs;

use chrono::{DateTime, Utc};
use disk::DiskUse;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The keep marker at a worktree's root.
pub const KEEP_FILE: &str = ".fw-keep-build";
/// The least time between two automatic looks at the disks.
pub const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);
/// The hub event (telemetry) and activity kind of one deletion.
pub const SWEPT_EVENT: &str = "build.swept";
/// The hub event and activity kind of a sweep that ran out of candidates
/// with a disk still past its high watermark.
pub const SHORT_EVENT: &str = "build.sweep_short";
/// The last sweep, in the state dir.
pub const LAST_FILE: &str = "build-sweep.json";

/// `[build_sweep]` resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepConfig {
    /// Automatic sweeps; `fw hub sweep` works either way.
    pub enabled: bool,
    pub high_watermark: u8,
    pub low_watermark: u8,
    pub merged_idle: Duration,
    pub stale_idle: Duration,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            high_watermark: 90,
            low_watermark: 85,
            merged_idle: Duration::from_secs(86_400),
            stale_idle: Duration::from_secs(14 * 86_400),
        }
    }
}

impl SweepConfig {
    /// The defaults with automatic sweeps off (a hub not started from
    /// `hub.toml`).
    pub fn off() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Why a candidate's `target/` may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// Its HEAD is in the default branch, idle for `merged_idle`.
    Merged,
    /// Idle for `stale_idle`, whatever its branch.
    Stale,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Stale => "stale",
        }
    }
}

/// Why a worktree is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// No `target/` of its own to delete (none, a symlink, not a Cargo
    /// project, or not ignored by git).
    NoBuild,
    /// Not a linked worktree (a main checkout).
    NotLinked,
    Session,
    KeepMarker,
    Process,
    /// Touched too recently.
    Fresh,
    /// Something could not be read: nothing is concluded.
    Uncertain,
}

/// A worktree whose `target/` may go.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub path: PathBuf,
    /// The repo's main checkout.
    pub repo: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub target: PathBuf,
    pub bytes: u64,
    /// When anything under `target/` last changed.
    pub newest: DateTime<Utc>,
    pub why: Why,
    /// Picked by this sweep's rule (the watermark, or `all`).
    pub delete: bool,
    #[serde(skip)]
    pub dev: u64,
}

/// A worktree kept, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub path: PathBuf,
    pub reason: Reason,
    pub detail: String,
}

/// A `target/` deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deleted {
    pub path: PathBuf,
    pub target: PathBuf,
    pub bytes: u64,
    pub why: Why,
}

/// What a sweep found and did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub at: DateTime<Utc>,
    pub dry_run: bool,
    pub all: bool,
    /// The filesystems looked at, before.
    pub disks: Vec<DiskUse>,
    /// Oldest first.
    pub candidates: Vec<Candidate>,
    pub kept: Vec<Kept>,
    pub deleted: Vec<Deleted>,
    /// Picked, but kept by the check right before deleting.
    pub skipped: Vec<Kept>,
    /// Picked, but deleting failed (part of it may be gone).
    pub failed: Vec<Kept>,
    pub reclaimed_bytes: u64,
    /// Repos whose worktrees could not be listed.
    pub errors: Vec<String>,
    /// Processes checked only by their command line and mounts.
    pub opaque_processes: usize,
    /// A filesystem is still past its high watermark with no candidate
    /// left on it (predicted, in a dry run).
    pub short: bool,
}

/// A keep marker's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    None,
    Forever,
    Until(DateTime<Utc>),
    Expired(DateTime<Utc>),
    /// Unreadable or unparsable: it keeps.
    Bad(String),
}

impl Marker {
    pub fn keeps(&self) -> bool {
        matches!(self, Self::Forever | Self::Until(_) | Self::Bad(_))
    }

    fn text(&self) -> String {
        match self {
            Self::None => "no keep marker".into(),
            Self::Forever => format!("{KEEP_FILE}, no expiry"),
            Self::Until(t) => format!("{KEEP_FILE} until {}", t.to_rfc3339()),
            Self::Expired(t) => format!("{KEEP_FILE} expired {}", t.to_rfc3339()),
            Self::Bad(why) => format!("{KEEP_FILE} cannot be read ({why}): kept"),
        }
    }
}

/// The keep marker of the worktree at `dir`, as of `now`.
pub fn marker(dir: &Path, now: DateTime<Utc>) -> Marker {
    let path = dir.join(KEEP_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Marker::None,
        Err(e) => return Marker::Bad(e.to_string()),
        Ok(_) => {}
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return Marker::Bad(e.to_string()),
    };
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return Marker::Forever;
    }
    match DateTime::parse_from_rfc3339(first) {
        Ok(t) => {
            let t = t.with_timezone(&Utc);
            if t > now {
                Marker::Until(t)
            } else {
                Marker::Expired(t)
            }
        }
        Err(e) => Marker::Bad(format!("first line {first:?} is not an RFC 3339 time: {e}")),
    }
}

/// Write the keep marker at the root of the worktree `dir` (`until`:
/// its expiry; none keeps it until the file is removed). `dir` must be
/// the top of a git work tree.
pub fn write_keep(dir: &Path, until: Option<DateTime<Utc>>) -> Result<PathBuf, String> {
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let top = crate::git::git_answer(&dir, &["rev-parse", "--show-toplevel"])?;
    let top = match top {
        (true, top) => PathBuf::from(top.trim()),
        (false, _) => return Err(format!("{} is not in a git work tree", dir.display())),
    };
    if top.canonicalize().ok().as_deref() != Some(dir.as_path()) {
        return Err(format!(
            "{} is not the top of a git work tree (that is {})",
            dir.display(),
            top.display()
        ));
    }
    let path = dir.join(KEEP_FILE);
    let body = until.map_or_else(String::new, |t| {
        format!("{}\n", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    });
    let tmp = dir.join(format!("{KEEP_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(path)
}

/// A linked worktree found to look at.
#[derive(Debug, Clone)]
struct Tree {
    path: PathBuf,
    repo: PathBuf,
    branch: Option<String>,
    head: String,
    /// The repo's default branch as a ref; `Err`: git failed finding it.
    default: Result<Option<String>, String>,
}

/// The default branch of the repo whose main checkout is `main`, as a
/// ref: `origin/HEAD`'s branch (local when it exists), else the branch
/// the main checkout has out; `None` when neither names one.
fn default_branch(main: &Path) -> Result<Option<String>, String> {
    use crate::git::git_answer;
    if let (true, remote) = git_answer(main, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])? {
        let remote = remote.trim().to_owned();
        if let Some(name) = remote.strip_prefix("refs/remotes/origin/") {
            let local = format!("refs/heads/{name}");
            if git_answer(main, &["show-ref", "--verify", "-q", &local])?.0 {
                return Ok(Some(local));
            }
        }
        return Ok(Some(remote));
    }
    match git_answer(main, &["symbolic-ref", "-q", "HEAD"])? {
        (true, head) => Ok(Some(head.trim().to_owned())),
        (false, _) => Ok(None),
    }
}

/// Whether `head` is in `default` (in the repo at `main`).
fn merged(main: &Path, head: &str, default: &str) -> Result<bool, String> {
    crate::git::git_answer(main, &["merge-base", "--is-ancestor", head, default])
        .map(|(yes, _)| yes)
}

/// Every linked worktree of `repos` (each repo once, by its main
/// checkout), and the repos that could not be listed.
fn trees(repos: &[PathBuf]) -> (Vec<Tree>, Vec<PathBuf>, Vec<String>) {
    let mut mains: Vec<PathBuf> = Vec::new();
    let mut errors = Vec::new();
    for repo in repos {
        match crate::git::main_worktree(repo) {
            Ok((_, true)) => {}
            Ok((main, false)) => {
                if !mains.contains(&main) {
                    mains.push(main);
                }
            }
            Err(e) => {
                if repo.join(".git").exists() {
                    errors.push(e);
                }
            }
        }
    }
    let mut out: Vec<Tree> = Vec::new();
    for main in &mains {
        let listed = match crate::git::linked_worktrees(main) {
            Ok(listed) => listed,
            Err(e) => {
                errors.push(format!("{}: {e}", main.display()));
                continue;
            }
        };
        let default = default_branch(main);
        for entry in listed {
            let Ok(path) = entry.path.canonicalize() else {
                // Gone from disk (prunable): nothing to delete.
                continue;
            };
            if mains.contains(&path) || out.iter().any(|t| t.path == path) {
                continue;
            }
            out.push(Tree {
                path,
                repo: main.clone(),
                branch: entry.branch,
                head: entry.head,
                default: default.clone(),
            });
        }
    }
    (out, mains, errors)
}

/// Whether `dir` is a linked worktree's top: its `.git` is a file.
fn is_linked(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.file_type().is_file())
}

/// Checks 1-3 (sessions, marker, processes) on `dir`: why it is kept,
/// if it is.
fn in_use(
    dir: &Path,
    scan: &procs::Scan,
    sessions: &dyn Fn(&Path) -> Option<String>,
    now: DateTime<Utc>,
) -> Option<(Reason, String)> {
    if let Some(who) = sessions(dir) {
        return Some((Reason::Session, who));
    }
    let marker = marker(dir, now);
    if marker.keeps() {
        return Some((Reason::KeepMarker, marker.text()));
    }
    match scan.user_of(dir) {
        Err(why) => Some((Reason::Uncertain, format!("processes: {why}"))),
        Ok(Some(user)) => Some((Reason::Process, user.text())),
        Ok(None) => None,
    }
}

fn ago(t: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(t).unwrap_or_default().as_secs();
    crate::share::format_ttl(Duration::from_secs(secs - secs % 60))
}

/// What to look at and how.
pub struct Look<'a> {
    /// The repos (any folder in each) whose worktrees are candidates.
    pub repos: &'a [PathBuf],
    /// Folders whose filesystems count for the watermark besides the
    /// candidates' own (the workspace folders).
    pub folders: &'a [PathBuf],
    pub config: SweepConfig,
    /// The hub session (not exited) working in a directory, named.
    pub sessions: &'a dyn Fn(&Path) -> Option<String>,
    pub all: bool,
}

/// Find the candidates and pick the ones this sweep's rule deletes.
pub fn survey(look: &Look<'_>) -> Report {
    let now = Utc::now();
    let wall = SystemTime::now();
    let (trees, mains, errors) = trees(look.repos);
    let scan = procs::Scan::take();
    let mut candidates = Vec::new();
    let mut kept = Vec::new();
    for tree in trees {
        let keep = |reason: Reason, detail: String| Kept {
            path: tree.path.clone(),
            reason,
            detail,
        };
        if !is_linked(&tree.path) {
            kept.push(keep(Reason::NotLinked, "not a linked worktree".into()));
            continue;
        }
        let target = match crate::close::build_dir(&tree.path) {
            Ok(target) => target,
            Err(why) => {
                kept.push(keep(Reason::NoBuild, why));
                continue;
            }
        };
        if let Some((reason, detail)) = in_use(&tree.path, &scan, look.sessions, now) {
            kept.push(keep(reason, detail));
            continue;
        }
        let is_merged = match &tree.default {
            Err(e) => Err(e.clone()),
            Ok(None) => Ok(false),
            Ok(Some(default)) if tree.head.is_empty() => {
                Err(format!("no HEAD listed for it (default branch {default})"))
            }
            Ok(Some(default)) => merged(&tree.repo, &tree.head, default),
        };
        let is_merged = match is_merged {
            Ok(m) => m,
            Err(e) => {
                kept.push(keep(Reason::Uncertain, format!("git: {e}")));
                continue;
            }
        };
        let (why, idle) = if is_merged {
            (
                Why::Merged,
                look.config.merged_idle.min(look.config.stale_idle),
            )
        } else {
            (Why::Stale, look.config.stale_idle)
        };
        let cutoff = wall.checked_sub(idle).unwrap_or(SystemTime::UNIX_EPOCH);
        let measured = match disk::measure(&target, cutoff) {
            Ok(m) => m,
            Err(e) => {
                kept.push(keep(Reason::Uncertain, format!("build dir: {e}")));
                continue;
            }
        };
        if !measured.complete {
            kept.push(keep(
                Reason::Fresh,
                format!(
                    "target/ changed {} ago; {} needs {} idle",
                    ago(measured.newest, wall),
                    if is_merged { "merged" } else { "unmerged" },
                    crate::share::format_ttl(idle)
                ),
            ));
            continue;
        }
        let dev = match disk::device(&tree.path) {
            Ok(dev) => dev,
            Err(e) => {
                kept.push(keep(
                    Reason::Uncertain,
                    format!("{}: {e}", tree.path.display()),
                ));
                continue;
            }
        };
        candidates.push(Candidate {
            path: tree.path.clone(),
            repo: tree.repo.clone(),
            branch: tree.branch.clone(),
            target,
            bytes: measured.bytes,
            newest: DateTime::<Utc>::from(measured.newest),
            why,
            delete: false,
            dev,
        });
    }
    candidates.sort_by(|a, b| a.newest.cmp(&b.newest).then_with(|| a.path.cmp(&b.path)));
    let mut disks: Vec<DiskUse> = Vec::new();
    let mut errors = errors;
    let places = look
        .folders
        .iter()
        .chain(mains.iter())
        .chain(candidates.iter().map(|c| &c.path));
    for place in places {
        let Ok(dev) = disk::device(place) else {
            continue;
        };
        if disks.iter().any(|d| d.dev == dev) {
            continue;
        }
        match disk::disk_use(place) {
            Ok(d) => disks.push(d),
            Err(e) => errors.push(format!("{}: {e}", place.display())),
        }
    }
    let mut report = Report {
        at: now,
        dry_run: true,
        all: look.all,
        disks,
        candidates,
        kept,
        deleted: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
        reclaimed_bytes: 0,
        errors,
        opaque_processes: scan.opaque(),
        short: false,
    };
    pick(&mut report, look.config);
    report
}

/// Mark what this sweep's rule takes: every candidate with `all`, else
/// oldest first on each filesystem past the high watermark, until its
/// predicted use is under the low one. `short`: a filesystem would stay
/// past the high watermark.
fn pick(report: &mut Report, config: SweepConfig) {
    let mut used: HashMap<u64, (u64, u64)> = report
        .disks
        .iter()
        .map(|d| (d.dev, (d.used_bytes, d.free_bytes)))
        .collect();
    let over = |d: &DiskUse| d.used_pct >= f64::from(config.high_watermark);
    let hot: Vec<u64> = report
        .disks
        .iter()
        .filter(|d| over(d))
        .map(|d| d.dev)
        .collect();
    for c in &mut report.candidates {
        if report.all {
            c.delete = true;
            continue;
        }
        if !hot.contains(&c.dev) {
            continue;
        }
        let Some((u, f)) = used.get_mut(&c.dev) else {
            continue;
        };
        if disk::pct(*u, *f) < f64::from(config.low_watermark) {
            continue;
        }
        c.delete = true;
        *u = u.saturating_sub(c.bytes);
        *f = f.saturating_add(c.bytes);
    }
    report.short = hot.iter().any(|dev| {
        used.get(dev)
            .is_some_and(|(u, f)| disk::pct(*u, *f) >= f64::from(config.high_watermark))
    });
}

/// Delete what `report` picked, oldest first, checking 1-3 again right
/// before each; by the watermark rule, a filesystem is left once it is
/// under the low watermark. `deleted` is told of each deletion as it
/// happens.
pub fn execute(
    report: &mut Report,
    config: SweepConfig,
    sessions: &dyn Fn(&Path) -> Option<String>,
    deleted: &mut dyn FnMut(&Deleted),
) {
    report.dry_run = false;
    let picked: Vec<Candidate> = report
        .candidates
        .iter()
        .filter(|c| c.delete)
        .cloned()
        .collect();
    for c in picked {
        if !report.all {
            match disk::disk_use(&c.path) {
                Ok(d) if d.used_pct < f64::from(config.low_watermark) => continue,
                Ok(_) => {}
                Err(e) => {
                    report.skipped.push(Kept {
                        path: c.path.clone(),
                        reason: Reason::Uncertain,
                        detail: format!("disk: {e}"),
                    });
                    continue;
                }
            }
        }
        if let Some((reason, detail)) = recheck(&c, sessions) {
            report.skipped.push(Kept {
                path: c.path.clone(),
                reason,
                detail,
            });
            continue;
        }
        match std::fs::remove_dir_all(&c.target) {
            Ok(()) => {
                let gone = Deleted {
                    path: c.path.clone(),
                    target: c.target.clone(),
                    bytes: c.bytes,
                    why: c.why,
                };
                report.reclaimed_bytes += c.bytes;
                deleted(&gone);
                report.deleted.push(gone);
            }
            Err(e) => report.failed.push(Kept {
                path: c.path.clone(),
                reason: Reason::Uncertain,
                detail: format!("{}: {e}", c.target.display()),
            }),
        }
    }
    let now_short = report.disks.iter().any(|before| {
        before.used_pct >= f64::from(config.high_watermark)
            && disk::disk_use(&before.path)
                .is_ok_and(|d| d.used_pct >= f64::from(config.high_watermark))
    });
    report.short = !report.all && now_short;
}

/// Right before deleting `c`'s `target/`: still a linked worktree with
/// that build dir, and checks 1-3 against a fresh look at every process.
fn recheck(c: &Candidate, sessions: &dyn Fn(&Path) -> Option<String>) -> Option<(Reason, String)> {
    if !is_linked(&c.path) {
        return Some((Reason::NotLinked, "not a linked worktree".into()));
    }
    match crate::close::build_dir(&c.path) {
        Ok(target) if target == c.target => {}
        Ok(_) => return Some((Reason::NoBuild, "its build dir moved".into())),
        Err(why) => return Some((Reason::NoBuild, why)),
    }
    in_use(&c.path, &procs::Scan::take(), sessions, Utc::now())
}

/// A sweep in one line: `deleted 3 target/ dirs, 12.4 GiB`.
pub fn summary(report: &Report) -> String {
    let gib = |b: u64| format!("{:.1} GiB", b as f64 / f64::from(1u32 << 30));
    if report.dry_run {
        let picked: Vec<&Candidate> = report.candidates.iter().filter(|c| c.delete).collect();
        format!(
            "would delete {} of {} candidate target/ dirs, {}",
            picked.len(),
            report.candidates.len(),
            gib(picked.iter().map(|c| c.bytes).sum())
        )
    } else {
        format!(
            "deleted {} target/ dirs, {}",
            report.deleted.len(),
            gib(report.reclaimed_bytes)
        )
    }
}

/// The last sweep that deleted or was asked for, as kept in the state
/// dir and shown in the budget reading.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct LastSweep {
    pub at: DateTime<Utc>,
    /// `auto` (the watermark) or `manual`.
    pub by: String,
    pub deleted: usize,
    pub reclaimed_bytes: u64,
    pub short: bool,
}

pub fn load_last(state_dir: &Path) -> Option<LastSweep> {
    let text = std::fs::read_to_string(state_dir.join(LAST_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_last(state_dir: &Path, last: &LastSweep) -> std::io::Result<()> {
    let path = state_dir.join(LAST_FILE);
    let tmp = state_dir.join(format!("{LAST_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(last).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests;
