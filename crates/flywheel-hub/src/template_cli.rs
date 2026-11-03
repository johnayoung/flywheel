//! `fw hub template ls|save|rm`: a workspace's saved ways to start an
//! agent (kind, first prompt, model, permission options, worktree), and
//! the lookup `fw hub new --template` uses.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{query_escape, resolve};
use serde_json::{Value, json};
use std::io::{Read, Write};

fn str_field<'a>(item: &'a Value, field: &str) -> &'a str {
    item.get(field).and_then(Value::as_str).unwrap_or("")
}

fn fail(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "fw hub: {message}");
    1
}

#[derive(Debug, clap::Args)]
pub(crate) struct TemplateArgs {
    #[command(subcommand)]
    command: TemplateCommand,
}

#[derive(Debug, clap::Subcommand)]
enum TemplateCommand {
    /// List launch templates (every workspace's, or one's).
    Ls(LsArgs),
    /// Save a launch template (replacing the workspace's one of that name).
    Save(SaveArgs),
    /// Remove a launch template.
    Rm(RmArgs),
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Workspace id, unique id prefix, or name (default: all).
    workspace: Option<String>,
    /// Print JSON: {"templates":[{workspace_id, name, kind, ...}]}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// The launch options shared by `template save` and `new`.
#[derive(Debug, Clone, Default, clap::Args)]
pub(crate) struct LaunchFlags {
    /// The agent's first user turn (`-` reads it from stdin).
    #[arg(long, value_name = "TEXT")]
    pub prompt: Option<String>,
    /// Model: claude --model (opus, sonnet, haiku, or a full id) / codex -m.
    #[arg(long)]
    pub model: Option<String>,
    /// Claude: --permission-mode (manual, acceptEdits, plan, auto,
    /// dontAsk, bypassPermissions -- the last skips every check).
    #[arg(long, value_name = "MODE")]
    pub permission_mode: Option<String>,
    /// Codex: --ask-for-approval (untrusted, on-failure, on-request, never).
    #[arg(long, value_name = "POLICY")]
    pub approval: Option<String>,
    /// Codex: --sandbox (read-only, workspace-write, danger-full-access).
    #[arg(long, value_name = "POLICY")]
    pub sandbox: Option<String>,
}

impl LaunchFlags {
    /// The prompt, read from stdin for `-`.
    pub fn prompt_text(&self) -> Result<Option<String>, String> {
        match self.prompt.as_deref() {
            Some("-") => {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| format!("cannot read the prompt from stdin: {e}"))?;
                Ok(Some(text))
            }
            other => Ok(other.map(str::to_owned)),
        }
    }

    /// Write the options that are set over `body`'s.
    pub fn apply(&self, body: &mut Value, prompt: Option<String>) {
        if let Some(prompt) = prompt {
            body["prompt"] = json!(prompt);
        }
        for (key, value) in [
            ("model", &self.model),
            ("permission_mode", &self.permission_mode),
            ("approval", &self.approval),
            ("sandbox", &self.sandbox),
        ] {
            if let Some(value) = value {
                body[key] = json!(value);
            }
        }
    }
}

#[derive(Debug, clap::Args)]
struct SaveArgs {
    /// Workspace id, unique id prefix, or name.
    workspace: String,
    /// Template name (unique in the workspace).
    name: String,
    /// claude, codex, or shell.
    #[arg(long, value_parser = ["claude", "codex", "shell"])]
    kind: String,
    #[command(flatten)]
    launch: LaunchFlags,
    /// Launch it in a new sibling worktree (of a git folder; the task name
    /// is made from the template's name unless `new --worktree` gives one).
    #[arg(long)]
    worktree: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct RmArgs {
    /// Workspace id, unique id prefix, or name.
    workspace: String,
    /// Template name.
    name: String,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &TemplateArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        TemplateCommand::Ls(args) => ls(args, out),
        TemplateCommand::Save(args) => save(args, out),
        TemplateCommand::Rm(args) => rm(args, out),
    };
    match result {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

/// The id of the workspace `query` names.
fn workspace_id(conn: &Endpoint, query: &str) -> Result<String, String> {
    let workspaces = conn.blocking_workspaces()?;
    Ok(str_field(resolve(&workspaces, query, "workspace")?, "id").to_owned())
}

/// Templates (of one workspace, or all).
pub(crate) fn list(conn: &Endpoint, workspace_id: Option<&str>) -> Result<Vec<Value>, String> {
    let path = match workspace_id {
        Some(id) => format!("/api/templates?workspace_id={}", query_escape(id)),
        None => "/api/templates".to_owned(),
    };
    let listed = conn.blocking_call("GET", &path, None)?;
    match listed.get("templates") {
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err("the hub sent no template list (is it older than templates?)".into()),
    }
}

/// Workspace `workspace_id`'s template named exactly `name`.
pub(crate) fn find(conn: &Endpoint, workspace_id: &str, name: &str) -> Result<Value, String> {
    let templates = list(conn, Some(workspace_id))?;
    templates
        .into_iter()
        .find(|t| str_field(t, "name") == name.trim())
        .ok_or_else(|| {
            format!("no template named {name:?} in that workspace (fw hub template ls lists them)")
        })
}

/// A worktree task name for a launch from template `name`: its name made
/// branch-safe, plus a short random suffix.
pub(crate) fn task_from(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..4];
    if slug.is_empty() {
        format!("agent-{suffix}")
    } else {
        format!("{slug}-{suffix}")
    }
}

/// One line of the prompt for listings.
fn prompt_preview(template: &Value) -> String {
    let prompt = str_field(template, "prompt");
    let first = prompt.lines().next().unwrap_or("").trim();
    let mut shown: String = first.chars().take(60).collect();
    if first.chars().count() > 60 || prompt.lines().count() > 1 {
        shown.push_str("...");
    }
    shown
}

/// `model, mode` options of a template for listings.
fn options_label(template: &Value) -> String {
    let parts: Vec<String> = [
        ("model", ""),
        ("permission_mode", ""),
        ("approval", "approval "),
        ("sandbox", "sandbox "),
    ]
    .iter()
    .filter_map(|(key, tag)| {
        template
            .get(*key)
            .and_then(Value::as_str)
            .map(|v| format!("{tag}{v}"))
    })
    .collect();
    if parts.is_empty() {
        "defaults".into()
    } else {
        parts.join(", ")
    }
}

fn ls(args: &LsArgs, out: &mut dyn Write) -> Result<(), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let workspaces = conn.blocking_workspaces()?;
    let only = args
        .workspace
        .as_deref()
        .map(|q| resolve(&workspaces, q, "workspace").map(|w| str_field(w, "id").to_owned()))
        .transpose()?;
    let templates = list(&conn, only.as_deref())?;
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&json!({ "templates": templates })).unwrap_or_default()
        );
        return Ok(());
    }
    if templates.is_empty() {
        let _ = writeln!(
            out,
            "no templates (save one with `fw hub template save WORKSPACE NAME --kind claude --prompt ...` or from the new-session form)"
        );
        return Ok(());
    }
    let workspace_name = |id: &str| {
        workspaces
            .iter()
            .find(|w| str_field(w, "id") == id)
            .map_or(id, |w| str_field(w, "name"))
            .to_owned()
    };
    let rows: Vec<[String; 5]> = templates
        .iter()
        .map(|t| {
            [
                workspace_name(str_field(t, "workspace_id")),
                str_field(t, "name").to_owned(),
                format!(
                    "{}{}",
                    str_field(t, "kind"),
                    if t.get("worktree").and_then(Value::as_bool) == Some(true) {
                        " +worktree"
                    } else {
                        ""
                    }
                ),
                options_label(t),
                prompt_preview(t),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..4)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let _ = writeln!(
            out,
            "{:w0$}  {:w1$}  {:w2$}  {:w3$}  {}",
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
    }
    Ok(())
}

fn save(args: &SaveArgs, out: &mut dyn Write) -> Result<(), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let id = workspace_id(&conn, &args.workspace)?;
    let mut body = json!({ "name": args.name, "kind": args.kind, "worktree": args.worktree });
    args.launch.apply(&mut body, args.launch.prompt_text()?);
    let saved = conn.blocking_call(
        "POST",
        &format!("/api/workspaces/{}/templates", query_escape(&id)),
        Some(&body),
    )?;
    let _ = writeln!(out, "saved template {:?}", str_field(&saved, "name"));
    Ok(())
}

fn rm(args: &RmArgs, out: &mut dyn Write) -> Result<(), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let id = workspace_id(&conn, &args.workspace)?;
    conn.blocking_call(
        "DELETE",
        &format!(
            "/api/workspaces/{}/templates/{}",
            query_escape(&id),
            query_escape(args.name.trim())
        ),
        None,
    )?;
    let _ = writeln!(out, "removed template {:?}", args.name.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_names_from_templates_are_branch_safe() {
        let task = task_from("Fix the flaky tests!");
        assert!(task.starts_with("fix-the-flaky-tests-"), "{task}");
        assert_eq!(task.len(), "fix-the-flaky-tests-".len() + 4);
        assert!(crate::git::validate_task(&task).is_ok());
        assert!(task_from("***").starts_with("agent-"));
        assert!(crate::git::validate_task(&task_from(&"x".repeat(200))).is_ok());
    }

    #[test]
    fn launch_flags_override_template_fields() {
        let mut body = json!({ "model": "sonnet", "permission_mode": "plan" });
        let flags = LaunchFlags {
            model: Some("opus".into()),
            ..LaunchFlags::default()
        };
        flags.apply(&mut body, Some("go".into()));
        assert_eq!(
            body,
            json!({ "model": "opus", "permission_mode": "plan", "prompt": "go" })
        );
        let template =
            json!({ "prompt": "line one\nline two", "model": "opus", "approval": "never" });
        assert_eq!(prompt_preview(&template), "line one...");
        assert_eq!(options_label(&template), "opus, approval never");
        assert_eq!(options_label(&json!({})), "defaults");
    }
}
