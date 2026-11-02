//! `fw worker` shutdown end to end through the real binary: a repo with two
//! queued tasks, a scripted `claude` stand-in that blocks (with a
//! grandchild in its process group), and the worker killed the way an
//! operator's environment kills it -- SIGHUP to its process group (a
//! terminal / PTY closing), SIGTERM, or the pool supervisor dying outright.
//! Every case asserts nothing survives (pool members, agents, their
//! children) and no claim or running lifecycle is stranded.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a shutdown may take before the test calls it hung. Well past
/// the agent kill grace; well short of the pool's SIGKILL escalation
/// window doubled.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

/// Write an executable script without this process ever holding a writable
/// descriptor to it (a parallel test's fork would copy it and exec would
/// fail with ETXTBSY).
fn write_script(path: &Path, contents: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Gone, or a zombie awaiting its reaper.
fn process_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.split_whitespace().next() == Some("Z")),
        Err(_) => true,
    }
}

/// Live (non-zombie) children of `parent`.
fn children_of(parent: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let mut fields = stat.rsplit(')').next().unwrap_or("").split_whitespace();
        let state = fields.next();
        let ppid = fields.next().and_then(|p| p.parse::<i32>().ok());
        if ppid == Some(parent) && state != Some("Z") {
            out.push(pid);
        }
    }
    out
}

fn pids_in(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn signal(pid: i32, sig: libc::c_int) {
    // SAFETY: plain kill(2) on a pid this test spawned (or a negative pgid
    // of a group it leads); a vanished target yields ESRCH.
    unsafe {
        libc::kill(pid, sig);
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A repo with two queued tasks and the scripted agent on PATH.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    /// Every pid this test learned about, SIGKILLed on drop so a failing
    /// assertion never leaks processes.
    tracked: Vec<i32>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // The agent: records its pid, parks a grandchild in its own process
        // group, and blocks until signalled.
        write_script(
            &bin.join("claude"),
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\n\
                 echo $$ >> '{agents}'\n\
                 sleep 300 &\n\
                 echo $! >> '{grandchildren}'\n\
                 wait\n",
                agents = root.join("agents.pids").display(),
                grandchildren = root.join("grandchildren.pids").display(),
            ),
        );
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &["config", "user.email", "worker-shutdown@example.com"],
        );
        git(&repo, &["config", "user.name", "worker shutdown"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        std::fs::write(
            repo.join("flywheel.toml"),
            "[source]\nkind = \"directory\"\n",
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let phase = repo.join(".flywheel/tasks/active/01-shutdown");
        std::fs::create_dir_all(&phase).unwrap();
        for id in ["task-a", "task-b"] {
            std::fs::write(
                phase.join(format!("{id}.json")),
                format!(
                    r#"{{"id": "{id}", "goal": "Goal {id}.", "graders": [{{"type": "command", "run": "true"}}]}}"#
                ),
            )
            .unwrap();
        }
        Self {
            _dir: dir,
            root,
            repo,
            tracked: Vec::new(),
        }
    }

    /// `fw worker --concurrency N` as the leader of a fresh session and
    /// process group -- the shape of a shell's foreground job on a PTY, so
    /// `kill(-pid, SIGHUP)` is exactly what the PTY closing delivers.
    fn spawn_worker(&mut self, concurrency: u32) -> Child {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let log = std::fs::File::create(self.root.join("worker.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .args(["worker", "--concurrency", &concurrency.to_string()])
            .args(["--heartbeat", "1", "--poll-interval", "1"])
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.root)
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        // SAFETY: setsid(2) is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        self.tracked.push(child.id() as i32);
        child
    }

    fn agents(&self) -> Vec<i32> {
        pids_in(&self.root.join("agents.pids"))
    }

    fn grandchildren(&self) -> Vec<i32> {
        pids_in(&self.root.join("grandchildren.pids"))
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.repo.join(".flywheel/flywheel.sqlite")).unwrap()
    }

    fn claims(&self) -> Vec<(String, String)> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT task_id, worker_id FROM task_claims ORDER BY task_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn lifecycle_statuses(&self) -> Vec<(String, String)> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT task_id, status FROM lifecycles ORDER BY task_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn worker_log(&self) -> String {
        let mut text = std::fs::read_to_string(self.root.join("worker.log")).unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(self.repo.join(".flywheel/logs/worker")) {
            for entry in entries.flatten() {
                text.push_str(&format!("\n== {}\n", entry.path().display()));
                text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        text
    }

    /// Wait until `tasks` agents are running and every task is claimed;
    /// returns the pool members (children of `worker`; empty for a single
    /// worker).
    fn wait_in_flight(&mut self, worker: &Child, tasks: usize, members: usize) -> Vec<i32> {
        let pid = worker.id() as i32;
        wait_until("agents in flight", Duration::from_secs(60), || {
            self.agents().len() >= tasks
                && self.grandchildren().len() >= tasks
                && self.claims().len() >= tasks
        });
        let members_now: Vec<i32> = children_of(pid)
            .into_iter()
            .filter(|child| {
                std::fs::read_to_string(format!("/proc/{child}/cmdline"))
                    .is_ok_and(|cmd| cmd.contains("worker"))
            })
            .collect();
        assert_eq!(members_now.len(), members, "{}", self.worker_log());
        self.tracked.extend(&members_now);
        let agents = self.agents();
        let grandchildren = self.grandchildren();
        self.tracked.extend(agents);
        self.tracked.extend(grandchildren);
        members_now
    }

    /// The post-shutdown invariants: every tracked process is gone, no
    /// claim is held, and no lifecycle is left running.
    fn assert_clean_shutdown(&self, members: &[i32]) {
        let agents = self.agents();
        let grandchildren = self.grandchildren();
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        let survivors = || -> Vec<i32> {
            members
                .iter()
                .chain(&agents)
                .chain(&grandchildren)
                .copied()
                .filter(|pid| !process_gone(*pid))
                .collect()
        };
        while Instant::now() < deadline && !survivors().is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            survivors().is_empty(),
            "survivors {:?} (members {members:?}, agents {agents:?}, grandchildren \
             {grandchildren:?})\n{}",
            survivors(),
            self.worker_log()
        );
        assert_eq!(self.claims(), Vec::new(), "{}", self.worker_log());
        let stranded: Vec<_> = self
            .lifecycle_statuses()
            .into_iter()
            .filter(|(_, status)| status == "running" || status == "validating")
            .collect();
        assert_eq!(stranded, Vec::new(), "{}", self.worker_log());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.tracked {
            signal(-*pid, libc::SIGKILL);
            signal(*pid, libc::SIGKILL);
        }
    }
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A PTY closing on `fw worker --concurrency 2` SIGHUPs the foreground
/// group -- the pool supervisor only; members run in sessions of their own.
/// The whole pool must still come down, agents with it, claims released.
#[test]
fn sighup_to_the_pool_group_stops_every_member_and_agent() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker(2);
    let members = fx.wait_in_flight(&worker, 2, 2);
    signal(-(worker.id() as i32), libc::SIGHUP);
    let status = wait_exit(&mut worker, SHUTDOWN_DEADLINE);
    assert!(
        status.is_some(),
        "the pool supervisor ignored SIGHUP\n{}",
        fx.worker_log()
    );
    fx.assert_clean_shutdown(&members);
}

/// SIGTERM drains the pool: every member finalizes its run and exits on
/// its own, well inside the supervisor's SIGKILL escalation window.
#[test]
fn sigterm_drains_the_pool_without_kill_escalation() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker(2);
    let members = fx.wait_in_flight(&worker, 2, 2);
    let started = Instant::now();
    signal(worker.id() as i32, libc::SIGTERM);
    let status = wait_exit(&mut worker, SHUTDOWN_DEADLINE);
    assert!(
        status.is_some(),
        "the pool never exited\n{}",
        fx.worker_log()
    );
    fx.assert_clean_shutdown(&members);
    // The pool's stop window is 10s before it SIGKILLs a member that has
    // not exited; an orderly member drain never gets near it.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "members hung after SIGTERM until the kill escalation ({:?})\n{}",
        started.elapsed(),
        fx.worker_log()
    );
    let interrupted = fx
        .lifecycle_statuses()
        .into_iter()
        .filter(|(_, status)| status == "interrupted")
        .count();
    assert_eq!(interrupted, 2, "{}", fx.worker_log());
}

/// The pool supervisor dying outright (SIGKILL: no handler runs) must not
/// orphan its members: each notices its supervisor is gone and shuts down
/// exactly as on SIGTERM.
#[test]
fn pool_members_shut_down_when_the_supervisor_dies() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker(2);
    let members = fx.wait_in_flight(&worker, 2, 2);
    signal(worker.id() as i32, libc::SIGKILL);
    assert!(wait_exit(&mut worker, SHUTDOWN_DEADLINE).is_some());
    fx.assert_clean_shutdown(&members);
}

/// A single worker on a PTY gets the SIGHUP itself: it must finalize its
/// in-flight run, sweep its agent's group, and release its claim instead
/// of dying on the default action.
#[test]
fn sighup_to_a_single_worker_releases_its_claim_and_agent() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker(1);
    let members = fx.wait_in_flight(&worker, 1, 0);
    signal(-(worker.id() as i32), libc::SIGHUP);
    let status = wait_exit(&mut worker, SHUTDOWN_DEADLINE);
    assert!(
        status.is_some(),
        "the worker never exited\n{}",
        fx.worker_log()
    );
    fx.assert_clean_shutdown(&members);
    assert_eq!(
        fx.lifecycle_statuses(),
        vec![("task-a".to_owned(), "interrupted".to_owned())],
        "{}",
        fx.worker_log()
    );
}
