//! Folding a session's events into what it is doing now.
//!
//! [`SessionState::apply`] is the one reducer: it runs where an event was
//! observed (and there names the work item the event belongs to) and again
//! on a collector the event was forwarded to, so both hold the same state.
//! The state is small and bounded ([`MAX_WORK`] work items, [`MAX_PLAN`]
//! plan items a plan) and is saved whole, so a restarted hub resumes
//! without replaying the log.
//!
//! The **work tree** is what the session is working on, seen two ways:
//!
//! * observed: each prompt is a turn; each subagent a turn spawned hangs
//!   under it; every tool call counts on the item it ran in. This needs
//!   nothing from the agent.
//! * declared: the agent's own plan (its todo list, its task tools, Codex's
//!   plan, the hub's progress tool), per agent, diffed on every rewrite so
//!   a changed plan shows as a revision. A declared status is the agent's
//!   claim: it is shown beside the observed calls, never instead of them.

use super::event::{Event, ItemStatus, Outcome, PlanStep, WaitReason, What};
use serde::{Deserialize, Serialize};

/// Work items (turns and subagents) kept per session; the oldest finished
/// ones go first. Older turns stay in the event log.
pub const MAX_WORK: usize = 240;
/// Items kept of one plan.
pub const MAX_PLAN: usize = 200;
/// Tool calls tracked as running at once (a lost finish must not grow it).
const MAX_OPEN: usize = 64;
/// The tools an agent declares its plan with.
const PLAN_TOOLS: &[&str] = &[
    super::claude::PROGRESS_TOOL,
    "TodoWrite",
    "TaskCreate",
    "TaskUpdate",
    "update_plan",
];
/// Tools that spawn a subagent.
const SPAWNING: &[&str] = &["Agent", "Task"];

/// What a work item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSource {
    /// One prompt and the work it started.
    Turn,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Active,
    Done,
    Failed,
    /// Cut short: a new prompt arrived, or the session ended.
    Cancelled,
}

/// A turn or a subagent: one node of the observed work tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub source: WorkSource,
    pub title: String,
    pub status: WorkStatus,
    /// A subagent's id, once the agent reported it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// A subagent's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    /// Tool calls started in it.
    #[serde(default)]
    pub tools: u32,
    #[serde(default)]
    pub tool_errors: u32,
    /// Time its finished calls took.
    #[serde(default)]
    pub tool_ms: u64,
    /// Tokens a subagent used, when the agent said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// The call it is in now (`name: summary`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doing: Option<String>,
}

/// One item of a declared plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub id: String,
    pub title: String,
    pub status: ItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    /// Tool calls made while it was the active item.
    #[serde(default)]
    pub tools: u32,
}

/// An agent's declared plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Whose it is (`None`: the main agent's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub source: String,
    /// Bumped whenever the items themselves change (added, dropped,
    /// reordered), not when only their statuses do.
    pub revision: u32,
    /// What the last revision added and dropped.
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub dropped: u32,
    pub updated: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<PlanEntry>,
}

impl Plan {
    fn progress(&self) -> PlanProgress {
        let counted = || {
            self.items
                .iter()
                .filter(|i| i.status != ItemStatus::Cancelled)
        };
        let current = counted()
            .find(|i| i.status == ItemStatus::Active)
            .or_else(|| counted().find(|i| i.status == ItemStatus::Pending));
        PlanProgress {
            source: self.source.clone(),
            revision: self.revision,
            done: counted().filter(|i| i.status == ItemStatus::Done).count() as u32,
            total: counted().count() as u32,
            current: current.map(|i| i.title.clone()),
        }
    }

    fn active(&self) -> Option<&PlanEntry> {
        self.items.iter().find(|i| i.status == ItemStatus::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OpenTool {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Wait {
    reason: WaitReason,
    since: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
}

/// How full the context window is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub used: u64,
    pub window: u64,
    pub pct: f64,
    pub exact: bool,
    pub at: i64,
}

/// Token and cost totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub at: i64,
}

/// One rate-limit window's gauge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateWindow {
    pub window: String,
    pub used_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    pub at: i64,
}

/// Everything known about one session, folded from its events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub host: String,
    pub session: String,
    /// The last event applied.
    pub seq: u64,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    conversation: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    started: Option<i64>,
    #[serde(default)]
    pub last_ts: i64,
    #[serde(default)]
    ended: bool,
    #[serde(default)]
    turns: u32,
    /// The running turn's work item.
    #[serde(default)]
    turn: Option<String>,
    #[serde(default)]
    last_turn_failed: bool,
    /// Since when no turn runs (the agent awaits the next prompt).
    #[serde(default)]
    idle_since: Option<i64>,
    #[serde(default)]
    awaiting_ms: u64,
    #[serde(default)]
    wait: Option<Wait>,
    #[serde(default)]
    blocked_ms: u64,
    #[serde(default)]
    waits: u32,
    #[serde(default)]
    open: Vec<OpenTool>,
    #[serde(default)]
    tools: u64,
    #[serde(default)]
    tool_errors: u64,
    #[serde(default)]
    denied: u64,
    #[serde(default)]
    errors: u32,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    work: Vec<WorkItem>,
    #[serde(default)]
    plans: Vec<Plan>,
    #[serde(default)]
    context: Option<Context>,
    #[serde(default)]
    usage: Option<Totals>,
    #[serde(default)]
    rate: Vec<RateWindow>,
    #[serde(default)]
    limited_at: Option<i64>,
    #[serde(default)]
    compactions: u32,
    #[serde(default)]
    subagents: u32,
    /// Numbers plan items declared without ids.
    #[serde(default)]
    next_item: u32,
}

/// The running tool call a snapshot names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolNow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub since: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanProgress {
    pub source: String,
    pub revision: u32,
    pub done: u32,
    pub total: u32,
    /// The item being worked on (else the next pending one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// One session on the fleet board: state, progress, context, cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub host: String,
    pub session: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The session's name on its hub, and its directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// `working`, `blocked`, `done` (awaiting the next prompt), `failed`
    /// (the last turn ended on an error), `idle` (no turn yet), `ended`.
    pub status: String,
    /// Since when it has been in that status.
    pub since: i64,
    /// What it is blocked on: `permission` or `question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<WaitReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    pub last_ts: i64,
    pub turns: u32,
    /// The running turn (else the last one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started: Option<i64>,
    /// Tool calls in that turn (its subagents' included).
    pub turn_tools: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolNow>,
    pub open_tools: u32,
    pub tools: u64,
    pub tool_errors: u64,
    pub denied: u64,
    /// The main agent's declared plan, when it declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanProgress>,
    /// Subagents of the running (else last) turn: running, and all.
    pub subagents_active: u32,
    pub subagents_total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Totals>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate: Vec<RateWindow>,
    /// When a request was last refused for the rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited_at: Option<i64>,
    pub errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Time spent blocked on a person, closed waits only; a running wait
    /// began at `since` (`status` is `blocked`).
    pub blocked_ms: u64,
    pub waits: u32,
    /// Time spent finished, awaiting the next prompt (closed spans).
    pub awaiting_ms: u64,
    pub compactions: u32,
}

/// A session in full: its snapshot, work tree, and plans.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Detail {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub work: Vec<WorkItem>,
    pub plans: Vec<Plan>,
}

impl SessionState {
    pub fn new(host: &str, session: &str) -> Self {
        Self {
            host: host.to_owned(),
            session: session.to_owned(),
            ..Self::default()
        }
    }

    /// Whether a gauge reading says nothing new (it is not recorded).
    pub fn unchanged(&self, what: &What) -> bool {
        match what {
            What::ContextUsage { used, window, .. } => self
                .context
                .as_ref()
                .is_some_and(|c| c.used == *used && c.window == *window),
            What::Usage {
                input,
                output,
                cache_read,
                cache_write,
                cost_usd,
            } => self.usage.as_ref().is_some_and(|u| {
                u.input == *input
                    && u.output == *output
                    && u.cache_read == *cache_read
                    && u.cache_write == *cache_write
                    && u.cost_usd == *cost_usd
            }),
            What::RateLimit {
                window,
                used_pct,
                resets_at,
            } => self.rate.iter().any(|r| {
                r.window == *window && r.used_pct == *used_pct && r.resets_at == *resets_at
            }),
            What::SessionStarted {
                conversation,
                model,
                label,
                dir,
                ..
            } => {
                !self.ended
                    && self.harness.is_some()
                    && (conversation.is_none() || *conversation == self.conversation)
                    && (model.is_none() || *model == self.model)
                    && (label.is_none() || *label == self.label)
                    && (dir.is_none() || *dir == self.dir)
            }
            What::SessionFinished { .. } => self.ended,
            // A second notice of the prompt already waited on.
            What::WaitStarted { .. } => self.wait.is_some(),
            _ => false,
        }
    }

    /// Fold one event in, naming the work and plan items it belongs to.
    pub fn apply(&mut self, event: &mut Event) {
        let ts = event.ts;
        self.seq = self.seq.max(event.seq);
        self.last_ts = self.last_ts.max(ts);
        self.started.get_or_insert(ts);
        let agent = event.agent.clone();
        // A finish that names no call id the reducer knows (or no tool:
        // Codex's outputs) is paired with the oldest running call of its
        // name, else of its agent.
        if let What::ToolFinished { id, name, .. } = &mut event.what {
            let known = self.open.iter().find(|o| o.id == *id);
            let paired = known.or_else(|| {
                self.open
                    .iter()
                    .find(|o| o.agent == agent && (name.is_empty() || o.name == *name))
                    .filter(|_| name.is_empty() || id.ends_with('?'))
            });
            if let Some(open) = paired {
                id.clone_from(&open.id);
                name.clone_from(&open.name);
            } else if name.is_empty() {
                "tool".clone_into(name);
            }
        }
        match &event.what {
            What::SessionStarted {
                harness,
                conversation,
                model,
                label,
                dir,
            } => {
                self.harness = Some(harness.clone());
                if label.is_some() {
                    self.label.clone_from(label);
                }
                if dir.is_some() {
                    self.dir.clone_from(dir);
                }
                if conversation.is_some() {
                    self.conversation = conversation.clone();
                }
                if model.is_some() {
                    self.model = model.clone();
                }
                self.ended = false;
            }
            What::SessionFinished { .. } => {
                self.close_wait(ts);
                self.finish_turn(ts, WorkStatus::Cancelled);
                self.finish_subagents(ts);
                self.open.clear();
                self.ended = true;
            }
            What::TurnStarted { title } => {
                self.close_wait(ts);
                self.ended = false;
                // The prompt's text arriving after its turn began (Codex
                // writes the two apart) titles that turn.
                let untitled = self.turn.clone().filter(|id| {
                    self.item(id)
                        .is_some_and(|t| t.title.is_empty() && t.tools == 0)
                });
                if let Some(id) = untitled {
                    if let Some(turn) = self.item_mut(&id) {
                        turn.title.clone_from(title);
                    }
                    event.work = Some(id);
                    return;
                }
                // A prompt while a turn runs: the agent was interrupted.
                self.finish_turn(ts, WorkStatus::Cancelled);
                event.work = Some(self.start_turn(ts, title));
            }
            What::TurnFinished {} => {
                self.close_wait(ts);
                event.work = self.turn.clone();
                self.last_turn_failed = false;
                self.finish_turn(ts, WorkStatus::Done);
            }
            What::TurnFailed { error, detail } => {
                self.close_wait(ts);
                event.work = self.turn.clone();
                self.errors += 1;
                self.last_error = Some(match detail {
                    Some(detail) => format!("{error}: {detail}"),
                    None => error.clone(),
                });
                self.finish_turn(ts, WorkStatus::Failed);
                self.last_turn_failed = true;
            }
            What::ToolStarted {
                id,
                name,
                summary,
                subagent,
            } => {
                // A call no prompt was waiting on a tool for: it answered.
                if self.wait.as_ref().is_some_and(|w| w.tool.is_none()) {
                    self.close_wait(ts);
                }
                let work = self.work_of(agent.as_deref(), ts);
                self.tools += 1;
                let doing = match summary {
                    Some(summary) => format!("{name}: {summary}"),
                    None => name.clone(),
                };
                if let Some(item) = self.item_mut(&work) {
                    item.tools += 1;
                    item.doing = Some(doing);
                }
                // Declaring the plan is not work on one of its items.
                if !PLAN_TOOLS.contains(&name.as_str()) {
                    event.item = self.count_on_plan(agent.as_deref());
                }
                if SPAWNING.contains(&name.as_str()) {
                    self.subagents += 1;
                    self.push_work(WorkItem {
                        id: format!("a:{id}"),
                        parent: Some(work.clone()),
                        source: WorkSource::Subagent,
                        title: summary.clone().unwrap_or_else(|| name.clone()),
                        status: WorkStatus::Active,
                        agent: None,
                        kind: subagent.clone(),
                        started: ts,
                        finished: None,
                        tools: 0,
                        tool_errors: 0,
                        tool_ms: 0,
                        tokens: None,
                        doing: None,
                    });
                }
                self.open.retain(|o| o.id != *id);
                if self.open.len() >= MAX_OPEN {
                    self.open.remove(0);
                }
                self.open.push(OpenTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: summary.clone(),
                    ts,
                    agent,
                    work: Some(work.clone()),
                });
                event.work = Some(work);
            }
            What::ToolFinished {
                id,
                name,
                outcome,
                duration_ms,
                error,
                tokens,
            } => {
                let open = self
                    .open
                    .iter()
                    .position(|o| o.id == *id)
                    .map(|at| self.open.remove(at));
                let took = duration_ms.unwrap_or_else(|| {
                    open.as_ref()
                        .map_or(0, |o| u64::try_from(ts - o.ts).unwrap_or(0))
                });
                // The wait this call was held up by ended when it ran.
                if self
                    .wait
                    .as_ref()
                    .is_some_and(|w| w.tool.as_deref().is_none_or(|t| t == name))
                {
                    let ran = i64::try_from(took).unwrap_or(0);
                    self.close_wait(if duration_ms.is_some() { ts - ran } else { ts });
                }
                let work = match open.and_then(|o| o.work) {
                    Some(work) => work,
                    None => self.work_of(agent.as_deref(), ts),
                };
                let failed = *outcome == Outcome::Error;
                match outcome {
                    Outcome::Error => {
                        self.tool_errors += 1;
                        if let Some(error) = error {
                            self.last_error = Some(format!("{name}: {error}"));
                        }
                    }
                    Outcome::Denied => self.denied += 1,
                    Outcome::Ok | Outcome::Interrupted => {}
                }
                if let Some(item) = self.item_mut(&work) {
                    item.tool_ms += took;
                    item.tool_errors += u32::from(failed);
                    item.doing = None;
                }
                // The subagent this call spawned: a foreground one is over
                // with its call; one the agent reported started (it may
                // run in the background) ends when the agent says so.
                let spawned = format!("a:{id}");
                if let Some(item) = self.item_mut(&spawned) {
                    if tokens.is_some() {
                        item.tokens = *tokens;
                    }
                    if item.agent.is_none() && item.status == WorkStatus::Active {
                        item.status = if *outcome == Outcome::Ok {
                            WorkStatus::Done
                        } else {
                            WorkStatus::Failed
                        };
                        item.finished = Some(ts);
                    }
                }
                event.work = Some(work);
            }
            What::WaitStarted { reason, tool } => {
                event.work = self.turn.clone();
                if self.wait.is_none() {
                    self.waits += 1;
                    self.wait = Some(Wait {
                        reason: *reason,
                        since: ts,
                        tool: tool.clone(),
                    });
                }
            }
            What::SubagentStarted { id, kind } => {
                // The oldest spawned item of its type nobody claimed yet.
                let unclaimed = |item: &&mut WorkItem| {
                    item.source == WorkSource::Subagent
                        && item.agent.is_none()
                        && item.status == WorkStatus::Active
                };
                let claimed = self
                    .work
                    .iter_mut()
                    .filter(unclaimed)
                    .find(|item| kind.is_none() || item.kind == *kind)
                    .map(|item| {
                        item.agent = Some(id.clone());
                        item.id.clone()
                    });
                event.work = Some(match claimed {
                    Some(work) => work,
                    None => {
                        let parent = self.work_of(None, ts);
                        self.subagents += 1;
                        let work = format!("a:{id}");
                        self.push_work(WorkItem {
                            id: work.clone(),
                            parent: Some(parent),
                            source: WorkSource::Subagent,
                            title: kind.clone().unwrap_or_else(|| "subagent".to_owned()),
                            status: WorkStatus::Active,
                            agent: Some(id.clone()),
                            kind: kind.clone(),
                            started: ts,
                            finished: None,
                            tools: 0,
                            tool_errors: 0,
                            tool_ms: 0,
                            tokens: None,
                            doing: None,
                        });
                        work
                    }
                });
            }
            What::SubagentFinished { id } => {
                if let Some(item) = self
                    .work
                    .iter_mut()
                    .find(|item| item.agent.as_deref() == Some(id))
                {
                    if item.status == WorkStatus::Active {
                        item.status = WorkStatus::Done;
                        item.finished = Some(ts);
                    }
                    item.doing = None;
                    event.work = Some(item.id.clone());
                }
                self.open.retain(|o| o.agent.as_deref() != Some(id));
            }
            What::PlanSet {
                source,
                items,
                note,
            } => {
                event.work = self.existing_work_of(agent.as_deref());
                self.set_plan(agent, source, items, note.clone(), ts);
            }
            What::PlanItem {
                source,
                id,
                title,
                status,
            } => {
                event.work = self.existing_work_of(agent.as_deref());
                self.set_plan_item(agent, source, id, title.as_deref(), *status, ts);
            }
            What::ContextUsage {
                used,
                window,
                exact,
            } => {
                let pct = if *window == 0 {
                    0.0
                } else {
                    ((*used as f64 / *window as f64) * 1000.0).round() / 10.0
                };
                self.context = Some(Context {
                    used: *used,
                    window: *window,
                    pct,
                    exact: *exact,
                    at: ts,
                });
            }
            What::Usage {
                input,
                output,
                cache_read,
                cache_write,
                cost_usd,
            } => {
                self.usage = Some(Totals {
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    cost_usd: *cost_usd,
                    at: ts,
                });
            }
            What::RateLimit {
                window,
                used_pct,
                resets_at,
            } => {
                let reading = RateWindow {
                    window: window.clone(),
                    used_pct: *used_pct,
                    resets_at: *resets_at,
                    at: ts,
                };
                match self.rate.iter().position(|r| r.window == *window) {
                    Some(at) => self.rate[at] = reading,
                    None if self.rate.len() < 8 => self.rate.push(reading),
                    None => {}
                }
            }
            What::RateLimited { .. } => {
                event.work = self.turn.clone();
                self.limited_at = Some(ts);
            }
            What::Compact { phase, .. } => {
                event.work = self.turn.clone();
                if phase == "pre" {
                    self.compactions += 1;
                }
            }
        }
    }

    fn close_wait(&mut self, at: i64) {
        if let Some(wait) = self.wait.take() {
            self.blocked_ms += u64::try_from(at.max(wait.since) - wait.since).unwrap_or(0);
        }
    }

    fn start_turn(&mut self, ts: i64, title: &str) -> String {
        if let Some(since) = self.idle_since.take() {
            self.awaiting_ms += u64::try_from(ts - since).unwrap_or(0);
        }
        self.turns += 1;
        let id = format!("t{}", self.turns);
        self.push_work(WorkItem {
            id: id.clone(),
            parent: None,
            source: WorkSource::Turn,
            title: title.to_owned(),
            status: WorkStatus::Active,
            agent: None,
            kind: None,
            started: ts,
            finished: None,
            tools: 0,
            tool_errors: 0,
            tool_ms: 0,
            tokens: None,
            doing: None,
        });
        self.turn = Some(id.clone());
        id
    }

    fn finish_turn(&mut self, ts: i64, status: WorkStatus) {
        let Some(id) = self.turn.take() else {
            return;
        };
        if let Some(item) = self.item_mut(&id) {
            item.status = status;
            item.finished = Some(ts);
            item.doing = None;
        }
        // The main agent's calls are over with its turn; a background
        // subagent's go on.
        self.open.retain(|o| o.agent.is_some());
        self.idle_since = Some(ts);
    }

    fn finish_subagents(&mut self, ts: i64) {
        for item in &mut self.work {
            if item.status == WorkStatus::Active {
                item.status = WorkStatus::Cancelled;
                item.finished = Some(ts);
                item.doing = None;
            }
        }
    }

    /// The work item `agent`'s events belong to, if one is there.
    fn existing_work_of(&self, agent: Option<&str>) -> Option<String> {
        agent
            .and_then(|agent| {
                self.work
                    .iter()
                    .rev()
                    .find(|item| item.agent.as_deref() == Some(agent))
                    .map(|item| item.id.clone())
            })
            .or_else(|| self.turn.clone())
    }

    /// The work item `agent`'s events belong to: its subagent item, else
    /// the running turn (one is begun when events arrive without a prompt:
    /// the hub started watching mid-turn).
    fn work_of(&mut self, agent: Option<&str>, ts: i64) -> String {
        match self.existing_work_of(agent) {
            Some(work) => work,
            None => self.start_turn(ts, ""),
        }
    }

    fn item_mut(&mut self, id: &str) -> Option<&mut WorkItem> {
        self.work.iter_mut().rev().find(|item| item.id == id)
    }

    fn push_work(&mut self, item: WorkItem) {
        self.work.push(item);
        while self.work.len() > MAX_WORK {
            let Some(at) = self
                .work
                .iter()
                .position(|item| item.status != WorkStatus::Active)
            else {
                break;
            };
            self.work.remove(at);
        }
    }

    fn plan_mut(&mut self, owner: Option<String>, source: &str, ts: i64) -> &mut Plan {
        let at = match self.plans.iter().position(|p| p.owner == owner) {
            Some(at) => at,
            None => {
                // One plan per agent; a bound on subagents that declare.
                if self.plans.len() >= 32 {
                    self.plans
                        .remove(usize::from(self.plans[0].owner.is_none()));
                }
                self.plans.push(Plan {
                    owner,
                    source: source.to_owned(),
                    revision: 0,
                    added: 0,
                    dropped: 0,
                    updated: ts,
                    note: None,
                    items: Vec::new(),
                });
                self.plans.len() - 1
            }
        };
        let plan = &mut self.plans[at];
        plan.source = source.to_owned();
        plan.updated = ts;
        plan
    }

    /// The plan item `agent` is on now; a call counts on it.
    fn count_on_plan(&mut self, agent: Option<&str>) -> Option<String> {
        let plan = self
            .plans
            .iter_mut()
            .find(|p| p.owner.as_deref() == agent)?;
        let entry = plan
            .items
            .iter_mut()
            .find(|i| i.status == ItemStatus::Active)?;
        entry.tools += 1;
        Some(entry.id.clone())
    }

    fn set_plan(
        &mut self,
        owner: Option<String>,
        source: &str,
        steps: &[PlanStep],
        note: Option<String>,
        ts: i64,
    ) {
        let mut next = self.next_item;
        let plan = self.plan_mut(owner, source, ts);
        let mut before = std::mem::take(&mut plan.items);
        let titles_before: Vec<String> = before.iter().map(|i| i.title.clone()).collect();
        let mut added = 0;
        for step in steps.iter().take(MAX_PLAN) {
            let kept = before
                .iter()
                .position(|i| i.title == step.title)
                .map(|at| before.remove(at));
            let mut entry = kept.unwrap_or_else(|| {
                added += 1;
                next += 1;
                PlanEntry {
                    id: format!("p{next}"),
                    title: step.title.clone(),
                    status: ItemStatus::Pending,
                    started: None,
                    finished: None,
                    tools: 0,
                }
            });
            move_to(&mut entry, step.status, ts);
            plan.items.push(entry);
        }
        let titles: Vec<&str> = plan.items.iter().map(|i| i.title.as_str()).collect();
        if titles != titles_before {
            plan.revision += 1;
            plan.added = added;
            plan.dropped = before.len() as u32;
        }
        plan.note = note;
        self.next_item = next;
    }

    fn set_plan_item(
        &mut self,
        owner: Option<String>,
        source: &str,
        id: &str,
        title: Option<&str>,
        status: Option<ItemStatus>,
        ts: i64,
    ) {
        let plan = self.plan_mut(owner, source, ts);
        let id = format!("task:{id}");
        let at = match plan.items.iter().position(|i| i.id == id) {
            Some(at) => at,
            None => {
                if plan.items.len() >= MAX_PLAN {
                    return;
                }
                plan.revision += 1;
                plan.added = 1;
                plan.dropped = 0;
                plan.items.push(PlanEntry {
                    id,
                    title: title.unwrap_or("task").to_owned(),
                    status: ItemStatus::Pending,
                    started: None,
                    finished: None,
                    tools: 0,
                });
                plan.items.len() - 1
            }
        };
        let entry = &mut plan.items[at];
        if let Some(title) = title {
            entry.title = title.to_owned();
        }
        if let Some(status) = status {
            move_to(entry, status, ts);
        }
    }

    /// The session on the fleet board.
    pub fn snapshot(&self) -> Snapshot {
        let (status, since) = if self.ended {
            ("ended", self.last_ts)
        } else if let Some(wait) = &self.wait {
            ("blocked", wait.since)
        } else if let Some(turn) = self.turn.as_deref().and_then(|id| self.item(id)) {
            ("working", turn.started)
        } else if self.last_turn_failed {
            ("failed", self.idle_since.unwrap_or(self.last_ts))
        } else if self.turns > 0 {
            ("done", self.idle_since.unwrap_or(self.last_ts))
        } else {
            ("idle", self.started.unwrap_or(self.last_ts))
        };
        let turn = self
            .turn
            .as_deref()
            .and_then(|id| self.item(id))
            .or_else(|| {
                self.work
                    .iter()
                    .rev()
                    .find(|item| item.source == WorkSource::Turn)
            });
        let children = |turn: &WorkItem| {
            self.work
                .iter()
                .filter(|item| item.source == WorkSource::Subagent)
                .filter(|item| self.turn_of(item) == Some(turn.id.as_str()))
                .collect::<Vec<_>>()
        };
        let subagents = turn.map(children).unwrap_or_default();
        let tool = self
            .open
            .iter()
            .rev()
            .find(|o| o.agent.is_none())
            .or(self.open.last());
        Snapshot {
            host: self.host.clone(),
            session: self.session.clone(),
            seq: self.seq,
            harness: self.harness.clone(),
            conversation: self.conversation.clone(),
            model: self.model.clone(),
            label: self.label.clone(),
            dir: self.dir.clone(),
            status: status.to_owned(),
            since,
            blocked_on: self.wait.as_ref().map(|w| w.reason),
            blocked_tool: self.wait.as_ref().and_then(|w| w.tool.clone()),
            started: self.started,
            last_ts: self.last_ts,
            turns: self.turns,
            turn_title: turn.map(|t| t.title.clone()).filter(|t| !t.is_empty()),
            turn_started: turn.map(|t| t.started),
            turn_tools: turn.map_or(0, |t| t.tools)
                + subagents.iter().map(|item| item.tools).sum::<u32>(),
            tool: tool.map(|o| ToolNow {
                name: o.name.clone(),
                summary: o.summary.clone(),
                since: o.ts,
            }),
            open_tools: self.open.len() as u32,
            tools: self.tools,
            tool_errors: self.tool_errors,
            denied: self.denied,
            plan: self
                .plans
                .iter()
                .find(|p| p.owner.is_none())
                .filter(|p| !p.items.is_empty())
                .map(Plan::progress),
            subagents_active: subagents
                .iter()
                .filter(|item| item.status == WorkStatus::Active)
                .count() as u32,
            subagents_total: subagents.len() as u32,
            context: self.context.clone(),
            usage: self.usage.clone(),
            rate: self.rate.clone(),
            limited_at: self.limited_at,
            errors: self.errors,
            last_error: self.last_error.clone(),
            blocked_ms: self.blocked_ms,
            waits: self.waits,
            awaiting_ms: self.awaiting_ms,
            compactions: self.compactions,
        }
    }

    /// The session in full.
    pub fn detail(&self) -> Detail {
        Detail {
            snapshot: self.snapshot(),
            work: self.work.clone(),
            plans: self.plans.clone(),
        }
    }

    /// The main agent's active plan item, for callers that label work.
    pub fn active_plan_item(&self) -> Option<&PlanEntry> {
        self.plans
            .iter()
            .find(|p| p.owner.is_none())
            .and_then(Plan::active)
    }

    fn item(&self, id: &str) -> Option<&WorkItem> {
        self.work.iter().rev().find(|item| item.id == id)
    }

    /// The turn a work item hangs under (through nested subagents).
    fn turn_of<'a>(&'a self, item: &'a WorkItem) -> Option<&'a str> {
        let mut at = item;
        for _ in 0..16 {
            if at.source == WorkSource::Turn {
                return Some(&at.id);
            }
            at = self.item(at.parent.as_deref()?)?;
        }
        None
    }
}

fn move_to(entry: &mut PlanEntry, status: ItemStatus, ts: i64) {
    if entry.status == status {
        return;
    }
    match status {
        ItemStatus::Active => {
            entry.started.get_or_insert(ts);
            entry.finished = None;
        }
        ItemStatus::Done | ItemStatus::Cancelled => entry.finished = Some(ts),
        ItemStatus::Pending => entry.finished = None,
    }
    entry.status = status;
}
