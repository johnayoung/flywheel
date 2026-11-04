//! `fw hub perms show|allow|suggest|codex`: agent permission rules through
//! the running hub (see [`crate::perms`]). A target is a folder (a path)
//! or a session (id, unique id prefix, or name). Writes preview first and
//! need a confirmation (`--yes`), a dangerous rule its text typed back on
//! a terminal.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::find_session;
use crate::util::{fail, print_json, str_field};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

#[derive(Debug, clap::Args)]
pub(crate) struct PermsArgs {
    #[command(subcommand)]
    command: PermsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PermsCommand {
    /// Every permission source for a folder or session, highest precedence
    /// first: Claude Code's managed, command-line, local, project, and user
    /// settings with each rule and its file; Codex's approval policy,
    /// sandbox, trust, and prefix rules; a blocked session's pending
    /// request with the narrowest rule for it.
    Show(ShowArgs),
    /// Add allow rules (Claude Code `Tool(spec)` rules, or with --agent
    /// codex a command prefix): previews the file, the diff, and what the
    /// rule lets the agent do, then writes it once confirmed (--yes).
    /// Dangerous rules are written only after they are typed back.
    Allow(AllowArgs),
    /// The requests the folder's repo blocked on most in the last days,
    /// each with the narrow rule that would stop the prompt.
    Suggest(SuggestArgs),
    /// Set a documented Codex setting for the folder: trust_level (user
    /// config, [projects."<repo>"]), approval_policy or sandbox_mode (the
    /// project's .codex/config.toml).
    Codex(CodexArgs),
}

// `--machine` is refused (permission writes are this hub's own): hidden
// from help so help matches what the verb takes.
#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct ShowArgs {
    /// A folder (path) or a session (id, unique id prefix, or name).
    target: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct AllowArgs {
    /// A folder (path) or a session (id, unique id prefix, or name).
    target: String,
    /// The rules: `Bash(cargo test -p app *)`, `Edit(/src/**)`,
    /// `WebFetch(domain:docs.rs)`; with --agent codex, a command prefix
    /// (`cargo test -p app`, split like a shell line).
    #[arg(required = true)]
    rules: Vec<String>,
    /// Claude Code: local (.claude/settings.local.json, the default),
    /// project (.claude/settings.json), or user (~/.claude/settings.json).
    /// Codex rules: project (.codex/rules/default.rules) or user.
    #[arg(long, value_parser = ["local", "project", "user"])]
    file: Option<String>,
    /// claude (default; a Codex session: codex).
    #[arg(long, value_parser = ["claude", "codex"])]
    agent: Option<String>,
    /// Write without asking (dangerous rules still need typing on a
    /// terminal).
    #[arg(long)]
    yes: bool,
    /// Also add .claude/settings.local.json to the checkout's .gitignore
    /// (when it is neither ignored nor tracked).
    #[arg(long)]
    gitignore: bool,
    /// A session target blocked on a prompt the rules cover: leave the
    /// prompt waiting (default: answer it "Yes" once written).
    #[arg(long)]
    no_answer: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct SuggestArgs {
    /// A folder (path) or a session (id, unique id prefix, or name).
    target: String,
    /// How far back to look (1-30).
    #[arg(long, default_value_t = 14)]
    days: i64,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct CodexArgs {
    /// A folder (path) or a session (id, unique id prefix, or name).
    target: String,
    /// trust_level, approval_policy, or sandbox_mode.
    #[arg(value_parser = ["trust_level", "approval_policy", "sandbox_mode"])]
    setting: String,
    /// trust_level: trusted|untrusted; approval_policy:
    /// on-request|never; sandbox_mode:
    /// read-only|workspace-write|danger-full-access.
    value: String,
    /// Write without asking (dangerous values still need typing on a
    /// terminal).
    #[arg(long)]
    yes: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &PermsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn connect(endpoint: &EndpointArgs) -> Result<Endpoint, String> {
    if endpoint.machine.is_some() {
        return Err(
            "perms has no --machine: a machine's permissions are changed on that machine".into(),
        );
    }
    Endpoint::resolve(endpoint)
}

/// The target as API fields: `{"folder": ...}` for an existing directory,
/// else `{"session": id}`.
fn target(conn: &Endpoint, raw: &str) -> Result<(String, Value), String> {
    let path = Path::new(raw);
    if (raw.contains('/') || raw == "." || raw == "..") && path.is_dir() {
        let abs = path.canonicalize().map_err(|e| format!("{raw}: {e}"))?;
        let abs = abs.to_string_lossy().into_owned();
        return Ok((
            format!("folder={}", utf8_percent_encode(&abs, NON_ALPHANUMERIC)),
            json!({ "folder": abs }),
        ));
    }
    let (id, _) = find_session(conn, raw)?;
    Ok((
        format!("session={}", utf8_percent_encode(&id, NON_ALPHANUMERIC)),
        json!({ "session": id }),
    ))
}

fn list(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn print_view(out: &mut dyn Write, view: &Value) {
    let _ = writeln!(out, "folder {}", str_field(view, "folder"));
    if let Some(session) = view.get("session").filter(|s| s.is_object()) {
        let _ = writeln!(
            out,
            "session {} ({}{})",
            str_field(session, "name"),
            str_field(session, "kind"),
            session
                .get("status")
                .and_then(Value::as_str)
                .map(|s| format!(", {s}"))
                .unwrap_or_default()
        );
    }
    let _ = writeln!(
        out,
        "\nClaude Code, highest precedence first (deny > ask > allow across all):"
    );
    for source in list(&view["claude"]["sources"]) {
        let place = source
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| str_field(source, "label").to_owned());
        let state = if source.get("error").is_some() {
            format!("  ERROR {}", str_field(source, "error"))
        } else if source["exists"] == true {
            String::new()
        } else {
            "  (none)".to_owned()
        };
        let _ = writeln!(out, "  {:<12} {place}{state}", str_field(source, "scope"));
        for kind in ["deny", "ask", "allow"] {
            for rule in list(&source[kind]) {
                let rule = rule.as_str().unwrap_or("");
                let over = (kind == "allow")
                    .then(|| {
                        list(&source["overridden"])
                            .into_iter()
                            .find(|o| str_field(o, "rule") == rule)
                    })
                    .flatten()
                    .map(|o| {
                        format!(
                            "  (overridden by {} {} in {})",
                            str_field(o, "decision"),
                            str_field(o, "by"),
                            str_field(o, "scope")
                        )
                    })
                    .unwrap_or_default();
                let _ = writeln!(out, "      {kind:<5} {rule}{over}");
            }
        }
        if let Some(mode) = source.get("default_mode").and_then(Value::as_str) {
            let _ = writeln!(out, "      mode  {mode}");
        }
        for note in list(&source["notes"]) {
            let _ = writeln!(out, "      note: {}", note.as_str().unwrap_or(""));
        }
    }
    for t in list(&view["claude"]["targets"]) {
        if let Some(w) = t.get("warning").and_then(Value::as_str)
            && t["file"] == "local"
        {
            let _ = writeln!(out, "  warning (local file): {w}");
        }
    }
    let codex = &view["codex"];
    let _ = writeln!(out, "\nCodex ({}):", str_field(codex, "home"));
    let _ = writeln!(
        out,
        "  trust_level      {}{}",
        codex
            .get("trust_level")
            .and_then(Value::as_str)
            .unwrap_or("(not set: Codex asks)"),
        codex
            .get("trust_from")
            .and_then(Value::as_str)
            .map(|f| format!("  [projects.\"{f}\"]"))
            .unwrap_or_default()
    );
    for key in ["approval_policy", "sandbox_mode"] {
        let shown = match codex.get(key).and_then(Value::as_array) {
            Some(pair) => format!(
                "{}  from {}",
                pair.first().and_then(Value::as_str).unwrap_or(""),
                pair.get(1).and_then(Value::as_str).unwrap_or("")
            ),
            None => "(Codex's default)".to_owned(),
        };
        let _ = writeln!(out, "  {key:<16} {shown}");
    }
    for file in list(&codex["rules"]) {
        let _ = writeln!(
            out,
            "  rules ({}) {}",
            str_field(file, "scope"),
            str_field(file, "path")
        );
        for r in list(&file["rules"]) {
            let words: Vec<&str> = list(&r["pattern"])
                .iter()
                .filter_map(|w| w.as_str())
                .collect();
            let _ = writeln!(
                out,
                "      {:<9} {}",
                str_field(r, "decision"),
                words.join(" ")
            );
        }
    }
    for note in list(&codex["notes"]) {
        let _ = writeln!(out, "  note: {}", note.as_str().unwrap_or(""));
    }
    if let Some(pending) = view.get("pending").filter(|p| p.is_object()) {
        let _ = writeln!(
            out,
            "\nBlocked on: {} ({})",
            str_field(pending, "summary"),
            str_field(&pending["request"], "source")
        );
        if let Some(d) = pending.get("decision").filter(|d| d.is_object()) {
            let _ = writeln!(out, "  now: {}", str_field(d, "text"));
        }
        let proposal = if pending["claude"].is_object() {
            &pending["claude"]
        } else {
            &pending["codex"]
        };
        for c in list(&proposal["candidates"]) {
            let rule = c
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_else(|| str_field(c, "rule"));
            let flag = if c["risk"]["dangerous"] == true {
                format!("  DANGEROUS: {}", str_field(&c["risk"], "reason"))
            } else {
                String::new()
            };
            let _ = writeln!(out, "  {:<9} {rule}{flag}", str_field(c, "kind"));
        }
        let proposed: Vec<String> = list(&proposal["rules"])
            .iter()
            .map(|r| match r {
                Value::String(s) => s.clone(),
                other => str_field(other, "text").to_owned(),
            })
            .collect();
        if !proposed.is_empty() {
            let _ = writeln!(out, "  proposed: {}", proposed.join("  "));
        }
        for note in list(&proposal["notes"]) {
            let _ = writeln!(out, "  note: {}", note.as_str().unwrap_or(""));
        }
    }
}

fn print_preview(out: &mut dyn Write, preview: &Value) {
    let _ = writeln!(
        out,
        "file: {} ({})",
        str_field(preview, "path"),
        str_field(preview, "file")
    );
    if let Some(w) = preview.get("warning").and_then(Value::as_str) {
        let _ = writeln!(out, "warning: {w}");
    }
    for r in list(&preview["rules"]) {
        if r["risk"]["dangerous"] == true {
            let _ = writeln!(
                out,
                "DANGEROUS {}: {}",
                str_field(r, "rule"),
                str_field(&r["risk"], "reason")
            );
        }
    }
    let _ = write!(out, "{}", str_field(preview, "diff"));
    let sentence = str_field(preview, "sentence");
    if !sentence.is_empty() {
        let _ = writeln!(out, "{sentence}");
    }
    for note in list(&preview["notes"]) {
        let _ = writeln!(out, "note: {}", note.as_str().unwrap_or(""));
    }
}

/// Ask on the terminal (`None`: stdin is not one).
fn ask(err: &mut dyn Write, prompt: &str) -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let _ = write!(err, "{prompt}");
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim().to_owned())
}

/// Preview `body`, then write it once confirmed.
fn preview_then_write(
    conn: &Endpoint,
    mut body: Value,
    yes: bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let preview = conn.blocking_call("POST", "/api/perms/preview", Some(&body))?;
    if !json_out {
        print_preview(out, &preview);
    }
    if list(&preview["added"]).is_empty() {
        if json_out {
            print_json(out, &preview);
        } else {
            let _ = writeln!(out, "nothing to write");
        }
        return Ok(0);
    }
    if let Some(expected) = preview.get("type_to_confirm").and_then(Value::as_str) {
        let Some(typed) = ask(
            err,
            &format!("This is dangerous. Type it exactly to write it:\n  {expected}\n> "),
        ) else {
            return Err(format!(
                "{expected} is dangerous: run this on a terminal and type it to confirm"
            ));
        };
        if typed != expected {
            return Err("not confirmed: what was typed does not match".into());
        }
        body["typed"] = json!(typed);
    } else if !yes {
        match ask(err, "Write it? [y/N] ") {
            Some(a) if a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes") => {}
            Some(_) => return Err("not written".into()),
            None => return Err("not confirmed: re-run with --yes to write it".into()),
        }
    }
    body["confirm"] = json!(true);
    body["base"] = preview["base"].clone();
    let done = conn.blocking_call("POST", "/api/perms/allow", Some(&body))?;
    if json_out {
        print_json(out, &done);
        return Ok(0);
    }
    if done["written"] == true {
        let _ = writeln!(out, "wrote {}", str_field(&done, "path"));
    } else {
        let _ = writeln!(out, "nothing written");
    }
    if let Some(ignore) = done.get("gitignored").and_then(Value::as_str) {
        let _ = writeln!(out, "added it to {ignore}");
    }
    for note in list(&done["notes"]).iter().filter(|n| {
        n.as_str()
            .is_some_and(|n| n.starts_with("not added to .gitignore"))
    }) {
        let _ = writeln!(out, "note: {}", note.as_str().unwrap_or(""));
    }
    match done.get("answered").and_then(Value::as_bool) {
        Some(true) => {
            let _ = writeln!(out, "answered the pending prompt: Yes");
        }
        Some(false) => {
            let _ = writeln!(
                out,
                "the session is still waiting: {}",
                str_field(&done, "answer_note")
            );
        }
        None => {}
    }
    Ok(0)
}

/// For a session target blocked on a prompt: ask the write to answer it
/// (only while it still shows the request the rules are for).
fn answer_pending(conn: &Endpoint, query: &str, body: &mut Value) -> Result<(), String> {
    if body.get("session").is_none() {
        return Ok(());
    }
    let view = conn.blocking_call("GET", &format!("/api/perms?{query}"), None)?;
    if let Some(key) = view["pending"]["request"]["key"].as_str() {
        body["answer"] = json!(true);
        body["request_key"] = json!(key);
    }
    Ok(())
}

fn run(args: &PermsArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    match &args.command {
        PermsCommand::Show(a) => {
            let conn = connect(&a.endpoint)?;
            let (query, _) = target(&conn, &a.target)?;
            let view = conn.blocking_call("GET", &format!("/api/perms?{query}"), None)?;
            if a.json {
                print_json(out, &view);
            } else {
                print_view(out, &view);
            }
            Ok(0)
        }
        PermsCommand::Allow(a) => {
            let conn = connect(&a.endpoint)?;
            let (query, mut body) = target(&conn, &a.target)?;
            if a.agent.as_deref() == Some("codex") {
                let mut patterns = Vec::new();
                for raw in &a.rules {
                    let parts =
                        crate::perms::shell::split(raw).map_err(|e| format!("{raw}: {e}"))?;
                    if parts.len() != 1 {
                        return Err(format!("{raw}: one command prefix per argument"));
                    }
                    let words: Vec<String> =
                        parts[0].words.iter().map(|w| w.text.clone()).collect();
                    patterns.push(words);
                }
                body["patterns"] = json!(patterns);
                body["agent"] = json!("codex");
            } else {
                body["rules"] = json!(a.rules);
                if let Some(agent) = &a.agent {
                    body["agent"] = json!(agent);
                }
            }
            if let Some(file) = &a.file {
                body["file"] = json!(file);
            }
            if a.gitignore {
                body["gitignore"] = json!(true);
            }
            if !a.no_answer {
                answer_pending(&conn, &query, &mut body)?;
            }
            preview_then_write(&conn, body, a.yes, a.json, out, err)
        }
        PermsCommand::Codex(a) => {
            let conn = connect(&a.endpoint)?;
            let (_, mut body) = target(&conn, &a.target)?;
            body["agent"] = json!("codex");
            body["setting"] = json!(a.setting);
            body["value"] = json!(a.value);
            preview_then_write(&conn, body, a.yes, a.json, out, err)
        }
        PermsCommand::Suggest(a) => {
            let conn = connect(&a.endpoint)?;
            let (query, _) = target(&conn, &a.target)?;
            let found = conn.blocking_call(
                "GET",
                &format!("/api/perms/suggest?{query}&days={}", a.days),
                None,
            )?;
            if a.json {
                print_json(out, &found);
                return Ok(0);
            }
            let suggestions = list(&found["suggestions"]);
            if suggestions.is_empty() {
                let _ = writeln!(
                    out,
                    "no repeated permission prompts in {} in the last {} days",
                    str_field(&found, "repo"),
                    a.days
                );
            }
            for s in suggestions {
                let rules: Vec<&str> = list(&s["rules"])
                    .iter()
                    .map(|r| str_field(r, "rule"))
                    .collect();
                let _ = writeln!(
                    out,
                    "{:>4}x  {:<6} {}",
                    s["count"].as_u64().unwrap_or(0),
                    str_field(s, "agent"),
                    rules.join("  ")
                );
                for r in list(&s["rules"]) {
                    let _ = writeln!(
                        out,
                        "         lets the agent {} without asking",
                        str_field(r, "sentence")
                    );
                }
            }
            if let Some(n) = found["skipped_dangerous"].as_u64().filter(|n| *n > 0) {
                let _ = writeln!(out, "({n} more need a rule too broad to suggest)");
            }
            Ok(0)
        }
    }
}
