//! Windowed crash-loop budget: the shared supervision policy core (spec 00070).
//!
//! Every long-running flywheel daemon -- the autopilot intake loop, a
//! git-worktree worker, a worker-pool slot -- can die. This module owns the
//! one shared decision those supervisors make on a death: *respawn it, or
//! declare it exhausted?* It answers with a rolling-window crash-loop budget
//! rather than a lifetime counter: a transient blip inside a healthy fleet
//! recovers, but a persistent boot-loop is contained before it can crash-loop
//! (for the autopilot worker, that means unbounded unattended writes to the
//! operator's base branch -- so the budget is a safety interlock).
//!
//! Two objects, deliberately split:
//!
//! * [`SupervisionBudget`] -- the *parameter shape*: `max_respawns` deaths
//!   tolerated inside a rolling `window_seconds`. A `max_respawns` of 0
//!   disables respawn entirely (`budget.disabled()` reads `true`).
//! * [`SupervisionPolicy`] -- the stateful decider. It records death instants
//!   against an *injected* monotonic clock (never wall-clock in the decision
//!   path, so tests drive time deterministically) and returns a
//!   [`RespawnDecision`] per death.
//!
//! Windowing rule (deterministic at the edge): a prior death is still counted
//! while it is *strictly newer* than `now - window_seconds`; a death exactly
//! on the far edge has decayed out and no longer counts.
//!
//! Boundary of the budget itself: with `max_respawns == N`, the Nth death
//! inside the window is still a `Respawn`; the (N+1)th is the first
//! `Exhausted`. Budget 0 makes the very first death `Exhausted`.
//!
//! This is pure counting -- it knows nothing of tasks, leases, subprocesses,
//! or how a supervisor acts on the decision.

use std::collections::VecDeque;
use std::fmt;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// What a supervisor should do about a death it just observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RespawnDecision {
    /// The death is inside budget; launch a fresh child.
    Respawn,
    /// The crash-loop budget is spent for the current window; stop
    /// respawning and surface the loud, queryable DEAD-after-budget state.
    Exhausted,
}

impl RespawnDecision {
    /// The wire spelling (`"respawn"` / `"exhausted"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            RespawnDecision::Respawn => "respawn",
            RespawnDecision::Exhausted => "exhausted",
        }
    }
}

impl fmt::Display for RespawnDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A [`SupervisionBudget`] parameter out of range.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SupervisionBudgetError {
    #[error("max_respawns must be >= 0, got {0}")]
    NegativeRespawns(i64),
    #[error("window_seconds must be > 0, got {0}")]
    NonPositiveWindow(f64),
}

/// The shared crash-loop budget parameter shape.
///
/// Tolerate at most `max_respawns` deaths within any rolling window of
/// `window_seconds` before declaring the daemon exhausted. This is the single
/// surface the autopilot supervisor, worker supervisor, and worker pool all
/// construct, so the three sites enforce one policy rather than three.
///
/// `max_respawns == 0` is the explicit disable: the very first death reports
/// `Exhausted` and nothing is ever respawned. `window_seconds` must be
/// positive even when disabled -- it is nominal in that case.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SupervisionBudget {
    max_respawns: u32,
    window_seconds: f64,
}

impl SupervisionBudget {
    /// Validate and build a budget. `max_respawns` is taken as a signed
    /// value so a negative configuration is rejected with the same
    /// diagnostic the Python dataclass raised, rather than wrapping.
    pub fn new(max_respawns: i64, window_seconds: f64) -> Result<Self, SupervisionBudgetError> {
        if max_respawns < 0 {
            return Err(SupervisionBudgetError::NegativeRespawns(max_respawns));
        }
        if window_seconds.is_nan() || window_seconds <= 0.0 {
            return Err(SupervisionBudgetError::NonPositiveWindow(window_seconds));
        }
        let max_respawns = u32::try_from(max_respawns).unwrap_or(u32::MAX);
        Ok(Self {
            max_respawns,
            window_seconds,
        })
    }

    /// Deaths tolerated inside one rolling window.
    pub const fn max_respawns(&self) -> u32 {
        self.max_respawns
    }

    /// The rolling window length in seconds.
    pub const fn window_seconds(&self) -> f64 {
        self.window_seconds
    }

    /// Whether respawn is turned off entirely (budget 0).
    pub const fn disabled(&self) -> bool {
        self.max_respawns == 0
    }
}

/// A monotonic clock reading in seconds; injected so tests drive time.
pub type Clock = Box<dyn FnMut() -> f64 + Send>;

/// Decide respawn-vs-exhausted for a daemon's deaths under a windowed budget.
///
/// Holds the death history for one supervised entity and an injected clock.
/// Call [`record_death`](Self::record_death) once per observed death; it
/// returns the [`RespawnDecision`] the supervisor acts on. One policy
/// instance tracks one entity (one daemon, or one pool slot); a pool
/// constructs one per slot so slots decay independently.
pub struct SupervisionPolicy {
    budget: SupervisionBudget,
    clock: Clock,
    /// Death instants inside the current window, oldest first. Pruned from
    /// the front on each death as the window slides forward.
    deaths: VecDeque<f64>,
}

impl fmt::Debug for SupervisionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisionPolicy")
            .field("budget", &self.budget)
            .field("deaths", &self.deaths)
            .finish_non_exhaustive()
    }
}

impl SupervisionPolicy {
    /// A policy on the process-monotonic clock (seconds since construction).
    pub fn new(budget: SupervisionBudget) -> Self {
        let anchor = Instant::now();
        Self::with_clock(budget, Box::new(move || anchor.elapsed().as_secs_f64()))
    }

    /// A policy driven by `clock` -- the deterministic path for tests and
    /// for supervisors that share one clock across many slots.
    pub fn with_clock(budget: SupervisionBudget, clock: Clock) -> Self {
        Self {
            budget,
            clock,
            deaths: VecDeque::new(),
        }
    }

    /// The budget this policy enforces.
    pub const fn budget(&self) -> &SupervisionBudget {
        &self.budget
    }

    /// Deaths currently counted in the window (as of the last record).
    ///
    /// Reflects state after the most recent [`record_death`]
    /// (Self::record_death); it does not re-read the clock, so a long idle
    /// gap since the last death is not decayed until the next death arrives.
    pub fn deaths_in_window(&self) -> usize {
        self.deaths.len()
    }

    /// Record one death at the current clock instant and decide.
    ///
    /// Decays any prior deaths that have fallen out of the rolling window
    /// (not strictly newer than `now - window_seconds`), counts this death,
    /// and returns `Respawn` while the count is within `max_respawns` or
    /// `Exhausted` once it exceeds it. Budget 0 exhausts on the first death.
    pub fn record_death(&mut self) -> RespawnDecision {
        let now = (self.clock)();
        let cutoff = now - self.budget.window_seconds;
        // A death exactly on the far edge (t == cutoff) has decayed out; only
        // deaths strictly newer than the cutoff remain counted.
        while self.deaths.front().is_some_and(|t| *t <= cutoff) {
            self.deaths.pop_front();
        }
        self.deaths.push_back(now);
        if self.deaths.len() <= self.budget.max_respawns as usize {
            RespawnDecision::Respawn
        } else {
            RespawnDecision::Exhausted
        }
    }

    /// Forget all recorded deaths (a clean supervised restart).
    pub fn reset(&mut self) {
        self.deaths.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A hand-driven clock shared with the policy under test.
    fn fake_clock() -> (Arc<Mutex<f64>>, Clock) {
        let now = Arc::new(Mutex::new(0.0));
        let handle = Arc::clone(&now);
        (now, Box::new(move || *handle.lock().unwrap()))
    }

    fn budget(max_respawns: i64, window_seconds: f64) -> SupervisionBudget {
        SupervisionBudget::new(max_respawns, window_seconds).unwrap()
    }

    #[test]
    fn window_decays_replenishes_budget() {
        let (now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(3, 10.0), clock);
        for t in [0.0, 1.0, 2.0] {
            *now.lock().unwrap() = t;
            assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        }
        assert_eq!(policy.deaths_in_window(), 3);
        // Well past the window: the burst has decayed, a fresh death respawns.
        *now.lock().unwrap() = 100.0;
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        assert_eq!(policy.deaths_in_window(), 1);
    }

    #[test]
    fn window_decay_only_prunes_deaths_past_the_edge() {
        let (now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(5, 10.0), clock);
        policy.record_death(); // t=0
        *now.lock().unwrap() = 6.0;
        policy.record_death(); // t=6
        assert_eq!(policy.deaths_in_window(), 2);
        *now.lock().unwrap() = 11.0; // cutoff 1: t=0 decays, t=6 stays
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        assert_eq!(policy.deaths_in_window(), 2);
    }

    #[test]
    fn budget_zero_no_respawn() {
        let b = budget(0, 10.0);
        assert!(b.disabled());
        let (_now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(b, clock);
        assert_eq!(policy.record_death(), RespawnDecision::Exhausted);
        assert_eq!(policy.record_death(), RespawnDecision::Exhausted);
    }

    #[test]
    fn nth_death_respawns_and_n_plus_one_exhausts() {
        let (now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(3, 100.0), clock);
        for i in 0..3 {
            *now.lock().unwrap() = i as f64;
            assert_eq!(
                policy.record_death(),
                RespawnDecision::Respawn,
                "death {}",
                i + 1
            );
        }
        *now.lock().unwrap() = 3.0;
        assert_eq!(policy.record_death(), RespawnDecision::Exhausted);
        *now.lock().unwrap() = 4.0;
        assert_eq!(policy.record_death(), RespawnDecision::Exhausted);
    }

    #[test]
    fn single_death_within_window_respawns() {
        let (_now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(1, 10.0), clock);
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        assert!(!policy.budget().disabled());
    }

    #[test]
    fn death_exactly_on_window_edge_has_decayed() {
        let (now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(1, 10.0), clock);
        assert_eq!(policy.record_death(), RespawnDecision::Respawn); // t=0
        *now.lock().unwrap() = 10.0; // cutoff 0: t=0 is on the edge, decayed
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        assert_eq!(policy.deaths_in_window(), 1);
    }

    #[test]
    fn death_just_inside_window_edge_still_counts() {
        let (now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(1, 10.0), clock);
        assert_eq!(policy.record_death(), RespawnDecision::Respawn); // t=0
        *now.lock().unwrap() = 9.999;
        assert_eq!(policy.record_death(), RespawnDecision::Exhausted);
        assert_eq!(policy.deaths_in_window(), 2);
    }

    #[test]
    fn reset_clears_death_history() {
        let (_now, clock) = fake_clock();
        let mut policy = SupervisionPolicy::with_clock(budget(1, 10.0), clock);
        policy.record_death();
        policy.record_death();
        assert_eq!(policy.deaths_in_window(), 2);
        policy.reset();
        assert_eq!(policy.deaths_in_window(), 0);
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
    }

    #[test]
    fn default_clock_is_monotonic() {
        let mut policy = SupervisionPolicy::new(budget(1, 10.0));
        assert_eq!(policy.record_death(), RespawnDecision::Respawn);
        assert_eq!(policy.deaths_in_window(), 1);
        assert!(format!("{policy:?}").contains("SupervisionPolicy"));
    }

    #[test]
    fn budget_validation() {
        assert_eq!(
            SupervisionBudget::new(-1, 10.0),
            Err(SupervisionBudgetError::NegativeRespawns(-1))
        );
        for bad in [0.0, -5.0, f64::NAN] {
            let err = SupervisionBudget::new(1, bad).unwrap_err();
            assert!(
                matches!(err, SupervisionBudgetError::NonPositiveWindow(_)),
                "{bad}"
            );
        }
        assert_eq!(
            SupervisionBudget::new(-1, 10.0).unwrap_err().to_string(),
            "max_respawns must be >= 0, got -1"
        );
        assert_eq!(
            SupervisionBudget::new(1, 0.0).unwrap_err().to_string(),
            "window_seconds must be > 0, got 0"
        );
        let b = budget(2, 1.5);
        assert_eq!(b.max_respawns(), 2);
        assert_eq!(b.window_seconds(), 1.5);
    }

    #[test]
    fn decision_wire_spelling() {
        assert_eq!(RespawnDecision::Respawn.as_str(), "respawn");
        assert_eq!(RespawnDecision::Exhausted.to_string(), "exhausted");
        assert_eq!(
            serde_json::to_string(&RespawnDecision::Exhausted).unwrap(),
            "\"exhausted\""
        );
    }
}
