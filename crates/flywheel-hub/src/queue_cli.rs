//! `fw hub queue add|ls|rm|clear|move|pause|resume`: per-session prompt
//! queues (see [`crate::queue`]) through the running hub (or, with
//! `--machine`, its proxy to the hub that owns the session).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{find_session, session_label};
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::{Read, Write};

#[derive(Debug, clap::Args)]
pub(crate) struct QueueArgs {
    #[command(subcommand)]
    command: QueueCommand,
}

#[derive(Debug, clap::Subcommand)]
enum QueueCommand {
    /// Queue a prompt for an agent session: typed in (bracketed paste,
    /// then Enter) once the agent is done -- never while it is blocked on
    /// a prompt, someone types into it, or a fan-out grading or review
    /// action uses it.
    Add(AddArgs),
    /// A session's queue (or, without SESSION, every queue, each with its
    /// latest 3 sent): pending items in order, delivered ones with their
    /// time, and why the next one waits.
    Ls(LsArgs),
    /// Remove an item (by position or id prefix).
    Rm(ItemArgs),
    /// Remove every pending item (--all: the delivered history too).
    Clear(ClearArgs),
    /// Move a pending item to POSITION (1 = next).
    Move(MoveArgs),
    /// Deliver nothing from this queue until resumed.
    Pause(SessionOnly),
    /// Deliver again; also clears a stall (the next item is tried again).
    Resume(SessionOnly),
}

#[derive(Debug, clap::Args)]
struct AddArgs {
    /// The agent session (id, unique id prefix, or name).
    session: String,
    /// The prompt (`-`: read stdin).
    #[arg(allow_hyphen_values = true)]
    text: String,
    /// done: once its turn finished (default); idle: once someone has
    /// also looked at the answer; blocked-cleared: once a permission
    /// prompt it hits is answered and that turn finished.
    #[arg(long, value_parser = ["done", "idle", "blocked-cleared"], default_value = "done")]
    when: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// The agent session (id, unique id prefix, or name); default: every
    /// session with a queue.
    session: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ItemArgs {
    /// The agent session (id, unique id prefix, or name).
    session: String,
    /// A pending item's position (1 = next) or any item's id prefix.
    item: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ClearArgs {
    /// The agent session (id, unique id prefix, or name).
    session: String,
    /// Also forget the delivered items.
    #[arg(long)]
    all: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct MoveArgs {
    /// The agent session (id, unique id prefix, or name).
    session: String,
    /// A pending item's position (1 = next) or any item's id prefix.
    item: String,
    /// Where it goes among the pending items (1 = next; past the end =
    /// last).
    position: usize,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct SessionOnly {
    /// The agent session (id, unique id prefix, or name).
    session: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &QueueArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

/// An item reference as a path segment: a position or a hex id prefix.
fn item_ref(raw: &str) -> Result<String, String> {
    let reference = raw.trim().trim_start_matches('#');
    if reference.is_empty() || !reference.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "bad item {raw:?}: give its position (1 = next) or its id"
        ));
    }
    Ok(reference.to_ascii_lowercase())
}

/// When `iso` was, as local `HH:MM:SS` (the date too when not today).
fn local_time(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(at) => {
            let local = at.with_timezone(&chrono::Local);
            if local.date_naive() == chrono::Local::now().date_naive() {
                local.format("%H:%M:%S").to_string()
            } else {
                local.format("%Y-%m-%d %H:%M").to_string()
            }
        }
        Err(_) => iso.to_owned(),
    }
}

/// The first line of `text`, cut to `max` characters.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let more = text.lines().nth(1).is_some();
    let mut shown: String = line.chars().take(max).collect();
    if more || line.chars().count() > max {
        shown.push_str(" ...");
    }
    crate::commands_cli::visible_controls(&shown)
}

/// Sent items each queue shows in the every-queue listing.
const OVERVIEW_SENT: usize = 3;

/// A queue for a terminal: every sent item, or (`sent_shown`) only the
/// latest few.
fn print_queue(out: &mut dyn Write, view: &Value, sent_shown: Option<usize>) {
    let pending = view["pending"].as_u64().unwrap_or(0);
    let mut head = format!("{} -- {pending} pending", str_field(view, "session"));
    if view["paused"] == true {
        head.push_str(", paused");
    }
    let _ = writeln!(out, "{head}");
    if let Some(stall) = view.get("stalled").filter(|s| s.is_object()) {
        let _ = writeln!(
            out,
            "  STALLED {}: {} (fw hub queue resume to retry)",
            local_time(str_field(stall, "at")),
            str_field(stall, "reason")
        );
    } else if let Some(why) = view.get("waiting").and_then(Value::as_str) {
        let _ = writeln!(out, "  next: {why}");
    }
    let items: Vec<&Value> = view["items"].as_array().into_iter().flatten().collect();
    let sent = items.iter().filter(|i| i["state"] == "delivered").count();
    let mut skip = sent_shown.map_or(0, |n| sent.saturating_sub(n));
    if skip > 0 {
        let _ = writeln!(
            out,
            "  ... {skip} sent before (fw hub queue ls {} shows them)",
            str_field(view, "session")
        );
    }
    for item in items {
        if skip > 0 && item["state"] == "delivered" {
            skip -= 1;
            continue;
        }
        let id = str_field(item, "id");
        let short = &id[..id.len().min(8)];
        let text = first_line(str_field(item, "text"), 60);
        let when = str_field(item, "when");
        let line = match str_field(item, "state") {
            "delivered" => format!(
                "  sent {}  {short}  {text}{}",
                local_time(str_field(item, "delivered_at")),
                if item["reconciled"] == true {
                    "  (found in its transcript after a restart)"
                } else {
                    ""
                }
            ),
            "sending" => format!("  sending  {short}  {text}"),
            _ => format!(
                "  {:>3}.  {short}  [{when}]  {text}",
                item["position"].as_u64().unwrap_or(0)
            ),
        };
        let _ = writeln!(out, "{line}");
    }
}

fn finish(out: &mut dyn Write, json_out: bool, view: &Value) -> Result<i32, String> {
    if json_out {
        print_json(out, view);
    } else {
        print_queue(out, view, None);
    }
    Ok(0)
}

fn run(args: &QueueArgs, out: &mut dyn Write) -> Result<i32, String> {
    match &args.command {
        QueueCommand::Add(a) => {
            let text = if a.text == "-" {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| format!("cannot read stdin: {e}"))?;
                text
            } else {
                a.text.clone()
            };
            let conn = Endpoint::resolve(&a.endpoint)?;
            let (id, session) = find_session(&conn, &a.session)?;
            let added = conn.blocking_call(
                "POST",
                &format!("/api/sessions/{id}/queue"),
                Some(&json!({ "text": text, "when": a.when })),
            )?;
            if a.json {
                print_json(out, &added);
            } else {
                let item = &added["item"];
                let queue = &added["queue"];
                let _ = writeln!(
                    out,
                    "queued {} for {} [{}]: {} pending{}",
                    &str_field(item, "id")[..8.min(str_field(item, "id").len())],
                    session_label(&session),
                    str_field(item, "when"),
                    queue["pending"],
                    queue
                        .get("waiting")
                        .and_then(Value::as_str)
                        .map(|w| format!("; next: {w}"))
                        .unwrap_or_default()
                );
            }
            Ok(0)
        }
        QueueCommand::Ls(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            match &a.session {
                Some(query) => {
                    let (id, _) = find_session(&conn, query)?;
                    let view =
                        conn.blocking_call("GET", &format!("/api/sessions/{id}/queue"), None)?;
                    finish(out, a.json, &view)
                }
                None => {
                    let all = conn.blocking_call("GET", "/api/queues", None)?;
                    if a.json {
                        print_json(out, &all);
                        return Ok(0);
                    }
                    let queues = all.as_array().cloned().unwrap_or_default();
                    if queues.is_empty() {
                        let _ = writeln!(out, "no queues");
                    }
                    for view in &queues {
                        print_queue(out, view, Some(OVERVIEW_SENT));
                    }
                    Ok(0)
                }
            }
        }
        QueueCommand::Rm(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let (id, _) = find_session(&conn, &a.session)?;
            let item = item_ref(&a.item)?;
            let view = conn.blocking_call(
                "DELETE",
                &format!("/api/sessions/{id}/queue/items/{item}"),
                None,
            )?;
            finish(out, a.json, &view)
        }
        QueueCommand::Clear(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let (id, _) = find_session(&conn, &a.session)?;
            let view = conn.blocking_call(
                "POST",
                &format!("/api/sessions/{id}/queue/clear"),
                Some(&json!({ "all": a.all })),
            )?;
            finish(out, a.json, &view)
        }
        QueueCommand::Move(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let (id, _) = find_session(&conn, &a.session)?;
            let item = item_ref(&a.item)?;
            if a.position == 0 {
                return Err("positions start at 1 (the next item)".into());
            }
            let view = conn.blocking_call(
                "POST",
                &format!("/api/sessions/{id}/queue/items/{item}/move"),
                Some(&json!({ "position": a.position })),
            )?;
            finish(out, a.json, &view)
        }
        QueueCommand::Pause(a) | QueueCommand::Resume(a) => {
            let verb = if matches!(args.command, QueueCommand::Pause(_)) {
                "pause"
            } else {
                "resume"
            };
            let conn = Endpoint::resolve(&a.endpoint)?;
            let (id, _) = find_session(&conn, &a.session)?;
            let view = conn.blocking_call(
                "POST",
                &format!("/api/sessions/{id}/queue/{verb}"),
                Some(&json!({})),
            )?;
            finish(out, a.json, &view)
        }
    }
}
