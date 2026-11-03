//! What a candidate's worktree holds, read with git (through the review
//! runner: argv only, no terminal, a deadline, output capped): HEAD, the
//! status a verdict describes, and the diff against the fan-out's base
//! commit.

use super::DiffStat;
use crate::review::Tools;
use crate::review::runner::{self, Spec};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const READ_LIMIT: usize = 16 * 1024 * 1024;
/// Untracked files are counted as added lines up to this size each.
const UNTRACKED_READ: u64 = 1024 * 1024;

/// Config a worktree could set to make git report less than is there: an
/// fsmonitor hook answering "nothing changed", a stale untracked cache.
/// Overridden on every read (`-c` beats the repo's config).
const HONEST: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
];

fn git(tools: &Tools, dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    git_with(tools, dir, args, None)
}

fn git_with(
    tools: &Tools,
    dir: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let output = runner::run(&Spec {
        program: &tools.git,
        dir,
        args: HONEST
            .iter()
            .chain(args.iter())
            .map(|a| (*a).to_owned())
            .collect(),
        stdin,
        timeout: READ_TIMEOUT,
        limit: READ_LIMIT,
    })
    .map_err(|e| e.to_string())?;
    if !output.success() {
        return Err(format!("git {}: {}", args.join(" "), output.error_text()));
    }
    if output.truncated {
        return Err(format!("git {}: output too large", args.join(" ")));
    }
    Ok(output.stdout)
}

/// The worktree's state a verdict describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    pub head: Option<String>,
    /// Tracked paths with changes (staged or not, conflicts included).
    pub dirty: u32,
    /// Untracked paths (ignored ones not counted).
    pub untracked_paths: Vec<String>,
    /// Digest of HEAD and the full status: equal digests, same tree as
    /// far as git's status tells (a clean tree is exact: HEAD alone).
    pub fingerprint: String,
}

/// HEAD (`None`: no commits) and `git status` of `dir`.
pub fn tree_state(tools: &Tools, dir: &Path) -> Result<TreeState, String> {
    if !dir.is_dir() {
        return Err(format!("{} is gone", dir.display()));
    }
    let head = git(tools, dir, &["rev-parse", "--verify", "-q", "HEAD"])
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|h| !h.is_empty());
    let status = git(
        tools,
        dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let (dirty, untracked_paths) = parse_status(&status);
    let mut hasher = Sha256::new();
    hasher.update(head.as_deref().unwrap_or("-").as_bytes());
    hasher.update([0]);
    hasher.update(&status);
    let digest = hasher.finalize();
    let fingerprint = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    Ok(TreeState {
        head,
        dirty,
        untracked_paths,
        fingerprint,
    })
}

/// `git status --porcelain=v1 -z` -> (changed tracked paths, untracked
/// paths). A rename or copy entry carries its source as the next field.
pub fn parse_status(raw: &[u8]) -> (u32, Vec<String>) {
    let mut dirty = 0;
    let mut untracked = Vec::new();
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry[0], entry[1]);
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if x == b'?' && y == b'?' {
            untracked.push(path);
            continue;
        }
        if x == b'!' {
            continue;
        }
        dirty += 1;
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            fields.next();
        }
    }
    (dirty, untracked)
}

/// What `dir` changed against commit `base_sha`: tracked paths (committed,
/// staged, and unstaged: the working tree against that commit) plus the
/// untracked ones; the changed paths, for the protected-path check.
pub fn diff_against(
    tools: &Tools,
    dir: &Path,
    base_sha: &str,
    untracked: &[String],
) -> Result<(DiffStat, Vec<String>), String> {
    let out = git(
        tools,
        dir,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base_sha,
            "--",
        ],
    )?;
    let mut stat = DiffStat::default();
    let mut paths = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // `added\tdeleted\tpath` (`-\t-\t` for binary files).
        let text = String::from_utf8_lossy(record);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        stat.files += 1;
        stat.insertions += added.parse::<u64>().unwrap_or(0);
        stat.deletions += deleted.parse::<u64>().unwrap_or(0);
        paths.push(path.to_owned());
    }
    for path in untracked {
        stat.files += 1;
        stat.untracked += 1;
        stat.insertions += count_lines(&dir.join(path));
        paths.push(path.clone());
    }
    Ok((stat, paths))
}

/// Lines in a (text) regular file, reading at most [`UNTRACKED_READ`]; a
/// binary file, a symlink, a FIFO, or a device counts none. Opened without
/// following a symlink and without blocking (a FIFO would block an open
/// for reading until a writer came), then checked to be a regular file.
fn count_lines(path: &Path) -> u64 {
    let Ok(file) = open_regular(path) else {
        return 0;
    };
    let mut bytes = Vec::new();
    if file.take(UNTRACKED_READ).read_to_end(&mut bytes).is_err() || bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    newlines + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

#[cfg(unix)]
fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    std::fs::File::open(path)
}

/// Tracked paths git status and diff skip in `dir`: flagged
/// assume-unchanged (`git ls-files -v` tag in lower case) or skip-worktree
/// (tag `S`). Their working-tree contents are invisible to every check
/// that reads git status, so a grading refuses to run over them.
pub fn hidden_paths(tools: &Tools, dir: &Path) -> Result<Vec<String>, String> {
    let out = git(tools, dir, &["ls-files", "-v", "-z"])?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|entry| entry.len() > 2)
        .filter(|entry| entry[0].is_ascii_lowercase() || entry[0] == b'S')
        .map(|entry| String::from_utf8_lossy(&entry[2..]).into_owned())
        .collect())
}

/// The files under `protect` (repo-relative paths) whose working-tree
/// contents differ from commit `since`, compared by content hash rather
/// than through git status (which index flags and config can blind): a
/// file of `since` changed, replaced by another type, or deleted.
pub fn protected_changes(
    tools: &Tools,
    dir: &Path,
    since: &str,
    protect: &[String],
) -> Result<Vec<String>, String> {
    if protect.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", since, "--"];
    args.extend(protect.iter().map(String::as_str));
    let listing = git(tools, dir, &args)?;
    let mut changed = Vec::new();
    // (path, the blob it holds at `since`) of regular files to hash.
    let mut files: Vec<(String, String)> = Vec::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // `<mode> <type> <object>\t<path>`
        let text = String::from_utf8_lossy(record);
        let Some((meta, path)) = text.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(object)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if kind != "blob" {
            continue;
        }
        let on_disk = dir.join(path);
        let meta = std::fs::symlink_metadata(&on_disk);
        if mode == "120000" {
            let same = meta.as_ref().is_ok_and(|m| m.file_type().is_symlink())
                && std::fs::read_link(&on_disk).ok().is_some_and(|target| {
                    git(tools, dir, &["cat-file", "blob", object])
                        .is_ok_and(|blob| blob == target.as_os_str().as_encoded_bytes())
                });
            if !same {
                changed.push(path.to_owned());
            }
            continue;
        }
        if meta.as_ref().is_ok_and(std::fs::Metadata::is_file) {
            files.push((path.to_owned(), object.to_owned()));
        } else {
            changed.push(path.to_owned());
        }
    }
    if !files.is_empty() {
        let mut stdin = Vec::new();
        for (path, _) in &files {
            stdin.extend_from_slice(path.as_bytes());
            stdin.push(b'\n');
        }
        let out = git_with(tools, dir, &["hash-object", "--stdin-paths"], Some(stdin))?;
        let hashes: Vec<&str> = std::str::from_utf8(&out)
            .map_err(|_| "git hash-object: unreadable output".to_owned())?
            .lines()
            .collect();
        if hashes.len() != files.len() {
            return Err("git hash-object: one hash per protected file expected".into());
        }
        for ((path, object), hash) in files.iter().zip(hashes) {
            if hash.trim() != object {
                changed.push(path.clone());
            }
        }
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

/// The commit `rev` names in `dir` (`None`: it names none).
pub fn resolve_commit(tools: &Tools, dir: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    git(tools, dir, &["rev-parse", "--verify", "-q", &spec])
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|sha| !sha.is_empty())
}

/// Where `dir`'s HEAD left branch `base` (`None`: no common commit, or
/// the branch is gone): a rebased candidate's own work starts there.
pub fn merge_base(tools: &Tools, dir: &Path, base: &str) -> Option<String> {
    let base = format!("refs/heads/{base}");
    git(tools, dir, &["merge-base", "HEAD", &base])
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|sha| !sha.is_empty())
}

/// The branch checked out in `dir` (`None`: detached HEAD).
pub fn current_branch(tools: &Tools, dir: &Path) -> Option<String> {
    git(tools, dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|b| !b.is_empty())
}

/// Whether `name` is a local branch of the repo at `dir`.
pub fn is_local_branch(tools: &Tools, dir: &Path, name: &str) -> bool {
    git(
        tools,
        dir,
        &["show-ref", "--verify", "-q", &format!("refs/heads/{name}")],
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_entries_are_counted_with_renames_skipping_their_source() {
        let raw = b" M src/a.rs\0R  new.rs\0old.rs\0?? notes/todo.txt\0A  added.rs\0";
        let (dirty, untracked) = parse_status(raw);
        assert_eq!(dirty, 3);
        assert_eq!(untracked, vec!["notes/todo.txt".to_owned()]);
        assert_eq!(parse_status(b""), (0, vec![]));
    }

    #[test]
    fn untracked_lines_count_text_only() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("a.txt");
        std::fs::write(&text, "one\ntwo\nthree").unwrap();
        assert_eq!(count_lines(&text), 3);
        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [0u8, 1, 2, b'\n']).unwrap();
        assert_eq!(count_lines(&bin), 0);
        assert_eq!(count_lines(&dir.path().join("missing")), 0);
    }

    fn sh_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repo with a committed grader script that fails and a test file.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        sh_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("check.sh"), "exit 1\n").unwrap();
        std::fs::create_dir(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/a.txt"), "a\n").unwrap();
        std::fs::write(root.join("free.txt"), "free\n").unwrap();
        sh_git(&root, &["add", "-A"]);
        sh_git(&root, &["commit", "-qm", "base"]);
        let base = resolve_commit(&Tools::default(), &root, "HEAD").unwrap();
        (dir, root, base)
    }

    #[test]
    fn index_flags_cannot_hide_a_protected_edit() {
        let tools = Tools::default();
        let (_dir, root, base) = repo();
        sh_git(&root, &["update-index", "--skip-worktree", "check.sh"]);
        sh_git(
            &root,
            &["update-index", "--assume-unchanged", "tests/a.txt"],
        );
        std::fs::write(root.join("check.sh"), "exit 0\n").unwrap();
        std::fs::write(root.join("tests/a.txt"), "rigged\n").unwrap();
        // git status sees nothing...
        let state = tree_state(&tools, &root).unwrap();
        assert_eq!((state.dirty, state.untracked_paths.len()), (0, 0));
        // ...but the flags are found, and the contents compared by hash.
        let mut hidden = hidden_paths(&tools, &root).unwrap();
        hidden.sort();
        assert_eq!(hidden, ["check.sh", "tests/a.txt"]);
        let protect = vec!["check.sh".to_owned(), "tests".to_owned()];
        assert_eq!(
            protected_changes(&tools, &root, &base, &protect).unwrap(),
            ["check.sh", "tests/a.txt"]
        );
        // Unprotected, unchanged, and deleted files.
        assert!(
            protected_changes(&tools, &root, &base, &["free.txt".to_owned()])
                .unwrap()
                .is_empty()
        );
        std::fs::remove_file(root.join("free.txt")).unwrap();
        assert_eq!(
            protected_changes(&tools, &root, &base, &["free.txt".to_owned()]).unwrap(),
            ["free.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_untracked_symlink_to_a_fifo_does_not_block_the_diff() {
        let tools = Tools::default();
        let (_dir, root, base) = repo();
        let fifo = root.parent().unwrap().join(format!(
            "fifo-{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path; mkfifo(3) only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(&fifo, root.join("hang")).unwrap();
        let c2 =
            std::ffi::CString::new(root.join("direct").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { libc::mkfifo(c2.as_ptr(), 0o600) }, 0);
        let state = tree_state(&tools, &root).unwrap();
        assert!(state.untracked_paths.contains(&"hang".to_owned()));
        let (tx, rx) = std::sync::mpsc::channel();
        let untracked = state.untracked_paths.clone();
        let dir = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(diff_against(&Tools::default(), &dir, &base, &untracked));
        });
        let (stat, _) = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the diff blocked on a FIFO")
            .unwrap();
        assert_eq!(stat.insertions, 0);
        assert_eq!(count_lines(&root.join("direct")), 0);
        let _ = std::fs::remove_file(&fifo);
    }
}
