//! Operator rulings on the hub (see [`crate::rulings`]): recorded and
//! retired by a session's agent (`record_ruling`), each change logged as a
//! hub event (`ruling.recorded`, `ruling.retired`), said again in the
//! compaction and handoff briefs.

use super::{Hub, HubError};
use crate::rulings::{self, Ruling, RulingAction, RulingError};
use crate::state::StateError;

impl From<RulingError> for HubError {
    fn from(e: RulingError) -> Self {
        StateError::Invalid(e.0).into()
    }
}

impl Hub {
    /// Session `id`'s rulings, oldest first (none for a session gone).
    pub fn rulings(&self, id: &str) -> Vec<Ruling> {
        self.store()
            .session(id)
            .map(|r| r.rulings.clone())
            .unwrap_or_default()
    }

    /// The `record_ruling` agent tool: `action` on the caller's own
    /// rulings; what to tell the agent.
    pub fn agent_ruling(&self, caller: &str, action: RulingAction) -> Result<String, HubError> {
        let name = self.store().session(caller)?.name.clone();
        let said = match action {
            RulingAction::Record(new) => {
                let now = chrono::Utc::now();
                let (ruling, replaced) = self.store().update_session(caller, |record| {
                    rulings::record(&mut record.rulings, &new, &name, now)
                })??;
                self.log_hub_event(
                    "ruling.recorded",
                    Some(caller),
                    serde_json::json!({
                        "ruling": ruling.id,
                        "topic": ruling.topic,
                        "text": ruling.text,
                        "replaced": replaced.as_ref().map(|r| &r.id),
                    }),
                );
                match replaced {
                    Some(old) => format!("Recorded {} (replaces {}).", ruling.id, old.id),
                    None => format!("Recorded {}.", ruling.id),
                }
            }
            RulingAction::Retire(which) => {
                let gone = self.store().update_session(caller, |record| {
                    rulings::retire(&mut record.rulings, &which)
                })??;
                self.log_hub_event(
                    "ruling.retired",
                    Some(caller),
                    serde_json::json!({ "ruling": gone.id, "topic": gone.topic, "text": gone.text }),
                );
                format!("Retired {}.", gone.id)
            }
            RulingAction::List => String::new(),
        };
        let held = self.rulings(caller);
        let list = if held.is_empty() {
            "You hold no rulings.".to_owned()
        } else {
            let mut lines = vec![format!("Your rulings ({}):", held.len())];
            lines.extend(held.iter().map(|r| {
                rulings::lines(std::slice::from_ref(r))
                    .into_iter()
                    .next()
                    .unwrap_or_default()
            }));
            lines.join("\n")
        };
        Ok(if said.is_empty() {
            list
        } else {
            format!("{said}\n{list}")
        })
    }

    /// The rulings block of a brief for session `id`: a heading and one
    /// line per ruling; empty when it holds none.
    pub(crate) fn ruling_lines(&self, id: &str, heading: &str) -> Vec<String> {
        let held = self.rulings(id);
        if held.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![heading.to_owned()];
        lines.extend(rulings::lines(&held));
        lines
    }

    /// Give `to` the rulings of `from` it does not already hold (a
    /// handoff); best effort.
    pub(crate) fn inherit_rulings(&self, from: &str, to: &str) {
        let inherited = self.rulings(from);
        if inherited.is_empty() {
            return;
        }
        match self.store().update_session(to, |record| {
            rulings::inherit(&mut record.rulings, &inherited)
        }) {
            Ok(added) if added > 0 => self.log_hub_event(
                "ruling.inherited",
                Some(to),
                serde_json::json!({ "from": from, "count": added }),
            ),
            Ok(_) => {}
            Err(e) => eprintln!("fw hub: rulings of {from}: not passed on: {e}"),
        }
    }
}
