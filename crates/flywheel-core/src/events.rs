//! Domain events: the event-sourced source of truth for a lifecycle.
//!
//! A [`Lifecycle`] is the *fold* of a closed, ordered sequence of domain
//! events. This module defines that closed set plus the pure reducer
//! ([`apply`] / [`replay`]) that derives lifecycle state from it.
//!
//! Two invariants make this safe to depend on:
//!
//! - **Closed taxonomy.** Only the event kinds defined here may change
//!   lifecycle state. Pure-observability telemetry (`harness.iteration_completed`
//!   and friends) lives outside this module and is never folded into state.
//! - **Purity.** Like `crate::lifecycle`, this module performs no IO.
//!   Serialization of an event to/from a store row is `crate::event_serde`'s
//!   job; here events are plain data.
//!
//! `version` semantics: it is the *domain-event offset*. Every domain event
//! advances `version` by exactly one, so for a folded lifecycle
//! `version == number of domain events folded`. This doubles as the
//! optimistic-concurrency compare-and-swap key when the store conditionally
//! appends the next event.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

use crate::lifecycle::{Attempt, Lifecycle, LifecycleTransitionError, Outcome, Status};

/// Discriminator for the closed domain-event set.
///
/// The string form is the stable wire identifier a store persists in the
/// `events.kind` column alongside the serialized event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainEventKind {
    LifecycleInitialized,
    TransitionedTo,
    Blocked,
    Unblocked,
    AwaitingApproval,
    RetryScheduled,
    AttemptStarted,
    AttemptFinalized,
    SessionRecorded,
    GraderEvaluated,
    CommandApplied,
    LandingParked,
    Landed,
    HeldOutGateEvaluated,
    LandingRedriven,
    LandingRouted,
}

impl DomainEventKind {
    pub const ALL: [DomainEventKind; 16] = [
        DomainEventKind::LifecycleInitialized,
        DomainEventKind::TransitionedTo,
        DomainEventKind::Blocked,
        DomainEventKind::Unblocked,
        DomainEventKind::AwaitingApproval,
        DomainEventKind::RetryScheduled,
        DomainEventKind::AttemptStarted,
        DomainEventKind::AttemptFinalized,
        DomainEventKind::SessionRecorded,
        DomainEventKind::GraderEvaluated,
        DomainEventKind::CommandApplied,
        DomainEventKind::LandingParked,
        DomainEventKind::Landed,
        DomainEventKind::HeldOutGateEvaluated,
        DomainEventKind::LandingRedriven,
        DomainEventKind::LandingRouted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DomainEventKind::LifecycleInitialized => "lifecycle_initialized",
            DomainEventKind::TransitionedTo => "transitioned_to",
            DomainEventKind::Blocked => "blocked",
            DomainEventKind::Unblocked => "unblocked",
            DomainEventKind::AwaitingApproval => "awaiting_approval",
            DomainEventKind::RetryScheduled => "retry_scheduled",
            DomainEventKind::AttemptStarted => "attempt_started",
            DomainEventKind::AttemptFinalized => "attempt_finalized",
            DomainEventKind::SessionRecorded => "session_recorded",
            DomainEventKind::GraderEvaluated => "grader_evaluated",
            DomainEventKind::CommandApplied => "command_applied",
            DomainEventKind::LandingParked => "landing_parked",
            DomainEventKind::Landed => "landed",
            DomainEventKind::HeldOutGateEvaluated => "held_out_gate_evaluated",
            DomainEventKind::LandingRedriven => "landing_redriven",
            DomainEventKind::LandingRouted => "landing_routed",
        }
    }

    /// Kinds whose fold onto the [`Lifecycle`] is the identity: they carry
    /// audit intent or project a separate table, and advance `version` only.
    pub const fn is_identity_fold(self) -> bool {
        matches!(
            self,
            DomainEventKind::Unblocked
                | DomainEventKind::RetryScheduled
                | DomainEventKind::GraderEvaluated
                | DomainEventKind::CommandApplied
                | DomainEventKind::LandingParked
                | DomainEventKind::Landed
                | DomainEventKind::HeldOutGateEvaluated
                | DomainEventKind::LandingRedriven
                | DomainEventKind::LandingRouted
        )
    }
}

impl fmt::Display for DomainEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DomainEventKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DomainEventKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown domain event kind {s:?}"))
    }
}

// --- Per-kind payloads ---------------------------------------------------------

/// Seeds the lifecycle. Must be the first event in any stream.
///
/// Appending this event is what brings a lifecycle row into existence, so
/// there is no window in which events exist before the lifecycle does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleInitialized {
    pub task_id: String,
    #[serde(default)]
    pub worker_id: String,
    #[serde(default)]
    pub artifacts_dir: String,
    #[serde(default)]
    pub task_content_hash: String,
    #[serde(default)]
    pub source: String,
}

/// A state-machine move. Carries the status and, for failure states, the
/// error string. The reducer derives the retry-counter increment and the
/// READY-clears-`blocked_requires_json` rule from the edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionedTo {
    pub target: Status,
    #[serde(default)]
    pub error: String,
}

/// Records the machine-readable blocked-requires snapshot. Always paired
/// with a subsequent `TransitionedTo(Interrupted)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub requires_json: String,
}

/// Records which manual-grader gate a lifecycle is parking on.
///
/// Always paired with a subsequent `TransitionedTo(AwaitingApproval)` (or,
/// on re-park to a later gate, a re-emit while the lifecycle is already in
/// `AwaitingApproval`). The reducer folds `awaiting_ordinal` onto the
/// lifecycle's `awaiting_manual_ordinal`; the centralized clear on
/// `-> Ready` / `-> Done` / `-> FailedValidation` later nulls it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitingApproval {
    pub awaiting_ordinal: u32,
}

/// Audit witness of the harness's decision to retry. The retry-counter bump
/// is applied by the paired `TransitionedTo(Ready)` from a retry-source
/// status, so this event's fold is the identity (it still advances
/// `version`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryScheduled {
    pub retries_used: u32,
    pub max_retries: u32,
}

/// Opens a new [`Attempt`] in the lifecycle's attempts projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptStarted {
    pub number: u32,
    pub attempt_run_id: String,
    #[serde(with = "crate::event_serde::timestamp")]
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub agent_context: BTreeMap<String, String>,
}

/// Closes the matching [`Attempt`] and records the lifecycle's last agent
/// output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptFinalized {
    pub number: u32,
    pub outcome: Outcome,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ended_at: DateTime<Utc>,
    #[serde(default)]
    pub agent_output: String,
    #[serde(default)]
    pub error: String,
}

/// Records the agent session id for resumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecorded {
    pub session_id: String,
}

/// A grader receipt. Projects a `grader_results` row; its fold onto the
/// [`Lifecycle`] is the identity (it still advances `version`).
///
/// `grader_type` is the plain wire string (`command` / `transcript` /
/// `rubric` / `manual`); the projection parses it into
/// `crate::task::GraderType`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraderEvaluated {
    pub ordinal: u32,
    pub grader_type: String,
    pub passed: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub grader_name: Option<String>,
    #[serde(default)]
    pub grader_spec: Map<String, Value>,
    #[serde(default)]
    pub payload: Map<String, Value>,
}

/// Records an operator control command applied against the run.
///
/// A ledger fact (spec 00025 FR-10): the operator intervention survives even
/// after the applied `control_commands` queue row is deleted. The event's
/// `ts` is the applied-at moment; `command_kind` / `command_payload` snapshot
/// the verb and the operator-supplied fields; `command_id` keeps the
/// queue-row provenance. Its fold onto the [`Lifecycle`] is the identity (it
/// still advances `version`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandApplied {
    pub command_kind: String,
    #[serde(default)]
    pub command_payload: Map<String, Value>,
    #[serde(default)]
    pub command_id: Option<i64>,
}

/// Per-grader output excerpt bound carried on a [`GateGraderReceipt`]. The
/// excerpt is a *tail* of the grader's captured output, capped at this many
/// bytes and stored raw; redaction is a render-time concern, never applied
/// at persist time (spec 00073, D-2). Mirrors the command-runner's stream
/// tail bound so a receipt shows the same final content an operator would
/// see.
pub const GATE_EXCERPT_MAX_BYTES: usize = 8192;

/// One executed check's receipt on a decision record.
///
/// Carries the diagnosable minimum for a single check: its `grader_name`
/// (`None` for an unnamed check), its `passed` outcome, and a bounded
/// `output_excerpt`: a raw tail of the check's captured output capped at
/// [`GATE_EXCERPT_MAX_BYTES`] bytes (spec 00073, criteria 1/11). The excerpt
/// retains the *final* content when the check emitted more than the bound,
/// so the reason a decision was taken is visible from the store alone.
/// Stored raw: redaction is applied at render time, not persist time (D-2).
///
/// Reused by the held-out landing gate's [`HeldOutGateEvaluated`] verdict and
/// by a grader-decided [`LandingParked`] record (the `[submit] verify`
/// standing build invariant or a post-rebase re-verification), so both
/// surface a deciding check's output through one shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateGraderReceipt {
    #[serde(default)]
    pub grader_name: Option<String>,
    #[serde(default)]
    pub passed: bool,
    #[serde(default)]
    pub output_excerpt: String,
}

/// Records that a DONE run's branch could not be landed at submit time and
/// its worktree was parked for forensics.
///
/// An audit-witness event (like [`Unblocked`](DomainEventPayload::Unblocked)
/// / [`RetryScheduled`]): landing happens after the run already finalized
/// `Done`, the harness owns lifecycle transitions, and `Done` is terminal,
/// so this event's fold is the identity (it advances `version` only and
/// performs no state change). The run stays `Status::Done`.
///
/// `park_kind` discriminates the cause on one shared surface (see
/// [`LANDING_PARK_KINDS`]); `detail` is a human-readable reason, queryable
/// via `list_domain_events(run_id)`.
///
/// `receipts` carries the deciding check's output for a park a grader
/// decided (the `[submit] verify` standing build invariant, or a post-rebase
/// re-verification under `divergent-base`), one [`GateGraderReceipt`] per
/// executed check. Empty for every other park kind, whose cause is fully
/// carried by `park_kind` / `detail`; omitted from the wire payload when
/// empty so pre-existing rows stay byte-identical.
///
/// `agent_turns` / `agent_wall_seconds` record the bounded
/// conflict-resolution session's usage when a park followed an
/// agent-resolution attempt (the `merge-conflict` rung's escalation). Both
/// default to `None` and are omitted from the wire payload when absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LandingParked {
    pub park_kind: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<GateGraderReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_wall_seconds: Option<f64>,
}

/// Records that a DONE run's branch actually landed at submit time, carrying
/// the landed reference.
///
/// The success counterpart to [`LandingParked`]: appended only *after* the
/// land completes (a fast-forward merge into the base or an opened pull
/// request), so an incomplete land leaves no `Landed` record. An
/// audit-witness event: its fold is the identity and the run stays
/// `Status::Done`.
///
/// `strategy` names the submit strategy that landed the work (see
/// [`LANDING_STRATEGIES`]): `merge` for a fast-forward merge or `pr` for an
/// opened pull request. `landed_ref` is the corresponding reference: the
/// landed commit sha for a merge land, the pull-request identifier for a PR
/// land.
///
/// `rung` discriminates which rung of the merge strategy's recovery ladder
/// landed the work (see [`LANDING_RUNGS`]). It defaults to empty, and is
/// omitted from the wire payload when empty, so a PR land and any record
/// written before the field existed stay valid.
///
/// `agent_turns` / `agent_wall_seconds` record the resolution session's usage
/// on an `agent-resolved` land. Both default to `None` and are omitted from
/// the wire payload when absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Landed {
    pub strategy: String,
    pub landed_ref: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rung: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_wall_seconds: Option<f64>,
}

/// Records the disposition of one bounded landing re-drive of a parked run.
///
/// Appended by the orchestrator's landing re-driver only *after* a real land
/// re-attempt (or the terminal routing) produced its outcome, so the record
/// is always paired with a durable outcome witness (spec 00073, criterion
/// 5): a [`Landed`] for `landed`, a fresh [`LandingParked`] for `re-parked`,
/// or a human-review queue entry for `routed`. Recording a re-drive without a
/// real re-attempt is the foreclosed cheat. An audit-witness event: its fold
/// is the identity. `result` is one of [`LANDING_REDRIVE_RESULTS`];
/// `park_kind` snapshots the park cause the re-drive was clearing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingRedriven {
    pub result: String,
    #[serde(default)]
    pub park_kind: String,
}

/// Records a held-out landing-gate evaluation's verdict on the run's ledger.
///
/// Emitted for *every* gate evaluation (pass, fail, or no-gate; spec 00073,
/// D-1) so a gate-decided park is diagnosable from the store alone. An
/// audit-witness event: its fold is the identity.
///
/// `outcome` is the terminal gate outcome's stable wire value (`no_gate` /
/// `pass` / `fail`), a plain string because the gate engine's outcome enum
/// lives in the orchestrator, downstream of this pure module. `reason`
/// mirrors the verdict's operator-readable summary. `receipts` carries one
/// [`GateGraderReceipt`] per executed held-out grader in execution order:
/// populated for `pass`/`fail`, empty for `no_gate` and for a load-time
/// fail-closed where no grader ran. The `receipts` key is always written, so
/// a `no_gate` record with empty receipts stays distinguishable from no
/// record at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldOutGateEvaluated {
    pub outcome: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub receipts: Vec<GateGraderReceipt>,
}

/// Records a risk-tier routing decision on the run's ledger (spec 00080).
///
/// Appended by the tier-routing submitter at classification time, before the
/// routed strategy runs, so the decision is diagnosable from the store alone
/// even when the routed landing subsequently parks. `per_file_tiers` maps
/// every changed file to its computed tier, `winning_tier` is the highest of
/// those, and `strategy` names the submit-strategy registry entry the
/// decision routed to (`merge` / `phase` / `pr`: the route vocabulary,
/// distinct from the landing mechanism a later [`Landed`] record names). An
/// audit-witness event: its fold is the identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingRouted {
    pub winning_tier: u32,
    pub strategy: String,
    #[serde(default)]
    pub per_file_tiers: BTreeMap<String, u32>,
}

/// The closed set of domain-event payloads, tagged by [`DomainEventKind`].
///
/// Serializes as an object carrying `"kind": "<wire id>"` beside the
/// variant's fields. The persisted `events.payload_json` column holds this
/// object *without* the `kind` key (it is its own column); see
/// `crate::event_serde` for the row projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainEventPayload {
    LifecycleInitialized(LifecycleInitialized),
    TransitionedTo(TransitionedTo),
    Blocked(Blocked),
    /// Audit witness that a blocked lifecycle's requirements were satisfied.
    /// The actual snapshot clear happens via the paired
    /// `TransitionedTo(Ready)`, so this event's fold is the identity (it
    /// still advances `version`).
    Unblocked,
    AwaitingApproval(AwaitingApproval),
    RetryScheduled(RetryScheduled),
    AttemptStarted(AttemptStarted),
    AttemptFinalized(AttemptFinalized),
    SessionRecorded(SessionRecorded),
    GraderEvaluated(GraderEvaluated),
    CommandApplied(CommandApplied),
    LandingParked(LandingParked),
    Landed(Landed),
    HeldOutGateEvaluated(HeldOutGateEvaluated),
    LandingRedriven(LandingRedriven),
    LandingRouted(LandingRouted),
}

impl DomainEventPayload {
    pub const fn kind(&self) -> DomainEventKind {
        match self {
            DomainEventPayload::LifecycleInitialized(_) => DomainEventKind::LifecycleInitialized,
            DomainEventPayload::TransitionedTo(_) => DomainEventKind::TransitionedTo,
            DomainEventPayload::Blocked(_) => DomainEventKind::Blocked,
            DomainEventPayload::Unblocked => DomainEventKind::Unblocked,
            DomainEventPayload::AwaitingApproval(_) => DomainEventKind::AwaitingApproval,
            DomainEventPayload::RetryScheduled(_) => DomainEventKind::RetryScheduled,
            DomainEventPayload::AttemptStarted(_) => DomainEventKind::AttemptStarted,
            DomainEventPayload::AttemptFinalized(_) => DomainEventKind::AttemptFinalized,
            DomainEventPayload::SessionRecorded(_) => DomainEventKind::SessionRecorded,
            DomainEventPayload::GraderEvaluated(_) => DomainEventKind::GraderEvaluated,
            DomainEventPayload::CommandApplied(_) => DomainEventKind::CommandApplied,
            DomainEventPayload::LandingParked(_) => DomainEventKind::LandingParked,
            DomainEventPayload::Landed(_) => DomainEventKind::Landed,
            DomainEventPayload::HeldOutGateEvaluated(_) => DomainEventKind::HeldOutGateEvaluated,
            DomainEventPayload::LandingRedriven(_) => DomainEventKind::LandingRedriven,
            DomainEventPayload::LandingRouted(_) => DomainEventKind::LandingRouted,
        }
    }
}

/// One domain event: the common columns plus the kind-specific payload.
///
/// `sequence` and `id` are assigned by the store on append and are `None`
/// for in-memory events that have not been persisted yet. They are never
/// read by the reducer; ordering during a fold is the caller's
/// responsibility (the store returns events in `sequence` order).
/// `attempt_number` locates the event within the run when it belongs to an
/// attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub run_id: String,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub attempt_number: Option<u32>,
    #[serde(default)]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(flatten)]
    pub payload: DomainEventPayload,
}

impl DomainEvent {
    /// An unpersisted event (`sequence`/`id` unset, no attempt coordinate).
    pub fn new(run_id: impl Into<String>, ts: DateTime<Utc>, payload: DomainEventPayload) -> Self {
        Self {
            run_id: run_id.into(),
            ts,
            attempt_number: None,
            sequence: None,
            id: None,
            payload,
        }
    }

    pub fn with_attempt_number(mut self, attempt_number: u32) -> Self {
        self.attempt_number = Some(attempt_number);
        self
    }

    pub const fn kind(&self) -> DomainEventKind {
        self.payload.kind()
    }
}

// --- Landing vocabularies -----------------------------------------------------

// Landing-park cause vocabulary. Every site that suppresses a land (the merge
// and PR protected-path refusals, a failed push / PR open, a swallowed submit
// error, the held-out landing gate, an unresolvable merge conflict) names its
// cause with one of these stable kebab-case spellings, kept identical across
// the worker, PR, and orchestrate emit sites so a single status reader
// recognizes them all.
pub const PARK_KIND_UNCOMMITTED_WORK: &str = "uncommitted-work";
pub const PARK_KIND_DIVERGENT_BASE: &str = "divergent-base";
pub const PARK_KIND_STANDING_VERIFY: &str = "standing-verify";
pub const PARK_KIND_HELD_OUT_GATE: &str = "held-out-gate";
pub const PARK_KIND_PROTECTED_PATHS: &str = "protected-paths";
pub const PARK_KIND_PUSH_FAILED: &str = "push-failed";
pub const PARK_KIND_SUBMIT_ERROR: &str = "submit-error";
/// A DONE branch whose rebase onto the advanced base conflicts falls through
/// to the merge-fallback rung; when the merge itself conflicts (no clean tree
/// to re-verify), the land is suppressed under this kind. Distinct from
/// `divergent-base` (which marks a *merged* tree that failed re-verify).
pub const PARK_KIND_MERGE_CONFLICT: &str = "merge-conflict";

/// The closed `park_kind` vocabulary on a [`LandingParked`] record.
pub const LANDING_PARK_KINDS: [&str; 8] = [
    PARK_KIND_UNCOMMITTED_WORK,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_STANDING_VERIFY,
    PARK_KIND_HELD_OUT_GATE,
    PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_PUSH_FAILED,
    PARK_KIND_SUBMIT_ERROR,
    PARK_KIND_MERGE_CONFLICT,
];

/// The landing-strand subset the archive sweep treats as resolvable ONLY by
/// git-truth or a deliberate operator, never by the act of archiving (spec
/// 00077, criteria 3/4; D-2/D-3). A DONE task parked under one of these kinds
/// is a *landing strand*: whether its verified work is reachable from the
/// landing base is a git fact the sweep must prove, so archival must never
/// stamp such a strand `stop-resolved` on its own. The remaining
/// [`LANDING_PARK_KINDS`] members keep the plain archival-supersession
/// contract, so a single reader tells the two apart by membership alone.
pub const LANDING_STRAND_KINDS: [&str; 4] = [
    PARK_KIND_UNCOMMITTED_WORK,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_STANDING_VERIFY,
    PARK_KIND_PROTECTED_PATHS,
];

// Landing-strategy vocabulary carried on a `Landed` record's `strategy`
// field, so a single reader can tell a landed commit sha from a landed PR
// identifier.
pub const LANDING_STRATEGY_MERGE: &str = "merge";
pub const LANDING_STRATEGY_PR: &str = "pr";

pub const LANDING_STRATEGIES: [&str; 2] = [LANDING_STRATEGY_MERGE, LANDING_STRATEGY_PR];

// Landing-rung vocabulary carried on a `Landed` record's `rung` field: which
// rung of the merge strategy's recovery ladder (fast-forward -> rebase ->
// merge-fallback -> agent-resolved) actually landed the work. Empty is the
// default so a PR land and any pre-existing record stay valid.
pub const RUNG_FAST_FORWARD: &str = "fast-forward";
pub const RUNG_REBASE: &str = "rebase";
pub const RUNG_MERGE_FALLBACK: &str = "merge-fallback";
pub const RUNG_AGENT_RESOLVED: &str = "agent-resolved";

pub const LANDING_RUNGS: [&str; 4] = [
    RUNG_FAST_FORWARD,
    RUNG_REBASE,
    RUNG_MERGE_FALLBACK,
    RUNG_AGENT_RESOLVED,
];

// Landing re-drive result vocabulary carried on a `LandingRedriven` record's
// `result` field, naming which outcome witness the record is paired with.
pub const REDRIVE_RESULT_LANDED: &str = "landed";
pub const REDRIVE_RESULT_REPARKED: &str = "re-parked";
pub const REDRIVE_RESULT_ROUTED: &str = "routed";

pub const LANDING_REDRIVE_RESULTS: [&str; 3] = [
    REDRIVE_RESULT_LANDED,
    REDRIVE_RESULT_REPARKED,
    REDRIVE_RESULT_ROUTED,
];

// --- Reducer ------------------------------------------------------------------

/// Raised when an event stream cannot be folded into a coherent state.
///
/// Surfaced loudly rather than coerced: an empty stream, a stream that does
/// not begin with [`LifecycleInitialized`], a duplicate initialization, a
/// start for an attempt number already open, or a finalize for an attempt
/// that was never started all indicate a corrupt log, not a recoverable
/// condition. An illegal state-machine edge surfaces the underlying
/// [`LifecycleTransitionError`] for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventReplayError {
    #[error("cannot replay an empty event stream")]
    EmptyStream,
    #[error("event stream must begin with LifecycleInitialized")]
    MissingSeed,
    #[error("LifecycleInitialized must be the first and only seed event")]
    DuplicateSeed,
    #[error("AttemptStarted for attempt number {number} that is already open")]
    DuplicateAttempt { number: u32 },
    #[error("AttemptFinalized for unknown attempt number {number}")]
    UnknownAttempt { number: u32 },
    #[error(transparent)]
    Transition(#[from] LifecycleTransitionError),
}

fn seed(event: &DomainEvent, init: &LifecycleInitialized) -> Lifecycle {
    let mut lifecycle = Lifecycle::new(init.task_id.clone()).with_run_id(event.run_id.clone());
    lifecycle.worker_id = init.worker_id.clone();
    lifecycle.artifacts_dir = init.artifacts_dir.clone();
    lifecycle.task_content_hash = init.task_content_hash.clone();
    lifecycle.source = init.source.clone();
    lifecycle
}

/// Fold one non-seed event into `state` in place.
///
/// The in-place core shared by [`apply`] (which clones first so its input is
/// never mutated) and [`replay`] (which owns its accumulator and skips the
/// per-event clone). `state` must already be seeded; a seed event here is a
/// [`EventReplayError::DuplicateSeed`]. On error `state` may be left
/// partially advanced and must be discarded.
pub fn fold_into(state: &mut Lifecycle, event: &DomainEvent) -> Result<(), EventReplayError> {
    state.version += 1;
    match &event.payload {
        DomainEventPayload::LifecycleInitialized(_) => {
            return Err(EventReplayError::DuplicateSeed);
        }
        DomainEventPayload::TransitionedTo(t) => {
            state.apply_transition(t.target, &t.error, Some(event.ts))?;
        }
        DomainEventPayload::Blocked(b) => {
            state.blocked_requires_json = Some(b.requires_json.clone());
        }
        DomainEventPayload::AwaitingApproval(a) => {
            state.awaiting_manual_ordinal = Some(a.awaiting_ordinal);
        }
        DomainEventPayload::AttemptStarted(a) => {
            if state
                .attempts
                .iter()
                .any(|existing| existing.number == a.number)
            {
                return Err(EventReplayError::DuplicateAttempt { number: a.number });
            }
            let mut attempt = Attempt::new(a.number, a.started_at, a.attempt_run_id.clone());
            attempt.agent_context = a.agent_context.clone();
            state.attempts.push(attempt);
        }
        DomainEventPayload::AttemptFinalized(f) => {
            let attempt = state
                .attempts
                .iter_mut()
                .find(|existing| existing.number == f.number)
                .ok_or(EventReplayError::UnknownAttempt { number: f.number })?;
            attempt.ended_at = Some(f.ended_at);
            attempt.outcome = Some(f.outcome);
            attempt.agent_output = f.agent_output.clone();
            attempt.error = f.error.clone();
            state.agent_output = f.agent_output.clone();
        }
        DomainEventPayload::SessionRecorded(s) => {
            state.session_id = s.session_id.clone();
        }
        // Identity fold: these carry audit intent or project a separate
        // table. They still advance version as members of the sequence.
        DomainEventPayload::Unblocked
        | DomainEventPayload::RetryScheduled(_)
        | DomainEventPayload::GraderEvaluated(_)
        | DomainEventPayload::CommandApplied(_)
        | DomainEventPayload::LandingParked(_)
        | DomainEventPayload::Landed(_)
        | DomainEventPayload::HeldOutGateEvaluated(_)
        | DomainEventPayload::LandingRedriven(_)
        | DomainEventPayload::LandingRouted(_) => {}
    }
    Ok(())
}

/// Fold one domain event onto `state`, returning a new lifecycle.
///
/// `state` is `None` only for the leading [`LifecycleInitialized`]; every
/// other event requires an existing state. The returned lifecycle is a fresh
/// value and the input is never mutated, so a fold is referentially
/// transparent and repeatable.
pub fn apply(
    state: Option<&Lifecycle>,
    event: &DomainEvent,
) -> Result<Lifecycle, EventReplayError> {
    match (&event.payload, state) {
        (DomainEventPayload::LifecycleInitialized(init), None) => Ok(seed(event, init)),
        (DomainEventPayload::LifecycleInitialized(_), Some(_)) => {
            Err(EventReplayError::DuplicateSeed)
        }
        (_, None) => Err(EventReplayError::MissingSeed),
        (_, Some(current)) => {
            let mut next = current.clone();
            fold_into(&mut next, event)?;
            Ok(next)
        }
    }
}

/// Fold an ordered domain-event sequence into a [`Lifecycle`].
///
/// The sequence must begin with [`LifecycleInitialized`]. Fails with
/// [`EventReplayError::EmptyStream`] for an empty stream and surfaces any
/// state-machine illegality from the underlying transitions. Folds in place
/// over a single accumulator, so a replay allocates one lifecycle rather
/// than one per event.
pub fn replay<'a, I>(events: I) -> Result<Lifecycle, EventReplayError>
where
    I: IntoIterator<Item = &'a DomainEvent>,
{
    let mut state: Option<Lifecycle> = None;
    for event in events {
        match state.as_mut() {
            None => state = Some(apply(None, event)?),
            Some(current) => fold_into(current, event)?,
        }
    }
    state.ok_or(EventReplayError::EmptyStream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(secs: i64) -> DateTime<Utc> {
        // 2026-05-29T04:00:00Z + secs
        DateTime::from_timestamp(1_780_027_200 + secs, 0).unwrap()
    }

    fn ev(secs: i64, payload: DomainEventPayload) -> DomainEvent {
        DomainEvent::new("run-1", ts(secs), payload)
    }

    fn init(secs: i64) -> DomainEvent {
        ev(
            secs,
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "task-1".into(),
                worker_id: "worker-1".into(),
                artifacts_dir: "/artifacts/run-1".into(),
                ..LifecycleInitialized::default()
            }),
        )
    }

    fn to(secs: i64, target: Status) -> DomainEvent {
        ev(
            secs,
            DomainEventPayload::TransitionedTo(TransitionedTo {
                target,
                error: String::new(),
            }),
        )
    }

    fn to_err(secs: i64, target: Status, error: &str) -> DomainEvent {
        ev(
            secs,
            DomainEventPayload::TransitionedTo(TransitionedTo {
                target,
                error: error.into(),
            }),
        )
    }

    /// init -> ready -> running -> attempt -> validating -> grader -> done.
    fn happy_path() -> Vec<DomainEvent> {
        vec![
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            ev(
                3,
                DomainEventPayload::AttemptStarted(AttemptStarted {
                    number: 1,
                    attempt_run_id: "run-1".into(),
                    started_at: ts(3),
                    agent_context: BTreeMap::from([(
                        "model".to_owned(),
                        "claude-opus-4-8".to_owned(),
                    )]),
                }),
            )
            .with_attempt_number(1),
            to(4, Status::Validating),
            ev(
                5,
                DomainEventPayload::GraderEvaluated(GraderEvaluated {
                    ordinal: 0,
                    grader_type: "command".into(),
                    passed: true,
                    duration_ms: 12,
                    grader_name: Some("pytest".into()),
                    grader_spec: json!({"run": "uv run pytest"}).as_object().unwrap().clone(),
                    payload: json!({"exit_code": 0}).as_object().unwrap().clone(),
                }),
            )
            .with_attempt_number(1),
            ev(
                6,
                DomainEventPayload::AttemptFinalized(AttemptFinalized {
                    number: 1,
                    outcome: Outcome::Succeeded,
                    ended_at: ts(6),
                    agent_output: "all done".into(),
                    error: String::new(),
                }),
            )
            .with_attempt_number(1),
            to(7, Status::Done),
        ]
    }

    fn done_run() -> Lifecycle {
        replay(&[
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            to(3, Status::Validating),
            to(4, Status::Done),
        ])
        .unwrap()
    }

    #[test]
    fn replay_rejects_empty_stream() {
        assert_eq!(replay(&[]).unwrap_err(), EventReplayError::EmptyStream);
    }

    #[test]
    fn replay_requires_leading_initialized() {
        assert_eq!(
            replay(&[to(1, Status::Ready)]).unwrap_err(),
            EventReplayError::MissingSeed
        );
    }

    #[test]
    fn duplicate_initialization_raises() {
        assert_eq!(
            replay(&[init(0), init(1)]).unwrap_err(),
            EventReplayError::DuplicateSeed
        );
        let seeded = replay(&[init(0)]).unwrap();
        assert_eq!(
            apply(Some(&seeded), &init(1)).unwrap_err(),
            EventReplayError::DuplicateSeed
        );
    }

    #[test]
    fn version_equals_domain_event_count() {
        let events = happy_path();
        let lc = replay(&events).unwrap();
        assert_eq!(lc.version as usize, events.len());
    }

    #[test]
    fn replay_is_deterministic_and_matches_stepwise_apply() {
        let events = happy_path();
        let whole = replay(&events).unwrap();
        assert_eq!(whole, replay(&events).unwrap());
        let mut stepwise: Option<Lifecycle> = None;
        for event in &events {
            stepwise = Some(apply(stepwise.as_ref(), event).unwrap());
        }
        assert_eq!(stepwise.unwrap(), whole);
    }

    #[test]
    fn happy_path_folds_to_done_with_finalized_attempt() {
        let lc = replay(&happy_path()).unwrap();
        assert_eq!(lc.status, Status::Done);
        assert_eq!(lc.task_id, "task-1");
        assert_eq!(lc.run_id, "run-1");
        assert_eq!(lc.worker_id, "worker-1");
        assert_eq!(lc.artifacts_dir, "/artifacts/run-1");
        assert_eq!(lc.agent_output, "all done");
        assert_eq!(lc.attempts.len(), 1);
        let only = &lc.attempts[0];
        assert_eq!(only.number, 1);
        assert_eq!(only.run_id, "run-1");
        assert_eq!(only.outcome, Some(Outcome::Succeeded));
        assert_eq!(only.ended_at, Some(ts(6)));
        assert_eq!(only.agent_output, "all done");
        assert_eq!(only.agent_context["model"], "claude-opus-4-8");
        assert_eq!(lc.timestamps[&Status::Done], ts(7));
    }

    #[test]
    fn illegal_transition_surfaces_loudly() {
        let err = replay(&[init(0), to(1, Status::Running)]).unwrap_err();
        assert_eq!(
            err,
            EventReplayError::Transition(LifecycleTransitionError::IllegalEdge {
                from: Status::Pending,
                to: Status::Running,
            })
        );
    }

    #[test]
    fn transition_to_failure_requires_error() {
        let err = replay(&[
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            to(3, Status::Validating),
            to(4, Status::FailedValidation),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            EventReplayError::Transition(LifecycleTransitionError::MissingError {
                to: Status::FailedValidation,
            })
        );
    }

    #[test]
    fn retry_edge_increments_retries_and_clears_error() {
        let events = vec![
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            to(3, Status::Validating),
            to_err(4, Status::FailedValidation, "grader failed"),
            ev(
                5,
                DomainEventPayload::RetryScheduled(RetryScheduled {
                    retries_used: 0,
                    max_retries: 2,
                }),
            ),
            to(6, Status::Ready),
        ];
        let lc = replay(&events).unwrap();
        assert_eq!(lc.status, Status::Ready);
        assert_eq!(lc.retries, 1);
        assert_eq!(lc.error, "");
        assert_eq!(lc.version as usize, events.len());
    }

    #[test]
    fn blocked_snapshot_set_then_cleared_on_ready() {
        let snapshot = r#"[{"type": "command_grader", "name": "build"}]"#;
        let mut events = vec![
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            ev(
                3,
                DomainEventPayload::Blocked(Blocked {
                    requires_json: snapshot.into(),
                }),
            ),
            to(4, Status::Interrupted),
        ];
        let interrupted = replay(&events).unwrap();
        assert_eq!(interrupted.status, Status::Interrupted);
        assert_eq!(interrupted.blocked_requires_json.as_deref(), Some(snapshot));

        events.push(ev(5, DomainEventPayload::Unblocked));
        events.push(to(6, Status::Ready));
        let recovered = replay(&events).unwrap();
        assert_eq!(recovered.status, Status::Ready);
        assert_eq!(recovered.blocked_requires_json, None);
        assert_eq!(recovered.retries, 0);
    }

    #[test]
    fn awaiting_approval_folds_ordinal_and_clears_on_exit() {
        let mut events = vec![
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            to(3, Status::Validating),
            ev(
                4,
                DomainEventPayload::AwaitingApproval(AwaitingApproval {
                    awaiting_ordinal: 2,
                }),
            ),
            to(5, Status::AwaitingApproval),
        ];
        let parked = replay(&events).unwrap();
        assert_eq!(parked.status, Status::AwaitingApproval);
        assert_eq!(parked.awaiting_manual_ordinal, Some(2));
        events.push(to(6, Status::Done));
        let approved = replay(&events).unwrap();
        assert_eq!(approved.awaiting_manual_ordinal, None);
    }

    #[test]
    fn session_recorded_folds_session_id() {
        let lc = replay(&[
            init(0),
            ev(
                1,
                DomainEventPayload::SessionRecorded(SessionRecorded {
                    session_id: "sess-42".into(),
                }),
            ),
        ])
        .unwrap();
        assert_eq!(lc.session_id, "sess-42");
    }

    #[test]
    fn identity_fold_events_only_advance_version() {
        let before = replay(&[init(0), to(1, Status::Ready)]).unwrap();
        let identity_events = [
            DomainEventPayload::Unblocked,
            DomainEventPayload::RetryScheduled(RetryScheduled {
                retries_used: 0,
                max_retries: 1,
            }),
            DomainEventPayload::GraderEvaluated(GraderEvaluated {
                ordinal: 0,
                grader_type: "command".into(),
                passed: true,
                duration_ms: 1,
                grader_name: None,
                grader_spec: Map::new(),
                payload: Map::new(),
            }),
            DomainEventPayload::CommandApplied(CommandApplied {
                command_kind: "say".into(),
                command_payload: json!({"text": "focus on graders"})
                    .as_object()
                    .unwrap()
                    .clone(),
                command_id: Some(7),
            }),
            DomainEventPayload::LandingParked(LandingParked {
                park_kind: PARK_KIND_UNCOMMITTED_WORK.into(),
                detail: "DONE with an uncommitted tree on flywheel/01/t1".into(),
                ..LandingParked::default()
            }),
            DomainEventPayload::Landed(Landed {
                strategy: LANDING_STRATEGY_MERGE.into(),
                landed_ref: "a".repeat(40),
                ..Landed::default()
            }),
            DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                outcome: "fail".into(),
                reason: "held-out gate FAILED".into(),
                receipts: vec![GateGraderReceipt {
                    grader_name: Some("oracle".into()),
                    passed: false,
                    output_excerpt: "boom\n".into(),
                }],
            }),
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: REDRIVE_RESULT_LANDED.into(),
                park_kind: PARK_KIND_STANDING_VERIFY.into(),
            }),
            DomainEventPayload::LandingRouted(LandingRouted {
                winning_tier: 1,
                strategy: "phase".into(),
                per_file_tiers: BTreeMap::from([("src/x.py".to_owned(), 1)]),
            }),
        ];
        for payload in identity_events {
            assert!(payload.kind().is_identity_fold(), "{}", payload.kind());
            let after = apply(Some(&before), &ev(2, payload)).unwrap();
            assert_eq!(after.version, before.version + 1);
            assert_eq!(after.status, before.status);
            assert_eq!(after.retries, before.retries);
            assert_eq!(after.error, before.error);
            assert_eq!(after.attempts, before.attempts);
            assert_eq!(after.blocked_requires_json, before.blocked_requires_json);
            assert_eq!(after.agent_output, before.agent_output);
            assert_eq!(after.timestamps, before.timestamps);
        }
    }

    #[test]
    fn landing_witnesses_leave_terminal_done_in_place() {
        let done = done_run();
        for payload in [
            DomainEventPayload::LandingParked(LandingParked {
                park_kind: PARK_KIND_DIVERGENT_BASE.into(),
                detail: "base advanced; rebase conflicted".into(),
                ..LandingParked::default()
            }),
            DomainEventPayload::Landed(Landed {
                strategy: LANDING_STRATEGY_PR.into(),
                landed_ref: "https://example.test/pr/7".into(),
                ..Landed::default()
            }),
            DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                outcome: "no_gate".into(),
                ..HeldOutGateEvaluated::default()
            }),
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: REDRIVE_RESULT_ROUTED.into(),
                park_kind: String::new(),
            }),
            DomainEventPayload::LandingRouted(LandingRouted {
                winning_tier: 2,
                strategy: "pr".into(),
                per_file_tiers: BTreeMap::from([
                    ("docs/a.md".to_owned(), 0),
                    (".github/ci.yml".to_owned(), 2),
                ]),
            }),
        ] {
            let after = apply(Some(&done), &ev(5, payload)).unwrap();
            assert_eq!(after.status, Status::Done);
            assert_eq!(after.version, done.version + 1);
        }
    }

    #[test]
    fn apply_does_not_mutate_input() {
        let before = replay(&[init(0), to(1, Status::Ready)]).unwrap();
        let snapshot = before.clone();
        let after = apply(Some(&before), &to(2, Status::Running)).unwrap();
        assert_eq!(before, snapshot);
        assert_eq!(after.status, Status::Running);
        assert_eq!(after.version, before.version + 1);
    }

    #[test]
    fn finalize_unknown_attempt_raises() {
        let err = replay(&[
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            ev(
                3,
                DomainEventPayload::AttemptFinalized(AttemptFinalized {
                    number: 99,
                    outcome: Outcome::Succeeded,
                    ended_at: ts(3),
                    agent_output: String::new(),
                    error: String::new(),
                }),
            ),
        ])
        .unwrap_err();
        assert_eq!(err, EventReplayError::UnknownAttempt { number: 99 });
    }

    #[test]
    fn duplicate_attempt_start_raises() {
        let start = |secs| {
            ev(
                secs,
                DomainEventPayload::AttemptStarted(AttemptStarted {
                    number: 1,
                    attempt_run_id: "run-1".into(),
                    started_at: ts(secs),
                    agent_context: BTreeMap::new(),
                }),
            )
        };
        let err = replay(&[
            init(0),
            to(1, Status::Ready),
            to(2, Status::Running),
            start(3),
            start(4),
        ])
        .unwrap_err();
        assert_eq!(err, EventReplayError::DuplicateAttempt { number: 1 });
    }

    #[test]
    fn reducer_matches_imperative_transition_to() {
        let mut imperative = Lifecycle::new("task-1").with_run_id("run-1");
        imperative.worker_id = "worker-1".into();
        imperative.artifacts_dir = "/artifacts/run-1".into();
        imperative
            .transition_to(Status::Ready, "", Some(ts(1)))
            .unwrap();
        imperative
            .transition_to(Status::Running, "", Some(ts(2)))
            .unwrap();

        let folded = replay(&[init(0), to(1, Status::Ready), to(2, Status::Running)]).unwrap();
        assert_eq!(folded, imperative);
    }

    #[test]
    fn kind_strings_are_stable_and_round_trip() {
        for kind in DomainEventKind::ALL {
            assert_eq!(kind.as_str().parse::<DomainEventKind>().unwrap(), kind);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{kind}\""));
        }
        assert_eq!(DomainEventKind::LandingParked.as_str(), "landing_parked");
        assert_eq!(DomainEventKind::Landed.as_str(), "landed");
        assert_eq!(
            DomainEventKind::HeldOutGateEvaluated.as_str(),
            "held_out_gate_evaluated"
        );
        assert_eq!(
            DomainEventKind::LandingRedriven.as_str(),
            "landing_redriven"
        );
        assert_eq!(DomainEventKind::LandingRouted.as_str(), "landing_routed");
        assert!("nope".parse::<DomainEventKind>().is_err());
        assert_eq!(
            ev(0, DomainEventPayload::Unblocked).kind(),
            DomainEventKind::Unblocked
        );
    }

    #[test]
    fn vocabularies_are_closed_and_strands_are_a_subset_of_park_kinds() {
        assert_eq!(LANDING_PARK_KINDS.len(), 8);
        for kind in LANDING_STRAND_KINDS {
            assert!(LANDING_PARK_KINDS.contains(&kind), "{kind}");
        }
        assert!(!LANDING_STRAND_KINDS.contains(&PARK_KIND_MERGE_CONFLICT));
        assert_eq!(LANDING_STRATEGIES, ["merge", "pr"]);
        assert_eq!(LANDING_RUNGS.len(), 4);
        assert_eq!(LANDING_REDRIVE_RESULTS, ["landed", "re-parked", "routed"]);
        assert_eq!(GATE_EXCERPT_MAX_BYTES, 8192);
    }
}
