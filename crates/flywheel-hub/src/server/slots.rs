//! Connection slots, by whether a request on the connection authenticated.
//!
//! * **Main** ([`Slots::new`]'s `main_cap`): connections whose latest
//!   request carried a valid credential (token, login cookie, peer
//!   credential). Kept alive; a stop lets their requests finish.
//! * **Share links** (as many as the unauthenticated pool, [`UNAUTH_PER_CLIENT`]
//!   per client): connections whose latest request carried a live share
//!   link's credential. A link is handed to outsiders by design, so its
//!   holders never take a main slot (they would lock the operator out):
//!   a full share pool, or a client over its share of it, is refused at
//!   once instead of waiting. Kept alive like main ones.
//! * **Unauthenticated** (`pending_cap`, much smaller): every connection
//!   starts here -- nothing is known about it before its first request
//!   head -- and leaves the moment a head carries a valid credential (it
//!   is then never evicted, even while it waits for a main slot). Each has
//!   a deadline -- its first request head within [`UNAUTH_HEADER_TIMEOUT`],
//!   then that request done within [`UNAUTH_REQUEST_TIMEOUT`] -- and closes
//!   after its one response. A stop or a handoff freeze drops them at once.
//!
//! Clients are keyed by the TCP peer, or -- for loopback, a trusted proxy,
//! or this machine's own address -- by the `X-Forwarded-For` client a
//! request names (none until its head arrives); IPv6 by /64. Nothing is
//! refused at accept (that would lock out a credential-carrying client
//! sharing an address with a flood). Instead:
//!
//! * A client over [`UNAUTH_PER_CLIENT`] loses its own oldest connection
//!   that has not sent a head yet; a request head without a credential
//!   from a client already holding its share of requests gets 429.
//! * A full pool evicts from the client holding the most connections
//!   (each not yet keyed counts as its own); among equals, one that has
//!   sent no head, then one whose head is not sorted yet, then one
//!   mid-request; oldest first. An evicted connection whose head arrived
//!   or is arriving (or was accepted moments ago) gets a moment for it:
//!   a credential still moves it to the main pool, none has it compete
//!   for a slot again ([`super::listen`]).
//!   A flood from a few addresses only churns itself; one with more
//!   addresses than the pool has slots can still reset strangers' logins,
//!   never an authenticated client's request.

use crate::auth::client_key;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// How long a connection no request has authenticated has to deliver its
/// first request head (an authenticated one gets
/// [`super::listen::HEADER_READ_TIMEOUT`] per head).
pub const UNAUTH_HEADER_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an unauthenticated request may take once its head arrived:
/// longer than the public body deadline (`BodyBound::PUBLIC`), so that one
/// answers first.
pub const UNAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Unauthenticated connections one client may hold at once (a browser
/// opens up to six per host).
pub const UNAUTH_PER_CLIENT: usize = 8;
/// Bounds of [`unauthenticated_cap`].
const MIN_UNAUTH: usize = 8;
const MAX_UNAUTH: usize = 128;

/// The unauthenticated pool for a main pool of `main_cap`: a quarter of
/// it, within `MIN_UNAUTH..=MAX_UNAUTH`.
pub fn unauthenticated_cap(main_cap: usize) -> usize {
    (main_cap / 4).clamp(MIN_UNAUTH, MAX_UNAUTH)
}

/// Where a connection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// In the unauthenticated pool.
    Pending,
    /// Holding a main slot.
    Main,
    /// Holding a share-link slot.
    Viewer,
    /// Evicted from the pending pool: its task closes it, unless a request
    /// head already in its socket buffer authenticates first (it would
    /// have, but for the race).
    Evicted,
    /// Expired, or dropped by a stop or freeze: its task closes it.
    Closed,
}

/// Both pools.
#[derive(Debug)]
pub struct Slots {
    main: Arc<Semaphore>,
    pending: Mutex<Pool>,
    pending_cap: usize,
    per_client: usize,
    viewer_cap: usize,
}

#[derive(Debug, Default)]
struct Pool {
    next: u64,
    /// Pending connections, oldest first.
    order: BTreeMap<u64, Arc<Conn>>,
    /// Pending connections per client key (`None`: not known yet, or this
    /// machine -- never capped, and each its own client for eviction).
    clients: HashMap<Option<IpAddr>, usize>,
    /// Share-link connections per client key, and in all.
    viewers: HashMap<Option<IpAddr>, usize>,
    viewer_total: usize,
}

/// One connection's standing, shared by its task and its requests.
#[derive(Debug)]
pub struct Conn {
    state: Mutex<ConnState>,
    /// Woken on every change (an eviction included).
    pub wake: Notify,
}

#[derive(Debug)]
struct ConnState {
    phase: Phase,
    /// Its key in [`Pool::order`] while pending.
    seq: u64,
    /// The client key it counts against while pending or a viewer.
    client: Option<IpAddr>,
    /// A request head arrived on it.
    head: bool,
    /// A request without a credential was sorted (keyed by its client).
    sorted: bool,
    /// When a pending connection is dropped.
    deadline: Instant,
    main: Option<OwnedSemaphorePermit>,
}

impl Conn {
    fn lock(&self) -> MutexGuard<'_, ConnState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Its phase and (while pending) deadline.
    pub fn standing(&self) -> (Phase, Instant) {
        let state = self.lock();
        (state.phase, state.deadline)
    }

    /// Whether a request head arrived on it.
    pub fn has_head(&self) -> bool {
        self.lock().head
    }

    pub fn is_pending(&self) -> bool {
        self.lock().phase == Phase::Pending
    }
}

impl Pool {
    fn count(&self, key: Option<IpAddr>) -> usize {
        self.clients.get(&key).copied().unwrap_or(0)
    }

    fn add_client(&mut self, key: Option<IpAddr>) {
        *self.clients.entry(key).or_default() += 1;
    }

    /// A share-link connection of `state.client` leaves its pool.
    fn drop_viewer(&mut self, state: &mut ConnState) {
        if state.phase != Phase::Viewer {
            return;
        }
        self.viewer_total = self.viewer_total.saturating_sub(1);
        if let Some(n) = self.viewers.get_mut(&state.client) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.viewers.remove(&state.client);
            }
        }
        state.phase = Phase::Pending;
    }

    fn drop_client(&mut self, key: Option<IpAddr>) {
        if let Some(n) = self.clients.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.clients.remove(&key);
            }
        }
    }

    /// Take the connection whose locked state is `state` out of the pool
    /// (a no-op when it is not in it).
    fn remove(&mut self, state: &ConnState) {
        if self.order.remove(&state.seq).is_some() {
            self.drop_client(state.client);
        }
    }

    /// Close the pending connection `seq`. Called with no connection lock
    /// held but possibly the caller's own, which is never the victim.
    fn evict(&mut self, seq: u64) {
        let Some(victim) = self.order.remove(&seq) else {
            return;
        };
        let mut state = victim.lock();
        self.drop_client(state.client);
        state.phase = Phase::Evicted;
        drop(state);
        victim.wake.notify_one();
    }

    /// The oldest connection of `key` that has sent no request head.
    /// `skip`: the caller's own (locked) connection.
    fn oldest_silent(&self, key: Option<IpAddr>, skip: u64) -> Option<u64> {
        self.order.iter().find_map(|(&seq, conn)| {
            if seq == skip {
                return None;
            }
            let state = conn.lock();
            (state.client == key && !state.head).then_some(seq)
        })
    }

    /// Who a full pool evicts: from the client holding the most
    /// connections; then one that sent no head, then one whose head is not
    /// sorted yet (it is given a moment to re-compete), then one mid-request;
    /// the oldest.
    fn victim(&self, skip: u64) -> Option<u64> {
        let mut best: Option<((usize, u8), u64)> = None;
        for (&seq, conn) in &self.order {
            if seq == skip {
                continue;
            }
            let (client, class) = {
                let state = conn.lock();
                let class = match (state.head, state.sorted) {
                    (false, _) => 2u8,
                    (true, false) => 1,
                    (true, true) => 0,
                };
                (state.client, class)
            };
            // Connections not keyed yet are strangers to each other: one
            // proxy carries many clients, the operator among them.
            let held = if client.is_some() {
                self.count(client)
            } else {
                1
            };
            let rank = (held, class);
            if best.is_none_or(|(top, _)| rank > top) {
                best = Some((rank, seq));
            }
        }
        best.map(|(_, seq)| seq)
    }

    /// Make room for one more: evict until fewer than `cap` remain.
    fn make_room(&mut self, cap: usize, skip: u64) {
        while self.order.len() >= cap.max(1) {
            let Some(seq) = self.victim(skip) else {
                return;
            };
            self.evict(seq);
        }
    }

    /// Put `conn` in as its newest pending connection, counted against
    /// `key`.
    fn insert(&mut self, conn: &Arc<Conn>, state: &mut ConnState, key: Option<IpAddr>) {
        self.next += 1;
        state.seq = self.next;
        state.client = key;
        self.order.insert(state.seq, Arc::clone(conn));
        self.add_client(key);
    }
}

impl Slots {
    pub fn new(main_cap: usize, pending_cap: usize, per_client: usize) -> Self {
        Self {
            main: Arc::new(Semaphore::new(main_cap)),
            pending: Mutex::new(Pool::default()),
            pending_cap,
            per_client,
            viewer_cap: pending_cap,
        }
    }

    fn pool(&self) -> MutexGuard<'_, Pool> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Room for one more connection of `key` (`Some`: capped per client):
    /// over its share, its own oldest silent connection goes first; then a
    /// full pool evicts fairly. `skip`: the caller's own connection.
    fn make_room_for(&self, pool: &mut Pool, key: Option<IpAddr>, skip: u64) {
        if key.is_some()
            && pool.count(key) >= self.per_client
            && let Some(seq) = pool.oldest_silent(key, skip)
        {
            pool.evict(seq);
        }
        pool.make_room(self.pending_cap, skip);
    }

    /// Admit a new connection as pending, counted against `client`
    /// (`None`: loopback, a trusted proxy, or this machine, whose requests
    /// name their clients). Never refused: see the module doc.
    pub fn admit(&self, client: Option<IpAddr>) -> Arc<Conn> {
        let key = client.map(client_key);
        let mut pool = self.pool();
        self.make_room_for(&mut pool, key, 0);
        let conn = Arc::new(Conn {
            state: Mutex::new(ConnState {
                phase: Phase::Pending,
                seq: 0,
                client: None,
                head: false,
                sorted: false,
                deadline: Instant::now() + UNAUTH_HEADER_TIMEOUT,
                main: None,
            }),
            wake: Notify::new(),
        });
        {
            let mut state = conn.lock();
            pool.insert(&conn, &mut state, key);
        }
        conn
    }

    /// A request head arrived on `conn`: a pending one now has
    /// [`UNAUTH_REQUEST_TIMEOUT`] to finish it, and a full pool evicts
    /// silent connections before it.
    pub fn request_started(&self, conn: &Conn) {
        let mut state = conn.lock();
        state.head = true;
        if state.phase == Phase::Pending {
            state.deadline = Instant::now() + UNAUTH_REQUEST_TIMEOUT;
            drop(state);
            conn.wake.notify_one();
        }
    }

    /// A request on `conn` authenticated: take it out of the pending pool
    /// at once (nothing evicts it from here on) and into the main pool
    /// once a main slot is free. False once the connection was closed.
    pub async fn authenticated(&self, conn: &Conn) -> bool {
        {
            let mut pool = self.pool();
            let mut state = conn.lock();
            match state.phase {
                Phase::Main => return true,
                Phase::Closed => return false,
                Phase::Pending => pool.remove(&state),
                Phase::Evicted => state.phase = Phase::Pending,
                Phase::Viewer => {
                    // It waits for a main slot like a fresh request.
                    pool.drop_viewer(&mut state);
                    state.deadline = Instant::now() + UNAUTH_REQUEST_TIMEOUT;
                    drop(state);
                    conn.wake.notify_one();
                }
            }
        }
        let Ok(permit) = Arc::clone(&self.main).acquire_owned().await else {
            return false;
        };
        let mut state = conn.lock();
        match state.phase {
            Phase::Pending => {
                state.phase = Phase::Main;
                state.main = Some(permit);
                drop(state);
                conn.wake.notify_one();
                true
            }
            Phase::Main => true,
            Phase::Viewer | Phase::Evicted | Phase::Closed => false,
        }
    }

    /// A request on `conn` carried a live share link's credential: hold a
    /// share-link slot, counted against `client` (`None`: this machine,
    /// uncounted per client). Never waits: false when the share pool is
    /// full, `client` holds its share of it, or the connection was closed.
    /// A main connection keeps its slot.
    pub fn shared(&self, conn: &Conn, client: Option<IpAddr>) -> bool {
        let key = client.map(client_key);
        let mut pool = self.pool();
        let mut state = conn.lock();
        match state.phase {
            Phase::Main => return true,
            Phase::Closed => return false,
            Phase::Viewer if state.client == key => return true,
            Phase::Viewer => pool.drop_viewer(&mut state),
            Phase::Pending | Phase::Evicted => {}
        }
        let held = pool.viewers.get(&key).copied().unwrap_or(0);
        if pool.viewer_total >= self.viewer_cap || (key.is_some() && held >= self.per_client) {
            return false;
        }
        if state.phase == Phase::Pending {
            pool.remove(&state);
        }
        pool.viewer_total += 1;
        *pool.viewers.entry(key).or_default() += 1;
        state.client = key;
        state.phase = Phase::Viewer;
        drop(state);
        conn.wake.notify_one();
        true
    }

    /// A request on `conn` did not authenticate: it counts (from now on)
    /// against `client` (`None`: this machine, uncounted) in the pending
    /// pool, leaving the main pool if it held a slot there (or re-entering
    /// it if evicted as this request arrived). False when
    /// that client already holds its share of connections with a request
    /// head (silent ones of its own make way), or the connection was
    /// closed.
    pub fn unauthenticated(&self, conn: &Arc<Conn>, client: Option<IpAddr>) -> bool {
        let key = client.map(client_key);
        let mut pool = self.pool();
        let mut state = conn.lock();
        if state.phase == Phase::Closed {
            return false;
        }
        pool.drop_viewer(&mut state);
        let in_pool = state.phase == Phase::Pending && pool.order.contains_key(&state.seq);
        // Others of `key` it would sit beside.
        let others = pool.count(key) - usize::from(in_pool && state.client == key);
        if key.is_some() && others >= self.per_client {
            match pool.oldest_silent(key, state.seq) {
                Some(seq) => pool.evict(seq),
                None => return false,
            }
        }
        match state.phase {
            Phase::Pending if in_pool => {
                pool.drop_client(state.client);
                pool.add_client(key);
                state.client = key;
                state.sorted = true;
                true
            }
            _ => {
                // Back from the main pool, or evicted as its head arrived
                // (it competes for a slot again, fairly, now it is keyed).
                pool.make_room(self.pending_cap, state.seq);
                pool.insert(conn, &mut state, key);
                state.phase = Phase::Pending;
                state.sorted = true;
                state.main = None;
                state.deadline = Instant::now() + UNAUTH_REQUEST_TIMEOUT;
                drop(state);
                conn.wake.notify_one();
                true
            }
        }
    }

    /// Close `conn` if it is pending (true then, or if already closed);
    /// false for a main connection.
    pub fn close_if_pending(&self, conn: &Conn) -> bool {
        let mut pool = self.pool();
        let mut state = conn.lock();
        match state.phase {
            Phase::Main | Phase::Viewer => false,
            Phase::Closed => true,
            Phase::Evicted | Phase::Pending => {
                pool.remove(&state);
                state.phase = Phase::Closed;
                true
            }
        }
    }

    /// The connection ended: free whatever it held.
    pub fn release(&self, conn: &Conn) {
        let mut pool = self.pool();
        let mut state = conn.lock();
        if state.phase == Phase::Pending {
            pool.remove(&state);
        }
        pool.drop_viewer(&mut state);
        state.phase = Phase::Closed;
        state.main = None;
    }

    /// Pending connections now (for tests and the pool's own checks).
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pool().order.len()
    }

    #[cfg(test)]
    pub fn main_available(&self) -> usize {
        self.main.available_permits()
    }

    #[cfg(test)]
    pub fn viewers_len(&self) -> usize {
        self.pool().viewer_total
    }
}

/// A request's handle on its connection's slot, put in its extensions by
/// the accept loop.
#[derive(Debug, Clone)]
pub struct Admission {
    slots: Arc<Slots>,
    conn: Arc<Conn>,
}

impl Admission {
    pub fn new(slots: Arc<Slots>, conn: Arc<Conn>) -> Self {
        Self { slots, conn }
    }

    /// See [`Slots::authenticated`].
    pub async fn authenticated(&self) -> bool {
        self.slots.authenticated(&self.conn).await
    }

    /// See [`Slots::shared`].
    pub fn shared(&self, client: Option<IpAddr>) -> bool {
        self.slots.shared(&self.conn, client)
    }

    /// See [`Slots::unauthenticated`].
    pub fn unauthenticated(&self, client: Option<IpAddr>) -> bool {
        self.slots.unauthenticated(&self.conn, client)
    }

    /// Whether the connection is still unauthenticated (it closes after
    /// this response).
    pub fn pending(&self) -> bool {
        self.conn.is_pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn the_unauthenticated_pool_scales_with_the_main_one_within_bounds() {
        assert_eq!(unauthenticated_cap(16), MIN_UNAUTH);
        assert_eq!(unauthenticated_cap(64), 16);
        assert_eq!(unauthenticated_cap(1024), MAX_UNAUTH);
        assert!(UNAUTH_HEADER_TIMEOUT < super::super::listen::HEADER_READ_TIMEOUT);
        assert!(super::super::BodyBound::PUBLIC.timeout < UNAUTH_REQUEST_TIMEOUT);
    }

    /// A connection whose request head arrived.
    fn with_head(slots: &Slots, client: Option<IpAddr>) -> Arc<Conn> {
        let conn = slots.admit(client);
        slots.request_started(&conn);
        conn
    }

    fn phase(conn: &Conn) -> Phase {
        conn.standing().0
    }

    #[test]
    fn a_client_over_its_share_loses_its_own_silent_connections_never_the_newcomer() {
        let slots = Slots::new(4, 16, 2);
        let a = slots.admit(ip("198.51.100.1"));
        let b = slots.admit(ip("198.51.100.1"));
        // A third from the same address is admitted (it may carry a
        // credential); the address's oldest silent one makes way.
        let c = slots.admit(ip("198.51.100.1"));
        assert_eq!(phase(&a), Phase::Evicted);
        assert_eq!((phase(&b), phase(&c)), (Phase::Pending, Phase::Pending));
        // Another client is unaffected.
        let other = slots.admit(ip("198.51.100.2"));
        assert_eq!(phase(&other), Phase::Pending);
        // IPv6 counts by /64.
        let d = slots.admit(ip("2001:db8::1"));
        let _e = slots.admit(ip("2001:db8::2"));
        let _f = slots.admit(ip("2001:db8::3"));
        assert_eq!(phase(&d), Phase::Evicted);
        assert_eq!(slots.pending_len(), 5);
    }

    #[test]
    fn requests_without_a_credential_are_capped_per_client() {
        let slots = Slots::new(4, 16, 2);
        let a = with_head(&slots, ip("198.51.100.1"));
        assert!(slots.unauthenticated(&a, ip("198.51.100.1")));
        let b = with_head(&slots, ip("198.51.100.1"));
        assert!(slots.unauthenticated(&b, ip("198.51.100.1")));
        // Two requests in flight: a third is refused, and nothing of the
        // two is evicted for it.
        let c = with_head(&slots, ip("198.51.100.1"));
        assert!(!slots.unauthenticated(&c, ip("198.51.100.1")));
        assert_eq!((phase(&a), phase(&b)), (Phase::Pending, Phase::Pending));
        // A slot freed is a slot back.
        slots.release(&a);
        slots.release(&c);
        let d = with_head(&slots, ip("198.51.100.1"));
        assert!(slots.unauthenticated(&d, ip("198.51.100.1")));
    }

    #[tokio::test]
    async fn a_flood_from_one_address_never_locks_out_a_credential_from_it() {
        let slots = Slots::new(4, 16, 8);
        let flood: Vec<_> = (0..8).map(|_| slots.admit(ip("198.51.100.1"))).collect();
        // The operator, behind the same NAT, reconnects again and again.
        for _ in 0..20 {
            let operator = with_head(&slots, ip("198.51.100.1"));
            assert!(slots.authenticated(&operator).await);
            slots.release(&operator);
        }
        drop(flood);
    }

    #[test]
    fn a_full_pool_evicts_from_the_client_holding_the_most() {
        let slots = Slots::new(4, 4, 8);
        // A stranger's slow login, mid-request.
        let login = with_head(&slots, ip("203.0.113.5"));
        assert!(slots.unauthenticated(&login, ip("203.0.113.5")));
        // One forwarded client churning fast requests behind a proxy.
        for _ in 0..50 {
            let churn = with_head(&slots, None);
            assert!(slots.unauthenticated(&churn, ip("198.51.100.66")));
        }
        assert_eq!(phase(&login), Phase::Pending);
        assert_eq!(slots.pending_len(), 4);
    }

    #[test]
    fn silent_connections_go_before_ones_with_a_request() {
        let slots = Slots::new(4, 3, 8);
        let login = with_head(&slots, ip("203.0.113.5"));
        assert!(slots.unauthenticated(&login, ip("203.0.113.5")));
        // Many addresses, one silent connection each: as many as the pool
        // holds, so no client holds more than the login's.
        let silent: Vec<_> = (1..=10u8)
            .map(|n| slots.admit(Some(IpAddr::from([198, 51, 100, n]))))
            .collect();
        assert_eq!(phase(&login), Phase::Pending);
        assert_eq!(
            silent.iter().filter(|c| phase(c) == Phase::Pending).count(),
            2
        );
    }

    #[tokio::test]
    async fn an_authenticating_connection_is_never_evicted() {
        // No main slot free: the credential-carrying one waits for one,
        // out of the pending pool.
        let slots = Arc::new(Slots::new(1, 2, 8));
        let holder = slots.admit(None);
        assert!(slots.authenticated(&holder).await);
        let waiting = with_head(&slots, None);
        let task = {
            let (slots, waiting) = (Arc::clone(&slots), Arc::clone(&waiting));
            tokio::spawn(async move { slots.authenticated(&waiting).await })
        };
        tokio::task::yield_now().await;
        while slots.pending_len() != 0 {
            tokio::task::yield_now().await;
        }
        let _flood: Vec<_> = (0..10).map(|_| slots.admit(None)).collect();
        assert_eq!(phase(&waiting), Phase::Pending);
        slots.release(&holder);
        assert!(task.await.unwrap());
        assert_eq!(phase(&waiting), Phase::Main);
    }

    #[test]
    fn a_full_pool_evicts_its_oldest_among_equals() {
        let slots = Slots::new(4, 3, 8);
        let first = slots.admit(None);
        let second = slots.admit(None);
        let third = slots.admit(None);
        let fourth = slots.admit(None);
        assert_eq!(phase(&first), Phase::Evicted);
        for live in [&second, &third, &fourth] {
            assert_eq!(phase(live), Phase::Pending);
        }
        assert_eq!(slots.pending_len(), 3);
        // Releasing the evicted one does not free a live one's slot.
        slots.release(&first);
        assert_eq!(slots.pending_len(), 3);
    }

    #[tokio::test]
    async fn authenticating_moves_a_connection_to_the_main_pool() {
        let slots = Slots::new(2, 2, 1);
        let conn = slots.admit(ip("198.51.100.1"));
        assert!(slots.authenticated(&conn).await);
        assert_eq!(conn.standing().0, Phase::Main);
        assert_eq!(slots.pending_len(), 0);
        assert_eq!(slots.main_available(), 1);
        // Its client's share is free again, and nothing evicts it.
        let others: Vec<_> = (0..4).map(|_| slots.admit(None)).collect();
        assert_eq!(conn.standing().0, Phase::Main);
        assert_eq!(phase(&slots.admit(ip("198.51.100.1"))), Phase::Pending);
        // A stop or freeze leaves it alone.
        assert!(!slots.close_if_pending(&conn));
        slots.release(&conn);
        assert_eq!(slots.main_available(), 2);
        drop(others);
    }

    #[tokio::test]
    async fn an_evicted_connection_closes_unless_it_authenticates_first() {
        let slots = Slots::new(2, 1, 8);
        // Its head was already buffered when it was evicted: honoured.
        let raced = slots.admit(None);
        let _new = slots.admit(None);
        assert_eq!(phase(&raced), Phase::Evicted);
        assert!(slots.authenticated(&raced).await);
        assert_eq!(phase(&raced), Phase::Main);
        assert_eq!(slots.pending_len(), 1);
        // A request without a credential on one competes for a slot again.
        let old = slots.admit(None);
        assert_eq!(phase(&_new), Phase::Evicted);
        assert!(slots.unauthenticated(&_new, ip("203.0.113.5")));
        assert_eq!(
            (phase(&_new), phase(&old)),
            (Phase::Pending, Phase::Evicted)
        );
        assert_eq!(slots.pending_len(), 1);
        // Once its task closed it, nothing brings it back.
        slots.release(&old);
        assert!(!slots.unauthenticated(&old, None));
        assert!(!slots.authenticated(&old).await);
        assert_eq!(slots.main_available(), 1);
    }

    #[tokio::test]
    async fn an_unauthenticated_request_demotes_and_rekeys() {
        let slots = Slots::new(2, 4, 1);
        // A proxy's connection: its request names the client.
        let proxied = with_head(&slots, None);
        assert!(slots.unauthenticated(&proxied, ip("203.0.113.9")));
        let again = with_head(&slots, None);
        assert!(!slots.unauthenticated(&again, ip("203.0.113.9")));
        // A connection that authenticated before, then sends a request
        // without a credential, leaves the main pool.
        let conn = with_head(&slots, ip("198.51.100.1"));
        assert!(slots.authenticated(&conn).await);
        assert_eq!(slots.main_available(), 1);
        assert!(slots.unauthenticated(&conn, ip("198.51.100.1")));
        assert_eq!(conn.standing().0, Phase::Pending);
        assert_eq!(slots.main_available(), 2);
        // This machine's requests are not counted per client.
        assert!(slots.unauthenticated(&again, None));
        assert!(slots.close_if_pending(&again));
        assert!(!slots.unauthenticated(&again, None));
    }

    #[tokio::test]
    async fn share_link_connections_hold_their_own_slots_never_main_ones() {
        let slots = Slots::new(1, 3, 2);
        // A link's holder fills its share of the share pool, from one
        // address and from others; never a main slot.
        let a = with_head(&slots, ip("198.51.100.1"));
        let b = with_head(&slots, ip("198.51.100.1"));
        assert!(slots.shared(&a, ip("198.51.100.1")));
        assert!(slots.shared(&b, ip("198.51.100.1")));
        let c = with_head(&slots, ip("198.51.100.1"));
        assert!(!slots.shared(&c, ip("198.51.100.1")), "per-client cap");
        let d = with_head(&slots, ip("198.51.100.2"));
        assert!(slots.shared(&d, ip("198.51.100.2")));
        let e = with_head(&slots, None);
        assert!(!slots.shared(&e, None), "the pool is full");
        assert_eq!((slots.viewers_len(), slots.main_available()), (3, 1));
        assert_eq!(slots.pending_len(), 2);
        // Kept alive: a stop or freeze leaves them to finish.
        assert!(!slots.close_if_pending(&a));
        // The operator still gets the main slot at once.
        assert!(slots.authenticated(&e).await);
        assert_eq!(phase(&e), Phase::Main);
        // A viewer connection that then authenticates leaves the share
        // pool; one that sends nothing competes as unauthenticated.
        slots.release(&e);
        assert!(slots.authenticated(&d).await);
        assert_eq!(slots.viewers_len(), 2);
        assert!(slots.unauthenticated(&b, ip("198.51.100.1")));
        assert_eq!((phase(&b), slots.viewers_len()), (Phase::Pending, 1));
        slots.release(&a);
        assert_eq!(slots.viewers_len(), 0);
        assert!(slots.shared(&c, ip("198.51.100.1")));
    }
}
