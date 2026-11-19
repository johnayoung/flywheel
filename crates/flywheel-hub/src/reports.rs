//! Reports the hub keeps a copy of: a review's full text, a session's
//! final status file. Copied into `<state-dir>/reports/` when given, so
//! they outlive the worktree they were written in.
//!
//! An agent may only name a regular file under the directories its
//! session works in (its cwd, its worktree, its workspace's folders and
//! their sibling `.worktrees` dirs), symlinks resolved first: the hub
//! reads it with the operator's rights, and a report path must not be a
//! way to copy `~/.ssh` somewhere others can read it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The reports dir in the state dir.
pub const REPORTS_DIR: &str = "reports";
/// The largest report kept.
pub const MAX_REPORT_BYTES: u64 = 256 * 1024;
/// Characters of its first line kept as its title.
const TITLE_CHARS: usize = 120;

/// A report the hub keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRef {
    /// The hub's copy.
    pub stored: PathBuf,
    /// Where it was read from.
    pub from: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    /// Its first non-empty line.
    pub title: String,
}

/// Copy the report at `path` into `<state_dir>/reports/<kind>/<name>.md`.
/// `roots`: the directories it must be under (`None`: anywhere, the
/// operator's own ask).
pub fn keep(
    state_dir: &Path,
    kind: &str,
    name: &str,
    path: &Path,
    roots: Option<&[PathBuf]>,
) -> Result<ReportRef, String> {
    let real = path
        .canonicalize()
        .map_err(|e| format!("report {}: {e}", path.display()))?;
    if let Some(roots) = roots {
        let inside = roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|root| real.starts_with(&root))
        });
        if !inside {
            return Err(format!(
                "report {} is outside the directories this session works in",
                path.display()
            ));
        }
    }
    let meta = std::fs::metadata(&real).map_err(|e| format!("report {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("report {} is not a regular file", path.display()));
    }
    if meta.len() > MAX_REPORT_BYTES {
        return Err(format!(
            "report {} is {} bytes; at most {MAX_REPORT_BYTES} are kept",
            path.display(),
            meta.len()
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&real)
        .and_then(|f| f.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("report {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(format!(
            "report {} grew past {MAX_REPORT_BYTES} bytes",
            path.display()
        ));
    }
    let dir = state_dir.join(REPORTS_DIR).join(kind);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stored = dir.join(format!("{name}.md"));
    write_private(&stored, &bytes).map_err(|e| format!("{}: {e}", stored.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let title = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
        .chars()
        .take(TITLE_CHARS)
        .collect();
    Ok(ReportRef {
        stored,
        from: real,
        bytes: bytes.len() as u64,
        sha256: hex(&Sha256::digest(&bytes)),
        title,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// The directories an agent session may name reports under: its cwd,
/// its worktree, and its workspace's folders with their `.worktrees`
/// siblings.
pub fn roots_of(cwd: &Path, worktree: Option<&Path>, folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![cwd.to_path_buf()];
    roots.extend(worktree.map(Path::to_path_buf));
    for folder in folders {
        roots.push(folder.clone());
        if let (Some(parent), Some(name)) = (folder.parent(), folder.file_name()) {
            let mut sibling = name.to_os_string();
            sibling.push(".worktrees");
            roots.push(parent.join(sibling));
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_inside_the_roots_is_copied_and_one_outside_refused() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("repo");
        std::fs::create_dir_all(&work).unwrap();
        let state = dir.path().join("state");
        let report = work.join("REVIEW.md");
        std::fs::write(&report, "\n# Review of abc\n\nfine\n").unwrap();
        let roots = roots_of(&work, None, &[]);
        let kept = keep(&state, "reviews", "r1", &report, Some(&roots)).unwrap();
        assert_eq!(kept.title, "Review of abc");
        assert_eq!(
            std::fs::read_to_string(&kept.stored).unwrap(),
            "\n# Review of abc\n\nfine\n"
        );
        std::fs::remove_file(&report).unwrap();
        assert!(kept.stored.exists(), "the copy outlives the original");

        let secret = dir.path().join("secret");
        std::fs::write(&secret, "key").unwrap();
        assert!(keep(&state, "reviews", "r2", &secret, Some(&roots)).is_err());
        #[cfg(unix)]
        {
            let link = work.join("link.md");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            let refused = keep(&state, "reviews", "r3", &link, Some(&roots)).unwrap_err();
            assert!(refused.contains("outside"), "{refused}");
        }
        assert!(
            keep(&state, "reviews", "r4", &work, Some(&roots)).is_err(),
            "a dir"
        );
        let big = work.join("big.md");
        std::fs::write(&big, vec![b'x'; MAX_REPORT_BYTES as usize + 1]).unwrap();
        assert!(keep(&state, "reviews", "r5", &big, Some(&roots)).is_err());
        // The operator may name any file.
        assert!(keep(&state, "reviews", "r6", &secret, None).is_ok());
    }

    #[test]
    fn a_folders_worktrees_are_roots_too() {
        let roots = roots_of(Path::new("/w/a"), None, &[PathBuf::from("/code/repo")]);
        assert!(roots.contains(&PathBuf::from("/code/repo.worktrees")));
    }
}
