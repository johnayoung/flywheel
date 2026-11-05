//! Broadcast: one prompt to several agent sessions at once ("everyone
//! rebase on main", "stop and write a summary").
//!
//! Targets are sessions and workspaces, each by id, unique id prefix, or
//! name; a workspace stands for every Claude Code and Codex session in it
//! that has not exited. Each target gets the prompt through the prompt
//! queue's own path ([`Hub::send_or_queue`]): typed in now when its queue
//! would deliver now (at rest, nobody typing, nothing half-typed or queued
//! before it, no review or grading using it), else queued for it (`done`)
//! -- never typed while it is blocked or someone types into it. A target
//! that cannot take it (a shell, an agent that exited or came back as a
//! plain shell, a full queue, nothing matching) is refused with why; the
//! others still get it. The answer lists every target's result.
//!
//! An optional provenance header opens the prompt (`Broadcast to 3 agent
//! sessions (api-1, api-2, web): ...`), so each agent knows its siblings got
//! the same. Each broadcast announces `broadcast.sent` (plugins, and one
//! activity row) with the per-target results and the text (capped), never
//! in the activity row.
//!
//! A broadcast runs on the hub that owns the sessions; other machines'
//! sessions are reached through the `/m/<machine>` proxy, one call per
//! machine (the web UI splits a mixed selection that way).

use crate::agent::SessionKind;
use crate::hub::{Hub, HubError};
use crate::queue::Delivery;
use crate::state::{SessionRecord, SessionState, StateError, new_id};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most targets (selectors, and sessions they name) one broadcast
/// reaches.
pub const MAX_TARGETS: usize = 64;
/// Sessions the provenance header names before `+N more`.
const HEADER_NAMES: usize = 8;

/// `POST /api/broadcast`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BroadcastRequest {
    /// Sessions and workspaces (id, unique id prefix, or name).
    #[serde(default)]
    pub targets: Vec<String>,
    /// The prompt.
    pub text: String,
    /// Open it with a provenance line naming who got it.
    #[serde(default)]
    pub header: bool,
}

/// What became of the prompt for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Typed in now (bracketed paste, then Enter).
    Sent,
    /// Queued: it goes once the agent's turn is done.
    Queued,
    /// Not delivered (`why` says why).
    Refused,
}

/// One target's result.
#[derive(Debug, Clone, Serialize)]
pub struct TargetResult {
    /// The selector it came from (as given).
    pub target: String,
    /// The session (absent: the selector named none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<SessionKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub result: Outcome,
    /// Why it was queued or refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The queued item's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Prompts waiting in its queue after this one was queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<usize>,
}

/// Per-outcome counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub sent: usize,
    pub queued: usize,
    pub refused: usize,
}

/// A finished broadcast: what the API and CLI answer.
#[derive(Debug, Clone, Serialize)]
pub struct Broadcast {
    pub id: String,
    pub at: DateTime<Utc>,
    /// Bytes typed per target (the header included).
    pub bytes: usize,
    pub header: bool,
    pub counts: Counts,
    pub results: Vec<TargetResult>,
}

/// A selector resolved: the sessions it names, or why none.
enum Resolved {
    Sessions(Vec<SessionRecord>),
    Refused(String),
}

impl TargetResult {
    fn refused(target: &str, record: Option<&SessionRecord>, why: String) -> Self {
        let mut result = Self::about(target, record);
        result.result = Outcome::Refused;
        result.why = Some(why);
        result
    }

    fn about(target: &str, record: Option<&SessionRecord>) -> Self {
        Self {
            target: target.to_owned(),
            session_id: record.map(|r| r.id.clone()),
            session: record.map(|r| r.name.clone()),
            kind: record.map(|r| r.kind),
            workspace_id: record.map(|r| r.workspace_id.clone()),
            result: Outcome::Refused,
            why: None,
            item: None,
            pending: None,
        }
    }
}

/// The provenance line opening a broadcast to `names` (every target).
pub fn header(names: &[&str]) -> String {
    let n = names.len();
    let mut shown: Vec<&str> = names.iter().take(HEADER_NAMES).copied().collect();
    let more = n.saturating_sub(HEADER_NAMES);
    let more = (more > 0).then(|| format!("+{more} more"));
    if let Some(more) = &more {
        shown.push(more);
    }
    format!(
        "Broadcast to {n} agent session{} ({}):",
        if n == 1 { "" } else { "s" },
        shown.join(", ")
    )
}

impl Hub {
    /// What `selector` names: one session, or a workspace's agents.
    fn broadcast_resolve(&self, selector: &str) -> Resolved {
        let store = self.store();
        let sessions = store.sessions();
        let workspaces = store.workspaces();
        let session = crate::script::find_by(
            sessions,
            |s| (s.id.as_str(), s.name.as_str()),
            selector,
            "session",
        );
        let workspace = crate::script::find_by(
            workspaces,
            |w| (w.id.as_str(), w.name.as_str()),
            selector,
            "workspace",
        );
        let workspace = match (session, workspace) {
            (Err(why), _) | (_, Err(why)) => return Resolved::Refused(why),
            (Ok(None), Ok(None)) => {
                return Resolved::Refused(format!(
                    "no session or workspace matches {:?} (id, id prefix, or name)",
                    selector.trim()
                ));
            }
            (Ok(Some(s)), Ok(None)) => return Resolved::Sessions(vec![s.clone()]),
            (Ok(Some(s)), Ok(Some(w))) => {
                let selector = selector.trim();
                if s.id == selector {
                    return Resolved::Sessions(vec![s.clone()]);
                }
                if w.id != selector {
                    return Resolved::Refused(format!(
                        "{selector:?} names both session {} and workspace {} ({}): use an id",
                        s.label(),
                        w.name,
                        w.id
                    ));
                }
                w
            }
            (Ok(None), Ok(Some(w))) => w,
        };
        let agents = Self::workspace_agents(sessions, &workspace.id);
        if agents.is_empty() {
            Resolved::Refused(format!(
                "workspace {} has no Claude Code or Codex session (exited ones aside)",
                workspace.name
            ))
        } else {
            Resolved::Sessions(agents)
        }
    }

    /// A workspace's agent sessions that have not exited, in sidebar
    /// order.
    fn workspace_agents(sessions: &[SessionRecord], workspace_id: &str) -> Vec<SessionRecord> {
        sessions
            .iter()
            .filter(|s| {
                s.workspace_id == workspace_id
                    && s.kind.is_agent()
                    && !matches!(s.state, SessionState::Exited { .. })
            })
            .cloned()
            .collect()
    }

    /// Send `request.text` to every target (see the module docs): typed
    /// in now, queued, or refused, each target on its own.
    pub fn broadcast(&self, request: &BroadcastRequest) -> Result<Broadcast, HubError> {
        let text = crate::queue::validate_text(&request.text)?;
        let selectors: Vec<&str> = request
            .targets
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if selectors.is_empty() {
            return Err(
                StateError::Invalid("name the sessions or workspaces to send to".into()).into(),
            );
        }
        if selectors.len() > MAX_TARGETS {
            return Err(StateError::Invalid(format!(
                "{} targets: at most {MAX_TARGETS} per broadcast",
                selectors.len()
            ))
            .into());
        }
        // Resolve every selector first: the header names them all, and a
        // session named twice (itself and its workspace) gets it once.
        let mut refused = Vec::new();
        let mut targets: Vec<(String, SessionRecord)> = Vec::new();
        for selector in &selectors {
            match self.broadcast_resolve(selector) {
                Resolved::Refused(why) => refused.push(TargetResult::refused(selector, None, why)),
                Resolved::Sessions(found) => {
                    for record in found {
                        if !targets.iter().any(|(_, t)| t.id == record.id) {
                            targets.push(((*selector).to_owned(), record));
                        }
                    }
                }
            }
        }
        if targets.len() > MAX_TARGETS {
            return Err(StateError::Invalid(format!(
                "those name {} sessions: at most {MAX_TARGETS} per broadcast",
                targets.len()
            ))
            .into());
        }
        let prompt = if request.header {
            // The agents it is for (a shell named among them is refused).
            let names: Vec<&str> = targets
                .iter()
                .filter(|(_, r)| r.runs_agent())
                .map(|(_, r)| r.name.as_str())
                .collect();
            crate::queue::validate_text(&format!("{}\n\n{text}", header(&names)))?
        } else {
            text
        };
        let id = new_id();
        let mut results = Vec::with_capacity(targets.len() + refused.len());
        for (selector, record) in &targets {
            let mut result = TargetResult::about(selector, Some(record));
            match self.send_or_queue(&record.id, &prompt) {
                Ok(Delivery::Pasted { .. }) => result.result = Outcome::Sent,
                Ok(Delivery::Queued { item, why, pending }) => {
                    result.result = Outcome::Queued;
                    result.why = Some(why);
                    result.item = Some(item);
                    result.pending = Some(pending);
                }
                Err(e) => result = TargetResult::refused(selector, Some(record), e.to_string()),
            }
            results.push(result);
        }
        results.extend(refused);
        let mut counts = Counts::default();
        for r in &results {
            match r.result {
                Outcome::Sent => counts.sent += 1,
                Outcome::Queued => counts.queued += 1,
                Outcome::Refused => counts.refused += 1,
            }
        }
        let broadcast = Broadcast {
            id,
            at: Utc::now(),
            bytes: prompt.len(),
            header: request.header,
            counts,
            results,
        };
        eprintln!(
            "fw hub: broadcast {}: {} target(s), {} bytes -> {} sent, {} queued, {} refused",
            &broadcast.id[..8],
            broadcast.results.len(),
            broadcast.bytes,
            counts.sent,
            counts.queued,
            counts.refused
        );
        self.emit_lifecycle("broadcast.sent", self.broadcast_event(&broadcast, &prompt));
        Ok(broadcast)
    }

    /// The `broadcast.sent` payload: each target as plugin events describe
    /// sessions, with its result.
    fn broadcast_event(&self, broadcast: &Broadcast, prompt: &str) -> Value {
        let targets: Vec<Value> = broadcast
            .results
            .iter()
            .map(|r| {
                let session = r
                    .session_id
                    .as_deref()
                    .and_then(|id| self.event_session(id))
                    .unwrap_or(Value::Null);
                json!({
                    "target": r.target,
                    "session": session,
                    "result": r.result,
                    "why": r.why,
                })
            })
            .collect();
        // One workspace for the activity row when every target is in it.
        let mut workspaces: Vec<&str> = broadcast
            .results
            .iter()
            .filter_map(|r| r.workspace_id.as_deref())
            .collect();
        workspaces.sort_unstable();
        workspaces.dedup();
        let workspace = match workspaces.as_slice() {
            [one] => self
                .workspace(one)
                .ok()
                .map(|w| json!({ "id": w.id, "name": w.name })),
            _ => None,
        };
        json!({
            "broadcast": {
                "id": broadcast.id,
                "text": crate::queue::event_text(prompt),
                "bytes": broadcast.bytes,
                "header": broadcast.header,
                "counts": broadcast.counts,
            },
            "workspace": workspace,
            "targets": targets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_names_every_target_up_to_a_few() {
        assert_eq!(header(&["a"]), "Broadcast to 1 agent session (a):");
        assert_eq!(
            header(&["api-1", "api-2", "web"]),
            "Broadcast to 3 agent sessions (api-1, api-2, web):"
        );
        let many: Vec<String> = (1..=11).map(|i| format!("s{i}")).collect();
        let names: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            header(&names),
            "Broadcast to 11 agent sessions (s1, s2, s3, s4, s5, s6, s7, s8, +3 more):"
        );
    }

    #[test]
    fn outcomes_serialize_as_words() {
        assert_eq!(serde_json::to_value(Outcome::Sent).unwrap(), "sent");
        assert_eq!(serde_json::to_value(Outcome::Queued).unwrap(), "queued");
        assert_eq!(serde_json::to_value(Outcome::Refused).unwrap(), "refused");
    }
}
