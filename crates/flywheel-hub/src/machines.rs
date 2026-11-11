//! Multiple machines: the other hubs this one shows beside its own
//! (`fw hub machine add|ls|rm|test`, the UI's machines).
//!
//! A machine is a name, the remote hub's base URL, and a **peer
//! credential** minted on that hub (`fw hub peer issue`, see
//! [`crate::peers`]) -- never its token. This hub keeps them in
//! [`MACHINES_FILE`] (0600) and proxies the browser's traffic to them under
//! `/m/<name>/api/...` (HTTP and WebSockets, `server/proxy.rs`), so the
//! browser only ever talks to its own origin and never holds a remote's
//! credential. The proxy reaches nothing but a registered machine's base
//! URL ([`Machine::target`]). https machines are verified against the
//! system roots, or against a pinned SHA-256 certificate fingerprint (a
//! self-signed hub).

use crate::auth::{unix_now, write_private};
use crate::net::Socket;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

/// The machines file inside the state dir.
pub const MACHINES_FILE: &str = "machines.json";
/// Machines kept at once.
pub const MAX_MACHINES: usize = 32;
/// Longest machine name.
const MAX_NAME: usize = 32;
/// Connecting (TCP, TLS, WebSocket handshake) to a machine.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// One proxied REST call (review actions run git and gh there).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// A probe (`whoami`, `test`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest WebSocket message from a machine (a hub sends at most 256 KiB).
const WS_MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// After a machine refused its credential, how long the proxy answers for
/// it without asking (each refusal counts against this hub's address in
/// the machine's failure limit).
pub const REFUSED_BACKOFF: Duration = Duration::from_secs(90);
/// This hub's id file inside the state dir (`whoami`'s `hub_id`).
pub const HUB_ID_FILE: &str = "hub-id";
/// Largest probe answer (`whoami`, minting, revoking) read from a machine.
const MAX_PROBE_BODY: usize = 1024 * 1024;

/// Machine registry failures.
#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("machines file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What is stored per machine (0600; the credential is never sent to a
/// browser).
#[derive(Clone, Serialize, Deserialize)]
pub struct MachineRecord {
    pub name: String,
    /// The remote hub's origin (`https://box.tailnet.ts.net`).
    pub url: String,
    /// The peer credential minted on it.
    pub credential: String,
    /// Pinned SHA-256 fingerprint of its TLS certificate (lowercase hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The peer it is on the remote (id and name), for `machine ls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
    /// The remote hub's id (`whoami`): one hub is registered once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_id: Option<String>,
    /// Unix seconds.
    pub added: u64,
}

impl std::fmt::Debug for MachineRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineRecord")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct MachinesFile {
    machines: Vec<MachineRecord>,
}

/// A machine name: 1-32 of `[a-z0-9-]`, starting with a letter or digit;
/// `local` is this hub.
pub fn validate_machine_name(raw: &str) -> Result<String, MachineError> {
    let name = raw.trim();
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-');
    if !ok {
        return Err(MachineError::Invalid(format!(
            "a machine name is 1-{MAX_NAME} of a-z, 0-9 and '-' (not starting with '-')"
        )));
    }
    if name == "local" {
        return Err(MachineError::Invalid(
            "`local` means this hub; pick another name".into(),
        ));
    }
    Ok(name.to_owned())
}

/// A machine's base URL: an http(s) origin (no path, query, or
/// credentials), normalized without a trailing slash.
pub fn parse_machine_url(raw: &str) -> Result<Url, MachineError> {
    let bad = |why: &str| MachineError::Invalid(format!("machine URL {raw:?}: {why}"));
    if !raw.contains("://") {
        return Err(bad(&format!(
            "start it with http:// or https:// (http://{0} or https://{0})",
            raw.trim()
        )));
    }
    let url = Url::parse(raw.trim()).map_err(|e| bad(&e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("expected http:// or https://"));
    }
    if url.host_str().is_none() {
        return Err(bad("no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("no credentials in the URL"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(bad("give the hub's origin only (it is served from /)"));
    }
    Ok(url)
}

/// A pinned certificate fingerprint: 64 hex digits, colons and a `sha256:`
/// prefix allowed (`openssl x509 -fingerprint -sha256` prints
/// `AB:CD:...`); normalized to lowercase hex.
pub fn parse_fingerprint(raw: &str) -> Result<String, MachineError> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("sha256:")
        .or_else(|| trimmed.strip_prefix("SHA256:"))
        .or_else(|| trimmed.strip_prefix("sha256 Fingerprint="))
        .unwrap_or(trimmed);
    let hex: String = trimmed.chars().filter(|c| *c != ':').collect();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MachineError::Invalid(
            "a fingerprint is the certificate's SHA-256: 64 hex digits (colons allowed)".into(),
        ));
    }
    Ok(hex.to_ascii_lowercase())
}

/// `AB:CD:...` (as openssl prints it).
pub fn display_fingerprint(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// This machine's host name (`whoami`, a default peer name).
pub fn host_name() -> String {
    let mut name = [0u8; 256];
    // SAFETY: writes at most `name.len()` bytes into an owned buffer.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return String::new();
    }
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    String::from_utf8_lossy(&name[..end]).into_owned()
}

// --- TLS --------------------------------------------------------------------

/// A machine's TLS: the system roots, or the pinned fingerprint (its own
/// certificate, or the CA it chains to -- a hub's `fw hub tls ca`).
fn tls_config(
    pin: Option<&str>,
    observed: Arc<Mutex<Option<String>>>,
) -> Result<rustls::ClientConfig, String> {
    crate::net::pinned_config(
        pin,
        &[],
        observed,
        "an https machine (pin its certificate with --fingerprint)",
    )
}

// --- one machine --------------------------------------------------------------

/// How this hub last fared with a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// Not tried since this hub started.
    Unknown,
    Online,
    /// Unreachable (connect, TLS, timeout, 5xx from a proxy in front).
    Offline,
    /// It refused the credential (revoked?).
    Refused,
}

/// [`LinkState`] with its details, as `GET /api/machines` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct LinkStatus {
    pub state: LinkState,
    /// Unix seconds of the last change.
    pub since: u64,
    pub error: Option<String>,
    /// The remote's flywheel version and host name (from `whoami`).
    pub version: Option<String>,
    pub host: Option<String>,
    /// The SHA-256 fingerprint the remote last presented (https).
    pub presented_fingerprint: Option<String>,
    #[serde(skip)]
    refused_at: Option<Instant>,
}

/// Why a machine could not be used.
#[derive(Debug, Clone)]
pub enum LinkError {
    /// Not reachable (or not TLS-verifiable); the message says which.
    Unreachable(String),
    /// It answered 401: the credential is not (any longer) valid there.
    Refused(String),
    /// The request itself is not proxyable (outside `/api`).
    Invalid(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) | Self::Refused(m) | Self::Invalid(m) => f.write_str(m),
        }
    }
}

/// What `GET /api/whoami` on a hub says.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Whoami {
    pub scope: String,
    pub peer: Option<crate::peers::PeerInfo>,
    pub version: String,
    #[serde(default)]
    pub host: String,
    /// The hub's id (stable across restarts; absent on older hubs).
    #[serde(default)]
    pub hub_id: Option<String>,
}

/// A registered machine and its connection to the remote hub.
pub struct Machine {
    record: MachineRecord,
    base: Url,
    /// Unique per registration (a re-added name is a new machine).
    epoch: u64,
    http: reqwest::Client,
    tls: Option<Arc<rustls::ClientConfig>>,
    observed: Arc<Mutex<Option<String>>>,
    status: Mutex<LinkStatus>,
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("name", &self.record.name)
            .field("url", &self.record.url)
            .finish_non_exhaustive()
    }
}

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

impl Machine {
    /// A connection for `record` (validated: name, URL, fingerprint).
    pub fn new(record: MachineRecord) -> Result<Self, MachineError> {
        let name = validate_machine_name(&record.name)?;
        let base = parse_machine_url(&record.url)?;
        let fingerprint = record
            .fingerprint
            .as_deref()
            .map(parse_fingerprint)
            .transpose()?;
        if fingerprint.is_some() && base.scheme() != "https" {
            return Err(MachineError::Invalid(
                "--fingerprint pins an https machine's certificate; this URL is http".into(),
            ));
        }
        let credential = record.credential.trim().to_owned();
        if credential.is_empty() || credential.bytes().any(|b| !b.is_ascii_graphic()) {
            return Err(MachineError::Invalid(
                "the credential must be printable ASCII without spaces".into(),
            ));
        }
        let observed = Arc::new(Mutex::new(None));
        let tls = (base.scheme() == "https")
            .then(|| tls_config(fingerprint.as_deref(), Arc::clone(&observed)))
            .transpose()
            .map_err(MachineError::Invalid)?
            .map(Arc::new);
        let preconfigured = match &tls {
            Some(config) => config.as_ref().clone(),
            None => crate::net::plain_config().map_err(MachineError::Invalid)?,
        };
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!(
                "flywheel-hub/",
                env!("CARGO_PKG_VERSION"),
                " (machine proxy)"
            ))
            .tls_backend_preconfigured(preconfigured)
            .build()
            .map_err(|e| MachineError::Invalid(format!("HTTP client setup failed: {e}")))?;
        let record = MachineRecord {
            name,
            url: base.as_str().trim_end_matches('/').to_owned(),
            credential,
            fingerprint,
            ..record
        };
        Ok(Self {
            record,
            base,
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            http,
            tls,
            observed,
            status: Mutex::new(LinkStatus {
                state: LinkState::Unknown,
                since: unix_now(),
                error: None,
                version: None,
                host: None,
                presented_fingerprint: None,
                refused_at: None,
            }),
        })
    }

    pub fn name(&self) -> &str {
        &self.record.name
    }

    pub fn url(&self) -> &str {
        &self.record.url
    }

    pub fn record(&self) -> &MachineRecord {
        &self.record
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether `other` reaches the same hub: the same URL, or the same hub
    /// id or peer id learned from `whoami`.
    pub fn same_hub(&self, other: &Machine) -> bool {
        let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
        self.record.url == other.record.url
            || same(&self.record.hub_id, &other.record.hub_id)
            || same(&self.record.peer_id, &other.record.peer_id)
    }

    pub fn status(&self) -> LinkStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The machine's view for the API and `machine ls` (never the
    /// credential).
    pub fn view(&self) -> Value {
        let status = self.status();
        serde_json::json!({
            "name": self.record.name,
            "url": self.record.url,
            "fingerprint": self.record.fingerprint.as_deref().map(display_fingerprint),
            "hub_id": self.record.hub_id,
            "peer": self.record.peer_id.as_ref().map(|id| serde_json::json!({
                "id": id,
                "name": self.record.peer_name,
            })),
            "added": self.record.added,
            "status": status,
        })
    }

    /// The URL `path_and_query` (`/api/...`) names on this machine -- and
    /// only on it: whatever the path, the result has this machine's
    /// scheme, host, and port, and stays under `/api` (the SSRF guard).
    pub fn target(&self, path_and_query: &str) -> Result<Url, LinkError> {
        let invalid = |why: &str| LinkError::Invalid(format!("not proxyable: {why}"));
        if !(path_and_query == "/api" || path_and_query.starts_with("/api/"))
            || path_and_query.starts_with("/api//")
            || path_and_query.contains('\\')
            || path_and_query.contains('#')
            || path_and_query
                .bytes()
                .any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(invalid("only the hub API (/api/...) is proxied"));
        }
        let url = Url::parse(&format!("{}{path_and_query}", self.record.url))
            .map_err(|e| invalid(&e.to_string()))?;
        let same = url.scheme() == self.base.scheme()
            && url.host() == self.base.host()
            && url.port_or_known_default() == self.base.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none();
        if !same || !(url.path() == "/api" || url.path().starts_with("/api/")) {
            return Err(invalid("the path leaves the machine's API"));
        }
        Ok(url)
    }

    fn note(&self, outcome: Result<(), &LinkError>) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let state = match outcome {
            Ok(()) => LinkState::Online,
            Err(LinkError::Refused(_)) => LinkState::Refused,
            Err(LinkError::Unreachable(_)) => LinkState::Offline,
            Err(LinkError::Invalid(_)) => return,
        };
        if status.state != state {
            status.state = state;
            status.since = unix_now();
        }
        status.error = outcome.err().map(ToString::to_string);
        status.refused_at = (state == LinkState::Refused).then(Instant::now);
        if let Some(seen) = self
            .observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            status.presented_fingerprint = Some(display_fingerprint(&seen));
        }
    }

    /// While the machine recently refused the credential, the refusal
    /// (without asking it again).
    pub fn held_refusal(&self) -> Option<LinkError> {
        let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let at = status.refused_at?;
        (at.elapsed() < REFUSED_BACKOFF).then(|| {
            LinkError::Refused(
                status
                    .error
                    .clone()
                    .unwrap_or_else(|| self.refused_message()),
            )
        })
    }

    fn refused_message(&self) -> String {
        format!(
            "machine {} refused this hub's peer credential (revoked there?): \
             re-pair it with `fw hub machine add {0} {1} --replace`",
            self.record.name, self.record.url
        )
    }

    fn unreachable(&self, why: &str) -> LinkError {
        let mut message = format!(
            "machine {} ({}) is unreachable: {why}",
            self.record.name, self.record.url
        );
        if self.tls.is_some() && self.record.fingerprint.is_none() && looks_like_tls(why) {
            let seen = self
                .observed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(seen) = seen {
                message.push_str(&format!(
                    "; it presented a certificate with SHA-256 fingerprint {} -- if that is \
                     its own (self-signed) certificate or its hub's local CA (`fw hub tls ca` \
                     there prints it), pin it with --fingerprint",
                    display_fingerprint(&seen)
                ));
            }
        }
        LinkError::Unreachable(message)
    }

    /// Send a request to `path_and_query` on the machine with its peer
    /// credential; records how it went. A 401 is [`LinkError::Refused`];
    /// every other answer is the caller's.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path_and_query: &str,
        body: Option<(Vec<u8>, String)>,
        timeout: Duration,
    ) -> Result<reqwest::Response, LinkError> {
        let url = self.target(path_and_query)?;
        let mut request = self
            .http
            .request(method, url)
            .timeout(timeout)
            .bearer_auth(&self.record.credential);
        if let Some((bytes, content_type)) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes);
        }
        let outcome = match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                Err(LinkError::Refused(self.refused_message()))
            }
            Ok(response) => Ok(response),
            Err(e) => Err(self.unreachable(&crate::net::describe(&e))),
        };
        self.note(outcome.as_ref().map(|_| ()));
        outcome
    }

    /// `POST` an encoded body (`Content-Encoding: encoding`) to
    /// `path_and_query` with the peer credential, as [`Machine::send`].
    pub async fn send_encoded(
        &self,
        path_and_query: &str,
        body: Vec<u8>,
        content_type: &str,
        encoding: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, LinkError> {
        let url = self.target(path_and_query)?;
        let request = self
            .http
            .post(url)
            .timeout(timeout)
            .bearer_auth(&self.record.credential)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_ENCODING, encoding)
            .body(body);
        let outcome = match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                Err(LinkError::Refused(self.refused_message()))
            }
            Ok(response) => Ok(response),
            Err(e) => Err(self.unreachable(&crate::net::describe(&e))),
        };
        self.note(outcome.as_ref().map(|_| ()));
        outcome
    }

    /// `GET /api/whoami` there: what the credential is on the machine.
    pub async fn whoami(&self) -> Result<Whoami, LinkError> {
        let response = self
            .send(reqwest::Method::GET, "/api/whoami", None, PROBE_TIMEOUT)
            .await?;
        let status = response.status();
        let body = read_capped(response, MAX_PROBE_BODY)
            .await
            .map_err(|e| self.unreachable(&e))?;
        if !status.is_success() {
            let error = LinkError::Unreachable(format!(
                "machine {} answered HTTP {} to whoami: {} (is it a flywheel hub new enough for machines?)",
                self.record.name,
                status.as_u16(),
                crate::net::error_text(&body)
            ));
            self.note(Err(&error));
            return Err(error);
        }
        let who: Whoami = serde_json::from_slice(&body).map_err(|e| {
            LinkError::Unreachable(format!(
                "machine {} sent an unexpected whoami: {e}",
                self.record.name
            ))
        })?;
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        status.version = Some(who.version.clone());
        status.host = Some(who.host.clone());
        Ok(who)
    }

    /// `POST /api/peers` there, with the credential this machine was made
    /// with (the remote's token, used once): a fresh peer credential.
    pub async fn mint_peer(&self, name: &str) -> Result<(crate::peers::PeerInfo, String), String> {
        let body =
            serde_json::to_vec(&serde_json::json!({ "name": name })).map_err(|e| e.to_string())?;
        let response = self
            .send(
                reqwest::Method::POST,
                "/api/peers",
                Some((body, "application/json".into())),
                PROBE_TIMEOUT,
            )
            .await
            .map_err(|e| match e {
                LinkError::Refused(_) => format!(
                    "machine {} refused the token (give `fw hub token` from that machine)",
                    self.record.name
                ),
                other => other.to_string(),
            })?;
        let status = response.status();
        let bytes = read_capped(response, MAX_PROBE_BODY).await?;
        if !status.is_success() {
            return Err(format!(
                "machine {} would not mint a peer credential: {} (HTTP {})",
                self.record.name,
                crate::net::error_text(&bytes),
                status.as_u16()
            ));
        }
        #[derive(Deserialize)]
        struct Minted {
            peer: crate::peers::PeerInfo,
            credential: String,
        }
        let minted: Minted = serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "machine {} sent an unexpected answer: {e}",
                self.record.name
            )
        })?;
        Ok((minted.peer, minted.credential))
    }

    /// `DELETE /api/peer` there: the credential revokes itself.
    pub async fn revoke_self(&self) -> Result<(), LinkError> {
        let response = self
            .send(reqwest::Method::DELETE, "/api/peer", None, PROBE_TIMEOUT)
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status().as_u16();
            let body = read_capped(response, MAX_PROBE_BODY)
                .await
                .unwrap_or_default();
            Err(LinkError::Unreachable(format!(
                "machine {} did not revoke the credential: {} (HTTP {status})",
                self.record.name,
                crate::net::error_text(&body)
            )))
        }
    }

    /// Open the WebSocket at `path_and_query` (`/api/events?client=...`,
    /// `/api/sessions/{id}/ws`) on the machine. A refused handshake keeps
    /// the machine's answer: `Err(Ok((status, body)))`.
    pub async fn websocket(
        &self,
        path_and_query: &str,
    ) -> Result<Socket, Result<(u16, Vec<u8>), LinkError>> {
        if let Some(held) = self.held_refusal() {
            return Err(Err(held));
        }
        let url = self.target(path_and_query).map_err(Err)?;
        let connect = async {
            use crate::net::WsError;
            use tokio_tungstenite::tungstenite::Error;
            match crate::net::connect_ws(
                &url,
                &self.record.credential,
                self.tls.as_ref(),
                WS_MAX_MESSAGE,
            )
            .await
            {
                Ok(socket) => Ok(socket),
                Err(WsError::Invalid(message)) => Err(Err(LinkError::Invalid(message))),
                Err(WsError::Connect(why)) => Err(Err(self.unreachable(&why))),
                Err(WsError::Tls(why)) => Err(Err(self.unreachable(&format!("TLS: {why}")))),
                Err(WsError::Handshake(Error::Http(response))) => {
                    let status = response.status().as_u16();
                    if status == 401 {
                        return Err(Err(LinkError::Refused(self.refused_message())));
                    }
                    Err(Ok((status, response.body().clone().unwrap_or_default())))
                }
                Err(WsError::Handshake(e)) => {
                    Err(Err(self.unreachable(&format!("WebSocket handshake: {e}"))))
                }
            }
        };
        let outcome = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .unwrap_or_else(|_| Err(Err(self.unreachable("timed out connecting"))));
        match &outcome {
            Ok(_) | Err(Ok(_)) => self.note(Ok(())),
            Err(Err(e)) => self.note(Err(e)),
        }
        outcome
    }
}

/// A response's body, refused once it passes `limit` bytes (a machine
/// cannot fill this hub's memory).
pub async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|n| usize::try_from(n).map_or(true, |n| n > limit))
    {
        return Err(format!("its answer is larger than {limit} bytes: refused"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| crate::net::describe(&e))?
    {
        if body.len() + chunk.len() > limit {
            return Err(format!("its answer is larger than {limit} bytes: refused"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn looks_like_tls(why: &str) -> bool {
    let why = why.to_ascii_lowercase();
    why.contains("certificate") || why.contains("tls") || why.contains("unknownissuer")
}

// --- the registry -------------------------------------------------------------

/// This hub's machines, persisted in the state dir.
pub struct Machines {
    state_dir: PathBuf,
    /// This hub's own id (`whoami`), kept in [`HUB_ID_FILE`].
    hub_id: String,
    machines: RwLock<Vec<Arc<Machine>>>,
    /// Why the file could not be read (then nothing is saved over it).
    broken: Option<String>,
    changes: watch::Sender<u64>,
}

impl std::fmt::Debug for Machines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machines")
            .field("count", &self.list().len())
            .finish_non_exhaustive()
    }
}

impl Machines {
    /// The machines recorded in `state_dir`. An unreadable file leaves the
    /// registry empty and read-only (said on stderr and by every change).
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(MACHINES_FILE);
        let (machines, broken) = match read_records(&path) {
            Ok(records) => {
                let mut machines = Vec::new();
                for record in records {
                    let name = record.name.clone();
                    match Machine::new(record) {
                        Ok(machine) => machines.push(Arc::new(machine)),
                        Err(e) => eprintln!("fw hub: machine {name} in {}: {e}", path.display()),
                    }
                }
                (machines, None)
            }
            Err(e) => {
                eprintln!("fw hub: {e}; machines are off until it is fixed or removed");
                (Vec::new(), Some(e.to_string()))
            }
        };
        Self {
            state_dir: state_dir.to_owned(),
            hub_id: load_hub_id(state_dir),
            machines: RwLock::new(machines),
            broken,
            changes: watch::Sender::new(0),
        }
    }

    /// This hub's id: a hub refuses to register itself, and one hub is
    /// registered once.
    pub fn hub_id(&self) -> &str {
        &self.hub_id
    }

    pub fn list(&self) -> Vec<Arc<Machine>> {
        self.machines
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<Machine>> {
        self.list().into_iter().find(|m| m.name() == name)
    }

    /// Whether the machine registered as `epoch` still is.
    pub fn alive(&self, epoch: u64) -> bool {
        self.list().iter().any(|m| m.epoch() == epoch)
    }

    /// Bumped on every add and remove.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn writable(&self) -> Result<(), MachineError> {
        match &self.broken {
            Some(why) => Err(MachineError::Conflict(format!(
                "{why}: fix or remove it, then restart the hub"
            ))),
            None => Ok(()),
        }
    }

    /// Register `machine` (its name must be free) and save.
    pub fn insert(&self, machine: Machine) -> Result<Arc<Machine>, MachineError> {
        self.writable()?;
        let machine = Arc::new(machine);
        {
            let mut machines = self
                .machines
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if machines.iter().any(|m| m.name() == machine.name()) {
                return Err(MachineError::Conflict(format!(
                    "a machine named {0} exists (re-pair it with --replace, or fw hub machine rm {0})",
                    machine.name()
                )));
            }
            if let Some(other) = machines.iter().find(|m| m.same_hub(&machine)) {
                return Err(duplicate(other.name()));
            }
            if machines.len() >= MAX_MACHINES {
                return Err(MachineError::Conflict(format!(
                    "at most {MAX_MACHINES} machines"
                )));
            }
            machines.push(Arc::clone(&machine));
            self.save(&machines)?;
        }
        self.changes.send_modify(|n| *n += 1);
        Ok(machine)
    }

    /// Swap machine `name`'s registration for `machine` (same name: a new
    /// credential, URL, or pin) and save; the old one, returned, is no
    /// longer alive (its proxied sockets close).
    pub fn replace(&self, machine: Machine) -> Result<Arc<Machine>, MachineError> {
        self.writable()?;
        let machine = Arc::new(machine);
        let old = {
            let mut machines = self
                .machines
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let index = machines
                .iter()
                .position(|m| m.name() == machine.name())
                .ok_or_else(|| {
                    MachineError::NotFound(format!("no machine named {}", machine.name()))
                })?;
            if let Some(other) = machines
                .iter()
                .find(|m| m.name() != machine.name() && m.same_hub(&machine))
            {
                return Err(duplicate(other.name()));
            }
            let old = std::mem::replace(&mut machines[index], Arc::clone(&machine));
            if let Err(e) = self.save(&machines) {
                machines[index] = old;
                return Err(e);
            }
            old
        };
        self.changes.send_modify(|n| *n += 1);
        Ok(old)
    }

    /// The registered machine that is the same hub as `machine` would be
    /// (same URL, hub id, or peer there), if any (besides `except`).
    pub fn duplicate_of(&self, machine: &Machine, except: Option<&str>) -> Option<String> {
        self.list()
            .iter()
            .find(|m| Some(m.name()) != except && m.same_hub(machine))
            .map(|m| m.name().to_owned())
    }

    /// Forget machine `name` and save.
    pub fn remove(&self, name: &str) -> Result<Arc<Machine>, MachineError> {
        self.writable()?;
        let removed = {
            let mut machines = self
                .machines
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let index = machines
                .iter()
                .position(|m| m.name() == name)
                .ok_or_else(|| MachineError::NotFound(format!("no machine named {name}")))?;
            let removed = machines.remove(index);
            if let Err(e) = self.save(&machines) {
                machines.insert(index, Arc::clone(&removed));
                return Err(e);
            }
            removed
        };
        self.changes.send_modify(|n| *n += 1);
        Ok(removed)
    }

    fn save(&self, machines: &[Arc<Machine>]) -> Result<(), MachineError> {
        let path = self.state_dir.join(MACHINES_FILE);
        let file = MachinesFile {
            machines: machines.iter().map(|m| m.record().clone()).collect(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| MachineError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        write_private(&path, &json).map_err(|source| MachineError::Io { path, source })
    }
}

fn read_records(path: &Path) -> Result<Vec<MachineRecord>, MachineError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(MachineError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_slice::<MachinesFile>(&bytes)
        .map(|file| file.machines)
        .map_err(|e| MachineError::Io {
            path: path.to_owned(),
            source: std::io::Error::other(e),
        })
}

/// A fresh record (now).
pub fn record(name: &str, url: &str, credential: &str, fingerprint: Option<&str>) -> MachineRecord {
    MachineRecord {
        name: name.to_owned(),
        url: url.to_owned(),
        credential: credential.to_owned(),
        fingerprint: fingerprint.map(str::to_owned),
        peer_id: None,
        peer_name: None,
        hub_id: None,
        added: unix_now(),
    }
}

fn duplicate(name: &str) -> MachineError {
    MachineError::Conflict(format!(
        "that hub is already registered as machine {name} (one hub is registered once; \
         re-pair it with `fw hub machine add {name} <url> --replace`)"
    ))
}

/// This hub's id in `state_dir` (made on first use). Unreadable or
/// unwritable: a fresh one for this process.
pub(crate) fn load_hub_id(state_dir: &Path) -> String {
    let path = state_dir.join(HUB_ID_FILE);
    if let Ok(text) = fs::read_to_string(&path) {
        let id = text.trim();
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return id.to_ascii_lowercase();
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    if fs::create_dir_all(state_dir).is_ok()
        && let Err(e) = write_private(&path, id.as_bytes())
    {
        eprintln!("fw hub: cannot save {}: {e}", path.display());
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(url: &str) -> Machine {
        Machine::new(record("box", url, "fwpeer_x_y", None)).unwrap()
    }

    #[test]
    fn names_urls_and_fingerprints_are_validated() {
        assert_eq!(validate_machine_name(" box-2 ").unwrap(), "box-2");
        for bad in ["", "Box", "a_b", "-a", "local", &"a".repeat(33), "a b"] {
            assert!(validate_machine_name(bad).is_err(), "{bad}");
        }
        assert!(parse_machine_url("https://box.tail.ts.net").is_ok());
        for bad in [
            "ftp://x",
            "https://x/hub",
            "https://u:p@x",
            "http://x/?a",
            "x",
        ] {
            assert!(parse_machine_url(bad).is_err(), "{bad}");
        }
        let hex = "ab".repeat(32);
        assert_eq!(parse_fingerprint(&hex.to_uppercase()).unwrap(), hex);
        assert_eq!(parse_fingerprint(&display_fingerprint(&hex)).unwrap(), hex);
        assert_eq!(parse_fingerprint(&format!("sha256:{hex}")).unwrap(), hex);
        assert!(parse_fingerprint("abcd").is_err());
        assert!(display_fingerprint(&hex).starts_with("AB:AB:"));
        // A pin on a plain-http machine pins nothing: refused.
        assert!(Machine::new(record("box", "http://x:1", "c", Some(&hex))).is_err());
    }

    #[test]
    fn the_proxy_only_reaches_the_machine_api() {
        let m = machine("http://127.0.0.1:7601");
        assert_eq!(
            m.target("/api/sessions?workspace_id=a").unwrap().as_str(),
            "http://127.0.0.1:7601/api/sessions?workspace_id=a"
        );
        assert_eq!(
            m.target("/api/sessions/abc/ws").unwrap().host_str(),
            Some("127.0.0.1")
        );
        for bad in [
            "//evil.example/api/x",
            "/api//evil.example/x",
            "@evil.example/api/x",
            "/login",
            "/",
            "/api/../login",
            "/api/%2e%2e/login",
            "/api\\..\\x",
            "/api/x#frag",
            "/api/x y",
            "http://evil.example/api/x",
        ] {
            match m.target(bad) {
                Err(LinkError::Invalid(_)) => {}
                Ok(url) => {
                    // What parses must still be this machine's API.
                    assert_eq!(url.host_str(), Some("127.0.0.1"), "{bad}");
                    assert!(url.path().starts_with("/api/"), "{bad} -> {url}");
                }
                Err(other) => panic!("{bad}: {other}"),
            }
        }
        assert!(m.target("/api/../login").is_err());
    }

    #[test]
    fn the_registry_persists_privately_and_refuses_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let machines = Machines::load(dir.path());
        machines
            .insert(Machine::new(record("b", "http://127.0.0.1:1", "fwpeer_a_b", None)).unwrap())
            .unwrap();
        assert!(matches!(
            machines.insert(Machine::new(record("b", "http://127.0.0.1:2", "c", None)).unwrap()),
            Err(MachineError::Conflict(_))
        ));
        let path = dir.path().join(MACHINES_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let reloaded = Machines::load(dir.path());
        let b = reloaded.get("b").unwrap();
        assert_eq!(b.url(), "http://127.0.0.1:1");
        // The view never carries the credential.
        assert!(!b.view().to_string().contains("fwpeer_a_b"));
        assert!(!format!("{b:?}").contains("fwpeer_a_b"));
        let epoch = b.epoch();
        assert!(reloaded.alive(epoch));
        reloaded.remove("b").unwrap();
        assert!(!reloaded.alive(epoch));
        assert!(Machines::load(dir.path()).list().is_empty());
    }

    /// An answer past the limit is refused, announced (Content-Length) or
    /// streamed (chunked) -- never held whole.
    #[tokio::test]
    async fn answers_past_the_limit_are_refused() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for announced in [true, false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                if announced {
                    let small = request.starts_with(b"GET /small");
                    let body: &[u8] = if small { b"ok" } else { b"" };
                    let length = if small { 2 } else { 1 << 30 };
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {length}\r\n\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
                        .await;
                    let chunk = vec![b'x'; 64 * 1024];
                    let frame = [
                        format!("{:x}\r\n", chunk.len()).into_bytes(),
                        chunk,
                        b"\r\n".to_vec(),
                    ]
                    .concat();
                    // Far past the limit, unless the reader stops.
                    for _ in 0..1024 {
                        if socket.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        crate::install_crypto_provider();
        let http = reqwest::Client::new();
        for path in ["/announced", "/chunked"] {
            let response = http
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            let error = read_capped(response, 1024 * 1024).await.unwrap_err();
            assert!(error.contains("larger than"), "{path}: {error}");
        }
        let response = http
            .get(format!("http://{addr}/small"))
            .send()
            .await
            .unwrap();
        assert_eq!(read_capped(response, 1024).await.unwrap(), b"ok");
    }

    #[test]
    fn a_broken_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MACHINES_FILE), b"not json").unwrap();
        let machines = Machines::load(dir.path());
        assert!(machines.list().is_empty());
        assert!(
            machines
                .insert(Machine::new(record("b", "http://127.0.0.1:1", "c", None)).unwrap())
                .is_err()
        );
        assert_eq!(
            fs::read(dir.path().join(MACHINES_FILE)).unwrap(),
            b"not json"
        );
    }
}
