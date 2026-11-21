//! Watches: a subscription that wakes a session. When an event in the
//! hub's log ([`crate::telemetry::log`]) matches a watch's filter, the hub
//! queues a prompt (the watch's text, then the events and their cursors)
//! into the watch's owner through its prompt queue ([`crate::queue`]), so
//! the wake keeps the queue's rules: delivered exactly once, one at a time,
//! never while the agent works, is blocked, or has something half-typed.
//!
//! Bursts are coalesced: matches gather for the watch's window (default
//! [`DEFAULT_WINDOW_SECS`]) before they are queued, and while the
//! watch's queued prompt is still waiting, later matches join it (its
//! text is rewritten, at most [`MAX_LINES`] events listed, the rest
//! counted with the cursor to read them from). Five hundred children
//! finishing together wake their lead with one prompt.
//!
//! Exactly once from log to queue: the prompt and the watch's cursor are
//! saved in one write of `state.json`. A hub that stops in between reads
//! the log again from the saved cursor.
//!
//! A watch belongs to its owner: removed with it, ended with it, gone
//! when its process exits (not when the hub stops or relaunches it). The
//! owner's own events never match its watches (no wake loops).
//!
//! Scale: one scan of the log per tick from the lowest watch's cursor;
//! each row is matched only against the watches that can match its
//! session (by id, by `fw.parent`, or fleet-wide), labels read once per
//! session per tick.

use crate::eventlog::Filter;
use crate::hub::{Hub, HubError};
use crate::state::{StateError, new_id};
use crate::telemetry::log::{Cursor, LogQuery, LogRow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The window a watch gathers matches for, unless it names one.
pub const DEFAULT_WINDOW_SECS: u32 = 3;
/// The longest window.
pub const MAX_WINDOW_SECS: u32 = 3600;
/// Watches per owner, and per hub.
pub const MAX_PER_OWNER: usize = 20;
pub const MAX_PER_HUB: usize = 2000;
/// The longest watch prompt.
pub const MAX_PROMPT_BYTES: usize = 4096;
/// Events a queued prompt lists; the rest are counted.
pub const MAX_LINES: usize = 40;
/// Most bytes of closing messages one wake carries in full; past them a
/// finished turn is one summary line (`fw hub tail` has the rest).
pub const MAX_ENDINGS_BYTES: usize = 12 * 1024;
/// The watch the hub arms for a session when it starts its first child
/// ([`Hub::ensure_children_watch`]).
pub const DEFAULT_CHILDREN_FILTER: &str = "children state=done|blocked|failed|asking";
/// That watch's prompt.
pub const DEFAULT_CHILDREN_PROMPT: &str = "A session you started moved (the hub armed this \
     watch when you started it). Its closing message below is its report: act on it; fetch \
     more only if the report is not enough.";
/// The shortest and longest `every`, and how far ahead (or behind) `at`
/// may be.
pub const MIN_EVERY_SECS: u32 = 300;
pub const MAX_EVERY_SECS: u32 = 30 * 86_400;
pub const MAX_AHEAD_DAYS: i64 = 366;
pub const MAX_BEHIND_DAYS: i64 = 1;
/// How often the engine looks at the log.
const TICK: Duration = Duration::from_millis(250);
/// Rows one tick reads.
const SCAN_ROWS: usize = 5000;
/// How often a quiet watch's cursor is saved (so a restart rescans less).
const SAVE_EVERY: Duration = Duration::from_secs(60);

fn default_window() -> u32 {
    DEFAULT_WINDOW_SECS
}

/// The events a watch's waiting prompt lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// The queue item it is in.
    pub item: String,
    pub lines: Vec<String>,
    /// Matches past [`MAX_LINES`], counted.
    #[serde(default)]
    pub more: u32,
    /// The cursor just before its first event, and after its last.
    pub from: String,
    pub to: String,
}

/// One watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    /// The session it wakes (an agent).
    pub owner: String,
    /// What it matches, its sessions and workspaces named by id.
    pub filter: Filter,
    /// What the woken agent is told, before the events.
    pub prompt: String,
    #[serde(default = "default_window")]
    pub window_secs: u32,
    /// Every log row up to here was looked at and, matching, queued.
    pub cursor: i64,
    pub created_at: DateTime<Utc>,
    /// The agent session that armed it (`None`: the operator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Prompts it queued, and when last.
    #[serde(default)]
    pub fired: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired: Option<DateTime<Utc>>,
    /// Events matched so far, all prompts.
    #[serde(default)]
    pub events: u64,
    /// Its prompt still waiting in the owner's queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<Batch>,
    /// A time watch: when it wakes its owner (its filter is empty and
    /// never matches the log). Passes to the owner's successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeTrigger>,
}

/// When a time watch fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeTrigger {
    /// Once at this time (with `every`: the first time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// Again this often.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u32>,
    /// When it fires next.
    pub next: DateTime<Utc>,
}

impl TimeTrigger {
    /// After firing at `now`: the next time (past `now`), or `None` for a
    /// one-shot.
    pub fn after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let every = chrono::Duration::seconds(i64::from(self.every_secs?));
        let mut next = self.next;
        while next <= now {
            next += every;
        }
        Some(next)
    }

    /// In words.
    pub fn words(&self) -> String {
        match (self.at, self.every_secs) {
            (Some(at), Some(every)) => format!(
                "from {} every {}",
                at.format("%Y-%m-%d %H:%M UTC"),
                crate::share::format_ttl(Duration::from_secs(u64::from(every)))
            ),
            (Some(at), None) => format!("at {}", at.format("%Y-%m-%d %H:%M UTC")),
            (None, Some(every)) => format!(
                "every {}",
                crate::share::format_ttl(Duration::from_secs(u64::from(every)))
            ),
            (None, None) => String::new(),
        }
    }
}

/// The time trigger `new` asks for (`None`: an event watch).
fn time_trigger(new: &NewWatch, now: DateTime<Utc>) -> Result<Option<TimeTrigger>, String> {
    let given = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let (at, every) = (given(&new.at), given(&new.every));
    if at.is_none() && every.is_none() {
        return Ok(None);
    }
    let at = at.map(|a| parse_at(&a, now)).transpose()?;
    if let Some(at) = at {
        if at > now + chrono::Duration::days(MAX_AHEAD_DAYS) {
            return Err(format!("at is at most {MAX_AHEAD_DAYS} days ahead"));
        }
        if at < now - chrono::Duration::days(MAX_BEHIND_DAYS) {
            return Err("at is in the past".into());
        }
    }
    let every_secs = every
        .map(|e| crate::lease_cli::parse_span(&e))
        .transpose()?;
    if let Some(secs) = every_secs
        && !(MIN_EVERY_SECS..=MAX_EVERY_SECS).contains(&secs)
    {
        return Err(format!(
            "every is {}-{} (at least 5m, at most 30d)",
            MIN_EVERY_SECS, MAX_EVERY_SECS
        ));
    }
    let next = at.unwrap_or_else(|| {
        now + chrono::Duration::seconds(i64::from(every_secs.unwrap_or(MIN_EVERY_SECS)))
    });
    Ok(Some(TimeTrigger {
        at,
        every_secs,
        next,
    }))
}

/// `raw` as a time: RFC 3339 (`2026-11-23T12:00:00Z`, also without
/// seconds), or `+SPAN` from `now` (`+36h`).
pub fn parse_at(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let raw = raw.trim();
    if let Some(span) = raw.strip_prefix('+') {
        let secs = crate::lease_cli::parse_span(span)?;
        return Ok(now + chrono::Duration::seconds(i64::from(secs)));
    }
    let with_seconds = if raw.len() == 17 && raw.ends_with('Z') {
        format!("{}:00Z", &raw[..16])
    } else {
        raw.to_owned()
    };
    DateTime::parse_from_rfc3339(&with_seconds)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| format!("at {raw:?}: a time like 2026-11-23T12:00Z, or +36h"))
}

impl Watch {
    /// The short id people type (`w` and 7 hex).
    pub fn short(&self) -> String {
        format!("w{}", &self.id[..7.min(self.id.len())])
    }

    /// The queued prompt's text for `batch`.
    pub fn text(&self, batch: &Batch) -> String {
        if let Some(time) = &self.time {
            let count = batch.lines.len() + batch.more as usize;
            return format!(
                "{}\n\n[watch {}, {}] due {}{}\n",
                self.prompt.trim_end(),
                self.short(),
                time.words(),
                batch.lines.join(", "),
                if count > 1 {
                    format!(" ({count} times while this waited)")
                } else {
                    String::new()
                }
            );
        }
        let count = batch.lines.len() + batch.more as usize;
        let mut text = format!(
            "{}\n\n[watch {} on `{}`] {count} event{} (cursor {} .. {}):\n",
            self.prompt.trim_end(),
            self.short(),
            self.filter,
            if count == 1 { "" } else { "s" },
            batch.from,
            batch.to
        );
        for line in &batch.lines {
            text.push_str("- ");
            text.push_str(line);
            text.push('\n');
        }
        if batch.more > 0 {
            text.push_str(&format!(
                "- and {} more: fw hub tail --after {} (filter: {})\n",
                batch.more, batch.from, self.filter
            ));
        }
        text
    }
}

/// Watch input, as the API, CLI, and agent tool give it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewWatch {
    /// The session it wakes (id, prefix, or name).
    pub owner: String,
    /// A filter string (see [`crate::eventlog`]).
    pub filter: String,
    pub prompt: String,
    #[serde(default)]
    pub window_secs: Option<u32>,
    /// Match from this cursor on (default: from now).
    #[serde(default)]
    pub after: Option<String>,
    /// A time watch: once at this time (RFC 3339, or `+36h`)...
    #[serde(default)]
    pub at: Option<String>,
    /// ...and/or this often (`6h`, at least 5m).
    #[serde(default)]
    pub every: Option<String>,
}

/// One event in a watch's prompt: when, who, what. A finished turn's
/// closing message (`ending`, already bounded at capture) follows in full,
/// indented: it is the session's report, so the woken agent need not
/// fetch it.
pub fn line(row: &LogRow) -> String {
    line_with(row, true)
}

/// [`line`], the closing message given in full only when `full` (else
/// left to the one-line summary).
fn line_with(row: &LogRow, full: bool) -> String {
    let when = DateTime::from_timestamp_millis(row.ts)
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default();
    let who = row
        .name
        .clone()
        .unwrap_or_else(|| row.session.chars().take(8).collect());
    let moved = row
        .state
        .as_deref()
        .map(|s| format!(" -> {s}"))
        .unwrap_or_default();
    let mut body = row.body.clone();
    let ending = body
        .as_object_mut()
        .filter(|_| full)
        .and_then(|map| map.remove("ending"))
        .and_then(|e| e.as_str().map(str::to_owned))
        .filter(|e| !e.trim().is_empty());
    let what = crate::telemetry::log::summary(&body);
    let mut out = format!(
        "{when} {who} ({}) {}{moved}{}{what}",
        row.session,
        row.kind,
        if what.is_empty() { "" } else { " " }
    );
    if let Some(ending) = ending {
        out.push_str(", closing message:");
        for text in ending.lines() {
            out.push_str("\n    ");
            out.push_str(&crate::commands_cli::visible_controls(text));
        }
    }
    out
}

/// Each row's [`line`], closing messages in full while they fit in
/// `budget` bytes.
fn lines_within(rows: &[LogRow], budget: usize) -> Vec<String> {
    let mut left = budget;
    rows.iter()
        .map(|row| {
            let full = line_with(row, true);
            let short = line_with(row, false);
            let extra = full.len().saturating_sub(short.len());
            if extra <= left {
                left -= extra;
                full
            } else {
                short
            }
        })
        .collect()
}

/// The engine's memory: how far each watch has looked, and its matches
/// gathering for their window.
#[derive(Default)]
pub struct Engine {
    inner: Mutex<Scan>,
}

#[derive(Default)]
struct Scan {
    /// Watch id -> the last row it looked at.
    looked: HashMap<String, i64>,
    /// Watch id -> matches not yet queued, and when the first came.
    gathered: HashMap<String, (Vec<LogRow>, Instant)>,
    /// The log's end at the last scan.
    head: i64,
    saved: Option<Instant>,
}

impl Hub {
    fn engine(&self) -> std::sync::MutexGuard<'_, Scan> {
        self.watch_engine()
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Arm a watch (`by`: the agent session arming it; `None`: the
    /// operator).
    pub fn watch_add(&self, new: &NewWatch, by: Option<&str>) -> Result<Watch, HubError> {
        let owner = {
            let store = self.store();
            crate::script::find_by(
                store.sessions(),
                |s| (s.id.as_str(), s.name.as_str()),
                &new.owner,
                "session",
            )
            .map_err(StateError::Invalid)?
            .cloned()
            .ok_or_else(|| StateError::Invalid(format!("no session matches {:?}", new.owner)))?
        };
        if !owner.runs_agent() {
            return Err(StateError::Invalid(format!(
                "{} is not an agent session: a watch wakes an agent through its prompt queue",
                owner.label()
            ))
            .into());
        }
        let prompt = new.prompt.trim();
        if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(StateError::Invalid(format!(
                "a watch's prompt is 1-{MAX_PROMPT_BYTES} bytes: what to do when it fires"
            ))
            .into());
        }
        let time = time_trigger(new, Utc::now()).map_err(StateError::Invalid)?;
        let parsed = Filter::parse(&new.filter).map_err(StateError::Invalid)?;
        if time.is_some() && !parsed.is_empty() {
            return Err(StateError::Invalid(
                "a watch wakes on events (filter) or on time (at, every), not both".into(),
            )
            .into());
        }
        if parsed.is_empty() && time.is_none() {
            return Err(StateError::Invalid(
                "a watch needs a filter (state=done|blocked children, kind=review, team=web, ...) \
                 or a time (at, every)"
                    .into(),
            )
            .into());
        }
        let filter = if time.is_some() {
            Filter::default()
        } else {
            self.resolve_filter(&parsed, Some(&owner.id))?
        };
        let window_secs = new.window_secs.unwrap_or(DEFAULT_WINDOW_SECS);
        if window_secs > MAX_WINDOW_SECS {
            return Err(StateError::Invalid(format!(
                "window is at most {MAX_WINDOW_SECS} seconds"
            ))
            .into());
        }
        // The same filter armed again by its owner: one watch, its prompt
        // and window replaced (two would wake it twice for every event).
        // A new watch on its children replaces the one the hub armed for
        // it when it started its first ([`DEFAULT_CHILDREN_PROMPT`]).
        if time.is_none() {
            let mine = self.watches(Some(&owner.id));
            if let Some(same) = mine.iter().find(|w| w.time.is_none() && w.filter == filter) {
                let watch = self.store().rearm_watch(&same.id, prompt, window_secs)?;
                self.queue_changed();
                return Ok(watch);
            }
            if filter.children.is_some() {
                for default in mine
                    .iter()
                    .filter(|w| w.time.is_none() && w.prompt == DEFAULT_CHILDREN_PROMPT)
                {
                    self.watch_remove(&default.id)?;
                }
            }
        }
        let cursor = match self.log_cursor(new.after.as_deref())? {
            Some(at) => at,
            None => self.telemetry().log_head()?.at,
        };
        let watch = Watch {
            id: new_id(),
            owner: owner.id.clone(),
            filter,
            prompt: prompt.to_owned(),
            window_secs,
            cursor,
            created_at: Utc::now(),
            by: by.map(str::to_owned),
            fired: 0,
            last_fired: None,
            events: 0,
            batch: None,
            time,
        };
        self.store()
            .add_watch(watch.clone(), MAX_PER_OWNER, MAX_PER_HUB)?;
        self.queue_changed();
        Ok(watch)
    }

    /// The `watch` agent tool: arm a watch that wakes `caller`.
    pub fn agent_watch(
        &self,
        caller: &str,
        filter: &str,
        prompt: &str,
        window_secs: Option<u32>,
        at: Option<&str>,
        every: Option<&str>,
    ) -> Result<String, HubError> {
        let watch = self.watch_add(
            &NewWatch {
                owner: caller.to_owned(),
                filter: filter.to_owned(),
                prompt: prompt.to_owned(),
                window_secs,
                after: None,
                at: at.map(str::to_owned),
                every: every.map(str::to_owned),
            },
            Some(caller),
        )?;
        if let Some(time) = &watch.time {
            return Ok(format!(
                "Watch {} armed {}: next {}. It wakes you with your prompt then (queued, \
                 delivered once your turn is over), survives hub restarts, and passes to your \
                 successor; unwatch {} stops it.",
                watch.short(),
                time.words(),
                time.next.format("%Y-%m-%d %H:%M:%S UTC"),
                watch.short()
            ));
        }
        Ok(format!(
            "Watch {} armed on `{}` (window {} s): matching events wake you as your next prompt, \
             with their cursors. Keep working or end your turn; unwatch {} stops it.",
            watch.short(),
            watch.filter,
            watch.window_secs,
            watch.short()
        ))
    }

    /// Arm the default watch on `caller`'s children unless it already
    /// watches them: a session it starts reports by ending its turn, and
    /// that report reaches `caller` only through a watch. Best effort: a
    /// start never fails for it.
    pub(crate) fn ensure_children_watch(&self, caller: &str) {
        if self
            .watches(Some(caller))
            .iter()
            .any(|w| w.filter.children.is_some())
        {
            return;
        }
        let new = NewWatch {
            owner: caller.to_owned(),
            filter: DEFAULT_CHILDREN_FILTER.to_owned(),
            prompt: DEFAULT_CHILDREN_PROMPT.to_owned(),
            ..NewWatch::default()
        };
        if let Err(e) = self.watch_add(&new, None) {
            eprintln!("fw hub: default children watch for {caller}: {e}");
        }
    }

    /// The `unwatch` agent tool: disarm one of `caller`'s watches, or all.
    pub fn agent_unwatch(&self, caller: &str, id: &str) -> Result<String, HubError> {
        if id.trim() == "all" {
            let mine = self.watches(Some(caller));
            for watch in &mine {
                self.watch_remove(&watch.id)?;
            }
            return Ok(format!("{} watch(es) disarmed.", mine.len()));
        }
        let watch = self.watch_named(id)?;
        if watch.owner != caller {
            return Err(
                StateError::Invalid(format!("watch {} is not yours", watch.short())).into(),
            );
        }
        self.watch_remove(&watch.id)?;
        Ok(format!("Watch {} disarmed.", watch.short()))
    }

    /// A session's id from its id, prefix, or name.
    pub fn session_id_named(&self, query: &str) -> Result<String, HubError> {
        let store = self.store();
        crate::script::find_by(
            store.sessions(),
            |s| (s.id.as_str(), s.name.as_str()),
            query,
            "session",
        )
        .map_err(StateError::Invalid)?
        .map(|s| s.id.clone())
        .ok_or_else(|| StateError::Invalid(format!("no session matches {query:?}")).into())
    }

    /// Every watch (`owner`: only that session's).
    pub fn watches(&self, owner: Option<&str>) -> Vec<Watch> {
        self.store()
            .watches()
            .iter()
            .filter(|w| owner.is_none_or(|o| w.owner == o))
            .cloned()
            .collect()
    }

    /// A watch by id or unique id prefix (with or without its `w`).
    pub fn watch_named(&self, reference: &str) -> Result<Watch, HubError> {
        let reference = reference.trim();
        let bare = reference.strip_prefix('w').filter(|r| r.len() >= 4);
        let store = self.store();
        let found: Vec<&Watch> = store
            .watches()
            .iter()
            .filter(|w| w.id == reference || w.id.starts_with(bare.unwrap_or(reference)))
            .collect();
        match found.as_slice() {
            [one] if reference.len() >= 4 => Ok((*one).clone()),
            [] | [_] => Err(StateError::NotFound(format!("watch {reference}")).into()),
            _ => Err(StateError::Invalid(format!(
                "watch {reference} is ambiguous: give more of its id"
            ))
            .into()),
        }
    }

    /// Disarm a watch; its prompt already queued stays (it is the
    /// owner's now: `fw hub queue rm` takes it back).
    pub fn watch_remove(&self, id: &str) -> Result<Watch, HubError> {
        let removed = self.store().remove_watch(id)?;
        self.forget_scan(std::slice::from_ref(&removed.id));
        self.queue_changed();
        Ok(removed)
    }

    /// Disarm every watch `owner` has (it was removed, ended, or exited).
    pub(crate) fn drop_watches_of(&self, owner: &str) {
        // Its time watches pass to its successor (a replacement, or a
        // session continuing its work), if one runs.
        let successor = {
            let store = self.store();
            let named = store
                .session(owner)
                .ok()
                .and_then(|r| r.ended.as_ref().and_then(|e| e.successor.clone()));
            named
                .or_else(|| {
                    store
                        .sessions()
                        .iter()
                        .filter(|s| s.continues.as_deref() == Some(owner))
                        .max_by_key(|s| s.created_at)
                        .map(|s| s.id.clone())
                })
                .filter(|id| {
                    store.session(id).is_ok_and(|s| {
                        s.runs_agent() && s.state == crate::state::SessionState::Running
                    })
                })
        };
        if let Some(to) = &successor {
            match self.store().move_time_watches(owner, to, MAX_PER_OWNER) {
                Ok(moved) => {
                    for watch in &moved {
                        self.log_hub_event(
                            "watch.moved",
                            Some(to),
                            serde_json::json!({ "watch": watch.id, "from": owner, "to": to }),
                        );
                    }
                }
                Err(e) => eprintln!("fw hub: watches of {owner}: not passed on: {e}"),
            }
        }
        let removed = match self.store().remove_watches_of(owner) {
            Ok(removed) => removed,
            Err(e) => {
                eprintln!("fw hub: watches of {owner}: {e}");
                return;
            }
        };
        // Its children pass to its successor (their owner is followed
        // through successors), and they report by ending their turns: the
        // successor watches them, or it would never hear them.
        if let Some(to) = &successor
            && removed.iter().any(|w| w.filter.children.is_some())
        {
            self.ensure_children_watch(to);
        }
        if !removed.is_empty() {
            let ids: Vec<String> = removed.iter().map(|w| w.id.clone()).collect();
            eprintln!("fw hub: {} watch(es) of {owner} ended with it", ids.len());
            self.forget_scan(&ids);
        }
    }

    fn forget_scan(&self, ids: &[String]) {
        let mut scan = self.engine();
        for id in ids {
            scan.looked.remove(id);
            scan.gathered.remove(id);
        }
    }

    /// One look at the log for every watch: gather what matches, queue
    /// what has waited its window. Whether anything is still gathering.
    pub(crate) fn watch_tick(&self) -> bool {
        let (timed, watches): (Vec<Watch>, Vec<Watch>) = self
            .store()
            .watches()
            .iter()
            .cloned()
            .partition(|w| w.time.is_some());
        self.time_tick(&timed, Utc::now());
        if watches.is_empty() {
            return false;
        }
        let head = *self.telemetry().log_moves().borrow();
        let from = {
            let mut scan = self.engine();
            scan.looked
                .retain(|id, _| watches.iter().any(|w| w.id == *id));
            scan.gathered
                .retain(|id, _| watches.iter().any(|w| w.id == *id));
            for watch in &watches {
                scan.looked.entry(watch.id.clone()).or_insert(watch.cursor);
            }
            let from = watches
                .iter()
                .map(|w| scan.looked[&w.id])
                .min()
                .unwrap_or(head);
            scan.head = head;
            from
        };
        if from < head {
            self.watch_scan(&watches, from);
        }
        self.watch_deliver(&watches)
    }

    /// Match the log's rows after `from` against `watches`.
    fn watch_scan(&self, watches: &[Watch], from: i64) {
        let page = match self.read_log(&LogQuery {
            after: Some(from),
            limit: SCAN_ROWS,
            ..LogQuery::default()
        }) {
            Ok(page) => page,
            Err(e) => {
                eprintln!("fw hub: watches: the log does not read: {e}");
                return;
            }
        };
        let epoch = self.telemetry().log_epoch().unwrap_or(0);
        let looked_to = Cursor::parse(&page.cursor, epoch).map_or(from, |c| c.at);
        // Which watches may match a row: by session, by parent, or any.
        let mut by_session: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_parent: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut anywhere: Vec<usize> = Vec::new();
        let queries: Vec<LogQuery> = watches
            .iter()
            .map(|w| {
                let mut query = self.log_query(&w.filter, None, 1);
                query.not_sessions.push(w.owner.clone());
                query
            })
            .collect();
        for (at, (watch, query)) in watches.iter().zip(&queries).enumerate() {
            if !query.sessions.is_empty() {
                for s in &query.sessions {
                    by_session.entry(s.as_str()).or_default().push(at);
                }
            } else if let Some(parent) = &watch.filter.children {
                by_parent.entry(parent.as_str()).or_default().push(at);
            } else {
                anywhere.push(at);
            }
        }
        let mut labels: HashMap<(String, String), crate::labels::Labels> = HashMap::new();
        let mut matched: Vec<(usize, LogRow)> = Vec::new();
        {
            let scan = self.engine();
            for row in &page.events {
                let key = (row.host.clone(), row.session.clone());
                if !labels.contains_key(&key) {
                    labels.insert(
                        key.clone(),
                        self.telemetry().session_labels(&row.host, &row.session),
                    );
                }
                let now = &labels[&key];
                let parent = now.get(crate::labels::PARENT).map(String::as_str);
                let candidates = by_session
                    .get(row.session.as_str())
                    .into_iter()
                    .flatten()
                    .chain(parent.and_then(|p| by_parent.get(p)).into_iter().flatten())
                    .chain(anywhere.iter());
                for &at in candidates {
                    if scan
                        .looked
                        .get(&watches[at].id)
                        .is_some_and(|l| *l >= row.at)
                    {
                        continue;
                    }
                    if queries[at].matches(row, now) {
                        matched.push((at, row.clone()));
                    }
                }
            }
        }
        let mut scan = self.engine();
        for (at, row) in matched {
            let gathered = scan
                .gathered
                .entry(watches[at].id.clone())
                .or_insert_with(|| (Vec::new(), Instant::now()));
            gathered.0.push(row);
        }
        for watch in watches {
            let looked = scan.looked.entry(watch.id.clone()).or_insert(watch.cursor);
            *looked = (*looked).max(looked_to);
        }
    }

    /// Queue what each watch gathered once its window passed; save quiet
    /// watches' cursors now and then. Whether anything still gathers.
    fn watch_deliver(&self, watches: &[Watch]) -> bool {
        let due: Vec<(Watch, Vec<LogRow>, i64)> = {
            let mut scan = self.engine();
            let mut due = Vec::new();
            for watch in watches {
                let window = Duration::from_secs(u64::from(watch.window_secs));
                let ready = scan
                    .gathered
                    .get(&watch.id)
                    .is_some_and(|(rows, first)| !rows.is_empty() && first.elapsed() >= window);
                if ready {
                    let rows = scan
                        .gathered
                        .remove(&watch.id)
                        .map(|(rows, _)| rows)
                        .unwrap_or_default();
                    let looked = scan.looked.get(&watch.id).copied().unwrap_or(watch.cursor);
                    due.push((watch.clone(), rows, looked));
                }
            }
            due
        };
        for (watch, rows, looked) in due {
            if let Err(e) = self.watch_queue(&watch, &rows, looked) {
                // Not queued (the owner is ending, its queue is full): the
                // matches stay gathered and are tried again.
                eprintln!("fw hub: watch {}: not queued: {e}", watch.short());
                let mut scan = self.engine();
                let entry = scan
                    .gathered
                    .entry(watch.id.clone())
                    .or_insert_with(|| (Vec::new(), Instant::now()));
                let mut rows = rows;
                rows.append(&mut entry.0);
                entry.0 = rows;
            }
        }
        let mut scan = self.engine();
        let gathering = scan.gathered.values().any(|(rows, _)| !rows.is_empty());
        if scan.saved.is_none_or(|t| t.elapsed() >= SAVE_EVERY) {
            scan.saved = Some(Instant::now());
            let quiet: Vec<(String, i64)> = watches
                .iter()
                .filter(|w| !scan.gathered.contains_key(&w.id))
                .filter_map(|w| scan.looked.get(&w.id).map(|l| (w.id.clone(), *l)))
                .filter(|(id, l)| watches.iter().any(|w| w.id == *id && w.cursor < *l))
                .collect();
            drop(scan);
            if !quiet.is_empty()
                && let Err(e) = self.store().set_watch_cursors(&quiet)
            {
                eprintln!("fw hub: watches: cursors not saved: {e}");
            }
        }
        gathering
    }

    /// Fire the time watches due at `now`: a prompt queued (or, while
    /// the last waits, a line added to it), the next time set (a one-shot
    /// gone), in one write.
    fn time_tick(&self, timed: &[Watch], now: DateTime<Utc>) {
        for watch in timed {
            let Some(time) = &watch.time else { continue };
            if time.next > now {
                continue;
            }
            let late = (now - time.next).num_seconds();
            let line = format!(
                "{}{}",
                time.next.format("%Y-%m-%d %H:%M UTC"),
                if late >= 60 {
                    format!(
                        " (late {})",
                        crate::share::format_ttl(Duration::from_secs(late.unsigned_abs()))
                    )
                } else {
                    String::new()
                }
            );
            let queued = self.store().queue_watch(
                &watch.id,
                std::slice::from_ref(&line),
                "",
                "",
                watch.cursor,
                Some(now),
            );
            match queued {
                Ok(fresh) => {
                    eprintln!(
                        "fw hub: watch {} -> {}: due {line} {}",
                        watch.short(),
                        watch.owner,
                        if fresh {
                            "queued"
                        } else {
                            "joined its waiting prompt"
                        }
                    );
                    self.log_hub_event(
                        "watch.fired",
                        Some(&watch.owner),
                        serde_json::json!({
                            "watch": watch.id,
                            "trigger": if time.every_secs.is_some() { "every" } else { "at" },
                            "due": time.next,
                            "late_ms": (now - time.next).num_milliseconds(),
                        }),
                    );
                    self.queue_changed();
                }
                // Not queued (its owner is ending, its queue is full): tried
                // again next tick.
                Err(e) => eprintln!("fw hub: watch {}: not queued: {e}", watch.short()),
            }
        }
    }

    /// Queue `rows` for `watch`'s owner: into its waiting prompt, else a
    /// new one; the watch's cursor moves to `looked` in the same write.
    fn watch_queue(&self, watch: &Watch, rows: &[LogRow], looked: i64) -> Result<(), HubError> {
        let record = self.store().session(&watch.owner)?.clone();
        if let Some(ending) = &record.ending {
            return Err(StateError::Conflict(format!(
                "{} is ending (request {})",
                record.name, ending.id
            ))
            .into());
        }
        let epoch = self.telemetry().log_epoch()?;
        let first = rows.first().map_or(looked, |r| r.at - 1);
        let from = Cursor { epoch, at: first }.encode();
        let to = Cursor { epoch, at: looked }.encode();
        let lines = lines_within(rows, MAX_ENDINGS_BYTES);
        let delivered = self
            .store()
            .queue_watch(&watch.id, &lines, &from, &to, looked, None)?;
        eprintln!(
            "fw hub: watch {} -> {}: {} event(s) {}",
            watch.short(),
            record.name,
            rows.len(),
            if delivered {
                "queued"
            } else {
                "joined its waiting prompt"
            }
        );
        self.log_hub_event(
            "watch.fired",
            Some(&watch.owner),
            serde_json::json!({ "watch": watch.id, "events": rows.len(), "cursor": to }),
        );
        self.queue_changed();
        Ok(())
    }
}

impl Hub {
    /// Look at the log for every watch every [`TICK`] (its own thread:
    /// the queue's sleeps while nothing is queued).
    pub(crate) fn start_watches(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-watches".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    // Frozen for a live handoff: the next hub goes on from
                    // the saved cursors.
                    if !hub.queue_runtime().is_paused() {
                        hub.watch_tick();
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: watches are off: cannot start their thread: {e}");
        }
    }
}

#[cfg(test)]
mod line_tests {
    use super::*;

    fn finished(name: &str, ending: Option<&str>) -> LogRow {
        LogRow {
            cursor: String::new(),
            at: 1,
            host: "h".into(),
            session: format!("{name}-id"),
            seq: 1,
            ts: 0,
            kind: "turn.finished".into(),
            state: Some("done".into()),
            agent: None,
            body: match ending {
                Some(e) => serde_json::json!({ "turn": "t1", "ending": e }),
                None => serde_json::json!({ "turn": "t1" }),
            },
            name: Some(name.into()),
            workspace: None,
        }
    }

    #[test]
    fn a_finished_turns_closing_message_comes_in_full() {
        let report = format!(
            "Outcome: shipped.\nbranch fix/x at abc123\n{}",
            "y".repeat(400)
        );
        let text = line(&finished("c1", Some(&report)));
        assert!(
            text.starts_with("00:00:00 c1 (c1-id) turn.finished -> done turn=t1, closing message:"),
            "{text}"
        );
        assert!(
            text.contains("\n    Outcome: shipped.\n    branch fix/x at abc123\n"),
            "{text}"
        );
        assert!(text.ends_with(&"y".repeat(400)), "{text}");
        assert!(!text.contains("ending="), "{text}");
        let quiet = line(&finished("c2", None));
        assert!(!quiet.contains("closing message"), "{quiet}");
    }

    #[test]
    fn closing_messages_past_the_budget_are_one_line() {
        let long = "z".repeat(1000);
        let rows: Vec<LogRow> = (0..5)
            .map(|n| finished(&format!("c{n}"), Some(&long)))
            .collect();
        let lines = lines_within(&rows, 2500);
        let full = lines
            .iter()
            .filter(|l| l.contains("closing message"))
            .count();
        assert_eq!(full, 2, "{lines:?}");
        assert!(lines[4].contains("ending="), "{}", lines[4]);
        assert!(lines[4].len() < 200, "{}", lines[4]);
    }
}

#[cfg(test)]
mod time_tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn a_repeating_time_moves_past_now_and_a_one_shot_ends() {
        let every = TimeTrigger {
            at: None,
            every_secs: Some(3600),
            next: at(0),
        };
        // Down for 2.5 periods: fires once, next is the first time ahead.
        assert_eq!(every.after(at(9000)), Some(at(10_800)));
        let once = TimeTrigger {
            at: Some(at(0)),
            every_secs: None,
            next: at(0),
        };
        assert_eq!(once.after(at(5)), None);
    }

    #[test]
    fn times_read_as_rfc3339_or_from_now() {
        let now = at(0);
        assert_eq!(
            parse_at("+36h", now).unwrap(),
            now + chrono::Duration::hours(36)
        );
        assert_eq!(
            parse_at("2026-11-23T12:00Z", now).unwrap().to_rfc3339(),
            "2026-11-23T12:00:00+00:00"
        );
        assert!(parse_at("tomorrow", now).is_err());
    }

    #[test]
    fn a_time_watch_says_when_and_counts_times_it_was_due_while_waiting() {
        let watch = Watch {
            id: "abcdef0123".into(),
            owner: "o".into(),
            filter: Filter::default(),
            prompt: "check".into(),
            window_secs: 3,
            cursor: 0,
            created_at: at(0),
            by: None,
            fired: 0,
            last_fired: None,
            events: 0,
            batch: None,
            time: Some(TimeTrigger {
                at: None,
                every_secs: Some(21600),
                next: at(0),
            }),
        };
        let batch = Batch {
            item: "i".into(),
            lines: vec!["a".into(), "b".into()],
            more: 0,
            from: String::new(),
            to: String::new(),
        };
        let text = watch.text(&batch);
        assert!(
            text.starts_with(
                "check\n\n[watch wabcdef0, every 6h] due a, b (2 times while this waited)"
            ),
            "{text}"
        );
    }
}
