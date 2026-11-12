//! Sessions tied to sessions: a handoff's successor ([`Hub::continue_session`]),
//! a message from one agent to another ([`Hub::agent_message`]), and the
//! list an agent picks a recipient from ([`Hub::agent_sessions`]).
//!
//! An agent proposes a handoff with the hub's `handoff` tool
//! ([`Hub::propose_handoff`]): its prompt for a fresh session is kept on
//! its session record and waits on the operator's Needs-you list until it
//! is started or dismissed (never cleared by a prompt, a view, or another
//! handoff); continuing starts that session in the same directory
//! (worktree, model, account) with that prompt, linked both ways. A message is typed into
//! the recipient the way a broadcast is, under a provenance line naming
//! its sender: now when the recipient is at rest, else queued for when
//! its turn is done.

use super::{Hub, HubError, HubEvent};
use crate::agent::{self, SessionKind};
use crate::queue::Delivery;
use crate::state::{
    PendingHandoff, SessionRecord, SessionState, StateError, new_id, validate_name,
};
use crate::telemetry::{Draft, What, now_ms};
use chrono::Utc;
use std::sync::Arc;

use super::SessionView;

/// Most sessions [`Hub::agent_sessions`] lists.
const LIST_MAX: usize = 200;

/// A handoff successor's name: its predecessor's, numbered on (`docs` ->
/// `docs 2`, `docs 2` -> `docs 3`), the first one free across the hub, so
/// the pair reads as one line of work and the name still addresses it.
fn successor_name(parent: &str, taken: &[&str]) -> String {
    let base = match parent.rsplit_once(' ') {
        Some((stem, n)) if !stem.is_empty() && n.parse::<u32>().is_ok() => stem,
        _ => parent,
    };
    // Session names are at most 100 characters; leave room for the number.
    let base: String = base.chars().take(90).collect();
    let base = base.trim_end();
    (2u32..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !taken.contains(&name.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

/// A pending handoff in an error's list: its id, and its name if any.
fn described(handoff: &PendingHandoff) -> String {
    match &handoff.name {
        Some(name) => format!("{} ({name:?})", handoff.id),
        None => handoff.id.clone(),
    }
}

impl Hub {
    /// Agent session `id` proposes continuing its work in a fresh session
    /// from `prompt` (the `handoff` tool): kept on its record until the
    /// operator starts or dismisses it.
    pub fn propose_handoff(
        &self,
        id: &str,
        prompt: &str,
        name: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PendingHandoff, HubError> {
        let from = self.store().session(id)?.clone();
        if !from.runs_agent() {
            return Err(StateError::Invalid(format!(
                "{} is not an agent session: only an agent hands its work off",
                from.name
            ))
            .into());
        }
        let prompt = agent::validate_prompt(Some(prompt))
            .map_err(StateError::Invalid)?
            .ok_or_else(|| StateError::Invalid("give the new session's whole prompt".into()))?;
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(validate_name)
            .transpose()?;
        let reason = reason
            .map(crate::telemetry::summary::text)
            .filter(|r| !r.is_empty());
        let handoff = PendingHandoff {
            id: new_id(),
            name,
            reason,
            prompt,
            created_at: Utc::now(),
        };
        self.store().add_handoff(&from.id, handoff.clone())?;
        self.emit(HubEvent::Sessions);
        Ok(handoff)
    }

    /// Drop session `id`'s pending handoff `handoff` unstarted.
    pub fn dismiss_handoff(&self, id: &str, handoff: &str) -> Result<(), HubError> {
        self.store().remove_handoff(id, handoff)?;
        self.emit(HubEvent::Sessions);
        Ok(())
    }

    /// Start a new session that continues agent session `id`'s work: its
    /// directory, worktree, kind, model, permission options, and account.
    /// The prompt is `prompt` when given, else that of its pending handoff
    /// `handoff` (default: the only one pending). The name is `name`, else
    /// the handoff's, made unique, else the predecessor's numbered on. The
    /// two are linked, and the handoff it started is no longer pending.
    pub fn continue_session(
        self: &Arc<Self>,
        id: &str,
        prompt: Option<&str>,
        handoff: Option<&str>,
        name: Option<&str>,
    ) -> Result<SessionView, HubError> {
        let from = self.store().session(id)?.clone();
        if !from.runs_agent() {
            return Err(StateError::Invalid(format!(
                "{} is not an agent session: only an agent's work is continued",
                from.name
            ))
            .into());
        }
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let chosen = match handoff.map(str::trim).filter(|h| !h.is_empty()) {
            Some(wanted) => Some(
                from.handoffs
                    .iter()
                    .find(|h| h.id == wanted)
                    .cloned()
                    .ok_or_else(|| {
                        StateError::NotFound(format!(
                            "{} has no pending handoff {wanted}",
                            from.name
                        ))
                    })?,
            ),
            None if prompt.is_some() => None,
            None => match from.handoffs.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                many => {
                    return Err(StateError::Invalid(format!(
                        "{} has {} pending handoffs, pick one: {}",
                        from.name,
                        many.len(),
                        many.iter().map(described).collect::<Vec<_>>().join("; ")
                    ))
                    .into());
                }
            },
        };
        // A handoff proposed before handoffs were kept on the session: its
        // telemetry ask.
        let legacy = || {
            self.telemetry()
                .snapshot(&from.id)
                .and_then(|snap| snap.ask)
                .and_then(|ask| ask.prompt)
        };
        let prompt = match (prompt, &chosen) {
            (Some(prompt), _) => prompt.to_owned(),
            (None, Some(chosen)) => chosen.prompt.clone(),
            (None, None) => legacy().ok_or_else(|| {
                StateError::Invalid(format!(
                    "{} proposed no handoff: give the new session's prompt",
                    from.name
                ))
            })?,
        };
        let prompt = agent::validate_prompt(Some(&prompt))
            .map_err(StateError::Invalid)?
            .ok_or_else(|| StateError::Invalid("the prompt is empty".into()))?;
        let wanted_name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .or_else(|| chosen.as_ref().and_then(|h| h.name.as_deref()))
            .map(validate_name)
            .transpose()?;
        if !from.cwd.is_dir() {
            return Err(StateError::Invalid(format!(
                "{}'s directory {} no longer exists",
                from.name,
                from.cwd.display()
            ))
            .into());
        }
        let command = vec![
            self.agent_program(from.kind)?
                .to_string_lossy()
                .into_owned(),
        ];
        let name = match wanted_name {
            Some(wanted) => self.unique_session_name(&wanted),
            None => {
                let store = self.store();
                let taken: Vec<&str> = store.sessions().iter().map(|s| s.name.as_str()).collect();
                successor_name(&from.name, &taken)
            }
        };
        let record = SessionRecord {
            id: new_id(),
            workspace_id: from.workspace_id.clone(),
            name,
            kind: from.kind,
            cwd: from.cwd.clone(),
            command,
            agent_session_id: (from.kind == SessionKind::Claude)
                .then(|| uuid::Uuid::new_v4().hyphenated().to_string()),
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            // Its directory is the same worktree, as a companion's is.
            worktree: from.worktree.clone(),
            companion_of: None,
            launch: from.launch.clone(),
            reviewer: None,
            review_of: None,
            account: from.account.clone(),
            continues: Some(from.id.clone()),
            opened_by: None,
            handoffs: Vec::new(),
        };
        // Taken before the launch, so two starts of one handoff start one
        // session; put back if the launch fails.
        if let Some(chosen) = &chosen {
            self.store().remove_handoff(&from.id, &chosen.id)?;
        }
        let view = match self.launch(record, None, None, false, Some(&prompt)) {
            Ok(view) => view,
            Err(failed) => {
                if let Some(chosen) = chosen {
                    // The session may be gone meanwhile: nothing to put back.
                    let _ = self.store().add_handoff(&from.id, chosen);
                    self.emit(HubEvent::Sessions);
                }
                return Err(failed);
            }
        };
        self.telemetry().record(
            &from.id,
            vec![Draft::new(
                now_ms(),
                What::Continued {
                    successor: view.record.id.clone(),
                },
            )],
        );
        Ok(view)
    }

    /// Agent session `from` sends `text` to the session `to` names (id, id
    /// prefix, or name) on this hub: typed in now, or queued. What became
    /// of it, in words for the sender.
    pub fn agent_message(&self, from: &str, to: &str, text: &str) -> Result<String, HubError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(StateError::Invalid("the message is empty".into()).into());
        }
        let (sender, target) = {
            let store = self.store();
            let sender = store.session(from)?.clone();
            let found = crate::script::find_by(
                store.sessions(),
                |s| (s.id.as_str(), s.name.as_str()),
                to,
                "session",
            )
            .map_err(StateError::Invalid)?
            .cloned()
            .ok_or_else(|| {
                StateError::Invalid(format!(
                    "no session matches {:?} (list_sessions names them)",
                    to.trim()
                ))
            })?;
            (sender, found)
        };
        if target.id == sender.id {
            return Err(StateError::Invalid("that is this session".into()).into());
        }
        if !target.runs_agent() || target.state != SessionState::Running {
            return Err(StateError::Invalid(format!(
                "{} is not a running agent session",
                target.name
            ))
            .into());
        }
        let from_line = crate::agent_handoff::header(
            &crate::agent_handoff::Provenance {
                session_id: sender.id.clone(),
                session: sender.name.clone(),
                kind: sender.kind,
                branch: sender.worktree.as_ref().map(|w| w.branch.clone()),
                machine: None,
            },
            sender.cwd.file_name().and_then(|n| n.to_str()),
            "a message from that agent session",
        );
        let prompt = crate::queue::validate_text(&format!("{from_line}\n\n{text}"))?;
        Ok(match self.send_or_queue(&target.id, &prompt)? {
            Delivery::Pasted { .. } => format!("Delivered to {}: it reads it now.", target.name),
            Delivery::Queued { why, .. } => format!(
                "Queued for {} ({why}): it reads it when its turn is done.",
                target.name
            ),
        })
    }

    /// The running agent sessions on this hub, one line each: name, id,
    /// folder, status, and what it is doing. `except` (the asker) is left
    /// out.
    pub fn agent_sessions(&self, except: &str) -> String {
        let records: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.id != except && s.runs_agent() && s.state == SessionState::Running)
            .take(LIST_MAX)
            .cloned()
            .collect();
        if records.is_empty() {
            return "No other agent session runs on this hub.".to_owned();
        }
        let mut lines = Vec::with_capacity(records.len());
        for record in &records {
            let snap = self.telemetry().snapshot(&record.id);
            let status = snap.as_ref().map_or("unknown", |s| s.status.as_str());
            let doing = snap
                .as_ref()
                .and_then(|s| {
                    s.plan
                        .as_ref()
                        .and_then(|p| p.current.clone())
                        .or_else(|| s.turn_title.clone())
                })
                .unwrap_or_default();
            let mut line = format!(
                "{} (id {}) [{}] in {}: {status}",
                record.name,
                record.id,
                record.kind.as_str(),
                record.cwd.display()
            );
            if let Some(worktree) = &record.worktree {
                line.push_str(&format!(", branch {}", worktree.branch));
            }
            if !doing.is_empty() {
                line.push_str(&format!(" -- {doing}"));
            }
            lines.push(line);
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod successor_name_tests {
    use super::successor_name;

    #[test]
    fn numbers_on_from_the_predecessor() {
        assert_eq!(successor_name("docs", &["docs"]), "docs 2");
        assert_eq!(successor_name("docs 2", &["docs", "docs 2"]), "docs 3");
        assert_eq!(
            successor_name("claude 1", &["claude 1", "claude 2"]),
            "claude 3"
        );
        // A taken number is skipped, hub-wide.
        assert_eq!(
            successor_name("docs", &["docs", "docs 2", "docs 3"]),
            "docs 4"
        );
        // A trailing word that is not a number is part of the name.
        assert_eq!(successor_name("fix v2", &["fix v2"]), "fix v2 2");
    }

    #[test]
    fn stays_within_the_name_limit() {
        let long = "x".repeat(100);
        let name = successor_name(&long, &[long.as_str()]);
        assert!(name.chars().count() <= 100, "{name}");
        assert!(name.ends_with(" 2"));
    }
}
