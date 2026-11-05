//! `fw hub digest`: the running hub's digest since the last one (or
//! `--since`), previewed or sent now (see [`crate::digest`]).

use crate::digest::Report;
use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub struct DigestArgs {
    /// From when: an age (`12h`, `7d`), an RFC 3339 time, or unix ms.
    /// Default: where the last digest ended (24 hours before the first).
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,
    /// Send it now over the configured channels (`[digest] channels`),
    /// even with nothing in it; the next scheduled one starts where it
    /// ends. Without it: a preview, neither sent nor kept.
    #[arg(long)]
    send: bool,
    /// The digest as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// Run `fw hub digest`.
pub fn run(args: &DigestArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match fetch(args).and_then(|value| render(args, &value, out)) {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

fn fetch(args: &DigestArgs) -> Result<Value, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    if args.send {
        let body = match &args.since {
            Some(since) => json!({ "since": since }),
            None => json!({}),
        };
        conn.blocking_call("POST", "/api/digest/send", Some(&body))
    } else {
        let path = match &args.since {
            Some(since) => format!("/api/digest/preview?since={}", query_escape(since)),
            None => "/api/digest/preview".to_owned(),
        };
        conn.blocking_call("GET", &path, None)
    }
}

fn render(args: &DigestArgs, value: &Value, out: &mut dyn Write) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    if args.json {
        let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
        return writeln!(out, "{text}").map_err(io);
    }
    let report: Report = serde_json::from_value(value.clone())
        .map_err(|e| format!("the hub answered a digest this fw cannot read: {e}"))?;
    let tz = report
        .timezone
        .parse::<chrono_tz::Tz>()
        .unwrap_or(chrono_tz::Tz::UTC);
    // Names come from the hub: control bytes are shown as spaces.
    let text: String = report
        .text(&tz)
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    write!(out, "{text}").map_err(io)
}
