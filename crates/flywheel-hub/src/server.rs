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
//! * Protocol 2 (`?proto=2`, `server/terminal_ws.rs`; docs/hub/api.md):
//!   output offsets (`seq`, `epoch`) and resuming from one (`since`),
//!   paced screen frames for slow viewers (`lite`, `mode`), deflated binary
//!   messages (`z`), and input ids acknowledged and never typed twice
//!   (`client`, `ack`).
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
//!   the last two minutes, not a transcript's backlog);
//!   `{"type":"replies","session_id","replies":[...]}` when one wrote
//!   replies (newest first; a `seq` already known replaces that reply). A
//!   `status` is
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

mod activity_api;
mod admission;
mod auth;
mod autopilot_api;
mod broadcast_api;
mod ca_page;
mod commands_api;
mod conversations_api;
mod deflate;
mod detect_api;
mod digest_api;
mod events_ws;
mod fanout_api;
mod files_api;
mod flywheel_api;
mod fs_api;
mod handoff_api;
mod import_api;
mod listen;
mod notify_api;
mod perms_api;
mod plugins_api;
mod pr_api;
mod promote_api;
mod proxy;
mod pwa;
mod queue_api;
mod record_api;
mod review_api;
mod search_api;
mod sessions_api;
mod settings_api;
mod share_api;
mod slots;
mod telemetry_api;
mod templates_api;
mod terminal_ws;
mod tls_listen;
mod workspaces_api;
mod worktree_api;

use auth::{
    Client, CredentialWatch, asset, index, login_cookies, login_form, login_query, login_recheck,
    logout, mint_login_code, origin_guard, origin_ok, require_auth, rotate_token, security_headers,
};
pub use events_ws::SESSIONS_PUSH_INTERVAL;
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

use crate::auth::FailureLimiter;
use crate::hub::{Hub, HubError};
use crate::state::StateError;
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
/// WebSocket close code sent to a share link's viewer when the link was
/// revoked or expired.
pub const WS_CLOSE_SHARE_ENDED: u16 = 4004;
/// WebSocket close code sent to a terminal viewer of a session that was
/// relaunched in place (another process, another output stream):
/// reconnect.
pub const WS_CLOSE_RELAUNCHED: u16 = 4005;
/// How long one WebSocket write may wait on a reader that stopped
/// reading before that socket is dropped (see
/// [`ServerOptions::ws_send_timeout`]).
pub const WS_SEND_TIMEOUT: Duration = Duration::from_secs(60);
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
    /// Scheduled digests (`GET /api/digest`, `fw hub digest`).
    pub digest: Option<Arc<crate::digest::DigestService>>,
    /// The Flywheel panel's monitor (`/api/flywheel*` and the events
    /// socket's `flywheel` messages); `None` turns the panel off.
    pub flywheel: Option<Arc<crate::flywheel::FlywheelMonitor>>,
    /// The `git` and `gh` the review endpoints run.
    pub review_tools: crate::review::Tools,
    /// Verified fan-outs (`/api/fanouts*`, the events socket's `fanout`
    /// messages); `None` turns them off.
    pub fanouts: Option<Arc<crate::fanout::Fanouts>>,
    /// `hub.toml` as the running hub reads it (`GET /api/config`); `None`
    /// answers from the file and the defaults.
    pub config: Option<Arc<crate::config::ConfigWatch>>,
    /// The plugin host (`/api/plugins*`); `None`: no plugins run.
    pub plugins: Option<Arc<crate::plugins::PluginHost>>,
    /// Built-in TLS (`--tls`): https on the same port, `Secure` cookies,
    /// the `/ca` onboarding page, and plain http from the network
    /// redirected to https. `None`: plain http only.
    pub tls: Option<Arc<crate::tls::TlsServing>>,
    /// How long one WebSocket write (terminal, share link, events, the
    /// machine proxy in both directions) may wait on a reader that stopped
    /// reading before that socket is dropped and what it holds freed: a
    /// stalled reader never holds a socket, nor what would close it (a
    /// revoked credential, shutdown, a handoff), forever. `None`:
    /// [`WS_SEND_TIMEOUT`].
    pub ws_send_timeout: Option<Duration>,
    /// The registered machine this hub forwards its session telemetry to
    /// (`[telemetry] upstream`); `None`: it keeps its events to itself.
    pub telemetry_upstream: Option<String>,
}

impl ServerOptions {
    /// [`Self::ws_send_timeout`], or its default.
    pub fn send_timeout(&self) -> Duration {
        self.ws_send_timeout.unwrap_or(WS_SEND_TIMEOUT)
    }
}

/// Await one WebSocket write within `limit` (the one bounded-send policy
/// of every socket the hub writes to); false when it failed or its reader
/// stalled past `limit` (the caller drops the socket).
pub(crate) async fn write_within<E>(
    limit: Duration,
    write: impl Future<Output = Result<(), E>>,
) -> bool {
    matches!(tokio::time::timeout(limit, write).await, Ok(Ok(())))
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
    /// The UI's preferences and keymap (`settings.json`).
    settings: Arc<crate::settings::SettingsStore>,
    /// The folder picker's last discovery scan (`/api/fs/suggestions`).
    repo_scan: Arc<crate::fsbrowse::RepoScanCache>,
}

/// Build the router. Flipping `shutdown` to `true` closes every open
/// WebSocket so a graceful shutdown can finish.
pub fn router(hub: Arc<Hub>, options: ServerOptions, shutdown: watch::Receiver<bool>) -> Router {
    let peers = Arc::new(crate::peers::PeerStore::load(hub.state_dir()));
    let machines = Arc::new(crate::machines::Machines::load(hub.state_dir()));
    let settings = Arc::new(crate::settings::SettingsStore::load(hub.state_dir()));
    let state = AppState {
        settings,
        hub,
        options: Arc::new(options),
        shutdown,
        failures: Arc::new(Mutex::new(FailureLimiter::default())),
        peers,
        machines,
        repo_scan: Arc::default(),
    };
    if let Some(upstream) = state.options.telemetry_upstream.clone() {
        tokio::spawn(crate::telemetry::forward::run(
            Arc::clone(state.hub.telemetry()),
            Arc::clone(&state.machines),
            upstream,
            state.shutdown.clone(),
        ));
    }
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
        .route(
            "/api/workspaces/{id}/conversations",
            get(conversations_api::list),
        )
        .route(
            "/api/claude-accounts",
            get(conversations_api::accounts).post(conversations_api::add_account),
        )
        .route(
            "/api/claude-accounts/{name}",
            axum::routing::delete(conversations_api::remove_account),
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
        // The folder picker: directories on this machine (names only).
        .route("/api/fs/dirs", get(fs_api::dirs))
        .route("/api/fs/suggestions", get(fs_api::suggestions))
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
        .route(
            "/api/sessions/{id}/replies",
            get(commands_api::list_replies),
        )
        .route("/api/sessions/{id}/paste", post(commands_api::paste))
        .route("/api/sessions/{id}/file", get(files_api::read_file))
        .route(
            "/api/sessions/{id}/files/check",
            post(files_api::check_files),
        )
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
        .route("/api/sessions/{id}/pr", get(pr_api::view))
        .route("/api/sessions/{id}/pr/log", get(pr_api::log))
        .route("/api/sessions/{id}/pr/fix", post(pr_api::fix))
        .route("/api/sessions/{id}/pr/rerun", post(pr_api::rerun))
        .route("/api/sessions/{id}/pr/ready", post(pr_api::ready))
        .route("/api/sessions/{id}/pr/merge", post(pr_api::merge))
        .route(
            "/api/sessions/{id}/review/reviewer",
            post(handoff_api::reviewer),
        )
        .route(
            "/api/sessions/{id}/review/findings",
            get(handoff_api::findings),
        )
        .route(
            "/api/sessions/{id}/review/findings/send",
            post(handoff_api::send_findings),
        )
        .route(
            "/api/sessions/{id}/handoff/compose",
            post(handoff_api::compose),
        )
        .route("/api/sessions/{id}/handoff", post(handoff_api::deliver))
        .route("/api/broadcast", post(broadcast_api::broadcast))
        .route("/api/queues", get(queue_api::list))
        .route(
            "/api/sessions/{id}/queue",
            get(queue_api::get_queue).post(queue_api::add),
        )
        .route("/api/sessions/{id}/queue/clear", post(queue_api::clear))
        .route("/api/sessions/{id}/queue/pause", post(queue_api::pause))
        .route("/api/sessions/{id}/queue/resume", post(queue_api::resume))
        .route(
            "/api/sessions/{id}/queue/items/{item}",
            axum::routing::patch(queue_api::edit).delete(queue_api::remove),
        )
        .route(
            "/api/sessions/{id}/queue/items/{item}/move",
            post(queue_api::move_item),
        )
        .route(
            "/api/sessions/{id}/queue/items/{item}/editing",
            post(queue_api::editing),
        )
        .route("/api/sessions/{id}/ws", get(session_ws))
        .route("/api/search", get(search_api::search))
        .route("/api/activity", get(activity_api::list))
        .route("/api/activity/digest", get(activity_api::digest))
        .route("/api/activity/seen", post(activity_api::seen))
        .route("/api/digest", get(digest_api::view))
        .route("/api/digest/preview", get(digest_api::preview))
        .route("/api/digest/send", post(digest_api::send))
        .route(
            "/api/digest/config",
            axum::routing::put(digest_api::put_config),
        )
        .route("/api/digests/{id}", get(digest_api::get_one))
        .route("/api/sessions/{id}/transcript", get(search_api::transcript))
        .route("/api/login-codes", post(mint_login_code))
        .route("/api/token/rotate", post(rotate_token))
        .route("/api/events", get(events_ws))
        .route("/api/notify", get(notify_api::status))
        .route("/api/notify/test", post(notify_api::test))
        .route(
            "/api/settings",
            get(settings_api::get_settings).put(settings_api::put_settings),
        )
        .route("/api/settings/keys/check", post(settings_api::check_keys))
        .route("/api/config", get(settings_api::get_config))
        .route("/api/tls", get(ca_page::status))
        .route("/api/tls/reload", post(ca_page::reload))
        .route("/api/perms", get(perms_api::view))
        .route("/api/perms/suggest", get(perms_api::suggest))
        .route("/api/perms/preview", post(perms_api::preview))
        .route("/api/perms/allow", post(perms_api::allow))
        .route("/api/plugins", get(plugins_api::list))
        .route(
            "/api/plugins/{name}",
            axum::routing::put(plugins_api::toggle),
        )
        .route("/api/plugins/{name}/logs", get(plugins_api::logs))
        .route(
            "/api/plugins/{name}/actions/{action}",
            post(plugins_api::run_action),
        )
        .route("/api/flywheel", get(flywheel_api::list))
        .route("/api/flywheel/panel", get(flywheel_api::panel))
        .route("/api/flywheel/run", get(flywheel_api::run_output))
        .route("/api/flywheel/history", get(flywheel_api::task_history))
        .route("/api/flywheel/worker", post(flywheel_api::start_worker))
        .route("/api/flywheel/reland", post(flywheel_api::start_reland))
        .route(
            "/api/flywheel/pauses/clear",
            post(flywheel_api::clear_pauses),
        )
        .route("/api/flywheel/steer/say", post(flywheel_api::say))
        .route(
            "/api/flywheel/steer/interrupt",
            post(flywheel_api::interrupt),
        )
        .route("/api/flywheel/autopilot", get(autopilot_api::view))
        .route("/api/flywheel/autopilot/start", post(autopilot_api::start))
        .route("/api/flywheel/autopilot/stop", post(autopilot_api::stop))
        .route("/api/flywheel/autopilot/pause", post(autopilot_api::pause))
        .route(
            "/api/flywheel/autopilot/resume",
            post(autopilot_api::resume),
        )
        .route(
            "/api/flywheel/autopilot/decide",
            post(autopilot_api::decide),
        )
        .route(
            "/api/fanouts",
            get(fanout_api::list).post(fanout_api::create),
        )
        .route(
            "/api/fanouts/{id}",
            get(fanout_api::get_one).delete(fanout_api::remove),
        )
        .route("/api/fanouts/{id}/grade", post(fanout_api::grade))
        .route("/api/fanouts/{id}/fix", post(fanout_api::fix))
        .route("/api/fanouts/{id}/pick", post(fanout_api::pick))
        .route("/api/fanouts/{id}/rebase", post(fanout_api::rebase))
        .route("/api/fanouts/{id}/cleanup", post(fanout_api::cleanup))
        .route("/api/fanouts/{id}/seen", post(fanout_api::seen))
        .route("/api/fanouts/{id}/promote", post(promote_api::promote))
        .route("/api/flywheel/task", get(promote_api::task_seed))
        .route("/api/flywheel/tasks", post(promote_api::create_task))
        // Read-only share links and session recordings.
        .route(
            "/api/shares",
            get(share_api::list)
                .post(share_api::create)
                .delete(share_api::revoke_all),
        )
        .route("/api/shares/{which}", delete(share_api::revoke))
        .route("/api/recordings", get(record_api::list))
        .route(
            "/api/recordings/{name}",
            get(record_api::download).delete(record_api::delete),
        )
        .route("/api/sessions/{id}/record/start", post(record_api::start))
        .route("/api/sessions/{id}/record/stop", post(record_api::stop))
        // Multiple machines: this hub's peers (remote side) and machines
        // (local side), and the proxy to them.
        .route("/api/telemetry/fleet", get(telemetry_api::fleet))
        .route("/api/telemetry/sessions/{id}", get(telemetry_api::session))
        .route(
            "/api/telemetry/sessions/{id}/events",
            get(telemetry_api::events),
        )
        .route(
            "/api/telemetry/batch",
            post(telemetry_api::batch).layer(axum::extract::DefaultBodyLimit::max(
                telemetry_api::BATCH_WIRE_MAX,
            )),
        )
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
        .route("/replay", get(record_api::player))
        // Built-in TLS onboarding: the local CA for a device to trust.
        .route("/ca", get(ca_page::page))
        .route("/ca/", get(ca_page::page))
        .route("/ca/hub-ca.crt", get(ca_page::der))
        .route("/ca/hub-ca.pem", get(ca_page::pem))
        .route("/ca/hub-ca.mobileconfig", get(ca_page::mobileconfig))
        .route("/ca/probe", get(ca_page::probe))
        .route("/ca/ca.js", get(ca_page::script))
        .route("/ca/ca.css", get(ca_page::style))
        // A share link: its credential in the path is its only key.
        .route("/share/{credential}", get(share_api::viewer_page))
        .route("/share/{credential}/info", get(share_api::info))
        .route("/share/{credential}/ws", get(share_api::viewer_ws))
        .route(
            "/share/{credential}/assets/{*path}",
            get(share_api::viewer_asset),
        )
        .route("/share/{credential}/{*rest}", any(share_api::elsewhere))
        .route_layer(middleware::from_fn_with_state(
            BodyBound::PUBLIC,
            bound_body,
        ));
    // What agent sessions send (hooks, the progress tool): each request
    // carries its session's report secret, checked by the handler. No
    // browser is involved, so these sit outside the origin guard; the
    // body is read whole within a bound, as for the public routes.
    let agents = Router::new()
        .route("/api/telemetry/hook", post(telemetry_api::hook))
        .route("/api/telemetry/mcp", post(telemetry_api::progress_tool))
        .route_layer(middleware::from_fn_with_state(
            telemetry_api::AGENT_BODY,
            bound_body,
        ));
    public
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .merge(agents)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            tls_listen::plain_http_policy,
        ))
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
    let tls = options
        .tls
        .as_ref()
        .map(|t| tls_listen::TlsAccept::new(t.acceptor(), t.ca_epoch()));
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
        tls,
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

// --- connections ------------------------------------------------------------

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// A connection's two ends, recorded per connection by [`serve`].
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
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

/// `Query` whose rejections (a number that is not one, a missing or
/// repeated parameter) answer in the API's `{"error": ...}` form, worded
/// for the caller rather than in serde's terms.
struct ApiQuery<T>(T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Response> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(error_response(
                rejection.status(),
                &bounded_message(&query_message(&rejection_message(&rejection.body_text()))),
            )),
        }
    }
}

/// `Query` for the pages (`/`, `/login`): a query string serde cannot
/// read answers like [`ApiQuery`]'s, as `{"error": ...}` to a client that
/// asks for JSON and as a page to a browser, never axum's plain text.
struct PageQuery<T>(T);

impl<S, T> axum::extract::FromRequestParts<S> for PageQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Response> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(page_refusal(
                &parts.headers,
                rejection.status(),
                "That link is not valid",
                &query_message(&rejection_message(&rejection.body_text())),
            )),
        }
    }
}

/// `Form` for the pages (`POST /login`): a body serde cannot read (a
/// field given twice) or one that is not a form answers like
/// [`PageQuery`]'s, never axum's plain text.
struct PageForm<T>(T);

impl<S, T> FromRequest<S> for PageForm<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Response> {
        let headers = request.headers().clone();
        match axum::Form::<T>::from_request(request, state).await {
            Ok(axum::Form(value)) => Ok(Self(value)),
            Err(rejection) => Err(page_refusal(
                &headers,
                rejection.status(),
                "That sign-in was not valid",
                &query_message(&rejection_message(&rejection.body_text())),
            )),
        }
    }
}

/// A page route's refusal: `{"error": message}` to a client that asks for
/// JSON, a notice page (with a link back to the hub) to anything else;
/// never cached.
fn page_refusal(headers: &HeaderMap, status: StatusCode, heading: &str, message: &str) -> Response {
    let message = bounded_message(message);
    let mut response = if wants_json(headers) {
        error_response(status, &message)
    } else {
        let mut page =
            axum::response::Html(crate::web::notice_page(heading, &message, "Open the hub"))
                .into_response();
        *page.status_mut() = status;
        page
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Whether a request asks for JSON rather than a page: its `Accept` names
/// `application/json` (or `+json`) and not `text/html`.
fn wants_json(headers: &HeaderMap) -> bool {
    let accept = header_str(headers, header::ACCEPT)
        .unwrap_or_default()
        .to_ascii_lowercase();
    (accept.contains("application/json") || accept.contains("+json"))
        && !accept.contains("text/html")
}

/// A boolean query flag: absent or `false`/`0` is false; `true`/`1`/empty
/// (`?all`) is true; anything else is an error naming `name`.
fn query_flag(name: &str, raw: Option<&str>) -> Result<bool, String> {
    match raw {
        None | Some("false" | "0") => Ok(false),
        Some("true" | "1" | "") => Ok(true),
        Some(other) => Err(format!("{name} must be true or false, not {other:?}")),
    }
}

/// serde's query-string errors in the caller's words: `limit: invalid
/// digit found in string` -> `limit must be a number`, ``missing field
/// `folder` `` -> `folder is required`, ``duplicate field `x` `` -> `x is
/// given twice`.
fn query_message(text: &str) -> String {
    let quoted = |rest: &str| {
        rest.strip_prefix('`')
            .and_then(|r| r.split_once('`'))
            .map(|(name, _)| name.to_owned())
    };
    if let Some(name) = text.strip_prefix("missing field ").and_then(quoted) {
        return format!("{name} is required");
    }
    if let Some(name) = text.strip_prefix("duplicate field ").and_then(quoted) {
        return format!("{name} is given twice");
    }
    if let Some((field, reason)) = text.split_once(": ")
        && !field.is_empty()
        && field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        let number = [
            "invalid digit found in string",
            "cannot parse integer from empty string",
            "number too large to fit in target type",
            "number too small to fit in target type",
            "invalid float literal",
        ];
        if number.contains(&reason) {
            return format!("{field} must be a number");
        }
        if reason.starts_with("provided string was not `true` or `false`") {
            return format!("{field} must be true or false");
        }
        return format!("{field}: {reason}");
    }
    text.to_owned()
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

/// The login URL for a one-time `code` under `base`.
pub fn code_login_url(base: &str, code: &str) -> String {
    format!("{}/login?code={code}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use std::time::Instant;

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
            None,
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

    /// Share-link connections never take main slots: a link's holder
    /// filling every slot it can still leaves the operator's requests
    /// served.
    #[tokio::test]
    async fn share_link_connections_cannot_lock_the_operator_out() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(crate::HubOptions::new(dir.path().join("state"))).unwrap();
        let token = hub.token().expose().to_owned();
        let (_, credential) = hub
            .shares()
            .issue("s1", crate::share::DEFAULT_TTL, None)
            .unwrap();
        let (closing, rx) = watch::channel(false);
        let app = router(Arc::clone(&hub), ServerOptions::default(), rx);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = crate::handoff::GatedListener::new(listener, crate::handoff::Gate::new());
        let cap = 4;
        tokio::spawn(listen::run(
            listener,
            app,
            cap,
            Vec::new(),
            None,
            std::future::pending(),
            closing,
        ));
        // Read one response's status line and head (no body framing
        // needed: the line is all a test asserts).
        async fn status(stream: &mut tokio::net::TcpStream) -> String {
            let mut buf = vec![0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("answered in time")
                .unwrap();
            String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        }
        // Keep-alive share-link connections, many more than main slots.
        let viewer_cap = slots::unauthenticated_cap(cap);
        let mut held = Vec::new();
        let mut refused = 0;
        for _ in 0..viewer_cap + 4 {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let head = format!("GET /share/{credential}/info HTTP/1.1\r\nHost: x\r\n\r\n");
            stream.write_all(head.as_bytes()).await.unwrap();
            let line = status(&mut stream).await;
            if line.contains(" 200 ") {
                held.push(stream);
            } else {
                assert!(line.contains(" 429 "), "{line}");
                refused += 1;
            }
        }
        assert_eq!(held.len(), viewer_cap);
        assert_eq!(refused, 4);
        // The operator is still served, at once.
        for _ in 0..cap + 1 {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let head = format!(
                "GET /api/shares HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            let line = status(&mut stream).await;
            assert!(line.contains(" 200 "), "{line}");
        }
        // A share-link slot freed is a slot back.
        drop(held.pop());
        let mut stream = loop {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let head = format!("GET /share/{credential}/info HTTP/1.1\r\nHost: x\r\n\r\n");
            stream.write_all(head.as_bytes()).await.unwrap();
            if status(&mut stream).await.contains(" 200 ") {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _ = stream.shutdown().await;
    }
}
