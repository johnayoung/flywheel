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
    /// Only sessions in this status (working, blocked, done, failed, idle,
    /// ended); repeatable.
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
    let mut params = vec![
        format!("hours={}", args.hours),
        format!("limit={}", args.limit),
    ];
    if !args.status.is_empty() {
        params.push(format!("status={}", query_escape(&args.status.join(","))));
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
    match session["blocked_on"].as_str() {
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
    if sessions.is_empty() {
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
    let _ = writeln!(
        out,
        "{} sessions ({}), {}{}",
        page["total"],
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

    fn text(print: impl Fn(&mut dyn Write)) -> String {
        let mut out = Vec::new();
        print(&mut out);
        String::from_utf8(out).unwrap()
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
