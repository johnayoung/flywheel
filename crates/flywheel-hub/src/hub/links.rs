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
            start_sessions: None,
            start_requests: Vec::new(),
            starts_made: 0,
            owner: None,
            ending: None,
            ended: None,
            end_requests: Vec::new(),
            stale: None,
            never_relaunch: false,
            labels: Default::default(),
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
    /// prefix, or name) on this hub: typed in now, or queued (`priority`:
    /// ahead of the waiting messages that are not). With a `topic`, it
    /// supersedes `from`'s earlier message on it (see
    /// [`crate::queue::Supersede`]). What became of it, in words for the
    /// sender.
    pub fn agent_message(
        &self,
        from: &str,
        to: &str,
        text: &str,
        priority: bool,
        topic: Option<&str>,
    ) -> Result<String, HubError> {
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
        let topic = topic.map(str::trim).filter(|t| !t.is_empty());
        let (delivery, superseded) = self.send_or_queue_topic(
            &target.id,
            &from_line,
            text,
            Some(&sender.id),
            priority,
            topic,
        )?;
        let after_read = match (&superseded, topic) {
            (crate::queue::Supersede::Replaced { item, place }, Some(topic)) => {
                return Ok(format!(
                    "Replaced your unread message {} on topic {topic} for {} (still #{place} in \
                     its queue): it reads only the new text, when its turn is done.",
                    &item[..item.len().min(8)],
                    target.name
                ));
            }
            (crate::queue::Supersede::AfterRead { .. }, Some(topic)) => format!(
                " It had read your earlier message on topic {topic}: this one says it supersedes it."
            ),
            _ => String::new(),
        };
        Ok(match delivery {
            Delivery::Pasted { .. } => {
                format!("Delivered to {}: it reads it now.{after_read}", target.name)
            }
            Delivery::Queued { item, .. } if priority => {
                let place = self
                    .store()
                    .queue(&target.id)
                    .and_then(|q| q.waiting().position(|i| i.id == item))
                    .map_or(1, |at| at + 1);
                format!(
                    "Queued for {} as priority (id {item}, #{place} in its queue): it reads it \
                         when its turn is done.{after_read}",
                    target.name
                )
            }
            Delivery::Queued { why, .. } => format!(
                "Queued for {} ({why}): it reads it when its turn is done.{after_read}",
                target.name
            ),
        })
    }

    /// The running agent sessions on this hub, one line each: name, id,
    /// folder, status, who started it, and what it is doing. `except`
    /// (the asker) is left out; `mine`: only the sessions it started,
    /// exited ones too.
    pub fn agent_sessions(&self, except: &str, mine: bool) -> String {
        type Listed = (SessionRecord, Option<String>);
        let (records, names): (Vec<Listed>, Vec<(String, String)>) = {
            let store = self.store();
            let sessions = store.sessions();
            let records = sessions
                .iter()
                .filter(|s| s.id != except && s.runs_agent())
                .map(|s| (s, super::endings::effective_parent(s, sessions)))
                .filter(|(s, parent)| {
                    if mine {
                        parent.as_deref() == Some(except)
                    } else {
                        s.state == SessionState::Running
                    }
                })
                .take(LIST_MAX)
                .map(|(s, parent)| (s.clone(), parent))
                .collect();
            let names = store
                .sessions()
                .iter()
                .map(|s| (s.id.clone(), s.name.clone()))
                .collect();
            (records, names)
        };
        if records.is_empty() {
            return if mine {
                "You lead no session that is still on this hub.".to_owned()
            } else {
                "No other agent session runs on this hub.".to_owned()
            };
        }
        let mut lines = Vec::with_capacity(records.len());
        for (record, parent) in &records {
            let snap = self.telemetry().snapshot(&record.id);
            let status = if record.state == SessionState::Running {
                snap.as_ref().map_or("unknown", |s| s.status.as_str())
            } else {
                "exited"
            };
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
            let named = |id: &str| {
                if id == except {
                    Some("you".to_owned())
                } else {
                    names
                        .iter()
                        .find(|(n, _)| n == id)
                        .map(|(_, name)| name.clone())
                }
            };
            if let Some(words) = lineage_words(
                record.opened_by.as_deref().and_then(named),
                parent.as_deref().and_then(named),
            ) {
                line.push_str(&format!(", {words}"));
            }
            if !doing.is_empty() {
                line.push_str(&format!(" -- {doing}"));
            }
            lines.push(line);
        }
        lines.join("\n")
    }
}

/// Who started a session and who leads it now, in words: "started by X",
/// "started by X, now led by Y" when control moved, "led by Y" when its
/// starter is unknown or gone.
pub(crate) fn lineage_words(starter: Option<String>, parent: Option<String>) -> Option<String> {
    match (starter, parent) {
        (Some(s), Some(p)) if s != p => Some(format!("started by {s}, now led by {p}")),
        (Some(s), _) => Some(format!("started by {s}")),
        (None, Some(p)) => Some(format!("led by {p}")),
        (None, None) => None,
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
