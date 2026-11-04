use super::*;
use crate::test_golden::{TRANSCRIPTS, assert_golden, fixture};
use serde_json::json;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// Every line of `path`, parsed, as (start, line).
fn parse_file(path: &Path, format: Format) -> Vec<(u64, Line)> {
    let mut tail = JsonlTail::new(path.to_path_buf());
    let mut out = Vec::new();
    let done = tail
        .read(None, &mut |read| match read {
            Read::Restart => panic!("a first read never restarts"),
            Read::Line { bytes, start } => {
                let value: Value = serde_json::from_slice(bytes).unwrap();
                out.push((start, parse(format, &value, start)));
            }
        })
        .unwrap();
    assert!(done);
    out
}

#[test]
fn captured_transcripts_parse_to_the_golden_events() {
    for &(name, codex) in TRANSCRIPTS {
        let format = if codex { Format::Codex } else { Format::Claude };
        let mut out = String::new();
        for (start, line) in parse_file(&fixture(name), format) {
            writeln!(out, "@{start} {:?}", line.timestamp).unwrap();
            for event in line.events {
                writeln!(out, "  {event:?}").unwrap();
            }
        }
        assert_golden(&format!("{name}.events.txt"), &out);
    }
}

#[test]
fn line_offsets_are_where_each_line_starts() {
    for &(name, codex) in TRANSCRIPTS {
        let format = if codex { Format::Codex } else { Format::Claude };
        let bytes = std::fs::read(fixture(name)).unwrap();
        let mut expect = vec![0u64];
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' && i + 1 < bytes.len() {
                expect.push(i as u64 + 1);
            }
        }
        let starts: Vec<u64> = parse_file(&fixture(name), format)
            .into_iter()
            .map(|(start, _)| start)
            .collect();
        assert_eq!(starts, expect, "{name}");
    }
}

#[test]
fn a_real_claude_tool_run_gives_its_tool_calls_model_and_conversation() {
    let lines = parse_file(&fixture("claude-tools.jsonl"), Format::Claude);
    let events: Vec<&Event> = lines.iter().flat_map(|(_, l)| &l.events).collect();
    let calls: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { name, input, .. } => Some((name.as_str(), input.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|(name, _)| *name == "Bash"), "{calls:?}");
    assert!(calls[0].1.starts_with("command: ls -la "), "{calls:?}");
    assert!(calls[1].1.contains("Cargo.toml"), "{calls:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Model(m) if m == "claude-haiku-5-5"))
    );
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Session(id) if id == "45e7bccc-889d-471a-942e-c00859549c20")
        )
    );
}

#[test]
fn claude_lines_name_their_conversation_and_model_and_keep_sidechains_to_usage() {
    let lines = parse_file(&fixture("claude-edge-cases.jsonl"), Format::Claude);
    let sessions: Vec<&String> = lines
        .iter()
        .flat_map(|(_, l)| &l.events)
        .filter_map(|e| match e {
            Event::Session(id) => Some(id),
            _ => None,
        })
        .collect();
    // Every line but the summary names the conversation.
    assert_eq!(sessions.len(), lines.len() - 1);
    assert!(
        sessions
            .iter()
            .all(|s| *s == "5c0e2f8a-1b7d-4e55-9a3c-0d2b6f4e8a11")
    );
    let side = &lines[4].1.events;
    assert!(side.iter().any(|e| matches!(e, Event::Usage(t) if t.fast)));
    assert!(
        !side
            .iter()
            .any(|e| matches!(e, Event::Text { .. } | Event::ToolCall { .. })),
        "a sidechain's text is a subagent's: {side:?}"
    );
    // A synthetic message has text but no model and no usage.
    let synthetic = &lines[6].1.events;
    assert!(synthetic.iter().any(|e| matches!(e, Event::Text { .. })));
    assert!(
        !synthetic
            .iter()
            .any(|e| matches!(e, Event::Model(_) | Event::Usage(_)))
    );
}

#[test]
fn codex_lines_give_session_model_and_count_events_and_drop_injected_context() {
    let lines = parse_file(&fixture("codex-rollout.jsonl"), Format::Codex);
    let events: Vec<&Event> = lines.iter().flat_map(|(_, l)| &l.events).collect();
    assert_eq!(
        events[0],
        &Event::Session("0199a6c2-5d3e-7f10-8b4a-2e6c9d1f0a77".into())
    );
    let models: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Model(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["gpt-5-codex", "gpt-5"]);
    let counts = events
        .iter()
        .filter(|e| matches!(e, Event::TokenCount { .. }))
        .count();
    // `info: null` is no count.
    assert_eq!(counts, 3);
    assert!(
        !events.iter().any(|e| matches!(e,
        Event::Text { parts, .. } if parts.iter().any(|p| p.text.contains("environment_context"))))
    );
    // A developer message is neither the user's nor the assistant's.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Text { parts, .. } if parts[0].text == "sandbox rules"))
    );
}

#[test]
fn ids_fall_back_in_order() {
    let claude = |line: Value| parse(Format::Claude, &line, 0).events;
    let by_message = claude(json!({"type": "user", "message": {"id": "m", "content": "hi"}}));
    assert!(matches!(&by_message[0], Event::Text { id, .. } if id == "m"));
    // Neither: no text (nothing to name it by).
    assert!(claude(json!({"type": "user", "message": {"content": "hi"}})).is_empty());
    let codex = |line: Value| parse(Format::Codex, &line, 9).events;
    let call = codex(json!({"type": "function_call", "call_id": "c", "name": "shell"}));
    assert_eq!(
        call,
        [Event::ToolCall {
            id: "c".into(),
            name: "shell".into(),
            input: String::new(),
            call: Some("c".into()),
            subject: None,
        }]
    );
    let bare = codex(json!({"type": "custom_tool_call"}));
    assert!(matches!(&bare[0], Event::ToolCall { id, name, .. } if id == "@9" && name == "tool"));
}

#[test]
fn tool_inputs_read_as_key_value_lines() {
    assert_eq!(
        tool_text(&json!({"command": "ls", "timeout": 5})),
        "command: ls\ntimeout: 5"
    );
    assert_eq!(tool_text(&json!("{\"a\":\"b\"}")), "a: b");
    assert_eq!(tool_text(&json!("plain")), "plain");
    assert_eq!(tool_text(&Value::Null), "");
    assert_eq!(tool_text(&json!([1, 2])), "[1,2]");
    // A question tool's input: its questions and options as items.
    let asked = json!({"questions": [{
        "question": "Which color?",
        "options": [
            {"label": "Red", "description": "Warm"},
            {"label": "Blue", "description": "Cool"}
        ],
        "multiSelect": false
    }]});
    assert_eq!(
        tool_text(&asked),
        "questions:\n  - question: Which color?\n    options:\n      - label: Red\n        description: Warm\n      - label: Blue\n        description: Cool\n    multiSelect: false"
    );
}

// --- the tail -----------------------------------------------------------

fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// What a read hands out, owned.
fn drain(tail: &mut JsonlTail) -> Vec<(Option<u64>, String)> {
    let mut out = Vec::new();
    tail.read(None, &mut |read| match read {
        Read::Restart => out.push((None, "restart".to_owned())),
        Read::Line { bytes, start } => {
            out.push((Some(start), String::from_utf8_lossy(bytes).into_owned()));
        }
    })
    .unwrap();
    out
}

#[test]
fn tails_hold_the_incomplete_line_and_resume_from_a_line_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, b"one\ntw");
    let mut tail = JsonlTail::new(path.clone());
    assert_eq!(drain(&mut tail), [(Some(0), "one".to_owned())]);
    assert_eq!(tail.position(), 4);
    assert_eq!(tail.pending_len(), 2);
    append(&path, b"o\nthree\n");
    assert_eq!(
        drain(&mut tail),
        [(Some(4), "two".to_owned()), (Some(8), "three".to_owned())]
    );
    assert_eq!(tail.position(), 14);
    // Resumed where it stopped (mid-line bytes are read again).
    append(&path, b"fo");
    drain(&mut tail);
    let mut resumed = JsonlTail::resume(path.clone(), tail.position(), tail.identity());
    append(&path, b"ur\n");
    assert_eq!(drain(&mut resumed), [(Some(14), "four".to_owned())]);
    assert!(drain(&mut resumed).is_empty());
}

#[test]
fn replaced_or_truncated_files_restart_and_over_long_lines_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, b"a\nb\n");
    let mut tail = JsonlTail::new(path.clone());
    assert_eq!(drain(&mut tail).len(), 2);
    // Truncated (same file, shorter).
    std::fs::write(&path, b"c\n").unwrap();
    assert_eq!(
        drain(&mut tail),
        [(None, "restart".to_owned()), (Some(0), "c".to_owned())]
    );
    // Replaced (a new file at the path, longer than what was read).
    let other = dir.path().join("new.jsonl");
    std::fs::write(&other, b"d\ne\nf\n").unwrap();
    std::fs::rename(&other, &path).unwrap();
    assert_eq!(drain(&mut tail)[0], (None, "restart".to_owned()));
    // An over-long line is dropped whole; the next one reads.
    let mut long = vec![b'x'; MAX_LINE_BYTES + 10];
    long.extend_from_slice(b"\ng\n");
    append(&path, &long);
    assert_eq!(
        drain(&mut tail),
        [(Some(6 + MAX_LINE_BYTES as u64 + 11), "g".to_owned())]
    );
    assert!(tail.pending.capacity() <= MAX_LINE_BYTES + READ_CHUNK);
}

#[test]
fn tails_stop_at_the_deadline_and_report_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut tail = JsonlTail::new(path.clone());
    assert!(tail.read(None, &mut |_| {}).is_err());
    append(&path, b"a\n");
    let past = Instant::now() - Duration::from_millis(1);
    let mut lines = 0;
    assert!(!tail.read(Some(past), &mut |_| lines += 1).unwrap());
    assert_eq!(lines, 0);
    assert!(
        tail.read(Some(Instant::now() + Duration::from_secs(60)), &mut |_| {
            lines += 1
        })
        .unwrap()
    );
    assert_eq!(lines, 1);
}

#[test]
fn tool_results_pair_with_their_calls() {
    let claude = |line: Value| parse(Format::Claude, &line, 0).events;
    let used = claude(
        json!({"type": "assistant", "uuid": "a", "message": {"content": [
        {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}]}}),
    );
    assert!(matches!(&used[0], Event::ToolCall { call: Some(c), .. } if c == "toolu_1"));
    let results = claude(json!({"type": "user", "uuid": "u", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb"},
        {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": true,
         "content": [{"type": "text", "text": "Exit code 1"}, {"type": "image", "source": {}}, {"type": "text", "text": "boom"}]},
    ]}}));
    assert_eq!(
        results,
        [
            Event::ToolResult {
                id: "u#result0".into(),
                call: "toolu_1".into(),
                output: "a\nb".into(),
                error: false
            },
            Event::ToolResult {
                id: "u#result1".into(),
                call: "toolu_2".into(),
                output: "Exit code 1\n[image]\nboom".into(),
                error: true
            },
        ]
    );
    let codex = |line: Value| parse(Format::Codex, &line, 3).events;
    let wrapped = codex(
        json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call_9",
        "output": "{\"output\":\"total 0\\n\",\"metadata\":{\"exit_code\":0}}"}}),
    );
    assert_eq!(
        wrapped,
        [Event::ToolResult {
            id: "call_9#result".into(),
            call: "call_9".into(),
            output: "total 0\n".into(),
            error: false
        }]
    );
    let plain =
        codex(json!({"type": "custom_tool_call_output", "call_id": "c2", "output": "Done!"}));
    assert!(matches!(&plain[0], Event::ToolResult { output, .. } if output == "Done!"));
    assert!(codex(json!({"type": "function_call_output", "output": "orphan"})).is_empty());
}
