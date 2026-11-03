//! Why a session shows the status it does: `fw hub explain <session>`
//! (the running hub's view: `GET /api/sessions/{id}/explain`) and
//! `fw hub explain --file screen.txt --kind codex` (a saved screen against
//! this machine's manifests, no hub needed -- for writing overrides).

use super::{Detection, Origin, Problem, RuleTrace, Verdict};
use crate::agent::SessionKind;
use crate::cli::{StateDirArg, resolve_state_dir};
use crate::endpoint::{Endpoint, EndpointArgs};
use crate::status::{StatusSource, StatusView};
use serde::Serialize;
use serde_json::Value;
use std::io::{Read, Write};
use std::path::PathBuf;

/// What every explanation ends with.
pub const IDLE_NOTE: &str = "idle is not authorization: it only means the screen shows no running turn and no prompt the rules know. Nothing here approves anything; a prompt the rules do not know reads as idle or unknown, never blocked.";

/// The manifest an explanation read the screen with.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestInfo {
    pub kind: SessionKind,
    pub file: &'static str,
    pub version: u32,
    #[serde(flatten)]
    pub origin: Origin,
    pub rules: Vec<RuleTrace>,
}

/// A fresh read of the live screen.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenRead {
    #[serde(flatten)]
    pub verdict: Verdict,
    /// What no match means for the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// The session an explanation is about.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
}

/// Everything `fw hub explain` prints.
#[derive(Debug, Clone, Serialize)]
pub struct Explanation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub kind: SessionKind,
    /// The status clients see (agent sessions only).
    pub status: Option<StatusView>,
    /// What set it.
    pub source: Option<StatusSource>,
    /// `hook`, `manifest:<file>#<rule>`, `heuristic`, `hub`, `none`.
    pub source_label: Option<String>,
    /// The live screen read now (`None` without a terminal).
    pub screen: Option<ScreenRead>,
    pub manifest: ManifestInfo,
    /// Override files not in force, and why.
    pub problems: Vec<Problem>,
    pub note: &'static str,
}

/// What a missing verdict means for `kind`.
fn fallback(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => "no rule matched: nothing to say (shells have no agent status)",
        _ => {
            "no rule matched: the screen changes nothing on its own; a running turn whose screen stays unrecognized for 3 s settles to idle (reported) or unknown (screen-driven), never blocked"
        }
    }
}

/// Explain `rows` (a screen, top to bottom) against `kind`'s manifest in
/// force; the caller fills in the session and its status.
pub fn explain_screen(
    detection: &Detection,
    kind: SessionKind,
    rows: Option<&[String]>,
) -> Explanation {
    let manifest = detection.manifest(kind);
    let (screen, rules) = match rows {
        Some(rows) => {
            let (verdict, rules) = manifest.trace(rows);
            let fallback = verdict.state.is_none().then(|| fallback(kind).to_owned());
            (Some(ScreenRead { verdict, fallback }), rules)
        }
        None => (None, Vec::new()),
    };
    Explanation {
        session: None,
        kind,
        status: None,
        source: None,
        source_label: None,
        screen,
        manifest: ManifestInfo {
            kind,
            file: manifest.file(),
            version: manifest.version,
            origin: manifest.origin.clone(),
            rules,
        },
        problems: detection.problems(),
        note: IDLE_NOTE,
    }
}

// --- CLI ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct ExplainArgs {
    /// Session id, unique id prefix, or name.
    #[arg(required_unless_present = "file", conflicts_with = "file")]
    session: Option<String>,
    /// Explain a saved screen (`fw hub read S > screen.txt`; `-` for
    /// stdin) against this machine's manifests instead; no hub needed.
    #[arg(long, value_name = "FILE", requires = "kind")]
    file: Option<PathBuf>,
    /// With --file: the manifest to read it with.
    #[arg(long, alias = "agent", value_parser = ["claude", "codex", "shell"])]
    kind: Option<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// `fw hub explain`.
pub fn run(args: &ExplainArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.file {
        Some(file) => offline(file, args.kind.as_deref(), &args.endpoint.state),
        None => online(args),
    };
    let value = match result {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        render(&value, out);
    }
    0
}

fn offline(file: &PathBuf, kind: Option<&str>, state: &StateDirArg) -> Result<Value, String> {
    let kind = match kind {
        Some("claude") => SessionKind::Claude,
        Some("codex") => SessionKind::Codex,
        _ => SessionKind::Shell,
    };
    let mut text = String::new();
    if file.as_os_str() == "-" {
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    let rows: Vec<String> = text.lines().map(str::to_owned).collect();
    let state_dir = resolve_state_dir(state)?;
    let detection = Detection::with_overrides_quiet(&state_dir);
    serde_json::to_value(explain_screen(&detection, kind, Some(&rows))).map_err(|e| e.to_string())
}

fn online(args: &ExplainArgs) -> Result<Value, String> {
    let query = args.session.as_deref().unwrap_or_default();
    let conn = Endpoint::resolve(&args.endpoint)?;
    let sessions = conn.blocking_sessions()?;
    let session = crate::script::resolve(&sessions, query, "session")?;
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    conn.blocking_call("GET", &format!("/api/sessions/{id}/explain"), None)
}

fn text<'a>(value: &'a Value, path: &str) -> &'a str {
    value.pointer(path).and_then(Value::as_str).unwrap_or("")
}

/// Cut `s` to `max` characters (an ellipsis marks the cut).
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// The human form of an explanation (from its JSON, so a remote hub's
/// answer prints the same).
pub fn render(value: &Value, out: &mut dyn Write) {
    let kind = text(value, "/kind");
    if let Some(session) = value.get("session").filter(|s| s.is_object()) {
        let _ = writeln!(
            out,
            "session  {} ({})  {kind}",
            text(session, "/name"),
            text(session, "/id")
        );
    } else {
        let _ = writeln!(out, "screen   read with the {kind} manifest");
    }
    if let Some(status) = value.get("status").filter(|s| s.is_object()) {
        let detail = text(status, "/detail");
        let _ = writeln!(
            out,
            "status   {}  since {}{}",
            text(status, "/status"),
            text(status, "/since"),
            if detail.is_empty() {
                String::new()
            } else {
                format!("  -- {detail}")
            }
        );
        let source = value.get("source").cloned().unwrap_or(Value::Null);
        let label = text(value, "/source_label");
        let why = match text(&source, "/kind") {
            "hook" => format!(
                "{label} ({} {})",
                text(&source, "/agent"),
                text(&source, "/event")
            ),
            "heuristic" | "hub" => format!("{label} ({})", text(&source, "/reason")),
            "none" => "none (no signal yet)".to_owned(),
            _ => label.to_owned(),
        };
        let _ = writeln!(out, "source   {why}");
    } else if value.get("session").is_some_and(Value::is_object) {
        let _ = writeln!(out, "status   none (shell sessions have no agent status)");
    }
    let manifest = value.get("manifest").cloned().unwrap_or(Value::Null);
    let origin = match text(&manifest, "/origin") {
        "override" => format!("override {}", text(&manifest, "/path")),
        other => other.to_owned(),
    };
    let version = manifest.get("version").and_then(Value::as_u64).unwrap_or(0);
    match value.get("screen").filter(|s| s.is_object()) {
        None => {
            let _ = writeln!(out, "screen   no terminal on this hub (nothing to read)");
        }
        Some(screen) => {
            let file = text(&manifest, "/file");
            match screen.get("state").and_then(Value::as_str) {
                Some(state) => {
                    let detail = text(screen, "/detail");
                    let _ = writeln!(
                        out,
                        "screen   {state} by {file}#{} ({origin}, version {version}){}",
                        text(screen, "/rule"),
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!("  -- {detail}")
                        }
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "screen   {} ({file}: {origin}, version {version})",
                        text(screen, "/fallback")
                    );
                }
            }
            let evidence = screen
                .get("evidence")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for e in &evidence {
                let row = e.get("row").and_then(Value::as_u64).unwrap_or(0);
                let _ = writeln!(out, "  row {row:>3}  {}", cut(text(e, "/text").trim(), 90));
            }
        }
    }
    let rules = manifest
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !rules.is_empty() {
        let width = rules
            .iter()
            .map(|r| text(r, "/id").chars().count())
            .max()
            .unwrap_or(0);
        let _ = writeln!(out, "rules");
        for rule in &rules {
            let matched = rule.get("matched").and_then(Value::as_bool) == Some(true);
            let reason = text(rule, "/reason");
            let _ = writeln!(
                out,
                "  {} {:<width$}  {:<7}  {}",
                if matched { "+" } else { "-" },
                text(rule, "/id"),
                text(rule, "/state"),
                if matched {
                    "matched".to_owned()
                } else {
                    reason.to_owned()
                }
            );
        }
    }
    let problems = value
        .get("problems")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !problems.is_empty() {
        let _ = writeln!(out, "problems");
        for p in &problems {
            let _ = writeln!(out, "  {}: {}", text(p, "/path"), text(p, "/error"));
        }
    }
    let _ = writeln!(out, "note     {}", text(value, "/note"));
}
