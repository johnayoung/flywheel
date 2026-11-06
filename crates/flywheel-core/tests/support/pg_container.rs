//! A throwaway `postgres:16` container shared by every test in one test
//! binary.
//!
//! The first test that calls [`Postgres::acquire`] starts the container
//! (`docker run -d --rm` on an ephemeral loopback port) and waits until the
//! server accepts connections; later tests in the same process reuse it. The
//! last live handle tears it down after a short grace period, so a burst of
//! tests scheduled back to back shares one server while a test binary that
//! has finished leaves nothing behind. The container also runs under a hard
//! TTL inside the image, so a killed test process cannot leak it forever.
//!
//! Nothing here blocks without bound: every `docker` call runs under a
//! deadline, every connection the helper opens has a connect timeout and
//! TCP keepalives and a bound on the whole handshake ([`connect_within`]),
//! and the server aborts any statement (lock waits included) after
//! [`STATEMENT_TIMEOUT_MS`]. A container that died, or is close to its TTL,
//! is replaced on the next [`Postgres::acquire`] instead of being handed out
//! again. Each test process runs its own container on its own ephemeral
//! port, so concurrent suites never share (or race on) one server.
//!
//! When Docker is unavailable (not on PATH, daemon unreachable) the first
//! call prints the reason and every call returns `None`; callers skip. When
//! Docker is there but the container cannot be started or never becomes
//! ready, every call panics with the reason: a broken fixture fails loudly
//! instead of silently skipping the suite. A pre-existing server is never
//! required.
//!
//! Included by `#[path]` from each test binary that needs a database, so the
//! fixture is compiled only into those targets; not every binary uses every
//! helper, hence the dead-code allowance.

#![allow(dead_code)]

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use postgres::{Client, Config, NoTls};

const IMAGE: &str = "postgres:16";
const PASSWORD: &str = "fw-test";
const DATABASE: &str = "postgres";
const SUPERUSER: &str = "postgres";
/// How long to wait for the server to accept connections after `docker run`.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard lifetime of the container, enforced inside the image.
const TTL_SECONDS: u64 = 1800;
/// A container this close to its TTL is not handed to a new test: the
/// server could exit while that test still uses it.
const TTL_MARGIN: Duration = Duration::from_secs(300);
/// How long the last handle waits for a successor before tearing down.
const GRACE: Duration = Duration::from_secs(2);
/// Bound on one TCP connect attempt (the driver's `connect_timeout`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on one whole connection (TCP, startup, auth) plus `SELECT 1`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Keepalive schedule: a peer that vanished without closing its sockets is
/// detected after about idle + interval * retries.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_RETRIES: u32 = 3;
/// Server-side bound on any statement, lock waits included: two test
/// connections waiting on each other error out instead of hanging.
const STATEMENT_TIMEOUT_MS: u64 = 120_000;
/// Deadlines for the `docker` CLI calls; `run` may pull the image.
const DOCKER_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(300);
const DOCKER_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Why there is no container.
enum StartError {
    /// Docker itself is missing or unreachable: callers skip.
    Unavailable(String),
    /// Docker is there but the server could not be brought up: a failure.
    Failed(String),
}

/// What a finished `docker` call produced.
struct DockerOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `docker args...` to completion, or kill it at `timeout`.
fn docker(args: &[&str], timeout: Duration) -> Result<DockerOutput, String> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("docker is not on PATH: {err}"))?;
    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    }
    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`docker {}` did not finish within {timeout:?}",
                    args.first().copied().unwrap_or_default()
                ));
            }
            Err(err) => return Err(format!("waiting on docker failed: {err}")),
        }
    };
    Ok(DockerOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// The driver config for `dsn` with every client-side bound applied.
fn bounded_config(dsn: &str) -> Result<Config, String> {
    let mut config: Config = dsn.parse().map_err(|err| format!("invalid DSN: {err}"))?;
    config
        .connect_timeout(CONNECT_TIMEOUT)
        .keepalives(true)
        .keepalives_idle(KEEPALIVE_IDLE)
        .keepalives_interval(KEEPALIVE_INTERVAL)
        .keepalives_retries(KEEPALIVE_RETRIES);
    Ok(config)
}

/// Connect to `dsn` and run `SELECT 1`, failing after `timeout` even when
/// the peer accepts the socket but never answers the startup handshake (a
/// case the driver's TCP-level connect timeout does not cover). A timed-out
/// attempt is abandoned on its own thread; its socket's keepalives end it
/// once the peer is gone.
pub fn connect_within(dsn: &str, timeout: Duration) -> Result<Client, String> {
    let config = bounded_config(dsn)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let attempt = config.connect(NoTls).and_then(|mut client| {
            client.simple_query("SELECT 1")?;
            Ok(client)
        });
        let _ = tx.send(attempt.map_err(|err| err.to_string()));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "no answer from the server within {timeout:?} (connect, startup, SELECT 1)"
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("the connection attempt panicked".to_owned())
        }
    }
}

struct Container {
    id: String,
    host: String,
    port: u16,
    started: Instant,
    /// Live [`Postgres`] handles on this container.
    handles: AtomicUsize,
}

impl Container {
    fn start() -> Result<Self, StartError> {
        let probe = docker(
            &["version", "--format", "{{.Server.Version}}"],
            DOCKER_PROBE_TIMEOUT,
        )
        .map_err(StartError::Unavailable)?;
        if !probe.success {
            return Err(StartError::Unavailable(format!(
                "docker daemon unreachable: {}",
                probe.stderr.trim()
            )));
        }
        // `timeout` wraps the image's own entrypoint so the server exits on
        // its own after TTL_SECONDS even if nobody stops it; `--rm` then
        // removes the container. fsync is off: the data is disposable and
        // the suite is write-heavy.
        let password = format!("POSTGRES_PASSWORD={PASSWORD}");
        let ttl = TTL_SECONDS.to_string();
        let statement_timeout = format!("statement_timeout={STATEMENT_TIMEOUT_MS}");
        let run = docker(
            &[
                "run",
                "-d",
                "--rm",
                "--label",
                "flywheel-rust-test=1",
                "-e",
                &password,
                "-p",
                "127.0.0.1:0:5432",
                IMAGE,
                "timeout",
                &ttl,
                "docker-entrypoint.sh",
                "postgres",
                "-c",
                "fsync=off",
                "-c",
                "synchronous_commit=off",
                "-c",
                "full_page_writes=off",
                "-c",
                &statement_timeout,
            ],
            DOCKER_RUN_TIMEOUT,
        )
        .map_err(StartError::Failed)?;
        if !run.success {
            return Err(StartError::Failed(format!(
                "docker run {IMAGE} failed: {}",
                run.stderr.trim()
            )));
        }
        let id = run.stdout.trim().to_owned();
        let container = match Self::published_port(&id) {
            Ok(port) => Self {
                id,
                host: "127.0.0.1".to_owned(),
                port,
                started: Instant::now(),
                handles: AtomicUsize::new(0),
            },
            Err(reason) => {
                Self::remove(&id);
                return Err(StartError::Failed(reason));
            }
        };
        if let Err(reason) = container.wait_ready() {
            Self::remove(&container.id);
            return Err(StartError::Failed(reason));
        }
        Ok(container)
    }

    fn published_port(id: &str) -> Result<u16, String> {
        let out = docker(&["port", id, "5432/tcp"], DOCKER_QUERY_TIMEOUT)?;
        out.stdout
            .lines()
            .filter_map(|line| line.trim().rsplit(':').next())
            .find_map(|port| port.parse::<u16>().ok())
            .ok_or_else(|| {
                format!(
                    "docker port returned no published port: {} {}",
                    out.stdout.trim(),
                    out.stderr.trim()
                )
            })
    }

    fn wait_ready(&self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let dsn = self.dsn();
        let mut last_error = String::new();
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match connect_within(&dsn, HANDSHAKE_TIMEOUT.min(left)) {
                Ok(_) => return Ok(()),
                Err(err) => last_error = err,
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "postgres container {} did not become ready within {READY_TIMEOUT:?}: {last_error}",
            self.id
        ))
    }

    /// Whether a new test may use this container: well inside its TTL and
    /// still accepting connections (a dead container's published port is
    /// gone, so the connect is refused at once).
    fn usable(&self) -> bool {
        if self.started.elapsed() + TTL_MARGIN >= Duration::from_secs(TTL_SECONDS) {
            return false;
        }
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
    }

    fn dsn(&self) -> String {
        self.dsn_with_credentials(SUPERUSER, PASSWORD)
    }

    fn dsn_with_credentials(&self, user: &str, password: &str) -> String {
        format!(
            "postgresql://{user}:{password}@{}:{}/{DATABASE}",
            self.host, self.port
        )
    }

    fn remove(id: &str) {
        let _ = docker(&["rm", "-f", id], DOCKER_QUERY_TIMEOUT);
    }
}

struct State {
    container: Option<Arc<Container>>,
    /// Docker is missing: every caller skips.
    unavailable: Option<String>,
    /// Docker is there but the server could not be brought up: every
    /// caller fails with this reason.
    failed: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    container: None,
    unavailable: None,
    failed: None,
});

static SCHEMA_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A live handle on the shared container. Keep it for the test's duration.
pub struct Postgres {
    container: Arc<Container>,
}

impl Postgres {
    /// The shared container, started (or a dead one replaced) on demand;
    /// `None`, with the reason printed, when Docker is unavailable.
    ///
    /// # Panics
    ///
    /// When Docker is available but the container cannot be started or
    /// never becomes ready.
    pub fn acquire() -> Option<Self> {
        let mut state = state();
        if let Some(reason) = &state.unavailable {
            eprintln!("skipping: postgres container unavailable: {reason}");
            return None;
        }
        if let Some(reason) = state.failed.clone() {
            drop(state);
            panic!("postgres test container failed: {reason}");
        }
        if state.container.as_ref().is_some_and(|c| !c.usable()) {
            // Dead or close to its TTL. Handles still on it remove it when
            // the last one drops; an unused one is removed now (a dead
            // one's `--rm` normally already did).
            let stale = state.container.take().expect("checked above");
            if stale.handles.load(Ordering::SeqCst) == 0 {
                Container::remove(&stale.id);
            }
        }
        if state.container.is_none() {
            match Container::start() {
                Ok(container) => state.container = Some(Arc::new(container)),
                Err(StartError::Unavailable(reason)) => {
                    eprintln!("skipping: postgres container unavailable: {reason}");
                    state.unavailable = Some(reason);
                    return None;
                }
                Err(StartError::Failed(reason)) => {
                    state.failed = Some(reason.clone());
                    drop(state);
                    panic!("postgres test container failed: {reason}");
                }
            }
        }
        let container = Arc::clone(state.container.as_ref().expect("container just started"));
        container.handles.fetch_add(1, Ordering::SeqCst);
        Some(Self { container })
    }

    /// Superuser DSN for the throwaway server.
    pub fn dsn(&self) -> String {
        self.container.dsn()
    }

    /// A DSN for another role on the same server.
    pub fn dsn_with_credentials(&self, user: &str, password: &str) -> String {
        self.container.dsn_with_credentials(user, password)
    }

    /// An identifier-safe schema name no other test in this process uses.
    pub fn fresh_schema(&self, prefix: &str) -> String {
        format!(
            "{prefix}_{}_{}",
            std::process::id(),
            SCHEMA_COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// A raw superuser connection, for inspection and sabotage. Bounded: a
    /// server that does not answer fails the test instead of hanging it.
    pub fn admin(&self) -> Client {
        connect_within(&self.dsn(), HANDSHAKE_TIMEOUT)
            .unwrap_or_else(|err| panic!("connect to the test container: {err}"))
    }

    /// A raw superuser connection with `search_path` pinned to `schema`.
    pub fn admin_in(&self, schema: &str) -> Client {
        let mut client = self.admin();
        client
            .batch_execute(&format!("SET search_path TO \"{schema}\", public"))
            .expect("pin search_path");
        client
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        if self.container.handles.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let current = state()
            .container
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &self.container));
        if !current {
            // Replaced while in use: nothing hands it out any more.
            Container::remove(&self.container.id);
            return;
        }
        // Give a test scheduled right behind this one a chance to reuse
        // the server before paying for a restart.
        std::thread::sleep(GRACE);
        let mut state = state();
        let still_ours = self.container.handles.load(Ordering::SeqCst) == 0
            && state
                .container
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(c, &self.container));
        if still_ours && let Some(container) = state.container.take() {
            Container::remove(&container.id);
        }
    }
}
