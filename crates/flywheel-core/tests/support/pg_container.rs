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
//! When Docker is unavailable the first call prints the reason and every call
//! returns `None`; callers skip. A pre-existing server is never required.
//!
//! Included by `#[path]` from each test binary that needs a database, so the
//! fixture is compiled only into those targets; not every binary uses every
//! helper, hence the dead-code allowance.

#![allow(dead_code)]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};

const IMAGE: &str = "postgres:16";
const PASSWORD: &str = "fw-test";
const DATABASE: &str = "postgres";
const SUPERUSER: &str = "postgres";
/// How long to wait for the server to accept connections after `docker run`.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard lifetime of the container, enforced inside the image.
const TTL_SECONDS: u32 = 1800;
/// How long the last handle waits for a successor before tearing down.
const GRACE: Duration = Duration::from_secs(2);

struct Container {
    id: String,
    host: String,
    port: u16,
}

impl Container {
    fn start() -> Result<Self, String> {
        let probe = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        match probe {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                return Err(format!(
                    "docker daemon unreachable: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Err(err) => return Err(format!("docker is not on PATH: {err}")),
        }
        // `timeout` wraps the image's own entrypoint so the server exits on
        // its own after TTL_SECONDS even if nobody stops it; `--rm` then
        // removes the container. fsync is off: the data is disposable and
        // the suite is write-heavy.
        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                "flywheel-rust-test=1",
                "-e",
                &format!("POSTGRES_PASSWORD={PASSWORD}"),
                "-p",
                "127.0.0.1:0:5432",
                IMAGE,
                "timeout",
                &TTL_SECONDS.to_string(),
                "docker-entrypoint.sh",
                "postgres",
                "-c",
                "fsync=off",
                "-c",
                "synchronous_commit=off",
                "-c",
                "full_page_writes=off",
            ])
            .output()
            .map_err(|err| format!("docker run failed to spawn: {err}"))?;
        if !run.status.success() {
            return Err(format!(
                "docker run {IMAGE} failed: {}",
                String::from_utf8_lossy(&run.stderr).trim()
            ));
        }
        let id = String::from_utf8_lossy(&run.stdout).trim().to_owned();
        let container = match Self::published_port(&id) {
            Ok(port) => Self {
                id,
                host: "127.0.0.1".to_owned(),
                port,
            },
            Err(reason) => {
                Self::remove(&id);
                return Err(reason);
            }
        };
        if let Err(reason) = container.wait_ready() {
            Self::remove(&container.id);
            return Err(reason);
        }
        Ok(container)
    }

    fn published_port(id: &str) -> Result<u16, String> {
        let out = Command::new("docker")
            .args(["port", id, "5432/tcp"])
            .output()
            .map_err(|err| format!("docker port failed to spawn: {err}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .filter_map(|line| line.trim().rsplit(':').next())
            .find_map(|port| port.parse::<u16>().ok())
            .ok_or_else(|| format!("docker port returned no published port: {}", text.trim()))
    }

    fn wait_ready(&self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let dsn = self.dsn();
        let mut last_error = String::new();
        while Instant::now() < deadline {
            match Client::connect(&dsn, NoTls) {
                Ok(mut client) => match client.simple_query("SELECT 1") {
                    Ok(_) => return Ok(()),
                    Err(err) => last_error = err.to_string(),
                },
                Err(err) => last_error = err.to_string(),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "postgres container did not become ready within {READY_TIMEOUT:?}: {last_error}"
        ))
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
        let _ = Command::new("docker").args(["rm", "-f", id]).output();
    }
}

struct State {
    container: Option<Arc<Container>>,
    active: usize,
    unavailable: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    container: None,
    active: 0,
    unavailable: None,
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
    /// The shared container, starting it on first use; `None` (with the
    /// reason printed once) when Docker is unavailable.
    pub fn acquire() -> Option<Self> {
        let mut state = state();
        if let Some(reason) = &state.unavailable {
            eprintln!("skipping: postgres container unavailable: {reason}");
            return None;
        }
        if state.container.is_none() {
            match Container::start() {
                Ok(container) => state.container = Some(Arc::new(container)),
                Err(reason) => {
                    eprintln!("skipping: postgres container unavailable: {reason}");
                    state.unavailable = Some(reason);
                    return None;
                }
            }
        }
        state.active += 1;
        let container = Arc::clone(state.container.as_ref().expect("container just started"));
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

    /// A raw superuser connection, for inspection and sabotage.
    pub fn admin(&self) -> Client {
        Client::connect(&self.dsn(), NoTls).expect("connect to the test container")
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
        let last = {
            let mut state = state();
            state.active -= 1;
            state.active == 0
        };
        if !last {
            return;
        }
        // Give a test scheduled right behind this one a chance to reuse
        // the server before paying for a restart.
        std::thread::sleep(GRACE);
        let mut state = state();
        let still_ours = state.active == 0
            && state
                .container
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(c, &self.container));
        if still_ours && let Some(container) = state.container.take() {
            Container::remove(&container.id);
        }
    }
}
