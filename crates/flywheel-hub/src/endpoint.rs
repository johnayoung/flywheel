//! The one hub client: every `fw hub` verb that talks to a running hub
//! (`ls`, `new`, `send`, `read`, `kill`, `relaunch`, `import`, `commands`,
//! `review`, `merge`,
//! `open`, `url`, `token --rotate`, `status`, `wait`, `attach`) resolves
//! it here and speaks to it through [`Endpoint`].
//!
//! A hub is either the one running for this machine's state dir (address
//! from its runtime file, token from its token file), or, for the verbs
//! that take [`EndpointArgs`], any hub by `--url` and token (a remote one
//! behind `tailscale serve`, over https). The state dir's token is never
//! sent to a `--url` host unless that URL is the state dir's own running
//! hub; any other needs `--token` / FW_HUB_TOKEN.
//!
//! Speaks the same API the browser does: bearer-authenticated JSON over
//! HTTP and the `/api/sessions/{id}/ws` and `/api/events` WebSockets. No
//! `Origin` is sent: the hub's cross-origin checks are for browsers. The
//! transport is async; the scripting verbs use its `blocking_*` forms.

use crate::auth::Token;
use crate::cli::{StateDirArg, resolve_state_dir, running_hub};
use crate::paths::default_state_dir;
use crate::remote::RuntimeInfo;
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;
use url::Url;

/// How long connecting (TCP, TLS, WebSocket handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one REST call may take by default (session spawns included);
/// [`Endpoint::with_timeout`] changes it.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest WebSocket message accepted (the hub sends at most 256 KiB).
const WS_MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// The environment variable holding a hub's token (for `--url`).
pub const TOKEN_ENV: &str = "FW_HUB_TOKEN";

/// Which hub to talk to.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct EndpointArgs {
    /// A hub elsewhere: its base URL (`https://box.tailnet.ts.net`,
    /// `http://127.0.0.1:7420`). Default: the hub running for --state-dir.
    #[arg(long, value_name = "URL")]
    pub(crate) url: Option<String>,
    /// The hub's access token (`fw hub token` on its machine). Prefer the
    /// FW_HUB_TOKEN environment variable: argv is visible to other users.
    /// Default: FW_HUB_TOKEN, else the state dir's token file (with --url:
    /// only when it names the hub running for that state dir).
    #[arg(long, value_name = "TOKEN")]
    pub(crate) token: Option<String>,
    /// A machine registered on that hub (`fw hub machine ls`): reach its
    /// sessions through the hub's proxy, with the peer credential the hub
    /// keeps for it.
    #[arg(long, value_name = "NAME")]
    pub(crate) machine: Option<String>,
    #[command(flatten)]
    pub(crate) state: StateDirArg,
}

pub use crate::net::{Io, Socket};

/// A resolved hub: base URL and credential.
pub struct Endpoint {
    base: Url,
    /// `/m/<machine>` when a machine is reached through this hub's proxy,
    /// else empty.
    prefix: String,
    token: RwLock<String>,
    /// The state dir the token was read from (re-read after a rotation);
    /// `None` for a token given by hand.
    token_dir: Option<PathBuf>,
    /// The hub running for this machine's state dir, when that is the one
    /// reached.
    local: Option<RuntimeInfo>,
    /// Per-call limit of REST calls.
    timeout: Duration,
    tls: Option<Arc<rustls::ClientConfig>>,
    http: reqwest::Client,
    /// Drives the `blocking_*` calls (built on first use).
    blocking: OnceLock<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Endpoint")
            .field("base", &self.base.as_str())
            .field("local", &self.local.as_ref().map(|info| info.pid))
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Resolve `args`: `--url` (with `--token` or FW_HUB_TOKEN; the state
    /// dir's token only when the URL is that state dir's running hub), else
    /// the hub running for the state dir (`--token` / FW_HUB_TOKEN, else
    /// its token file).
    pub(crate) fn resolve(args: &EndpointArgs) -> Result<Self, String> {
        let endpoint = Self::resolve_hub(args)?;
        match &args.machine {
            Some(machine) => endpoint.via_machine(machine),
            None => Ok(endpoint),
        }
    }

    /// The same hub, its calls going to machine `name` through its proxy.
    pub fn via_machine(mut self, name: &str) -> Result<Self, String> {
        let name = crate::machines::validate_machine_name(name).map_err(|e| e.to_string())?;
        self.prefix = format!("/m/{name}");
        Ok(self)
    }

    /// The machine reached through the proxy, if any.
    pub fn machine(&self) -> Option<&str> {
        self.prefix.strip_prefix("/m/")
    }

    fn resolve_hub(args: &EndpointArgs) -> Result<Self, String> {
        let explicit = args.token.clone().or_else(|| {
            std::env::var(TOKEN_ENV)
                .ok()
                .filter(|t| !t.trim().is_empty())
        });
        let Some(raw) = &args.url else {
            let dir = resolve_state_dir(&args.state)?;
            return Self::for_state_dir(&dir, explicit)?.ok_or_else(|| no_hub_message(&dir));
        };
        let base = parse_base(raw)?;
        if let Some(token) = explicit {
            return Self::new(base, token, None, None);
        }
        // Never hand this machine's credential to another host: the local
        // token only goes to the local hub.
        let dir = resolve_state_dir(&args.state)?;
        let Some(info) = running_hub(&dir)?.filter(|info| is_local_hub(&base, info.listen)) else {
            return Err(format!(
                "--url needs the hub's token: set {TOKEN_ENV} (or --token) to the output of \
                 `fw hub token` on its machine (the local token is only sent to the hub \
                 running for {})",
                dir.display()
            ));
        };
        let token = local_token(&dir)?;
        Self::new(base, token, Some(dir), Some(info))
    }

    /// The hub running for `state_dir` with its own token, or why there is
    /// none (how to start one).
    pub fn local(state_dir: &Path) -> Result<Self, String> {
        Self::running(state_dir)?.ok_or_else(|| no_hub_message(state_dir))
    }

    /// The hub running for `state_dir` with its own token, or `None` when
    /// none runs.
    pub fn running(state_dir: &Path) -> Result<Option<Self>, String> {
        Self::for_state_dir(state_dir, None)
    }

    /// The hub running for `state_dir`, with `token` or the state dir's.
    fn for_state_dir(state_dir: &Path, token: Option<String>) -> Result<Option<Self>, String> {
        let Some(info) = running_hub(state_dir)? else {
            return Ok(None);
        };
        let base = Url::parse(&format!("http://{}", info.connect_addr()))
            .map_err(|e| format!("bad hub address: {e}"))?;
        let endpoint = match token {
            Some(token) => Self::new(base, token, None, Some(info))?,
            None => Self::new(
                base,
                local_token(state_dir)?,
                Some(state_dir.to_owned()),
                Some(info),
            )?,
        };
        Ok(Some(endpoint))
    }

    /// The same hub, each REST call waited on for at most `timeout` (a
    /// probe of a hub that may be wedged).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The hub running for this machine's state dir, when that is the one
    /// reached (its pid, listen address, public URLs).
    pub fn local_info(&self) -> Option<&RuntimeInfo> {
        self.local.as_ref()
    }

    /// Re-read a token that came from the state dir (the hub rotated it):
    /// `None` when it was given by hand (or the file is unreadable), else
    /// whether it changed. Several connections may race to re-read it;
    /// each then uses the file's current token.
    pub fn reload_token(&self) -> Option<bool> {
        let dir = self.token_dir.as_ref()?;
        let fresh = Token::load(dir).ok()??.expose().trim().to_owned();
        if fresh.is_empty() {
            return None;
        }
        let mut token = self.token.write().unwrap_or_else(PoisonError::into_inner);
        if *token == fresh {
            return Some(false);
        }
        *token = fresh;
        Some(true)
    }

    fn token(&self) -> String {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn new(
        base: Url,
        token: String,
        token_dir: Option<PathBuf>,
        local: Option<RuntimeInfo>,
    ) -> Result<Self, String> {
        let token = token.trim().to_owned();
        if token.is_empty() || token.bytes().any(|b| !b.is_ascii_graphic()) {
            return Err("the token must be printable ASCII without spaces".into());
        }
        let https = base.scheme() == "https";
        let tls = https.then(tls_config).transpose()?.map(Arc::new);
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("flywheel-hub/", env!("CARGO_PKG_VERSION")));
        builder = match &tls {
            Some(config) => builder.tls_backend_preconfigured(config.as_ref().clone()),
            None => builder.tls_backend_preconfigured(crate::net::plain_config()?),
        };
        let http = builder
            .build()
            .map_err(|e| format!("HTTP client setup failed: {e}"))?;
        Ok(Self {
            base,
            prefix: String::new(),
            token: RwLock::new(token),
            token_dir,
            local,
            timeout: CALL_TIMEOUT,
            tls,
            http,
            blocking: OnceLock::new(),
        })
    }

    /// The hub's base URL (for messages; carries no credential).
    pub fn base(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        let full = if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}{path}", self.prefix)
        };
        self.base
            .join(&full)
            .map_err(|e| format!("bad API path {path:?}: {e}"))
    }

    /// Why the hub could not be reached: `why`, naming the hub.
    fn unreachable(&self, why: &str) -> String {
        if let Some(machine) = self.machine() {
            return format!(
                "cannot reach machine {machine} through the hub at {}: {why}",
                self.base()
            );
        }
        match &self.local {
            Some(info) => format!(
                "cannot reach the running hub (pid {}) at {}: {why}",
                info.pid,
                info.connect_addr()
            ),
            None => format!("cannot reach the hub at {}: {why}", self.base()),
        }
    }

    /// `method path` with an optional JSON body: the parsed JSON response
    /// (`null` when empty) on 2xx, else the API's error.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("bad HTTP method {method:?}"))?;
        let mut request = self
            .http
            .request(method, self.url(path)?)
            .timeout(self.timeout)
            .bearer_auth(self.token());
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|e| self.unreachable(&crate::net::describe(&e)))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading the hub's answer: {}", crate::net::describe(&e)))?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &crate::net::error_text(&body)));
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body)
            .map_err(|e| format!("the hub sent invalid JSON for {path}: {e}"))
    }

    /// `GET path`: the answer's raw bytes on 2xx (a download), else the
    /// API's error.
    pub async fn download(&self, path: &str) -> Result<Vec<u8>, String> {
        let response = self
            .http
            .get(self.url(path)?)
            .timeout(self.timeout)
            .bearer_auth(self.token())
            .send()
            .await
            .map_err(|e| self.unreachable(&crate::net::describe(&e)))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading the hub's answer: {}", crate::net::describe(&e)))?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &crate::net::error_text(&body)));
        }
        Ok(body.to_vec())
    }

    /// [`Endpoint::download`], blocking.
    pub fn blocking_download(&self, path: &str) -> Result<Vec<u8>, String> {
        self.block_on(self.download(path))?
    }

    /// `GET /api/{what}` -> its `what` array.
    async fn list(&self, what: &str) -> Result<Vec<Value>, String> {
        let mut value = self.request("GET", &format!("/api/{what}"), None).await?;
        match value.get_mut(what).map(Value::take) {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(format!("the hub sent no {what} list")),
        }
    }

    /// Every session record (`GET /api/sessions`).
    pub async fn sessions(&self) -> Result<Vec<Value>, String> {
        self.list("sessions").await
    }

    /// Every workspace (`GET /api/workspaces`).
    pub async fn workspaces(&self) -> Result<Vec<Value>, String> {
        self.list("workspaces").await
    }

    /// Run `future` to completion on this endpoint's own runtime (never
    /// from inside an async context). Every call on one endpoint must use
    /// this runtime: its HTTP client pools connections that are bound to
    /// the runtime that opened them (another runtime's call reusing one
    /// hangs until its timeout).
    pub fn block_on<F: std::future::Future>(&self, future: F) -> Result<F::Output, String> {
        let runtime = match self.blocking.get() {
            Some(runtime) => runtime,
            None => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start the hub client: {e}"))?;
                self.blocking.get_or_init(|| runtime)
            }
        };
        Ok(runtime.block_on(future))
    }

    /// [`Endpoint::request`], blocking.
    pub fn blocking_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        self.block_on(self.request(method, path, body))?
    }

    /// [`Endpoint::sessions`], blocking.
    pub fn blocking_sessions(&self) -> Result<Vec<Value>, String> {
        self.block_on(self.sessions())?
    }

    /// [`Endpoint::workspaces`], blocking.
    pub fn blocking_workspaces(&self) -> Result<Vec<Value>, String> {
        self.block_on(self.workspaces())?
    }

    /// Open the WebSocket at `path` (`/api/events?client=...`,
    /// `/api/sessions/{id}/ws`).
    pub async fn websocket(&self, path: &str) -> Result<Socket, String> {
        use crate::net::WsError;
        let url = self.url(path)?;
        let token = self.token();
        let connect = crate::net::connect_ws(&url, &token, self.tls.as_ref(), WS_MAX_MESSAGE);
        tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| format!("timed out connecting to the hub at {}", self.base()))?
            .map_err(|e| match e {
                WsError::Invalid(message) => message,
                WsError::Connect(why) => self.unreachable(&why),
                WsError::Tls(why) => format!("TLS with {} failed: {why}", self.base()),
                WsError::Handshake(e) => ws_error(&e),
            })
    }
}

fn ws_error(error: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => {
            let message = response
                .body()
                .as_deref()
                .map(crate::net::error_text)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| {
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("refused")
                        .to_owned()
                });
            api_error(response.status().as_u16(), &message)
        }
        other => format!("WebSocket handshake failed: {other}"),
    }
}

/// An API failure for humans: a bad token says so; a request the hub
/// refused (4xx) says why in its own words; anything else keeps the HTTP
/// status to go on.
fn api_error(status: u16, message: &str) -> String {
    match (status, message.is_empty()) {
        (401, false) => format!("the hub refused the token ({message})"),
        (401, true) => "the hub refused the token".to_owned(),
        (400..500, false) => message.to_owned(),
        (_, true) => format!("the hub answered HTTP {status}"),
        _ => format!("{message} (HTTP {status})"),
    }
}

/// The state dir's token (for its own running hub).
fn local_token(state_dir: &Path) -> Result<String, String> {
    Token::load(state_dir)
        .map_err(|e| e.to_string())?
        .map(|t| t.expose().to_owned())
        .ok_or_else(|| "the running hub's token file is missing".to_owned())
}

/// What a verb that needs the running hub says when there is none: how to
/// start one for the same state dir (`--state-dir` spelled out unless it
/// is the default).
pub fn no_hub_message(state_dir: &Path) -> String {
    let flag = if default_state_dir().as_deref() == Some(state_dir) {
        String::new()
    } else {
        format!(" --state-dir {}", crate::util::shell_path(state_dir))
    };
    format!(
        "no hub is running for state dir {} (start one with `fw hub start{flag}`)",
        state_dir.display()
    )
}

/// `--url`: an http(s) origin, no path, query, or credentials.
fn parse_base(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("--url {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("--url {raw:?}: expected http:// or https://"));
    }
    if url.host_str().is_none() {
        return Err(format!("--url {raw:?}: no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "--url {raw:?}: credentials go in {TOKEN_ENV}, not the URL"
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "--url {raw:?}: give the hub's origin only (it is served from /)"
        ));
    }
    Ok(url)
}

/// Whether `base` (a `--url`) is the hub listening on `listen` here: plain
/// http to its port at its address, or at loopback when it listens on
/// loopback or every address.
fn is_local_hub(base: &Url, listen: SocketAddr) -> bool {
    if base.scheme() != "http" || base.port_or_known_default() != Some(listen.port()) {
        return false;
    }
    let host: Option<IpAddr> = match base.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        Some(url::Host::Domain(name)) if name.eq_ignore_ascii_case("localhost") => {
            Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        }
        _ => None,
    };
    let Some(host) = host else {
        return false;
    };
    let listen = listen.ip();
    host == listen || (host.is_loopback() && (listen.is_loopback() || listen.is_unspecified()))
}

/// rustls on ring with the system's roots (an https hub).
fn tls_config() -> Result<rustls::ClientConfig, String> {
    crate::net::roots_config(crate::net::native_roots("an https hub")?)
}

/// A fresh client id for the events socket (`[A-Za-z0-9_-]`, unique per
/// process): its own `done`/`idle` view of every session.
pub fn client_id(prefix: &str) -> String {
    format!("{prefix}-{}", sanitize_id(&crate::state::new_id()))
}

/// The same client id every run for this user on this machine (as a
/// browser keeps one): a turn seen in one `fw hub attach` stays seen in
/// the next.
pub fn stable_client_id(prefix: &str) -> String {
    let mut name = [0u8; 256];
    // SAFETY: writes at most `name.len()` bytes into an owned buffer.
    let host = if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0 {
        let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
        String::from_utf8_lossy(&name[..end]).into_owned()
    } else {
        String::new()
    };
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let host: String = sanitize_id(&host).chars().take(40).collect();
    format!("{prefix}-{uid}-{host}").chars().take(64).collect()
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_are_origins_only() {
        assert_eq!(
            parse_base("https://box.tail.ts.net").unwrap().as_str(),
            "https://box.tail.ts.net/"
        );
        assert_eq!(
            parse_base("http://127.0.0.1:7420/").unwrap().port(),
            Some(7420)
        );
        for bad in [
            "ftp://x",
            "https://x/hub",
            "https://u:p@x",
            "https://x/?a=1",
            "nonsense",
        ] {
            assert!(parse_base(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn client_ids_are_valid_and_distinct() {
        let (a, b) = (client_id("attach"), client_id("attach"));
        assert_ne!(a, b);
        assert!(a.len() <= 64 && a.starts_with("attach-"));
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn stable_client_ids_repeat_and_are_valid() {
        let a = stable_client_id("attach");
        assert_eq!(a, stable_client_id("attach"));
        assert!(a.len() <= 64 && a.starts_with("attach-"));
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn the_local_token_only_goes_to_the_local_hub() {
        let url = |s: &str| parse_base(s).unwrap();
        let lo: SocketAddr = "127.0.0.1:7420".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:7420".parse().unwrap();
        let lan: SocketAddr = "192.168.1.5:7420".parse().unwrap();
        assert!(is_local_hub(&url("http://127.0.0.1:7420"), lo));
        assert!(is_local_hub(&url("http://localhost:7420"), lo));
        assert!(is_local_hub(&url("http://127.0.0.1:7420"), any));
        assert!(is_local_hub(&url("http://192.168.1.5:7420"), lan));
        assert!(!is_local_hub(&url("http://127.0.0.1:7421"), lo));
        assert!(!is_local_hub(&url("http://other-box:7420"), lo));
        assert!(!is_local_hub(&url("http://10.0.0.9:7420"), any));
        assert!(!is_local_hub(&url("https://127.0.0.1:7420"), lo));
    }

    #[test]
    fn the_debug_form_never_shows_the_token() {
        let endpoint = Endpoint::new(
            Url::parse("http://127.0.0.1:1").unwrap(),
            "s3cr3t-token".into(),
            None,
            None,
        )
        .unwrap();
        assert!(!format!("{endpoint:?}").contains("s3cr3t"));
    }

    #[test]
    fn the_no_hub_hint_starts_a_hub_for_the_same_state_dir() {
        let message = no_hub_message(Path::new("/tmp/hub state"));
        assert!(
            message.ends_with("(start one with `fw hub start --state-dir '/tmp/hub state'`)"),
            "{message}"
        );
        assert_eq!(crate::util::shell_quote("/a/b-c.d"), "/a/b-c.d");
        assert_eq!(crate::util::shell_quote("it's"), "'it'\\''s'");
        assert_eq!(crate::util::shell_quote("~/x"), "'~/x'");
        if let Some(default) = default_state_dir() {
            assert!(no_hub_message(&default).ends_with("(start one with `fw hub start`)"));
        }
    }

    #[test]
    fn api_refusals_read_without_http_codes() {
        assert_eq!(api_error(409, "taken"), "taken");
        assert_eq!(api_error(500, "boom"), "boom (HTTP 500)");
        assert_eq!(api_error(502, ""), "the hub answered HTTP 502");
        assert_eq!(
            api_error(401, "unauthorized"),
            "the hub refused the token (unauthorized)"
        );
    }
}
