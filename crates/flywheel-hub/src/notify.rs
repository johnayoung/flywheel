//! Push notifications: when an agent session becomes `blocked` (waits on
//! a person) or `done` (finished a turn), POST a short message to an
//! ntfy-compatible webhook (`https://ntfy.sh/<topic>` or a self-hosted
//! ntfy), so a phone hears about it with the browser closed.
//!
//! What a push says: the session's and workspace's names, the agent kind,
//! and the status detail (the permission or question a blocked agent
//! asks) -- never terminal content, paths, or credentials. The webhook URL
//! (its topic is the secret on ntfy.sh) and token are never logged: only
//! the host is.
//!
//! When: a status spell must last [`NotifyConfig::settle`] (a prompt
//! answered at once never pushes), nobody may be viewing the session (a
//! person looking at it needs no push; once the last viewer looks away a
//! still-unanswered spell pushes), a finished turn must still be unseen,
//! and each spell pushes at most once. One session pushes one status at
//! most once per [`NotifyConfig::debounce`]: a new spell inside the
//! window is held until it ends, then pushes if it still stands.
//!
//! Configured by `fw hub serve --notify-url URL` and/or
//! `<state-dir>/notify.toml`:
//!
//! ```toml
//! url = "https://ntfy.sh/my-secret-topic"
//! token = "tk_..."             # optional: Authorization: Bearer
//! events = ["blocked", "done"] # default both
//! debounce_secs = 30
//! detail = false               # names only: leave out what a blocked agent asks
//! ```

use crate::agent::SessionKind;
use crate::hub::{Hub, HubEvent};
use crate::status::{AgentStatus, Alert};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

/// The config file under the state dir.
pub const CONFIG_FILE: &str = "notify.toml";
/// Default least time between two pushes of one status for one session.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(30);
/// Default time a status must hold before it pushes.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(3);
/// How long one webhook request may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest title / message sent (characters).
const MAX_TITLE_CHARS: usize = 120;
const MAX_MESSAGE_CHARS: usize = 400;

/// Where and when to push.
#[derive(Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    url: url::Url,
    token: Option<String>,
    /// Push when an agent becomes blocked.
    pub blocked: bool,
    /// Push when an agent finishes a turn nobody has seen.
    pub done: bool,
    pub debounce: Duration,
    pub settle: Duration,
    /// Include the status detail (what a blocked agent asks) in pushes.
    pub detail: bool,
    /// Origin a push's click opens (`<origin>/#session=<id>`): the hub's
    /// public URL, when it has one a phone can reach.
    pub click_base: Option<String>,
}

impl std::fmt::Debug for NotifyConfig {
    // The URL's path (an ntfy topic) and the token are secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyConfig")
            .field("host", &self.host())
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("blocked", &self.blocked)
            .field("done", &self.done)
            .field("debounce", &self.debounce)
            .field("settle", &self.settle)
            .field("detail", &self.detail)
            .field("click_base", &self.click_base)
            .finish()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    url: Option<String>,
    token: Option<String>,
    events: Option<Vec<String>>,
    debounce_secs: Option<u64>,
    settle_secs: Option<u64>,
    detail: Option<bool>,
}

impl NotifyConfig {
    /// Push both statuses to `url` with the default timings.
    pub fn new(url: &str) -> Result<Self, String> {
        Ok(Self {
            url: parse_url(url)?,
            token: None,
            blocked: true,
            done: true,
            debounce: DEFAULT_DEBOUNCE,
            settle: DEFAULT_SETTLE,
            detail: true,
            click_base: None,
        })
    }

    /// The config from `<state_dir>/notify.toml` (if any) with `flag_url`
    /// (`--notify-url`) taking the place of its `url`. `None`: no push.
    pub fn load(state_dir: &Path, flag_url: Option<&str>) -> Result<Option<Self>, String> {
        let path = state_dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => {
                check_private(&path)?;
                Some(text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Self::from_parts(text.as_deref(), flag_url).map_err(|e| match text {
            Some(_) => format!("{}: {e}", path.display()),
            None => e,
        })
    }

    /// [`NotifyConfig::load`] on the file's text.
    pub fn from_parts(file: Option<&str>, flag_url: Option<&str>) -> Result<Option<Self>, String> {
        let parsed: FileConfig = match file {
            Some(text) => toml::from_str(text).map_err(|e| e.message().to_owned())?,
            None => FileConfig::default(),
        };
        let Some(url) = flag_url.or(parsed.url.as_deref()) else {
            if file.is_some() {
                return Err("no `url` (and no --notify-url)".into());
            }
            return Ok(None);
        };
        let mut config = Self::new(url)?;
        config.token = match parsed.token {
            Some(token) if token.trim().is_empty() => return Err("`token` is empty".into()),
            Some(token) if token.bytes().any(|b| !(0x21..0x7f).contains(&b)) => {
                return Err("`token` must be printable ASCII without spaces".into());
            }
            other => other,
        };
        if let Some(events) = parsed.events {
            config.blocked = false;
            config.done = false;
            for event in events {
                match event.as_str() {
                    "blocked" => config.blocked = true,
                    "done" => config.done = true,
                    other => {
                        return Err(format!(
                            "unknown event {other:?} in `events` (blocked, done)"
                        ));
                    }
                }
            }
            if !config.blocked && !config.done {
                return Err("`events` is empty: remove the file to turn push off".into());
            }
        }
        if let Some(secs) = parsed.debounce_secs {
            config.debounce = Duration::from_secs(secs);
        }
        if let Some(secs) = parsed.settle_secs {
            config.settle = Duration::from_secs(secs.min(60));
        }
        if let Some(detail) = parsed.detail {
            config.detail = detail;
        }
        Ok(Some(config))
    }

    /// The webhook's host (with a non-default port): what logs and the UI
    /// may show of it.
    pub fn host(&self) -> String {
        let host = self.url.host_str().unwrap_or("?");
        match self.url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    }

    /// The statuses that push.
    pub fn events(&self) -> Vec<&'static str> {
        let mut events = Vec::new();
        if self.blocked {
            events.push("blocked");
        }
        if self.done {
            events.push("done");
        }
        events
    }

    fn enabled_for(&self, status: AgentStatus) -> bool {
        match status {
            AgentStatus::Blocked => self.blocked,
            AgentStatus::Done => self.done,
            _ => false,
        }
    }

    /// Whether `alert` deserves a push now.
    pub fn wants(&self, alert: &Alert) -> bool {
        self.enabled_for(alert.status) && !alert.watched && !alert.seen
    }
}

fn parse_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw.trim()).map_err(|e| format!("notify URL is not a URL ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("notify URL must be http:// or https://".into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("notify URL has no host".into());
    }
    if url.fragment().is_some() {
        return Err("notify URL must not have a #fragment".into());
    }
    Ok(url)
}

/// The URL is a secret (an ntfy topic), and so is a token: the file must
/// be readable by its owner only.
fn check_private(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} is readable by other users (mode {:o}); its URL and token are secrets: chmod 600 it",
                path.display(),
                mode & 0o777
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// A session a push may be about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub session_id: String,
    pub session: String,
    pub workspace: String,
    pub kind: SessionKind,
    pub alert: Alert,
}

/// One webhook message (ntfy's publish-by-headers form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    pub title: String,
    pub message: String,
    /// ntfy priority: `high` or `default`.
    pub priority: &'static str,
    pub tags: String,
    pub click: Option<String>,
}

/// The push for `subject` (`None` unless it is blocked or done).
/// `detail`: say what a blocked agent asks (else names only).
pub fn compose(subject: &Subject, click_base: Option<&str>, detail: bool) -> Option<Push> {
    let session = clean(&subject.session);
    let workspace = clean(&subject.workspace);
    let place = if workspace.is_empty() {
        format!("{session} ({})", subject.kind.as_str())
    } else {
        format!("{workspace} / {session} ({})", subject.kind.as_str())
    };
    let (title, message, priority) = match subject.alert.status {
        AgentStatus::Blocked => (
            format!("{session} is blocked"),
            match subject
                .alert
                .detail
                .as_deref()
                .filter(|_| detail)
                .map(clean)
            {
                Some(detail) if !detail.is_empty() => {
                    format!("{place} waits on you: {detail}")
                }
                _ => format!("{place} waits on you."),
            },
            "high",
        ),
        AgentStatus::Done => (
            format!("{session} is done"),
            format!("{place} finished its turn."),
            "default",
        ),
        _ => return None,
    };
    Some(Push {
        title: bounded(&title, MAX_TITLE_CHARS),
        message: bounded(&message, MAX_MESSAGE_CHARS),
        priority,
        tags: format!("flywheel,{}", subject.alert.status.as_str()),
        click: click_base.map(|base| {
            format!(
                "{}/#session={}",
                base.trim_end_matches('/'),
                subject.session_id
            )
        }),
    })
}

/// Control characters out, whitespace runs to one space.
fn clean(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn bounded(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// A header value: as is when printable ASCII, else RFC 2047 encoded
/// (ntfy decodes `=?UTF-8?B?...?=`).
fn header_text(text: &str) -> String {
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        text.to_owned()
    } else {
        format!(
            "=?UTF-8?B?{}?=",
            crate::commands_cli::base64(text.as_bytes())
        )
    }
}

/// Sends pushes and decides when.
pub struct Notifier {
    config: NotifyConfig,
    /// TLS for an https URL.
    tls: Option<Arc<rustls::ClientConfig>>,
    /// Which spells pushed, which wait to, and when each status last
    /// pushed.
    state: Mutex<PushState>,
}

#[derive(Debug, Default)]
struct PushState {
    /// Last push per (session, status).
    sent: HashMap<(String, AgentStatus), Instant>,
    /// The spell each session last pushed.
    pushed: HashMap<String, u64>,
    /// The spell a settle task is waiting on, per session.
    pending: HashMap<String, u64>,
}

/// What a settle task does next.
enum Next {
    Stop,
    Wait(Duration),
    Send,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Notifier {
    pub fn new(config: NotifyConfig) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            tls: match config.url.scheme() {
                "https" => Some(tls_config()?),
                _ => None,
            },
            config,
            state: Mutex::new(PushState::default()),
        }))
    }

    pub fn config(&self) -> &NotifyConfig {
        &self.config
    }

    /// POST one push. Errors name the host and what failed, never the URL.
    pub async fn send(&self, push: &Push) -> Result<(), String> {
        let mut headers = vec![
            ("Title", header_text(&push.title)),
            ("Priority", push.priority.to_owned()),
            ("Tags", header_text(&push.tags)),
        ];
        if let Some(click) = &push.click {
            headers.push(("Click", header_text(click)));
        }
        if let Some(token) = &self.config.token {
            headers.push(("Authorization", format!("Bearer {token}")));
        } else if let Some(basic) = basic_auth(&self.config.url) {
            headers.push(("Authorization", basic));
        }
        let host = self.config.host();
        let status =
            tokio::time::timeout(SEND_TIMEOUT, self.post(&headers, push.message.as_bytes()))
                .await
                .map_err(|_| format!("{host}: no answer within {}s", SEND_TIMEOUT.as_secs()))?
                .map_err(|e| format!("{host}: {e}"))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("{host} answered HTTP {status}"))
        }
    }

    /// One HTTP/1.1 POST of `body` (plain text) with `headers`; returns
    /// the response's status code. Redirects are not followed (one could
    /// carry the message elsewhere).
    async fn post(&self, headers: &[(&str, String)], body: &[u8]) -> Result<u16, String> {
        let url = &self.config.url;
        let host = url.host_str().ok_or("no host")?;
        let port = url.port_or_known_default().ok_or("no port")?;
        let mut request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: flywheel-hub/{}\r\n\
             Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
            &url[url::Position::BeforePath..url::Position::AfterQuery],
            self.config.host(),
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(body);
        let stream = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|e| format!("cannot connect: {e}"))?;
        match &self.tls {
            None => exchange(stream, &bytes).await,
            Some(tls) => {
                let name = rustls::pki_types::ServerName::try_from(
                    host.trim_matches(['[', ']']).to_owned(),
                )
                .map_err(|e| format!("bad host name: {e}"))?;
                let stream = tokio_rustls::TlsConnector::from(Arc::clone(tls))
                    .connect(name, stream)
                    .await
                    .map_err(|e| format!("TLS: {e}"))?;
                exchange(stream, &bytes).await
            }
        }
    }

    /// A test push (`POST /api/notify/test`).
    pub async fn send_test(&self) -> Result<(), String> {
        self.send(&Push {
            title: "flywheel hub".into(),
            message: "Test notification: blocked and finished agents will reach you here.".into(),
            priority: "default",
            tags: "flywheel,test".into(),
            click: self.config.click_base.clone(),
        })
        .await
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PushState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start waiting on `subject`'s spell unless it already pushed or a
    /// task already waits on it. Returns whether a task should start.
    fn schedule(&self, subject: &Subject) -> bool {
        if !self.config.enabled_for(subject.alert.status) || subject.alert.watched {
            return false;
        }
        let spell = subject.alert.spell;
        let mut state = self.state();
        if state.pushed.get(&subject.session_id) == Some(&spell)
            || state.pending.get(&subject.session_id) == Some(&spell)
        {
            return false;
        }
        state.pending.insert(subject.session_id.clone(), spell);
        true
    }

    /// Decide, for the settle task waiting on `spell` of `session_id`,
    /// with `now` the session's current facts. Reads and decides under
    /// one lock, so a viewer leaving between the two cannot be missed:
    /// either this sees the viewer gone, or its `pending` mark is gone
    /// by the time the leave event is handled.
    fn decide(
        &self,
        session_id: &str,
        spell: u64,
        read: impl FnOnce() -> Option<Subject>,
        at: Instant,
    ) -> (Next, Option<Subject>) {
        let mut state = self.state();
        let now = read();
        let next = self.decide_locked(&mut state, session_id, spell, now.as_ref(), at);
        (next, now)
    }

    fn decide_locked(
        &self,
        state: &mut PushState,
        session_id: &str,
        spell: u64,
        now: Option<&Subject>,
        at: Instant,
    ) -> Next {
        let stop = |state: &mut PushState| {
            if state.pending.get(session_id) == Some(&spell) {
                state.pending.remove(session_id);
            }
            Next::Stop
        };
        let Some(now) = now else {
            state.pushed.remove(session_id);
            return stop(state);
        };
        if now.alert.spell != spell
            || !self.config.wants(&now.alert)
            || state.pending.get(session_id) != Some(&spell)
            || state.pushed.get(session_id) == Some(&spell)
        {
            return stop(state);
        }
        let debounce = self.config.debounce;
        state
            .sent
            .retain(|_, sent| at.duration_since(*sent) < debounce);
        let key = (session_id.to_owned(), now.alert.status);
        if let Some(sent) = state.sent.get(&key) {
            return Next::Wait(debounce.saturating_sub(at.duration_since(*sent)));
        }
        state.sent.insert(key, at);
        state.pushed.insert(session_id.to_owned(), spell);
        state.pending.remove(session_id);
        Next::Send
    }

    /// Follow `hub`'s status changes and push (on the current tokio
    /// runtime) until the hub goes away.
    pub fn watch(self: &Arc<Self>, hub: &Arc<Hub>) -> tokio::task::JoinHandle<()> {
        let mut events = hub.subscribe();
        let weak = Arc::downgrade(hub);
        let notifier = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let session_id = match events.recv().await {
                    Ok(HubEvent::Status { session_id } | HubEvent::Unwatched { session_id }) => {
                        session_id
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                let Some(hub) = weak.upgrade() else { break };
                let subject = hub.alert_subject(&session_id);
                drop(hub);
                let Some(subject) = subject else {
                    let mut state = notifier.state();
                    state.pushed.remove(&session_id);
                    state.pending.remove(&session_id);
                    continue;
                };
                if !notifier.schedule(&subject) {
                    continue;
                }
                let spell = subject.alert.spell;
                let notifier = Arc::clone(&notifier);
                let weak = weak.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(notifier.config.settle).await;
                    loop {
                        let Some(hub) = weak.upgrade() else { return };
                        let (next, now) = notifier.decide(
                            &session_id,
                            spell,
                            || hub.alert_subject(&session_id),
                            Instant::now(),
                        );
                        drop(hub);
                        match next {
                            Next::Stop => return,
                            Next::Wait(wait) => tokio::time::sleep(wait).await,
                            Next::Send => {
                                let Some(push) = now.as_ref().and_then(|now| {
                                    compose(
                                        now,
                                        notifier.config.click_base.as_deref(),
                                        notifier.config.detail,
                                    )
                                }) else {
                                    return;
                                };
                                if let Err(e) = notifier.send(&push).await {
                                    eprintln!("fw hub: push notification failed: {e}");
                                }
                                return;
                            }
                        }
                    }
                });
            }
        })
    }
}

/// Write `request`, read the status line back.
async fn exchange<S>(mut stream: S, request: &[u8]) -> Result<u16, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(request)
        .await
        .map_err(|e| format!("cannot send: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("cannot send: {e}"))?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(2).any(|w| w == b"\r\n") {
        if head.len() > 8192 {
            return Err("answer is not HTTP".into());
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("no answer: {e}"))?;
        if n == 0 {
            return Err("closed without an answer".into());
        }
        head.extend_from_slice(&chunk[..n]);
    }
    status_code(&head).ok_or_else(|| "answer is not HTTP".into())
}

/// The status code of an HTTP/1.x response's first line.
fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end();
    let mut parts = line.split(' ');
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let code = parts.next()?;
    (code.len() == 3).then(|| code.parse().ok()).flatten()
}

/// `Authorization: Basic` for credentials in the URL (`https://user:pass@host/topic`).
fn basic_auth(url: &url::Url) -> Option<String> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    // Userinfo is percent-encoded only: '+', '&', and '=' are literal.
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    let pair = format!(
        "{}:{}",
        decode(url.username()),
        decode(url.password().unwrap_or(""))
    );
    Some(format!(
        "Basic {}",
        crate::commands_cli::base64(pair.as_bytes())
    ))
}

/// TLS for the webhook: rustls on ring, the platform's trusted roots.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(
            "no trusted root certificates found on this system for an https notify URL".into(),
        );
    }
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(status: AgentStatus, detail: Option<&str>) -> Subject {
        Subject {
            session_id: "abc123".into(),
            session: "claude 1".into(),
            workspace: "infrared".into(),
            kind: SessionKind::Claude,
            alert: Alert {
                status,
                detail: detail.map(str::to_owned),
                spell: 1,
                watched: false,
                seen: false,
            },
        }
    }

    #[test]
    fn config_from_flag_file_or_both() {
        assert_eq!(NotifyConfig::from_parts(None, None).unwrap(), None);
        let flag = NotifyConfig::from_parts(None, Some("https://ntfy.sh/topic"))
            .unwrap()
            .unwrap();
        assert!(flag.blocked && flag.done);
        assert_eq!(flag.debounce, DEFAULT_DEBOUNCE);
        assert_eq!(flag.host(), "ntfy.sh");
        let file = "url = \"http://127.0.0.1:8080/hub\"\ntoken = \"tk_abc\"\nevents = [\"blocked\"]\ndebounce_secs = 5\nsettle_secs = 1\n";
        let both = NotifyConfig::from_parts(Some(file), Some("https://push.example.com/t"))
            .unwrap()
            .unwrap();
        assert_eq!(both.host(), "push.example.com");
        assert_eq!(both.token.as_deref(), Some("tk_abc"));
        assert!(both.blocked && !both.done);
        assert_eq!(both.debounce, Duration::from_secs(5));
        assert_eq!(both.settle, Duration::from_secs(1));
        assert!(both.detail, "detail is on by default");
        let quiet = NotifyConfig::from_parts(Some("url = \"https://a/b\"\ndetail = false\n"), None)
            .unwrap()
            .unwrap();
        assert!(!quiet.detail);
        let only_file = NotifyConfig::from_parts(Some(file), None).unwrap().unwrap();
        assert_eq!(only_file.host(), "127.0.0.1:8080");
    }

    #[test]
    fn bad_configs_are_refused() {
        for (file, flag) in [
            (None, Some("ftp://ntfy.sh/t")),
            (None, Some("not a url")),
            (None, Some("https://ntfy.sh/t#x")),
            (Some("url = 3"), None),
            (Some("token = \"x\""), None),
            (Some("url = \"https://a/b\"\nevents = [\"working\"]"), None),
            (Some("url = \"https://a/b\"\nevents = []"), None),
            (Some("url = \"https://a/b\"\ntoken = \"a b\""), None),
            (Some("url = \"https://a/b\"\nsurprise = 1"), None),
        ] {
            assert!(
                NotifyConfig::from_parts(file, flag).is_err(),
                "{file:?} {flag:?}"
            );
        }
    }

    #[test]
    fn debug_output_hides_the_topic_and_token() {
        let config = NotifyConfig::from_parts(
            Some("url = \"https://ntfy.sh/s3cret-topic\"\ntoken = \"tk_hidden\""),
            None,
        )
        .unwrap()
        .unwrap();
        let shown = format!("{config:?}");
        assert!(shown.contains("ntfy.sh"));
        assert!(
            !shown.contains("s3cret") && !shown.contains("tk_hidden"),
            "{shown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_config_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "url = \"https://ntfy.sh/t\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = NotifyConfig::load(dir.path(), None).unwrap_err();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(NotifyConfig::load(dir.path(), None).unwrap().is_some());
        // No file, no flag: push is off.
        assert!(
            NotifyConfig::load(&dir.path().join("none"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pushes_name_the_session_and_say_only_the_detail() {
        let blocked = compose(
            &subject(AgentStatus::Blocked, Some("permission to use Bash")),
            Some("https://box.tail.ts.net/"),
            true,
        )
        .unwrap();
        assert_eq!(blocked.title, "claude 1 is blocked");
        assert_eq!(
            blocked.message,
            "infrared / claude 1 (claude) waits on you: permission to use Bash"
        );
        assert_eq!(blocked.priority, "high");
        assert_eq!(blocked.tags, "flywheel,blocked");
        assert_eq!(
            blocked.click.as_deref(),
            Some("https://box.tail.ts.net/#session=abc123")
        );
        let done = compose(&subject(AgentStatus::Done, None), None, true).unwrap();
        assert_eq!(done.title, "claude 1 is done");
        assert_eq!(
            done.message,
            "infrared / claude 1 (claude) finished its turn."
        );
        assert_eq!(done.click, None);
        assert!(compose(&subject(AgentStatus::Working, None), None, true).is_none());
        assert!(compose(&subject(AgentStatus::Idle, None), None, true).is_none());
        // Control characters cannot forge lines or headers.
        let mut evil = subject(AgentStatus::Blocked, Some("a\r\nTitle: x\x1b[2J"));
        evil.session = "s\nx".into();
        let push = compose(&evil, None, true).unwrap();
        assert!(!push.title.contains('\n') && !push.message.contains('\r'));
        assert!(!push.message.contains('\x1b'));
        // `detail = false`: names only.
        let quiet = compose(
            &subject(AgentStatus::Blocked, Some("Deploy with key sk-live-1?")),
            None,
            false,
        )
        .unwrap();
        assert_eq!(quiet.message, "infrared / claude 1 (claude) waits on you.");
    }

    #[test]
    fn wants_skips_watched_seen_and_disabled_statuses() {
        let mut config = NotifyConfig::new("https://ntfy.sh/t").unwrap();
        let mut alert = subject(AgentStatus::Blocked, None).alert;
        assert!(config.wants(&alert));
        alert.watched = true;
        assert!(!config.wants(&alert));
        let mut done = subject(AgentStatus::Done, None).alert;
        assert!(config.wants(&done));
        done.seen = true;
        assert!(!config.wants(&done));
        config.blocked = false;
        assert!(!config.wants(&subject(AgentStatus::Blocked, None).alert));
        assert!(!config.wants(&subject(AgentStatus::Working, None).alert));
    }

    #[test]
    fn non_ascii_headers_are_rfc2047_encoded() {
        assert_eq!(header_text("plain title"), "plain title");
        assert_eq!(header_text("caf\u{e9}"), "=?UTF-8?B?Y2Fmw6k=?=");
    }

    fn decide(notifier: &Notifier, subject: &Subject, at: Instant) -> Next {
        let mut state = notifier.state();
        notifier.decide_locked(
            &mut state,
            &subject.session_id,
            subject.alert.spell,
            Some(subject),
            at,
        )
    }

    #[test]
    fn each_spell_pushes_once_and_a_debounced_one_waits_for_the_window() {
        let mut config = NotifyConfig::new("http://127.0.0.1:9/t").unwrap();
        config.debounce = Duration::from_secs(30);
        let notifier = Notifier::new(config).unwrap();
        let t0 = Instant::now();
        let mut blocked = subject(AgentStatus::Blocked, None);
        assert!(notifier.schedule(&blocked));
        assert!(!notifier.schedule(&blocked), "one task per spell");
        assert!(matches!(decide(&notifier, &blocked, t0), Next::Send));
        assert!(
            !notifier.schedule(&blocked),
            "a pushed spell never pushes again"
        );

        // Answered, then blocked again 8s later: a new spell, held until
        // the window ends, then sent.
        blocked.alert.spell = 3;
        assert!(notifier.schedule(&blocked));
        let at = t0 + Duration::from_secs(8);
        match decide(&notifier, &blocked, at) {
            Next::Wait(wait) => assert_eq!(wait, Duration::from_secs(22)),
            _ => panic!("expected a wait"),
        }
        assert!(matches!(
            decide(&notifier, &blocked, t0 + Duration::from_secs(30)),
            Next::Send
        ));

        // Another session or status is not held.
        let mut other = subject(AgentStatus::Blocked, None);
        other.session_id = "other".into();
        assert!(notifier.schedule(&other));
        assert!(matches!(decide(&notifier, &other, at), Next::Send));
        let mut done = subject(AgentStatus::Done, None);
        done.alert.spell = 4;
        assert!(notifier.schedule(&done));
        assert!(matches!(decide(&notifier, &done, at), Next::Send));
    }

    #[test]
    fn a_watched_spell_gives_up_and_can_be_scheduled_again_once_unwatched() {
        let notifier = Notifier::new(NotifyConfig::new("http://127.0.0.1:9/t").unwrap()).unwrap();
        let mut blocked = subject(AgentStatus::Blocked, None);
        assert!(notifier.schedule(&blocked));
        blocked.alert.watched = true;
        assert!(matches!(
            decide(&notifier, &blocked, Instant::now()),
            Next::Stop
        ));
        assert!(!notifier.schedule(&blocked), "still watched");
        blocked.alert.watched = false;
        assert!(
            notifier.schedule(&blocked),
            "the viewer left: same spell again"
        );
        assert!(matches!(
            decide(&notifier, &blocked, Instant::now()),
            Next::Send
        ));
    }

    #[test]
    fn status_lines_parse_and_url_credentials_become_basic_auth() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(status_code(b"HTTP/1.0 403 Forbidden\r\nx"), Some(403));
        assert_eq!(status_code(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(status_code(b"HTTP/1.1 20 OK\r\n"), None);
        let url = url::Url::parse("https://me:p%40ss@ntfy.example.com/t").unwrap();
        assert_eq!(
            basic_auth(&url).as_deref(),
            Some(format!("Basic {}", crate::commands_cli::base64(b"me:p@ss")).as_str())
        );
        assert_eq!(
            basic_auth(&url::Url::parse("https://ntfy.sh/t").unwrap()),
            None
        );
        // '&', '=' and '+' are literal in userinfo.
        let url = url::Url::parse("https://me:a&b=c%2Bd+e@ntfy.example.com/t").unwrap();
        assert_eq!(
            basic_auth(&url).as_deref(),
            Some(format!("Basic {}", crate::commands_cli::base64(b"me:a&b=c+d+e")).as_str())
        );
    }
}
