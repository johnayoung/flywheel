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
//! * Rotating the token closes every socket with code 4001.
//!
//! Events WebSocket (`GET /api/events?client=<id>`), hub-wide, text JSON:
//!
//! * server -> client: `{"type":"snapshot","sessions":{id:status},
//!   "workspaces":{id:rollup}}` on connect (and after falling behind);
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
//! * Rotating the token closes it with code 4001 too.

mod commands_api;
mod flywheel_api;
mod import_api;
mod notify_api;
mod pwa;
mod worktree_api;

use crate::auth::{
    FailureLimiter, LOGIN_CODE_TTL, bearer_value, cookie_name, cookie_value, login_cookie,
    logout_cookie, origin_allowed,
};
use crate::hub::{CreateSession, Hub, HubError, HubEvent};
use crate::pty::{Attachment, ExitInfo, PtySession, SessionEvent, WriteOutcome};
use crate::state::{StateError, WorkspaceInput, is_git_root, validate_folder};
use crate::vt::{DEFAULT_RECENT_LINES, SNAPSHOT_HISTORY_BYTES, TextSource, ViewerColors};
use crate::web;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{
    ConnectInfo, Form, FromRequest, FromRequestParts, Path, Query, Request, State,
};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::SinkExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

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
}

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    options: Arc<ServerOptions>,
    shutdown: watch::Receiver<bool>,
    failures: Arc<Mutex<FailureLimiter>>,
}

/// Build the router. Flipping `shutdown` to `true` closes every open
/// WebSocket so a graceful shutdown can finish.
pub fn router(hub: Arc<Hub>, options: ServerOptions, shutdown: watch::Receiver<bool>) -> Router {
    let state = AppState {
        hub,
        options: Arc::new(options),
        shutdown,
        failures: Arc::new(Mutex::new(FailureLimiter::default())),
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
        .route("/api/folders/check", get(check_folder))
        // The pre-folders name of the same check.
        .route("/api/repos/check", get(check_folder))
        .route("/api/workspaces/import", post(import_api::import_workspace))
        .route(
            "/api/code-workspaces",
            get(import_api::find_code_workspaces),
        )
        .route("/api/sessions/{id}/kill", post(kill_session))
        .route("/api/sessions/{id}/restart", post(restart_session))
        .route("/api/sessions/{id}/screen", get(session_screen))
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
        .route("/api/sessions/{id}/ws", get(session_ws))
        .route("/api/login-codes", post(mint_login_code))
        .route("/api/token/rotate", post(rotate_token))
        .route("/api/events", get(events_ws))
        .route("/api/notify", get(notify_api::status))
        .route("/api/notify/test", post(notify_api::test))
        .route("/api/flywheel", get(flywheel_api::list))
        .route("/api/flywheel/panel", get(flywheel_api::panel))
        .route("/api/flywheel/run", get(flywheel_api::run_output))
        .route("/api/flywheel/worker", post(flywheel_api::start_worker))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_query).post(login_form))
        .route("/logout", post(logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(asset))
        .route("/manifest.webmanifest", get(pwa::manifest))
        .route("/sw.js", get(pwa::service_worker))
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Serve on `listener` until `shutdown_signal` resolves, then close
/// WebSockets, hang up every session, and record them stopped.
pub async fn serve(
    listener: TcpListener,
    hub: Arc<Hub>,
    options: ServerOptions,
    shutdown_signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let (tx, rx) = watch::channel(false);
    let app = router(Arc::clone(&hub), options, rx);
    let app = app.into_make_service_with_connect_info::<Peer>();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal.await;
            let _ = tx.send(true);
        })
        .await;
    let _ = tokio::task::spawn_blocking(move || hub.shutdown(SHUTDOWN_TIMEOUT)).await;
    result
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

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        Self {
            remote: *stream.remote_addr(),
            local: stream.io().local_addr().ok(),
        }
    }
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
    Granted,
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
    if cookies.iter().any(|c| app.hub.login_valid(c)) {
        return Auth::Granted;
    }
    let bearer_ok = || bearer.is_some_and(|b| token.matches(b));
    if client.same_machine {
        return if bearer_ok() {
            Auth::Granted
        } else {
            Auth::Denied
        };
    }
    let mut failures = app.failures.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if let Some(wait) = failures.locked(client.ip, now) {
        return Auth::Limited(wait);
    }
    if bearer_ok() {
        Auth::Granted
    } else {
        failures.record_failure(client.ip, now);
        Auth::Denied
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

/// The token-rotation watch a request subscribed to before its
/// credentials were checked: a WebSocket granted on a token that rotates
/// at any later moment sees the change and closes.
#[derive(Clone)]
struct RotationWatch(watch::Receiver<u64>);

async fn require_auth(
    State(app): State<AppState>,
    client: Client,
    mut request: Request,
    next: Next,
) -> Response {
    let rotations = app.hub.subscribe_rotations();
    match authenticate(&app, request.headers(), client) {
        Auth::Granted => {
            request.extensions_mut().insert(RotationWatch(rotations));
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
        Auth::Granted => {
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
        Auth::Anonymous | Auth::Denied => {
            let notice = match query.signed_out.as_deref() {
                Some("rotated") => Some(
                    "Signed out: the hub's token was rotated. Sign in again with a fresh link from `fw hub url`.",
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
            Auth::Granted => {}
            Auth::Limited(wait) => return too_many_attempts(wait),
            Auth::Anonymous | Auth::Denied => {
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
                &bounded_message(&rejection.body_text()),
            )),
        }
    }
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

// Both stat every folder (git/missing flags): off the executor.
async fn list_workspaces(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspaces = blocking(move || Ok(hub.workspaces())).await?;
    Ok(Json(json!({ "workspaces": workspaces })).into_response())
}

async fn get_workspace(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    Ok(Json(blocking(move || hub.workspace(&id)).await?).into_response())
}

async fn create_workspace(
    State(app): State<AppState>,
    ApiJson(input): ApiJson<WorkspaceInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = blocking(move || hub.create_workspace(input)).await?;
    Ok((StatusCode::CREATED, Json(workspace)).into_response())
}

async fn update_workspace(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<WorkspaceInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = blocking(move || hub.update_workspace(&id, input)).await?;
    Ok(Json(workspace).into_response())
}

async fn delete_workspace(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.delete_workspace(&id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- layouts ----------------------------------------------------------------

async fn get_layout(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let layout = app.hub.layout(&id)?;
    Ok(Json(json!({ "workspace_id": id, "rev": layout.rev, "tabs": layout.tabs })).into_response())
}

#[derive(Deserialize)]
struct LayoutInput {
    rev: u64,
    tabs: Vec<crate::layout::Tab>,
}

/// Save a layout written against `rev`. A stale `rev` is a 409 carrying
/// the current layout (`{"error","layout"}`), so the client can rebase.
async fn put_layout(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<LayoutInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = id.clone();
    let saved = blocking(move || hub.set_layout(&workspace, input.rev, input.tabs)).await;
    match saved {
        Ok(layout) => Ok(Json(
            json!({ "workspace_id": id, "rev": layout.rev, "tabs": layout.tabs }),
        )
        .into_response()),
        Err(ApiError(HubError::State(StateError::Conflict(message)))) => {
            let current = app.hub.layout(&id)?;
            Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": message,
                    "layout": { "workspace_id": id, "rev": current.rev, "tabs": current.tabs },
                })),
            )
                .into_response())
        }
        Err(e) => Err(e),
    }
}

// --- sessions ---------------------------------------------------------------

#[derive(Deserialize)]
struct SessionFilter {
    workspace_id: Option<String>,
}

async fn list_sessions(
    State(app): State<AppState>,
    Query(filter): Query<SessionFilter>,
) -> Response {
    Json(json!({ "sessions": app.hub.sessions(filter.workspace_id.as_deref()) })).into_response()
}

async fn get_session(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(app.hub.session(&id)?).into_response())
}

async fn create_session(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<CreateSession>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.create_session(request)).await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

#[derive(Deserialize)]
struct SessionPatch {
    name: Option<String>,
}

async fn update_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<SessionPatch>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || match patch.name {
        Some(name) => hub.rename_session(&id, &name),
        None => Ok(hub.session(&id)?),
    })
    .await?;
    Ok(Json(view).into_response())
}

async fn list_kinds(State(app): State<AppState>) -> Response {
    let hub = Arc::clone(&app.hub);
    match tokio::task::spawn_blocking(move || hub.kinds()).await {
        Ok(kinds) => Json(json!({ "kinds": kinds })).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}

#[derive(Deserialize)]
struct RepoQuery {
    path: Option<String>,
}

/// Validate one candidate folder path for the workspace form: always 200,
/// `{"ok":true,"path":canonical,"git":bool}` or `{"ok":false,"error":...}`
/// (a missing or blank `path` included).
async fn check_folder(Query(query): Query<RepoQuery>) -> Response {
    let Some(path) = query.path.filter(|p| !p.trim().is_empty()) else {
        return Json(json!({ "ok": false, "error": "path is required" })).into_response();
    };
    let raw = std::path::PathBuf::from(path.trim());
    let result = tokio::task::spawn_blocking(move || {
        validate_folder(&raw).map(|path| {
            let git = is_git_root(&path);
            (path, git)
        })
    })
    .await;
    match result {
        Ok(Ok((path, git))) => {
            Json(json!({ "ok": true, "path": path, "git": git })).into_response()
        }
        Ok(Err(e)) => Json(json!({ "ok": false, "error": e.to_string() })).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}

async fn kill_session(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.kill_session(&id)).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(Deserialize, Default)]
struct RestartRequest {
    cols: Option<u16>,
    rows: Option<u16>,
}

async fn restart_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<RestartRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.restart_session(&id, request.cols, request.rows)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
struct ScreenQuery {
    source: Option<String>,
    /// Parsed by hand so a bad value gets the API's JSON error.
    lines: Option<String>,
}

/// The rendered text of a session's terminal: `source=visible` (default)
/// is the screen's rows; `source=recent` the last `lines` logical lines of
/// scrollback plus screen.
async fn session_screen(
    State(app): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<ScreenQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Response> {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return Ok(error_response(rejection.status(), &rejection.body_text()));
        }
    };
    let lines = match query.lines.as_deref() {
        None => DEFAULT_RECENT_LINES,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_SCREEN_LINES).contains(&n) => n,
            _ => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("lines must be an integer between 1 and {MAX_SCREEN_LINES}"),
                ));
            }
        },
    };
    let source = match query.source.as_deref().unwrap_or("visible") {
        "visible" if query.lines.is_some() => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "lines applies to source=recent only (visible is always the whole screen)",
            ));
        }
        "visible" => TextSource::Visible,
        "recent" => TextSource::Recent { lines },
        other => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("unknown source {other:?} (visible or recent)"),
            ));
        }
    };
    app.hub.session(&id)?;
    let Some(session) = app.hub.live_session(&id) else {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            &format!("session {id} has no terminal on this server"),
        ));
    };
    let text = blocking(move || Ok(session.screen_text(source))).await?;
    let source = match source {
        TextSource::Visible => "visible",
        TextSource::Recent { .. } => "recent",
    };
    let mut body =
        serde_json::to_value(text).map_err(|e| ApiError(HubError::Internal(e.to_string())))?;
    body["source"] = json!(source);
    Ok(Json(body).into_response())
}

#[derive(Deserialize)]
struct InputRequest {
    #[serde(default)]
    data: String,
    /// Send `data` the way a terminal pastes it: newlines as `CR`, wrapped
    /// in bracketed-paste markers when the program enabled them.
    #[serde(default)]
    paste: bool,
    /// Press Enter afterwards (a separate `CR`, [`ENTER_DELAY`](crate::pty::ENTER_DELAY) later).
    #[serde(default)]
    enter: bool,
}

fn input_refused(outcome: WriteOutcome) -> Option<Response> {
    match outcome {
        WriteOutcome::Queued => None,
        WriteOutcome::Closed => Some(error_response(
            StatusCode::CONFLICT,
            "the session has exited",
        )),
        WriteOutcome::Backlogged => Some(error_response(
            StatusCode::CONFLICT,
            "input dropped: the terminal is not reading its input",
        )),
    }
}

/// Type into a session (for scripts and `fw hub send`): `data` as is, or
/// pasted (`paste`), then optionally Enter. Answers
/// `{"bytes","bracketed"}`.
async fn session_input(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<InputRequest>,
) -> ApiResult<Response> {
    let view = app.hub.session(&id)?;
    let Some(session) = app.hub.live_session(&id) else {
        return Ok(error_response(
            StatusCode::CONFLICT,
            &format!("session {id} has no terminal on this server"),
        ));
    };
    if session.exit_info().is_some() {
        return Ok(error_response(
            StatusCode::CONFLICT,
            "the session has exited",
        ));
    }
    if request.data.is_empty() && !request.enter {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "nothing to send: give data and/or enter",
        ));
    }
    let bracketed = request.paste && session.bracketed_paste();
    let bytes = if request.paste {
        crate::pty::paste_bytes(&request.data, bracketed)
    } else {
        request.data.into_bytes()
    };
    let mut sent = bytes.len();
    if !bytes.is_empty()
        && let Some(refused) = input_refused(session.write(bytes))
    {
        return Ok(refused);
    }
    if request.enter {
        if sent > 0 {
            tokio::time::sleep(crate::pty::ENTER_DELAY).await;
        }
        if let Some(refused) = input_refused(session.write(b"\r".to_vec())) {
            return Ok(refused);
        }
        sent += 1;
        // A command may have run: its git badge is looked at shortly.
        app.hub.nudge_badge_soon(&view.record);
    }
    Ok(Json(json!({ "bytes": sent, "bracketed": bracketed })).into_response())
}

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

async fn session_ws(
    State(app): State<AppState>,
    Path(id): Path<String>,
    client: Client,
    headers: HeaderMap,
    rotations: Option<axum::Extension<RotationWatch>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&headers, client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let Some(session) = app.hub.live_session(&id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("session {id} has no terminal on this server"),
        );
    };
    let shutdown = app.shutdown.clone();
    // Taken before the credentials were checked (see `require_auth`).
    let rotations = match rotations {
        Some(axum::Extension(RotationWatch(rx))) => rx,
        None => app.hub.subscribe_rotations(),
    };
    let hub = Arc::clone(&app.hub);
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            pump(socket, Arc::clone(&session), shutdown, rotations, &hub).await;
            // One viewer fewer: panes that follow another's size may now be
            // the only viewer and fit themselves (re-list for `live.clients`).
            drop(session);
            hub.emit(HubEvent::Sessions);
        })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Theme {
        foreground: String,
        background: String,
        cursor: String,
        ansi: Vec<String>,
    },
}

fn exit_message(info: &ExitInfo) -> Message {
    Message::Text(
        json!({ "type": "exit", "code": info.code, "signal": info.signal })
            .to_string()
            .into(),
    )
}

/// Snapshot a session (and subscribe) off the async workers.
async fn attach(session: &Arc<PtySession>, history_budget: usize) -> Option<Attachment> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || session.attach_with(history_budget))
        .await
        .ok()
}

/// Send an attachment's snapshot (and exit, if already exited), returning
/// its live receiver. The snapshot is released once sent.
async fn send_attachment(
    socket: &mut WebSocket,
    attachment: Attachment,
) -> Option<broadcast::Receiver<SessionEvent>> {
    let Attachment {
        replay,
        cols,
        rows,
        alt_screen,
        exit,
        events,
    } = attachment;
    let header = json!({
        "type": "snapshot",
        "cols": cols,
        "rows": rows,
        "alt_screen": alt_screen,
        "bytes": replay.len(),
    });
    socket
        .send(Message::Text(header.to_string().into()))
        .await
        .ok()?;
    let replay = Bytes::from(replay);
    let mut start = 0;
    while start < replay.len() {
        let end = replay.len().min(start + SNAPSHOT_FRAME);
        socket
            .send(Message::Binary(replay.slice(start..end)))
            .await
            .ok()?;
        start = end;
    }
    drop(replay);
    if let Some(info) = &exit {
        socket.send(exit_message(info)).await.ok()?;
    }
    Some(events)
}

/// Queue client input; tell the client when it was dropped because the
/// terminal is not reading. Returns false when the socket is gone.
async fn forward_input(socket: &mut WebSocket, session: &PtySession, bytes: Vec<u8>) -> bool {
    match session.write(bytes) {
        WriteOutcome::Queued | WriteOutcome::Closed => true,
        WriteOutcome::Backlogged => {
            let error = json!({
                "type": "error",
                "message": "input dropped: the terminal is not reading its input",
            });
            socket
                .send(Message::Text(error.to_string().into()))
                .await
                .is_ok()
        }
    }
}

async fn pump(
    mut socket: WebSocket,
    session: Arc<PtySession>,
    mut shutdown: watch::Receiver<bool>,
    mut rotations: watch::Receiver<u64>,
    hub: &Hub,
) {
    let Some(attachment) = attach(&session, SNAPSHOT_HISTORY_BYTES).await else {
        return;
    };
    let Some(mut events) = send_attachment(&mut socket, attachment).await else {
        return;
    };
    // One viewer more (`live.clients` in session listings).
    hub.emit(HubEvent::Sessions);
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    loop {
        tokio::select! {
            event = events.recv() => {
                let sent = match event {
                    Ok(SessionEvent::Output(bytes)) => socket.send(Message::Binary(bytes)).await.is_ok(),
                    Ok(SessionEvent::Exit(info)) => socket.send(exit_message(&info)).await.is_ok(),
                    Ok(SessionEvent::Resize { cols, rows }) => {
                        let size = json!({ "type": "size", "cols": cols, "rows": rows });
                        socket.send(Message::Text(size.to_string().into())).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Unsubscribe first, so the backlog this client
                        // will never read is not kept alive for it.
                        drop(events);
                        let reset = Message::Text(json!({ "type": "reset" }).to_string().into());
                        if socket.send(reset).await.is_err() {
                            break;
                        }
                        let Some(fresh) = attach(&session, LAGGED_SNAPSHOT_HISTORY).await else {
                            break;
                        };
                        let Some(fresh) = send_attachment(&mut socket, fresh).await else {
                            break;
                        };
                        events = fresh;
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if !sent {
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(ClientMessage::Input { data }) => {
                                if !forward_input(&mut socket, &session, data.into_bytes()).await {
                                    break;
                                }
                            }
                            Ok(ClientMessage::Resize { cols, rows }) => {
                                let _ = session.resize(cols, rows);
                            }
                            Ok(ClientMessage::Theme { foreground, background, cursor, ansi }) => {
                                match ViewerColors::from_hex(&foreground, &background, &cursor, &ansi) {
                                    Ok(colors) => session.set_viewer_colors(colors),
                                    Err(e) => {
                                        let error = json!({ "type": "error", "message": format!("bad theme: {e}") });
                                        if socket.send(Message::Text(error.to_string().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if socket.send(Message::Text(error.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if !forward_input(&mut socket, &session, bytes.to_vec()).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        // Complete the closing handshake: tungstenite queued
                        // the reply Close on receipt but only writes it on a
                        // flush (a send would be refused after closing), so
                        // the client sees a clean close instead of 1006.
                        let _ = socket.flush().await;
                        break;
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            changed = rotations.changed() => {
                if changed.is_ok() {
                    let close = CloseFrame {
                        code: WS_CLOSE_ROTATED,
                        reason: "token rotated: log in again".into(),
                    };
                    let _ = socket.send(Message::Close(Some(close))).await;
                    break;
                }
                // The hub is gone: shutdown handles it.
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }
}

// --- events -------------------------------------------------------------------

#[derive(Deserialize)]
struct EventsQuery {
    client: Option<String>,
}

/// A client id: 1..=64 of `[A-Za-z0-9_-]`.
fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn events_ws(
    State(app): State<AppState>,
    Query(query): Query<EventsQuery>,
    client: Client,
    headers: HeaderMap,
    rotations: Option<axum::Extension<RotationWatch>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&headers, client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let client = match query.client {
        Some(id) if valid_client_id(&id) => id,
        Some(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "client must be 1-64 letters, digits, '-' or '_'",
            );
        }
        None => crate::state::new_id(),
    };
    let hub = Arc::clone(&app.hub);
    let shutdown = app.shutdown.clone();
    let flywheel = app.options.flywheel.as_ref().map(|m| m.subscribe());
    // Taken before the credentials were checked (see `require_auth`).
    let rotations = match rotations {
        Some(axum::Extension(RotationWatch(rx))) => rx,
        None => app.hub.subscribe_rotations(),
    };
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| events_pump(socket, hub, client, shutdown, rotations, flywheel))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventsClientMessage {
    /// `session_id`: the focused pane's session; `visible`: the other
    /// panes on screen (a split layout), which count as viewed too.
    View {
        session_id: Option<String>,
        #[serde(default)]
        visible: Vec<String>,
    },
}

/// Unregisters a viewer however the pump ends.
struct ViewerGuard {
    hub: Arc<Hub>,
    viewer: u64,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.hub.disconnect_viewer(self.viewer);
    }
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

fn snapshot_message(hub: &Hub, client: &str) -> Message {
    let snapshot = hub.status_snapshot(client);
    text(json!({
        "type": "snapshot",
        "sessions": snapshot.sessions,
        "workspaces": snapshot.workspaces,
    }))
}

fn status_message(hub: &Hub, client: &str, session_id: &str) -> Message {
    match hub.status_for(client, session_id) {
        Some((view, workspace_id, rollup)) => text(json!({
            "type": "status",
            "session_id": session_id,
            "status": view,
            "workspace_id": workspace_id,
            "workspace_status": rollup,
        })),
        None => text(json!({
            "type": "status",
            "session_id": session_id,
            "status": null,
        })),
    }
}

async fn events_pump(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    client: String,
    mut shutdown: watch::Receiver<bool>,
    mut rotations: watch::Receiver<u64>,
    mut flywheel: Option<broadcast::Receiver<crate::flywheel::FlywheelEvent>>,
) {
    let mut events = hub.subscribe();
    let guard = ViewerGuard {
        viewer: hub.connect_viewer(&client),
        hub: Arc::clone(&hub),
    };
    if socket.send(snapshot_message(&hub, &client)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    loop {
        tokio::select! {
            event = events.recv() => {
                let sent = match event {
                    Ok(HubEvent::Status { session_id }) => {
                        socket.send(status_message(&hub, &client, &session_id)).await.is_ok()
                    }
                    Ok(HubEvent::Sessions) => {
                        socket.send(text(json!({ "type": "sessions" }))).await.is_ok()
                    }
                    Ok(HubEvent::Commands { session_id, added }) => {
                        socket.send(commands_api::event_message(&session_id, added)).await.is_ok()
                    }
                    Ok(HubEvent::Unwatched { .. }) => true,
                    Ok(HubEvent::Layout { workspace_id, rev }) => {
                        socket.send(text(json!({ "type": "layout", "workspace_id": workspace_id, "rev": rev }))).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        socket.send(snapshot_message(&hub, &client)).await.is_ok()
                            && socket.send(text(json!({ "type": "sessions" }))).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if !sent {
                    break;
                }
            }
            event = flywheel_api::next_event(&mut flywheel) => {
                if socket.send(flywheel_api::event_message(event)).await.is_err() {
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(body))) => {
                        match serde_json::from_str::<EventsClientMessage>(body.as_str()) {
                            Ok(EventsClientMessage::View { session_id, visible }) => {
                                let ids: Vec<&str> = session_id
                                    .iter()
                                    .chain(visible.iter())
                                    .map(String::as_str)
                                    .collect();
                                hub.set_viewing_many(guard.viewer, &ids);
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if socket.send(text(error)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        let _ = socket.flush().await;
                        break;
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            changed = rotations.changed() => {
                if changed.is_ok() {
                    let close = CloseFrame {
                        code: WS_CLOSE_ROTATED,
                        reason: "token rotated: log in again".into(),
                    };
                    let _ = socket.send(Message::Close(Some(close))).await;
                    break;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }
    drop(guard);
}

/// The login URL for a one-time `code` under `base`.
pub fn code_login_url(base: &str, code: &str) -> String {
    format!("{}/login?code={code}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
