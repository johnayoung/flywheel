//! Who is using a directory, read from `/proc`: each process's working
//! directory, root, executable, open files, and mapped files; for a
//! process the kernel will not let this user inspect (another user's, or
//! a non-dumpable one of its own), what stays world-readable: its
//! command line's absolute paths. Every mount table any process sees is
//! read too, so a bind mount of a directory (a container's worktree) or
//! a mount inside one counts as a use.
//!
//! Anything that cannot be read and is not one of those two expected
//! cases (a process gone meanwhile, inspection denied) makes the whole
//! scan uncertain, and an uncertain scan answers no question.

use serde::Serialize;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// How a process uses a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    Cwd,
    Root,
    Exe,
    File,
    Mapped,
    /// An absolute path on the command line of a process that cannot be
    /// inspected.
    Arg,
    /// A mount whose source, or mount point, is there.
    Mount,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cwd => "cwd",
            Self::Root => "root",
            Self::Exe => "exe",
            Self::File => "open file",
            Self::Mapped => "mapped file",
            Self::Arg => "command-line path",
            Self::Mount => "mount",
        }
    }
}

/// One process's use of a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Use {
    pub pid: u32,
    pub comm: String,
    pub how: How,
    pub path: PathBuf,
}

impl Use {
    pub fn text(&self) -> String {
        format!(
            "pid {} ({}) {} {}",
            self.pid,
            self.comm,
            self.how.as_str(),
            self.path.display()
        )
    }
}

#[derive(Debug, Default)]
struct Seen {
    pid: u32,
    comm: String,
    paths: Vec<(How, PathBuf)>,
}

/// One look at every process.
#[derive(Debug, Default)]
pub struct Scan {
    seen: Vec<Seen>,
    /// Processes checked only by their command line and mounts.
    opaque: usize,
    /// Why nothing may be concluded from this scan.
    uncertain: Option<String>,
}

enum Fail {
    /// The process (or the fd) is gone.
    Gone,
    Denied,
    Other(String),
}

fn fail(e: &io::Error, what: &Path) -> Fail {
    match e.kind() {
        io::ErrorKind::NotFound => Fail::Gone,
        io::ErrorKind::PermissionDenied => Fail::Denied,
        _ if e.raw_os_error() == Some(libc::ESRCH) => Fail::Gone,
        _ => Fail::Other(format!("{}: {e}", what.display())),
    }
}

/// A `/proc` link target as a path: ` (deleted)` dropped; `None` for
/// anything that is not a path (`socket:[..]`, `anon_inode:..`).
fn link_path(target: PathBuf) -> Option<PathBuf> {
    let text = target.to_string_lossy();
    if !text.starts_with('/') {
        return None;
    }
    Some(match text.strip_suffix(" (deleted)") {
        Some(rest) => PathBuf::from(rest),
        None => target,
    })
}

/// `\040`-style escapes of `/proc/*/mountinfo` and `maps` undone.
fn unescape(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let n = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(n);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// One mount: its device, the path inside that device's filesystem it
/// shows, and where it is mounted (in the namespace it was read from).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    dev: String,
    root: PathBuf,
    point: PathBuf,
    fstype: String,
    options: String,
}

fn mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let dash = fields.iter().position(|f| *f == "-")?;
            Some(Mount {
                dev: (*fields.get(2)?).to_owned(),
                root: unescape(fields.get(3)?),
                point: unescape(fields.get(4)?),
                fstype: (*fields.get(dash + 1)?).to_owned(),
                options: (*fields.get(dash + 3).unwrap_or(&"")).to_owned(),
            })
        })
        .collect()
}

/// Where `mount` (read from any namespace) shows from, in this process's
/// own view: the mount here of the same device whose root holds it
/// (longest first), joined with the rest. `None` when its device is not
/// mounted here.
fn source_here(mount: &Mount, own: &[Mount]) -> Option<PathBuf> {
    own.iter()
        .filter(|m| m.dev == mount.dev && mount.root.starts_with(&m.root))
        .max_by_key(|m| m.root.as_os_str().len())
        .map(|m| {
            let rest = mount.root.strip_prefix(&m.root).unwrap_or(Path::new(""));
            if rest.as_os_str().is_empty() {
                m.point.clone()
            } else {
                m.point.join(rest)
            }
        })
}

impl Scan {
    /// Read every process now.
    pub fn take() -> Self {
        Self::take_in(Path::new("/proc"))
    }

    fn take_in(proc: &Path) -> Self {
        let mut scan = Self::default();
        let own_text = match std::fs::read_to_string(proc.join("self/mountinfo")) {
            Ok(text) => text,
            Err(e) => {
                scan.uncertain = Some(format!("{}/self/mountinfo: {e}", proc.display()));
                return scan;
            }
        };
        let own = mounts(&own_text);
        if let Some(m) = own
            .iter()
            .find(|m| m.fstype == "proc" && m.point == proc && m.options.contains("hidepid="))
        {
            scan.uncertain = Some(format!(
                "{} is mounted with {}: other users' processes are hidden",
                proc.display(),
                m.options
            ));
            return scan;
        }
        let entries = match std::fs::read_dir(proc) {
            Ok(entries) => entries,
            Err(e) => {
                scan.uncertain = Some(format!("{}: {e}", proc.display()));
                return scan;
            }
        };
        let mut tables: HashSet<String> = HashSet::new();
        let uncertain = |scan: &mut Self, why: String| {
            if scan.uncertain.is_none() {
                scan.uncertain = Some(why);
            }
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let dir = entry.path();
            match scan_pid(pid, &dir, &own, &mut tables) {
                Ok(Some((seen, opaque))) => {
                    scan.opaque += usize::from(opaque);
                    scan.seen.push(seen);
                }
                Ok(None) => {}
                Err(why) => uncertain(&mut scan, why),
            }
        }
        // This process's own mounts: one inside a directory counts.
        scan.seen.push(Seen {
            pid: std::process::id(),
            comm: "mount".into(),
            paths: own.iter().map(|m| (How::Mount, m.point.clone())).collect(),
        });
        scan
    }

    /// How many processes could be checked only by their command line
    /// and mounts.
    pub fn opaque(&self) -> usize {
        self.opaque
    }

    /// Why this scan answers nothing, if so.
    pub fn uncertain(&self) -> Option<&str> {
        self.uncertain.as_deref()
    }

    /// The first use of `dir` (canonical) or anything under it; `Err`
    /// when the scan is uncertain.
    pub fn user_of(&self, dir: &Path) -> Result<Option<Use>, String> {
        if let Some(why) = &self.uncertain {
            return Err(why.clone());
        }
        for seen in &self.seen {
            if let Some((how, path)) = seen.paths.iter().find(|(_, p)| p.starts_with(dir)) {
                return Ok(Some(Use {
                    pid: seen.pid,
                    comm: seen.comm.clone(),
                    how: *how,
                    path: path.clone(),
                }));
            }
        }
        Ok(None)
    }
}

/// Whether the process at `dir` is gone, a zombie, or dead (`Z`/`X` in
/// its `stat`): it holds no files, no directory, no mounts.
fn exited(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(e) => matches!(fail(&e, dir), Fail::Gone),
    }
}

/// One process: what it uses and whether it was opaque; `None` when it
/// is gone (or is a kernel thread with nothing of its own).
fn scan_pid(
    pid: u32,
    dir: &Path,
    own: &[Mount],
    tables: &mut HashSet<String>,
) -> Result<Option<(Seen, bool)>, String> {
    let comm = match std::fs::read_to_string(dir.join("comm")) {
        Ok(c) => c.trim_end().to_owned(),
        Err(e) => match fail(&e, dir) {
            Fail::Gone => return Ok(None),
            Fail::Denied => String::new(),
            Fail::Other(why) => return Err(why),
        },
    };
    let mut seen = Seen {
        pid,
        comm,
        paths: Vec::new(),
    };
    let mut opaque = false;
    // The mount table this process sees (world-readable).
    match std::fs::read_to_string(dir.join("mountinfo")) {
        Ok(text) => {
            if tables.insert(text.clone()) {
                for mount in mounts(&text) {
                    if let Some(source) = source_here(&mount, own) {
                        seen.paths.push((How::Mount, source));
                    }
                }
            }
        }
        // A zombie (or a process exiting) has no namespaces left: EINVAL.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) && exited(dir) => return Ok(None),
        Err(e) => match fail(&e, &dir.join("mountinfo")) {
            Fail::Gone => return Ok(None),
            Fail::Denied => {
                return Err(format!(
                    "{}: permission denied",
                    dir.join("mountinfo").display()
                ));
            }
            Fail::Other(why) => return Err(why),
        },
    }
    for (name, how) in [("cwd", How::Cwd), ("root", How::Root), ("exe", How::Exe)] {
        let at = dir.join(name);
        match std::fs::read_link(&at) {
            Ok(target) => {
                if let Some(path) = link_path(target) {
                    seen.paths.push((how, path));
                }
            }
            Err(e) => match fail(&e, &at) {
                // A kernel thread has no exe; a zombie has no cwd.
                Fail::Gone if name == "exe" => {}
                Fail::Gone => return Ok(None),
                Fail::Denied => opaque = true,
                Fail::Other(why) => return Err(why),
            },
        }
    }
    if !opaque {
        let fds = dir.join("fd");
        match std::fs::read_dir(&fds) {
            Ok(entries) => {
                for fd in entries {
                    let fd = fd.map_err(|e| format!("{}: {e}", fds.display()))?;
                    match std::fs::read_link(fd.path()) {
                        Ok(target) => {
                            if let Some(path) = link_path(target) {
                                seen.paths.push((How::File, path));
                            }
                        }
                        Err(e) => match fail(&e, &fd.path()) {
                            Fail::Gone => {}
                            Fail::Denied => opaque = true,
                            Fail::Other(why) => return Err(why),
                        },
                    }
                }
            }
            Err(e) => match fail(&e, &fds) {
                Fail::Gone => return Ok(None),
                Fail::Denied => opaque = true,
                Fail::Other(why) => return Err(why),
            },
        }
    }
    if !opaque {
        let maps = dir.join("maps");
        match std::fs::read_to_string(&maps) {
            Ok(text) => {
                for line in text.lines() {
                    // address perms offset dev inode path
                    let path = line.splitn(6, ' ').nth(5).map(str::trim_start);
                    if let Some(path) = path
                        .filter(|p| p.starts_with('/'))
                        .and_then(|p| link_path(unescape(p)))
                    {
                        seen.paths.push((How::Mapped, path));
                    }
                }
            }
            Err(e) => match fail(&e, &maps) {
                Fail::Gone => return Ok(None),
                Fail::Denied => opaque = true,
                Fail::Other(why) => return Err(why),
            },
        }
    }
    if opaque {
        let at = dir.join("cmdline");
        match std::fs::read(&at) {
            Ok(raw) => {
                for arg in raw.split(|b| *b == 0) {
                    let arg = String::from_utf8_lossy(arg);
                    let value = arg.split_once('=').map_or(&*arg, |(_, v)| v);
                    for text in [&*arg, value] {
                        if text.starts_with('/') {
                            seen.paths.push((How::Arg, PathBuf::from(text)));
                        }
                    }
                }
            }
            Err(e) => match fail(&e, &at) {
                Fail::Gone => return Ok(None),
                Fail::Denied => {
                    return Err(format!(
                        "pid {pid} cannot be inspected and its command line is unreadable"
                    ));
                }
                Fail::Other(why) => return Err(why),
            },
        }
    }
    Ok(Some((seen, opaque)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_fields_and_escapes_parse() {
        let text = "36 35 98:0 /mnt1 /mnt/with\\040space rw,noatime master:1 - ext3 /dev/root rw,errors=continue\n";
        let m = &mounts(text)[0];
        assert_eq!(m.dev, "98:0");
        assert_eq!(m.root, PathBuf::from("/mnt1"));
        assert_eq!(m.point, PathBuf::from("/mnt/with space"));
        assert_eq!(m.fstype, "ext3");
    }

    #[test]
    fn a_bind_mount_maps_back_through_the_longest_own_root() {
        let own = vec![
            Mount {
                dev: "8:1".into(),
                root: "/".into(),
                point: "/".into(),
                fstype: "ext4".into(),
                options: String::new(),
            },
            Mount {
                dev: "8:2".into(),
                root: "/@home".into(),
                point: "/home".into(),
                fstype: "btrfs".into(),
                options: String::new(),
            },
        ];
        let bind = Mount {
            dev: "8:2".into(),
            root: "/@home/u/code/x.worktrees/a".into(),
            point: "/workspace".into(),
            fstype: "btrfs".into(),
            options: String::new(),
        };
        assert_eq!(
            source_here(&bind, &own),
            Some(PathBuf::from("/home/u/code/x.worktrees/a"))
        );
        let elsewhere = Mount {
            dev: "9:9".into(),
            ..bind
        };
        assert_eq!(source_here(&elsewhere, &own), None);
    }

    #[test]
    fn a_zombie_leaves_the_scan_certain() {
        let child = std::process::Command::new("true").spawn().unwrap();
        // Exited, not yet waited for: a zombie.
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !exited(&PathBuf::from(format!("/proc/{pid}"))) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let scan = Scan::take();
        assert_eq!(scan.uncertain(), None);
        let mut child = child;
        child.wait().unwrap();
    }

    #[test]
    fn a_process_that_cannot_be_inspected_is_seen_by_its_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inside = root.join("w");
        std::fs::create_dir_all(&inside).unwrap();
        // Non-dumpable: its cwd and fds are closed to this user.
        let Ok(mut child) = std::process::Command::new("python3")
            .args([
                "-c",
                "import ctypes,time; ctypes.CDLL(None).prctl(4,0,0,0,0); time.sleep(30)",
            ])
            .arg(inside.join("data.bin"))
            .spawn()
        else {
            eprintln!("skipped: no python3");
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let found = loop {
            let scan = Scan::take();
            let found = scan.user_of(&inside).unwrap();
            if found.as_ref().is_some_and(|u| u.how == How::Arg)
                || std::time::Instant::now() > deadline
            {
                assert!(scan.opaque() > 0);
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        let found = found.expect("its command line names the directory");
        assert_eq!((found.pid, found.how), (child.id(), How::Arg));
    }

    #[test]
    fn a_process_working_in_a_directory_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inside = root.join("w");
        std::fs::create_dir_all(&inside).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&inside)
            .spawn()
            .unwrap();
        let scan = Scan::take();
        let found = scan.user_of(&inside);
        let other = scan.user_of(&root.join("other"));
        child.kill().unwrap();
        child.wait().unwrap();
        let found = found.unwrap().expect("sleep works in it");
        assert_eq!(found.pid, child.id());
        assert_eq!(found.how, How::Cwd);
        assert_eq!(other.unwrap(), None);
    }
}
