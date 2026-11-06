//! The workspace dialog's folder picker: one directory's subdirectories on
//! the hub's machine ([`list_dirs`], for path completion and the folder
//! browser) and git repos worth suggesting ([`discover_repos`] behind a
//! short-lived [`RepoScanCache`], ranked by [`suggestions`]).
//!
//! Directory metadata only: names, git and link flags, modification times.
//! Never a file's name, never anything's contents. A browser cannot hand
//! the hub a path from the OS's own picker (and its machine's paths are
//! not the hub's when the hub is remote), so the hub lists its own.

use crate::state::{expand_home, is_git_root};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Subdirectories one listing returns at most (visible ones first).
pub const MAX_ENTRIES: usize = 2000;
/// Suggestions one answer carries at most.
pub const MAX_SUGGESTIONS: usize = 60;
/// How long a discovery scan's result is reused.
pub const SCAN_TTL: Duration = Duration::from_secs(60);

/// Where repos usually live, each searched [`ROOT_DEPTH`] levels down
/// (`~/code/org/team/repo`); the home directory itself [`HOME_DEPTH`].
pub const SCAN_ROOTS: &[&str] = &["code", "src", "projects", "dev", "repos", "work"];
/// Levels below a [`SCAN_ROOTS`] directory the scan looks.
pub const ROOT_DEPTH: usize = 3;
/// Levels below the home directory the scan looks (`~/thing/repo`).
pub const HOME_DEPTH: usize = 2;

/// Directory names the scan never enters: build output and dependency
/// trees, large and never a repo of yours.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
    "dist",
    "build",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrowseError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} does not exist")]
    NotFound(String),
    #[error("{0} is not accessible (permission denied)")]
    Denied(String),
    #[error("{0}: {1}")]
    Io(String, String),
}

/// One subdirectory of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirEntry {
    pub name: String,
    /// The directory's path under the listed one (a symlink keeps its own
    /// name; the folder check resolves it).
    pub path: PathBuf,
    /// A git work-tree root (`.git` dir, or a linked worktree's file).
    pub git: bool,
    /// A dot-directory (the UI shows them when asked).
    pub hidden: bool,
    /// A symlink to a directory (or a broken one: see `error`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
    /// Why it cannot be opened, when that is known while listing (a
    /// broken or unreadable symlink).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A directory's subdirectories, visible ones first, each group by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    /// The listed directory, canonical.
    pub path: PathBuf,
    /// Its parent (`None` at `/`).
    pub parent: Option<PathBuf>,
    /// The hub user's home directory (the UI shows paths under it as `~`).
    pub home: Option<PathBuf>,
    /// Whether the listed directory is a git work-tree root itself.
    pub git: bool,
    pub entries: Vec<DirEntry>,
    /// More subdirectories matched than `entries` holds.
    pub truncated: bool,
}

/// Resolve a picker path: `~` expanded, absolute, canonical, a directory.
/// Blank is the home directory (`/` without one).
pub fn resolve_dir(raw: &str, home: Option<&Path>) -> Result<PathBuf, BrowseError> {
    let raw = raw.trim();
    let path = if raw.is_empty() {
        home.map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
    } else {
        expand_home(Path::new(raw), home)
    };
    if !path.is_absolute() {
        return Err(BrowseError::Invalid(format!(
            "{} must be absolute (or start with ~)",
            path.display()
        )));
    }
    let shown = path.display().to_string();
    let canonical = path.canonicalize().map_err(|e| io_error(&shown, &e))?;
    if !canonical.is_dir() {
        return Err(BrowseError::Invalid(format!("{shown} is not a directory")));
    }
    Ok(canonical)
}

fn io_error(shown: &str, error: &std::io::Error) -> BrowseError {
    match error.kind() {
        ErrorKind::NotFound => BrowseError::NotFound(shown.to_owned()),
        ErrorKind::PermissionDenied => BrowseError::Denied(shown.to_owned()),
        ErrorKind::NotADirectory => BrowseError::Invalid(format!("{shown} is not a directory")),
        _ => BrowseError::Io(shown.to_owned(), error.to_string()),
    }
}

/// The subdirectories of `raw` (see [`resolve_dir`]) whose names start
/// with `prefix` (any case), at most `limit`. Files are never listed;
/// symlinks to directories are (`link`), broken ones with an `error`;
/// names that are not UTF-8 are passed over.
pub fn list_dirs(
    raw: &str,
    prefix: Option<&str>,
    home: Option<&Path>,
    limit: usize,
) -> Result<Listing, BrowseError> {
    let dir = resolve_dir(raw, home)?;
    let shown = dir.display().to_string();
    let read = fs::read_dir(&dir).map_err(|e| io_error(&shown, &e))?;
    let prefix = prefix.unwrap_or_default().to_lowercase();
    let mut entries = Vec::new();
    for entry in read.filter_map(Result::ok) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.to_lowercase().starts_with(&prefix) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let (link, error) = if kind.is_dir() {
            (false, None)
        } else if kind.is_symlink() {
            match fs::metadata(&path) {
                Ok(target) if target.is_dir() => (true, None),
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    (true, Some("link target not accessible".to_owned()))
                }
                Err(_) => (true, Some("broken link".to_owned())),
            }
        } else {
            continue;
        };
        entries.push(DirEntry {
            hidden: name.starts_with('.'),
            name,
            path,
            git: false,
            link,
            error,
        });
    }
    entries.sort_by(|a, b| {
        (a.hidden, a.name.to_lowercase(), &a.name).cmp(&(b.hidden, b.name.to_lowercase(), &b.name))
    });
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    // One stat each, for the kept entries only.
    for entry in &mut entries {
        entry.git = entry.error.is_none() && is_git_root(&entry.path);
    }
    Ok(Listing {
        git: is_git_root(&dir),
        parent: dir.parent().map(Path::to_path_buf),
        path: dir,
        home: home.map(Path::to_path_buf),
        entries,
        truncated,
    })
}

// --- discovery ----------------------------------------------------------

/// A git repo the scan found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repo {
    pub path: PathBuf,
    /// A linked worktree (its `.git` is a file): usually a sibling
    /// checkout of a repo listed too, so ranked after the rest.
    pub worktree: bool,
    /// Last change, unix seconds: the later of the directory's and its
    /// git index's modification times.
    pub modified: Option<i64>,
}

/// How far one scan may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanBounds {
    /// Directories read at most, over every root.
    pub max_dirs: usize,
    /// Repos recorded at most.
    pub max_repos: usize,
    /// Wall-clock budget.
    pub budget: Duration,
}

impl ScanBounds {
    pub const DEFAULT: Self = Self {
        max_dirs: 6000,
        max_repos: 400,
        budget: Duration::from_millis(1500),
    };
}

/// What one scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Scan {
    pub repos: Vec<Repo>,
    /// The roots that existed and were searched, canonical.
    pub roots: Vec<PathBuf>,
    /// A bound stopped it before it searched everything.
    pub truncated: bool,
}

/// The default roots under `home`: [`SCAN_ROOTS`] at [`ROOT_DEPTH`], then
/// the home directory at [`HOME_DEPTH`].
pub fn default_roots(home: &Path) -> Vec<(PathBuf, usize)> {
    SCAN_ROOTS
        .iter()
        .map(|name| (home.join(name), ROOT_DEPTH))
        .chain(std::iter::once((home.to_path_buf(), HOME_DEPTH)))
        .collect()
}

/// Find git repos under `roots` (each `(dir, depth)`), breadth first.
/// A repo is recorded and not entered (its inside is its own); a root is
/// searched, never recorded (the home directory may be a dotfiles repo,
/// not a project). Hidden directories (`.git` included),
/// [`SKIP_DIRS`], and symlinks are never entered, so the walk cannot loop;
/// a directory reached from two roots is read once. Unreadable directories
/// are passed over.
pub fn discover_repos(roots: &[(PathBuf, usize)], bounds: ScanBounds) -> Scan {
    let deadline = Instant::now() + bounds.budget;
    let mut scan = Scan::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut read = 0;
    let mut queue: VecDeque<(PathBuf, usize, usize)> = VecDeque::new();
    for (root, depth) in roots {
        // A root may be a symlink (`~/code` -> another disk): resolved once.
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if !root.is_dir() || scan.roots.contains(&root) {
            continue;
        }
        scan.roots.push(root.clone());
        queue.push_back((root, 0, *depth));
    }
    let mut recorded: HashSet<PathBuf> = HashSet::new();
    'walk: while let Some((dir, depth, max_depth)) = queue.pop_front() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if depth > 0 && is_git_root(&dir) {
            if recorded.insert(dir.clone()) {
                scan.repos.push(repo_at(dir));
                if scan.repos.len() >= bounds.max_repos {
                    scan.truncated = true;
                    break;
                }
            }
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        if read >= bounds.max_dirs || Instant::now() >= deadline {
            scan.truncated = true;
            break;
        }
        read += 1;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            // `file_type` does not follow links: a symlink is never a dir here.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            children.push((name, entry.path()));
            if Instant::now() >= deadline {
                scan.truncated = true;
                break 'walk;
            }
        }
        children.sort();
        for (_, path) in children {
            queue.push_back((path, depth + 1, max_depth));
        }
    }
    scan
}

fn repo_at(path: PathBuf) -> Repo {
    let git = path.join(".git");
    let worktree = git.is_file();
    let mut probes = vec![path.clone()];
    if !worktree {
        probes.push(git.join("index"));
    }
    let modified = probes
        .iter()
        .filter_map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
        .map(unix_seconds);
    Repo {
        path,
        worktree,
        modified,
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The last discovery scan of the default roots, reused for [`SCAN_TTL`].
/// Callers wait on one running scan rather than starting their own.
#[derive(Debug, Default)]
pub struct RepoScanCache {
    last: Mutex<Option<(Instant, PathBuf, Arc<Scan>)>>,
}

impl RepoScanCache {
    /// The scan of `home`'s default roots: the cached one while fresh,
    /// else a new one. Blocking filesystem I/O: call off the executor.
    pub fn get(&self, home: &Path, bounds: ScanBounds) -> Arc<Scan> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, for_home, scan)) = last.as_ref()
            && for_home == home
            && at.elapsed() < SCAN_TTL
        {
            return Arc::clone(scan);
        }
        let scan = Arc::new(discover_repos(&default_roots(home), bounds));
        *last = Some((Instant::now(), home.to_path_buf(), Arc::clone(&scan)));
        scan
    }
}

// --- suggestions --------------------------------------------------------

/// A folder the hub has used: a workspace's folder or a session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsedFolder {
    pub path: PathBuf,
    /// The workspace it belongs to.
    pub workspace: String,
    /// When it was last used: the newest session opened in it, else when
    /// its workspace was created.
    pub when: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A folder of a workspace or of a session.
    Used,
    /// Found by the discovery scan.
    Found,
}

/// One suggested folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub path: PathBuf,
    pub git: bool,
    pub worktree: bool,
    pub source: Source,
    /// The workspace that has it (a `used` one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Last change or use, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

/// Rank suggestions: folders the hub has used, most recent first (those
/// still on disk), then the scan's repos not among them, newest change
/// first with linked worktrees after the rest; at most `limit`.
pub fn suggestions(used: &[UsedFolder], scan: &Scan, limit: usize) -> Vec<Suggestion> {
    // One entry per path: its latest use.
    let mut latest: HashMap<&Path, &UsedFolder> = HashMap::new();
    for folder in used {
        let entry = latest.entry(folder.path.as_path()).or_insert(folder);
        if folder.when > entry.when {
            *entry = folder;
        }
    }
    let mut used: Vec<&UsedFolder> = latest.into_values().filter(|f| f.path.is_dir()).collect();
    used.sort_by(|a, b| (b.when, &a.path).cmp(&(a.when, &b.path)));
    let mut out: Vec<Suggestion> = used
        .iter()
        .map(|f| {
            let git = f.path.join(".git");
            Suggestion {
                path: f.path.clone(),
                git: git.exists(),
                worktree: git.is_file(),
                source: Source::Used,
                workspace: Some(f.workspace.clone()),
                modified: Some(f.when.timestamp()),
            }
        })
        .collect();
    let taken: HashSet<PathBuf> = out.iter().map(|s| s.path.clone()).collect();
    let mut found: Vec<&Repo> = scan
        .repos
        .iter()
        .filter(|r| !taken.contains(&r.path))
        .collect();
    found.sort_by(|a, b| {
        (a.worktree, std::cmp::Reverse(a.modified), &a.path).cmp(&(
            b.worktree,
            std::cmp::Reverse(b.modified),
            &b.path,
        ))
    });
    out.extend(found.into_iter().map(|r| Suggestion {
        path: r.path.clone(),
        git: true,
        worktree: r.worktree,
        source: Source::Found,
        workspace: None,
        modified: r.modified,
    }));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn mkdirs(root: &Path, dirs: &[&str]) {
        for dir in dirs {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
    }

    fn names(listing: &Listing) -> Vec<&str> {
        listing.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_directories_only_visible_first_with_git_flags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdirs(root, &["beta/.git", "Alpha", ".hidden", "gamma"]);
        fs::write(root.join("file.txt"), "x").unwrap();
        let listing = list_dirs(root.to_str().unwrap(), None, None, MAX_ENTRIES).unwrap();
        assert_eq!(names(&listing), ["Alpha", "beta", "gamma", ".hidden"]);
        assert_eq!(listing.path, root.canonicalize().unwrap());
        assert_eq!(
            listing.parent,
            root.canonicalize().unwrap().parent().map(Path::to_path_buf)
        );
        assert!(!listing.truncated);
        let flags: Vec<(bool, bool)> = listing.entries.iter().map(|e| (e.git, e.hidden)).collect();
        assert_eq!(
            flags,
            [(false, false), (true, false), (false, false), (false, true)]
        );
    }

    #[test]
    fn a_linked_worktree_counts_as_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("wt")).unwrap();
        fs::write(dir.path().join("wt/.git"), "gitdir: /x").unwrap();
        let listing = list_dirs(dir.path().to_str().unwrap(), None, None, 10).unwrap();
        assert!(listing.entries[0].git);
    }

    #[test]
    fn prefix_filters_any_case_and_limit_truncates_hidden_first() {
        let dir = tempfile::tempdir().unwrap();
        mkdirs(dir.path(), &["code", "Config", "docs", ".cache", ".config"]);
        let raw = dir.path().to_str().unwrap();
        let listing = list_dirs(raw, Some("co"), None, MAX_ENTRIES).unwrap();
        assert_eq!(names(&listing), ["code", "Config"]);
        let dotted = list_dirs(raw, Some(".c"), None, MAX_ENTRIES).unwrap();
        assert_eq!(names(&dotted), [".cache", ".config"]);
        let capped = list_dirs(raw, None, None, 3).unwrap();
        assert_eq!(names(&capped), ["code", "Config", "docs"]);
        assert!(capped.truncated);
    }

    #[test]
    fn tilde_and_blank_resolve_to_home() {
        let dir = tempfile::tempdir().unwrap();
        mkdirs(dir.path(), &["code/app"]);
        let home = dir.path().canonicalize().unwrap();
        let listing = list_dirs("~/code", None, Some(&home), 10).unwrap();
        assert_eq!(listing.path, home.join("code"));
        assert_eq!(names(&listing), ["app"]);
        assert_eq!(listing.home.as_deref(), Some(home.as_path()));
        assert_eq!(list_dirs("~", None, Some(&home), 10).unwrap().path, home);
        assert_eq!(list_dirs("  ", None, Some(&home), 10).unwrap().path, home);
        assert_eq!(resolve_dir("", None).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn bad_paths_explain_themselves() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(matches!(
            list_dirs("relative/path", None, None, 10),
            Err(BrowseError::Invalid(_))
        ));
        assert!(matches!(
            list_dirs(dir.path().join("nope").to_str().unwrap(), None, None, 10),
            Err(BrowseError::NotFound(_))
        ));
        let not_dir = list_dirs(file.to_str().unwrap(), None, None, 10).unwrap_err();
        assert!(not_dir.to_string().contains("not a directory"), "{not_dir}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_dirs_list_broken_ones_carry_an_error_files_never() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdirs(root, &["real/.git"]);
        fs::write(root.join("file"), "").unwrap();
        symlink(root.join("real"), root.join("to-real")).unwrap();
        symlink(root.join("file"), root.join("to-file")).unwrap();
        symlink(root.join("gone"), root.join("to-gone")).unwrap();
        let listing = list_dirs(root.to_str().unwrap(), None, None, 10).unwrap();
        assert_eq!(names(&listing), ["real", "to-gone", "to-real"]);
        let gone = &listing.entries[1];
        assert!(gone.link && !gone.git);
        assert_eq!(gone.error.as_deref(), Some("broken link"));
        let linked = &listing.entries[2];
        assert!(linked.link && linked.git && linked.error.is_none());
        assert_eq!(linked.path, root.join("to-real"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_lists_and_refuses_to_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything: nothing to prove there.
        let readable = fs::read_dir(&locked).is_ok();
        let listing = list_dirs(dir.path().to_str().unwrap(), None, None, 10).unwrap();
        assert_eq!(names(&listing), ["locked"]);
        let opened = list_dirs(locked.to_str().unwrap(), None, None, 10);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(matches!(opened, Err(BrowseError::Denied(_))), "{opened:?}");
        }
    }

    fn paths(scan: &Scan, root: &Path) -> Vec<String> {
        let mut out: Vec<String> = scan
            .repos
            .iter()
            .map(|r| r.path.strip_prefix(root).unwrap().display().to_string())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn discovery_finds_repos_within_depth_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        mkdirs(
            &home,
            &[
                "code/app/.git",
                "code/app/nested/.git",
                "code/org/team/svc/.git",
                "code/org/team/deeper/repo/.git",
                "code/node_modules/pkg/.git",
                "code/target/x/.git",
                "code/.hidden/repo/.git",
                "loose/.git",
                "stuff/proj/.git",
                "stuff/a/b/.git",
            ],
        );
        fs::create_dir_all(home.join("code/app.worktrees/feature")).unwrap();
        fs::write(home.join("code/app.worktrees/feature/.git"), "gitdir: x").unwrap();
        let scan = discover_repos(&default_roots(&home), ScanBounds::DEFAULT);
        assert_eq!(
            paths(&scan, &home),
            [
                "code/app",
                "code/app.worktrees/feature",
                "code/org/team/svc",
                "loose",
                "stuff/proj"
            ]
        );
        assert!(!scan.truncated);
        assert_eq!(scan.roots, [home.join("code"), home.clone()]);
        let feature = scan
            .repos
            .iter()
            .find(|r| r.path.ends_with("feature"))
            .unwrap();
        assert!(feature.worktree);
        assert!(scan.repos.iter().all(|r| r.modified.is_some()));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_never_follows_symlinks_so_loops_end() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        mkdirs(&home, &["code/a/.git", "code/loop"]);
        symlink(home.join("code"), home.join("code/loop/back")).unwrap();
        symlink(home.join("code/a"), home.join("code/alias")).unwrap();
        let scan = discover_repos(&[(home.join("code"), 50)], ScanBounds::DEFAULT);
        assert_eq!(paths(&scan, &home), ["code/a"]);
    }

    #[test]
    fn discovery_stops_at_its_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..20 {
            fs::create_dir_all(root.join(format!("r{i:02}/.git"))).unwrap();
        }
        let few = ScanBounds {
            max_repos: 5,
            ..ScanBounds::DEFAULT
        };
        let scan = discover_repos(&[(root.clone(), 2)], few);
        assert_eq!(scan.repos.len(), 5);
        assert!(scan.truncated);
        let one_dir = ScanBounds {
            max_dirs: 1,
            ..ScanBounds::DEFAULT
        };
        mkdirs(&root, &["sub/inner/.git"]);
        let scan = discover_repos(&[(root.clone(), 3)], one_dir);
        assert!(scan.truncated);
        assert!(!paths(&scan, &root).contains(&"sub/inner".to_owned()));
        let no_time = ScanBounds {
            budget: Duration::ZERO,
            ..ScanBounds::DEFAULT
        };
        assert!(discover_repos(&[(root, 2)], no_time).truncated);
    }

    #[test]
    fn the_cache_reuses_a_fresh_scan_per_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        mkdirs(&home, &["code/one/.git"]);
        let cache = RepoScanCache::default();
        let first = cache.get(&home, ScanBounds::DEFAULT);
        assert_eq!(paths(&first, &home), ["code/one"]);
        mkdirs(&home, &["code/two/.git"]);
        let again = cache.get(&home, ScanBounds::DEFAULT);
        assert!(Arc::ptr_eq(&first, &again));
        let elsewhere = tempfile::tempdir().unwrap();
        let other = cache.get(
            &elsewhere.path().canonicalize().unwrap(),
            ScanBounds::DEFAULT,
        );
        assert!(other.repos.is_empty());
    }

    #[test]
    fn suggestions_rank_used_folders_then_fresh_repos_worktrees_last() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        mkdirs(
            &root,
            &[
                "used-old/.git",
                "used-new",
                "found-a/.git",
                "found-b/.git",
                "wt",
            ],
        );
        fs::write(root.join("wt/.git"), "gitdir: x").unwrap();
        let at = |s| Utc.timestamp_opt(s, 0).unwrap();
        let used = [
            UsedFolder {
                path: root.join("used-old"),
                workspace: "one".into(),
                when: at(100),
            },
            UsedFolder {
                path: root.join("used-new"),
                workspace: "two".into(),
                when: at(200),
            },
            UsedFolder {
                path: root.join("gone"),
                workspace: "two".into(),
                when: at(500),
            },
        ];
        let repo = |name: &str, modified, worktree| Repo {
            path: root.join(name),
            worktree,
            modified: Some(modified),
        };
        let scan = Scan {
            repos: vec![
                repo("wt", 900, true),
                repo("found-a", 10, false),
                repo("used-old", 999, false),
                repo("found-b", 20, false),
            ],
            ..Scan::default()
        };
        let ranked = suggestions(&used, &scan, MAX_SUGGESTIONS);
        let order: Vec<_> = ranked
            .iter()
            .map(|s| s.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(order, ["used-new", "used-old", "found-b", "found-a", "wt"]);
        assert_eq!(ranked[0].source, Source::Used);
        assert_eq!(ranked[0].workspace.as_deref(), Some("two"));
        assert!(!ranked[0].git && ranked[1].git);
        assert_eq!(ranked[2].source, Source::Found);
        assert!(ranked[4].worktree);
        assert_eq!(suggestions(&used, &scan, 2).len(), 2);
    }
}
