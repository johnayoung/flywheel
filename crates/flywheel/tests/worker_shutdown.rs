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

/// The session id of `pid` (`None` once it is gone).
fn session_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(3)?.parse().ok()
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

/// Whether `sig` is ignored (SIG_IGN) by `pid`, from `/proc/<pid>/status`.
/// The kernel discards an ignored signal when it is sent, so a signal
/// sent while this holds can have no later effect.
fn ignores(pid: i32, sig: libc::c_int) -> bool {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & (1 << (sig - 1)) != 0)
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
        self.spawn_worker_with(concurrency, false)
    }

    /// [`Fixture::spawn_worker`], optionally with SIGHUP ignored the way
    /// `nohup` leaves it (SIG_IGN survives exec).
    fn spawn_worker_with(&mut self, concurrency: u32, nohup: bool) -> Child {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let log = std::fs::File::create(self.root.join("worker.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            // The repo's flywheel.toml is legacy: name it explicitly.
            .args(["worker", "--policy", "flywheel.toml"])
            .args(["--concurrency", &concurrency.to_string()])
            .args(["--heartbeat", "1", "--poll-interval", "1"])
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.root)
            .env_remove("FLYWHEEL_HOME")
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        // SAFETY: setsid(2) is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if nohup && libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
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

    /// A reader on the worker's store. WAL readers rarely block, but one
    /// can still meet a transient lock (WAL recovery, the last writer's
    /// close-time checkpoint), so wait it out instead of failing the read.
    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.repo.join(".flywheel/flywheel.sqlite")).unwrap();
        db.busy_timeout(SHUTDOWN_DEADLINE).unwrap();
        db
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
    signal(worker.id() as i32, libc::SIGTERM);
    let status = wait_exit(&mut worker, SHUTDOWN_DEADLINE);
    assert!(
        status.is_some(),
        "the pool never exited\n{}",
        fx.worker_log()
    );
    fx.assert_clean_shutdown(&members);
    // The pool SIGKILLs (and logs) any member still alive when its stop
    // window closes; an orderly drain has every member exit on its own.
    let log = fx.worker_log();
    assert!(
        !log.contains("of SIGTERM; killing its group"),
        "members hung after SIGTERM until the kill escalation\n{log}"
    );
    assert_eq!(
        log.matches("in-flight task finalized to interrupted")
            .count(),
        2,
        "{log}"
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

/// `nohup fw worker --concurrency 2`: SIGHUP was ignored when the worker
/// started, so a terminal closing (SIGHUP to the supervisor's group, and to
/// each member directly) must not stop anything. SIGTERM still drains it.
#[test]
fn nohup_pool_ignores_sighup_and_still_drains_on_sigterm() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker_with(2, true);
    let members = fx.wait_in_flight(&worker, 2, 2);
    // SIGHUP stays ignored (no handler installed over the inherited
    // SIG_IGN), so the kernel drops the signals below on sending: once they
    // are sent, nothing can still be on its way to stop the pool.
    for pid in std::iter::once(worker.id() as i32).chain(members.iter().copied()) {
        assert!(
            ignores(pid, libc::SIGHUP),
            "{pid} does not ignore SIGHUP\n{}",
            fx.worker_log()
        );
    }
    signal(-(worker.id() as i32), libc::SIGHUP);
    for member in &members {
        signal(*member, libc::SIGHUP);
    }
    assert!(
        worker.try_wait().unwrap().is_none(),
        "the nohup'd pool supervisor stopped on SIGHUP\n{}",
        fx.worker_log()
    );
    let stopped: Vec<i32> = members
        .iter()
        .chain(&fx.agents())
        .copied()
        .filter(|pid| process_gone(*pid))
        .collect();
    assert_eq!(stopped, Vec::<i32>::new(), "{}", fx.worker_log());
    assert_eq!(fx.claims().len(), 2, "{}", fx.worker_log());
    signal(worker.id() as i32, libc::SIGTERM);
    assert!(wait_exit(&mut worker, SHUTDOWN_DEADLINE).is_some());
    fx.assert_clean_shutdown(&members);
}

/// `nohup fw worker` (a single worker): SIGHUP to its group is ignored;
/// SIGTERM is still an orderly stop.
#[test]
fn nohup_single_worker_ignores_sighup_and_still_stops_on_sigterm() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker_with(1, true);
    let members = fx.wait_in_flight(&worker, 1, 0);
    // As above: an ignored signal is dropped on sending.
    assert!(
        ignores(worker.id() as i32, libc::SIGHUP),
        "the worker does not ignore SIGHUP\n{}",
        fx.worker_log()
    );
    signal(-(worker.id() as i32), libc::SIGHUP);
    assert!(
        worker.try_wait().unwrap().is_none(),
        "the nohup'd worker stopped on SIGHUP\n{}",
        fx.worker_log()
    );
    assert!(fx.agents().iter().all(|pid| !process_gone(*pid)));
    assert_eq!(fx.claims().len(), 1, "{}", fx.worker_log());
    signal(worker.id() as i32, libc::SIGTERM);
    assert!(wait_exit(&mut worker, SHUTDOWN_DEADLINE).is_some());
    fx.assert_clean_shutdown(&members);
}

/// A pool member SIGKILLed mid-run (operator, OOM killer, the stop
/// window's escalation) runs no handler, so its agent group -- and the
/// agent's own children -- would run on as orphans in the task worktree.
/// The supervisor must kill them when it reaps the member, before anything
/// can re-claim the task; the pool still shuts down clean afterwards.
#[test]
fn sigkilled_pool_member_leaves_no_orphaned_agent() {
    let mut fx = Fixture::new();
    let mut worker = fx.spawn_worker(2);
    let members = fx.wait_in_flight(&worker, 2, 2);
    let victim = members[0];
    let orphans: Vec<i32> = fx
        .agents()
        .into_iter()
        .chain(fx.grandchildren())
        .filter(|pid| session_of(*pid) == Some(victim))
        .collect();
    assert_eq!(orphans.len(), 2, "agent + grandchild in {victim}'s session");
    signal(victim, libc::SIGKILL);
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    while Instant::now() < deadline && !orphans.iter().all(|pid| process_gone(*pid)) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survivors: Vec<i32> = orphans
        .iter()
        .copied()
        .filter(|pid| !process_gone(*pid))
        .collect();
    assert_eq!(
        survivors,
        Vec::<i32>::new(),
        "the SIGKILLed member's agent group outlived it\n{}",
        fx.worker_log()
    );
    assert!(worker.try_wait().unwrap().is_none(), "{}", fx.worker_log());
    // The SIGKILLed member never released its claim; its respawn (same
    // worker id) re-acquires that task straight away and runs a fresh
    // agent. Stop only once it has, so the shutdown below covers a pool
    // that is whole again rather than racing the respawn's startup.
    let respawned = {
        let pid = worker.id() as i32;
        let mut respawned = Vec::new();
        wait_until(
            "the respawned member to re-run its task",
            Duration::from_secs(60),
            || {
                respawned = children_of(pid)
                    .into_iter()
                    .filter(|child| !members.contains(child))
                    .collect();
                respawned.len() == 1
                    && fx.agents().len() >= 3
                    && fx.grandchildren().len() >= 3
                    && fx.claims().len() == 2
            },
        );
        respawned
    };
    fx.tracked.extend(&respawned);
    let late: Vec<i32> = fx.agents().into_iter().chain(fx.grandchildren()).collect();
    fx.tracked.extend(late);
    signal(worker.id() as i32, libc::SIGTERM);
    assert!(wait_exit(&mut worker, SHUTDOWN_DEADLINE).is_some());
    let mut every_member = members.clone();
    every_member.extend(respawned);
    fx.assert_clean_shutdown(&every_member);
}

impl Fixture {
    /// Put a `git` on the worker's PATH whose first run (the worker's repo
    /// lookup, before any handler of its own is armed) blocks reading
    /// `gate.fifo`; [`GitGate::reached`] returns once that run is parked
    /// there, [`GitGate::open`] lets it go on to the real git.
    fn install_git_gate(&self) -> GitGate {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let real = std::env::split_paths(&path)
            .map(|dir| dir.join("git"))
            .find(|git| git.is_file())
            .expect("git on PATH");
        let fifo = self.root.join("gate.fifo");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: mkfifo(3) on a NUL-terminated path this test owns.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        write_script(
            &self.root.join("bin").join("git"),
            &format!(
                "#!/bin/sh\n\
                 if mkdir '{taken}' 2>/dev/null; then read _ < '{fifo}'; fi\n\
                 exec '{real}' \"$@\"\n",
                taken = self.root.join("gate.taken").display(),
                fifo = fifo.display(),
                real = real.display(),
            ),
        );
        GitGate { fifo, writer: None }
    }
}

struct GitGate {
    fifo: PathBuf,
    writer: Option<std::fs::File>,
}

impl GitGate {
    /// Block until the gated git has opened the fifo (opening a fifo for
    /// writing waits for its reader).
    fn reached(&mut self) {
        let fifo = self.fifo.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(std::fs::OpenOptions::new().write(true).open(fifo));
        });
        let writer = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the worker never ran git")
            .unwrap();
        self.writer = Some(writer);
    }

    fn open(&mut self) {
        let mut writer = self.writer.take().expect("reached first");
        writer.write_all(b"go\n").unwrap();
    }
}

/// A stop signal while the worker is still starting -- here parked in its
/// first `git` run, before the policy loads or any store opens -- is an
/// orderly stop once it is up, not the default action: exit 0, nothing
/// claimed, no agent started.
#[test]
fn a_signal_during_startup_is_an_orderly_stop() {
    for (concurrency, said) in [
        (1, "Shutdown requested (signal 15 during startup)"),
        (
            2,
            "pool shutdown requested (signal 15) before any member started",
        ),
    ] {
        let mut fx = Fixture::new();
        let mut gate = fx.install_git_gate();
        let mut worker = fx.spawn_worker(concurrency);
        gate.reached();
        signal(worker.id() as i32, libc::SIGTERM);
        gate.open();
        let status = wait_exit(&mut worker, SHUTDOWN_DEADLINE);
        let log = fx.worker_log();
        let status = status.unwrap_or_else(|| panic!("the worker never exited\n{log}"));
        assert_eq!(status.code(), Some(0), "{status:?}\n{log}");
        assert!(log.contains(said), "{log}");
        assert_eq!(fx.agents(), Vec::<i32>::new(), "{log}");
        // The store may not even hold its tables yet: none is no claim.
        let has = |table: &str| {
            fx.repo.join(".flywheel/flywheel.sqlite").exists()
                && fx
                    .db()
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
                    == 1
        };
        if has("task_claims") {
            assert_eq!(fx.claims(), Vec::new(), "{log}");
        }
        if has("lifecycles") {
            assert_eq!(fx.lifecycle_statuses(), Vec::new(), "{log}");
        }
    }
}
