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
use std::sync::Arc;
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
    /// Default: FW_HUB_TOKEN, else the state dir's token file.
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

/// A resolved hub: base URL and credential.
pub struct Endpoint {
    base: Url,
    token: String,
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
    /// Resolve `args`: `--url` (with `--token`, FW_HUB_TOKEN, or the state
    /// dir's token), else the hub running for the state dir.
    pub(crate) fn resolve(args: &EndpointArgs) -> Result<Self, String> {
        let env_token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let explicit = args.token.clone().or(env_token);
        match &args.url {
            Some(raw) => {
                let base = parse_base(raw)?;
                let token = match explicit {
                    Some(token) => token,
                    None => {
                        let dir = resolve_state_dir(&args.state)?;
                        Token::load(&dir)
                            .map_err(|e| e.to_string())?
                            .map(|t| t.expose().to_owned())
                            .ok_or_else(|| {
                                format!(
                                    "--url needs the hub's token: set {TOKEN_ENV} (or --token) \
                                     to the output of `fw hub token` on its machine"
                                )
                            })?
                    }
                };
                Self::new(base, token)
            }
            None => {
                let dir = resolve_state_dir(&args.state)?;
                let info = running_hub(&dir)?.ok_or_else(|| {
                    format!(
                        "no hub is running for state dir {} (start one with `fw hub start`, \
                         or pass --url)",
                        dir.display()
                    )
                })?;
                let token = match explicit {
                    Some(token) => token,
                    None => Token::load(&dir)
                        .map_err(|e| e.to_string())?
                        .map(|t| t.expose().to_owned())
                        .ok_or_else(|| "the running hub's token file is missing".to_owned())?,
                };
                let base = Url::parse(&format!("http://{}", info.connect_addr()))
                    .map_err(|e| format!("bad hub address: {e}"))?;
                Self::new(base, token)
            }
        }
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
            token,
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
        let response =
            request.bearer_auth(&self.token).send().await.map_err(|e| {
                format!("cannot reach the hub at {}: {}", self.base(), describe(&e))
            })?;
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
        let bearer = format!("Bearer {}", self.token)
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
    let id = crate::state::new_id();
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!("{prefix}-{id}")
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
    fn the_debug_form_never_shows_the_token() {
        let endpoint = Endpoint::new(
            Url::parse("http://127.0.0.1:1").unwrap(),
            "s3cr3t-token".into(),
        )
        .unwrap();
        assert!(!format!("{endpoint:?}").contains("s3cr3t"));
    }
}
