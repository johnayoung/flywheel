//! Shared fixture for the `self_update_*` integration tests.
//!
//! Every run is confined to disposable dirs: `HOME` and `FLYWHEEL_HOME`
//! are fresh temp dirs, every inherited `FLYWHEEL_*` variable (and
//! `CARGO_TARGET_DIR`) is removed from the child environment, and the
//! install dir is a temp dir seeded with copies of the `fw` under test.
//! A fake `cargo` first on `PATH` logs its cwd and argv to a control dir
//! outside the home and "builds" by copying the `fw` under test to both
//! artifact paths with a random trailing marker (unique bytes per run,
//! still executable). Nothing here signals a process it did not spawn.

#![allow(dead_code)]

#[path = "../../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub use script::write_script;

/// The fake `cargo`. Control variables (all optional):
/// `FAKE_CARGO_EXIT` exits with that code before building;
/// `FAKE_CARGO_HOLD` waits for that file to exist (after touching
/// `$FAKE_CARGO_HOLD.started`); `FAKE_CARGO_NOEXEC` leaves `fw` mode 0644.
pub const FAKE_CARGO: &str = r#"#!/bin/sh
printf '%s\t%s\n' "$(pwd -P)" "$*" >> "$FAKE_CARGO_CONTROL/cargo.log"
if [ -n "$FAKE_CARGO_HOLD" ]; then
  : > "$FAKE_CARGO_HOLD.started"
  while [ ! -e "$FAKE_CARGO_HOLD" ]; do sleep 0.05; done
fi
if [ -n "$FAKE_CARGO_EXIT" ]; then echo "fake cargo: failing" >&2; exit "$FAKE_CARGO_EXIT"; fi
t="${CARGO_TARGET_DIR:-$(pwd -P)/target}/release"
mkdir -p "$t" || exit 1
marker="fake-cargo-marker-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
for b in fw flywheel; do
  rm -f "$t/$b" "$t/$b.tmp"
  cp "$FAKE_CARGO_FW" "$t/$b.tmp" || exit 1
  printf '%s' "$marker" >> "$t/$b.tmp" || exit 1
  mv "$t/$b.tmp" "$t/$b" || exit 1
done
if [ -n "$FAKE_CARGO_NOEXEC" ]; then chmod 644 "$t/fw"; fi
printf '%s\n' "$marker" >> "$FAKE_CARGO_CONTROL/markers"
"#;

/// The minimal flywheel manifest a scratch checkout carries.
pub const FLYWHEEL_MANIFEST: &str = r#"[package]
name = "flywheel"
version = "0.0.0"
edition = "2024"

[[bin]]
name = "flywheel"
path = "src/main.rs"

[[bin]]
name = "fw"
path = "src/bin/fw.rs"
"#;

pub fn fw_under_test() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fw"))
}

pub fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
}

/// Copy a file through a `/bin/cp` child, so this process never holds a
/// writable descriptor to an executable another thread may exec (ETXTBSY).
pub fn copy_exe(from: &Path, to: &Path) {
    let status = Command::new("/bin/cp").arg(from).arg(to).status().unwrap();
    assert!(status.success(), "cp {} {}", from.display(), to.display());
}

/// `relative path -> sha256` of every file under `dir` (symlinks by
/// target), minus `skip` (relative paths).
pub fn manifest(dir: &Path, skip: &[&str]) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, skip: &[&str], out: &mut BTreeMap<String, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if skip.contains(&rel.as_str()) {
                continue;
            }
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(rel, format!("-> {}", target.display()));
            } else if kind.is_dir() {
                walk(root, &path, skip, out);
            } else {
                out.insert(rel, sha256(&path));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, skip, &mut out);
    out
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {output:?}",
        dir.display()
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env("GIT_AUTHOR_NAME", "self-update test")
        .env("GIT_AUTHOR_EMAIL", "self-update@example.invalid")
        .env("GIT_COMMITTER_NAME", "self-update test")
        .env("GIT_COMMITTER_EMAIL", "self-update@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    command
}

/// One finished `fw self-update` run.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn from_output(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Every stdout line as JSON (panics on a non-JSON line).
    pub fn json_lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("non-JSON stdout line {l:?}: {e}\n{self:?}"))
            })
            .collect()
    }

    /// The `--json` summary, asserting the stream shape: the eight steps in
    /// order, each once, then exactly one summary.
    pub fn summary(&self) -> Value {
        let lines = self.json_lines();
        assert_eq!(lines.len(), STEPS.len() + 1, "{self:?}");
        for (line, step) in lines.iter().zip(STEPS) {
            assert_eq!(line["step"], step, "{self:?}");
            assert!(
                ["ok", "skipped", "failed", "reverted", "planned"]
                    .contains(&line["status"].as_str().unwrap()),
                "{line}"
            );
            assert!(line.get("detail").is_some(), "{line}");
        }
        let summary = lines.last().unwrap()["summary"].clone();
        assert!(summary.is_object(), "{self:?}");
        summary
    }

    /// `(step, status)` pairs from `--json` output.
    pub fn json_statuses(&self) -> Vec<(String, String)> {
        self.json_lines()
            .iter()
            .filter(|l| l.get("step").is_some())
            .map(|l| {
                (
                    l["step"].as_str().unwrap().to_owned(),
                    l["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// `(step, status)` pairs from human output, asserting one line per
    /// step in order and a final `self-update: <outcome>` line.
    pub fn human_statuses(&self) -> (Vec<(String, String)>, String) {
        let lines: Vec<&str> = self.stdout.lines().collect();
        let step_lines: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| {
                STEPS
                    .iter()
                    .any(|s| l.starts_with(&format!("self-update: {s} ")))
            })
            .collect();
        assert_eq!(step_lines.len(), STEPS.len(), "{self:?}");
        let statuses: Vec<(String, String)> = step_lines
            .iter()
            .zip(STEPS)
            .map(|(line, step)| {
                let rest = line
                    .strip_prefix(&format!("self-update: {step} "))
                    .unwrap_or_else(|| panic!("{line:?} is not step {step}"));
                let status = rest.split(' ').next().unwrap().to_owned();
                (step.to_owned(), status)
            })
            .collect();
        let last = lines.last().copied().unwrap_or_default();
        let outcome = last
            .strip_prefix("self-update: ")
            .unwrap_or_else(|| panic!("last line {last:?}"))
            .to_owned();
        (statuses, outcome)
    }
}

pub const STEPS: [&str; 8] = [
    "preflight",
    "build",
    "drain",
    "backup",
    "install",
    "db-upgrade",
    "hub",
    "restart",
];

/// The disposable world one test runs in.
pub struct Fixture {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    /// `HOME`.
    pub home: PathBuf,
    /// `FLYWHEEL_HOME`.
    pub flywheel_home: PathBuf,
    /// The fake `cargo`'s log dir (outside both homes).
    pub control: PathBuf,
    /// Holds the fake `cargo`.
    pub bin: PathBuf,
    /// The install dir, seeded with copies of the `fw` under test.
    pub install: PathBuf,
    /// The bare `origin` the checkout was cloned from.
    pub origin: PathBuf,
    /// A clean flywheel checkout (a clone of `origin`) at a landed HEAD.
    pub checkout: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let flywheel_home = root.join("flywheel-home");
        let control = root.join("control");
        let bin = root.join("bin");
        let install = root.join("install");
        for d in [&home, &flywheel_home, &control, &bin, &install] {
            std::fs::create_dir_all(d).unwrap();
        }
        write_script(&bin.join("cargo"), FAKE_CARGO);
        copy_exe(&fw_under_test(), &install.join("fw"));
        copy_exe(&fw_under_test(), &install.join("flywheel"));

        let seed = root.join("seed");
        std::fs::create_dir_all(seed.join("crates/flywheel/src/bin")).unwrap();
        std::fs::write(seed.join("crates/flywheel/Cargo.toml"), FLYWHEEL_MANIFEST).unwrap();
        std::fs::write(seed.join("crates/flywheel/src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(seed.join("crates/flywheel/src/bin/fw.rs"), "fn main() {}\n").unwrap();
        std::fs::write(seed.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(seed.join("README.md"), "scratch flywheel\n").unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-qm", "init", "--no-gpg-sign"]);
        let origin = root.join("origin.git");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let checkout = root.join("checkout");
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::remove_dir_all(&seed).unwrap();
        Self {
            _dir: dir,
            root,
            home,
            flywheel_home,
            control,
            bin,
            install,
            origin,
            checkout,
        }
    }

    /// `fw self-update args...` in `cwd`, fully confined to this fixture.
    /// The install dir is always explicit unless `args` names one.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(fw_under_test());
        command.arg("self-update").args(args);
        if !args.iter().any(|a| a.starts_with("--install-dir")) {
            command.arg("--install-dir").arg(&self.install);
        }
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().into_owned();
            if key.starts_with("FLYWHEEL_") || key.starts_with("FAKE_CARGO_") {
                command.env_remove(&key);
            }
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&path));
        command
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("FLYWHEEL_HOME", &self.flywheel_home)
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("FAKE_CARGO_CONTROL", &self.control)
            .env("FAKE_CARGO_FW", fw_under_test())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    pub fn run(&self, cwd: &Path, args: &[&str]) -> Run {
        self.run_with(cwd, args, |_| {})
    }

    pub fn run_with(&self, cwd: &Path, args: &[&str], tweak: impl FnOnce(&mut Command)) -> Run {
        let mut command = self.command(cwd, args);
        tweak(&mut command);
        Run::from_output(command.output().unwrap())
    }

    pub fn spawn_with(&self, cwd: &Path, args: &[&str], tweak: impl FnOnce(&mut Command)) -> Child {
        let mut command = self.command(cwd, args);
        tweak(&mut command);
        command.spawn().unwrap()
    }

    /// The fake `cargo`'s invocations: `(cwd, argv)`.
    pub fn cargo_log(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.control.join("cargo.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let (cwd, argv) = l.split_once('\t').unwrap();
                (cwd.to_owned(), argv.to_owned())
            })
            .collect()
    }

    /// The marker of the fake `cargo`'s most recent completed build.
    pub fn last_marker(&self) -> Option<String> {
        std::fs::read_to_string(self.control.join("markers"))
            .ok()?
            .lines()
            .last()
            .map(str::to_owned)
    }

    pub fn head(&self) -> String {
        git(&self.checkout, &["rev-parse", "HEAD"])
    }

    /// The install dir's manifest.
    pub fn install_manifest(&self) -> BTreeMap<String, String> {
        manifest(&self.install, &[])
    }

    /// The `FLYWHEEL_HOME` manifest minus the lock file.
    pub fn home_manifest(&self) -> BTreeMap<String, String> {
        manifest(&self.flywheel_home, &["self-update/lock"])
    }

    /// The `HOME` manifest.
    pub fn user_home_manifest(&self) -> BTreeMap<String, String> {
        manifest(&self.home, &[])
    }

    /// Assert both installed binaries end with `marker` and equal the
    /// artifacts at `release`.
    pub fn assert_installed_from(&self, release: &Path, marker: &str) {
        for name in ["fw", "flywheel"] {
            let installed = self.install.join(name);
            let bytes = std::fs::read(&installed).unwrap();
            assert!(
                bytes.ends_with(marker.as_bytes()),
                "{name} does not carry this run's marker"
            );
            assert_eq!(
                sha256(&installed),
                sha256(&release.join(name)),
                "{name} differs from its artifact"
            );
        }
    }
}

// --- processes the drain tests spawn ------------------------------------------

/// Clear every inherited `FLYWHEEL_*` / `FAKE_CARGO_*` variable and confine
/// `command` to `home` / `flywheel_home`, with `extra_path` first on PATH.
pub fn confine(command: &mut Command, home: &Path, flywheel_home: &Path, extra_path: &[&Path]) {
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("FLYWHEEL_") || key.starts_with("FAKE_CARGO_") {
            command.env_remove(&key);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = extra_path.iter().map(|p| p.to_path_buf()).collect();
    paths.extend(std::env::split_paths(&path));
    command
        .env("HOME", home)
        .env("FLYWHEEL_HOME", flywheel_home)
        .env("XDG_STATE_HOME", home.join("xdg-state"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude-config"))
        .env("CODEX_HOME", home.join("codex-home"))
        .env("SHELL", "/bin/sh")
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("DATABASE_URL")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
}

/// A git repo with one commit at `dir`, registered as a project of the
/// home at `flywheel_home` (`fw project path` run there); returns the
/// project dir.
pub fn project_repo(dir: &Path, home: &Path, flywheel_home: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "drain test\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "init", "--no-gpg-sign"]);
    let mut command = Command::new(fw_under_test());
    command
        .args(["project", "path"])
        .current_dir(dir)
        .stdin(Stdio::null());
    confine(&mut command, home, flywheel_home, &[]);
    let output = command.output().unwrap();
    assert!(output.status.success(), "fw project path: {output:?}");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

/// `(ppid, starttime, state)` of `pid`.
pub fn stat_of(pid: u32) -> Option<(u32, u64, char)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some((
        fields.get(1)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
        fields.first()?.chars().next()?,
    ))
}

/// Gone, or a zombie awaiting its reaper.
pub fn gone(pid: u32) -> bool {
    stat_of(pid).is_none_or(|(_, _, state)| state == 'Z')
}

/// `/proc/<pid>/cmdline` as argv.
pub fn argv_of(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Live (non-zombie) children of `parent`.
pub fn children_of(parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some((ppid, _, state)) = stat_of(pid)
            && ppid == parent
            && state != 'Z'
        {
            out.push(pid);
        }
    }
    out
}

/// Every live descendant of `root` (not `root` itself).
pub fn descendants_of(root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        for child in children_of(pid) {
            out.push(child);
            queue.push(child);
        }
    }
    out
}

pub fn signal(pid: u32, sig: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: kill(2) on a pid (or the group) of a process this test
    // spawned; a vanished target yields ESRCH.
    unsafe {
        libc::kill(pid, sig);
    }
}

pub fn wait_until<T>(
    what: &str,
    timeout: std::time::Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Processes this test spawned (and their descendants, recorded as they
/// are learned), SIGKILLed with their groups on drop so a failed
/// assertion leaks nothing. Only pids the test itself started go in.
#[derive(Default)]
pub struct Spawned {
    children: Vec<Child>,
    pids: Vec<u32>,
}

impl Spawned {
    pub fn push(&mut self, child: Child) -> u32 {
        let pid = child.id();
        self.children.push(child);
        pid
    }

    /// Record a descendant of a spawned process.
    pub fn track(&mut self, pid: u32) {
        self.pids.push(pid);
    }

    /// The child spawned with `pid`.
    pub fn child(&mut self, pid: u32) -> &mut Child {
        self.children.iter_mut().find(|c| c.id() == pid).unwrap()
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let roots: Vec<u32> = self.children.iter().map(Child::id).collect();
        let mut all: Vec<u32> = self.pids.clone();
        for root in &roots {
            all.extend(descendants_of(*root));
        }
        for pid in roots.iter().chain(&all) {
            signal(*pid, libc::SIGCONT);
            signal(*pid, libc::SIGKILL);
        }
        for root in &roots {
            // Group leaders (setsid) take their groups down with them.
            if let Ok(pgid) = libc::pid_t::try_from(*root) {
                signal_group(pgid, libc::SIGKILL);
            }
        }
        for child in &mut self.children {
            let _ = child.wait();
        }
    }
}

fn signal_group(pgid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: kill(2) on the negative pgid of a group this test's own
    // setsid child leads.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// `command` runs as the leader of a new session and process group.
pub fn new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid(2) is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// A running `fw hub serve` for a fixture home (state dir
/// `$FLYWHEEL_HOME/hub`, the default).
pub struct Hub {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

impl Hub {
    /// Start `program hub serve --port 0` confined to `home` /
    /// `flywheel_home`, with `extra_path` first on PATH.
    pub fn start(
        spawned: &mut Spawned,
        program: &Path,
        home: &Path,
        flywheel_home: &Path,
        extra_path: &[&Path],
    ) -> Self {
        use std::io::BufRead;
        let mut command = Command::new(program);
        command
            .args(["hub", "serve", "--port", "0"])
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        confine(&mut command, home, flywheel_home, extra_path);
        new_session(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pid = spawned.push(child);
        let mut lines = std::io::BufReader::new(stdout).lines();
        let first = lines.next().unwrap().unwrap();
        let port = first
            .rsplit(':')
            .next()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("unexpected banner {first:?}"));
        std::thread::spawn(move || for _ in lines {});
        let token = std::fs::read_to_string(flywheel_home.join("hub/token"))
            .unwrap()
            .trim()
            .to_owned();
        Self { pid, port, token }
    }

    /// One HTTP request with the hub token: `(status, JSON body)`.
    pub fn http(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.port,
            self.token,
            body.len()
        )
        .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let raw = String::from_utf8_lossy(&raw);
        let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
        let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
        let payload = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(payload)
        } else {
            payload.to_owned()
        };
        (
            status,
            serde_json::from_str(&payload).unwrap_or(Value::Null),
        )
    }

    /// A workspace holding `folder`, and a hub worker session started in
    /// it: `(session id, the session's wrapper pid)`.
    pub fn start_worker(&self, folder: &Path) -> (String, u32) {
        let (code, workspace) = self.http(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({ "name": "drain", "folders": [{"path": folder}] })),
        );
        assert_eq!(code, 201, "{workspace}");
        let (code, session) = self.http(
            "POST",
            "/api/flywheel/worker",
            Some(serde_json::json!({ "folder": folder, "workspace_id": workspace["id"] })),
        );
        assert_eq!(code, 201, "{session}");
        assert_eq!(session["command"][3], "fw-worker", "{session}");
        let sid = session["id"].as_str().unwrap().to_owned();
        let wrapper = wait_until(
            "the session's process",
            std::time::Duration::from_secs(30),
            || {
                let (_, view) = self.http("GET", &format!("/api/sessions/{sid}"), None);
                view["live"]["pid"].as_u64().map(|p| p as u32)
            },
        );
        (sid, wrapper)
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}

/// The `fw worker` child of a hub worker session's wrapper `wrapper`.
pub fn worker_under(wrapper: u32) -> u32 {
    wait_until(
        "the hub worker process",
        std::time::Duration::from_secs(30),
        || {
            children_of(wrapper)
                .into_iter()
                .find(|pid| argv_of(*pid).get(1).map(String::as_str) == Some("worker"))
        },
    )
}

// --- project stores the stores tests seed and inspect -------------------------

/// `fw args...` in `cwd`, confined to `home` / `flywheel_home`.
pub fn fw_confined(cwd: &Path, home: &Path, flywheel_home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(fw_under_test());
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    confine(&mut command, home, flywheel_home, &[]);
    command.output().unwrap()
}

/// The project id a project dir is named by.
pub fn project_id_of(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Build `db` from the frozen previous-version DDL (run store v14,
/// orchestrator store v9, both WAL) with rows in both halves: run rows in
/// partition `project_id` (v14 is already partitioned), orchestrator rows
/// unkeyed (v9 predates the partition, so the upgrade assigns it).
pub fn seed_behind_store(db: &Path, project_id: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(include_str!(
        "../../../flywheel-core/tests/fixtures/persistence-schema-v14.sql"
    ))
    .unwrap();
    conn.execute_batch(include_str!(
        "../../../flywheel-orchestrator/tests/fixtures/orchestrator-schema-v9.sql"
    ))
    .unwrap();
    let keyed = "INSERT INTO tasks (project_id, id, first_seen_at, updated_at)
           VALUES (?1, 'legacy', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
         INSERT INTO task_versions
           (project_id, task_id, content_hash, goal, graders_json, tags_json, context_json,
            created_at)
           VALUES (?1, 'legacy', 'h-legacy', 'Goal legacy.',
                   '[{\"type\": \"command\", \"run\": \"true\"}]', '[]', 'null',
                   '2026-01-01T00:00:00+00:00');
         INSERT INTO lifecycles
           (project_id, run_id, task_id, status, version, retries, timestamps_json,
            updated_at, task_content_hash, source)
           VALUES (?1, 'run-legacy', 'legacy', 'failed', 3, 0, '{}',
                   '2026-01-01T00:00:00+00:00', 'h-legacy', '')";
    for statement in keyed.split(';').filter(|s| !s.trim().is_empty()) {
        conn.execute(statement, [project_id]).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, version, \
           conflict_keys_json) VALUES ('t1', 'w1', '2026-05-28T12:00:00+00:00', \
           '2026-05-28T12:00:30+00:00', 2, '[\"k\"]');
         INSERT INTO orchestrator_events (task_id, worker_id, event_type, version, \
           lease_expires_at, occurred_at) VALUES ('t1', 'w1', 'acquired', 1, \
           '2026-05-28T12:00:30+00:00', '2026-05-28T12:00:00+00:00');
         INSERT INTO work_items (task_id, first_seen_at, last_seen_at) VALUES ('t1', \
           '2026-05-28T12:00:00+00:00', '2026-05-28T12:00:00+00:00');",
    )
    .unwrap();
    conn.close().unwrap();
}

fn open_read_only(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// `(run, orchestrator)` schema versions `db` records.
pub fn store_versions(db: &Path) -> (Option<i64>, Option<i64>) {
    let conn = open_read_only(db);
    let read = |table: &str| match conn.query_row(
        &format!("SELECT version FROM {table} WHERE id = 1"),
        [],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) if e.to_string().contains("no such table") => None,
        Err(e) => panic!("cannot read {table} of {}: {e}", db.display()),
    };
    (read("schema_version"), read("orchestrator_schema_version"))
}

/// A `.dump`-equivalent of `db`, read through its own connection: every
/// schema object's SQL, then every table's rows (sorted), so two stores
/// with equal content dump equal.
pub fn store_dump(db: &Path) -> String {
    let conn = open_read_only(db);
    let mut out = String::new();
    let objects: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (kind, name, sql) in &objects {
        out.push_str(&format!(
            "{kind} {name}: {}\n",
            sql.as_deref().unwrap_or("")
        ));
    }
    for (kind, name, _) in &objects {
        if kind != "table" {
            continue;
        }
        let mut statement = conn.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
        let width = statement.column_count();
        let mut rows: Vec<String> = statement
            .query_map([], |r| {
                (0..width)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|cells| cells.join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.sort();
        for row in rows {
            out.push_str(&format!("{name}: {row}\n"));
        }
    }
    out
}

/// Every `(table, project_id)` value of the tables that carry the column.
pub fn partition_values(db: &Path) -> Vec<(String, String)> {
    let conn = open_read_only(db);
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut out = Vec::new();
    for table in tables {
        let has_column: bool = conn
            .query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('{table}') \
                     WHERE name = 'project_id')"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        if !has_column {
            continue;
        }
        let values: Vec<String> = conn
            .prepare(&format!("SELECT project_id FROM \"{table}\""))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        out.extend(values.into_iter().map(|v| (table.clone(), v)));
    }
    out
}

// --- hubs a self-update hands off ---------------------------------------------

/// Stops whatever hub serves a fixture home when dropped -- the one a
/// test started, or the process a self-update handed it off to (spawned
/// by the test's own run) -- with `fw hub stop --force`, then SIGKILLs
/// every pid it was told about and every pid listed in its pid files
/// (agents the test's processes started). Declare it after the
/// [`Spawned`] it complements so it runs first.
pub struct HubCleanup {
    program: PathBuf,
    home: PathBuf,
    flywheel_home: PathBuf,
    pid_files: Vec<PathBuf>,
    pids: Vec<u32>,
}

impl HubCleanup {
    pub fn new(program: &Path, home: &Path, flywheel_home: &Path, pid_files: &[&Path]) -> Self {
        Self {
            program: program.to_path_buf(),
            home: home.to_path_buf(),
            flywheel_home: flywheel_home.to_path_buf(),
            pid_files: pid_files.iter().map(|p| p.to_path_buf()).collect(),
            pids: Vec::new(),
        }
    }

    /// Kill `pid` (and what runs under it) at drop: a process the test's
    /// own processes started.
    pub fn track(&mut self, pid: u32) {
        self.pids.push(pid);
    }

    /// The pid `server.json` names now.
    pub fn hub_pid(&self) -> Option<u32> {
        server_pid(&self.flywheel_home)
    }
}

/// Whether `pid` runs with `FLYWHEEL_HOME=<flywheel_home>`.
pub fn of_home(pid: u32, flywheel_home: &Path) -> bool {
    let wanted = format!("FLYWHEEL_HOME={}", flywheel_home.display());
    std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .any(|kv| kv == wanted.as_bytes())
}

/// The pid this home's `server.json` names.
pub fn server_pid(flywheel_home: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(flywheel_home.join("hub/server.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["pid"].as_u64().and_then(|p| u32::try_from(p).ok())
}

impl Drop for HubCleanup {
    fn drop(&mut self) {
        let mut doomed: Vec<u32> = self.pids.clone();
        if let Some(hub) = self.hub_pid() {
            doomed.push(hub);
        }
        for pid in doomed.clone() {
            doomed.extend(descendants_of(pid));
        }
        let mut stop = Command::new(&self.program);
        stop.args(["hub", "stop", "--force", "--timeout", "10"])
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        confine(&mut stop, &self.home, &self.flywheel_home, &[]);
        let _ = stop.status();
        for file in &self.pid_files {
            let listed = std::fs::read_to_string(file).unwrap_or_default();
            // A listed pid may have been reused: only one still running
            // with this home in its environment is the test's.
            doomed.extend(
                listed
                    .lines()
                    .filter_map(|l| l.trim().parse::<u32>().ok())
                    .filter(|pid| of_home(*pid, &self.flywheel_home)),
            );
        }
        for pid in doomed.clone() {
            doomed.extend(descendants_of(pid));
        }
        for pid in doomed {
            signal(pid, libc::SIGCONT);
            signal(pid, libc::SIGKILL);
        }
    }
}
