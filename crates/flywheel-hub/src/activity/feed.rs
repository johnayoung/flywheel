//! The hub's activity feed: one thread follows the event bus and writes a
//! row per thing that happened (see [`draft_for`]), then announces it on
//! the bus (`HubEvent::Activity`, the events socket's `activity`).
//! Usage growth is gathered per session and written as one `usage` row
//! when its turn settles (or every [`SPEND_FLUSH`] during a long one).

use super::{Digest, DigestSession, Draft, Filter, Journal, Limits, Marks, Return, Row};
use crate::hub::{Hub, HubEvent};
use crate::status::AgentStatus;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

/// How often pending usage and presence are looked at.
const TICK: Duration = Duration::from_secs(5);
/// A working session's gathered usage is written at least this often.
pub const SPEND_FLUSH: Duration = Duration::from_secs(300);
/// Usage arriving after a turn ended (the transcript is read a moment
/// later) still counts toward its row within this.
const SPEND_SETTLE: Duration = Duration::from_secs(15);
/// Suggested commands older than this when read (a transcript re-read
/// after a restart) are not "suggested now".
const COMMANDS_FRESH: chrono::Duration = chrono::Duration::minutes(15);

/// Usage gathered for one session, not yet written.
#[derive(Debug)]
struct Pending {
    about: Value,
    cost_usd: f64,
    tokens: u64,
    first: Instant,
    last: Instant,
}

#[derive(Debug, Default)]
struct Spend {
    /// Each session's usage totals as last seen: (cost, tokens).
    seen: HashMap<String, (f64, u64)>,
    pending: HashMap<String, Pending>,
}

/// The most rows held while a live handoff has the hub frozen.
const HELD_MAX: usize = 1000;

/// The journal, and the rows held while a live handoff froze the hub
/// (`Some`): written in order if the handoff is called off. One lock, so
/// no row is written after [`Activity::pause`] returns -- the next hub
/// numbers its rows from what is on disk then.
#[derive(Debug)]
struct Book {
    journal: Journal,
    held: Option<Vec<(Draft, DateTime<Utc>)>>,
}

/// The hub's activity journal, presence marks, and usage gathering.
#[derive(Debug)]
pub struct Activity {
    book: Mutex<Book>,
    marks: Mutex<Marks>,
    spend: Mutex<Spend>,
    /// The hub is stopping: the sessions it hangs up did not end of
    /// their own (no `session.exited` rows for them).
    stopping: AtomicBool,
    /// This hub took over from another (`fw hub upgrade`): its
    /// `hub.started` row says so.
    handed_over: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Activity {
    pub fn open(state_dir: &Path) -> Self {
        Self::with_limits(state_dir, Limits::default())
    }

    pub fn with_limits(state_dir: &Path, limits: Limits) -> Self {
        Self {
            book: Mutex::new(Book {
                journal: Journal::open(state_dir, limits, Utc::now()),
                held: None,
            }),
            marks: Mutex::new(Marks::load(state_dir)),
            spend: Mutex::new(Spend::default()),
            stopping: AtomicBool::new(false),
            handed_over: AtomicBool::new(false),
        }
    }

    /// Rows matching `filter`, newest first, before `before`; and whether
    /// more match.
    pub fn page(&self, filter: &Filter, before: Option<u64>, limit: usize) -> (Vec<Row>, bool) {
        lock(&self.book).journal.page(filter, before, limit)
    }

    /// The rows over `filter`'s span, oldest first.
    pub fn rows(&self, filter: &Filter) -> Vec<Row> {
        lock(&self.book).journal.matching(filter)
    }

    /// Write `draft` now; `None` while frozen for a handoff (it is held).
    pub fn record(&self, draft: Draft) -> Option<Row> {
        let now = Utc::now();
        let mut book = lock(&self.book);
        if let Some(held) = &mut book.held {
            if held.len() < HELD_MAX {
                held.push((draft, now));
            }
            return None;
        }
        Some(book.journal.append(draft, now))
    }

    /// When `client` was last active (now while present).
    pub fn last_active(&self, client: &str) -> Option<DateTime<Utc>> {
        lock(&self.marks).last_active(client, Utc::now())
    }

    /// Connection `conn` of `client` reports whether someone is at it;
    /// a return after [`super::AWAY_MIN`] away gives the span missed.
    pub fn set_present(&self, client: &str, conn: u64, present: bool) -> Option<Return> {
        let away = chrono::Duration::from_std(super::AWAY_MIN).unwrap_or(chrono::Duration::MAX);
        let mut marks = lock(&self.marks);
        let was = marks.is_present(client);
        let back = marks.set_present(client, conn, present, Utc::now(), away);
        if was && !marks.is_present(client) {
            // Gone: when it left is what its next digest starts from.
            marks.save(Utc::now(), true);
        }
        back
    }

    pub fn disconnect(&self, client: &str, conn: u64) {
        let mut marks = lock(&self.marks);
        let was = marks.is_present(client);
        marks.disconnect(client, conn, Utc::now());
        if was && !marks.is_present(client) {
            marks.save(Utc::now(), true);
        }
    }

    /// `client` saw everything up to now (`fw hub activity --since
    /// last-seen`).
    pub fn mark_seen(&self, client: &str) {
        let mut marks = lock(&self.marks);
        marks.touch(client, Utc::now());
        marks.save(Utc::now(), true);
    }

    /// The digest of `filter`'s span (its `since`..`until`, workspace, and
    /// session), with the agents of `hub` blocked now.
    pub fn digest(&self, hub: &Hub, filter: &Filter) -> Digest {
        let now = Utc::now();
        let since = filter.since.unwrap_or(now - chrono::Duration::days(1));
        let until = filter.until.unwrap_or(now);
        let span = Filter {
            since: Some(since),
            until: Some(until),
            kinds: Vec::new(),
            ..filter.clone()
        };
        let mut digest = Digest::of(&self.rows(&span), since, until);
        for view in hub.sessions(None) {
            let Some(agent) = &view.agent else { continue };
            if agent.status != AgentStatus::Blocked {
                continue;
            }
            let workspace = hub.workspace(&view.record.workspace_id).ok();
            let picked = filter.workspace.as_deref().is_none_or(|w| {
                super::selects(
                    w,
                    Some(&view.record.workspace_id),
                    workspace.as_ref().map(|w| w.name.as_str()),
                )
            }) && filter
                .session
                .as_deref()
                .is_none_or(|s| super::selects(s, Some(&view.record.id), Some(&view.record.name)));
            if !picked {
                continue;
            }
            digest.blocked_now.push(DigestSession {
                session_id: view.record.id.clone(),
                session: view.record.name.clone(),
                workspace_id: Some(view.record.workspace_id.clone()),
                workspace: workspace.map(|w| w.name),
                detail: agent.detail.clone(),
                machine: None,
            });
        }
        digest
    }

    /// Stop writing for a live handoff: gathered usage is written first,
    /// then rows are held (the journal on disk is the next hub's).
    pub fn pause(&self) {
        let drafts = self.take_spend(|_| false, true);
        let mut book = lock(&self.book);
        for draft in drafts {
            book.journal.append(draft, Utc::now());
        }
        book.held.get_or_insert_with(Vec::new);
        drop(book);
        lock(&self.marks).save(Utc::now(), true);
    }

    /// The handoff was called off: the rows held meanwhile are written
    /// (returned, to announce), and writing carries on.
    pub fn resume(&self) -> Vec<Row> {
        let mut book = lock(&self.book);
        let held = book.held.take().unwrap_or_default();
        held.into_iter()
            .map(|(draft, at)| book.journal.append(draft, at))
            .collect()
    }

    /// This hub took over from another (see [`Activity::handed_over`]).
    pub fn mark_handed_over(&self) {
        self.handed_over.store(true, Ordering::SeqCst);
    }

    /// The hub is stopping and hangs up its `sessions` running ones: write
    /// what was gathered and one row saying so, save the marks.
    pub fn shutdown(&self, sessions: usize) {
        self.stopping.store(true, Ordering::SeqCst);
        let drafts = self.take_spend(|_| false, true);
        for draft in drafts {
            self.record(draft);
        }
        let text = match sessions {
            0 => "hub stopped".to_owned(),
            1 => "hub stopped, hanging up 1 session".to_owned(),
            n => format!("hub stopped, hanging up {n} sessions"),
        };
        self.record(Draft::new("hub.stopped", text).with("sessions", sessions));
        lock(&self.marks).save(Utc::now(), true);
    }

    /// The hub is stopping (see [`Activity::shutdown`]).
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Remember every session's usage as it stands (growth from here on
    /// is new).
    fn seed(&self, hub: &Hub) {
        let mut spend = lock(&self.spend);
        for view in hub.sessions(None) {
            if let Some(usage) = &view.usage {
                spend.seen.insert(
                    view.record.id.clone(),
                    (usage.total.cost_usd, usage.total.total_tokens),
                );
            }
        }
    }

    /// Session `id`'s usage now stands at `cost`/`tokens`: gather the
    /// growth.
    fn note_usage(&self, id: &str, about: Value, cost: f64, tokens: u64) {
        let mut spend = lock(&self.spend);
        let (was_cost, was_tokens) = spend.seen.get(id).copied().unwrap_or((0.0, 0));
        spend.seen.insert(id.to_owned(), (cost, tokens));
        let grew_cost = (cost - was_cost).max(0.0);
        let grew_tokens = tokens.saturating_sub(was_tokens);
        if grew_tokens == 0 && grew_cost <= 0.0 {
            return;
        }
        let now = Instant::now();
        let pending = spend.pending.entry(id.to_owned()).or_insert(Pending {
            about,
            cost_usd: 0.0,
            tokens: 0,
            first: now,
            last: now,
        });
        pending.cost_usd += grew_cost;
        pending.tokens += grew_tokens;
        pending.last = now;
    }

    /// The `usage` rows due: a settled session's (not `working`, quiet for
    /// [`SPEND_SETTLE`]), one gathering for [`SPEND_FLUSH`], or all.
    fn take_spend(&self, working: impl Fn(&str) -> bool, all: bool) -> Vec<Draft> {
        let mut spend = lock(&self.spend);
        let due: Vec<String> = spend
            .pending
            .iter()
            .filter(|(id, p)| {
                all || p.first.elapsed() >= SPEND_FLUSH
                    || (p.last.elapsed() >= SPEND_SETTLE && !working(id))
            })
            .map(|(id, _)| id.clone())
            .collect();
        due.into_iter()
            .filter_map(|id| spend.pending.remove(&id))
            .map(|p| {
                let cost = super::digest::round_cents(p.cost_usd);
                Draft::new(
                    "usage",
                    format!("used {} tokens, ${cost:.2}", short_count(p.tokens)),
                )
                .about(&p.about)
                .with("cost_usd", cost)
                .with("tokens", p.tokens)
            })
            .collect()
    }

    /// A session was removed: its gathered usage is written now.
    fn forget(&self, id: &str) -> Option<Draft> {
        let mut spend = lock(&self.spend);
        spend.seen.remove(id);
        let p = spend.pending.remove(id)?;
        let cost = super::digest::round_cents(p.cost_usd);
        Some(
            Draft::new(
                "usage",
                format!("used {} tokens, ${cost:.2}", short_count(p.tokens)),
            )
            .about(&p.about)
            .with("cost_usd", cost)
            .with("tokens", p.tokens),
        )
    }

    fn save_marks(&self) {
        lock(&self.marks).save(Utc::now(), false);
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        lock(&self.book).journal.len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.book).journal.is_empty()
    }
}

impl Hub {
    /// Follow the event bus into the activity journal (a thread of its own,
    /// until the hub is gone).
    pub(crate) fn start_activity(self: &Arc<Self>) {
        let mut events = self.subscribe();
        self.activity().seed(self);
        let weak: Weak<Hub> = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-activity".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                else {
                    eprintln!("fw hub: activity journal off: no runtime");
                    return;
                };
                runtime.block_on(async move {
                    let mut tick = tokio::time::interval(TICK);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tokio::select! {
                            event = events.recv() => {
                                let event = match event {
                                    Ok(event) => event,
                                    Err(RecvError::Lagged(n)) => {
                                        eprintln!("fw hub: activity journal fell behind: {n} events not recorded");
                                        continue;
                                    }
                                    Err(RecvError::Closed) => break,
                                };
                                let Some(hub) = weak.upgrade() else { break };
                                hub.on_activity_event(event);
                            }
                            _ = tick.tick() => {
                                let Some(hub) = weak.upgrade() else { break };
                                hub.activity_tick();
                            }
                        }
                    }
                });
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: activity journal off: {e}");
        }
    }

    /// Write `draft` and announce the row.
    fn write_activity(&self, draft: Draft) {
        if let Some(row) = self.activity().record(draft) {
            self.emit(HubEvent::Activity {
                row: serde_json::to_value(&row).unwrap_or(Value::Null),
            });
        }
    }

    fn activity_tick(&self) {
        let drafts = self.activity().take_spend(
            |id| {
                self.agent_status(id)
                    .is_some_and(|s| s.status == AgentStatus::Working)
            },
            false,
        );
        for draft in drafts {
            self.write_activity(draft);
        }
        self.activity().save_marks();
    }

    fn on_activity_event(&self, event: HubEvent) {
        match event {
            HubEvent::Lifecycle { event, payload } => {
                // Hung up by the hub stopping: not an exit of their own
                // (the `hub.stopped` row says it).
                if event == "session.exited"
                    && payload.get("hub_stopping").and_then(Value::as_bool) == Some(true)
                {
                    return;
                }
                if event == "hub.started"
                    && self.activity().handed_over.swap(false, Ordering::SeqCst)
                {
                    let version = str_at(&payload, "/version").unwrap_or("?");
                    let sessions = payload.get("sessions").and_then(Value::as_u64).unwrap_or(0);
                    self.write_activity(
                        Draft::new(
                            event,
                            format!("hub upgraded to flywheel {version}, {sessions} sessions carried over"),
                        )
                        .with("version", version)
                        .with("upgraded", true),
                    );
                    return;
                }
                if event == "session.removed"
                    && let Some(id) = payload.pointer("/session/id").and_then(Value::as_str)
                    && let Some(draft) = self.activity().forget(id)
                {
                    self.write_activity(draft);
                }
                if let Some(draft) = draft_for(event, &payload) {
                    self.write_activity(draft);
                }
            }
            HubEvent::TurnFinished { session_id } => {
                let Some(about) = self.event_session(&session_id) else {
                    return;
                };
                // The newest transcript message: the reply that ended it.
                let message = self
                    .transcript_page(&session_id, None, 0, 0)
                    .ok()
                    .filter(|page| page.next > 0)
                    .map(|page| page.next - 1);
                let mut draft = Draft::new("turn.finished", "finished a turn").about(&about);
                if let Some(message) = message {
                    draft = draft.with("message", message);
                }
                self.write_activity(draft);
            }
            HubEvent::Commands { session_id, added } => {
                let now = Utc::now();
                let fresh: Vec<&crate::commands::Command> = added
                    .iter()
                    .filter(|c| {
                        c.timestamp
                            .as_deref()
                            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                            .is_none_or(|at| now - at.with_timezone(&Utc) <= COMMANDS_FRESH)
                    })
                    .collect();
                let Some(first) = fresh.first() else { return };
                let Some(about) = self.event_session(&session_id) else {
                    return;
                };
                let count = fresh.len();
                let shell = fresh.iter().filter(|c| c.shell).count();
                let text = if count == 1 {
                    "suggested a code block".to_owned()
                } else {
                    format!("suggested {count} code blocks")
                };
                self.write_activity(
                    Draft::new("commands.suggested", text)
                        .about(&about)
                        .with("count", count)
                        .with("shell", shell)
                        .with("seq", first.seq),
                );
            }
            HubEvent::Usage { session_id } => {
                let Ok(view) = self.session(&session_id) else {
                    return;
                };
                let Some(usage) = &view.usage else { return };
                let Some(about) = self.event_session(&session_id) else {
                    return;
                };
                self.activity().note_usage(
                    &session_id,
                    about,
                    usage.total.cost_usd,
                    usage.total.total_tokens,
                );
            }
            HubEvent::PluginRan {
                plugin,
                trigger,
                ok,
            } => {
                let text = format!(
                    "plugin {plugin} ran on {trigger}: {}",
                    if ok { "ok" } else { "failed" }
                );
                self.write_activity(
                    Draft::new("plugin.ran", text)
                        .with("plugin", plugin)
                        .with("trigger", trigger)
                        .with("ok", ok),
                );
            }
            _ => {}
        }
    }
}

fn str_at<'a>(payload: &'a Value, pointer: &str) -> Option<&'a str> {
    payload.pointer(pointer).and_then(Value::as_str)
}

/// The row a plugin lifecycle event makes (`None`: not one the timeline
/// keeps -- a status change other than to `blocked`, which finished turns
/// and the live sidebar cover).
pub(crate) fn draft_for(event: &str, payload: &Value) -> Option<Draft> {
    let session = payload.get("session").unwrap_or(&Value::Null);
    let draft = |text: String| Draft::new(event, text).about(session);
    let branch = str_at(payload, "/branch");
    let base = str_at(payload, "/base");
    Some(match event {
        "hub.started" => {
            let version = str_at(payload, "/version").unwrap_or("?");
            let sessions = payload.get("sessions").and_then(Value::as_u64).unwrap_or(0);
            Draft::new(
                event,
                format!("hub started (flywheel {version}, {sessions} sessions)"),
            )
            .with("version", version)
        }
        "session.created" => {
            let kind = str_at(session, "/kind").unwrap_or("shell");
            draft(format!("started a {kind} session"))
        }
        "session.exited" => {
            let code = payload.pointer("/exit/code").and_then(Value::as_i64);
            // The signal's name (`Hangup`), as `fw hub ls` shows it.
            let signal = str_at(payload, "/exit/signal");
            let how = match (code, signal) {
                (Some(code), _) => format!("exited with code {code}"),
                (None, Some(signal)) => format!("was killed ({signal})"),
                (None, None) => "exited".to_owned(),
            };
            draft(how).with("code", code).with("signal", signal)
        }
        "session.removed" => draft("was removed".to_owned()),
        "status.changed" => {
            if str_at(payload, "/status") != Some("blocked") {
                return None;
            }
            draft("is blocked: needs you".to_owned()).with("status", "blocked")
        }
        "review.committed" => draft(match branch {
            Some(b) => format!("committed on {b}"),
            None => "committed".to_owned(),
        })
        .with("branch", branch),
        "review.pushed" => draft(match branch {
            Some(b) => format!("pushed {b}"),
            None => "pushed".to_owned(),
        })
        .with("branch", branch),
        "review.pr_opened" => draft("opened a pull request".to_owned())
            .with("branch", branch)
            .with("url", str_at(payload, "/url")),
        "review.rebased" => draft(match (branch, base) {
            (Some(b), Some(base)) => format!("rebased {b} onto {base}"),
            _ => "rebased".to_owned(),
        })
        .with("branch", branch),
        "review.merged" => draft(match (branch, base) {
            (Some(b), Some(base)) => format!("merged {b} into {base}"),
            _ => "merged".to_owned(),
        })
        .with("branch", branch)
        .with("base", base),
        "review.requested" => {
            let author = payload.get("author").unwrap_or(&Value::Null);
            let reviewer = str_at(payload, "/reviewer/name").unwrap_or("a reviewer");
            Draft::new(event, format!("asked {reviewer} for a review"))
                .about(author)
                .with_id("reviewer_id", str_at(payload, "/reviewer/id"))
        }
        "handoff.sent" => {
            let to = payload.get("to").unwrap_or(&Value::Null);
            let from = str_at(payload, "/from/session").unwrap_or("another session");
            let bytes = payload.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            Draft::new(event, format!("received work from {from} ({bytes} bytes)"))
                .about(to)
                .with_id("from_id", str_at(payload, "/from/session_id"))
                .with("source", str_at(payload, "/source"))
        }
        "fanout.finished" => {
            let fanout = payload.get("fanout").unwrap_or(&Value::Null);
            let name = str_at(fanout, "/name").unwrap_or("fan-out");
            let winner = str_at(fanout, "/winner/label");
            let leader = str_at(fanout, "/leader");
            let passing = fanout
                .pointer("/counts/passing")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let candidates = fanout
                .pointer("/counts/candidates")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let lead = match (winner, leader) {
                (Some(w), _) => format!("winner {w}"),
                (None, Some(l)) => format!("{l} leads"),
                (None, None) => "no candidate passes".to_owned(),
            };
            Draft::new(
                event,
                format!("fan-out {name} graded: {lead} ({passing}/{candidates} passing)"),
            )
            .in_workspace(
                str_at(fanout, "/workspace_id"),
                str_at(fanout, "/workspace"),
            )
            .with_id("fanout_id", str_at(fanout, "/id"))
            .with("name", name)
            .with("winner", winner)
            .with("leader", leader)
            .with("passing", passing)
            .with("candidates", candidates)
        }
        "queue.delivered" => {
            let pending = payload.get("pending").and_then(Value::as_u64).unwrap_or(0);
            draft(format!("typed in a queued prompt ({pending} left)"))
                .with("pending", pending)
                .with("bytes", payload.pointer("/item/bytes").cloned())
        }
        "queue.stalled" => {
            let reason = str_at(payload, "/reason").unwrap_or("stalled");
            draft(format!("prompt queue stalled: {reason}"))
                .with("pending", payload.get("pending").cloned())
        }
        "share.created" | "share.revoked" => {
            let verb = if event == "share.created" {
                "shared a read-only link"
            } else {
                "revoked a share link"
            };
            let name = str_at(payload, "/share/name");
            draft(match name {
                Some(n) => format!("{verb} ({n})"),
                None => verb.to_owned(),
            })
            .with_id("share_id", str_at(payload, "/share/id"))
        }
        "record.started" => draft("started recording".to_owned())
            .with_id("recording", str_at(payload, "/recording/name")),
        "record.stopped" => draft("stopped recording".to_owned())
            .with_id("recording", str_at(payload, "/recording/name"))
            .with("reason", str_at(payload, "/reason")),
        _ => return None,
    })
}

/// `n` as the sidebar and `fw hub ls` show counts (`950`, `110.0k`, `2.3M`).
pub(crate) fn short_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}
