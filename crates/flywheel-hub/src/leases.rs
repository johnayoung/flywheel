//! Leases: a named resource (`cpu:test-suite`, `node:desktop:heavy`,
//! `host:prod-read`) held shared or exclusive by a session, a process, or
//! the operator, for a time to live. Waiting is first come, first served
//! per resource and bounded by the asker's deadline: a shared ask behind a
//! waiting exclusive one waits too (no writer starves). A lease ends when
//! released, when its time runs out (acquire again to renew), when its
//! holder ends, or when the operator breaks it.
//!
//! [`Book`] is the rules, pure and clocked by its caller; the hub side
//! (`crate::hub::leases`) keeps one under a lock, writes every change
//! through to `coord.db`, logs it, wakes blocked callers, and turns a
//! long wait's grant into a prompt.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Longest a call waits in the call (Codex's tool timeout is 60 s).
pub const MAX_BLOCK_SECS: u32 = 50;
/// Longest wait in all (beyond [`MAX_BLOCK_SECS`]: a prompt on grant).
pub const MAX_WAIT_SECS: u32 = 86_400;
/// Time to live bounds, and the default.
pub const MIN_TTL_SECS: u32 = 30;
pub const MAX_TTL_SECS: u32 = 86_400;
pub const DEFAULT_TTL_SECS: u32 = 1800;
/// Leases and waits one holder may have together.
pub const MAX_PER_HOLDER: usize = 16;
/// Leases and waits on one hub.
pub const MAX_HUB: usize = 10_000;
/// The longest resource name and note.
pub const MAX_RESOURCE_CHARS: usize = 128;
pub const MAX_NOTE_CHARS: usize = 200;
/// How long a granted wait is remembered for a caller to collect.
const COLLECT_MS: i64 = 10 * 60 * 1000;

/// Fewer released holds of a resource than this, and its typical hold
/// is not known: [`DEFAULT_STUCK_MS`] stands in for the threshold.
pub const MIN_SAMPLES: usize = 5;
/// The stuck threshold of a resource with no history (ms): an hour.
pub const DEFAULT_STUCK_MS: i64 = 60 * 60 * 1000;
/// Never stuck before this (ms): ten minutes over the typical hold.
pub const STUCK_MARGIN_MS: i64 = 10 * 60 * 1000;

/// The 90th percentile of `held` (ms): `None` with fewer than
/// [`MIN_SAMPLES`].
pub fn p90(held: &[i64]) -> Option<i64> {
    if held.len() < MIN_SAMPLES {
        return None;
    }
    let mut sorted = held.to_vec();
    sorted.sort_unstable();
    // Nearest rank: the smallest value at or above 90% of them.
    let rank = (sorted.len() * 9).div_ceil(10).max(1);
    Some(sorted[rank - 1])
}

/// How long a lease may be held while someone waits before it reads as
/// stuck (ms): `max(3 x p90, p90 + margin)` (`margin`: normally
/// [`STUCK_MARGIN_MS`]), or `unknown` (normally [`DEFAULT_STUCK_MS`])
/// with no typical hold to go by.
pub fn stuck_after(p90: Option<i64>, margin: i64, unknown: i64) -> i64 {
    match p90 {
        Some(p) => (3 * p).max(p + margin),
        None => unknown,
    }
}

/// How a lease is held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Shared,
    #[default]
    Exclusive,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "exclusive" => Ok(Self::Exclusive),
            "shared" => Ok(Self::Shared),
            other => Err(format!("mode {other:?}: shared or exclusive")),
        }
    }
}

/// Who holds (or waits for) a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Holder {
    /// An agent session (the `lease` tool).
    Session {
        id: String,
    },
    /// A process (`fw hub lease`): gone when it exits (on this hub's
    /// machine) or when its lease's time runs out; with `session`, also
    /// when that session ends.
    Process {
        host: String,
        pid: u32,
        /// Its start time (`/proc/<pid>/stat`, clock ticks): a reused
        /// pid is not it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        label: String,
    },
    Operator,
}

impl Holder {
    /// The key one holder is known by.
    pub fn key(&self) -> String {
        match self {
            Self::Session { id } => format!("s:{id}"),
            Self::Process {
                host, pid, started, ..
            } => format!("p:{host}:{pid}:{}", started.unwrap_or(0)),
            Self::Operator => "operator".to_owned(),
        }
    }

    /// The session it belongs to, if any.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Session { id } => Some(id),
            Self::Process { session, .. } => session.as_deref(),
            Self::Operator => None,
        }
    }

    /// In words.
    pub fn label(&self) -> String {
        match self {
            Self::Session { id } => id.chars().take(8).collect(),
            Self::Process { label, pid, .. } => format!("{label} (pid {pid})"),
            Self::Operator => "operator".to_owned(),
        }
    }
}

/// A lease held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub resource: String,
    pub mode: Mode,
    pub holder: Holder,
    /// Unix ms.
    pub granted_at: i64,
    pub expires_at: i64,
    pub ttl_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// How long it waited before it was granted (ms).
    #[serde(default)]
    pub waited_ms: i64,
}

impl Lease {
    pub fn short(&self) -> String {
        short(&self.id, 'l')
    }
}

/// An ask waiting its turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiter {
    pub id: String,
    pub resource: String,
    pub mode: Mode,
    pub holder: Holder,
    pub ttl_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub queued_at: i64,
    /// Given up at (unix ms).
    pub deadline: i64,
    /// Its grant goes to the holder as a prompt (the call that asked has
    /// returned).
    #[serde(default)]
    pub wake: bool,
}

impl Waiter {
    pub fn short(&self) -> String {
        short(&self.id, 'q')
    }
}

fn short(id: &str, prefix: char) -> String {
    format!("{prefix}{}", &id[..7.min(id.len())])
}

/// An ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub resource: String,
    pub mode: Mode,
    pub holder: Holder,
    pub ttl_secs: u32,
    pub wait_secs: u32,
    pub note: Option<String>,
    /// The holder keys ([`Holder::key`]) the asker runs inside: its
    /// ancestor processes. One of them holding the
    /// resource makes the ask re-entrant (see [`Book::acquire`]).
    pub within: Vec<String>,
}

/// What an ask got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    Granted(Lease),
    /// The holder had it already: its time starts again.
    Renewed(Lease),
    /// In line: 1-based place, and who holds it now.
    Waiting {
        waiter: Waiter,
        place: usize,
        holders: Vec<Lease>,
    },
}

/// Why a lease or a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gone {
    Released,
    Expired,
    /// Its holder ended (or its process exited).
    Dropped,
    Broken,
    /// A wait whose deadline passed.
    TimedOut,
    /// A wait taken back.
    Cancelled,
}

impl Gone {
    /// The event a lease (or wait) ending this way is.
    pub fn event(self) -> &'static str {
        match self {
            Self::Released => "lease.released",
            Self::Expired => "lease.expired",
            Self::Dropped => "lease.dropped",
            Self::Broken => "lease.broken",
            Self::TimedOut => "lease.timed_out",
            Self::Cancelled => "lease.cancelled",
        }
    }
}

/// One change the book made: what the hub writes, logs, and tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Granted {
        lease: Lease,
        waiter: Option<Waiter>,
    },
    Renewed(Lease),
    Queued(Waiter),
    Ended {
        lease: Lease,
        why: Gone,
    },
    WaitEnded {
        waiter: Waiter,
        why: Gone,
    },
}

/// The leases and waits of one hub.
#[derive(Debug, Default, Clone)]
pub struct Book {
    /// By id.
    leases: BTreeMap<String, Lease>,
    /// Per resource, in line.
    lines: BTreeMap<String, VecDeque<Waiter>>,
    /// Waits granted, for their callers to collect: waiter id -> lease
    /// id, and when.
    granted: HashMap<String, (String, i64)>,
}

/// A resource name: 1-128 of `a-z 0-9 . _ : / -`, starting alnum.
pub fn validate_resource(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_ascii_lowercase();
    let ok = name.chars().count() <= MAX_RESOURCE_CHARS
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if ok {
        Ok(name)
    } else {
        Err(format!(
            "resource {raw:?}: 1-{MAX_RESOURCE_CHARS} of a-z 0-9 . _ : / -, starting with a \
             letter or digit (cpu:test-suite, node:desktop:heavy)"
        ))
    }
}

impl Book {
    /// A book holding `leases` and `waiters` (as stored; in line order).
    pub fn restore(leases: Vec<Lease>, waiters: Vec<Waiter>) -> Self {
        let mut book = Self::default();
        for lease in leases {
            book.leases.insert(lease.id.clone(), lease);
        }
        for waiter in waiters {
            book.lines
                .entry(waiter.resource.clone())
                .or_default()
                .push_back(waiter);
        }
        book
    }

    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.leases.values()
    }

    pub fn waiters(&self) -> impl Iterator<Item = &Waiter> {
        self.lines.values().flatten()
    }

    pub fn lease(&self, id: &str) -> Option<&Lease> {
        self.leases.get(id)
    }

    pub fn waiter(&self, id: &str) -> Option<&Waiter> {
        self.waiters().find(|w| w.id == id)
    }

    /// The holders of `resource`.
    pub fn holders(&self, resource: &str) -> Vec<Lease> {
        self.leases
            .values()
            .filter(|l| l.resource == resource)
            .cloned()
            .collect()
    }

    /// A wait granted: its lease (once; `None` while it waits or after it
    /// ended).
    pub fn collect(&mut self, waiter: &str) -> Option<Lease> {
        let (lease, _) = self.granted.remove(waiter)?;
        self.leases.get(&lease).cloned()
    }

    /// Whether `waiter` still waits.
    pub fn is_waiting(&self, waiter: &str) -> bool {
        self.waiter(waiter).is_some()
    }

    /// Mark a waiter's grant as a prompt to its holder.
    pub fn set_wake(&mut self, waiter: &str) -> Option<Waiter> {
        let found = self.lines.values_mut().flatten().find(|w| w.id == waiter)?;
        found.wake = true;
        Some(found.clone())
    }

    fn held_by(&self, key: &str) -> usize {
        self.leases
            .values()
            .filter(|l| l.holder.key() == key)
            .count()
            + self.waiters().filter(|w| w.holder.key() == key).count()
    }

    fn total(&self) -> usize {
        self.leases.len() + self.waiters().count()
    }

    /// Ask for a lease at `now` (unix ms); ids from `new_id`.
    pub fn acquire(
        &mut self,
        ask: Ask,
        now: i64,
        new_id: &mut dyn FnMut() -> String,
    ) -> Result<(Acquired, Vec<Change>), String> {
        let resource = validate_resource(&ask.resource)?;
        if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&ask.ttl_secs) {
            return Err(format!(
                "ttl_secs is {MIN_TTL_SECS}-{MAX_TTL_SECS}: how long it may be held before it \
                 must be renewed"
            ));
        }
        if ask.wait_secs > MAX_WAIT_SECS {
            return Err(format!("wait_secs is at most {MAX_WAIT_SECS}"));
        }
        let note = ask
            .note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(MAX_NOTE_CHARS).collect::<String>());
        let key = ask.holder.key();
        if let Some(held) = self
            .leases
            .values_mut()
            .find(|l| l.resource == resource && l.holder.key() == key)
        {
            if held.mode != ask.mode {
                return Err(format!(
                    "you hold {resource} {} ({}): release it before asking for it {}",
                    held.mode.as_str(),
                    held.short(),
                    ask.mode.as_str()
                ));
            }
            held.expires_at = now + i64::from(ask.ttl_secs) * 1000;
            held.ttl_secs = ask.ttl_secs;
            if note.is_some() {
                held.note = note;
            }
            let lease = held.clone();
            return Ok((
                Acquired::Renewed(lease.clone()),
                vec![Change::Renewed(lease)],
            ));
        }
        // Re-entrant: a process the asker runs inside (an ancestor)
        // holds it already. Waiting would wait
        // on itself: the holder cannot release before the asker returns.
        // Exclusive on either side is an error: two "only one" holders at
        // once would be a lie, and the ask is nearly always a bug (a test
        // taking the suite lock its own suite holds), best said at once.
        // Both shared is granted beside the ancestor's, past the line (an
        // exclusive ask in line waits on that ancestor too).
        let mut nested = false;
        if let Some(held) = self
            .leases
            .values()
            .find(|l| l.resource == resource && ask.within.contains(&l.holder.key()))
        {
            if held.mode == Mode::Exclusive || ask.mode == Mode::Exclusive {
                return Err(format!(
                    "{resource} is already held by your ancestor {} ({}, {}): {} asks for it \
                     from inside that hold, so waiting would wait on itself. Use the hold you \
                     are in (scripts/check.sh: FLYWHEEL_CHECK_LOCK=0 for nested runs)",
                    held.holder.label(),
                    held.short(),
                    held.mode.as_str(),
                    ask.holder.label()
                ));
            }
            nested = true;
        }
        if let Some((at, waiting)) = self.lines.get_mut(&resource).and_then(|line| {
            line.iter_mut()
                .enumerate()
                .find(|(_, w)| w.holder.key() == key)
        }) {
            // Asked again while in line: the same place, the later deadline.
            waiting.deadline = waiting.deadline.max(now + i64::from(ask.wait_secs) * 1000);
            let waiter = waiting.clone();
            let holders = self.holders(&resource);
            return Ok((
                Acquired::Waiting {
                    waiter,
                    place: at + 1,
                    holders,
                },
                Vec::new(),
            ));
        }
        if self.held_by(&key) >= MAX_PER_HOLDER {
            return Err(format!(
                "{} holds or waits for {MAX_PER_HOLDER} leases already: release some first",
                ask.holder.label()
            ));
        }
        if self.total() >= MAX_HUB {
            return Err(format!("this hub has {MAX_HUB} leases and waits already"));
        }
        let line_empty = self.lines.get(&resource).is_none_or(VecDeque::is_empty);
        if (nested || line_empty) && self.fits(&resource, ask.mode) {
            let lease = Lease {
                id: new_id(),
                resource,
                mode: ask.mode,
                holder: ask.holder,
                granted_at: now,
                expires_at: now + i64::from(ask.ttl_secs) * 1000,
                ttl_secs: ask.ttl_secs,
                note,
                waited_ms: 0,
            };
            self.leases.insert(lease.id.clone(), lease.clone());
            return Ok((
                Acquired::Granted(lease.clone()),
                vec![Change::Granted {
                    lease,
                    waiter: None,
                }],
            ));
        }
        let holders = self.holders(&resource);
        if ask.wait_secs == 0 {
            return Ok((
                Acquired::Waiting {
                    waiter: Waiter {
                        id: String::new(),
                        resource,
                        mode: ask.mode,
                        holder: ask.holder,
                        ttl_secs: ask.ttl_secs,
                        note,
                        queued_at: now,
                        deadline: now,
                        wake: false,
                    },
                    place: 0,
                    holders,
                },
                Vec::new(),
            ));
        }
        let waiter = Waiter {
            id: new_id(),
            resource: resource.clone(),
            mode: ask.mode,
            holder: ask.holder,
            ttl_secs: ask.ttl_secs,
            note,
            queued_at: now,
            deadline: now + i64::from(ask.wait_secs) * 1000,
            wake: false,
        };
        let line = self.lines.entry(resource).or_default();
        line.push_back(waiter.clone());
        let place = line.len();
        Ok((
            Acquired::Waiting {
                waiter: waiter.clone(),
                place,
                holders,
            },
            vec![Change::Queued(waiter)],
        ))
    }

    /// Whether a `mode` lease on `resource` fits beside its holders.
    fn fits(&self, resource: &str, mode: Mode) -> bool {
        let mut holders = self.leases.values().filter(|l| l.resource == resource);
        match mode {
            Mode::Exclusive => holders.next().is_none(),
            Mode::Shared => holders.all(|l| l.mode == Mode::Shared),
        }
    }

    /// Grant from the front of `resource`'s line while each fits.
    fn pump(&mut self, resource: &str, now: i64, changes: &mut Vec<Change>) {
        loop {
            let Some(head) = self.lines.get(resource).and_then(VecDeque::front) else {
                break;
            };
            if !self.fits(resource, head.mode) {
                break;
            }
            let waiter = self
                .lines
                .get_mut(resource)
                .and_then(VecDeque::pop_front)
                .expect("a head");
            let lease = Lease {
                id: format!("{}l", waiter.id),
                resource: waiter.resource.clone(),
                mode: waiter.mode,
                holder: waiter.holder.clone(),
                granted_at: now,
                expires_at: now + i64::from(waiter.ttl_secs) * 1000,
                ttl_secs: waiter.ttl_secs,
                note: waiter.note.clone(),
                waited_ms: now - waiter.queued_at,
            };
            self.leases.insert(lease.id.clone(), lease.clone());
            self.granted
                .insert(waiter.id.clone(), (lease.id.clone(), now));
            changes.push(Change::Granted {
                lease,
                waiter: Some(waiter),
            });
        }
        if self.lines.get(resource).is_some_and(VecDeque::is_empty) {
            self.lines.remove(resource);
        }
    }

    /// End lease `id` (`why`), then grant what now fits.
    pub fn end(&mut self, id: &str, why: Gone, now: i64) -> Option<Vec<Change>> {
        let lease = self.leases.remove(id)?;
        let resource = lease.resource.clone();
        let mut changes = vec![Change::Ended { lease, why }];
        self.pump(&resource, now, &mut changes);
        Some(changes)
    }

    /// Take a wait out of line (`why`), then grant what now fits (a
    /// waiting exclusive ask at the front may have held shared ones back).
    pub fn end_wait(&mut self, id: &str, why: Gone, now: i64) -> Option<Vec<Change>> {
        let (resource, at) = self.lines.iter().find_map(|(resource, line)| {
            line.iter()
                .position(|w| w.id == id)
                .map(|at| (resource.clone(), at))
        })?;
        let waiter = self.lines.get_mut(&resource)?.remove(at)?;
        let mut changes = vec![Change::WaitEnded { waiter, why }];
        self.pump(&resource, now, &mut changes);
        Some(changes)
    }

    /// Everything `gone` says is gone ends: leases `Dropped`, waits
    /// `Cancelled`.
    pub fn drop_holders(&mut self, gone: impl Fn(&Holder) -> bool, now: i64) -> Vec<Change> {
        let leases: Vec<String> = self
            .leases
            .values()
            .filter(|l| gone(&l.holder))
            .map(|l| l.id.clone())
            .collect();
        let waits: Vec<String> = self
            .waiters()
            .filter(|w| gone(&w.holder))
            .map(|w| w.id.clone())
            .collect();
        let mut changes = Vec::new();
        for id in waits {
            changes.extend(self.end_wait(&id, Gone::Cancelled, now).unwrap_or_default());
        }
        for id in leases {
            changes.extend(self.end(&id, Gone::Dropped, now).unwrap_or_default());
        }
        changes
    }

    /// Expire leases and waits whose time is up at `now`.
    pub fn tick(&mut self, now: i64) -> Vec<Change> {
        let expired: Vec<String> = self
            .leases
            .values()
            .filter(|l| l.expires_at <= now)
            .map(|l| l.id.clone())
            .collect();
        let late: Vec<String> = self
            .waiters()
            .filter(|w| w.deadline <= now)
            .map(|w| w.id.clone())
            .collect();
        let mut changes = Vec::new();
        for id in late {
            changes.extend(self.end_wait(&id, Gone::TimedOut, now).unwrap_or_default());
        }
        for id in expired {
            changes.extend(self.end(&id, Gone::Expired, now).unwrap_or_default());
        }
        self.granted.retain(|_, (_, at)| now - *at < COLLECT_MS);
        changes
    }

    /// The soonest time something is due (an expiry or a deadline).
    pub fn next_due(&self) -> Option<i64> {
        self.leases
            .values()
            .map(|l| l.expires_at)
            .chain(self.waiters().map(|w| w.deadline))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("{n:07}")
        }
    }

    fn who(name: &str) -> Holder {
        Holder::Session { id: name.into() }
    }

    fn ask(name: &str, mode: Mode, wait: u32) -> Ask {
        Ask {
            resource: "cpu:quiet".into(),
            mode,
            holder: who(name),
            ttl_secs: 60,
            wait_secs: wait,
            note: None,
            within: Vec::new(),
        }
    }

    fn granted_to(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::Granted { lease, .. } => lease.holder.session().map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    fn waiter_of(got: &Acquired) -> String {
        match got {
            Acquired::Waiting { waiter, .. } => waiter.id.clone(),
            other => panic!("not waiting: {other:?}"),
        }
    }

    #[test]
    fn first_come_first_served_and_shared_asks_never_jump_a_waiting_exclusive_one() {
        let mut book = Book::default();
        let mut id = ids();
        let (e1, _) = book
            .acquire(ask("e1", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        let Acquired::Granted(e1) = e1 else { panic!() };
        for (name, mode) in [
            ("s1", Mode::Shared),
            ("s2", Mode::Shared),
            ("e2", Mode::Exclusive),
            ("s3", Mode::Shared),
        ] {
            let (got, _) = book.acquire(ask(name, mode, 100), 1, &mut id).unwrap();
            assert!(matches!(got, Acquired::Waiting { .. }), "{name}");
        }
        let changes = book.end(&e1.id, Gone::Released, 2).unwrap();
        assert_eq!(
            granted_to(&changes),
            ["s1", "s2"],
            "shared ones go together"
        );
        let leases: Vec<String> = book.leases().map(|l| l.id.clone()).collect();
        let mut changes = Vec::new();
        for id in leases {
            changes.extend(book.end(&id, Gone::Released, 3).unwrap());
        }
        assert_eq!(granted_to(&changes), ["e2"], "s3 waits behind e2");
        let e2 = book.leases().next().unwrap().id.clone();
        assert_eq!(
            granted_to(&book.end(&e2, Gone::Released, 4).unwrap()),
            ["s3"]
        );
    }

    #[test]
    fn an_ask_from_inside_the_holder_fails_at_once_or_nests_when_both_are_shared() {
        let parent = Holder::Process {
            host: "h".into(),
            pid: 10,
            started: Some(5),
            session: None,
            label: "fw hub lease run scripts/check.sh".into(),
        };
        let child = Holder::Process {
            host: "h".into(),
            pid: 11,
            started: Some(6),
            session: None,
            label: "fw hub lease run fw".into(),
        };
        let inside = |mode, holder: &Holder| Ask {
            resource: "cpu:test-suite".into(),
            mode,
            holder: holder.clone(),
            ttl_secs: 60,
            wait_secs: 3600,
            note: None,
            within: vec![parent.key()],
        };
        for (held, asked) in [
            (Mode::Exclusive, Mode::Exclusive),
            (Mode::Exclusive, Mode::Shared),
            (Mode::Shared, Mode::Exclusive),
        ] {
            let mut book = Book::default();
            let mut id = ids();
            let mut first = inside(held, &parent);
            first.within.clear();
            book.acquire(first, 0, &mut id).unwrap();
            let e = book.acquire(inside(asked, &child), 1, &mut id).unwrap_err();
            assert!(
                e.contains("already held by your ancestor") && e.contains("pid 10"),
                "{e}"
            );
            assert_eq!(book.waiters().count(), 0, "it never waits");
        }
        // Both shared: granted beside its ancestor's, even past an
        // exclusive ask in line (which waits on the ancestor).
        let mut book = Book::default();
        let mut id = ids();
        let mut first = inside(Mode::Shared, &parent);
        first.within.clear();
        book.acquire(first, 0, &mut id).unwrap();
        let mut exclusive = ask("e", Mode::Exclusive, 100);
        exclusive.resource = "cpu:test-suite".into();
        let (got, _) = book.acquire(exclusive, 1, &mut id).unwrap();
        assert!(matches!(got, Acquired::Waiting { .. }));
        let (got, _) = book
            .acquire(inside(Mode::Shared, &child), 2, &mut id)
            .unwrap();
        assert!(matches!(got, Acquired::Granted(_)), "{got:?}");
    }

    #[test]
    fn a_hold_is_stuck_past_three_typical_ones_or_ten_minutes_over() {
        let min = 60 * 1000;
        assert_eq!(p90(&[min; 4]), None, "too few to know");
        let held: Vec<i64> = (1..=10).map(|m| m * min).collect();
        assert_eq!(p90(&held), Some(9 * min));
        let after = |p| stuck_after(p, STUCK_MARGIN_MS, DEFAULT_STUCK_MS);
        // Long holds: three of them.
        assert_eq!(after(Some(9 * min)), 27 * min);
        // Short ones: ten minutes over.
        assert_eq!(after(Some(2 * min)), 12 * min);
        assert_eq!(after(None), DEFAULT_STUCK_MS);
    }

    #[test]
    fn a_holder_asking_again_renews_and_a_mode_change_is_refused() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("a", Mode::Shared, 0), 0, &mut id).unwrap();
        let (got, _) = book
            .acquire(ask("a", Mode::Shared, 0), 30_000, &mut id)
            .unwrap();
        let Acquired::Renewed(lease) = got else {
            panic!("{got:?}")
        };
        assert_eq!(lease.expires_at, 90_000);
        assert!(
            book.acquire(ask("a", Mode::Exclusive, 0), 1, &mut id)
                .is_err()
        );
    }

    #[test]
    fn time_runs_out_for_leases_and_waits() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("a", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        let (b, _) = book
            .acquire(ask("b", Mode::Exclusive, 10), 0, &mut id)
            .unwrap();
        let b = waiter_of(&b);
        let (c, _) = book
            .acquire(ask("c", Mode::Exclusive, 120), 0, &mut id)
            .unwrap();
        let c = waiter_of(&c);
        assert_eq!(book.next_due(), Some(10_000));
        let changes = book.tick(10_000);
        assert!(
            matches!(&changes[0], Change::WaitEnded { why: Gone::TimedOut, waiter } if waiter.id == b)
        );
        let changes = book.tick(60_000);
        assert!(changes.iter().any(|c| matches!(
            c,
            Change::Ended {
                why: Gone::Expired,
                ..
            }
        )));
        assert_eq!(granted_to(&changes), ["c"]);
        assert!(
            book.collect(&c).is_some(),
            "the waiter's grant is collected"
        );
        assert!(book.collect(&c).is_none(), "once");
    }

    #[test]
    fn a_gone_holder_drops_its_leases_and_waits_and_the_line_moves() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("lead", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        book.acquire(ask("w", Mode::Exclusive, 100), 0, &mut id)
            .unwrap();
        let mut other = ask("lead", Mode::Exclusive, 100);
        other.resource = "node:x".into();
        book.acquire(other, 0, &mut id).unwrap();
        let changes = book.drop_holders(|h| h.session() == Some("lead"), 5);
        assert_eq!(granted_to(&changes), ["w"]);
        assert_eq!(book.leases().count(), 1);
    }

    #[test]
    fn taking_an_exclusive_wait_out_of_line_lets_shared_ones_through() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("s0", Mode::Shared, 0), 0, &mut id)
            .unwrap();
        let (e, _) = book
            .acquire(ask("e", Mode::Exclusive, 100), 0, &mut id)
            .unwrap();
        book.acquire(ask("s1", Mode::Shared, 100), 0, &mut id)
            .unwrap();
        let changes = book.end_wait(&waiter_of(&e), Gone::Cancelled, 1).unwrap();
        assert_eq!(granted_to(&changes), ["s1"]);
    }

    #[test]
    fn limits_and_names_are_checked() {
        let mut book = Book::default();
        let mut id = ids();
        let mut bad = ask("a", Mode::Shared, 0);
        bad.resource = "Has Space".into();
        assert!(book.acquire(bad, 0, &mut id).is_err());
        let mut short = ask("a", Mode::Shared, 0);
        short.ttl_secs = 5;
        assert!(book.acquire(short, 0, &mut id).is_err());
        for n in 0..MAX_PER_HOLDER {
            let mut each = ask("a", Mode::Shared, 0);
            each.resource = format!("r{n}");
            book.acquire(each, 0, &mut id).unwrap();
        }
        let mut one_more = ask("a", Mode::Shared, 0);
        one_more.resource = "r-last".into();
        assert!(
            book.acquire(one_more, 0, &mut id)
                .unwrap_err()
                .contains("release some")
        );
        assert_eq!(validate_resource(" CPU:Quiet ").unwrap(), "cpu:quiet");
    }

    #[test]
    fn asking_with_no_wait_does_not_queue() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("a", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        let (got, changes) = book
            .acquire(ask("b", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        assert!(matches!(got, Acquired::Waiting { place: 0, .. }));
        assert!(changes.is_empty());
        assert_eq!(book.waiters().count(), 0);
    }

    #[test]
    fn a_restored_book_keeps_its_line() {
        let mut book = Book::default();
        let mut id = ids();
        book.acquire(ask("a", Mode::Exclusive, 0), 0, &mut id)
            .unwrap();
        book.acquire(ask("b", Mode::Exclusive, 100), 0, &mut id)
            .unwrap();
        book.acquire(ask("c", Mode::Exclusive, 100), 0, &mut id)
            .unwrap();
        let leases: Vec<Lease> = book.leases().cloned().collect();
        let waiters: Vec<Waiter> = book.waiters().cloned().collect();
        let mut back = Book::restore(leases.clone(), waiters);
        let changes = back.end(&leases[0].id, Gone::Released, 1).unwrap();
        assert_eq!(granted_to(&changes), ["b"]);
    }
}
