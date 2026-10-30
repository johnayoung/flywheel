//! Git metadata mounts for a linked-worktree sandbox.
//!
//! The landing strategy provisions the sandbox as a `git worktree`, whose
//! `.git` is a *file* (`gitdir: <repo>/.git/worktrees/<name>`) naming the
//! per-worktree git dir on the host, which in turn names the repository's
//! common git dir (`commondir`, usually `../..`). Bind-mounting only the
//! worktree leaves both paths dangling inside the container, so every
//! in-container `git` command fails with `not a git repository` and the
//! agent cannot commit its work (an uncommitted tree parks as
//! `uncommitted-work` instead of landing).
//!
//! [`git_metadata_mounts`] resolves those paths and returns the extra bind
//! mounts that make them resolve inside the container at the path the
//! `.git` file names: the common git dir read-write (a commit writes
//! objects, the branch ref, and the per-worktree index/HEAD), with the
//! repository `config` and `hooks/` overlaid read-only so the agent cannot
//! plant configuration or hooks that host-side git (the operator's checkout,
//! graders, landing) would later execute.

use crate::docker::VolumeMount;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why the worktree's git metadata could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum GitDirError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not a gitdir file (expected a `gitdir: <path>` line)")]
    Malformed { path: PathBuf },
    #[error("git dir {path} of the worktree does not exist")]
    MissingGitDir { path: PathBuf },
    #[error("relative git path {path} escapes the container root from {base}")]
    EscapesRoot { path: PathBuf, base: PathBuf },
    #[error("cannot create {path} to mount it read-only: {source}")]
    CreateHooks {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The bind mounts that make `worktree`'s git metadata reachable when the
/// worktree itself is mounted at `workdir` inside the container.
///
/// Empty when the worktree has no `.git` (a plain directory sandbox) or a
/// `.git` directory (a self-contained repository already inside the mount).
///
/// # Errors
///
/// [`GitDirError`] when the `.git` file or `commondir` is unreadable or
/// malformed, the named git dir is missing, a relative path climbs above the
/// container root, or the missing `hooks/` dir cannot be created.
pub fn git_metadata_mounts(
    worktree: &Path,
    workdir: &Path,
) -> Result<Vec<VolumeMount>, GitDirError> {
    let dotgit = worktree.join(".git");
    match fs::symlink_metadata(&dotgit) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(GitDirError::Read {
                path: dotgit,
                source,
            });
        }
    }
    let text = read(&dotgit)?;
    let target = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| GitDirError::Malformed {
            path: dotgit.clone(),
        })?;
    let target = Path::new(target);
    let host_gitdir = resolve(worktree, target)?;
    let sandbox_gitdir = resolve(workdir, target)?;
    if !host_gitdir.is_dir() {
        return Err(GitDirError::MissingGitDir { path: host_gitdir });
    }

    // A linked worktree's git dir names the shared common dir; a gitfile
    // without `commondir` (`--separate-git-dir`) is itself the whole repo.
    let commondir_file = host_gitdir.join("commondir");
    let (host_common, sandbox_common) = if commondir_file.is_file() {
        let common = read(&commondir_file)?;
        let common = Path::new(common.trim());
        (
            resolve(&host_gitdir, common)?,
            resolve(&sandbox_gitdir, common)?,
        )
    } else {
        (host_gitdir.clone(), sandbox_gitdir.clone())
    };

    let mut mounts = vec![VolumeMount::new(
        path_string(&host_common),
        path_string(&sandbox_common),
    )];
    if !host_gitdir.starts_with(&host_common) {
        mounts.push(VolumeMount::new(
            path_string(&host_gitdir),
            path_string(&sandbox_gitdir),
        ));
    }
    let config = host_common.join("config");
    if config.is_file() {
        mounts.push(
            VolumeMount::new(
                path_string(&config),
                path_string(&sandbox_common.join("config")),
            )
            .readonly(),
        );
    }
    // Mounted even when absent (created empty, as `git init` would): an
    // agent-created `hooks/` in a read-write common dir would run host-side.
    let hooks = host_common.join("hooks");
    if !hooks.is_dir() {
        fs::create_dir_all(&hooks).map_err(|source| GitDirError::CreateHooks {
            path: hooks.clone(),
            source,
        })?;
    }
    mounts.push(
        VolumeMount::new(
            path_string(&hooks),
            path_string(&sandbox_common.join("hooks")),
        )
        .readonly(),
    );
    Ok(mounts)
}

fn read(path: &Path) -> Result<String, GitDirError> {
    fs::read_to_string(path).map_err(|source| GitDirError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `path` as-is when absolute, else lexically joined onto `base` (git
/// resolves gitdir/commondir relative to the file's directory without
/// following symlinks in the relative part).
fn resolve(base: &Path, path: &Path) -> Result<PathBuf, GitDirError> {
    if path.is_absolute() {
        return Ok(normalize(path).unwrap_or_else(|| path.to_path_buf()));
    }
    normalize(&base.join(path)).ok_or_else(|| GitDirError::EscapesRoot {
        path: path.to_path_buf(),
        base: base.to_path_buf(),
    })
}

/// Lexically fold `.` and `..`; `None` when `..` climbs above the root.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_with_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = repo.join(".flywheel/worktrees/alpha");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "flywheel/alpha",
                wt.to_str().unwrap(),
            ],
        );
        (repo, wt)
    }

    #[test]
    fn plain_directory_and_dot_git_directory_need_no_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(
            git_metadata_mounts(&plain, Path::new("/w"))
                .unwrap()
                .is_empty()
        );
        fs::create_dir_all(plain.join(".git")).unwrap();
        assert!(
            git_metadata_mounts(&plain, Path::new("/w"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn linked_worktree_mounts_common_dir_with_config_and_hooks_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, wt) = repo_with_worktree(dir.path());
        let common = fs::canonicalize(repo.join(".git")).unwrap();
        fs::remove_dir_all(common.join("hooks")).unwrap();

        let mounts = git_metadata_mounts(&wt, Path::new("/home/agent/workspace")).unwrap();
        let c = common.display().to_string();
        assert_eq!(
            mounts,
            vec![
                VolumeMount::new(c.clone(), c.clone()),
                VolumeMount::new(format!("{c}/config"), format!("{c}/config")).readonly(),
                VolumeMount::new(format!("{c}/hooks"), format!("{c}/hooks")).readonly(),
            ]
        );
        assert!(
            common.join("hooks").is_dir(),
            "missing hooks dir is created"
        );
    }

    #[test]
    fn relative_gitdir_resolves_against_each_side_of_the_mount() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let common = root.join("repo/.git");
        let gitdir = common.join("worktrees/alpha");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(common.join("config"), "").unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let wt = root.join("repo/wt/alpha");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: ../../.git/worktrees/alpha\n").unwrap();

        let mounts = git_metadata_mounts(&wt, Path::new("/home/agent/workspace")).unwrap();
        assert_eq!(mounts[0].host_path, common.display().to_string());
        assert_eq!(mounts[0].sandbox_path, "/home/.git");
        assert_eq!(mounts[1].sandbox_path, "/home/.git/config");
        assert!(mounts[1].readonly);
        assert_eq!(mounts[2].sandbox_path, "/home/.git/hooks");

        let err = git_metadata_mounts(&wt, Path::new("/w")).unwrap_err();
        assert!(matches!(err, GitDirError::EscapesRoot { .. }), "{err}");
    }

    #[test]
    fn malformed_or_dangling_gitfile_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "not a gitfile\n").unwrap();
        assert!(matches!(
            git_metadata_mounts(&wt, Path::new("/w")),
            Err(GitDirError::Malformed { .. })
        ));
        fs::write(wt.join(".git"), "gitdir: /nonexistent/flywheel/gitdir\n").unwrap();
        assert!(matches!(
            git_metadata_mounts(&wt, Path::new("/w")),
            Err(GitDirError::MissingGitDir { .. })
        ));
    }

    #[test]
    fn normalize_folds_dots_and_refuses_to_climb_above_root() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            Some(PathBuf::from("/a/c/d"))
        );
        assert_eq!(normalize(Path::new("/a/..")), Some(PathBuf::from("/")));
        assert_eq!(normalize(Path::new("/..")), None);
    }
}
