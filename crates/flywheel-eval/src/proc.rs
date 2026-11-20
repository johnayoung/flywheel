//! Running a command the way graders run: `/bin/sh -c` in its own process
//! group, output captured to a log file, a wall-clock bound that kills the
//! whole group, and optionally under a named lock (a hub lease with a
//! `flock` fallback, the `scripts/check.sh` convention), so eval's oracle
//! runs and trials take turns with every other test suite on the machine.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How much of a log the outcome keeps.
pub const TAIL_BYTES: usize = 8192;

/// How a command ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    /// The exit code; `None` when killed by a signal or the timeout.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub secs: f64,
    /// The last [`TAIL_BYTES`] of its combined output.
    pub tail: String,
}

impl Ran {
    pub fn passed(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// `exit 1`, `timed out after 30s`, `killed by a signal`.
    pub fn describe(&self) -> String {
        match (self.code, self.timed_out) {
            (_, true) => format!("timed out after {:.0}s", self.secs),
            (Some(c), _) => format!("exit {c}"),
            (None, _) => "killed by a signal".to_owned(),
        }
    }
}

/// The turn-taking a command runs under.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "with", rename_all = "snake_case")]
pub enum Locking {
    /// Run as is.
    #[default]
    None,
    /// `flock PATH` (exclusive).
    Flock(PathBuf),
    /// `fw hub lease run RESOURCE` (waiting up to `wait`), taking `flock`
    /// too (or instead, when no hub answers): what `scripts/check.sh` does.
    Lease {
        fw: PathBuf,
        /// `--url` and the like, naming which hub.
        endpoint: Vec<String>,
        resource: String,
        shared: bool,
        ttl: String,
        wait: String,
        note: String,
        flock: Option<PathBuf>,
    },
    /// Each in turn, outermost first.
    Chain(Vec<Locking>),
}

impl Locking {
    /// The argv prefix that wraps a command in this turn-taking.
    pub fn prefix(&self) -> Vec<String> {
        match self {
            Self::None => Vec::new(),
            Self::Flock(path) => vec!["flock".into(), path.display().to_string()],
            Self::Lease {
                fw,
                endpoint,
                resource,
                shared,
                ttl,
                wait,
                note,
                flock,
            } => {
                let mut argv = vec![
                    fw.display().to_string(),
                    "hub".into(),
                    "lease".into(),
                    "run".into(),
                    resource.clone(),
                    "--ttl".into(),
                    ttl.clone(),
                    "--wait".into(),
                    wait.clone(),
                    "--note".into(),
                    note.clone(),
                ];
                if *shared {
                    argv.push("--shared".into());
                }
                if let Some(lock) = flock {
                    argv.push("--or-flock".into());
                    argv.push(lock.display().to_string());
                }
                argv.extend(endpoint.iter().cloned());
                argv.push("--".into());
                argv
            }
            Self::Chain(all) => all.iter().flat_map(Self::prefix).collect(),
        }
    }
}

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct Spec<'a> {
    pub argv: Vec<String>,
    pub cwd: Option<&'a Path>,
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub timeout: Option<Duration>,
    /// Where the combined output goes (else a temporary file).
    pub log: Option<&'a Path>,
    pub locking: Locking,
}

impl Spec<'_> {
    /// `/bin/sh -c CMD`.
    pub fn shell(cmd: &str) -> Self {
        Self {
            argv: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
            ..Self::default()
        }
    }
}

fn tail_of(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let start = bytes.len().saturating_sub(TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Run `spec` to completion (or its timeout, which kills its process
/// group). An `Err` is a failure to start it at all.
pub fn run(spec: &Spec<'_>) -> std::io::Result<Ran> {
    let mut argv = spec.locking.prefix();
    argv.extend(spec.argv.iter().cloned());
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty command"))?;
    let scratch;
    let log_path = match spec.log {
        Some(path) => path.to_owned(),
        None => {
            scratch = tempfile::NamedTempFile::new()?;
            scratch.path().to_owned()
        }
    };
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    for key in &spec.unset {
        cmd.env_remove(key);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Its own process group, so a timeout reaches every child.
    // SAFETY: setpgid is async-signal-safe and touches only this child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if spec.timeout.is_some_and(|t| started.elapsed() >= t) {
            timed_out = true;
            // SAFETY: signalling the group this function created.
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_secs(2));
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok(Ran {
        code: status.code(),
        timed_out,
        secs: started.elapsed().as_secs_f64(),
        tail: tail_of(&log_path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_captures_and_times_out_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("out.log");
        let ran = run(&Spec {
            log: Some(&log),
            env: vec![("X".into(), "42".into())],
            ..Spec::shell("echo hello $X; exit 3")
        })
        .unwrap();
        assert_eq!(ran.code, Some(3));
        assert!(!ran.passed());
        assert!(ran.tail.contains("hello 42"));
        let marker = dir.path().join("late");
        let ran = run(&Spec {
            timeout: Some(Duration::from_millis(300)),
            ..Spec::shell(&format!("(sleep 3; touch {}) & sleep 5", marker.display()))
        })
        .unwrap();
        assert!(ran.timed_out);
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the background child died with the group");
    }

    #[test]
    fn locking_wraps_the_command() {
        let lease = Locking::Lease {
            fw: "/bin/fw".into(),
            endpoint: vec!["--url".into(), "http://h".into()],
            resource: "cpu:test-suite".into(),
            shared: false,
            ttl: "45m".into(),
            wait: "4h".into(),
            note: "eval".into(),
            flock: Some("/tmp/l".into()),
        };
        let p = lease.prefix();
        assert_eq!(
            &p[..5],
            ["/bin/fw", "hub", "lease", "run", "cpu:test-suite"]
        );
        assert!(p.windows(2).any(|w| w == ["--or-flock", "/tmp/l"]));
        assert_eq!(p.last().unwrap(), "--");
        assert_eq!(
            Locking::Flock("/tmp/l".into()).prefix(),
            ["flock", "/tmp/l"]
        );
    }
}
