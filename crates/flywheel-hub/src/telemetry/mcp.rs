//! The hub's agent tools: an MCP server (streamable HTTP, JSON responses
//! only). `report_progress` declares the agent's plan and how far along it
//! is; `ask_operator` declares what it needs from the operator before it
//! can go on (a reply, a decision, a command only they can run);
//! `handoff` proposes continuing the work in a fresh session from a
//! prompt it wrote; `start_session` starts another agent session (within
//! the operator's setting and limits) to take on parallel work. The first
//! two only declare: the hook path records them. The rest act, through a
//! [`Host`]: a handoff kept on the session until the operator starts or
//! dismisses it, a session started (or kept for the operator), the other
//! agent sessions on the hub, one session's status, and a message typed
//! into one (or queued, with priority if asked). Session lifecycle:
//! `end_session` and `replace_session` retire a session the caller owns
//! (else they wait for the operator's approval), `give_session` hands
//! that control on, and `queued_messages` / `cancel_message` show and take
//! back the caller's own messages still waiting in another's queue.
//!
//! Agents rarely keep a plan on their own; the hub gives every Claude
//! Code session it starts this tool and one sentence asking for its use
//! ([`NUDGE`]). The call itself changes nothing here: Claude Code's
//! `PreToolUse` hook carries the same input, with the subagent that made
//! it, into the telemetry log ([`super::claude`]), so a plan is recorded
//! on one path whoever declares it. What is declared is the agent's
//! claim; the board shows it beside the calls the hub observed.

use crate::state::EndMode;
use serde_json::{Value, json};

/// The server's name in an agent's MCP configuration (so the tool is
/// `mcp__flywheel__report_progress` to Claude Code).
pub const SERVER_NAME: &str = "flywheel";
/// The tool's name on the server.
pub const TOOL: &str = "report_progress";
/// The ask tool's name on the server.
pub const ASK_TOOL: &str = "ask_operator";
/// The handoff tool's name on the server.
pub const HANDOFF_TOOL: &str = "handoff";
/// The session list tool's name on the server.
pub const LIST_TOOL: &str = "list_sessions";
/// The message tool's name on the server.
pub const MESSAGE_TOOL: &str = "message_session";
/// The start tool's name on the server.
pub const START_TOOL: &str = "start_session";
/// The status tool's name on the server.
pub const STATUS_TOOL: &str = "session_status";
/// The end tool's name on the server.
pub const END_TOOL: &str = "end_session";
/// The replace tool's name on the server.
pub const REPLACE_TOOL: &str = "replace_session";
/// The queued-messages tool's name on the server.
pub const QUEUED_TOOL: &str = "queued_messages";
/// The cancel tool's name on the server.
pub const CANCEL_TOOL: &str = "cancel_message";
/// The give tool's name on the server.
pub const GIVE_TOOL: &str = "give_session";
/// The label tool's name on the server.
pub const LABEL_TOOL: &str = "label";
/// The watch tools' names on the server.
pub const WATCH_TOOL: &str = "watch";
pub const UNWATCH_TOOL: &str = "unwatch";
/// The lease tool's name on the server.
pub const LEASE_TOOL: &str = "lease";
/// The review record tools' names on the server.
pub const RECORD_REVIEW_TOOL: &str = "record_review";
pub const CHECK_REVIEWED_TOOL: &str = "check_reviewed";
/// The adopt tool's name on the server.
pub const ADOPT_TOOL: &str = "adopt_worktree";

/// What an agent asks the `lease` tool for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseArgs {
    /// acquire, release, or list.
    pub action: String,
    pub resource: Option<String>,
    pub mode: Option<String>,
    pub ttl_secs: Option<u32>,
    pub wait_secs: Option<u32>,
    pub note: Option<String>,
    /// release: a lease id instead of the resource.
    pub lease: Option<String>,
}

/// What an agent asks `start_session` for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartArgs {
    pub name: String,
    pub prompt: String,
    pub folder: Option<String>,
    /// A new worktree's task, and the branch to cut it from.
    pub worktree: Option<(String, Option<String>)>,
    pub model: Option<String>,
}

/// What an agent asks `replace_session` for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceArgs {
    pub to: String,
    pub name: String,
    pub model: Option<String>,
    pub prompt: String,
    pub final_prompt: String,
    pub folder: Option<String>,
    /// A new worktree's task, and the branch to cut it from.
    pub worktree: Option<(String, Option<String>)>,
    pub wait_minutes: Option<u32>,
}

/// What the tools that act do, for the session calling them.
pub trait Host {
    /// The other agent sessions, one line each (`mine`: only those the
    /// caller started).
    fn list_sessions(&self, mine: bool) -> String;
    /// Start a new agent session (or keep the start for the operator);
    /// what to tell the agent.
    fn start_session(&self, args: &StartArgs) -> Result<String, String>;
    /// The session `to` names, as text.
    fn session_status(&self, to: &str) -> Result<String, String>;
    /// Send `text` to the session `to` names (`priority`: ahead of its
    /// waiting messages that are not; `topic`: superseding the caller's
    /// earlier message on it); what became of it.
    fn message(
        &self,
        to: &str,
        text: &str,
        priority: bool,
        topic: Option<&str>,
    ) -> Result<String, String>;
    /// End the session `to` names (or ask the operator to); what to tell
    /// the agent now (the outcome arrives as a message).
    fn end_session(
        &self,
        to: &str,
        mode: EndMode,
        final_prompt: Option<&str>,
        wait_minutes: Option<u32>,
        close: Option<crate::close::CloseChoices>,
        dry_run: bool,
    ) -> Result<String, String>;
    /// Replace the session `to` names with a new one.
    fn replace_session(&self, args: &ReplaceArgs) -> Result<String, String>;
    /// The caller's messages still waiting in `to`'s queue (`None`: in
    /// every queue).
    fn queued_messages(&self, to: Option<&str>) -> Result<String, String>;
    /// Take back one (`item`) or all of the caller's waiting messages.
    fn cancel_message(&self, to: &str, item: Option<&str>) -> Result<String, String>;
    /// Give control of `to` to `new_owner`.
    fn give_session(&self, to: &str, new_owner: &str) -> Result<String, String>;
    /// Change the labels of `to` (the caller when `None`) when `cond`
    /// holds; what to tell the agent.
    fn label(
        &self,
        to: Option<&str>,
        change: &crate::labels::Change,
        cond: Option<&crate::labels::Cond>,
    ) -> Result<String, String>;
    /// Arm a watch that wakes the caller when the log matches `filter`,
    /// or at a time (`at`, `every`).
    fn watch(
        &self,
        filter: &str,
        prompt: &str,
        window_secs: Option<u32>,
        at: Option<&str>,
        every: Option<&str>,
    ) -> Result<String, String>;
    /// Disarm one of the caller's watches (`all`: every one).
    fn unwatch(&self, id: &str) -> Result<String, String>;
    /// Acquire, release, or list leases for the caller.
    fn lease(&self, args: &LeaseArgs) -> Result<String, String>;
    /// Record a review the caller made (or ran).
    fn record_review(&self, input: &crate::hub::reviews::ReviewInput) -> Result<String, String>;
    /// Whether every commit of `range` in `repo` is covered by a READY
    /// review.
    fn check_reviewed(
        &self,
        repo: &str,
        range: &str,
        patch_equivalent: bool,
    ) -> Result<String, String>;
    /// Track a worktree made outside the hub.
    fn adopt_worktree(&self, adopt: &crate::hub::changes::Adopt) -> Result<String, String>;
    /// Keep a proposed handoff for the operator; what to tell the agent.
    fn handoff(
        &self,
        prompt: &str,
        name: Option<&str>,
        reason: Option<&str>,
    ) -> Result<String, String>;
}
/// The protocol revision answered when the client names none.
const PROTOCOL: &str = "2025-06-18";

/// What the hub appends to a session's system prompt.
pub const NUDGE: &str = "This session is shown on its operator's fleet board. When a task takes \
more than one step, call the mcp__flywheel__report_progress tool with your full list of steps \
before you start, and again each time a step starts, finishes, or the plan changes: always the \
whole list, with exactly one step in_progress while you work. Keep step titles short. When you \
cannot go on without the operator (an answer, a decision between options, or a command only they \
can run, such as one needing sudo or another machine), call mcp__flywheel__ask_operator once \
with that ask, then end your turn. When the work should go on in a fresh session, call \
mcp__flywheel__handoff with that session's whole prompt instead of printing one. To delegate \
parallel work to a new agent session, call mcp__flywheel__start_session (handoff stays for \
continuing your own work); to wait on the sessions you started, call mcp__flywheel__watch once \
(for example filter \"children state=done|blocked|failed\") and end your turn instead of polling \
session_status; to retire one, call mcp__flywheel__end_session or \
mcp__flywheel__replace_session, never a shell kill. Do not mention these tools or the board in \
your replies.";

fn tool() -> Value {
    json!({
        "name": TOOL,
        "title": "Report progress",
        "description": "Declare your plan and progress for the operator's fleet board. Send the \
                        complete list of steps every time: when you start multi-step work, and \
                        whenever a step starts, finishes, or the plan changes. Mark exactly one \
                        step in_progress while you work.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "Every step of the plan, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "The step, in a few words." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                        },
                        "required": ["title", "status"]
                    }
                },
                "note": {
                    "type": "string",
                    "description": "Optional: why the plan changed, or what is blocking."
                }
            },
            "required": ["items"]
        }
    })
}

fn ask_tool() -> Value {
    json!({
        "name": ASK_TOOL,
        "title": "Ask the operator",
        "description": "Put what you need from the operator on their Needs-you list, then end \
                        your turn: their reply arrives as your next prompt. Use it only when you \
                        cannot go on without them. kind: answer (a reply in words), decide (pick \
                        one of options), run (a command only they can run; they send its output \
                        back). Optional: hold (needs_input, the default, or awaiting_approval: a \
                        yes or no on something you want to do), cause (a short key shared by asks \
                        the operator can answer together), recommendation (your answer).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["answer", "decide", "run"] },
                "text": {
                    "type": "string",
                    "description": "The ask in one plain sentence the operator can act on without reading the conversation."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "decide: the choices, recommended first."
                },
                "command": {
                    "type": "string",
                    "description": "run: the exact command to run."
                },
                "hold": { "type": "string", "enum": ["needs_input", "awaiting_approval"] },
                "cause": {
                    "type": "string",
                    "description": "A short key; asks with the same one are answered together."
                },
                "recommendation": {
                    "type": "string",
                    "description": "The answer you recommend (decide: default the first option)."
                }
            },
            "required": ["kind", "text"]
        }
    })
}

fn handoff_tool() -> Value {
    json!({
        "name": HANDOFF_TOOL,
        "title": "Hand off to a new session",
        "description": "Propose continuing this work in a fresh agent session (same directory, \
                        model, and account), for when this conversation is long or the next \
                        phase deserves a clean context. Write the new session's complete first \
                        prompt: it has none of this conversation, so say the goal, what is done, \
                        what is next, where things are, and the rules to keep. The operator starts \
                        it with one click; then end your turn.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The new session's first prompt, in full." },
                "name": { "type": "string", "description": "Optional: a short name for the new session." },
                "reason": { "type": "string", "description": "One line: why hand off now." }
            },
            "required": ["prompt"]
        }
    })
}

fn list_tool() -> Value {
    json!({
        "name": LIST_TOOL,
        "title": "List agent sessions",
        "description": "The other agent sessions running on this hub: name, id, kind, folder, \
                        branch, status, and what each is doing. Use it to find who to message.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "mine": {
                    "type": "boolean",
                    "description": "Only the sessions you started (exited ones included)."
                }
            }
        }
    })
}

fn start_tool() -> Value {
    json!({
        "name": START_TOOL,
        "title": "Start an agent session",
        "description": "Start a new agent session to work in parallel (same kind, account, and \
                        permission options as you; it is linked to you). Write its complete \
                        first prompt: it has none of this conversation. It starts at once, or \
                        waits for the operator's approval if they set it so; either way keep \
                        working, and call it again for more sessions. Message it with \
                        message_session; check on it with session_status. To continue your own \
                        work in a fresh session, use handoff instead.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "A short name for the new session." },
                "prompt": { "type": "string", "description": "The new session's first prompt, in full." },
                "folder": {
                    "type": "string",
                    "description": "Optional: one of the workspace's folders (absolute path). Default: your own directory."
                },
                "worktree": {
                    "type": "object",
                    "description": "Optional: run it in a new sibling git worktree.",
                    "properties": {
                        "task": { "type": "string", "description": "Short kebab-case task name (the worktree and branch name)." },
                        "base": { "type": "string", "description": "Optional: the branch to cut it from." }
                    },
                    "required": ["task"]
                },
                "model": { "type": "string", "description": "Optional: its model. Default: yours." }
            },
            "required": ["name", "prompt"]
        }
    })
}

fn status_tool() -> Value {
    json!({
        "name": STATUS_TOOL,
        "title": "Check on an agent session",
        "description": "Read another session's status without messaging it: state (idle, \
                        working, asking, done, exited), its plan and current step, what it asks \
                        the operator, its last note, who started it, runtime, and cost.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." }
            },
            "required": ["to"]
        }
    })
}

fn message_tool() -> Value {
    json!({
        "name": MESSAGE_TOOL,
        "title": "Message another agent session",
        "description": "Send a message to another agent session on this hub (by name or id from \
                        list_sessions): it arrives as that agent's next prompt, under a line \
                        naming you, now if it is idle or once its current turn is done. For \
                        coordinating shared work: a heads-up, a result it needs, a request. Do \
                        not use it to reach the operator (ask_operator does that). Give a \
                        topic to one decision or request so a later change to it replaces \
                        the earlier message instead of arriving after it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." },
                "text": { "type": "string", "description": "The message." },
                "priority": {
                    "type": "boolean",
                    "description": "Optional: true queues it ahead of the other messages waiting for that session (behind earlier priority ones); it still waits for the current turn to end. Default false."
                },
                "topic": {
                    "type": "string",
                    "description": "Optional: a short key for what this message is about (ruling:schema, stop). A newer message from you with the same topic replaces your older one while it is still unread in that session's queue (keeping its place); if the older one was already delivered, the new one arrives with a first line saying it supersedes it. Letters, digits, . _ : / -; at most 64."
                }
            },
            "required": ["to", "text"]
        }
    })
}

fn end_tool() -> Value {
    json!({
        "name": END_TOOL,
        "title": "End an agent session",
        "description": "End another agent session you own (one you started, one given to you, \
                        or one your predecessor owned), for example to retire a worker. Its \
                        other queued messages are set aside. graceful (default): final_prompt \
                        is delivered at its next turn boundary as its last prompt (say: wrap up, \
                        commit, report, stop); once that turn is over and no tool call or \
                        subagent runs, the session ends. at_rest: no final prompt; it ends at its \
                        next rest. Nothing happens mid-flight: if it does not get there within \
                        wait_minutes, the end is called off and it keeps running. The reply says \
                        what would be lost (processes in its terminal, uncommitted work, which \
                        is never touched); the outcome arrives later as a message. Ending a \
                        session you do not own waits for the operator's approval. You cannot \
                        end yourself. dry_run: true answers its close checklist and ends \
                        nothing (branch merged into its base?, uncommitted and untracked \
                        files, build output size, other sessions there, which cleanups are \
                        allowed). close: what to do once it ended: worktree remove (only if \
                        clean and unshared; its branch is kept unless fully merged) or keep; \
                        build clean (deletes its target/ only) or keep; report: its final \
                        status file, kept by the hub and shown after the worktree is gone. \
                        Each is checked again at the end; one that no longer holds is \
                        refused and kept.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." },
                "mode": {
                    "type": "string",
                    "enum": ["graceful", "at_rest"],
                    "description": "graceful (default) or at_rest."
                },
                "final_prompt": {
                    "type": "string",
                    "description": "graceful: its last prompt, in full."
                },
                "wait_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 120,
                    "description": "Optional: how long the end may take before it is called off. Default 20."
                },
                "dry_run": { "type": "boolean", "description": "Optional: answer the close checklist only; end nothing." },
                "close": {
                    "type": "object",
                    "description": "Optional: cleanups once it ended.",
                    "properties": {
                        "worktree": { "type": "string", "enum": ["keep", "remove"] },
                        "build": { "type": "string", "enum": ["keep", "clean"] },
                        "report": { "type": "string", "description": "Its final status file (relative to its directory, or absolute)." }
                    }
                }
            },
            "required": ["to"]
        }
    })
}

fn replace_tool() -> Value {
    json!({
        "name": REPLACE_TOOL,
        "title": "Replace an agent session",
        "description": "Retire a session you own and start its successor, for example on \
                        another model or with a fresh context. final_prompt winds the old one \
                        down (like end_session graceful); once that turn is over and nothing \
                        runs, the new session starts (the old one's kind, account, and \
                        permission options, only model may change; its folder and worktree \
                        unless you give others; opened by you, continuing the old one's work), \
                        then the old one ends. Write the new session's complete first prompt: it \
                        has none of the old conversation. It counts as a session you start (it \
                        may wait for the operator's approval). If the old one does not come to \
                        rest within wait_minutes, nothing changes. The outcome arrives as a \
                        message.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session to retire: its name or id." },
                "name": { "type": "string", "description": "A short name for the new session." },
                "prompt": { "type": "string", "description": "The new session's first prompt, in full." },
                "final_prompt": { "type": "string", "description": "The old session's last prompt, in full." },
                "model": { "type": "string", "description": "Optional: the new session's model. Default: the old one's." },
                "folder": {
                    "type": "string",
                    "description": "Optional: one of the workspace's folders (absolute path). Default: the old session's."
                },
                "worktree": {
                    "type": "object",
                    "description": "Optional: run it in a new sibling git worktree.",
                    "properties": {
                        "task": { "type": "string", "description": "Short kebab-case task name (the worktree and branch name)." },
                        "base": { "type": "string", "description": "Optional: the branch to cut it from." }
                    },
                    "required": ["task"]
                },
                "wait_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 120,
                    "description": "Optional: how long the old one may take to come to rest. Default 20."
                }
            },
            "required": ["to", "name", "prompt", "final_prompt"]
        }
    })
}

fn queued_tool() -> Value {
    json!({
        "name": QUEUED_TOOL,
        "title": "List your queued messages",
        "description": "Your messages (message_session) that still wait in another session's \
                        queue, not yet delivered: id, place in its queue, priority, topic, when \
                        sent, and first line. Only your own are shown. Without to: every \
                        session you have messages waiting for.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Optional: the session's name or id. Default: every session." }
            }
        }
    })
}

fn cancel_tool() -> Value {
    json!({
        "name": CANCEL_TOOL,
        "title": "Take back queued messages",
        "description": "Take back messages you sent that still wait in another session's queue: \
                        one (item: its id from queued_messages), or all of yours without item. \
                        Only your own; a message being delivered right now cannot be taken \
                        back.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." },
                "item": { "type": "string", "description": "Optional: the message's id. Default: all of yours." }
            },
            "required": ["to"]
        }
    })
}

fn give_tool() -> Value {
    json!({
        "name": GIVE_TOOL,
        "title": "Give a session to another",
        "description": "Give control of a session you own to another running agent session (for \
                        example to your successor before you hand off): it may then end, \
                        replace, or give it, and you no longer may. A session you start is yours \
                        until you give it; when you end, your successor inherits it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session to give: its name or id." },
                "owner": { "type": "string", "description": "The new owner: its name or id." }
            },
            "required": ["to", "owner"]
        }
    })
}

fn label_tool() -> Value {
    json!({
        "name": LABEL_TOOL,
        "title": "Label a session",
        "description": "Set or remove key=value labels on a session (yours by default): one value \
                        per key, used to group and filter sessions (fw hub ls --label team=web) \
                        and to route watches and broadcasts. Keys are lowercase words; fw.* keys \
                        are the hub's. if: change only when the one key now holds that value; \
                        if_unset: only when it holds none (a claim: the first one wins).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Optional: the session's name or id. Default: you." },
                "set": {
                    "type": "object",
                    "description": "Each key to its new value, or null to remove it.",
                    "additionalProperties": { "type": ["string", "null"] }
                },
                "if": { "type": "string", "description": "Optional: change only if the one key holds this value now." },
                "if_unset": { "type": "boolean", "description": "Optional: change only if the one key holds no value now." }
            },
            "required": ["set"]
        }
    })
}

fn watch_tool() -> Value {
    json!({
        "name": WATCH_TOOL,
        "title": "Watch for events",
        "description": "Get woken when something happens instead of polling: when an event on \
                        this hub matches filter, prompt arrives as your next prompt (queued, \
                        delivered once your turn is over), followed by the matching events and \
                        their cursors; events within window_secs, and any while that prompt \
                        waits, arrive together in it. filter: space-separated terms, all of \
                        which must hold: children (sessions you started), session=NAME, \
                        state=done|blocked|failed|asking (a session moving there), \
                        kind=review|pr|queue (event kinds or families), or labels (team=web). \
                        Example: \"children state=done|blocked|failed\". Your own events never \
                        match. By time instead of filter: at (\"2026-11-23T12:00Z\" or \
                        \"+36h\") wakes you once; every (\"6h\", at least 5m) wakes you \
                        repeatedly; a time watch survives hub restarts and passes to your \
                        successor. A watch lasts until unwatch or until you end.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "What to watch for (see above); not with at or every." },
                "at": { "type": "string", "description": "Optional: wake once at this time (RFC 3339 UTC, or +36h)." },
                "every": { "type": "string", "description": "Optional: wake this often (6h, 1d; at least 5m); with at, from then." },
                "prompt": { "type": "string", "description": "What to tell yourself when it fires: what to do next." },
                "window_secs": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 3600,
                    "description": "Optional: gather matches this long before waking you. Default 3."
                }
            },
            "required": ["prompt"]
        }
    })
}

fn unwatch_tool() -> Value {
    json!({
        "name": UNWATCH_TOOL,
        "title": "Stop watching",
        "description": "Disarm one of your watches (its id, from watch) or all of yours (\"all\"). \
                        A prompt it already queued for you stays queued.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The watch's id, or all." }
            },
            "required": ["id"]
        }
    })
}

fn lease_tool() -> Value {
    json!({
        "name": LEASE_TOOL,
        "title": "Lease a shared resource",
        "description": "Coordinate a shared resource with other sessions (a quiet machine for a \
                        benchmark, a node only one session may stream at, a test-suite slot). \
                        action acquire: take resource, shared or exclusive, for ttl_secs; if \
                        others hold it you wait in line, first come first served, up to \
                        wait_secs. A wait over 50 s goes on after this call returns: you are \
                        told your place, and the grant arrives as your next prompt (keep \
                        working or end your turn). The lease ends when you release it, when \
                        ttl_secs runs out (acquire again to renew), or when you end. action \
                        release: give it back (resource, or lease: its id). action list: holders \
                        and waiters, all or one resource. Release as soon as you are done: \
                        others may be waiting.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["acquire", "release", "list"], "description": "acquire, release, or list." },
                "resource": { "type": "string", "description": "The resource's name: lowercase letters, digits, . _ : / - (cpu:quiet, node:desktop:heavy)." },
                "mode": { "type": "string", "enum": ["shared", "exclusive"], "description": "Optional: shared (others may share it) or exclusive (default)." },
                "ttl_secs": { "type": "integer", "minimum": 30, "maximum": 86400, "description": "Optional: how long you may hold it before it must be renewed. Default 1800." },
                "wait_secs": { "type": "integer", "minimum": 0, "maximum": 86400, "description": "Optional: how long to wait in line. Default 0: answer at once." },
                "note": { "type": "string", "description": "Optional: what it is for, shown to others." },
                "lease": { "type": "string", "description": "release: the lease's id instead of the resource." }
            },
            "required": ["action"]
        }
    })
}

fn record_review_tool() -> Value {
    json!({
        "name": RECORD_REVIEW_TOOL,
        "title": "Record a review",
        "description": "Record your review of a commit range so leads and the operator can \
                        check it later by SHA. Give repo (any path inside it; default your \
                        directory), base and head (SHAs or refs, resolved now), verdict READY, \
                        READY_WITH_CHANGES or NOT_READY, findings counted by severity, and \
                        report (path to your full review; the hub keeps a copy). follow_up_of: \
                        the review id this re-review answers. If you record for a subagent you \
                        ran, set reviewer to its label. Records cannot be changed: record a \
                        follow-up instead.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Optional: a path inside the repo. Default: your directory." },
                "base": { "type": "string", "description": "The commit the reviewed range starts after." },
                "head": { "type": "string", "description": "The last commit reviewed." },
                "verdict": { "type": "string", "enum": ["READY", "READY_WITH_CHANGES", "NOT_READY"] },
                "findings": {
                    "type": "object",
                    "description": "Optional: findings counted by severity.",
                    "properties": {
                        "critical": { "type": "integer", "minimum": 0 },
                        "high": { "type": "integer", "minimum": 0 },
                        "medium": { "type": "integer", "minimum": 0 },
                        "low": { "type": "integer", "minimum": 0 },
                        "info": { "type": "integer", "minimum": 0 }
                    }
                },
                "report": { "type": "string", "description": "Optional: path to the full review (at most 256 KiB)." },
                "follow_up_of": { "type": "string", "description": "Optional: the review id this one answers." },
                "reviewer": { "type": "string", "description": "Optional: the label of a subagent you ran that did the review. Default: you." },
                "note": { "type": "string", "description": "Optional: a short note." }
            },
            "required": ["base", "head", "verdict"]
        }
    })
}

fn adopt_tool() -> Value {
    json!({
        "name": ADOPT_TOOL,
        "title": "Adopt a worktree",
        "description": "Have the hub track a git worktree that was made outside it (plain git \
                        worktree add, an older session), as if start_session had made it: its \
                        uncommitted, untracked, and unmerged counts are kept in view, and the \
                        hub never removes it while it holds work. path: the worktree's \
                        directory. base: the branch it merges into (default: worked out from \
                        git, and the reply says how). session: optionally a session you own \
                        that works in it. Changes nothing on disk.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The worktree's directory (absolute, or relative to yours)." },
                "base": { "type": "string", "description": "Optional: the branch it merges into." },
                "session": { "type": "string", "description": "Optional: a session you own that works in it (name or id)." }
            },
            "required": ["path"]
        }
    })
}

fn check_reviewed_tool() -> Value {
    json!({
        "name": CHECK_REVIEWED_TOOL,
        "title": "Check review coverage",
        "description": "Check that every commit in repo's base..head is covered by a READY \
                        review (directly or through follow-ups). Answers ok, then each uncovered \
                        commit with why: no review, NOT_READY, READY_WITH_CHANGES with no READY \
                        follow-up, or reviewed only by its author. Use it before merging or \
                        releasing reviewed work.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Optional: a path inside the repo. Default: your directory." },
                "range": { "type": "string", "description": "BASE..HEAD (SHAs or refs)." },
                "patch_equivalent": { "type": "boolean", "description": "Optional: a commit with no review counts if a covered commit has the same patch (a rebase). Default false." }
            },
            "required": ["range"]
        }
    })
}

/// The `label` tool's arguments as a change and its condition.
fn label_args(
    args: &Value,
) -> Result<(crate::labels::Change, Option<crate::labels::Cond>), String> {
    let set = args
        .get("set")
        .and_then(Value::as_object)
        .ok_or("give set: an object of key to value (null removes it)")?;
    let mut change = crate::labels::Change::new();
    for (key, value) in set {
        let value = match value {
            Value::Null => None,
            Value::String(v) if v.is_empty() => None,
            Value::String(v) => Some(v.clone()),
            other => Some(other.to_string()),
        };
        change.insert(key.clone(), value);
    }
    if change.is_empty() {
        return Err("set names no label".into());
    }
    let cond = match (
        args.get("if").and_then(Value::as_str),
        args.get("if_unset").and_then(Value::as_bool) == Some(true),
    ) {
        (Some(_), true) => return Err("give if or if_unset, not both".into()),
        (Some(value), false) => Some(crate::labels::Cond::Is(value.to_owned())),
        (None, true) => Some(crate::labels::Cond::Unset),
        (None, false) => None,
    };
    Ok((change, cond))
}

/// `end_session`'s mode: graceful (also when not given) or at_rest;
/// force is refused by name.
fn end_mode(raw: &str) -> Result<EndMode, String> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" | "graceful" => Ok(EndMode::Graceful),
        "at_rest" => Ok(EndMode::AtRest),
        "force" => Err(crate::hub::FORCE_REFUSED.to_owned()),
        other => Err(format!("unknown mode {other:?}: graceful or at_rest")),
    }
}

fn text_result(text: impl Into<String>, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": error })
}

/// Answer one JSON-RPC message (`None`: a notification, nothing to say).
pub fn answer(message: &Value, host: &dyn Host) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [
                tool(),
                ask_tool(),
                handoff_tool(),
                list_tool(),
                message_tool(),
                start_tool(),
                status_tool(),
                end_tool(),
                replace_tool(),
                queued_tool(),
                cancel_tool(),
                give_tool(),
                label_tool(),
                watch_tool(),
                unwatch_tool(),
                lease_tool(),
                record_review_tool(),
                check_reviewed_tool(),
                adopt_tool(),
            ] })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str);
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
            if name == Some(HANDOFF_TOOL) {
                let given = |key: &str| Some(arg(key)).filter(|v| !v.trim().is_empty());
                Ok(if arg("prompt").trim().is_empty() {
                    text_result("Give the new session's whole prompt.", true)
                } else {
                    match host.handoff(arg("prompt"), given("name"), given("reason")) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not handed off: {why}"), true),
                    }
                })
            } else if name == Some(LIST_TOOL) {
                let mine = args.get("mine").and_then(Value::as_bool) == Some(true);
                Ok(text_result(host.list_sessions(mine), false))
            } else if name == Some(START_TOOL) {
                let given = |v: Option<&Value>, key: &str| {
                    v.and_then(|v| v.get(key))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let start = StartArgs {
                    name: arg("name").to_owned(),
                    prompt: arg("prompt").to_owned(),
                    folder: given(Some(args), "folder"),
                    worktree: given(args.get("worktree"), "task")
                        .map(|task| (task, given(args.get("worktree"), "base"))),
                    model: given(Some(args), "model"),
                };
                Ok(
                    if start.name.trim().is_empty() || start.prompt.trim().is_empty() {
                        text_result("Give the new session a name and its whole prompt.", true)
                    } else {
                        match host.start_session(&start) {
                            Ok(said) => text_result(said, false),
                            Err(why) => text_result(format!("Not started: {why}"), true),
                        }
                    },
                )
            } else if name == Some(STATUS_TOOL) {
                Ok(match host.session_status(arg("to")) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("No status: {why}"), true),
                })
            } else if name == Some(MESSAGE_TOOL) {
                let priority = args.get("priority").and_then(Value::as_bool) == Some(true);
                let topic = Some(arg("topic")).filter(|v| !v.trim().is_empty());
                Ok(
                    match host.message(arg("to"), arg("text"), priority, topic) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not sent: {why}"), true),
                    },
                )
            } else if name == Some(END_TOOL) {
                let given = |key: &str| Some(arg(key)).filter(|v| !v.trim().is_empty());
                let wait = args
                    .get("wait_minutes")
                    .and_then(Value::as_u64)
                    .map(|m| u32::try_from(m).unwrap_or(u32::MAX));
                let dry_run = args.get("dry_run").and_then(Value::as_bool) == Some(true);
                let close: Result<Option<crate::close::CloseChoices>, String> = args
                    .get("close")
                    .filter(|c| !c.is_null())
                    .map(|c| serde_json::from_value(c.clone()).map_err(|e| format!("close: {e}")))
                    .transpose();
                Ok(match (end_mode(arg("mode")), close) {
                    (Err(why), _) | (_, Err(why)) => text_result(format!("Not ended: {why}"), true),
                    (Ok(mode), Ok(close)) => {
                        match host.end_session(
                            arg("to"),
                            mode,
                            given("final_prompt"),
                            wait,
                            close,
                            dry_run,
                        ) {
                            Ok(said) => text_result(said, false),
                            Err(why) => text_result(format!("Not ended: {why}"), true),
                        }
                    }
                })
            } else if name == Some(REPLACE_TOOL) {
                let given = |v: Option<&Value>, key: &str| {
                    v.and_then(|v| v.get(key))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let replace = ReplaceArgs {
                    to: arg("to").to_owned(),
                    name: arg("name").to_owned(),
                    model: given(Some(args), "model"),
                    prompt: arg("prompt").to_owned(),
                    final_prompt: arg("final_prompt").to_owned(),
                    folder: given(Some(args), "folder"),
                    worktree: given(args.get("worktree"), "task")
                        .map(|task| (task, given(args.get("worktree"), "base"))),
                    wait_minutes: args
                        .get("wait_minutes")
                        .and_then(Value::as_u64)
                        .map(|m| u32::try_from(m).unwrap_or(u32::MAX)),
                };
                Ok(
                    if replace.name.trim().is_empty()
                        || replace.prompt.trim().is_empty()
                        || replace.final_prompt.trim().is_empty()
                    {
                        text_result(
                            "Give the new session a name and its whole prompt, and the old one \
                             its final_prompt.",
                            true,
                        )
                    } else {
                        match host.replace_session(&replace) {
                            Ok(said) => text_result(said, false),
                            Err(why) => text_result(format!("Not replaced: {why}"), true),
                        }
                    },
                )
            } else if name == Some(QUEUED_TOOL) {
                let to = Some(arg("to")).filter(|v| !v.trim().is_empty());
                Ok(match host.queued_messages(to) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("No list: {why}"), true),
                })
            } else if name == Some(CANCEL_TOOL) {
                let item = Some(arg("item")).filter(|v| !v.trim().is_empty());
                Ok(match host.cancel_message(arg("to"), item) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not cancelled: {why}"), true),
                })
            } else if name == Some(LABEL_TOOL) {
                let to = Some(arg("to")).filter(|v| !v.trim().is_empty());
                Ok(match label_args(args) {
                    Err(why) => text_result(format!("Not labelled: {why}"), true),
                    Ok((change, cond)) => match host.label(to, &change, cond.as_ref()) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not labelled: {why}"), true),
                    },
                })
            } else if name == Some(WATCH_TOOL) {
                let window = args
                    .get("window_secs")
                    .and_then(Value::as_u64)
                    .map(|w| u32::try_from(w).unwrap_or(u32::MAX));
                let given = |key: &str| Some(arg(key).trim()).filter(|v| !v.is_empty());
                Ok(
                    match host.watch(
                        arg("filter"),
                        arg("prompt"),
                        window,
                        given("at"),
                        given("every"),
                    ) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not watching: {why}"), true),
                    },
                )
            } else if name == Some(LEASE_TOOL) {
                let text = |key: &str| {
                    Some(arg(key).trim())
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let number = |key: &str| {
                    args.get(key)
                        .and_then(Value::as_u64)
                        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                };
                let lease = LeaseArgs {
                    action: arg("action").trim().to_ascii_lowercase(),
                    resource: text("resource"),
                    mode: text("mode"),
                    ttl_secs: number("ttl_secs"),
                    wait_secs: number("wait_secs"),
                    note: text("note"),
                    lease: text("lease"),
                };
                Ok(match host.lease(&lease) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("No lease: {why}"), true),
                })
            } else if name == Some(RECORD_REVIEW_TOOL) {
                let input: Result<crate::hub::reviews::ReviewInput, _> =
                    serde_json::from_value(args.clone());
                Ok(match input {
                    Err(e) => text_result(format!("Not recorded: {e}"), true),
                    Ok(input) => match host.record_review(&input) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not recorded: {why}"), true),
                    },
                })
            } else if name == Some(ADOPT_TOOL) {
                let given = |key: &str| {
                    Some(arg(key).trim())
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let adopt = crate::hub::changes::Adopt {
                    path: arg("path").to_owned(),
                    base: given("base"),
                    session: given("session"),
                };
                Ok(match host.adopt_worktree(&adopt) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not adopted: {why}"), true),
                })
            } else if name == Some(CHECK_REVIEWED_TOOL) {
                let twins = args.get("patch_equivalent").and_then(Value::as_bool) == Some(true);
                Ok(
                    match host.check_reviewed(arg("repo"), arg("range"), twins) {
                        Ok(said) => text_result(said, false),
                        Err(why) => text_result(format!("Not checked: {why}"), true),
                    },
                )
            } else if name == Some(UNWATCH_TOOL) {
                Ok(match host.unwatch(arg("id")) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not removed: {why}"), true),
                })
            } else if name == Some(GIVE_TOOL) {
                Ok(match host.give_session(arg("to"), arg("owner")) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not given: {why}"), true),
                })
            } else if name == Some(ASK_TOOL) {
                Ok(json!({
                    "content": [{ "type": "text", "text": "The operator sees your ask. End your turn now; their reply arrives as your next prompt." }],
                    "isError": false,
                }))
            } else if name == Some(TOOL) {
                let items = params
                    .get("arguments")
                    .and_then(|a| a.get("items"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Ok(json!({
                    "content": [{ "type": "text", "text": format!("Recorded {items} steps.") }],
                    "isError": false,
                }))
            } else {
                Err((-32602, "unknown tool"))
            }
        }
        _ => Err((-32601, "method not found")),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, text)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } })
        }
    })
}

/// Answer a request body: one message, or a batch.
pub fn answer_body(body: &Value, host: &dyn Host) -> Option<Value> {
    match body {
        Value::Array(batch) => {
            let answers: Vec<Value> = batch.iter().filter_map(|m| answer(m, host)).collect();
            (!answers.is_empty()).then_some(Value::Array(answers))
        }
        one => answer(one, host),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub with one other session, which takes every message.
    struct OneOther;

    impl Host for OneOther {
        fn list_sessions(&self, mine: bool) -> String {
            if mine {
                "You started no session.".to_owned()
            } else {
                "claude 4 (id s4) [claude] in /r: working".to_owned()
            }
        }
        fn start_session(&self, args: &StartArgs) -> Result<String, String> {
            if args.folder.as_deref() == Some("/elsewhere") {
                Err("folder /elsewhere is not part of workspace w".to_owned())
            } else {
                Ok(format!(
                    "Started {} ({:?}, {:?})",
                    args.name, args.worktree, args.model
                ))
            }
        }
        fn session_status(&self, to: &str) -> Result<String, String> {
            if to == "claude 4" {
                Ok("state: working".to_owned())
            } else {
                Err(format!("no session matches {to:?}"))
            }
        }
        fn message(
            &self,
            to: &str,
            text: &str,
            priority: bool,
            topic: Option<&str>,
        ) -> Result<String, String> {
            if let Some(topic) = topic {
                return Ok(format!(
                    "Replaced your unread message on topic {topic} for {to}"
                ));
            }
            if to == "claude 4" {
                Ok(format!(
                    "{} claude 4: {text}",
                    if priority {
                        "Queued as priority for"
                    } else {
                        "Delivered to"
                    }
                ))
            } else {
                Err(format!("no session matches {to:?}"))
            }
        }
        fn end_session(
            &self,
            to: &str,
            mode: EndMode,
            final_prompt: Option<&str>,
            wait_minutes: Option<u32>,
            _close: Option<crate::close::CloseChoices>,
            _dry_run: bool,
        ) -> Result<String, String> {
            if to == "claude 4" {
                Ok(format!(
                    "Ending {to} ({}, {final_prompt:?}, {wait_minutes:?})",
                    mode.as_str()
                ))
            } else {
                Err(format!("no session matches {to:?}"))
            }
        }
        fn replace_session(&self, args: &ReplaceArgs) -> Result<String, String> {
            Ok(format!(
                "Replacing {} with {} ({:?}, {:?})",
                args.to, args.name, args.model, args.worktree
            ))
        }
        fn queued_messages(&self, to: Option<&str>) -> Result<String, String> {
            Ok(match to {
                Some(to) => format!("None of your messages wait for {to}."),
                None => "None of your messages wait in any session's queue.".to_owned(),
            })
        }
        fn cancel_message(&self, to: &str, item: Option<&str>) -> Result<String, String> {
            Ok(format!("Cancelled {} for {to}", item.unwrap_or("all")))
        }
        fn give_session(&self, to: &str, new_owner: &str) -> Result<String, String> {
            Ok(format!("{to} is now {new_owner}'s"))
        }
        fn lease(&self, args: &LeaseArgs) -> Result<String, String> {
            Ok(format!("{} {:?}", args.action, args.resource))
        }
        fn record_review(
            &self,
            input: &crate::hub::reviews::ReviewInput,
        ) -> Result<String, String> {
            Ok(format!("Recorded {}", input.verdict))
        }
        fn check_reviewed(
            &self,
            repo: &str,
            range: &str,
            _patch_equivalent: bool,
        ) -> Result<String, String> {
            Ok(format!("ok {repo} {range}"))
        }
        fn adopt_worktree(&self, adopt: &crate::hub::changes::Adopt) -> Result<String, String> {
            Ok(format!("Tracking {}", adopt.path))
        }
        fn label(
            &self,
            to: Option<&str>,
            change: &crate::labels::Change,
            cond: Option<&crate::labels::Cond>,
        ) -> Result<String, String> {
            Ok(format!("{to:?} {change:?} {cond:?}"))
        }
        fn watch(
            &self,
            filter: &str,
            prompt: &str,
            window_secs: Option<u32>,
            _at: Option<&str>,
            _every: Option<&str>,
        ) -> Result<String, String> {
            Ok(format!("{filter} / {prompt} / {window_secs:?}"))
        }
        fn unwatch(&self, id: &str) -> Result<String, String> {
            Ok(format!("unwatched {id}"))
        }
        fn handoff(
            &self,
            prompt: &str,
            name: Option<&str>,
            _reason: Option<&str>,
        ) -> Result<String, String> {
            if prompt.contains('\0') {
                Err("the prompt holds a NUL byte".to_owned())
            } else {
                Ok(format!("Kept as h1 ({})", name.unwrap_or("unnamed")))
            }
        }
    }

    fn answer(message: &Value) -> Option<Value> {
        super::answer(message, &OneOther)
    }

    #[test]
    fn the_acting_tools_go_through_the_host() {
        let call = |name: &str, args: Value| {
            answer(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": args}}))
            .unwrap()["result"]
                .clone()
        };
        let listed = call(LIST_TOOL, json!({}));
        assert_eq!(
            listed["content"][0]["text"],
            "claude 4 (id s4) [claude] in /r: working"
        );
        assert_eq!(
            call(LIST_TOOL, json!({"mine": true}))["content"][0]["text"],
            "You started no session."
        );
        let started = call(
            START_TOOL,
            json!({"name": "docs", "prompt": "Goal: ...", "worktree": {"task": "docs-fix"}, "model": "sonnet"}),
        );
        assert_eq!(started["isError"], false);
        assert_eq!(
            started["content"][0]["text"],
            "Started docs (Some((\"docs-fix\", None)), Some(\"sonnet\"))"
        );
        assert_eq!(
            call(START_TOOL, json!({"name": "docs", "prompt": " "}))["isError"],
            true
        );
        let outside = call(
            START_TOOL,
            json!({"name": "x", "prompt": "p", "folder": "/elsewhere"}),
        );
        assert_eq!(outside["isError"], true);
        assert!(
            outside["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Not started: folder /elsewhere")
        );
        assert_eq!(
            call(STATUS_TOOL, json!({"to": "claude 4"}))["content"][0]["text"],
            "state: working"
        );
        assert_eq!(call(STATUS_TOOL, json!({"to": "nobody"}))["isError"], true);
        let sent = call(MESSAGE_TOOL, json!({"to": "claude 4", "text": "rebased"}));
        assert_eq!(sent["isError"], false);
        assert_eq!(sent["content"][0]["text"], "Delivered to claude 4: rebased");
        let refused = call(MESSAGE_TOOL, json!({"to": "nobody", "text": "x"}));
        assert_eq!(refused["isError"], true);
        assert!(
            refused["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Not sent: ")
        );
        let kept = call(HANDOFF_TOOL, json!({"prompt": "Goal: ...", "name": "docs"}));
        assert_eq!(kept["isError"], false);
        assert_eq!(kept["content"][0]["text"], "Kept as h1 (docs)");
        assert_eq!(call(HANDOFF_TOOL, json!({"prompt": " "}))["isError"], true);
        // Priority is optional; without it a message goes as before.
        assert_eq!(
            call(
                MESSAGE_TOOL,
                json!({"to": "claude 4", "text": "stop", "priority": true})
            )["content"][0]["text"],
            "Queued as priority for claude 4: stop"
        );
        // Ends: graceful by default, at_rest by name, force refused here.
        assert_eq!(
            call(
                END_TOOL,
                json!({"to": "claude 4", "final_prompt": "wrap up", "wait_minutes": 5})
            )["content"][0]["text"],
            "Ending claude 4 (graceful, Some(\"wrap up\"), Some(5))"
        );
        assert_eq!(
            call(END_TOOL, json!({"to": "claude 4", "mode": "at_rest"}))["content"][0]["text"],
            "Ending claude 4 (at_rest, None, None)"
        );
        let forced = call(END_TOOL, json!({"to": "claude 4", "mode": "force"}));
        assert_eq!(forced["isError"], true);
        assert_eq!(
            forced["content"][0]["text"],
            "Not ended: force is operator only (pane menu or fw hub kill)"
        );
        assert_eq!(
            call(END_TOOL, json!({"to": "x", "mode": "soon"}))["isError"],
            true
        );
        assert_eq!(
            call(
                REPLACE_TOOL,
                json!({"to": "claude 4", "name": "w2", "prompt": "Goal", "final_prompt": "stop",
                       "model": "sonnet", "worktree": {"task": "t"}})
            )["content"][0]["text"],
            "Replacing claude 4 with w2 (Some(\"sonnet\"), Some((\"t\", None)))"
        );
        assert_eq!(
            call(
                REPLACE_TOOL,
                json!({"to": "claude 4", "name": "w2", "prompt": "Goal"})
            )["isError"],
            true
        );
        assert_eq!(
            call(QUEUED_TOOL, json!({"to": "claude 4"}))["content"][0]["text"],
            "None of your messages wait for claude 4."
        );
        assert_eq!(
            call(CANCEL_TOOL, json!({"to": "claude 4"}))["content"][0]["text"],
            "Cancelled all for claude 4"
        );
        assert_eq!(
            call(CANCEL_TOOL, json!({"to": "claude 4", "item": "abcd"}))["content"][0]["text"],
            "Cancelled abcd for claude 4"
        );
        assert_eq!(
            call(GIVE_TOOL, json!({"to": "w", "owner": "lead 2"}))["content"][0]["text"],
            "w is now lead 2's"
        );
        let failed = call(HANDOFF_TOOL, json!({"prompt": "a\u{0}b"}));
        assert_eq!(failed["isError"], true);
        assert_eq!(
            failed["content"][0]["text"],
            "Not handed off: the prompt holds a NUL byte"
        );
    }

    #[test]
    fn the_handshake_lists_one_tool_and_takes_its_call() {
        let init = answer(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}}}))
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(
            answer(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            None
        );
        let labelled = answer(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": LABEL_TOOL, "arguments": {"set": {"team": "web", "old": null}, "if_unset": true}}}))
        .unwrap();
        assert_eq!(
            labelled["result"]["content"][0]["text"],
            "None {\"old\": None, \"team\": Some(\"web\")} Some(Unset)"
        );
        let refused = answer(&json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": LABEL_TOOL, "arguments": {"set": {}}}}))
        .unwrap();
        assert_eq!(refused["result"]["isError"], true);
        let list = answer(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], TOOL);
        assert_eq!(list["result"]["tools"][1]["name"], ASK_TOOL);
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                TOOL,
                ASK_TOOL,
                HANDOFF_TOOL,
                LIST_TOOL,
                MESSAGE_TOOL,
                START_TOOL,
                STATUS_TOOL,
                END_TOOL,
                REPLACE_TOOL,
                QUEUED_TOOL,
                CANCEL_TOOL,
                GIVE_TOOL,
                LABEL_TOOL,
                WATCH_TOOL,
                UNWATCH_TOOL,
                LEASE_TOOL,
                RECORD_REVIEW_TOOL,
                CHECK_REVIEWED_TOOL,
                ADOPT_TOOL
            ]
        );
        let asked = answer(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": ASK_TOOL, "arguments": {"kind": "run", "text": "Run it", "command": "sudo true"}}}))
        .unwrap();
        assert_eq!(asked["result"]["isError"], false);
        let call = answer(&json!({"jsonrpc": "2.0", "id": "a", "method": "tools/call",
            "params": {"name": TOOL, "arguments": {"items": [{"title": "x", "status": "pending"}]}}}))
        .unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(call["id"], "a");
        let unknown = answer(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "other"}}))
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn the_nudge_names_the_tool_claude_code_sees() {
        assert!(NUDGE.contains(crate::telemetry::claude::PROGRESS_TOOL));
        assert!(NUDGE.contains(crate::telemetry::claude::ASK_TOOL));
        assert!(NUDGE.contains(crate::telemetry::claude::HANDOFF_TOOL));
        assert!(NUDGE.contains(crate::telemetry::claude::START_TOOL));
        assert!(NUDGE.contains(crate::telemetry::claude::END_TOOL));
        assert!(NUDGE.contains(crate::telemetry::claude::REPLACE_TOOL));
        for (claude, ours) in [
            (crate::telemetry::claude::HANDOFF_TOOL, HANDOFF_TOOL),
            (crate::telemetry::claude::LIST_TOOL, LIST_TOOL),
            (crate::telemetry::claude::MESSAGE_TOOL, MESSAGE_TOOL),
            (crate::telemetry::claude::START_TOOL, START_TOOL),
            (crate::telemetry::claude::STATUS_TOOL, STATUS_TOOL),
            (crate::telemetry::claude::END_TOOL, END_TOOL),
            (crate::telemetry::claude::REPLACE_TOOL, REPLACE_TOOL),
            (crate::telemetry::claude::QUEUED_TOOL, QUEUED_TOOL),
            (crate::telemetry::claude::CANCEL_TOOL, CANCEL_TOOL),
            (crate::telemetry::claude::GIVE_TOOL, GIVE_TOOL),
        ] {
            assert_eq!(claude, format!("mcp__{SERVER_NAME}__{ours}"));
        }
        assert_eq!(
            crate::telemetry::claude::ASK_TOOL,
            format!("mcp__{SERVER_NAME}__{ASK_TOOL}")
        );
        assert_eq!(
            crate::telemetry::claude::PROGRESS_TOOL,
            format!("mcp__{SERVER_NAME}__{TOOL}")
        );
    }
}
