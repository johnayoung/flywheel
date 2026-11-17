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
    /// waiting messages that are not); what became of it.
    fn message(&self, to: &str, text: &str, priority: bool) -> Result<String, String>;
    /// End the session `to` names (or ask the operator to); what to tell
    /// the agent now (the outcome arrives as a message).
    fn end_session(
        &self,
        to: &str,
        mode: EndMode,
        final_prompt: Option<&str>,
        wait_minutes: Option<u32>,
    ) -> Result<String, String>;
    /// Replace the session `to` names with a new one.
    fn replace_session(&self, args: &ReplaceArgs) -> Result<String, String>;
    /// The caller's messages still waiting in `to`'s queue.
    fn queued_messages(&self, to: &str) -> Result<String, String>;
    /// Take back one (`item`) or all of the caller's waiting messages.
    fn cancel_message(&self, to: &str, item: Option<&str>) -> Result<String, String>;
    /// Give control of `to` to `new_owner`.
    fn give_session(&self, to: &str, new_owner: &str) -> Result<String, String>;
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
continuing your own work); to retire one, call mcp__flywheel__end_session or \
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
                        back).",
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
                        not use it to reach the operator (ask_operator does that).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." },
                "text": { "type": "string", "description": "The message." },
                "priority": {
                    "type": "boolean",
                    "description": "Optional: true queues it ahead of the other messages waiting for that session (behind earlier priority ones); it still waits for the current turn to end. Default false."
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
                        end yourself.",
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
                        queue, not yet delivered: id, place in its queue, priority, when sent, \
                        and first line. Only your own are shown.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "The session's name or id." }
            },
            "required": ["to"]
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
                Ok(match host.message(arg("to"), arg("text"), priority) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not sent: {why}"), true),
                })
            } else if name == Some(END_TOOL) {
                let given = |key: &str| Some(arg(key)).filter(|v| !v.trim().is_empty());
                let wait = args
                    .get("wait_minutes")
                    .and_then(Value::as_u64)
                    .map(|m| u32::try_from(m).unwrap_or(u32::MAX));
                Ok(match end_mode(arg("mode")) {
                    Err(why) => text_result(format!("Not ended: {why}"), true),
                    Ok(mode) => {
                        match host.end_session(arg("to"), mode, given("final_prompt"), wait) {
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
                Ok(match host.queued_messages(arg("to")) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("No list: {why}"), true),
                })
            } else if name == Some(CANCEL_TOOL) {
                let item = Some(arg("item")).filter(|v| !v.trim().is_empty());
                Ok(match host.cancel_message(arg("to"), item) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not cancelled: {why}"), true),
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
        fn message(&self, to: &str, text: &str, priority: bool) -> Result<String, String> {
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
        fn queued_messages(&self, to: &str) -> Result<String, String> {
            Ok(format!("None of your messages wait for {to}."))
        }
        fn cancel_message(&self, to: &str, item: Option<&str>) -> Result<String, String> {
            Ok(format!("Cancelled {} for {to}", item.unwrap_or("all")))
        }
        fn give_session(&self, to: &str, new_owner: &str) -> Result<String, String> {
            Ok(format!("{to} is now {new_owner}'s"))
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
                GIVE_TOOL
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
