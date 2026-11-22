//! `fw hub sweep`: delete finished worktrees' `target/` dirs (see
//! [`crate::sweep`]) through the running hub, which knows the sessions;
//! `--keep PATH [--until TIME]` writes a keep marker here, no hub needed.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::util::{fail, human_bytes, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// How long a sweep may take (it walks every candidate's `target/`).
const SWEEP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, clap::Args)]
pub(crate) struct SweepArgs {
    /// List what would go and what is kept, and why; delete nothing.
    #[arg(long)]
    dry_run: bool,
    /// Every eligible worktree, whatever the disk use (default: only on
    /// a filesystem past the high watermark, down to the low one).
    #[arg(long)]
    all: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Write a keep marker (`.fw-keep-build`) at the root of the worktree
    /// PATH instead of sweeping: its `target/` is never swept while the
    /// marker holds.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["dry_run", "all"])]
    keep: Option<PathBuf>,
    /// With --keep: the marker expires at TIME (RFC 3339, or a span from
    /// now: 12h, 3d).
    #[arg(long, value_name = "TIME", requires = "keep")]
    until: Option<String>,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// `--until`: an RFC 3339 time, or a span from now.
fn until_time(raw: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let now = chrono::Utc::now();
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
        return Ok(at.with_timezone(&chrono::Utc));
    }
    let span = crate::activity::parse_age(raw)
        .map_err(|_| format!("--until {raw}: want an RFC 3339 time or a span like 12h, 3d"))?;
    Ok(now + span)
}

pub(crate) fn sweep(args: &SweepArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if let Some(path) = &args.keep {
        let until = match args.until.as_deref().map(until_time).transpose() {
            Ok(until) => until,
            Err(message) => return fail(err, &message),
        };
        return match crate::sweep::write_keep(path, until) {
            Ok(marker) => {
                if args.json {
                    print_json(
                        out,
                        &json!({ "marker": marker, "until": until.map(|t| t.to_rfc3339()) }),
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "kept {}{}",
                        marker.display(),
                        until.map_or_else(
                            || " (until the file is removed)".to_owned(),
                            |t| format!(" until {}", t.to_rfc3339())
                        )
                    );
                }
                0
            }
            Err(message) => fail(err, &message),
        };
    }
    let run = || -> Result<Value, String> {
        Endpoint::resolve(&args.endpoint)?
            .with_timeout(SWEEP_TIMEOUT)
            .blocking_call(
                "POST",
                "/api/sweep",
                Some(&json!({ "all": args.all, "dry_run": args.dry_run })),
            )
    };
    match run() {
        Ok(report) if args.json => {
            print_json(out, &report);
            0
        }
        Ok(report) => {
            print_report(out, &report);
            0
        }
        Err(message) => fail(err, &message),
    }
}

fn num(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

/// `/home: 92% used, 230.1 GiB free of 2.9 TiB`.
pub(crate) fn disk_line(disk: &Value) -> String {
    let at = disk["mount"]
        .as_str()
        .unwrap_or_else(|| str_field(disk, "path"));
    format!(
        "{at}: {:.0}% used, {} free of {}",
        disk["used_pct"].as_f64().unwrap_or(0.0),
        human_bytes(num(disk, "free_bytes")),
        human_bytes(num(disk, "total_bytes")),
    )
}

fn print_report(out: &mut dyn Write, report: &Value) {
    let list = |key: &str| report[key].as_array().map_or(&[][..], Vec::as_slice);
    for disk in list("disks") {
        let _ = writeln!(out, "disk {}", disk_line(disk));
    }
    let dry = report["dry_run"] == true;
    let candidates = list("candidates");
    if candidates.is_empty() {
        let _ = writeln!(out, "no candidates");
    } else {
        let _ = writeln!(out, "candidates (oldest first):");
    }
    for c in candidates {
        let mark = match (c["delete"] == true, dry) {
            (true, true) => "would delete",
            (true, false) => "picked",
            (false, _) => "left",
        };
        let _ = writeln!(
            out,
            "  {mark:<12} {}/target  {}, {}, untouched since {}",
            str_field(c, "path"),
            human_bytes(num(c, "bytes")),
            str_field(c, "why"),
            str_field(c, "newest"),
        );
    }
    let kept = list("kept");
    if !kept.is_empty() {
        let _ = writeln!(out, "kept:");
    }
    for k in kept {
        let _ = writeln!(
            out,
            "  {}  {}: {}",
            str_field(k, "path"),
            str_field(k, "reason"),
            str_field(k, "detail")
        );
    }
    for (key, label) in [
        ("skipped", "kept by the check before deleting"),
        ("failed", "failed"),
    ] {
        for k in list(key) {
            let _ = writeln!(
                out,
                "{label}: {}  {}: {}",
                str_field(k, "path"),
                str_field(k, "reason"),
                str_field(k, "detail")
            );
        }
    }
    for d in list("deleted") {
        let _ = writeln!(
            out,
            "deleted {}  {} ({})",
            str_field(d, "target"),
            human_bytes(num(d, "bytes")),
            str_field(d, "why"),
        );
    }
    for e in list("errors") {
        let _ = writeln!(out, "not looked at: {}", e.as_str().unwrap_or(""));
    }
    let opaque = num(report, "opaque_processes");
    if opaque > 0 {
        let _ = writeln!(
            out,
            "{opaque} processes not inspectable here were checked by command line and mounts"
        );
    }
    let picked: Vec<&Value> = candidates.iter().filter(|c| c["delete"] == true).collect();
    let summary = if dry {
        format!(
            "would delete {} of {} target/ dirs, {}",
            picked.len(),
            candidates.len(),
            human_bytes(picked.iter().map(|c| num(c, "bytes")).sum())
        )
    } else {
        format!(
            "deleted {} target/ dirs, {}",
            list("deleted").len(),
            human_bytes(num(report, "reclaimed_bytes"))
        )
    };
    let _ = writeln!(out, "{summary}");
    if report["short"] == true {
        let _ = writeln!(
            out,
            "a disk stays past the high watermark: nothing more is eligible"
        );
    }
}
