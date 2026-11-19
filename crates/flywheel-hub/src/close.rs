//! The close checklist: what ending a session leaves behind in its
//! directory, and the cleanups that lose nothing.
//!
//! Before an end (`end_session` with `dry_run`, `fw hub close --dry-run`)
//! the checklist says whether its branch is merged into its base (by
//! ancestry, or every patch of it), its uncommitted and untracked files,
//! its build output, and who else works there. The end may carry choices:
//!
//! * **remove** the worktree: only when it has no uncommitted or
//!   untracked files and no other session works in it; ignored files
//!   (build output) go with it; its branch is deleted only when its base
//!   has every commit of it, else kept. Never forced.
//! * **clean** the build: deletes the worktree's own `target/` (a Cargo
//!   project's, ignored by git, not a symlink). Never `cargo clean`: that
//!   follows a shared `CARGO_TARGET_DIR` into every worker's cache.
//! * **report**: the session's final status file, copied into the hub's
//!   `reports/` before anything is removed, shown after the end.
//!
//! Choices are applied after the session ended (nothing runs then), each
//! checked again against a fresh checklist: one that no longer holds is
//! refused, with why, and kept. The outcome is the end record's `close`.

use crate::git;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long sizing one directory may take before it answers "at least".
const SIZE_BUDGET: Duration = Duration::from_secs(60);
/// Untracked paths a checklist lists (the rest counted).
const UNTRACKED_LISTED: usize = 20;

/// What to do with a worktree when its session ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeChoice {
    #[default]
    Keep,
    Remove,
}

/// What to do with build output when its session ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildChoice {
    #[default]
    Keep,
    Clean,
}

/// The choices an end carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseChoices {
    #[serde(default)]
    pub worktree: WorktreeChoice,
    #[serde(default)]
    pub build: BuildChoice,
    /// The final status file (absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<PathBuf>,
}

impl CloseChoices {
    pub fn is_empty(&self) -> bool {
        self.worktree == WorktreeChoice::Keep
            && self.build == BuildChoice::Keep
            && self.report.is_none()
    }
}

/// A path and its size on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizedPath {
    pub path: PathBuf,
    pub bytes: u64,
    /// The walk ran out of time: at least `bytes`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// Whether a branch is in its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Merged {
    /// Every commit is in the base.
    Ancestor,
    /// Not by ancestry, but every patch is (squashed or rebased in):
    /// `commits` of them.
    PatchEquivalent { commits: u32 },
    /// `commits` with patches the base lacks.
    Unmerged { commits: u32 },
}

/// What a session's directory holds before it is closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checklist {
    pub path: PathBuf,
    /// Its own worktree (the hub's), not a shared checkout.
    pub worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<Merged>,
    pub dirty: u32,
    /// Untracked files and directories at its top, sized (the first
    /// [`UNTRACKED_LISTED`]).
    pub untracked: Vec<SizedPath>,
    pub untracked_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<SizedPath>,
    /// Other sessions in the directory or on the branch (names).
    pub shared_with: Vec<String>,
    /// Why removing it is refused (`None`: it may be removed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cannot_remove: Option<String>,
    /// Removing it deletes its branch too (it is merged by ancestry).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removes_branch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cannot_clean: Option<String>,
}

/// What became of one choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Kept,
    Done { detail: String },
    Refused { why: String },
}

/// How a session's directory was closed (on its end record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseRecord {
    pub checklist: Checklist,
    pub worktree: Outcome,
    pub build: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<crate::reports::ReportRef>,
    /// Why the report was not kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_error: Option<String>,
    /// Unix ms.
    pub at: i64,
}

/// Bytes under `path` (not following symlinks), walking for at most
/// `budget`.
pub fn size_of(path: &Path, budget: Duration) -> SizedPath {
    let started = Instant::now();
    let mut bytes = 0u64;
    let mut partial = false;
    let mut stack = vec![path.to_path_buf()];
    while let Some(at) = stack.pop() {
        if started.elapsed() > budget {
            partial = true;
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&at) {
                stack.extend(entries.filter_map(Result::ok).map(|e| e.path()));
            }
        } else {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    SizedPath {
        path: path.to_path_buf(),
        bytes,
        partial,
    }
}

/// The build output a clean may delete in `dir`: its own `target/`, in a
/// Cargo project, ignored by git, and not a symlink.
pub fn build_dir(dir: &Path) -> Result<PathBuf, String> {
    let target = dir.join("target");
    let meta = std::fs::symlink_metadata(&target)
        .map_err(|_| format!("{} has no target/ dir", dir.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "{} is not a directory of its own",
            target.display()
        ));
    }
    if !dir.join("Cargo.toml").is_file() {
        return Err(format!("{} is not a Cargo project", dir.display()));
    }
    if !git::is_ignored(dir, &target) {
        return Err(format!("{} is not ignored by git", target.display()));
    }
    Ok(target)
}

/// The checklist for `dir`. `worktree`: its hub worktree, if it runs in
/// one; `shared_with`: the other sessions working there or on its branch.
pub fn checklist(
    dir: &Path,
    worktree: Option<&crate::git::WorktreeRef>,
    shared_with: Vec<String>,
) -> Checklist {
    let status = git::status(dir, worktree.map(|w| w.base.as_str())).ok();
    let branch = worktree
        .map(|w| w.branch.clone())
        .or_else(|| status.as_ref().and_then(|s| s.branch.clone()));
    let base = worktree.map(|w| w.base.clone());
    let merged = worktree.and_then(|w| {
        let check = git::removal_check(w).ok()?;
        if check.unmerged == 0 {
            return Some(Merged::Ancestor);
        }
        Some(match git::unmerged_patches(&w.repo, &w.branch, &w.base) {
            Ok(0) => Merged::PatchEquivalent {
                commits: check.unmerged,
            },
            Ok(n) => Merged::Unmerged { commits: n },
            Err(_) => Merged::Unmerged {
                commits: check.unmerged,
            },
        })
    });
    let untracked_all = git::untracked_paths(dir).unwrap_or_default();
    let untracked_count = u32::try_from(untracked_all.len()).unwrap_or(u32::MAX);
    let untracked: Vec<SizedPath> = untracked_all
        .iter()
        .take(UNTRACKED_LISTED)
        .map(|p| size_of(p, Duration::from_secs(5)))
        .collect();
    let dirty = status.as_ref().map_or(0, |s| s.dirty);
    let (build, cannot_clean) = match build_dir(dir) {
        Ok(target) => (Some(size_of(&target, SIZE_BUDGET)), None),
        Err(why) => (None, Some(why)),
    };
    let cannot_clean = cannot_clean.or_else(|| {
        (!shared_with.is_empty()).then(|| format!("{} also work there", shared_with.join(", ")))
    });
    let cannot_remove = if worktree.is_none() {
        Some("it does not run in a worktree of its own".to_owned())
    } else if dirty > 0 || untracked_count > 0 {
        Some(format!(
            "{dirty} uncommitted change(s) and {untracked_count} untracked path(s) would be lost"
        ))
    } else if !shared_with.is_empty() {
        Some(format!("{} also work there", shared_with.join(", ")))
    } else {
        None
    };
    Checklist {
        path: dir.to_path_buf(),
        worktree: worktree.is_some(),
        branch,
        base,
        merged,
        dirty,
        untracked,
        untracked_count,
        build,
        shared_with,
        removes_branch: matches!(merged, Some(Merged::Ancestor)),
        cannot_remove,
        cannot_clean,
    }
}

fn gib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // a size in words
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 0.1 {
        format!("{gib:.1} GiB")
    } else {
        format!("{} MiB", bytes / (1024 * 1024))
    }
}

impl Checklist {
    /// In words, for an agent or a terminal.
    pub fn text(&self) -> String {
        let mut lines = vec![format!(
            "{}{}",
            self.path.display(),
            self.branch
                .as_deref()
                .map(|b| format!(" (branch {b})"))
                .unwrap_or_default()
        )];
        if let (Some(merged), Some(base)) = (self.merged, &self.base) {
            lines.push(match merged {
                Merged::Ancestor => format!("- merged into {base}"),
                Merged::PatchEquivalent { commits } => format!(
                    "- merged into {base} by patch ({commits} commits squashed or rebased in; \
                     the branch is kept)"
                ),
                Merged::Unmerged { commits } => {
                    format!("- NOT merged: {commits} commit(s) {base} lacks (the branch is kept)")
                }
            });
        }
        lines.push(format!(
            "- {} uncommitted change(s), {} untracked path(s){}",
            self.dirty,
            self.untracked_count,
            if self.untracked.is_empty() {
                String::new()
            } else {
                format!(
                    ": {}",
                    self.untracked
                        .iter()
                        .map(|u| format!(
                            "{} ({})",
                            u.path.strip_prefix(&self.path).unwrap_or(&u.path).display(),
                            gib(u.bytes)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ));
        lines.push(match &self.build {
            Some(b) => format!(
                "- build output target/: {}{}",
                if b.partial { "at least " } else { "" },
                gib(b.bytes)
            ),
            None => "- no build output to clean".to_owned(),
        });
        if !self.shared_with.is_empty() {
            lines.push(format!("- also used by: {}", self.shared_with.join(", ")));
        }
        lines.push(match &self.cannot_remove {
            None => format!(
                "- remove: allowed ({})",
                if self.removes_branch {
                    "its branch is deleted too: fully merged"
                } else {
                    "its branch is kept"
                }
            ),
            Some(why) => format!("- remove: refused: {why}"),
        });
        lines.push(match &self.cannot_clean {
            None => "- clean build: allowed".to_owned(),
            Some(why) => format!("- clean build: refused: {why}"),
        });
        lines.join("\n")
    }
}

/// Apply `choices` to `dir` against a fresh checklist (the session has
/// ended); the report is copied first. `state_dir`, `name`: where the
/// report copy goes; `roots`: where it may be read from.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    choices: &CloseChoices,
    dir: &Path,
    worktree: Option<&crate::git::WorktreeRef>,
    shared_with: Vec<String>,
    state_dir: &Path,
    name: &str,
    roots: Option<&[PathBuf]>,
    at: i64,
) -> CloseRecord {
    let (report, report_error) = match &choices.report {
        Some(path) => match crate::reports::keep(state_dir, "endings", name, path, roots) {
            Ok(kept) => (Some(kept), None),
            Err(why) => (None, Some(why)),
        },
        None => (None, None),
    };
    let checklist = checklist(dir, worktree, shared_with);
    let build = match choices.build {
        BuildChoice::Keep => Outcome::Kept,
        BuildChoice::Clean => match (&checklist.cannot_clean, &checklist.build) {
            (Some(why), _) => Outcome::Refused { why: why.clone() },
            (None, Some(b)) => match std::fs::remove_dir_all(&b.path) {
                Ok(()) => Outcome::Done {
                    detail: format!("deleted {} ({})", b.path.display(), gib(b.bytes)),
                },
                Err(e) => Outcome::Refused {
                    why: format!("{}: {e}", b.path.display()),
                },
            },
            (None, None) => Outcome::Refused {
                why: "no build output".into(),
            },
        },
    };
    let worktree_outcome = match (choices.worktree, worktree) {
        (WorktreeChoice::Keep, _) => Outcome::Kept,
        (WorktreeChoice::Remove, None) => Outcome::Refused {
            why: "it does not run in a worktree of its own".into(),
        },
        (WorktreeChoice::Remove, Some(w)) => match &checklist.cannot_remove {
            Some(why) => Outcome::Refused { why: why.clone() },
            None => match git::remove_keeping_work(w) {
                Ok(removed) => Outcome::Done {
                    detail: format!(
                        "removed {}; branch {} {}",
                        removed.path.display(),
                        removed.branch,
                        if removed.branch_deleted {
                            "deleted (fully merged)"
                        } else {
                            "kept"
                        }
                    ),
                },
                Err(why) => Outcome::Refused { why },
            },
        },
    };
    CloseRecord {
        checklist,
        worktree: worktree_outcome,
        build,
        report,
        report_error,
        at,
    }
}

impl Outcome {
    pub fn text(&self) -> String {
        match self {
            Self::Kept => "kept".to_owned(),
            Self::Done { detail } => detail.clone(),
            Self::Refused { why } => format!("refused ({why}), kept"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with `main`, and a worktree on `feat` cut from it with one
    /// commit; the worktree's ref.
    fn fixture(root: &Path) -> crate::git::WorktreeRef {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let wt = root.join("repo.worktrees/feat");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("a"), "a").unwrap();
        git(&wt, &["add", "a"]);
        git(&wt, &["commit", "-q", "-m", "a"]);
        crate::git::WorktreeRef {
            repo: repo.canonicalize().unwrap(),
            path: wt.canonicalize().unwrap(),
            branch: "feat".into(),
            base: "main".into(),
        }
    }

    fn remove_and_clean() -> CloseChoices {
        CloseChoices {
            worktree: WorktreeChoice::Remove,
            build: BuildChoice::Clean,
            report: None,
        }
    }

    #[test]
    fn an_unmerged_clean_worktree_is_removed_and_its_branch_kept() {
        let dir = tempfile::tempdir().unwrap();
        let w = fixture(dir.path());
        std::fs::create_dir_all(w.path.join("target/debug")).unwrap();
        std::fs::write(w.path.join("target/debug/big"), vec![0u8; 4096]).unwrap();
        let list = checklist(&w.path, Some(&w), Vec::new());
        assert_eq!(list.merged, Some(Merged::Unmerged { commits: 1 }));
        assert_eq!(list.build.as_ref().unwrap().bytes, 4096);
        assert!(list.cannot_remove.is_none(), "{list:?}");
        assert!(!list.removes_branch);
        let done = apply(
            &remove_and_clean(),
            &w.path,
            Some(&w),
            Vec::new(),
            dir.path(),
            "x",
            None,
            0,
        );
        assert!(matches!(done.build, Outcome::Done { .. }), "{done:?}");
        assert!(matches!(done.worktree, Outcome::Done { .. }), "{done:?}");
        assert!(!w.path.exists());
        let branches = std::process::Command::new("git")
            .args(["-C", w.repo.to_str().unwrap(), "branch", "--list", "feat"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains("feat"),
            "the branch is kept"
        );
    }

    #[test]
    fn a_merged_worktree_takes_its_branch_and_a_squashed_one_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let w = fixture(dir.path());
        git(&w.repo, &["merge", "-q", "--ff-only", "feat"]);
        let list = checklist(&w.path, Some(&w), Vec::new());
        assert_eq!(list.merged, Some(Merged::Ancestor));
        assert!(list.removes_branch);

        let dir2 = tempfile::tempdir().unwrap();
        let w2 = fixture(dir2.path());
        git(&w2.repo, &["merge", "-q", "--squash", "feat"]);
        git(&w2.repo, &["commit", "-q", "-m", "squashed"]);
        let list = checklist(&w2.path, Some(&w2), Vec::new());
        assert_eq!(list.merged, Some(Merged::PatchEquivalent { commits: 1 }));
        assert!(!list.removes_branch);
    }

    #[test]
    fn work_that_would_be_lost_or_shared_refuses_and_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let w = fixture(dir.path());
        std::fs::write(w.path.join("notes.md"), "keep me").unwrap();
        let done = apply(
            &remove_and_clean(),
            &w.path,
            Some(&w),
            Vec::new(),
            dir.path(),
            "x",
            None,
            0,
        );
        assert!(matches!(done.worktree, Outcome::Refused { .. }), "{done:?}");
        assert_eq!(done.checklist.untracked_count, 1);
        assert!(w.path.join("notes.md").exists());
        std::fs::remove_file(w.path.join("notes.md")).unwrap();
        let done = apply(
            &remove_and_clean(),
            &w.path,
            Some(&w),
            vec!["other".into()],
            dir.path(),
            "x",
            None,
            0,
        );
        assert!(
            matches!(&done.worktree, Outcome::Refused { why } if why.contains("other")),
            "{done:?}"
        );
        assert!(w.path.exists());
    }

    #[test]
    fn clean_never_follows_a_symlinked_target() {
        let dir = tempfile::tempdir().unwrap();
        let w = fixture(dir.path());
        let shared = dir.path().join("shared-target");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("cache"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&shared, w.path.join("target")).unwrap();
        let done = apply(
            &CloseChoices {
                build: BuildChoice::Clean,
                ..CloseChoices::default()
            },
            &w.path,
            Some(&w),
            Vec::new(),
            dir.path(),
            "x",
            None,
            0,
        );
        assert!(matches!(done.build, Outcome::Refused { .. }), "{done:?}");
        assert!(shared.join("cache").exists());
    }

    #[test]
    fn the_report_is_kept_before_the_worktree_goes() {
        let dir = tempfile::tempdir().unwrap();
        let w = fixture(dir.path());
        // A committed status file: the tree stays clean.
        std::fs::write(w.path.join("STATUS.md"), "# Done\n\nall of it\n").unwrap();
        git(&w.path, &["add", "STATUS.md"]);
        git(&w.path, &["commit", "-q", "-m", "status"]);
        let choices = CloseChoices {
            worktree: WorktreeChoice::Remove,
            build: BuildChoice::Keep,
            report: Some(w.path.join("STATUS.md")),
        };
        let roots = vec![w.path.clone()];
        let state = dir.path().join("state");
        let done = apply(
            &choices,
            &w.path,
            Some(&w),
            Vec::new(),
            &state,
            "s-1",
            Some(&roots),
            0,
        );
        assert!(!w.path.exists());
        let report = done.report.unwrap();
        assert_eq!(report.title, "Done");
        assert!(
            std::fs::read_to_string(report.stored)
                .unwrap()
                .contains("all of it")
        );
    }
}
