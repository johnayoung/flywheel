//! The hub's side of permissions: resolving which folder a request is
//! for, tracking each blocked session's pending request, and the view,
//! preview, write, and suggestion operations the API and CLI use.

use super::{
    Decision, Folder, Preview, RuleView, SessionInfo, View, claude, codex, edit, pending_view,
    rule, sentence, synth,
};
use crate::agent::SessionKind;
use crate::hub::{Hub, HubError};
use crate::pty::WriteOutcome;
use crate::state::{SessionRecord, StateError};
use crate::status::{AgentStatus, Signal};
use crate::vt::TextSource;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

/// Which folder (or session) a request is about.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Where {
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
}

/// `POST /api/perms/preview` and `/api/perms/allow`: Claude Code rules
/// (`rules`), Codex prefix rules (`patterns`), or one Codex setting
/// (`setting` + `value`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowBody {
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    /// `claude` or `codex` (default: the session's kind, else claude).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<Vec<String>>,
    #[serde(default)]
    pub setting: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    /// `local` (default), `project`, or `user`.
    #[serde(default)]
    pub file: Option<String>,
    /// The preview's `base`: refused if the file changed since.
    #[serde(default)]
    pub base: Option<String>,
    /// Required to write.
    #[serde(default)]
    pub confirm: bool,
    /// A dangerous rule or setting, typed back exactly.
    #[serde(default)]
    pub typed: Option<String>,
    /// Also add the local settings file to the checkout's .gitignore
    /// (when the preview offers `can_gitignore`).
    #[serde(default)]
    pub gitignore: bool,
    /// Answer the session's pending prompt with "Yes" after writing.
    #[serde(default)]
    pub answer: bool,
    /// The request the rules were proposed for (`pending.request.key`):
    /// the prompt is answered only while it still shows that request.
    #[serde(default)]
    pub request_key: Option<String>,
}

/// Most rules one write takes.
const MAX_RULES: usize = 8;
/// Longest look-back for suggestions.
pub const MAX_SUGGEST_DAYS: i64 = super::journal::KEEP_DAYS;

fn invalid(message: impl Into<String>) -> HubError {
    StateError::Invalid(message.into()).into()
}

fn conflict(message: impl Into<String>) -> HubError {
    StateError::Conflict(message.into()).into()
}

/// What a write is about to do: the preview and the text to write.
struct Plan {
    preview: Preview,
    new_text: String,
    /// What the activity row and plugins hear about.
    tools: Vec<String>,
}

impl Hub {
    /// The folder (and session) a request names. A folder must be inside
    /// a workspace folder or a session's directory.
    fn perms_resolve(&self, at: &Where) -> Result<(Folder, Option<SessionRecord>), HubError> {
        match (
            at.folder.as_deref().map(str::trim),
            at.session.as_deref().map(str::trim),
        ) {
            (Some(_), Some(_)) => Err(invalid("name a folder or a session, not both")),
            (None, None) | (Some(""), None) | (None, Some("")) => {
                Err(invalid("name a folder or a session"))
            }
            (None, Some(id)) => {
                let record = self.store().session(id)?.clone();
                let folder = Folder::resolve(&record.cwd).map_err(invalid)?;
                Ok((folder, Some(record)))
            }
            (Some(raw), None) => {
                let path = Path::new(raw);
                if !path.is_absolute() {
                    return Err(invalid(format!("{raw}: give an absolute path")));
                }
                let folder = Folder::resolve(path).map_err(invalid)?;
                let known = self
                    .workspaces()
                    .iter()
                    .flat_map(|w| w.folders.iter().map(|f| f.path.clone()))
                    .chain(self.sessions(None).into_iter().map(|s| s.record.cwd))
                    .any(|root| {
                        let root = root.canonicalize().unwrap_or(root);
                        folder.dir.starts_with(&root)
                    });
                if !known {
                    return Err(invalid(format!(
                        "{} is not in any workspace folder or session directory: make a workspace for it (`fw hub new <new-name> --folder {} --create`) or add it to an existing one in the web UI",
                        folder.dir.display(),
                        folder.dir.display()
                    )));
                }
                Ok((folder, None))
            }
        }
    }

    /// The request a blocked session waits on: its hook's, else read off
    /// its permission dialog. `None` when it is not blocked.
    pub fn perms_request(&self, record: &SessionRecord, folder: &Folder) -> Option<synth::Request> {
        if !record.kind.is_agent() {
            return None;
        }
        let status = self.agent_status(&record.id)?;
        if status.status != AgentStatus::Blocked {
            return None;
        }
        if record.kind == SessionKind::Claude
            && let Some(request) = self.perms().pending_for(&record.id)
        {
            return Some(request);
        }
        let rows = self
            .live_session(&record.id)?
            .screen_text(TextSource::Visible)
            .lines;
        match record.kind {
            SessionKind::Claude => synth::from_claude_screen(&rows, &folder.dir),
            SessionKind::Codex => synth::from_codex_screen(&rows, &folder.dir),
            SessionKind::Shell => None,
        }
    }

    /// A Claude Code hook report: a `PermissionRequest` names the pending
    /// request (and is journaled); a turn moving on clears it.
    pub(crate) fn perms_note_report(
        &self,
        session: &str,
        event: &str,
        signal: Option<Signal>,
        payload: &Value,
    ) {
        if event == "PermissionRequest" {
            let Ok(record) = self.store().session(session).cloned() else {
                return;
            };
            // Cheap here (the hook waits on this report); the journal
            // needs the repo (git) and is written off this thread.
            let cwd = record.cwd.canonicalize().unwrap_or(record.cwd);
            match synth::from_hook(payload, &cwd) {
                Some(request) => {
                    self.perms().set_pending(session, request.clone());
                    let runtime = std::sync::Arc::clone(self.perms());
                    let session = session.to_owned();
                    let spawned = std::thread::Builder::new()
                        .name("fw-hub-perms-journal".into())
                        .spawn(move || {
                            if let Ok(folder) = Folder::resolve(&cwd) {
                                runtime.journal_request(&session, &folder, &request);
                            }
                        });
                    if let Err(e) = spawned {
                        eprintln!("fw hub: cannot journal a permission request: {e}");
                    }
                }
                None => self.perms().clear_pending(session),
            }
            return;
        }
        if matches!(
            signal,
            Some(Signal::Working | Signal::Done | Signal::Ready | Signal::Ended)
        ) {
            self.perms().clear_pending(session);
        }
    }

    /// A session just became blocked: journal what its screen asks for
    /// (Codex, or Claude without a hook report).
    pub(crate) fn perms_note_blocked(&self, id: &str) {
        let Ok(record) = self.store().session(id).cloned() else {
            return;
        };
        if !record.kind.is_agent()
            || (record.kind == SessionKind::Claude && self.perms().pending_for(id).is_some())
        {
            return;
        }
        let Ok(folder) = Folder::resolve(&record.cwd) else {
            return;
        };
        if let Some(request) = self.perms_request(&record, &folder) {
            self.perms().journal_request(id, &folder, &request);
        }
    }

    /// `GET /api/perms`: every source for the folder, and the pending
    /// request with its proposal when the session is blocked.
    pub fn perms_view(&self, at: &Where) -> Result<View, HubError> {
        let (folder, record) = self.perms_resolve(at)?;
        let env = self.perms().env().clone();
        let launch = record
            .as_ref()
            .and_then(|r| r.launch.clone())
            .unwrap_or_default();
        let is_claude_session = record
            .as_ref()
            .is_some_and(|r| r.kind == SessionKind::Claude);
        let (mut sources, managed_only) = claude::sources(
            &folder,
            &env,
            is_claude_session,
            launch.permission_mode.as_deref(),
        );
        claude::mark_overridden(&mut sources, &folder.anchors(&env, claude::File::Local));
        let codex_launch = record
            .as_ref()
            .filter(|r| r.kind == SessionKind::Codex)
            .map(|_| (launch.approval.as_deref(), launch.sandbox.as_deref()));
        let codex = codex::view(&folder, &env, codex_launch);
        let pending = record
            .as_ref()
            .and_then(|r| self.perms_request(r, &folder))
            .map(|request| pending_view(request, &folder, &env, &sources, managed_only));
        let targets = [
            claude::File::Local,
            claude::File::Project,
            claude::File::User,
        ]
        .into_iter()
        .map(|f| claude::target(f, &folder, &env))
        .collect();
        Ok(View {
            local_root: folder.local_root(&env),
            repo: folder.repo().to_path_buf(),
            session: record.as_ref().map(|r| SessionInfo {
                id: r.id.clone(),
                name: r.name.clone(),
                kind: r.kind,
                status: self.agent_status(&r.id).map(|s| s.status),
            }),
            folder: folder.dir.clone(),
            claude: super::ClaudeView {
                sources,
                managed_only,
                targets,
            },
            codex,
            pending,
        })
    }

    fn perms_agent(
        body: &AllowBody,
        record: Option<&SessionRecord>,
    ) -> Result<&'static str, HubError> {
        match body.agent.as_deref().map(str::trim) {
            Some("claude") => Ok("claude"),
            Some("codex") => Ok("codex"),
            Some(other) => Err(invalid(format!("unknown agent {other:?}: claude or codex"))),
            None if !body.patterns.is_empty() || body.setting.is_some() => Ok("codex"),
            None => Ok(match record.map(|r| r.kind) {
                Some(SessionKind::Codex) => "codex",
                _ => "claude",
            }),
        }
    }

    /// Work out a write without doing it.
    fn perms_plan(
        &self,
        body: &AllowBody,
    ) -> Result<(Plan, Folder, Option<SessionRecord>), HubError> {
        let at = Where {
            folder: body.folder.clone(),
            session: body.session.clone(),
        };
        let (folder, record) = self.perms_resolve(&at)?;
        let env = self.perms().env().clone();
        let agent = Self::perms_agent(body, record.as_ref())?;
        let given = [
            !body.rules.is_empty(),
            !body.patterns.is_empty(),
            body.setting.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if given != 1 {
            return Err(invalid(
                "give exactly one of rules (Claude), patterns (Codex rules), or setting + value (Codex)",
            ));
        }
        let plan = if !body.rules.is_empty() {
            if agent != "claude" {
                return Err(invalid(
                    "rules are Claude Code rules; Codex takes patterns or a setting",
                ));
            }
            self.plan_claude(body, &folder, &env, record.as_ref())?
        } else if !body.patterns.is_empty() {
            self.plan_codex_rules(body, &folder, &env)?
        } else {
            Self::plan_codex_setting(body, &folder, &env)?
        };
        Ok((plan, folder, record))
    }

    fn plan_claude(
        &self,
        body: &AllowBody,
        folder: &Folder,
        env: &super::Env,
        record: Option<&SessionRecord>,
    ) -> Result<Plan, HubError> {
        if body.rules.len() > MAX_RULES {
            return Err(invalid(format!("at most {MAX_RULES} rules at once")));
        }
        let file = claude::File::parse(body.file.as_deref().unwrap_or("local")).map_err(invalid)?;
        if file == claude::File::User {
            env.user_layer(false).map_err(invalid)?;
        }
        let anchors = folder.anchors(env, file);
        let mut rules = Vec::new();
        let mut views = Vec::new();
        let mut tools = Vec::new();
        for raw in &body.rules {
            let parsed = rule::Rule::parse(raw).map_err(invalid)?;
            let text = parsed.to_string();
            views.push(RuleView {
                risk: rule::risk(&parsed, &anchors, folder.repo()),
                sentence: rule::describe(&parsed, &anchors),
                rule: text.clone(),
            });
            if !tools.contains(&parsed.tool) {
                tools.push(parsed.tool.clone());
            }
            rules.push(text);
        }
        let dangerous: Vec<&RuleView> = views.iter().filter(|v| v.risk.dangerous).collect();
        if dangerous.len() > 1 {
            return Err(invalid(
                "write dangerous rules one at a time (each is typed to confirm)",
            ));
        }
        let target = claude::target(file, folder, env);
        let (old, new_text, added) = claude::plan(&target.path, &rules).map_err(invalid)?;
        let mut notes = Vec::new();
        if added.is_empty() {
            notes.push("every rule is already in this file".to_owned());
        }
        let (all_sources, _) = claude::sources(folder, env, false, None);
        for raw in &added {
            if let Some(o) = claude::overriding(&all_sources, raw, &anchors) {
                notes.push(o.text());
            }
        }
        // How the pending request fares with these rules.
        if let Some(record) = record
            && let Some(request) = self.perms_request(record, folder)
        {
            let covered = rules
                .iter()
                .filter_map(|r| rule::Rule::parse_any(r))
                .any(|r| rule::covers(&r, &request.target, &anchors));
            if !covered {
                notes.push(
                    "these rules do not cover the request the agent is blocked on".to_owned(),
                );
            }
            let launch = record.launch.clone().unwrap_or_default();
            let (sources, managed_only) =
                claude::sources(folder, env, true, launch.permission_mode.as_deref());
            let decision: Decision =
                super::decide(&sources, &request.target, folder, env, managed_only);
            if matches!(decision.outcome, "deny" | "ask") {
                notes.push(format!(
                    "{}: the allow rule will not stop the prompt",
                    decision.text
                ));
            }
            if managed_only {
                notes.push(
                    "managed settings allow only their own rules: this rule will be ignored"
                        .to_owned(),
                );
            }
        }
        let shown: Vec<String> = views
            .iter()
            .filter(|v| added.iter().any(|a| rule::same(a, &v.rule)))
            .map(|v| v.sentence.clone())
            .collect();
        let preview = Preview {
            agent: "claude",
            file: file.as_str().to_owned(),
            diff: edit::unified_diff(
                &target.path.display().to_string(),
                old.as_deref().unwrap_or(""),
                &new_text,
            ),
            base: claude::digest(old.as_deref()),
            exists: old.is_some(),
            path: target.path,
            can_gitignore: file == claude::File::Local
                && target.ignored == Some(false)
                && target.tracked == Some(false),
            tracked: target.tracked,
            ignored: target.ignored,
            warning: target.warning,
            type_to_confirm: dangerous.first().map(|v| v.rule.clone()),
            sentence: sentence(&shown, &target.reach),
            rules: views,
            added,
            notes,
            ..Preview::default()
        };
        Ok(Plan {
            preview,
            new_text,
            tools,
        })
    }

    fn plan_codex_rules(
        &self,
        body: &AllowBody,
        folder: &Folder,
        env: &super::Env,
    ) -> Result<Plan, HubError> {
        if body.patterns.len() > MAX_RULES {
            return Err(invalid(format!("at most {MAX_RULES} rules at once")));
        }
        let user = match body.file.as_deref().unwrap_or("project") {
            "project" | "local" => false,
            "user" => true,
            other => {
                return Err(invalid(format!(
                    "unknown rules file {other:?}: project or user"
                )));
            }
        };
        let anchors = folder.anchors(env, claude::File::Local);
        let mut views = Vec::new();
        let mut lines = Vec::new();
        for pattern in &body.patterns {
            let candidate =
                synth::codex_candidate(pattern.clone(), "custom", &anchors, folder.repo())
                    .map_err(invalid)?;
            lines.push((candidate.pattern.clone(), candidate.text.clone()));
            views.push(RuleView {
                rule: candidate.text,
                risk: candidate.risk,
                sentence: candidate.sentence,
            });
        }
        let dangerous: Vec<&RuleView> = views.iter().filter(|v| v.risk.dangerous).collect();
        if dangerous.len() > 1 {
            return Err(invalid(
                "write dangerous rules one at a time (each is typed to confirm)",
            ));
        }
        if user {
            env.user_layer(true).map_err(invalid)?;
        }
        let path = codex::rules_path(user, folder, env);
        if !user {
            codex::no_symlink_in_project(folder, &path).map_err(invalid)?;
        }
        let (old, new_text, added) = codex::plan_rules(&path, &lines).map_err(invalid)?;
        let view = codex::view(folder, env, None);
        let mut notes = Vec::new();
        if added.is_empty() {
            notes.push("every rule is already in this file".to_owned());
        }
        notes.push(
            "Codex reads rules when it starts: restart the session for them to apply".to_owned(),
        );
        let warning = if user {
            Some("user rules: they apply in every project on this machine".to_owned())
        } else if view.trust_level.as_deref() != Some("trusted") {
            Some(format!(
                "Codex loads {}'s .codex/rules only once the project is trusted (set trust_level = trusted)",
                codex::project_key(folder).display()
            ))
        } else {
            None
        };
        let reach = if user {
            "in every project on this machine".to_owned()
        } else {
            format!(
                "in Codex sessions in {}",
                codex::project_key(folder).display()
            )
        };
        let shown: Vec<String> = views
            .iter()
            .filter(|v| added.contains(&v.rule))
            .map(|v| v.sentence.clone())
            .collect();
        let (tracked, ignored) = if user {
            (None, None)
        } else {
            claude::git_state(&path)
        };
        let preview = Preview {
            agent: "codex",
            file: if user { "user" } else { "project" }.to_owned(),
            diff: edit::unified_diff(
                &path.display().to_string(),
                old.as_deref().unwrap_or(""),
                &new_text,
            ),
            base: claude::digest(old.as_deref()),
            exists: old.is_some(),
            path,
            tracked,
            ignored,
            warning,
            type_to_confirm: dangerous.first().map(|v| v.rule.clone()),
            sentence: sentence(&shown, &reach),
            rules: views,
            added,
            notes,
            ..Preview::default()
        };
        Ok(Plan {
            preview,
            new_text,
            tools: vec!["exec".to_owned()],
        })
    }

    fn plan_codex_setting(
        body: &AllowBody,
        folder: &Folder,
        env: &super::Env,
    ) -> Result<Plan, HubError> {
        let setting =
            codex::Setting::parse(body.setting.as_deref().unwrap_or("").trim()).map_err(invalid)?;
        let value = body
            .value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("give the setting's value"))?;
        if setting == codex::Setting::TrustLevel {
            env.user_layer(true).map_err(invalid)?;
        }
        let (path, old, new_text) =
            codex::plan_setting(setting, value, folder, env).map_err(invalid)?;
        let project = codex::project_key(folder);
        let changed = old.as_deref() != Some(new_text.as_str());
        // Trusting a project turns on its own .codex/ layers: whatever
        // there would need typing if written here needs typing to trust.
        let activates = if setting == codex::Setting::TrustLevel && value == "trusted" {
            codex::trust_activates(folder, &folder.anchors(env, claude::File::Local))
        } else {
            Vec::new()
        };
        let danger: Option<String> = setting.danger(value).map(str::to_owned).or_else(|| {
            (!activates.is_empty()).then(|| {
                format!(
                    "trusting it turns on its .codex/ settings: {}",
                    activates.join("; ")
                )
            })
        });
        let (_, table, key) = codex::setting_place(setting, folder, env);
        let shown = if table.is_empty() {
            format!("{key} = {value}")
        } else {
            format!("[{}] {key} = {value}", table.join("."))
        };
        let (tracked, ignored) = if setting == codex::Setting::TrustLevel {
            (None, None)
        } else {
            claude::git_state(&path)
        };
        let preview = Preview {
            agent: "codex",
            file: if setting == codex::Setting::TrustLevel { "user" } else { "project" }.to_owned(),
            diff: edit::unified_diff(&path.display().to_string(), old.as_deref().unwrap_or(""), &new_text),
            base: claude::digest(old.as_deref()),
            exists: old.is_some(),
            tracked,
            ignored,
            warning: danger.clone().or_else(|| {
                (setting != codex::Setting::TrustLevel).then(|| {
                    "a project setting applies only once the project is trusted, and is shared once committed".to_owned()
                })
            }),
            rules: vec![RuleView {
                rule: shown.clone(),
                risk: rule::Risk {
                    dangerous: danger.is_some(),
                    reason: danger.clone().unwrap_or_default(),
                },
                sentence: setting.sentence(value, &project),
            }],
            added: if changed { vec![shown] } else { Vec::new() },
            sentence: format!("{}.", setting.sentence(value, &project)),
            type_to_confirm: danger.map(|_| value.to_owned()),
            notes: if changed {
                activates
                    .iter()
                    .map(|a| format!("turns on {a}"))
                    .chain(["Codex reads its config when it starts: restart the session for it to apply".to_owned()])
                    .collect()
            } else {
                vec!["already set".to_owned()]
            },
            path,
            ..Preview::default()
        };
        Ok(Plan {
            preview,
            new_text,
            tools: vec![body.setting.clone().unwrap_or_default()],
        })
    }

    /// `POST /api/perms/preview`.
    pub fn perms_preview(&self, body: &AllowBody) -> Result<Preview, HubError> {
        Ok(self.perms_plan(body)?.0.preview)
    }

    /// `POST /api/perms/allow`: write (after the confirmation and, for a
    /// dangerous rule, the typed one), log, and answer the prompt.
    pub fn perms_allow(&self, body: &AllowBody) -> Result<Preview, HubError> {
        if !body.confirm {
            return Err(invalid(
                "not confirmed: preview it, then send confirm: true",
            ));
        }
        let Some(base) = body.base.as_deref() else {
            return Err(invalid(
                "give the preview's base: the file is written only if it has not changed since",
            ));
        };
        // Held until written: a write that read the file before another
        // wrote it would drop the other's rule.
        let guard = self.perms().write_lock();
        let (plan, folder, record) = self.perms_plan(body)?;
        let mut preview = plan.preview;
        if let Some(expected) = &preview.type_to_confirm
            && body.typed.as_deref().map(str::trim) != Some(expected.as_str())
        {
            return Err(invalid(format!(
                "this is dangerous ({}): type it back exactly to confirm: {expected}",
                preview
                    .rules
                    .iter()
                    .find(|r| r.risk.dangerous)
                    .map(|r| r.risk.reason.as_str())
                    .unwrap_or("dangerous")
            )));
        }
        if body.gitignore && !preview.can_gitignore {
            return Err(invalid(
                "gitignore: the file is already ignored, tracked, or not the local one",
            ));
        }
        if base != preview.base {
            return Err(conflict(format!(
                "{} changed since the preview: preview it again",
                preview.path.display()
            )));
        }
        if !preview.added.is_empty() {
            claude::write_atomic(&preview.path, &plan.new_text).map_err(HubError::Internal)?;
            preview.written = true;
            eprintln!(
                "fw hub: perms: {} wrote {} to {} ({} file) for {}",
                preview.agent,
                preview.added.join(" | "),
                preview.path.display(),
                preview.file,
                folder.dir.display()
            );
            let session = record.as_ref().and_then(|r| self.event_session(&r.id));
            self.emit_lifecycle(
                "perms.allowed",
                json!({
                    "session": session,
                    "agent": preview.agent,
                    "file": preview.file,
                    "path": preview.path,
                    "rules": preview.added,
                    "tools": plan.tools,
                    "folder": folder.dir,
                    "dangerous": preview.type_to_confirm.is_some(),
                }),
            );
        }
        if body.gitignore {
            match self.perms_gitignore(&folder, &preview.path) {
                Ok(ignore) => preview.gitignored = Some(ignore),
                Err(e) => preview.notes.push(format!("not added to .gitignore: {e}")),
            }
        }
        drop(guard);
        if body.answer {
            let (answered, note) = match &record {
                Some(record) => self.perms_answer(record, &folder, body.request_key.as_deref()),
                None => (false, "no session named: nothing to answer".to_owned()),
            };
            preview.answered = Some(answered);
            preview.answer_note = Some(note);
        }
        Ok(preview)
    }

    /// Add the local settings file at `path` to its checkout's
    /// .gitignore: the path added.
    fn perms_gitignore(&self, folder: &Folder, path: &Path) -> Result<std::path::PathBuf, String> {
        let root = folder.local_root(self.perms().env());
        let line = path
            .strip_prefix(&root)
            .map_err(|_| "the local settings file is outside its checkout".to_owned())?
            .to_string_lossy()
            .into_owned();
        let ignore = root.join(".gitignore");
        let mut text = match std::fs::read_to_string(&ignore) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", ignore.display())),
        };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("/{line}\n"));
        claude::write_atomic(&ignore, &text)?;
        eprintln!("fw hub: perms: added /{line} to {}", ignore.display());
        Ok(ignore)
    }

    /// Answer a blocked session's permission prompt "Yes" (once), if it
    /// still shows the request the rules were proposed for.
    fn perms_answer(
        &self,
        record: &SessionRecord,
        folder: &Folder,
        key: Option<&str>,
    ) -> (bool, String) {
        let Some(request) = self.perms_request(record, folder) else {
            return (
                false,
                "the agent is not blocked on a permission now: nothing to answer".into(),
            );
        };
        if key.is_some_and(|k| k != request.key) {
            return (
                false,
                "the agent now asks something else: answer it yourself".into(),
            );
        }
        let Some(pty) = self.live_session(&record.id) else {
            return (false, "the session has no terminal on this hub".into());
        };
        let rows = pty.screen_text(TextSource::Visible).lines;
        // The dialog on screen must be the request approved (a hook's
        // request can be stale, or reported by the agent itself).
        let on_screen = match record.kind {
            SessionKind::Claude => synth::from_claude_screen(&rows, &folder.dir),
            SessionKind::Codex => synth::from_codex_screen(&rows, &folder.dir),
            SessionKind::Shell => None,
        };
        if !on_screen
            .as_ref()
            .is_some_and(|s| same_request(s, &request))
        {
            return (
                false,
                "the dialog on screen is not the request these rules were written for: answer it yourself".into(),
            );
        }
        let Some(keys) = yes_key(record.kind, &rows) else {
            return (
                false,
                "no plain \"Yes\" option on the screen: answer it yourself".into(),
            );
        };
        match pty.write(keys.as_bytes().to_vec()) {
            WriteOutcome::Queued => {
                self.perms().clear_pending(&record.id);
                (true, "answered Yes".into())
            }
            other => (false, format!("could not type the answer ({other:?})")),
        }
    }

    /// `GET /api/perms/suggest`: the requests the folder's repo blocked on
    /// most in the last `days`, minus those a rule already allows.
    pub fn perms_suggest(&self, at: &Where, days: i64) -> Result<Value, HubError> {
        if !(1..=MAX_SUGGEST_DAYS).contains(&days) {
            return Err(invalid(format!("days is 1-{MAX_SUGGEST_DAYS}")));
        }
        let (folder, _) = self.perms_resolve(at)?;
        let env = self.perms().env().clone();
        let (sources, _) = claude::sources(&folder, &env, false, None);
        let codex_view = codex::view(&folder, &env, None);
        let anchors = folder.anchors(&env, claude::File::Local);
        let since = chrono::Utc::now().timestamp() - days * 86_400;
        // Handled: an allow rule covers it, or a deny / ask rule decides
        // it (the user chose to be refused or asked).
        let allowed_claude = |raw: &str| {
            let Some(wanted) = rule::Rule::parse_any(raw) else {
                return false;
            };
            sources
                .iter()
                .filter(|s| s.error.is_none())
                .flat_map(|s| s.allow.iter().chain(&s.ask).chain(&s.deny))
                .filter_map(|a| rule::Rule::parse_any(a))
                .any(|have| rule::subsumes(&have, &wanted, &anchors))
        };
        let allowed_codex = |pattern: &[String]| {
            codex_view
                .rules
                .iter()
                .flat_map(|f| f.rules.iter())
                .any(|r| pattern.starts_with(&r.pattern))
        };
        let mut skipped = 0usize;
        let suggestions = self
            .perms()
            .journal()
            .suggestions(&folder.key(), since, |entry| {
                entry.rules.iter().any(|r| match (entry.agent.as_str(), r) {
                    ("codex", Value::Array(words)) => {
                        let words: Vec<String> = words
                            .iter()
                            .filter_map(|w| w.as_str().map(str::to_owned))
                            .collect();
                        !allowed_codex(&words)
                    }
                    (_, Value::String(rule)) => !allowed_claude(rule),
                    _ => false,
                })
            });
        let mut out = Vec::new();
        for s in suggestions {
            let views: Vec<Value> = s
                .rules
                .iter()
                .filter_map(|r| match (s.agent.as_str(), r) {
                    ("codex", Value::Array(words)) => {
                        let words: Vec<String> = words.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect();
                        let c = synth::codex_candidate(words, "suggested", &anchors, folder.repo()).ok()?;
                        Some(json!({"pattern": c.pattern, "rule": c.text, "risk": c.risk, "sentence": c.sentence}))
                    }
                    (_, Value::String(raw)) => {
                        let parsed = rule::Rule::parse(raw).ok()?;
                        Some(json!({
                            "rule": raw,
                            "risk": rule::risk(&parsed, &anchors, folder.repo()),
                            "sentence": rule::describe(&parsed, &anchors),
                        }))
                    }
                    _ => None,
                })
                .collect();
            if views.iter().any(|v| v["risk"]["dangerous"] == true) || views.is_empty() {
                skipped += 1;
                continue;
            }
            out.push(json!({
                "agent": s.agent,
                "tool": s.tool,
                "rules": views,
                "count": s.count,
                "sessions": s.sessions,
                "last_at": s.last_at,
            }));
        }
        Ok(json!({
            "folder": folder.dir,
            "repo": folder.key(),
            "days": days,
            "suggestions": out,
            "skipped_dangerous": skipped,
        }))
    }
}

/// The keys that answer a permission prompt's plain "Yes" (Claude Code:
/// its option 1; Codex: `y`), when the screen shows one.
/// Whether the request read off the screen is `request` (a dialog may
/// wrap or cut a long command: one must start the other, spaces aside).
fn same_request(screen: &synth::Request, request: &synth::Request) -> bool {
    use super::rule::Target;
    if screen.key == request.key {
        return true;
    }
    match (&screen.target, &request.target) {
        (Target::Command { command: a }, Target::Command { command: b }) => {
            let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            let (a, b) = (squash(a), squash(b));
            !a.is_empty() && (a == b || (a.len() >= 8 && b.starts_with(&a)))
        }
        (a, b) => a == b,
    }
}

pub fn yes_key(kind: SessionKind, rows: &[String]) -> Option<&'static str> {
    let options: Vec<String> = rows
        .iter()
        .map(|r| {
            r.trim()
                .trim_start_matches(['❯', '›', '▶', '│'])
                .trim_end_matches('│')
                .trim()
                .to_owned()
        })
        .collect();
    match kind {
        SessionKind::Claude => options.iter().any(|o| o == "1. Yes").then_some("1"),
        SessionKind::Codex => options
            .iter()
            .any(|o| o == "1. Yes, proceed (y)" || o == "Yes (y)")
            .then_some("y"),
        SessionKind::Shell => None,
    }
}
