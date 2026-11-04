//! Agent permission rules from the hub: fewer interruptions without
//! handing agents more than they asked for.
//!
//! * **View**: per folder (or session) every Claude Code settings source
//!   in precedence order (managed > command line > local > project >
//!   user) with the rules each holds, and Codex's approval policy, sandbox
//!   mode, project trust, and prefix rules ([`view`]).
//! * **Always allow**: for a blocked agent's pending request (the
//!   `PermissionRequest` hook payload, else its permission dialog read off
//!   the screen) the narrowest rule that covers it ([`synth`]); a preview
//!   names the file, shows the diff and what the rule lets the agent do;
//!   the write is confirm-gated, atomic, and format-preserving
//!   ([`edit`]), then the pending prompt is answered.
//! * **Never auto-proposed**: rules covering every command, `rm`, `sudo`,
//!   `curl`, interpreters, `git push`, pipes into a shell, edits outside
//!   the repo or of `.claude/`, ... ([`rule::risk`]) are written only after
//!   the rule is typed back.
//! * **Suggestions**: the requests a repo blocked on most over the last
//!   days ([`journal`]).
//!
//! Rules are agent-controlling config: writes take the hub's token or a
//! login (peers are refused), are logged to the hub log, and land in the
//! activity journal (`perms.allowed`).

pub mod claude;
pub mod codex;
pub mod edit;
pub mod journal;
pub mod rule;
pub mod shell;
pub mod synth;

mod hub_side;
#[cfg(test)]
mod tests;

pub use hub_side::{AllowBody, Where};

use crate::agent::SessionKind;
use rule::{Anchors, Rule, Target};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use synth::Request;

/// Where the agents' config lives on this machine.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: PathBuf,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex_home: PathBuf,
    /// The managed settings directory (`/etc/claude-code` on Linux and
    /// WSL, `/Library/Application Support/ClaudeCode` on macOS).
    pub managed_dir: PathBuf,
}

/// The managed settings directory Claude Code reads on this OS.
pub fn managed_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    }
}

impl Env {
    pub fn new(home: PathBuf, claude_dir: Option<PathBuf>, codex_home: Option<PathBuf>) -> Self {
        Self {
            claude_dir: claude_dir.unwrap_or_else(|| home.join(".claude")),
            codex_home: codex_home.unwrap_or_else(|| home.join(".codex")),
            managed_dir: managed_dir(),
            home,
        }
    }
}

/// A folder the rules are for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The session's directory (canonical): its primary working directory.
    pub dir: PathBuf,
    /// Its git top (a worktree's own), if in git.
    pub top: Option<PathBuf>,
    /// The main checkout, if in git (worktrees share its local settings).
    pub main: Option<PathBuf>,
}

impl Folder {
    pub fn resolve(dir: &Path) -> Result<Self, String> {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        let top = crate::review::runner::run(&crate::review::runner::Spec {
            program: "git",
            dir: &dir,
            args: vec!["rev-parse".into(), "--show-toplevel".into()],
            stdin: None,
            timeout: std::time::Duration::from_secs(10),
            limit: 64 * 1024,
        })
        .ok()
        .filter(|o| o.success())
        .map(|o| PathBuf::from(o.stdout_text().trim()))
        .filter(|p| p.is_absolute())
        .map(|p| p.canonicalize().unwrap_or(p));
        let main = top
            .as_ref()
            .and_then(|_| crate::git::main_worktree(&dir).ok())
            .filter(|(_, bare)| !bare)
            .map(|(m, _)| m);
        Ok(Self { dir, top, main })
    }

    /// Where Claude Code keeps this folder's local settings: the main
    /// checkout's root, except outside git or when that root is home.
    pub fn local_root(&self, env: &Env) -> PathBuf {
        match &self.main {
            Some(main) if *main != env.home => main.clone(),
            _ => self.dir.clone(),
        }
    }

    /// The repo a rule must stay inside (the git top, else the folder).
    pub fn repo(&self) -> &Path {
        self.top.as_deref().unwrap_or(&self.dir)
    }

    /// The key requests are journaled under: the main checkout (worktrees
    /// roll up to their repo), else the folder.
    pub fn key(&self) -> String {
        self.main
            .as_deref()
            .unwrap_or(&self.dir)
            .to_string_lossy()
            .into_owned()
    }

    /// Anchors for a rule written to `file`.
    pub fn anchors(&self, env: &Env, file: claude::File) -> Anchors {
        Anchors {
            cwd: self.dir.clone(),
            slash: match file {
                claude::File::User => env.claude_dir.clone(),
                _ => self.dir.clone(),
            },
            home: env.home.clone(),
        }
    }
}

/// The hub's permissions state: each blocked session's pending request
/// (from its hook) and the request journal.
#[derive(Debug)]
pub struct Runtime {
    env: Env,
    pending: Mutex<HashMap<String, Request>>,
    /// The request each session was last journaled for (one entry per
    /// blocked prompt).
    journaled: Mutex<HashMap<String, String>>,
    journal: journal::Journal,
    /// Held from reading a file to writing it: two writes at once must not
    /// lose one's rule.
    writes: Mutex<()>,
}

impl Runtime {
    pub fn new(state_dir: &Path, env: Env) -> Self {
        Self {
            env,
            pending: Mutex::default(),
            journaled: Mutex::default(),
            journal: journal::Journal::new(state_dir),
            writes: Mutex::default(),
        }
    }

    /// Serializes read-plan-write of a settings or rules file.
    pub fn write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    pub fn journal(&self) -> &journal::Journal {
        &self.journal
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Request>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_pending(&self, session: &str, request: Request) {
        self.pending().insert(session.to_owned(), request);
    }

    pub fn clear_pending(&self, session: &str) {
        self.pending().remove(session);
    }

    pub fn pending_for(&self, session: &str) -> Option<Request> {
        self.pending().get(session).cloned()
    }

    /// Record `request` (once per prompt) with its default proposal.
    fn journal_request(&self, session: &str, folder: &Folder, request: &Request) {
        {
            let mut journaled = self
                .journaled
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if journaled.get(session) == Some(&request.key) {
                return;
            }
            journaled.insert(session.to_owned(), request.key.clone());
        }
        let rules: Vec<serde_json::Value> = match request.agent {
            "codex" => synth::propose_codex(
                request,
                &folder.anchors(&self.env, claude::File::Local),
                folder.repo(),
            )
            .rules
            .into_iter()
            .map(|c| serde_json::json!(c.pattern))
            .collect(),
            _ => synth::propose_claude(
                request,
                &folder.anchors(&self.env, claude::File::Local),
                folder.repo(),
                synth::Anchor::Project,
            )
            .rules
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
        };
        self.journal.append(&journal::Entry {
            at: chrono::Utc::now().timestamp(),
            folder: folder.key(),
            agent: request.agent.to_owned(),
            tool: request.tool.clone(),
            rules,
            session: session.to_owned(),
        });
    }
}

// --- the view ----------------------------------------------------------------

/// How Claude Code's merged rules decide a request.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    /// `deny`, `ask`, `allow`, or `none` (no rule: the mode decides).
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub text: String,
}

/// Evaluate `target` against `sources` in Claude Code's order: deny, then
/// ask, then allow, from any source.
pub fn decide(
    sources: &[claude::Source],
    target: &Target,
    folder: &Folder,
    env: &Env,
    managed_only: bool,
) -> Decision {
    for (outcome, pick) in [
        (
            "deny",
            (|s: &claude::Source| &s.deny) as fn(&claude::Source) -> &Vec<String>,
        ),
        ("ask", |s: &claude::Source| &s.ask),
        ("allow", |s: &claude::Source| &s.allow),
    ] {
        for source in sources {
            if managed_only && source.scope != "managed" {
                continue;
            }
            let anchors = folder.anchors(
                env,
                if source.scope == "user" {
                    claude::File::User
                } else {
                    claude::File::Local
                },
            );
            for raw in pick(source) {
                let Some(rule) = Rule::parse_any(raw) else {
                    continue;
                };
                if rule::covers(&rule, target, &anchors) {
                    let place = source
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| source.scope.to_owned());
                    let text = match outcome {
                        "deny" => {
                            format!("denied by {raw} in {place}: no allow rule can override it")
                        }
                        "ask" => format!(
                            "the ask rule {raw} in {place} prompts even when an allow rule matches"
                        ),
                        _ => format!(
                            "already allowed by {raw} in {place} (Claude Code may hold it until the folder is trusted)"
                        ),
                    };
                    return Decision {
                        outcome,
                        rule: Some(raw.clone()),
                        path: source.path.clone(),
                        text,
                    };
                }
            }
        }
    }
    Decision {
        outcome: "none",
        rule: None,
        path: None,
        text: "no rule matches: the permission mode decides (Claude Code asks)".into(),
    }
}

/// A session, as the view names it.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub kind: SessionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::status::AgentStatus>,
}

/// A pending request with what the hub offers for it.
#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub request: Request,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<synth::Proposal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<synth::CodexProposal>,
    /// Claude Code: how the rules in force decide it now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

/// Claude Code's side of the view.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeView {
    pub sources: Vec<claude::Source>,
    pub managed_only: bool,
    /// The files the hub can write (local is the default; user only when
    /// asked for by name).
    pub targets: Vec<claude::Target>,
}

/// `fw hub perms show` / `GET /api/perms`.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub folder: PathBuf,
    pub repo: PathBuf,
    pub local_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub claude: ClaudeView,
    pub codex: codex::View,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// The pending request with its proposals and decision.
pub fn pending_view(
    request: Request,
    folder: &Folder,
    env: &Env,
    sources: &[claude::Source],
    managed_only: bool,
) -> Pending {
    let anchors = folder.anchors(env, claude::File::Local);
    let summary = request.summary();
    match request.agent {
        "codex" => Pending {
            codex: Some(synth::propose_codex(&request, &anchors, folder.repo())),
            claude: None,
            decision: None,
            summary,
            request,
        },
        _ => {
            let proposal =
                synth::propose_claude(&request, &anchors, folder.repo(), synth::Anchor::Project);
            let decision = decide(sources, &request.target, folder, env, managed_only);
            Pending {
                claude: Some(proposal),
                codex: None,
                decision: Some(decision),
                summary,
                request,
            }
        }
    }
}

// --- preview and write ---------------------------------------------------------

/// One rule as a preview shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RuleView {
    pub rule: String,
    pub risk: rule::Risk,
    pub sentence: String,
}

/// What a write would do (`POST /api/perms/preview`), and what it did
/// (`POST /api/perms/allow`, with `written`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Preview {
    pub agent: &'static str,
    /// `local`, `project`, `user` (Claude), `project`/`user` rules or
    /// config (Codex).
    pub file: String,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub rules: Vec<RuleView>,
    /// What is new (rules already there are not written again).
    pub added: Vec<String>,
    pub diff: String,
    /// "This lets the agent ... without asking, ...".
    pub sentence: String,
    /// The digest of the file the diff was made from: `allow` refuses if
    /// the file changed since.
    pub base: String,
    /// The local settings file is neither ignored nor tracked: `allow`
    /// with `gitignore: true` also adds it to the checkout's .gitignore.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub can_gitignore: bool,
    /// After a write: the .gitignore line was added there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gitignored: Option<PathBuf>,
    /// A dangerous rule or setting: the exact text to type to confirm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_to_confirm: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// After a write: it happened.
    pub written: bool,
    /// After a write: the pending prompt was answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_note: Option<String>,
}

/// "This lets the agent X, Y, and Z without asking, <reach>."
pub fn sentence(parts: &[String], reach: &str) -> String {
    let list = match parts {
        [] => return String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    };
    format!("This lets the agent {list} without asking, {reach}.")
}
