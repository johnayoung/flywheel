//! How full a filesystem is (`statvfs`), and what a build dir holds: its
//! size on disk and when anything under it last changed.

use serde::Serialize;
use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One filesystem's use, as `df` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskUse {
    /// A folder on it (the first one looked at).
    pub path: PathBuf,
    /// Its mount point, when this process can see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<PathBuf>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    /// Free to an unprivileged user.
    pub free_bytes: u64,
    /// `used / (used + free)`, as `df` computes it.
    pub used_pct: f64,
    #[serde(skip)]
    pub dev: u64,
}

/// The device `path` lives on.
pub fn device(path: &Path) -> io::Result<u64> {
    std::fs::metadata(path).map(|m| m.dev())
}

/// The filesystem holding `path`, now.
pub fn disk_use(path: &Path) -> io::Result<DiskUse> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: a zeroed statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a pointer to a statvfs.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::useless_conversion)]
    let (frsize, blocks, bfree, bavail) = (
        u64::from(st.f_frsize),
        u64::from(st.f_blocks),
        u64::from(st.f_bfree),
        u64::from(st.f_bavail),
    );
    let total_bytes = blocks.saturating_mul(frsize);
    let used_bytes = blocks.saturating_sub(bfree).saturating_mul(frsize);
    let free_bytes = bavail.saturating_mul(frsize);
    Ok(DiskUse {
        path: path.to_owned(),
        mount: mount_point(path),
        total_bytes,
        used_bytes,
        free_bytes,
        used_pct: pct(used_bytes, free_bytes),
        dev: device(path)?,
    })
}

/// `used` as a percentage of `used + free`.
pub fn pct(used: u64, free: u64) -> f64 {
    let all = used.saturating_add(free);
    if all == 0 {
        0.0
    } else {
        used as f64 * 100.0 / all as f64
    }
}

/// The longest mount point holding `path`, from this process's table.
fn mount_point(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|p| PathBuf::from(p.replace("\\040", " ")))
        .filter(|p| path.starts_with(p))
        .max_by_key(|p| p.as_os_str().len())
}

/// What a build dir holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    /// On disk (blocks; each hard-linked file once).
    pub bytes: u64,
    /// The newest modification time of anything under it, itself too.
    pub newest: SystemTime,
    /// `false`: the walk stopped at something newer than its cutoff, so
    /// `bytes` is a lower bound.
    pub complete: bool,
}

/// Walk `dir` (never following symlinks), stopping at the first entry
/// modified after `stop_after`. Anything unreadable is an error: what it
/// holds, and how new, is unknown.
pub fn measure(dir: &Path, stop_after: SystemTime) -> Result<Measured, String> {
    let mut bytes = 0u64;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let meta = std::fs::symlink_metadata(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        let modified = meta
            .modified()
            .map_err(|e| format!("{}: {e}", at.display()))?;
        newest = newest.max(modified);
        if modified > stop_after {
            return Ok(Measured {
                bytes,
                newest,
                complete: false,
            });
        }
        if meta.nlink() < 2 || meta.is_dir() || linked.insert((meta.dev(), meta.ino())) {
            bytes = bytes.saturating_add(meta.blocks().saturating_mul(512));
        }
        if meta.is_dir() {
            let entries = std::fs::read_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", at.display()))?;
                stack.push(entry.path());
            }
        }
    }
    Ok(Measured {
        bytes,
        newest,
        complete: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_walk_counts_hard_links_once_and_finds_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("target");
        std::fs::create_dir_all(t.join("debug/deps")).unwrap();
        std::fs::write(t.join("debug/deps/bin-1"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::hard_link(t.join("debug/deps/bin-1"), t.join("debug/bin")).unwrap();
        let far = SystemTime::now() + Duration::from_secs(3600);
        let m = measure(&t, far).unwrap();
        assert!(m.complete);
        assert!(
            m.bytes >= 64 * 1024 && m.bytes < 2 * 64 * 1024,
            "{}",
            m.bytes
        );
        assert!(m.newest > SystemTime::now() - Duration::from_secs(60));
        let early = measure(&t, SystemTime::now() - Duration::from_secs(3600)).unwrap();
        assert!(!early.complete);
    }

    #[test]
    fn disk_use_reads_like_df() {
        let d = disk_use(Path::new("/")).unwrap();
        assert!(d.total_bytes > 0 && d.used_pct >= 0.0 && d.used_pct <= 100.0);
        assert!(d.mount.is_some());
    }
}
