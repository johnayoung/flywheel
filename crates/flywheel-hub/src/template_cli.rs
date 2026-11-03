//! `fw hub template ls|save|rm`: a workspace's saved ways to start an
//! agent (kind, first prompt, model, permission options, worktree), and
//! the lookup `fw hub new --template` uses.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{query_escape, resolve};
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::{Read, Write};

#[derive(Debug, clap::Args)]
pub(crate) struct TemplateArgs {
    #[command(subcommand)]
    command: TemplateCommand,
}

#[derive(Debug, clap::Subcommand)]
enum TemplateCommand {
    /// List launch templates (every workspace's, or one's).
    Ls(LsArgs),
    /// Save a launch template (--replace overwrites one of that name,
    /// --edit changes only the options given).
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
    /// claude or codex (required unless --edit).
    #[arg(long, value_parser = ["claude", "codex"], required_unless_present = "edit")]
    kind: Option<String>,
    #[command(flatten)]
    launch: LaunchFlags,
    /// Launch it in a new sibling worktree (of a git folder; the task name
    /// is made from the template's name unless `new --worktree` gives one).
    #[arg(long, conflicts_with = "no_worktree")]
    worktree: bool,
    /// With --edit: launch it in its folder, not a new worktree.
    #[arg(long, requires = "edit")]
    no_worktree: bool,
    /// Overwrite the template of this name (every option not given here is
    /// cleared).
    #[arg(long, conflicts_with = "edit")]
    replace: bool,
    /// Change only the options given on the existing template of this
    /// name (an empty value, e.g. --model '', clears that option).
    #[arg(long)]
    edit: bool,
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
        print_json(out, &json!({ "templates": templates }));
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
    let existing = list(&conn, Some(&id))?
        .into_iter()
        .find(|t| str_field(t, "name") == args.name.trim());
    let body = save_body(args, existing.as_ref(), args.launch.prompt_text()?)?;
    let saved = conn.blocking_call(
        "POST",
        &format!("/api/workspaces/{}/templates", query_escape(&id)),
        Some(&body),
    )?;
    let verb = match (&existing, args.edit) {
        (Some(_), true) => "updated",
        (Some(_), false) => "replaced",
        (None, _) => "saved",
    };
    let _ = writeln!(out, "{verb} template {:?}", str_field(&saved, "name"));
    Ok(())
}

/// The request `template save` sends, given the template of that name
/// the workspace has now (if any).
fn save_body(
    args: &SaveArgs,
    existing: Option<&Value>,
    prompt: Option<String>,
) -> Result<Value, String> {
    let name = args.name.trim();
    let mut body = if args.edit {
        let existing = existing.ok_or_else(|| {
            format!("no template named {name:?} to edit (save it without --edit first)")
        })?;
        let mut body = json!({ "name": name });
        for key in [
            "kind",
            "prompt",
            "model",
            "permission_mode",
            "approval",
            "sandbox",
            "worktree",
        ] {
            if let Some(value) = existing.get(key).filter(|v| !v.is_null()) {
                body[key] = value.clone();
            }
        }
        if let Some(kind) = &args.kind {
            body["kind"] = json!(kind);
        }
        body["replace"] = json!(true);
        body
    } else {
        if existing.is_some() && !args.replace {
            return Err(format!(
                "a template named {name:?} already exists: --replace overwrites it (clearing every option not given), --edit changes only the options given"
            ));
        }
        json!({ "name": name, "kind": args.kind, "replace": args.replace })
    };
    if args.worktree {
        body["worktree"] = json!(true);
    } else if args.no_worktree || !args.edit {
        body["worktree"] = json!(false);
    }
    args.launch.apply(&mut body, prompt);
    Ok(body)
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

    fn save_args(argv: &[&str]) -> SaveArgs {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            save: SaveArgs,
        }
        let mut full = vec!["save"];
        full.extend_from_slice(argv);
        <Cli as clap::Parser>::try_parse_from(full).unwrap().save
    }

    #[test]
    fn saving_over_a_template_needs_replace_and_edit_keeps_the_rest() {
        let existing = json!({"name": "demo", "kind": "claude", "prompt": "Say hi",
            "model": "haiku", "permission_mode": "plan", "worktree": true,
            "workspace_id": "w1", "updated_at": "2026-11-02T00:00:00Z"});
        // A plain save over it is refused, naming both ways out.
        let args = save_args(&["W", " demo ", "--kind", "claude"]);
        let refused = save_body(&args, Some(&existing), None).unwrap_err();
        assert!(
            refused.contains("--replace") && refused.contains("--edit"),
            "{refused}"
        );
        // --replace: exactly what was given.
        let args = save_args(&["W", "demo", "--kind", "claude", "--replace"]);
        assert_eq!(
            save_body(&args, Some(&existing), None).unwrap(),
            json!({"name": "demo", "kind": "claude", "replace": true, "worktree": false})
        );
        // --edit: one option changed (and one cleared), the rest kept.
        let args = save_args(&[
            "W",
            "demo",
            "--edit",
            "--model",
            "opus",
            "--permission-mode",
            "",
        ]);
        assert_eq!(
            save_body(&args, Some(&existing), None).unwrap(),
            json!({"name": "demo", "kind": "claude", "prompt": "Say hi", "model": "opus",
                "permission_mode": "", "worktree": true, "replace": true})
        );
        let args = save_args(&["W", "demo", "--edit", "--no-worktree"]);
        assert_eq!(
            save_body(&args, Some(&existing), None).unwrap()["worktree"],
            false
        );
        assert!(
            save_body(&args, None, None)
                .unwrap_err()
                .contains("no template")
        );
        // New: saved as given.
        let args = save_args(&["W", "new", "--kind", "codex", "--worktree"]);
        assert_eq!(
            save_body(&args, None, Some("go".into())).unwrap(),
            json!({"name": "new", "kind": "codex", "replace": false, "worktree": true, "prompt": "go"})
        );
        assert!(
            <SaveArgs as clap::Args>::augment_args(clap::Command::new("x"))
                .try_get_matches_from(["x", "W", "n"])
                .is_err(),
            "--kind is needed without --edit"
        );
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
