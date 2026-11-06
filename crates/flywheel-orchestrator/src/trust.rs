//! Repo-file trust: `$FLYWHEEL_HOME/trust.json`.
//!
//! A committed `.flywheel.toml` can name a command (`verify`) flywheel will
//! run, so it is honored only after the operator approves those exact
//! bytes. An approval is keyed by (LOCAL project id, sha256 of the file):
//! editing the file changes the hash and so revokes trust, and the repo file
//! cannot choose its own key (its `project_id` only picks the shared project
//! dir). `fw trust` records an approval, `fw trust --revoke` drops every
//! approval of a project.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::home::Home;

/// The trust.json format version.
pub const TRUST_VERSION: u32 = 1;

/// trust.json could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", path.display())]
pub struct TrustError {
    pub path: PathBuf,
    pub message: String,
}

/// One approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The local project id (from the repo's `.git/config`).
    pub project_id: String,
    /// sha256 of the approved file's bytes, lowercase hex.
    pub sha256: String,
    /// The file approved (informational).
    pub file: PathBuf,
    pub approved_at: String,
}

/// The whole of trust.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustFile {
    pub version: u32,
    #[serde(default)]
    pub approvals: Vec<Approval>,
}

impl Default for TrustFile {
    fn default() -> Self {
        Self {
            version: TRUST_VERSION,
            approvals: Vec::new(),
        }
    }
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Read trust.json (empty when absent).
pub fn read(home: &Home) -> Result<TrustFile, TrustError> {
    let path = home.trust_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustFile::default());
        }
        Err(err) => {
            return Err(TrustError {
                path,
                message: err.to_string(),
            });
        }
    };
    serde_json::from_slice(&bytes).map_err(|err| TrustError {
        path,
        message: format!("invalid trust.json: {err}"),
    })
}

/// Write trust.json atomically (temp file + rename), creating the home.
pub fn write(home: &Home, file: &TrustFile) -> Result<(), TrustError> {
    let path = home.trust_file();
    let fail = |path: &Path, err: &dyn std::fmt::Display| TrustError {
        path: path.to_path_buf(),
        message: err.to_string(),
    };
    fs::create_dir_all(home.root()).map_err(|e| fail(home.root(), &e))?;
    let tmp = home
        .root()
        .join(format!(".trust.json.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut text = serde_json::to_string_pretty(file).map_err(|e| fail(&path, &e))?;
    text.push('\n');
    fs::write(&tmp, text).map_err(|e| fail(&tmp, &e))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        fail(&path, &e)
    })
}

/// Whether (`project_id`, `sha256`) is approved.
pub fn is_trusted(home: &Home, project_id: &str, sha256: &str) -> Result<bool, TrustError> {
    Ok(read(home)?
        .approvals
        .iter()
        .any(|a| a.project_id == project_id && a.sha256 == sha256))
}

/// Record approval of `file` (whose bytes hash to `sha256`) for
/// `project_id`. Idempotent: re-approving the same bytes refreshes the
/// record.
pub fn approve(
    home: &Home,
    project_id: &str,
    sha256: &str,
    file: &Path,
) -> Result<Approval, TrustError> {
    let mut trust = read(home)?;
    trust
        .approvals
        .retain(|a| !(a.project_id == project_id && a.sha256 == sha256));
    let approval = Approval {
        project_id: project_id.to_owned(),
        sha256: sha256.to_owned(),
        file: file.to_path_buf(),
        approved_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    trust.approvals.push(approval.clone());
    trust.version = TRUST_VERSION;
    write(home, &trust)?;
    Ok(approval)
}

/// Drop every approval of `project_id`; returns how many were removed.
pub fn revoke(home: &Home, project_id: &str) -> Result<usize, TrustError> {
    let mut trust = read(home)?;
    let before = trust.approvals.len();
    trust.approvals.retain(|a| a.project_id != project_id);
    let removed = before - trust.approvals.len();
    if removed > 0 {
        write(home, &trust)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn approve_check_and_revoke() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let a = sha256_hex(b"verify = \"make\"\n");
        let b = sha256_hex(b"verify = \"make check\"\n");
        assert_ne!(a, b);
        assert!(!is_trusted(&home, "p1", &a).unwrap());
        assert!(!home.trust_file().exists());

        approve(&home, "p1", &a, Path::new("/r/.flywheel.toml")).unwrap();
        approve(&home, "p1", &a, Path::new("/r/.flywheel.toml")).unwrap();
        assert!(is_trusted(&home, "p1", &a).unwrap());
        // The key is (project, hash): another hash or project is not trusted.
        assert!(!is_trusted(&home, "p1", &b).unwrap());
        assert!(!is_trusted(&home, "p2", &a).unwrap());
        assert_eq!(read(&home).unwrap().approvals.len(), 1);

        approve(&home, "p2", &a, Path::new("/s/.flywheel.toml")).unwrap();
        assert_eq!(revoke(&home, "p1").unwrap(), 1);
        assert!(!is_trusted(&home, "p1", &a).unwrap());
        assert!(is_trusted(&home, "p2", &a).unwrap());
        assert_eq!(revoke(&home, "p1").unwrap(), 0);
    }

    #[test]
    fn a_corrupt_trust_file_is_an_error_not_trust() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path());
        fs::write(home.trust_file(), "{not json").unwrap();
        assert!(is_trusted(&home, "p", "x").is_err());
    }
}
