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

fn git(tools: &Tools, dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = runner::run(&Spec {
        program: &tools.git,
        dir,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdin: None,
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
        &["diff", "--numstat", "-z", "--no-renames", base_sha, "--"],
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

/// Lines in a (text) file, reading at most [`UNTRACKED_READ`]; a binary
/// file counts none.
fn count_lines(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut bytes = Vec::new();
    if file.take(UNTRACKED_READ).read_to_end(&mut bytes).is_err() || bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    newlines + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
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
}
