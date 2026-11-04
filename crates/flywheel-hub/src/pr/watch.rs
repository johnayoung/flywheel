//! The pull request cache: what GitHub said per (directory, branch), when
//! to ask again, the hub-wide rate limit, and the events a change makes.

use super::actions::LogView;
use super::gh::{
    ChecksSummary, Gh, GhProblem, MergeMethods, ProblemKind, PullRequest, Rollup, remote_facts,
};
use crate::git::GitStatus;
use crate::hub::{Hub, HubEvent, SessionView};
use crate::review::Tools;
use crate::state::SessionRecord;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, mpsc};
use std::time::{Duration, Instant};

/// When the hub asks GitHub (see [`PrWatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrTiming {
    /// How often the watcher looks for what is due.
    pub tick: Duration,
    /// An open pull request with checks running (or just pushed).
    pub pending_every: Duration,
    /// An open pull request whose checks have settled.
    pub open_every: Duration,
    /// A merged or closed one.
    pub settled_every: Duration,
    /// No pull request, or gh cannot answer (not installed, signed out).
    pub idle_every: Duration,
    /// A manual refresh within this of the last one answers from the cache.
    pub forced_gap: Duration,
    /// After GitHub's rate limit is hit: nothing is asked for this long.
    pub rate_backoff: Duration,
    /// gh calls the timer may make per minute, hub-wide (manual refreshes
    /// and review actions are not held to it).
    pub per_minute: usize,
}

impl Default for PrTiming {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(15),
            pending_every: Duration::from_secs(60),
            open_every: Duration::from_secs(5 * 60),
            settled_every: Duration::from_secs(30 * 60),
            idle_every: Duration::from_secs(5 * 60),
            forced_gap: Duration::from_secs(5),
            rate_backoff: Duration::from_secs(15 * 60),
            per_minute: 30,
        }
    }
}

/// What the cache is keyed on: a session's directory (its worktree, else
/// its folder) and branch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub dir: PathBuf,
    pub branch: String,
}

impl Key {
    pub(crate) fn of(record: &SessionRecord, git: Option<&GitStatus>) -> Option<Self> {
        let branch = git?.branch.clone()?;
        let dir = record
            .worktree
            .as_ref()
            .map_or_else(|| record.cwd.clone(), |w| w.path.clone());
        Some(Self { dir, branch })
    }
}

/// A pull request and its checks as last read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Snap {
    pub pr: PullRequest,
    pub checks: Vec<super::Check>,
    pub summary: ChecksSummary,
}

/// GitHub's answer for a key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Found {
    /// No pull request has the branch as its head.
    None,
    Pr(Box<Snap>),
}

/// One key's state.
#[derive(Debug, Clone, Default)]
pub(crate) struct Entry {
    /// GitHub's last good answer.
    pub found: Option<Found>,
    /// Why the last ask failed (the answer above, if any, is older).
    pub problem: Option<GhProblem>,
    /// Why GitHub is not asked (not pushed, a default branch).
    pub note: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
    pub due: Option<Instant>,
    forced_at: Option<Instant>,
    nudged_at: Option<Instant>,
    pub methods: Option<(Instant, MergeMethods)>,
    /// The session that last looked or acted here: events name it (else
    /// the first session on the branch).
    pub by: Option<String>,
}

impl Entry {
    pub(crate) fn snap(&self) -> Option<&Snap> {
        match &self.found {
            Some(Found::Pr(snap)) => Some(snap),
            _ => None,
        }
    }
}

/// A session row's pull request badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrBrief {
    pub number: u64,
    pub url: String,
    /// `open`, `draft`, `merged`, `closed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<String>,
    pub checks: ChecksSummary,
    /// gh could not answer the last time (the rest is older).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
}

/// Why a refresh happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The slow timer: held to the per-minute budget.
    Timer,
    /// A review action or the hub's own PR action changed things.
    Nudge,
    /// Someone asked (the refresh button, `fw hub pr --refresh`).
    Manual,
}

#[derive(Debug, Default)]
struct Limiter {
    calls: VecDeque<Instant>,
    paused_until: Option<Instant>,
}

enum Wake {
    Key(Key),
    /// The timing changed: look again with it.
    Timing,
}

/// A fix-CI ask pasted: head commit, check, when.
type Asked = (String, String, Instant);

/// The pull request cache and its watcher (see the module doc).
#[derive(Default)]
pub struct PrWatch {
    tools: RwLock<Tools>,
    timing: RwLock<PrTiming>,
    entries: Mutex<HashMap<Key, Entry>>,
    limiter: Mutex<Limiter>,
    /// Keys being refreshed now (one refresh per key at a time).
    busy: Mutex<HashSet<Key>>,
    freed: Condvar,
    wake: Mutex<Option<mpsc::Sender<Wake>>>,
    /// Failed logs read lately: (when, job/run key, view).
    logs: Mutex<VecDeque<(Instant, String, LogView)>>,
    /// Fix-CI asks pasted lately: (agent session, pull request) -> (head
    /// commit, check, when).
    asked: Mutex<HashMap<(String, u64), Asked>>,
}

impl std::fmt::Debug for PrWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrWatch").finish_non_exhaustive()
    }
}

/// Failed logs kept, and for how long.
const LOGS_KEPT: usize = 16;
const LOG_TTL: Duration = Duration::from_secs(120);
/// How long merge methods are trusted.
const METHODS_TTL: Duration = Duration::from_secs(10 * 60);
/// After a push or a new pull request: checks are looked for this soon,
/// for this long.
const NUDGE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// A manual refresh waits this long for one already running.
const BUSY_WAIT: Duration = Duration::from_secs(90);
/// Failing checks named in an event.
const EVENT_NAMES: usize = 20;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PrWatch {
    /// The `git` and `gh` it runs.
    pub fn set_tools(&self, tools: Tools) {
        *self.tools.write().unwrap_or_else(PoisonError::into_inner) = tools;
    }

    pub fn tools(&self) -> Tools {
        self.tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_timing(&self, timing: PrTiming) {
        *self.timing.write().unwrap_or_else(PoisonError::into_inner) = timing;
        if let Some(wake) = lock(&self.wake).as_ref() {
            let _ = wake.send(Wake::Timing);
        }
    }

    pub fn timing(&self) -> PrTiming {
        *self.timing.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn entry(&self, key: &Key) -> Option<Entry> {
        lock(&self.entries).get(key).cloned()
    }

    /// Session `id` looks at or acts on `key`'s pull request now: what
    /// changes next is announced as its.
    pub(crate) fn note_session(&self, key: &Key, id: &str) {
        lock(&self.entries).entry(key.clone()).or_default().by = Some(id.to_owned());
    }

    /// A manual refresh now would answer from the cache: how long ago
    /// GitHub was asked by one (`None`: it would ask).
    pub(crate) fn throttled(&self, key: &Key) -> Option<Duration> {
        let gap = self.timing().forced_gap;
        let entries = lock(&self.entries);
        let entry = entries.get(key)?;
        let since = entry.forced_at?.elapsed();
        (entry.checked_at.is_some() && since < gap).then_some(since)
    }

    /// A session row's badge (`None`: no pull request known).
    pub(crate) fn brief(&self, record: &SessionRecord, git: Option<&GitStatus>) -> Option<PrBrief> {
        let key = Key::of(record, git)?;
        let entries = lock(&self.entries);
        let entry = entries.get(&key)?;
        let snap = entry.snap()?;
        Some(PrBrief {
            number: snap.pr.number,
            url: snap.pr.url.clone(),
            state: snap.pr.shown_state().to_owned(),
            review: snap.pr.review.clone(),
            mergeable: snap.pr.mergeable.clone(),
            checks: snap.summary,
            stale: entry.problem.as_ref().map(|p| p.reason.clone()),
        })
    }

    /// Ask again soon for every key in or around `dir` (a push, a new
    /// pull request, a merge there).
    pub(crate) fn nudge_dir(&self, dir: &Path) {
        let keys: Vec<Key> = {
            let mut entries = lock(&self.entries);
            let now = Instant::now();
            entries
                .iter_mut()
                .filter(|(k, _)| k.dir.starts_with(dir) || dir.starts_with(&k.dir))
                .map(|(k, e)| {
                    e.due = Some(now);
                    e.nudged_at = Some(now);
                    k.clone()
                })
                .collect()
        };
        if let Some(wake) = lock(&self.wake).as_ref() {
            for key in keys {
                let _ = wake.send(Wake::Key(key));
            }
        }
    }

    /// A failed log read in the last [`LOG_TTL`].
    pub(crate) fn cached_log(&self, key: &str) -> Option<LogView> {
        let logs = lock(&self.logs);
        logs.iter()
            .find(|(at, k, _)| k == key && at.elapsed() < LOG_TTL)
            .map(|(_, _, v)| v.clone())
    }

    pub(crate) fn keep_log(&self, key: String, view: LogView) {
        let mut logs = lock(&self.logs);
        logs.retain(|(at, k, _)| *k != key && at.elapsed() < LOG_TTL);
        if logs.len() >= LOGS_KEPT {
            logs.pop_front();
        }
        logs.push_back((Instant::now(), key, view));
    }

    /// Agent `agent` was asked to fix `check` on pull request `pr` at
    /// `head` this long ago (within `within`).
    pub(crate) fn asked_lately(
        &self,
        agent: &str,
        pr: u64,
        head: &str,
        check: &str,
        within: Duration,
    ) -> Option<Duration> {
        let asked = lock(&self.asked);
        let (at_head, at_check, at) = asked.get(&(agent.to_owned(), pr))?;
        (at_head == head && at_check == check && at.elapsed() < within).then(|| at.elapsed())
    }

    pub(crate) fn note_asked(&self, agent: &str, pr: u64, head: &str, check: &str) {
        let mut asked = lock(&self.asked);
        asked.retain(|_, (_, _, at)| at.elapsed() < NUDGE_WINDOW);
        asked.insert(
            (agent.to_owned(), pr),
            (head.to_owned(), check.to_owned(), Instant::now()),
        );
    }

    pub(crate) fn keep_methods(&self, key: &Key, methods: MergeMethods) {
        if let Some(entry) = lock(&self.entries).get_mut(key) {
            entry.methods = Some((Instant::now(), methods));
        }
    }

    pub(crate) fn fresh_methods(&self, key: &Key) -> Option<MergeMethods> {
        lock(&self.entries)
            .get(key)?
            .methods
            .as_ref()
            .filter(|(at, _)| at.elapsed() < METHODS_TTL)
            .map(|(_, m)| m.clone())
    }

    /// Whether GitHub's rate limit pauses asking now (and until when).
    fn paused(&self) -> Option<Instant> {
        let limiter = lock(&self.limiter);
        limiter.paused_until.filter(|u| *u > Instant::now())
    }

    /// Take one call from the timer's budget (`false`: spent).
    fn budget(&self, reason: Reason, calls: usize) -> bool {
        let per_minute = self.timing().per_minute;
        let mut limiter = lock(&self.limiter);
        let now = Instant::now();
        while limiter
            .calls
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            limiter.calls.pop_front();
        }
        if reason == Reason::Timer && limiter.calls.len() + calls > per_minute {
            return false;
        }
        for _ in 0..calls {
            limiter.calls.push_back(now);
        }
        true
    }

    fn pause(&self, for_: Duration) {
        lock(&self.limiter).paused_until = Some(Instant::now() + for_);
    }

    /// Hold `key` for one refresh (waiting out one under way, up to
    /// [`BUSY_WAIT`]); `false`: still busy.
    fn hold(&self, key: &Key) -> bool {
        let deadline = Instant::now() + BUSY_WAIT;
        let mut busy = lock(&self.busy);
        while busy.contains(key) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            busy = self
                .freed
                .wait_timeout(busy, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        busy.insert(key.clone());
        true
    }

    fn release(&self, key: &Key) {
        lock(&self.busy).remove(key);
        self.freed.notify_all();
    }
}

/// When to ask again about what `entry` holds.
fn next_due(timing: &PrTiming, entry: &Entry, now: Instant) -> Instant {
    if let Some(problem) = &entry.problem {
        return now
            + match problem.kind {
                ProblemKind::RateLimited => timing.rate_backoff,
                ProblemKind::Failed => timing.pending_every.max(timing.tick),
                _ => timing.idle_every,
            };
    }
    let just_nudged = entry.nudged_at.is_some_and(|t| t.elapsed() < NUDGE_WINDOW);
    let every = match entry.snap() {
        Some(snap) if snap.pr.is_open() => {
            if snap.summary.pending > 0 || (just_nudged && snap.summary.total == 0) {
                timing.pending_every
            } else {
                timing.open_every
            }
        }
        Some(_) => timing.settled_every,
        None => timing.idle_every,
    };
    now + every
}

/// The plugin events a change from `before` to `after` makes (none on a
/// first look, or another pull request).
pub(crate) fn transitions(before: Option<&Snap>, after: &Snap) -> Vec<&'static str> {
    let Some(before) = before.filter(|b| b.pr.number == after.pr.number) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    let (was, now) = (before.summary.rollup, after.summary.rollup);
    if after.pr.is_open() {
        if now == Rollup::Failing && was != Rollup::Failing {
            events.push("pr.checks_failed");
        }
        if now == Rollup::Passing && matches!(was, Rollup::Pending | Rollup::Failing) {
            events.push("pr.checks_passed");
        }
        let changes = |s: &Snap| s.pr.review.as_deref() == Some("changes_requested");
        if changes(after) && !changes(before) {
            events.push("pr.review_changes_requested");
        }
    }
    if after.pr.state == "merged" && before.pr.state != "merged" {
        events.push("pr.merged");
    }
    events
}

impl Hub {
    /// The sessions whose pull request `key` is (an agent first).
    pub(crate) fn sessions_on(&self, key: &Key) -> Vec<SessionView> {
        let mut on: Vec<SessionView> = self
            .sessions(None)
            .into_iter()
            .filter(|v| Key::of(&v.record, v.git.as_ref()).as_ref() == Some(key))
            .collect();
        on.sort_by_key(|v| !v.record.runs_agent());
        on
    }

    /// Ask GitHub about `key` now (within the rules for `reason`), store
    /// the answer, and announce what changed. Blocking.
    pub(crate) fn pr_refresh(&self, key: &Key, reason: Reason) -> Entry {
        let watch = self.pr_watch();
        if !watch.hold(key) {
            return watch.entry(key).unwrap_or_default();
        }
        let result = self.pr_refresh_held(key, reason);
        watch.release(key);
        result
    }

    fn pr_refresh_held(&self, key: &Key, reason: Reason) -> Entry {
        let watch = self.pr_watch();
        let timing = watch.timing();
        let tools = watch.tools();
        let now = Instant::now();
        let before = watch.entry(key).unwrap_or_default();
        if reason == Reason::Manual
            && before.checked_at.is_some()
            && before
                .forced_at
                .is_some_and(|t| t.elapsed() < timing.forced_gap)
        {
            return before;
        }
        let mut entry = before.clone();
        if reason == Reason::Manual {
            entry.forced_at = Some(now);
        }
        if reason == Reason::Nudge {
            entry.nudged_at = Some(now);
        }
        let store = |entry: Entry| {
            lock(&watch.entries).insert(key.clone(), entry.clone());
            entry
        };
        if let Some(until) = watch.paused() {
            let left = until.saturating_duration_since(now).as_secs().div_ceil(60);
            entry.problem = Some(GhProblem::new(
                ProblemKind::RateLimited,
                format!("GitHub's API rate limit was hit: the hub asks again in about {left} min"),
            ));
            entry.due = Some(until);
            return store(entry);
        }
        match remote_facts(&tools, &key.dir, &key.branch) {
            Some((_, true)) => {
                entry.found = Some(Found::None);
                entry.problem = None;
                entry.note = Some(format!("{} is a default branch", key.branch));
                entry.due = Some(now + timing.idle_every);
                return self.pr_store(key, &before, entry);
            }
            // Not on a remote (any more): a pull request known for it is
            // still asked about -- GitHub deletes a merged head branch, and
            // a prune then drops the ref while the merge is not seen yet.
            Some((false, false)) if before.snap().is_none() => {
                entry.found = Some(Found::None);
                entry.problem = None;
                entry.note = Some(format!(
                    "{} is not pushed: push it, then open a pull request",
                    key.branch
                ));
                entry.due = Some(now + timing.idle_every);
                return self.pr_store(key, &before, entry);
            }
            Some((_, false)) => entry.note = None,
            None => {
                entry.found = Some(Found::None);
                entry.problem = None;
                entry.note = Some("no git remotes to look on".into());
                entry.due = Some(now + timing.idle_every);
                return self.pr_store(key, &before, entry);
            }
        }
        let gh = match Gh::find(&tools, &key.dir) {
            Ok(gh) => gh,
            Err(problem) => {
                entry.problem = Some(problem);
                entry.due = Some(next_due(&timing, &entry, now));
                return self.pr_store(key, &before, entry);
            }
        };
        if !watch.budget(reason, 1) {
            entry.due = Some(now + timing.tick);
            return store(entry);
        }
        let answer = gh.pr_view(&key.branch).and_then(|pr| match pr {
            None => Ok(Found::None),
            Some(pr) => {
                let previous = before.snap().filter(|s| s.pr.number == pr.number);
                let checks = if pr.is_open() {
                    watch.budget(Reason::Nudge, 1);
                    gh.checks(pr.number)?
                } else {
                    // Settled: its checks no longer change anything.
                    previous.map(|s| s.checks.clone()).unwrap_or_default()
                };
                let summary = ChecksSummary::of(&checks);
                Ok(Found::Pr(Box::new(Snap {
                    pr,
                    checks,
                    summary,
                })))
            }
        });
        match answer {
            Ok(found) => {
                entry.found = Some(found);
                entry.problem = None;
                entry.checked_at = Some(Utc::now());
            }
            Err(problem) => {
                if problem.kind == ProblemKind::RateLimited {
                    watch.pause(timing.rate_backoff);
                    eprintln!(
                        "fw hub: pr: GitHub's rate limit was hit; asking again in {} min",
                        timing.rate_backoff.as_secs() / 60
                    );
                }
                entry.problem = Some(problem);
            }
        }
        entry.due = Some(next_due(&timing, &entry, now));
        self.pr_store(key, &before, entry)
    }

    /// Store `entry`, tell the UI when a badge changed, and announce the
    /// transitions from `before`.
    fn pr_store(&self, key: &Key, before: &Entry, entry: Entry) -> Entry {
        let watch = self.pr_watch();
        lock(&watch.entries).insert(key.clone(), entry.clone());
        let badge = |e: &Entry| {
            (
                e.snap().map(|s| {
                    (
                        s.pr.number,
                        s.pr.shown_state().to_owned(),
                        s.pr.review.clone(),
                        s.pr.mergeable.clone(),
                        s.summary,
                    )
                }),
                e.problem.is_some(),
            )
        };
        if badge(before) != badge(&entry) {
            self.emit(HubEvent::Sessions);
        }
        let Some(after) = entry.snap() else {
            return entry;
        };
        if !(after.pr.is_open() && after.summary.rollup == Rollup::Failing) {
            self.drop_stale_fix_asks(key, &after.pr);
        }
        let events = transitions(before.snap(), after);
        if events.is_empty() {
            return entry;
        }
        let sessions = self.sessions_on(key);
        let session = entry
            .by
            .as_deref()
            .filter(|by| sessions.iter().any(|v| v.record.id == *by))
            .or_else(|| sessions.first().map(|v| v.record.id.as_str()))
            .and_then(|id| self.event_session(id));
        let failing: Vec<&str> = after
            .checks
            .iter()
            .filter(|c| c.bucket.failing())
            .map(|c| c.name.as_str())
            .take(EVENT_NAMES)
            .collect();
        for event in events {
            eprintln!(
                "fw hub: pr #{} ({}): {event}",
                after.pr.number, after.pr.head
            );
            self.emit_lifecycle(
                event,
                serde_json::json!({
                    "session": session,
                    "dir": key.dir,
                    "branch": key.branch,
                    "pr": {
                        "number": after.pr.number,
                        "title": after.pr.title,
                        "url": after.pr.url,
                        "state": after.pr.shown_state(),
                        "review": after.pr.review,
                        "base": after.pr.base,
                        "head_oid": after.pr.head_oid,
                    },
                    "checks": after.summary,
                    "failing": failing,
                }),
            );
        }
        entry
    }

    /// The keys the sessions are on now.
    fn pr_keys(&self) -> HashSet<Key> {
        self.sessions(None)
            .iter()
            .filter_map(|v| Key::of(&v.record, v.git.as_ref()))
            .collect()
    }
}

/// Start the watcher thread: it asks about each session's branch when due
/// (see [`PrTiming`]), and at once on a nudge. Ends with the hub.
pub(crate) fn spawn_watcher(hub: &Arc<Hub>) {
    let (wake, woken) = mpsc::channel::<Wake>();
    *lock(&hub.pr_watch().wake) = Some(wake);
    let weak = Arc::downgrade(hub);
    let _ = std::thread::Builder::new()
        .name("hub-pr".into())
        .spawn(move || {
            loop {
                let tick = match weak.upgrade() {
                    Some(hub) => hub.pr_watch().timing().tick,
                    None => break,
                };
                let mut nudged: Vec<Key> = Vec::new();
                match woken.recv_timeout(tick) {
                    Ok(Wake::Key(key)) => nudged.push(key),
                    Ok(Wake::Timing) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(wake) = woken.try_recv() {
                    if let Wake::Key(key) = wake {
                        nudged.push(key);
                    }
                }
                let Some(hub) = weak.upgrade() else { break };
                sweep(&hub, &nudged);
            }
        });
}

/// Refresh what is due (nudged keys first), and forget keys no session
/// is on any more.
fn sweep(hub: &Hub, nudged: &[Key]) {
    let keys = hub.pr_keys();
    let watch = hub.pr_watch();
    lock(&watch.entries).retain(|k, _| keys.contains(k));
    for key in nudged {
        if keys.contains(key) {
            hub.pr_refresh(key, Reason::Nudge);
        }
    }
    let now = Instant::now();
    let mut due: Vec<(Instant, Key)> = keys
        .into_iter()
        .filter(|k| !nudged.contains(k))
        .filter_map(|k| {
            let at = watch.entry(&k).and_then(|e| e.due).unwrap_or(now);
            (at <= now).then_some((at, k))
        })
        .collect();
    due.sort_by_key(|(at, _)| *at);
    for (_, key) in due {
        hub.pr_refresh(&key, Reason::Timer);
    }
}
