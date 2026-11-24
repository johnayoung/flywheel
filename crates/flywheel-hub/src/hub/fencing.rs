//! One lead per workstream: once a session's work passes to a running
//! successor (a handoff or a replacement: the successor `continues` it),
//! the predecessor is fenced. The hub refuses its calls that act on the
//! work (messaging others, starting, ending, or giving sessions, reviews,
//! watches, asks, rulings) and names the successor instead, and messages
//! agents send it go to the successor. A deposed leader's writes are
//! refused by the resource, as fencing tokens do in lock services.

use super::Hub;
use crate::state::{SessionRecord, SessionState};
use crate::telemetry::mcp;

/// Tools a fenced session may still call: reading, its own plan and
/// queue, its labels, and proposing another handoff (a split; the
/// operator starts it or not).
const OPEN_TOOLS: &[&str] = &[
    mcp::TOOL,
    mcp::HANDOFF_TOOL,
    mcp::LIST_TOOL,
    mcp::STATUS_TOOL,
    mcp::QUEUED_TOOL,
    mcp::CANCEL_TOOL,
    mcp::UNWATCH_TOOL,
    mcp::CHECK_REVIEWED_TOOL,
    mcp::LABEL_TOOL,
];

impl Hub {
    /// The running session that took over `id`'s work, newest first.
    pub(crate) fn successor_of(&self, id: &str) -> Option<SessionRecord> {
        self.store()
            .sessions()
            .iter()
            .filter(|s| {
                s.continues.as_deref() == Some(id) && s.state == SessionState::Running && s.id != id
            })
            .max_by_key(|s| s.created_at)
            .cloned()
    }

    /// Why agent session `caller` may not call `tool` with `args`, when
    /// its work passed to a running successor: a message to that
    /// successor and an end of itself still go through.
    pub fn fenced(&self, caller: &str, tool: &str, args: &serde_json::Value) -> Option<String> {
        if OPEN_TOOLS.contains(&tool) {
            return None;
        }
        let successor = self.successor_of(caller)?;
        let names = |key: &str, id: &str| {
            let to = args.get(key).and_then(serde_json::Value::as_str)?.trim();
            let store = self.store();
            let found = crate::script::find_by(
                store.sessions(),
                |s| (s.id.as_str(), s.name.as_str()),
                to,
                "session",
            )
            .ok()
            .flatten()?;
            Some(found.id == id)
        };
        let allowed = match tool {
            t if t == mcp::MESSAGE_TOOL => names("to", &successor.id) == Some(true),
            t if t == mcp::END_TOOL => names("to", caller) == Some(true),
            _ => false,
        };
        (!allowed).then(|| {
            format!(
                "{} (id {}) took over your work: the hub takes {tool} from it, not from you. \
                 Send it anything it still needs with message_session, then end your turn.",
                successor.name, successor.id
            )
        })
    }

    /// Where a message an agent sends to `target` goes: `target`, or the
    /// running successor that took over its work (unless that successor
    /// is the sender). With the note to put before it.
    pub(crate) fn message_target(
        &self,
        target: SessionRecord,
        sender: &str,
    ) -> (SessionRecord, Option<String>) {
        match self.successor_of(&target.id) {
            Some(successor) if successor.id != sender => {
                let note = format!(
                    "(Sent to {}, whose work you took over: the hub passed it on.)",
                    target.name
                );
                (successor, Some(note))
            }
            _ => (target, None),
        }
    }
}
