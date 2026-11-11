//! The telemetry event: one observation of what an agent session did.
//!
//! Events are observations, never derived state: the work tree, the plan's
//! progress, and the time spent blocked are folded from them by
//! [`super::reduce`], the same way on the hub that saw them and on a
//! collector they were forwarded to.

use serde::{Deserialize, Serialize};

/// One observation, as stored and as forwarded. `(host, session, seq)`
/// names it: forwarding is at-least-once and a repeat is ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The hub that observed it (its `hub-id`).
    pub host: String,
    /// The hub session it belongs to.
    pub session: String,
    /// Its place in the session's log, from 1, assigned where observed.
    pub seq: u64,
    /// When it happened (unix milliseconds).
    pub ts: i64,
    /// The subagent it happened in (`None`: the session's main agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The turn or subagent work item it belongs to (see
    /// [`super::reduce::WorkItem`]), assigned by the reducer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    /// The declared plan item that was active when it happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(flatten)]
    pub what: What,
}

/// An observation before the session's log numbers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub ts: i64,
    pub agent: Option<String>,
    pub what: What,
}

impl Draft {
    pub fn new(ts: i64, what: What) -> Self {
        Self {
            ts,
            agent: None,
            what,
        }
    }

    pub fn by(mut self, agent: Option<String>) -> Self {
        self.agent = agent;
        self
    }
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error,
    /// The user interrupted it.
    Interrupted,
    /// A permission rule or the user refused it.
    Denied,
}

/// What a session waits on a person for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// A permission prompt.
    Permission,
    /// A question the agent asked.
    Question,
}

/// What an agent asks the operator for when it ends its turn on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// A reply in words.
    Answer,
    /// A choice between named options.
    Decide,
    /// A command only the operator can run (`sudo`, another machine),
    /// and its output back.
    Run,
    /// Continue the work in a new session, from the prompt it wrote.
    Handoff,
}

impl AskKind {
    pub fn parse(word: &str) -> Self {
        match word.to_ascii_lowercase().as_str() {
            "decide" | "decision" | "choose" => Self::Decide,
            "run" | "command" => Self::Run,
            "handoff" => Self::Handoff,
            _ => Self::Answer,
        }
    }
}

/// How a session is tied to others on its hub (their hub session ids).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLinks {
    /// The session whose work this one continues (a handoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// The agent session that opened this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_by: Option<String>,
}

impl SessionLinks {
    pub fn is_empty(&self) -> bool {
        self.continues.is_none() && self.opened_by.is_none()
    }
}

/// A plan item's status, in the agent's words normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    #[default]
    Pending,
    Active,
    Done,
    Cancelled,
}

impl ItemStatus {
    /// From an agent's own word (`in_progress`, `completed`, ...).
    pub fn parse(word: &str) -> Self {
        match word.to_ascii_lowercase().as_str() {
            "in_progress" | "in-progress" | "active" | "doing" | "started" | "working" => {
                Self::Active
            }
            "completed" | "complete" | "done" | "finished" => Self::Done,
            "cancelled" | "canceled" | "deleted" | "skipped" | "dropped" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Active)
    }
}

/// One item of a plan as the agent declared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub title: String,
    #[serde(default)]
    pub status: ItemStatus,
}

/// What was observed. The names are shared with `flywheel-agents`'
/// normalized events where the two overlap (no crate dependency).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body")]
pub enum What {
    #[serde(rename = "session.started")]
    SessionStarted {
        /// `claude` or `codex`.
        harness: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The session's name on its hub, and the directory it runs in
        /// (what a collector shows for a session it does not own).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        /// The sessions it is tied to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        links: Option<SessionLinks>,
    },
    /// A new session took over this one's work (a handoff accepted).
    #[serde(rename = "session.continued")]
    Continued { successor: String },
    #[serde(rename = "session.finished")]
    SessionFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A prompt was submitted: the agent starts working on it.
    #[serde(rename = "turn.started")]
    TurnStarted { title: String },
    #[serde(rename = "turn.finished")]
    TurnFinished {
        /// The end of the turn's closing message, scrubbed and capped
        /// (see [`super::summary::ending`]): what the agent left the
        /// operator with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ending: Option<String>,
    },
    /// The turn ended on an error (an API failure, a rate limit).
    #[serde(rename = "turn.failed")]
    TurnFailed {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "tool.started")]
    ToolStarted {
        /// The agent's id for the call: pairs it with its finish.
        id: String,
        name: String,
        /// What it acts on, scrubbed and capped (see [`super::summary`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// The subagent type a subagent-spawning call asks for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent: Option<String>,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        id: String,
        name: String,
        outcome: Outcome,
        /// The agent's own measure (permission prompts excluded); else
        /// the reducer measures start to finish.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Tokens a subagent-spawning call's subagent used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
    },
    /// The agent declared what it needs from the operator (the hub's
    /// `ask_operator` tool); it holds once the turn ends, until the next
    /// prompt.
    #[serde(rename = "ask")]
    Asked {
        kind: AskKind,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        /// The successor's first prompt (`handoff`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// The session started waiting on a person.
    #[serde(rename = "wait.started")]
    WaitStarted {
        reason: WaitReason,
        /// The tool the prompt is about.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    #[serde(rename = "subagent.started")]
    SubagentStarted {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    #[serde(rename = "subagent.finished")]
    SubagentFinished { id: String },
    /// The agent declared (or rewrote) its whole plan.
    #[serde(rename = "plan.set")]
    PlanSet {
        /// Where it was declared: `todo`, `codex`, `declared` (the hub's
        /// own progress tool).
        source: String,
        items: Vec<PlanStep>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// One plan item was created or changed (id-based task tools).
    #[serde(rename = "plan.item")]
    PlanItem {
        source: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },
    /// How full the context window is after the last response.
    #[serde(rename = "context.usage")]
    ContextUsage {
        used: u64,
        window: u64,
        /// False when the window size is assumed from the model's name.
        #[serde(default = "yes")]
        exact: bool,
    },
    /// The session's token and cost totals so far.
    #[serde(rename = "usage")]
    Usage {
        input: u64,
        output: u64,
        #[serde(default)]
        cache_read: u64,
        #[serde(default)]
        cache_write: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// The account's rate-limit gauge for one window.
    #[serde(rename = "rate.limit")]
    RateLimit {
        /// `five_hour`, `seven_day`, or the agent's own name for it.
        window: String,
        used_pct: f64,
        /// Unix seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<i64>,
    },
    /// A request was refused for the rate limit.
    #[serde(rename = "rate.limited")]
    RateLimited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The conversation was compacted (`phase`: `pre` or `post`).
    #[serde(rename = "compact")]
    Compact {
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
}

fn yes() -> bool {
    true
}

impl What {
    /// The event's kind as stored (`tool.started`, ...).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session.started",
            Self::Continued { .. } => "session.continued",
            Self::SessionFinished { .. } => "session.finished",
            Self::TurnStarted { .. } => "turn.started",
            Self::TurnFinished { .. } => "turn.finished",
            Self::TurnFailed { .. } => "turn.failed",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolFinished { .. } => "tool.finished",
            Self::Asked { .. } => "ask",
            Self::WaitStarted { .. } => "wait.started",
            Self::SubagentStarted { .. } => "subagent.started",
            Self::SubagentFinished { .. } => "subagent.finished",
            Self::PlanSet { .. } => "plan.set",
            Self::PlanItem { .. } => "plan.item",
            Self::ContextUsage { .. } => "context.usage",
            Self::Usage { .. } => "usage",
            Self::RateLimit { .. } => "rate.limit",
            Self::RateLimited { .. } => "rate.limited",
            Self::Compact { .. } => "compact",
        }
    }

    /// The body alone, as stored beside the kind.
    pub fn body(&self) -> serde_json::Value {
        serde_json::to_value(self)
            .ok()
            .and_then(|mut v| v.get_mut("body").map(serde_json::Value::take))
            .unwrap_or(serde_json::Value::Null)
    }

    /// Back from a stored kind and body.
    pub fn from_parts(kind: &str, body: serde_json::Value) -> Option<Self> {
        serde_json::from_value(serde_json::json!({ "kind": kind, "body": body })).ok()
    }
}

/// Every event kind (documented in `docs/hub/api.md`).
pub const KINDS: &[&str] = &[
    "session.started",
    "session.continued",
    "session.finished",
    "turn.started",
    "turn.finished",
    "turn.failed",
    "tool.started",
    "tool.finished",
    "ask",
    "wait.started",
    "subagent.started",
    "subagent.finished",
    "plan.set",
    "plan.item",
    "context.usage",
    "usage",
    "rate.limit",
    "rate.limited",
    "compact",
];
