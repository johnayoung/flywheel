//! Run transcript bodies and their ledger rows.
//!
//! A run's transcript body is its per-run JSONL file (written by
//! [`crate::telemetry_file::FileTelemetrySink`]). When the run finalizes,
//! [`finalize_transcript`] records one [`TranscriptRecord`] in the store:
//! the body's `file://` URI, the SHA-256 of its bytes at that moment, their
//! count, the run id, and the time. Rows are append-only and kept forever;
//! the bodies are not: [`sweep_expired`] deletes bodies older than a
//! retention window and [`purge_bodies`] deletes all of them, and neither
//! touches a row.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::store::{LedgerStore, StoreError, TranscriptRecord};

/// The URI scheme prefix every transcript row carries.
pub const FILE_URI_PREFIX: &str = "file://";

/// Seconds in one retention day.
const SECONDS_PER_DAY: u64 = 86_400;

/// Recording a transcript row failed.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("reading transcript {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `file://` + the absolute form of `path` (canonical when it exists;
/// relative paths are anchored at the working directory). Not
/// percent-encoded: the URI names the path byte-for-byte.
pub fn file_uri(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    format!("{FILE_URI_PREFIX}{}", absolute.to_string_lossy())
}

/// The path a `file://` URI names, or `None` for any other scheme.
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix(FILE_URI_PREFIX).map(PathBuf::from)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Record the transcript body at `path` for `run_id` in `store`.
///
/// Reads the file's bytes once, hashes exactly those bytes, and appends a
/// row (`uri`, `sha256`, `bytes`, `run_id`, `created_at = now`). A run that
/// wrote no transcript (the file does not exist) records nothing and
/// returns `Ok(None)`.
pub fn finalize_transcript(
    store: &dyn LedgerStore,
    path: &Path,
    run_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<TranscriptRecord>, TranscriptError> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TranscriptError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let record = TranscriptRecord {
        run_id: run_id.to_owned(),
        uri: file_uri(path),
        sha256: sha256_hex(&body),
        bytes: u64::try_from(body.len()).unwrap_or(u64::MAX),
        created_at: now,
        id: None,
    };
    Ok(Some(store.append_transcript(&record)?))
}

/// Every regular file under `dir` (recursively), or none when `dir` does
/// not exist.
fn body_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            out.extend(body_files(&entry.path())?);
        } else {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Delete every transcript body under `dir` last modified more than
/// `retention_days` days before `now`; `0` keeps every body forever.
/// Returns the deleted paths. Ledger rows are untouched.
pub fn sweep_expired(dir: &Path, retention_days: u64, now: SystemTime) -> io::Result<Vec<PathBuf>> {
    if retention_days == 0 {
        return Ok(Vec::new());
    }
    let window = Duration::from_secs(retention_days.saturating_mul(SECONDS_PER_DAY));
    let Some(cutoff) = now.checked_sub(window) else {
        return Ok(Vec::new());
    };
    let mut deleted = Vec::new();
    for path in body_files(dir)? {
        let modified = match fs::symlink_metadata(&path).and_then(|m| m.modified()) {
            Ok(modified) => modified,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if modified < cutoff {
            remove(&path, &mut deleted)?;
        }
    }
    Ok(deleted)
}

/// Delete every transcript body under `dir` now. Returns the deleted
/// paths. Ledger rows are untouched.
pub fn purge_bodies(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut deleted = Vec::new();
    for path in body_files(dir)? {
        remove(&path, &mut deleted)?;
    }
    Ok(deleted)
}

fn remove(path: &Path, deleted: &mut Vec<PathBuf>) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            deleted.push(path.to_path_buf());
            Ok(())
        }
        // A concurrent sweep got there first.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LedgerStore;
    use crate::store_memory::InMemoryStore;

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn sha_is_lowercase_hex_of_the_bytes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn finalize_records_the_body_bytes_and_a_file_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-1.jsonl");
        fs::write(&path, b"{\"kind\":\"x\"}\n").unwrap();
        let store = InMemoryStore::new();
        let now = Utc::now();
        let row = finalize_transcript(&store, &path, "run-1", now)
            .unwrap()
            .unwrap();
        assert_eq!(row.run_id, "run-1");
        assert_eq!(row.bytes, 13);
        assert_eq!(row.sha256, sha256_hex(b"{\"kind\":\"x\"}\n"));
        assert_eq!(uri_path(&row.uri).unwrap(), path.canonicalize().unwrap());
        assert_eq!(row.created_at, now);
        assert_eq!(store.list_transcripts().unwrap(), vec![row]);
    }

    #[test]
    fn a_run_without_a_body_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        let none = finalize_transcript(&store, &dir.path().join("missing.jsonl"), "r", Utc::now())
            .unwrap();
        assert!(none.is_none());
        assert!(store.list_transcripts().unwrap().is_empty());
    }

    #[test]
    fn the_sweep_deletes_only_bodies_past_the_window_and_zero_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = dir.path().join("old.jsonl");
        let fresh = dir.path().join("fresh.jsonl");
        fs::write(&old, b"old").unwrap();
        fs::write(&fresh, b"fresh").unwrap();
        set_mtime(&old, now - Duration::from_secs(10 * SECONDS_PER_DAY));
        set_mtime(&fresh, now - Duration::from_secs(SECONDS_PER_DAY));

        assert!(sweep_expired(dir.path(), 0, now).unwrap().is_empty());
        assert!(old.exists());

        let deleted = sweep_expired(dir.path(), 5, now).unwrap();
        assert_eq!(deleted, vec![old.clone()]);
        assert!(!old.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn purge_deletes_every_body_and_tolerates_a_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jsonl"), b"a").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/b.jsonl"), b"b").unwrap();
        assert_eq!(purge_bodies(dir.path()).unwrap().len(), 2);
        assert!(body_files(dir.path()).unwrap().is_empty());
        assert!(purge_bodies(&dir.path().join("absent")).unwrap().is_empty());
    }
}
