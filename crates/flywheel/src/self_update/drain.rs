//! The drain step: find this home's `worker` / `autopilot` / `triage`
//! daemons from the process table, classify each, refuse while an
//! operator console of this home runs, and stop them with SIGTERM.
//!
//! Discovery reads Linux `/proc` for the invoking uid only. A process is
//! in the drain set when its argv[0] basename is `fw` or `flywheel`, its
//! first verb is `worker`, `autopilot`, or `triage`, its argv carries no
//! `--supervisor-fd` (a pool member is stopped by its supervisor), and the
//! `FLYWHEEL_HOME` in its environment (`~/.flywheel` when unset) resolves
//! to this home. The store cannot name worker processes (claims carry only
//! a random worker id), so no presence table or pidfile is consulted.
//!
//! Kinds: `hub-worker` / `hub-autopilot` when an ancestor is the session
//! wrapper (`/bin/sh -c SCRIPT fw-worker|fw-autopilot ...`) of a session
//! recorded in this home's hub state (`$FLYWHEEL_HOME/hub/state.json`),
//! `terminal` otherwise.
//!
//! Drain is SIGTERM only, never SIGKILL: an in-flight run finalizes
//! `interrupted` and releases its claim; a process still alive at the
//! timeout is reported, never killed.

use flywheel_hub::flywheel::{AUTOPILOT_MARKER, WORKER_MARKER};
use flywheel_orchestrator::home::{Home, home_from};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The daemon verbs a self-update stops.
pub const DAEMON_VERBS: [&str; 3] = ["worker", "autopilot", "triage"];
/// A pool member's lifeline flag: its supervisor stops it.
pub const SUPERVISOR_FD_FLAG: &str = "--supervisor-fd";
/// The hub session id every session process inherits.
pub const HUB_SESSION_ENV: &str = "FLYWHEEL_HUB_SESSION";
/// The hub's persisted state inside its state dir.
const HUB_STATE_FILE: &str = "state.json";
/// How often the drain re-checks its processes.
const POLL: Duration = Duration::from_millis(100);

/// What a drained process is, for the summary and the later restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    HubWorker,
    HubAutopilot,
    Terminal,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::HubWorker => "hub-worker",
            Self::HubAutopilot => "hub-autopilot",
            Self::Terminal => "terminal",
        }
    }
}

/// What an `fw` / `flywheel` argv is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A daemon of the drain set (the verb).
    Daemon(String),
    /// The operator console: no verb, or a leading-flag argv.
    Console,
    /// Anything else (another verb, a pool member, not flywheel at all).
    Other,
}

/// Whether `argv0`'s basename is `fw` or `flywheel`.
pub fn is_flywheel_program(argv0: &str) -> bool {
    matches!(
        Path::new(argv0).file_name().and_then(|n| n.to_str()),
        Some("fw" | "flywheel")
    )
}

/// Classify a full argv (argv[0] included).
pub fn role(argv: &[String]) -> Role {
    let Some(argv0) = argv.first() else {
        return Role::Other;
    };
    if !is_flywheel_program(argv0) {
        return Role::Other;
    }
    let Some(first) = argv.get(1) else {
        return Role::Console;
    };
    if first.starts_with('-') {
        // `--help` / `--version` print and exit; every other leading flag
        // is a console flag (`fw --json`, `fw --db PATH`).
        return if matches!(first.as_str(), "-h" | "--help" | "-V" | "--version") {
            Role::Other
        } else {
            Role::Console
        };
    }
    if !DAEMON_VERBS.contains(&first.as_str()) {
        return Role::Other;
    }
    let pool_member = argv[2..].iter().any(|a| {
        a == SUPERVISOR_FD_FLAG
            || a.strip_prefix(SUPERVISOR_FD_FLAG)
                .is_some_and(|rest| rest.starts_with('='))
    });
    if pool_member {
        Role::Other
    } else {
        Role::Daemon(first.clone())
    }
}

/// The hub session marker a wrapper argv carries (`/bin/sh -c SCRIPT
/// <marker> ...`), when it is a worker or autopilot wrapper.
pub fn wrapper_marker(argv: &[String]) -> Option<&'static str> {
    if argv.get(1).map(String::as_str) != Some("-c") {
        return None;
    }
    match argv.get(3).map(String::as_str) {
        Some(WORKER_MARKER) => Some(WORKER_MARKER),
        Some(AUTOPILOT_MARKER) => Some(AUTOPILOT_MARKER),
        _ => None,
    }
}

/// One process of the invoking uid, read from `/proc`.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Clock ticks since boot at which it started: with the pid, its
    /// identity (a pid alone can be reused).
    pub start: u64,
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

impl Proc {
    fn env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }
}

/// `(ppid, starttime, state)` from `/proc/<pid>/stat`.
fn stat_of(pid: u32) -> Option<(u32, u64, char)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the comm: [0] state (field 3), [1] ppid (4), [19]
    // starttime (22).
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start, state))
}

/// The real uid in `/proc/<pid>/status`.
fn uid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

fn nul_split(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

fn read_proc(pid: u32, uid: u32) -> Option<Proc> {
    if uid_of(pid)? != uid {
        return None;
    }
    let (ppid, start, state) = stat_of(pid)?;
    if state == 'Z' {
        return None;
    }
    let argv = nul_split(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
    if argv.is_empty() {
        return None;
    }
    let env = std::fs::read(format!("/proc/{pid}/environ"))
        .map(|bytes| {
            nul_split(&bytes)
                .into_iter()
                .filter_map(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    Some(Proc {
        pid,
        ppid,
        start,
        argv,
        env,
        cwd,
    })
}

/// The invoking uid.
pub fn current_uid() -> u32 {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Every readable process of `uid` (an empty table where `/proc` is
/// absent).
pub fn scan(uid: u32) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let me = std::process::id();
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter(|pid| *pid != me)
        .filter_map(|pid| read_proc(pid, uid))
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The flywheel home a process resolves: its `FLYWHEEL_HOME`, else
/// `~/.flywheel` (its own `HOME`, else `fallback_home`).
pub fn home_of(process: &Proc, fallback_home: Option<&str>) -> Option<PathBuf> {
    let cwd = process.cwd.clone().unwrap_or_default();
    let home = process.env("HOME").or(fallback_home);
    home_from(
        process.env(flywheel_orchestrator::home::HOME_ENV),
        home,
        &cwd,
    )
    .map(|p| canonical(&p))
}

/// The hub sessions recorded in `hub_dir`'s state: session id -> the
/// daemon marker its wrapper command carries.
pub fn hub_sessions(hub_dir: &Path) -> HashMap<String, &'static str> {
    fn walk(value: &Value, out: &mut HashMap<String, &'static str>) {
        match value {
            Value::Object(map) => {
                if let (Some(Value::String(id)), Some(Value::Array(command))) =
                    (map.get("id"), map.get("command"))
                {
                    let argv: Vec<String> = command
                        .iter()
                        .filter_map(|a| a.as_str().map(str::to_owned))
                        .collect();
                    if let Some(marker) = wrapper_marker(&argv) {
                        out.insert(id.clone(), marker);
                    }
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(hub_dir.join(HUB_STATE_FILE))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
    {
        walk(&value, &mut out);
    }
    out
}

/// The pid of the hub serving `hub_dir`, read without writing anything:
/// its runtime file, trusted only while its serve lock is held.
pub fn hub_pid(hub_dir: &Path) -> Option<u32> {
    let info = flywheel_hub::remote::RuntimeInfo::read(hub_dir).ok()??;
    let lock = std::fs::File::open(hub_dir.join(flywheel_hub::remote::LOCK_FILE)).ok()?;
    match lock.try_lock_shared() {
        // Nobody holds it: the runtime file is stale.
        Ok(()) => {
            let _ = lock.unlock();
            None
        }
        Err(std::fs::TryLockError::WouldBlock) => Some(info.pid),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// One process of the drain set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drainable {
    pub pid: u32,
    pub start: u64,
    pub kind: Kind,
    pub verb: String,
    pub cwd: Option<PathBuf>,
    pub project_id: Option<String>,
    /// The hub session it runs in (hub kinds only).
    pub session_id: Option<String>,
}

impl Drainable {
    pub fn cwd_display(&self) -> String {
        self.cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// `{pid, kind, verb, cwd, project_id|null}`.
    pub fn to_json(&self) -> Value {
        json!({
            "pid": self.pid,
            "kind": self.kind.name(),
            "verb": self.verb,
            "cwd": self.cwd.as_ref().map(|p| p.display().to_string()),
            "project_id": self.project_id,
        })
    }
}

/// Discovery's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// The drain set, by pid.
    Drain(Vec<Drainable>),
    /// An operator console of this home runs (its pid): refuse.
    Console(u32),
}

/// Classify `table` (processes of the invoking uid) against `home`.
/// `fallback_home` is the invoking `HOME`, for processes without one.
pub fn classify(table: &[Proc], home: &Home, fallback_home: Option<&str>) -> Discovery {
    let this_home = canonical(home.root());
    let by_pid: BTreeMap<u32, &Proc> = table.iter().map(|p| (p.pid, p)).collect();
    let sessions = hub_sessions(&home.hub_dir());
    let mut drain = Vec::new();
    let mut consoles = BTreeSet::new();
    for process in by_pid.values() {
        let role = role(&process.argv);
        if role == Role::Other {
            continue;
        }
        if home_of(process, fallback_home).as_deref() != Some(this_home.as_path()) {
            continue;
        }
        let verb = match role {
            Role::Daemon(verb) => verb,
            Role::Console => {
                consoles.insert(process.pid);
                continue;
            }
            Role::Other => continue,
        };
        let (kind, session_id) = hub_session_of(process, &by_pid, &sessions);
        let project_id = process.cwd.as_deref().and_then(|cwd| {
            flywheel_orchestrator::home::stored_project_id(cwd)
                .ok()
                .flatten()
        });
        drain.push(Drainable {
            pid: process.pid,
            start: process.start,
            kind,
            verb,
            cwd: process.cwd.clone(),
            project_id,
            session_id,
        });
    }
    match consoles.first() {
        Some(pid) => Discovery::Console(*pid),
        None => Discovery::Drain(drain),
    }
}

/// The kind of a daemon: a hub kind when an ancestor is a worker or
/// autopilot wrapper of a session this home's hub records.
fn hub_session_of(
    process: &Proc,
    by_pid: &BTreeMap<u32, &Proc>,
    sessions: &HashMap<String, &'static str>,
) -> (Kind, Option<String>) {
    let mut seen = BTreeSet::new();
    let mut next = process.ppid;
    while next > 1 && seen.insert(next) {
        let Some(ancestor) = by_pid.get(&next) else {
            break;
        };
        if let Some(marker) = wrapper_marker(&ancestor.argv)
            && let Some(id) = ancestor.env(HUB_SESSION_ENV)
            && sessions.get(id) == Some(&marker)
        {
            let kind = if marker == WORKER_MARKER {
                Kind::HubWorker
            } else {
                Kind::HubAutopilot
            };
            return (kind, Some(id.to_owned()));
        }
        next = ancestor.ppid;
    }
    (Kind::Terminal, None)
}

/// Scan `/proc` and classify against `home`.
pub fn discover(home: &Home) -> Discovery {
    let fallback = std::env::var("HOME").ok();
    classify(&scan(current_uid()), home, fallback.as_deref())
}

/// Whether the process `pid` that started at `start` is gone (exited, a
/// zombie awaiting its reaper, or the pid reused).
pub fn is_gone(pid: u32, start: u64) -> bool {
    match stat_of(pid) {
        Some((_, now_start, state)) => state == 'Z' || now_start != start,
        None => true,
    }
}

/// The drain's result.
#[derive(Debug, Clone, Default)]
pub struct Drained {
    /// Processes that exited after SIGTERM.
    pub stopped: Vec<Drainable>,
    /// Processes still alive at the timeout.
    pub not_stopped: Vec<Drainable>,
}

/// SIGTERM every process of `set` (each checked to still be the process
/// discovery saw), then wait up to `timeout` for all of them to exit.
pub fn drain(set: &[Drainable], timeout: Duration) -> Drained {
    for process in set {
        if is_gone(process.pid, process.start) {
            continue;
        }
        let Ok(pid) = libc::pid_t::try_from(process.pid) else {
            continue;
        };
        // SAFETY: kill(2) on a pid just verified (by start time) to be the
        // process discovery classified; SIGTERM only, never SIGKILL.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        let alive: Vec<&Drainable> = set.iter().filter(|p| !is_gone(p.pid, p.start)).collect();
        if alive.is_empty() || Instant::now() >= deadline {
            let (not_stopped, stopped) = set
                .iter()
                .cloned()
                .partition(|p| alive.iter().any(|a| a.pid == p.pid));
            return Drained {
                stopped,
                not_stopped,
            };
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Live leases (`lease_expires_at > now`) in every SQLite project store of
/// `home`: `{project_id, task_id, worker_id, lease_expires_at}`. Read
/// only; a store that cannot be read is skipped.
pub fn live_claims(home: &Home) -> Vec<Value> {
    let Ok(projects) = flywheel_orchestrator::home::list_projects(home) else {
        return Vec::new();
    };
    let now = chrono::Utc::now();
    let mut out = Vec::new();
    for project in projects {
        let db = home.project(&project.id).db();
        if !db.is_file() {
            continue;
        }
        for (task_id, worker_id, lease) in claims_in(&db, &project.id) {
            if parse_time(&lease).is_some_and(|at| at > now) {
                out.push(json!({
                    "project_id": project.id,
                    "task_id": task_id,
                    "worker_id": worker_id,
                    "lease_expires_at": lease,
                }));
            }
        }
    }
    out
}

fn claims_in(db: &Path, project_id: &str) -> Vec<(String, String, String)> {
    use rusqlite::{Connection, OpenFlags};
    let Ok(conn) = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let query = |sql: &str, bind: bool| -> rusqlite::Result<Vec<(String, String, String)>> {
        let mut stmt = conn.prepare(sql)?;
        let map = |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?, row.get(2)?));
        let rows = if bind {
            stmt.query_map([project_id], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    };
    query(
        "SELECT task_id, worker_id, lease_expires_at FROM task_claims \
         WHERE project_id = ?1 ORDER BY task_id",
        true,
    )
    .or_else(|_| {
        // A store from before partitioning has no project_id column.
        query(
            "SELECT task_id, worker_id, lease_expires_at FROM task_claims ORDER BY task_id",
            false,
        )
    })
    .unwrap_or_default()
}

fn parse_time(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(at.with_timezone(&chrono::Utc));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(raw, f).ok())
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn proc(pid: u32, ppid: u32, args: &[&str], env: &[(&str, &str)], cwd: &str) -> Proc {
        Proc {
            pid,
            ppid,
            start: 1,
            argv: argv(args),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            cwd: Some(PathBuf::from(cwd)),
        }
    }

    #[test]
    fn roles_follow_program_name_verb_and_pool_flag() {
        let daemon = |v: &str| Role::Daemon(v.to_owned());
        assert_eq!(role(&argv(&["/x/fw", "worker"])), daemon("worker"));
        assert_eq!(
            role(&argv(&["flywheel", "autopilot", "--once"])),
            daemon("autopilot")
        );
        assert_eq!(role(&argv(&["fw", "triage"])), daemon("triage"));
        assert_eq!(
            role(&argv(&["fw", "worker", "--supervisor-fd", "7"])),
            Role::Other
        );
        assert_eq!(
            role(&argv(&["fw", "worker", "--supervisor-fd=7"])),
            Role::Other
        );
        assert_eq!(role(&argv(&["fw", "status"])), Role::Other);
        assert_eq!(role(&argv(&["fw", "self-update"])), Role::Other);
        assert_eq!(role(&argv(&["fw", "--version"])), Role::Other);
        assert_eq!(role(&argv(&["/bin/fwx", "worker"])), Role::Other);
        assert_eq!(role(&argv(&["fw-worker", "worker"])), Role::Other);
        assert_eq!(role(&argv(&["/usr/bin/fw"])), Role::Console);
        assert_eq!(role(&argv(&["fw", "--json"])), Role::Console);
        assert_eq!(role(&[]), Role::Other);
    }

    #[test]
    fn wrapper_markers_are_read_from_the_fourth_argument() {
        assert_eq!(
            wrapper_marker(&argv(&["/bin/sh", "-c", "s", "fw-worker", "fw", "worker"])),
            Some(WORKER_MARKER)
        );
        assert_eq!(
            wrapper_marker(&argv(&["/bin/sh", "-c", "s", "fw-autopilot", "fw"])),
            Some(AUTOPILOT_MARKER)
        );
        assert_eq!(
            wrapper_marker(&argv(&["/bin/sh", "-c", "s", "fw-reland"])),
            None
        );
        assert_eq!(wrapper_marker(&argv(&["/bin/sh", "-i"])), None);
    }

    #[test]
    fn classify_keeps_this_home_only_and_kinds_from_hub_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = Home::at(root.join("fh"));
        std::fs::create_dir_all(home.hub_dir()).unwrap();
        std::fs::write(
            home.hub_dir().join(HUB_STATE_FILE),
            json!({"sessions": [
                {"id": "s-work", "command": ["/bin/sh", "-c", "x", "fw-worker", "fw", "worker"]},
                {"id": "s-auto", "command": ["/bin/sh", "-c", "x", "fw-autopilot", "fw", "autopilot"]},
            ]})
            .to_string(),
        )
        .unwrap();
        let fh = home.root().to_str().unwrap().to_owned();
        let other = root.join("other").to_str().unwrap().to_owned();
        let user = root.to_str().unwrap().to_owned();
        let table = vec![
            proc(
                10,
                1,
                &["/bin/sh", "-c", "x", "fw-worker", "fw", "worker"],
                &[("FLYWHEEL_HOME", &fh), (HUB_SESSION_ENV, "s-work")],
                "/r",
            ),
            proc(11, 10, &["fw", "worker"], &[("FLYWHEEL_HOME", &fh)], "/r"),
            proc(
                20,
                1,
                &["/bin/sh", "-c", "x", "fw-autopilot", "fw", "autopilot"],
                &[("FLYWHEEL_HOME", &fh), (HUB_SESSION_ENV, "s-auto")],
                "/r",
            ),
            proc(
                21,
                20,
                &["fw", "autopilot"],
                &[("FLYWHEEL_HOME", &fh)],
                "/r",
            ),
            // A wrapper of a session this hub does not record.
            proc(
                30,
                1,
                &["/bin/sh", "-c", "x", "fw-worker", "fw", "worker"],
                &[("FLYWHEEL_HOME", &fh), (HUB_SESSION_ENV, "s-unknown")],
                "/r",
            ),
            proc(31, 30, &["fw", "worker"], &[("FLYWHEEL_HOME", &fh)], "/r"),
            proc(
                40,
                1,
                &["flywheel", "triage"],
                &[("FLYWHEEL_HOME", &fh)],
                "/r",
            ),
            // Another home, and a pool member.
            proc(50, 1, &["fw", "worker"], &[("FLYWHEEL_HOME", &other)], "/r"),
            proc(
                60,
                40,
                &["fw", "worker", "--supervisor-fd", "3"],
                &[("FLYWHEEL_HOME", &fh)],
                "/r",
            ),
            // Unset FLYWHEEL_HOME: ~/.flywheel, not this home.
            proc(70, 1, &["fw", "worker"], &[("HOME", &user)], "/r"),
        ];
        let Discovery::Drain(set) = classify(&table, &home, None) else {
            panic!("no console runs");
        };
        let kinds: Vec<(u32, &str, &str)> = set
            .iter()
            .map(|d| (d.pid, d.kind.name(), d.verb.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (11, "hub-worker", "worker"),
                (21, "hub-autopilot", "autopilot"),
                (31, "terminal", "worker"),
                (40, "terminal", "triage"),
            ]
        );
        // With HOME pointing at a parent whose .flywheel is this home, an
        // unset FLYWHEEL_HOME matches.
        let dotted = Home::at(root.join(".flywheel"));
        let Discovery::Drain(set) = classify(&table, &dotted, None) else {
            panic!("no console runs");
        };
        assert_eq!(set.iter().map(|d| d.pid).collect::<Vec<_>>(), [70]);

        let mut with_console = table.clone();
        with_console.push(proc(80, 1, &["fw"], &[("FLYWHEEL_HOME", &fh)], "/r"));
        with_console.push(proc(81, 1, &["fw"], &[("FLYWHEEL_HOME", &other)], "/r"));
        assert_eq!(classify(&with_console, &home, None), Discovery::Console(80));
    }

    #[test]
    fn parse_time_accepts_the_store_formats() {
        assert!(parse_time("2026-05-28T12:00:30+00:00").is_some());
        assert!(parse_time("2026-05-28T12:00:30.123456+00:00").is_some());
        assert!(parse_time("2026-05-28T12:00:30").is_some());
        assert!(parse_time("soon").is_none());
    }
}
