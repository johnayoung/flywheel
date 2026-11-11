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
            ("SessionStart", json!({"session_id": "conv", "model": "claude-x"})),
            ("UserPromptSubmit", json!({"prompt": "fix the failing test"})),
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
    let bash = json!({"tool_name": "Bash", "tool_use_id": "tu1", "tool_input": {"command": "rm -rf x"}});
    telemetry.record("s", claude::hook("UserPromptSubmit", &json!({"prompt": "clean"}), 0));
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
    telemetry.record("s", claude::hook("UserPromptSubmit", &json!({"prompt": "more"}), 100_000));
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
            ("SubagentStart", json!({"agent_id": "ag1", "agent_type": "Explore"})),
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
    assert_eq!(running.turn_tools, 2, "the turn's own call and its subagent's");
    hooks(
        &telemetry,
        "s",
        10_000,
        &[
            ("SubagentStop", json!({"agent_id": "ag1", "agent_type": "Explore"})),
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
    assert_eq!((turn.source, turn.status), (WorkSource::Turn, WorkStatus::Done));
    assert_eq!(sub.source, WorkSource::Subagent);
    assert_eq!(sub.parent.as_deref(), Some("t1"));
    assert_eq!(sub.title, "Map routes");
    assert_eq!(sub.kind.as_deref(), Some("Explore"));
    assert_eq!(sub.agent.as_deref(), Some("ag1"));
    assert_eq!((sub.status, sub.tools, sub.tokens), (WorkStatus::Done, 1, Some(4321)));
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
    let plan = |items: serde_json::Value| {
        json!({"tool_name": claude::PROGRESS_TOOL, "tool_use_id": "p", "tool_input": {"items": items}})
    };
    telemetry.record("s", claude::hook("UserPromptSubmit", &json!({"prompt": "ship it"}), 0));
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
    assert_eq!((declared.revision, declared.added, declared.dropped), (2, 1, 1));
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
    assert_eq!((todo.source.as_str(), todo.done, todo.total), ("todo", 1, 2));

    hooks(
        &telemetry,
        "task",
        0,
        &[
            ("TaskCreated", json!({"task_id": "1", "task_subject": "Explore"})),
            ("TaskCreated", json!({"task_id": "2", "task_subject": "Implement"})),
            (
                "PreToolUse",
                json!({"tool_name": "TaskUpdate", "tool_use_id": "u", "tool_input":
                    {"taskId": "1", "status": "in_progress"}}),
            ),
            ("TaskCompleted", json!({"task_id": "1", "task_subject": "Explore"})),
        ],
    );
    let task = telemetry.snapshot("task").unwrap().plan.unwrap();
    assert_eq!((task.source.as_str(), task.done, task.total), ("task", 1, 2));
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
    assert_eq!((context.used, context.window, context.pct), (85_000, 200_000, 42.5));
    assert!(context.exact);
    assert_eq!(snap.usage.unwrap().cost_usd, Some(1.25));
    assert_eq!(snap.rate.len(), 2);
    assert_eq!(snap.rate[1].window, "seven_day");
    assert_eq!(snap.rate[1].resets_at, Some(1_900_500_000));
    // The second, identical reading recorded nothing.
    assert_eq!(telemetry.events(None, "s", 0, 100, false).unwrap().len(), 5);
    assert_eq!(claude::default_line(&line), "Fable 5.2 \u{b7} ctx 42% \u{b7} $1.25");
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
    assert_eq!((usage.input, usage.cache_read, usage.output), (600, 400, 50));
    assert_eq!(snap.rate[0].window, "five_hour");
    assert_eq!(snap.rate[1].window, "seven_day");
    let plan = snap.plan.as_ref().unwrap();
    assert_eq!((plan.source.as_str(), plan.done, plan.total), ("codex", 0, 2));
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
    assert_eq!(telemetry.events(None, "cx", 0, 100, false).unwrap().len(), before);
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
    assert_eq!(collector.ingest(received).unwrap(), 0, "a repeat is ignored");
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
    push(HOST, "done", 2, 3_000, What::TurnFinished {});
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
        all.sessions.iter().map(|s| s.session.as_str()).collect::<Vec<_>>(),
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
        },
        What::SessionFinished { reason: None },
        What::TurnStarted { title: "t".into() },
        What::TurnFinished {},
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
