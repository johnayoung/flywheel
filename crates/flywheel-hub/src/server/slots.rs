//! Connection slots, by whether a request on the connection authenticated.
//!
//! * **Main** ([`Slots::new`]'s `main_cap`): connections whose latest
//!   request carried a valid credential (token, login cookie, peer
//!   credential). Kept alive; a stop lets their requests finish.
//! * **Unauthenticated** (`pending_cap`, much smaller): every connection
//!   starts here and stays until a request on it authenticates. Each has a
//!   deadline -- its first request head within [`UNAUTH_HEADER_TIMEOUT`],
//!   then that request done within [`UNAUTH_REQUEST_TIMEOUT`] -- and closes
//!   after its one response. At most `per_client` are held per client (by
//!   the TCP peer, or for a trusted proxy by the `X-Forwarded-For` client
//!   its request names; IPv6 by /64). When the pool is full, a new
//!   connection evicts the oldest: a flood can churn the pool but never
//!   keep a client whose request head arrives promptly out of it. A stop
//!   or a handoff freeze drops them at once.

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
    /// Evicted, expired, or dropped by a stop or freeze: its task closes it.
    Closed,
}

/// Both pools.
#[derive(Debug)]
pub struct Slots {
    main: Arc<Semaphore>,
    pending: Mutex<Pool>,
    pending_cap: usize,
    per_client: usize,
}

#[derive(Debug, Default)]
struct Pool {
    next: u64,
    /// Pending connections, oldest first.
    order: BTreeMap<u64, Arc<Conn>>,
    /// Pending connections per client key.
    clients: HashMap<IpAddr, usize>,
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
    /// The client key it counts against while pending.
    client: Option<IpAddr>,
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

    pub fn is_pending(&self) -> bool {
        self.lock().phase == Phase::Pending
    }
}

impl Pool {
    fn count(&self, key: Option<IpAddr>) -> usize {
        key.and_then(|k| self.clients.get(&k).copied()).unwrap_or(0)
    }

    fn add_client(&mut self, key: Option<IpAddr>) {
        if let Some(key) = key {
            *self.clients.entry(key).or_default() += 1;
        }
    }

    fn drop_client(&mut self, key: Option<IpAddr>) {
        let Some(key) = key else { return };
        if let Some(n) = self.clients.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.clients.remove(&key);
            }
        }
    }

    /// Take `conn` (whose locked state is `state`) out of the pool.
    fn remove(&mut self, state: &ConnState) {
        if self.order.remove(&state.seq).is_some() {
            self.drop_client(state.client);
        }
    }

    /// Make room for one more: close the oldest pending connections until
    /// fewer than `cap` remain. Called with no connection lock held but
    /// possibly the newcomer's, which is never in `order` yet.
    fn make_room(&mut self, cap: usize) {
        while self.order.len() >= cap.max(1) {
            let Some((_, victim)) = self.order.pop_first() else {
                return;
            };
            let mut state = victim.lock();
            self.drop_client(state.client);
            state.phase = Phase::Closed;
            drop(state);
            victim.wake.notify_one();
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
        }
    }

    fn pool(&self) -> MutexGuard<'_, Pool> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit a new connection as pending, counted against `client` (`None`:
    /// a trusted proxy, whose requests name their clients); `None` when
    /// that client already holds its share.
    pub fn admit(&self, client: Option<IpAddr>) -> Option<Arc<Conn>> {
        let key = client.map(client_key);
        let mut pool = self.pool();
        if pool.count(key) >= self.per_client {
            return None;
        }
        pool.make_room(self.pending_cap);
        let conn = Arc::new(Conn {
            state: Mutex::new(ConnState {
                phase: Phase::Pending,
                seq: 0,
                client: None,
                deadline: Instant::now() + UNAUTH_HEADER_TIMEOUT,
                main: None,
            }),
            wake: Notify::new(),
        });
        {
            let mut state = conn.lock();
            pool.insert(&conn, &mut state, key);
        }
        Some(conn)
    }

    /// A request head arrived on `conn`: a pending one now has
    /// [`UNAUTH_REQUEST_TIMEOUT`] to finish it.
    pub fn request_started(&self, conn: &Conn) {
        let mut state = conn.lock();
        if state.phase == Phase::Pending {
            state.deadline = Instant::now() + UNAUTH_REQUEST_TIMEOUT;
            drop(state);
            conn.wake.notify_one();
        }
    }

    /// A request on `conn` authenticated: move it to the main pool (waiting
    /// for a main slot). False once the connection was closed meanwhile.
    pub async fn authenticated(&self, conn: &Conn) -> bool {
        match conn.lock().phase {
            Phase::Main => return true,
            Phase::Closed => return false,
            Phase::Pending => {}
        }
        let Ok(permit) = Arc::clone(&self.main).acquire_owned().await else {
            return false;
        };
        let mut pool = self.pool();
        let mut state = conn.lock();
        match state.phase {
            Phase::Pending => {
                pool.remove(&state);
                state.phase = Phase::Main;
                state.main = Some(permit);
                drop(state);
                conn.wake.notify_one();
                true
            }
            Phase::Main => true,
            Phase::Closed => false,
        }
    }

    /// A request on `conn` did not authenticate: it counts (from now on)
    /// against `client` (`None`: this machine, uncounted) in the pending
    /// pool, leaving the main pool if it held a slot there. False when
    /// that client already holds its share or the connection was closed.
    pub fn unauthenticated(&self, conn: &Arc<Conn>, client: Option<IpAddr>) -> bool {
        let key = client.map(client_key);
        let mut pool = self.pool();
        let mut state = conn.lock();
        match state.phase {
            Phase::Closed => false,
            Phase::Pending => {
                if state.client == key {
                    return true;
                }
                if pool.count(key) >= self.per_client {
                    return false;
                }
                pool.drop_client(state.client);
                pool.add_client(key);
                state.client = key;
                true
            }
            Phase::Main => {
                if pool.count(key) >= self.per_client {
                    return false;
                }
                pool.make_room(self.pending_cap);
                pool.insert(conn, &mut state, key);
                state.phase = Phase::Pending;
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
            Phase::Main => false,
            Phase::Closed => true,
            Phase::Pending => {
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

    #[test]
    fn one_client_holds_at_most_its_share() {
        let slots = Slots::new(4, 16, 2);
        let a = slots.admit(ip("198.51.100.1")).unwrap();
        let _b = slots.admit(ip("198.51.100.1")).unwrap();
        assert!(slots.admit(ip("198.51.100.1")).is_none());
        // Another client is unaffected; an unkeyed (proxy) one too.
        assert!(slots.admit(ip("198.51.100.2")).is_some());
        assert!(slots.admit(None).is_some());
        // IPv6 counts by /64.
        let _c = slots.admit(ip("2001:db8::1")).unwrap();
        let _d = slots.admit(ip("2001:db8::2")).unwrap();
        assert!(slots.admit(ip("2001:db8::3")).is_none());
        // A slot freed is a slot back.
        slots.release(&a);
        assert!(slots.admit(ip("198.51.100.1")).is_some());
    }

    #[test]
    fn a_full_pool_evicts_its_oldest_connection() {
        let slots = Slots::new(4, 3, 8);
        let first = slots.admit(None).unwrap();
        let second = slots.admit(None).unwrap();
        let third = slots.admit(None).unwrap();
        let fourth = slots.admit(None).unwrap();
        assert_eq!(first.standing().0, Phase::Closed);
        for live in [&second, &third, &fourth] {
            assert_eq!(live.standing().0, Phase::Pending);
        }
        assert_eq!(slots.pending_len(), 3);
        // Releasing the evicted one does not free a live one's slot.
        slots.release(&first);
        assert_eq!(slots.pending_len(), 3);
    }

    #[tokio::test]
    async fn authenticating_moves_a_connection_to_the_main_pool() {
        let slots = Slots::new(2, 2, 1);
        let conn = slots.admit(ip("198.51.100.1")).unwrap();
        assert!(slots.authenticated(&conn).await);
        assert_eq!(conn.standing().0, Phase::Main);
        assert_eq!(slots.pending_len(), 0);
        assert_eq!(slots.main_available(), 1);
        // Its client's share is free again, and nothing evicts it.
        let others: Vec<_> = (0..4).map(|_| slots.admit(None).unwrap()).collect();
        assert_eq!(conn.standing().0, Phase::Main);
        assert!(slots.admit(ip("198.51.100.1")).is_some());
        // A stop or freeze leaves it alone.
        assert!(!slots.close_if_pending(&conn));
        slots.release(&conn);
        assert_eq!(slots.main_available(), 2);
        drop(others);
    }

    #[tokio::test]
    async fn an_evicted_connection_cannot_authenticate_afterwards() {
        let slots = Slots::new(2, 1, 8);
        let old = slots.admit(None).unwrap();
        let _new = slots.admit(None).unwrap();
        assert!(!slots.authenticated(&old).await);
        assert_eq!(slots.main_available(), 2);
    }

    #[tokio::test]
    async fn an_unauthenticated_request_demotes_and_rekeys() {
        let slots = Slots::new(2, 4, 1);
        // A proxy's connection: its request names the client.
        let proxied = slots.admit(None).unwrap();
        assert!(slots.unauthenticated(&proxied, ip("203.0.113.9")));
        let again = slots.admit(None).unwrap();
        assert!(!slots.unauthenticated(&again, ip("203.0.113.9")));
        // A connection that authenticated before, then sends a request
        // without a credential, leaves the main pool.
        let conn = slots.admit(ip("198.51.100.1")).unwrap();
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
}
