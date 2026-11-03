//! The hub's HTTP surface: login, the embedded web app, the REST API, and
//! one WebSocket per session.
//!
//! WebSocket protocol (`GET /api/sessions/{id}/ws`):
//!
//! * server -> client, **text** JSON `{"type":"snapshot","cols":N,"rows":N,
//!   "alt_screen":B,"bytes":N}` first: the size the snapshot was rendered at
//!   (size the terminal to it before replaying, then fit and resize) and its
//!   length.
//! * server -> client, **binary**: terminal output. The first `bytes` bytes
//!   after the snapshot message are the snapshot -- escape sequences
//!   rebuilding the session's terminal (scrollback, screen, alternate
//!   screen, cursor, modes, title) in a reset client -- then live PTY
//!   output, with no gap or overlap.
//! * The server answers terminal queries (device attributes, cursor
//!   position, mode and colour reports) itself; clients must not.
//! * server -> client, **text** JSON: `{"type":"exit","code":N|null,
//!   "signal":S|null}` once the process ends; `{"type":"reset"}` when the
//!   client fell too far behind -- clear the terminal, a fresh snapshot of
//!   the screen alone (no scrollback, so a slow link catches up) follows;
//!   `{"type":"size","cols":N,"rows":N}` when a viewer resized the terminal
//!   (in order with the output: what follows is drawn at that size).
//! * client -> server, **text** JSON: `{"type":"input","data":"..."}`,
//!   `{"type":"resize","cols":N,"rows":N}`, and `{"type":"theme",
//!   "foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}`
//!   (the viewer's colours, which the server's colour-query answers use from
//!   then on; send on attach, theme change, and focus); **binary** frames
//!   are raw input.
//! * Rotating the token closes every socket with code 4001; the end of the
//!   browser login that opened it (logout, expiry, eviction), with 4002.
//!
//! Events WebSocket (`GET /api/events?client=<id>`), hub-wide, text JSON:
//!
//! * server -> client: `{"type":"snapshot","sessions":{id:status},
//!   "workspaces":{id:rollup},"version":V}` on connect (and after falling
//!   behind; `version`: the serving flywheel's);
//!   `{"type":"status","session_id","status":status|null,"workspace_id",
//!   "workspace_status"}` on each change; `{"type":"sessions"}` when the
//!   session or workspace list changed (re-list over REST);
//!   `{"type":"commands","session_id","fresh":B,"commands":[...]}` when an
//!   agent session suggested code blocks (newest first; `fresh`: written in
//!   the last two minutes, not a transcript's backlog). A `status` is
//!   `{"status":"working|blocked|done|idle|unknown","detail"?,"since"}`, as
//!   this client sees it (`done` turns into `idle` once it viewed them).
//!   `{"type":"layout","workspace_id","rev"}` when a workspace's tabs and
//!   splits were saved or a removed session's panes closed (re-read
//!   `GET /api/workspaces/{id}/layout`).
//! * client -> server: `{"type":"view","session_id":id|null,"visible"?:
//!   [ids]}` -- what this page shows: the focused pane's session and the
//!   other panes on screen (all count as viewed; nothing while hidden). The
//!   `client` id (a browser keeps one in local storage) scopes
//!   seen-tracking; without one, the connection gets its own.
//! * Rotating the token closes it with code 4001 too, a login ending 4002.

mod admission;
mod commands_api;
mod detect_api;
mod events_ws;
mod flywheel_api;
mod import_api;
mod listen;
mod notify_api;
mod proxy;
mod pwa;
mod review_api;
mod search_api;
mod sessions_api;
mod slots;
mod templates_api;
mod terminal_ws;
mod workspaces_api;
mod worktree_api;

use events_ws::{events_ws, text};
use sessions_api::{
    check_folder, create_session, get_session, kill_session, list_kinds, list_sessions,
    restart_session, session_input, session_screen, update_session,
};
use terminal_ws::{handoff_restarting, session_ws};
use workspaces_api::{
    create_workspace, delete_workspace, get_layout, get_workspace, list_workspaces, put_layout,
    update_workspace,
};

use crate::auth::{
    FailureLimiter, LOGIN_CODE_TTL, bearer_value, cookie_name, cookie_value, login_cookie,
    logout_cookie, origin_allowed,
};
use crate::hub::{Hub, HubError};
use crate::state::StateError;
use crate::web;
use axum::extract::ws::CloseFrame;
use axum::extract::{
    ConnectInfo, Form, FromRequest, FromRequestParts, Path, Query, Request, State,
};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// How often the server pings an idle WebSocket (keeps proxies from
/// reaping it and detects dead peers).
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Largest client message accepted (input is typed or pasted text).
const WS_MAX_MESSAGE: usize = 1024 * 1024;
/// Snapshot bytes per binary frame.
const SNAPSHOT_FRAME: usize = 256 * 1024;
/// Scrollback in the snapshot sent after a client fell behind: none. It
/// could not keep up with live output, so it gets the smallest snapshot
/// that is still faithful to the screen.
const LAGGED_SNAPSHOT_HISTORY: usize = 0;
/// Largest `lines` the screen endpoint accepts.
const MAX_SCREEN_LINES: usize = 100_000;
/// WebSocket close code sent when the token rotates (the client must log
/// in again).
pub const WS_CLOSE_ROTATED: u16 = 4001;
/// WebSocket close code when the hub process is being replaced (a live
/// handoff, `fw hub upgrade`): RFC 6455 "service restart". Reconnect: the
/// sessions carry on.
pub const WS_CLOSE_RESTART: u16 = 1012;
/// WebSocket close code sent when the browser login that opened the socket
/// ended (logout, expiry, eviction).
pub const WS_CLOSE_SIGNED_OUT: u16 = 4002;
/// WebSocket close code sent when the peer credential that opened the
/// socket was revoked (`fw hub peer revoke`).
pub const WS_CLOSE_PEER_REVOKED: u16 = 4003;
/// How long [`serve`] waits for sessions to exit on shutdown.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// Server options.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Extra browser origins allowed on WebSocket and state-changing
    /// requests (e.g. a reverse proxy's public `https://host`), on top of
    /// same-origin. Requests arriving under an `https` one get `Secure`
    /// cookies.
    pub allowed_origins: Vec<String>,
    /// Reverse proxies on other machines whose `X-Forwarded-For` / `-Host`
    /// / `-Proto` are believed, as from a loopback proxy -- so clients
    /// behind them are rate-limited by their own address, not the proxy's.
    pub trusted_proxies: Vec<IpAddr>,
    /// Push notifications (`--notify-url` / `notify.toml`), for the
    /// notify API (`GET /api/notify`, `POST /api/notify/test`).
    pub notifier: Option<Arc<crate::notify::Notifier>>,
    /// The Flywheel panel's monitor (`/api/flywheel*` and the events
    /// socket's `flywheel` messages); `None` turns the panel off.
    pub flywheel: Option<Arc<crate::flywheel::FlywheelMonitor>>,
    /// The `git` and `gh` the review endpoints run.
    pub review_tools: crate::review::Tools,
}

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    options: Arc<ServerOptions>,
    shutdown: watch::Receiver<bool>,
    failures: Arc<Mutex<FailureLimiter>>,
    /// Credentials other hubs hold to reach this one (`fw hub peer`).
    peers: Arc<crate::peers::PeerStore>,
    /// Other hubs this one proxies to (`fw hub machine`).
    machines: Arc<crate::machines::Machines>,
}

/// Build the router. Flipping `shutdown` to `true` closes every open
/// WebSocket so a graceful shutdown can finish.
pub fn router(hub: Arc<Hub>, options: ServerOptions, shutdown: watch::Receiver<bool>) -> Router {
    let peers = Arc::new(crate::peers::PeerStore::load(hub.state_dir()));
    let machines = Arc::new(crate::machines::Machines::load(hub.state_dir()));
    let state = AppState {
        hub,
        options: Arc::new(options),
        shutdown,
        failures: Arc::new(Mutex::new(FailureLimiter::default())),
        peers,
        machines,
    };
    let protected = Router::new()
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            get(get_workspace)
                .patch(update_workspace)
                .delete(delete_workspace),
        )
        .route(
            "/api/workspaces/{id}/layout",
            get(get_layout).put(put_layout),
        )
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session)
                .patch(update_session)
                .delete(worktree_api::remove_session),
        )
        .route("/api/kinds", get(list_kinds))
        .route("/api/templates", get(templates_api::list))
        .route("/api/workspaces/{id}/templates", post(templates_api::save))
        .route(
            "/api/workspaces/{id}/templates/{name}",
            axum::routing::delete(templates_api::remove),
        )
        .route("/api/usage", get(templates_api::usage))
        .route("/api/folders/check", get(check_folder))
        // The pre-folders name of the same check.
        .route("/api/repos/check", get(check_folder))
        .route("/api/workspaces/import", post(import_api::import_workspace))
        .route(
            "/api/code-workspaces",
            get(import_api::find_code_workspaces),
        )
        .route("/api/sessions/{id}/kill", post(kill_session))
        .route("/api/sessions/{id}/relaunch", post(restart_session))
        // The route's first name, kept for older clients.
        .route("/api/sessions/{id}/restart", post(restart_session))
        .route("/api/sessions/{id}/screen", get(session_screen))
        .route("/api/sessions/{id}/explain", get(detect_api::explain))
        .route("/api/sessions/{id}/input", post(session_input))
        .route(
            "/api/sessions/{id}/commands",
            get(commands_api::list_commands),
        )
        .route("/api/sessions/{id}/paste", post(commands_api::paste))
        .route(
            "/api/sessions/{id}/companion",
            post(worktree_api::companion),
        )
        .route(
            "/api/sessions/{id}/worktree",
            get(worktree_api::worktree_info),
        )
        .route("/api/worktrees/plan", get(worktree_api::plan))
        .route("/api/sessions/{id}/review", get(review_api::review))
        .route("/api/sessions/{id}/review/diff", get(review_api::diff))
        .route("/api/sessions/{id}/review/commit", post(review_api::commit))
        .route("/api/sessions/{id}/review/push", post(review_api::push))
        .route("/api/sessions/{id}/review/pr", post(review_api::pr))
        .route("/api/sessions/{id}/review/rebase", post(review_api::rebase))
        .route("/api/sessions/{id}/review/abort", post(review_api::abort))
        .route("/api/sessions/{id}/review/merge", post(review_api::merge))
        .route("/api/sessions/{id}/review/ask", post(review_api::ask))
        .route("/api/sessions/{id}/ws", get(session_ws))
        .route("/api/search", get(search_api::search))
        .route("/api/sessions/{id}/transcript", get(search_api::transcript))
        .route("/api/login-codes", post(mint_login_code))
        .route("/api/token/rotate", post(rotate_token))
        .route("/api/events", get(events_ws))
        .route("/api/notify", get(notify_api::status))
        .route("/api/notify/test", post(notify_api::test))
        .route("/api/flywheel", get(flywheel_api::list))
        .route("/api/flywheel/panel", get(flywheel_api::panel))
        .route("/api/flywheel/run", get(flywheel_api::run_output))
        .route("/api/flywheel/worker", post(flywheel_api::start_worker))
        // Multiple machines: this hub's peers (remote side) and machines
        // (local side), and the proxy to them.
        .route("/api/whoami", get(proxy::whoami))
        .route("/api/peer", delete(proxy::revoke_self))
        .route("/api/peers", get(proxy::list_peers).post(proxy::issue_peer))
        .route("/api/peers/{which}", delete(proxy::revoke_peer))
        .route(
            "/api/machines",
            get(proxy::list_machines).post(proxy::add_machine),
        )
        .route("/api/machines/{name}", delete(proxy::remove_machine))
        .route("/api/machines/{name}/test", post(proxy::test_machine))
        .route("/m/{name}/{*rest}", any(proxy::forward))
        // Anything else under /api answers in the API's error form too.
        .route("/api/{*rest}", any(api_not_found))
        .route("/api", any(api_not_found))
        .method_not_allowed_fallback(api_method_not_allowed)
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    // Reachable without a credential: a body must arrive whole, small, and
    // soon, so a client trickling one cannot hold a connection slot (or a
    // handoff's or shutdown's wait for requests to settle) indefinitely.
    let public = Router::new()
        .route("/", get(index))
        .route("/login", get(login_query).post(login_form))
        .route("/logout", post(logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(asset))
        .route("/manifest.webmanifest", get(pwa::manifest))
        .route("/sw.js", get(pwa::service_worker))
        .route_layer(middleware::from_fn_with_state(
            BodyBound::PUBLIC,
            bound_body,
        ));
    public
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admission::admit,
        ))
        .with_state(state)
}

/// Serve on `listener` until `shutdown_signal` resolves, then close
/// WebSockets, hang up every session, and record them stopped. Raises this
/// process's soft descriptor limit to the hard one, and bounds what an
/// unauthenticated client can hold open (see `server/listen.rs`).
pub async fn serve(
    listener: TcpListener,
    hub: Arc<Hub>,
    options: ServerOptions,
    shutdown_signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = crate::handoff::GatedListener::new(listener, crate::handoff::Gate::new());
    serve_gated(listener, hub, options, shutdown_signal).await
}

/// [`serve`] behind a live-handoff [`crate::handoff::Gate`]: while a
/// handoff has the hub frozen, connections wait in the backlog and
/// requests get 503; a shutdown signal waits for a handoff under way.
pub async fn serve_gated(
    listener: crate::handoff::GatedListener,
    hub: Arc<Hub>,
    options: ServerOptions,
    shutdown_signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let (tx, rx) = watch::channel(false);
    let gate = listener.gate().clone();
    let trusted_proxies = options.trusted_proxies.clone();
    let app = router(Arc::clone(&hub), options, rx).layer(middleware::from_fn_with_state(
        gate.clone(),
        crate::handoff::gate_requests,
    ));
    listen::raise_fd_limit();
    let shutdown_signal = async move {
        shutdown_signal.await;
        // A handoff under way finishes (exiting this process) or rolls
        // back first.
        gate.handoff_settled().await;
    };
    listen::run(
        listener,
        app,
        listen::connection_cap(),
        trusted_proxies,
        shutdown_signal,
        tx,
    )
    .await;
    let _ = tokio::task::spawn_blocking(move || hub.shutdown(SHUTDOWN_TIMEOUT)).await;
    Ok(())
}

// --- bounded bodies ---------------------------------------------------------

/// How much body a request may send, and how long it has to send it all.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BodyBound {
    pub limit: usize,
    pub timeout: Duration,
}

impl BodyBound {
    /// The unauthenticated routes: a login form is a few hundred bytes.
    /// Shorter than the handoff's freeze wait ([`crate::handoff`]) and
    /// [`listen::CLOSE_GRACE`], so a slow client never makes either give up.
    pub const PUBLIC: Self = Self {
        limit: 4 * 1024,
        timeout: Duration::from_secs(5),
    };
}

/// Middleware: read the whole body within `bound` (408 when too slow, 413
/// when too big; the connection closes either way), then hand the handler
/// the buffered bytes.
pub(crate) async fn bound_body(
    State(bound): State<BodyBound>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let read = tokio::time::timeout(bound.timeout, axum::body::to_bytes(body, bound.limit)).await;
    let refused = match read {
        Ok(Ok(bytes)) => {
            return next
                .run(axum::extract::Request::from_parts(
                    parts,
                    axum::body::Body::from(bytes),
                ))
                .await;
        }
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            "the request body was too slow\n",
        ),
        Ok(Err(_)) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "the request body is too large\n",
        ),
    };
    let mut response = refused.into_response();
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

// --- auth -------------------------------------------------------------------

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// A connection's two ends, recorded per connection by [`serve`].
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
}

/// Who sent a request: the client address the failure limiter keys on,
/// whether the TCP peer is a proxy we trust (loopback -- e.g.
/// `tailscale serve` -- or a `trusted_proxies` address: its
/// `X-Forwarded-For` / `-Host` / `-Proto` are believed), and whether the
/// request comes straight from this machine (no proxy in between).
#[derive(Debug, Clone, Copy)]
struct Client {
    ip: IpAddr,
    trusted_peer: bool,
    /// A process on this machine talking to the hub directly (the CLI, a
    /// local browser): exempt from the failure lockout, which guards
    /// against the network.
    same_machine: bool,
}

impl Client {
    fn of(extensions: &Extensions, headers: &HeaderMap, options: &ServerOptions) -> Self {
        // `router` used without connect info (an in-process caller) is
        // this machine. Under a dual-stack (`::`) bind IPv4 peers arrive
        // IPv4-mapped (`::ffff:127.0.0.1`): fold them, or a local proxy
        // would not count as loopback.
        let (peer, local) = match extensions.get::<ConnectInfo<Peer>>() {
            Some(ConnectInfo(p)) => (
                p.remote.ip().to_canonical(),
                p.local.map(|l| l.ip().to_canonical()),
            ),
            None => (IpAddr::V4(Ipv4Addr::LOCALHOST), None),
        };
        let trusted_peer = peer.is_loopback() || options.trusted_proxies.contains(&peer);
        // The proxy appends the address it saw: the last entry is the one
        // a client cannot forge.
        let forwarded_for = trusted_peer
            .then(|| header_str(headers, header::HeaderName::from_static("x-forwarded-for")))
            .flatten();
        let forwarded = forwarded_for
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
            .map(|ip| ip.to_canonical());
        // From this machine: loopback, or our own address as the source
        // (a connection to a LAN bind from this host).
        let from_here = peer.is_loopback() || local == Some(peer);
        Self {
            ip: forwarded.unwrap_or(peer),
            trusted_peer,
            same_machine: from_here && forwarded_for.is_none(),
        }
    }
}

impl FromRequestParts<AppState> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self::of(&parts.extensions, &parts.headers, &app.options))
    }
}

/// The outcome of checking a request's credentials.
enum Auth {
    /// Admitted: by a login cookie (its secret) or by the token (`None`).
    Granted(Option<String>),
    /// Admitted with a peer credential (another hub): the API only, within
    /// the peer scope (`peers::peer_forbidden`).
    Peer(crate::peers::PeerInfo),
    /// No credential presented.
    Anonymous,
    /// A wrong credential (counted against the client).
    Denied,
    /// The client failed too often; nothing it presents is checked.
    Limited(Duration),
}

/// The login cookie values a request carries for the current token.
fn login_cookies<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| cookie_value(v, name))
        .collect()
}

/// Check a request's login cookie and bearer token.
///
/// A live login cookie is accepted even from a locked-out address: it is a
/// 256-bit secret the hub issued (guessing one is hopeless at any rate),
/// and refusing it would let one attacker sharing a NAT or proxy address
/// with real users lock them all out. Everything else from a locked-out
/// address is refused unchecked.
fn authenticate(app: &AppState, headers: &HeaderMap, client: Client) -> Auth {
    let token = app.hub.token();
    let cookies = login_cookies(headers, &cookie_name(token.expose()));
    let bearer = header_str(headers, header::AUTHORIZATION).and_then(bearer_value);
    if cookies.is_empty() && bearer.is_none() {
        return Auth::Anonymous;
    }
    let bearer_ok = || bearer.is_some_and(|b| token.matches(b));
    if let Some(login) = cookies.iter().find(|c| app.hub.login_valid(c)) {
        // A valid bearer alongside makes the token the credential: it
        // outlives the login.
        return Auth::Granted((!bearer_ok()).then(|| (*login).to_owned()));
    }
    let check = || {
        if bearer_ok() {
            return Some(Auth::Granted(None));
        }
        bearer.and_then(|b| app.peers.verify(b)).map(Auth::Peer)
    };
    if client.same_machine {
        return check().unwrap_or(Auth::Denied);
    }
    let mut failures = app.failures.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if let Some(wait) = failures.locked(client.ip, now) {
        return Auth::Limited(wait);
    }
    match check() {
        Some(auth) => auth,
        None => {
            failures.record_failure(client.ip, now);
            Auth::Denied
        }
    }
}

fn origin_ok(headers: &HeaderMap, client: Client, options: &ServerOptions) -> bool {
    let forwarded_host = client
        .trusted_peer
        .then(|| header_str(headers, header::HeaderName::from_static("x-forwarded-host")))
        .flatten();
    let hosts: Vec<&str> = header_str(headers, header::HOST)
        .into_iter()
        .chain(forwarded_host)
        .collect();
    origin_allowed(
        header_str(headers, header::ORIGIN),
        &hosts,
        &options.allowed_origins,
    )
}

fn too_many_attempts(wait: Duration) -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "too many failed login attempts from this address; try again later",
    );
    insert_retry_after(&mut response, wait);
    response
}

fn insert_retry_after(response: &mut Response, wait: Duration) {
    let seconds = wait.as_secs().max(1).to_string();
    if let Ok(value) = HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
}

/// How often a WebSocket a login opened re-checks that the login is still
/// live (expiry is not announced; logout and eviction are).
const LOGIN_RECHECK: Duration = Duration::from_secs(60);

/// What ends a WebSocket's right to stay open: the credential that opened
/// it. Subscribed before the credentials were checked, so a socket granted
/// on a token that rotates at any later moment sees the change and closes;
/// one a login cookie opened also closes once that login ends (logout,
/// expiry, eviction).
#[derive(Clone)]
struct CredentialWatch {
    rotations: watch::Receiver<u64>,
    login: Option<LoginWatch>,
    peer: Option<PeerWatch>,
}

/// A peer credential's socket closes when that peer is revoked (rotating
/// the token leaves peers alone).
#[derive(Clone)]
struct PeerWatch {
    id: String,
    store: Arc<crate::peers::PeerStore>,
    changes: watch::Receiver<u64>,
}

#[derive(Clone)]
struct LoginWatch {
    secret: String,
    changes: watch::Receiver<u64>,
}

impl CredentialWatch {
    fn new(hub: &Hub) -> Self {
        Self {
            rotations: hub.subscribe_rotations(),
            login: None,
            peer: None,
        }
    }

    /// `changes` must be subscribed before the credential was verified, so
    /// a revocation racing the check is still seen.
    fn for_peer(
        mut self,
        store: &Arc<crate::peers::PeerStore>,
        changes: watch::Receiver<u64>,
        id: String,
    ) -> Self {
        self.peer = Some(PeerWatch {
            id,
            store: Arc::clone(store),
            changes,
        });
        self
    }

    fn for_login(mut self, hub: &Hub, secret: Option<String>) -> Self {
        self.login = secret.map(|secret| LoginWatch {
            secret,
            changes: hub.subscribe_login_changes(),
        });
        self
    }

    /// Resolves with the close frame to send once the credential is no
    /// longer valid; never while it is. Cancel-safe (each `select!` arm
    /// re-polls it), with `recheck` kept by the caller so it keeps time
    /// across polls.
    async fn revoked(&mut self, hub: &Hub, recheck: &mut tokio::time::Interval) -> CloseFrame {
        loop {
            let login_event = async {
                match &mut self.login {
                    Some(login) => {
                        tokio::select! {
                            changed = login.changes.changed() => changed.is_ok(),
                            _ = recheck.tick() => true,
                        }
                    }
                    None => std::future::pending().await,
                }
            };
            let peer_event = async {
                match &mut self.peer {
                    Some(peer) => peer.changes.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                changed = self.rotations.changed() => {
                    if changed.is_ok() {
                        if self.peer.is_some() {
                            continue;
                        }
                        return CloseFrame {
                            code: WS_CLOSE_ROTATED,
                            reason: "token rotated: log in again".into(),
                        };
                    }
                    // The hub is gone: shutdown handles it.
                    std::future::pending::<()>().await;
                }
                alive = peer_event => {
                    if !alive {
                        std::future::pending::<()>().await;
                    }
                    if let Some(peer) = &self.peer
                        && !peer.store.contains(&peer.id)
                    {
                        return CloseFrame {
                            code: WS_CLOSE_PEER_REVOKED,
                            reason: "peer credential revoked".into(),
                        };
                    }
                }
                alive = login_event => {
                    if !alive {
                        std::future::pending::<()>().await;
                    }
                    if let Some(login) = &self.login
                        && !hub.login_valid(&login.secret)
                    {
                        return CloseFrame {
                            code: WS_CLOSE_SIGNED_OUT,
                            reason: "signed out: log in again".into(),
                        };
                    }
                }
            }
        }
    }
}

fn login_recheck() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(LOGIN_RECHECK);
    interval.reset();
    interval
}

async fn require_auth(
    State(app): State<AppState>,
    client: Client,
    mut request: Request,
    next: Next,
) -> Response {
    // Both subscribed before the check: a rotation or a peer revocation
    // racing it is still seen by the socket it opens.
    let watch = CredentialWatch::new(&app.hub);
    let peer_changes = app.peers.subscribe();
    match authenticate(&app, request.headers(), client) {
        Auth::Granted(login) => {
            request
                .extensions_mut()
                .insert(watch.for_login(&app.hub, login));
            next.run(request).await
        }
        Auth::Peer(peer) => {
            if crate::peers::peer_forbidden(request.uri().path()) {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "not allowed with a peer credential (this endpoint takes the hub's \
                     token)",
                );
            }
            let watch = watch.for_peer(&app.peers, peer_changes, peer.id.clone());
            request.extensions_mut().insert(watch);
            request.extensions_mut().insert(peer);
            next.run(request).await
        }
        Auth::Limited(wait) => too_many_attempts(wait),
        Auth::Anonymous | Auth::Denied => {
            error_response(StatusCode::UNAUTHORIZED, "authentication required")
        }
    }
}

/// Cross-origin browser requests may not change state.
async fn origin_guard(
    State(app): State<AppState>,
    client: Client,
    request: Request,
    next: Next,
) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe && !origin_ok(request.headers(), client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    next.run(request).await
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let fixed = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        // Not `no-referrer`: under it browsers send `Origin: null` on
        // same-origin form posts, which the origin guard must refuse.
        (header::REFERRER_POLICY, "same-origin"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; \
             base-uri 'none'; form-action 'self'",
        ),
    ];
    for (name, value) in fixed {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

/// Whether the browser reached the hub over HTTPS (through a TLS proxy):
/// an `X-Forwarded-Proto: https` from a local proxy, or a `Host` that is
/// one of the configured `https://` origins.
fn browser_https(headers: &HeaderMap, client: Client, options: &ServerOptions) -> bool {
    let proto = client
        .trusted_peer
        .then(|| {
            header_str(
                headers,
                header::HeaderName::from_static("x-forwarded-proto"),
            )
        })
        .flatten()
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    proto
        || header_str(headers, header::HOST).is_some_and(|host| {
            options.allowed_origins.iter().any(|origin| {
                origin
                    .trim_end_matches('/')
                    .strip_prefix("https://")
                    .is_some_and(|authority| {
                        let authority = authority.to_ascii_lowercase();
                        let host = host.trim().to_ascii_lowercase();
                        host == authority || host.strip_suffix(":443") == Some(authority.as_str())
                    })
            })
        })
}

// --- pages ------------------------------------------------------------------

#[derive(Deserialize)]
struct IndexQuery {
    /// Why the web UI signed out (`rotated`: the token was rotated).
    signed_out: Option<String>,
}

async fn index(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Response {
    match authenticate(&app, &headers, client) {
        Auth::Granted(_) => {
            let mut response = no_store(Html(web::INDEX_HTML).into_response());
            // The service worker keeps only this page (never the sign-in
            // page) as the offline app shell.
            response.headers_mut().insert(
                header::HeaderName::from_static(web::SHELL_HEADER),
                HeaderValue::from_static("app"),
            );
            response
        }
        Auth::Limited(wait) => login_refused(wait),
        Auth::Anonymous | Auth::Denied | Auth::Peer(_) => {
            let notice = match query.signed_out.as_deref() {
                Some("rotated") => Some(
                    "Signed out: the hub's token was rotated. Sign in again with a fresh link from `fw hub url`.",
                ),
                Some("ended") => Some(
                    "Signed out: this browser's login ended. Sign in again with a fresh link from `fw hub url`.",
                ),
                _ => None,
            };
            no_store(Html(web::login_page(notice)).into_response())
        }
    }
}

/// `/login` parameters: the token, or a one-time code (the form's single
/// field takes either).
#[derive(Deserialize)]
struct LoginParam {
    token: Option<String>,
    code: Option<String>,
}

impl LoginParam {
    fn presented(&self) -> Option<&str> {
        self.code
            .as_deref()
            .or(self.token.as_deref())
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }
}

async fn login_query(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Query(param): Query<LoginParam>,
) -> Response {
    login(&app, &headers, client, param.presented())
}

async fn login_form(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Form(param): Form<LoginParam>,
) -> Response {
    login(&app, &headers, client, param.presented())
}

fn login_refused(wait: Duration) -> Response {
    let minutes = wait.as_secs().div_ceil(60).max(1);
    let message = format!(
        "Too many failed attempts from this address. Try again in {minutes} minute{}.",
        if minutes == 1 { "" } else { "s" }
    );
    let mut response = Html(web::login_page(Some(&message))).into_response();
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    insert_retry_after(&mut response, wait);
    no_store(response)
}

/// Trade the token or a one-time code for a login cookie carrying a fresh
/// per-login secret. Success answers with a page that replaces itself with
/// `/` (keeping any `#session=` fragment) from this origin: a same-site
/// navigation, so the `SameSite=Strict` cookie is sent even when the login
/// link was opened from another site (a redirect would stay cross-site),
/// and the spent credential URL leaves the address bar and history.
fn login(app: &AppState, headers: &HeaderMap, client: Client, presented: Option<&str>) -> Response {
    let Some(presented) = presented else {
        return no_store(Html(web::login_page(None)).into_response());
    };
    let token = app.hub.token();
    {
        let mut failures = app.failures.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if !client.same_machine
            && let Some(wait) = failures.locked(client.ip, now)
        {
            return login_refused(wait);
        }
        if !token.matches(presented) && !app.hub.redeem_login_code(presented) {
            if !client.same_machine {
                failures.record_failure(client.ip, now);
            }
            let mut response = Html(web::login_page(Some(
                "That token or code is not valid (codes work once, for 5 minutes).",
            )))
            .into_response();
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            return no_store(response);
        }
    }
    let secret = match app.hub.issue_login() {
        Ok(secret) => secret,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let cookie = login_cookie(
        &token,
        &secret,
        browser_https(headers, client, &app.options),
    );
    let mut response = Html(web::LOGGED_IN_HTML).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    no_store(response)
}

/// Forget this browser's login (server side too) and clear its cookie.
async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let token = app.hub.token();
    for secret in login_cookies(&headers, &cookie_name(token.expose())) {
        if let Err(e) = app.hub.revoke_login(secret) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&logout_cookie(&token)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// Embedded assets. The app's code needs the token; the fonts and icon the
/// login page also uses are public.
async fn asset(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    if !web::is_public_asset(&path) {
        match authenticate(&app, &headers, client) {
            Auth::Granted(_) => {}
            Auth::Limited(wait) => return too_many_attempts(wait),
            Auth::Anonymous | Auth::Denied | Auth::Peer(_) => {
                return error_response(StatusCode::UNAUTHORIZED, "authentication required");
            }
        }
    }
    match web::asset(&path) {
        Some((bytes, content_type)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        None => error_response(StatusCode::NOT_FOUND, "no such asset"),
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// --- errors -----------------------------------------------------------------

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

struct ApiError(HubError);

impl From<HubError> for ApiError {
    fn from(e: HubError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            HubError::State(StateError::Invalid(_)) => StatusCode::BAD_REQUEST,
            HubError::State(StateError::NotFound(_)) => StatusCode::NOT_FOUND,
            HubError::State(StateError::Conflict(_)) => StatusCode::CONFLICT,
            HubError::Spawn(crate::pty::SpawnError::EmptyCommand) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error_response(status, &self.0.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

async fn api_not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "no such endpoint")
}

async fn api_method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed for this endpoint",
    )
}

/// A WebSocket upgrade the request could not make, in the API's form.
fn upgrade_refused(
    rejection: &axum::extract::ws::rejection::WebSocketUpgradeRejection,
) -> Response {
    error_response(
        rejection.status(),
        &format!("WebSocket upgrade required: {}", rejection.body_text()),
    )
}

/// `Json` whose rejections (bad syntax, wrong shape, missing content type)
/// answer in the API's `{"error": ...}` form like every other error.
struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(error_response(
                rejection.status(),
                &bounded_message(&rejection_message(&rejection.body_text())),
            )),
        }
    }
}

/// An extractor rejection's text without axum's boilerplate lead-in
/// ("Failed to deserialize the JSON body into the target type: kind:
/// unknown variant ..." -> "kind: unknown variant ...").
fn rejection_message(text: &str) -> String {
    const LEADS: [&str; 4] = [
        "Failed to deserialize the JSON body into the target type: ",
        "Failed to parse the request body as JSON: ",
        "Failed to deserialize query string: ",
        "Failed to deserialize form body: ",
    ];
    let text = LEADS
        .iter()
        .find_map(|lead| text.strip_prefix(lead))
        .unwrap_or(text);
    text.to_owned()
}

/// Longest body-rejection message echoed back. serde's error names the
/// path to the bad field, which for a deeply nested body (a layout tree)
/// is the whole nesting; the head and the reason at the end are kept.
const MAX_REJECTION_MESSAGE: usize = 240;

fn bounded_message(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    if chars.len() <= MAX_REJECTION_MESSAGE {
        return message.to_string();
    }
    let head: String = chars[..120].iter().collect();
    let tail: String = chars[chars.len() - 100..].iter().collect();
    format!("{head} ... {tail}")
}

/// Run a blocking hub call (state-file writes, PTY spawns) off the async
/// workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, HubError> + Send + 'static,
) -> ApiResult<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(ApiError),
        Err(join) => Err(ApiError(HubError::Internal(format!(
            "worker task failed: {join}"
        )))),
    }
}

// --- workspaces -------------------------------------------------------------

// --- layouts ----------------------------------------------------------------

// --- sessions ---------------------------------------------------------------

// --- credentials ------------------------------------------------------------

/// Mint a one-time login code: `{"code","expires_in"}` (seconds). Open
/// `/login?code=<code>` once within that time to get the login cookie.
async fn mint_login_code(State(app): State<AppState>) -> ApiResult<Response> {
    let code = app.hub.mint_login_code()?;
    Ok(no_store(
        Json(json!({ "code": code, "expires_in": LOGIN_CODE_TTL.as_secs() })).into_response(),
    ))
}

/// Rotate the token: every cookie, code, and socket of the old one stops
/// working at once.
async fn rotate_token(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.rotate_token()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- websocket --------------------------------------------------------------

// --- events -------------------------------------------------------------------

/// The login URL for a one-time `code` under `base`.
pub fn code_login_url(base: &str, code: &str) -> String {
    format!("{}/login?code={code}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;

    fn client(remote: &str, local: &str, xff: Option<&str>, options: &ServerOptions) -> Client {
        let mut extensions = Extensions::new();
        extensions.insert(ConnectInfo(Peer {
            remote: remote.parse().unwrap(),
            local: Some(local.parse().unwrap()),
        }));
        let mut headers = HeaderMap::new();
        if let Some(xff) = xff {
            headers.insert("x-forwarded-for", xff.parse().unwrap());
        }
        Client::of(&extensions, &headers, options)
    }

    #[test]
    fn ipv4_mapped_loopback_is_a_local_proxy() {
        // `--bind ::` sees a local proxy (tailscale serve -> 127.0.0.1) as
        // ::ffff:127.0.0.1: its X-Forwarded-For must be believed, and the
        // proxied client must not count as this machine.
        let options = ServerOptions::default();
        let proxied = client(
            "[::ffff:127.0.0.1]:5000",
            "[::ffff:127.0.0.1]:7420",
            Some("100.101.102.103"),
            &options,
        );
        assert!(proxied.trusted_peer);
        assert!(!proxied.same_machine);
        assert_eq!(proxied.ip, "100.101.102.103".parse::<IpAddr>().unwrap());
        let direct = client(
            "[::ffff:127.0.0.1]:5000",
            "[::ffff:127.0.0.1]:7420",
            None,
            &options,
        );
        assert!(direct.same_machine && direct.trusted_peer);
        // A mapped LAN peer is keyed as plain IPv4 and is not trusted.
        let lan = client(
            "[::ffff:192.168.1.9]:5000",
            "[::ffff:192.168.1.5]:7420",
            Some("1.2.3.4"),
            &options,
        );
        assert!(!lan.trusted_peer && !lan.same_machine);
        assert_eq!(lan.ip, "192.168.1.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_proxies_forward_their_clients() {
        let options = ServerOptions {
            trusted_proxies: vec!["10.0.0.2".parse().unwrap()],
            ..ServerOptions::default()
        };
        let behind = client(
            "10.0.0.2:5000",
            "10.0.0.5:7420",
            Some("evil, 198.51.100.7"),
            &options,
        );
        assert!(behind.trusted_peer && !behind.same_machine);
        assert_eq!(behind.ip, "198.51.100.7".parse::<IpAddr>().unwrap());
        let untrusted = client(
            "10.0.0.3:5000",
            "10.0.0.5:7420",
            Some("198.51.100.7"),
            &options,
        );
        assert!(!untrusted.trusted_peer);
        assert_eq!(untrusted.ip, "10.0.0.3".parse::<IpAddr>().unwrap());
        // This host connecting to its own LAN address is this machine.
        let own = client("10.0.0.5:5000", "10.0.0.5:7420", None, &options);
        assert!(own.same_machine && !own.trusted_peer);
    }
    /// Serve `bound_body` over `bound` on a real socket; send a head
    /// promising `length` bytes, then `body` (one byte at a time, `pause`
    /// apart, or at once); return the status line.
    async fn bounded(bound: BodyBound, length: usize, body: &[u8], pause: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let app = Router::new()
            .route(
                "/login",
                post(|body: Bytes| async move { body.len().to_string() }),
            )
            .route_layer(middleware::from_fn_with_state(bound, bound_body));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // The hub's own accept loop (its hyper settings decide the close).
        let listener = crate::handoff::GatedListener::new(listener, crate::handoff::Gate::new());
        let (closing, _) = watch::channel(false);
        tokio::spawn(listen::run(
            listener,
            app,
            4,
            Vec::new(),
            std::future::pending(),
            closing,
        ));
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        let head = format!(
            "POST /login HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n"
        );
        write.write_all(head.as_bytes()).await.unwrap();
        let body = body.to_vec();
        // The trickle goes on while the answer is read (as a slow client's).
        let writer = tokio::spawn(async move {
            if pause.is_zero() {
                let _ = write.write_all(&body).await;
                return write;
            }
            for byte in body {
                if write.write_all(&[byte]).await.is_err() {
                    break;
                }
                tokio::time::sleep(pause).await;
            }
            write
        });
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), read.read_to_end(&mut answer)).await;
        writer.abort();
        String::from_utf8_lossy(&answer)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    #[tokio::test]
    async fn a_trickled_public_body_times_out_and_closes() {
        let bound = BodyBound {
            limit: 4096,
            timeout: Duration::from_millis(300),
        };
        let started = Instant::now();
        // 100 bytes promised, one sent every 100 ms: over the deadline.
        let status = bounded(bound, 100, &[b'a'; 100], Duration::from_millis(100)).await;
        assert!(status.contains("408"), "{status}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        // A prompt body passes through whole.
        let ok = bounded(bound, 5, b"token", Duration::ZERO).await;
        assert!(ok.contains("200"), "{ok}");
        // Too big is refused before the handler.
        let big = bounded(bound, 5000, &[b'a'; 5000], Duration::ZERO).await;
        assert!(big.contains("413"), "{big}");
    }

    #[test]
    fn the_public_body_deadline_is_inside_the_settle_waits() {
        assert!(BodyBound::PUBLIC.timeout < crate::handoff::FREEZE_TIMEOUT);
        assert!(BodyBound::PUBLIC.timeout < listen::CLOSE_GRACE);
    }

    /// A peer revoked after its credential was checked but before its
    /// socket's watch existed still has that socket closed.
    #[tokio::test]
    async fn a_revocation_racing_the_check_still_closes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(crate::HubOptions::new(dir.path().join("state"))).unwrap();
        let store = Arc::new(crate::peers::PeerStore::load(&dir.path().join("state")));
        let (peer, credential) = store.issue("box").unwrap();
        // As require_auth: subscribe, verify, (revoked here), watch.
        let changes = store.subscribe();
        assert!(store.verify(&credential).is_some());
        store.revoke(&peer.id).unwrap();
        let mut watch = CredentialWatch::new(&hub).for_peer(&store, changes, peer.id.clone());
        let mut recheck = login_recheck();
        let close = tokio::time::timeout(Duration::from_secs(5), watch.revoked(&hub, &mut recheck))
            .await
            .expect("the racing revocation is seen");
        assert_eq!(close.code, WS_CLOSE_PEER_REVOKED);
    }
}
