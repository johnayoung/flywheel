//! Session labels on the hub (see [`crate::labels`]): set by the operator,
//! agents, and the hub itself (`fw.*`), each change saved on the session
//! record and recorded in its telemetry log.

use super::{Hub, HubError, HubEvent};
use crate::labels::{self, Change, Cond, LabelError, Labels};
use crate::state::{SessionRecord, StateError};
use crate::telemetry::{Draft, What, now_ms};

impl From<LabelError> for HubError {
    fn from(e: LabelError) -> Self {
        match e {
            LabelError::Invalid(why) => StateError::Invalid(why).into(),
            LabelError::Condition(why) => StateError::Conflict(why).into(),
        }
    }
}

/// The `fw.*` labels a new session starts with, from its record.
pub(super) fn seed(record: &mut SessionRecord) {
    let labels = &mut record.labels;
    labels.insert(labels::KIND.to_owned(), record.kind.as_str().to_owned());
    if let Some(parent) = &record.opened_by {
        labels.insert(labels::PARENT.to_owned(), parent.clone());
    }
    if let Some(branch) = record
        .worktree
        .as_ref()
        .map(|w| w.branch.as_str())
        .filter(|b| labels::validate_value(labels::CHANGE, b).is_ok())
    {
        labels.insert(labels::CHANGE.to_owned(), branch.to_owned());
    }
}

impl Hub {
    /// Change session `id`'s labels (`hub`: `fw.*` keys, set by the hub;
    /// else none of them), when `cond` holds; record what changed. The
    /// labels it holds after.
    pub fn set_labels(
        &self,
        id: &str,
        change: &Change,
        cond: Option<&Cond>,
        hub: bool,
    ) -> Result<Labels, HubError> {
        let (before, after) = self.store().update_session(id, |record| {
            let mut labels = record.labels.clone();
            let before = labels::apply(&mut labels, change, cond, hub)?;
            record.labels.clone_from(&labels);
            Ok::<_, LabelError>((before, labels))
        })??;
        if !before.is_empty() {
            let set: Change = before
                .keys()
                .map(|k| (k.clone(), after.get(k).cloned()))
                .collect();
            self.record_labels(id, set, before);
            self.emit(HubEvent::Sessions);
        }
        Ok(after)
    }

    /// Point every session's `fw.parent` at its effective parent (see
    /// [`super::endings::effective_parent`]): after a give, a successor,
    /// an end, a removal, and once at open (records from before owners
    /// or labels existed).
    pub(crate) fn refresh_parents(&self) {
        let stale: Vec<(String, Option<String>)> = {
            let store = self.store();
            let sessions = store.sessions();
            sessions
                .iter()
                .filter_map(|record| {
                    let parent = super::endings::effective_parent(record, sessions);
                    (record.labels.get(labels::PARENT) != parent.as_ref())
                        .then(|| (record.id.clone(), parent))
                })
                .collect()
        };
        for (id, parent) in stale {
            self.set_hub_label(&id, labels::PARENT, parent.as_deref());
        }
    }

    /// Set one `fw.*` label of session `id` (the hub's own; `None`
    /// unsets it), quietly: a session gone or a refused value is logged.
    pub(crate) fn set_hub_label(&self, id: &str, key: &str, value: Option<&str>) {
        let unchanged = self
            .store()
            .session(id)
            .is_ok_and(|r| r.labels.get(key).map(String::as_str) == value);
        if unchanged {
            return;
        }
        let change: Change = [(key.to_owned(), value.map(str::to_owned))].into();
        if let Err(e) = self.set_labels(id, &change, None, true) {
            eprintln!("fw hub: session {id}: label {key}: {e}");
        }
    }

    /// Record a label change in the session's telemetry log.
    pub(super) fn record_labels(&self, id: &str, set: Change, before: Change) {
        self.telemetry().record(
            id,
            vec![Draft::new(now_ms(), What::LabelsChanged { set, before })],
        );
    }

    /// The live value of `fw.state` for each of `ids`: its fleet status,
    /// when its agent has reported anything.
    pub fn fleet_states(&self, ids: &[&str]) -> std::collections::HashMap<String, String> {
        self.telemetry()
            .refinements(ids)
            .into_iter()
            .map(|(id, (status, ..))| (id, status.to_owned()))
            .collect()
    }

    /// The sessions (of `workspace_id`, else all) whose labels hold every
    /// term, oldest first.
    pub fn sessions_labelled(
        &self,
        workspace_id: Option<&str>,
        terms: &[labels::Term],
    ) -> Vec<super::SessionView> {
        let records: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| workspace_id.is_none_or(|w| s.workspace_id == w))
            .cloned()
            .collect();
        let live = terms.iter().any(|t| t.key == labels::STATE);
        let states = if live {
            let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
            self.fleet_states(&ids)
        } else {
            std::collections::HashMap::new()
        };
        records
            .into_iter()
            .filter(|r| labels::matches(terms, &r.labels, states.get(&r.id).map(String::as_str)))
            .map(|r| self.view(r))
            .collect()
    }

    /// The `label` agent tool: change the labels of session `to` (the
    /// caller itself when `None`); what to tell the agent.
    pub fn agent_label(
        &self,
        caller: &str,
        to: Option<&str>,
        change: &Change,
        cond: Option<&Cond>,
    ) -> Result<String, HubError> {
        let target = match to.map(str::trim).filter(|t| !t.is_empty()) {
            Some(to) => self.session_named(to)?,
            None => self.store().session(caller)?.clone(),
        };
        let labels = self.set_labels(&target.id, change, cond, false)?;
        let shown: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
        Ok(format!(
            "{} (id {}) labels: {}",
            target.name,
            target.id,
            if shown.is_empty() {
                "none".to_owned()
            } else {
                shown.join(" ")
            }
        ))
    }
}
