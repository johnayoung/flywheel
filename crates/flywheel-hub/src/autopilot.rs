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

/// What the last completed cycle decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotDecision {
    pub cycle: i64,
    pub emitted: i64,
    pub dropped: i64,
    pub reason: String,
    pub relevant_tiers: Vec<u8>,
    pub prefiltered_tiers: Vec<u8>,
    pub discovery_cache: String,
    pub emitted_tasks: Vec<PilotEmit>,
    pub dropped_findings: Vec<PilotDrop>,
}

/// One tier of the model, with the last cycle's verdict on it.
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
    /// Tasks the last cycle emitted at this tier.
    pub emitted: usize,
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
    pub decision: Option<PilotDecision>,
    pub tiers: Vec<PilotTier>,
    /// Newest first, bounded.
    pub candidates: Vec<PilotCandidate>,
    /// Every manual gate parked in the folder, autopilot's first.
    pub gates: Vec<PilotGate>,
    pub holds: Vec<PilotHold>,
    /// What could not be read (a malformed task file, a store error).
    pub problem: Option<String>,
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
            tiers: Vec::new(),
            candidates: Vec::new(),
            gates: Vec::new(),
            holds: Vec::new(),
            problem: None,
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
    /// The store could not be read or written.
    Store(String),
}

impl std::fmt::Display for GateRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun(m) | Self::NotThatGate(m) | Self::Store(m) => f.write_str(m),
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

/// The folder as events name it: its last component (never the path in
/// activity rows; plugins get the path too).
fn folder_name(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.display().to_string())
}

/// The plugin events between two reads of `folder`'s panel. The first
/// read (`old` = `None`, e.g. after a hub restart) announces nothing: it is
/// a baseline, not a change.
pub fn events_between(
    old: Option<&FlywheelPanel>,
    new: &FlywheelPanel,
) -> Vec<(&'static str, Value)> {
    let (Some(old), Some(now)) = (old.and_then(|p| p.intake.as_ref()), new.intake.as_ref()) else {
        return Vec::new();
    };
    let folder = &new.folder;
    let base = |extra: Value| -> Value {
        let mut payload = json!({ "folder": folder, "name": folder_name(folder) });
        if let (Value::Object(map), Value::Object(more)) = (&mut payload, extra) {
            map.extend(more);
        }
        payload
    };
    let mut events = Vec::new();
    let started = now.live && (!old.live || old.pid != now.pid);
    if old.live && (!now.live || old.pid != now.pid) {
        events.push((
            "autopilot.stopped",
            base(json!({ "pid": old.pid, "state": now.state.as_str() })),
        ));
    }
    if started {
        events.push(("autopilot.started", base(json!({ "pid": now.pid }))));
    }
    if let Some(decision) = &now.decision {
        // A new cycle is news when it did something, or decided otherwise
        // than the last one: a queue held at its target every interval is
        // not.
        let fresh = match &old.decision {
            None => true,
            Some(d) if d.cycle == decision.cycle && old.pid == now.pid => false,
            Some(d) => decision.emitted > 0 || decision.dropped > 0 || d.reason != decision.reason,
        };
        if fresh {
            events.push((
                "autopilot.decision",
                base(json!({
                    "pid": now.pid,
                    "cycle": decision.cycle,
                    "emitted": decision.emitted,
                    "dropped": decision.dropped,
                    "reason": decision.reason,
                    "tasks": decision.emitted_tasks.iter().map(|t| &t.task_id).collect::<Vec<_>>(),
                })),
            ));
        }
    }
    let seen: HashSet<(&str, u32, u32)> = old
        .autopilot_gates()
        .map(|g| (g.run_id.as_str(), g.gate, g.attempt))
        .collect();
    for gate in now.autopilot_gates() {
        if seen.contains(&(gate.run_id.as_str(), gate.gate, gate.attempt)) {
            continue;
        }
        events.push((
            "autopilot.approval_needed",
            base(json!({
                "task_id": gate.task_id,
                "run_id": gate.run_id,
                "instruction": gate.instruction,
                "gate": gate.gate,
                "attempt": gate.attempt,
            })),
        ));
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

    fn decided(mut view: AutopilotView, cycle: i64) -> AutopilotView {
        view.decision = Some(PilotDecision {
            cycle,
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
            dropped_findings: vec![],
        });
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
        }
    }

    fn names(events: &[(&'static str, Value)]) -> Vec<&'static str> {
        events.iter().map(|e| e.0).collect()
    }

    #[test]
    fn the_first_read_is_a_baseline() {
        assert!(events_between(None, &panel(Some(live(1, 1)))).is_empty());
        assert!(events_between(Some(&panel(None)), &panel(Some(live(1, 1)))).is_empty());
    }

    #[test]
    fn start_decision_gate_and_stop_are_each_announced_once() {
        let absent = panel(Some(AutopilotView::absent()));
        let up = panel(Some(live(7, 1)));
        let events = events_between(Some(&absent), &up);
        assert_eq!(names(&events), ["autopilot.started"]);
        assert_eq!(events[0].1["name"], "repo");
        assert_eq!(events[0].1["pid"], 7);

        let first = panel(Some(decided(live(7, 1), 1)));
        let events = events_between(Some(&up), &first);
        assert_eq!(names(&events), ["autopilot.decision"]);
        assert_eq!(events[0].1["tasks"], json!(["add-tests"]));
        // The same decision read again: nothing new.
        assert!(events_between(Some(&first), &first).is_empty());

        let mut parked = decided(live(7, 1), 1);
        parked.gates = vec![gate("run-1", 0, true), gate("run-x", 0, false)];
        let parked = panel(Some(parked));
        let events = events_between(Some(&first), &parked);
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
            names(&events_between(Some(&parked), &next)),
            ["autopilot.approval_needed"]
        );

        let mut down = decided(live(7, 1), 1);
        down.live = false;
        down.state = PilotState::Stopped;
        let events = events_between(Some(&next), &panel(Some(down)));
        assert_eq!(names(&events), ["autopilot.stopped"]);
        assert_eq!(events[0].1["state"], "stopped");
    }

    #[test]
    fn a_cycle_that_decides_the_same_nothing_is_not_news() {
        let mut idle = live(7, 2);
        idle.decision = Some(PilotDecision {
            cycle: 2,
            emitted: 0,
            dropped: 0,
            reason: "queue at target".into(),
            relevant_tiers: vec![],
            prefiltered_tiers: vec![],
            discovery_cache: String::new(),
            emitted_tasks: vec![],
            dropped_findings: vec![],
        });
        let mut again = idle.clone();
        again.cycle = 3;
        again.decision.as_mut().unwrap().cycle = 3;
        assert!(events_between(Some(&panel(Some(idle.clone()))), &panel(Some(again))).is_empty());
        let mut other = idle.clone();
        other.decision.as_mut().unwrap().cycle = 3;
        other.decision.as_mut().unwrap().reason = "no actionable finding".into();
        assert_eq!(
            names(&events_between(
                Some(&panel(Some(idle))),
                &panel(Some(other))
            )),
            ["autopilot.decision"]
        );
    }

    #[test]
    fn a_restarted_daemon_stops_starts_and_decides_anew() {
        let old = panel(Some(decided(live(7, 4), 4)));
        let new = panel(Some(decided(live(9, 1), 1)));
        assert_eq!(
            names(&events_between(Some(&old), &new)),
            [
                "autopilot.stopped",
                "autopilot.started",
                "autopilot.decision"
            ]
        );
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
