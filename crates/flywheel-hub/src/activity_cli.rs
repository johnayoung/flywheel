//! `fw hub activity`: the "while you were away" digest and the activity
//! timeline of the running hub (and its machines), as the web UI's
//! Activity view shows them.

use crate::endpoint::{Endpoint, EndpointArgs, stable_client_id};
use crate::script::{query_escape, resolve};
use crate::util::str_field;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub struct ActivityArgs {
    /// From when: an age (`30m`, `2h`, `7d`), an RFC 3339 time, or
    /// `last-seen` (since this CLI last ran with it on this machine; the
    /// last 24 hours the first time), which then marks now as seen.
    #[arg(long, default_value = "24h", value_name = "WHEN")]
    since: String,
    /// Until when (default now).
    #[arg(long, value_name = "WHEN")]
    until: Option<String>,
    /// Only this workspace (id, unique id prefix, or name).
    #[arg(long = "workspace", visible_alias = "ws", value_name = "WS")]
    workspace: Option<String>,
    /// Only this session (id, unique id prefix, or name).
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
    /// Only rows of this kind (`turn.finished`, `review` for every
    /// `review.*`, ...; repeatable).
    #[arg(long, value_name = "KIND")]
    kind: Vec<String>,
    /// Timeline rows listed (0: the digest only).
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(0..=500))]
    limit: u16,
    /// The hub's JSON answers: `{"digest","activity"}`.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// Run `fw hub activity`.
pub fn run(args: &ActivityArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match fetch(args).and_then(|(digest, activity)| render(args, &digest, &activity, out)) {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

fn fetch(args: &ActivityArgs) -> Result<(Value, Value), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let workspace = match &args.workspace {
        Some(wanted) => {
            let workspaces = conn.blocking_workspaces()?;
            Some(str_field(resolve(&workspaces, wanted, "workspace")?, "id").to_owned())
        }
        None => None,
    };
    let session = match &args.session {
        Some(wanted) => {
            let sessions = conn.blocking_sessions()?;
            Some(str_field(resolve(&sessions, wanted, "session")?, "id").to_owned())
        }
        None => None,
    };
    let last_seen = args.since == "last-seen";
    let client = stable_client_id("cli");
    let mut scope = String::new();
    if let Some(until) = &args.until {
        scope.push_str(&format!("&until={}", query_escape(until)));
    }
    if let Some(ws) = &workspace {
        scope.push_str(&format!("&ws={}", query_escape(ws)));
    }
    if let Some(session) = &session {
        scope.push_str(&format!("&session={}", query_escape(session)));
    }
    let digest_path = if last_seen {
        format!(
            "/api/activity/digest?since=last-seen&client={}{scope}",
            query_escape(&client)
        )
    } else {
        format!(
            "/api/activity/digest?since={}{scope}",
            query_escape(&args.since)
        )
    };
    let digest = conn.blocking_call("GET", &digest_path, None)?;
    let activity = if args.limit == 0 {
        Value::Null
    } else {
        // The timeline over exactly the digest's span.
        let mut path = format!(
            "/api/activity?limit={}&since={}{}",
            args.limit,
            query_escape(str_field(&digest, "since")),
            scope
        );
        if args.until.is_none() {
            path.push_str(&format!(
                "&until={}",
                query_escape(str_field(&digest, "until"))
            ));
        }
        if !args.kind.is_empty() {
            path.push_str(&format!("&kind={}", query_escape(&args.kind.join(","))));
        }
        conn.blocking_call("GET", &path, None)?
    };
    if last_seen {
        conn.blocking_call(
            "POST",
            "/api/activity/seen",
            Some(&json!({ "client": client })),
        )?;
    }
    Ok((digest, activity))
}

/// A string safe for a terminal (agent- or user-typed control bytes as
/// spaces).
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn local_time(raw: &str, format: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(at) => at.with_timezone(&chrono::Local).format(format).to_string(),
        Err(_) => plain(raw),
    }
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The digest as a few lines.
pub fn digest_lines(digest: &Value) -> Vec<String> {
    let mut lines = vec![format!(
        "since {} until {}",
        local_time(str_field(digest, "since"), "%Y-%m-%d %H:%M"),
        local_time(str_field(digest, "until"), "%Y-%m-%d %H:%M")
    )];
    let blocked = digest
        .get("blocked_now")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fanouts = digest
        .get("fanouts_graded")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cost = digest
        .get("cost_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if digest.get("empty").and_then(Value::as_bool) == Some(true) {
        lines.push("  nothing happened, nobody is blocked".into());
    } else {
        lines.push(format!(
            "  {} · {} blocked now · {} · {} · ${cost:.2}",
            plural(
                count(digest, "turns_finished"),
                "turn finished",
                "turns finished"
            ),
            blocked.len(),
            plural(fanouts.len() as u64, "fan-out graded", "fan-outs graded"),
            plural(
                count(digest, "commands_suggested"),
                "command suggested",
                "commands suggested"
            ),
        ));
    }
    let on = |item: &Value| match item.get("machine").and_then(Value::as_str) {
        Some(m) => format!(" (on {})", plain(m)),
        None => String::new(),
    };
    for item in &blocked {
        let detail = item
            .get("detail")
            .and_then(Value::as_str)
            .map(|d| format!(": {}", plain(d)))
            .unwrap_or_default();
        lines.push(format!(
            "  blocked  {} / {}{}{detail}",
            plain(str_field(item, "workspace")),
            plain(str_field(item, "session")),
            on(item)
        ));
    }
    for item in &fanouts {
        let lead = match (
            item.get("winner").and_then(Value::as_str),
            item.get("leader").and_then(Value::as_str),
        ) {
            (Some(w), _) => format!("winner {}", plain(w)),
            (None, Some(l)) => format!("{} leads", plain(l)),
            (None, None) => "none passing".into(),
        };
        lines.push(format!(
            "  fan-out  {}: {lead} ({}/{} passing){}",
            plain(str_field(item, "name")),
            count(item, "passing"),
            count(item, "candidates"),
            on(item)
        ));
    }
    for machine in digest
        .get("machines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if machine.get("ok").and_then(Value::as_bool) == Some(false) {
            lines.push(format!(
                "  machine {} not included: {}",
                plain(str_field(machine, "name")),
                plain(str_field(machine, "error"))
            ));
        }
    }
    lines
}

/// One timeline row.
pub fn row_line(row: &Value) -> String {
    let machine = row
        .get("machine")
        .and_then(Value::as_str)
        .map(|m| format!("{}: ", plain(m)))
        .unwrap_or_default();
    let place = match (
        row.get("workspace").and_then(Value::as_str),
        row.get("session").and_then(Value::as_str),
    ) {
        (Some(w), Some(s)) => format!("{} / {}", plain(w), plain(s)),
        (Some(w), None) => plain(w),
        (None, Some(s)) => plain(s),
        (None, None) => "hub".into(),
    };
    format!(
        "{}  {machine}{place}  {}",
        local_time(str_field(row, "at"), "%m-%d %H:%M"),
        plain(str_field(row, "text"))
    )
}

fn render(
    args: &ActivityArgs,
    digest: &Value,
    activity: &Value,
    out: &mut dyn Write,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    if args.json {
        let doc = json!({ "digest": digest, "activity": activity });
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&doc).unwrap_or_default()
        )
        .map_err(io)?;
        return Ok(());
    }
    for line in digest_lines(digest) {
        writeln!(out, "{line}").map_err(io)?;
    }
    let rows = activity
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !rows.is_empty() {
        writeln!(out).map_err(io)?;
        for row in &rows {
            writeln!(out, "{}", row_line(row)).map_err(io)?;
        }
        if activity.get("next").is_some_and(|n| !n.is_null()) {
            writeln!(out, "... more (raise --limit, or --json for the cursor)").map_err(io)?;
        }
    }
    Ok(())
}
