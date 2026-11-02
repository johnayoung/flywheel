//! Scripting the running hub from a shell or another agent: `fw hub ls`,
//! `new`, `send`, `read`, `kill`, all over its HTTP API (bearer token from
//! the state dir, address from its runtime file).
//!
//! Sessions and workspaces are addressed by id, unique id prefix, or name.

use crate::auth::Token;
use crate::cli::{StateDirArg, resolve_state_dir, running_hub};
use crate::client;
use crate::remote::RuntimeInfo;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a scripting call waits for the hub (session spawns included).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Characters of an id shown by `ls` (any unique prefix addresses it).
pub const SHORT_ID: usize = 8;

/// The running hub, ready to call.
#[derive(Debug)]
pub struct HubConn {
    pub info: RuntimeInfo,
    token: Token,
}

impl HubConn {
    /// Connect to the hub serving `state_dir`.
    pub fn connect(state_dir: &Path) -> Result<Self, String> {
        let info = running_hub(state_dir)?.ok_or_else(|| {
            format!(
                "no hub is running for state dir {} (start one with `fw hub start`)",
                state_dir.display()
            )
        })?;
        let token = Token::load(state_dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the running hub's token file is missing".to_owned())?;
        Ok(Self { info, token })
    }

    /// `method path` with an optional JSON body; the parsed JSON response
    /// (`null` when empty) on 2xx, else the API's error.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let body = body.map(Value::to_string);
        let response = client::request(
            self.info.connect_addr(),
            method,
            path,
            self.token.expose(),
            body.as_deref(),
            CALL_TIMEOUT,
        )
        .map_err(|e| {
            format!(
                "cannot reach the running hub (pid {}) at {}: {e}",
                self.info.pid,
                self.info.connect_addr()
            )
        })?;
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "{} ({})",
                response.error_message(),
                response.status
            ));
        }
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&response.body)
            .map_err(|e| format!("the hub sent invalid JSON for {path}: {e}"))
    }

    /// `GET /api/{what}` -> its `what` array.
    fn list(&self, what: &str) -> Result<Vec<Value>, String> {
        let body = self.call("GET", &format!("/api/{what}"), None)?;
        match body.get(what) {
            Some(Value::Array(items)) => Ok(items.clone()),
            _ => Err(format!("the hub sent no {what} list")),
        }
    }

    pub fn workspaces(&self) -> Result<Vec<Value>, String> {
        self.list("workspaces")
    }

    pub fn sessions(&self) -> Result<Vec<Value>, String> {
        self.list("sessions")
    }
}

fn str_field<'a>(item: &'a Value, field: &str) -> &'a str {
    item.get(field).and_then(Value::as_str).unwrap_or("")
}

/// Find the item `query` names among `items` (each with `id` and
/// `name`): an exact id, else an exact name, else a unique id prefix.
/// Ambiguity is an error listing the candidates (`what` names the kind).
pub fn resolve<'a>(items: &'a [Value], query: &str, what: &str) -> Result<&'a Value, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err(format!("empty {what} name"));
    }
    if let Some(item) = items.iter().find(|i| str_field(i, "id") == query) {
        return Ok(item);
    }
    let pick = |matches: Vec<&'a Value>, how: &str| -> Result<Option<&'a Value>, String> {
        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            many => Err(format!(
                "{what} {how} {query:?} is ambiguous: {}; use the id",
                many.iter()
                    .map(|i| format!("{} ({:?})", str_field(i, "id"), str_field(i, "name")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    };
    let named = items
        .iter()
        .filter(|i| str_field(i, "name") == query)
        .collect();
    if let Some(item) = pick(named, "name")? {
        return Ok(item);
    }
    let prefixed = items
        .iter()
        .filter(|i| str_field(i, "id").starts_with(query))
        .collect();
    pick(prefixed, "id prefix")?.ok_or_else(|| format!("no {what} named {query:?}"))
}

fn short(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

/// A session's state for humans: `running`, `exited 0`, `killed (Hangup)`,
/// `stopped`, `stopped (restore failed: ...)`, `gone` (recorded running
/// but without a terminal on this hub).
pub fn state_label(session: &Value) -> String {
    match str_field(session, "state") {
        "running" if session.get("live").is_some_and(|l| !l.is_null()) => "running".into(),
        "running" => "gone".into(),
        "exited" => match session.get("signal").and_then(Value::as_str) {
            Some(signal) => format!("killed ({signal})"),
            None => format!(
                "exited {}",
                session
                    .get("code")
                    .and_then(Value::as_i64)
                    .map_or_else(|| "?".to_owned(), |c| c.to_string())
            ),
        },
        "stopped" => match session.get("restore_error").and_then(Value::as_str) {
            Some(error) => format!("stopped (restore failed: {error})"),
            None => "stopped".into(),
        },
        other => other.to_owned(),
    }
}

fn fail(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "fw hub: {message}");
    1
}

fn connect(state: &StateDirArg) -> Result<HubConn, String> {
    HubConn::connect(&resolve_state_dir(state)?)
}

fn print_json(out: &mut dyn Write, value: &Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

// --- ls ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct LsArgs {
    /// Print JSON: {"workspaces":[{...,"sessions":[...]}]}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn ls(args: &LsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let listed = connect(&args.state).and_then(|conn| Ok((conn.workspaces()?, conn.sessions()?)));
    let (workspaces, sessions) = match listed {
        Ok(lists) => lists,
        Err(message) => return fail(err, &message),
    };
    let of = |w: &Value| -> Vec<Value> {
        sessions
            .iter()
            .filter(|s| str_field(s, "workspace_id") == str_field(w, "id"))
            .cloned()
            .collect()
    };
    if args.json {
        let nested: Vec<Value> = workspaces
            .iter()
            .map(|w| {
                let mut w = w.clone();
                w["sessions"] = Value::Array(of(&w));
                w
            })
            .collect();
        print_json(out, &json!({ "workspaces": nested }));
        return 0;
    }
    if workspaces.is_empty() {
        let _ = writeln!(
            out,
            "no workspaces (create one with `fw hub new <name> --repo PATH --create` or in the web UI)"
        );
        return 0;
    }
    for (i, workspace) in workspaces.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let repos: Vec<&str> = workspace
            .get("repos")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{}  {}  {}",
            str_field(workspace, "name"),
            short(str_field(workspace, "id")),
            repos.join(" ")
        );
        let rows: Vec<[String; 5]> = of(workspace)
            .iter()
            .map(|s| {
                [
                    short(str_field(s, "id")).to_owned(),
                    str_field(s, "kind").to_owned(),
                    state_label(s),
                    str_field(s, "name").to_owned(),
                    str_field(s, "cwd").to_owned(),
                ]
            })
            .collect();
        if rows.is_empty() {
            let _ = writeln!(out, "  (no sessions)");
            continue;
        }
        let widths: Vec<usize> = (0..4)
            .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
            .collect();
        for row in &rows {
            let _ = writeln!(
                out,
                "  {:w0$}  {:w1$}  {:w2$}  {:w3$}  {}",
                row[0],
                row[1],
                row[2],
                row[3],
                row[4],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3],
            );
        }
    }
    0
}

// --- new --------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct NewArgs {
    /// Workspace id, unique id prefix, or name.
    workspace: String,
    /// shell (default), claude, or codex.
    #[arg(long, default_value = "shell", value_parser = ["shell", "claude", "codex"])]
    kind: String,
    /// One of the workspace's repos (default: its first). Relative paths
    /// are taken from the current directory.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,
    /// Session name (default: program and number).
    #[arg(long)]
    name: Option<String>,
    /// Create the workspace (named WORKSPACE, with --repo as its repo)
    /// when none matches.
    #[arg(long, requires = "repo")]
    create: bool,
    /// Print the new session as JSON instead of its id.
    #[arg(long)]
    json: bool,
    /// For shells: the argv to run instead of the hub's shell.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
    #[command(flatten)]
    state: StateDirArg,
}

/// `path` made absolute against this process's directory (the hub runs
/// elsewhere), `~` expanded.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let expanded = crate::state::expand_home(path, home.as_deref());
    std::path::absolute(&expanded).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn new(args: &NewArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match new_session(args, err) {
        Ok(view) if args.json => {
            print_json(out, &view);
            0
        }
        Ok(view) => {
            let _ = writeln!(out, "{}", str_field(&view, "id"));
            0
        }
        Err(message) => fail(err, &message),
    }
}

fn new_session(args: &NewArgs, err: &mut dyn Write) -> Result<Value, String> {
    let conn = connect(&args.state)?;
    let repo = args.repo.as_deref().map(absolute).transpose()?;
    let workspaces = conn.workspaces()?;
    let workspace_id = match resolve(&workspaces, &args.workspace, "workspace") {
        Ok(workspace) => str_field(workspace, "id").to_owned(),
        Err(message) if !args.create || message.contains("ambiguous") => {
            return Err(message);
        }
        Err(_) => {
            let created = conn.call(
                "POST",
                "/api/workspaces",
                Some(&json!({ "name": args.workspace, "repos": [repo] })),
            )?;
            let _ = writeln!(
                err,
                "fw hub: created workspace {:?} ({})",
                str_field(&created, "name"),
                str_field(&created, "id")
            );
            str_field(&created, "id").to_owned()
        }
    };
    let mut body = json!({ "workspace_id": workspace_id, "kind": args.kind });
    if let Some(repo) = repo {
        body["repo"] = json!(repo);
    }
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    if !args.command.is_empty() {
        body["command"] = json!(args.command);
    }
    conn.call("POST", "/api/sessions", Some(&body))
}

// --- send / read / kill -----------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct SessionArg {
    /// Session id, unique id prefix, or name.
    session: String,
}

/// The session `query` names, as `(id, record)`.
fn find_session(conn: &HubConn, query: &str) -> Result<(String, Value), String> {
    let sessions = conn.sessions()?;
    let session = resolve(&sessions, query, "session")?;
    Ok((str_field(session, "id").to_owned(), session.clone()))
}

#[derive(Debug, clap::Args)]
pub(crate) struct SendArgs {
    #[command(flatten)]
    target: SessionArg,
    /// Text to send (`-` reads it from stdin). Sent like a paste: newlines
    /// become Enter, wrapped in bracketed-paste markers when the program
    /// enabled them (so a shell or agent TUI takes it as one paste).
    #[arg(allow_hyphen_values = true)]
    text: Option<String>,
    /// Press Enter afterwards.
    #[arg(long)]
    enter: bool,
    /// Send the bytes exactly as given (no paste handling): for control
    /// keys, e.g. `fw hub send s $'\x03' --raw` for Ctrl-C.
    #[arg(long)]
    raw: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn send(args: &SendArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = out;
    let text = match args.text.as_deref() {
        Some("-") => {
            let mut text = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut text) {
                return fail(err, &format!("cannot read stdin: {e}"));
            }
            text
        }
        Some(text) => text.to_owned(),
        None if args.enter => String::new(),
        None => return fail(err, "nothing to send: give TEXT and/or --enter"),
    };
    let result = connect(&args.state).and_then(|conn| {
        let (id, _) = find_session(&conn, &args.target.session)?;
        conn.call(
            "POST",
            &format!("/api/sessions/{id}/input"),
            Some(&json!({ "data": text, "paste": !args.raw, "enter": args.enter })),
        )
    });
    match result {
        Ok(_) => 0,
        Err(message) => fail(err, &message),
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct ReadArgs {
    #[command(flatten)]
    target: SessionArg,
    /// visible: the screen's rows; recent: the last --lines lines of
    /// scrollback plus screen.
    #[arg(long, default_value = "visible", value_parser = ["visible", "recent"])]
    source: String,
    /// Lines for --source recent (default 100).
    #[arg(long, value_name = "N")]
    lines: Option<usize>,
    /// Print the hub's JSON (cursor, modes, title, ...).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn read(args: &ReadArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.lines.is_some() && args.source != "recent" {
        return fail(err, "--lines applies to --source recent");
    }
    let result = connect(&args.state).and_then(|conn| {
        let (id, _) = find_session(&conn, &args.target.session)?;
        let mut path = format!("/api/sessions/{id}/screen?source={}", args.source);
        if let Some(lines) = args.lines {
            path.push_str(&format!("&lines={lines}"));
        }
        conn.call("GET", &path, None)
    });
    let screen = match result {
        Ok(screen) => screen,
        Err(message) => return fail(err, &message),
    };
    if args.json {
        print_json(out, &screen);
        return 0;
    }
    let mut lines: Vec<&str> = screen
        .get("lines")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    0
}

#[derive(Debug, clap::Args)]
pub(crate) struct KillArgs {
    #[command(flatten)]
    target: SessionArg,
    /// Also forget the session (its record and scrollback).
    #[arg(long)]
    rm: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn kill(args: &KillArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = connect(&args.state).and_then(|conn| {
        let (id, session) = find_session(&conn, &args.target.session)?;
        if args.rm {
            conn.call("DELETE", &format!("/api/sessions/{id}"), None)?;
            return Ok(format!(
                "removed session {:?} ({id})",
                str_field(&session, "name")
            ));
        }
        conn.call("POST", &format!("/api/sessions/{id}/kill"), None)?;
        Ok(format!(
            "hung up session {:?} ({id})",
            str_field(&session, "name")
        ))
    });
    match result {
        Ok(message) => {
            let _ = writeln!(out, "{message}");
            0
        }
        Err(message) => fail(err, &message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Value> {
        vec![
            json!({"id": "abc123", "name": "shell 1"}),
            json!({"id": "abd456", "name": "build"}),
            json!({"id": "ff0000", "name": "build"}),
            json!({"id": "e00000", "name": "abd"}),
        ]
    }

    #[test]
    fn resolves_by_id_then_name_then_unique_prefix() {
        let items = items();
        let id = |q: &str| resolve(&items, q, "session").map(|v| str_field(v, "id").to_owned());
        assert_eq!(id("abc123").unwrap(), "abc123");
        assert_eq!(id("shell 1").unwrap(), "abc123");
        assert_eq!(id("abc").unwrap(), "abc123");
        assert_eq!(id("ff").unwrap(), "ff0000");
        // A name wins over an id prefix it also is.
        assert_eq!(id("abd").unwrap(), "e00000");
        let ambiguous = id("build").unwrap_err();
        assert!(
            ambiguous.contains("ambiguous")
                && ambiguous.contains("abd456")
                && ambiguous.contains("ff0000"),
            "{ambiguous}"
        );
        assert!(id("ab").unwrap_err().contains("ambiguous"));
        assert_eq!(id("zz").unwrap_err(), "no session named \"zz\"");
        assert!(id("  ").is_err());
    }

    #[test]
    fn state_labels_explain_the_record() {
        let label = |v: Value| state_label(&v);
        assert_eq!(
            label(json!({"state": "running", "live": {"pid": 1}})),
            "running"
        );
        assert_eq!(label(json!({"state": "running", "live": null})), "gone");
        assert_eq!(
            label(json!({"state": "exited", "code": 0, "signal": null})),
            "exited 0"
        );
        assert_eq!(
            label(json!({"state": "exited", "code": null, "signal": "Hangup"})),
            "killed (Hangup)"
        );
        assert_eq!(label(json!({"state": "stopped"})), "stopped");
        assert_eq!(
            label(json!({"state": "stopped", "restore_error": "cwd gone"})),
            "stopped (restore failed: cwd gone)"
        );
    }
}
