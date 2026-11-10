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
