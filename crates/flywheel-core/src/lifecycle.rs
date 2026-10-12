//! `Status` / `Attempt` / `Outcome` / `Lifecycle` and the legal transitions.
//!
//! Authoritative spec: `docs/task-lifecycle.md`. The `VALID_EDGES` table is
//! the exact set of edges the harness enforces; any other transition is a
//! [`LifecycleTransitionError`].
//!
//! This module is **pure**: no filesystem, no IO, no clock besides the
//! injectable `now` argument (defaulting to `Utc::now`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Ready,
    Running,
    Validating,
    AwaitingApproval,
    FailedValidation,
    InternalError,
    Done,
    Failed,
    Interrupted,
}

impl Status {
    pub const ALL: [Status; 10] = [
        Status::Pending,
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
        Status::FailedValidation,
        Status::InternalError,
        Status::Done,
        Status::Failed,
        Status::Interrupted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Ready => "ready",
            Status::Running => "running",
            Status::Validating => "validating",
            Status::AwaitingApproval => "awaiting_approval",
            Status::FailedValidation => "failed_validation",
            Status::InternalError => "internal_error",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Interrupted => "interrupted",
        }
    }

    /// `done` and `failed` have no outgoing edges.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Failed)
    }

    /// Statuses whose `-> ready` edge consumes retry budget.
    pub const fn is_retry_source(self) -> bool {
        matches!(self, Status::FailedValidation | Status::InternalError)
    }

    /// Statuses that require a non-empty error on entry.
    pub const fn requires_error(self) -> bool {
        matches!(
            self,
            Status::Failed | Status::FailedValidation | Status::InternalError
        )
    }

    /// The legal targets from this status (`docs/task-lifecycle.md`).
    pub const fn valid_targets(self) -> &'static [Status] {
        match self {
            Status::Pending => &[Status::Ready],
            Status::Ready => &[Status::Running],
            Status::Running => &[
                Status::Validating,
                Status::Failed,
                Status::Interrupted,
                Status::InternalError,
            ],
            Status::Validating => &[
                Status::Done,
                Status::AwaitingApproval,
                Status::FailedValidation,
                Status::Interrupted,
                Status::InternalError,
            ],
            Status::AwaitingApproval => &[Status::Done, Status::FailedValidation],
            Status::FailedValidation => &[Status::Ready, Status::Failed],
            Status::InternalError => &[Status::Ready, Status::Failed],
            Status::Interrupted => &[Status::Ready],
            Status::Done | Status::Failed => &[],
        }
    }

    pub fn can_transition_to(self, target: Status) -> bool {
        self.valid_targets().contains(&target)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Status::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| format!("unknown status {s:?}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    ValidationFailed,
    AgentError,
    Cancelled,
    InternalError,
    Recovered,
}

impl Outcome {
    pub const ALL: [Outcome; 6] = [
        Outcome::Succeeded,
        Outcome::ValidationFailed,
        Outcome::AgentError,
        Outcome::Cancelled,
        Outcome::InternalError,
        Outcome::Recovered,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Succeeded => "succeeded",
            Outcome::ValidationFailed => "validation_failed",
            Outcome::AgentError => "agent_error",
            Outcome::Cancelled => "cancelled",
            Outcome::InternalError => "internal_error",
            Outcome::Recovered => "recovered",
        }
    }

    /// Outcomes that count toward `consecutive_failed_runs`.
    pub const fn is_failed(self) -> bool {
        matches!(
            self,
            Outcome::ValidationFailed | Outcome::AgentError | Outcome::InternalError
        )
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Outcome {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Outcome::ALL
            .into_iter()
            .find(|o| o.as_str() == s)
            .ok_or_else(|| format!("unknown outcome {s:?}"))
    }
}

/// Raised when a `Lifecycle` transition violates `docs/task-lifecycle.md`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleTransitionError {
    #[error("illegal transition {from} -> {to}")]
    IllegalEdge { from: Status, to: Status },
    #[error("transition to {to} requires a non-empty error")]
    MissingError { to: Status },
}

/// Mint the default run id: `run-<uuid4 hex>`.
pub fn default_run_id() -> String {
    format!("run-{}", uuid::Uuid::new_v4().simple())
}

/// One execution attempt of a run.
///
/// The aggregate counters (`input_tokens` through `last_activity_at`) are
/// rolled up by the harness at iteration boundaries through the store's
/// versioned `save_attempt` write. Token fields mirror the SDK usage
/// breakdown keys and accumulate per-iteration deltas; `turns` and
/// `total_cost_usd` accumulate the SDK's session-cumulative per-iteration
/// readings (summed, accepting the known overcount when a session spans
/// iterations). A zero-iteration attempt carries all-zero counters and a
/// `None` last-activity timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub number: u32,
    pub started_at: DateTime<Utc>,
    pub run_id: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<Outcome>,
    pub agent_output: String,
    pub error: String,
    pub agent_context: BTreeMap<String, String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub iterations_completed: u64,
    pub turns: u64,
    pub total_cost_usd: f64,
    pub last_activity_at: Option<DateTime<Utc>>,
}

impl Attempt {
    pub fn new(number: u32, started_at: DateTime<Utc>, run_id: impl Into<String>) -> Self {
        Self {
            number,
            started_at,
            run_id: run_id.into(),
            ended_at: None,
            outcome: None,
            agent_output: String::new(),
            error: String::new(),
            agent_context: BTreeMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            iterations_completed: 0,
            turns: 0,
            total_cost_usd: 0.0,
            last_activity_at: None,
        }
    }

    /// Sum of the four token-usage fields (the dashboard's tokens).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

/// The single mutable record per run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lifecycle {
    pub task_id: String,
    pub run_id: String,
    pub worker_id: String,
    pub status: Status,
    pub timestamps: BTreeMap<Status, DateTime<Utc>>,
    pub version: u64,
    pub retries: u32,
    pub error: String,
    pub agent_output: String,
    pub attempts: Vec<Attempt>,
    pub session_id: String,
    pub artifacts_dir: String,
    pub blocked_requires_json: Option<String>,
    pub awaiting_manual_ordinal: Option<u32>,
    /// Content hash of the exact Task definition this run executed; pins the
    /// run to a row in the content-addressed `task_versions` store. Identity
    /// shaping like `task_id`/`run_id`: set once at seed, never copied by
    /// `replace_from`.
    pub task_content_hash: String,
    /// Opaque provenance label for where this run's work item came from (a
    /// task-file path, a tracker ref like `owner/repo#123`). Supplied by the
    /// caller, never interpreted here.
    pub source: String,
}

impl Lifecycle {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            run_id: default_run_id(),
            worker_id: String::new(),
            status: Status::Pending,
            timestamps: BTreeMap::new(),
            version: 1,
            retries: 0,
            error: String::new(),
            agent_output: String::new(),
            attempts: Vec::new(),
            session_id: String::new(),
            artifacts_dir: String::new(),
            blocked_requires_json: None,
            awaiting_manual_ordinal: None,
            task_content_hash: String::new(),
            source: String::new(),
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
    }

    /// Mutate state for one transition WITHOUT bumping `version`.
    ///
    /// This is the pure state-machine core shared by [`Self::transition_to`]
    /// (the in-place path that also bumps `version`) and the event reducer
    /// (`crate::events::apply`). Under event sourcing `version` is the
    /// domain-event offset and is owned by the appending caller, so the
    /// legality/retry/clear rules live here and the increment lives in the
    /// wrapper.
    pub fn apply_transition(
        &mut self,
        target: Status,
        error: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), LifecycleTransitionError> {
        if !self.status.can_transition_to(target) {
            return Err(LifecycleTransitionError::IllegalEdge {
                from: self.status,
                to: target,
            });
        }
        if target.requires_error() && error.is_empty() {
            return Err(LifecycleTransitionError::MissingError { to: target });
        }
        let is_retry_edge = self.status.is_retry_source() && target == Status::Ready;
        self.status = target;
        self.timestamps
            .insert(target, now.unwrap_or_else(Utc::now));
        if is_retry_edge {
            self.retries += 1;
            self.error.clear();
        } else if !error.is_empty() {
            self.error = error.to_owned();
        }
        // Centralized clear: any transition that lands on READY drops the
        // pending blocked-requires snapshot.
        if target == Status::Ready {
            self.blocked_requires_json = None;
        }
        // Centralized clear for the manual-approval gate ordinal: any edge
        // that exits the gate or restarts from READY drops it.
        if matches!(
            target,
            Status::Ready | Status::Done | Status::FailedValidation
        ) {
            self.awaiting_manual_ordinal = None;
        }
        Ok(())
    }

    /// Apply a transition and bump `version` (the in-place path).
    pub fn transition_to(
        &mut self,
        target: Status,
        error: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), LifecycleTransitionError> {
        self.apply_transition(target, error, now)?;
        self.version += 1;
        Ok(())
    }

    /// Overwrite this lifecycle's mutable state from a persisted row.
    ///
    /// Not a state-machine transition: used by the harness at `run_task`
    /// entry to align an in-memory lifecycle with the canonical row a prior
    /// run persisted. The identity-shaping fields (`run_id`, `task_id`,
    /// `task_content_hash`, `source`) are deliberately not copied, and
    /// `attempts` is left alone (the harness reloads it independently).
    pub fn replace_from(&mut self, persisted: &Lifecycle) {
        self.status = persisted.status;
        self.version = persisted.version;
        self.retries = persisted.retries;
        self.error = persisted.error.clone();
        self.timestamps = persisted.timestamps.clone();
        self.blocked_requires_json = persisted.blocked_requires_json.clone();
        self.awaiting_manual_ordinal = persisted.awaiting_manual_ordinal;
        self.agent_output = persisted.agent_output.clone();
        self.session_id = persisted.session_id.clone();
        self.artifacts_dir = persisted.artifacts_dir.clone();
        self.worker_id = persisted.worker_id.clone();
    }

    pub fn is_retry_eligible(&self, max_retries: u32) -> bool {
        self.status.is_retry_source() && self.retries < max_retries
    }

    /// Sequential failed attempts (grouped by `run_id`) from the tail of the
    /// attempts list; drives circuit-breaker logic.
    pub fn consecutive_failed_runs(&self) -> usize {
        let Some(tail) = self.attempts.last() else {
            return 0;
        };
        if !tail.outcome.is_some_and(Outcome::is_failed) {
            return 0;
        }
        self.attempts
            .iter()
            .rev()
            .take_while(|a| a.run_id == tail.run_id && a.outcome.is_some_and(Outcome::is_failed))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn edges_match_spec() {
        // Every legal edge from docs/task-lifecycle.md, and nothing else.
        let spec: &[(Status, &[Status])] = &[
            (Status::Pending, &[Status::Ready]),
            (Status::Ready, &[Status::Running]),
            (
                Status::Running,
                &[Status::Validating, Status::Failed, Status::Interrupted, Status::InternalError],
            ),
            (
                Status::Validating,
                &[
                    Status::Done,
                    Status::AwaitingApproval,
                    Status::FailedValidation,
                    Status::Interrupted,
                    Status::InternalError,
                ],
            ),
            (Status::AwaitingApproval, &[Status::Done, Status::FailedValidation]),
            (Status::FailedValidation, &[Status::Ready, Status::Failed]),
            (Status::InternalError, &[Status::Ready, Status::Failed]),
            (Status::Interrupted, &[Status::Ready]),
            (Status::Done, &[]),
            (Status::Failed, &[]),
        ];
        for (from, targets) in spec {
            for to in Status::ALL {
                assert_eq!(from.can_transition_to(to), targets.contains(&to), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn transition_bumps_version_and_records_timestamp() {
        let mut lc = Lifecycle::new("t");
        lc.transition_to(Status::Ready, "", Some(at(10))).unwrap();
        assert_eq!(lc.version, 2);
        assert_eq!(lc.timestamps[&Status::Ready], at(10));
        assert_eq!(
            lc.transition_to(Status::Done, "", None).unwrap_err(),
            LifecycleTransitionError::IllegalEdge { from: Status::Ready, to: Status::Done }
        );
    }

    #[test]
    fn error_required_on_failure_edges() {
        let mut lc = Lifecycle::new("t");
        lc.transition_to(Status::Ready, "", None).unwrap();
        lc.transition_to(Status::Running, "", None).unwrap();
        assert_eq!(
            lc.transition_to(Status::Failed, "", None).unwrap_err(),
            LifecycleTransitionError::MissingError { to: Status::Failed }
        );
        lc.transition_to(Status::InternalError, "boom", None).unwrap();
        assert_eq!(lc.error, "boom");
    }

    #[test]
    fn retry_edge_increments_retries_and_clears() {
        let mut lc = Lifecycle::new("t");
        for (s, e) in [
            (Status::Ready, ""),
            (Status::Running, ""),
            (Status::Validating, ""),
            (Status::FailedValidation, "grader failed"),
        ] {
            lc.transition_to(s, e, None).unwrap();
        }
        lc.blocked_requires_json = Some("[]".into());
        lc.awaiting_manual_ordinal = Some(1);
        assert!(lc.is_retry_eligible(1));
        assert!(!lc.is_retry_eligible(0));
        lc.transition_to(Status::Ready, "", None).unwrap();
        assert_eq!(lc.retries, 1);
        assert_eq!(lc.error, "");
        assert_eq!(lc.blocked_requires_json, None);
        assert_eq!(lc.awaiting_manual_ordinal, None);
    }

    #[test]
    fn interrupted_resume_does_not_consume_budget() {
        let mut lc = Lifecycle::new("t");
        lc.transition_to(Status::Ready, "", None).unwrap();
        lc.transition_to(Status::Running, "", None).unwrap();
        lc.transition_to(Status::Interrupted, "", None).unwrap();
        lc.blocked_requires_json = Some("[{}]".into());
        lc.transition_to(Status::Ready, "", None).unwrap();
        assert_eq!(lc.retries, 0);
        assert_eq!(lc.blocked_requires_json, None);
    }

    #[test]
    fn replace_from_skips_identity_fields() {
        let mut live = Lifecycle::new("t").with_run_id("r1");
        live.task_content_hash = "h1".into();
        let mut persisted = Lifecycle::new("other").with_run_id("r2");
        persisted.status = Status::Interrupted;
        persisted.version = 9;
        persisted.retries = 2;
        persisted.worker_id = "w".into();
        persisted.task_content_hash = "h2".into();
        live.replace_from(&persisted);
        assert_eq!(live.status, Status::Interrupted);
        assert_eq!(live.version, 9);
        assert_eq!(live.retries, 2);
        assert_eq!(live.worker_id, "w");
        assert_eq!(live.run_id, "r1");
        assert_eq!(live.task_id, "t");
        assert_eq!(live.task_content_hash, "h1");
    }

    #[test]
    fn consecutive_failed_runs_groups_by_run_id() {
        let mut lc = Lifecycle::new("t");
        let mut a = |n, run, outcome| {
            let mut att = Attempt::new(n, at(0), run);
            att.outcome = outcome;
            att
        };
        assert_eq!(lc.consecutive_failed_runs(), 0);
        lc.attempts = vec![
            a(1, "r1", Some(Outcome::ValidationFailed)),
            a(2, "r2", Some(Outcome::AgentError)),
            a(3, "r2", Some(Outcome::InternalError)),
        ];
        assert_eq!(lc.consecutive_failed_runs(), 2);
        lc.attempts.push(a(4, "r2", Some(Outcome::Succeeded)));
        assert_eq!(lc.consecutive_failed_runs(), 0);
        lc.attempts.push(a(5, "r2", Some(Outcome::Cancelled)));
        assert_eq!(lc.consecutive_failed_runs(), 0);
    }

    #[test]
    fn status_strings_round_trip() {
        for s in Status::ALL {
            assert_eq!(s.as_str().parse::<Status>().unwrap(), s);
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
        for o in Outcome::ALL {
            assert_eq!(o.as_str().parse::<Outcome>().unwrap(), o);
        }
    }
}
