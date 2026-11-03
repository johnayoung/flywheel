//! Deciding: whether a session's head item may go now, from the facts
//! the hub gathers.

use super::{ACK_TIMEOUT, EVENT_TEXT_CHARS, ItemState, QueueItem, SETTLE, SessionQueue, When};
use crate::pty::ExitInfo;
use crate::status::{AgentStatus, StatusView};
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Whether the session can take a prompt at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Its agent runs.
    Ready,
    /// It will (a restore or a launch is under way): wait.
    Starting(&'static str),
    /// It will not without someone acting: stall.
    Gone(String),
}

/// What the head item is decided on.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub now: DateTime<Utc>,
    pub presence: Presence,
    /// Its agent status, a finished turn counted as seen once any client
    /// viewed it.
    pub status: Option<StatusView>,
    /// Since input was last typed into it.
    pub typed_ago: Option<Duration>,
    /// Characters typed into its input line and never sent.
    pub draft: usize,
    /// The head item is open in an editor.
    pub editing: bool,
    pub quiet: Duration,
    /// What holds it now (a review action, a fan-out grading).
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Deliver(String),
    Wait(String),
    Stall(String),
    /// The agent is blocked with this `blocked-cleared` item at the front:
    /// record when (then wait).
    NoteBlocked {
        item: String,
        at: DateTime<Utc>,
    },
}

fn secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_millis() >= 500)
}

/// What to do about `queue`'s head item now. Pure.
pub(crate) fn decide(queue: &SessionQueue, facts: &Facts) -> Decision {
    let wait = |why: &str| Decision::Wait(why.to_owned());
    if let Some(stall) = &queue.stalled {
        return Decision::Wait(format!("stalled: {}", stall.reason));
    }
    if queue.paused {
        return wait("paused");
    }
    let Some(head) = queue.head() else {
        return wait("nothing queued");
    };
    if matches!(head.state, ItemState::Sending { .. }) {
        return wait("being delivered");
    }
    match &facts.presence {
        Presence::Gone(why) => return Decision::Stall(why.clone()),
        Presence::Starting(why) => return wait(why),
        Presence::Ready => {}
    }
    let Some(status) = &facts.status else {
        return wait("waiting for the agent to report a status");
    };
    match status.status {
        AgentStatus::Blocked => {
            if head.when == When::BlockedCleared && head.blocked_at.is_none() {
                return Decision::NoteBlocked {
                    item: head.id.clone(),
                    at: status.since,
                };
            }
            return wait("blocked on a prompt: never sent until it is answered");
        }
        AgentStatus::Working => return wait("working: waiting for its turn to finish"),
        AgentStatus::Unknown => return wait("waiting for the agent to report a status"),
        AgentStatus::Done | AgentStatus::Idle => {}
    }
    if let Some(last) = queue.last_delivered_at
        && status.since <= last
    {
        let ago = (facts.now - last).to_std().unwrap_or_default();
        if ago >= ACK_TIMEOUT {
            return Decision::Stall(format!(
                "the agent did not start a turn in the {}s after the last delivered prompt",
                ACK_TIMEOUT.as_secs()
            ));
        }
        return wait("waiting for the agent to take up the last delivered prompt");
    }
    if head.when == When::Idle && status.status != AgentStatus::Idle {
        return wait("finished: waiting until someone has looked at its answer (idle)");
    }
    if head.when == When::BlockedCleared {
        match head.blocked_at {
            None => return wait("waiting for the agent to block on a prompt (blocked-cleared)"),
            Some(at) if status.since <= at => {
                return wait("waiting for the turn after the block to finish (blocked-cleared)");
            }
            Some(_) => {}
        }
    }
    let rested = (facts.now - status.since).to_std().unwrap_or_default();
    if rested < SETTLE {
        return wait("just finished: settling");
    }
    if let Some(owner) = &facts.owner {
        return Decision::Wait(format!("{owner} is using the session"));
    }
    if let Some(ago) = facts.typed_ago
        && ago < facts.quiet
    {
        return Decision::Wait(format!(
            "someone typed {}s ago: waiting for {}s without typing",
            secs(ago),
            facts.quiet.as_secs()
        ));
    }
    if facts.draft > 0 {
        return Decision::Wait(format!(
            "{} character{} typed into its input never sent: send or clear them (Ctrl-U), then it goes",
            facts.draft,
            if facts.draft == 1 { "" } else { "s" }
        ));
    }
    if facts.editing {
        return wait("the next prompt is open in an editor");
    }
    Decision::Deliver(head.id.clone())
}

pub(crate) fn exit_text(exit: &ExitInfo) -> String {
    match (&exit.signal, exit.code) {
        (Some(signal), _) => format!("killed by {signal}"),
        (None, Some(code)) => format!("exit code {code}"),
        (None, None) => "exit code unknown".to_owned(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// An item in a plugin event (`queue.delivered`, `queue.stalled`): one
/// shape for both.
pub(crate) fn event_item(
    item: &QueueItem,
    delivered_at: Option<DateTime<Utc>>,
) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "text": one_line(&item.text, EVENT_TEXT_CHARS),
        "bytes": item.text.len(),
        "when": item.when,
        "added_at": item.added_at,
        "delivered_at": delivered_at,
    })
}

/// Text compared for a reconcile: whitespace runs as one space.
pub(crate) fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
