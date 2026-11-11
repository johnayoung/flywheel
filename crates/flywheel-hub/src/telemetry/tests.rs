use super::event::AskKind;
use super::reduce::{WorkSource, WorkStatus};
use super::*;
use serde_json::json;

const HOST: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";

fn open() -> (tempfile::TempDir, Telemetry) {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open(dir.path(), HOST).unwrap();
    (dir, telemetry)
}

/// Record Claude hook payloads as they would arrive, a second apart.
fn hooks(telemetry: &Telemetry, session: &str, start: i64, events: &[(&str, serde_json::Value)]) {
    for (at, (event, payload)) in events.iter().enumerate() {
        telemetry.record(
            session,
            claude::hook(event, payload, start + at as i64 * 1000),
        );
    }
}

#[test]
fn a_tool_call_is_paired_by_its_id_with_duration_and_outcome() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "s1",
        10_000,
        &[
            (
                "SessionStart",
                json!({"session_id": "conv", "model": "claude-x"}),
            ),
            (
                "UserPromptSubmit",
                json!({"prompt": "fix the failing test"}),
            ),
            (
                "PreToolUse",
                json!({"tool_name": "Bash", "tool_use_id": "tu1", "tool_input": {"command": "cargo test"}}),
            ),
            (
                "PreToolUse",
                json!({"tool_name": "Read", "tool_use_id": "tu2", "tool_input": {"file_path": "/r/a.rs"}}),
            ),
            (
                "PostToolUse",
                json!({"tool_name": "Read", "tool_use_id": "tu2", "duration_ms": 12}),
            ),
            (
                "PostToolUseFailure",
                json!({"tool_name": "Bash", "tool_use_id": "tu1", "duration_ms": 900, "error": "exit 101"}),
            ),
        ],
    );
    let snap = telemetry.snapshot("s1").unwrap();
    assert_eq!(snap.status, "working");
    assert_eq!(snap.turn_title.as_deref(), Some("fix the failing test"));
    assert_eq!((snap.tools, snap.tool_errors, snap.open_tools), (2, 1, 0));
    assert_eq!(snap.last_error.as_deref(), Some("Bash: exit 101"));
    assert_eq!(snap.model.as_deref(), Some("claude-x"));

    let events = telemetry.events(None, "s1", 0, 100, false).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.what.kind()).collect();
    assert_eq!(
        kinds,
        [
            "session.started",
            "turn.started",
            "tool.started",
            "tool.started",
            "tool.finished",
            "tool.finished"
        ]
    );
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    // Every call names the turn it ran in.
    assert!(events[2..].iter().all(|e| e.work.as_deref() == Some("t1")));
    let What::ToolFinished {
        id,
        outcome,
        duration_ms,
        ..
    } = &events[5].what
    else {
        panic!("{:?}", events[5]);
    };
    assert_eq!(
        (id.as_str(), *outcome, *duration_ms),
        ("tu1", Outcome::Error, Some(900))
    );
    let What::ToolStarted { summary, .. } = &events[2].what else {
        panic!();
    };
    assert_eq!(summary.as_deref(), Some("cargo test"));
}

#[test]
fn time_blocked_on_a_person_excludes_the_tools_own_run_time() {
    let (_dir, telemetry) = open();
    let bash =
        json!({"tool_name": "Bash", "tool_use_id": "tu1", "tool_input": {"command": "rm -rf x"}});
    telemetry.record(
        "s",
        claude::hook("UserPromptSubmit", &json!({"prompt": "clean"}), 0),
    );
    telemetry.record("s", claude::hook("PreToolUse", &bash, 1_000));
    telemetry.record("s", claude::hook("PermissionRequest", &bash, 1_100));
    let blocked = telemetry.snapshot("s").unwrap();
    assert_eq!(blocked.status, "blocked");
    assert_eq!(blocked.since, 1_100);
    assert_eq!(blocked.blocked_on, Some(WaitReason::Permission));
    assert_eq!(blocked.blocked_tool.as_deref(), Some("Bash"));
    // Approved 60 s later; the command then ran for 5 s.
    telemetry.record(
        "s",
        claude::hook(
            "PostToolUse",
            &json!({"tool_name": "Bash", "tool_use_id": "tu1", "duration_ms": 5_000}),
            66_100,
        ),
    );
    let after = telemetry.snapshot("s").unwrap();
    assert_eq!(after.status, "working");
    assert_eq!(after.blocked_ms, 60_000);
    assert_eq!(after.waits, 1);
    // Finished, then a new prompt 30 s later: that is awaiting, not blocked.
    telemetry.record("s", claude::hook("Stop", &json!({}), 70_000));
    assert_eq!(telemetry.snapshot("s").unwrap().status, "done");
    telemetry.record(
        "s",
        claude::hook("UserPromptSubmit", &json!({"prompt": "more"}), 100_000),
    );
    let again = telemetry.snapshot("s").unwrap();
    assert_eq!((again.blocked_ms, again.awaiting_ms), (60_000, 30_000));
    assert_eq!(again.turns, 2);
}

#[test]
fn subagents_hang_under_their_turn_and_own_their_calls() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "map the repo"})),
            (
                "PreToolUse",
                json!({"tool_name": "Agent", "tool_use_id": "tuA", "tool_input":
                    {"description": "Map routes", "subagent_type": "Explore", "prompt": "..."}}),
            ),
            (
                "SubagentStart",
                json!({"agent_id": "ag1", "agent_type": "Explore"}),
            ),
            (
                "PreToolUse",
                json!({"agent_id": "ag1", "tool_name": "Grep", "tool_use_id": "tuG",
                    "tool_input": {"pattern": "route("}}),
            ),
            (
                "PostToolUse",
                json!({"agent_id": "ag1", "tool_name": "Grep", "tool_use_id": "tuG", "duration_ms": 30}),
            ),
        ],
    );
    let running = telemetry.snapshot("s").unwrap();
    assert_eq!((running.subagents_active, running.subagents_total), (1, 1));
    assert_eq!(
        running.turn_tools, 2,
        "the turn's own call and its subagent's"
    );
    hooks(
        &telemetry,
        "s",
        10_000,
        &[
            (
                "SubagentStop",
                json!({"agent_id": "ag1", "agent_type": "Explore"}),
            ),
            (
                "PostToolUse",
                json!({"tool_name": "Agent", "tool_use_id": "tuA", "duration_ms": 9_000,
                    "tool_response": {"totalTokens": 4321}}),
            ),
            ("Stop", json!({})),
        ],
    );
    let detail = telemetry.detail(None, "s").unwrap();
    assert_eq!(detail.work.len(), 2);
    let (turn, sub) = (&detail.work[0], &detail.work[1]);
    assert_eq!(
        (turn.source, turn.status),
        (WorkSource::Turn, WorkStatus::Done)
    );
    assert_eq!(sub.source, WorkSource::Subagent);
    assert_eq!(sub.parent.as_deref(), Some("t1"));
    assert_eq!(sub.title, "Map routes");
    assert_eq!(sub.kind.as_deref(), Some("Explore"));
    assert_eq!(sub.agent.as_deref(), Some("ag1"));
    assert_eq!(
        (sub.status, sub.tools, sub.tokens),
        (WorkStatus::Done, 1, Some(4321))
    );
    assert_eq!(detail.snapshot.subagents_active, 0);
    let events = telemetry.events(None, "s", 0, 100, false).unwrap();
    let grep = events
        .iter()
        .find(|e| matches!(&e.what, What::ToolStarted { name, .. } if name == "Grep"))
        .unwrap();
    assert_eq!(grep.work.as_deref(), Some("a:tuA"));
    assert_eq!(grep.agent.as_deref(), Some("ag1"));
}

#[test]
fn a_declared_plan_is_diffed_and_its_calls_counted_on_the_active_item() {
    let (_dir, telemetry) = open();
    let plan = |items: serde_json::Value| json!({"tool_name": claude::PROGRESS_TOOL, "tool_use_id": "p", "tool_input": {"items": items}});
    telemetry.record(
        "s",
        claude::hook("UserPromptSubmit", &json!({"prompt": "ship it"}), 0),
    );
    telemetry.record(
        "s",
        claude::hook(
            "PreToolUse",
            &plan(json!([
                {"title": "Write the store", "status": "in_progress"},
                {"title": "Wire the route", "status": "pending"},
                {"title": "Docs", "status": "pending"},
            ])),
            1_000,
        ),
    );
    telemetry.record(
        "s",
        claude::hook(
            "PreToolUse",
            &json!({"tool_name": "Edit", "tool_use_id": "e1", "tool_input": {"file_path": "store.rs"}}),
            2_000,
        ),
    );
    let first = telemetry.snapshot("s").unwrap().plan.unwrap();
    assert_eq!((first.done, first.total, first.revision), (0, 3, 1));
    assert_eq!(first.current.as_deref(), Some("Write the store"));
    assert_eq!(first.source, "declared");
    // The plan changes shape: one item done, one dropped, one added.
    telemetry.record(
        "s",
        claude::hook(
            "PreToolUse",
            &plan(json!([
                {"title": "Write the store", "status": "completed"},
                {"title": "Wire the route", "status": "in_progress"},
                {"title": "Load test", "status": "pending"},
            ])),
            3_000,
        ),
    );
    let detail = telemetry.detail(None, "s").unwrap();
    let declared = &detail.plans[0];
    assert_eq!(
        (declared.revision, declared.added, declared.dropped),
        (2, 1, 1)
    );
    let store = &declared.items[0];
    assert_eq!(store.status, ItemStatus::Done);
    assert_eq!((store.started, store.finished), (Some(1_000), Some(3_000)));
    // The progress tool's own first call came before the item was active;
    // the edit ran under it.
    assert_eq!(store.tools, 1);
    let progress = detail.snapshot.plan.unwrap();
    assert_eq!((progress.done, progress.total), (1, 3));
    assert_eq!(progress.current.as_deref(), Some("Wire the route"));
    // Statuses alone moving is not a revision.
    telemetry.record(
        "s",
        claude::hook(
            "PreToolUse",
            &plan(json!([
                {"title": "Write the store", "status": "completed"},
                {"title": "Wire the route", "status": "completed"},
                {"title": "Load test", "status": "in_progress"},
            ])),
            4_000,
        ),
    );
    assert_eq!(telemetry.snapshot("s").unwrap().plan.unwrap().revision, 2);
    let edit = telemetry
        .events(None, "s", 0, 100, false)
        .unwrap()
        .into_iter()
        .find(|e| matches!(&e.what, What::ToolStarted { name, .. } if name == "Edit"))
        .unwrap();
    assert_eq!(edit.item.as_deref(), Some(store.id.as_str()));
}

#[test]
fn todo_and_task_tools_are_plans_too() {
    let (_dir, telemetry) = open();
    telemetry.record(
        "todo",
        claude::hook(
            "PreToolUse",
            &json!({"tool_name": "TodoWrite", "tool_use_id": "t", "tool_input": {"todos": [
                {"content": "A", "status": "completed", "activeForm": "Doing A"},
                {"content": "B", "status": "in_progress", "activeForm": "Doing B"}]}}),
            0,
        ),
    );
    let todo = telemetry.snapshot("todo").unwrap().plan.unwrap();
    assert_eq!(
        (todo.source.as_str(), todo.done, todo.total),
        ("todo", 1, 2)
    );

    hooks(
        &telemetry,
        "task",
        0,
        &[
            (
                "TaskCreated",
                json!({"task_id": "1", "task_subject": "Explore"}),
            ),
            (
                "TaskCreated",
                json!({"task_id": "2", "task_subject": "Implement"}),
            ),
            (
                "PreToolUse",
                json!({"tool_name": "TaskUpdate", "tool_use_id": "u", "tool_input":
                    {"taskId": "1", "status": "in_progress"}}),
            ),
            (
                "TaskCompleted",
                json!({"task_id": "1", "task_subject": "Explore"}),
            ),
        ],
    );
    let task = telemetry.snapshot("task").unwrap().plan.unwrap();
    assert_eq!(
        (task.source.as_str(), task.done, task.total),
        ("task", 1, 2)
    );
    assert_eq!(task.current.as_deref(), Some("Implement"));
}

#[test]
fn the_status_line_gives_context_cost_and_rate_limits_once_per_change() {
    let (_dir, telemetry) = open();
    let line = json!({
        "session_id": "conv",
        "model": {"id": "claude-fable-5-2", "display_name": "Fable 5.2"},
        "cost": {"total_cost_usd": 1.25},
        "context_window": {"used_percentage": 42.5, "context_window_size": 200000,
            "total_input_tokens": 90000, "total_output_tokens": 4000},
        "rate_limits": {"five_hour": {"used_percentage": 12.0, "resets_at": 1900000000},
            "seven_day": {"used_percentage": 61.0, "resets_at": 1900500000}},
    });
    telemetry.record("s", claude::status_line(&line, 1_000));
    telemetry.record("s", claude::status_line(&line, 2_000));
    let snap = telemetry.snapshot("s").unwrap();
    let context = snap.context.unwrap();
    assert_eq!(
        (context.used, context.window, context.pct),
        (85_000, 200_000, 42.5)
    );
    assert!(context.exact);
    assert_eq!(snap.usage.unwrap().cost_usd, Some(1.25));
    assert_eq!(snap.rate.len(), 2);
    assert_eq!(snap.rate[1].window, "seven_day");
    assert_eq!(snap.rate[1].resets_at, Some(1_900_500_000));
    // The second, identical reading recorded nothing.
    assert_eq!(telemetry.events(None, "s", 0, 100, false).unwrap().len(), 5);
    assert_eq!(
        claude::default_line(&line),
        "Fable 5.2 \u{b7} ctx 42% \u{b7} $1.25"
    );
}

#[test]
fn a_prompt_claude_code_submits_itself_is_named_not_shown() {
    let (_dir, telemetry) = open();
    telemetry.record(
        "s",
        claude::hook(
            "UserPromptSubmit",
            &json!({"prompt": "<task-notification>\n<task-id>a1</task-id>\n</task-notification>"}),
            0,
        ),
    );
    assert_eq!(
        telemetry.snapshot("s").unwrap().turn_title.as_deref(),
        Some("(a background task finished)")
    );
}

#[test]
fn a_second_notice_of_the_same_prompt_is_not_a_second_wait() {
    let (_dir, telemetry) = open();
    let bash = json!({"tool_name": "Bash", "tool_use_id": "t"});
    telemetry.record("s", claude::hook("PreToolUse", &bash, 0));
    telemetry.record("s", claude::hook("PermissionRequest", &bash, 10));
    telemetry.record(
        "s",
        claude::hook(
            "Notification",
            &json!({"notification_type": "permission_prompt"}),
            20,
        ),
    );
    let waits = telemetry
        .events(None, "s", 0, 100, false)
        .unwrap()
        .iter()
        .filter(|e| e.what.kind() == "wait.started")
        .count();
    assert_eq!(waits, 1);
    assert_eq!(telemetry.snapshot("s").unwrap().waits, 1);
}

#[test]
fn a_failed_turn_and_a_rate_limit_show_on_the_board() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "go"})),
            (
                "StopFailure",
                json!({"error": "rate_limit", "error_details": "429 Too Many Requests"}),
            ),
        ],
    );
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!(snap.status, "failed");
    assert_eq!(snap.errors, 1);
    assert_eq!(snap.limited_at, Some(1_000));
    assert_eq!(
        snap.last_error.as_deref(),
        Some("rate_limit: 429 Too Many Requests")
    );
}

#[test]
fn a_codex_rollout_reads_as_the_same_events() {
    let (_dir, telemetry) = open();
    let lines = [
        json!({"timestamp": "2026-11-11T10:00:00.000Z", "type": "session_meta", "payload": {"id": "thread-1"}}),
        json!({"timestamp": "2026-11-11T10:00:01.000Z", "type": "event_msg", "payload":
            {"type": "task_started", "model_context_window": 258400}}),
        json!({"timestamp": "2026-11-11T10:00:01.100Z", "type": "event_msg", "payload":
            {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": "tidy the docs"}]}}}),
        json!({"timestamp": "2026-11-11T10:00:01.200Z", "type": "turn_context", "payload": {"model": "gpt-6"}}),
        json!({"timestamp": "2026-11-11T10:00:02.000Z", "type": "response_item", "payload":
            {"type": "function_call", "name": "update_plan", "call_id": "c0",
             "arguments": "{\"explanation\":\"start\",\"plan\":[{\"step\":\"Read\",\"status\":\"in_progress\"},{\"step\":\"Edit\",\"status\":\"pending\"}]}"}}),
        json!({"timestamp": "2026-11-11T10:00:02.100Z", "type": "response_item", "payload":
            {"type": "function_call_output", "call_id": "c0", "output": "Plan updated"}}),
        json!({"timestamp": "2026-11-11T10:00:03.000Z", "type": "response_item", "payload":
            {"type": "function_call", "name": "shell", "call_id": "c1",
             "arguments": "{\"command\":[\"bash\",\"-lc\",\"make docs\"]}"}}),
        json!({"timestamp": "2026-11-11T10:00:05.500Z", "type": "response_item", "payload":
            {"type": "function_call_output", "call_id": "c1",
             "output": "{\"output\":\"boom\",\"metadata\":{\"exit_code\":2}}"}}),
        json!({"timestamp": "2026-11-11T10:00:06.000Z", "type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": {"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 50},
                     "last_token_usage": {"input_tokens": 600}, "model_context_window": 258400},
            "rate_limits": {"primary": {"used_percent": 2.0, "window_minutes": 300, "resets_at": 1788813431},
                            "secondary": {"used_percent": 1.0, "window_minutes": 10080}}}}),
        json!({"timestamp": "2026-11-11T10:00:07.000Z", "type": "event_msg", "payload": {"type": "task_complete"}}),
    ];
    for line in &lines {
        assert!(codex::relevant(line.to_string().as_bytes()), "{line}");
        telemetry.record_log("cx", codex::line(line, 0));
    }
    let detail = telemetry.detail(None, "cx").unwrap();
    let snap = &detail.snapshot;
    assert_eq!(snap.harness.as_deref(), Some("codex"));
    assert_eq!(snap.conversation.as_deref(), Some("thread-1"));
    assert_eq!(snap.model.as_deref(), Some("gpt-6"));
    assert_eq!(snap.status, "done");
    assert_eq!(snap.turns, 1);
    assert_eq!(snap.turn_title.as_deref(), Some("tidy the docs"));
    assert_eq!((snap.tools, snap.tool_errors), (2, 1));
    assert_eq!(snap.context.as_ref().unwrap().used, 600);
    let usage = snap.usage.as_ref().unwrap();
    assert_eq!(
        (usage.input, usage.cache_read, usage.output),
        (600, 400, 50)
    );
    assert_eq!(snap.rate[0].window, "five_hour");
    assert_eq!(snap.rate[1].window, "seven_day");
    let plan = snap.plan.as_ref().unwrap();
    assert_eq!(
        (plan.source.as_str(), plan.done, plan.total),
        ("codex", 0, 2)
    );
    // The shell call's duration is the time between its two lines.
    assert_eq!(detail.work[0].tool_ms, 100 + 2_500);
    let finished: Vec<_> = telemetry
        .events(None, "cx", 0, 100, false)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.what {
            What::ToolFinished { name, outcome, .. } => Some((name, outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [
            ("update_plan".to_owned(), Outcome::Ok),
            ("shell".to_owned(), Outcome::Error)
        ]
    );
    // The rollout read again from the start records nothing twice.
    let before = telemetry.events(None, "cx", 0, 100, false).unwrap().len();
    for line in &lines[..9] {
        telemetry.record_log("cx", codex::line(line, 0));
    }
    assert_eq!(
        telemetry.events(None, "cx", 0, 100, false).unwrap().len(),
        before
    );
}

#[test]
fn state_survives_a_restart_and_the_log_continues_its_numbering() {
    let dir = tempfile::tempdir().unwrap();
    // Recent: rows a month old are pruned when the writer starts.
    let start = now_ms() - 60_000;
    {
        let telemetry = Telemetry::open(dir.path(), HOST).unwrap();
        hooks(
            &telemetry,
            "s",
            start,
            &[
                ("UserPromptSubmit", json!({"prompt": "one"})),
                (
                    "PreToolUse",
                    json!({"tool_name": "Bash", "tool_use_id": "tu1", "tool_input": {"command": "sleep 9"}}),
                ),
            ],
        );
        telemetry.stop();
    }
    let telemetry = Telemetry::open(dir.path(), HOST).unwrap();
    assert!(telemetry.snapshot("s").is_none(), "not in memory yet");
    let saved = telemetry.detail(None, "s").unwrap();
    assert_eq!(saved.snapshot.open_tools, 1);
    telemetry.record(
        "s",
        claude::hook(
            "PostToolUse",
            &json!({"tool_name": "Bash", "tool_use_id": "tu1", "duration_ms": 9_000}),
            start + 9_500,
        ),
    );
    let events = telemetry.events(None, "s", 0, 100, false).unwrap();
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(telemetry.snapshot("s").unwrap().open_tools, 0);
    let newest = telemetry.events(None, "s", 0, 2, true).unwrap();
    assert_eq!(newest.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(telemetry.events(None, "s", 2, 100, false).unwrap().len(), 1);
}

#[test]
fn a_collector_takes_batches_once_and_folds_the_same_state() {
    let (_edge_dir, edge) = open();
    hooks(
        &edge,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "build"})),
            (
                "PreToolUse",
                json!({"tool_name": "Bash", "tool_use_id": "tu1", "tool_input": {"command": "make"}}),
            ),
            ("PermissionRequest", json!({"tool_name": "Bash"})),
        ],
    );
    let batch: Vec<Event> = edge
        .outbox("upstream:c", 100)
        .unwrap()
        .into_iter()
        .map(|(_, event)| event)
        .collect();
    assert_eq!(batch.len(), 3);

    let dir = tempfile::tempdir().unwrap();
    let collector = Telemetry::open(dir.path(), OTHER).unwrap();
    let wire = forward::encode(&batch).unwrap();
    let received = forward::decode(&wire, true, 1 << 20).unwrap();
    assert_eq!(collector.ingest(received.clone()).unwrap(), 3);
    assert_eq!(
        collector.ingest(received).unwrap(),
        0,
        "a repeat is ignored"
    );
    assert_eq!(
        collector.detail(Some(HOST), "s").unwrap().snapshot,
        edge.snapshot("s").unwrap(),
        "the collector's board is the edge's"
    );
    // The cursor moves only when told the batch was accepted.
    assert_eq!(edge.outbox("upstream:c", 100).unwrap().len(), 3);
    let last = edge.outbox("upstream:c", 100).unwrap().last().unwrap().0;
    edge.advance("upstream:c", last).unwrap();
    assert!(edge.outbox("upstream:c", 100).unwrap().is_empty());
    // A collector does not forward what it collected, and refuses events
    // claiming to be its own or naming no hub.
    assert!(collector.outbox("upstream:x", 100).unwrap().is_empty());
    let mut forged = batch[0].clone();
    forged.host = OTHER.to_owned();
    assert!(collector.ingest(vec![forged.clone()]).is_err());
    forged.host = "nope".to_owned();
    assert!(collector.ingest(vec![forged]).is_err());
}

#[test]
fn the_fleet_lists_what_needs_a_person_first_and_counts_by_status() {
    let dir = tempfile::tempdir().unwrap();
    let collector = Telemetry::open(dir.path(), OTHER).unwrap();
    let mut events = Vec::new();
    let mut push = |host: &str, session: &str, seq: u64, ts: i64, what: What| {
        events.push(Event {
            host: host.to_owned(),
            session: session.to_owned(),
            seq,
            ts,
            agent: None,
            work: None,
            item: None,
            what,
        });
    };
    let turn = || What::TurnStarted {
        title: "work".to_owned(),
    };
    let h2 = "a".repeat(32);
    push(HOST, "working", 1, 1_000, turn());
    push(HOST, "done", 1, 2_000, turn());
    push(HOST, "done", 2, 3_000, What::TurnFinished { ending: None });
    push(&h2, "blocked", 1, 500, turn());
    push(
        &h2,
        "blocked",
        2,
        600,
        What::WaitStarted {
            reason: WaitReason::Question,
            tool: None,
        },
    );
    push(
        &h2,
        "blocked",
        3,
        700,
        What::Usage {
            input: 10,
            output: 5,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(2.5),
        },
    );
    collector.ingest(events).unwrap();
    let all = collector
        .fleet(&FleetQuery {
            limit: 50,
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(
        all.sessions
            .iter()
            .map(|s| s.session.as_str())
            .collect::<Vec<_>>(),
        ["blocked", "working", "done"]
    );
    assert_eq!((all.total, all.hosts), (3, 2));
    let blocked = all.counts.iter().find(|c| c.status == "blocked").unwrap();
    assert_eq!((blocked.sessions, blocked.cost_usd), (1, 2.5));
    let filtered = collector
        .fleet(&FleetQuery {
            statuses: vec!["working".to_owned(), "done".to_owned()],
            host: Some(HOST.to_owned()),
            limit: 1,
            offset: 1,
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(filtered.total, 2);
    assert_eq!(filtered.sessions.len(), 1);
    assert_eq!(filtered.sessions[0].session, "done");
    let recent = collector
        .fleet(&FleetQuery {
            active_since: Some(900),
            limit: 50,
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(recent.total, 2);
}

#[test]
fn changed_sessions_are_announced_once_per_flush() {
    let (_dir, telemetry) = open();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    telemetry.on_change(move |snapshot| sink.lock().unwrap().push(snapshot.session));
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "a"})),
            ("Stop", json!({})),
            ("UserPromptSubmit", json!({"prompt": "b"})),
        ],
    );
    telemetry.flush();
    telemetry.flush();
    assert_eq!(*seen.lock().unwrap(), ["s"]);
}

#[test]
fn every_kind_is_listed_and_round_trips_through_the_store() {
    let whats = [
        What::SessionStarted {
            harness: "claude".into(),
            conversation: None,
            model: None,
            label: None,
            dir: None,
            links: None,
        },
        What::Continued {
            successor: "s2".into(),
        },
        What::SessionFinished { reason: None },
        What::TurnStarted { title: "t".into() },
        What::TurnFinished { ending: None },
        What::TurnFailed {
            error: "e".into(),
            detail: None,
        },
        What::ToolStarted {
            id: "i".into(),
            name: "n".into(),
            summary: None,
            subagent: None,
        },
        What::ToolFinished {
            id: "i".into(),
            name: "n".into(),
            outcome: Outcome::Denied,
            duration_ms: None,
            error: None,
            tokens: None,
        },
        What::Asked {
            kind: AskKind::Decide,
            text: "pick".into(),
            command: None,
            options: vec!["a".into()],
            prompt: None,
        },
        What::WaitStarted {
            reason: WaitReason::Permission,
            tool: None,
        },
        What::SubagentStarted {
            id: "a".into(),
            kind: None,
        },
        What::SubagentFinished { id: "a".into() },
        What::PlanSet {
            source: "todo".into(),
            items: vec![],
            note: None,
        },
        What::PlanItem {
            source: "task".into(),
            id: "1".into(),
            title: None,
            status: None,
        },
        What::ContextUsage {
            used: 1,
            window: 2,
            exact: false,
        },
        What::Usage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            cost_usd: None,
        },
        What::RateLimit {
            window: "five_hour".into(),
            used_pct: 1.0,
            resets_at: None,
        },
        What::RateLimited { detail: None },
        What::Compact {
            phase: "pre".into(),
            trigger: None,
        },
    ];
    let kinds: Vec<&str> = whats.iter().map(What::kind).collect();
    assert_eq!(kinds, event::KINDS);
    for what in whats {
        assert_eq!(
            What::from_parts(what.kind(), what.body()).as_ref(),
            Some(&what)
        );
    }
}

/// The collector's side at the scale the design is for: ten thousand
/// sessions on a hundred hosts, delivered in forwarder-sized batches in
/// the order hosts would interleave them.
#[test]
fn ten_thousand_sessions_on_a_hundred_hosts_fold_and_list() {
    const HOSTS: usize = 100;
    const PER_HOST: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let collector = Telemetry::open(dir.path(), OTHER).unwrap();
    let start = now_ms() - 60_000;
    let event = |host: usize, session: usize, seq: u64, what: What| Event {
        host: format!("{host:032x}"),
        session: format!("s{session}"),
        seq,
        ts: start + seq as i64 * 1000,
        agent: None,
        work: None,
        item: None,
        what,
    };
    // Per session: a prompt, three paired calls, a usage reading; then a
    // tenth block on a person and a tenth finish.
    let mut events = Vec::new();
    for round in 0..9u64 {
        for host in 0..HOSTS {
            for session in 0..PER_HOST {
                let n = host * PER_HOST + session;
                let call = format!("c{}", (round.max(1) - 1) / 2);
                let what = match round {
                    0 => What::TurnStarted {
                        title: format!("task {n}"),
                    },
                    1 | 3 | 5 => What::ToolStarted {
                        id: call,
                        name: "Bash".to_owned(),
                        summary: Some("cargo test".to_owned()),
                        subagent: None,
                    },
                    2 | 4 | 6 => What::ToolFinished {
                        id: call,
                        name: "Bash".to_owned(),
                        outcome: Outcome::Ok,
                        duration_ms: Some(900),
                        error: None,
                        tokens: None,
                    },
                    7 => What::Usage {
                        input: 1000,
                        output: 100,
                        cache_read: 0,
                        cache_write: 0,
                        cost_usd: Some(0.01),
                    },
                    _ if n.is_multiple_of(10) => What::WaitStarted {
                        reason: WaitReason::Question,
                        tool: None,
                    },
                    _ if n % 10 == 1 => What::TurnFinished { ending: None },
                    _ => continue,
                };
                events.push(event(host, session, round + 1, what));
            }
        }
    }
    let total = events.len();
    let began = std::time::Instant::now();
    let mut accepted = 0;
    for batch in events.chunks(forward::BATCH_EVENTS) {
        accepted += collector.ingest(batch.to_vec()).unwrap();
    }
    let ingested = began.elapsed();
    assert_eq!(accepted, total);
    let began = std::time::Instant::now();
    let page = collector
        .fleet(&FleetQuery {
            limit: 200,
            ..FleetQuery::default()
        })
        .unwrap();
    let first_page = began.elapsed();
    let began = std::time::Instant::now();
    let again = collector
        .fleet(&FleetQuery {
            limit: 200,
            offset: 5000,
            ..FleetQuery::default()
        })
        .unwrap();
    let later_page = began.elapsed();
    eprintln!(
        "telemetry scale: {total} events for {} sessions ingested in {ingested:?} \
         ({:.0} events/s); first board page (commit included) {first_page:?}; a later page {later_page:?}",
        HOSTS * PER_HOST,
        total as f64 / ingested.as_secs_f64()
    );
    assert_eq!((page.total, page.hosts), (10_000, 100));
    let count = |status: &str| {
        page.counts
            .iter()
            .find(|c| c.status == status)
            .map_or(0, |c| c.sessions)
    };
    assert_eq!(
        (count("blocked"), count("done"), count("working")),
        (1000, 1000, 8000)
    );
    assert_eq!(page.sessions.len(), 200);
    assert!(page.sessions.iter().all(|s| s.status == "blocked"));
    assert!(
        page.sessions
            .iter()
            .all(|s| s.tools == 3 && s.open_tools == 0)
    );
    assert_eq!(again.sessions.len(), 200);
    assert!(again.sessions.iter().all(|s| s.status == "working"));
    // Every event is in the log once; a whole batch again changes nothing.
    assert_eq!(
        collector
            .ingest(events[..forward::BATCH_EVENTS].to_vec())
            .unwrap(),
        0
    );
    let one = collector
        .events(Some(&format!("{:032x}", 42)), "s7", 0, 100, false)
        .unwrap();
    assert_eq!(one.len(), 8);
}

#[test]
fn a_new_session_arriving_as_one_large_batch_is_on_the_board_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let collector = Telemetry::open(dir.path(), OTHER).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    collector.on_change(move |snapshot| sink.lock().unwrap().push(snapshot.seq));
    let start = now_ms() - 1_000_000;
    let events: Vec<Event> = (1..=924u64)
        .map(|seq| Event {
            host: HOST.to_owned(),
            session: "big".to_owned(),
            seq,
            ts: start + seq as i64,
            agent: None,
            work: None,
            item: None,
            what: if seq == 1 {
                What::TurnStarted {
                    title: "long".to_owned(),
                }
            } else if seq == 500 {
                What::Compact {
                    phase: "pre".to_owned(),
                    trigger: Some("auto".to_owned()),
                }
            } else if seq.is_multiple_of(2) {
                What::ToolStarted {
                    id: format!("c{seq}"),
                    name: "Read".to_owned(),
                    summary: None,
                    subagent: None,
                }
            } else {
                What::ToolFinished {
                    id: format!("c{}", seq - 1),
                    name: "Read".to_owned(),
                    outcome: Outcome::Ok,
                    duration_ms: Some(1),
                    error: None,
                    tokens: None,
                }
            },
        })
        .collect();
    assert_eq!(collector.ingest(events).unwrap(), 924);
    let page = collector
        .fleet(&FleetQuery {
            limit: 10,
            active_since: Some(now_ms() - 3_600_000),
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions[0].seq, 924);
    assert_eq!(page.sessions[0].compactions, 1);
    assert_eq!(*seen.lock().unwrap(), [924], "announced once, whole");
    // A compaction belongs to the turn it happened in.
    let compact = collector
        .events(Some(HOST), "big", 499, 1, false)
        .unwrap()
        .remove(0);
    assert_eq!(compact.what.kind(), "compact");
    assert_eq!(compact.work.as_deref(), Some("t1"));
}

#[test]
fn a_turn_that_ends_on_a_question_is_asking_until_the_next_prompt() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "remove the board"})),
            (
                "Stop",
                json!({"last_assistant_message": "Removed it.\n\nShould I delete the mockup branch too?"}),
            ),
        ],
    );
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!(snap.status, "asking");
    assert_eq!(snap.since, 1_000);
    let ask = snap.ask.unwrap();
    assert_eq!(ask.kind, AskKind::Answer);
    assert_eq!(ask.source, "inferred");
    assert_eq!(ask.text, "Should I delete the mockup branch too?");
    // The turn's ending is in the log, for whoever reads it later.
    let events = telemetry.events(None, "s", 0, 10, false).unwrap();
    assert!(matches!(
        &events[1].what,
        What::TurnFinished { ending: Some(e) } if e.ends_with("branch too?")
    ));
    // The reply is the next prompt: the ask is answered.
    hooks(
        &telemetry,
        "s",
        5_000,
        &[("UserPromptSubmit", json!({"prompt": "yes"}))],
    );
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!((snap.status.as_str(), snap.ask), ("working", None));
    // A report is not an ask.
    hooks(
        &telemetry,
        "s",
        6_000,
        &[(
            "Stop",
            json!({"last_assistant_message": "Deleted. Nothing else is pending."}),
        )],
    );
    assert_eq!(telemetry.snapshot("s").unwrap().status, "done");
}

#[test]
fn a_declared_ask_holds_once_its_turn_ends() {
    let (_dir, telemetry) = open();
    let ask = |agent: Option<&str>, kind: &str, text: &str| {
        let mut payload = json!({"tool_name": claude::ASK_TOOL, "tool_use_id": format!("a-{text}"),
            "tool_input": {"kind": kind, "text": text, "command": "sudo systemctl restart rh-relay",
                           "options": ["keep", "drop"]}});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
        }
        payload
    };
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "cut over the relay"})),
            // A subagent's ask is its main agent's to pass on.
            ("PreToolUse", ask(Some("sub1"), "decide", "sub asks")),
            (
                "PreToolUse",
                ask(None, "run", "Restart the relay; it needs sudo."),
            ),
        ],
    );
    // While the turn runs the session is working, not asking.
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!(
        (snap.status.as_str(), snap.ask.is_none()),
        ("working", true)
    );
    hooks(
        &telemetry,
        "s",
        10_000,
        &[(
            "Stop",
            json!({"last_assistant_message": "Over to you. Paste the output when done?"}),
        )],
    );
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!((snap.status.as_str(), snap.since), ("asking", 10_000));
    let ask = snap.ask.unwrap();
    // The declared ask wins over the inferred question.
    assert_eq!(ask.kind, AskKind::Run);
    assert_eq!(ask.source, "declared");
    assert_eq!(ask.text, "Restart the relay; it needs sudo.");
    assert_eq!(
        ask.command.as_deref(),
        Some("sudo systemctl restart rh-relay")
    );
    assert_eq!(ask.options, ["keep", "drop"]);
    // A declared ask does not outlive the turn after it.
    hooks(
        &telemetry,
        "s",
        20_000,
        &[
            (
                "UserPromptSubmit",
                json!({"prompt": "done: active (running)"}),
            ),
            (
                "Stop",
                json!({"last_assistant_message": "Relay is healthy."}),
            ),
        ],
    );
    assert_eq!(telemetry.snapshot("s").unwrap().status, "done");
}

#[test]
fn asking_sessions_rank_right_after_blocked_ones() {
    let (_dir, telemetry) = open();
    let turn = |s: &str, at: i64, end: &str| {
        hooks(
            &telemetry,
            s,
            at,
            &[
                ("UserPromptSubmit", json!({"prompt": "go"})),
                ("Stop", json!({"last_assistant_message": end})),
            ],
        );
    };
    turn("done", 0, "All done.");
    turn("asking", 0, "Which one do you want?");
    hooks(
        &telemetry,
        "failed",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "go"})),
            ("StopFailure", json!({"error": "server_error"})),
        ],
    );
    hooks(
        &telemetry,
        "blocked",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "go"})),
            ("PermissionRequest", json!({"tool_name": "Bash"})),
        ],
    );
    let page = telemetry
        .fleet(&FleetQuery {
            limit: 10,
            ..FleetQuery::default()
        })
        .unwrap();
    let order: Vec<&str> = page.sessions.iter().map(|s| s.session.as_str()).collect();
    assert_eq!(order, ["blocked", "asking", "failed", "done"]);
}

#[test]
fn a_handoff_waits_with_its_prompt_until_a_successor_takes_it() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "old",
        0,
        &[
            (
                "UserPromptSubmit",
                json!({"prompt": "give me a prompt for a new session"}),
            ),
            (
                "PreToolUse",
                json!({"tool_name": claude::HANDOFF_TOOL, "tool_use_id": "h1",
                       "tool_input": {"reason": "Context is at 64%",
                                      "prompt": "Goal: build C2.\nDone: C1 (528d3993).\nToken ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"}}),
            ),
            (
                "Stop",
                json!({"last_assistant_message": "Handed off. Anything else?"}),
            ),
        ],
    );
    let snap = telemetry.snapshot("old").unwrap();
    assert_eq!(snap.status, "asking");
    let ask = snap.ask.unwrap();
    assert_eq!(ask.kind, AskKind::Handoff);
    assert_eq!(ask.text, "Context is at 64%");
    let prompt = ask.prompt.unwrap();
    assert!(prompt.starts_with("Goal: build C2.\nDone: C1"), "{prompt}");
    assert!(!prompt.contains("ghp_"), "scrubbed: {prompt}");
    // The successor starts: the old session's handoff is answered and it
    // names who took over; the new one names what it continues.
    telemetry.record(
        "old",
        vec![Draft::new(
            9_000,
            What::Continued {
                successor: "new".into(),
            },
        )],
    );
    let old = telemetry.snapshot("old").unwrap();
    assert_eq!(
        (old.status.as_str(), old.continued_by.as_deref()),
        ("done", Some("new"))
    );
    telemetry.record(
        "new",
        vec![Draft::new(
            9_500,
            What::SessionStarted {
                harness: "claude".into(),
                conversation: None,
                model: None,
                label: Some("claude 7".into()),
                dir: None,
                links: Some(event::SessionLinks {
                    continues: Some("old".into()),
                    ..Default::default()
                }),
            },
        )],
    );
    let new = telemetry.snapshot("new").unwrap();
    assert_eq!(new.links.continues.as_deref(), Some("old"));
    // Saying the same links again is not news.
    telemetry.record(
        "new",
        vec![Draft::new(
            9_600,
            What::SessionStarted {
                harness: "claude".into(),
                conversation: None,
                model: None,
                label: Some("claude 7".into()),
                dir: None,
                links: Some(event::SessionLinks {
                    continues: Some("old".into()),
                    ..Default::default()
                }),
            },
        )],
    );
    assert_eq!(
        telemetry.events(None, "new", 0, 10, false).unwrap().len(),
        1
    );
}

#[test]
fn a_handoff_without_a_prompt_is_not_an_ask() {
    let (_dir, telemetry) = open();
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("UserPromptSubmit", json!({"prompt": "go"})),
            (
                "PreToolUse",
                json!({"tool_name": claude::HANDOFF_TOOL, "tool_use_id": "h1", "tool_input": {"prompt": "  "}}),
            ),
            ("Stop", json!({"last_assistant_message": "Done."})),
        ],
    );
    assert_eq!(telemetry.snapshot("s").unwrap().status, "done");
}

#[test]
fn captured_runs_are_rolled_up_by_batch_parent_or_outside() {
    let (_dir, telemetry) = open();
    let run = |id: &str, links: SessionLinks, at: i64, cost: f64, end: bool| {
        let mut drafts = vec![
            Draft::new(
                at,
                What::SessionStarted {
                    harness: "claude".to_owned(),
                    conversation: Some(id.to_owned()),
                    model: None,
                    label: Some(id.to_owned()),
                    dir: None,
                    links: Some(links),
                },
            ),
            Draft::new(
                at,
                What::TurnStarted {
                    title: "go".to_owned(),
                },
            ),
            Draft::new(
                at,
                What::Usage {
                    input: 1,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: Some(cost),
                },
            ),
        ];
        if end {
            drafts.push(Draft::new(at, What::SessionFinished { reason: None }));
        }
        telemetry.record(id, drafts);
    };
    let batch = |b: &str| SessionLinks {
        captured: true,
        batch: Some(b.to_owned()),
        budget_usd: Some(1.0),
        ..SessionLinks::default()
    };
    run("a1", batch("analysts"), 100, 0.75, true);
    run("a2", batch("analysts"), 200, 0.5, false);
    run(
        "c1",
        SessionLinks {
            captured: true,
            opened_by: Some("lead".to_owned()),
            ..SessionLinks::default()
        },
        300,
        0.25,
        false,
    );
    run(
        "t1",
        SessionLinks {
            captured: true,
            ..SessionLinks::default()
        },
        50,
        0.0,
        false,
    );
    // The hub's own session is listed, never rolled up.
    run("lead", SessionLinks::default(), 400, 2.0, false);
    let page = telemetry
        .fleet(&FleetQuery {
            limit: 50,
            rollup: true,
            ..FleetQuery::default()
        })
        .unwrap();
    let listed: Vec<&str> = page.sessions.iter().map(|s| s.session.as_str()).collect();
    assert_eq!(listed, ["lead"]);
    assert_eq!(page.total, 1);
    let keys: Vec<&str> = page.batches.iter().map(|b| b.key.as_str()).collect();
    assert_eq!(keys, ["under:lead", "batch:analysts", "outside"]);
    let analysts = &page.batches[1];
    assert_eq!(analysts.label.as_deref(), Some("analysts"));
    assert_eq!(analysts.sessions, 2);
    assert!((analysts.cost_usd - 1.25).abs() < 1e-9);
    assert_eq!(analysts.budget_usd, Some(1.0));
    assert!(analysts.over_budget);
    // Most urgent status first.
    let statuses: Vec<&str> = analysts.counts.iter().map(|c| c.status.as_str()).collect();
    assert_eq!(statuses, ["working", "ended"]);
    assert_eq!(page.batches[0].parent.as_deref(), Some("lead"));
    assert!(!page.batches[0].over_budget);
    // One batch's runs.
    let one = telemetry
        .fleet(&FleetQuery {
            limit: 50,
            batch: Some("batch:analysts".to_owned()),
            ..FleetQuery::default()
        })
        .unwrap();
    let mut runs: Vec<&str> = one.sessions.iter().map(|s| s.session.as_str()).collect();
    runs.sort_unstable();
    assert_eq!(runs, ["a1", "a2"]);
    assert!(one.batches.is_empty());
    // Not rolled up: every session.
    let all = telemetry
        .fleet(&FleetQuery {
            limit: 50,
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(all.total, 5);
}

#[test]
fn a_store_from_before_rollups_gains_the_batch_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(store::DB_FILE);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 host TEXT NOT NULL, session TEXT NOT NULL, status TEXT NOT NULL,
                 rank INTEGER NOT NULL, last_ts INTEGER NOT NULL, cost REAL NOT NULL DEFAULT 0,
                 snap TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY (host, session)
             ) WITHOUT ROWID;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let telemetry = Telemetry::open(dir.path(), HOST).unwrap();
    telemetry.record(
        "s",
        vec![Draft::new(
            1,
            What::TurnStarted {
                title: "go".to_owned(),
            },
        )],
    );
    let page = telemetry
        .fleet(&FleetQuery {
            limit: 5,
            rollup: true,
            ..FleetQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 1);
}

#[test]
fn an_unanswered_ask_survives_a_relaunch_but_not_a_new_conversation() {
    let (_dir, telemetry) = open();
    let start = |conversation: &str| json!({"session_id": conversation, "source": "resume"});
    hooks(
        &telemetry,
        "s",
        0,
        &[
            ("SessionStart", start("c1")),
            ("UserPromptSubmit", json!({"prompt": "go"})),
            (
                "Stop",
                json!({"last_assistant_message": "Should I ship it?"}),
            ),
            ("SessionEnd", json!({"reason": "other"})),
        ],
    );
    assert_eq!(telemetry.snapshot("s").unwrap().status, "ended");
    // Resumed: the question still waits.
    hooks(&telemetry, "s", 100, &[("SessionStart", start("c1"))]);
    let snap = telemetry.snapshot("s").unwrap();
    assert_eq!(snap.status, "asking");
    assert_eq!(snap.ask.unwrap().text, "Should I ship it?");
    // `/clear`: a new conversation asks nothing.
    hooks(&telemetry, "s", 200, &[("SessionStart", start("c2"))]);
    assert_ne!(telemetry.snapshot("s").unwrap().status, "asking");
}
