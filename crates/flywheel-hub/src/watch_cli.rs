//! `fw hub watch add|ls|rm`: watches (see [`crate::watch`]) that wake an
//! agent session with a queued prompt when the hub's event log matches a
//! filter.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::{Read, Write};

#[derive(Debug, clap::Args)]
pub(crate) struct WatchArgs {
    #[command(subcommand)]
    command: WatchCommand,
}

#[derive(Debug, clap::Subcommand)]
enum WatchCommand {
    /// Arm a watch: when an event matches FILTER, PROMPT (then the events
    /// and their cursors) is queued into SESSION; matches within the
    /// window, and any while that prompt waits, join it.
    Add(AddArgs),
    /// Every armed watch (or SESSION's): its filter, prompts queued, and
    /// events matched.
    Ls(LsArgs),
    /// Disarm a watch (a prompt it queued stays queued).
    Rm(RmArgs),
}

#[derive(Debug, clap::Args)]
struct AddArgs {
    /// The agent session it wakes (id, unique id prefix, or name).
    session: String,
    /// What to match (`state=done|blocked|failed children`, `team=web
    /// kind=review`; see `fw hub tail`). Its own events never match.
    #[arg(long = "on", value_name = "FILTER", required_unless_present_any = ["at", "every"])]
    on: Option<String>,
    /// Wake it once at this time instead (2026-11-23T12:00Z, or +36h).
    #[arg(long, value_name = "TIME", conflicts_with = "on")]
    at: Option<String>,
    /// Wake it this often instead (6h, 1d; at least 5m); with --at, from
    /// then.
    #[arg(long, value_name = "SPAN", conflicts_with = "on")]
    every: Option<String>,
    /// What the session is told when it fires (`-`: read stdin).
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    prompt: String,
    /// Gather matches this many seconds before queueing them (default 3).
    #[arg(long, value_name = "SECS")]
    window: Option<u32>,
    /// Match events after this cursor (default: from now).
    #[arg(long, value_name = "CURSOR")]
    after: Option<String>,
    /// Print JSON instead of its id.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Only this session's watches.
    session: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RmArgs {
    /// The watch (its id or a prefix of 4+).
    watch: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &WatchArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn run(args: &WatchArgs, out: &mut dyn Write) -> Result<(), String> {
    match &args.command {
        WatchCommand::Add(add) => {
            let prompt = if add.prompt == "-" {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| format!("cannot read stdin: {e}"))?;
                text
            } else {
                add.prompt.clone()
            };
            let on = add.on.clone().unwrap_or_default();
            crate::eventlog::Filter::parse(&on)?;
            let conn = Endpoint::resolve(&add.endpoint)?;
            let mut body = json!({ "owner": add.session, "filter": on, "prompt": prompt,
                                   "at": add.at, "every": add.every });
            if let Some(window) = add.window {
                body["window_secs"] = json!(window);
            }
            if let Some(after) = &add.after {
                body["after"] = json!(after);
            }
            let watch = conn.blocking_call("POST", "/api/watches", Some(&body))?;
            if add.json {
                print_json(out, &watch);
            } else {
                let _ = writeln!(out, "{}", str_field(&watch, "id"));
            }
        }
        WatchCommand::Ls(ls) => {
            let conn = Endpoint::resolve(&ls.endpoint)?;
            let path = match &ls.session {
                Some(session) => format!("/api/watches?session={}", query_escape(session)),
                None => "/api/watches".to_owned(),
            };
            let listed = conn.blocking_call("GET", &path, None)?;
            if ls.json {
                print_json(out, &listed);
            } else {
                print_watches(out, &listed, &conn);
            }
        }
        WatchCommand::Rm(rm) => {
            let conn = Endpoint::resolve(&rm.endpoint)?;
            let removed = conn.blocking_call(
                "DELETE",
                &format!("/api/watches/{}", query_escape(rm.watch.trim())),
                None,
            )?;
            if rm.json {
                print_json(out, &removed);
            } else {
                let _ = writeln!(out, "disarmed {}", str_field(&removed, "id"));
            }
        }
    }
    Ok(())
}

/// `id  owner  filter  fired N, M events`, one line per watch.
fn print_watches(out: &mut dyn Write, listed: &Value, conn: &Endpoint) {
    let watches = listed["watches"].as_array().cloned().unwrap_or_default();
    if watches.is_empty() {
        let _ = writeln!(
            out,
            "no watches (fw hub watch add SESSION --on FILTER --prompt TEXT)"
        );
        return;
    }
    let names: Vec<Value> = conn.blocking_sessions().unwrap_or_default();
    let name_of = |id: &str| {
        names.iter().find(|s| str_field(s, "id") == id).map_or_else(
            || id.chars().take(8).collect(),
            |s| str_field(s, "name").to_owned(),
        )
    };
    for watch in watches {
        let filter = &watch["filter"];
        let shown = match watch.get("time").filter(|t| t.is_object()) {
            Some(time) => format!(
                "{}next {}",
                time["every_secs"]
                    .as_u64()
                    .map(|s| format!("every {}s, ", s))
                    .unwrap_or_default(),
                str_field(time, "next")
            ),
            None => filter_text(filter),
        };
        let _ = writeln!(
            out,
            "{}  {}  {}  fired {}, {} events{}",
            str_field(&watch, "id").chars().take(8).collect::<String>(),
            name_of(str_field(&watch, "owner")),
            shown,
            watch["fired"].as_u64().unwrap_or(0),
            watch["events"].as_u64().unwrap_or(0),
            if watch["batch"].is_object() {
                ", a prompt waiting"
            } else {
                ""
            }
        );
    }
}

/// A stored filter back as the words that make it.
fn filter_text(filter: &Value) -> String {
    serde_json::from_value::<crate::eventlog::Filter>(filter.clone())
        .map(|f| f.to_string())
        .unwrap_or_default()
}
