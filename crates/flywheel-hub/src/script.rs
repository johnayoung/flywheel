//! Scripting a running hub from a shell or another agent: `fw hub ls`,
//! `new`, `send`, `read`, `kill`, `relaunch`, `import`, all over its HTTP
//! API through the one hub client ([`crate::endpoint`]): the hub running
//! for the state dir, or any by `--url` (all but `import`, whose files the
//! hub reads on its own machine).
//!
//! Sessions and workspaces are addressed by id, unique id prefix, or name.

use crate::cli::{StateDirArg, resolve_state_dir};
use crate::endpoint::{Endpoint, EndpointArgs};
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Characters of an id shown by `ls` (any unique prefix addresses it).
pub const SHORT_ID: usize = 8;

/// How messages name a session record: `name (id)`, as the hub's own
/// errors do.
pub fn session_label(session: &Value) -> String {
    format!(
        "{} ({})",
        str_field(session, "name"),
        str_field(session, "id")
    )
}

/// Find the item `query` names among `items` (each with `id` and
/// `name`): an exact id, else an exact name, else a unique id prefix.
/// Ambiguity is an error listing the candidates (`what` names the kind).
pub fn resolve<'a>(items: &'a [Value], query: &str, what: &str) -> Result<&'a Value, String> {
    resolve_by(
        items,
        |i| (str_field(i, "id"), str_field(i, "name")),
        query,
        what,
    )
}

/// [`resolve`] over any items, `key` giving each one's (id, name): the one
/// resolver every verb (and the hub's own selectors) addresses things by.
pub fn resolve_by<'a, T>(
    items: &'a [T],
    key: impl Fn(&T) -> (&str, &str),
    query: &str,
    what: &str,
) -> Result<&'a T, String> {
    find_by(items, key, query, what)?.ok_or_else(|| {
        format!(
            "no {what} matches {:?} (id, id prefix, or name)",
            query.trim()
        )
    })
}

/// [`resolve_by`], with nothing matching `query` as `None` (for a caller
/// that looks further, as the hub does across its machines).
pub fn find_by<'a, T>(
    items: &'a [T],
    key: impl Fn(&T) -> (&str, &str),
    query: &str,
    what: &str,
) -> Result<Option<&'a T>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err(format!("empty {what} name"));
    }
    if let Some(item) = items.iter().find(|i| key(i).0 == query) {
        return Ok(Some(item));
    }
    let pick = |matches: Vec<&'a T>, how: &str| -> Result<Option<&'a T>, String> {
        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            many => Err(format!(
                "{what} {how} {query:?} is ambiguous: {}; use the id",
                many.iter()
                    .map(|i| {
                        let (id, name) = key(i);
                        format!("{id} ({name:?})")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    };
    let named = items.iter().filter(|i| key(i).1 == query).collect();
    if let Some(item) = pick(named, "name")? {
        return Ok(Some(item));
    }
    let prefixed = items
        .iter()
        .filter(|i| key(i).0.starts_with(query))
        .collect();
    pick(prefixed, "id prefix")
}

fn short(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

/// A session's state for humans: `running`, `exited 0`, `killed (Hangup)`,
/// `stopped`, `not restored` (see [`restore_error`]), `gone` (recorded
/// running but without a terminal on this hub).
pub fn state_label(session: &Value) -> String {
    match str_field(session, "state") {
        "running" if session.get("live").is_some_and(|l| !l.is_null()) => {
            // An agent's status (working, waiting, done, ...) alongside.
            match session
                .get("agent")
                .and_then(|a| a.get("status"))
                .and_then(Value::as_str)
            {
                Some(status) => format!("running ({status})"),
                None => "running".into(),
            }
        }
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
        "stopped" if restore_error(session).is_some() => "not restored".into(),
        "stopped" => "stopped".into(),
        other => other.to_owned(),
    }
}

/// A session's kind, marked when a restore runs a shell in its place:
/// what it was (an agent, or a shell's own command) "now a shell".
pub fn kind_label(session: &Value) -> String {
    let kind = str_field(session, "kind");
    if session.get("fallback_command").is_none_or(Value::is_null) {
        return kind.to_owned();
    }
    let was = if kind == "shell" || kind.is_empty() {
        session
            .get("command")
            .and_then(Value::as_array)
            .and_then(|argv| argv.first())
            .and_then(Value::as_str)
            .map(|program| program.rsplit('/').next().unwrap_or(program))
            .filter(|program| !program.is_empty())
            .unwrap_or("command")
    } else {
        kind
    };
    format!("{was} (now a shell)")
}

/// Why a stopped session was not restored, when it was not.
pub fn restore_error(session: &Value) -> Option<&str> {
    session.get("restore_error").and_then(Value::as_str)
}

/// Where a session is: its process's directory while that is known, else
/// the repo it was opened in.
fn live_cwd(session: &Value) -> &str {
    session
        .get("live")
        .and_then(|l| l.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| str_field(session, "cwd"))
}

/// One workspace folder as the API lists it.
struct FolderRow {
    path: PathBuf,
    label: Option<String>,
    /// `None` from a hub that predates folders (`repos` only).
    git: Option<bool>,
    missing: bool,
}

impl FolderRow {
    fn dir_name(&self) -> Option<&str> {
        self.path.file_name().and_then(|n| n.to_str())
    }

    /// The label, when it says more than the directory's name.
    fn distinct_label(&self) -> Option<&str> {
        self.label
            .as_deref()
            .filter(|l| Some(*l) != self.dir_name())
    }

    /// `path`, with its label and plain/missing marks, for `ls`.
    fn shown(&self) -> String {
        let mut notes: Vec<&str> = self.distinct_label().into_iter().collect();
        if self.missing {
            notes.push("missing");
        } else if self.git == Some(false) {
            notes.push("no git");
        }
        let path = self.path.display();
        if notes.is_empty() {
            path.to_string()
        } else {
            format!("{path} ({})", notes.join(", "))
        }
    }
}

/// A workspace's folders; reads the legacy `repos` list from a hub that
/// predates folders.
fn folders_of(workspace: &Value) -> Vec<FolderRow> {
    if let Some(folders) = workspace.get("folders").and_then(Value::as_array) {
        return folders
            .iter()
            .map(|f| FolderRow {
                path: PathBuf::from(str_field(f, "path")),
                label: f.get("name").and_then(Value::as_str).map(str::to_owned),
                git: f.get("git").and_then(Value::as_bool),
                missing: f.get("missing").and_then(Value::as_bool) == Some(true),
            })
            .collect();
    }
    workspace
        .get("repos")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(|path| FolderRow {
                    path: PathBuf::from(path),
                    label: None,
                    git: None,
                    missing: false,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A workspace's folders for `ls`: paths (with labels), plain (non-git)
/// and missing directories marked.
fn folder_list(workspace: &Value) -> Vec<String> {
    folders_of(workspace).iter().map(FolderRow::shown).collect()
}

fn connect(endpoint: &EndpointArgs) -> Result<Endpoint, String> {
    Endpoint::resolve(endpoint)
}

// --- ls ---------------------------------------------------------------------

/// A session's git badge for `ls`: ` [branch +ahead -behind vs AGAINST
/// ~changed ?untracked]` (ahead/behind whenever there is something to
/// count against, other zero counts left out; a worktree session's
/// branch before its first read), else empty.
fn git_label(session: &Value) -> String {
    let count = |git: &Value, field: &str| git.get(field).and_then(Value::as_u64).unwrap_or(0);
    let mut parts: Vec<String> = Vec::new();
    match session.get("git").filter(|g| g.is_object()) {
        Some(git) => {
            parts.push(
                git.get("branch")
                    .and_then(Value::as_str)
                    .unwrap_or("(detached)")
                    .to_owned(),
            );
            if let Some(against) = git.get("against").and_then(Value::as_str) {
                parts.push(format!(
                    "+{} -{} vs {against}",
                    count(git, "ahead"),
                    count(git, "behind")
                ));
            }
            for (field, mark) in [
                ("ahead", "+"),
                ("behind", "-"),
                ("dirty", "~"),
                ("untracked", "?"),
            ] {
                if git.get("against").is_some_and(Value::is_string)
                    && matches!(field, "ahead" | "behind")
                {
                    continue;
                }
                let n = count(git, field);
                if n > 0 {
                    parts.push(format!("{mark}{n}"));
                }
            }
        }
        None => match session.get("worktree") {
            Some(worktree) if worktree.is_object() => {
                parts.push(str_field(worktree, "branch").to_owned());
            }
            _ => return String::new(),
        },
    }
    if session.get("worktree").is_some_and(Value::is_object) {
        parts.push("worktree".to_owned());
    }
    format!(" [{}]", parts.join(" "))
}

/// ` (companion of AGENT)` for a companion shell, else empty.
fn companion_label(session: &Value, sessions: &[Value]) -> String {
    match session.get("companion_of").and_then(Value::as_str) {
        Some(agent) => {
            let name = sessions
                .iter()
                .find(|s| str_field(s, "id") == agent)
                .map_or_else(
                    || short(agent).to_owned(),
                    |s| str_field(s, "name").to_owned(),
                );
            format!(" (companion of {name})")
        }
        None => String::new(),
    }
}

/// ` (reviewed by R)` / ` (reviews A)` for a review pairing, else empty.
fn pairing_label(session: &Value, sessions: &[Value]) -> String {
    let name_of = |id: &str| {
        sessions
            .iter()
            .find(|s| str_field(s, "id") == id)
            .map_or_else(|| short(id).to_owned(), |s| str_field(s, "name").to_owned())
    };
    let mut label = String::new();
    if let Some(reviewer) = session.get("reviewer").and_then(Value::as_str) {
        label.push_str(&format!(" (reviewed by {})", name_of(reviewer)));
    }
    if let Some(author) = session.get("review_of").and_then(Value::as_str) {
        label.push_str(&format!(" (reviews {})", name_of(author)));
    }
    label
}

/// ` (queue: 2)`, ` (queue: 2, paused)`, ` (queue stalled: why)` for a
/// session with a prompt queue, else empty.
fn queue_label(session: &Value) -> String {
    let Some(queue) = session.get("queue").filter(|q| q.is_object()) else {
        return String::new();
    };
    if let Some(stall) = queue.get("stalled").filter(|s| s.is_object()) {
        return format!(" (queue stalled: {})", str_field(stall, "reason"));
    }
    let pending = queue.get("pending").and_then(Value::as_u64).unwrap_or(0);
    match (pending, queue.get("paused") == Some(&Value::Bool(true))) {
        (0, false) => String::new(),
        (n, false) => format!(" (queue: {n})"),
        (n, true) => format!(" (queue: {n}, paused)"),
    }
}

/// A token count for humans: `950`, `12.3k`, `4.56M`.
pub fn tokens_label(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

/// A cost for humans: `$0.42`, `$0.0031`; marked `+` when some tokens
/// are of a model without a known price.
pub fn cost_label(cost: &Value) -> String {
    let usd = cost.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
    let priced = cost.get("priced").and_then(Value::as_bool) != Some(false);
    let amount = crate::util::dollars(usd);
    if priced {
        amount
    } else if usd == 0.0 {
        "unpriced".to_owned()
    } else {
        format!("{amount}+unpriced")
    }
}

/// ` · 12.3k tok $0.42` for an agent session with usage, else empty.
fn usage_label(session: &Value) -> String {
    match session.get("usage").filter(|u| u.is_object()) {
        Some(usage) => format!(
            " \u{b7} {} tok {}",
            tokens_label(
                usage
                    .get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            cost_label(usage)
        ),
        None => String::new(),
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct LsArgs {
    /// Print JSON: {"workspaces":[{...,"sessions":[...]}],"usage":{...}}
    /// (each agent session with its `usage`; `usage` is the hub's and
    /// each workspace's today, this week, and all time).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn ls(args: &LsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let conn = match connect(&args.endpoint) {
        Ok(conn) => conn,
        Err(message) => return fail(err, &message),
    };
    let listed = conn
        .blocking_workspaces()
        .and_then(|w| Ok((w, conn.blocking_sessions()?)));
    let (workspaces, sessions) = match listed {
        Ok(lists) => lists,
        Err(message) => return fail(err, &message),
    };
    // A hub from before usage has no /api/usage: the listing stands.
    let usage = conn.blocking_call("GET", "/api/usage", None).ok();
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
        let mut listing = json!({ "workspaces": nested });
        if let Some(usage) = usage {
            listing["usage"] = usage;
        }
        print_json(out, &listing);
        return 0;
    }
    if workspaces.is_empty() {
        let _ = writeln!(
            out,
            "no workspaces (create one with `fw hub new <name> --folder PATH --create` or in the web UI)"
        );
        return 0;
    }
    for (i, workspace) in workspaces.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let _ = writeln!(
            out,
            "{}  {}  {}",
            str_field(workspace, "name"),
            short(str_field(workspace, "id")),
            folder_list(workspace).join(" ")
        );
        let listed = of(workspace);
        let rows: Vec<[String; 5]> = listed
            .iter()
            .map(|s| {
                [
                    short(str_field(s, "id")).to_owned(),
                    kind_label(s),
                    state_label(s),
                    str_field(s, "name").to_owned(),
                    format!(
                        "{}{}{}{}{}{}",
                        live_cwd(s),
                        git_label(s),
                        companion_label(s, &sessions),
                        pairing_label(s, &sessions),
                        queue_label(s),
                        usage_label(s)
                    ),
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
        for (row, session) in rows.iter().zip(&listed) {
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
            if let Some(error) = restore_error(session) {
                let _ = writeln!(
                    out,
                    "  {:w0$}  {error}; `fw hub relaunch {}`",
                    "",
                    row[0],
                    w0 = widths[0]
                );
            }
        }
    }
    if let Some(line) = usage.as_ref().and_then(usage_footer) {
        let _ = writeln!(out, "\n{line}");
    }
    0
}

/// `usage: today $1.20 (310k tok), this week $8.95 (2.1M tok)` when the
/// hub counted any.
fn usage_footer(usage: &Value) -> Option<String> {
    let total = usage.get("total")?;
    if total
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        == 0
    {
        return None;
    }
    let part = |key: &str| {
        let cost = usage.get(key).cloned().unwrap_or(Value::Null);
        format!(
            "{} ({} tok)",
            cost_label(&cost),
            tokens_label(
                cost.get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            )
        )
    };
    Some(format!(
        "usage: today {}, this week {}, all time {}",
        part("today"),
        part("week"),
        part("total")
    ))
}

// --- new --------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct NewArgs {
    /// Workspace id, unique id prefix, or name.
    workspace: String,
    /// shell, claude, or codex (default: the template's, else shell).
    #[arg(long, value_parser = ["shell", "claude", "codex"])]
    kind: Option<String>,
    /// Start from the workspace's launch template NAME (kind, prompt,
    /// model, permission options, worktree); flags given here win.
    #[arg(long, value_name = "NAME")]
    template: Option<String>,
    #[command(flatten)]
    launch: crate::template_cli::LaunchFlags,
    /// One of the workspace's folders (default: its first): a path
    /// (relative to the current directory), or a folder's label or
    /// directory name.
    #[arg(long, visible_alias = "repo", value_name = "FOLDER")]
    folder: Option<PathBuf>,
    /// Session name (default: program and number).
    #[arg(long)]
    name: Option<String>,
    /// Create the workspace (named WORKSPACE, with --folder as its
    /// folder) when none matches.
    #[arg(long, requires = "folder")]
    create: bool,
    /// Run it in a new sibling worktree of its (git) folder:
    /// `<folder-parent>/<folder>.worktrees/TASK` on branch TASK (after the
    /// hub's --worktree-branch-prefix). Names the session TASK unless
    /// --name.
    #[arg(long, value_name = "TASK")]
    worktree: Option<String>,
    /// Branch or commit the worktree's branch is cut from (default: the
    /// folder's current branch).
    #[arg(long, requires = "worktree", value_name = "REF")]
    base: Option<String>,
    /// Print the new session as JSON instead of its id.
    #[arg(long)]
    json: bool,
    /// For shells: the argv to run instead of the hub's shell.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
    #[command(flatten)]
    endpoint: EndpointArgs,
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
    let conn = connect(&args.endpoint)?;
    // Names address sessions in every other verb: a duplicate would make
    // both unreachable by name.
    if let Some(name) = &args.name {
        let sessions = conn.blocking_sessions()?;
        if let Some(taken) = sessions
            .iter()
            .find(|s| str_field(s, "name") == name.trim())
        {
            return Err(format!(
                "a session named {name:?} already exists ({}); pick another --name",
                str_field(taken, "id")
            ));
        }
    }
    let workspaces = conn.blocking_workspaces()?;
    let (workspace_id, folder) = match resolve(&workspaces, &args.workspace, "workspace") {
        Ok(workspace) => {
            let folder = args
                .folder
                .as_deref()
                .map(|query| pick_folder(workspace, query))
                .transpose()?;
            (str_field(workspace, "id").to_owned(), folder)
        }
        Err(message) if !args.create || message.contains("ambiguous") => {
            return Err(message);
        }
        Err(_) => {
            let folder = args.folder.as_deref().map(absolute).transpose()?;
            // Check the worktree first: a refused one must not leave an
            // empty workspace behind.
            if let (Some(task), Some(folder)) =
                (&args.worktree, folder.as_ref().filter(|f| f.is_dir()))
            {
                let mut query = format!(
                    "folder={}&task={}",
                    query_escape(&folder.to_string_lossy()),
                    query_escape(task)
                );
                if let Some(base) = &args.base {
                    query.push_str(&format!("&base={}", query_escape(base)));
                }
                let plan =
                    conn.blocking_call("GET", &format!("/api/worktrees/plan?{query}"), None)?;
                if let Some(error) = plan.get("error").and_then(Value::as_str) {
                    return Err(error.to_owned());
                }
            }
            let created = conn.blocking_call(
                "POST",
                "/api/workspaces",
                Some(&json!({ "name": args.workspace, "folders": [folder] })),
            )?;
            let _ = writeln!(
                err,
                "fw hub: created workspace {:?} ({})",
                str_field(&created, "name"),
                str_field(&created, "id")
            );
            (str_field(&created, "id").to_owned(), folder)
        }
    };
    let template = args
        .template
        .as_deref()
        .map(|name| crate::template_cli::find(&conn, &workspace_id, name))
        .transpose()?;
    let kind = args
        .kind
        .clone()
        .or_else(|| template.as_ref().map(|t| str_field(t, "kind").to_owned()))
        .unwrap_or_else(|| "shell".to_owned());
    let mut body = json!({ "workspace_id": workspace_id, "kind": kind });
    if let Some(template) = &template {
        template_options(template, &kind, &mut body);
    }
    args.launch.apply(&mut body, args.launch.prompt_text()?);
    if body.get("permission_mode").and_then(Value::as_str) == Some("bypassPermissions")
        || (body.get("approval").and_then(Value::as_str) == Some("never")
            && body.get("sandbox").and_then(Value::as_str) == Some("danger-full-access"))
    {
        let _ = writeln!(
            err,
            "fw hub: warning: this agent runs every tool (shell, edits, network) without asking"
        );
    }
    // A template that runs in a worktree gets one named after it, unless
    // --worktree names it.
    let worktree = args.worktree.clone().or_else(|| {
        template
            .as_ref()
            .filter(|t| t.get("worktree").and_then(Value::as_bool) == Some(true))
            .map(|t| crate::template_cli::task_from(str_field(t, "name")))
    });
    if let Some(folder) = folder {
        body["folder"] = json!(folder);
    }
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    if !args.command.is_empty() {
        body["command"] = json!(args.command);
    }
    if let Some(task) = &worktree {
        body["worktree"] = json!({ "task": task, "base": args.base });
    }
    let view = conn.blocking_call("POST", "/api/sessions", Some(&body))?;
    if let Some(worktree) = view.get("worktree") {
        let _ = writeln!(
            err,
            "fw hub: worktree {} on branch {} (from {})",
            str_field(worktree, "path"),
            str_field(worktree, "branch"),
            str_field(worktree, "base")
        );
    }
    Ok(view)
}

/// Copy `template`'s launch options into `body`: only those a `kind`
/// session takes. `--kind` may override the template's kind; then only
/// the prompt carries over (a Claude model or permission mode is not
/// Codex's, and a shell takes neither).
fn template_options(template: &Value, kind: &str, body: &mut Value) {
    let same = template.get("kind").and_then(Value::as_str) == Some(kind);
    let keys: &[&str] = match kind {
        "claude" if same => &["prompt", "model", "permission_mode"],
        "codex" if same => &["prompt", "model", "approval", "sandbox"],
        "claude" | "codex" => &["prompt"],
        _ => &[],
    };
    for key in keys {
        if let Some(value) = template.get(*key).filter(|v| v.is_string()) {
            body[*key] = value.clone();
        }
    }
}

/// `raw` percent-escaped for a URL query value (RFC 3986 unreserved
/// characters kept).
pub(crate) fn query_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The folder of `workspace` that `query` names: a path (relative to this
/// directory) that is one of its folders, else a folder's label or
/// directory name (unique). A path naming none of them is passed on for
/// the hub to refuse.
pub(crate) fn pick_folder(workspace: &Value, query: &Path) -> Result<PathBuf, String> {
    let folders = folders_of(workspace);
    let path = absolute(query)?;
    if folders.iter().any(|f| f.path == path) {
        return Ok(path);
    }
    let wanted = query.to_string_lossy();
    let matches: Vec<&FolderRow> = folders
        .iter()
        .filter(|f| f.label.as_deref() == Some(&*wanted) || f.dir_name() == Some(&*wanted))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.path.clone()),
        [] if path.exists() => Ok(path),
        [] => Err(format!(
            "workspace {:?} has no folder {wanted:?}; its folders: {}",
            str_field(workspace, "name"),
            folders
                .iter()
                .map(FolderRow::shown)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        many => Err(format!(
            "folder {wanted:?} is ambiguous in workspace {:?}: {}; give the path",
            str_field(workspace, "name"),
            many.iter()
                .map(|f| f.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

// --- import -----------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct ImportArgs {
    /// VS Code multi-root workspace files (*.code-workspace).
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Workspace name (default: the file name without .code-workspace).
    /// One file only.
    #[arg(long)]
    name: Option<String>,
    /// Replace the folders of the workspace already so named (refused
    /// when that drops a folder a running session is in).
    #[arg(long)]
    update: bool,
    /// Print each import report as JSON (one document per line).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

/// `fw hub import`: every file is tried; exit 1 when any failed.
pub(crate) fn import(args: &ImportArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.name.is_some() && args.files.len() > 1 {
        return fail(err, "--name names one workspace: give a single file");
    }
    let conn = match resolve_state_dir(&args.state).and_then(|dir| Endpoint::local(&dir)) {
        Ok(conn) => conn,
        Err(message) => return fail(err, &message),
    };
    let mut failed = false;
    for file in &args.files {
        let path = absolute(file);
        let report = path.as_ref().map_err(Clone::clone).and_then(|path| {
            let mut body = json!({ "path": path, "update": args.update });
            if let Some(name) = &args.name {
                body["name"] = json!(name);
            }
            conn.blocking_call("POST", "/api/workspaces/import", Some(&body))
        });
        match report {
            Ok(report) if args.json => {
                let _ = writeln!(out, "{report}");
            }
            Ok(report) => print_import(&report, out, err),
            Err(message) => {
                failed = true;
                // Most refusals already name the file.
                let named = path
                    .as_ref()
                    .is_ok_and(|p| message.contains(&*p.to_string_lossy()));
                if named {
                    let _ = writeln!(err, "fw hub: {message}");
                } else {
                    let _ = writeln!(err, "fw hub: {}: {message}", file.display());
                }
            }
        }
    }
    i32::from(failed)
}

/// An import report for humans: what was made, each folder (git or plain),
/// and every entry skipped with why.
fn print_import(report: &Value, out: &mut dyn Write, err: &mut dyn Write) {
    let workspace = &report["workspace"];
    let verb = if report["created"].as_bool() == Some(true) {
        "created"
    } else {
        "updated"
    };
    let _ = writeln!(
        out,
        "{verb} workspace {:?} ({}) from {}",
        str_field(workspace, "name"),
        short(str_field(workspace, "id")),
        str_field(report, "file")
    );
    let folders = workspace["folders"].as_array().cloned().unwrap_or_default();
    for folder in &folders {
        let path = str_field(folder, "path");
        // A label that only repeats the directory's name adds nothing.
        let label = match folder.get("name").and_then(Value::as_str) {
            Some(name) if Some(name) != Path::new(path).file_name().and_then(|n| n.to_str()) => {
                format!("  ({name})")
            }
            _ => String::new(),
        };
        let kind = if folder["git"].as_bool() == Some(true) {
            "git   "
        } else {
            "folder"
        };
        let _ = writeln!(out, "  {kind}  {path}{label}");
    }
    let skipped = report["skipped"].as_array().cloned().unwrap_or_default();
    for entry in &skipped {
        let at = match entry.get("path").and_then(Value::as_str) {
            Some(path) if path != str_field(entry, "entry") => format!(" -> {path}"),
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "  skipped {}{at}: {}",
            str_field(entry, "entry"),
            str_field(entry, "reason")
        );
    }
    if !skipped.is_empty() {
        let _ = writeln!(
            err,
            "fw hub: {:?}: {} of {} folders not imported (listed above)",
            str_field(workspace, "name"),
            skipped.len(),
            skipped.len() + folders.len()
        );
    }
}

// --- send / read / kill -----------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct SessionArg {
    /// Session id, unique id prefix, or name.
    session: String,
}

/// The session `query` names, as `(id, record)`.
/// The session `query` names on `conn` (id, unique id prefix, or name):
/// `(id, record)`.
pub(crate) fn find_session(conn: &Endpoint, query: &str) -> Result<(String, Value), String> {
    let sessions = conn.blocking_sessions()?;
    let session = resolve(&sessions, query, "session")?;
    Ok((str_field(session, "id").to_owned(), session.clone()))
}

#[derive(Debug, clap::Args)]
pub(crate) struct SendArgs {
    #[command(flatten)]
    target: SessionArg,
    /// Text to send (`-` reads it from stdin). Sent like a paste: newlines
    /// become Enter, wrapped in bracketed-paste markers when the program
    /// enabled them (so a shell or agent TUI takes it as one paste). A
    /// trailing newline is sent as Enter after the paste, so `printf
    /// 'make\n' | fw hub send s -` runs it.
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
    endpoint: EndpointArgs,
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
    let (text, enter) = if args.raw {
        (text, args.enter)
    } else {
        split_final_enter(text, args.enter)
    };
    let result = connect(&args.endpoint).and_then(|conn| {
        let (id, _) = find_session(&conn, &args.target.session)?;
        conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/input"),
            Some(&json!({ "data": text, "paste": !args.raw, "enter": enter })),
        )
    });
    match result {
        Ok(_) => 0,
        Err(message) => fail(err, &message),
    }
}

/// A pasted text's final line break, as the Enter that follows the paste:
/// inside a bracketed paste it would only add a line to the prompt.
fn split_final_enter(mut text: String, enter: bool) -> (String, bool) {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
        return (text, true);
    }
    (text, enter)
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
    endpoint: EndpointArgs,
}

pub(crate) fn read(args: &ReadArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.lines.is_some() && args.source != "recent" {
        return fail(err, "--lines applies to --source recent");
    }
    let result = connect(&args.endpoint).and_then(|conn| {
        let (id, _) = find_session(&conn, &args.target.session)?;
        let mut path = format!("/api/sessions/{id}/screen?source={}", args.source);
        if let Some(lines) = args.lines {
            path.push_str(&format!("&lines={lines}"));
        }
        conn.blocking_call("GET", &path, None)
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
    /// With --rm: also remove the session's worktree (and its companion
    /// shells). Refused when it has uncommitted changes or commits its
    /// base lacks, unless --force; the branch is deleted only when merged.
    #[arg(long, requires = "rm")]
    remove_worktree: bool,
    /// With --remove-worktree: remove it even so (uncommitted work is
    /// lost; an unmerged branch is kept).
    #[arg(long, requires = "remove_worktree")]
    force: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn kill(args: &KillArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = connect(&args.endpoint).and_then(|conn| {
        let (id, session) = find_session(&conn, &args.target.session)?;
        if args.remove_worktree {
            let outcome = conn.blocking_call(
                "DELETE",
                &format!(
                    "/api/sessions/{id}?remove_worktree=true&force={}",
                    args.force
                ),
                None,
            )?;
            let worktree = outcome.get("worktree").cloned().unwrap_or(Value::Null);
            let companions = outcome
                .get("removed")
                .and_then(Value::as_array)
                .map_or(0, |r| r.len().saturating_sub(1));
            let branch = str_field(&worktree, "branch");
            return Ok(format!(
                "removed session {:?} ({id}){}, worktree {}; branch {branch} {}",
                str_field(&session, "name"),
                match companions {
                    0 => String::new(),
                    1 => " and its companion shell".to_owned(),
                    n => format!(" and {n} companion shells"),
                },
                str_field(&worktree, "path"),
                if worktree.get("branch_deleted").and_then(Value::as_bool) == Some(true) {
                    "deleted (merged)"
                } else {
                    "kept (it has commits its base lacks)"
                }
            ));
        }
        if args.rm {
            conn.blocking_call("DELETE", &format!("/api/sessions/{id}"), None)?;
            return Ok(format!(
                "removed session {:?} ({id})",
                str_field(&session, "name")
            ));
        }
        conn.blocking_call("POST", &format!("/api/sessions/{id}/kill"), None)?;
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

#[derive(Debug, clap::Args)]
pub(crate) struct RelaunchArgs {
    #[command(flatten)]
    target: SessionArg,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn relaunch(args: &RelaunchArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = connect(&args.endpoint).and_then(|conn| {
        let (id, _) = find_session(&conn, &args.target.session)?;
        conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/relaunch"),
            Some(&json!({})),
        )
    });
    match result {
        Ok(view) => {
            let _ = writeln!(
                out,
                "relaunched session {:?} ({}) in {}",
                str_field(&view, "name"),
                str_field(&view, "id"),
                str_field(&view, "cwd")
            );
            0
        }
        Err(message) => fail(err, &message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_costs_read_as_the_activity_digest_and_the_web_ui_do() {
        let label = |usd: f64| cost_label(&json!({ "cost_usd": usd }));
        assert_eq!(label(0.0), "$0");
        assert_eq!(label(0.0042), "$0.0042");
        assert_eq!(label(3.5), "$3.50");
        assert_eq!(label(1234.4), "$1234");
        assert_eq!(
            cost_label(&json!({ "cost_usd": 0.4, "priced": false })),
            "$0.40+unpriced"
        );
    }

    #[test]
    fn a_kind_override_takes_only_the_template_options_that_kind_has() {
        let template = json!({"kind": "claude", "prompt": "go", "model": "opus",
            "permission_mode": "plan", "worktree": true});
        let mut body = json!({});
        template_options(&template, "codex", &mut body);
        assert_eq!(body, json!({"prompt": "go"}));
        let mut body = json!({});
        template_options(&template, "shell", &mut body);
        assert_eq!(body, json!({}));
        let mut body = json!({});
        template_options(&template, "claude", &mut body);
        assert_eq!(
            body,
            json!({"prompt": "go", "model": "opus", "permission_mode": "plan"})
        );
    }

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
        assert_eq!(
            id("zz").unwrap_err(),
            "no session matches \"zz\" (id, id prefix, or name)"
        );
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
            label(json!({"state": "running", "live": {"pid": 1}, "agent": {"status": "done"}})),
            "running (done)"
        );
        assert_eq!(kind_label(&json!({"kind": "claude"})), "claude");
        assert_eq!(
            kind_label(&json!({"kind": "claude", "fallback_command": ["/bin/sh"]})),
            "claude (now a shell)"
        );
        assert_eq!(
            kind_label(
                &json!({"kind": "shell", "command": ["/usr/bin/fw", "worker"], "fallback_command": ["/bin/sh"]})
            ),
            "fw (now a shell)"
        );
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
            "not restored"
        );
    }

    #[test]
    fn a_final_newline_becomes_enter() {
        let split = |t: &str, e| split_final_enter(t.to_owned(), e);
        assert_eq!(split("ls\n", false), ("ls".to_owned(), true));
        assert_eq!(split("a\nb\r\n", false), ("a\nb".to_owned(), true));
        assert_eq!(split("ls", false), ("ls".to_owned(), false));
        assert_eq!(split("ls", true), ("ls".to_owned(), true));
        assert_eq!(split("\n", false), (String::new(), true));
    }

    #[test]
    fn ls_marks_plain_folders_and_reads_legacy_repos() {
        let ws = json!({"folders": [
            {"path": "/code/app", "git": true},
            {"path": "/notes", "git": false, "name": "Notes"}
        ]});
        assert_eq!(folder_list(&ws), ["/code/app", "/notes (Notes, no git)"]);
        assert_eq!(folder_list(&json!({"repos": ["/r"]})), ["/r"]);
        // A deleted folder is not mistaken for a plain one; a label that
        // repeats the directory's name is not shown.
        let gone = json!({"folders": [
            {"path": "/code/gone", "git": false, "missing": true, "name": "gone"}
        ]});
        assert_eq!(folder_list(&gone), ["/code/gone (missing)"]);
    }

    #[test]
    fn new_picks_a_folder_by_path_label_or_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("john-finances");
        let b = dir.path().join("pickem");
        let c = dir.path().join("other").join("pickem");
        let ws = json!({"name": "personal", "folders": [
            {"path": a, "git": false, "name": "Money"},
            {"path": b, "git": false},
        ]});
        assert_eq!(pick_folder(&ws, Path::new("Money")).unwrap(), a);
        assert_eq!(pick_folder(&ws, Path::new("john-finances")).unwrap(), a);
        assert_eq!(pick_folder(&ws, Path::new("pickem")).unwrap(), b);
        assert_eq!(pick_folder(&ws, &b).unwrap(), b);
        let missing = pick_folder(&ws, Path::new("nope")).unwrap_err();
        assert!(missing.contains("no folder \"nope\""), "{missing}");
        assert!(missing.contains("Money"), "{missing}");
        let twice = json!({"name": "w", "folders": [{"path": b}, {"path": c}]});
        assert!(
            pick_folder(&twice, Path::new("pickem"))
                .unwrap_err()
                .contains("ambiguous")
        );
    }

    #[test]
    fn import_reports_folders_and_skips() {
        let report = json!({
            "created": true,
            "file": "/w/personal.code-workspace",
            "workspace": {"id": "0123456789ab", "name": "personal", "folders": [
                {"path": "/code/app", "git": true, "name": "App"},
                {"path": "/pickem", "git": false}
            ]},
            "skipped": [
                {"entry": "../gone", "path": "/gone", "reason": "does not exist"},
                {"entry": "vscode-remote://ssh-remote+box/x", "reason": "remote"}
            ]
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        print_import(&report, &mut out, &mut err);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "created workspace \"personal\" (01234567) from /w/personal.code-workspace\n\
             \x20 git     /code/app  (App)\n\
             \x20 folder  /pickem\n\
             \x20 skipped ../gone -> /gone: does not exist\n\
             \x20 skipped vscode-remote://ssh-remote+box/x: remote\n"
        );
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("2 of 4 folders not imported"), "{err}");
    }

    #[test]
    fn ls_shows_the_live_directory() {
        let s = json!({"cwd": "/repo", "live": {"cwd": "/repo/sub"}});
        assert_eq!(live_cwd(&s), "/repo/sub");
        assert_eq!(live_cwd(&json!({"cwd": "/repo", "live": null})), "/repo");
    }
}
