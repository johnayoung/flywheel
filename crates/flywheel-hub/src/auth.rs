//! Hub authentication: the state-dir token, the login cookie, and the
//! same-origin check.
//!
//! A hub serves shells over HTTP -- remote code execution by design -- so
//! every route except the login page requires the token. The token is 256
//! random bits, hex-encoded, stored `0600` in the state dir, and compared in
//! constant time. Browsers trade it once (`/login?token=...`) -- or a
//! short-lived single-use [`LoginCodes`] code (`/login?code=...`), so the
//! long-lived token never has to be shown on a screen or in a QR code --
//! for an `HttpOnly; SameSite=Strict` cookie carrying a per-login secret
//! ([`Logins`]; never the token itself); scripts send
//! `Authorization: Bearer`. Failed credentials are rate-limited per client
//! address ([`FailureLimiter`]).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

/// Token file name inside the state dir.
pub const TOKEN_FILE: &str = "token";
/// The login cookie's name prefix; see [`cookie_name`].
pub const COOKIE_PREFIX: &str = "fw_hub_";
/// Login cookie lifetime: 30 days (rotating the token invalidates it sooner).
pub const COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
/// Random bytes in a token (256 bits).
const TOKEN_BYTES: usize = 32;
/// The browser logins file inside the state dir (digests only).
pub const LOGINS_FILE: &str = "logins.json";
/// Browser logins kept at once; issuing beyond this drops the one closest
/// to expiry.
pub const MAX_LOGINS: usize = 256;

pub(crate) fn random_hex(bytes: usize) -> Result<String, TokenError> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| TokenError::Random(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Token file failures.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("token file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("token file {path} is malformed (expected {expected} hex characters)")]
    Malformed { path: PathBuf, expected: usize },
    #[error("system random source failed: {0}")]
    Random(String),
}

/// The hub's shared secret.
#[derive(Clone)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// A fresh random token.
    pub fn generate() -> Result<Self, TokenError> {
        random_hex(TOKEN_BYTES).map(Self)
    }

    /// A secret handed over by another hub process (a live handoff).
    pub(crate) fn from_secret(secret: String) -> Self {
        Self(secret)
    }

    /// The token text (for the login URL and `fw hub token`).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented credential.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        // Token length is public (fixed); only content must not leak.
        expected.len() == presented.len() && bool::from(expected.ct_eq(presented))
    }

    /// Load the token from `state_dir`, creating it (mode 0600) if absent.
    pub fn load_or_create(state_dir: &Path) -> Result<Self, TokenError> {
        match Self::load(state_dir)? {
            Some(token) => Ok(token),
            None => Self::rotate(state_dir),
        }
    }

    /// Load the token from `state_dir`; `None` when there is none yet.
    pub fn load(state_dir: &Path) -> Result<Option<Self>, TokenError> {
        let path = state_dir.join(TOKEN_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let text = text.trim();
                let expected = TOKEN_BYTES * 2;
                if text.len() != expected || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(TokenError::Malformed { path, expected });
                }
                Ok(Some(Self(text.to_ascii_lowercase())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(TokenError::Io { path, source }),
        }
    }

    /// Generate a fresh token and store it (mode 0600) in `state_dir`,
    /// replacing any previous one.
    pub fn rotate(state_dir: &Path) -> Result<Self, TokenError> {
        let path = state_dir.join(TOKEN_FILE);
        let token = Self::generate()?;
        write_private(&path, format!("{}\n", token.0).as_bytes())
            .map_err(|source| TokenError::Io { path, source })?;
        Ok(token)
    }
}

/// Browser logins: each successful sign-in gets its own 256-bit secret,
/// which is what the login cookie carries -- so a phone that scanned a QR
/// code holds a revocable login, not the long-lived token. Only SHA-256
/// digests are kept, persisted (0600) in [`LOGINS_FILE`] so logins survive
/// a hub restart. Rotating the token forgets every login; logging out
/// forgets that one.
#[derive(Debug, Default)]
pub struct Logins {
    entries: Vec<LoginEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LoginEntry {
    /// Hex SHA-256 of the secret.
    digest: String,
    /// Unix seconds.
    expires: u64,
}

#[derive(Serialize, Deserialize)]
struct LoginsFile {
    logins: Vec<LoginEntry>,
}

fn login_digest(secret: &str) -> String {
    Sha256::new()
        .chain_update(b"flywheel-hub-login\0")
        .chain_update(secret.as_bytes())
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Logins {
    /// The logins recorded in `state_dir`. A missing file is no logins; an
    /// unreadable or malformed one too (everyone signs in again) -- it is
    /// a cache of sessions, never a reason to refuse to start.
    pub fn load(state_dir: &Path, now: u64) -> Self {
        let entries = fs::read(state_dir.join(LOGINS_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LoginsFile>(&bytes).ok())
            .map(|file| file.logins)
            .unwrap_or_default();
        let mut logins = Self { entries };
        logins.prune(now);
        logins
    }

    /// Persist to `state_dir` (0600, atomic).
    pub fn save(&self, state_dir: &Path) -> Result<(), TokenError> {
        let path = state_dir.join(LOGINS_FILE);
        let file = LoginsFile {
            logins: self.entries.clone(),
        };
        let json = serde_json::to_vec(&file).map_err(|e| TokenError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        write_private(&path, &json).map_err(|source| TokenError::Io { path, source })
    }

    /// A fresh login secret valid for [`COOKIE_MAX_AGE_SECS`].
    pub fn issue(&mut self, now: u64) -> Result<String, TokenError> {
        let secret = random_hex(TOKEN_BYTES)?;
        self.prune(now);
        if self.entries.len() >= MAX_LOGINS
            && let Some(soonest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.expires)
                .map(|(i, _)| i)
        {
            self.entries.remove(soonest);
        }
        self.entries.push(LoginEntry {
            digest: login_digest(&secret),
            expires: now.saturating_add(COOKIE_MAX_AGE_SECS),
        });
        Ok(secret)
    }

    /// Whether `secret` is a live login.
    pub fn valid(&self, secret: &str, now: u64) -> bool {
        let digest = login_digest(secret);
        self.entries
            .iter()
            .any(|e| e.expires > now && bool::from(e.digest.as_bytes().ct_eq(digest.as_bytes())))
    }

    /// Forget `secret`; true when it was a login.
    pub fn revoke(&mut self, secret: &str) -> bool {
        let digest = login_digest(secret);
        let before = self.entries.len();
        self.entries.retain(|e| e.digest != digest);
        self.entries.len() != before
    }

    /// Forget every login (the token was rotated).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Logins still live.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No live logins.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn prune(&mut self, now: u64) {
        self.entries.retain(|e| e.expires > now);
    }
}

/// How long a one-time login code stays redeemable.
pub const LOGIN_CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Characters in a login code: 20 x 5 bits = 100 random bits.
const LOGIN_CODE_LEN: usize = 20;
/// Unredeemed codes kept at once; minting beyond this drops the oldest.
const MAX_LOGIN_CODES: usize = 64;
/// Crockford base32: no `i l o u`, so a code read aloud or typed from a
/// screen survives.
const CODE_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Short-lived, single-use login codes: what `fw hub url` and the QR code
/// carry instead of the token. Only digests are kept.
#[derive(Debug, Default)]
pub struct LoginCodes {
    codes: Vec<([u8; 32], Instant)>,
}

fn code_digest(code: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"flywheel-hub-login-code\0")
        .chain_update(code.as_bytes())
        .finalize()
        .into()
}

/// A presented code in canonical form: lowercase, group separators and
/// spaces dropped, look-alikes (`i l` -> `1`, `o` -> `0`) folded.
fn canonical_code(presented: &str) -> String {
    presented
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_lowercase() {
            'i' | 'l' => '1',
            'o' => '0',
            other => other,
        })
        .collect()
}

impl LoginCodes {
    /// Mint a code redeemable once until `now + LOGIN_CODE_TTL`.
    pub fn mint(&mut self, now: Instant) -> Result<String, TokenError> {
        let mut bytes = [0u8; LOGIN_CODE_LEN];
        getrandom::fill(&mut bytes).map_err(|e| TokenError::Random(e.to_string()))?;
        // 256 is a multiple of 32: masking keeps every character uniform.
        let code: String = bytes
            .iter()
            .map(|b| char::from(CODE_ALPHABET[usize::from(b & 31)]))
            .collect();
        self.prune(now);
        if self.codes.len() >= MAX_LOGIN_CODES {
            self.codes.remove(0);
        }
        self.codes.push((code_digest(&code), now + LOGIN_CODE_TTL));
        Ok(code)
    }

    /// Redeem `presented`: true (and the code is spent) when it is a live
    /// code.
    pub fn redeem(&mut self, presented: &str, now: Instant) -> bool {
        self.prune(now);
        let canonical = canonical_code(presented);
        if canonical.len() != LOGIN_CODE_LEN {
            return false;
        }
        let digest = code_digest(&canonical);
        let found = self
            .codes
            .iter()
            .position(|(d, _)| bool::from(d.ct_eq(&digest)));
        match found {
            Some(i) => {
                self.codes.remove(i);
                true
            }
            None => false,
        }
    }

    /// Forget every outstanding code (the token was rotated).
    pub fn clear(&mut self) {
        self.codes.clear();
    }

    /// Codes still redeemable.
    pub fn outstanding(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.codes.len()
    }

    fn prune(&mut self, now: Instant) {
        self.codes.retain(|(_, expires)| *expires > now);
    }
}

/// Failed credentials one client may present within [`FAILURE_WINDOW`]
/// before it is refused outright.
pub const MAX_FAILURES: usize = 10;
/// The sliding window [`MAX_FAILURES`] counts over.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Clients tracked at once (bounds memory under an address-spraying
/// attacker; the least recently failing one is forgotten first).
const MAX_TRACKED_CLIENTS: usize = 4096;

/// Per-client rate limit on failed credentials (a wrong token, code,
/// cookie, or bearer). A client at the limit is refused -- even with a
/// valid token or code, or guessing could go on -- until its oldest failure
/// leaves the window (the server still honours live login cookies). IPv6 clients are keyed by their /64, which one host
/// usually owns whole.
#[derive(Debug, Default)]
pub struct FailureLimiter {
    clients: HashMap<IpAddr, VecDeque<Instant>>,
}

fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut segments = v6.segments();
                segments[4..].fill(0);
                IpAddr::V6(segments.into())
            }
        },
        v4 => v4,
    }
}

impl FailureLimiter {
    /// `Some(wait)` while `ip` is locked out.
    pub fn locked(&mut self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let key = client_key(ip);
        let failures = self.clients.get_mut(&key)?;
        while failures
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        if failures.is_empty() {
            self.clients.remove(&key);
            return None;
        }
        (failures.len() >= MAX_FAILURES).then(|| {
            let oldest = failures.front().copied().unwrap_or(now);
            (oldest + FAILURE_WINDOW).saturating_duration_since(now)
        })
    }

    /// Record a failed credential from `ip`.
    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        let key = client_key(ip);
        if !self.clients.contains_key(&key) && self.clients.len() >= MAX_TRACKED_CLIENTS {
            self.clients.retain(|_, f| {
                f.back()
                    .is_some_and(|t| now.saturating_duration_since(*t) < FAILURE_WINDOW)
            });
            if self.clients.len() >= MAX_TRACKED_CLIENTS {
                let stalest = self
                    .clients
                    .iter()
                    .min_by_key(|(_, f)| f.back().copied())
                    .map(|(ip, _)| *ip);
                if let Some(stalest) = stalest {
                    self.clients.remove(&stalest);
                }
            }
        }
        let failures = self.clients.entry(key).or_default();
        failures.push_back(now);
        while failures.len() > MAX_FAILURES {
            failures.pop_front();
        }
    }

    /// Clients currently tracked.
    pub fn tracked(&self) -> usize {
        self.clients.len()
    }
}

/// Write `bytes` to `path` atomically with mode 0600 (temp file + rename).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            // A pre-existing temp file keeps its old mode; force 0600.
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// The login cookie's name for a hub with `token`: [`COOKIE_PREFIX`] plus a
/// digest of the token. Browsers do not separate cookies by port, so a
/// fixed name would let two hubs on one host (or one restarted with a new
/// token) overwrite each other's login.
pub fn cookie_name(token: &str) -> String {
    let digest = Sha256::new()
        .chain_update(b"flywheel-hub-cookie-name\0")
        .chain_update(token.as_bytes())
        .finalize();
    let suffix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("{COOKIE_PREFIX}{suffix}")
}

/// The login cookie value from a `Cookie` header, if present.
pub fn cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value.trim().trim_matches('"'))
    })
}

/// The bearer credential from an `Authorization` header, if present.
pub fn bearer_value(authorization: &str) -> Option<&str> {
    let (scheme, value) = authorization.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(value.trim())
}

/// The `Set-Cookie` value that logs a browser in: the cookie named after
/// `token`, carrying the login `secret` from [`Logins::issue`].
pub fn login_cookie(token: &Token, secret: &str, secure: bool) -> String {
    format!(
        "{}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE_SECS}{}",
        cookie_name(token.expose()),
        if secure { "; Secure" } else { "" }
    )
}

/// The `Set-Cookie` value that logs a browser out.
pub fn logout_cookie(token: &Token) -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        cookie_name(token.expose())
    )
}

/// Whether a request's `Origin` is acceptable for a state-changing or
/// WebSocket request.
///
/// * no `Origin` header: a non-browser client (curl, the CLI); allowed --
///   the credential check still applies;
/// * `Origin` whose `host[:port]` equals one of `hosts` (the request's
///   `Host`, and the `X-Forwarded-Host` a local reverse proxy set):
///   same-origin;
/// * `Origin` listed verbatim in `allowed` (e.g. a reverse proxy's public
///   origin): allowed;
/// * anything else, including the opaque `null` origin: rejected.
pub fn origin_allowed(origin: Option<&str>, hosts: &[&str], allowed: &[String]) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let origin = origin.trim().trim_end_matches('/');
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = normalize_authority(authority, scheme);
    hosts
        .iter()
        .any(|host| normalize_authority(host.trim(), scheme) == authority)
}

/// Lowercase an authority and drop the scheme's default port.
fn normalize_authority(authority: &str, scheme: &str) -> String {
    let lower = authority.to_ascii_lowercase();
    let default = if scheme.eq_ignore_ascii_case("https") {
        ":443"
    } else {
        ":80"
    };
    lower
        .strip_suffix(default)
        .map(str::to_owned)
        .unwrap_or(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_256_bit_hex_and_distinct() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.expose(), b.expose());
        assert_eq!(format!("{a:?}"), "Token(<redacted>)");
    }

    #[test]
    fn matches_is_exact() {
        let t = Token::generate().unwrap();
        assert!(t.matches(t.expose()));
        assert!(!t.matches(""));
        assert!(!t.matches(&t.expose()[..63]));
        let mut flipped = t.expose().to_owned();
        let last = if flipped.ends_with('0') { "1" } else { "0" };
        flipped.replace_range(63.., last);
        assert!(!t.matches(&flipped));
    }

    #[test]
    fn token_file_is_created_private_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let created = Token::load_or_create(dir.path()).unwrap();
        let reloaded = Token::load_or_create(dir.path()).unwrap();
        assert_eq!(created.expose(), reloaded.expose());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn malformed_token_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(TOKEN_FILE), "short\n").unwrap();
        assert!(matches!(
            Token::load_or_create(dir.path()),
            Err(TokenError::Malformed { .. })
        ));
    }

    #[test]
    fn parses_cookie_and_bearer_headers() {
        assert_eq!(
            cookie_value("a=1; fw_hub_x=abc; b=2", "fw_hub_x"),
            Some("abc")
        );
        assert_eq!(cookie_value("fw_hub_xy=abc", "fw_hub_x"), None);
        assert_eq!(bearer_value("Bearer xyz"), Some("xyz"));
        assert_eq!(bearer_value("bearer  xyz "), Some("xyz"));
        assert_eq!(bearer_value("Basic xyz"), None);
    }

    #[test]
    fn cookie_names_differ_per_token() {
        let (a, b) = (Token::generate().unwrap(), Token::generate().unwrap());
        let name = cookie_name(a.expose());
        assert!(name.starts_with(COOKIE_PREFIX) && name.len() == COOKIE_PREFIX.len() + 12);
        assert_eq!(name, cookie_name(a.expose()));
        assert_ne!(name, cookie_name(b.expose()));
        assert!(
            !name.contains(&a.expose()[..12]),
            "the name must not reveal the token"
        );
        assert!(logout_cookie(&a).starts_with(&format!("{name}=;")));
    }

    #[test]
    fn login_cookie_is_http_only_and_strict() {
        let t = Token::generate().unwrap();
        let c = login_cookie(&t, "s3cret", false);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict"));
        assert!(c.starts_with(&format!("{}=s3cret;", cookie_name(t.expose()))));
        assert!(
            !c.contains(t.expose()),
            "the cookie never carries the token"
        );
        assert!(!c.contains("Secure"));
        assert!(login_cookie(&t, "s3cret", true).ends_with("; Secure"));
        assert!(logout_cookie(&t).contains("Max-Age=0"));
    }

    #[test]
    fn origin_rules() {
        let host: &[&str] = &["127.0.0.1:7420"];
        assert!(origin_allowed(None, host, &[]));
        assert!(origin_allowed(Some("http://127.0.0.1:7420"), host, &[]));
        assert!(origin_allowed(Some("HTTP://127.0.0.1:7420/"), host, &[]));
        assert!(!origin_allowed(Some("http://evil.example"), host, &[]));
        assert!(!origin_allowed(Some("http://127.0.0.1:9999"), host, &[]));
        assert!(!origin_allowed(Some("null"), host, &[]));
        assert!(!origin_allowed(Some("file://127.0.0.1:7420"), host, &[]));
        assert!(!origin_allowed(Some("http://127.0.0.1:7420"), &[], &[]));
        assert!(origin_allowed(
            Some("https://box.ts.net"),
            &["box.ts.net:443"],
            &[]
        ));
        // A proxy's forwarded host counts as a second same-origin host.
        assert!(origin_allowed(
            Some("https://box.ts.net"),
            &["127.0.0.1:7420", "box.ts.net"],
            &[]
        ));
        let allow = vec!["https://hub.example.com".to_owned()];
        assert!(origin_allowed(
            Some("https://hub.example.com"),
            host,
            &allow
        ));
    }

    #[test]
    fn load_and_rotate() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Token::load(dir.path()).unwrap().is_none());
        let first = Token::load_or_create(dir.path()).unwrap();
        let rotated = Token::rotate(dir.path()).unwrap();
        assert_ne!(first.expose(), rotated.expose());
        let loaded = Token::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.expose(), rotated.expose());
    }

    #[test]
    fn logins_are_per_sign_in_persisted_and_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000;
        let mut logins = Logins::load(dir.path(), now);
        assert!(logins.is_empty());
        let a = logins.issue(now).unwrap();
        let b = logins.issue(now).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(logins.valid(&a, now) && logins.valid(&b, now));
        assert!(!logins.valid("", now) && !logins.valid(&a[..63], now));
        logins.save(dir.path()).unwrap();
        let text = fs::read_to_string(dir.path().join(LOGINS_FILE)).unwrap();
        assert!(!text.contains(&a) && !text.contains(&b), "digests only");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(LOGINS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let mut reloaded = Logins::load(dir.path(), now);
        assert!(reloaded.valid(&a, now), "survives a restart");
        assert!(reloaded.revoke(&a));
        assert!(!reloaded.revoke(&a));
        assert!(!reloaded.valid(&a, now) && reloaded.valid(&b, now));
        let expiry = now + COOKIE_MAX_AGE_SECS;
        assert!(!reloaded.valid(&b, expiry), "expired");
        assert_eq!(Logins::load(dir.path(), expiry).len(), 0);
        reloaded.clear();
        assert!(!reloaded.valid(&b, now));
        fs::write(dir.path().join(LOGINS_FILE), "{").unwrap();
        assert!(Logins::load(dir.path(), now).is_empty(), "malformed: none");
    }

    #[test]
    fn logins_are_bounded() {
        let mut logins = Logins::default();
        let first = logins.issue(10).unwrap();
        for i in 0..MAX_LOGINS {
            logins.issue(11 + i as u64).unwrap();
        }
        assert_eq!(logins.len(), MAX_LOGINS);
        assert!(
            !logins.valid(&first, 20),
            "the soonest to expire was dropped"
        );
    }

    #[test]
    fn login_codes_are_single_use_and_expire() {
        let mut codes = LoginCodes::default();
        let t0 = Instant::now();
        let code = codes.mint(t0).unwrap();
        assert_eq!(code.len(), LOGIN_CODE_LEN);
        assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_ne!(code, codes.mint(t0).unwrap());
        assert_eq!(codes.outstanding(t0), 2);
        assert!(!codes.redeem("", t0));
        assert!(!codes.redeem(&code[..19], t0));
        // Case, separators, and look-alikes fold.
        let mut typed = String::new();
        for (i, c) in code.to_ascii_uppercase().chars().enumerate() {
            if i > 0 && i % 5 == 0 {
                typed.push('-');
            }
            typed.push(c);
        }
        assert!(codes.redeem(&typed, t0), "{typed}");
        assert!(!codes.redeem(&code, t0), "single use");
        let late = codes.mint(t0).unwrap();
        assert!(!codes.redeem(&late, t0 + LOGIN_CODE_TTL), "expired");
        assert_eq!(codes.outstanding(t0 + LOGIN_CODE_TTL), 0);
        let cleared = codes.mint(t0).unwrap();
        codes.clear();
        assert!(!codes.redeem(&cleared, t0));
        assert_eq!(canonical_code("IlO0-ab"), "1100ab");
    }

    #[test]
    fn login_codes_are_bounded() {
        let mut codes = LoginCodes::default();
        let t0 = Instant::now();
        let first = codes.mint(t0).unwrap();
        for _ in 0..MAX_LOGIN_CODES {
            codes.mint(t0).unwrap();
        }
        assert_eq!(codes.outstanding(t0), MAX_LOGIN_CODES);
        assert!(!codes.redeem(&first, t0), "the oldest was dropped");
    }

    #[test]
    fn failure_limiter_locks_out_then_recovers() {
        let mut limiter = FailureLimiter::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES {
            assert_eq!(limiter.locked(ip, t0), None, "attempt {i}");
            limiter.record_failure(ip, t0 + Duration::from_secs(i as u64));
        }
        let now = t0 + Duration::from_secs(MAX_FAILURES as u64);
        let wait = limiter.locked(ip, now).expect("locked");
        assert_eq!(
            wait,
            FAILURE_WINDOW - Duration::from_secs(MAX_FAILURES as u64)
        );
        assert_eq!(limiter.locked(other, now), None, "per client");
        // The oldest failure leaves the window: one more attempt allowed.
        assert_eq!(limiter.locked(ip, t0 + FAILURE_WINDOW), None);
        assert_eq!(limiter.locked(ip, t0 + FAILURE_WINDOW * 2), None);
        assert_eq!(limiter.tracked(), 0, "expired clients are forgotten");
    }

    #[test]
    fn failure_limiter_keys_ipv6_by_prefix_and_is_bounded() {
        let mut limiter = FailureLimiter::default();
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES {
            let ip: IpAddr = format!("2001:db8:1:2::{:x}", i + 1).parse().unwrap();
            limiter.record_failure(ip, t0);
        }
        let neighbour: IpAddr = "2001:db8:1:2:ffff::1".parse().unwrap();
        assert!(limiter.locked(neighbour, t0).is_some(), "same /64");
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        limiter.record_failure(mapped, t0);
        assert_eq!(client_key(mapped), "192.0.2.1".parse::<IpAddr>().unwrap());
        for i in 0..(MAX_TRACKED_CLIENTS as u32 + 10) {
            limiter.record_failure(IpAddr::V4(i.into()), t0);
        }
        assert!(limiter.tracked() <= MAX_TRACKED_CLIENTS);
    }
}
