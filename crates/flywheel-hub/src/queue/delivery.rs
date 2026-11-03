//! Delivery: reconcile what a dead hub left `sending`, the tick, stalls
//! and the paste itself.

use super::deciding::{Decision, decide, event_item, normalized};
use super::runtime::Delivering;
use super::{ItemState, RECONCILE_SKEW, SessionQueue, Stall, TICK};
use crate::agentlog::{self, Event, Role};
use crate::hub::Hub;
use crate::state::SessionRecord;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Hub {
    /// The queue's start: settle items a dead hub left `sending`, then
    /// look at the queues every [`TICK`] until the hub stops. Once per
    /// hub; a live handoff's new hub starts it at commit.
    pub(crate) fn start_queues(self: &Arc<Self>) {
        if self.queue_runtime().started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_queues();
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-queue".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    if !hub.queue_runtime().is_paused() {
                        hub.queue_tick();
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: prompt queues are off: cannot start their thread: {e}");
        }
    }

    /// Items left `sending` by a hub that stopped mid-delivery: delivered
    /// when the agent's transcript has the text as a user message from
    /// then on (never typed again), else back to pending (sent again).
    fn reconcile_queues(&self) {
        let sending: Vec<(String, String, String, DateTime<Utc>)> = self
            .store()
            .queues()
            .iter()
            .flat_map(|(sid, q)| {
                q.items.iter().filter_map(move |i| match i.state {
                    ItemState::Sending { sending_at } => {
                        Some((sid.clone(), i.id.clone(), i.text.clone(), sending_at))
                    }
                    _ => None,
                })
            })
            .collect();
        for (sid, item_id, text, sending_at) in sending {
            let record = self.store().session(&sid).ok().cloned();
            let found = record
                .as_ref()
                .is_some_and(|r| self.transcript_has(r, &text, sending_at));
            let result = self.store().update_queue(&sid, |queue| {
                if let Some(item) = queue.item_mut(&item_id) {
                    item.state = if found {
                        ItemState::Delivered {
                            delivered_at: sending_at,
                            reconciled: true,
                        }
                    } else {
                        ItemState::Pending
                    };
                }
                if found && queue.last_delivered_at.is_none_or(|t| t < sending_at) {
                    queue.last_delivered_at = Some(sending_at);
                }
                queue.settle_order();
            });
            eprintln!(
                "fw hub: queue {sid}: item {} was being sent when the hub stopped: {}{}",
                &item_id[..item_id.len().min(8)],
                if found {
                    "its transcript has it, so it is marked delivered (not sent again)"
                } else {
                    "its transcript does not have it, so it will be sent again"
                },
                match result {
                    Ok(()) => String::new(),
                    Err(e) => format!(" (not saved: {e})"),
                }
            );
        }
    }

    /// Whether `record`'s agent transcript holds a user message that is
    /// `text` (whitespace runs aside) written from about `since` on.
    fn transcript_has(&self, record: &SessionRecord, text: &str, since: DateTime<Utc>) -> bool {
        let Some((path, format)) = self.command_book().transcript_file(record) else {
            return false;
        };
        let needle = normalized(text);
        let earliest = since - RECONCILE_SKEW;
        let mut tail = agentlog::JsonlTail::new(path);
        let mut found = false;
        let _ = tail.read(None, &mut |read| {
            let agentlog::Read::Line { bytes, start } = read else {
                return;
            };
            if found {
                return;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                return;
            };
            let line = agentlog::parse(format, &value, start);
            if let Some(at) = line
                .timestamp
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                && at.with_timezone(&Utc) < earliest
            {
                return;
            }
            found = line.events.iter().any(|event| match event {
                Event::Text {
                    role: Role::User,
                    parts,
                    ..
                } => {
                    let joined: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
                    // The whole message, not a part of one: a short
                    // prompt ("continue") inside another is not it.
                    normalized(&joined.join("\n")) == needle
                }
                _ => false,
            });
        });
        found
    }

    /// One look at every queue with something to send.
    fn queue_tick(&self) {
        let candidates: Vec<(SessionQueue, SessionRecord)> = {
            let store = self.store();
            store
                .queues()
                .iter()
                .filter(|(_, q)| q.head().is_some() && !q.paused && q.stalled.is_none())
                .filter_map(|(id, q)| Some((q.clone(), store.session(id).ok()?.clone())))
                .collect()
        };
        for (queue, record) in candidates {
            let facts = self.queue_facts(&record, &queue);
            match decide(&queue, &facts) {
                Decision::Wait(_) => {}
                Decision::NoteBlocked { item, at } => {
                    let _ = self.store().update_queue(&record.id, |q| {
                        if let Some(i) = q.item_mut(&item) {
                            i.blocked_at.get_or_insert(at);
                        }
                    });
                }
                Decision::Stall(reason) => {
                    let item = queue.head().map(|i| i.id.clone());
                    self.stall_queue(&record.id, &reason, item.as_deref());
                }
                Decision::Deliver(item) => self.deliver(&record.id, &item),
            }
        }
    }

    pub(crate) fn stall_queue(&self, id: &str, reason: &str, item: Option<&str>) {
        let stall = Stall {
            at: Utc::now(),
            reason: reason.to_owned(),
            item: item.map(str::to_owned),
        };
        let saved = self.store().update_queue(id, |queue| {
            queue.stalled = Some(stall.clone());
            let item = item
                .and_then(|i| queue.items.iter().find(|q| q.id == i))
                .cloned();
            (queue.pending_count(), item)
        });
        let (pending, item) = match saved {
            Ok(saved) => saved,
            Err(e) => {
                eprintln!("fw hub: queue {id}: stalling: {e}");
                return;
            }
        };
        eprintln!("fw hub: queue {id}: stalled: {reason}");
        self.queue_changed();
        if let Some(session) = self.event_session(id) {
            self.emit_lifecycle(
                "queue.stalled",
                serde_json::json!({
                    "session": session,
                    "reason": reason,
                    "item": item.as_ref().map(|i| event_item(i, None)),
                    "pending": pending,
                }),
            );
        }
    }

    /// Deliver item `item_id` of session `id`: `sending` saved, the paste,
    /// then `delivered` saved (or the item back to pending and the queue
    /// stalled when the paste was refused).
    fn deliver(&self, id: &str, item_id: &str) {
        let runtime = self.queue_runtime();
        if !runtime.begin_delivery() {
            return;
        }
        let _delivering = Delivering(runtime);
        // No other paste writes from here to the Enter, and the decision is
        // taken again under the turn: a paste that noted its typing while
        // this one waited for the turn keeps the item back.
        let _turn = runtime.paste_turn();
        let fresh = {
            let store = self.store();
            store
                .queue(id)
                .cloned()
                .zip(store.session(id).ok().cloned())
        };
        let Some((queue, record)) = fresh else {
            return;
        };
        if decide(&queue, &self.queue_facts(&record, &queue)) != Decision::Deliver(item_id.into()) {
            return;
        }
        let now = Utc::now();
        let claimed = self.store().update_queue(id, |queue| {
            if queue.paused || queue.stalled.is_some() {
                return None;
            }
            let head = queue.head_mut()?;
            if head.id != item_id || !matches!(head.state, ItemState::Pending) {
                return None;
            }
            head.state = ItemState::Sending { sending_at: now };
            Some(head.clone())
        });
        let item = match claimed {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(e) => {
                eprintln!("fw hub: queue {id}: not delivered: cannot record it: {e}");
                return;
            }
        };
        let pasted = self.paste_turn_held(id, &item.text, true, false);
        let delivered_at = Utc::now();
        match pasted {
            Ok(_) => {
                // Kept in memory even when the save fails: the paste
                // happened, and the file's `sending` is settled from the
                // transcript on the next start.
                let saved = self.store().update_queue_keeping(id, |queue| {
                    if let Some(i) = queue.item_mut(item_id) {
                        i.state = ItemState::Delivered {
                            delivered_at,
                            reconciled: false,
                        };
                    }
                    queue.last_delivered_at = Some(delivered_at);
                    queue.settle_order();
                    queue.trim_history();
                    queue.pending_count()
                });
                let pending = match saved {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("fw hub: queue {id}: delivered, but not recorded: {e}");
                        0
                    }
                };
                eprintln!(
                    "fw hub: queue {id}: delivered {} ({} bytes, {}); {pending} pending",
                    &item_id[..item_id.len().min(8)],
                    item.text.len(),
                    item.when.as_str()
                );
                self.queue_changed();
                if let Some(session) = self.event_session(id) {
                    self.emit_lifecycle(
                        "queue.delivered",
                        serde_json::json!({
                            "session": session,
                            "item": event_item(&item, Some(delivered_at)),
                            "pending": pending,
                        }),
                    );
                }
            }
            Err(e) => {
                // Nothing was typed: pending again, in memory even when
                // the save fails (the file's `sending` is settled from the
                // transcript on the next start).
                if let Err(e) = self.store().update_queue_keeping(id, |queue| {
                    if let Some(i) = queue.item_mut(item_id) {
                        i.state = ItemState::Pending;
                    }
                }) {
                    eprintln!("fw hub: queue {id}: not recorded: {e}");
                }
                self.stall_queue(id, &format!("delivery failed: {e}"), Some(item_id));
            }
        }
    }
}
