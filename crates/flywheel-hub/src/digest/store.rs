//! The digest marker, `<state-dir>/digest.json` (0600): which slot was
//! last sent, what span the last digest covered, a send under way, and the
//! recent digests (the in-app card and `#digest=` links read them).
//!
//! Every read-modify-write holds `digest.lock` (an exclusive `flock`), so
//! two hub processes on one state dir -- the old and the new one of a live
//! handoff, or a restart racing a slot -- decide each slot once: whoever
//! claims it first writes the marker, the other finds it taken.
//!
//! A claim is written before anything is delivered (`pending`, with the
//! claiming process's pid): a hub that dies mid-send leaves it, and the
//! next hub delivers it again if that process is gone and the claim is
//! within [`super::GRACE`]; a live claimer's send is not repeated. A claim
//! is dead when its pid is gone or is the claiming hub's own pid under
//! another owner (a restart that reused it), and abandoned (dropped, not
//! resent) past [`super::CLAIM_TTL`] whatever its pid.

use super::Report;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A send claimed and not yet finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    /// The scheduled slot (`None`: sent by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub pid: u32,
    /// Which digest service in that process (tests run several in one).
    pub owner: String,
    pub claimed_at: DateTime<Utc>,
}

/// The file's contents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// The schedule the slots below belong to (`08:00,18:00@Europe/Berlin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// When that schedule took effect: slots before it are not owed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_at: Option<DateTime<Utc>>,
    /// The latest slot decided (sent, found empty, or skipped as stale).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_slot: Option<DateTime<Utc>>,
    /// The end of the last digest's span: the next one starts there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Newest last, at most [`super::KEEP_RECENT`].
    #[serde(default)]
    pub recent: Vec<Report>,
}

/// The marker file and its lock.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    lock: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(super::STATE_FILE),
            lock: state_dir.join(super::LOCK_FILE),
        }
    }

    /// The marker as on disk (a missing or unreadable file: empty).
    pub fn read(&self) -> Marker {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Run `change` on the marker under the lock; it is written back when
    /// `change` says so (`true` in the pair).
    pub fn update<T>(&self, change: impl FnOnce(&mut Marker) -> (T, bool)) -> Result<T, String> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .map_err(|e| format!("{}: {e}", self.lock.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.lock, std::fs::Permissions::from_mode(0o600));
        }
        lock.lock()
            .map_err(|e| format!("{}: {e}", self.lock.display()))?;
        let mut marker = self.read();
        let (out, write) = change(&mut marker);
        if write {
            let mut bytes = serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            crate::util::write_atomic(&self.path, &bytes, crate::util::FileMode::Private)
                .map_err(|e| format!("{}: {e}", self.path.display()))?;
        }
        drop(lock);
        Ok(out)
    }
}

/// Whether process `pid` is running (a zombie counts until reaped).
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 only checks the process exists and may be
        // signalled; nothing is sent.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
