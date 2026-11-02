//! The hub's HTTP surface: login, the embedded web app, the REST API, and
//! one WebSocket per session.
//!
//! WebSocket protocol (`GET /api/sessions/{id}/ws`):
//!
//! * server -> client, **binary**: raw PTY output. The first frames after
//!   connect are the session's retained scrollback, then live output.
//! * server -> client, **text** JSON: `{"type":"exit","code":N|null,
//!   "signal":S|null}` once the process ends; `{"type":"reset"}` when the
//!   client fell too far behind -- clear the terminal, a fresh replay
//!   follows.
//! * client -> server, **text** JSON: `{"type":"input","data":"..."}` and
//!   `{"type":"resize","cols":N,"rows":N}`; **binary** frames are raw input.

use crate::auth::{
    self, COOKIE_NAME, bearer_value, cookie_value, login_cookie, logout_cookie, origin_allowed,
};
use crate::hub::{CreateSession, Hub, HubError};
use crate::pty::{Attachment, ExitInfo, PtySession, SessionEvent, WriteOutcome};
use crate::state::{StateError, WorkspaceInput, validate_repo};
use crate::web;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Form, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::SinkExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

/// How often the server pings an idle WebSocket (keeps proxies from
/// reaping it and detects dead peers).
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Largest client message accepted (input is typed or pasted text).
const WS_MAX_MESSAGE: usize = 1024 * 1024;
/// How long [`serve`] waits for sessions to exit on shutdown.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// Server options.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Extra browser origins allowed on WebSocket and state-changing
    /// requests (e.g. a reverse proxy's public `https://host`), on top of
    /// same-origin.
    pub allowed_origins: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    options: Arc<ServerOptions>,
    shutdown: watch::Receiver<bool>,
}

/// Build the router. Flipping `shutdown` to `true` closes every open
/// WebSocket so a graceful shutdown can finish.
pub fn router(hub: Arc<Hub>, options: ServerOptions, shutdown: watch::Receiver<bool>) -> Router {
    let state = AppState {
        hub,
        options: Arc::new(options),
        shutdown,
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
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session)
                .patch(update_session)
                .delete(remove_session),
        )
        .route("/api/kinds", get(list_kinds))
        .route("/api/repos/check", get(check_repo))
        .route("/api/sessions/{id}/kill", post(kill_session))
        .route("/api/sessions/{id}/restart", post(restart_session))
        .route("/api/sessions/{id}/ws", get(session_ws))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_query).post(login_form))
        .route("/logout", post(logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(asset))
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

fn is_authenticated(headers: &HeaderMap, hub: &Hub) -> bool {
    let token = hub.token();
    let by_cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| cookie_value(v, COOKIE_NAME))
        .any(|v| token.matches(v));
    by_cookie
        || header_str(headers, header::AUTHORIZATION)
            .and_then(bearer_value)
            .is_some_and(|v| token.matches(v))
}

fn origin_ok(headers: &HeaderMap, options: &ServerOptions) -> bool {
    origin_allowed(
        header_str(headers, header::ORIGIN),
        header_str(headers, header::HOST),
        &options.allowed_origins,
    )
}

async fn require_auth(State(app): State<AppState>, request: Request, next: Next) -> Response {
    if is_authenticated(request.headers(), &app.hub) {
        next.run(request).await
    } else {
        error_response(StatusCode::UNAUTHORIZED, "authentication required")
    }
}

/// Cross-origin browser requests may not change state.
async fn origin_guard(State(app): State<AppState>, request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe && !origin_ok(request.headers(), &app.options) {
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

fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

// --- pages ------------------------------------------------------------------

async fn index(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if is_authenticated(&headers, &app.hub) {
        no_store(Html(web::INDEX_HTML).into_response())
    } else {
        no_store(Html(web::login_page(None)).into_response())
    }
}

#[derive(Deserialize)]
struct TokenParam {
    token: Option<String>,
}

async fn login_query(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(param): Query<TokenParam>,
) -> Response {
    login(&app, &headers, param.token.as_deref())
}

async fn login_form(
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(param): Form<TokenParam>,
) -> Response {
    login(&app, &headers, param.token.as_deref())
}

/// Trade the token for the login cookie. Success answers with a page that
/// navigates to `/` from this origin (a same-site navigation, so the
/// `SameSite=Strict` cookie is sent even when the login link was opened
/// from another app) and drops the token from the address bar.
fn login(app: &AppState, headers: &HeaderMap, presented: Option<&str>) -> Response {
    let Some(presented) = presented.map(str::trim).filter(|t| !t.is_empty()) else {
        return no_store(Html(web::login_page(None)).into_response());
    };
    if !app.hub.token().matches(presented) {
        let mut response = Html(web::login_page(Some("That token is not valid."))).into_response();
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return no_store(response);
    }
    let cookie = login_cookie(app.hub.token(), forwarded_https(headers));
    let mut response = Html(web::LOGGED_IN_HTML).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    no_store(response)
}

async fn logout() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&logout_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// Embedded assets. The app's code needs the token; the fonts and icon the
/// login page also uses are public.
async fn asset(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    if !web::is_public_asset(&path) && !is_authenticated(&headers, &app.hub) {
        return error_response(StatusCode::UNAUTHORIZED, "authentication required");
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
            Err(rejection) => Err(error_response(rejection.status(), &rejection.body_text())),
        }
    }
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

async fn list_workspaces(State(app): State<AppState>) -> Response {
    Json(json!({ "workspaces": app.hub.workspaces() })).into_response()
}

async fn get_workspace(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(app.hub.workspace(&id)?).into_response())
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

/// Validate one candidate repo path for the new-workspace form: always 200,
/// `{"ok":true,"path":canonical}` or `{"ok":false,"error":...}` (a missing
/// or blank `path` included).
async fn check_repo(Query(query): Query<RepoQuery>) -> Response {
    let Some(path) = query.path.filter(|p| !p.trim().is_empty()) else {
        return Json(json!({ "ok": false, "error": "path is required" })).into_response();
    };
    let raw = std::path::PathBuf::from(path.trim());
    let result = tokio::task::spawn_blocking(move || validate_repo(&raw)).await;
    match result {
        Ok(Ok(path)) => Json(json!({ "ok": true, "path": path })).into_response(),
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

async fn remove_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.remove_session(&id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- websocket --------------------------------------------------------------

async fn session_ws(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&headers, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let Some(session) = app.hub.live_session(&id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("session {id} has no terminal on this server"),
        );
    };
    let shutdown = app.shutdown.clone();
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| pump(socket, session, shutdown))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

fn exit_message(info: &ExitInfo) -> Message {
    Message::Text(
        json!({ "type": "exit", "code": info.code, "signal": info.signal })
            .to_string()
            .into(),
    )
}

/// Send an attachment's replay (and exit, if already exited).
async fn send_attachment(socket: &mut WebSocket, attachment: &Attachment) -> bool {
    if !attachment.replay.is_empty()
        && socket
            .send(Message::Binary(attachment.replay.clone().into()))
            .await
            .is_err()
    {
        return false;
    }
    if let Some(info) = &attachment.exit {
        return socket.send(exit_message(info)).await.is_ok();
    }
    true
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
) {
    let mut attachment = session.attach();
    if !send_attachment(&mut socket, &attachment).await {
        return;
    }
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    loop {
        tokio::select! {
            event = attachment.events.recv() => {
                let sent = match event {
                    Ok(SessionEvent::Output(bytes)) => socket.send(Message::Binary(bytes)).await.is_ok(),
                    Ok(SessionEvent::Exit(info)) => socket.send(exit_message(&info)).await.is_ok(),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        attachment = session.attach();
                        let reset = Message::Text(json!({ "type": "reset" }).to_string().into());
                        socket.send(reset).await.is_ok()
                            && send_attachment(&mut socket, &attachment).await
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
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }
}

/// The login URL printed by `fw hub serve`.
pub fn login_url(base: &str, token: &auth::Token) -> String {
    format!(
        "{}/login?token={}",
        base.trim_end_matches('/'),
        token.expose()
    )
}
