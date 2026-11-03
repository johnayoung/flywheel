//! `fw hub search <query>`: full-text search across the running hub's
//! sessions (scrollback, agent transcripts, suggested commands).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::resolve;
use crate::search::{MAX_LIMIT, MAX_PER_SESSION};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::io::{IsTerminal, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SourceArg {
    Scrollback,
    Transcript,
    Commands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum KindArg {
    Shell,
    Claude,
    Codex,
}

#[derive(Debug, clap::Args)]
pub struct SearchArgs {
    /// What to find: literal text (case-insensitive unless --case), or a
    /// pattern with --regex.
    query: String,
    /// QUERY is a regular expression (Rust `regex` syntax: linear time;
    /// `^`/`$` match at line ends).
    #[arg(long)]
    regex: bool,
    /// Match case.
    #[arg(long)]
    case: bool,
    /// Only this session (id, unique id prefix, or name).
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
    /// Only this workspace (id, unique id prefix, or name).
    #[arg(long = "workspace", visible_alias = "ws", value_name = "WS")]
    workspace: Option<String>,
    /// Only sessions of this kind (repeatable).
    #[arg(long, value_enum)]
    kind: Vec<KindArg>,
    /// Only this source (repeatable; default all).
    #[arg(long, value_enum)]
    source: Vec<SourceArg>,
    /// Most hits in all.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=MAX_LIMIT as i64))]
    limit: u16,
    /// Most hits per session and source (the newest).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..=MAX_PER_SESSION as i64))]
    per_session: u16,
    /// The hub's JSON answer.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

const SOURCES: [(SourceArg, &str); 3] = [
    (SourceArg::Scrollback, "scrollback"),
    (SourceArg::Transcript, "transcript"),
    (SourceArg::Commands, "commands"),
];
const KINDS: [(KindArg, &str); 3] = [
    (KindArg::Shell, "shell"),
    (KindArg::Claude, "claude"),
    (KindArg::Codex, "codex"),
];

fn names<T: PartialEq>(picked: &[T], table: &[(T, &'static str)]) -> String {
    table
        .iter()
        .filter(|(value, _)| picked.contains(value))
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(",")
}

fn encode(text: &str) -> String {
    utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}

/// The `/api/search` path for `args` (ids already resolved).
fn path_of(args: &SearchArgs, session: Option<&str>, workspace: Option<&str>) -> String {
    let mut path = format!(
        "/api/search?q={}&limit={}&per_session={}",
        encode(&args.query),
        args.limit,
        args.per_session
    );
    if args.regex {
        path.push_str("&regex=true");
    }
    if args.case {
        path.push_str("&case=true");
    }
    if let Some(id) = session {
        path.push_str(&format!("&session={}", encode(id)));
    }
    if let Some(id) = workspace {
        path.push_str(&format!("&ws={}", encode(id)));
    }
    if !args.kind.is_empty() {
        path.push_str(&format!("&kind={}", names(&args.kind, &KINDS)));
    }
    if !args.source.is_empty() {
        path.push_str(&format!("&source={}", names(&args.source, &SOURCES)));
    }
    path
}

/// Run `fw hub search`: exit 0 with hits, 1 with none (or on an error).
pub fn run(args: &SearchArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let color = std::io::stdout().is_terminal();
    match search(args).and_then(|answer| render(args, &answer, color, out, err)) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

fn search(args: &SearchArgs) -> Result<Value, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let session = match &args.session {
        Some(wanted) => {
            let sessions = conn.blocking_sessions()?;
            Some(field(resolve(&sessions, wanted, "session")?, "id").to_owned())
        }
        None => None,
    };
    let workspace = match &args.workspace {
        Some(wanted) => {
            let workspaces = conn.blocking_workspaces()?;
            Some(field(resolve(&workspaces, wanted, "workspace")?, "id").to_owned())
        }
        None => None,
    };
    conn.blocking_call(
        "GET",
        &path_of(args, session.as_deref(), workspace.as_deref()),
        None,
    )
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// One hit as a line: where, then the snippet with the match marked.
pub fn hit_line(hit: &Value, color: bool) -> String {
    let n = |key: &str| hit.get(key).and_then(Value::as_u64).unwrap_or_default();
    let place = match field(hit, "source") {
        "scrollback" => {
            let screen = if field(hit, "screen") == "alternate" {
                " alt"
            } else {
                ""
            };
            format!("scrollback{screen} -{}", n("from_end"))
        }
        "transcript" => {
            let who = match field(hit, "kind") {
                "tool" => format!("tool {}", field(hit, "tool")),
                _ => field(hit, "role").to_owned(),
            };
            format!("transcript #{} {who}", n("index"))
        }
        "commands" => {
            let lang = hit.get("lang").and_then(Value::as_str).unwrap_or("text");
            format!("commands #{} {lang}", n("seq"))
        }
        other => other.to_owned(),
    };
    let when = hit
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|t| format!(" {}", t.get(..16).unwrap_or(t).replace('T', " ")))
        .unwrap_or_default();
    let (on, off) = if color {
        ("\x1b[1;7m", "\x1b[0m")
    } else {
        ("[", "]")
    };
    let more = match n("matches") {
        0 | 1 => String::new(),
        m => format!(" (+{})", m - 1),
    };
    format!(
        "  {place}{when}{more}  {}{on}{}{off}{}",
        field(hit, "before"),
        field(hit, "match"),
        field(hit, "after"),
    )
}

fn render(
    args: &SearchArgs,
    answer: &Value,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let hits = answer.get("hits").and_then(Value::as_u64).unwrap_or(0);
    if args.json {
        writeln!(out, "{answer}").map_err(io)?;
        return Ok(if hits > 0 { 0 } else { 1 });
    }
    let groups = answer
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            writeln!(out).map_err(io)?;
        }
        let terminal = if group.get("terminal").and_then(Value::as_bool) == Some(true) {
            ""
        } else {
            ", no terminal"
        };
        writeln!(
            out,
            "{}/{} ({}, {}{terminal}) {}",
            field(group, "workspace"),
            field(group, "session"),
            field(group, "kind"),
            field(group, "state"),
            field(group, "session_id").get(..8).unwrap_or_default(),
        )
        .map_err(io)?;
        for hit in group
            .get("hits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            writeln!(out, "{}", hit_line(hit, color)).map_err(io)?;
        }
        if group.get("more").and_then(Value::as_bool) == Some(true) {
            let counts = group.get("counts").cloned().unwrap_or(Value::Null);
            let count = |k: &str| counts.get(k).and_then(Value::as_u64).unwrap_or(0);
            writeln!(
                out,
                "  ... more: {} scrollback, {} transcript, {} commands matches in all (--session, --per-session)",
                count("scrollback"),
                count("transcript"),
                count("commands"),
            )
            .map_err(io)?;
        }
    }
    let flag = |k: &str| answer.get(k).and_then(Value::as_bool) == Some(true);
    let mut summary = format!(
        "fw hub: {hits} hit{} in {} session{} ({} ms)",
        if hits == 1 { "" } else { "s" },
        groups.len(),
        if groups.len() == 1 { "" } else { "s" },
        answer
            .get("elapsed_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    if flag("truncated") {
        summary.push_str("; limit reached (--limit)");
    }
    if flag("partial") {
        summary.push_str("; time budget ran out: some sessions were not searched (narrow it)");
    }
    writeln!(err, "{summary}").map_err(io)?;
    for note in answer
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        writeln!(err, "fw hub: {}", note.as_str().unwrap_or_default()).map_err(io)?;
    }
    Ok(if hits > 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hits_print_where_when_and_the_marked_match() {
        let hit = json!({"source": "transcript", "index": 4, "role": "assistant", "kind": "text",
            "timestamp": "2026-11-02T10:11:12.000Z", "matches": 3,
            "before": "run ", "match": "cargo", "after": " test"});
        assert_eq!(
            hit_line(&hit, false),
            "  transcript #4 assistant 2026-11-02 10:11 (+2)  run [cargo] test"
        );
        let hit = json!({"source": "scrollback", "screen": "primary", "from_end": 12,
            "matches": 1, "before": "", "match": "x", "after": ""});
        assert_eq!(hit_line(&hit, true), "  scrollback -12  \x1b[1;7mx\x1b[0m");
        let hit = json!({"source": "commands", "seq": 9, "lang": null, "matches": 1,
            "before": "a", "match": "b", "after": "c", "timestamp": null});
        assert_eq!(hit_line(&hit, false), "  commands #9 text  a[b]c");
    }

    #[test]
    fn the_path_encodes_the_query_and_names_filters() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: SearchArgs,
        }
        let cli = Cli::parse_from([
            "x",
            "a b&c",
            "--regex",
            "--source",
            "commands",
            "--source",
            "scrollback",
            "--kind",
            "claude",
        ]);
        assert_eq!(
            path_of(&cli.args, Some("s1"), None),
            "/api/search?q=a%20b%26c&limit=100&per_session=10&regex=true&session=s1&kind=claude&source=scrollback,commands"
        );
    }
}
