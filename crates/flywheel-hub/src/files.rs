//! Files a session's output names, read for the web viewer: a path as an
//! agent or a command prints it (relative to the session's directory or to
//! one of its workspace's folders, absolute, or `~/`), resolved to a file
//! under the session's own roots. Symlinks resolve first, so a link out of
//! the roots is refused like any other path outside them: the route reads
//! the session's work, not the disk.

use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest prefix of a file the viewer is given.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Most paths one check resolves.
pub const MAX_CHECK_PATHS: usize = 64;
/// Longest path accepted.
const MAX_PATH_BYTES: usize = 4096;
/// How much of a file is looked at for a NUL (a binary file).
const SNIFF_BYTES: usize = 8192;

/// Where a session's paths may resolve: the directories looked in for a
/// relative path, in order, and the roots a resolved file must be under.
#[derive(Debug, Clone)]
pub struct Scope {
    /// Where a relative path is looked for, in order: the session's
    /// directory now (a shell that `cd`ed), then the roots.
    pub bases: Vec<PathBuf>,
    /// What a file must be under: the directory the session was opened
    /// in, its worktree, its workspace's folders.
    pub roots: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

/// A path resolved to a readable file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    /// Canonical absolute path.
    pub path: PathBuf,
    pub size: u64,
}

/// A file's text for the viewer.
#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub path: PathBuf,
    pub size: u64,
    pub text: String,
    /// Only the first [`MAX_FILE_BYTES`] are in `text`.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FileError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}: no such file in this session's folders")]
    NotFound(String),
    #[error("{0} is outside this session's folders")]
    Outside(String),
    #[error("{0} is a directory")]
    Directory(String),
    #[error("{0} is a binary file")]
    Binary(String),
    #[error("{0}: {1}")]
    Io(String, String),
}

impl Scope {
    /// The canonical roots a file must be under (missing ones dropped).
    fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .filter_map(|b| std::fs::canonicalize(b).ok())
            .collect()
    }

    /// Resolve `requested` to an existing path under the roots (a
    /// directory too: the caller decides).
    fn resolve(&self, requested: &str) -> Result<(PathBuf, std::fs::Metadata), FileError> {
        let raw = requested.trim();
        if raw.is_empty() || raw.len() > MAX_PATH_BYTES || raw.contains('\0') {
            return Err(FileError::Invalid("a path is required".to_owned()));
        }
        let given = Path::new(raw);
        let candidates: Vec<PathBuf> = if raw == "~" || raw.starts_with("~/") {
            match &self.home {
                Some(home) => vec![crate::state::expand_home(given, Some(home))],
                None => Vec::new(),
            }
        } else if given.is_absolute() {
            vec![given.to_path_buf()]
        } else {
            self.bases.iter().map(|b| b.join(given)).collect()
        };
        let roots = self.roots();
        let mut outside = false;
        for candidate in candidates {
            let Ok(path) = std::fs::canonicalize(&candidate) else {
                continue;
            };
            if !roots.iter().any(|r| path.starts_with(r)) {
                outside = true;
                continue;
            }
            let meta = std::fs::metadata(&path)
                .map_err(|e| FileError::Io(raw.to_owned(), e.to_string()))?;
            return Ok((path, meta));
        }
        Err(if outside {
            FileError::Outside(raw.to_owned())
        } else {
            FileError::NotFound(raw.to_owned())
        })
    }

    /// `requested` as a file under the roots.
    pub fn find(&self, requested: &str) -> Result<Found, FileError> {
        let (path, meta) = self.resolve(requested)?;
        if meta.is_dir() {
            return Err(FileError::Directory(requested.trim().to_owned()));
        }
        Ok(Found {
            path,
            size: meta.len(),
        })
    }

    /// Read `requested`: its first [`MAX_FILE_BYTES`] as text (invalid
    /// UTF-8 replaced); a file with a NUL near its start is binary.
    pub fn read(&self, requested: &str) -> Result<FileView, FileError> {
        let found = self.find(requested)?;
        let io = |e: std::io::Error| FileError::Io(requested.trim().to_owned(), e.to_string());
        let file = std::fs::File::open(&found.path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
            return Err(FileError::Binary(requested.trim().to_owned()));
        }
        let truncated = found.size > bytes.len() as u64;
        Ok(FileView {
            path: found.path,
            size: found.size,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Scope) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let other = root.join("other");
        std::fs::create_dir_all(repo.join("src/sub")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(repo.join("README.md"), "# hi\n").unwrap();
        std::fs::write(other.join("notes.txt"), "other\n").unwrap();
        std::fs::write(root.join("secret.txt"), "no\n").unwrap();
        std::fs::write(root.join("home/dot.txt"), "home\n").unwrap();
        let roots = vec![repo.clone(), other];
        let scope = Scope {
            bases: [vec![repo.join("src/sub")], roots.clone()].concat(),
            roots,
            home: Some(root.join("home")),
        };
        (dir, scope)
    }

    #[test]
    fn relative_paths_resolve_against_the_cwd_then_the_folders() {
        let (dir, scope) = setup();
        let root = dir.path().canonicalize().unwrap();
        let found = scope.find("src/main.rs").unwrap();
        assert_eq!(found.path, root.join("repo/src/main.rs"));
        assert_eq!(found.size, 13);
        assert_eq!(
            scope.find("../main.rs").unwrap().path,
            root.join("repo/src/main.rs")
        );
        assert_eq!(
            scope.find("notes.txt").unwrap().path,
            root.join("other/notes.txt")
        );
        let absolute = root.join("repo/README.md");
        assert_eq!(
            scope.find(absolute.to_str().unwrap()).unwrap().path,
            absolute
        );
        assert_eq!(
            scope.find("nope.rs"),
            Err(FileError::NotFound("nope.rs".to_owned()))
        );
        assert_eq!(
            scope.find("src"),
            Err(FileError::Directory("src".to_owned()))
        );
        assert!(matches!(scope.find(""), Err(FileError::Invalid(_))));
    }

    #[test]
    fn nothing_outside_the_roots_is_read() {
        let (dir, scope) = setup();
        let root = dir.path().canonicalize().unwrap();
        let secret = root.join("secret.txt");
        assert!(matches!(
            scope.read(secret.to_str().unwrap()),
            Err(FileError::Outside(_))
        ));
        assert!(matches!(
            scope.read("../../../secret.txt"),
            Err(FileError::Outside(_))
        ));
        // `~/` is the home directory, outside the roots here.
        assert!(matches!(
            scope.read("~/dot.txt"),
            Err(FileError::Outside(_))
        ));
        // A shell that `cd`ed out of the roots finds paths there, but
        // reads nothing outside them.
        let wandered = Scope {
            bases: vec![root.clone()],
            ..scope.clone()
        };
        assert!(matches!(
            wandered.read("secret.txt"),
            Err(FileError::Outside(_))
        ));
        assert!(wandered.read("repo/README.md").is_ok());
        // A symlink out of the roots is refused once resolved.
        std::os::unix::fs::symlink(&secret, root.join("repo/link.txt")).unwrap();
        assert!(matches!(scope.read("link.txt"), Err(FileError::Outside(_))));
    }

    #[test]
    fn text_is_read_up_to_the_cap_and_binary_is_refused() {
        let (dir, scope) = setup();
        let repo = dir.path().join("repo");
        let view = scope.read("README.md").unwrap();
        assert_eq!((view.text.as_str(), view.truncated), ("# hi\n", false));
        std::fs::write(
            repo.join("big.txt"),
            vec![b'a'; MAX_FILE_BYTES as usize + 10],
        )
        .unwrap();
        let big = scope.read("big.txt").unwrap();
        assert!(big.truncated);
        assert_eq!(big.text.len() as u64, MAX_FILE_BYTES);
        assert_eq!(big.size, MAX_FILE_BYTES + 10);
        std::fs::write(repo.join("bin.dat"), b"\x7fELF\0\0\x01").unwrap();
        assert_eq!(
            scope.read("bin.dat").unwrap_err(),
            FileError::Binary("bin.dat".to_owned())
        );
    }
}
