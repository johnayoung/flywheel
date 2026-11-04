//! Read-only share links: a scoped, expiring, revocable view credential for
//! one session (`fw hub share SESSION`, the UI's Share action).
//!
//! A share credential (`fwview_<id>_<secret>`, 256 random bits) is carried
//! in its URL's path (`/share/<credential>`) and reaches that path only:
//! the viewer page, the session's title and status, and a read-only
//! terminal stream (input, resize, paste, and theme messages are refused;
//! it never counts as viewing, never sizes the PTY). Presented anywhere
//! else (a bearer on `/api/...`) it is refused with 403. Only SHA-256
//! digests are kept, in [`SHARES_FILE`] (0600). Revoking or expiry ends it
//! at once: its open sockets close. A viewer sees everything that terminal
//! shows -- nothing is redacted.

use crate::auth::{TokenError, random_hex, unix_now, write_private};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};
use subtle::ConstantTimeEq;

/// The shares file inside the state dir (digests only).
pub const SHARES_FILE: &str = "shares.json";
/// Every share credential starts with this.
pub const CREDENTIAL_PREFIX: &str = "fwview_";
/// The path a share link opens (`/share/<credential>`).
pub const SHARE_PATH: &str = "/share/";
/// Live shares kept at once.
pub const MAX_SHARES: usize = 128;
/// How long a share lasts when no expiry is given.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// The shortest expiry accepted.
pub const MIN_TTL: Duration = Duration::from_secs(60);
/// The longest expiry accepted (a share link always ends).
pub const MAX_TTL: Duration = Duration::from_secs(30 * 86_400);
/// Viewer sockets one share link may have open at once.
pub const MAX_VIEWERS_PER_SHARE: usize = 32;
/// Viewer sockets open at once across every share link.
pub const MAX_VIEWERS: usize = 256;
/// Random bytes in a share secret (256 bits).
const SECRET_BYTES: usize = 32;
/// Random bytes in a share id.
const ID_BYTES: usize = 6;
/// Longest share label, in characters.
const MAX_NAME: usize = 64;

/// Share store failures.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("shares file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Token(#[from] TokenError),
}

/// A share as listed (never its secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub id: String,
    /// The one session it shows.
    pub session_id: String,
    /// Its label (`--name`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Unix seconds.
    pub created: u64,
    /// Unix seconds; it stops working at this moment.
    pub expires: u64,
}

impl ShareInfo {
    /// A share as plugin events carry it: never the credential, its digest, or
    /// a link (the link is the credential).
    pub fn event(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "session_id": self.session_id,
            "name": self.name,
            "created": self.created,
            "expires": self.expires,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShareEntry {
    #[serde(flatten)]
    info: ShareInfo,
    /// Hex SHA-256 of the secret.
    digest: String,
}

#[derive(Serialize, Deserialize, Default)]
struct SharesFile {
    shares: Vec<ShareEntry>,
}

/// What a presented share credential is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A live share.
    Live(ShareInfo),
    /// A share of this hub's that has expired (its secret matched).
    Expired,
    /// Not a share of this hub's (or revoked): counts as a wrong
    /// credential.
    Unknown,
}

fn secret_digest(secret: &str) -> String {
    Sha256::new()
        .chain_update(b"flywheel-hub-share\0")
        .chain_update(secret.as_bytes())
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Split a presented credential into its id and secret.
fn parse_credential(presented: &str) -> Option<(&str, &str)> {
    let rest = presented.strip_prefix(CREDENTIAL_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(id, ID_BYTES * 2) && hex(secret, SECRET_BYTES * 2)).then_some((id, secret))
}

/// Whether `presented` looks like a share credential (a token or peer
/// credential never does).
pub fn is_share_credential(presented: &str) -> bool {
    parse_credential(presented.trim()).is_some()
}

/// The credential a `/share/<credential>[/...]` path carries, and the
/// rest of the path after it (`""`, `"/ws"`, `"/info"`, ...).
pub fn credential_in_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(SHARE_PATH)?;
    let end = rest.find('/').unwrap_or(rest.len());
    let (credential, tail) = rest.split_at(end);
    is_share_credential(credential).then_some((credential, tail))
}

/// A share label: trimmed, 1-64 characters, no control characters.
pub fn validate_share_name(raw: &str) -> Result<String, ShareError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(ShareError::Invalid(format!(
            "a share name is 1-{MAX_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(ShareError::Invalid(
            "a share name has no control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

/// Parse an expiry: `90s`, `30m`, `1h`, `2d`, `1w`, combinations
/// (`1h30m`), or plain seconds. Must lie within [`MIN_TTL`]..=[`MAX_TTL`].
pub fn parse_ttl(raw: &str) -> Result<Duration, String> {
    if raw.trim().is_empty() {
        return Err("an expiry is a duration like 30m, 1h, or 2d".into());
    }
    let total =
        crate::util::parse_duration(raw).map_err(|why| format!("bad expiry {raw:?}: {why}"))?;
    let ttl = Duration::from_secs(total);
    if ttl < MIN_TTL || ttl > MAX_TTL {
        return Err(format!(
            "an expiry is {}s to {} days",
            MIN_TTL.as_secs(),
            MAX_TTL.as_secs() / 86_400
        ));
    }
    Ok(ttl)
}

/// An expiry written back the way [`parse_ttl`] reads it (`1h`, `30d`,
/// `1h30m`, `90s`).
pub fn format_ttl(ttl: Duration) -> String {
    let mut left = ttl.as_secs();
    if left == 0 {
        return "0s".into();
    }
    let mut text = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if left >= size {
            text.push_str(&format!("{}{unit}", left / size));
            left %= size;
        }
    }
    text
}

/// How long new share links last: the expiry one gets when it names none,
/// and the longest one may name (`hub.toml` `[shares]`; both within
/// [`MIN_TTL`]..=[`MAX_TTL`], the default no longer than the longest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharePolicy {
    pub default_ttl: Duration,
    pub max_ttl: Duration,
}

impl Default for SharePolicy {
    fn default() -> Self {
        Self {
            default_ttl: DEFAULT_TTL,
            max_ttl: MAX_TTL,
        }
    }
}

impl SharePolicy {
    /// The expiry a new link gets: `asked`, else the default; refused
    /// past the longest allowed (or under [`MIN_TTL`]).
    pub fn expiry(&self, asked: Option<Duration>) -> Result<Duration, String> {
        let ttl = asked.unwrap_or(self.default_ttl);
        if ttl < MIN_TTL || ttl > self.max_ttl {
            return Err(format!(
                "an expiry is {} to {} on this hub",
                format_ttl(MIN_TTL),
                format_ttl(self.max_ttl)
            ));
        }
        Ok(ttl)
    }
}

/// This hub's shares, as on disk.
#[derive(Debug, Default)]
struct Shares {
    entries: Vec<ShareEntry>,
}

impl Shares {
    /// A missing file is none; an unreadable or malformed one is an error
    /// (refusing every share rather than starting from someone else's).
    fn load(state_dir: &Path) -> Result<Self, ShareError> {
        let path = state_dir.join(SHARES_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ShareError::Io { path, source }),
        };
        let file: SharesFile = serde_json::from_slice(&bytes).map_err(|e| ShareError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        Ok(Self {
            entries: file.shares,
        })
    }

    /// Persist (0600, atomic), dropping expired shares.
    fn save(&mut self, state_dir: &Path, now: u64) -> Result<(), ShareError> {
        self.entries.retain(|e| e.info.expires > now);
        let path = state_dir.join(SHARES_FILE);
        let file = SharesFile {
            shares: self.entries.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| ShareError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        write_private(&path, &json).map_err(|source| ShareError::Io { path, source })
    }

    fn verify(&self, presented: &str, now: u64) -> Verdict {
        let Some((id, secret)) = parse_credential(presented.trim()) else {
            return Verdict::Unknown;
        };
        let digest = secret_digest(secret);
        let Some(entry) = self.entries.iter().find(|e| e.info.id == id) else {
            return Verdict::Unknown;
        };
        if !bool::from(entry.digest.as_bytes().ct_eq(digest.as_bytes())) {
            return Verdict::Unknown;
        }
        if entry.info.expires <= now {
            return Verdict::Expired;
        }
        Verdict::Live(entry.info.clone())
    }

    fn live(&self, now: u64) -> impl Iterator<Item = &ShareEntry> {
        self.entries.iter().filter(move |e| e.info.expires > now)
    }
}

/// The shares as last read, and the file's identity then (re-read when
/// it changed behind the hub's back: `fw hub share revoke` with no hub
/// recorded as running, an edit).
#[derive(Debug)]
struct Loaded {
    shares: Result<Shares, String>,
    stamp: Option<(SystemTime, u64)>,
}

fn file_stamp(state_dir: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(state_dir.join(SHARES_FILE)).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// A running hub's shares: what share links authenticate against, with a
/// change signal that closes revoked shares' sockets, and the open viewer
/// count per share.
#[derive(Debug)]
pub struct ShareStore {
    state_dir: PathBuf,
    loaded: Mutex<Loaded>,
    changes: tokio::sync::watch::Sender<u64>,
    /// Open viewer sockets per share id and per session.
    viewers: Arc<Mutex<Viewers>>,
    /// Unix seconds a share last opened (this run).
    last_used: Mutex<HashMap<String, u64>>,
}

impl ShareStore {
    /// The shares of `state_dir` (an unreadable file is said on stderr).
    pub fn load(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            loaded: Mutex::new(Self::read(state_dir)),
            changes: tokio::sync::watch::Sender::new(0),
            viewers: Arc::default(),
            last_used: Mutex::default(),
        }
    }

    fn read(state_dir: &Path) -> Loaded {
        let stamp = file_stamp(state_dir);
        let shares = Shares::load(state_dir).map_err(|e| {
            eprintln!("fw hub: {e}; share links are refused until it is fixed or removed");
            e.to_string()
        });
        Loaded { shares, stamp }
    }

    /// The shares, re-read first when the file changed since.
    fn lock(&self) -> MutexGuard<'_, Loaded> {
        let mut guard = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if file_stamp(&self.state_dir) != guard.stamp {
            *guard = Self::read(&self.state_dir);
            // Possibly revoked behind the hub's back: sockets re-check.
            self.changes.send_modify(|n| *n += 1);
        }
        guard
    }

    /// What `presented` is, at `now` (unix seconds).
    pub fn verify_at(&self, presented: &str, now: u64) -> Verdict {
        if !is_share_credential(presented) {
            return Verdict::Unknown;
        }
        match &self.lock().shares {
            Ok(shares) => shares.verify(presented, now),
            Err(_) => Verdict::Unknown,
        }
    }

    /// What `presented` is now.
    pub fn verify(&self, presented: &str) -> Verdict {
        self.verify_at(presented, unix_now())
    }

    /// Whether share `id` is still live (not revoked, not expired).
    pub fn is_live(&self, id: &str) -> bool {
        let now = unix_now();
        self.lock()
            .shares
            .as_ref()
            .is_ok_and(|s| s.live(now).any(|e| e.info.id == id))
    }

    /// Mint (and save) a share of session `session_id` for `ttl`: its
    /// record and the credential, shown once.
    pub fn issue(
        &self,
        session_id: &str,
        ttl: Duration,
        name: Option<&str>,
    ) -> Result<(ShareInfo, String), ShareError> {
        // The API and CLI hold expiries to MIN_TTL..=MAX_TTL; the store
        // takes any whole second up to MAX_TTL.
        if ttl.as_secs() == 0 || ttl > MAX_TTL {
            return Err(ShareError::Invalid(format!(
                "an expiry is {}s to {} days",
                MIN_TTL.as_secs(),
                MAX_TTL.as_secs() / 86_400
            )));
        }
        let name = name.map(validate_share_name).transpose()?;
        let now = unix_now();
        let mut guard = self.lock();
        let shares = guard
            .shares
            .as_mut()
            .map_err(|e| ShareError::Conflict(e.clone()))?;
        if let Some(name) = &name
            && shares.live(now).any(|e| e.info.name.as_ref() == Some(name))
        {
            return Err(ShareError::Conflict(format!(
                "a share named {name:?} exists (revoke it first, or pick another name)"
            )));
        }
        if shares.live(now).count() >= MAX_SHARES {
            return Err(ShareError::Conflict(format!(
                "at most {MAX_SHARES} share links at once; revoke one first"
            )));
        }
        let id = loop {
            let id = random_hex(ID_BYTES)?;
            if !shares.entries.iter().any(|e| e.info.id == id) {
                break id;
            }
        };
        let secret = random_hex(SECRET_BYTES)?;
        let info = ShareInfo {
            id: id.clone(),
            session_id: session_id.to_owned(),
            name,
            created: now,
            expires: now.saturating_add(ttl.as_secs()),
        };
        shares.entries.push(ShareEntry {
            info: info.clone(),
            digest: secret_digest(&secret),
        });
        if let Err(e) = shares.save(&self.state_dir, now) {
            shares.entries.retain(|e| e.info.id != id);
            return Err(e);
        }
        guard.stamp = file_stamp(&self.state_dir);
        Ok((info, format!("{CREDENTIAL_PREFIX}{id}_{secret}")))
    }

    /// Revoke (and save) the share with id `which`, else every live share
    /// labelled `which`; their sockets close at once.
    pub fn revoke(&self, which: &str) -> Result<Vec<ShareInfo>, ShareError> {
        let which = which.trim();
        self.revoke_where(|info| info.id == which)
            .and_then(|by_id| {
                if by_id.is_empty() {
                    self.revoke_where(|info| info.name.as_deref() == Some(which))
                } else {
                    Ok(by_id)
                }
            })
            .and_then(|revoked| {
                if revoked.is_empty() {
                    Err(ShareError::NotFound(format!(
                        "share {which:?} (see fw hub share ls)"
                    )))
                } else {
                    Ok(revoked)
                }
            })
    }

    /// Revoke every share (of `session_id` only, when given).
    pub fn revoke_all(&self, session_id: Option<&str>) -> Result<Vec<ShareInfo>, ShareError> {
        self.revoke_where(|info| session_id.is_none_or(|s| info.session_id == s))
    }

    fn revoke_where(
        &self,
        matches: impl Fn(&ShareInfo) -> bool,
    ) -> Result<Vec<ShareInfo>, ShareError> {
        let now = unix_now();
        let revoked = {
            let mut guard = self.lock();
            let shares = guard
                .shares
                .as_mut()
                .map_err(|e| ShareError::Conflict(e.clone()))?;
            let revoked: Vec<ShareInfo> = shares
                .live(now)
                .filter(|e| matches(&e.info))
                .map(|e| e.info.clone())
                .collect();
            if revoked.is_empty() {
                return Ok(revoked);
            }
            let before = std::mem::take(&mut shares.entries);
            shares.entries = before
                .iter()
                .filter(|e| !revoked.iter().any(|r| r.id == e.info.id))
                .cloned()
                .collect();
            if let Err(e) = shares.save(&self.state_dir, now) {
                shares.entries = before;
                return Err(e);
            }
            guard.stamp = file_stamp(&self.state_dir);
            revoked
        };
        let mut last_used = self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for info in &revoked {
            last_used.remove(&info.id);
        }
        drop(last_used);
        self.changes.send_modify(|n| *n += 1);
        Ok(revoked)
    }

    /// Every live share, oldest first, with its open viewer count and when
    /// it last opened (this run).
    pub fn list(&self) -> Result<Vec<ShareListing>, ShareError> {
        let now = unix_now();
        let guard = self.lock();
        let shares = guard
            .shares
            .as_ref()
            .map_err(|e| ShareError::Conflict(e.clone()))?;
        let viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
        let last_used = self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(shares
            .live(now)
            .map(|e| ShareListing {
                viewers: viewers.by_share.get(&e.info.id).copied().unwrap_or(0),
                last_used: last_used.get(&e.info.id).copied(),
                info: e.info.clone(),
            })
            .collect())
    }

    /// Bumped on every revocation (and on a file changed behind the hub).
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Count an open viewer of `share` until the guard drops; `None` when
    /// the link already has [`MAX_VIEWERS_PER_SHARE`] open, or every link
    /// together [`MAX_VIEWERS`] (a link's holder is an outsider: their
    /// sockets must not exhaust the hub's descriptors).
    pub fn hold_viewer(&self, share: &ShareInfo) -> Option<ViewerHold> {
        {
            let mut viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
            let total: usize = viewers.by_share.values().sum();
            if total >= MAX_VIEWERS
                || viewers.by_share.get(&share.id).copied().unwrap_or(0) >= MAX_VIEWERS_PER_SHARE
            {
                return None;
            }
            *viewers.by_share.entry(share.id.clone()).or_insert(0) += 1;
            *viewers
                .by_session
                .entry(share.session_id.clone())
                .or_insert(0) += 1;
        }
        self.last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(share.id.clone(), unix_now());
        Some(ViewerHold {
            share: share.id.clone(),
            session: share.session_id.clone(),
            viewers: Arc::clone(&self.viewers),
        })
    }

    /// Share-link viewers of session `session_id` open now.
    pub fn session_viewers(&self, session_id: &str) -> usize {
        self.viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_session
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }
}

/// Open viewer counts.
#[derive(Debug, Default)]
struct Viewers {
    by_share: HashMap<String, usize>,
    by_session: HashMap<String, usize>,
}

fn release(map: &mut HashMap<String, usize>, key: &str) {
    if let Some(n) = map.get_mut(key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(key);
        }
    }
}

/// One live share as listed.
#[derive(Debug, Clone, Serialize)]
pub struct ShareListing {
    #[serde(flatten)]
    pub info: ShareInfo,
    /// Viewer sockets open now.
    pub viewers: usize,
    /// Unix seconds it last opened a viewer (this run).
    pub last_used: Option<u64>,
}

/// An open viewer, counted while it lives.
#[derive(Debug)]
pub struct ViewerHold {
    share: String,
    session: String,
    viewers: Arc<Mutex<Viewers>>,
}

impl Drop for ViewerHold {
    fn drop(&mut self) {
        let mut viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
        release(&mut viewers.by_share, &self.share);
        release(&mut viewers.by_session, &self.session);
    }
}

/// Edit the shares file of a state dir no hub runs for: list.
pub fn list_offline(state_dir: &Path) -> Result<Vec<ShareInfo>, ShareError> {
    let now = unix_now();
    Ok(Shares::load(state_dir)?
        .live(now)
        .map(|e| e.info.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiries_round_trip_and_the_policy_bounds_them() {
        for raw in ["90s", "30m", "1h", "1h30m", "2d", "30d", "1d1h1m1s"] {
            let ttl = parse_ttl(raw).unwrap();
            assert_eq!(parse_ttl(&format_ttl(ttl)).unwrap(), ttl, "{raw}");
        }
        assert_eq!(format_ttl(Duration::from_secs(7 * 86_400)), "7d");
        let policy = SharePolicy {
            default_ttl: Duration::from_secs(600),
            max_ttl: Duration::from_secs(3600),
        };
        assert_eq!(policy.expiry(None).unwrap(), Duration::from_secs(600));
        assert_eq!(
            policy.expiry(Some(Duration::from_secs(3600))).unwrap(),
            Duration::from_secs(3600)
        );
        let refused = policy.expiry(Some(Duration::from_secs(3601))).unwrap_err();
        assert!(refused.contains("1h"), "{refused}");
        assert!(policy.expiry(Some(Duration::from_secs(59))).is_err());
        assert_eq!(SharePolicy::default().max_ttl, MAX_TTL);
    }

    #[test]
    fn issue_verify_expire_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::load(dir.path());
        let (info, credential) = store
            .issue("s1", Duration::from_secs(120), Some("demo"))
            .unwrap();
        assert!(credential.starts_with(CREDENTIAL_PREFIX));
        assert!(is_share_credential(&credential));
        assert!(!crate::peers::is_peer_credential(&credential));
        assert_eq!(store.verify(&credential), Verdict::Live(info.clone()));
        // Past its expiry it is recognised but refused.
        assert_eq!(store.verify_at(&credential, info.expires), Verdict::Expired);
        // Digests only on disk, mode 0600.
        let text = fs::read_to_string(dir.path().join(SHARES_FILE)).unwrap();
        let secret = credential.rsplit('_').next().unwrap();
        assert!(!text.contains(secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(SHARES_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A second store (another process) sees it, and a revocation there
        // is seen here.
        let other = ShareStore::load(dir.path());
        assert!(matches!(other.verify(&credential), Verdict::Live(_)));
        let mut changes = store.subscribe();
        changes.mark_unchanged();
        assert_eq!(other.revoke("demo").unwrap()[0].id, info.id);
        assert_eq!(store.verify(&credential), Verdict::Unknown);
        assert!(changes.has_changed().unwrap());
        assert!(store.revoke(&info.id).is_err());
    }

    #[test]
    fn forged_and_malformed_credentials_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::load(dir.path());
        let (_, credential) = store.issue("s1", DEFAULT_TTL, None).unwrap();
        let mut forged = credential.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == '0' { '1' } else { '0' });
        assert_eq!(store.verify(&forged), Verdict::Unknown);
        for bad in [
            "",
            "fwview_",
            "fwview_zz_zz",
            &credential[1..],
            &"a".repeat(64),
        ] {
            assert_eq!(store.verify(bad), Verdict::Unknown, "{bad}");
        }
    }

    #[test]
    fn paths_carry_the_credential_and_the_rest() {
        let credential = format!("{CREDENTIAL_PREFIX}{}_{}", "a".repeat(12), "b".repeat(64));
        assert_eq!(
            credential_in_path(&format!("/share/{credential}")),
            Some((credential.as_str(), ""))
        );
        assert_eq!(
            credential_in_path(&format!("/share/{credential}/ws")),
            Some((credential.as_str(), "/ws"))
        );
        assert_eq!(credential_in_path("/share/nope/ws"), None);
        assert_eq!(credential_in_path("/api/sessions"), None);
    }

    #[test]
    fn expiries_parse_within_bounds() {
        assert_eq!(parse_ttl("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_ttl("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_ttl("2d").unwrap(), Duration::from_secs(172_800));
        assert_eq!(parse_ttl("600").unwrap(), Duration::from_secs(600));
        for bad in ["", "10s", "31d", "5x", "h", "1h5"] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_are_unique_among_live_shares_and_viewers_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::load(dir.path());
        let (info, _) = store.issue("s1", DEFAULT_TTL, Some("x")).unwrap();
        assert!(matches!(
            store.issue("s2", DEFAULT_TTL, Some("x")),
            Err(ShareError::Conflict(_))
        ));
        assert!(matches!(
            store.issue("s2", DEFAULT_TTL, Some("\u{7}")),
            Err(ShareError::Invalid(_))
        ));
        let hold = store.hold_viewer(&info).unwrap();
        assert_eq!(store.list().unwrap()[0].viewers, 1);
        assert_eq!(store.session_viewers("s1"), 1);
        drop(hold);
        assert_eq!(store.list().unwrap()[0].viewers, 0);
        assert_eq!(store.session_viewers("s1"), 0);
        assert!(store.list().unwrap()[0].last_used.is_some());
        // A link's open viewers are capped.
        let holds: Vec<_> = (0..MAX_VIEWERS_PER_SHARE)
            .map(|_| store.hold_viewer(&info).unwrap())
            .collect();
        assert!(store.hold_viewer(&info).is_none());
        drop(holds);
        assert!(store.hold_viewer(&info).is_some());
        store.issue("s2", DEFAULT_TTL, None).unwrap();
        assert_eq!(store.revoke_all(Some("s2")).unwrap().len(), 1);
        assert_eq!(store.revoke_all(None).unwrap().len(), 1);
        assert!(store.list().unwrap().is_empty());
    }
}
