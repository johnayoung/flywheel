//! The build budget: one machine-wide compile-job pool every session the
//! hub starts shares (`[build_budget]` in `hub.toml`, `fw hub budget`).
//!
//! The pool is a GNU make jobserver on a named FIFO under the state dir,
//! filled with `jobs - 1` tokens. Every session gets `MAKEFLAGS` and
//! `CARGO_MAKEFLAGS` naming it, so Cargo, `cc`, and `make` take a token
//! per job instead of assuming they own the machine; each building cargo
//! also holds one implicit slot of its own, so the ceiling is `jobs - 1`
//! plus the cargo processes building at that moment. Test binaries are
//! outside any jobserver: `RUST_TEST_THREADS` / `NEXTEST_TEST_THREADS`
//! cap them per session. A variable already in the hub's environment
//! wins; the jobserver pair goes together.
//!
//! A killed `rustc` takes its token with it. Whenever no process but the
//! hub has the FIFO open, the hub drains it and refills it to exactly
//! `jobs - 1` ([`BuildBudget::tick`]).
//!
//! Beside the pool the hub reads how loaded the machine is ([`pressure`]):
//! a CPU budget cannot fix an I/O or memory bottleneck, so the readout
//! names which one it is. A sample of both is recorded into telemetry
//! (`budget.sample`, at most every [`SAMPLE_EVERY`] and only when it
//! moved) and so forwarded to a collector with the rest.

pub mod pressure;

use pressure::{Constraint, Pressure, Sampler};
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The FIFO's name in the state dir.
pub const FIFO_FILE: &str = "build-jobserver.fifo";
/// How often the hub looks at the pool and the machine.
pub const TICK: Duration = Duration::from_secs(2);
/// The least time between two recorded samples.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(30);
/// The hub event a sample is recorded as.
pub const SAMPLE_EVENT: &str = "budget.sample";
/// Upper bound of `build_budget.jobs` and `build_budget.test_threads`.
pub const MAX_JOBS: usize = 4096;
/// The variables a session gets, in the order they are set.
pub const VARS: [&str; 4] = [
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "RUST_TEST_THREADS",
    "NEXTEST_TEST_THREADS",
];
/// The token byte written into the pool (GNU make's).
const TOKEN: u8 = b'+';

/// `[build_budget]` resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BudgetConfig {
    pub enabled: bool,
    /// The pool's size: `jobs - 1` tokens plus each cargo's own slot.
    pub jobs: usize,
    /// `RUST_TEST_THREADS` / `NEXTEST_TEST_THREADS` per session.
    pub test_threads: usize,
}

impl BudgetConfig {
    /// The defaults for a machine of `threads` hardware threads: `threads
    /// - 2` jobs (at least 1), `max(2, threads / 4)` test threads.
    pub fn for_threads(threads: usize) -> Self {
        Self {
            enabled: true,
            jobs: default_jobs(threads),
            test_threads: default_test_threads(threads),
        }
    }

    /// Off: sessions see the environment they would without the feature.
    pub fn off() -> Self {
        Self {
            enabled: false,
            ..Self::for_threads(threads())
        }
    }
}

/// This machine's hardware threads (as the process may use them).
pub fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

pub fn default_jobs(threads: usize) -> usize {
    threads.saturating_sub(2).max(1)
}

pub fn default_test_threads(threads: usize) -> usize {
    (threads / 4).max(2)
}

/// The variables the budget adds to a session's environment: each of
/// `wanted` unless the session already gets it -- from the hub's own
/// environment (`inherited`), or set explicitly (`explicit`). The
/// jobserver pair goes together: either one inherited keeps both out, so
/// a session never mixes two pools.
pub fn session_env(
    wanted: &[(String, String)],
    inherited: impl Fn(&str) -> bool,
    explicit: &[(String, String)],
) -> Vec<(String, String)> {
    let present = |key: &str| inherited(key) || explicit.iter().any(|(k, _)| k == key);
    let pair_taken = present("MAKEFLAGS") || present("CARGO_MAKEFLAGS");
    wanted
        .iter()
        .filter(|(key, _)| {
            let jobserver = key == "MAKEFLAGS" || key == "CARGO_MAKEFLAGS";
            if jobserver {
                !pair_taken
            } else {
                !present(key)
            }
        })
        .cloned()
        .collect()
}

/// The jobserver FIFO: created at the hub's start, kept open read-write by
/// the hub (so it never reads end-of-file), never consumed from except to
/// reset it.
pub struct Pool {
    path: PathBuf,
    jobs: usize,
    file: std::fs::File,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("path", &self.path)
            .field("jobs", &self.jobs)
            .finish()
    }
}

impl Pool {
    /// Open (making as needed) the FIFO at `path` for a pool of `jobs`.
    /// It is filled only when no other process holds it: sessions a live
    /// upgrade kept may be building from it.
    #[cfg(unix)]
    pub fn open(path: &Path, jobs: usize) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_fifo() => {}
            Ok(_) => std::fs::remove_file(path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if !path.exists() {
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            // SAFETY: a valid NUL-terminated path and a mode.
            if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let path = std::fs::canonicalize(path)?;
        let pool = Self {
            path,
            jobs: jobs.max(1),
            file,
        };
        if holders(&pool.path, std::process::id()).is_some_and(|h| h.is_empty()) {
            pool.refill()?;
        }
        Ok(pool)
    }

    #[cfg(not(unix))]
    pub fn open(_path: &Path, _jobs: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a jobserver FIFO needs a Unix system",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn jobs(&self) -> usize {
        self.jobs
    }

    /// The tokens a full pool holds.
    pub fn tokens(&self) -> usize {
        self.jobs - 1
    }

    /// `-j<jobs> --jobserver-auth=fifo:<path>`.
    pub fn flags(&self) -> String {
        format!(
            "-j{} --jobserver-auth=fifo:{}",
            self.jobs,
            self.path.display()
        )
    }

    /// Tokens in the FIFO now, read without taking any (`FIONREAD`).
    #[cfg(unix)]
    pub fn free(&self) -> io::Result<usize> {
        use std::os::fd::AsRawFd;
        let mut n: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int through the pointer.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::FIONREAD, &mut n) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(usize::try_from(n).unwrap_or(0))
    }

    #[cfg(not(unix))]
    pub fn free(&self) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no FIFO"))
    }

    /// Drain the FIFO and put exactly [`Self::tokens`] back.
    pub fn refill(&self) -> io::Result<()> {
        use std::io::{Read, Write};
        let mut file = &self.file;
        let mut sink = [0u8; 4096];
        loop {
            match file.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        file.write_all(&vec![TOKEN; self.tokens()])
    }
}

/// A process other than the hub with the pool's FIFO open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub pid: u32,
    /// Its command name (`cargo`, `rustc`, `make`, ...).
    pub comm: String,
    /// The hub session it runs in (its `FLYWHEEL_HUB_SESSION`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Every process but `except` with `fifo` open, read from `/proc`; `None`
/// where there is no `/proc` (holders unknown: the pool is never reset).
pub fn holders(fifo: &Path, except: u32) -> Option<Vec<Holder>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == except {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let open = fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|to| to == fifo));
        if !open {
            continue;
        }
        let comm = std::fs::read_to_string(entry.path().join("comm"))
            .map(|c| c.trim_end().to_owned())
            .unwrap_or_default();
        found.push(Holder {
            pid,
            comm,
            session: session_of(pid),
        });
    }
    found.sort_by_key(|h| h.pid);
    Some(found)
}

/// The hub session process `pid` runs in, from its environment.
fn session_of(pid: u32) -> Option<String> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{}=", crate::report::SESSION_ENV);
    env.split(|b| *b == 0)
        .find_map(|var| var.strip_prefix(prefix.as_bytes()))
        .map(|id| String::from_utf8_lossy(id).into_owned())
        .filter(|id| !id.is_empty())
}

/// One session's share of the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionUse {
    /// `None`: processes outside any hub session (another tool, a
    /// container a worker started).
    pub session: Option<String>,
    /// Filled in by the hub from its records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cargo: usize,
    pub rustc: usize,
    /// Every other process holding the pool (`make`, `cc`, build scripts).
    pub other: usize,
}

/// The pool now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolReading {
    pub path: PathBuf,
    pub jobs: usize,
    pub tokens: usize,
    /// Tokens in the FIFO.
    pub free: usize,
    /// Tokens taken plus one implicit slot per building cargo.
    pub in_use: usize,
    /// Cargo processes holding the pool (each has one implicit slot).
    pub cargo: usize,
    pub rustc: usize,
    /// The most jobs that can run now: tokens plus each cargo's slot.
    pub ceiling: usize,
    /// `None`: holders cannot be read here (no `/proc`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionUse>>,
}

/// Fold `free` tokens and `holders` into a reading of `pool`.
pub fn read_pool(pool: &Pool, free: usize, holders: Option<&[Holder]>) -> PoolReading {
    let tokens = pool.tokens();
    let taken = tokens.saturating_sub(free);
    let count = |name: &str| holders.map_or(0, |h| h.iter().filter(|h| h.comm == name).count());
    let cargo = count("cargo");
    let sessions = holders.map(|holders| {
        let mut by: Vec<SessionUse> = Vec::new();
        for holder in holders {
            let at = match by.iter().position(|u| u.session == holder.session) {
                Some(at) => at,
                None => {
                    by.push(SessionUse {
                        session: holder.session.clone(),
                        name: None,
                        cargo: 0,
                        rustc: 0,
                        other: 0,
                    });
                    by.len() - 1
                }
            };
            match holder.comm.as_str() {
                "cargo" => by[at].cargo += 1,
                "rustc" => by[at].rustc += 1,
                _ => by[at].other += 1,
            }
        }
        by
    });
    PoolReading {
        path: pool.path().to_owned(),
        jobs: pool.jobs(),
        tokens,
        free,
        in_use: taken + cargo,
        cargo,
        rustc: count("rustc"),
        ceiling: tokens + cargo,
        sessions,
    }
}

/// What `budget.sample` carries, and what `GET /api/budget` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    pub enabled: bool,
    pub jobs: usize,
    pub test_threads: usize,
    pub threads: usize,
    /// `None` with the budget off, or when the FIFO could not be made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolReading>,
    /// `None` where it cannot be read (not Linux), or before the first
    /// window has passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<Pressure>,
    pub constraint: Constraint,
}

/// A reading without the per-session breakdown, as recorded and
/// forwarded (sessions are this hub's own business).
pub fn sample_body(reading: &Reading) -> serde_json::Value {
    let mut reading = reading.clone();
    if let Some(pool) = &mut reading.pool {
        pool.sessions = None;
    }
    serde_json::to_value(&reading).unwrap_or(serde_json::Value::Null)
}

/// What decides whether a sample says something new: the pool's numbers
/// and the constraint exactly, the gauges in coarse steps (10 points of
/// CPU or I/O wait, a GiB of memory, swapping or not).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    enabled: bool,
    pool: Option<(usize, usize, usize)>,
    constraint: Constraint,
    gauges: Option<(u32, u32, u64, bool)>,
}

fn shape(reading: &Reading) -> Shape {
    Shape {
        enabled: reading.enabled,
        pool: reading.pool.as_ref().map(|p| (p.jobs, p.in_use, p.cargo)),
        constraint: reading.constraint,
        gauges: reading.pressure.as_ref().map(|p| {
            (
                (p.cpu_busy_pct / 10.0).round() as u32,
                (p.iowait_pct / 10.0).round() as u32,
                p.mem_available_bytes >> 30,
                p.swap_out_pages_per_sec > 0.0,
            )
        }),
    }
}

/// When to record a sample: the first, then one that moved, at most one
/// per `every`.
#[derive(Debug)]
pub struct Throttle {
    every: Duration,
    last: Option<(Instant, Shape)>,
}

impl Throttle {
    pub fn new(every: Duration) -> Self {
        Self { every, last: None }
    }

    /// Whether `reading`, taken at `now`, is recorded (and if so, it is
    /// the one later readings are compared with).
    pub fn admit(&mut self, reading: &Reading, now: Instant) -> bool {
        let shape = shape(reading);
        let due = match &self.last {
            None => true,
            Some((at, last)) => now.duration_since(*at) >= self.every && *last != shape,
        };
        if due {
            self.last = Some((now, shape));
        }
        due
    }
}

struct Live {
    sampler: Sampler,
    pressure: Option<Pressure>,
    throttle: Throttle,
}

/// The hub's budget: its config, the pool (when on), and the machine
/// readings the ticker keeps.
pub struct BuildBudget {
    config: BudgetConfig,
    pool: Option<Pool>,
    live: Mutex<Live>,
}

impl std::fmt::Debug for BuildBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildBudget")
            .field("config", &self.config)
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

impl BuildBudget {
    /// The budget for `config` in `state_dir`: its pool made and filled
    /// when on. A pool that cannot be made is reported and left out
    /// (sessions then start without it).
    pub fn open(state_dir: &Path, config: BudgetConfig, sample_every: Duration) -> Self {
        let pool = if config.enabled {
            match Pool::open(&state_dir.join(FIFO_FILE), config.jobs) {
                Ok(pool) => Some(pool),
                Err(e) => {
                    eprintln!(
                        "fw hub: build budget: no jobserver pool ({e}); sessions start without one"
                    );
                    None
                }
            }
        } else {
            None
        };
        // The first window starts now: the ticker's first look has one.
        let mut sampler = Sampler::default();
        if config.enabled {
            sampler.sample();
        }
        Self {
            config,
            pool,
            live: Mutex::new(Live {
                sampler,
                pressure: None,
                throttle: Throttle::new(sample_every),
            }),
        }
    }

    pub fn config(&self) -> BudgetConfig {
        self.config
    }

    pub fn pool(&self) -> Option<&Pool> {
        self.pool.as_ref()
    }

    /// Whether the ticker runs: only with the budget on.
    pub fn ticks(&self) -> bool {
        self.config.enabled
    }

    /// The four variables every session wants (none with the budget off
    /// or without a pool; then test threads are left alone too).
    pub fn wanted_env(&self) -> Vec<(String, String)> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let flags = pool.flags();
        let threads = self.config.test_threads.to_string();
        vec![
            (VARS[0].to_owned(), flags.clone()),
            (VARS[1].to_owned(), flags),
            (VARS[2].to_owned(), threads.clone()),
            (VARS[3].to_owned(), threads),
        ]
    }

    /// What this session adds to `explicit` (its spawn's own variables):
    /// see [`session_env`].
    pub fn env_for(&self, explicit: &[(String, String)]) -> Vec<(String, String)> {
        session_env(
            &self.wanted_env(),
            |key| std::env::var_os(key).is_some(),
            explicit,
        )
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One look: the machine's pressure since the last, and the pool
    /// reset when nobody holds it and tokens went missing (or extra). The
    /// reading, and whether it is due to be recorded.
    pub fn tick(&self) -> (Reading, bool) {
        let pressure = {
            let mut live = self.live();
            if let Some(p) = live.sampler.sample() {
                live.pressure = Some(p);
            }
            live.pressure.clone()
        };
        let pool = self.pool.as_ref().map(|pool| {
            let mut free = pool.free().unwrap_or(0);
            let held = holders(pool.path(), std::process::id());
            if free != pool.tokens()
                && held.as_ref().is_some_and(Vec::is_empty)
                && pool.refill().is_ok()
            {
                free = pool.free().unwrap_or(free);
            }
            read_pool(pool, free, held.as_deref())
        });
        let reading = self.reading_with(pool, pressure);
        let due = self.live().throttle.admit(&reading, Instant::now());
        (reading, due)
    }

    /// A reading now: the pool as it is, the pressure the ticker last
    /// measured (with the budget off, measured now over `window`).
    pub fn read(&self, window: Duration) -> Reading {
        let pressure = if self.ticks() {
            self.live().pressure.clone()
        } else {
            pressure::measure(window)
        };
        let pool = self.pool.as_ref().map(|pool| {
            let free = pool.free().unwrap_or(0);
            let held = holders(pool.path(), std::process::id());
            read_pool(pool, free, held.as_deref())
        });
        self.reading_with(pool, pressure)
    }

    fn reading_with(&self, pool: Option<PoolReading>, pressure: Option<Pressure>) -> Reading {
        let threads = threads();
        let constraint = pressure
            .as_ref()
            .map_or(Constraint::None, |p| p.constraint(threads));
        Reading {
            enabled: self.config.enabled,
            jobs: self.config.jobs,
            test_threads: self.config.test_threads,
            threads,
            pool,
            pressure,
            constraint,
        }
    }
}

#[cfg(test)]
mod tests;
