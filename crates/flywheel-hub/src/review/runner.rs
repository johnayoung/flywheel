//! The bounded runner every review `git` / `gh` call goes through: argv
//! only (never a shell), its own session (no controlling terminal, so
//! nothing can prompt: no passphrase, no credential, no editor), stdin a
//! given byte string or nothing, stdout and stderr capped, a deadline that
//! kills the whole process group, and at most [`MAX_RUNNING`] at once.
//!
//! Blocking: call off the async executor.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Review commands running at once, hub-wide; more wait for a slot.
pub const MAX_RUNNING: usize = 4;
/// How long a command waits for a slot before giving up.
const SLOT_WAIT: Duration = Duration::from_secs(30);
/// stderr kept per command (the rest is drained and dropped).
const STDERR_LIMIT: usize = 64 * 1024;
/// How often a running command is checked for exit / its deadline.
const POLL: Duration = Duration::from_millis(5);
/// How long output is still read once the program ended.
const READER_GRACE: Duration = Duration::from_secs(2);

/// Variables that would point git at another repository than the one
/// asked for (a hub started from inside a git hook inherits them).
const GIT_ENV_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
];

/// What one run produced.
#[derive(Debug, Clone)]
pub struct Output {
    /// The exit code; `None` when it was killed (deadline or signal).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// stdout went past the limit (only the first `limit` bytes are kept).
    pub truncated: bool,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr, line by line as git wrote it: `fatal: ` / `error: ` /
    /// `hint: ` prefixes dropped, blank lines skipped.
    pub fn error_text(&self) -> String {
        let joined = self
            .stderr
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches("fatal: ")
                    .trim_start_matches("error: ")
                    .trim_start_matches("hint: ")
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return joined;
        }
        let out = String::from_utf8_lossy(&self.stdout).trim().to_owned();
        if !out.is_empty() {
            return out.lines().last().unwrap_or_default().to_owned();
        }
        match (self.timed_out, self.code) {
            (true, _) => "timed out".to_owned(),
            (false, Some(code)) => format!("exit code {code}"),
            (false, None) => "killed by a signal".to_owned(),
        }
    }
}

/// One command to run.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    pub program: &'a str,
    pub dir: &'a Path,
    pub args: Vec<String>,
    /// Bytes written to its stdin (then closed); `None`: stdin is null.
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    /// stdout bytes kept.
    pub limit: usize,
}

/// Why a command could not be run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The program is not installed (not found on `PATH`).
    NotFound(String),
    /// Every slot stayed busy for [`SLOT_WAIT`].
    Busy,
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(program) => write!(f, "{program} is not installed (not found on PATH)"),
            Self::Busy => write!(
                f,
                "the hub is already running {MAX_RUNNING} git commands; try again shortly"
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

struct Slots {
    running: Mutex<usize>,
    freed: Condvar,
}

static SLOTS: Slots = Slots {
    running: Mutex::new(0),
    freed: Condvar::new(),
};

struct Slot;

impl Slot {
    fn take() -> Result<Self, RunError> {
        let deadline = Instant::now() + SLOT_WAIT;
        let mut running = SLOTS.running.lock().unwrap_or_else(PoisonError::into_inner);
        while *running >= MAX_RUNNING {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RunError::Busy);
            }
            running = SLOTS
                .freed
                .wait_timeout(running, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *running += 1;
        Ok(Self)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut running = SLOTS.running.lock().unwrap_or_else(PoisonError::into_inner);
        *running = running.saturating_sub(1);
        SLOTS.freed.notify_one();
    }
}

/// Read `from` to its end, keeping at most `limit` bytes (the rest is
/// drained so the child never blocks on a full pipe); whether any was
/// dropped.
fn drain(mut from: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    (kept, truncated)
}

/// Run `spec` to its end or its deadline.
pub fn run(spec: &Spec<'_>) -> Result<Output, RunError> {
    let _slot = Slot::take()?;
    let mut command = Command::new(spec.program);
    command
        .args(spec.args.iter().map(OsStr::new))
        .current_dir(spec.dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GCM_INTERACTIVE", "never")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .env("LC_ALL", "C")
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in GIT_ENV_REMOVED {
        command.env_remove(name);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe and touches no memory. A
        // new session has no controlling terminal (ssh / credential
        // helpers cannot prompt on the hub's tty) and its own process
        // group (the deadline kills all of it).
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RunError::NotFound(spec.program.to_owned())
        } else {
            RunError::Io(format!("cannot run {}: {e}", spec.program))
        }
    })?;
    // Pipe ends a program handed to something that escaped its group
    // could stay open: readers are waited on for a short grace only.
    let (done, finished) = std::sync::mpsc::channel::<(u8, Vec<u8>, bool)>();
    if let (Some(mut stdin), Some(bytes)) = (child.stdin.take(), spec.stdin.clone()) {
        std::thread::spawn(move || {
            // A program that exits without reading it all is not an error.
            let _ = stdin.write_all(&bytes);
        });
    }
    let limit = spec.limit;
    if let Some(out) = child.stdout.take() {
        let done = done.clone();
        std::thread::spawn(move || {
            let (bytes, truncated) = drain(out, limit);
            let _ = done.send((1, bytes, truncated));
        });
    }
    if let Some(err) = child.stderr.take() {
        let done = done.clone();
        std::thread::spawn(move || {
            let (bytes, truncated) = drain(err, STDERR_LIMIT);
            let _ = done.send((2, bytes, truncated));
        });
    }
    drop(done);
    let deadline = Instant::now() + spec.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_group(&mut child);
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                kill_group(&mut child);
                let _ = child.wait();
                return Err(RunError::Io(format!("waiting for {}: {e}", spec.program)));
            }
        }
    };
    // Anything the program left running in its group (a credential
    // helper, ssh) goes too, so the pipes close and the readers finish.
    kill_group(&mut child);
    let (mut stdout, mut stderr, mut truncated) = (Vec::new(), Vec::new(), false);
    let grace = Instant::now() + READER_GRACE;
    for _ in 0..2 {
        match finished.recv_timeout(grace.saturating_duration_since(Instant::now())) {
            Ok((1, bytes, cut)) => {
                stdout = bytes;
                truncated = cut;
            }
            Ok((_, bytes, _)) => stderr = bytes,
            Err(_) => break,
        }
    }
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated,
        timed_out,
    })
}

/// SIGKILL the group the child leads (setsid made it a leader); the
/// child itself, when that fails.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id())
        // SAFETY: kill(2) on the process group the child leads.
        && unsafe { libc::kill(-pid, libc::SIGKILL) } == 0
    {
        return;
    }
    let _ = child.kill();
}

/// `argv` as one line a person can read and paste into a shell
/// (single-quoted where needed). For showing only: nothing is ever run
/// through a shell.
pub fn display(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One word, single-quoted for a shell when it needs to be.
pub fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,^~".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration, limit: usize, stdin: Option<&[u8]>) -> Output {
        run(&Spec {
            program: "/bin/sh",
            dir: Path::new("/"),
            args: vec!["-c".into(), script.into()],
            stdin: stdin.map(<[u8]>::to_vec),
            timeout,
            limit,
        })
        .unwrap()
    }

    #[test]
    fn output_is_capped_and_the_rest_drained() {
        // 1 MiB written, 1000 bytes kept; the writer never blocks.
        let out = sh(
            "head -c 1048576 /dev/zero; echo done >&2",
            Duration::from_secs(20),
            1000,
            None,
        );
        assert!(out.success());
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.truncated);
        assert_eq!(out.stderr.trim(), "done");
    }

    #[test]
    fn a_deadline_kills_the_whole_group() {
        let started = Instant::now();
        // A background child holding the pipe open dies with the group.
        let out = sh("sleep 30 & sleep 30", Duration::from_millis(300), 100, None);
        assert!(out.timed_out);
        assert_eq!(out.code, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn stdin_is_given_and_there_is_no_terminal() {
        let out = sh(
            "cat; tty || true",
            Duration::from_secs(10),
            4096,
            Some(b"hello\n"),
        );
        let text = out.stdout_text();
        assert!(text.starts_with("hello\n"), "{text}");
        assert!(text.contains("not a tty"), "{text}");
        // Without a given stdin, it reads nothing (null).
        let out = sh("wc -c", Duration::from_secs(10), 100, None);
        assert_eq!(out.stdout_text().trim(), "0");
    }

    #[test]
    fn a_missing_program_is_named() {
        let err = run(&Spec {
            program: "fw-hub-no-such-program",
            dir: Path::new("/"),
            args: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(1),
            limit: 10,
        })
        .unwrap_err();
        assert_eq!(err, RunError::NotFound("fw-hub-no-such-program".into()));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        assert_eq!(
            display(
                "git",
                &["-C".into(), "/a b".into(), "push".into(), "it's".into()]
            ),
            r"git -C '/a b' push 'it'\''s'"
        );
        assert_eq!(
            display("git", &["refs/heads/x:refs/heads/x".into()]),
            "git refs/heads/x:refs/heads/x"
        );
    }

    #[test]
    fn error_text_strips_git_prefixes() {
        let out = Output {
            code: Some(128),
            stdout: Vec::new(),
            stderr: "fatal: bad thing\n\nhint: do this\n".into(),
            truncated: false,
            timed_out: false,
        };
        assert_eq!(out.error_text(), "bad thing\ndo this");
    }
}
