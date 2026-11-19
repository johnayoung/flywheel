//! `fw hub tail`: the hub's event log (see [`crate::telemetry::log`]),
//! filtered, from a cursor; `--follow` keeps reading as events arrive
//! (long polls, never a busy loop). Against a collector (`--url`), every
//! hub that reports to it.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{fail, print_json, str_field};
use serde_json::Value;
use std::io::Write;
use std::time::Duration;

/// How long one follow read waits on the hub.
const FOLLOW_WAIT_SECS: u64 = 25;

#[derive(Debug, clap::Args)]
pub(crate) struct TailArgs {
    /// Only this session's events (id, unique id prefix, or name);
    /// repeatable.
    #[arg(long, value_name = "SESSION")]
    session: Vec<String>,
    /// Only the events of this workspace's sessions; repeatable.
    #[arg(long, value_name = "WS")]
    ws: Vec<String>,
    /// Only events that moved their session to this status (blocked,
    /// asking, failed, working, done, idle, ended, ...); repeatable.
    #[arg(long, value_name = "STATE")]
    state: Vec<String>,
    /// Only sessions whose labels hold KEY or KEY=V1|V2 (now, or on
    /// either side of a label change); repeatable.
    #[arg(long = "label", value_name = "KEY[=VALUE]")]
    labels: Vec<String>,
    /// Only this kind (`review.merged`) or family (`review`); repeatable.
    #[arg(long, value_name = "KIND")]
    kind: Vec<String>,
    /// Only events after this cursor (each event and every read print
    /// one); `now`: from here on. Default: the newest --limit.
    #[arg(long, value_name = "CURSOR")]
    after: Option<String>,
    /// Keep reading as events arrive (Ctrl-C ends).
    #[arg(long, short = 'f')]
    follow: bool,
    /// The most events one read prints.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: u32,
    /// Only one hub's events (its id; `local`: the hub asked).
    #[arg(long, value_name = "HUB_ID")]
    host: Option<String>,
    /// JSON: one read as {"events","cursor","more"}; with --follow, an
    /// event per line, each with its cursor.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn tail(args: &TailArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out, err) {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

/// The query string for one read from `after`.
fn query(args: &TailArgs, after: Option<&str>, wait: Option<u64>) -> Result<String, String> {
    let mut params = vec![format!("limit={}", args.limit)];
    let list = |name: &str, values: &[String], params: &mut Vec<String>| {
        if !values.is_empty() {
            params.push(format!("{name}={}", query_escape(&values.join(","))));
        }
    };
    list("session", &args.session, &mut params);
    list("ws", &args.ws, &mut params);
    list("state", &args.state, &mut params);
    list("kind", &args.kind, &mut params);
    if !args.labels.is_empty() {
        let terms = crate::labels::parse_terms(&args.labels).map_err(|e| e.to_string())?;
        params.push(format!(
            "label={}",
            query_escape(&crate::labels::query_value(&terms))
        ));
    }
    if let Some(host) = &args.host {
        params.push(format!("host={}", query_escape(host)));
    }
    if let Some(after) = after {
        params.push(format!("after={}", query_escape(after)));
    }
    if let Some(wait) = wait {
        params.push(format!("wait={wait}"));
    }
    Ok(format!("/api/log?{}", params.join("&")))
}

fn run(args: &TailArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let page = conn.blocking_call("GET", &query(args, args.after.as_deref(), None)?, None)?;
    if !args.follow {
        if args.json {
            print_json(out, &page);
        } else {
            print_events(out, &page);
            let _ = writeln!(
                err,
                "next: fw hub tail --after {}{}",
                str_field(&page, "cursor"),
                if page["more"] == true {
                    " (more waiting)"
                } else {
                    ""
                }
            );
        }
        return Ok(());
    }
    let conn = conn.with_timeout(Duration::from_secs(FOLLOW_WAIT_SECS + 15));
    let mut page = page;
    loop {
        if args.json {
            for event in page["events"].as_array().into_iter().flatten() {
                let _ = writeln!(out, "{event}");
            }
        } else {
            print_events(out, &page);
        }
        if out.flush().is_err() {
            return Ok(());
        }
        let cursor = str_field(&page, "cursor").to_owned();
        let wait = (page["more"] != true).then_some(FOLLOW_WAIT_SECS);
        page = conn.blocking_call("GET", &query(args, Some(&cursor), wait)?, None)?;
    }
}

/// `HH:MM:SS  name  kind  -> state  what`, one line per event.
fn print_events(out: &mut dyn Write, page: &Value) {
    for event in page["events"].as_array().into_iter().flatten() {
        let when = event["ts"]
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        let who = event
            .get("name")
            .and_then(Value::as_str)
            .map_or_else(|| short(str_field(event, "session")), str::to_owned);
        let moved = event
            .get("state")
            .and_then(Value::as_str)
            .map(|s| format!("  -> {s}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{when}  {who}  {}{moved}  {}",
            str_field(event, "kind"),
            what(&event["body"])
        );
    }
}

/// A value in a word: a string as is, `-` for null, `..` for nesting.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        Value::Number(_) | Value::Bool(_) => value.to_string(),
        _ => "..".to_owned(),
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A body in one short line: its plain fields, the session object left
/// out (the line names it).
fn what(body: &Value) -> String {
    let Some(map) = body.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (key, value) in map {
        if key == "session" {
            continue;
        }
        let shown = match value {
            Value::Null => continue,
            Value::Object(map) => format!(
                "{{{}}}",
                map.iter()
                    .map(|(k, v)| format!("{k}={}", scalar(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            other => scalar(other),
        };
        parts.push(format!("{key}={shown}"));
    }
    let line = crate::commands_cli::visible_controls(&parts.join(" "));
    if line.chars().count() > 120 {
        format!("{}...", line.chars().take(117).collect::<String>())
    } else {
        line
    }
}
