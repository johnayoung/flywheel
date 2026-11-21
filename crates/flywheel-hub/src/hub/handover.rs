//! Handing a lead's work to its successor: the sessions it controls and
//! the watches it armed pass to the new session, and the new session's
//! first prompt carries a brief of what it takes over, so it starts
//! knowing its children instead of rediscovering them.

use super::Hub;
use crate::state::SessionState;

/// At most this many children are listed in a brief.
const BRIEF_CHILDREN: usize = 20;
/// A child's line says at most this many characters of what it is on.
const BRIEF_TITLE_CHARS: usize = 120;

impl Hub {
    /// Running sessions that agent session `from` controls (see
    /// [`super::endings::effective_owner`]).
    fn controlled_by(&self, from: &str) -> Vec<crate::state::SessionRecord> {
        let store = self.store();
        let sessions = store.sessions();
        sessions
            .iter()
            .filter(|s| s.id != from && s.state == SessionState::Running)
            .filter(|s| super::endings::effective_owner(s, sessions).as_deref() == Some(from))
            .cloned()
            .collect()
    }

    /// The brief for a session taking over `from`'s work: each running
    /// session `from` controls (name, id, state, what it is on), what
    /// `from` still asks the operator, and how reports reach the new
    /// session. `None` when there is nothing to hand over.
    pub(crate) fn handover_brief(&self, from: &str) -> Option<String> {
        let name = self.store().session(from).ok()?.name.clone();
        let children = self.controlled_by(from);
        let ask = self
            .telemetry()
            .snapshot(from)
            .and_then(|snap| snap.ask)
            .filter(|ask| ask.prompt.is_none());
        if children.is_empty() && ask.is_none() {
            return None;
        }
        let mut lines = vec![format!("--- Handed over by the hub from {name} ---")];
        if !children.is_empty() {
            lines.push(
                "These sessions now answer to you; each one's report (its closing message) \
                 reaches you through your children watch, so end your turn to wait:"
                    .to_owned(),
            );
            for child in children.iter().take(BRIEF_CHILDREN) {
                let state = self
                    .agent_status(&child.id)
                    .map_or_else(|| "unknown".to_owned(), |s| s.state.as_str().to_owned());
                let on = self
                    .telemetry()
                    .snapshot(&child.id)
                    .and_then(|snap| snap.turn_title)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| {
                        let short: String = t.chars().take(BRIEF_TITLE_CHARS).collect();
                        format!(": {short}")
                    })
                    .unwrap_or_default();
                lines.push(format!("- {} (id {}), {state}{on}", child.name, child.id));
            }
            if children.len() > BRIEF_CHILDREN {
                lines.push(format!(
                    "- and {} more (list_sessions mine: true)",
                    children.len() - BRIEF_CHILDREN
                ));
            }
        }
        if let Some(ask) = ask {
            lines.push(format!(
                "{name} was waiting on the operator for: {} (their answer comes to you).",
                ask.text
            ));
        }
        Some(lines.join("\n"))
    }

    /// Pass agent session `from`'s children and watches to `to` (a
    /// session continuing its work while `from` stays on the hub): each
    /// session `from` controls is given to `to`, and its watches move
    /// (a watch on its children then watches `to`'s). Best effort.
    pub(crate) fn hand_over(&self, from: &str, to: &str) {
        for child in self.controlled_by(from) {
            if child.id == to {
                continue;
            }
            let owner = to.to_owned();
            if let Err(e) = self
                .store()
                .update_session(&child.id, |r| r.owner = Some(owner))
            {
                eprintln!("fw hub: hand {} over to {to}: {e}", child.name);
            }
        }
        self.refresh_parents();
        match self
            .store()
            .move_watches(from, to, crate::watch::MAX_PER_OWNER)
        {
            Ok(moved) => {
                for watch in &moved {
                    self.log_hub_event(
                        "watch.moved",
                        Some(to),
                        serde_json::json!({ "watch": watch.id, "from": from, "to": to }),
                    );
                }
            }
            Err(e) => eprintln!("fw hub: watches of {from}: not passed on: {e}"),
        }
        // Children it had but no watch on them: the successor hears them.
        if !self.controlled_by(to).is_empty() {
            self.ensure_children_watch(to);
        }
        self.emit(super::HubEvent::Sessions);
    }
}
