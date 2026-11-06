//! `fw hub autopilot FOLDER status|start|stop|pause|resume|approve|deny`:
//! a folder's autopilot through the running hub (see [`crate::autopilot`]
//! and `server::autopilot_api`). Start and gate decisions are confirmed
//! (`--yes` skips the question; never asked under `--json` or without a
//! terminal, where they need `--yes`).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::util::{approved, fail, list, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct AutopilotArgs {
    /// The adopted folder (a workspace folder with flywheel.toml or
    /// .flywheel/); a relative path is resolved here for this machine's
    /// hub.
    folder: String,
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// The daemon's state, what its last cycle decided and why, the tasks
    /// it emitted (score, latest run), and the gates its work waits on.
    Status(Plain),
    /// Run `fw autopilot` (the neverending daemon) in a new hub shell
    /// session there. Its work lands on the base branch unattended.
    Start(StartArgs),
    /// Stop this hub's autopilot session there (Ctrl-C: it stops
    /// gracefully; the session stays a shell).
    Stop(Plain),
    /// Hold the daemon between cycles (`fw autopilot --pause`).
    Pause(Plain),
    /// Let a paused daemon run cycles again (`fw autopilot --resume`).
    Resume(Plain),
    /// Approve the manual gate RUN is parked on (`fw approve`), pinned to
    /// the gate shown (or --gate / --attempt).
    Approve(DecideArgs),
    /// Deny it (`fw reject`): the run goes to failed_validation and
    /// --feedback flows into its next attempt.
    Deny(DenyArgs),
}

#[derive(Debug, clap::Args)]
struct Plain {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct StartArgs {
    /// The workspace whose session it is (default: the first holding the
    /// folder).
    #[arg(long, value_name = "ID")]
    workspace: Option<String>,
    /// Do not ask.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct DecideArgs {
    /// The parked run's id.
    run: String,
    /// The gate (awaiting ordinal) decided; default: the one parked now.
    #[arg(long, value_name = "ORDINAL")]
    gate: Option<u32>,
    /// The attempt decided; default: the one parked now.
    #[arg(long, value_name = "N")]
    attempt: Option<u32>,
    /// Do not ask.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct DenyArgs {
    #[command(flatten)]
    decide: DecideArgs,
    /// Reviewer feedback for the next attempt.
    #[arg(long, value_name = "TEXT")]
    feedback: Option<String>,
}

pub(crate) fn main(args: &AutopilotArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn endpoint_of(action: &Action) -> &EndpointArgs {
    match action {
        Action::Status(a) | Action::Stop(a) | Action::Pause(a) | Action::Resume(a) => &a.endpoint,
        Action::Start(a) => &a.endpoint,
        Action::Approve(a) => &a.endpoint,
        Action::Deny(a) => &a.decide.endpoint,
    }
}

/// The folder as the hub names it: for this machine's hub, absolute and
/// with symlinks resolved (as workspaces store folders); as given for a
/// hub elsewhere.
fn folder_for(raw: &str, endpoint: &EndpointArgs) -> String {
    if endpoint.url.is_some() || endpoint.machine.is_some() {
        return raw.to_owned();
    }
    let path = std::path::Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

fn query(folder: &str) -> String {
    url::form_urlencoded::byte_serialize(folder.as_bytes()).collect()
}

fn run(args: &AutopilotArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let endpoint = endpoint_of(&args.action);
    let folder = folder_for(&args.folder, endpoint);
    let conn = Endpoint::resolve(endpoint)?;
    let view = || -> Result<Value, String> {
        conn.blocking_call(
            "GET",
            &format!("/api/flywheel/autopilot?folder={}", query(&folder)),
            None,
        )
    };
    match &args.action {
        Action::Status(a) => {
            let answer = view()?;
            if a.json {
                print_json(out, &answer);
            } else {
                print_status(out, &answer);
            }
            Ok(0)
        }
        Action::Start(a) => {
            let question = format!(
                "Start autopilot in {folder}? It authors tasks and its work lands on the base \
                 branch unattended."
            );
            if !approved(a.yes, a.json, &question, err) {
                return Err("not started (confirm, or pass --yes)".into());
            }
            let mut body = json!({ "folder": folder, "confirm": true });
            if let Some(workspace) = &a.workspace {
                body["workspace_id"] = json!(workspace);
            }
            let session =
                conn.blocking_call("POST", "/api/flywheel/autopilot/start", Some(&body))?;
            if a.json {
                print_json(out, &session);
            } else {
                let _ = writeln!(
                    out,
                    "started {} ({}): fw autopilot in {folder}",
                    str_field(&session, "name"),
                    str_field(&session, "id")
                );
            }
            Ok(0)
        }
        Action::Stop(a) => {
            let answer = conn.blocking_call(
                "POST",
                "/api/flywheel/autopilot/stop",
                Some(&json!({ "folder": folder })),
            )?;
            if a.json {
                print_json(out, &answer);
            } else {
                for session in list(&answer, "stopping") {
                    let _ = writeln!(
                        out,
                        "stopping {} ({}): Ctrl-C sent, it stops gracefully",
                        str_field(session, "name"),
                        str_field(session, "id")
                    );
                }
            }
            Ok(0)
        }
        Action::Pause(a) | Action::Resume(a) => {
            let pause = matches!(args.action, Action::Pause(_));
            let verb = if pause { "pause" } else { "resume" };
            let answer = conn.blocking_call(
                "POST",
                &format!("/api/flywheel/autopilot/{verb}"),
                Some(&json!({ "folder": folder })),
            )?;
            if a.json {
                print_json(out, &answer);
            } else {
                let done = if pause { "paused" } else { "resumed" };
                let _ = writeln!(
                    out,
                    "autopilot {}{done} in {folder}",
                    if answer["changed"] == true {
                        ""
                    } else {
                        "already "
                    }
                );
            }
            Ok(0)
        }
        Action::Approve(a) => decide(&conn, &view()?, &folder, a, None, true, out, err),
        Action::Deny(a) => decide(
            &conn,
            &view()?,
            &folder,
            &a.decide,
            a.feedback.as_deref(),
            false,
            out,
            err,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn decide(
    conn: &Endpoint,
    answer: &Value,
    folder: &str,
    args: &DecideArgs,
    feedback: Option<&str>,
    approve: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let shown = list(&answer["view"], "gates")
        .iter()
        .find(|g| str_field(g, "run_id") == args.run);
    // Pin the gate shown (as the console does): a decision never lands on
    // a later gate nobody looked at.
    let gate = args
        .gate
        .or_else(|| shown.and_then(|g| g["gate"].as_u64()).map(|g| g as u32));
    let attempt = args
        .attempt
        .or_else(|| shown.and_then(|g| g["attempt"].as_u64()).map(|a| a as u32));
    let what = match shown {
        Some(g) => format!(
            "gate {} of run {} ({}): {}",
            g["gate"],
            args.run,
            str_field(g, "task_id"),
            str_field(g, "instruction")
        ),
        None => format!("the gate run {} is parked on", args.run),
    };
    let verb = if approve { "Approve" } else { "Deny" };
    if !approved(args.yes, args.json, &format!("{verb} {what}?"), err) {
        return Err("nothing decided (confirm, or pass --yes)".into());
    }
    let mut body = json!({
        "folder": folder,
        "run": args.run,
        "decision": if approve { "approve" } else { "deny" },
        "confirm": true,
    });
    if let Some(gate) = gate {
        body["gate"] = json!(gate);
    }
    if let Some(attempt) = attempt {
        body["attempt"] = json!(attempt);
    }
    if let Some(feedback) = feedback {
        body["feedback"] = json!(feedback);
    }
    let decided = conn.blocking_call("POST", "/api/flywheel/autopilot/decide", Some(&body))?;
    if args.json {
        print_json(out, &decided);
    } else {
        let _ = writeln!(
            out,
            "enqueued #{} {} run_id={} (gate {}, attempt {}); the worker applies it",
            decided["command_id"],
            str_field(&decided, "kind"),
            str_field(&decided, "run_id"),
            decided["gate"],
            decided["attempt"]
        );
    }
    Ok(0)
}

fn tiers(value: &Value) -> String {
    let shown: Vec<String> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .map(|t| t.to_string())
        .collect();
    if shown.is_empty() {
        "none".into()
    } else {
        shown.join(",")
    }
}

fn print_status(out: &mut dyn Write, answer: &Value) {
    let view = &answer["view"];
    let mut head = format!("autopilot: {}", str_field(view, "state"));
    if let Some(pid) = view["pid"].as_i64() {
        head.push_str(&format!(" (pid {pid}, cycle {})", view["cycle"]));
    }
    if view["paused"] == true && str_field(view, "state") != "paused" {
        head.push_str(", pause marker set");
    }
    for session in list(answer, "sessions") {
        head.push_str(&format!("; hub session {}", str_field(session, "name")));
    }
    let _ = writeln!(out, "{head}");
    let config = &view["config"];
    let mut gates = vec![format!("submit {}", str_field(config, "submit_strategy"))];
    if config["submit_verify"] == true {
        gates.push("submit verify".into());
    }
    if config["held_out"] == true {
        gates.push("held-out gate".into());
    }
    if config["team_mode"] == true {
        gates.push("team mode (GitHub reviews)".into());
    }
    let _ = writeln!(
        out,
        "config: target depth {}, landing {}, every {}s; gates: {}",
        config["target_depth"],
        str_field(config, "landing"),
        config["interval_seconds"],
        gates.join(", ")
    );
    if let Some(problem) = view["problem"].as_str() {
        let _ = writeln!(out, "problem: {problem}");
    }
    // The newest cycle that decided anything, in full; then the last cycle
    // when it is a later one (often the queue held at its target).
    let decided = &view["last_decided"];
    let last = &view["decision"];
    let print_decision = |out: &mut dyn Write, label: &str, d: &Value| {
        let _ = writeln!(
            out,
            "{label} {}: emitted {}, dropped {} -- {}",
            d["cycle"],
            d["emitted"],
            d["dropped"],
            str_field(d, "reason")
        );
    };
    if decided.is_object() {
        print_decision(out, "decided in cycle", decided);
        let _ = writeln!(
            out,
            "  relevant tiers {}; prefiltered {}; discovery cache {}",
            tiers(&decided["relevant_tiers"]),
            tiers(&decided["prefiltered_tiers"]),
            str_field(decided, "discovery_cache")
        );
        for drop in list(decided, "dropped_findings") {
            let _ = writeln!(
                out,
                "  dropped {} (tier {}): {}",
                str_field(drop, "finding_id"),
                drop["tier"],
                str_field(drop, "reason")
            );
        }
    }
    if last.is_object() && (!decided.is_object() || last["cycle"] != decided["cycle"]) {
        print_decision(out, "last cycle", last);
    }
    if !decided.is_object() && !last.is_object() {
        let _ = writeln!(out, "no cycle recorded yet");
    }
    let gates = list(view, "gates");
    if !gates.is_empty() {
        let _ = writeln!(out, "awaiting approval:");
        for gate in gates {
            let _ = writeln!(
                out,
                "  {} {} gate {} attempt {}{}: {}",
                str_field(gate, "run_id"),
                str_field(gate, "task_id"),
                gate["gate"],
                gate["attempt"],
                if gate["autopilot"] == true {
                    " (autopilot)"
                } else {
                    ""
                },
                str_field(gate, "instruction")
            );
            if gate["pending"].is_object() {
                let _ = writeln!(
                    out,
                    "    {} queued (command #{}): the next worker run applies it",
                    str_field(&gate["pending"], "kind"),
                    gate["pending"]["command_id"]
                );
            }
        }
    }
    for hold in list(view, "holds") {
        let _ = writeln!(
            out,
            "held: {} {}: {}",
            str_field(hold, "task_id"),
            str_field(hold, "kind"),
            str_field(hold, "detail")
        );
    }
    let candidates = list(view, "candidates");
    if !candidates.is_empty() {
        let _ = writeln!(out, "emitted tasks (newest first):");
        for c in candidates {
            let _ = writeln!(
                out,
                "  {:<10} tier {:>2} score {:>8.1}  {}{}",
                c["state"].as_str().unwrap_or("archived"),
                c["tier"].as_u64().unwrap_or(0),
                c["score"].as_f64().unwrap_or(0.0),
                str_field(c, "task_id"),
                c["run_id"]
                    .as_str()
                    .map(|r| format!("  (run {r} {})", str_field(c, "run_status")))
                    .unwrap_or_default()
            );
        }
    }
    // The newest decisions here, from the hub's activity journal.
    let history = list(view, "history");
    if !history.is_empty() {
        let _ = writeln!(out, "decisions (newest first):");
        for row in history {
            let _ = writeln!(
                out,
                "  {}  {}",
                str_field(row, "at")
                    .get(..16)
                    .unwrap_or(str_field(row, "at")),
                str_field(row, "text")
            );
        }
    }
}
