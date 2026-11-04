//! The hub's autopilot view and actions for one adopted folder
//! (`flywheel_hub::autopilot`), over the code the CLI and console use:
//!
//! * state: the daemon's activity record (`autopilot_activity`), its pause
//!   marker, and -- on this machine -- whether its pid still runs;
//! * what the last cycle decided: the record's last-cycle summary
//!   (emitted tasks, dropped findings with reasons, tier verdicts);
//! * candidates: the task files autopilot emitted (`active|archive/
//!   autopilot*/`), with the score breakdown recorded in their
//!   `autopilot` block and their latest run from the store;
//! * gates: runs parked at `awaiting_approval`, pinned by
//!   [`GateBinding::of_parked`]; a decision goes through
//!   [`enqueue_gate_decision`], exactly as `fw approve` / `fw reject
//!   --gate --attempt` and the console's keypress do;
//! * pause / resume: `autopilot_activity::set_paused`, as `fw autopilot
//!   --pause|--resume`.

use chrono::{DateTime, TimeZone, Utc};
use flywheel_core::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_REJECT, GateBinding, GateDecisionError,
    enqueue_gate_decision,
};
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{HarnessStore, LifecycleFilter};
use flywheel_hub::autopilot::{
    AutopilotView, GateDecided, GateDecision, GateRefused, PendingDecision, PilotCandidate,
    PilotConfig, PilotDecision, PilotDrop, PilotEmit, PilotGate, PilotHold, PilotState, PilotTier,
};
use flywheel_hub::flywheel::{FlywheelPanel, TaskLight};
use flywheel_orchestrator::autopilot::{AUTOPILOT_PHASE, Tier};
use flywheel_orchestrator::autopilot_activity::{
    AutopilotActivity, CycleSummary, DISCOVERY_NOT_CONSULTED, PHASE_IDLE, PHASE_PAUSED,
    PHASE_RUNNING, PHASE_STARTING, PHASE_STOPPED, epoch_now, is_paused, is_record_live,
    read_activity,
};
use flywheel_orchestrator::policy::{
    AutopilotWeights, DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY, DEFAULT_AUTOPILOT_INTERVAL_SECONDS,
    DEFAULT_AUTOPILOT_LANDING, DEFAULT_AUTOPILOT_TARGET_DEPTH, WorkPolicy,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// The most emitted tasks the view lists (newest first).
pub const MAX_CANDIDATES: usize = 40;
/// Task files larger than this are not read for the view.
const MAX_TASK_FILE_BYTES: u64 = 1024 * 1024;

/// Whether `pid` runs on this machine.
fn pid_runs(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks the pid; nothing is sent.
    let answer = unsafe { libc::kill(pid, 0) };
    answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn epoch(seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() {
        return None;
    }
    let millis = (seconds * 1000.0).round() as i64;
    Utc.timestamp_millis_opt(millis).single()
}

/// The daemon's state from its record (`None`: no record).
fn daemon(activity: Option<&AutopilotActivity>, paused: bool) -> (PilotState, bool) {
    let Some(activity) = activity else {
        return (PilotState::Absent, false);
    };
    let live = is_record_live(Some(activity), epoch_now()) && pid_runs(activity.pid);
    if !live || activity.phase == PHASE_STOPPED {
        return (PilotState::Stopped, false);
    }
    let state = match activity.phase.as_str() {
        PHASE_STARTING => PilotState::Starting,
        PHASE_RUNNING => PilotState::Running,
        PHASE_PAUSED => PilotState::Paused,
        PHASE_IDLE if paused => PilotState::Paused,
        _ => PilotState::Idle,
    };
    (state, true)
}

/// The cycle the record's last-cycle summary describes: the record says
/// (`last_cycle`). A record from before that field: its index once that
/// cycle finished, the one before while the next runs.
fn decided_cycle(activity: &AutopilotActivity) -> i64 {
    if activity.last_cycle > 0 {
        return activity.last_cycle;
    }
    if activity.phase == PHASE_RUNNING || activity.phase == PHASE_STARTING {
        activity.cycle_index - 1
    } else {
        activity.cycle_index
    }
}

fn pilot_decision(summary: &CycleSummary, at: Option<DateTime<Utc>>) -> PilotDecision {
    PilotDecision {
        cycle: summary.cycle,
        at,
        emitted: summary.emitted,
        dropped: summary.dropped,
        reason: summary.reason.clone(),
        relevant_tiers: summary.relevant_tiers.clone(),
        prefiltered_tiers: summary.prefiltered_tiers.clone(),
        discovery_cache: summary.discovery_cache.clone(),
        emitted_tasks: summary
            .emitted_tasks
            .iter()
            .map(|t| PilotEmit {
                task_id: t.task_id.clone(),
                tier: t.tier,
            })
            .collect(),
        dropped_findings: summary
            .dropped_findings
            .iter()
            .map(|d| PilotDrop {
                finding_id: d.finding_id.clone(),
                tier: d.tier,
                title: d.title.clone(),
                reason: d.reason.clone(),
            })
            .collect(),
    }
}

/// What the last completed cycle decided.
fn decision(activity: &AutopilotActivity) -> Option<PilotDecision> {
    let cycle = decided_cycle(activity);
    if cycle <= 0 || activity.last_reason.is_empty() {
        return None;
    }
    let summary = CycleSummary {
        cycle,
        at: 0.0,
        emitted: activity.last_emitted,
        dropped: activity.last_dropped,
        reason: activity.last_reason.clone(),
        relevant_tiers: activity.last_relevant_tiers.clone(),
        emitted_tasks: activity.last_emitted_tasks.clone(),
        prefiltered_tiers: activity.last_prefiltered_tiers.clone(),
        discovery_cache: activity.last_discovery_cache.clone(),
        dropped_findings: activity.last_dropped_findings.clone(),
    };
    // The last decision's own time when it is this cycle.
    let at = activity
        .last_decision
        .as_ref()
        .filter(|d| d.cycle == cycle)
        .and_then(|d| epoch(d.at));
    Some(pilot_decision(&summary, at))
}

/// The newest cycle that decided anything (an older record: the last
/// cycle, when it did).
fn last_decided(activity: &AutopilotActivity) -> Option<PilotDecision> {
    match &activity.last_decision {
        Some(summary) => Some(pilot_decision(summary, epoch(summary.at))),
        None if activity.last_cycle == 0 => decision(activity).filter(|d| {
            d.emitted > 0 || d.dropped > 0 || d.discovery_cache != DISCOVERY_NOT_CONSULTED
        }),
        None => None,
    }
}

fn tiers(decision: Option<&PilotDecision>) -> Vec<PilotTier> {
    Tier::ALL
        .iter()
        .map(|tier| {
            let value = tier.value();
            let verdict = match decision {
                None => "unknown",
                // A cycle that never reached discovery (queue at target).
                Some(d) if d.discovery_cache == DISCOVERY_NOT_CONSULTED => "not run",
                Some(d) if d.prefiltered_tiers.contains(&value) => "prefiltered",
                Some(d) if d.relevant_tiers.contains(&value) => "relevant",
                Some(_) => "not relevant",
            };
            PilotTier {
                tier: value,
                name: tier.name().to_owned(),
                preemptive: tier.is_preemptive(),
                weight: tier.weight(),
                verdict: verdict.to_owned(),
                emitted: decision.map_or(0, |d| {
                    d.emitted_tasks.iter().filter(|t| t.tier == value).count()
                }),
                dropped: decision.map_or(0, |d| {
                    d.dropped_findings
                        .iter()
                        .filter(|f| f.tier == value)
                        .count()
                }),
            }
        })
        .collect()
}

/// The first approve / reject queued on `run_id` and bound to `parked`
/// (the one the worker's resolver would apply).
fn pending_decision(
    core: &dyn HarnessStore,
    run_id: &str,
    parked: GateBinding,
) -> Result<Option<PendingDecision>, String> {
    let pending = core
        .pending_commands(run_id)
        .map_err(|e| format!("reading run {run_id}'s queued decisions: {e}"))?;
    Ok(pending
        .into_iter()
        .filter(|c| c.kind == CONTROL_COMMAND_APPROVE || c.kind == CONTROL_COMMAND_REJECT)
        .find(|c| GateBinding::of_command(c) == Some(parked))
        .map(|c| PendingDecision {
            kind: c.kind,
            command_id: c.id.unwrap_or_default(),
            enqueued_at: c.enqueued_at,
        }))
}

fn config(policy: Option<&WorkPolicy>) -> PilotConfig {
    let weights = policy
        .and_then(|p| p.autopilot_weights.clone())
        .unwrap_or_default();
    let AutopilotWeights {
        tier,
        urgency,
        importance,
        unblock,
        effort,
        interrupt_base,
    } = weights;
    let weights: BTreeMap<String, f64> = [
        ("tier", tier),
        ("urgency", urgency),
        ("importance", importance),
        ("unblock", unblock),
        ("effort", effort),
        ("interrupt_base", interrupt_base),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let mut risk_tiers: Vec<u8> = policy
        .map(|p| {
            p.submit_tiers
                .iter()
                .filter_map(|r| u8::try_from(r.tier).ok())
                .collect()
        })
        .unwrap_or_default();
    risk_tiers.sort_unstable();
    risk_tiers.dedup();
    PilotConfig {
        target_depth: policy.map_or(DEFAULT_AUTOPILOT_TARGET_DEPTH, |p| p.autopilot_target_depth),
        landing: policy.map_or_else(
            || DEFAULT_AUTOPILOT_LANDING.to_owned(),
            |p| p.autopilot_landing.clone(),
        ),
        interval_seconds: policy.map_or(DEFAULT_AUTOPILOT_INTERVAL_SECONDS, |p| {
            p.autopilot_interval_seconds
        }),
        authoring_concurrency: policy.map_or(DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY, |p| {
            p.autopilot_authoring_concurrency
        }),
        model: policy.and_then(|p| p.model.clone()),
        weights,
        submit_strategy: policy.map_or_else(|| "merge".to_owned(), |p| p.submit_strategy.clone()),
        risk_tiers,
        team_mode: policy.is_some_and(|p| p.execution_mode == "distributed"),
        held_out: policy.is_some_and(|p| p.held_out_root.is_some()),
        submit_verify: policy.is_some_and(|p| p.submit_verify.is_some()),
    }
}

/// One emitted task file read back.
struct Emitted {
    candidate: PilotCandidate,
    modified: std::time::SystemTime,
}

fn number(block: &Map<String, Value>, key: &str) -> f64 {
    block.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn text(block: &Map<String, Value>, key: &str) -> String {
    block
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn read_emitted(path: &Path, phase: &str, archived: bool) -> Option<Emitted> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_TASK_FILE_BYTES {
        return None;
    }
    let body: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let block = body.get("autopilot")?.as_object()?;
    let task = body.get("task").unwrap_or(&body);
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    let modified = meta.modified().ok()?;
    Some(Emitted {
        candidate: PilotCandidate {
            task_id,
            goal: task
                .get("goal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            phase: phase.to_owned(),
            archived,
            finding_id: text(block, "finding_id"),
            tier: block
                .get("tier")
                .and_then(Value::as_u64)
                .and_then(|t| u8::try_from(t).ok())
                .unwrap_or(0),
            preemptive: block
                .get("preemptive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            score: number(block, "final"),
            tier_weight: number(block, "tier_weight"),
            urgency: number(block, "urgency"),
            importance: number(block, "importance"),
            blocks: number(block, "blocks"),
            effort: number(block, "effort"),
            grader_source: text(block, "grader_source"),
            authoritative_grader: text(block, "authoritative_grader"),
            assumptions: block
                .get("assumptions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            state: None,
            run_id: None,
            run_status: None,
            emitted_at: Some(DateTime::<Utc>::from(modified)),
        },
        modified,
    })
}

/// Every task file under `<tasks_dir>/{active,archive}/autopilot*/`,
/// newest first, bounded.
fn emitted_tasks(tasks_dir: &Path) -> Vec<PilotCandidate> {
    let mut found: Vec<Emitted> = Vec::new();
    for (tree, archived) in [("active", false), ("archive", true)] {
        let Ok(phases) = std::fs::read_dir(tasks_dir.join(tree)) else {
            continue;
        };
        for phase in phases.flatten() {
            let name = phase.file_name().to_string_lossy().into_owned();
            if !name.starts_with(AUTOPILOT_PHASE) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(phase.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_some_and(|e| e == "json")
                    && let Some(emitted) = read_emitted(&path, &name, archived)
                {
                    found.push(emitted);
                }
            }
        }
    }
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.candidate.task_id.cmp(&b.candidate.task_id))
    });
    found
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|e| e.candidate)
        .collect()
}

fn newest(lifecycles: Vec<Lifecycle>) -> HashMap<String, Lifecycle> {
    let started = |l: &Lifecycle| l.timestamps.values().min().copied();
    let mut latest: HashMap<String, Lifecycle> = HashMap::new();
    for lc in lifecycles {
        match latest.get(&lc.task_id) {
            Some(kept) if started(kept) >= started(&lc) => {}
            _ => {
                latest.insert(lc.task_id.clone(), lc);
            }
        }
    }
    latest
}

/// The folder's autopilot view (see the module doc). `panel` is the
/// folder's panel read from the same stores, its tasks already folded.
pub(crate) fn read_view(
    root: &Path,
    policy: Option<&WorkPolicy>,
    tasks_dir: Option<&Path>,
    core: &dyn HarnessStore,
    panel: &FlywheelPanel,
) -> AutopilotView {
    let activity_path = root.join(crate::hub_flywheel::AUTOPILOT_ACTIVITY);
    let activity = read_activity(Some(&activity_path));
    let paused = is_paused(&activity_path);
    let (state, live) = daemon(activity.as_ref(), paused);
    let decision = activity.as_ref().and_then(decision);
    let last_decided = activity.as_ref().and_then(last_decided);
    let mut problems: Vec<String> = Vec::new();

    let mut candidates = tasks_dir.map(emitted_tasks).unwrap_or_default();
    let tasks: HashMap<&str, &flywheel_hub::flywheel::PanelTask> = panel
        .phases
        .iter()
        .flat_map(|p| &p.tasks)
        .map(|t| (t.id.as_str(), t))
        .collect();
    let archived: Vec<String> = candidates
        .iter()
        .filter(|c| !tasks.contains_key(c.task_id.as_str()))
        .map(|c| c.task_id.clone())
        .collect();
    let mut past = HashMap::new();
    if !archived.is_empty() {
        let filter = LifecycleFilter {
            task_ids: Some(archived),
            ..LifecycleFilter::default()
        };
        match core.list_lifecycles(&filter) {
            Ok(lifecycles) => past = newest(lifecycles),
            Err(e) => problems.push(format!("reading autopilot runs: {e}")),
        }
    }
    let mut holds = Vec::new();
    for candidate in &mut candidates {
        if let Some(task) = tasks.get(candidate.task_id.as_str()) {
            candidate.state = Some(task.state);
            candidate.run_id = task.run_id.clone();
            candidate.run_status = task.run_status.clone();
            let mut hold = |kind: &str, detail: String| {
                holds.push(PilotHold {
                    task_id: task.id.clone(),
                    kind: kind.to_owned(),
                    detail,
                    run_id: task.run_id.clone(),
                });
            };
            if let Some(note) = &task.stranded {
                hold("stranded", format!("{} -- {}", note.kind, note.detail));
            }
            // A parked gate's stop note says what the gate row says.
            if let Some(note) = task
                .stopped
                .as_ref()
                .filter(|_| task.state != TaskLight::Awaiting)
            {
                hold("stopped", format!("{} -- {}", note.kind, note.detail));
            }
            match task.state {
                TaskLight::Failed => hold("failed", task.error.clone()),
                TaskLight::Blocked => hold(
                    "blocked",
                    task.blocked_on
                        .clone()
                        .unwrap_or_else(|| task.error.clone()),
                ),
                _ => {}
            }
        } else if let Some(lc) = past.get(&candidate.task_id) {
            candidate.state = Some(match lc.status {
                Status::Done => TaskLight::Verified,
                Status::AwaitingApproval => TaskLight::Awaiting,
                s if s.is_terminal() => TaskLight::Failed,
                _ => TaskLight::Running,
            });
            candidate.run_id = Some(lc.run_id.clone());
            candidate.run_status = Some(lc.status.as_str().to_owned());
        }
    }

    // Every parked gate in the folder; autopilot's own first.
    let ours: HashSet<&str> = candidates.iter().map(|c| c.task_id.as_str()).collect();
    let mut gates: Vec<PilotGate> = Vec::new();
    for phase in &panel.phases {
        for task in &phase.tasks {
            let (TaskLight::Awaiting, Some(run_id)) = (task.state, task.run_id.as_ref()) else {
                continue;
            };
            let lifecycle = match core.load_lifecycle(run_id) {
                Ok(Some(lifecycle)) => lifecycle,
                Ok(None) => continue,
                Err(e) => {
                    problems.push(format!("reading run {run_id}: {e}"));
                    continue;
                }
            };
            let Some(binding) = GateBinding::of_parked(&lifecycle) else {
                continue;
            };
            let pending = match pending_decision(core, run_id, binding) {
                Ok(pending) => pending,
                Err(e) => {
                    problems.push(e);
                    None
                }
            };
            gates.push(PilotGate {
                task_id: task.id.clone(),
                run_id: run_id.clone(),
                instruction: task.awaiting.clone().unwrap_or_default(),
                gate: binding.awaiting_ordinal,
                attempt: binding.attempt_number,
                since: lifecycle.timestamps.get(&Status::AwaitingApproval).copied(),
                autopilot: ours.contains(task.id.as_str())
                    || phase.name.starts_with(AUTOPILOT_PHASE),
                pending,
            });
        }
    }
    gates.sort_by(|a, b| b.autopilot.cmp(&a.autopilot).then(a.since.cmp(&b.since)));

    AutopilotView {
        state,
        live,
        paused,
        pid: activity.as_ref().map(|a| a.pid),
        cycle: activity.as_ref().map_or(0, |a| a.cycle_index),
        updated_at: activity.as_ref().and_then(|a| epoch(a.updated_at)),
        next_cycle_at: activity
            .as_ref()
            .filter(|_| live)
            .and_then(|a| a.next_cycle_at)
            .and_then(epoch),
        config: config(policy),
        tiers: tiers(last_decided.as_ref().or(decision.as_ref())),
        decision,
        last_decided,
        candidates,
        gates,
        holds,
        problem: (!problems.is_empty()).then(|| problems.join("; ")),
    }
}

/// Enqueue `decision` through [`enqueue_gate_decision`] (pinned to the
/// gate and attempt it names).
pub(crate) fn decide(
    core: &dyn HarnessStore,
    decision: &GateDecision,
) -> Result<GateDecided, GateRefused> {
    let kind = if decision.approve {
        CONTROL_COMMAND_APPROVE
    } else {
        CONTROL_COMMAND_REJECT
    };
    let mut payload = Map::new();
    if let Some(feedback) = &decision.feedback {
        payload.insert("feedback".into(), Value::String(feedback.clone()));
    }
    let pin = flywheel_core::invoker_client::GatePin {
        awaiting_ordinal: decision.gate,
        attempt_number: decision.attempt,
    };
    pin.stamp(&mut payload);
    let lifecycle = core
        .load_lifecycle(&decision.run_id)
        .map_err(|e| GateRefused::Store(format!("reading run {}: {e}", decision.run_id)))?;
    let task_id = lifecycle
        .as_ref()
        .map(|l| l.task_id.clone())
        .unwrap_or_default();
    // One decision per gate: a second, queued before a worker applied the
    // first, would only be recorded as superseded -- refuse it here.
    if let Some(parked) = lifecycle.as_ref().and_then(GateBinding::of_parked)
        && let Some(queued) =
            pending_decision(core, &decision.run_id, parked).map_err(GateRefused::Store)?
    {
        return Err(GateRefused::AlreadyDecided(format!(
            "gate {} (attempt {}) of run {} already has a decision queued: {} (command #{}); \
             a worker applies it on its next pass",
            parked.awaiting_ordinal,
            parked.attempt_number,
            decision.run_id,
            queued.kind,
            queued.command_id
        )));
    }
    match enqueue_gate_decision(core, &decision.run_id, kind, payload, Utc::now()) {
        Ok((record, binding)) => Ok(GateDecided {
            run_id: decision.run_id.clone(),
            task_id,
            kind: kind.to_owned(),
            command_id: record.id.unwrap_or_default(),
            gate: binding.awaiting_ordinal,
            attempt: binding.attempt_number,
        }),
        Err(GateDecisionError::UnknownRun(_)) => Err(GateRefused::UnknownRun(format!(
            "no run {} in this repo's store",
            decision.run_id
        ))),
        Err(e @ (GateDecisionError::NotParked { .. } | GateDecisionError::GateMismatch { .. })) => {
            Err(GateRefused::NotThatGate(e.to_string()))
        }
        Err(GateDecisionError::Store(e)) => {
            Err(GateRefused::Store(format!("enqueueing the decision: {e}")))
        }
    }
}
