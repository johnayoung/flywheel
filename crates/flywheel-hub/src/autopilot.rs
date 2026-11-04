//! Autopilot from the hub: the intake daemon of a flywheel-adopted folder
//! (`fw autopilot`, `docs/autopilot.md`) shown and driven beside its
//! Flywheel panel.
//!
//! What the hub shows ([`AutopilotView`], part of the folder's
//! [`FlywheelPanel`]): the daemon's state from its activity record (pid,
//! cycle, paused), its configuration and the safety gates its work lands
//! through, the tier model with the last cycle's verdicts, what that cycle
//! decided and why (emitted tasks, dropped findings with their reasons),
//! the tasks it emitted with their recorded score breakdown and latest
//! run, and the manual gates its work waits on.
//!
//! What the hub does, each through the code the CLI and console use:
//! start runs the real `fw autopilot` daemon in a visible hub shell
//! session ([`autopilot_session_command`]; never auto-started, the start
//! is confirmed: autopilot writes the base branch unattended); stop sends
//! that session's daemon Ctrl-C (SIGINT, the daemon's graceful stop, as a
//! person at the terminal would); pause / resume write / remove the
//! daemon's pause marker as `fw autopilot --pause|--resume` do; approve /
//! deny enqueue a gate decision bound to the gate shown, as `fw approve` /
//! `fw reject --gate --attempt` do. The hub decides nothing itself: the
//! held-out gate, risk tiers, and team-mode reviews stay where they are
//! enforced (worker, submit strategy, GitHub) and are only shown.
//!
//! Events ([`events_between`]): `autopilot.started`, `autopilot.stopped`,
//! `autopilot.decision`, `autopilot.approval_needed` for plugins and the
//! activity timeline, derived from successive panel reads.

use crate::flywheel::{AUTOPILOT_MARKER, FlywheelPanel, TaskLight, daemon_session_command};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// The name an autopilot session gets (numbered when taken).
pub const AUTOPILOT_SESSION_NAME: &str = "fw autopilot";

/// The daemon's state, as the record and its pause marker say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PilotState {
    /// No activity record: never ran here (with this record path).
    Absent,
    Starting,
    /// Inside a refill pass.
    Running,
    /// Between cycles.
    Idle,
    /// Held between cycles by the pause marker.
    Paused,
    /// Stopped, or its record lapsed (it died).
    Stopped,
}

impl PilotState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }
}

/// `[autopilot]` and the landing gates its emitted work goes through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PilotConfig {
    pub target_depth: i64,
    /// `merge` or `pr`.
    pub landing: String,
    pub interval_seconds: f64,
    pub authoring_concurrency: i64,
    pub model: Option<String>,
    /// Every score weight in effect (`tier`, `urgency`, ...).
    pub weights: BTreeMap<String, f64>,
    /// `[submit] strategy`.
    pub submit_strategy: String,
    /// The `[[submit.tiers]]` risk tiers configured (empty: none).
    pub risk_tiers: Vec<u8>,
    /// `[execution] mode = "distributed"` (team mode: GitHub reviews).
    pub team_mode: bool,
    /// `[held_out] root` is set: the execute-time held-out gate applies.
    pub held_out: bool,
    /// `[submit] verify` is set.
    pub submit_verify: bool,
}

/// One finding the last cycle dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotDrop {
    pub finding_id: String,
    pub tier: u8,
    pub title: String,
    pub reason: String,
}

/// One task the last cycle emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotEmit {
    pub task_id: String,
    pub tier: u8,
}

/// What one completed cycle decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotDecision {
    pub cycle: i64,
    /// When the cycle finished (unknown for an older record's last cycle).
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    pub emitted: i64,
    pub dropped: i64,
    pub reason: String,
    pub relevant_tiers: Vec<u8>,
    pub prefiltered_tiers: Vec<u8>,
    pub discovery_cache: String,
    pub emitted_tasks: Vec<PilotEmit>,
    pub dropped_findings: Vec<PilotDrop>,
}

/// One tier of the model, with the verdict of the newest cycle that ran
/// discovery on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotTier {
    pub tier: u8,
    pub name: String,
    pub preemptive: bool,
    pub weight: u32,
    /// `relevant`, `prefiltered` (skipped before discovery), `not
    /// relevant`, `not run` (the cycle ran no discovery), or `unknown` (no
    /// cycle recorded).
    pub verdict: String,
    /// Tasks that cycle emitted at this tier.
    pub emitted: usize,
    /// Findings that cycle dropped at this tier.
    #[serde(default)]
    pub dropped: usize,
}

/// A task autopilot emitted (its task file's provenance block) and how
/// its work stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotCandidate {
    pub task_id: String,
    pub goal: String,
    /// The phase dir it was emitted to (`autopilot`, `autopilot-2`).
    pub phase: String,
    /// Its phase was archived.
    pub archived: bool,
    pub finding_id: String,
    pub tier: u8,
    pub preemptive: bool,
    /// The recorded final score and its inputs.
    pub score: f64,
    pub tier_weight: f64,
    pub urgency: f64,
    pub importance: f64,
    pub blocks: f64,
    pub effort: f64,
    pub grader_source: String,
    pub authoritative_grader: String,
    pub assumptions: Vec<String>,
    /// The panel's light for it (`None`: archived and never run here).
    pub state: Option<TaskLight>,
    pub run_id: Option<String>,
    pub run_status: Option<String>,
    pub emitted_at: Option<DateTime<Utc>>,
}

/// A manual gate a run is parked on: what `fw approve` / `fw reject`
/// decide, pinned to this gate and attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotGate {
    pub task_id: String,
    pub run_id: String,
    /// The manual grader's instruction.
    pub instruction: String,
    /// `awaiting_ordinal` (`fw approve --gate`).
    pub gate: u32,
    /// The attempt parked (`fw approve --attempt`).
    pub attempt: u32,
    pub since: Option<DateTime<Utc>>,
    /// The task was emitted by autopilot.
    pub autopilot: bool,
    /// A decision already queued on this gate (not yet applied by a
    /// worker): another one is refused.
    #[serde(default)]
    pub pending: Option<PendingDecision>,
}

/// An approve / reject enqueued on a gate and not yet claimed by the
/// worker's resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDecision {
    /// `approve` or `reject`.
    pub kind: String,
    pub command_id: i64,
    pub enqueued_at: DateTime<Utc>,
}

/// Autopilot work held by a gate the hub only shows (a strand, a stop, a
/// block, a failure): `kind` and the loop's own words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotHold {
    pub task_id: String,
    pub kind: String,
    pub detail: String,
    pub run_id: Option<String>,
}

/// Everything the hub shows about a folder's autopilot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutopilotView {
    pub state: PilotState,
    /// The record reads live (and, on this machine, its pid runs).
    pub live: bool,
    /// The pause marker exists (a daemon started now starts held).
    pub paused: bool,
    pub pid: Option<i64>,
    pub cycle: i64,
    pub updated_at: Option<DateTime<Utc>>,
    pub next_cycle_at: Option<DateTime<Utc>>,
    pub config: PilotConfig,
    /// What the last completed cycle decided (often "queue at target").
    pub decision: Option<PilotDecision>,
    /// The newest cycle that decided anything (ran discovery, emitted or
    /// dropped work): kept when later cycles only find the queue full.
    #[serde(default)]
    pub last_decided: Option<PilotDecision>,
    /// The tier model with the verdicts of `last_decided` (else of
    /// `decision`).
    pub tiers: Vec<PilotTier>,
    /// Newest first, bounded.
    pub candidates: Vec<PilotCandidate>,
    /// Every manual gate parked in the folder, autopilot's first.
    pub gates: Vec<PilotGate>,
    pub holds: Vec<PilotHold>,
    /// What could not be read (a malformed task file, a store error).
    pub problem: Option<String>,
    /// Its newest [`HISTORY_ROWS`] decisions as this hub's activity journal
    /// has them (cycle decisions, gate approvals and denials), newest
    /// first: filled in by the hub's API ([`with_history`]), never by a
    /// provider.
    #[serde(default)]
    pub history: Vec<crate::activity::Row>,
}

/// The autopilot decisions a folder's view keeps.
pub const HISTORY_ROWS: usize = 20;

/// The activity row kinds that are autopilot decisions.
pub const HISTORY_KINDS: [&str; 3] = [
    "autopilot.decision",
    "autopilot.approved",
    "autopilot.denied",
];

/// `view` with `folder`'s newest decisions from `activity` (rows name a
/// folder by [`folder_name`]: the journal never holds paths).
pub fn with_history(
    mut view: AutopilotView,
    activity: &crate::activity::Activity,
    folder: &Path,
) -> AutopilotView {
    let name = folder_name(folder);
    let filter = crate::activity::Filter {
        kinds: HISTORY_KINDS.iter().map(|k| (*k).to_owned()).collect(),
        ..crate::activity::Filter::default()
    };
    let mut out = Vec::new();
    let mut before = None;
    loop {
        let (rows, more) = activity.page(&filter, before, crate::activity::MAX_LIMIT);
        before = rows.last().map(|r| r.id);
        for row in rows {
            if row.data.get("folder").and_then(Value::as_str) == Some(name.as_str()) {
                out.push(row);
                if out.len() == HISTORY_ROWS {
                    view.history = out;
                    return view;
                }
            }
        }
        if !more || before.is_none() {
            view.history = out;
            return view;
        }
    }
}

impl AutopilotView {
    /// A view with nothing read.
    pub fn absent() -> Self {
        Self {
            state: PilotState::Absent,
            live: false,
            paused: false,
            pid: None,
            cycle: 0,
            updated_at: None,
            next_cycle_at: None,
            config: PilotConfig::default(),
            decision: None,
            last_decided: None,
            tiers: Vec::new(),
            candidates: Vec::new(),
            gates: Vec::new(),
            holds: Vec::new(),
            problem: None,
            history: Vec::new(),
        }
    }

    /// The gates on autopilot's own work.
    pub fn autopilot_gates(&self) -> impl Iterator<Item = &PilotGate> {
        self.gates.iter().filter(|g| g.autopilot)
    }
}

/// An approve or a deny of a parked gate, pinned to the gate shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateDecision {
    pub run_id: String,
    pub approve: bool,
    /// A deny's reviewer feedback (flows into the next attempt's prompt).
    pub feedback: Option<String>,
    /// `fw approve --gate`: refused unless this gate is the one parked.
    pub gate: Option<u32>,
    /// `fw approve --attempt`: refused unless this attempt is parked.
    pub attempt: Option<u32>,
}

/// A decision enqueued for the worker's resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateDecided {
    pub run_id: String,
    pub task_id: String,
    /// `approve` or `reject`.
    pub kind: String,
    /// The control command's id in the store.
    pub command_id: i64,
    pub gate: u32,
    pub attempt: u32,
}

/// Why a decision was not enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefused {
    /// No such run in this folder's store.
    UnknownRun(String),
    /// Not parked on a gate, or parked on another gate / attempt than the
    /// one named: nothing enqueued.
    NotThatGate(String),
    /// A decision is already queued on this gate (a worker has not applied
    /// it yet): a second one would conflict.
    AlreadyDecided(String),
    /// The store could not be read or written.
    Store(String),
}

impl std::fmt::Display for GateRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun(m)
            | Self::NotThatGate(m)
            | Self::AlreadyDecided(m)
            | Self::Store(m) => f.write_str(m),
        }
    }
}

/// The longest feedback a deny carries (characters).
pub const MAX_FEEDBACK_CHARS: usize = 4000;

/// The argv an autopilot session runs: `autopilot` (the provider's argv)
/// under the same wrapper as a worker session (Ctrl-C stops the daemon,
/// the shell stays).
pub fn autopilot_session_command(autopilot: &[String]) -> Vec<String> {
    daemon_session_command(AUTOPILOT_MARKER, "fw autopilot", autopilot)
}

/// An autopilot session name no session has.
pub fn autopilot_session_name<'a>(taken: impl IntoIterator<Item = &'a str>) -> String {
    let taken: HashSet<&str> = taken.into_iter().map(str::trim).collect();
    if !taken.contains(AUTOPILOT_SESSION_NAME) {
        return AUTOPILOT_SESSION_NAME.to_owned();
    }
    (2..)
        .map(|n| format!("{AUTOPILOT_SESSION_NAME} {n}"))
        .find(|name| !taken.contains(name.as_str()))
        .expect("an unbounded range has a free name")
}

/// The folder as events name it: its parent's name and its own
/// (`code/repo`), enough to tell two `repo` folders apart without putting
/// the path in activity rows (plugins get the path too).
pub fn folder_name(folder: &Path) -> String {
    let own = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.display().to_string());
    match folder
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
    {
        Some(parent) if !parent.is_empty() => format!("{parent}/{own}"),
        _ => own,
    }
}

/// An event's payload about `folder` with `extra` merged in.
pub fn folder_payload(folder: &Path, extra: Value) -> Value {
    let mut payload = json!({ "folder": folder, "name": folder_name(folder) });
    if let (Value::Object(map), Value::Object(more)) = (&mut payload, extra) {
        map.extend(more);
    }
    payload
}

/// The event a decision the hub enqueued raises (`autopilot.approved` /
/// `autopilot.denied`) and its payload: who decided, which gate and
/// attempt, whether feedback went with it (never the feedback itself).
pub fn gate_decided_event(
    folder: &Path,
    decided: &GateDecided,
    feedback: bool,
) -> (&'static str, Value) {
    let event = if decided.kind == "approve" {
        "autopilot.approved"
    } else {
        "autopilot.denied"
    };
    (
        event,
        folder_payload(
            folder,
            json!({
                "who": "hub",
                "task_id": decided.task_id,
                "run_id": decided.run_id,
                "gate": decided.gate,
                "attempt": decided.attempt,
                "command_id": decided.command_id,
                "feedback": feedback,
            }),
        ),
    )
}

fn approval_needed(folder: &Path, gate: &PilotGate) -> (&'static str, Value) {
    (
        "autopilot.approval_needed",
        folder_payload(
            folder,
            json!({
                "task_id": gate.task_id,
                "run_id": gate.run_id,
                "instruction": gate.instruction,
                "gate": gate.gate,
                "attempt": gate.attempt,
            }),
        ),
    )
}

/// The plugin events between two reads of `folder`'s panel. The first
/// read (`old` = `None`: a folder just added, or the hub restarted) is a
/// baseline: with `announced` (whether a gate's `approval_needed` was
/// raised before, e.g. by an earlier hub run, as its activity journal
/// says) it raises one for every gate on autopilot work parked now and not
/// announced yet -- once per gate and attempt, across restarts; without
/// it, nothing.
pub fn events_between(
    old: Option<&FlywheelPanel>,
    new: &FlywheelPanel,
    announced: Option<&dyn Fn(&PilotGate) -> bool>,
) -> Vec<(&'static str, Value)> {
    let Some(now) = new.intake.as_ref() else {
        return Vec::new();
    };
    let folder = &new.folder;
    let Some(old) = old.and_then(|p| p.intake.as_ref()) else {
        let Some(announced) = announced else {
            return Vec::new();
        };
        return now
            .autopilot_gates()
            .filter(|g| !announced(g))
            .map(|g| approval_needed(folder, g))
            .collect();
    };
    let mut events = Vec::new();
    let started = now.live && (!old.live || old.pid != now.pid);
    if old.live && (!now.live || old.pid != now.pid) {
        events.push((
            "autopilot.stopped",
            folder_payload(
                folder,
                json!({ "pid": old.pid, "state": now.state.as_str() }),
            ),
        ));
    }
    if started {
        events.push((
            "autopilot.started",
            folder_payload(folder, json!({ "pid": now.pid })),
        ));
    }
    if let Some(decision) = &now.last_decided {
        // A cycle that decided something, announced once: the record names
        // the cycle it came from, so a failed or no-op cycle after it never
        // makes the same decision news again.
        let fresh = match &old.last_decided {
            None => true,
            Some(d) => d.cycle != decision.cycle || old.pid != now.pid,
        };
        if fresh {
            events.push((
                "autopilot.decision",
                folder_payload(
                    folder,
                    json!({
                        "pid": now.pid,
                        "cycle": decision.cycle,
                        "emitted": decision.emitted,
                        "dropped": decision.dropped,
                        "reason": decision.reason,
                        "tasks": decision.emitted_tasks.iter().map(|t| &t.task_id).collect::<Vec<_>>(),
                        "dropped_findings": decision.dropped_findings,
                        "relevant_tiers": decision.relevant_tiers,
                        "prefiltered_tiers": decision.prefiltered_tiers,
                        "discovery_cache": decision.discovery_cache,
                    }),
                ),
            ));
        }
    }
    let seen: HashSet<(&str, u32, u32)> = old
        .autopilot_gates()
        .map(|g| (g.run_id.as_str(), g.gate, g.attempt))
        .collect();
    for gate in now.autopilot_gates() {
        if !seen.contains(&(gate.run_id.as_str(), gate.gate, gate.attempt)) {
            events.push(approval_needed(folder, gate));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(view: Option<AutopilotView>) -> FlywheelPanel {
        let mut panel = FlywheelPanel::empty("/code/repo".into(), Utc::now());
        panel.intake = view;
        panel
    }

    fn live(pid: i64, cycle: i64) -> AutopilotView {
        AutopilotView {
            state: PilotState::Idle,
            live: true,
            pid: Some(pid),
            cycle,
            ..AutopilotView::absent()
        }
    }

    fn decision(cycle: i64) -> PilotDecision {
        PilotDecision {
            cycle,
            at: None,
            emitted: 1,
            dropped: 1,
            reason: "emitted 1".into(),
            relevant_tiers: vec![6],
            prefiltered_tiers: vec![],
            discovery_cache: "miss".into(),
            emitted_tasks: vec![PilotEmit {
                task_id: "add-tests".into(),
                tier: 6,
            }],
            dropped_findings: vec![PilotDrop {
                finding_id: "t10-readme".into(),
                tier: 10,
                title: "Document it".into(),
                reason: "no committed check can grade a README".into(),
            }],
        }
    }

    fn decided(mut view: AutopilotView, cycle: i64) -> AutopilotView {
        view.decision = Some(decision(cycle));
        view.last_decided = Some(decision(cycle));
        view
    }

    fn gate(run: &str, ordinal: u32, autopilot: bool) -> PilotGate {
        PilotGate {
            task_id: "add-tests".into(),
            run_id: run.into(),
            instruction: "Confirm".into(),
            gate: ordinal,
            attempt: 1,
            since: None,
            autopilot,
            pending: None,
        }
    }

    fn names(events: &[(&'static str, Value)]) -> Vec<&'static str> {
        events.iter().map(|e| e.0).collect()
    }

    #[test]
    fn the_first_read_announces_each_parked_gate_once() {
        let none = |_: &PilotGate| false;
        assert!(events_between(None, &panel(Some(live(1, 1))), Some(&none)).is_empty());
        assert!(
            events_between(Some(&panel(None)), &panel(Some(live(1, 1))), Some(&none)).is_empty()
        );
        let mut parked = live(1, 1);
        let mut retried = gate("run-old", 1, true);
        retried.attempt = 2;
        parked.gates = vec![
            gate("run-new", 1, true),
            gate("run-old", 1, true),
            retried,
            gate("run-theirs", 1, false),
        ];
        // An earlier hub announced run-old's first attempt: its retry, and
        // a gate parked before this hub ever ran, are still news.
        let journal = |g: &PilotGate| g.run_id == "run-old" && g.attempt == 1;
        let events = events_between(None, &panel(Some(parked.clone())), Some(&journal));
        assert_eq!(
            names(&events),
            ["autopilot.approval_needed", "autopilot.approval_needed"]
        );
        assert_eq!(events[0].1["run_id"], "run-new");
        assert_eq!(
            (
                events[1].1["run_id"].clone(),
                events[1].1["attempt"].clone()
            ),
            (json!("run-old"), json!(2))
        );
        // Without the journal the first read stays silent.
        assert!(events_between(None, &panel(Some(parked)), None).is_empty());
    }

    #[test]
    fn a_decision_names_who_and_which_gate_never_the_feedback() {
        let decided = GateDecided {
            run_id: "run-1".into(),
            task_id: "add-tests".into(),
            kind: "reject".into(),
            command_id: 4,
            gate: 2,
            attempt: 1,
        };
        let (event, payload) = gate_decided_event(Path::new("/code/repo"), &decided, true);
        assert_eq!(event, "autopilot.denied");
        assert_eq!(payload["who"], "hub");
        assert_eq!(
            (payload["gate"].clone(), payload["attempt"].clone()),
            (json!(2), json!(1))
        );
        assert_eq!(payload["feedback"], true, "whether, not what");
        let approved = GateDecided {
            kind: "approve".into(),
            ..decided
        };
        assert_eq!(
            gate_decided_event(Path::new("/code/repo"), &approved, false).0,
            "autopilot.approved"
        );
    }

    #[test]
    fn start_decision_gate_and_stop_are_each_announced_once() {
        let absent = panel(Some(AutopilotView::absent()));
        let up = panel(Some(live(7, 1)));
        let events = events_between(Some(&absent), &up, None);
        assert_eq!(names(&events), ["autopilot.started"]);
        assert_eq!(events[0].1["name"], "code/repo");
        assert_eq!(events[0].1["pid"], 7);

        let first = panel(Some(decided(live(7, 1), 1)));
        let events = events_between(Some(&up), &first, None);
        assert_eq!(names(&events), ["autopilot.decision"]);
        assert_eq!(events[0].1["tasks"], json!(["add-tests"]));
        assert_eq!(
            events[0].1["dropped_findings"][0]["reason"],
            "no committed check can grade a README"
        );
        assert_eq!(events[0].1["relevant_tiers"], json!([6]));
        // The same decision read again: nothing new.
        assert!(events_between(Some(&first), &first, None).is_empty());

        let mut parked = decided(live(7, 1), 1);
        parked.gates = vec![gate("run-1", 0, true), gate("run-x", 0, false)];
        let parked = panel(Some(parked));
        let events = events_between(Some(&first), &parked, None);
        assert_eq!(
            names(&events),
            ["autopilot.approval_needed"],
            "only autopilot's own work"
        );
        assert_eq!(events[0].1["run_id"], "run-1");
        // The next gate of the same run is a new one.
        let mut next = decided(live(7, 1), 1);
        next.gates = vec![gate("run-1", 1, true)];
        let next = panel(Some(next));
        assert_eq!(
            names(&events_between(Some(&parked), &next, None)),
            ["autopilot.approval_needed"]
        );

        let mut down = decided(live(7, 1), 1);
        down.live = false;
        down.state = PilotState::Stopped;
        let events = events_between(Some(&next), &panel(Some(down)), None);
        assert_eq!(names(&events), ["autopilot.stopped"]);
        assert_eq!(events[0].1["state"], "stopped");
    }

    #[test]
    fn later_cycles_that_fail_or_find_the_queue_full_do_not_repeat_a_decision() {
        let first = decided(live(7, 1), 1);
        // Cycle 2 failed, cycle 3 runs: the record still names cycle 1.
        let mut later = first.clone();
        later.cycle = 3;
        later.state = PilotState::Running;
        assert!(
            events_between(
                Some(&panel(Some(first.clone()))),
                &panel(Some(later.clone())),
                None
            )
            .is_empty()
        );
        // A no-op cycle 4: the last cycle changes, the last decision not.
        let mut noop = later.clone();
        noop.cycle = 4;
        noop.decision = Some(PilotDecision {
            cycle: 4,
            emitted: 0,
            dropped: 0,
            reason: "queue at target".into(),
            discovery_cache: "not consulted".into(),
            emitted_tasks: vec![],
            dropped_findings: vec![],
            ..decision(4)
        });
        assert!(
            events_between(Some(&panel(Some(later))), &panel(Some(noop.clone())), None).is_empty()
        );
        // The daemon stops: no decision rides along.
        let mut down = noop.clone();
        down.live = false;
        down.state = PilotState::Stopped;
        assert_eq!(
            names(&events_between(
                Some(&panel(Some(noop))),
                &panel(Some(down)),
                None
            )),
            ["autopilot.stopped"]
        );
        // A new decision (cycle 5) is news.
        let mut next = decided(live(7, 5), 5);
        next.last_decided.as_mut().unwrap().reason = "emitted 2".into();
        assert_eq!(
            names(&events_between(
                Some(&panel(Some(first))),
                &panel(Some(next)),
                None
            )),
            ["autopilot.decision"]
        );
    }

    #[test]
    fn a_restarted_daemon_stops_starts_and_decides_anew() {
        let old = panel(Some(decided(live(7, 4), 4)));
        let new = panel(Some(decided(live(9, 1), 1)));
        assert_eq!(
            names(&events_between(Some(&old), &new, None)),
            [
                "autopilot.stopped",
                "autopilot.started",
                "autopilot.decision"
            ]
        );
    }

    #[test]
    fn the_history_keeps_a_folders_newest_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let activity = crate::activity::Activity::open(dir.path());
        let repo = Path::new("/code/repo");
        let record = |folder: &Path, event: &str, extra: Value| {
            let payload = folder_payload(folder, extra);
            let draft = crate::activity::draft_for(event, &payload).unwrap();
            activity.record(draft).unwrap();
        };
        for cycle in 1..=25 {
            record(
                repo,
                "autopilot.decision",
                json!({ "cycle": cycle, "emitted": 1 }),
            );
            record(
                Path::new("/code/other"),
                "autopilot.decision",
                json!({ "cycle": cycle }),
            );
        }
        record(repo, "autopilot.started", json!({ "pid": 3 }));
        let decided = GateDecided {
            run_id: "run-1".into(),
            task_id: "t".into(),
            kind: "approve".into(),
            command_id: 1,
            gate: 1,
            attempt: 1,
        };
        let (event, payload) = gate_decided_event(repo, &decided, false);
        activity
            .record(crate::activity::draft_for(event, &payload).unwrap())
            .unwrap();
        let view = with_history(AutopilotView::absent(), &activity, repo);
        assert_eq!(view.history.len(), HISTORY_ROWS);
        assert_eq!(view.history[0].kind, "autopilot.approved");
        assert_eq!(view.history[1].data["cycle"], 25);
        assert_eq!(view.history[HISTORY_ROWS - 1].data["cycle"], 7);
        assert!(view.history.iter().all(|r| r.data["folder"] == "code/repo"));
    }

    #[test]
    fn folders_are_named_with_their_parent() {
        assert_eq!(folder_name(Path::new("/home/me/code/repo")), "code/repo");
        assert_eq!(folder_name(Path::new("/repo")), "repo");
    }

    #[test]
    fn session_names_are_numbered_when_taken() {
        assert_eq!(autopilot_session_name(["x"]), "fw autopilot");
        assert_eq!(
            autopilot_session_name(["fw autopilot", "fw autopilot 2"]),
            "fw autopilot 3"
        );
        let argv = autopilot_session_command(&["/opt/fw".into(), "autopilot".into()]);
        assert_eq!(argv[3..], ["fw-autopilot", "/opt/fw", "autopilot"]);
        assert!(argv[2].contains("[fw autopilot exited with status %s]"));
    }
}
