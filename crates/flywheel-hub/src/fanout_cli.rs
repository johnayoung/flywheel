//! `fw hub fanout new|ls|show|wait|grade|fix|pick|promote|cleanup|rm`: verified
//! fan-outs over the running hub's API (see `crate::fanout`).
//!
//! Fan-outs are named by id, unique id prefix, or name; candidates by
//! label (`c1`) or session name.

use crate::endpoint::{Endpoint, EndpointArgs, client_id};
use crate::script::{pick_folder, resolve, tokens_label};
use crate::util::{fail, str_field};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Exit code of `wait` past `--timeout` (as `timeout(1)`).
pub const TIMED_OUT: i32 = 124;
/// A launch cuts worktrees and starts agents; a pick merges.
const ACTION_TIMEOUT: Duration = Duration::from_secs(330);
/// How long `wait` keeps reconnecting while the hub restarts.
const RECONNECT_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, clap::Args)]
pub struct FanoutArgs {
    #[command(subcommand)]
    command: FanoutCommand,
}

#[derive(Debug, clap::Subcommand)]
enum FanoutCommand {
    /// Launch agents on one prompt, each in its own sibling worktree cut
    /// from the same base commit; prints the fan-out's id.
    New(Box<NewArgs>),
    /// List fan-outs, newest first.
    Ls(ListArgs),
    /// A fan-out's ranked board: verdicts, diff, cost, and why.
    Show(ShowArgs),
    /// Wait until nothing is left to wait for (every agent finished and
    /// graded, or ended). Exit 0 settled, 124 --timeout, 1 error.
    Wait(WaitArgs),
    /// Grade a candidate again now.
    Grade(CandidateArgs),
    /// Paste what failed (the grader, its output) into a candidate's agent;
    /// its next finished turn is graded again.
    Fix(ConfirmArgs),
    /// Merge the winner into the base (git merge --ff-only in the main
    /// checkout), only while its worktree is exactly what passed.
    Pick(PickArgs),
    /// Write the fan-out as a flywheel task of its repo's directory work
    /// source (prompt as context, graders as command graders, the winner's
    /// commit as a reference), for `fw worker` to verify again: shows the
    /// file first, writes it uncommitted.
    Promote(PromoteArgs),
    /// Remove the losers' worktrees and sessions (default: every candidate
    /// but the winner); refused where unique work would be lost unless
    /// --force.
    Cleanup(CleanupArgs),
    /// Forget a fan-out (its sessions and worktrees stay).
    Rm(FanoutRef),
}

#[derive(Debug, clap::Args)]
struct NewArgs {
    /// Workspace id, unique id prefix, or name.
    workspace: String,
    /// The task every agent gets (`-` reads it from stdin). With
    /// --from-task: default the task's goal, context, and graders.
    #[arg(long, value_name = "TEXT", required_unless_present = "from_task")]
    prompt: Option<String>,
    /// Seed from this flywheel task of the folder (its directory source):
    /// its prompt and command graders unless given, plus its held-out
    /// graders, which run but are never shown to the agents.
    #[arg(long, value_name = "TASK_ID")]
    from_task: Option<String>,
    /// One agent: claude or codex, with a model after a colon
    /// (`claude:opus`, `codex:gpt-5.1-codex`). Give 2 to 6.
    #[arg(long = "agent", value_name = "KIND[:MODEL]", required = true)]
    agents: Vec<String>,
    /// A grader command, run with /bin/sh -c in each candidate's worktree
    /// (in order; the first failure skips the rest). `name=COMMAND` names
    /// it when name is a lowercase word (`unit=cargo test`; an uppercase
    /// `VAR=1 cmd` stays a command). Agents are not shown these.
    #[arg(
        long = "grader",
        value_name = "COMMAND",
        required_unless_present = "from_task"
    )]
    graders: Vec<String>,
    /// One of the workspace's git folders (default: the first).
    #[arg(long, value_name = "PATH|LABEL")]
    folder: Option<std::path::PathBuf>,
    /// Base branch (default: the folder's current branch).
    #[arg(long, value_name = "BRANCH")]
    base: Option<String>,
    /// Display name (default: the prompt's first words).
    #[arg(long)]
    name: Option<String>,
    /// Per-grader time limit in seconds.
    #[arg(long, value_name = "SECS", default_value_t = crate::fanout::DEFAULT_TIMEOUT_SECS)]
    timeout: u64,
    /// A path (relative to the repo; a directory covers what is under it)
    /// candidates must not change: changing it fails them.
    #[arg(long, value_name = "PATH")]
    protect: Vec<String>,
    /// Claude agents: --permission-mode.
    #[arg(long, value_name = "MODE")]
    permission_mode: Option<String>,
    /// Codex agents: --ask-for-approval.
    #[arg(long, value_name = "POLICY")]
    approval: Option<String>,
    /// Codex agents: --sandbox.
    #[arg(long, value_name = "POLICY")]
    sandbox: Option<String>,
    /// Do not append the instruction to commit the work when done.
    #[arg(long)]
    no_commit_instruction: bool,
    /// Print the fan-out as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ListArgs {
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct FanoutRef {
    /// Fan-out id, unique id prefix, or name.
    fanout: String,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// Fan-out id, unique id prefix, or name.
    fanout: String,
    /// Also print each grader's output tail.
    #[arg(long, short = 'v')]
    verbose: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct WaitArgs {
    /// Fan-out id, unique id prefix, or name.
    fanout: String,
    /// Give up after this many seconds (exit 124).
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,
    /// Print the settled board as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct CandidateArgs {
    /// Fan-out id, unique id prefix, or name.
    fanout: String,
    /// Candidate label (c1) or session name.
    candidate: String,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ConfirmArgs {
    fanout: String,
    candidate: String,
    /// Run without asking (what runs is still printed). Without it a
    /// terminal is asked; anything else only prints it.
    #[arg(long, short = 'y')]
    yes: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct PickArgs {
    fanout: String,
    candidate: String,
    #[arg(long, short = 'y')]
    yes: bool,
    /// When the base moved, rebase the candidate onto it first; it is
    /// graded again and must pass before it can be picked (run pick again).
    #[arg(long)]
    rebase: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct PromoteArgs {
    fanout: String,
    /// The task id (default: the fan-out's name and tag).
    #[arg(long, value_name = "ID")]
    task_id: Option<String>,
    /// The `active/<phase>` directory (default: a new phase after the
    /// existing ones).
    #[arg(long, value_name = "NAME")]
    phase: Option<String>,
    /// The goal (default: the seeding task's, else the prompt's first line).
    #[arg(long, value_name = "TEXT")]
    goal: Option<String>,
    /// The candidate whose passing commit is referenced (default: the
    /// winner); `none` for no reference.
    #[arg(long, value_name = "LABEL|none")]
    reference: Option<String>,
    #[arg(long, short = 'y')]
    yes: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct CleanupArgs {
    fanout: String,
    /// Candidates (labels or session names); default: all but the winner.
    candidates: Vec<String>,
    /// Only stop their sessions; keep the worktrees.
    #[arg(long)]
    stop_only: bool,
    /// Remove worktrees holding work nothing else has (commits not in the
    /// base, uncommitted changes).
    #[arg(long)]
    force: bool,
    #[arg(long, short = 'y')]
    yes: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// Output whose reader may go away (`| head`): from then on writes are
/// dropped, and the command ends quietly.
struct Piped<'a> {
    inner: &'a mut dyn Write,
    closed: bool,
}

impl Write for Piped<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed {
            return Ok(buf.len());
        }
        match self.inner.write(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(buf.len())
            }
            other => other,
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }
        match self.inner.flush() {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            other => other,
        }
    }
}

/// Run `fw hub fanout`.
pub(crate) fn main(args: &FanoutArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut piped = Piped {
        inner: out,
        closed: false,
    };
    let out: &mut dyn Write = &mut piped;
    let result = match &args.command {
        FanoutCommand::New(a) => new(a, out, err),
        FanoutCommand::Ls(a) => ls(a, out),
        FanoutCommand::Show(a) => show(a, out),
        FanoutCommand::Wait(a) => wait(a, out),
        FanoutCommand::Grade(a) => grade(a, out),
        FanoutCommand::Fix(a) => fix(a, out, err),
        FanoutCommand::Pick(a) => pick(a, out, err),
        FanoutCommand::Promote(a) => promote(a, out, err),
        FanoutCommand::Cleanup(a) => cleanup(a, out, err),
        FanoutCommand::Rm(a) => rm(a, out),
    };
    let _ = out.flush();
    match result {
        Ok(code) => code,
        Err(_) if piped.closed => 0,
        Err(message) => fail(err, &message),
    }
}

fn print_json(out: &mut dyn Write, value: &Value) -> Result<(), String> {
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
    .map_err(|e| e.to_string())
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Ask on the terminal; `false` unless the answer is yes.
fn ask(question: &str, err: &mut dyn Write) -> bool {
    let _ = write!(err, "{question} [y/N] ");
    let _ = err.flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
}

fn approved(yes: bool, json: bool, question: &str, err: &mut dyn Write) -> bool {
    if yes {
        return true;
    }
    if json || !std::io::stdin().is_terminal() {
        return false;
    }
    ask(question, err)
}

/// The fan-out `query` names: `(id, view)`.
fn find(conn: &Endpoint, query: &str) -> Result<(String, Value), String> {
    let listed = conn.blocking_call("GET", "/api/fanouts", None)?;
    if listed.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Err("fan-outs are off on this hub".into());
    }
    let fanouts = list(&listed, "fanouts").to_vec();
    let found = resolve(&fanouts, query, "fan-out")?;
    Ok((str_field(found, "id").to_owned(), found.clone()))
}

/// A candidate of `view` by label or session name.
fn candidate(view: &Value, query: &str) -> Result<String, String> {
    let candidates = list(view, "candidates");
    let q = query.trim();
    candidates
        .iter()
        .find(|c| str_field(c, "label") == q)
        .or_else(|| candidates.iter().find(|c| str_field(c, "session") == q))
        .map(|c| str_field(c, "label").to_owned())
        .ok_or_else(|| {
            format!(
                "fan-out {:?} has no candidate {q:?}; its candidates: {}",
                str_field(view, "name"),
                candidates
                    .iter()
                    .map(|c| str_field(c, "label"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn agent_label(c: &Value) -> String {
    match c.get("model").and_then(Value::as_str) {
        Some(model) => format!("{}:{model}", str_field(c, "kind")),
        None => str_field(c, "kind").to_owned(),
    }
}

/// `--agent claude:opus` -> `{"kind","model"?}` plus the kind's options.
fn agent_spec(raw: &str, args: &NewArgs) -> Result<Value, String> {
    let (kind, model) = match raw.split_once(':') {
        Some((kind, model)) => (kind.trim(), Some(model.trim()).filter(|m| !m.is_empty())),
        None => (raw.trim(), None),
    };
    let mut spec = match kind {
        "claude" => json!({ "kind": "claude", "permission_mode": args.permission_mode }),
        "codex" => json!({ "kind": "codex", "approval": args.approval, "sandbox": args.sandbox }),
        other => return Err(format!("--agent {other:?}: claude or codex (KIND[:MODEL])")),
    };
    if let Some(model) = model {
        spec["model"] = json!(model);
    }
    Ok(spec)
}

/// `--grader 'unit=cargo test'` -> `{"name","run"}`; anything else (an
/// uppercase `VAR=1 cmd` included) is the command as is.
fn grader_spec(raw: &str) -> Value {
    if let Some((name, run)) = raw.split_once('=')
        && !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !run.trim().is_empty()
    {
        return json!({ "name": name, "run": run });
    }
    json!({ "run": raw })
}

fn new(args: &NewArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(ACTION_TIMEOUT);
    let prompt = match args.prompt.as_deref() {
        Some("-") => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| format!("cannot read the prompt from stdin: {e}"))?;
            text
        }
        Some(prompt) => prompt.to_owned(),
        None => String::new(),
    };
    let workspaces = conn.blocking_workspaces()?;
    let workspace = resolve(&workspaces, &args.workspace, "workspace")?;
    let folder = args
        .folder
        .as_deref()
        .map(|query| pick_folder(workspace, query))
        .transpose()?;
    let candidates = args
        .agents
        .iter()
        .map(|a| agent_spec(a, args))
        .collect::<Result<Vec<_>, _>>()?;
    let body = json!({
        "workspace_id": str_field(workspace, "id"),
        "folder": folder,
        "base": args.base,
        "name": args.name,
        "prompt": prompt,
        "candidates": candidates,
        "graders": args.graders.iter().map(|g| grader_spec(g)).collect::<Vec<_>>(),
        "timeout_secs": args.timeout,
        "protect": args.protect,
        "commit_instruction": !args.no_commit_instruction,
        "from_task": args.from_task,
    });
    if args.permission_mode.as_deref() == Some("bypassPermissions") {
        let _ = writeln!(
            err,
            "fw hub: warning: the claude agents run with every permission check off"
        );
    }
    let view = conn.blocking_call("POST", "/api/fanouts", Some(&body))?;
    if args.json {
        print_json(out, &view)?;
    } else {
        writeln!(out, "{}", str_field(&view, "id")).map_err(|e| e.to_string())?;
    }
    Ok(0)
}

fn ls(args: &ListArgs, out: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let listed = conn.blocking_call("GET", "/api/fanouts", None)?;
    if args.json {
        print_json(out, &listed)?;
        return Ok(0);
    }
    let io = |e: std::io::Error| e.to_string();
    if listed.get("enabled").and_then(Value::as_bool) == Some(false) {
        writeln!(out, "fan-outs are off on this hub").map_err(io)?;
        return Ok(0);
    }
    let fanouts = list(&listed, "fanouts");
    if fanouts.is_empty() {
        writeln!(out, "no fan-outs").map_err(io)?;
    }
    for f in fanouts {
        let counts = f.get("counts").cloned().unwrap_or(Value::Null);
        let n = |k: &str| counts.get(k).and_then(Value::as_u64).unwrap_or(0);
        let state = match f.get("winner").filter(|w| !w.is_null()) {
            Some(w) => format!("merged {}", str_field(w, "label")),
            None if f.get("settled").and_then(Value::as_bool) == Some(true) => {
                match f.get("leader").and_then(Value::as_str) {
                    Some(leader) => format!("settled, {leader} leads"),
                    None => "settled, none pass".to_owned(),
                }
            }
            None => format!("{} running, {} grading", n("running"), n("grading")),
        };
        writeln!(
            out,
            "{}  {:<32}  {}/{} pass  {}  ({} on {})",
            &str_field(f, "id")[..8.min(str_field(f, "id").len())],
            str_field(f, "name"),
            n("passing"),
            n("candidates"),
            state,
            f.get("workspace").and_then(Value::as_str).unwrap_or("?"),
            str_field(f, "base")
        )
        .map_err(io)?;
    }
    Ok(0)
}

fn render(view: &Value, verbose: bool, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "fan-out {:?} ({})  {} @ {}  in {}",
        str_field(view, "name"),
        str_field(view, "id"),
        str_field(view, "base"),
        short(str_field(view, "base_sha")),
        // `repo` from a hub older than `folder`.
        Some(str_field(view, "folder"))
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| str_field(view, "repo")),
    )?;
    let graders: Vec<String> = list(view, "graders")
        .iter()
        .map(|g| {
            let label = match g.get("name").and_then(Value::as_str) {
                Some(name) => format!("{name} ({})", str_field(g, "run")),
                None => str_field(g, "run").to_owned(),
            };
            if g.get("held_out").and_then(Value::as_bool) == Some(true) {
                format!("{label} [held out]")
            } else {
                label
            }
        })
        .collect();
    writeln!(
        out,
        "graders: {}  (timeout {}s each; runner: {})",
        graders.join("; "),
        view.get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        view.get("runner").and_then(Value::as_str).unwrap_or("none")
    )?;
    let protect = list(view, "protect");
    if !protect.is_empty() {
        writeln!(
            out,
            "protected: {}",
            protect
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    if let Some(o) = view.get("origin").filter(|o| !o.is_null()) {
        writeln!(
            out,
            "from flywheel task {} ({})",
            str_field(o, "task_id"),
            str_field(o, "folder")
        )?;
    }
    for p in list(view, "promoted") {
        let at = match str_field(p, "state") {
            "archived" => format!("archived (done): {}", str_field(p, "current")),
            "gone" => format!("{} (no longer there)", str_field(p, "path")),
            _ => str_field(p, "path").to_owned(),
        };
        writeln!(
            out,
            "promoted to flywheel task {}: {at}",
            str_field(p, "task_id")
        )?;
    }
    if let Some(w) = view.get("winner").filter(|w| !w.is_null()) {
        writeln!(
            out,
            "winner: {} merged into {} at {} ({})",
            str_field(w, "label"),
            str_field(w, "base"),
            short(str_field(w, "head")),
            str_field(w, "at")
        )?;
    }
    writeln!(out)?;
    for c in list(view, "candidates") {
        let grade = c.get("grade").filter(|g| !g.is_null());
        let diff = grade
            .and_then(|g| g.get("diff"))
            .map(|d| {
                format!(
                    "+{} -{} in {}",
                    d.get("insertions").and_then(Value::as_u64).unwrap_or(0),
                    d.get("deletions").and_then(Value::as_u64).unwrap_or(0),
                    d.get("files").and_then(Value::as_u64).unwrap_or(0)
                )
            })
            .unwrap_or_else(|| "-".to_owned());
        let cost = match c.get("cost_usd").and_then(Value::as_f64) {
            Some(usd) => crate::script::dollars(usd),
            None if c.get("tokens").and_then(Value::as_u64).unwrap_or(0) > 0 => {
                "unpriced".to_owned()
            }
            None => "-".to_owned(),
        };
        let tokens = c.get("tokens").and_then(Value::as_u64).unwrap_or(0);
        let mut marks = String::new();
        if c.get("winner").and_then(Value::as_bool) == Some(true) {
            marks.push_str("  WINNER");
        }
        if c.get("cleaned").is_some_and(|v| !v.is_null()) {
            marks.push_str("  (worktree removed)");
        }
        writeln!(
            out,
            "#{} {:<3} {:<22} {:<7} {:<8} {:<16} {:>8} {:>7}  {}{marks}",
            c.get("rank").and_then(Value::as_u64).unwrap_or(0),
            str_field(c, "label"),
            agent_label(c),
            str_field(c, "verdict"),
            str_field(c, "phase"),
            diff,
            cost,
            if tokens > 0 {
                tokens_label(tokens)
            } else {
                String::new()
            },
            c.get("session")
                .and_then(Value::as_str)
                .unwrap_or("(removed)"),
        )?;
        for why in list(c, "why") {
            writeln!(out, "      {}", why.as_str().unwrap_or_default())?;
        }
        if let Some(grade) = grade {
            for g in list(grade, "graders") {
                let status = str_field(g, "status");
                writeln!(
                    out,
                    "      {} {}: {status}{} ({} ms)",
                    if status == "passed" { "ok " } else { "!! " },
                    g.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(str_field(g, "run")),
                    g.get("exit_code")
                        .and_then(Value::as_i64)
                        .map(|c| format!(", exit {c}"))
                        .unwrap_or_default(),
                    g.get("duration_ms").and_then(Value::as_u64).unwrap_or(0)
                )?;
                if verbose && status != "passed" && status != "skipped" {
                    for stream in ["stdout", "stderr"] {
                        let text = str_field(g, stream).trim_end();
                        if !text.is_empty() {
                            writeln!(out, "        --- {stream} ---")?;
                            for line in text.lines().rev().take(40).collect::<Vec<_>>().iter().rev()
                            {
                                writeln!(out, "        {line}")?;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn show(args: &ShowArgs, out: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, _) = find(&conn, &args.fanout)?;
    let view = conn.blocking_call("GET", &format!("/api/fanouts/{id}"), None)?;
    if args.json {
        print_json(out, &view)?;
    } else {
        render(&view, args.verbose, out).map_err(|e| e.to_string())?;
    }
    Ok(0)
}

/// What the wait's event loop came to.
enum WaitEnd {
    Settled(Value),
    TimedOut,
    Failed(String),
}

async fn wait_settled(
    conn: &Endpoint,
    id: &str,
    deadline: Option<tokio::time::Instant>,
) -> WaitEnd {
    let path = format!("/api/events?client={}", client_id("fanout-wait"));
    let mut reconnect_until: Option<tokio::time::Instant> = None;
    loop {
        // Subscribe first, then read: no change falls between the two.
        let attempt = async {
            let socket = conn.websocket(&path).await?;
            let view = conn
                .request("GET", &format!("/api/fanouts/{id}"), None)
                .await?;
            Ok::<_, String>((socket, view))
        };
        let attempt = match deadline {
            Some(at) => match tokio::time::timeout_at(at, attempt).await {
                Ok(result) => result,
                Err(_) => return WaitEnd::TimedOut,
            },
            None => attempt.await,
        };
        let (mut socket, view) = match attempt {
            Ok(found) => found,
            Err(e) => match reconnect_until {
                Some(until) if tokio::time::Instant::now() < until => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                _ => return WaitEnd::Failed(e),
            },
        };
        if view.get("settled").and_then(Value::as_bool) == Some(true) {
            return WaitEnd::Settled(view);
        }
        loop {
            let next = match deadline {
                Some(at) => match tokio::time::timeout_at(at, socket.next()).await {
                    Ok(next) => next,
                    Err(_) => return WaitEnd::TimedOut,
                },
                None => socket.next().await,
            };
            match next {
                Some(Ok(Message::Text(text))) => {
                    let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    // A `snapshot` after the first follows missed events:
                    // read it again then too.
                    let relevant = match event.get("type").and_then(Value::as_str) {
                        Some("fanout") => str_field(&event, "id") == id,
                        Some("snapshot") => true,
                        _ => false,
                    };
                    if !relevant {
                        continue;
                    }
                    match conn
                        .request("GET", &format!("/api/fanouts/{id}"), None)
                        .await
                    {
                        Ok(view) if view.get("settled").and_then(Value::as_bool) == Some(true) => {
                            return WaitEnd::Settled(view);
                        }
                        Ok(_) => {}
                        Err(e) => return WaitEnd::Failed(e),
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let restart = frame
                        .as_ref()
                        .is_some_and(|f| u16::from(f.code) == crate::server::WS_CLOSE_RESTART);
                    if !restart {
                        return WaitEnd::Failed("the hub closed the events connection".into());
                    }
                    reconnect_until = Some(tokio::time::Instant::now() + RECONNECT_WINDOW);
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return WaitEnd::Failed(format!("events connection failed: {e}")),
                None => {
                    reconnect_until = Some(tokio::time::Instant::now() + RECONNECT_WINDOW);
                    break;
                }
            }
        }
    }
}

fn wait(args: &WaitArgs, out: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, _) = find(&conn, &args.fanout)?;
    // On the endpoint's runtime: `find` left a pooled connection bound to it.
    let end = conn.block_on(async {
        let deadline = args
            .timeout
            .map(|secs| tokio::time::Instant::now() + Duration::from_secs(secs));
        wait_settled(&conn, &id, deadline).await
    })?;
    match end {
        WaitEnd::Settled(view) => {
            if args.json {
                print_json(out, &view)?;
            } else {
                render(&view, false, out).map_err(|e| e.to_string())?;
            }
            Ok(0)
        }
        WaitEnd::TimedOut => Ok(TIMED_OUT),
        WaitEnd::Failed(message) => Err(message),
    }
}

fn grade(args: &CandidateArgs, out: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, view) = find(&conn, &args.fanout)?;
    let label = candidate(&view, &args.candidate)?;
    conn.blocking_call(
        "POST",
        &format!("/api/fanouts/{id}/grade"),
        Some(&json!({ "candidate": label })),
    )?;
    writeln!(out, "{label}: queued for grading").map_err(|e| e.to_string())?;
    Ok(0)
}

fn fix(args: &ConfirmArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, view) = find(&conn, &args.fanout)?;
    let label = candidate(&view, &args.candidate)?;
    let url = format!("/api/fanouts/{id}/fix");
    let plan = conn.blocking_call(
        "POST",
        &url,
        Some(&json!({ "candidate": label, "dry_run": true })),
    )?;
    let io = |e: std::io::Error| e.to_string();
    if !args.json {
        writeln!(
            out,
            "pastes into {} ({label}):\n",
            str_field(&plan, "session")
        )
        .map_err(io)?;
        writeln!(out, "{}", str_field(&plan, "prompt")).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Paste it?", err) {
        if args.json {
            print_json(out, &plan)?;
            return Ok(0);
        }
        let _ = writeln!(
            err,
            "fw hub: not pasted (pass --yes to run without a prompt)"
        );
        return Ok(1);
    }
    let done = conn.blocking_call("POST", &url, Some(&json!({ "candidate": label })))?;
    if args.json {
        print_json(out, &done)?;
    } else {
        writeln!(
            out,
            "\npasted; {label} is graded again when its agent finishes"
        )
        .map_err(io)?;
    }
    Ok(0)
}

fn render_plan(plan: &Value, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "{}", str_field(plan, "summary"))?;
    for d in list(plan, "details") {
        writeln!(out, "  {}", d.as_str().unwrap_or_default())?;
    }
    for w in list(plan, "warnings") {
        writeln!(out, "  warning: {}", w.as_str().unwrap_or_default())?;
    }
    for c in list(plan, "commands") {
        writeln!(out, "  $ {}", str_field(c, "display"))?;
    }
    Ok(())
}

fn render_cleanup(items: &[Value], out: &mut dyn Write) -> std::io::Result<()> {
    for item in items {
        let state = if item.get("done").and_then(Value::as_bool) == Some(true) {
            match str_field(item, "action") {
                "stop" => "stopped".to_owned(),
                _ => match item.get("branch_deleted").and_then(Value::as_bool) {
                    Some(true) => "removed (branch deleted)".to_owned(),
                    Some(false) => "removed (branch kept)".to_owned(),
                    None => "removed".to_owned(),
                },
            }
        } else if let Some(error) = item.get("error").and_then(Value::as_str) {
            format!("failed: {error}")
        } else if let Some(why) = item.get("refusal").and_then(Value::as_str) {
            format!("needs --force: {why}")
        } else {
            "can be removed".to_owned()
        };
        writeln!(
            out,
            "  {} {}  {}",
            str_field(item, "candidate"),
            str_field(item, "path"),
            state
        )?;
    }
    Ok(())
}

fn pick(args: &PickArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(ACTION_TIMEOUT);
    let (id, view) = find(&conn, &args.fanout)?;
    let label = candidate(&view, &args.candidate)?;
    let io = |e: std::io::Error| e.to_string();
    let url = format!("/api/fanouts/{id}/pick");
    let dry = json!({ "candidate": label, "dry_run": true });
    let plan = conn.blocking_call("POST", &url, Some(&dry))?;
    let needs_rebase = plan.get("needs_rebase").and_then(Value::as_bool) == Some(true);
    if needs_rebase && args.rebase {
        let rebase_url = format!("/api/fanouts/{id}/rebase");
        let rebase = conn.blocking_call("POST", &rebase_url, Some(&dry))?;
        let rplan = rebase.get("plan").cloned().unwrap_or(Value::Null);
        if let Some(why) = rplan.get("refusal").and_then(Value::as_str) {
            return Err(format!("cannot rebase {label}: {why}"));
        }
        if !args.json {
            render_plan(&rplan, out).map_err(io)?;
        }
        if !approved(args.yes, args.json, "Rebase?", err) {
            let _ = writeln!(
                err,
                "fw hub: not rebased (pass --yes to run without a prompt)"
            );
            return Ok(1);
        }
        let done = conn.blocking_call(
            "POST",
            &rebase_url,
            Some(&json!({ "candidate": label, "confirm": str_field(&rplan, "fingerprint") })),
        )?;
        let ok = done
            .get("rebase")
            .and_then(|r| r.get("ok"))
            .and_then(Value::as_bool)
            == Some(true);
        if args.json {
            print_json(out, &done)?;
        } else {
            writeln!(
                out,
                "{}",
                str_field(done.get("rebase").unwrap_or(&Value::Null), "summary")
            )
            .map_err(io)?;
            if ok {
                writeln!(
                    out,
                    "{label} is graded again; pick it once it passes (fw hub fanout wait {})",
                    &id[..8]
                )
                .map_err(io)?;
            }
        }
        return Ok(if ok { 0 } else { 1 });
    }
    if let Some(why) = plan.get("refusal").and_then(Value::as_str) {
        if args.json {
            print_json(out, &plan)?;
        }
        let hint = if needs_rebase {
            " (--rebase rebases it first; it is then graded again)"
        } else {
            ""
        };
        return Err(format!("cannot pick {label}: {why}{hint}"));
    }
    let merge = plan.get("merge").cloned().unwrap_or(Value::Null);
    if !args.json {
        render_plan(&merge, out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Merge?", err) {
        if args.json {
            print_json(out, &plan)?;
            return Ok(0);
        }
        let _ = writeln!(
            err,
            "fw hub: not merged (pass --yes to run without a prompt)"
        );
        return Ok(1);
    }
    let done = conn.blocking_call(
        "POST",
        &url,
        Some(&json!({ "candidate": label, "confirm": str_field(&merge, "fingerprint") })),
    )?;
    if args.json {
        print_json(out, &done)?;
        return Ok(0);
    }
    let merged = done.get("merge").cloned().unwrap_or(Value::Null);
    writeln!(out, "{}", str_field(&merged, "summary")).map_err(io)?;
    if merged.get("ok").and_then(Value::as_bool) != Some(true) {
        return Ok(1);
    }
    let losers = list(&done, "losers");
    if !losers.is_empty() {
        writeln!(out, "\nthe others (fw hub fanout cleanup {}):", &id[..8]).map_err(io)?;
        render_cleanup(losers, out).map_err(io)?;
    }
    Ok(0)
}

fn render_task_plan(answer: &Value, out: &mut dyn Write) -> std::io::Result<()> {
    let plan = answer.get("plan").unwrap_or(&Value::Null);
    writeln!(
        out,
        "task {} in phase {}{}",
        str_field(plan, "task_id"),
        str_field(plan, "phase"),
        if plan.get("new_phase").and_then(Value::as_bool) == Some(true) {
            " (new)"
        } else {
            ""
        }
    )?;
    writeln!(out, "writes {}:", str_field(plan, "path"))?;
    for line in str_field(plan, "contents").lines() {
        writeln!(out, "  | {line}")?;
    }
    for note in list(plan, "notes") {
        writeln!(out, "{}", note.as_str().unwrap_or_default())?;
    }
    for warning in list(answer, "warnings") {
        writeln!(out, "warning: {}", warning.as_str().unwrap_or_default())?;
    }
    Ok(())
}

fn promote(args: &PromoteArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(ACTION_TIMEOUT);
    let (id, _) = find(&conn, &args.fanout)?;
    let io = |e: std::io::Error| e.to_string();
    let url = format!("/api/fanouts/{id}/promote");
    let mut body = json!({
        "task_id": args.task_id,
        "phase": args.phase,
        "goal": args.goal,
        "reference": args.reference,
        "dry_run": true,
    });
    let plan = conn.blocking_call("POST", &url, Some(&body))?;
    if !args.json {
        render_task_plan(&plan, out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Write it?", err) {
        if args.json {
            print_json(out, &plan)?;
            return Ok(0);
        }
        let _ = writeln!(
            err,
            "fw hub: not written (pass --yes to write without a prompt)"
        );
        return Ok(1);
    }
    body["dry_run"] = json!(false);
    body["confirm"] = json!(str_field(&plan, "fingerprint"));
    let done = conn.blocking_call("POST", &url, Some(&body))?;
    if args.json {
        print_json(out, &done)?;
        return Ok(0);
    }
    let written = done.get("written").unwrap_or(&Value::Null);
    writeln!(
        out,
        "wrote {} (uncommitted){}",
        str_field(written, "path"),
        if written.get("listed").and_then(Value::as_bool) == Some(true) {
            "; the directory source lists it: run `fw worker` there to verify it"
        } else {
            ""
        }
    )
    .map_err(io)?;
    let queue: Vec<&str> = list(written, "queue")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if !queue.is_empty() {
        writeln!(
            out,
            "note: `fw worker` drains every active task, not only this one; {} other{} not done \
             yet may run first: {}",
            queue.len(),
            if queue.len() == 1 { "" } else { "s" },
            queue.join(", ")
        )
        .map_err(io)?;
    }
    Ok(0)
}

fn cleanup(args: &CleanupArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(ACTION_TIMEOUT);
    let (id, view) = find(&conn, &args.fanout)?;
    let labels = args
        .candidates
        .iter()
        .map(|c| candidate(&view, c))
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = json!({
        "remove_worktrees": !args.stop_only,
        "force": args.force,
        "dry_run": true,
    });
    if !labels.is_empty() {
        body["candidates"] = json!(labels);
    }
    let url = format!("/api/fanouts/{id}/cleanup");
    let plan = conn.blocking_call("POST", &url, Some(&body))?;
    let io = |e: std::io::Error| e.to_string();
    let items = list(&plan, "items");
    if items.is_empty() {
        writeln!(out, "nothing to clean up").map_err(io)?;
        return Ok(0);
    }
    if !args.json {
        writeln!(
            out,
            "{}:",
            if args.stop_only {
                "stops these sessions"
            } else {
                "removes these worktrees and their sessions"
            }
        )
        .map_err(io)?;
        render_cleanup(items, out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Go ahead?", err) {
        if args.json {
            print_json(out, &plan)?;
            return Ok(0);
        }
        let _ = writeln!(
            err,
            "fw hub: nothing done (pass --yes to run without a prompt)"
        );
        return Ok(1);
    }
    body["dry_run"] = json!(false);
    let done = conn.blocking_call("POST", &url, Some(&body))?;
    let (carried, kept): (Vec<Value>, Vec<Value>) =
        list(&done, "items").iter().cloned().partition(|i| {
            i.get("done").and_then(Value::as_bool) == Some(true) || i.get("error").is_some()
        });
    if args.json {
        print_json(out, &done)?;
    } else if !carried.is_empty() {
        // The refused ones were listed above, reasons and all.
        writeln!(out).map_err(io)?;
        render_cleanup(&carried, out).map_err(io)?;
    }
    let failed = carried
        .iter()
        .any(|i| i.get("done").and_then(Value::as_bool) != Some(true));
    if !kept.is_empty() && !args.json {
        let _ = writeln!(
            err,
            "fw hub: kept {} (work nothing else has, listed above); --force removes them anyway",
            kept.iter()
                .map(|i| str_field(i, "candidate"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(if failed || !kept.is_empty() { 1 } else { 0 })
}

fn rm(args: &FanoutRef, out: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, view) = find(&conn, &args.fanout)?;
    conn.blocking_call("DELETE", &format!("/api/fanouts/{id}"), None)?;
    writeln!(
        out,
        "forgot fan-out {:?} ({id}); its sessions and worktrees stay",
        str_field(&view, "name")
    )
    .map_err(|e| e.to_string())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graders_take_an_optional_name() {
        assert_eq!(
            grader_spec("unit=cargo test -p x"),
            json!({"name": "unit", "run": "cargo test -p x"})
        );
        // An environment assignment stays part of the command.
        assert_eq!(
            grader_spec("RUST_LOG=1 make test"),
            json!({"run": "RUST_LOG=1 make test"})
        );
        assert_eq!(
            grader_spec("test \"$(cat x)\" = 42"),
            json!({"run": "test \"$(cat x)\" = 42"})
        );
        assert_eq!(grader_spec("=x"), json!({"run": "=x"}));
    }
}
