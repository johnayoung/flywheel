//! `fw hub machine add|ls|rm|test` (the other hubs this one shows and
//! proxies to) and `fw hub peer issue|ls|revoke` (the credentials other
//! hubs hold to reach this one). See `machines.rs` and `peers.rs`.
//!
//! Secrets never go on argv (`/proc/*/cmdline` is world-readable): `machine
//! add` reads the peer credential (or, with `--mint`, the remote's token)
//! from FW_HUB_MACHINE_SECRET, else stdin -- prompted without echo on a
//! terminal.

use crate::cli::{StateDirArg, resolve_state_dir};
use crate::endpoint::Endpoint;
use crate::machines::MACHINES_FILE;
use crate::peers::PeerStore;
use crate::script::query_escape;
use crate::util::{fail, str_field};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

/// Where `machine add` reads the secret from before stdin.
pub const SECRET_ENV: &str = "FW_HUB_MACHINE_SECRET";

fn when(unix: Option<u64>) -> String {
    unix.and_then(|s| i64::try_from(s).ok())
        .and_then(|s| Utc.timestamp_opt(s, 0).single())
        .map_or_else(
            || "-".to_owned(),
            |t| t.format("%Y-%m-%d %H:%M").to_string(),
        )
}

// --- machine --------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct MachineArgs {
    #[command(subcommand)]
    command: MachineCommand,
}

#[derive(Debug, clap::Subcommand)]
enum MachineCommand {
    /// Register another hub: its base URL and a peer credential minted
    /// there (`fw hub peer issue --name <this hub>` on it), read from
    /// FW_HUB_MACHINE_SECRET or stdin. --mint takes its token instead and
    /// mints one (the token is not stored).
    Add(AddArgs),
    /// List machines with their connection state (each is probed).
    Ls(LsArgs),
    /// Forget a machine; it is asked to revoke the credential.
    Rm(RmArgs),
    /// Probe machines (all without NAME): reachable, credential accepted.
    /// Exit 1 when any is not.
    Test(TestArgs),
}

#[derive(Debug, clap::Args)]
struct AddArgs {
    /// Its name here: 1-32 of a-z, 0-9, '-'.
    name: String,
    /// Its base URL (https://box.tailnet.ts.net, http://100.64.0.2:7420).
    url: String,
    /// The secret is the machine's token (`fw hub token` there): use it
    /// once to mint a peer credential, store only that.
    #[arg(long)]
    mint: bool,
    /// With --mint: this hub's peer name there [default: this host name].
    #[arg(long, value_name = "NAME", requires = "mint")]
    peer_name: Option<String>,
    /// Pin the https machine by a SHA-256 fingerprint: its hub's local CA
    /// (`fw hub tls ca` there; survives certificate renewals), or a
    /// self-signed certificate (`openssl x509 -noout -fingerprint -sha256`).
    #[arg(long, value_name = "SHA256")]
    fingerprint: Option<String>,
    /// Re-pair the registered machine NAME in place (a new credential,
    /// URL, or pin; its name, and the layouts naming it, stay).
    #[arg(long)]
    replace: bool,
    /// Print the registered machine as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Show the last known state without probing.
    #[arg(long)]
    no_probe: bool,
    /// Print JSON: the machines and their link state.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct RmArgs {
    /// The machine's name (as `fw hub machine ls` lists it).
    name: String,
    /// Print JSON: what was removed and revoked.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct TestArgs {
    /// The machine to probe (default: every one).
    name: Option<String>,
    /// Print JSON: each machine's probe result.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn machine(args: &MachineArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        MachineCommand::Add(a) => add(a, out, err),
        MachineCommand::Ls(a) => ls(a, out),
        MachineCommand::Rm(a) => rm(a, out, err),
        MachineCommand::Test(a) => test(a, out),
    };
    match result {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn local_hub(state: &StateDirArg) -> Result<Endpoint, String> {
    let dir = resolve_state_dir(state)?;
    // Probes and mints wait on the other machine.
    Ok(Endpoint::local(&dir)?.with_timeout(Duration::from_secs(60)))
}

/// Read a secret: the environment, else one line of stdin (no echo on a
/// terminal).
fn read_secret(prompt: &str, err: &mut dyn Write) -> Result<String, String> {
    if let Ok(secret) = std::env::var(SECRET_ENV)
        && !secret.trim().is_empty()
    {
        return Ok(secret.trim().to_owned());
    }
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    if terminal {
        let _ = write!(err, "{prompt}: ");
        let _ = err.flush();
    }
    let echo = terminal.then(EchoOff::new).flatten();
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    drop(echo);
    if terminal {
        let _ = writeln!(err);
    }
    read.map_err(|e| format!("reading the secret from stdin: {e}"))?;
    let secret = line.trim().to_owned();
    if secret.is_empty() {
        return Err(format!(
            "no secret given: set {SECRET_ENV} or pipe it on stdin (never on the command line)"
        ));
    }
    Ok(secret)
}

/// Terminal echo off while it lives.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on stdin with an owned termios.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            (libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) == 0).then_some(Self(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `new`.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

fn add(args: &AddArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let hub = local_hub(&args.state)?;
    let secret = read_secret(
        if args.mint {
            "the machine's token (fw hub token there; used once, not stored)"
        } else {
            "peer credential (fw hub peer issue --name <this hub> there)"
        },
        err,
    )?;
    let mut body = json!({ "name": args.name, "url": args.url });
    if args.mint {
        body["token"] = json!(secret);
        if let Some(name) = &args.peer_name {
            body["peer_name"] = json!(name);
        }
    } else {
        body["credential"] = json!(secret);
    }
    if let Some(fingerprint) = &args.fingerprint {
        body["fingerprint"] = json!(fingerprint);
    }
    if args.replace {
        body["replace"] = json!(true);
    }
    let view = hub.blocking_call("POST", "/api/machines", Some(&body))?;
    if args.json {
        let _ = writeln!(out, "{view}");
        return Ok(0);
    }
    let status = &view["status"];
    let _ = writeln!(
        out,
        "{} machine {} ({}): {}{}",
        if args.replace { "re-paired" } else { "added" },
        str_field(&view, "name"),
        str_field(&view, "url"),
        str_field(status, "state"),
        match (status["version"].as_str(), status["host"].as_str()) {
            (Some(v), Some(h)) if !h.is_empty() => format!(", flywheel {v} on {h}"),
            (Some(v), _) => format!(", flywheel {v}"),
            _ => String::new(),
        }
    );
    if let Some(peer) = view["peer"].as_object() {
        let _ = writeln!(
            out,
            "  peer {} ({}) there; revoke with `fw hub peer revoke {}` on it",
            peer.get("name").and_then(Value::as_str).unwrap_or("?"),
            peer.get("id").and_then(Value::as_str).unwrap_or("?"),
            peer.get("name").and_then(Value::as_str).unwrap_or("?"),
        );
    }
    let _ = writeln!(
        out,
        "  try: fw hub ls --machine {}",
        str_field(&view, "name")
    );
    Ok(0)
}

fn probe(hub: &Endpoint, name: &str) -> Result<Value, String> {
    hub.blocking_call(
        "POST",
        &format!("/api/machines/{}/test", query_escape(name)),
        None,
    )
}

fn print_machine(out: &mut dyn Write, view: &Value, probe: Option<&Value>) {
    let status = &view["status"];
    let state = str_field(status, "state");
    let extra = match probe {
        Some(p) if p["ok"].as_bool() == Some(true) => format!(
            "{} ms, flywheel {} on {}",
            p["latency_ms"].as_u64().unwrap_or(0),
            str_field(&p["whoami"], "version"),
            str_field(&p["whoami"], "host"),
        ),
        Some(p) => str_field(p, "error").to_owned(),
        None => status["error"].as_str().unwrap_or("").to_owned(),
    };
    let _ = writeln!(
        out,
        "{:<16} {:<8} {}{}",
        str_field(view, "name"),
        state,
        str_field(view, "url"),
        if extra.is_empty() {
            String::new()
        } else {
            format!("  ({extra})")
        }
    );
    if let Some(fingerprint) = view["fingerprint"].as_str() {
        let _ = writeln!(out, "{:<16} pinned   {fingerprint}", "");
    }
}

/// Where the peer credentials are kept: always the state dir's plaintext
/// machines file (no OS keyring; docs/hub/security.md says why), with its mode
/// when it exists.
fn credential_store(state: &StateDirArg) -> Result<Value, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = resolve_state_dir(state)?.join(MACHINES_FILE);
    let mode = std::fs::metadata(&path)
        .ok()
        .map(|m| format!("{:04o}", m.permissions().mode() & 0o7777));
    Ok(json!({ "kind": "file", "path": path.display().to_string(), "mode": mode }))
}

fn print_store(out: &mut dyn Write, store: &Value) {
    let _ = writeln!(
        out,
        "credentials: {} (plaintext file{})",
        str_field(store, "path"),
        store["mode"]
            .as_str()
            .map(|m| format!(", mode {m}"))
            .unwrap_or_default()
    );
}

fn ls(args: &LsArgs, out: &mut dyn Write) -> Result<i32, String> {
    let hub = local_hub(&args.state)?;
    let store = credential_store(&args.state)?;
    let list = hub.blocking_call("GET", "/api/machines", None)?;
    let machines = list["machines"].as_array().cloned().unwrap_or_default();
    let mut shown = Vec::new();
    for view in machines {
        let probed = if args.no_probe {
            None
        } else {
            Some(probe(&hub, str_field(&view, "name"))?)
        };
        let view = probed
            .as_ref()
            .and_then(|p| p.get("machine").cloned())
            .unwrap_or(view);
        shown.push((view, probed));
    }
    if args.json {
        let machines: Vec<Value> = shown.into_iter().map(|(v, _)| v).collect();
        let _ = writeln!(
            out,
            "{}",
            json!({ "host": list["host"], "machines": machines, "credential_store": store })
        );
        return Ok(0);
    }
    if shown.is_empty() {
        let _ = writeln!(
            out,
            "no machines (add one: fw hub peer issue --name {} on it, then fw hub machine add NAME URL)",
            list["host"]
                .as_str()
                .filter(|h| !h.is_empty())
                .unwrap_or("<this hub>")
        );
        print_store(out, &store);
        return Ok(0);
    }
    for (view, probed) in &shown {
        print_machine(out, view, probed.as_ref());
    }
    print_store(out, &store);
    Ok(0)
}

fn rm(args: &RmArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let hub = local_hub(&args.state)?;
    let done = hub.blocking_call(
        "DELETE",
        &format!("/api/machines/{}", query_escape(&args.name)),
        None,
    )?;
    if args.json {
        let _ = writeln!(out, "{done}");
        return Ok(0);
    }
    let _ = writeln!(out, "removed machine {}", args.name);
    if done["revoked"].as_bool() == Some(true) {
        let _ = writeln!(out, "  its peer credential was revoked there");
    } else if let Some(note) = done["note"].as_str() {
        let _ = writeln!(err, "fw hub: note: {note}");
    }
    Ok(0)
}

fn test(args: &TestArgs, out: &mut dyn Write) -> Result<i32, String> {
    let hub = local_hub(&args.state)?;
    let names: Vec<String> = match &args.name {
        Some(name) => vec![name.clone()],
        None => hub.blocking_call("GET", "/api/machines", None)?["machines"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|m| str_field(m, "name").to_owned())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut failed = false;
    let mut results = Vec::new();
    for name in &names {
        let result = probe(&hub, name)?;
        failed |= result["ok"].as_bool() != Some(true);
        results.push(result);
    }
    if args.json {
        let _ = writeln!(out, "{}", json!({ "results": results }));
    } else if results.is_empty() {
        let _ = writeln!(out, "no machines");
    } else {
        for result in &results {
            print_machine(out, &result["machine"], Some(result));
            if let Some(seen) = result["machine"]["status"]["presented_fingerprint"].as_str()
                && result["ok"].as_bool() != Some(true)
                && result["machine"]["fingerprint"].is_null()
            {
                let _ = writeln!(out, "{:<16} presents {seen}", "");
            }
        }
    }
    Ok(i32::from(failed))
}

// --- peer -----------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct PeerArgs {
    #[command(subcommand)]
    command: PeerCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PeerCommand {
    /// Mint a peer credential for another hub (printed once, alone on
    /// stdout): API and WebSockets only, never this hub's administration.
    Issue(IssueArgs),
    /// List peers (never their secrets).
    Ls(PeerLsArgs),
    /// Revoke a peer by name or id: its requests stop and its open
    /// sockets close at once.
    Revoke(RevokeArgs),
}

#[derive(Debug, clap::Args)]
struct IssueArgs {
    /// The other hub's name (1-64 of letters, digits, '.', '_', '-').
    #[arg(long)]
    name: String,
    /// Print JSON: {"peer":{...},"credential"}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct PeerLsArgs {
    /// Print JSON: the peers (never their credentials).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct RevokeArgs {
    /// Peer name or id.
    peer: String,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn peer(args: &PeerArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        PeerCommand::Issue(a) => issue(a, out, err),
        PeerCommand::Ls(a) => peer_ls(a, out),
        PeerCommand::Revoke(a) => revoke(a, out),
    };
    match result {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

/// The running hub for the state dir (so its live store changes), or
/// `None`: the file is edited directly.
fn running(state: &StateDirArg) -> Result<(std::path::PathBuf, Option<Endpoint>), String> {
    let dir = resolve_state_dir(state)?;
    crate::paths::ensure_private_dir(&dir)
        .map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let hub = Endpoint::running(&dir)?;
    Ok((dir, hub))
}

fn issue(args: &IssueArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let (dir, hub) = running(&args.state)?;
    let issued = match hub {
        Some(hub) => {
            hub.blocking_call("POST", "/api/peers", Some(&json!({ "name": args.name })))?
        }
        None => {
            let store = PeerStore::load(&dir);
            let (peer, credential) = store.issue(&args.name).map_err(|e| e.to_string())?;
            json!({ "peer": peer, "credential": credential })
        }
    };
    if args.json {
        let _ = writeln!(out, "{issued}");
        return Ok(0);
    }
    let _ = writeln!(out, "{}", str_field(&issued, "credential"));
    let _ = writeln!(
        err,
        "fw hub: peer {} ({}): shown once. It runs any command as this hub's user \
         (treat it like the token). On the other hub: \
         printf %s '<credential>' | fw hub machine add <name> <this hub's URL>",
        str_field(&issued["peer"], "name"),
        str_field(&issued["peer"], "id"),
    );
    Ok(0)
}

fn peer_ls(args: &PeerLsArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (dir, hub) = running(&args.state)?;
    let list = match hub {
        Some(hub) => hub.blocking_call("GET", "/api/peers", None)?,
        None => {
            let peers = crate::peers::Peers::load(&dir).map_err(|e| e.to_string())?;
            let peers: Vec<Value> = peers
                .list()
                .into_iter()
                .map(|(p, _)| json!({ "id": p.id, "name": p.name, "created": p.created, "last_used": null }))
                .collect();
            json!({ "peers": peers })
        }
    };
    if args.json {
        let _ = writeln!(out, "{list}");
        return Ok(0);
    }
    let peers = list["peers"].as_array().cloned().unwrap_or_default();
    if peers.is_empty() {
        let _ = writeln!(out, "no peers (fw hub peer issue --name NAME)");
    }
    for peer in peers {
        let _ = writeln!(
            out,
            "{:<24} {}  created {}  last used {}",
            str_field(&peer, "name"),
            str_field(&peer, "id"),
            when(peer["created"].as_u64()),
            when(peer["last_used"].as_u64()),
        );
    }
    Ok(0)
}

fn revoke(args: &RevokeArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (dir, hub) = running(&args.state)?;
    let peer = match hub {
        Some(hub) => hub.blocking_call(
            "DELETE",
            &format!("/api/peers/{}", query_escape(&args.peer)),
            None,
        )?["peer"]
            .clone(),
        None => {
            let store = PeerStore::load(&dir);
            serde_json::to_value(store.revoke(&args.peer).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        }
    };
    let _ = writeln!(
        out,
        "revoked peer {} ({})",
        str_field(&peer, "name"),
        str_field(&peer, "id")
    );
    Ok(0)
}
