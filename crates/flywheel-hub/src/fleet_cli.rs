//! `fw hub fleet [SESSION]`: the fleet board in a terminal (see
//! [`crate::telemetry`]): every agent session's status, what it is doing,
//! its progress, context window, and cost, most urgent first; with a
//! session, that session's plan and work tree.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{find_session, query_escape};
use crate::util::{dollars, fail, print_json, str_field};
use serde_json::Value;
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct FleetArgs {
    /// One session in full (id, unique id prefix, or name): its plan and
    /// its turns with their subagents. Default: the board.
    session: Option<String>,
    /// Only sessions in this status (blocked, asking, failed, working,
    /// done, idle, ended); repeatable.
    #[arg(long, value_name = "STATUS")]
    status: Vec<String>,
    /// Only sessions with an event in the last N hours (0: any age).
    #[arg(long, value_name = "N", default_value_t = 24)]
    hours: u32,
    /// Only the sessions of the hub with this id (`local`: the hub asked;
    /// a collector lists every hub that reports to it). With SESSION: the
    /// hub that owns it, SESSION then being its id there.
    #[arg(long, value_name = "HUB_ID")]
    host: Option<String>,
    /// The most sessions listed.
    #[arg(long, value_name = "N", default_value_t = 200)]
    limit: u32,
    /// Only the captured runs of one batch (the KEY the board shows:
    /// `batch:LABEL`, `under:SESSION`, or `outside`).
    #[arg(long, value_name = "KEY", conflicts_with = "all")]
    batch: Option<String>,
    /// List captured runs one by one instead of counted in their batches.
    #[arg(long)]
    all: bool,
    /// With SESSION: its newest N turns (default 10), each as where its
    /// time went, the files and commands it touched, cost, and ending.
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "10",
          requires = "session", conflicts_with_all = ["status", "batch", "all", "lanes"])]
    turns: Option<u32>,
    /// The fleet in lanes: needs you, moving, waiting on time, finished
    /// (nobody has looked since), idle; each grouped by project.
    #[arg(long, conflicts_with_all = ["session", "status", "batch", "all", "limit"])]
    lanes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn fleet(args: &FleetArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn run(args: &FleetArgs, out: &mut dyn Write) -> Result<(), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let host = args
        .host
        .as_deref()
        .map(|h| format!("host={}", query_escape(h)));
    if let Some(query) = &args.session {
        // Another hub's session is known here by its id alone.
        let id = match &host {
            Some(_) => query.clone(),
            None => find_session(&conn, query)?.0,
        };
        if let Some(limit) = args.turns {
            let mut params = vec![format!("limit={limit}")];
            params.extend(host);
            let found = conn.blocking_call(
                "GET",
                &format!(
                    "/api/telemetry/sessions/{}/turns?{}",
                    query_escape(&id),
                    params.join("&")
                ),
                None,
            )?;
            if args.json {
                print_json(out, &found);
            } else {
                print_turns(out, &found);
            }
            return Ok(());
        }
        let path = format!(
            "/api/telemetry/sessions/{}{}",
            query_escape(&id),
            host.map(|h| format!("?{h}")).unwrap_or_default()
        );
        let found = conn.blocking_call("GET", &path, None)?;
        if args.json {
            print_json(out, &found);
        } else {
            print_session(out, &found);
        }
        return Ok(());
    }
    if args.lanes {
        let mut params = vec![format!("hours={}", args.hours)];
        params.extend(host);
        let view = conn.blocking_call(
            "GET",
            &format!("/api/telemetry/lanes?{}", params.join("&")),
            None,
        )?;
        if args.json {
            print_json(out, &view);
        } else {
            print_lanes(out, &view);
        }
        return Ok(());
    }
    let mut params = vec![
        format!("hours={}", args.hours),
        format!("limit={}", args.limit),
    ];
    if !args.status.is_empty() {
        params.push(format!("status={}", query_escape(&args.status.join(","))));
    }
    match &args.batch {
        Some(batch) => params.push(format!("batch={}", query_escape(batch))),
        None if !args.all => params.push("rollup=1".to_owned()),
        None => {}
    }
    params.extend(host);
    let page = conn.blocking_call(
        "GET",
        &format!("/api/telemetry/fleet?{}", params.join("&")),
        None,
    )?;
    if args.json {
        print_json(out, &page);
    } else {
        print_board(out, &page);
    }
    Ok(())
}

/// Text from the hub, safe to print: control bytes shown as spaces, cut
/// to `max` characters.
fn shown(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= max {
        return clean;
    }
    let mut cut: String = clean.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// A span of milliseconds in the hub's short form (`42s`, `5m`, `3h`).
fn span(ms: i64) -> String {
    let seconds = (ms / 1000).max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// `4/7 Wire the route` for a declared plan; else what was observed.
fn progress(session: &Value) -> String {
    let plan = &session["plan"];
    if plan.is_object() {
        let current = plan["current"].as_str().unwrap_or_default();
        return format!("{}/{} {current}", plan["done"], plan["total"])
            .trim_end()
            .to_owned();
    }
    let mut parts = Vec::new();
    let calls = session["turn_tools"].as_u64().unwrap_or(0);
    if calls > 0 {
        parts.push(format!("{calls} calls"));
    }
    let subagents = session["subagents_total"].as_u64().unwrap_or(0);
    if subagents > 0 {
        parts.push(format!(
            "{}/{subagents} subagents running",
            session["subagents_active"].as_u64().unwrap_or(0)
        ));
    }
    parts.join(", ")
}

fn status(session: &Value, now: i64) -> String {
    let word = str_field(session, "status");
    let since = session["since"].as_i64().unwrap_or(now);
    match session["blocked_on"]
        .as_str()
        .or_else(|| session["ask"]["kind"].as_str())
    {
        Some(on) => format!("{word} {} ({on})", span(now - since)),
        None => format!("{word} {}", span(now - since)),
    }
}

fn context(session: &Value) -> String {
    match session["context"]["pct"].as_f64() {
        Some(pct) if session["context"]["exact"] == false => format!("~{pct:.0}%"),
        Some(pct) => format!("{pct:.0}%"),
        None => "-".to_owned(),
    }
}

fn cost(session: &Value) -> String {
    session["usage"]["cost_usd"]
        .as_f64()
        .map_or_else(|| "-".to_owned(), dollars)
}

fn print_board(out: &mut dyn Write, page: &Value) {
    let now = page["now"].as_i64().unwrap_or(0);
    let own = str_field(page, "host");
    let sessions = page["sessions"].as_array().map_or(&[][..], Vec::as_slice);
    let batches = page["batches"].as_array().map_or(&[][..], Vec::as_slice);
    if sessions.is_empty() && batches.is_empty() {
        let _ = writeln!(out, "no agent session has reported");
        return;
    }
    let counts: Vec<String> = page["counts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| format!("{} {}", c["sessions"], str_field(c, "status")))
        .collect();
    let spent: f64 = page["counts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["cost_usd"].as_f64())
        .sum();
    let hosts = page["hosts"].as_u64().unwrap_or(1);
    let runs: u64 = batches.iter().filter_map(|b| b["sessions"].as_u64()).sum();
    let _ = writeln!(
        out,
        "{} sessions{} ({}), {}{}",
        page["total"],
        if runs > 0 {
            format!(" + {runs} captured")
        } else {
            String::new()
        },
        counts.join(", "),
        dollars(spent),
        if hosts > 1 {
            format!(", {hosts} hubs")
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        out,
        "{:<26} {:<24} {:>5} {:>8}  {:<34} NOW",
        "STATUS", "SESSION", "CTX", "COST", "PROGRESS"
    );
    for session in sessions {
        let host = str_field(session, "host");
        let mut name = session["label"]
            .as_str()
            .map_or_else(|| str_field(session, "session").to_owned(), str::to_owned);
        if host != own {
            name = format!("{name}@{}", &host[..host.len().min(8)]);
        }
        let doing = match session["tool"]["name"].as_str() {
            _ if session["ask"]["text"].is_string() => {
                str_field(&session["ask"], "text").to_owned()
            }
            Some(tool) => match session["tool"]["summary"].as_str() {
                Some(summary) => format!("{tool}: {summary}"),
                None => tool.to_owned(),
            },
            None => session["turn_title"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        };
        let _ = writeln!(
            out,
            "{:<26} {:<24} {:>5} {:>8}  {:<34} {}",
            shown(&status(session, now), 26),
            shown(&name, 24),
            context(session),
            cost(session),
            shown(&progress(session), 34),
            shown(&doing, 60)
        );
    }
    let listed = sessions.len() as u64;
    let total = page["total"].as_u64().unwrap_or(listed);
    if total > listed {
        let _ = writeln!(out, "... and {} more (--limit)", total - listed);
    }
    print_batches(out, batches, sessions, now);
}

/// What a batch is called: its launcher's label, the hub session it ran
/// under (by name when that session is on the page), or the runs from
/// outside the hub.
fn batch_name(batch: &Value, sessions: &[Value]) -> String {
    if let Some(label) = batch["label"].as_str() {
        return label.to_owned();
    }
    match batch["parent"].as_str() {
        Some(parent) => {
            let name = batch["parent_name"]
                .as_str()
                .or_else(|| {
                    sessions
                        .iter()
                        .find(|s| s["session"].as_str() == Some(parent))
                        .and_then(|s| s["label"].as_str())
                })
                .map_or_else(|| parent[..parent.len().min(8)].to_owned(), str::to_owned);
            format!("under {name}")
        }
        None => "outside the hub".to_owned(),
    }
}

/// The captured runs, counted per batch: how many, in what status, what
/// they spent against the budget, and the key that lists them.
fn print_batches(out: &mut dyn Write, batches: &[Value], sessions: &[Value], now: i64) {
    if batches.is_empty() {
        return;
    }
    let runs: u64 = batches.iter().filter_map(|b| b["sessions"].as_u64()).sum();
    let _ = writeln!(
        out,
        "\n{runs} captured run{} in {} batch{}:",
        if runs == 1 { "" } else { "s" },
        batches.len(),
        if batches.len() == 1 { "" } else { "es" }
    );
    let _ = writeln!(
        out,
        "{:<30} {:>5} {:<30} {:>18} {:>6}  KEY",
        "BATCH", "RUNS", "STATUS", "COST", "LAST"
    );
    for batch in batches {
        let counts: Vec<String> = batch["counts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| format!("{} {}", c["sessions"], str_field(c, "status")))
            .collect();
        let spent = batch["cost_usd"].as_f64().unwrap_or(0.0);
        let cost = match batch["budget_usd"].as_f64() {
            Some(budget) if batch["over_budget"] == true => {
                format!("{} of {} OVER", dollars(spent), dollars(budget))
            }
            Some(budget) => format!("{} of {}", dollars(spent), dollars(budget)),
            None => dollars(spent),
        };
        let _ = writeln!(
            out,
            "{:<30} {:>5} {:<30} {:>18} {:>6}  {}",
            shown(&batch_name(batch, sessions), 30),
            batch["sessions"].as_u64().unwrap_or(0),
            shown(&counts.join(", "), 30),
            cost,
            span(now - batch["last_ts"].as_i64().unwrap_or(now)),
            shown(str_field(batch, "key"), 60)
        );
    }
    let _ = writeln!(out, "runs of one batch: fw hub fleet --batch KEY");
}

/// The fleet in lanes: a line of counts, then each lane but idle with its
/// cards grouped by project (idle: how many per project).
fn print_lanes(out: &mut dyn Write, view: &Value) {
    let now = view["now"].as_i64().unwrap_or(0);
    let own = str_field(view, "host");
    let lanes = view["lanes"].as_array().map_or(&[][..], Vec::as_slice);
    let counts: Vec<String> = lanes
        .iter()
        .map(|l| format!("{} {}", str_field(l, "title").to_lowercase(), l["count"]))
        .collect();
    let _ = writeln!(out, "{}", counts.join(" · "));
    let placed = view["placed"].as_u64().unwrap_or(0);
    let total = view["total"].as_u64().unwrap_or(0);
    if total > placed {
        let _ = writeln!(
            out,
            "(the {placed} most urgent of {total} sessions; fw hub fleet lists them all)"
        );
    }
    for lane in lanes {
        let groups = lane["groups"].as_array().map_or(&[][..], Vec::as_slice);
        if groups.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "\n{} ({})",
            str_field(lane, "title").to_uppercase(),
            lane["count"]
        );
        if lane["id"] == "idle" {
            let per: Vec<String> = groups
                .iter()
                .map(|g| {
                    let n = g["cards"].as_array().map_or(0, Vec::len);
                    format!("{} {n}", str_field(g, "project"))
                })
                .collect();
            let _ = writeln!(out, "  {}", shown(&per.join(", "), 200));
            continue;
        }
        for group in groups {
            let _ = writeln!(out, "  {}", shown(str_field(group, "project"), 60));
            for card in group["cards"].as_array().into_iter().flatten() {
                let since = card["since"].as_i64().unwrap_or(now);
                let (name, key) = match card.get("session").filter(|s| s.is_object()) {
                    Some(snap) => {
                        let host = str_field(snap, "host");
                        let mut name = snap["label"]
                            .as_str()
                            .map_or_else(|| str_field(snap, "session").to_owned(), str::to_owned);
                        if host != own {
                            name = format!("{name}@{}", &host[..host.len().min(8)]);
                        }
                        (name, str_field(snap, "session").to_owned())
                    }
                    None => (
                        batch_name(&card["batch"], &[]),
                        format!("--batch {}", str_field(&card["batch"], "key")),
                    ),
                };
                let _ = writeln!(
                    out,
                    "    {:>5}  {:<24} {}  [{}]",
                    span(now - since),
                    shown(&name, 24),
                    shown(str_field(card, "why"), 80),
                    shown(&key, 40)
                );
            }
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct InboxArgs {
    /// Only sessions with an event in the last N hours (0: any age).
    #[arg(long, value_name = "N", default_value_t = 24)]
    hours: u32,
    /// Only the sessions of the hub with this id (`local`: the hub asked).
    #[arg(long, value_name = "HUB_ID")]
    host: Option<String>,
    /// Print JSON (the fleet page of blocked and asking sessions).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// The most asks listed.
const INBOX_LIMIT: u32 = 1000;
/// Lines of a handoff's prompt the inbox shows.
const HANDOFF_PREVIEW: usize = 6;

/// `fw hub inbox`: every session that waits on the operator, most urgent
/// first: harness dialogs (`blocked`), then turns that ended asking for
/// an answer, a decision, or a command run (`asking`), with what to do.
pub(crate) fn inbox(args: &InboxArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut run = || -> Result<(), String> {
        let conn = Endpoint::resolve(&args.endpoint)?;
        let mut params = vec![
            format!("hours={}", args.hours),
            format!("limit={INBOX_LIMIT}"),
            "status=blocked,asking".to_owned(),
        ];
        if let Some(host) = &args.host {
            params.push(format!("host={}", query_escape(host)));
        }
        let page = conn.blocking_call(
            "GET",
            &format!("/api/telemetry/fleet?{}", params.join("&")),
            None,
        )?;
        if args.json {
            print_json(out, &page);
        } else {
            print_inbox(out, &page);
        }
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn print_inbox(out: &mut dyn Write, page: &Value) {
    let now = page["now"].as_i64().unwrap_or(0);
    let own = str_field(page, "host");
    let sessions = page["sessions"].as_array().map_or(&[][..], Vec::as_slice);
    if sessions.is_empty() {
        let _ = writeln!(out, "nothing needs you");
        return;
    }
    let _ = writeln!(
        out,
        "{} need{} you",
        sessions.len(),
        if sessions.len() == 1 { "s" } else { "" }
    );
    for session in sessions {
        let host = str_field(session, "host");
        let id = str_field(session, "session");
        let mut name = session["label"].as_str().unwrap_or(id).to_owned();
        if let Some(dir) = session["dir"].as_str() {
            let folder = dir.rsplit('/').find(|p| !p.is_empty()).unwrap_or(dir);
            name = format!("{name} ({folder})");
        }
        if host != own {
            name = format!("{name}@{}", &host[..host.len().min(8)]);
        }
        let ask = &session["ask"];
        let what = match ask["text"].as_str() {
            Some(text) => text.to_owned(),
            None => match session["blocked_tool"].as_str() {
                Some(tool) => format!("permission: {tool}"),
                None => "waiting on you".to_owned(),
            },
        };
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{}  {}",
            shown(&status(session, now), 26),
            shown(&name, 60)
        );
        let _ = writeln!(out, "  {}", shown(&what, 400));
        if let Some(command) = ask["command"].as_str() {
            for line in command.lines() {
                let _ = writeln!(out, "    $ {}", shown(line, 400));
            }
        }
        if let Some(prompt) = ask["prompt"].as_str() {
            let lines: Vec<&str> = prompt.lines().collect();
            for line in lines.iter().take(HANDOFF_PREVIEW) {
                let _ = writeln!(out, "    > {}", shown(line, 200));
            }
            if lines.len() > HANDOFF_PREVIEW {
                let _ = writeln!(
                    out,
                    "    > ... {} more lines",
                    lines.len() - HANDOFF_PREVIEW
                );
            }
        }
        for (n, option) in ask["options"].as_array().into_iter().flatten().enumerate() {
            let _ = writeln!(
                out,
                "    {}. {}",
                n + 1,
                shown(option.as_str().unwrap_or_default(), 200)
            );
        }
        // A dialog is answered in the session; an ask, by the next prompt
        // (another hub's session: on that hub; a captured run: where it
        // runs).
        if session["links"]["captured"] == true {
            let _ = writeln!(out, "  a captured run: answer it where it runs");
        } else if ask["kind"] == "handoff" && host == own {
            let _ = writeln!(out, "  start: fw hub continue {id}");
        } else if session["status"] == "asking" && host == own {
            let _ = writeln!(out, "  reply: fw hub broadcast {id} 'TEXT'");
        } else if session["status"] == "blocked" && host == own {
            let _ = writeln!(out, "  answer: fw hub attach {id}");
        }
    }
}

fn mark(status: &str) -> &'static str {
    match status {
        "done" => "[x]",
        "active" => "[>]",
        "failed" => "[!]",
        "cancelled" => "[-]",
        _ => "[ ]",
    }
}

/// A session's turns, newest first: one block each.
pub(crate) fn print_turns(out: &mut dyn Write, found: &Value) {
    let now = found["now"].as_i64().unwrap_or(0);
    let turns = found["turns"].as_array().map_or(&[][..], Vec::as_slice);
    if turns.is_empty() {
        let _ = writeln!(out, "no turn yet");
        return;
    }
    for (i, turn) in turns.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let started = turn["started"].as_i64().unwrap_or(now);
        let took = turn["finished"].as_i64().unwrap_or(now) - started;
        let mut facts = vec![
            str_field(turn, "status").to_owned(),
            span(took),
            format!("{} calls", turn["calls"].as_u64().unwrap_or(0)),
        ];
        if let Some(errors) = turn["errors"].as_u64().filter(|e| *e > 0) {
            facts.push(format!("{errors} failed"));
        }
        if let Some(cost) = turn["cost_usd"].as_f64() {
            facts.push(dollars(cost));
        }
        if turn["compacted"] == true {
            facts.push("compacted".to_owned());
        }
        if turn["partial"] == true {
            facts.push("older calls not read".to_owned());
        }
        let title = str_field(turn, "title");
        let _ = writeln!(
            out,
            "{} ({})",
            shown(
                if title.is_empty() {
                    "(prompt not seen)"
                } else {
                    title
                },
                90
            ),
            facts.join(", ")
        );
        let stations: Vec<String> = turn["stations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                format!(
                    "{} {} ({})",
                    str_field(s, "station"),
                    span(s["ms"].as_i64().unwrap_or(0)),
                    s["calls"]
                )
            })
            .collect();
        if !stations.is_empty() {
            let _ = writeln!(out, "  time   {}", stations.join(", "));
        }
        let read = turn["read"].as_u64().unwrap_or(0);
        let edited = turn["edited"].as_u64().unwrap_or(0);
        if read + edited > 0 {
            let names: Vec<String> = turn["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let path = str_field(f, "path");
                    let name = path.rsplit('/').next().unwrap_or(path);
                    match f["edits"].as_u64().unwrap_or(1) {
                        1 => shown(name, 40),
                        n => format!("{} x{n}", shown(name, 40)),
                    }
                })
                .collect();
            let _ = writeln!(
                out,
                "  files  read {read}, edited {edited}{}",
                if names.is_empty() {
                    String::new()
                } else {
                    format!(": {}", names.join(", "))
                }
            );
        }
        let commands: Vec<String> = turn["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let calls = c["calls"].as_u64().unwrap_or(0);
                let errors = c["errors"].as_u64().unwrap_or(0);
                format!(
                    "{}{}{}",
                    shown(str_field(c, "shape"), 30),
                    if calls > 1 {
                        format!(" x{calls}")
                    } else {
                        String::new()
                    },
                    if errors > 0 {
                        format!(" ({errors} failed)")
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        if !commands.is_empty() {
            let _ = writeln!(out, "  ran    {}", commands.join(", "));
        }
        if let Some(failure) = turn["failure"].as_object() {
            let _ = writeln!(
                out,
                "  failed {} {}{}",
                failure
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool"),
                shown(
                    failure.get("summary").and_then(Value::as_str).unwrap_or(""),
                    60
                ),
                failure
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|e| format!(": {}", shown(e, 80)))
                    .unwrap_or_default()
            );
        }
        if let Some(error) = turn["error"].as_str() {
            let _ = writeln!(out, "  error  {}", shown(error, 160));
        } else if let Some(ending) = turn["ending"].as_str() {
            // Its last line of prose (a table row says little alone).
            let last = ending
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('|') && !l.starts_with("```"))
                .unwrap_or("");
            let _ = writeln!(out, "  ended  {}", shown(last.trim(), 160));
        }
    }
}

fn print_session(out: &mut dyn Write, found: &Value) {
    let now = found["now"].as_i64().unwrap_or(0);
    let session = &found["session"];
    let _ = writeln!(
        out,
        "{} ({}{}): {}",
        shown(
            session["label"]
                .as_str()
                .unwrap_or_else(|| str_field(session, "session")),
            60
        ),
        str_field(session, "harness"),
        session["model"]
            .as_str()
            .map(|m| format!(", {}", shown(m, 40)))
            .unwrap_or_default(),
        status(session, now)
    );
    for (key, said) in [("continues", "continues"), ("opened_by", "opened by")] {
        if let Some(other) = session["links"][key].as_str() {
            let _ = writeln!(out, "{said} session {}", shown(other, 40));
        }
    }
    if let Some(next) = session["continued_by"].as_str() {
        let _ = writeln!(out, "continued in session {}", shown(next, 40));
    }
    let blocked = session["blocked_ms"].as_i64().unwrap_or(0)
        + if session["status"] == "blocked" {
            now - session["since"].as_i64().unwrap_or(now)
        } else {
            0
        };
    let _ = writeln!(
        out,
        "context {}, cost {}, {} calls ({} failed, {} denied), blocked on you {}",
        context(session),
        cost(session),
        session["tools"],
        session["tool_errors"],
        session["denied"],
        span(blocked)
    );
    for rate in session["rate"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "rate limit {}: {:.0}% used",
            shown(str_field(rate, "window"), 20),
            rate["used_pct"].as_f64().unwrap_or(0.0)
        );
    }
    if let Some(error) = session["last_error"].as_str() {
        let _ = writeln!(out, "last error: {}", shown(error, 200));
    }
    for plan in session["plans"].as_array().into_iter().flatten() {
        let owner = match plan["owner"].as_str() {
            Some(agent) => format!(" (subagent {})", shown(agent, 12)),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "\nplan{owner}, as the agent declared it ({}, revision {}):",
            str_field(plan, "source"),
            plan["revision"]
        );
        for item in plan["items"].as_array().into_iter().flatten() {
            let _ = writeln!(
                out,
                "  {} {}  ({} calls)",
                mark(str_field(item, "status")),
                shown(str_field(item, "title"), 100),
                item["tools"]
            );
        }
    }
    let work = session["work"].as_array().map_or(&[][..], Vec::as_slice);
    if !work.is_empty() {
        let _ = writeln!(out, "\nwork:");
    }
    for item in work {
        let nested = item["parent"].is_string();
        let started = item["started"].as_i64().unwrap_or(now);
        let took = item["finished"].as_i64().unwrap_or(now) - started;
        let title = match str_field(item, "title") {
            "" => "(prompt not seen)",
            title => title,
        };
        let kind = item["kind"]
            .as_str()
            .map(|k| format!("{} ", shown(k, 24)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{}{} {kind}{}  ({}, {} calls{})",
            if nested { "    " } else { "  " },
            mark(str_field(item, "status")),
            shown(title, 90),
            span(took),
            item["tools"],
            match item["tool_errors"].as_u64() {
                Some(n) if n > 0 => format!(", {n} failed"),
                _ => String::new(),
            }
        );
        if let Some(doing) = item["doing"].as_str() {
            let _ = writeln!(
                out,
                "{}    now: {}",
                if nested { "    " } else { "  " },
                shown(doing, 100)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn turns_read_as_time_files_commands_and_ending() {
        let found = json!({"now": 100_000, "turns": [{
            "id": "t1", "title": "fix the build", "status": "done", "started": 0,
            "finished": 90_000, "calls": 5, "errors": 1, "cost_usd": 0.42,
            "compacted": false, "stations": [
                {"station": "shell", "calls": 2, "ms": 60_000, "errors": 1},
                {"station": "files", "calls": 3, "ms": 2_000, "errors": 0}],
            "read": 3, "edited": 1, "edits": [{"path": "/r/src/main.rs", "edits": 2}],
            "commands": [{"shape": "cargo test", "calls": 2, "errors": 1, "ms": 60_000}],
            "failure": {"tool": "Bash", "summary": "cargo test", "error": "exit 101"},
            "ending": "Done.\n| a | b |\nThe build is green."}]});
        let shown = text(|out| print_turns(out, &found));
        for want in [
            "fix the build (done, 1m, 5 calls, 1 failed, $0.42)",
            "time   shell 1m (2), files 2s (3)",
            "files  read 3, edited 1: main.rs x2",
            "ran    cargo test x2 (1 failed)",
            "failed Bash cargo test: exit 101",
            "ended  The build is green.",
        ] {
            assert!(shown.contains(want), "missing {want:?} in:\n{shown}");
        }
    }

    fn text(print: impl Fn(&mut dyn Write)) -> String {
        let mut out = Vec::new();
        print(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_inbox_says_what_each_session_needs_and_how_to_answer() {
        let own = "aaaaaaaabbbbbbbbccccccccdddddddd";
        let page = json!({
            "host": own, "now": 600_000, "total": 3,
            "sessions": [
                {"host": own, "session": "s1", "label": "claude 3", "dir": "/r/flywheel",
                 "status": "blocked", "since": 540_000, "blocked_on": "permission", "blocked_tool": "Bash"},
                {"host": own, "session": "s2", "label": "claude 4", "dir": "/r/infrared/",
                 "status": "asking", "since": 420_000,
                 "ask": {"kind": "run", "text": "Restart the relay; it needs sudo.",
                         "command": "sudo systemctl restart rh-relay\nsystemctl status rh-relay",
                         "source": "declared", "since": 420_000}},
                {"host": "11111111222222223333333344444444", "session": "s3", "status": "asking",
                 "since": 0, "ask": {"kind": "decide", "text": "Which plan?", "options": ["A", "B"],
                                     "source": "declared", "since": 0}},
            ],
        });
        let inbox = text(|out| print_inbox(out, &page));
        assert!(inbox.starts_with("3 need you\n"), "{inbox}");
        assert!(inbox.contains("blocked 1m (permission)  claude 3 (flywheel)\n  permission: Bash\n  answer: fw hub attach s1"), "{inbox}");
        assert!(inbox.contains("asking 3m (run)  claude 4 (infrared)\n  Restart the relay; it needs sudo.\n    $ sudo systemctl restart rh-relay\n    $ systemctl status rh-relay\n  reply: fw hub broadcast s2 'TEXT'"), "{inbox}");
        // Another hub's ask is answered there.
        assert!(
            inbox.contains("s3@11111111\n  Which plan?\n    1. A\n    2. B\n"),
            "{inbox}"
        );
        assert!(!inbox.contains("broadcast s3"), "{inbox}");
        assert_eq!(
            text(|out| print_inbox(out, &json!({"sessions": []}))),
            "nothing needs you\n"
        );
        let handoff = json!({"host": own, "now": 0, "sessions": [
            {"host": own, "session": "s5", "status": "asking", "since": 0,
             "ask": {"kind": "handoff", "text": "Context is long", "source": "declared", "since": 0,
                     "prompt": "Goal: ship C2\nDone: links\n1\n2\n3\n4\n5"}}]});
        let inbox = text(|out| print_inbox(out, &handoff));
        assert!(
            inbox.contains("  Context is long\n    > Goal: ship C2\n    > Done: links\n"),
            "{inbox}"
        );
        assert!(
            inbox.contains("    > ... 1 more lines\n  start: fw hub continue s5\n"),
            "{inbox}"
        );
    }

    #[test]
    fn the_board_says_status_progress_context_and_cost_per_session() {
        let page = json!({
            "host": "aaaaaaaabbbbbbbbccccccccdddddddd", "now": 100_000, "total": 3, "hosts": 2,
            "counts": [{"status": "blocked", "sessions": 1, "cost_usd": 2.5},
                       {"status": "working", "sessions": 2, "cost_usd": 0.5}],
            "sessions": [
                {"host": "aaaaaaaabbbbbbbbccccccccdddddddd", "session": "s1", "label": "api",
                 "status": "blocked", "since": 40_000, "blocked_on": "permission",
                 "plan": {"done": 4, "total": 7, "current": "Wire the route"},
                 "context": {"pct": 42.4, "exact": true}, "usage": {"cost_usd": 2.5},
                 "tool": {"name": "Bash", "summary": "cargo test"}},
                {"host": "11111111222222223333333344444444", "session": "s2",
                 "status": "working", "since": 95_000, "turn_title": "tidy\u{1b}[31m docs",
                 "turn_tools": 14, "subagents_active": 1, "subagents_total": 3,
                 "context": {"pct": 80.0, "exact": false}},
            ],
        });
        let board = text(|out| print_board(out, &page));
        let lines: Vec<&str> = board.lines().collect();
        assert_eq!(lines[0], "3 sessions (1 blocked, 2 working), $3.00, 2 hubs");
        assert!(lines[2].starts_with("blocked 1m (permission)"), "{board}");
        assert!(lines[2].contains(" api "), "{board}");
        assert!(lines[2].contains("4/7 Wire the route"), "{board}");
        assert!(
            lines[2].contains("42%") && lines[2].contains("$2.50"),
            "{board}"
        );
        assert!(lines[2].ends_with("Bash: cargo test"), "{board}");
        // Another hub's session is tagged; nothing declared, so no
        // fraction: what was observed; an estimate is marked.
        assert!(lines[3].contains("s2@11111111"), "{board}");
        assert!(
            lines[3].contains("14 calls, 1/3 subagents running"),
            "{board}"
        );
        assert!(lines[3].contains("~80%"), "{board}");
        assert!(!board.contains('\u{1b}'), "control bytes are not printed");
        assert_eq!(lines[4], "... and 1 more (--limit)");
        assert_eq!(
            text(|out| print_board(out, &json!({"sessions": []}))),
            "no agent session has reported\n"
        );
    }

    #[test]
    fn captured_runs_are_counted_per_batch_with_spend_against_budget() {
        let own = "aaaaaaaabbbbbbbbccccccccdddddddd";
        let page = json!({
            "host": own, "now": 600_000, "total": 1,
            "counts": [{"status": "working", "sessions": 4, "cost_usd": 3.5}],
            "sessions": [{"host": own, "session": "p1", "label": "lead", "status": "working",
                          "since": 0, "last_ts": 0, "turns": 1, "turn_tools": 0,
                          "open_tools": 0, "tools": 0, "tool_errors": 0, "denied": 0,
                          "subagents_active": 0, "subagents_total": 0}],
            "batches": [
                {"key": "batch:analysts", "host": own, "label": "analysts", "sessions": 2,
                 "counts": [{"status": "working", "sessions": 1, "cost_usd": 0.5},
                            {"status": "ended", "sessions": 1, "cost_usd": 2.0}],
                 "cost_usd": 2.5, "budget_usd": 2.0, "over_budget": true,
                 "first_ts": 0, "last_ts": 540_000},
                {"key": "under:p1", "host": own, "parent": "p1", "sessions": 1,
                 "counts": [{"status": "working", "sessions": 1, "cost_usd": 1.0}],
                 "cost_usd": 1.0, "over_budget": false, "first_ts": 0, "last_ts": 600_000},
            ],
        });
        let board = text(|out| print_board(out, &page));
        assert!(board.contains("3 captured runs in 2 batches:"), "{board}");
        assert!(board.contains("analysts"), "{board}");
        assert!(board.contains("1 working, 1 ended"), "{board}");
        assert!(board.contains("$2.50 of $2.00 OVER"), "{board}");
        assert!(board.contains("batch:analysts"), "{board}");
        // A parent on the page is named.
        assert!(board.contains("under lead"), "{board}");
        assert!(board.contains("fw hub fleet --batch KEY"), "{board}");
    }

    #[test]
    fn a_session_prints_its_plan_and_its_work_tree() {
        let found = json!({"now": 600_000, "session": {
        "session": "s1", "label": "api", "harness": "claude", "model": "claude-x",
        "status": "working", "since": 0, "tools": 9, "tool_errors": 1, "denied": 0,
        "blocked_ms": 120_000, "context": {"pct": 12.0, "exact": true},
        "usage": {"cost_usd": 0.4}, "last_error": "Bash: exit 1",
        "rate": [{"window": "five_hour", "used_pct": 33.0}],
        "plans": [{"source": "declared", "revision": 2, "items": [
            {"title": "Write it", "status": "done", "tools": 5},
            {"title": "Test it", "status": "active", "tools": 3}]}],
        "work": [
            {"id": "t1", "source": "turn", "title": "ship it", "status": "active",
             "started": 0, "tools": 8, "tool_errors": 1, "doing": "Bash: cargo test"},
            {"id": "a:1", "parent": "t1", "source": "subagent", "title": "Map routes",
             "kind": "Explore", "status": "done", "started": 1000, "finished": 61_000, "tools": 1},
        ]}});
        let shown = text(|out| print_session(out, &found));
        assert!(
            shown.starts_with("api (claude, claude-x): working 10m\n"),
            "{shown}"
        );
        assert!(
            shown.contains(
                "context 12%, cost $0.40, 9 calls (1 failed, 0 denied), blocked on you 2m"
            )
        );
        assert!(shown.contains("rate limit five_hour: 33% used"));
        assert!(shown.contains("plan, as the agent declared it (declared, revision 2):"));
        assert!(shown.contains("  [x] Write it  (5 calls)\n  [>] Test it  (3 calls)"));
        assert!(
            shown.contains("  [>] ship it  (10m, 8 calls, 1 failed)\n      now: Bash: cargo test")
        );
        assert!(shown.contains("    [x] Explore Map routes  (1m, 1 calls)"));
    }
}
