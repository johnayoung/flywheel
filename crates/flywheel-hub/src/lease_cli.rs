//! `fw hub lease acquire|run|release|ls|break`: leases on named resources
//! (see [`crate::leases`]) through the running hub. `run` holds one while
//! a command runs (renewed as it goes, released when it exits); with
//! `--or-flock PATH` it falls back to `flock(PATH)` when no hub answers
//! and takes that lock after the lease too, so runners with and without a
//! hub still take turns.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::leases::{DEFAULT_TTL_SECS, MAX_BLOCK_SECS};
use crate::script::query_escape;
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, clap::Args)]
pub(crate) struct LeaseArgs {
    #[command(subcommand)]
    command: LeaseCommand,
}

#[derive(Debug, clap::Subcommand)]
enum LeaseCommand {
    /// Take a lease on RESOURCE (exclusive unless --shared), waiting in
    /// line up to --wait; prints its id. It lasts --ttl unless renewed
    /// (acquire again) or released; it ends when this process's session
    /// ends.
    Acquire(AcquireArgs),
    /// Run CMD holding a lease on RESOURCE: wait for it, renew it while
    /// CMD runs, release it when CMD exits; exits with CMD's status.
    Run(RunArgs),
    /// Release a lease this process's session or the operator holds (its
    /// id, or the resource).
    Release(ReleaseArgs),
    /// Every lease and wait (or RESOURCE's): holder, mode, age, expiry.
    Ls(LsArgs),
    /// End someone else's lease (the operator only).
    Break(BreakArgs),
}

#[derive(Debug, clap::Args)]
struct Common {
    /// shared: others may hold it shared too; default exclusive.
    #[arg(long)]
    shared: bool,
    /// How long it is held before it must be renewed (`90s`, `30m`, `2h`;
    /// default 30m).
    #[arg(long, value_name = "SPAN")]
    ttl: Option<String>,
    /// How long to wait in line (`0`: not at all; default 0 for acquire,
    /// 1h for run).
    #[arg(long, value_name = "SPAN")]
    wait: Option<String>,
    /// What it is for, shown beside it.
    #[arg(long, value_name = "TEXT")]
    note: Option<String>,
}

#[derive(Debug, clap::Args)]
struct AcquireArgs {
    /// The resource's name (cpu:test-suite, node:desktop:heavy).
    resource: String,
    #[command(flatten)]
    common: Common,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The resource's name (cpu:test-suite, node:desktop:heavy).
    resource: String,
    #[command(flatten)]
    common: Common,
    /// When no hub answers, take `flock(PATH)` instead; with a hub, take
    /// it after the lease too.
    #[arg(long, value_name = "PATH")]
    or_flock: Option<PathBuf>,
    #[command(flatten)]
    endpoint: EndpointArgs,
    /// The command and its arguments.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct ReleaseArgs {
    /// A lease id, or a resource this holder has.
    lease: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Only this resource's leases and waits.
    resource: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct BreakArgs {
    /// The lease's id.
    lease: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &LeaseArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

/// A span: seconds, or `90s`, `30m`, `2h`, `1d`, combined (`1h30m`).
pub(crate) fn parse_span(raw: &str) -> Result<u32, String> {
    let text = raw.trim().to_ascii_lowercase();
    if let Ok(n) = text.parse::<u32>() {
        return Ok(n);
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit: u64 = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(format!("bad span {raw:?}: like 90s, 30m, 2h, 1d")),
        };
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("bad span {raw:?}: a number comes before each unit"))?;
        digits.clear();
        total = total.saturating_add(n.saturating_mul(unit));
    }
    if !digits.is_empty() {
        return Err(format!("bad span {raw:?}: the last number has no unit"));
    }
    u32::try_from(total).map_err(|_| format!("span {raw:?} is too long"))
}

/// This process as a lease holder.
fn me(label: &str) -> Value {
    let pid = std::process::id();
    json!({
        "host": crate::machines::host_name(),
        "pid": pid,
        "started": crate::hub::leases::process_start(pid),
        "session": std::env::var(crate::report::SESSION_ENV).ok().filter(|s| !s.is_empty()),
        "label": label,
    })
}

fn connect(endpoint: &EndpointArgs) -> Result<Endpoint, String> {
    Ok(Endpoint::resolve(endpoint)?
        .with_timeout(Duration::from_secs(u64::from(MAX_BLOCK_SECS) + 20)))
}

/// Ask for a lease and wait in line until granted or `wait` ran out:
/// the lease, or why not.
fn take(
    conn: &Endpoint,
    resource: &str,
    common: &Common,
    default_wait: u32,
    holder: &Value,
    err: &mut dyn Write,
) -> Result<Value, String> {
    let ttl = common
        .ttl
        .as_deref()
        .map_or(Ok(DEFAULT_TTL_SECS), parse_span)?;
    let wait = common
        .wait
        .as_deref()
        .map_or(Ok(default_wait), parse_span)?;
    let answer = conn.blocking_call(
        "POST",
        "/api/leases",
        Some(&json!({
            "resource": resource,
            "mode": if common.shared { "shared" } else { "exclusive" },
            "ttl_secs": ttl,
            "wait_secs": wait,
            "note": common.note,
            "holder": holder,
            "block_secs": 0,
        })),
    )?;
    if answer["state"] != "waiting" {
        return Ok(answer["lease"].clone());
    }
    let waiter = str_field(&answer["waiter"], "id").to_owned();
    let _ = writeln!(
        err,
        "  ..   waiting for {resource}: #{} in line",
        answer["place"].as_u64().unwrap_or(0)
    );
    loop {
        let polled = conn.blocking_call(
            "GET",
            &format!(
                "/api/leases/waiters/{}?wait={MAX_BLOCK_SECS}",
                query_escape(&waiter)
            ),
            None,
        );
        match polled {
            Ok(got) if got["state"] == "granted" => return Ok(got["lease"].clone()),
            Ok(_) => continue,
            Err(e) => {
                return Err(format!(
                    "no lease on {resource} within {}: {e}",
                    crate::share::format_ttl(Duration::from_secs(u64::from(wait)))
                ));
            }
        }
    }
}

fn run(args: &LeaseArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    match &args.command {
        LeaseCommand::Acquire(a) => {
            let conn = connect(&a.endpoint)?;
            let lease = take(&conn, &a.resource, &a.common, 0, &me("fw hub lease"), err)?;
            if a.json {
                print_json(out, &lease);
            } else {
                let _ = writeln!(out, "{}", str_field(&lease, "id"));
            }
            Ok(0)
        }
        LeaseCommand::Run(a) => run_holding(a, err),
        LeaseCommand::Release(a) => {
            let conn = connect(&a.endpoint)?;
            let lease = conn.blocking_call(
                "POST",
                &format!("/api/leases/{}/release", query_escape(&a.lease)),
                Some(&json!({ "holder": me("fw hub lease") })),
            );
            // Not this process's: the operator's own, by id.
            let lease = match lease {
                Ok(lease) => lease,
                Err(_) => conn.blocking_call(
                    "POST",
                    &format!("/api/leases/{}/release", query_escape(&a.lease)),
                    Some(&json!({})),
                )?,
            };
            if a.json {
                print_json(out, &lease);
            } else {
                let _ = writeln!(
                    out,
                    "released {} on {}",
                    str_field(&lease, "id"),
                    str_field(&lease, "resource")
                );
            }
            Ok(0)
        }
        LeaseCommand::Break(a) => {
            let conn = connect(&a.endpoint)?;
            let lease = conn.blocking_call(
                "POST",
                &format!("/api/leases/{}/release", query_escape(&a.lease)),
                Some(&json!({ "break": true })),
            )?;
            if a.json {
                print_json(out, &lease);
            } else {
                let _ = writeln!(
                    out,
                    "broke {} on {} (held by {})",
                    str_field(&lease, "id"),
                    str_field(&lease, "resource"),
                    holder_text(&lease["holder"])
                );
            }
            Ok(0)
        }
        LeaseCommand::Ls(a) => {
            let conn = connect(&a.endpoint)?;
            let path = match &a.resource {
                Some(r) => format!("/api/leases?resource={}", query_escape(r)),
                None => "/api/leases".to_owned(),
            };
            let view = conn.blocking_call("GET", &path, None)?;
            if a.json {
                print_json(out, &view);
            } else {
                print_leases(out, &view);
            }
            Ok(0)
        }
    }
}

fn holder_text(holder: &Value) -> String {
    match str_field(holder, "kind") {
        "session" => format!("session {}", str_field(holder, "id")),
        "process" => format!(
            "{} (pid {} on {})",
            str_field(holder, "label"),
            holder["pid"],
            str_field(holder, "host")
        ),
        other => other.to_owned(),
    }
}

fn print_leases(out: &mut dyn Write, view: &Value) {
    let now = chrono::Utc::now().timestamp_millis();
    let ago = |ms: i64| crate::share::format_ttl(Duration::from_millis(ms.max(0).unsigned_abs()));
    let leases = view["leases"].as_array().cloned().unwrap_or_default();
    let waiters = view["waiters"].as_array().cloned().unwrap_or_default();
    if leases.is_empty() && waiters.is_empty() {
        let _ = writeln!(out, "no leases");
        return;
    }
    for lease in &leases {
        let _ = writeln!(
            out,
            "{}  {}  {}  {}  held {}, expires in {}{}",
            str_field(lease, "id"),
            str_field(lease, "resource"),
            str_field(lease, "mode"),
            holder_text(&lease["holder"]),
            ago(now - lease["granted_at"].as_i64().unwrap_or(now)),
            ago(lease["expires_at"].as_i64().unwrap_or(now) - now),
            lease
                .get("note")
                .and_then(Value::as_str)
                .map(|n| format!("  ({n})"))
                .unwrap_or_default()
        );
    }
    for waiter in &waiters {
        let _ = writeln!(
            out,
            "{}  {}  {}  {}  waiting {}",
            str_field(waiter, "id"),
            str_field(waiter, "resource"),
            str_field(waiter, "mode"),
            holder_text(&waiter["holder"]),
            ago(now - waiter["queued_at"].as_i64().unwrap_or(now))
        );
    }
}

/// `fw hub lease run`.
fn run_holding(a: &RunArgs, err: &mut dyn Write) -> Result<i32, String> {
    let label = format!("fw hub lease run {}", a.command[0]);
    let hub = connect(&a.endpoint).and_then(|conn| {
        let lease = take(&conn, &a.resource, &a.common, 3600, &me(&label), err)?;
        Ok((conn, lease))
    });
    let (conn, lease) = match hub {
        Ok(found) => (Some(found.0), Some(found.1)),
        Err(e) if a.or_flock.is_some() && unreachable(&e) => {
            let _ = writeln!(err, "  ..   no hub ({e}): taking the file lock instead");
            (None, None)
        }
        Err(e) => return Err(e),
    };
    let flocked = match &a.or_flock {
        Some(path) => Some(flock(path, err)?),
        None => None,
    };
    let renewer = match (&conn, &lease) {
        (Some(_), Some(lease)) => Some(renew_in_background(
            connect(&a.endpoint)?,
            str_field(lease, "id").to_owned(),
            lease["ttl_secs"]
                .as_u64()
                .unwrap_or(u64::from(DEFAULT_TTL_SECS)),
            me(&label),
        )),
        _ => None,
    };
    let status = std::process::Command::new(&a.command[0])
        .args(&a.command[1..])
        .status();
    if let Some((stop, handle)) = renewer {
        let _ = stop.send(());
        let _ = handle.join();
    }
    // The file lock first: whoever the lease goes to next takes it too.
    drop(flocked);
    if let (Some(conn), Some(lease)) = (&conn, &lease) {
        let _ = conn.blocking_call(
            "POST",
            &format!("/api/leases/{}/release", str_field(lease, "id")),
            Some(&json!({ "holder": me(&label) })),
        );
    }
    let status = status.map_err(|e| format!("cannot run {}: {e}", a.command[0]))?;
    Ok(status.code().unwrap_or(1))
}

/// Whether `error` says no hub answered (rather than that it refused).
fn unreachable(error: &str) -> bool {
    error.contains("no hub")
        || error.contains("cannot reach")
        || error.contains("unreachable")
        || error.contains("not running")
        || error.contains("connect")
}

/// Renew lease `id` every third of its time to live until told to stop.
fn renew_in_background(
    conn: Endpoint,
    id: String,
    ttl_secs: u64,
    holder: Value,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let every = Duration::from_secs((ttl_secs / 3).max(10));
    let handle = std::thread::spawn(move || {
        let mut next = Instant::now() + every;
        while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            stopped.recv_timeout(next.saturating_duration_since(Instant::now()))
        {
            if let Err(e) = conn.blocking_call(
                "POST",
                &format!("/api/leases/{id}/renew"),
                Some(&json!({ "holder": holder })),
            ) {
                eprintln!("fw hub lease: renewing {id}: {e}");
            }
            next = Instant::now() + every;
        }
    });
    (stop, handle)
}

/// Hold `flock(path)` (waiting for it) until dropped.
fn flock(path: &Path, err: &mut dyn Write) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a file descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let _ = writeln!(err, "  ..   waiting for the file lock {}", path.display());
        // SAFETY: as above.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "flock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: LeaseArgs,
    }

    fn run_cli(argv: &[&str]) -> (i32, String) {
        let cli = Cli::try_parse_from(std::iter::once("fw").chain(argv.iter().copied())).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main(&cli.args, &mut out, &mut err);
        (code, String::from_utf8_lossy(&err).into_owned())
    }

    #[test]
    fn spans_read_like_durations() {
        assert_eq!(parse_span("90").unwrap(), 90);
        assert_eq!(parse_span("1h30m").unwrap(), 5400);
        assert_eq!(parse_span("2d").unwrap(), 172_800);
        assert!(parse_span("5x").is_err());
        assert!(parse_span("5m3").is_err());
    }

    #[test]
    fn with_no_hub_run_takes_the_file_lock_and_passes_the_exit_status_on() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let lock = dir.path().join("check.lock");
        let marker = dir.path().join("ran");
        let script = format!("touch {} && exit 3", marker.display());
        let argv = [
            "run",
            "cpu:test-suite",
            "--state-dir",
            state.to_str().unwrap(),
            "--or-flock",
            lock.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            script.as_str(),
        ];
        let (code, err) = run_cli(&argv);
        assert_eq!(code, 3, "{err}");
        assert!(marker.exists());
        assert!(err.contains("taking the file lock instead"), "{err}");

        // Held by another: run waits for it.
        use std::os::fd::AsRawFd;
        let held = std::fs::File::create(&lock).unwrap();
        // SAFETY: flock on a file this test owns.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        std::fs::remove_file(&marker).unwrap();
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        let waiter = std::thread::spawn(move || {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            run_cli(&argv)
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists(), "ran while the lock was held");
        drop(held);
        let (code, err) = waiter.join().unwrap();
        assert_eq!(code, 3, "{err}");
        assert!(marker.exists());
    }

    #[test]
    fn with_no_hub_and_no_fallback_run_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (code, err) = run_cli(&[
            "run",
            "cpu:x",
            "--state-dir",
            state.to_str().unwrap(),
            "--",
            "true",
        ]);
        assert_ne!(code, 0);
        assert!(err.contains("no hub"), "{err}");
    }
}
