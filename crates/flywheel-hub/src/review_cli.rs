//! `fw hub review <session>` (a session's branch against its base, and
//! its working tree; `--diff PATH` one file's diff) and `fw hub merge
//! <session>` (fast-forward the base onto a worktree session's branch in
//! the main checkout, after showing exactly what runs), over the running
//! hub's review API.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{query_escape, session_label};
use crate::util::{approved, list, str_field, write_json};
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

/// Review actions can run commit hooks and push to a slow remote.
const ACTION_TIMEOUT: Duration = Duration::from_secs(330);

#[derive(Debug, clap::Args)]
pub struct ReviewArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// Compare against this branch or commit instead of the session's
    /// base (a worktree's base, else the branch's upstream).
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    /// Print one file's diff instead (all of it).
    #[arg(long, value_name = "PATH")]
    diff: Option<String>,
    /// Which change of --diff PATH: committed (the branch's commits),
    /// staged, unstaged, or untracked.
    #[arg(long, default_value = "committed", requires = "diff",
          value_parser = ["committed", "staged", "unstaged", "untracked"])]
    scope: String,
    /// The review (or the --diff page) as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// A worktree session: id, unique id prefix, or name.
    session: String,
    /// Run without asking (the plan is still printed). Without it a
    /// terminal is asked; anything else only prints the plan.
    #[arg(long, short = 'y')]
    yes: bool,
    /// When the branch is not a fast-forward of its base, first rebase it
    /// onto the base in the worktree (a conflict stops there, left for the
    /// agent or you to resolve).
    #[arg(long)]
    rebase: bool,
    /// After merging, remove the worktree and its companion shells (only
    /// when nothing would be lost; the merged branch is deleted too).
    #[arg(long)]
    remove_worktree: bool,
    /// Abort the worktree's stopped rebase instead (the branch goes back
    /// to where it was before it).
    #[arg(long, conflicts_with_all = ["rebase", "remove_worktree"])]
    abort_rebase: bool,
    /// Print the plan / outcome as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn find(conn: &Endpoint, query: &str) -> Result<(String, String), String> {
    let (id, session) = crate::script::find_session(conn, query)?;
    Ok((id, session_label(&session)))
}

/// Run `fw hub review`.
pub fn review(args: &ReviewArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = (|| -> Result<(), String> {
        let conn = Endpoint::resolve(&args.endpoint)?;
        let (id, _) = find(&conn, &args.session)?;
        let base = args
            .base
            .as_deref()
            .map(|b| format!("&base={}", query_escape(b)))
            .unwrap_or_default();
        let io = |e: std::io::Error| e.to_string();
        if let Some(path) = &args.diff {
            let mut offset = 0u64;
            let mut pages = Vec::new();
            loop {
                let page = conn.blocking_call(
                    "GET",
                    &format!(
                        "/api/sessions/{id}/review/diff?scope={}&path={}&offset={offset}{base}",
                        args.scope,
                        query_escape(path)
                    ),
                    None,
                )?;
                let next = page.get("next").and_then(Value::as_u64);
                if args.json {
                    pages.push(page);
                } else {
                    for line in list(&page, "lines") {
                        writeln!(out, "{}", line.as_str().unwrap_or_default()).map_err(io)?;
                    }
                    if page.get("truncated").and_then(Value::as_bool) == Some(true)
                        && next.is_none()
                    {
                        writeln!(
                            err,
                            "fw hub: the diff is longer than the hub reads; cut here"
                        )
                        .map_err(io)?;
                    }
                }
                match next {
                    Some(n) => offset = n,
                    None => break,
                }
            }
            if args.json {
                writeln!(out, "{}", Value::Array(pages)).map_err(io)?;
            }
            return Ok(());
        }
        let review = conn.blocking_call(
            "GET",
            &format!("/api/sessions/{id}/review?{}", base.trim_start_matches('&')),
            None,
        )?;
        if args.json {
            write_json(out, &review).map_err(io)?;
        } else {
            render_review(&review, out).map_err(io)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

fn counts(f: &Value) -> String {
    if f.get("binary").and_then(Value::as_bool) == Some(true) {
        return "binary".to_owned();
    }
    format!("+{} -{}", n(f, "insertions"), n(f, "deletions"))
}

fn file_line(f: &Value) -> String {
    let status = match str_field(f, "status") {
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "copied" => "C",
        "type" => "T",
        "unmerged" => "U",
        _ => "M",
    };
    let path = match f.get("old_path").and_then(Value::as_str) {
        Some(old) => format!("{old} -> {}", str_field(f, "path")),
        None => str_field(f, "path").to_owned(),
    };
    format!("  {status} {:>12}  {path}", counts(f))
}

fn render_set(out: &mut dyn Write, title: &str, set: &Value) -> std::io::Result<()> {
    let files = list(set, "files");
    if files.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "{title} ({} file{}, +{} -{}){}:",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        n(set, "insertions"),
        n(set, "deletions"),
        if set.get("truncated").and_then(Value::as_bool) == Some(true) {
            ", more not listed"
        } else {
            ""
        }
    )?;
    for f in files {
        writeln!(out, "{}", file_line(f))?;
    }
    Ok(())
}

fn render_review(r: &Value, out: &mut dyn Write) -> std::io::Result<()> {
    let rebasing = r
        .get("rebasing")
        .and_then(Value::as_str)
        .map(|b| format!("{b} (rebasing)"));
    let branch = r
        .get("branch")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(rebasing)
        .unwrap_or_else(|| "(detached HEAD)".to_owned());
    let base_is_upstream = r
        .get("base")
        .is_some_and(|b| str_field(b, "source") == "upstream");
    writeln!(
        out,
        "{} in {}",
        str_field(r, "session"),
        str_field(r, "dir")
    )?;
    match r.get("base").filter(|b| !b.is_null()) {
        Some(base) => writeln!(
            out,
            "{branch} vs {} ({}): {} ahead, {} behind",
            str_field(base, "name"),
            str_field(base, "source"),
            n(r, "ahead"),
            n(r, "behind")
        )?,
        None => writeln!(
            out,
            "{branch}: no base or upstream to compare against (--base REF)"
        )?,
    }
    if base_is_upstream {
        // The line above is the upstream comparison already.
    } else if let Some(up) = r.get("upstream").filter(|u| !u.is_null()) {
        writeln!(
            out,
            "upstream {}: {} ahead, {} behind",
            str_field(up, "name"),
            n(up, "ahead"),
            n(up, "behind")
        )?;
    } else if r.get("branch").is_some_and(|b| !b.is_null()) {
        writeln!(out, "not pushed (no upstream)")?;
    }
    if let Some(op) = r.get("operation").and_then(Value::as_str) {
        writeln!(out, "a {op} is in progress")?;
    }
    let commits = list(r, "commits");
    if !commits.is_empty() {
        writeln!(out, "\ncommits ({}):", n(r, "ahead"))?;
        for c in commits {
            writeln!(
                out,
                "  {}  {}  ({}, {})",
                str_field(c, "short"),
                str_field(c, "subject"),
                str_field(c, "author"),
                str_field(c, "date")
            )?;
        }
    }
    let mut first = true;
    let mut gap = |out: &mut dyn Write| -> std::io::Result<()> {
        if first {
            first = false;
            writeln!(out)?;
        }
        Ok(())
    };
    for (title, key) in [
        ("committed", "committed"),
        ("staged", "staged"),
        ("unstaged", "unstaged"),
    ] {
        if let Some(set) = r.get(key)
            && !list(set, "files").is_empty()
        {
            gap(out)?;
            render_set(out, title, set)?;
        }
    }
    let untracked = list(r, "untracked");
    if !untracked.is_empty() {
        gap(out)?;
        writeln!(out, "untracked ({}):", untracked.len())?;
        for u in untracked {
            writeln!(out, "  ? {}", str_field(u, "path"))?;
        }
    }
    let conflicts = list(r, "conflicts");
    if !conflicts.is_empty() {
        gap(out)?;
        writeln!(out, "conflicts ({}):", conflicts.len())?;
        for c in conflicts {
            writeln!(out, "  U {}", c.as_str().unwrap_or_default())?;
        }
    }
    Ok(())
}

pub(crate) fn render_plan(plan: &Value, out: &mut dyn Write) -> std::io::Result<()> {
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

/// Run `fw hub merge`.
pub fn merge(args: &MergeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match merge_inner(args, out, err) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

fn merge_inner(args: &MergeArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(ACTION_TIMEOUT);
    let (id, label) = find(&conn, &args.session)?;
    let io = |e: std::io::Error| e.to_string();
    if args.abort_rebase {
        return abort_rebase(args, &conn, &id, &label, out, err);
    }
    let dry = json!({ "dry_run": true });
    let mut plan = conn.blocking_call(
        "POST",
        &format!("/api/sessions/{id}/review/merge"),
        Some(&dry),
    )?;
    let needs_rebase = plan.get("needs_rebase").and_then(Value::as_bool) == Some(true);
    if needs_rebase && args.rebase {
        let rebase = conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/review/rebase"),
            Some(&dry),
        )?;
        if let Some(why) = rebase.get("refusal").and_then(Value::as_str) {
            if args.json {
                writeln!(out, "{rebase}").map_err(io)?;
            }
            return Err(format!("cannot rebase {label}: {why}"));
        }
        if !args.json {
            render_plan(&rebase, out).map_err(io)?;
        }
        if !approved(args.yes, args.json, "Rebase?", err) {
            if args.json {
                writeln!(out, "{rebase}").map_err(io)?;
            } else {
                writeln!(
                    err,
                    "fw hub: not rebased (pass --yes to run without a prompt)"
                )
                .map_err(io)?;
            }
            return Ok(if args.json { 0 } else { 1 });
        }
        let done = conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/review/rebase"),
            Some(&json!({ "confirm": str_field(&rebase, "fingerprint") })),
        )?;
        if done.get("ok").and_then(Value::as_bool) != Some(true) {
            if args.json {
                writeln!(out, "{done}").map_err(io)?;
            } else {
                writeln!(out, "{}", str_field(&done, "summary")).map_err(io)?;
            }
            return Ok(1);
        }
        if !args.json {
            writeln!(out, "{}\n", str_field(&done, "summary")).map_err(io)?;
        }
        plan = conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/review/merge"),
            Some(&dry),
        )?;
    }
    if let Some(why) = plan.get("refusal").and_then(Value::as_str) {
        if args.json {
            writeln!(out, "{plan}").map_err(io)?;
        }
        let hint = if plan.get("needs_rebase").and_then(Value::as_bool) == Some(true) {
            " (--rebase rebases it first)"
        } else {
            ""
        };
        return Err(format!("cannot merge {label}: {why}{hint}"));
    }
    if !args.json {
        render_plan(&plan, out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Merge?", err) {
        if args.json {
            writeln!(out, "{plan}").map_err(io)?;
            return Ok(0);
        }
        writeln!(
            err,
            "fw hub: not merged (pass --yes to run without a prompt)"
        )
        .map_err(io)?;
        return Ok(1);
    }
    let done = conn.blocking_call(
        "POST",
        &format!("/api/sessions/{id}/review/merge"),
        Some(&json!({ "confirm": str_field(&plan, "fingerprint") })),
    )?;
    let mut report = json!({ "merge": done });
    if !args.json {
        writeln!(out, "{}", str_field(&report["merge"], "summary")).map_err(io)?;
    }
    if args.remove_worktree {
        let removed = conn.blocking_call(
            "DELETE",
            &format!("/api/sessions/{id}?remove_worktree=true"),
            None,
        );
        match removed {
            Ok(removed) => {
                if !args.json {
                    let wt = removed.get("worktree").cloned().unwrap_or(Value::Null);
                    writeln!(
                        out,
                        "removed {label} and worktree {}; branch {} {}",
                        str_field(&wt, "path"),
                        str_field(&wt, "branch"),
                        if wt.get("branch_deleted").and_then(Value::as_bool) == Some(true) {
                            "deleted (merged)"
                        } else {
                            "kept"
                        }
                    )
                    .map_err(io)?;
                }
                report["removed"] = removed;
            }
            Err(e) => {
                if args.json {
                    report["remove_error"] = Value::String(e.clone());
                    writeln!(out, "{report}").map_err(io)?;
                }
                return Err(format!("merged, but the worktree was not removed: {e}"));
            }
        }
    } else if !args.json
        && let Some(wt) = report["merge"].get("worktree").filter(|w| !w.is_null())
    {
        let hint = match wt.get("refusal").and_then(Value::as_str) {
            None => format!(
                "remove the worktree: fw hub kill {} --rm --remove-worktree",
                args.session
            ),
            Some(why) => format!("the worktree is kept: {why}"),
        };
        writeln!(out, "{hint}").map_err(io)?;
    }
    if args.json {
        writeln!(out, "{report}").map_err(io)?;
    }
    Ok(0)
}

/// `fw hub merge --abort-rebase`: show the plan, confirm, run.
fn abort_rebase(
    args: &MergeArgs,
    conn: &Endpoint,
    id: &str,
    label: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let url = format!("/api/sessions/{id}/review/abort");
    let plan = conn.blocking_call("POST", &url, Some(&json!({ "dry_run": true })))?;
    if let Some(why) = plan.get("refusal").and_then(Value::as_str) {
        if args.json {
            writeln!(out, "{plan}").map_err(io)?;
        }
        return Err(format!("cannot abort {label}'s rebase: {why}"));
    }
    if !args.json {
        render_plan(&plan, out).map_err(io)?;
    }
    if !approved(args.yes, args.json, "Abort the rebase?", err) {
        if args.json {
            writeln!(out, "{plan}").map_err(io)?;
            return Ok(0);
        }
        writeln!(
            err,
            "fw hub: not aborted (pass --yes to run without a prompt)"
        )
        .map_err(io)?;
        return Ok(1);
    }
    let done = conn.blocking_call(
        "POST",
        &url,
        Some(&json!({ "confirm": str_field(&plan, "fingerprint") })),
    )?;
    if args.json {
        writeln!(out, "{done}").map_err(io)?;
    } else {
        writeln!(out, "{}", str_field(&done, "summary")).map_err(io)?;
    }
    Ok(0)
}
