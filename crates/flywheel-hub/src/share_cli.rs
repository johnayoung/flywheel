//! `fw hub share SESSION | ls | revoke` (read-only live view links, see
//! `share.rs`) and `fw hub record start|stop|ls|get|rm` (session
//! recordings, see `record.rs`).

use crate::cli::{Qr, hub_targets, resolve_state_dir};
use crate::endpoint::{Endpoint, EndpointArgs};
use crate::remote::{LoginTarget, qr_lines};
use crate::script::{query_escape, resolve};
use crate::util::{fail, str_field};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// A Unix time in UTC (as recording file names are), with how far off it
/// is: `2026-11-03 10:59 UTC (in 57m)`.
fn when(unix: Option<u64>) -> String {
    let Some(at) = unix
        .and_then(|s| i64::try_from(s).ok())
        .and_then(|s| Utc.timestamp_opt(s, 0).single())
    else {
        return "-".to_owned();
    };
    format!(
        "{} ({})",
        at.format("%Y-%m-%d %H:%M UTC"),
        relative(at.timestamp() - Utc::now().timestamp())
    )
}

/// `in 57m`, `3h ago`, `now`.
fn relative(delta: i64) -> String {
    let span = delta.unsigned_abs();
    let text = match span {
        0..60 => return "now".to_owned(),
        60..3600 => format!("{}m", span / 60),
        3600..86_400 => format!("{}h{:02}m", span / 3600, span % 3600 / 60),
        _ => format!("{}d{}h", span / 86_400, span % 86_400 / 3600),
    };
    if delta > 0 {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    match bytes {
        b if b < KIB => format!("{b} B"),
        b if b < KIB * KIB => format!("{:.1} KiB", b as f64 / KIB as f64),
        b if b < KIB * KIB * KIB => format!("{:.1} MiB", b as f64 / (KIB * KIB) as f64),
        b => format!("{:.2} GiB", b as f64 / (KIB * KIB * KIB) as f64),
    }
}

/// The session `query` names on `conn`: `(id, record)`.
fn find_session(conn: &Endpoint, query: &str) -> Result<(String, Value), String> {
    let sessions = conn.blocking_sessions()?;
    let session = resolve(&sessions, query, "session")?;
    Ok((str_field(session, "id").to_owned(), session.clone()))
}

// --- share ----------------------------------------------------------------------

// `--machine` is refused (a peer credential manages no share links):
// hidden from help so help matches what the verb takes.
#[derive(Debug, clap::Args)]
#[command(
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    mut_arg("machine", |arg| arg.hide(true))
)]
pub(crate) struct ShareArgs {
    #[command(subcommand)]
    command: Option<ShareCommand>,
    /// The session to share: id, unique id prefix, or name.
    session: Option<String>,
    /// How long the link works: 30m, 1h, 8h, 2d (60s to 30d).
    #[arg(long, default_value = "1h", value_name = "DURATION")]
    expires: String,
    /// A label for the link (as `share ls` and `share revoke` name it).
    #[arg(long, value_name = "LABEL")]
    name: Option<String>,
    /// Also draw the most remote-friendly link as a QR code.
    #[arg(long)]
    qr: bool,
    /// Print JSON: {"share","credential","path","urls"}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Subcommand)]
enum ShareCommand {
    /// List live share links (never their secrets): session, label,
    /// expiry, viewers watching now.
    Ls(ShareLsArgs),
    /// Revoke a share link by id or label (--all: every one, or every one
    /// of --session): it stops working and its open views close at once.
    Revoke(ShareRevokeArgs),
}

#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct ShareLsArgs {
    /// Print JSON: {"shares":[...]}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct ShareRevokeArgs {
    /// The share's id or label.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    which: Option<String>,
    /// Revoke every live share link.
    #[arg(long)]
    all: bool,
    /// With --all: only this session's links.
    #[arg(long, requires = "all", value_name = "SESSION")]
    session: Option<String>,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn share(args: &ShareArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        Some(ShareCommand::Ls(a)) => share_ls(a, out),
        Some(ShareCommand::Revoke(a)) => share_revoke(a, out),
        None => share_new(args, out, err),
    };
    match result {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn no_machine(endpoint: &EndpointArgs) -> Result<(), String> {
    match &endpoint.machine {
        Some(machine) => Err(format!(
            "share links have no --machine: share {machine}'s sessions from that machine \
             (`fw hub share` there)"
        )),
        None => Ok(()),
    }
}

/// The running hub `endpoint` names, or `None` (no `--url` and no hub
/// running for the state dir): the shares file is read or edited
/// directly.
fn share_hub(endpoint: &EndpointArgs) -> Result<(PathBuf, Option<Endpoint>), String> {
    no_machine(endpoint)?;
    let dir = resolve_state_dir(&endpoint.state)?;
    if endpoint.url.is_some() {
        return Ok((dir, Some(Endpoint::resolve(endpoint)?)));
    }
    let hub = Endpoint::running(&dir)?;
    Ok((dir, hub))
}

/// Where a link under `path` is opened: every origin of a hub on this
/// machine, else the hub's own URL.
fn link_targets(hub: &Endpoint) -> Vec<LoginTarget> {
    match hub.local_info() {
        Some(info) => hub_targets(info).0,
        None => vec![LoginTarget {
            label: "url".to_owned(),
            base: hub.base().trim_end_matches('/').to_owned(),
            remote: true,
        }],
    }
}

fn share_new(args: &ShareArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let Some(query) = &args.session else {
        return Err(
            "name a session to share (`fw hub share SESSION`), or `share ls` / `share revoke`"
                .into(),
        );
    };
    no_machine(&args.endpoint)?;
    crate::share::parse_ttl(&args.expires)?;
    let hub = Endpoint::resolve(&args.endpoint)?;
    let (id, session) = find_session(&hub, query)?;
    let mut body = json!({ "session_id": id, "expires": args.expires });
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    let mut made = hub.blocking_call("POST", "/api/shares", Some(&body))?;
    let path = str_field(&made, "path").to_owned();
    let targets = link_targets(&hub);
    if args.json {
        made["urls"] = targets
            .iter()
            .map(|t| json!({ "label": t.label, "url": format!("{}{path}", t.base) }))
            .collect();
        let _ = writeln!(out, "{made}");
        return Ok(0);
    }
    let share = &made["share"];
    let label = share["name"]
        .as_str()
        .map(|n| format!(" {n:?}"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "read-only link{label} to {} ({}), until {}:",
        str_field(&session, "name"),
        str_field(share, "id"),
        when(share["expires"].as_u64())
    );
    let width = targets.iter().map(|t| t.label.len()).max().unwrap_or(0);
    for target in &targets {
        let _ = writeln!(out, "  {:width$}  {}{path}", target.label, target.base);
    }
    let _ = writeln!(
        err,
        "fw hub: whoever opens it watches this terminal -- everything it shows, nothing redacted -- \
         and cannot type. Shown once; `fw hub share revoke {}` ends it.",
        str_field(share, "id")
    );
    if args.qr {
        let qr = if std::io::stdout().is_terminal() {
            Qr::Draw
        } else {
            Qr::NotATerminal
        };
        draw_qr(out, err, &targets, &path, qr);
    }
    Ok(0)
}

fn draw_qr(out: &mut dyn Write, err: &mut dyn Write, targets: &[LoginTarget], path: &str, qr: Qr) {
    let Some(remote) = targets.iter().find(|t| t.remote) else {
        let _ = writeln!(
            err,
            "fw hub: no QR code: the hub listens on loopback only, which a phone cannot reach \
             (use `tailscale serve`, --public-url, or `fw hub serve --bind`)"
        );
        return;
    };
    if qr == Qr::NotATerminal {
        let _ = writeln!(out, "(QR code not drawn: output is not a terminal)");
        return;
    }
    match qr_lines(&format!("{}{path}", remote.base)) {
        Ok(lines) => {
            let _ = writeln!(out, "\nscan to watch ({}):", remote.label);
            for line in lines {
                let _ = writeln!(out, "  {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: {e}");
        }
    }
}

fn share_ls(args: &ShareLsArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (dir, hub) = share_hub(&args.endpoint)?;
    let list = match hub {
        Some(hub) => hub.blocking_call("GET", "/api/shares", None)?,
        None => {
            let shares = crate::share::list_offline(&dir).map_err(|e| e.to_string())?;
            json!({ "shares": shares })
        }
    };
    if args.json {
        let _ = writeln!(out, "{list}");
        return Ok(0);
    }
    let shares = list["shares"].as_array().cloned().unwrap_or_default();
    if shares.is_empty() {
        let _ = writeln!(out, "no live share links (fw hub share SESSION)");
    }
    for share in shares {
        let session = share["session"]
            .as_str()
            .map_or_else(|| str_field(&share, "session_id").to_owned(), str::to_owned);
        let viewers = share["viewers"].as_u64().unwrap_or(0);
        let _ = writeln!(
            out,
            "{}  {:<20} {:<20} until {}{}",
            str_field(&share, "id"),
            session,
            share["name"].as_str().unwrap_or("-"),
            when(share["expires"].as_u64()),
            if viewers > 0 {
                format!("  {viewers} watching")
            } else {
                String::new()
            }
        );
    }
    Ok(0)
}

fn share_revoke(args: &ShareRevokeArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (dir, hub) = share_hub(&args.endpoint)?;
    let revoked = match hub {
        Some(hub) => {
            let path = if args.all {
                match &args.session {
                    Some(query) => {
                        let (id, _) = find_session(&hub, query)?;
                        format!("/api/shares?session={}", query_escape(&id))
                    }
                    None => "/api/shares".to_owned(),
                }
            } else {
                format!(
                    "/api/shares/{}",
                    query_escape(args.which.as_deref().unwrap_or_default())
                )
            };
            hub.blocking_call("DELETE", &path, None)?["revoked"].clone()
        }
        None => {
            let store = crate::share::ShareStore::load(&dir);
            let revoked = if args.all {
                if args.session.is_some() {
                    return Err(
                        "--session needs the running hub (it resolves the session); \
                         without one, revoke by id"
                            .into(),
                    );
                }
                store.revoke_all(None)
            } else {
                store.revoke(args.which.as_deref().unwrap_or_default())
            };
            serde_json::to_value(revoked.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
        }
    };
    let revoked = revoked.as_array().cloned().unwrap_or_default();
    if revoked.is_empty() {
        let _ = writeln!(out, "no live share links to revoke");
    }
    for share in revoked {
        let _ = writeln!(
            out,
            "revoked share {}{}",
            str_field(&share, "id"),
            share["name"]
                .as_str()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        );
    }
    Ok(0)
}

// --- record ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct RecordArgs {
    #[command(subcommand)]
    command: RecordCommand,
}

#[derive(Debug, clap::Subcommand)]
enum RecordCommand {
    /// Start recording a session (it must be running): its screen now,
    /// then every byte of output with its timing, and every resize, to
    /// <state-dir>/recordings/<session>-<when>.cast. Survives `fw hub
    /// upgrade`; ends when stopped, when the process exits, or the hub
    /// stops.
    Start(RecordTarget),
    /// Stop recording a session (the file is complete).
    Stop(RecordTarget),
    /// List recordings, newest first.
    Ls(RecordLsArgs),
    /// Download a recording (default: ./NAME; `-o -` writes stdout).
    Get(RecordGetArgs),
    /// Delete a recording (an active one: stop it first).
    Rm(RecordName),
}

#[derive(Debug, clap::Args)]
struct RecordTarget {
    /// Session id, unique id prefix, or name.
    session: String,
    /// Print JSON: {"recording":{...}}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RecordLsArgs {
    /// Print JSON: {"dir","limits","total_bytes","recordings":[...]}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RecordGetArgs {
    /// The recording's file name (as `record ls` lists it).
    name: String,
    /// Where to write it (`-`: stdout).
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Overwrite an existing file.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RecordName {
    /// The recording's file name (as `record ls` lists it).
    name: String,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn record(args: &RecordArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        RecordCommand::Start(a) => record_toggle(a, true, out),
        RecordCommand::Stop(a) => record_toggle(a, false, out),
        RecordCommand::Ls(a) => record_ls(a, out),
        RecordCommand::Get(a) => record_get(a, out, err),
        RecordCommand::Rm(a) => record_rm(a, out),
    };
    match result {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn record_toggle(args: &RecordTarget, start: bool, out: &mut dyn Write) -> Result<i32, String> {
    let hub = Endpoint::resolve(&args.endpoint)?;
    let (id, session) = find_session(&hub, &args.session)?;
    let verb = if start { "start" } else { "stop" };
    let answer = hub.blocking_call(
        "POST",
        &format!("/api/sessions/{}/record/{verb}", query_escape(&id)),
        Some(&json!({})),
    )?;
    if args.json {
        let _ = writeln!(out, "{answer}");
        return Ok(0);
    }
    let recording = &answer["recording"];
    let name = str_field(&session, "name");
    let file = str_field(recording, "name");
    let line = if !start {
        format!(
            "stopped recording {name}: {file} ({})",
            size(recording["size"].as_u64().unwrap_or(0))
        )
    } else if answer["started"].as_bool() == Some(true) {
        format!("recording {name} to {file}")
    } else {
        format!("already recording {name} to {file}")
    };
    let _ = writeln!(out, "{line}");
    Ok(0)
}

fn record_ls(args: &RecordLsArgs, out: &mut dyn Write) -> Result<i32, String> {
    let hub = Endpoint::resolve(&args.endpoint)?;
    let list = hub.blocking_call("GET", "/api/recordings", None)?;
    if args.json {
        let _ = writeln!(out, "{list}");
        return Ok(0);
    }
    let recordings = list["recordings"].as_array().cloned().unwrap_or_default();
    if recordings.is_empty() {
        let _ = writeln!(out, "no recordings (fw hub record start SESSION)");
        return Ok(0);
    }
    for rec in &recordings {
        let _ = writeln!(
            out,
            "{:<52} {:>7} {:>10}  {}  {}{}",
            str_field(rec, "name"),
            format!("part {}", rec["part"].as_u64().unwrap_or(1)),
            size(rec["size"].as_u64().unwrap_or(0)),
            when(rec["started"].as_u64()),
            rec["title"].as_str().unwrap_or("-"),
            if rec["active"].as_bool() == Some(true) {
                "  [recording]"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(
        out,
        "{} in {} (cap {}, {} per part)",
        size(list["total_bytes"].as_u64().unwrap_or(0)),
        list["dir"].as_str().unwrap_or("-"),
        size(list["limits"]["max_total_bytes"].as_u64().unwrap_or(0)),
        size(list["limits"]["max_file_bytes"].as_u64().unwrap_or(0)),
    );
    Ok(0)
}

fn record_get(
    args: &RecordGetArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    if !crate::record::valid_name(&args.name) {
        return Err(format!(
            "{:?} is not a recording name (fw hub record ls)",
            args.name
        ));
    }
    let hub = Endpoint::resolve(&args.endpoint)?;
    let bytes = hub.blocking_download(&format!("/api/recordings/{}", query_escape(&args.name)))?;
    let target = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(&args.name));
    if target.as_os_str() == "-" {
        out.write_all(&bytes)
            .map_err(|e| format!("writing stdout: {e}"))?;
        return Ok(0);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    // A recording holds whatever its terminal showed: as private as the
    // hub's own copy.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if args.force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} exists (--force overwrites it)", target.display())
        } else {
            format!("{}: {e}", target.display())
        }
    })?;
    #[cfg(unix)]
    {
        // An existing file --force overwrites keeps no wider mode.
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(&bytes)
        .map_err(|e| format!("{}: {e}", target.display()))?;
    let _ = writeln!(
        err,
        "fw hub: wrote {} ({}); play it with `asciinema play {}` or the hub's replay player",
        target.display(),
        size(bytes.len() as u64),
        target.display()
    );
    Ok(0)
}

fn record_rm(args: &RecordName, out: &mut dyn Write) -> Result<i32, String> {
    if !crate::record::valid_name(&args.name) {
        return Err(format!(
            "{:?} is not a recording name (fw hub record ls)",
            args.name
        ));
    }
    let hub = Endpoint::resolve(&args.endpoint)?;
    hub.blocking_call(
        "DELETE",
        &format!("/api/recordings/{}", query_escape(&args.name)),
        None,
    )?;
    let _ = writeln!(out, "deleted {}", args.name);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_in_utc_with_how_far_off_they_are() {
        assert_eq!(relative(30), "now");
        assert_eq!(relative(57 * 60 + 5), "in 57m");
        assert_eq!(relative(-(3 * 3600 + 4 * 60)), "3h04m ago");
        assert_eq!(relative(2 * 86_400 + 3600), "in 2d1h");
        assert_eq!(when(None), "-");
        assert!(when(Some(0)).starts_with("1970-01-01 00:00 UTC ("));
    }
}
