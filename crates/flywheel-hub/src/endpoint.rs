//! Where a terminal client (`fw hub attach`, `fw hub wait`) reaches a hub:
//! the one running for this machine's state dir (address from its runtime
//! file, token from its token file), or any hub by `--url` and token (a
//! remote one behind `tailscale serve`, over https).
//!
//! Speaks the same API the browser does: bearer-authenticated JSON over
//! HTTP and the `/api/sessions/{id}/ws` and `/api/events` WebSockets. No
//! `Origin` is sent: the hub's cross-origin checks are for browsers.

use crate::auth::Token;
use crate::cli::{StateDirArg, resolve_state_dir, running_hub};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use url::Url;

/// How long connecting (TCP, TLS, WebSocket handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one REST call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest WebSocket message accepted (the hub sends at most 256 KiB).
const WS_MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// The environment variable holding a remote hub's token.
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
    #[command(flatten)]
    pub(crate) state: StateDirArg,
}

/// Anything a WebSocket can run over (plain TCP or TLS).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A WebSocket to the hub.
pub type Socket = WebSocketStream<Box<dyn Io>>;

/// Where the token comes from.
enum TokenFrom {
    /// `--token` / FW_HUB_TOKEN.
    Given(String),
    /// The state dir's token file.
    StateDir(PathBuf),
}

/// A resolved hub: base URL and credential.
pub struct Endpoint {
    base: Url,
    token: RwLock<String>,
    /// The state dir the token was read from (re-read after a rotation);
    /// `None` for a token given by hand.
    token_dir: Option<PathBuf>,
    tls: Option<Arc<rustls::ClientConfig>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Endpoint")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl Endpoint {
    /// Resolve `args`: `--url` (with `--token` or FW_HUB_TOKEN; the state
    /// dir's token only when the URL is that state dir's running hub), else
    /// the hub running for the state dir.
    pub(crate) fn resolve(args: &EndpointArgs) -> Result<Self, String> {
        let env_token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let explicit = args.token.clone().or(env_token);
        let dir = || resolve_state_dir(&args.state);
        let local_token = |dir: &std::path::Path| {
            Token::load(dir)
                .map_err(|e| e.to_string())?
                .map(|t| t.expose().to_owned())
                .ok_or_else(|| "the running hub's token file is missing".to_owned())
        };
        let (base, from) = match &args.url {
            Some(raw) => {
                let base = parse_base(raw)?;
                if let Some(token) = explicit {
                    (base, TokenFrom::Given(token))
                } else {
                    // Never hand this machine's credential to another host:
                    // the local token only goes to the local hub.
                    let dir = dir()?;
                    let listen = running_hub(&dir)?.map(|info| info.listen);
                    if !listen.is_some_and(|listen| is_local_hub(&base, listen)) {
                        return Err(format!(
                            "--url needs the hub's token: set {TOKEN_ENV} (or --token) to the \
                             output of `fw hub token` on its machine (the local token is only \
                             sent to the hub running for {})",
                            dir.display()
                        ));
                    }
                    (base, TokenFrom::StateDir(dir))
                }
            }
            None => {
                let dir = dir()?;
                let info = running_hub(&dir)?.ok_or_else(|| {
                    format!(
                        "no hub is running for state dir {} (start one with `fw hub start`, \
                         or pass --url)",
                        dir.display()
                    )
                })?;
                let base = Url::parse(&format!("http://{}", info.connect_addr()))
                    .map_err(|e| format!("bad hub address: {e}"))?;
                match explicit {
                    Some(token) => (base, TokenFrom::Given(token)),
                    None => (base, TokenFrom::StateDir(dir)),
                }
            }
        };
        match from {
            TokenFrom::Given(token) => Self::new(base, token),
            TokenFrom::StateDir(dir) => {
                let mut endpoint = Self::new(base, local_token(&dir)?)?;
                endpoint.token_dir = Some(dir);
                Ok(endpoint)
            }
        }
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

    fn new(base: Url, token: String) -> Result<Self, String> {
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
            .timeout(CALL_TIMEOUT)
            .user_agent(concat!("flywheel-hub/", env!("CARGO_PKG_VERSION")));
        builder = match &tls {
            Some(config) => builder.tls_backend_preconfigured(config.as_ref().clone()),
            None => builder.tls_backend_preconfigured(tls_config_plain()?),
        };
        let http = builder
            .build()
            .map_err(|e| format!("HTTP client setup failed: {e}"))?;
        Ok(Self {
            base,
            token: RwLock::new(token),
            token_dir: None,
            tls,
            http,
        })
    }

    /// The hub's base URL (for messages; carries no credential).
    pub fn base(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        self.base
            .join(path)
            .map_err(|e| format!("bad API path {path:?}: {e}"))
    }

    /// `GET path` -> JSON (`null` when empty), or the API's error.
    pub async fn get(&self, path: &str) -> Result<Value, String> {
        let request = self.http.get(self.url(path)?);
        self.send(request).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, String> {
        let response = request
            .bearer_auth(self.token())
            .send()
            .await
            .map_err(|e| format!("cannot reach the hub at {}: {}", self.base(), describe(&e)))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading the hub's answer: {}", describe(&e)))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).trim().to_owned());
            return Err(match status {
                401 => format!("the hub refused the token ({message})"),
                _ => message,
            });
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).map_err(|e| format!("the hub sent bad JSON: {e}"))
    }

    /// Every session record (`GET /api/sessions`).
    pub async fn sessions(&self) -> Result<Vec<Value>, String> {
        list(self.get("/api/sessions").await?, "sessions")
    }

    /// Every workspace (`GET /api/workspaces`).
    pub async fn workspaces(&self) -> Result<Vec<Value>, String> {
        list(self.get("/api/workspaces").await?, "workspaces")
    }

    /// Open the WebSocket at `path` (`/api/events?client=...`,
    /// `/api/sessions/{id}/ws`).
    pub async fn websocket(&self, path: &str) -> Result<Socket, String> {
        let mut url = self.url(path)?;
        let secure = url.scheme() == "https";
        url.set_scheme(if secure { "wss" } else { "ws" })
            .map_err(|()| "cannot form the WebSocket URL".to_owned())?;
        let host = url
            .host_str()
            .ok_or_else(|| "the hub URL has no host".to_owned())?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "the hub URL has no port".to_owned())?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad WebSocket request: {e}"))?;
        let bearer = format!("Bearer {}", self.token())
            .parse()
            .map_err(|_| "the token is not a valid header value".to_owned())?;
        request.headers_mut().insert("authorization", bearer);
        let connect = async {
            let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| format!("cannot reach the hub at {}: {e}", self.base()))?;
            let _ = tcp.set_nodelay(true);
            let stream: Box<dyn Io> = match &self.tls {
                Some(config) => {
                    let name = rustls::pki_types::ServerName::try_from(host.clone())
                        .map_err(|e| format!("bad TLS server name {host:?}: {e}"))?;
                    let tls = tokio_rustls::TlsConnector::from(Arc::clone(config))
                        .connect(name, tcp)
                        .await
                        .map_err(|e| format!("TLS with {} failed: {e}", self.base()))?;
                    Box::new(tls)
                }
                None => Box::new(tcp),
            };
            let config = WebSocketConfig::default()
                .max_message_size(Some(WS_MAX_MESSAGE))
                .max_frame_size(Some(WS_MAX_MESSAGE));
            let (socket, _) =
                tokio_tungstenite::client_async_with_config(request, stream, Some(config))
                    .await
                    .map_err(|e| ws_error(&e))?;
            Ok::<_, String>(socket)
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| format!("timed out connecting to the hub at {}", self.base()))?
    }
}

fn ws_error(error: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => {
            let status = response.status().as_u16();
            let message = response
                .body()
                .as_deref()
                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| {
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("refused")
                        .to_owned()
                });
            match status {
                401 => format!("the hub refused the token ({message})"),
                _ => message,
            }
        }
        other => format!("WebSocket handshake failed: {other}"),
    }
}

/// The `{"<what>": [...]}` list of an answer.
fn list(mut value: Value, what: &str) -> Result<Vec<Value>, String> {
    match value.get_mut(what).map(Value::take) {
        Some(Value::Array(items)) => Ok(items),
        _ => Err(format!("the hub sent no {what} list")),
    }
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
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err("no trusted root certificates found on this system for an https hub".into());
    }
    client_config(roots)
}

/// A TLS config reqwest needs even for plain http (it is never used).
fn tls_config_plain() -> Result<rustls::ClientConfig, String> {
    client_config(rustls::RootCertStore::empty())
}

fn client_config(roots: rustls::RootCertStore) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn describe(error: &reqwest::Error) -> String {
    use std::error::Error as _;
    let what = if error.is_connect() {
        "cannot connect"
    } else if error.is_timeout() {
        "timed out"
    } else {
        "request failed"
    };
    match error.source() {
        Some(source) => format!("{what}: {source}"),
        None => what.to_owned(),
    }
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
        )
        .unwrap();
        assert!(!format!("{endpoint:?}").contains("s3cr3t"));
    }
}
