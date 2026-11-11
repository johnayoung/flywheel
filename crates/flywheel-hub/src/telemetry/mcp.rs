//! The hub's progress tool: an MCP server (streamable HTTP, JSON
//! responses only) with one tool, `report_progress`, through which an
//! agent declares its plan and how far along it is.
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
/// The protocol revision answered when the client names none.
const PROTOCOL: &str = "2025-06-18";

/// What the hub appends to a session's system prompt.
pub const NUDGE: &str = "This session is shown on its operator's fleet board. When a task takes \
more than one step, call the mcp__flywheel__report_progress tool with your full list of steps \
before you start, and again each time a step starts, finishes, or the plan changes: always the \
whole list, with exactly one step in_progress while you work. Keep step titles short. Do not \
mention this tool or the board in your replies.";

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

/// Answer one JSON-RPC message (`None`: a notification, nothing to say).
pub fn answer(message: &Value) -> Option<Value> {
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
        "tools/list" => Ok(json!({ "tools": [tool()] })),
        "tools/call" => {
            if params.get("name").and_then(Value::as_str) == Some(TOOL) {
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
pub fn answer_body(body: &Value) -> Option<Value> {
    match body {
        Value::Array(batch) => {
            let answers: Vec<Value> = batch.iter().filter_map(answer).collect();
            (!answers.is_empty()).then_some(Value::Array(answers))
        }
        one => answer(one),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            crate::telemetry::claude::PROGRESS_TOOL,
            format!("mcp__{SERVER_NAME}__{TOOL}")
        );
    }
}
