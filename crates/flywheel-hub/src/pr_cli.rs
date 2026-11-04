//! `fw hub pr SESSION` (a session's pull request, review decision,
//! mergeability, and CI checks), `fw hub pr log|fix-ci|rerun|ready|merge`,
//! over the running hub's pull request API (see [`crate::pr`]).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{query_escape, session_label};
use crate::util::{approved, fail, list, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

/// A check's name column is at most this wide.
const NAME_WIDTH: usize = 48;

/// gh can be slow (a merge, a long log).
const CALL_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct PrArgs {
    #[command(subcommand)]
    command: Option<PrCommand>,
    /// Session id, unique id prefix, or name.
    #[arg(required = true)]
    session: Option<String>,
    /// Ask GitHub now instead of answering from the hub's cache.
    #[arg(long)]
    refresh: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Subcommand)]
enum PrCommand {
    /// Print a failing check's failed-steps log (its last lines, secrets
    /// redacted).
    Log(LogArgs),
    /// Ask the session's agent to fix CI: the failing check and the tail
    /// of its failed log, pasted now when the agent is at rest, else
    /// queued for when its turn finishes.
    FixCi(FixArgs),
    /// Re-run the failed jobs of the failing checks' workflow runs.
    Rerun(ActionArgs),
    /// Mark a draft pull request ready for review.
    Ready(ActionArgs),
    /// Merge the pull request on GitHub with the repository's merge
    /// method, pinned to the head commit shown. Refused while checks fail
    /// or run or a review is missing, unless --force gives the pull
    /// request's number.
    Merge(MergeArgs),
}

#[derive(Debug, clap::Args)]
struct LogArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// The check's name (as `fw hub pr SESSION` lists it).
    check: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct FixArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// The failing check to send (default: the first failing one).
    #[arg(long, value_name = "NAME")]
    check: Option<String>,
    /// Send without asking (the prompt is still printed). Without it a
    /// terminal is asked; anything else only prints the prompt.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct ActionArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// Run without asking (the plan is still printed).
    #[arg(long, short = 'y')]
    yes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct MergeArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// Run without asking (the plan is still printed).
    #[arg(long, short = 'y')]
    yes: bool,
    /// Merge although checks fail or run or a review is missing: the pull
    /// request's number, typed back (GitHub's branch protection still
    /// applies).
    #[arg(long, value_name = "NUMBER")]
    force: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// Run `fw hub pr`.
pub fn main(args: &PrArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        None => show(args, out),
        Some(PrCommand::Log(a)) => log(a, out),
        Some(PrCommand::FixCi(a)) => fix(a, out, err),
        Some(PrCommand::Rerun(a)) => action(
            "rerun",
            &a.session,
            a.yes,
            a.json,
            None,
            &a.endpoint,
            out,
            err,
        ),
        Some(PrCommand::Ready(a)) => action(
            "ready",
            &a.session,
            a.yes,
            a.json,
            None,
            &a.endpoint,
            out,
            err,
        ),
        Some(PrCommand::Merge(a)) => action(
            "merge",
            &a.session,
            a.yes,
            a.json,
            a.force.as_deref(),
            &a.endpoint,
            out,
            err,
        ),
    };
    match result {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn connect(endpoint: &EndpointArgs, query: &str) -> Result<(Endpoint, String, String), String> {
    let conn = Endpoint::resolve(endpoint)?.with_timeout(CALL_TIMEOUT);
    let (id, session) = crate::script::find_session(&conn, query)?;
    Ok((conn, id, session_label(&session)))
}

/// `1m 05s`, `42s`.
fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn bucket_word(bucket: &str) -> &'static str {
    match bucket {
        "pass" => "PASS",
        "fail" => "FAIL",
        "cancel" => "CANCELLED",
        "skipping" => "SKIPPED",
        _ => "RUNNING",
    }
}

fn show(args: &PrArgs, out: &mut dyn Write) -> Result<i32, String> {
    let query = args.session.as_deref().unwrap_or_default();
    let (conn, id, _) = connect(&args.endpoint, query)?;
    let view = conn.blocking_call(
        "GET",
        &format!(
            "/api/sessions/{id}/pr{}",
            if args.refresh { "?refresh=1" } else { "" }
        ),
        None,
    )?;
    let problem = view.get("problem").filter(|p| p.is_object());
    if args.json {
        print_json(out, &view);
        return Ok(i32::from(problem.is_some() && view["pr"].is_null()));
    }
    let io = |e: std::io::Error| e.to_string();
    let branch = str_field(&view, "branch");
    let pr = &view["pr"];
    if pr.is_null() {
        let why = view
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| problem.map(|p| format!("gh: {}", str_field(p, "reason"))))
            .unwrap_or_else(|| "no pull request".into());
        writeln!(out, "{branch}: {why}").map_err(io)?;
        return Ok(i32::from(problem.is_some()));
    }
    let state = if pr["draft"] == true && pr["state"] == "open" {
        "draft"
    } else {
        str_field(pr, "state")
    };
    writeln!(
        out,
        "{branch} -> PR #{} {state}  {}\n  {}",
        pr["number"],
        str_field(pr, "url"),
        str_field(pr, "title")
    )
    .map_err(io)?;
    let review = pr
        .get("review")
        .and_then(Value::as_str)
        .unwrap_or("none needed")
        .replace('_', " ");
    let mergeable = pr
        .get("mergeable")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let merge_state = pr
        .get("merge_state")
        .and_then(Value::as_str)
        .map(|m| format!(" ({m})"))
        .unwrap_or_default();
    writeln!(
        out,
        "  into {}; review: {review}; mergeable: {mergeable}{merge_state}",
        str_field(pr, "base")
    )
    .map_err(io)?;
    let s = &view["summary"];
    writeln!(
        out,
        "  checks: {} passing, {} failing, {} running{}",
        s["passing"],
        s["failing"],
        s["pending"],
        if s["skipped"].as_u64().unwrap_or(0) > 0 {
            format!(", {} skipped", s["skipped"])
        } else {
            String::new()
        }
    )
    .map_err(io)?;
    let checks = list(&view, "checks");
    let names: Vec<String> = checks
        .iter()
        .map(
            |check| match check.get("workflow").and_then(Value::as_str) {
                Some(w) => format!("{} ({w})", str_field(check, "name")),
                None => str_field(check, "name").to_owned(),
            },
        )
        .collect();
    // As wide as the longest name, up to a point: a longer one puts its
    // link on the next line.
    let width = names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(16, NAME_WIDTH);
    let now = chrono::Utc::now();
    for (check, name) in checks.iter().zip(&names) {
        let took = match check.get("duration_secs").and_then(Value::as_u64) {
            Some(secs) => duration(secs),
            // Running: for how long so far.
            None if str_field(check, "bucket") == "pending" => check
                .get("started_at")
                .and_then(Value::as_str)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .and_then(|at| {
                    u64::try_from((now - at.with_timezone(&chrono::Utc)).num_seconds()).ok()
                })
                .map(|secs| format!("{}...", duration(secs)))
                .unwrap_or_default(),
            None => String::new(),
        };
        let word = bucket_word(str_field(check, "bucket"));
        let link = str_field(check, "link");
        if name.chars().count() > width {
            writeln!(out, "    {word:<9} {name}").map_err(io)?;
            writeln!(out, "    {:<9} {:<width$} {took:>10}  {link}", "", "").map_err(io)?;
        } else {
            writeln!(out, "    {word:<9} {name:<width$} {took:>10}  {link}").map_err(io)?;
        }
    }
    if let Some(p) = problem {
        writeln!(out, "  stale: gh: {}", str_field(p, "reason")).map_err(io)?;
    }
    if let Some(secs) = view.get("throttled_secs").and_then(Value::as_u64) {
        writeln!(
            out,
            "  note: GitHub was asked {secs}s ago by a refresh: this is that answer (refresh again in a few seconds)"
        )
        .map_err(io)?;
    }
    if s["failing"].as_u64().unwrap_or(0) > 0 {
        writeln!(
            out,
            "  fw hub pr fix-ci {query} asks the agent to fix it; fw hub pr log {query} CHECK prints a log"
        )
        .map_err(io)?;
    }
    Ok(0)
}

fn log(args: &LogArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (conn, id, _) = connect(&args.endpoint, &args.session)?;
    let view = conn.blocking_call(
        "GET",
        &format!(
            "/api/sessions/{id}/pr/log?check={}",
            query_escape(&args.check)
        ),
        None,
    )?;
    if args.json {
        print_json(out, &view);
        return Ok(0);
    }
    let io = |e: std::io::Error| e.to_string();
    writeln!(
        out,
        "# {}: last {} of {} lines ({}; secrets redacted){}",
        str_field(&view, "check"),
        view["shown"],
        view["lines"],
        str_field(&view, "command"),
        if view["truncated"] == true {
            "; the log was longer than the hub reads"
        } else {
            ""
        }
    )
    .map_err(io)?;
    writeln!(out, "{}", str_field(&view, "text")).map_err(io)?;
    Ok(0)
}

fn fix(args: &FixArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let (conn, id, label) = connect(&args.endpoint, &args.session)?;
    let path = format!("/api/sessions/{id}/pr/fix");
    let plan = conn.blocking_call(
        "POST",
        &path,
        Some(&json!({ "check": args.check, "dry_run": true })),
    )?;
    let io = |e: std::io::Error| e.to_string();
    let agent = str_field(&plan["agent"], "name").to_owned();
    if !args.json {
        // The agent is the session asked about, or the one running in its
        // folder.
        let other = if str_field(&plan["agent"], "id") == id {
            String::new()
        } else {
            format!(" (the agent in {label}'s folder)")
        };
        writeln!(
            out,
            "Ask {agent}{other} to fix \"{}\":\n",
            str_field(&plan, "check")
        )
        .map_err(io)?;
        for line in str_field(&plan, "prompt").lines() {
            writeln!(out, "  {line}").map_err(io)?;
        }
        writeln!(out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Send it?", err) {
        if args.json {
            print_json(out, &plan);
            return Ok(0);
        }
        writeln!(
            err,
            "fw hub: not sent (pass --yes to send without a prompt)"
        )
        .map_err(io)?;
        return Ok(1);
    }
    let sent = conn.blocking_call(
        "POST",
        &path,
        Some(&json!({ "check": str_field(&plan, "check"), "dry_run": false })),
    )?;
    if args.json {
        print_json(out, &sent);
        return Ok(0);
    }
    match str_field(&sent["delivery"], "delivery") {
        "queued" => writeln!(
            out,
            "queued for {agent} ({}); {} pending",
            str_field(&sent["delivery"], "why"),
            sent["delivery"]["pending"]
        ),
        _ => writeln!(out, "pasted into {agent}"),
    }
    .map_err(io)?;
    Ok(0)
}

fn print_plan(plan: &Value, forcing: bool, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "{}", str_field(plan, "summary"))?;
    for detail in list(plan, "details") {
        writeln!(out, "  {}", detail.as_str().unwrap_or_default())?;
    }
    for command in list(plan, "commands") {
        writeln!(out, "  $ {}", command.as_str().unwrap_or_default())?;
    }
    for warning in list(plan, "warnings") {
        writeln!(out, "  note: {}", warning.as_str().unwrap_or_default())?;
    }
    if forcing && let Some(note) = plan.get("force_note").and_then(Value::as_str) {
        writeln!(out, "  note: {note}")?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn action(
    name: &str,
    query: &str,
    yes: bool,
    json_out: bool,
    force: Option<&str>,
    endpoint: &EndpointArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let (conn, id, label) = connect(endpoint, query)?;
    let path = format!("/api/sessions/{id}/pr/{name}");
    let plan = conn.blocking_call("POST", &path, Some(&json!({ "dry_run": true })))?;
    let io = |e: std::io::Error| e.to_string();
    if !json_out {
        print_plan(&plan, force.is_some(), out).map_err(io)?;
    }
    if let Some(why) = plan.get("refusal").and_then(Value::as_str) {
        let word = plan.get("force_word").and_then(Value::as_str);
        let forcible = plan["forcible"] == true;
        if !(forcible && force.is_some() && force.map(str::trim) == word) {
            if json_out {
                print_json(out, &plan);
            }
            let hint = match (forcible, word, force) {
                (true, Some(w), None) => format!(" (--force {w} merges anyway)"),
                (true, Some(w), Some(_)) => format!(" (--force must be {w}, the PR's number)"),
                _ => String::new(),
            };
            return Err(format!("{label}: {name} refused: {why}{hint}"));
        }
        if !json_out {
            writeln!(out, "  FORCED: {why}").map_err(io)?;
        }
    }
    let question = match name {
        "merge" => "Merge?",
        "ready" => "Mark ready?",
        _ => "Re-run?",
    };
    if !approved(yes, json_out, question, err) {
        if json_out {
            print_json(out, &plan);
            return Ok(0);
        }
        writeln!(err, "fw hub: not run (pass --yes to run without a prompt)").map_err(io)?;
        return Ok(1);
    }
    let done = conn.blocking_call(
        "POST",
        &path,
        Some(&json!({ "confirm": str_field(&plan, "fingerprint"), "force": force })),
    )?;
    let ok = done.get("ok").and_then(Value::as_bool) == Some(true);
    if json_out {
        print_json(out, &done);
    } else {
        writeln!(out, "{}", str_field(&done, "summary")).map_err(io)?;
        if !ok {
            for ran in list(&done, "ran") {
                writeln!(out, "  {}", str_field(ran, "output")).map_err(io)?;
            }
        }
    }
    Ok(i32::from(!ok))
}
