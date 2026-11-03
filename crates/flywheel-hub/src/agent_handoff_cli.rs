//! `fw hub handoff FROM TO` and `fw hub ask-review SESSION`: the
//! cross-agent handoff and review pairing (see [`crate::agent_handoff`])
//! from a terminal, through the running hub (and, with `--machine` /
//! `--to-machine`, its proxy to other machines).

use crate::commands_cli::visible_controls;
use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{find_session, session_label};
use crate::util::{fail, str_field};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("what")
        .required(true)
        .args(["selection_file", "diff", "last_command", "command", "message"])
))]
pub(crate) struct HandoffArgs {
    /// The session handed from (id, unique id prefix, or name).
    from: String,
    /// The agent session handed to.
    to: String,
    /// Hand off the text in FILE (`-`: stdin), as selected terminal text.
    #[arg(long, value_name = "FILE")]
    selection_file: Option<PathBuf>,
    /// Hand off FROM's branch diff against its base (`git diff
    /// <base>...HEAD`; with --path, one file's).
    #[arg(long)]
    diff: bool,
    /// With --diff: only this file's part.
    #[arg(long, value_name = "PATH", requires = "diff")]
    path: Option<String>,
    /// With --diff: compare against REF instead of FROM's base.
    #[arg(long, value_name = "REF", requires = "diff")]
    base: Option<String>,
    /// Hand off the last shell command FROM suggested.
    #[arg(long)]
    last_command: bool,
    /// Hand off FROM's suggested code block SEQ (`fw hub commands --json`).
    #[arg(long, value_name = "SEQ")]
    command: Option<u64>,
    /// Hand off message N of FROM's transcript (`fw hub search --json`).
    #[arg(long, value_name = "N")]
    message: Option<u64>,
    /// TO is on this machine (registered on the hub): reached through
    /// its proxy; `local`: this hub (FROM on --machine, TO here).
    /// Default: where FROM is.
    #[arg(long, value_name = "NAME")]
    to_machine: Option<String>,
    /// Keep at most this many bytes of the content (default 32768).
    #[arg(long, value_name = "BYTES")]
    max_bytes: Option<usize>,
    /// Open the composed prompt in $VISUAL / $EDITOR before sending (an
    /// empty file sends nothing).
    #[arg(long, conflicts_with = "dry_run")]
    edit: bool,
    /// Print the composed prompt; send nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print JSON (the composed prompt, and what was delivered).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AskReviewArgs {
    /// The session whose work is reviewed (a worktree agent, its
    /// companion shell, or its reviewer: asks again).
    session: String,
    /// The reviewer's kind (default: hub.toml review.reviewer, "other":
    /// Claude work gets Codex and back; the other kind when that is not
    /// installed).
    #[arg(long, value_parser = ["claude", "codex"])]
    kind: Option<String>,
    /// Compare against REF instead of the session's base.
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    /// Print what would launch (kind, read-only flags, prompt); launch
    /// nothing.
    #[arg(long, conflicts_with_all = ["findings", "send_findings"])]
    dry_run: bool,
    /// Print the reviewer's findings so far (its latest reply).
    #[arg(long, conflicts_with = "send_findings")]
    findings: bool,
    /// Paste the reviewer's findings into the author agent, then Enter.
    #[arg(long)]
    send_findings: bool,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

fn print_json(out: &mut dyn Write, value: &Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// Agent-written text for a terminal: controls shown, not obeyed.
fn shown(text: &str) -> String {
    if std::io::stdout().is_terminal() {
        visible_controls(text)
    } else {
        text.to_owned()
    }
}

fn read_selection(path: &PathBuf) -> Result<String, String> {
    let bytes = if path.as_os_str() == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        bytes
    } else {
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?
    };
    String::from_utf8(bytes).map_err(|_| "the selection is not UTF-8 text".to_owned())
}

/// Let the user edit `prompt` in their editor; `None`: they emptied it.
fn edit(prompt: &str) -> Result<Option<String>, String> {
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned());
    let path = std::env::temp_dir().join(format!(
        "fw-hub-handoff-{}-{}.md",
        std::process::id(),
        crate::state::new_id()
    ));
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        file.write_all(prompt.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    // $EDITOR may carry arguments (`code --wait`): run it through sh.
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(format!("{editor} exited with {status}: nothing sent")),
        Err(e) => return Err(format!("cannot run {editor}: {e}")),
    }
    let edited = edited.map_err(|e| format!("cannot read the edited prompt: {e}"))?;
    Ok((!edited.trim().is_empty()).then_some(edited))
}

/// Run `fw hub handoff`.
pub(crate) fn handoff(args: &HandoffArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_handoff(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn run_handoff(
    args: &HandoffArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let source = if let Some(path) = &args.selection_file {
        json!({ "kind": "selection", "text": read_selection(path)?, "from": "terminal" })
    } else if args.diff {
        json!({ "kind": "diff", "path": args.path, "base": args.base })
    } else if args.last_command {
        json!({ "kind": "last_command" })
    } else if let Some(seq) = args.command {
        json!({ "kind": "command", "seq": seq })
    } else if let Some(index) = args.message {
        json!({ "kind": "transcript", "index": index })
    } else {
        return Err("say what to hand off".into());
    };
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (from_id, _) = find_session(&conn, &args.from)?;
    // Where TO is: a machine, this hub (`local`), or where FROM is.
    let to_machine = match args.to_machine.as_deref().map(str::trim) {
        Some("local") => None,
        Some(name) => Some(name.to_owned()),
        None => args.endpoint.machine.clone(),
    };
    let cross = to_machine != args.endpoint.machine;
    let elsewhere = if cross {
        Some(Endpoint::resolve(&EndpointArgs {
            machine: to_machine.clone(),
            ..args.endpoint.clone()
        })?)
    } else {
        None
    };
    let to_conn = elsewhere.as_ref().unwrap_or(&conn);
    let (to_id, to) = find_session(to_conn, &args.to)?;
    if !cross && to_id == from_id {
        return Err(format!(
            "{} is the session it comes from: pick another agent",
            session_label(&to)
        ));
    }
    let composed = conn.blocking_call(
        "POST",
        &format!("/api/sessions/{from_id}/handoff/compose"),
        Some(&json!({
            "source": source,
            "max_bytes": args.max_bytes,
            "cross_machine": cross,
        })),
    )?;
    let mut prompt = str_field(&composed, "prompt").to_owned();
    if composed.get("truncated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            err,
            "fw hub: the content is cut to {} of {} bytes (--max-bytes)",
            composed["kept_bytes"], composed["content_bytes"]
        );
    }
    if args.dry_run {
        if args.json {
            print_json(out, &composed);
        } else {
            let _ = write!(out, "{}", shown(&prompt));
        }
        return Ok(0);
    }
    if args.edit {
        match edit(&prompt)? {
            Some(edited) => prompt = edited,
            None => {
                let _ = writeln!(err, "fw hub: the prompt is empty: nothing sent");
                return Ok(1);
            }
        }
    }
    let delivered = to_conn.blocking_call(
        "POST",
        &format!("/api/sessions/{to_id}/handoff"),
        Some(&json!({
            "prompt": prompt,
            "from": composed["from"],
            "source": composed["source"],
            "truncated": composed["truncated"],
        })),
    )?;
    if args.json {
        print_json(
            out,
            &json!({ "composed": composed, "delivered": delivered }),
        );
    } else {
        let _ = writeln!(
            out,
            "handed {} to {}{} ({} bytes)",
            str_field(&composed, "what"),
            session_label(&to),
            if cross {
                format!(" on {}", to_machine.as_deref().unwrap_or("this hub"))
            } else {
                String::new()
            },
            prompt.len()
        );
    }
    Ok(0)
}

/// Run `fw hub ask-review`.
pub(crate) fn ask_review(args: &AskReviewArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_ask_review(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn run_ask_review(
    args: &AskReviewArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let (id, _) = find_session(&conn, &args.session)?;
    if args.findings {
        let findings =
            conn.blocking_call("GET", &format!("/api/sessions/{id}/review/findings"), None)?;
        if args.json {
            print_json(out, &findings);
            return Ok(0);
        }
        let reviewer = &findings["reviewer"];
        match findings.get("reply").and_then(Value::as_str) {
            Some(reply) => {
                let _ = writeln!(out, "{}", shown(reply));
                Ok(0)
            }
            None => {
                let _ = writeln!(
                    err,
                    "fw hub: {} has not answered yet ({})",
                    str_field(reviewer, "name"),
                    reviewer.get("status").and_then(Value::as_str).unwrap_or(
                        if reviewer["running"] == true {
                            "running"
                        } else {
                            "not running"
                        }
                    )
                );
                Ok(1)
            }
        }
    } else if args.send_findings {
        let delivered = conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/review/findings/send"),
            Some(&json!({})),
        )?;
        if args.json {
            print_json(out, &delivered);
        } else {
            let _ = writeln!(
                out,
                "sent the findings to {}",
                session_label(&delivered["to"])
            );
        }
        Ok(0)
    } else {
        let launched = conn.blocking_call(
            "POST",
            &format!("/api/sessions/{id}/review/reviewer"),
            Some(&json!({ "kind": args.kind, "base": args.base, "dry_run": args.dry_run })),
        )?;
        if let Some(note) = launched.get("fallback").and_then(Value::as_str) {
            let _ = writeln!(err, "fw hub: {note}");
        }
        if args.json {
            print_json(out, &launched);
            return Ok(0);
        }
        let flags = launched["flags"]
            .as_array()
            .map(|f| {
                f.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let what = format!(
            "{} vs {}",
            launched["branch"]
                .as_str()
                .map_or_else(|| "HEAD".to_owned(), |b| format!("branch {b}")),
            str_field(&launched, "base")
        );
        let author = str_field(&launched["author"], "name");
        if let Some(old) = launched.get("replaced").filter(|r| r.is_object()) {
            let _ = writeln!(
                err,
                "fw hub: {} ({}) is replaced: it is closed",
                str_field(old, "name"),
                str_field(old, "kind")
            );
        }
        let kind = str_field(&launched, "kind");
        if args.dry_run {
            let _ = writeln!(
                out,
                "would {} a {kind} reviewer ({flags}) of {author} ({what}); its prompt:\n\n{}",
                if launched["reused"] == true {
                    "ask again"
                } else {
                    "launch"
                },
                shown(str_field(&launched, "prompt"))
            );
        } else {
            let _ = writeln!(
                out,
                "{} {kind} reviewer {} ({flags}) for {author} ({what})",
                if launched["reused"] == true {
                    "asked"
                } else {
                    "launched"
                },
                session_label(&launched["reviewer"])
            );
        }
        Ok(0)
    }
}
