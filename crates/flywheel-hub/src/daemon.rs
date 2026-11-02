//! Running the hub detached (`fw hub start` / `stop` / `logs` /
//! `install-service`): the detached spawn, the rotating log its output goes
//! to, signalling and waiting on it, and the systemd `--user` unit.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The detached hub's log, inside the state dir.
pub const LOG_FILE: &str = "hub.log";
/// A log file is rotated before it would grow past this.
pub const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated logs kept (`hub.log.1` newest .. `hub.log.N` oldest).
pub const LOG_KEEP: usize = 3;

/// Open `path` for appending, creating it 0600.
fn open_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// A size-bounded log: `path` plus up to `keep` rotated generations
/// (`path.1` newest).
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    keep: usize,
}

impl RotatingLog {
    /// Open (appending to) `path`.
    pub fn open(path: &Path, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        let file = open_append(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            max_bytes: max_bytes.max(1),
            keep,
        })
    }

    /// The `n`th rotated generation of `path`.
    pub fn generation(path: &Path, n: usize) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if self.keep == 0 {
            self.file.set_len(0)?;
        } else {
            for n in (1..self.keep).rev() {
                let from = Self::generation(&self.path, n);
                if from.exists() {
                    std::fs::rename(&from, Self::generation(&self.path, n + 1))?;
                }
            }
            std::fs::rename(&self.path, Self::generation(&self.path, 1))?;
            self.file = open_append(&self.path)?;
        }
        self.size = 0;
        Ok(())
    }

    /// Append `bytes`, rotating first when they would push a non-empty
    /// file past the limit.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(len) > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(bytes)?;
        self.size = self.size.saturating_add(len);
        Ok(())
    }
}

/// This process's stdout and stderr, redirected into a [`RotatingLog`]
/// through a pipe (so everything written to fds 1 and 2 -- including
/// panics -- is captured and rotated). Dropping it flushes, restores
/// nothing (the fds point at `/dev/null` afterwards), and waits briefly
/// for the copier to drain.
#[cfg(unix)]
pub struct LogRedirect {
    done: std::sync::mpsc::Receiver<()>,
}

#[cfg(unix)]
impl LogRedirect {
    pub fn install(path: &Path) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let mut log = RotatingLog::open(path, LOG_MAX_BYTES, LOG_KEEP)?;
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let [read_fd, write_fd] = fds;
        for fd in fds {
            // SAFETY: fcntl on descriptors this function just created.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: dup2 onto the standard descriptors; `write_fd` is open.
        let redirected = unsafe {
            libc::dup2(write_fd, libc::STDOUT_FILENO) >= 0
                && libc::dup2(write_fd, libc::STDERR_FILENO) >= 0
        };
        let error = std::io::Error::last_os_error();
        // SAFETY: `write_fd` is ours and now duplicated (or unused).
        unsafe { libc::close(write_fd) };
        // SAFETY: `read_fd` is an open descriptor this function owns.
        let mut reader = unsafe { File::from_raw_fd(read_fd) };
        if !redirected {
            return Err(error);
        }
        let (tx, done) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("hub-log".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Nowhere left to report a failing log.
                            let _ = log.write(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(());
            })?;
        Ok(Self { done })
    }
}

#[cfg(unix)]
impl Drop for LogRedirect {
    fn drop(&mut self) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
            use std::os::fd::AsRawFd;
            // SAFETY: dup2 of an open descriptor onto the standard ones;
            // this closes the pipe's last write ends so the copier sees EOF.
            unsafe {
                libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
                libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO);
            }
        }
        let _ = self.done.recv_timeout(Duration::from_secs(2));
    }
}

/// Spawn `program args` detached from this terminal: a new session, stdin
/// from `/dev/null`, stdout and stderr appended to `log` (until the child
/// takes them over), working directory `/`.
pub fn spawn_detached(program: &Path, args: &[String], log: &Path) -> std::io::Result<Child> {
    let out = open_append(log)?;
    let err = out.try_clone()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn()
}

/// Whether `pid` names a live process (one this user may signal).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 only probes for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Send `signal` to `pid` (never to a group or to every process).
#[cfg(unix)]
pub fn send_signal(pid: u32, signal: libc::c_int) -> std::io::Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|&p| p > 0)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad pid"))?;
    // SAFETY: kill(2) on one positive pid.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Poll `done` every 50 ms until it is true or `timeout` passes.
pub fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last `lines` lines of `text` (all of it for 0).
pub fn last_lines(text: &str, lines: usize) -> &str {
    if lines == 0 {
        return text;
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let mut seen = 0;
    for (i, b) in trimmed.bytes().enumerate().rev() {
        if b == b'\n' {
            seen += 1;
            if seen == lines {
                return &text[i + 1..];
            }
        }
    }
    text
}

/// Read `path` from `offset` to its end (lossy UTF-8).
pub fn read_from(path: &Path, offset: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset.min(len)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Follow `path` like `tail -F` from `offset`, writing what is appended to
/// `out`. On rotation the old file is read to its end before the new one
/// is followed from its start; a truncated file is reread from the start.
/// Returns only when writing to `out` fails (the reader went away).
pub fn follow(path: &Path, offset: u64, out: &mut dyn Write) -> std::io::Result<()> {
    let mut current: Option<(File, Option<(u64, u64)>)> = None;
    let mut start = offset;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if current.is_none() {
            match File::open(path) {
                Ok(mut file) => {
                    // The identity of the file opened, not of whatever the
                    // path names by now (a rotation may land in between).
                    let meta = file.metadata()?;
                    file.seek(SeekFrom::Start(start.min(meta.len())))?;
                    current = Some((file, Some(identity_of(&meta))));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if let Some((file, identity)) = &mut current {
            let position = file.stream_position()?;
            if file.metadata()?.len() < position {
                file.seek(SeekFrom::Start(0))?;
            }
            let mut wrote = false;
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                wrote = true;
            }
            if wrote {
                out.flush()?;
            }
            if file_identity(path) != *identity {
                // Rotated (or replaced): the old file is drained; follow
                // the new one from its start.
                current = None;
                start = 0;
                continue;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Device and inode of `path` (changes when the log rotates).
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|meta| identity_of(&meta))
}

/// Device and inode of a file.
fn identity_of(meta: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

/// Quote one word for a systemd `ExecStart=` / `Environment=` line:
/// double-quoted, `\` and `"` escaped, `%` (specifiers) and `$`
/// (variable expansion) doubled.
pub fn systemd_quote(word: &str) -> String {
    let mut quoted = String::with_capacity(word.len() + 2);
    quoted.push('"');
    for c in word.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            '\n' => quoted.push_str("\\n"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// A systemd `--user` unit running `argv` (the hub in the foreground) with
/// `env` set, restarted if it fails. `KillMode=mixed`: `systemctl stop`
/// sends SIGTERM to the hub alone, which stops its sessions (recording
/// them for restore) before the rest of the cgroup is killed.
pub fn systemd_unit(argv: &[String], env: &[(String, String)]) -> String {
    let exec = argv
        .iter()
        .map(|w| systemd_quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    let mut unit = String::from(
        "[Unit]\n\
         Description=flywheel hub (web workspace for AI coding agents)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n",
    );
    unit.push_str(&format!("ExecStart={exec}\n"));
    for (name, value) in env {
        unit.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{name}={value}"))
        ));
    }
    unit.push_str(
        "Restart=on-failure\n\
         RestartSec=2\n\
         KillMode=mixed\n\
         TimeoutStopSec=20\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    unit
}

/// Where systemd looks for user units: `$XDG_CONFIG_HOME/systemd/user`,
/// else `~/.config/systemd/user`.
pub fn systemd_user_dir_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| Path::new(v).is_absolute()) {
        return Some(Path::new(xdg).join("systemd").join("user"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(".config").join("systemd").join("user"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_log_keeps_bounded_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.log");
        let mut log = RotatingLog::open(&path, 10, 2).unwrap();
        for chunk in ["aaaaaa", "bbbbbb", "cccccc", "dddddd"] {
            log.write(chunk.as_bytes()).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dddddd");
        let one = RotatingLog::generation(&path, 1);
        let two = RotatingLog::generation(&path, 2);
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "cccccc");
        assert_eq!(std::fs::read_to_string(&two).unwrap(), "bbbbbb");
        assert!(!RotatingLog::generation(&path, 3).exists());
        // An oversized write into an empty file is kept whole.
        drop(log);
        std::fs::write(&path, "").unwrap();
        let mut log = RotatingLog::open(&path, 4, 2).unwrap();
        log.write(b"0123456789").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0123456789");
        // Reopening appends and counts what is there.
        drop(log);
        let mut log = RotatingLog::open(&path, 12, 2).unwrap();
        log.write(b"xyz").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xyz");
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "0123456789");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn last_lines_counts_from_the_end() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb\n", 5), "a\nb\n");
        assert_eq!(last_lines("a\nb\n", 0), "a\nb\n");
        assert_eq!(last_lines("", 3), "");
    }

    #[test]
    fn follow_reads_appends_across_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.log");
        std::fs::write(&path, "old\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        struct Chan(std::sync::mpsc::Sender<Vec<u8>>);
        impl Write for Chan {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .send(buf.to_vec())
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let follow_path = path.clone();
        let offset = std::fs::metadata(&path).unwrap().len();
        std::thread::spawn(move || follow(&follow_path, offset, &mut Chan(tx)));
        let mut seen = String::new();
        let mut wait_for = |needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !seen.contains(needle) {
                let left = deadline.saturating_duration_since(Instant::now());
                let chunk = rx.recv_timeout(left).expect("followed output");
                seen.push_str(&String::from_utf8_lossy(&chunk));
            }
        };
        let mut log = RotatingLog::open(&path, 16, 1).unwrap();
        log.write(b"first\n").unwrap();
        wait_for("first\n");
        log.write(b"second line\n").unwrap();
        wait_for("second line\n");
        assert!(!seen.contains("old"), "{seen}");
    }

    #[test]
    fn systemd_words_are_quoted_and_escaped() {
        assert_eq!(systemd_quote("/usr/bin/fw"), "\"/usr/bin/fw\"");
        assert_eq!(
            systemd_quote(r#"a "b" \c 100% $HOME"#),
            r#""a \"b\" \\c 100%% $$HOME""#
        );
        let unit = systemd_unit(
            &["/opt/my bin/fw".into(), "hub".into(), "serve".into()],
            &[("PATH".into(), "/usr/bin:/bin".into())],
        );
        assert!(
            unit.contains("ExecStart=\"/opt/my bin/fw\" \"hub\" \"serve\"\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"),
            "{unit}"
        );
        assert!(unit.contains("KillMode=mixed\n") && unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_user_dir_prefers_absolute_xdg_config_home() {
        assert_eq!(
            systemd_user_dir_from(Some("/x/cfg"), Some("/home/u")),
            Some(PathBuf::from("/x/cfg/systemd/user"))
        );
        assert_eq!(
            systemd_user_dir_from(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/systemd/user"))
        );
        assert_eq!(systemd_user_dir_from(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn process_probe_and_signal() {
        assert!(process_alive(std::process::id()));
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(process_alive(pid));
        send_signal(pid, libc::SIGTERM).unwrap();
        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(send_signal(0, libc::SIGTERM).is_err());
    }
}
