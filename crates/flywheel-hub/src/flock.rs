//! Exclusive file locks (`flock(2)`) that never wait silently or forever.
//!
//! - Re-entrant asks fail at once: a process whose ancestor holds the lock
//!   (found through [`HELD_ENV`], which a holder exports to what it runs,
//!   or through `/proc/locks`) gets "already held by your ancestor", since
//!   waiting would wait on itself.
//! - Every wait is bounded; running out names the lock and its holder.
//! - A holder writes `PID LABEL` into the lock file, so a waiter can say
//!   who holds it even when the process that took the lock has handed its
//!   descriptor to children and exited (`flock(1)` in a shell script).

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Locks held by this process's ancestors, one `PID PATH` per line: set by
/// a holder for the processes it starts (`scripts/check.sh` sets it too).
pub const HELD_ENV: &str = "FLYWHEEL_FLOCKS_HELD";

/// How often a waiter tries again.
const POLL: Duration = Duration::from_millis(200);

/// A held lock; released on drop.
#[derive(Debug)]
pub struct Held {
    file: File,
    path: PathBuf,
}

impl Held {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// [`HELD_ENV`]'s value for a process this one starts: the locks its
    /// ancestors hold, and this one.
    pub fn child_env(&self) -> String {
        let mine = format!("{} {}", std::process::id(), self.path.display());
        match std::env::var(HELD_ENV) {
            Ok(up) if !up.trim().is_empty() => format!("{}\n{mine}", up.trim_end()),
            _ => mine,
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: flock on a descriptor this value owns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Take `path`'s exclusive lock, waiting at most `wait`; `label` says what
/// holds it (written into the file once taken). `waiting` hears who holds
/// it once, when the wait starts.
pub fn hold(
    path: &Path,
    wait: Duration,
    label: &str,
    waiting: &mut dyn FnMut(&str),
) -> Result<Held, String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(ancestor) = held_by_ancestor(path) {
        return Err(format!(
            "the lock {} is already held by your ancestor {}: this process runs inside that \
             hold, so waiting for it would wait on itself (nested scripts/check.sh runs: \
             FLYWHEEL_CHECK_LOCK=0)",
            path.display(),
            crate::procs::describe(ancestor)
        ));
    }
    wait_for(&file, path, wait, waiting)?;
    note_holder(&file, label);
    Ok(Held {
        file,
        path: path.to_path_buf(),
    })
}

/// Take `file`'s (`path`'s) exclusive lock, waiting at most `wait`;
/// running out names the holder. For a lock held only briefly (no note,
/// no re-entrancy check); [`hold`] for the rest.
pub fn wait_for(
    file: &File,
    path: &Path,
    wait: Duration,
    waiting: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    let mut told = false;
    loop {
        // SAFETY: flock on a descriptor the caller owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(format!("flock {}: {e}", path.display()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "waited {} for the lock {}, held by {}",
                span(wait),
                path.display(),
                holder(path)
            ));
        }
        if !told {
            waiting(&holder(path));
            told = true;
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Write `PID LABEL` into the held lock's file (best effort).
fn note_holder(file: &File, label: &str) {
    let mut file = file;
    let _ = file.set_len(0);
    let _ = std::io::Seek::rewind(&mut file);
    let _ = writeln!(file, "{} {label}", std::process::id());
}

/// Who holds `path`'s lock, in words: a live taker `/proc/locks` names,
/// else the topmost live process with the locked file open (one that
/// inherited the descriptor from a taker that exited), else the one the
/// file names, else that it is not known.
pub fn holder(path: &Path) -> String {
    if let Some(pid) = lock_pids(path)
        .into_iter()
        .find(|p| crate::procs::command(*p, 1).is_some())
    {
        return crate::procs::describe(pid);
    }
    if let Some(pid) = topmost_with_lock(path) {
        return crate::procs::describe(pid);
    }
    let note = std::fs::read_to_string(path).unwrap_or_default();
    let mut words = note.lines().next().unwrap_or("").splitn(2, ' ');
    match words.next().and_then(|p| p.parse::<u32>().ok()) {
        Some(pid) => {
            let label = words.next().unwrap_or("").trim();
            format!("pid {pid} ({label}), as its file says")
        }
        None => "an unknown process".to_owned(),
    }
}

/// The live processes holding `path`'s lock through an open descriptor
/// (Linux `fdinfo`), less those whose parent holds it too: the processes
/// that took or first inherited it.
fn topmost_with_lock(path: &Path) -> Option<u32> {
    let holders: Vec<u32> = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| crate::doctor::running::hub_holds_lock(*pid, path) == Some(true))
        .collect();
    holders
        .iter()
        .copied()
        .find(|pid| crate::procs::parent(*pid).is_none_or(|up| !holders.contains(&up)))
}

/// The ancestor of this process that holds `path`'s lock, if one does:
/// named by [`HELD_ENV`], or holding it through an open descriptor (as a
/// shell that ran `flock` on its descriptor does).
pub fn held_by_ancestor(path: &Path) -> Option<u32> {
    let me = std::process::id();
    let ancestors = crate::procs::ancestors(me);
    let marked = std::env::var(HELD_ENV).unwrap_or_default();
    let named = marked
        .lines()
        .filter_map(|line| {
            let (pid, held) = line.trim().split_once(' ')?;
            Some((pid.parse::<u32>().ok()?, PathBuf::from(held)))
        })
        .filter(|(_, held)| same_file(held, path))
        .map(|(pid, _)| pid)
        .find(|pid| ancestors.contains(pid));
    named.or_else(|| {
        ancestors
            .iter()
            .copied()
            .find(|up| crate::doctor::running::hub_holds_lock(*up, path) == Some(true))
    })
}

/// Whether `a` and `b` are one file (device and inode).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The pids `/proc/locks` lists as holding a `flock` on `path` (Linux).
pub fn lock_pids(path: &Path) -> Vec<u32> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let dev = meta.dev();
    let want = format!(
        "{:02x}:{:02x}:{}",
        libc::major(dev),
        libc::minor(dev),
        meta.ino()
    );
    let table = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    locks_on(&table, &want)
}

/// The holders in `/proc/locks` text `table` of a `flock` on the file
/// `want` (`MAJOR:MINOR:INODE`, as the table writes it); waiters (`->`)
/// are not holders.
fn locks_on(table: &str, want: &str) -> Vec<u32> {
    table
        .lines()
        .filter(|l| !l.contains("->"))
        .filter_map(|l| {
            let fields: Vec<&str> = l.split_whitespace().collect();
            let at = fields.iter().position(|f| *f == "FLOCK")?;
            let pid = fields.get(at + 3)?.parse().ok()?;
            (*fields.get(at + 4)? == want).then_some(pid)
        })
        .collect()
}

/// `90s`, `5m`, `1h30m`.
pub fn span(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs % 3600 / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn proc_locks_lines_name_their_holders_not_their_waiters() {
        let table = "1: FLOCK  ADVISORY  WRITE 120208 08:20:1234 0 EOF\n\
                     1: -> FLOCK  ADVISORY  WRITE 999 08:20:1234 0 EOF\n\
                     2: POSIX  ADVISORY  WRITE 77 08:20:1234 0 EOF\n\
                     3: FLOCK  ADVISORY  WRITE 55 08:20:99 0 EOF\n";
        assert_eq!(locks_on(table, "08:20:1234"), vec![120_208]);
    }

    #[test]
    fn a_held_lock_is_waited_for_only_as_long_as_asked_and_its_holder_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/check.lock");
        let held = hold(
            &path,
            Duration::from_secs(1),
            "scripts/check.sh",
            &mut |_| {},
        )
        .unwrap();
        // Another open of it (another process's, as flock sees it).
        let started = Instant::now();
        let mut heard = String::new();
        let e = hold(&path, Duration::from_secs(1), "second", &mut |h| {
            heard = h.to_owned();
        })
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        let me = std::process::id();
        assert!(
            e.contains("waited 1s for the lock") && e.contains(&format!("pid {me}")),
            "{e}"
        );
        assert!(heard.contains(&format!("pid {me}")), "{heard}");
        drop(held);
        hold(&path, Duration::from_secs(1), "third", &mut |_| {}).unwrap();
    }

    #[test]
    fn a_child_asking_for_its_ancestors_lock_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check.lock");
        let held = hold(&path, Duration::from_secs(1), "check.sh", &mut |_| {}).unwrap();
        // A child of this process (the holder) asks for it with an hour
        // to wait: it must say so at once. The test binary runs itself
        // as that child, through the ignored test below.
        let exe = std::env::current_exe().unwrap();
        for marked in [true, false] {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args([
                "--ignored",
                "--exact",
                "flock::tests::child_takes_the_lock",
                "--nocapture",
            ])
            .env("FLYWHEEL_TEST_LOCK", &path)
            .env_remove(HELD_ENV);
            if marked {
                cmd.env(HELD_ENV, held.child_env());
            }
            let started = Instant::now();
            let out = cmd.output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the child waited"
            );
            assert!(
                text.contains("CHILD: the lock") && text.contains("already held by your ancestor"),
                "marked {marked}: {text}"
            );
        }
    }

    #[test]
    #[ignore = "run by a_child_asking_for_its_ancestors_lock_fails_at_once"]
    fn child_takes_the_lock() {
        let Some(path) = std::env::var_os("FLYWHEEL_TEST_LOCK") else {
            return;
        };
        match hold(
            Path::new(&path),
            Duration::from_secs(3600),
            "child",
            &mut |_| {},
        ) {
            Ok(_) => println!("CHILD: took it"),
            Err(e) => println!("CHILD: {e}"),
        }
    }
}
