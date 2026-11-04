//! Who a request is and what it may reach: the client address and proxy
//! trust ([`Client`]), credential checks (token, login cookie, peer
//! credential), the auth / origin / security-header middleware, what ends a
//! WebSocket's credential ([`CredentialWatch`]), and the public pages that
//! sign a browser in and out.

use super::{
    ApiResult, AppState, PageForm, PageQuery, Peer, ServerOptions, WS_CLOSE_PEER_REVOKED,
    WS_CLOSE_ROTATED, WS_CLOSE_SIGNED_OUT, blocking, error_response, header_str,
};
use crate::auth::{
    LOGIN_CODE_TTL, bearer_value, cookie_name, cookie_value, login_cookie, logout_cookie,
    origin_allowed,
};
use crate::hub::Hub;
use crate::web;
use axum::Json;
use axum::extract::ws::CloseFrame;
use axum::extract::{ConnectInfo, FromRequestParts, Path, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Who sent a request: the client address the failure limiter keys on,
/// whether the TCP peer is a proxy we trust (loopback -- e.g.
/// `tailscale serve` -- or a `trusted_proxies` address: its
/// `X-Forwarded-For` / `-Host` / `-Proto` are believed), and whether the
/// request comes straight from this machine (no proxy in between).
#[derive(Debug, Clone, Copy)]
pub(super) struct Client {
    pub(super) ip: IpAddr,
    pub(super) trusted_peer: bool,
    /// A process on this machine talking to the hub directly (the CLI, a
    /// local browser): exempt from the failure lockout, which guards
    /// against the network.
    pub(super) same_machine: bool,
    /// Over the hub's own TLS (`--tls`).
    pub(super) tls: bool,
}

impl Client {
    pub(super) fn of(
        extensions: &Extensions,
        headers: &HeaderMap,
        options: &ServerOptions,
    ) -> Self {
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
            tls: super::tls_listen::over_tls(extensions),
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
pub(super) enum Auth {
    /// Admitted: by a login cookie (its secret) or by the token (`None`).
    Granted(Option<String>),
    /// Admitted with a peer credential (another hub): the API only, within
    /// the peer scope (`peers::peer_forbidden`).
    Peer(crate::peers::PeerInfo),
    /// A share link's credential, presented outside its `/share/` path:
    /// it reaches nothing else (403).
    View,
    /// No credential presented.
    Anonymous,
    /// A wrong credential (counted against the client).
    Denied,
    /// The client failed too often; nothing it presents is checked.
    Limited(Duration),
}

/// The login cookie values a request carries for the current token.
pub(super) fn login_cookies<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
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
pub(super) fn authenticate(app: &AppState, headers: &HeaderMap, client: Client) -> Auth {
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
        let bearer = bearer?;
        if let Some(peer) = app.peers.verify(bearer) {
            return Some(Auth::Peer(peer));
        }
        matches!(
            app.hub.shares().verify(bearer),
            crate::share::Verdict::Live(_)
        )
        .then_some(Auth::View)
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

pub(super) fn origin_ok(headers: &HeaderMap, client: Client, options: &ServerOptions) -> bool {
    let forwarded_host = client
        .trusted_peer
        .then(|| header_str(headers, header::HeaderName::from_static("x-forwarded-host")))
        .flatten();
    let hosts: Vec<&str> = header_str(headers, header::HOST)
        .into_iter()
        .chain(forwarded_host)
        .collect();
    // Over the hub's own TLS its pages are an https origin: an http page
    // on the same host and port is another origin.
    let origin = header_str(headers, header::ORIGIN);
    if client.tls
        && forwarded_host.is_none()
        && origin.is_some_and(|o| {
            let o = o.trim().trim_end_matches('/');
            o.get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
                && !options
                    .allowed_origins
                    .iter()
                    .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(o))
        })
    {
        return false;
    }
    origin_allowed(origin, &hosts, &options.allowed_origins)
}

pub(super) fn too_many_attempts(wait: Duration) -> Response {
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
pub(super) struct CredentialWatch {
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
    pub(super) fn new(hub: &Hub) -> Self {
        Self {
            rotations: hub.subscribe_rotations(),
            login: None,
            peer: None,
        }
    }

    /// `changes` must be subscribed before the credential was verified, so
    /// a revocation racing the check is still seen.
    pub(super) fn for_peer(
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

    pub(super) fn for_login(mut self, hub: &Hub, secret: Option<String>) -> Self {
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
    pub(super) async fn revoked(
        &mut self,
        hub: &Hub,
        recheck: &mut tokio::time::Interval,
    ) -> CloseFrame {
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

pub(super) fn login_recheck() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(LOGIN_RECHECK);
    interval.reset();
    interval
}

pub(super) async fn require_auth(
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
        Auth::View => view_only(),
        Auth::Anonymous | Auth::Denied => {
            error_response(StatusCode::UNAUTHORIZED, "authentication required")
        }
    }
}

/// A share link's credential used anywhere but its own view.
pub(super) fn view_only() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "a share link reaches only its read-only view (/share/...)",
    )
}

/// Cross-origin browser requests may not change state.
pub(super) async fn origin_guard(
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

pub(super) async fn security_headers(request: Request, next: Next) -> Response {
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

/// Whether the browser reached the hub over HTTPS: the hub's own TLS, an
/// `X-Forwarded-Proto: https` from a local proxy, or a `Host` that is one
/// of the configured `https://` origins.
fn browser_https(headers: &HeaderMap, client: Client, options: &ServerOptions) -> bool {
    if client.tls {
        return true;
    }
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
pub(super) struct IndexQuery {
    /// Why the web UI signed out (`rotated`: the token was rotated).
    signed_out: Option<String>,
}

pub(super) async fn index(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    PageQuery(query): PageQuery<IndexQuery>,
) -> Response {
    match authenticate(&app, &headers, client) {
        Auth::Granted(_) => {
            let mut response = no_store(index_page(&headers));
            // The service worker keeps only this page (never the sign-in
            // page) as the offline app shell.
            response.headers_mut().insert(
                header::HeaderName::from_static(web::SHELL_HEADER),
                HeaderValue::from_static("app"),
            );
            response
        }
        Auth::Limited(wait) => login_refused(wait),
        Auth::View => view_only(),
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
pub(super) struct LoginParam {
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

pub(super) async fn login_query(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    PageQuery(param): PageQuery<LoginParam>,
) -> Response {
    login(&app, &headers, client, param.presented())
}

pub(super) async fn login_form(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    PageForm(param): PageForm<LoginParam>,
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
pub(super) async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
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
pub(super) async fn asset(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if !web::is_public_asset(&path) {
        match authenticate(&app, &headers, client) {
            Auth::Granted(_) => {}
            Auth::Limited(wait) => return too_many_attempts(wait),
            Auth::View => return view_only(),
            Auth::Anonymous | Auth::Denied | Auth::Peer(_) => {
                return error_response(StatusCode::UNAUTHORIZED, "authentication required");
            }
        }
    }
    match web::asset_entry(&path) {
        Some(asset) => asset_response(&headers, query.as_deref(), &asset),
        None => error_response(StatusCode::NOT_FOUND, "no such asset"),
    }
}

/// Whether the request takes gzip (`Accept-Encoding`, not at q=0).
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|coding| {
            let mut parts = coding.split(';').map(str::trim);
            let name = parts.next().unwrap_or("");
            let refused = parts.any(|p| {
                p.strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
        })
}

/// The app page, gzipped when the browser takes it (it is `no-store`:
/// every load fetches it, and it names everything else by build).
fn index_page(headers: &HeaderMap) -> Response {
    let mut response = if accepts_gzip(headers) {
        let mut response = (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            web::index_html_gzip(),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        response
    } else {
        Html(web::index_html()).into_response()
    };
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    response
}

/// An embedded asset: gzipped when it compresses and the browser takes
/// it; tagged by build (`ETag`, answered 304 when it matches); and, when
/// asked for by build (`?v=<build>`, as the app page names its scripts and
/// styles), cacheable for good -- a new build names new URLs.
pub(super) fn asset_response(
    headers: &HeaderMap,
    query: Option<&str>,
    asset: &web::Asset,
) -> Response {
    let build = web::build_id();
    let gzip = asset.gzip.filter(|_| accepts_gzip(headers));
    let etag = if gzip.is_some() {
        format!("\"{build}-gz\"")
    } else {
        format!("\"{build}\"")
    };
    let pinned = query.is_some_and(|q| q.split('&').any(|p| p.strip_prefix("v=") == Some(build)));
    let cache = if pinned {
        "private, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let fresh = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag.trim_start_matches("W/") == etag
        });
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let body: &'static [u8] = gzip.unwrap_or(asset.bytes);
        let mut response = ([(header::CONTENT_TYPE, asset.content_type)], body).into_response();
        if gzip.is_some() {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        response
    };
    let out = response.headers_mut();
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        out.insert(header::ETAG, value);
    }
    if asset.gzip.is_some() {
        out.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    response
}

pub(super) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// --- credentials ------------------------------------------------------------

/// Mint a one-time login code: `{"code","expires_in"}` (seconds). Open
/// `/login?code=<code>` once within that time to get the login cookie.
pub(super) async fn mint_login_code(State(app): State<AppState>) -> ApiResult<Response> {
    let code = app.hub.mint_login_code()?;
    Ok(no_store(
        Json(json!({ "code": code, "expires_in": LOGIN_CODE_TTL.as_secs() })).into_response(),
    ))
}

/// Rotate the token: every cookie, code, and socket of the old one stops
/// working at once.
pub(super) async fn rotate_token(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.rotate_token()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
