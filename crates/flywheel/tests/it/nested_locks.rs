//! `scripts/check.sh`'s test-suite lock, run for real (with a stand-in
//! `cargo`) in a scratch copy: a run nested inside one that holds the lock
//! fails at once naming its ancestor, and a wait for another holder is
//! bounded and names it. Unix only (bash, flock(1), /proc).
#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FW: &str = env!("CARGO_BIN_EXE_fw");

struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
    lock: PathBuf,
}

fn scratch() -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("repo");
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    for name in ["check.sh", "run_silent.sh"] {
        std::fs::copy(scripts.join(name), repo.join("scripts").join(name)).unwrap();
    }
    // A cargo that passes every gate, and whose test run runs $NESTED.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\n\
         if [ \"$1\" = nextest ]; then exit 1; fi\n\
         if [ \"$1 $2 $3\" = \"test --workspace --no-fail-fast\" ]; then exec sh -c \"$NESTED\"; fi\n\
         exit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    let lock = root.join("run/flywheel-check.lock");
    Scratch {
        _dir: dir,
        root,
        repo,
        bin,
        lock,
    }
}

/// Run the scratch check.sh with `nested` as its test run; `with_fw`: an
/// `fw` on PATH (the hub lease path, no hub answering). Its exit code and
/// output, failing the test if it runs past `limit`.
fn check(
    s: &Scratch,
    nested: &str,
    with_fw: bool,
    env: &[(&str, &str)],
    limit: Duration,
) -> (i32, String) {
    let mut path = s.bin.display().to_string();
    if with_fw {
        let fw_dir = s.root.join("fwbin");
        std::fs::create_dir_all(&fw_dir).unwrap();
        let link = fw_dir.join("fw");
        if !link.exists() {
            std::os::unix::fs::symlink(FW, &link).unwrap();
        }
        path = format!("{}:{path}", fw_dir.display());
    }
    path.push_str(":/usr/bin:/bin");
    let out_path = s.root.join("out.txt");
    let out = std::fs::File::create(&out_path).unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg(s.repo.join("scripts/check.sh"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", s.root.join("home"))
        .env("FLYWHEEL_HOME", s.root.join("home/.flywheel"))
        .env("XDG_RUNTIME_DIR", s.root.join("run"))
        .env("NESTED", nested)
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let started = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code().unwrap_or(-1);
        }
        if started.elapsed() > limit {
            // Its whole process group: the nested run is a grandchild.
            let _ = Command::new("pkill")
                .args(["-KILL", "-f", &s.root.display().to_string()])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "check.sh still ran after {limit:?}:\n{}",
                std::fs::read_to_string(&out_path).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    (code, std::fs::read_to_string(&out_path).unwrap_or_default())
}

#[test]
fn a_run_nested_in_the_check_lock_fails_at_once_naming_its_ancestor() {
    let s = scratch();
    let lock = s.lock.display().to_string();
    let inner_lease = format!(
        "{FW} hub lease run cpu:test-suite --state-dir {}/nohub --or-flock {lock} -- true",
        s.root.display()
    );
    let inner_check = format!(
        "env -u FLYWHEEL_CHECK_PHASE bash {}/scripts/check.sh",
        s.repo.display()
    );
    for (with_fw, nested) in [
        // check.sh holds flock(1) itself; a tool its tests run asks for it.
        (false, inner_lease.as_str()),
        // ... and a whole check.sh run from inside its tests.
        (false, inner_check.as_str()),
        // check.sh's turn through `fw hub lease run --or-flock` (no hub).
        (true, inner_lease.as_str()),
        (true, inner_check.as_str()),
    ] {
        let (code, out) = check(&s, nested, with_fw, &[], Duration::from_secs(60));
        assert_ne!(code, 0, "with_fw {with_fw} {nested}: {out}");
        assert!(
            out.contains("already held by your ancestor"),
            "with_fw {with_fw} {nested}: {out}"
        );
    }
}

#[test]
fn a_wait_for_another_holder_is_bounded_and_names_it() {
    let s = scratch();
    // Held by a process beside this one's (an ancestor's hold would be
    // re-entrant): flock(1) holding it while it sleeps.
    use std::os::unix::process::CommandExt;
    let mut other = Command::new("flock")
        .arg(&s.lock)
        .args(["sleep", "120"])
        .process_group(0)
        .spawn()
        .unwrap();
    let probe = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.lock)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // SAFETY: flock on a file this test owns.
    while unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
        assert!(Instant::now() < deadline, "flock(1) never took it");
        std::thread::sleep(Duration::from_millis(50));
    }
    let me = other.id().to_string();
    for with_fw in [false, true] {
        let started = Instant::now();
        let (code, out) = check(
            &s,
            "true",
            with_fw,
            &[("FLYWHEEL_CHECK_LOCK_WAIT", "2")],
            Duration::from_secs(60),
        );
        assert_ne!(code, 0, "with_fw {with_fw}: {out}");
        assert!(out.contains("waited 2s"), "with_fw {with_fw}: {out}");
        assert!(
            out.contains(&format!("pid {me}")),
            "with_fw {with_fw}: {out}"
        );
        assert!(started.elapsed() < Duration::from_secs(30));
    }
    // SAFETY: signals the process group this test started.
    unsafe { libc::kill(-(other.id() as i32), libc::SIGKILL) };
    let _ = other.wait();
    let (code, out) = check(&s, "true", false, &[], Duration::from_secs(60));
    assert_eq!(code, 0, "{out}");
}
