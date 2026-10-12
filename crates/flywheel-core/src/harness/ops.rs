//! Out-of-loop operations on a persisted lifecycle: the stranded-run
//! backstop, the blocked-`requires` recheck, and the manual-approval
//! resolver. Each builds the same telemetry session and mirroring store the
//! loop uses so its ledger appends land in the run's stream.

use super::iterate::SteeringRun;
use super::requires::{graders_by_name, parse_blocked_requires_json};
use super::state::{Run, object};
use super::telemetry::{MirroringStore, RunTelemetry, SharedSink};
use super::validate::awaiting_approval_payload;
use super::{default_clock, default_monotonic};
use crate::envelope::BlockedRequirement;
use crate::events::{AttemptFinalized, AwaitingApproval, DomainEventPayload};
use crate::grader_manual::{
    ManualGate, build_approve_result, build_reject_result, next_pending_manual_gate,
};
use crate::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_REJECT, Clock, command_applied_event,
};
use crate::lifecycle::{Lifecycle, Outcome, Status};
use crate::store::{
    AttemptStore, ControlCommandRecord, ControlCommandStore, GraderResultStore, HarnessStore,
    JsonObject, LifecycleStore, StoreError,
};
use crate::task::{Grader, Task};
use serde_json::{Value, json};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Knobs for [`finalize_stranded_lifecycle`].
#[derive(Clone)]
pub struct FinalizeStrandedOptions {
    pub reason: String,
    pub classification: String,
    pub sink: Option<SharedSink>,
    pub now: Option<Clock>,
}

impl Default for FinalizeStrandedOptions {
    fn default() -> Self {
        Self {
            reason: "worker interrupted before finalization".to_owned(),
            classification: "worker_interrupted".to_owned(),
            sink: None,
            now: None,
        }
    }
}

impl fmt::Debug for FinalizeStrandedOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinalizeStrandedOptions")
            .field("reason", &self.reason)
            .field("classification", &self.classification)
            .field("sink", &self.sink.is_some())
            .finish_non_exhaustive()
    }
}

impl FinalizeStrandedOptions {
    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_now(mut self, now: Clock) -> Self {
        self.now = Some(now);
        self
    }
}

fn session(
    store: Arc<dyn HarnessStore>,
    run_id: &str,
    sink: Option<SharedSink>,
    now: Option<Clock>,
) -> (Arc<MirroringStore>, Arc<RunTelemetry>, Clock) {
    let clock = now.unwrap_or_else(default_clock);
    let telemetry = Arc::new(RunTelemetry::new(sink, run_id.to_owned(), clock.clone()));
    let store = Arc::new(MirroringStore::new(store, telemetry.clone()));
    (store, telemetry, clock)
}

/// Drain a lifecycle stranded in `RUNNING` or `VALIDATING` to `INTERRUPTED`.
///
/// The SIGKILL / OOM / reboot backstop: closes any open attempt as
/// `INTERNAL_ERROR`, emits a single `harness.crash`, and transitions the
/// lifecycle to `INTERRUPTED` so the retry budget is preserved. Returns
/// `false` (a no-op) when the lifecycle is missing or in any other status
/// -- `AWAITING_APPROVAL` is a durable park whose attempt was already
/// finalized at gate entry and is left untouched.
pub fn finalize_stranded_lifecycle(
    store: Arc<dyn HarnessStore>,
    run_id: &str,
    options: FinalizeStrandedOptions,
) -> Result<bool, StoreError> {
    let (store, telemetry, clock) = session(store, run_id, options.sink, options.now);
    let Some(lifecycle) = store.load_lifecycle(run_id)? else {
        return Ok(false);
    };
    if !matches!(lifecycle.status, Status::Running | Status::Validating) {
        return Ok(false);
    }
    let from_status = lifecycle.status;
    let run = Run::new(
        store.clone(),
        telemetry,
        lifecycle,
        clock,
        default_monotonic(),
        None,
    );

    let attempts = store.list_attempts(run_id)?;
    let open: Vec<_> = attempts.iter().filter(|a| a.ended_at.is_none()).collect();
    let last_attempt_number = open
        .last()
        .map(|a| a.number)
        .or_else(|| attempts.last().map(|a| a.number));
    for attempt in open {
        let ended_at = run.now();
        run.append_payload(
            ended_at,
            Some(attempt.number),
            DomainEventPayload::AttemptFinalized(AttemptFinalized {
                number: attempt.number,
                outcome: Outcome::InternalError,
                ended_at,
                agent_output: attempt.agent_output.clone(),
                error: options.reason.clone(),
            }),
        )?;
        run.emit_attempt_finalized(attempt.number, Outcome::InternalError, &options.reason);
    }
    run.emit(
        "harness.crash",
        object(json!({
            "classification": options.classification,
            "message": options.reason,
            "from_status": from_status.as_str(),
        })),
        last_attempt_number,
    );
    run.transition(Status::Interrupted, "")?;
    Ok(true)
}

/// Return value of [`recheck_blocked_lifecycle`].
///
/// `applied` is `true` only when `INTERRUPTED -> READY` actually landed.
/// `reason` is a short stable token (`not_blocked`, `dry_run`,
/// `unsatisfied`, `unblocked`, or `parse_error: ...`). `per_predicate`
/// mirrors the `harness.recheck_attempted` payload entries: one object per
/// persisted predicate with `type`, `identifier`, `satisfied`, `detail`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecheckOutcome {
    pub applied: bool,
    pub reason: String,
    pub per_predicate: Vec<JsonObject>,
}

impl RecheckOutcome {
    fn not_blocked() -> Self {
        Self {
            applied: false,
            reason: "not_blocked".to_owned(),
            per_predicate: Vec::new(),
        }
    }
}

/// Knobs for [`recheck_blocked_lifecycle`].
#[derive(Clone, Default)]
pub struct RecheckOptions {
    pub dry_run: bool,
    /// The task's sandbox the predicates are evaluated against; the
    /// process working directory when `None`.
    pub cwd: Option<PathBuf>,
    pub sink: Option<SharedSink>,
    pub now: Option<Clock>,
}

impl fmt::Debug for RecheckOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecheckOptions")
            .field("dry_run", &self.dry_run)
            .field("cwd", &self.cwd)
            .field("sink", &self.sink.is_some())
            .finish_non_exhaustive()
    }
}

impl RecheckOptions {
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_now(mut self, now: Clock) -> Self {
        self.now = Some(now);
        self
    }
}

fn predicate_entry(kind: &str, identifier: Value, satisfied: bool, detail: &str) -> JsonObject {
    object(json!({
        "type": kind,
        "identifier": identifier,
        "satisfied": satisfied,
        "detail": detail,
    }))
}

/// Evaluate one persisted predicate against `cwd` / the environment and the
/// task's named graders. Never fails: OS errors surface in `detail` with
/// `satisfied=false`.
///
/// `command_grader` predicates run the named grader's `run` string through
/// `/bin/sh -c` in `cwd` (the process working directory when `None`),
/// inheriting the environment; exit code `0` is satisfied. No
/// `grader_results` row is written: recheck is a control-plane operation
/// whose audit surface is the event payload.
pub fn evaluate_blocked_predicate(
    req: &BlockedRequirement,
    task: &Task,
    cwd: Option<&Path>,
) -> JsonObject {
    match req {
        BlockedRequirement::CommandGrader { name } => {
            let grader = graders_by_name(task).get(name.as_str()).copied();
            let Some(Grader::Command(grader)) = grader else {
                return predicate_entry("command_grader", json!(name), false, "grader not found");
            };
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-c").arg(&grader.run);
            command.stdin(std::process::Stdio::null());
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            match command.output() {
                Ok(output) => {
                    let exit_code = exit_code_of(&output.status);
                    predicate_entry(
                        "command_grader",
                        json!(name),
                        exit_code == 0,
                        &format!("exit_code={exit_code}"),
                    )
                }
                Err(err) => predicate_entry(
                    "command_grader",
                    json!(name),
                    false,
                    &format!("subprocess failed to start: {err}"),
                ),
            }
        }
        BlockedRequirement::FileExists { path, present } => {
            let probe = match cwd {
                Some(cwd) if !Path::new(path).is_absolute() => cwd.join(path),
                _ => PathBuf::from(path),
            };
            let exists = probe.exists();
            let satisfied = if *present { exists } else { !exists };
            let observed = if exists { "present" } else { "absent" };
            let expected = if *present { "present" } else { "absent" };
            let detail = if satisfied {
                observed.to_owned()
            } else {
                format!("observed={observed}, expected={expected}")
            };
            predicate_entry("file_exists", json!(path), satisfied, &detail)
        }
        BlockedRequirement::EnvVarSet { name } => {
            let satisfied = std::env::var(name).is_ok_and(|v| !v.is_empty());
            predicate_entry(
                "env_var_set",
                json!(name),
                satisfied,
                if satisfied { "set" } else { "unset_or_empty" },
            )
        }
    }
}

/// Python's `Popen.returncode`: the exit code, or `-signal` when killed.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    -1
}

/// Re-evaluate a blocked lifecycle's persisted `requires` predicates and,
/// when all are satisfied and not `dry_run`, transition
/// `INTERRUPTED -> READY` (which clears `blocked_requires_json`).
///
/// A lifecycle that is missing, not `INTERRUPTED`, or carries no snapshot
/// is a silent no-op (`not_blocked`). Every evaluated recheck emits
/// `harness.recheck_attempted` (per-predicate detail, `all_satisfied`,
/// `dry_run`); only an applied promotion emits `harness.unblocked`. An
/// unparseable snapshot is data corruption: surfaced as `parse_error: ...`
/// with one `parse_error` predicate entry, never a panic.
pub fn recheck_blocked_lifecycle(
    store: Arc<dyn HarnessStore>,
    run_id: &str,
    task: &Task,
    options: RecheckOptions,
) -> Result<RecheckOutcome, StoreError> {
    let (store, telemetry, clock) = session(store, run_id, options.sink, options.now);
    let Some(lifecycle) = store.load_lifecycle(run_id)? else {
        return Ok(RecheckOutcome::not_blocked());
    };
    if lifecycle.status != Status::Interrupted {
        return Ok(RecheckOutcome::not_blocked());
    }
    let Some(snapshot) = lifecycle.blocked_requires_json.clone() else {
        return Ok(RecheckOutcome::not_blocked());
    };
    let run = Run::new(
        store,
        telemetry,
        lifecycle,
        clock,
        default_monotonic(),
        None,
    );

    let parsed = match parse_blocked_requires_json(&snapshot) {
        Ok(parsed) => parsed,
        Err(error) => {
            let per_predicate = vec![predicate_entry("parse_error", Value::Null, false, &error)];
            run.emit(
                "harness.recheck_attempted",
                object(json!({
                    "per_predicate": per_predicate,
                    "all_satisfied": false,
                    "dry_run": options.dry_run,
                })),
                None,
            );
            return Ok(RecheckOutcome {
                applied: false,
                reason: format!("parse_error: {error}"),
                per_predicate,
            });
        }
    };

    let evaluated: Vec<JsonObject> = parsed
        .iter()
        .map(|req| evaluate_blocked_predicate(req, task, options.cwd.as_deref()))
        .collect();
    let all_satisfied = evaluated
        .iter()
        .all(|p| p.get("satisfied").and_then(Value::as_bool).unwrap_or(false));
    run.emit(
        "harness.recheck_attempted",
        object(json!({
            "per_predicate": evaluated,
            "all_satisfied": all_satisfied,
            "dry_run": options.dry_run,
        })),
        None,
    );
    if options.dry_run {
        return Ok(RecheckOutcome {
            applied: false,
            reason: "dry_run".to_owned(),
            per_predicate: evaluated,
        });
    }
    if !all_satisfied {
        return Ok(RecheckOutcome {
            applied: false,
            reason: "unsatisfied".to_owned(),
            per_predicate: evaluated,
        });
    }
    run.transition(Status::Ready, "")?;
    run.emit(
        "harness.unblocked",
        object(json!({
            "from_status": Status::Interrupted.as_str(),
            "to_status": Status::Ready.as_str(),
        })),
        None,
    );
    Ok(RecheckOutcome {
        applied: true,
        reason: "unblocked".to_owned(),
        per_predicate: evaluated,
    })
}

/// Return value of [`resolve_manual_approval`].
///
/// `applied` is `true` only when an `approve` / `reject` command was
/// claimed for the parked gate and an effect landed. `reason` is one of
/// `not_awaiting`, `missing_gate`, `no_attempt`, `no_pending_command`,
/// `approved_done`, `approved_next_gate`, `rejected_retry`,
/// `rejected_failed`. `command_id` is the applied command's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveApprovalOutcome {
    pub applied: bool,
    pub reason: String,
    pub command_id: Option<i64>,
}

impl ResolveApprovalOutcome {
    fn skipped(reason: &str) -> Self {
        Self {
            applied: false,
            reason: reason.to_owned(),
            command_id: None,
        }
    }
}

/// Knobs for [`resolve_manual_approval`].
#[derive(Clone, Default)]
pub struct ResolveApprovalOptions {
    pub max_retries: u32,
    pub sink: Option<SharedSink>,
    pub now: Option<Clock>,
}

impl fmt::Debug for ResolveApprovalOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolveApprovalOptions")
            .field("max_retries", &self.max_retries)
            .field("sink", &self.sink.is_some())
            .finish_non_exhaustive()
    }
}

impl ResolveApprovalOptions {
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_now(mut self, now: Clock) -> Self {
        self.now = Some(now);
        self
    }
}

/// Coerce a `reject` payload's optional `feedback` for the receipt summary.
fn reject_feedback_text(payload: &JsonObject) -> &str {
    match payload.get("feedback").and_then(Value::as_str) {
        Some(feedback) if !feedback.is_empty() => feedback,
        _ => crate::grader_manual::NO_FEEDBACK_PLACEHOLDER,
    }
}

/// Record the resolver's claim via `harness.control_command_applied`.
fn emit_control_applied(run: &Run, command: &ControlCommandRecord, attempt_number: Option<u32>) {
    run.emit(
        crate::invoker_client::EVENT_CONTROL_APPLIED,
        object(json!({
            "command_id": command.id,
            "kind": command.kind,
            "payload": command.payload,
        })),
        attempt_number,
    );
}

/// Apply the oldest pending `approve` / `reject` command to a lifecycle
/// parked at `AWAITING_APPROVAL`.
///
/// * approve: a `passed=true` manual receipt keyed to the finalized
///   `SUCCEEDED` attempt; re-park on the next gate (with a fresh
///   `harness.awaiting_approval`) or transition `-> DONE`. Emits
///   `harness.manual_approved`.
/// * reject: a `passed=false` receipt carrying the operator feedback (or
///   the documented placeholder); `-> FAILED_VALIDATION` with
///   `manual grader '<name>' rejected by operator`, then the same retry arm
///   `run_task` drives (`-> READY` consuming one retry with
///   `harness.retry_scheduled`, or `-> FAILED`). Emits
///   `harness.manual_rejected`.
///
/// The applied command is ledgered as a `CommandApplied` event and its
/// queue row deleted. `lifecycle` is mutated in place so the caller's view
/// stays aligned with the persisted projection.
pub fn resolve_manual_approval(
    lifecycle: &mut Lifecycle,
    store: Arc<dyn HarnessStore>,
    task: &Task,
    options: ResolveApprovalOptions,
) -> Result<ResolveApprovalOutcome, StoreError> {
    if lifecycle.status != Status::AwaitingApproval {
        return Ok(ResolveApprovalOutcome::skipped("not_awaiting"));
    }
    let Some(ordinal) = lifecycle.awaiting_manual_ordinal else {
        return Ok(ResolveApprovalOutcome::skipped("not_awaiting"));
    };
    let Some(Grader::Manual(grader)) = task.graders.get(ordinal as usize) else {
        return Ok(ResolveApprovalOutcome::skipped("missing_gate"));
    };
    let gate = ManualGate {
        ordinal,
        instruction: grader.instruction.clone(),
        grader_name: grader.name.clone(),
    };

    let (store, telemetry, clock) = session(store, &lifecycle.run_id, options.sink, options.now);
    let run = Run::new(
        store.clone(),
        telemetry,
        lifecycle.clone(),
        clock,
        default_monotonic(),
        None,
    );
    let outcome = resolve_on_run(&run, store, task, &gate, options.max_retries);
    *lifecycle = run.snapshot();
    outcome
}

fn resolve_on_run(
    run: &Run,
    store: Arc<MirroringStore>,
    task: &Task,
    gate: &ManualGate,
    max_retries: u32,
) -> Result<ResolveApprovalOutcome, StoreError> {
    let ordinal = gate.ordinal;
    // Claim every pending row; apply the first approve/reject in id order.
    let claimed = store.claim_commands(&run.run_id, run.now())?;
    let Some(target) = claimed
        .into_iter()
        .find(|cmd| cmd.kind == CONTROL_COMMAND_APPROVE || cmd.kind == CONTROL_COMMAND_REJECT)
    else {
        return Ok(ResolveApprovalOutcome::skipped("no_pending_command"));
    };

    // The receipt is keyed to the attempt finalized SUCCEEDED at gate entry.
    let attempts = store.list_attempts(&run.run_id)?;
    let Some(attempt_number) = attempts.last().map(|a| a.number) else {
        return Ok(ResolveApprovalOutcome::skipped("no_attempt"));
    };
    let steering = SteeringRun {
        store: store.clone(),
        lifecycle: run.lifecycle.clone(),
        run_id: run.run_id.clone(),
        clock: run.clock.clone(),
    };
    let artifacts_dir = run.with_lifecycle(|lc| lc.artifacts_dir.clone());

    if target.kind == CONTROL_COMMAND_APPROVE {
        store.append_grader_result(&build_approve_result(
            gate,
            &run.run_id,
            attempt_number,
            "",
            "approved",
            run.now(),
        ))?;
        let reason = match next_pending_manual_gate(task, Some(ordinal)) {
            Some(next_gate) => {
                run.append_payload(
                    run.now(),
                    Some(attempt_number),
                    DomainEventPayload::AwaitingApproval(AwaitingApproval {
                        awaiting_ordinal: next_gate.ordinal,
                    }),
                )?;
                run.emit(
                    "harness.awaiting_approval",
                    awaiting_approval_payload(
                        &next_gate,
                        &run.run_id,
                        attempt_number,
                        &artifacts_dir,
                    ),
                    Some(attempt_number),
                );
                "approved_next_gate"
            }
            None => {
                run.transition(Status::Done, "")?;
                "approved_done"
            }
        };
        run.emit(
            "harness.manual_approved",
            object(json!({"grader_name": gate.grader_name, "awaiting_ordinal": ordinal})),
            Some(attempt_number),
        );
        emit_control_applied(run, &target, Some(attempt_number));
        steering.ledger(
            &target,
            &command_applied_event(&target),
            Some(attempt_number),
        );
        return Ok(ResolveApprovalOutcome {
            applied: true,
            reason: reason.to_owned(),
            command_id: target.id,
        });
    }

    // Reject path.
    let feedback_text = reject_feedback_text(&target.payload).to_owned();
    store.append_grader_result(&build_reject_result(
        gate,
        &run.run_id,
        attempt_number,
        "",
        &feedback_text,
        run.now(),
    ))?;
    let grader_label = gate
        .grader_name
        .clone()
        .unwrap_or_else(|| "manual".to_owned());
    let error = format!("manual grader '{grader_label}' rejected by operator");
    run.transition(Status::FailedValidation, &error)?;
    run.emit(
        "harness.manual_rejected",
        object(json!({
            "grader_name": gate.grader_name,
            "awaiting_ordinal": ordinal,
            "feedback": feedback_text,
        })),
        Some(attempt_number),
    );
    emit_control_applied(run, &target, Some(attempt_number));
    steering.ledger(
        &target,
        &command_applied_event(&target),
        Some(attempt_number),
    );

    // The same retry arm run_task drives.
    let (eligible, retries) =
        run.with_lifecycle(|lc| (lc.is_retry_eligible(max_retries), lc.retries));
    if eligible {
        run.emit(
            "harness.retry_scheduled",
            object(json!({"retries_used": retries, "max_retries": max_retries})),
            Some(attempt_number),
        );
        run.transition(Status::Ready, "")?;
        return Ok(ResolveApprovalOutcome {
            applied: true,
            reason: "rejected_retry".to_owned(),
            command_id: target.id,
        });
    }
    run.transition(Status::Failed, &error)?;
    Ok(ResolveApprovalOutcome {
        applied: true,
        reason: "rejected_failed".to_owned(),
        command_id: target.id,
    })
}
