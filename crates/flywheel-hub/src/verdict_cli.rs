//! `fw hub verdict record|ls|check|retract`: review records (see
//! [`crate::reviews`]) through the running hub. `check` exits 1 when a
//! commit of the range is not covered by a READY review.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct VerdictArgs {
    #[command(subcommand)]
    command: VerdictCommand,
}

#[derive(Debug, clap::Subcommand)]
enum VerdictCommand {
    /// Record a review of BASE..HEAD (as the operator, or for a reviewer
    /// with --reviewer); kept once, never changed.
    Record(RecordArgs),
    /// Review records, newest first: all, a repo's, or those covering a
    /// commit.
    Ls(LsArgs),
    /// Is every commit of BASE..HEAD covered by a READY review (through
    /// follow-ups, not by its author)? Exit 1 when not, listing why.
    Check(CheckArgs),
    /// Take a record back (it stays, marked, and no longer covers).
    Retract(RetractArgs),
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// BASE..HEAD.
    range: String,
    /// A path inside the repo (default: here).
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: String,
    /// ready, ready-with-changes, or not-ready.
    #[arg(long)]
    verdict: String,
    /// Findings by severity: critical=0,high=1,medium=2,low=0,info=3.
    #[arg(long, value_name = "COUNTS")]
    findings: Option<String>,
    /// The full review (kept by the hub).
    #[arg(long, value_name = "PATH")]
    report: Option<String>,
    /// The review this one answers.
    #[arg(long, value_name = "ID")]
    follow_up_of: Option<String>,
    /// Who reviewed (default: operator).
    #[arg(long, value_name = "LABEL")]
    reviewer: Option<String>,
    /// A short note kept with it.
    #[arg(long, value_name = "TEXT")]
    note: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Only this repo's (a path inside it).
    #[arg(long, value_name = "PATH")]
    repo: Option<String>,
    /// Only records covering this commit.
    #[arg(long, value_name = "SHA")]
    sha: Option<String>,
    /// At most N (default 100).
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// BASE..HEAD.
    range: String,
    /// A path inside the repo (default: here).
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: String,
    /// A commit with no review counts if a covered one has the same patch.
    #[arg(long)]
    patch_equivalent: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RetractArgs {
    /// The review's id (or 4+ of it).
    id: String,
    /// Why it is taken back (kept with it).
    #[arg(long, value_name = "TEXT")]
    reason: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &VerdictArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

/// A path the hub can read: absolute (the hub may run elsewhere in the
/// tree than this CLI).
fn absolute(path: &str) -> String {
    std::path::Path::new(path)
        .canonicalize()
        .map_or_else(|_| path.to_owned(), |p| p.to_string_lossy().into_owned())
}

fn run(args: &VerdictArgs, out: &mut dyn Write) -> Result<i32, String> {
    match &args.command {
        VerdictCommand::Record(a) => {
            let (base, head) = a
                .range
                .split_once("..")
                .ok_or_else(|| format!("range {:?}: BASE..HEAD", a.range))?;
            let findings = match &a.findings {
                Some(raw) => crate::reviews::Findings::parse(raw)?,
                None => crate::reviews::Findings::default(),
            };
            let conn = Endpoint::resolve(&a.endpoint)?;
            let review = conn.blocking_call(
                "POST",
                "/api/verdicts",
                Some(&json!({
                    "repo": absolute(&a.repo),
                    "base": base,
                    "head": head,
                    "verdict": a.verdict,
                    "findings": findings,
                    "report": a.report.as_deref().map(absolute),
                    "follow_up_of": a.follow_up_of,
                    "reviewer": a.reviewer,
                    "note": a.note,
                })),
            )?;
            if a.json {
                print_json(out, &review);
            } else {
                let _ = writeln!(out, "{}", str_field(&review, "id"));
            }
            Ok(0)
        }
        VerdictCommand::Ls(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let mut query = Vec::new();
            if let Some(repo) = &a.repo {
                query.push(format!("repo={}", query_escape(&absolute(repo))));
            }
            if let Some(sha) = &a.sha {
                query.push(format!("sha={}", query_escape(sha)));
            }
            if let Some(limit) = a.limit {
                query.push(format!("limit={limit}"));
            }
            let path = if query.is_empty() {
                "/api/verdicts".to_owned()
            } else {
                format!("/api/verdicts?{}", query.join("&"))
            };
            let view = conn.blocking_call("GET", &path, None)?;
            if a.json {
                print_json(out, &view);
            } else {
                let reviews = view["reviews"].as_array().cloned().unwrap_or_default();
                if reviews.is_empty() {
                    let _ = writeln!(out, "no reviews");
                }
                for r in &reviews {
                    let short = |key: &str| str_field(r, key).chars().take(12).collect::<String>();
                    let _ = writeln!(
                        out,
                        "{}  {}..{}  {}  {} commits  by {}{}{}",
                        str_field(r, "id"),
                        short("base"),
                        short("head"),
                        str_field(r, "verdict"),
                        r["commits"],
                        str_field(r, "reviewer"),
                        r.get("follow_up_of")
                            .and_then(Value::as_str)
                            .map(|f| format!("  follows {f}"))
                            .unwrap_or_default(),
                        if r.get("retracted").is_some_and(|v| !v.is_null()) {
                            "  RETRACTED"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(0)
        }
        VerdictCommand::Check(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let coverage = conn.blocking_call(
                "GET",
                &format!(
                    "/api/verdicts/coverage?repo={}&range={}{}",
                    query_escape(&absolute(&a.repo)),
                    query_escape(&a.range),
                    if a.patch_equivalent {
                        "&patch_equivalent=1"
                    } else {
                        ""
                    }
                ),
                None,
            )?;
            if a.json {
                print_json(out, &coverage);
            } else {
                print_coverage(out, &coverage);
            }
            Ok(if coverage["ok"] == true { 0 } else { 1 })
        }
        VerdictCommand::Retract(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let review = conn.blocking_call(
                "POST",
                &format!("/api/verdicts/{}/retract", query_escape(&a.id)),
                Some(&json!({ "reason": a.reason })),
            )?;
            if a.json {
                print_json(out, &review);
            } else {
                let _ = writeln!(out, "retracted {}", str_field(&review, "id"));
            }
            Ok(0)
        }
    }
}

fn print_coverage(out: &mut dyn Write, coverage: &Value) {
    let short = |v: &Value| {
        v.as_str()
            .unwrap_or("")
            .chars()
            .take(12)
            .collect::<String>()
    };
    if coverage["ok"] == true {
        let _ = writeln!(
            out,
            "ok: all {} commits of {}..{} are covered by READY reviews",
            coverage["commits"],
            short(&coverage["base"]),
            short(&coverage["head"])
        );
        return;
    }
    let _ = writeln!(
        out,
        "NOT ok: {} of {} commits uncovered in {}..{}",
        coverage["uncovered_count"],
        coverage["commits"],
        short(&coverage["base"]),
        short(&coverage["head"])
    );
    for commit in coverage["uncovered"].as_array().into_iter().flatten() {
        let why = match str_field(commit, "why") {
            "no_review" => "no review".to_owned(),
            "not_ready" => format!("NOT_READY ({})", str_field(commit, "review")),
            "ready_with_changes" => format!(
                "READY_WITH_CHANGES with no READY follow-up ({})",
                str_field(commit, "review")
            ),
            "self_review" => format!(
                "reviewed only by its author ({})",
                str_field(commit, "review")
            ),
            other => other.to_owned(),
        };
        let _ = writeln!(
            out,
            "  {} {}: {why}",
            short(&commit["sha"]),
            str_field(commit, "subject")
        );
    }
}
