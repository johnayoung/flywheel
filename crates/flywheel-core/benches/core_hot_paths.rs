//! Criterion benches for flywheel-core hot paths.
//!
//! * `envelope_parse/*` -- `parse_envelope` over a realistic agent iteration
//!   transcript (several KB of prose and code before the trailing
//!   `<!-- LOOP_STATUS -->` fence), once per harness iteration.
//! * `core_store_sqlite/*` -- the file-backed `SqliteStore` read/write calls
//!   the harness makes per transition, against a temp database pre-populated
//!   with `STORE_RUNS` runs (three attempts each) so index and page-cache
//!   behaviour resembles a working store rather than an empty one.
//! * `agent_ingest/*` -- the per-agent-event persistence path of a live
//!   run: each normalized `AgentEvent` serialized to its recorded message
//!   (`event_to_message`) and appended to the run's JSONL telemetry stream
//!   (`RunTelemetry::sdk_message` -> `FileTelemetrySink`), for a realistic
//!   mix of assistant text, tool calls, usage readings and multi-KB tool
//!   results.
//!
//! Sample sizes and measurement windows are tuned so the whole file runs in
//! well under a minute; the improvement-loop scorecard reads the medians.

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Duration;

use chrono::{DateTime, Utc};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use flywheel_agents::{AgentEvent, EventType};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

use flywheel_core::agents_invoke::event_to_message;
use flywheel_core::harness::{RunTelemetry, SharedSink};
use flywheel_core::telemetry_file::FileTelemetrySink;

use flywheel_core::envelope::{CLOSING_FENCE, EnvelopeResult, OPENING_FENCE, parse_envelope};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::store::{AttemptStore, LifecycleFilter, LifecycleStore, TaskStore};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::{CommandGrader, Grader, Task};

/// Distinct runs seeded into the store before measuring.
const STORE_RUNS: usize = 500;
/// Runs per task id (retries / re-runs share a task id).
const RUNS_PER_TASK: usize = 2;
/// Attempts recorded per seeded run.
const ATTEMPTS_PER_RUN: u32 = 3;

fn t(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_779_969_600 + second, 0).expect("in-range timestamp")
}

/// A transcript shaped like a real agent iteration: narrative prose, a
/// fenced diff, tool-output excerpts, then the envelope at the end.
fn agent_transcript(envelope_payload: Option<&str>) -> String {
    let mut out = String::with_capacity(16 * 1024);
    for step in 0..24 {
        out.push_str(&format!(
            "Step {step}: inspected `crates/flywheel-core/src/module_{step}.rs` and ran \
             `cargo test -p flywheel-core module_{step}`; 14 passed, 0 failed. The change \
             keeps the public signature stable and threads the new field through the \
             loader so existing callers are unaffected.\n\n"
        ));
        if step % 4 == 0 {
            out.push_str("```diff\n");
            for line in 0..12 {
                out.push_str(&format!(
                    "-    let value_{line} = old_call(&input, {line});\n\
                     +    let value_{line} = new_call(&input, {line}).context(\"step {step}\")?;\n"
                ));
            }
            out.push_str("```\n\n");
        }
    }
    if let Some(payload) = envelope_payload {
        out.push_str(OPENING_FENCE);
        out.push('\n');
        out.push_str(payload);
        out.push('\n');
        out.push_str(CLOSING_FENCE);
        out.push('\n');
    }
    out
}

fn bench_envelope(c: &mut Criterion) {
    let verify = agent_transcript(Some(
        r#"{"intent":"verify","reason":"all command graders pass locally; ready for verification"}"#,
    ));
    let blocked = agent_transcript(Some(
        r#"{"intent":"blocked","reason":"needs credentials and a generated fixture","requires":[{"type":"env_var_set","name":"GITHUB_TOKEN"},{"type":"file_exists","path":"tests/fixtures/generated.json"},{"type":"command_grader","name":"integration"}]}"#,
    ));
    let missing = agent_transcript(None);
    assert!(matches!(parse_envelope(&verify), EnvelopeResult::Valid(_)));
    assert!(matches!(parse_envelope(&blocked), EnvelopeResult::Valid(_)));
    assert!(matches!(parse_envelope(&missing), EnvelopeResult::Missing));

    let mut group = c.benchmark_group("envelope_parse");
    group
        .sample_size(50)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    group.bench_function("verify", |b| b.iter(|| parse_envelope(black_box(&verify))));
    group.bench_function("blocked_requires", |b| {
        b.iter(|| parse_envelope(black_box(&blocked)));
    });
    group.bench_function("missing", |b| {
        b.iter(|| parse_envelope(black_box(&missing)))
    });
    group.finish();
}

fn command_task(task_id: &str) -> Task {
    Task::new(
        format!("Implement {task_id} end to end."),
        vec![Grader::Command(
            CommandGrader::new("cargo test --workspace").expect("valid grader"),
        )],
    )
    .with_id(task_id)
}

fn task_id_for(run: usize) -> String {
    format!("task-{:04}", run / RUNS_PER_TASK)
}

/// A seeded store: `STORE_RUNS` runs driven to DONE, each with attempts.
fn seeded_store() -> (TempDir, SqliteStore) {
    let dir = TempDir::new().expect("tempdir");
    let store = SqliteStore::open(dir.path().join("flywheel.db")).expect("open sqlite store");
    for run in 0..STORE_RUNS {
        let task_id = task_id_for(run);
        if run % RUNS_PER_TASK == 0 {
            store
                .save_task(&command_task(&task_id), t(0))
                .expect("save task");
        }
        let run_id = format!("run-{run:05}");
        let mut lc = Lifecycle::new(&task_id).with_run_id(&run_id);
        store.create_lifecycle(&lc).expect("create lifecycle");
        let offset = i64::try_from(run).expect("small run index") * 10;
        for (target, step) in [
            (Status::Ready, 1),
            (Status::Running, 2),
            (Status::Validating, 3),
            (Status::Done, 4),
        ] {
            let prior = lc.version;
            lc.transition_to(target, "", Some(t(offset + step)))
                .expect("legal transition");
            store
                .update_lifecycle(&lc, prior)
                .expect("update lifecycle");
        }
        for number in 1..=ATTEMPTS_PER_RUN {
            let mut attempt = Attempt::new(number, t(offset + 2), &run_id);
            attempt.ended_at = Some(t(offset + 3));
            attempt.outcome = Some(if number == ATTEMPTS_PER_RUN {
                Outcome::Succeeded
            } else {
                Outcome::ValidationFailed
            });
            attempt.input_tokens = 12_000;
            attempt.output_tokens = 3_400;
            attempt.turns = 18;
            store
                .save_attempt(&run_id, &attempt, None)
                .expect("save attempt");
        }
    }
    (dir, store)
}

fn bench_store(c: &mut Criterion) {
    let (_dir, store) = seeded_store();
    let probe_run = format!("run-{:05}", STORE_RUNS / 2);
    let probe_task = task_id_for(STORE_RUNS / 2);

    let mut group = c.benchmark_group("core_store_sqlite");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));

    group.bench_function("load_lifecycle", |b| {
        b.iter(|| {
            store
                .load_lifecycle(black_box(&probe_run))
                .expect("load lifecycle")
                .expect("seeded run present")
        });
    });

    group.bench_function("list_lifecycles_by_task", |b| {
        let filter = LifecycleFilter::default().with_task_id(probe_task.as_str());
        b.iter(|| {
            store
                .list_lifecycles(black_box(&filter))
                .expect("list lifecycles")
        });
    });

    // Versioned update of one live run: the per-transition write the
    // harness makes (the row's own version bumped, checked against prior).
    let hot_task = "task-hot";
    store
        .save_task(&command_task(hot_task), t(0))
        .expect("save hot task");
    let mut hot = Lifecycle::new(hot_task).with_run_id("run-hot");
    store.create_lifecycle(&hot).expect("create hot lifecycle");
    group.bench_function("update_lifecycle", |b| {
        b.iter(|| {
            let prior = hot.version;
            hot.version += 1;
            store
                .update_lifecycle(black_box(&hot), prior)
                .expect("versioned update");
        });
    });

    // Seed write: a brand-new run row (create) for an existing task.
    let mut next_run: u64 = 0;
    group.bench_function("create_lifecycle", |b| {
        b.iter(|| {
            next_run += 1;
            let lc = Lifecycle::new(hot_task).with_run_id(format!("run-new-{next_run:09}"));
            store.create_lifecycle(black_box(&lc)).expect("create");
        });
    });
    group.finish();
}

/// Agent events ingested per measured iteration (a long multi-iteration run).
const INGEST_EVENTS: usize = 2000;

/// A realistic normalized event stream: per agent turn an assistant text
/// block, a tool call, a usage reading, and the tool's multi-KB result.
fn agent_event_stream(count: usize) -> Vec<AgentEvent> {
    let prose = "Inspected the module and ran the focused tests; the change keeps the \
                 public signature stable and threads the new field through the loader. "
        .repeat(4);
    let mut tool_output = String::new();
    for line in 0..120 {
        let _ = writeln!(
            tool_output,
            "crates/flywheel-core/src/module_{line}.rs:{line}: fn handler_{line}(input: &str) -> Result<(), Error> {{"
        );
    }
    (0..count)
        .map(|i| {
            let turn = i / 4;
            let mut event = match i % 4 {
                0 => AgentEvent::with_native(
                    EventType::AssistantMessage,
                    "assistant",
                    json!({"text": format!("Turn {turn}: {prose}")}),
                ),
                1 => AgentEvent::with_native(
                    EventType::ToolCallStarted,
                    "assistant",
                    json!({
                        "tool_use_id": format!("toolu_{turn:08}"),
                        "tool_name": "Bash",
                        "tool_input": {
                            "command": format!("cargo test -p flywheel-core module_{turn} -- --nocapture"),
                            "description": "Run the focused module tests",
                        },
                    }),
                ),
                2 => AgentEvent::with_native(
                    EventType::ContextUsage,
                    "assistant",
                    json!({"usage": {
                        "input_tokens": 12 + turn,
                        "cache_creation_input_tokens": 1_800,
                        "cache_read_input_tokens": 40_000 + 900 * turn,
                        "output_tokens": 350,
                    }}),
                ),
                _ => AgentEvent::with_native(
                    EventType::ToolCallFinished,
                    "user",
                    json!({
                        "tool_use_id": format!("toolu_{turn:08}"),
                        "is_error": false,
                        "content": tool_output,
                    }),
                ),
            };
            "claude-code".clone_into(&mut event.source);
            event.sequence = i as u64;
            event
        })
        .collect()
}

fn bench_agent_ingest(c: &mut Criterion) {
    let events = agent_event_stream(INGEST_EVENTS);
    let mut group = c.benchmark_group("agent_ingest");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(5));

    // One fresh run file per measured iteration: the append-mode JSONL
    // stream a live run writes as each event arrives.
    group.bench_function("telemetry_file_2000_events", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().expect("tempdir");
                let sink: SharedSink = Arc::new(FileTelemetrySink::new(dir.path()));
                let telemetry = RunTelemetry::new(Some(sink), "run-ingest", Arc::new(Utc::now));
                (dir, telemetry)
            },
            |(dir, telemetry)| {
                for event in &events {
                    let message = event_to_message(black_box(event));
                    telemetry.sdk_message(&message, 1, 1);
                }
                (dir, telemetry)
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_envelope, bench_store, bench_agent_ingest);
criterion_main!(benches);
