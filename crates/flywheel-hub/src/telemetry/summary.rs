//! What a tool call acts on, in one short scrubbed line.
//!
//! The telemetry store keeps a summary of each call (a path, the start of
//! a command, a pattern), never the call's whole input and never its
//! output: those stay in the agent's transcript. Every summary passes
//! [`crate::activity::redact`] (token-shaped words, `KEY=value` secrets,
//! URL passwords) and is capped at [`SUMMARY_CAP`] characters.

use serde_json::Value;

/// Longest summary kept (characters).
pub const SUMMARY_CAP: usize = 200;

/// Longest turn ending kept (characters): enough for a closing question
/// or a command handed to the operator, never the whole reply.
pub const ENDING_CAP: usize = 1500;

/// The input fields that say what a call acts on, most telling first.
const TELLING: &[&str] = &[
    "command",
    "file_path",
    "notebook_path",
    "path",
    "pattern",
    "query",
    "url",
    "description",
    "subject",
    "skill",
    "taskId",
    "prompt",
    "name",
];

/// One line of free text: scrubbed, whitespace collapsed, capped.
pub fn text(raw: &str) -> String {
    // Bounded before it is scrubbed: a pasted megabyte is not walked.
    let head: String = raw.chars().take(SUMMARY_CAP * 8).collect();
    let scrubbed = crate::activity::redact(&head);
    let mut out = String::with_capacity(scrubbed.len().min(SUMMARY_CAP * 4));
    let mut gap = false;
    for c in scrubbed.chars() {
        if c.is_whitespace() || c.is_control() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.push(c);
        }
    }
    if out.chars().count() > SUMMARY_CAP {
        let mut cut: String = out.chars().take(SUMMARY_CAP - 1).collect();
        cut.push('\u{2026}');
        return cut;
    }
    out
}

/// The end of a turn's closing message: its last [`ENDING_CAP`]
/// characters (a cut start is marked with an ellipsis), scrubbed, line
/// breaks kept (a fenced command stays runnable), blank runs collapsed.
pub fn ending(raw: &str) -> Option<String> {
    let total = raw.chars().count();
    let skip = total.saturating_sub(ENDING_CAP);
    let tail: String = raw.chars().skip(skip).collect();
    let scrubbed = crate::activity::redact(&tail);
    let mut out = String::with_capacity(scrubbed.len());
    let mut blank = 0;
    for line in scrubbed.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    if out.is_empty() {
        return None;
    }
    Some(if skip > 0 {
        format!("\u{2026}{out}")
    } else {
        out
    })
}

/// What the call `name` with `input` acts on (`None`: nothing telling).
pub fn tool(name: &str, input: &Value) -> Option<String> {
    // Codex hands a call's arguments over as a JSON string.
    let parsed;
    let input = match input {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(value @ Value::Object(_)) => {
                parsed = value;
                &parsed
            }
            _ => return some(text(first_line(raw))),
        },
        other => other,
    };
    let str_of = |key: &str| input.get(key).and_then(Value::as_str);
    let line = match name {
        "Grep" | "Glob" => match (str_of("pattern"), str_of("path")) {
            (Some(pattern), Some(path)) => format!("{pattern} in {path}"),
            (Some(pattern), None) => pattern.to_owned(),
            _ => return None,
        },
        // Codex's shell: an argv, the script last (`bash -lc SCRIPT`).
        "shell" | "local_shell" | "exec_command" | "shell_command" => match input.get("command") {
            Some(Value::Array(argv)) => argv
                .iter()
                .filter_map(Value::as_str)
                .next_back()
                .unwrap_or_default()
                .to_owned(),
            Some(Value::String(command)) => command.clone(),
            _ => str_of("cmd")?.to_owned(),
        },
        "apply_patch" => patch_files(str_of("input").or_else(|| str_of("patch"))?),
        _ => TELLING.iter().find_map(|key| str_of(key))?.to_owned(),
    };
    some(text(&line))
}

fn some(line: String) -> Option<String> {
    (!line.is_empty()).then_some(line)
}

fn first_line(raw: &str) -> &str {
    raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// The files a Codex patch touches (`*** Update File: path`).
fn patch_files(patch: &str) -> String {
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("*** ")?;
            let (verb, path) = rest.split_once(" File: ")?;
            matches!(verb, "Update" | "Add" | "Delete").then_some(path.trim())
        })
        .collect();
    if files.is_empty() {
        first_line(patch).to_owned()
    } else {
        files.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_summary_is_one_capped_line() {
        assert_eq!(text("  cargo   test\n -p hub "), "cargo test -p hub");
        let long = text(&"x".repeat(900));
        assert_eq!(long.chars().count(), SUMMARY_CAP);
        assert!(long.ends_with('\u{2026}'));
    }

    #[test]
    fn secrets_never_reach_a_summary() {
        let line = tool(
            "Bash",
            &json!({"command": "curl -H 'Authorization: Bearer sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789' https://x.test && API_KEY=hunter2hunter2 ./run"}),
        )
        .unwrap();
        assert!(!line.contains("sk-ant-api03"), "{line}");
        assert!(!line.contains("hunter2hunter2"), "{line}");
        assert!(line.contains("curl"), "{line}");
    }

    #[test]
    fn each_tool_says_what_it_acts_on() {
        assert_eq!(
            tool(
                "Read",
                &json!({"file_path": "/repo/src/main.rs", "limit": 20})
            )
            .as_deref(),
            Some("/repo/src/main.rs")
        );
        assert_eq!(
            tool("Grep", &json!({"pattern": "fn main", "path": "src"})).as_deref(),
            Some("fn main in src")
        );
        assert_eq!(
            tool(
                "Agent",
                &json!({"description": "Map the routes", "prompt": "long"})
            )
            .as_deref(),
            Some("Map the routes")
        );
        assert_eq!(
            tool("mcp__x__do", &json!({"query": "hello"})).as_deref(),
            Some("hello")
        );
        assert_eq!(tool("Odd", &json!({"count": 3})), None);
    }

    #[test]
    fn codex_calls_are_read_from_their_argument_strings() {
        let shell = tool(
            "shell",
            &Value::String(r#"{"command":["bash","-lc","cargo fmt --check"]}"#.into()),
        );
        assert_eq!(shell.as_deref(), Some("cargo fmt --check"));
        let patch = tool(
            "apply_patch",
            &json!({"input": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n*** Add File: b.rs\n"}),
        );
        assert_eq!(patch.as_deref(), Some("src/a.rs, b.rs"));
    }
}
