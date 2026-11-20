//! Leases on the hub (see [`crate::leases`] for the rules): one
//! [`Book`] under a lock, every change written through to `coord.db`
//! (so leases and waits outlive a restart), logged as `lease.*` events,
//! and told: a blocked caller wakes on a condition variable (never a poll),
//! and a wait that outlived its call is granted as a prompt to its
//! session.

use super::{Hub, HubError};
use crate::leases::{Acquired, Ask, Book, Change, Gone, Holder, Lease, MAX_BLOCK_SECS, Waiter};
use crate::sqlite_store::LazyDb;
use crate::state::{SessionState, StateError, new_id};
use crate::telemetry::now_ms;
use rusqlite::params;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The lease store's file in the state dir.
pub const COORD_FILE: &str = "coord.db";
const SCHEMA: i64 = 1;
/// How often the hub looks for leases whose time is up.
const TICK: Duration = Duration::from_millis(500);

fn migrate(conn: &rusqlite::Connection, from: i64) -> rusqlite::Result<()> {
    if from < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS leases (
                 id   TEXT PRIMARY KEY,
                 body TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS lease_waiters (
                 id   TEXT PRIMARY KEY,
                 seq  INTEGER NOT NULL,
                 body TEXT NOT NULL
             );",
        )?;
    }
    Ok(())
}

/// The hub's leases.
pub struct LeaseRuntime {
    book: Mutex<Book>,
    changed: Condvar,
    db: LazyDb,
    /// Order waits were queued in (their place in line across a restart).
    seq: Mutex<i64>,
}

impl std::fmt::Debug for LeaseRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseRuntime").finish_non_exhaustive()
    }
}

impl LeaseRuntime {
    /// Leases kept in `state_dir`, loaded now (a file that cannot be read
    /// leaves the book empty and says why).
    pub fn open(state_dir: &Path) -> Self {
        let db = LazyDb::new(state_dir.join(COORD_FILE), SCHEMA, migrate);
        let loaded = db.with(|conn| {
            let mut leases = Vec::new();
            let mut statement = conn.prepare("SELECT body FROM leases")?;
            for body in statement.query_map([], |row| row.get::<_, String>(0))? {
                if let Ok(lease) = serde_json::from_str::<Lease>(&body?) {
                    leases.push(lease);
                }
            }
            let mut waiters = Vec::new();
            let mut statement = conn.prepare("SELECT body, seq FROM lease_waiters ORDER BY seq")?;
            let mut last = 0;
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })? {
                let (body, seq) = row?;
                last = last.max(seq);
                if let Ok(waiter) = serde_json::from_str::<Waiter>(&body) {
                    waiters.push(waiter);
                }
            }
            Ok((leases, waiters, last))
        });
        let (book, seq) = match loaded {
            Ok((leases, waiters, seq)) => (Book::restore(leases, waiters), seq),
            Err(e) => {
                eprintln!("fw hub: leases start empty: {e}");
                (Book::default(), 0)
            }
        };
        Self {
            book: Mutex::new(book),
            changed: Condvar::new(),
            db,
            seq: Mutex::new(seq),
        }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write `changes` through to the store.
    fn persist(&self, changes: &[Change]) {
        if changes.is_empty() {
            return;
        }
        let mut seq = self.seq.lock().unwrap_or_else(PoisonError::into_inner);
        let written = self.db.transaction(|tx| {
            for change in changes {
                match change {
                    Change::Granted { lease, waiter } => {
                        if let Some(waiter) = waiter {
                            tx.execute("DELETE FROM lease_waiters WHERE id = ?1", params![waiter.id])?;
                        }
                        put_lease(tx, lease)?;
                    }
                    Change::Renewed(lease) => put_lease(tx, lease)?,
                    Change::Queued(waiter) => {
                        *seq += 1;
                        tx.execute(
                            "INSERT OR REPLACE INTO lease_waiters (id, seq, body) VALUES (?1, ?2, ?3)",
                            params![waiter.id, *seq, json(waiter)],
                        )?;
                    }
                    Change::Ended { lease, .. } => {
                        tx.execute("DELETE FROM leases WHERE id = ?1", params![lease.id])?;
                    }
                    Change::WaitEnded { waiter, .. } => {
                        tx.execute("DELETE FROM lease_waiters WHERE id = ?1", params![waiter.id])?;
                    }
                }
            }
            Ok(())
        });
        if let Err(e) = written {
            eprintln!("fw hub: leases: not saved: {e}");
        }
    }

    /// A waiter changed in place (its wake flag).
    fn persist_waiter(&self, waiter: &Waiter) {
        let written = self.db.with(|conn| {
            conn.execute(
                "UPDATE lease_waiters SET body = ?2 WHERE id = ?1",
                params![waiter.id, json(waiter)],
            )
        });
        if let Err(e) = written {
            eprintln!("fw hub: leases: not saved: {e}");
        }
    }
}

fn put_lease(tx: &rusqlite::Transaction<'_>, lease: &Lease) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO leases (id, body) VALUES (?1, ?2)",
        params![lease.id, json(lease)],
    )?;
    Ok(())
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Every lease and wait, as the API and `list` show them.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseView {
    pub leases: Vec<Lease>,
    pub waiters: Vec<Waiter>,
}

/// What an acquire answered, after any wait in the call.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    Granted {
        lease: Lease,
    },
    Renewed {
        lease: Lease,
    },
    /// In line: the grant is collected by its waiter id (or, for a
    /// session, arrives as a prompt when `wake`).
    Waiting {
        waiter: Waiter,
        place: usize,
        holders: Vec<Lease>,
    },
    /// Not free, and the ask would not wait.
    Busy {
        holders: Vec<Lease>,
    },
}

impl Hub {
    pub(crate) fn lease_runtime(&self) -> &LeaseRuntime {
        &self.leases
    }

    /// Ask for a lease, waiting in the call at most `block_secs`
    /// ([`MAX_BLOCK_SECS`] at most). A session's ask still in line after
    /// that is granted as a prompt (`wake`); any other holder collects
    /// it with [`Hub::lease_collect`].
    pub fn lease_acquire(&self, ask: Ask, block_secs: u32) -> Result<Outcome, HubError> {
        if let Some(session) = ask.holder.session() {
            let record = self.store().session(session)?.clone();
            if record.state != SessionState::Running {
                return Err(StateError::Invalid(format!(
                    "{} is not running: a lease ends with its holder",
                    record.name
                ))
                .into());
            }
        }
        let mut ask = ask;
        ask.within
            .extend(within(&ask.holder, &crate::machines::host_name()));
        let runtime = self.lease_runtime();
        let wants_wake = matches!(ask.holder, Holder::Session { .. });
        let wait_secs = ask.wait_secs;
        let block = block_secs
            .min(self.options.lease_block_secs)
            .min(MAX_BLOCK_SECS)
            .min(wait_secs);
        let (got, changes) = {
            let mut book = runtime.book();
            book.acquire(ask, now_ms(), &mut new_id)
                .map_err(StateError::Invalid)?
        };
        self.lease_changed(changes);
        let (waiter, place) = match got {
            Acquired::Granted(lease) => return Ok(Outcome::Granted { lease }),
            Acquired::Renewed(lease) => return Ok(Outcome::Renewed { lease }),
            Acquired::Waiting {
                place: 0, holders, ..
            } => return Ok(Outcome::Busy { holders }),
            Acquired::Waiting { waiter, place, .. } => (waiter, place),
        };
        let whole_wait_in_call = block >= wait_secs;
        match self.lease_wait(&waiter.id, block) {
            Ok(Some(lease)) => return Ok(Outcome::Granted { lease }),
            Ok(None) => {}
            // Out of line without a grant. When the whole wait was this
            // call's, that is its deadline passing: the expiry tick may get
            // there before the call's own wait ends (or even starts, when
            // saving the ask was slow). Not free, not an error.
            Err(_) if whole_wait_in_call => {}
            Err(e) => return Err(e),
        }
        // Its whole wait was in the call: out of line, not granted.
        if whole_wait_in_call {
            let _ = self.lease_cancel_wait(None, &waiter.id);
            if let Some(lease) = runtime.book().collect(&waiter.id) {
                return Ok(Outcome::Granted { lease });
            }
            return Ok(Outcome::Busy {
                holders: runtime.book().holders(&waiter.resource),
            });
        }
        // Still in line after the call's wait: a session's grant becomes a
        // prompt.
        let waiter = if wants_wake {
            let marked = runtime.book().set_wake(&waiter.id);
            match marked {
                Some(waiter) => {
                    runtime.persist_waiter(&waiter);
                    waiter
                }
                // Granted or gone in between.
                None => {
                    return match runtime.book().collect(&waiter.id) {
                        Some(lease) => Ok(Outcome::Granted { lease }),
                        None => Err(StateError::Conflict(format!(
                            "the wait for {} ended before it was granted",
                            waiter.resource
                        ))
                        .into()),
                    };
                }
            }
        } else {
            waiter
        };
        let (place, holders) = {
            let book = runtime.book();
            let place = book
                .waiters()
                .filter(|w| w.resource == waiter.resource)
                .position(|w| w.id == waiter.id)
                .map_or(place, |at| at + 1);
            (place, book.holders(&waiter.resource))
        };
        Ok(Outcome::Waiting {
            waiter,
            place,
            holders,
        })
    }

    /// The `lease` agent tool, for agent session `caller`.
    pub fn agent_lease(
        &self,
        caller: &str,
        args: &crate::telemetry::mcp::LeaseArgs,
    ) -> Result<String, HubError> {
        let me = Holder::Session {
            id: caller.to_owned(),
        };
        let until = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .map(|t| t.format("%H:%M:%S UTC").to_string())
                .unwrap_or_default()
        };
        let holders_text = |holders: &[Lease]| {
            holders
                .iter()
                .map(|l| self.lease_line(l))
                .collect::<Vec<_>>()
                .join("; ")
        };
        match args.action.as_str() {
            "acquire" => {
                let resource = args
                    .resource
                    .clone()
                    .ok_or_else(|| StateError::Invalid("acquire needs resource".into()))?;
                let mode = crate::leases::Mode::parse(args.mode.as_deref().unwrap_or(""))
                    .map_err(StateError::Invalid)?;
                let wait = args.wait_secs.unwrap_or(0);
                let outcome = self.lease_acquire(
                    Ask {
                        resource,
                        mode,
                        holder: me,
                        ttl_secs: args.ttl_secs.unwrap_or(crate::leases::DEFAULT_TTL_SECS),
                        wait_secs: wait,
                        note: args.note.clone(),
                        within: Vec::new(),
                    },
                    MAX_BLOCK_SECS,
                )?;
                Ok(match outcome {
                    Outcome::Granted { lease } => format!(
                        "Granted {} on {} ({}) until {}{}. Release it when done.",
                        lease.short(),
                        lease.resource,
                        lease.mode.as_str(),
                        until(lease.expires_at),
                        if lease.waited_ms > 0 {
                            format!(" (waited {})", super::starts::span(lease.waited_ms))
                        } else {
                            String::new()
                        }
                    ),
                    Outcome::Renewed { lease } => format!(
                        "Renewed {} on {}: yours until {}.",
                        lease.short(),
                        lease.resource,
                        until(lease.expires_at)
                    ),
                    Outcome::Busy { holders } => format!(
                        "Not free: held by {}. Ask with wait_secs to wait in line.",
                        holders_text(&holders)
                    ),
                    Outcome::Waiting {
                        waiter,
                        place,
                        holders,
                    } => format!(
                        "Waiting: #{place} in line for {} ({}); held by {}. The grant arrives \
                         as your next prompt; you give up at {}. Keep working or end your turn.",
                        waiter.resource,
                        waiter.mode.as_str(),
                        holders_text(&holders),
                        until(waiter.deadline)
                    ),
                })
            }
            "release" => {
                let reference = args
                    .lease
                    .clone()
                    .or_else(|| args.resource.clone())
                    .ok_or_else(|| StateError::Invalid("release needs resource or lease".into()))?;
                // A wait of mine on that resource goes too.
                if let Ok(resource) = crate::leases::validate_resource(&reference) {
                    let waits: Vec<String> = self
                        .leases(Some(&resource))
                        .waiters
                        .into_iter()
                        .filter(|w| w.holder == me)
                        .map(|w| w.id)
                        .collect();
                    if !waits.is_empty()
                        && !self
                            .leases(Some(&resource))
                            .leases
                            .iter()
                            .any(|l| l.holder == me)
                    {
                        for id in waits {
                            self.lease_cancel_wait(Some(&me), &id)?;
                        }
                        return Ok(format!("Stopped waiting for {resource}."));
                    }
                }
                let lease = self.lease_release(Some(&me), &reference, false)?;
                Ok(format!(
                    "Released {} on {} (held {}).",
                    lease.short(),
                    lease.resource,
                    super::starts::span(now_ms() - lease.granted_at)
                ))
            }
            "list" => {
                let view = self.leases(args.resource.as_deref());
                if view.leases.is_empty() && view.waiters.is_empty() {
                    return Ok("No leases.".to_owned());
                }
                let mut lines: Vec<String> =
                    view.leases.iter().map(|l| self.lease_line(l)).collect();
                for w in &view.waiters {
                    lines.push(format!(
                        "waiting: {} {} for {} ({}, {})",
                        w.short(),
                        self.holder_name(&w.holder),
                        w.resource,
                        w.mode.as_str(),
                        super::starts::span(now_ms() - w.queued_at)
                    ));
                }
                Ok(lines.join("\n"))
            }
            other => Err(StateError::Invalid(format!(
                "action {other:?}: acquire, release, or list"
            ))
            .into()),
        }
    }

    /// A holder in words: a session by name.
    fn holder_name(&self, holder: &Holder) -> String {
        let name = |id: &str| {
            self.store()
                .session(id)
                .map_or_else(|_| id.chars().take(8).collect(), |r| r.name.clone())
        };
        match holder {
            Holder::Session { id } => name(id),
            Holder::Process {
                session: Some(id),
                label,
                ..
            } => format!("{label} in {}", name(id)),
            other => other.label(),
        }
    }

    /// One lease in words.
    fn lease_line(&self, lease: &Lease) -> String {
        format!(
            "{} {} {} ({}, {} held, {} left{})",
            lease.short(),
            lease.resource,
            self.holder_name(&lease.holder),
            lease.mode.as_str(),
            super::starts::span(now_ms() - lease.granted_at),
            super::starts::span(lease.expires_at - now_ms()),
            lease
                .note
                .as_deref()
                .map(|n| format!(", {n}"))
                .unwrap_or_default()
        )
    }

    /// Wait up to `secs` for waiter `id`'s grant (its lease), on the
    /// condition variable; `None` while it still waits; an error once it
    /// is out of line without one.
    pub fn lease_wait(&self, id: &str, secs: u32) -> Result<Option<Lease>, HubError> {
        let runtime = self.lease_runtime();
        let deadline = std::time::Instant::now() + Duration::from_secs(u64::from(secs));
        let mut book = runtime.book();
        loop {
            if let Some(lease) = book.collect(id) {
                return Ok(Some(lease));
            }
            if !book.is_waiting(id) {
                return Err(StateError::NotFound(format!(
                    "lease wait {id} (it timed out, was cancelled, or its holder ended)"
                ))
                .into());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            book = runtime
                .changed
                .wait_timeout(book, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Release lease `reference` (an id, a unique id prefix, or a resource
    /// `by` holds). `by`: the holder asking, or (`None`) the operator;
    /// `break_it`: the operator ending someone else's lease.
    pub fn lease_release(
        &self,
        by: Option<&Holder>,
        reference: &str,
        break_it: bool,
    ) -> Result<Lease, HubError> {
        let runtime = self.lease_runtime();
        let wanted = reference.trim().trim_start_matches('l').to_owned();
        let resource = crate::leases::validate_resource(reference).ok();
        let changes = {
            let mut book = runtime.book();
            let found: Vec<Lease> = book
                .leases()
                .filter(|l| {
                    (wanted.len() >= 4 && l.id.starts_with(&wanted))
                        || (resource.as_deref() == Some(l.resource.as_str())
                            && by.is_some_and(|b| b.key() == l.holder.key()))
                })
                .cloned()
                .collect();
            let lease = match found.as_slice() {
                [] => {
                    return Err(StateError::NotFound(format!("a lease {reference:?}")).into());
                }
                [one] => one.clone(),
                _ => {
                    return Err(StateError::Invalid(format!(
                        "{reference:?} matches more than one lease: give more of its id"
                    ))
                    .into());
                }
            };
            let mine = by.is_some_and(|b| b.key() == lease.holder.key());
            let owner = by
                .and_then(Holder::session)
                .is_some_and(|caller| self.owns_lease_holder(caller, &lease.holder));
            let why = if mine || owner {
                Gone::Released
            } else if by.is_none() && break_it {
                Gone::Broken
            } else if by.is_none() && lease.holder == Holder::Operator {
                Gone::Released
            } else {
                return Err(StateError::Invalid(format!(
                    "lease {} on {} is held by {}: only it, its owner, or the operator (fw hub \
                     lease break) may end it",
                    lease.short(),
                    lease.resource,
                    lease.holder.label()
                ))
                .into());
            };
            book.end(&lease.id, why, now_ms()).unwrap_or_default()
        };
        let ended = changes.iter().find_map(|c| match c {
            Change::Ended { lease, .. } => Some(lease.clone()),
            _ => None,
        });
        self.lease_changed(changes);
        ended.ok_or_else(|| StateError::NotFound(format!("a lease {reference:?}")).into())
    }

    /// Take back wait `id` (its holder, or the operator).
    pub fn lease_cancel_wait(&self, by: Option<&Holder>, id: &str) -> Result<Waiter, HubError> {
        let runtime = self.lease_runtime();
        let changes = {
            let mut book = runtime.book();
            let waiter = book
                .waiter(id)
                .cloned()
                .ok_or_else(|| StateError::NotFound(format!("lease wait {id}")))?;
            if by.is_some_and(|b| b.key() != waiter.holder.key()) {
                return Err(StateError::Invalid(format!("wait {id} is not yours")).into());
            }
            book.end_wait(id, Gone::Cancelled, now_ms())
                .unwrap_or_default()
        };
        let waiter = changes.iter().find_map(|c| match c {
            Change::WaitEnded { waiter, .. } => Some(waiter.clone()),
            _ => None,
        });
        self.lease_changed(changes);
        waiter.ok_or_else(|| StateError::NotFound(format!("lease wait {id}")).into())
    }

    /// Whether agent session `caller` owns the session `holder` belongs to
    /// (a lead releasing its worker's lease).
    fn owns_lease_holder(&self, caller: &str, holder: &Holder) -> bool {
        let Some(session) = holder.session() else {
            return false;
        };
        let store = self.store();
        store.session(session).is_ok_and(|record| {
            super::endings::effective_owner(record, store.sessions()).as_deref() == Some(caller)
        })
    }

    /// Every lease and wait (`resource`: only its).
    pub fn leases(&self, resource: Option<&str>) -> LeaseView {
        let book = self.lease_runtime().book();
        let keep = |r: &str| resource.is_none_or(|want| want == r);
        LeaseView {
            leases: book
                .leases()
                .filter(|l| keep(&l.resource))
                .cloned()
                .collect(),
            waiters: book
                .waiters()
                .filter(|w| keep(&w.resource))
                .cloned()
                .collect(),
        }
    }

    /// The leases and waits session `id` (or a process of it) has.
    pub fn leases_of(&self, id: &str) -> LeaseView {
        let book = self.lease_runtime().book();
        LeaseView {
            leases: book
                .leases()
                .filter(|l| l.holder.session() == Some(id))
                .cloned()
                .collect(),
            waiters: book
                .waiters()
                .filter(|w| w.holder.session() == Some(id))
                .cloned()
                .collect(),
        }
    }

    /// Session `id` ended (exited for good, or was removed): its leases
    /// and waits go.
    pub(crate) fn drop_leases_of(&self, id: &str) {
        let changes = self
            .lease_runtime()
            .book()
            .drop_holders(|h| h.session() == Some(id), now_ms());
        self.lease_changed(changes);
    }

    /// One look: time that ran out, and processes on this machine that
    /// exited.
    pub(crate) fn lease_tick(&self) {
        let host = crate::machines::host_name();
        let now = now_ms();
        let mut changes = {
            let mut book = self.lease_runtime().book();
            let mut changes = book.tick(now);
            changes.extend(book.drop_holders(|h| process_gone(h, &host), now));
            changes
        };
        // Sessions that are gone (removed while the hub was down).
        let gone: Vec<String> = {
            let book = self.lease_runtime().book();
            let store = self.store();
            book.leases()
                .map(|l| &l.holder)
                .chain(book.waiters().map(|w| &w.holder))
                .filter_map(Holder::session)
                // Removed (an exit drops them where it happens; a hub
                // stop keeps them for the session's restore).
                .filter(|s| store.session(s).is_err())
                .map(str::to_owned)
                .collect()
        };
        for id in gone {
            changes.extend(
                self.lease_runtime()
                    .book()
                    .drop_holders(|h| h.session() == Some(id.as_str()), now),
            );
        }
        self.lease_changed(changes);
    }

    /// Write, log, and tell `changes`.
    fn lease_changed(&self, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        let runtime = self.lease_runtime();
        runtime.persist(&changes);
        runtime.changed.notify_all();
        for change in changes {
            let (event, lease_like, extra) = match &change {
                Change::Granted { lease, waiter } => {
                    if let Some(waiter) = waiter.as_ref().filter(|w| w.wake) {
                        self.lease_wake(waiter, lease);
                    }
                    ("lease.granted", serde_json::to_value(lease), None)
                }
                Change::Renewed(lease) => ("lease.renewed", serde_json::to_value(lease), None),
                Change::Queued(waiter) => ("lease.waiting", serde_json::to_value(waiter), None),
                Change::Ended { lease, why } => {
                    if matches!(why, Gone::Expired | Gone::Broken)
                        && let Holder::Session { id } = &lease.holder
                    {
                        self.lease_tell(id, lease, *why);
                    }
                    (why.event(), serde_json::to_value(lease), None)
                }
                Change::WaitEnded { waiter, why } => (
                    why.event(),
                    serde_json::to_value(waiter),
                    Some(serde_json::json!(true)),
                ),
            };
            let Ok(mut data) = lease_like else { continue };
            if extra.is_some() {
                data["wait"] = serde_json::Value::Bool(true);
            }
            let session = data["holder"]["id"]
                .as_str()
                .or_else(|| data["holder"]["session"].as_str())
                .map(str::to_owned);
            self.log_hub_event(event, session.as_deref(), data);
        }
        self.emit(super::HubEvent::Sessions);
    }

    /// A long wait was granted: its session hears it as its next prompt
    /// (priority; a later grant on the same resource replaces it unread).
    fn lease_wake(&self, waiter: &Waiter, lease: &Lease) {
        let Holder::Session { id } = &waiter.holder else {
            return;
        };
        let text = format!(
            "From the flywheel hub: your lease on {} ({}) is granted: {} until {} UTC \
             (waited {}). Do the work that needed it now, then release it (lease action \
             release, resource {}).",
            lease.resource,
            lease.mode.as_str(),
            lease.short(),
            chrono::DateTime::from_timestamp_millis(lease.expires_at)
                .map(|t| t.format("%H:%M:%S").to_string())
                .unwrap_or_default(),
            super::starts::span(lease.waited_ms),
            lease.resource
        );
        let topic = format!("lease:{}", lease.resource);
        let item = crate::queue::NewItem {
            text,
            when: None,
            topic: Some(topic),
        };
        if let Err(e) = self.queue_add(id, &item) {
            eprintln!("fw hub: lease {}: grant not told to {id}: {e}", lease.id);
        }
    }

    /// Tell a session its lease ended without it releasing it.
    fn lease_tell(&self, id: &str, lease: &Lease, why: Gone) {
        let text = format!(
            "From the flywheel hub: your lease {} on {} {}. Ask again (lease action acquire) \
             if you still need it.",
            lease.short(),
            lease.resource,
            match why {
                Gone::Expired => format!("expired after {} s (ttl_secs)", lease.ttl_secs),
                _ => "was broken by the operator".to_owned(),
            }
        );
        let item = crate::queue::NewItem {
            text,
            when: None,
            topic: Some(format!("lease:{}", lease.resource)),
        };
        if let Err(e) = self.queue_add(id, &item) {
            eprintln!("fw hub: lease {}: end not told to {id}: {e}", lease.id);
        }
    }

    /// Look at leases every [`TICK`] (its own thread).
    pub(crate) fn start_leases(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-leases".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    if !hub.queue_runtime().is_paused() {
                        hub.lease_tick();
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: lease expiry is off: cannot start its thread: {e}");
        }
    }
}

/// The holder keys `holder` runs inside: for a process on this machine
/// (`host`), its ancestor processes. One of them holding what it asks for
/// makes the ask re-entrant. (A session holding it is not: the session
/// may release it while a process it started in the background waits.)
fn within(holder: &Holder, host: &str) -> Vec<String> {
    match holder {
        Holder::Process {
            host: theirs, pid, ..
        } if theirs == host => crate::procs::ancestors(*pid)
            .into_iter()
            .map(|up| format!("p:{host}:{up}:{}", process_start(up).unwrap_or(0)))
            .collect(),
        _ => Vec::new(),
    }
}

/// A process holder on this machine that is no longer running (or whose
/// pid now belongs to another process).
fn process_gone(holder: &Holder, host: &str) -> bool {
    let Holder::Process {
        host: theirs,
        pid,
        started,
        ..
    } = holder
    else {
        return false;
    };
    if theirs != host {
        return false;
    }
    let Ok(pid_t) = libc::pid_t::try_from(*pid) else {
        return true;
    };
    // SAFETY: signal 0 only checks that the pid exists.
    let alive = unsafe { libc::kill(pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return true;
    }
    match (started, process_start(*pid)) {
        (Some(want), Some(now)) => *want != now,
        _ => false,
    }
}

/// A process's start time (clock ticks since boot), on Linux.
pub fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22; the command (field 2) may hold spaces, so count from
    // after its closing parenthesis.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}
