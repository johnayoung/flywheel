//! The hub's agent tools: an MCP server (streamable HTTP, JSON responses
//! only). `report_progress` declares the agent's plan and how far along it
//! is; `ask_operator` declares what it needs from the operator before it
//! can go on (a reply, a decision, a command only they can run);
//! `handoff` proposes continuing the work in a fresh session from a
//! prompt it wrote. Those three only declare: the hook path records them.
//! `list_sessions` and `message_session` act, through a [`Host`]: the
//! other agent sessions on the hub, and a message typed into one.
//!
//! Agents rarely keep a plan on their own; the hub gives every Claude
//! Code session it starts this tool and one sentence asking for its use
//! ([`NUDGE`]). The call itself changes nothing here: Claude Code's
//! `PreToolUse` hook carries the same input, with the subagent that made
//! it, into the telemetry log ([`super::claude`]), so a plan is recorded
//! on one path whoever declares it. What is declared is the agent's
//! claim; the board shows it beside the calls the hub observed.

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

/// What the tools that act do, for the session calling them.
pub trait Host {
    /// The other agent sessions, one line each.
    fn list_sessions(&self) -> String;
    /// Send `text` to the session `to` names; what became of it.
    fn message(&self, to: &str, text: &str) -> Result<String, String>;
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
mcp__flywheel__handoff with that session's whole prompt instead of printing one. Do not mention \
these tools or the board in your replies.";

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
        "inputSchema": { "type": "object", "properties": {} }
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
                "text": { "type": "string", "description": "The message." }
            },
            "required": ["to", "text"]
        }
    })
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
        "tools/list" => Ok(
            json!({ "tools": [tool(), ask_tool(), handoff_tool(), list_tool(), message_tool()] }),
        ),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str);
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
            if name == Some(HANDOFF_TOOL) {
                Ok(if arg("prompt").trim().is_empty() {
                    text_result("Give the new session's whole prompt.", true)
                } else {
                    text_result(
                        "The operator sees your handoff and can start the new session. End your turn now.",
                        false,
                    )
                })
            } else if name == Some(LIST_TOOL) {
                Ok(text_result(host.list_sessions(), false))
            } else if name == Some(MESSAGE_TOOL) {
                Ok(match host.message(arg("to"), arg("text")) {
                    Ok(said) => text_result(said, false),
                    Err(why) => text_result(format!("Not sent: {why}"), true),
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
        fn list_sessions(&self) -> String {
            "claude 4 (id s4) [claude] in /r: working".to_owned()
        }
        fn message(&self, to: &str, text: &str) -> Result<String, String> {
            if to == "claude 4" {
                Ok(format!("Delivered to claude 4: {text}"))
            } else {
                Err(format!("no session matches {to:?}"))
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
        assert_eq!(
            call(HANDOFF_TOOL, json!({"prompt": "Goal: ..."}))["isError"],
            false
        );
        assert_eq!(call(HANDOFF_TOOL, json!({"prompt": " "}))["isError"], true);
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
            [TOOL, ASK_TOOL, HANDOFF_TOOL, LIST_TOOL, MESSAGE_TOOL]
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
        for (claude, ours) in [
            (crate::telemetry::claude::HANDOFF_TOOL, HANDOFF_TOOL),
            (crate::telemetry::claude::LIST_TOOL, LIST_TOOL),
            (crate::telemetry::claude::MESSAGE_TOOL, MESSAGE_TOOL),
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
