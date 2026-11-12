//! `fw hub insights`: where time and money went (see
//! [`crate::telemetry::insights`]): per project, tool time by command,
//! time spent waiting, what keeps failing, files written again and again,
//! and what re-reading context costs.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{dollars, fail, print_json, str_field};
use serde_json::Value;
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct InsightsArgs {
    /// The last N hours (0: everything kept, 30 days).
    #[arg(long, value_name = "N", default_value_t = 168)]
    hours: u32,
    /// One project only (a repository's folder name, as listed).
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Only the sessions of the hub with this id (`local`: the hub asked).
    #[arg(long, value_name = "HUB_ID")]
    host: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn insights(args: &InsightsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let run = || -> Result<Value, String> {
        let conn = Endpoint::resolve(&args.endpoint)?;
        let mut params = vec![format!("hours={}", args.hours)];
        if let Some(project) = &args.project {
            params.push(format!("project={}", query_escape(project)));
        }
        if let Some(host) = &args.host {
            params.push(format!("host={}", query_escape(host)));
        }
        conn.blocking_call(
            "GET",
            &format!("/api/telemetry/insights?{}", params.join("&")),
            None,
        )
    };
    match run() {
        Ok(page) if args.json => {
            print_json(out, &page);
            0
        }
        Ok(page) => {
            print_insights(out, &page);
            0
        }
        Err(message) => fail(err, &message),
    }
}

/// `1h 05m`, `12m`, `40s`.
fn took(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn pct(part: f64, whole: f64) -> String {
    if whole <= 0.0 {
        return "-".to_owned();
    }
    format!("{:.0}%", part / whole * 100.0)
}

fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[][..], Vec::as_slice)
}

fn num(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

/// What a shape is called: the command, else the tool.
fn shape_name(row: &Value) -> String {
    match str_field(row, "shape") {
        "" => str_field(row, "tool").to_owned(),
        shape => shape.to_owned(),
    }
}

fn clip(text: &str, max: usize) -> String {
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

pub(crate) fn print_insights(out: &mut dyn Write, page: &Value) {
    let found = &page["insights"];
    let hours = page["hours"].as_i64().unwrap_or(0);
    let window = match hours {
        0 => "everything kept".to_owned(),
        h if h % 24 == 0 => format!("last {} days", h / 24),
        h => format!("last {h} h"),
    };
    let projects = rows(&found["projects"]);
    if projects.is_empty() {
        let _ = writeln!(out, "nothing recorded ({window})");
        return;
    }
    let spent: f64 = projects.iter().filter_map(|p| p["cost_usd"].as_f64()).sum();
    let tool_ms: u64 = projects.iter().map(|p| num(p, "tool_ms")).sum();
    let _ = writeln!(
        out,
        "{window}: {} spent, {} in tools",
        dollars(spent),
        took(tool_ms)
    );

    let _ = writeln!(out, "\nPROJECTS");
    let _ = writeln!(
        out,
        "  {:<24} {:>8} {:>9} {:>6} {:>7} {:>8}",
        "PROJECT", "SESSIONS", "TOOL TIME", "CALLS", "FAILED", "COST"
    );
    for p in projects {
        let _ = writeln!(
            out,
            "  {:<24} {:>8} {:>9} {:>6} {:>7} {:>8}",
            clip(str_field(p, "project"), 24),
            num(p, "sessions"),
            took(num(p, "tool_ms")),
            num(p, "calls"),
            num(p, "errors"),
            dollars(p["cost_usd"].as_f64().unwrap_or(0.0))
        );
    }

    let time = rows(&found["time"]);
    if !time.is_empty() {
        let _ = writeln!(out, "\nTIME BY COMMAND");
        for row in time {
            let _ = writeln!(
                out,
                "  {:>9} {:>5}  {:<32} {:>5} calls in {} sessions{}",
                took(num(row, "ms")),
                pct(num(row, "ms") as f64, tool_ms as f64),
                clip(&shape_name(row), 32),
                num(row, "calls"),
                num(row, "sessions"),
                match num(row, "errors") {
                    0 => String::new(),
                    n => format!(", {n} failed"),
                }
            );
        }
    }

    let waiting = &found["waiting"];
    if num(waiting, "ms") > 0 {
        let calls = num(waiting, "calls");
        let _ = write!(
            out,
            "\nWAITING: {} in {} wait loops ({} of shell time, {} each on average)",
            took(num(waiting, "ms")),
            calls,
            pct(num(waiting, "ms") as f64, num(waiting, "shell_ms") as f64),
            took(num(waiting, "ms") / calls.max(1))
        );
        let reread = num(waiting, "reread");
        if reread > 0 {
            let usd = waiting["reread_usd"].as_f64().unwrap_or(0.0);
            let _ = write!(
                out,
                "; the steps after them re-read about {} tokens of context{}",
                tokens(reread),
                if usd > 0.0 {
                    format!(" ({} where priced)", dollars(usd))
                } else {
                    String::new()
                }
            );
        }
        let _ = writeln!(
            out,
            ".\n  Waiting on a run is not waste; each wait is one more step that re-reads the context, so a few long waits (gh run watch, wait PID) cost less than many short ones."
        );
    }

    let failing = rows(&found["failing"]);
    if !failing.is_empty() {
        let _ = writeln!(out, "\nKEEPS FAILING");
        for row in failing {
            let _ = writeln!(
                out,
                "  {:>3} of {:<4} {}{}",
                num(row, "errors"),
                num(row, "calls"),
                clip(&shape_name(row), 40),
                row["last_error"]
                    .as_str()
                    .map(|e| format!("  last: {}", clip(e, 70)))
                    .unwrap_or_default()
            );
        }
    }

    let rewritten = rows(&found["rewritten"]);
    if !rewritten.is_empty() {
        let _ = writeln!(out, "\nWRITTEN AGAIN AND AGAIN (keep one script instead)");
        for row in rewritten {
            let _ = writeln!(
                out,
                "  {:>3}x in {} sessions  {}",
                num(row, "writes"),
                num(row, "sessions"),
                clip(str_field(row, "path"), 80)
            );
        }
    }

    let context = &found["context"];
    let sessions = rows(&context["sessions"]);
    if !sessions.is_empty() {
        let read = num(context, "cache_read");
        let wrote = num(context, "output");
        let _ = write!(
            out,
            "\nCONTEXT TAX: {} tokens re-read from cache for {} written",
            tokens(read),
            tokens(wrote)
        );
        let usd = context["cache_read_usd"].as_f64().unwrap_or(0.0);
        let priced = context["priced_cost_usd"].as_f64().unwrap_or(0.0);
        if priced > 0.0 {
            let _ = write!(
                out,
                " ({} of {} spend where priced)",
                pct(usd, priced),
                dollars(priced)
            );
        }
        let _ = writeln!(out, "; where a long context is re-read most:");
        for row in sessions {
            let name = row["label"]
                .as_str()
                .unwrap_or_else(|| str_field(row, "session"));
            let _ = writeln!(
                out,
                "  {:>7} read, {:>6} written {:>9}  {:<20} {}{}",
                tokens(num(row, "cache_read")),
                tokens(num(row, "output")),
                row["cache_read_usd"]
                    .as_f64()
                    .map(|u| format!("({})", dollars(u)))
                    .unwrap_or_default(),
                clip(name, 20),
                clip(str_field(row, "project"), 20),
                row["context"]
                    .as_u64()
                    .map(|c| format!(", {} context now", tokens(c)))
                    .unwrap_or_default()
            );
        }
    }
}

/// `435M`, `1.3M`, `642k`, `900`.
fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        1_000_000..10_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{}M", n / 1_000_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insights_read_as_projects_time_polling_failures_rewrites_and_context() {
        let page = json!({"hours": 168, "insights": {
            "projects": [{"project": "flywheel", "sessions": 3, "calls": 40, "tool_ms": 3_600_000,
                          "errors": 4, "cost_usd": 12.5}],
            "time": [{"tool": "Bash", "shape": "cargo test", "calls": 10, "ms": 1_800_000,
                      "errors": 2, "sessions": 2},
                     {"tool": "Read", "shape": "", "calls": 20, "ms": 60_000, "errors": 0,
                      "sessions": 3}],
            "waiting": {"ms": 600_000, "calls": 5, "shell_ms": 2_400_000,
                        "reread": 3_000_000, "reread_usd": 1.5},
            "failing": [{"tool": "Bash", "shape": "gh run view", "calls": 6, "ms": 1, "errors": 4,
                         "sessions": 1, "last_error": "run not found"}],
            "rewritten": [{"path": "/tmp/probe.py", "writes": 5, "sessions": 3}],
            "context": {"cache_read": 435_000_000, "output": 1_300_000,
                        "cache_read_usd": 6.0, "priced_cost_usd": 12.5,
                        "sessions": [{"host": "h", "session": "s1", "label": "claude 3",
                                      "project": "flywheel", "cache_read": 435_000_000,
                                      "output": 1_300_000, "cache_read_usd": 5.0,
                                      "cost_usd": 8.0, "context": 642_000}]}
        }});
        let mut out = Vec::new();
        print_insights(&mut out, &page);
        let text = String::from_utf8(out).unwrap();
        for want in [
            "last 7 days: $12.50 spent, 1h 00m in tools",
            "flywheel",
            "50%  cargo test",
            "10 calls in 2 sessions, 2 failed",
            "WAITING: 10m in 5 wait loops (25% of shell time, 2m each on average); the steps after them re-read about 3.0M tokens of context ($1.50 where priced)",
            "4 of 6    gh run view  last: run not found",
            "5x in 3 sessions  /tmp/probe.py",
            "CONTEXT TAX: 435M tokens re-read from cache for 1.3M written (48% of $12.50 spend where priced)",
            "435M read,   1.3M written   ($5.00)  claude 3",
            "642k context now",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }
}
