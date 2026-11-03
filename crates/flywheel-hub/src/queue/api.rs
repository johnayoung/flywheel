//! The hub's queue calls: typing and holds, views, and the edits the API
//! and CLI make.

use super::deciding::{Decision, Facts, Presence, decide, exit_text};
use super::runtime::{SessionHold, lock};
use super::{
    ItemEdit, ItemState, ItemView, MAX_PENDING, NewItem, QueueBrief, QueueItem, QueueView,
    SessionQueue, parse_when, validate_text,
};
use crate::hub::{Hub, HubError, HubEvent};
use crate::state::{SessionRecord, SessionState, StateError, new_id};
use crate::status::AgentStatus;
use chrono::Utc;

impl Hub {
    /// Record that input was typed into session `id` (a browser, `fw hub
    /// attach`, `fw hub send`, Compose, any paste the hub makes): its time,
    /// and what it leaves unsent in the agent's input line -- except keys
    /// answering a prompt the agent blocked on (a permission dialog), which
    /// never reach that line. A lone focus report is not typing. Call it
    /// only for a session that exists.
    pub fn note_typing(&self, id: &str, bytes: &[u8]) {
        if bytes.is_empty() || bytes == b"\x1b[I" || bytes == b"\x1b[O" {
            return;
        }
        let blocked = self
            .agent_status(id)
            .is_some_and(|s| s.status == AgentStatus::Blocked);
        self.queue_runtime().note(id, bytes, !blocked);
    }

    /// Keep pending item `item` of session `id` back while an editor has
    /// it open (renewed by the editor; it lapses after [`EDIT_LEASE`]).
    pub fn queue_editing(&self, id: &str, item: &str, editing: bool) -> Result<(), HubError> {
        self.queue_record(id)?;
        let item_id = {
            let store = self.store();
            let queue = store.queue(id).cloned().unwrap_or_default();
            Self::queue_item_id(&queue, item)?
        };
        self.queue_runtime().set_editing(&item_id, editing);
        Ok(())
    }

    /// Hold sessions `ids` for `what` (a review action) until the guard
    /// drops: their queues deliver nothing meanwhile.
    pub fn hold_sessions(&self, ids: Vec<String>, what: &str) -> SessionHold<'_> {
        let runtime = self.queue_runtime();
        let mut holds = lock(&runtime.holds);
        holds.next += 1;
        let key = holds.next;
        holds.held.insert(key, (ids, what.to_owned()));
        SessionHold { runtime, key }
    }

    /// Hold every agent session working in `dir` (or under it).
    pub fn hold_dir(&self, dir: &std::path::Path, what: &str) -> SessionHold<'_> {
        let ids = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.kind.is_agent() && s.cwd.starts_with(dir))
            .map(|s| s.id.clone())
            .collect();
        self.hold_sessions(ids, what)
    }

    /// A session's queue summary (`None`: nothing queued, delivered, or
    /// stalled).
    pub(crate) fn queue_brief(&self, id: &str) -> Option<QueueBrief> {
        let store = self.store();
        let queue = store.queue(id)?;
        Some(QueueBrief {
            pending: queue.pending_count(),
            paused: queue.paused,
            stalled: queue.stalled.clone(),
            next_when: queue.head().map(|i| i.when),
            last_delivered_at: queue.last_delivered_at,
        })
    }

    fn queue_record(&self, id: &str) -> Result<SessionRecord, HubError> {
        let record = self.store().session(id)?.clone();
        if !record.kind.is_agent() {
            return Err(StateError::Invalid(format!(
                "{} is a {} session: prompts queue for Claude Code and Codex sessions (fw hub send types into any)",
                record.label(),
                record.kind.as_str()
            ))
            .into());
        }
        Ok(record)
    }

    /// Whether the session is one of a fan-out's candidates with a grading
    /// queued or running.
    fn grading_owns(&self, id: &str) -> bool {
        self.store().fanouts().iter().any(|f| {
            f.candidates
                .iter()
                .any(|c| c.session_id == id && c.pending.is_some())
        })
    }

    fn presence(&self, record: &SessionRecord) -> Presence {
        match &record.state {
            SessionState::Running => match self.live_session(&record.id) {
                Some(pty) => match pty.exit_info() {
                    Some(exit) => {
                        Presence::Gone(format!("its agent exited ({})", exit_text(&exit)))
                    }
                    None if record.runs_agent() => Presence::Ready,
                    None => Presence::Gone(format!(
                        "{} came back as a plain shell after a restart: relaunch it and the queue carries on",
                        record.kind.as_str()
                    )),
                },
                None => Presence::Starting("its terminal is not up yet"),
            },
            SessionState::Exited { exit, .. } => Presence::Gone(format!(
                "its agent exited ({}): relaunch it and the queue carries on",
                exit_text(exit)
            )),
            SessionState::Stopped {
                clean: true,
                restore_error: None,
                ..
            } => Presence::Starting("waiting for the hub to restore it"),
            SessionState::Stopped {
                restore_error: Some(why),
                ..
            } => Presence::Gone(format!("it could not be restored: {why}")),
            SessionState::Stopped { .. } => Presence::Gone(
                "it is stopped (not restored): relaunch it and the queue carries on".into(),
            ),
        }
    }

    pub(crate) fn queue_facts(&self, record: &SessionRecord, queue: &SessionQueue) -> Facts {
        let runtime = self.queue_runtime();
        let owner = runtime.holder(&record.id).or_else(|| {
            self.grading_owns(&record.id)
                .then(|| "a fan-out grading".to_owned())
        });
        Facts {
            now: Utc::now(),
            presence: self.presence(record),
            status: if record.runs_agent() {
                self.agent_status(&record.id)
            } else {
                None
            },
            typed_ago: runtime.typed_ago(&record.id),
            draft: runtime.draft_chars(&record.id),
            editing: queue.head().is_some_and(|h| runtime.is_editing(&h.id)),
            quiet: self.tunables().queue_typing_quiet,
            owner,
        }
    }

    /// Session `id`'s queue.
    pub fn queue_view(&self, id: &str) -> Result<QueueView, HubError> {
        let record = self.queue_record(id)?;
        let queue = self.store().queue(id).cloned().unwrap_or_default();
        let waiting = if queue.head().is_none() {
            None
        } else {
            match decide(&queue, &self.queue_facts(&record, &queue)) {
                Decision::Wait(why) => Some(why),
                Decision::Stall(why) => Some(format!("about to stall: {why}")),
                Decision::NoteBlocked { .. } => {
                    Some("blocked on a prompt: never sent until it is answered".into())
                }
                Decision::Deliver(_) => Some("ready: it goes within a moment".into()),
            }
        };
        let mut position = 0;
        let items = queue
            .items
            .iter()
            .map(|item| ItemView {
                position: (!item.is_delivered()).then(|| {
                    position += 1;
                    position
                }),
                item: item.clone(),
            })
            .collect();
        Ok(QueueView {
            session_id: record.id.clone(),
            session: record.name.clone(),
            paused: queue.paused,
            stalled: queue.stalled.clone(),
            last_delivered_at: queue.last_delivered_at,
            pending: queue.pending_count(),
            items,
            waiting,
            typing_quiet_secs: self.tunables().queue_typing_quiet.as_secs(),
        })
    }

    /// Every session with a queue.
    pub fn queue_views(&self) -> Vec<QueueView> {
        let ids: Vec<String> = self.store().queues().keys().cloned().collect();
        ids.iter()
            .filter_map(|id| self.queue_view(id).ok())
            .collect()
    }

    pub(crate) fn queue_changed(&self) {
        self.emit(HubEvent::Sessions);
    }

    /// Queue `item` at the end of session `id`'s queue.
    pub fn queue_add(&self, id: &str, item: &NewItem) -> Result<(QueueItem, QueueView), HubError> {
        let record = self.queue_record(id)?;
        let text = validate_text(&item.text)?;
        let when = parse_when(item.when.as_deref())?;
        let new = QueueItem {
            id: new_id(),
            text,
            when,
            added_at: Utc::now(),
            blocked_at: None,
            state: ItemState::Pending,
        };
        let added = new.clone();
        let label = record.label();
        self.store().update_queue(id, move |queue| {
            if queue.pending_count() >= MAX_PENDING {
                return Err(StateError::Conflict(format!(
                    "{label} has {MAX_PENDING} prompts queued already: remove some first"
                )));
            }
            queue.items.push(new);
            Ok(())
        })??;
        eprintln!(
            "fw hub: queue {id}: added {} ({}, {} bytes)",
            &added.id[..8],
            added.when.as_str(),
            added.text.len()
        );
        // An agent that is not there (exited, or back as a plain shell
        // after a restart) stalls the queue now, so the answer says why
        // nothing will be sent, rather than on the next tick.
        let (stalled, head) = self.store().queue(id).map_or((false, None), |q| {
            (q.stalled.is_some(), q.head().map(|i| i.id.clone()))
        });
        if !stalled && let Presence::Gone(why) = self.presence(&record) {
            self.stall_queue(id, &why, head.as_deref());
        }
        self.queue_changed();
        Ok((added, self.queue_view(id)?))
    }

    /// Resolve an item reference: an id or unique id prefix (4+ chars), or
    /// a 1-based position among the pending items.
    fn queue_item_id(queue: &SessionQueue, reference: &str) -> Result<String, StateError> {
        let reference = reference.trim().trim_start_matches('#');
        if !reference.is_empty()
            && reference.len() <= 3
            && reference.chars().all(|c| c.is_ascii_digit())
        {
            let n: usize = reference.parse().unwrap_or(0);
            return queue
                .waiting()
                .nth(n.wrapping_sub(1))
                .map(|i| i.id.clone())
                .ok_or_else(|| {
                    StateError::NotFound(format!(
                        "queue position {n} ({} pending)",
                        queue.pending_count()
                    ))
                });
        }
        if reference.len() < 4 {
            return Err(StateError::Invalid(
                "name an item by its id (4+ characters) or its position".into(),
            ));
        }
        let found: Vec<&QueueItem> = queue
            .items
            .iter()
            .filter(|i| i.id.starts_with(reference))
            .collect();
        match found.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(StateError::NotFound(format!("queue item {reference}"))),
            _ => Err(StateError::Invalid(format!(
                "queue item {reference} is ambiguous: give more of its id"
            ))),
        }
    }

    /// Edit pending item `reference`'s text and/or trigger.
    pub fn queue_edit(
        &self,
        id: &str,
        reference: &str,
        edit: &ItemEdit,
    ) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        let text = edit.text.as_deref().map(validate_text).transpose()?;
        let when = edit
            .when
            .as_deref()
            .map(|w| parse_when(Some(w)))
            .transpose()?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            let item = queue.item_mut(&item_id).expect("resolved");
            if !matches!(item.state, ItemState::Pending) {
                return Err(StateError::Conflict(
                    "that prompt was sent already: it cannot change".into(),
                ));
            }
            if let Some(text) = text {
                item.text = text;
            }
            if let Some(when) = when
                && when != item.when
            {
                item.when = when;
                item.blocked_at = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Remove item `reference` (pending or delivered; not one being sent).
    pub fn queue_remove(&self, id: &str, reference: &str) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            if queue
                .items
                .iter()
                .any(|i| i.id == item_id && matches!(i.state, ItemState::Sending { .. }))
            {
                return Err(StateError::Conflict(
                    "that prompt is being delivered right now".into(),
                ));
            }
            queue.items.retain(|i| i.id != item_id);
            if queue
                .stalled
                .as_ref()
                .is_some_and(|s| s.item.as_deref() == Some(item_id.as_str()))
                && queue.head().is_none()
            {
                queue.stalled = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Remove every pending item (`all`: the delivered history too).
    pub fn queue_clear(&self, id: &str, all: bool) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            queue.items.retain(|i| match i.state {
                ItemState::Sending { .. } => true,
                ItemState::Delivered { .. } => !all,
                ItemState::Pending => false,
            });
            queue.stalled = None;
        })?;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Move pending item `reference` to 1-based `position` among the
    /// pending items (clamped; never ahead of one being sent).
    pub fn queue_move(
        &self,
        id: &str,
        reference: &str,
        position: usize,
    ) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            let Some(from) = queue.items.iter().position(|i| i.id == item_id) else {
                return Err(StateError::NotFound(format!("queue item {reference}")));
            };
            if !matches!(queue.items[from].state, ItemState::Pending) {
                return Err(StateError::Conflict(
                    "only prompts still waiting can move".into(),
                ));
            }
            let old_head = queue.head().map(|i| i.id.clone());
            let item = queue.items.remove(from);
            // Pending slots start after the delivered and sending items.
            let first = queue
                .items
                .iter()
                .position(|i| matches!(i.state, ItemState::Pending))
                .unwrap_or(queue.items.len());
            let pending = queue.items.len() - first;
            let at = first + position.saturating_sub(1).min(pending);
            queue.items.insert(at, item);
            queue.settle_order();
            // An item that lost the front: a block it saw no longer counts.
            if let Some(old) = old_head
                && queue.head().is_some_and(|h| h.id != old)
                && let Some(item) = queue.item_mut(&old)
            {
                item.blocked_at = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Pause (`true`) or resume session `id`'s queue. Resuming also
    /// clears a stall (the next item is tried again).
    pub fn queue_set_paused(&self, id: &str, paused: bool) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            queue.paused = paused;
            if !paused {
                queue.stalled = None;
            }
        })?;
        eprintln!(
            "fw hub: queue {id}: {}",
            if paused { "paused" } else { "resumed" }
        );
        self.queue_changed();
        self.queue_view(id)
    }
}
