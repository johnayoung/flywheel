//! Peer credentials: what another hub holds to reach this one (multiple
//! machines, `fw hub machine add` on the other side).
//!
//! A peer credential is a dedicated, revocable bearer (`fwpeer_<id>_<secret>`,
//! 256 random bits) minted here by `fw hub peer issue --name X` or
//! `POST /api/peers`. It is scoped: the API and its WebSockets only (never a
//! browser login), and never the hub's own administration -- rotating the
//! token, minting login codes, managing peers or machines, importing files
//! from this machine (see [`peer_forbidden`]). Only SHA-256 digests are kept,
//! in [`PEERS_FILE`] (0600); rotating the token leaves peers alone (revoke
//! them with `fw hub peer revoke`).

use crate::auth::{TokenError, random_hex, unix_now, write_private};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use subtle::ConstantTimeEq;

/// The peers file inside the state dir (digests only).
pub const PEERS_FILE: &str = "peers.json";
/// Every peer credential starts with this.
pub const CREDENTIAL_PREFIX: &str = "fwpeer_";
/// Peers kept at once.
pub const MAX_PEERS: usize = 64;
/// Random bytes in a peer secret (256 bits).
const SECRET_BYTES: usize = 32;
/// Random bytes in a peer id.
const ID_BYTES: usize = 6;
/// Longest peer name.
const MAX_NAME: usize = 64;

/// Peer store failures.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("peers file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Token(#[from] TokenError),
}

/// A peer as listed (never its secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerEntry {
    id: String,
    name: String,
    created: u64,
    /// Hex SHA-256 of the secret.
    digest: String,
}

#[derive(Serialize, Deserialize)]
struct PeersFile {
    peers: Vec<PeerEntry>,
}

fn secret_digest(secret: &str) -> String {
    Sha256::new()
        .chain_update(b"flywheel-hub-peer\0")
        .chain_update(secret.as_bytes())
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A peer name: 1-64 of `[A-Za-z0-9._-]`.
pub fn validate_peer_name(raw: &str) -> Result<String, PeerError> {
    let name = raw.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(PeerError::Invalid(format!(
            "a peer name is 1-{MAX_NAME} characters"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(PeerError::Invalid(
            "a peer name is letters, digits, '.', '_' or '-'".into(),
        ));
    }
    Ok(name.to_owned())
}

/// Split a presented credential into its id and secret.
fn parse_credential(presented: &str) -> Option<(&str, &str)> {
    let rest = presented.strip_prefix(CREDENTIAL_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(id, ID_BYTES * 2) && hex(secret, SECRET_BYTES * 2)).then_some((id, secret))
}

/// Whether `presented` looks like a peer credential (a token never does).
pub fn is_peer_credential(presented: &str) -> bool {
    parse_credential(presented.trim()).is_some()
}

/// This hub's peers.
#[derive(Debug, Default)]
pub struct Peers {
    entries: Vec<PeerEntry>,
    /// Unix seconds a peer last authenticated (since this process started).
    last_used: HashMap<String, u64>,
}

impl Peers {
    /// The peers recorded in `state_dir`. A missing file is none; an
    /// unreadable or malformed one is an error (refusing every peer, never
    /// starting with someone else's list).
    pub fn load(state_dir: &Path) -> Result<Self, PeerError> {
        let path = state_dir.join(PEERS_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(PeerError::Io { path, source }),
        };
        let file: PeersFile = serde_json::from_slice(&bytes).map_err(|e| PeerError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        Ok(Self {
            entries: file.peers,
            last_used: HashMap::new(),
        })
    }

    /// Persist to `state_dir` (0600, atomic).
    pub fn save(&self, state_dir: &Path) -> Result<(), PeerError> {
        let path = state_dir.join(PEERS_FILE);
        let file = PeersFile {
            peers: self.entries.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| PeerError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        write_private(&path, &json).map_err(|source| PeerError::Io { path, source })
    }

    /// Mint a credential for a new peer `name` (unique): its record and the
    /// credential, shown once.
    pub fn issue(&mut self, name: &str) -> Result<(PeerInfo, String), PeerError> {
        let name = validate_peer_name(name)?;
        if self.entries.iter().any(|e| e.name == name) {
            return Err(PeerError::Conflict(format!(
                "a peer named {name} exists (revoke it first, or pick another name)"
            )));
        }
        if self.entries.len() >= MAX_PEERS {
            return Err(PeerError::Conflict(format!(
                "at most {MAX_PEERS} peers; revoke one first"
            )));
        }
        let id = loop {
            let id = random_hex(ID_BYTES)?;
            if !self.entries.iter().any(|e| e.id == id) {
                break id;
            }
        };
        let secret = random_hex(SECRET_BYTES)?;
        let entry = PeerEntry {
            id: id.clone(),
            name,
            created: unix_now(),
            digest: secret_digest(&secret),
        };
        let info = info(&entry);
        self.entries.push(entry);
        Ok((info, format!("{CREDENTIAL_PREFIX}{id}_{secret}")))
    }

    /// The peer `presented` authenticates as (constant-time on the
    /// secret); notes its use.
    pub fn verify(&mut self, presented: &str) -> Option<PeerInfo> {
        let (id, secret) = parse_credential(presented.trim())?;
        let digest = secret_digest(secret);
        let entry = self.entries.iter().find(|e| e.id == id)?;
        if !bool::from(entry.digest.as_bytes().ct_eq(digest.as_bytes())) {
            return None;
        }
        let info = info(entry);
        self.last_used.insert(info.id.clone(), unix_now());
        Some(info)
    }

    /// Whether peer `id` still exists.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// Forget the peer named or with id `which`.
    pub fn revoke(&mut self, which: &str) -> Result<PeerInfo, PeerError> {
        let which = which.trim();
        let index = self
            .entries
            .iter()
            .position(|e| e.id == which)
            .or_else(|| self.entries.iter().position(|e| e.name == which))
            .ok_or_else(|| PeerError::NotFound(format!("no peer {which} (fw hub peer ls)")))?;
        let entry = self.entries.remove(index);
        self.last_used.remove(&entry.id);
        Ok(info(&entry))
    }

    /// Every peer, oldest first, with when it was last used (this run).
    pub fn list(&self) -> Vec<(PeerInfo, Option<u64>)> {
        self.entries
            .iter()
            .map(|e| (info(e), self.last_used.get(&e.id).copied()))
            .collect()
    }
}

fn info(entry: &PeerEntry) -> PeerInfo {
    PeerInfo {
        id: entry.id.clone(),
        name: entry.name.clone(),
        created: entry.created,
    }
}

/// Whether a peer credential may not use `path` (`method` aside: the
/// refused endpoints are refused whatever the method). Defense in depth,
/// not a boundary: a peer starts sessions running any command as this
/// hub's user, so it can read the token file (and anything else that user
/// can) through one. The refusals keep the credential itself off the
/// token, login codes, peers, machines and the `/m/` proxy, off the file
/// import, off this hub's plugins, settings, and config (a machine's
/// UI is its own hub's), off its share links and recordings (a
/// machine's sessions are shared and recorded from that machine), and off
/// the permissions view and writes (they are guarded for this hub's own
/// user; a peer could still write a settings file from a shell session,
/// so this is not a boundary either).
pub fn peer_forbidden(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    path == "/api/token/rotate"
        || under("/api/login-codes")
        || under("/api/peers")
        || under("/api/machines")
        || under("/api/workspaces/import")
        || under("/api/code-workspaces")
        || under("/api/plugins")
        || under("/api/settings")
        || under("/api/config")
        || under("/api/tls")
        || under("/api/shares")
        || under("/api/recordings")
        || under("/api/perms")
        || path.starts_with("/api/flywheel/autopilot/")
        || under("/api/flywheel/steer")
        || under("/api/flywheel/tasks")
        || is_record_control(path)
        || under("/m")
        || !path.starts_with("/api")
}

/// `/api/sessions/{id}/record/...` (start and stop recording a session).
fn is_record_control(path: &str) -> bool {
    path.strip_prefix("/api/sessions/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, tail)| tail == "record" || tail.starts_with("record/"))
}

/// A running hub's peers: the store behind peer authentication and
/// `/api/peers`, with a change signal that closes a revoked peer's
/// WebSockets.
#[derive(Debug)]
pub struct PeerStore {
    state_dir: PathBuf,
    /// `Err`: the file could not be read -- every peer is refused and
    /// nothing is saved over it.
    peers: std::sync::Mutex<Loaded>,
    changes: tokio::sync::watch::Sender<u64>,
}

/// The peers as last read, and the file's identity then: a file changed
/// behind the hub's back (`fw hub peer` with the hub not recorded as
/// running, an edit) is re-read before it is used or written.
#[derive(Debug)]
struct Loaded {
    peers: Result<Peers, String>,
    stamp: Option<(std::time::SystemTime, u64)>,
}

fn file_stamp(state_dir: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = fs::metadata(state_dir.join(PEERS_FILE)).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl PeerStore {
    /// The peers of `state_dir` (an unreadable file is said on stderr).
    pub fn load(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            peers: std::sync::Mutex::new(Self::read(state_dir)),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn read(state_dir: &Path) -> Loaded {
        let stamp = file_stamp(state_dir);
        let peers = Peers::load(state_dir).map_err(|e| {
            eprintln!("fw hub: {e}; peer credentials are refused until it is fixed or removed");
            e.to_string()
        });
        Loaded { peers, stamp }
    }

    /// The peers, re-read first when the file changed since.
    fn lock(&self) -> std::sync::MutexGuard<'_, Loaded> {
        let mut guard = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if file_stamp(&self.state_dir) != guard.stamp {
            let was: Vec<String> = guard
                .peers
                .as_ref()
                .map(|p| p.entries.iter().map(|e| e.id.clone()).collect())
                .unwrap_or_default();
            let last_used = guard
                .peers
                .as_mut()
                .map(|p| std::mem::take(&mut p.last_used))
                .unwrap_or_default();
            *guard = Self::read(&self.state_dir);
            let gone = match &mut guard.peers {
                Ok(peers) => {
                    peers.last_used = last_used;
                    was.iter().any(|id| !peers.contains(id))
                }
                Err(_) => !was.is_empty(),
            };
            if gone {
                // Revoked behind the hub's back: their sockets re-check.
                self.changes.send_modify(|n| *n += 1);
            }
        }
        guard
    }

    /// The peer `presented` authenticates as.
    pub fn verify(&self, presented: &str) -> Option<PeerInfo> {
        if !is_peer_credential(presented) {
            return None;
        }
        self.lock().peers.as_mut().ok()?.verify(presented)
    }

    /// Whether peer `id` still exists.
    pub fn contains(&self, id: &str) -> bool {
        self.lock().peers.as_ref().is_ok_and(|p| p.contains(id))
    }

    /// Mint (and save) a credential for a new peer.
    pub fn issue(&self, name: &str) -> Result<(PeerInfo, String), PeerError> {
        let mut guard = self.lock();
        let peers = guard
            .peers
            .as_mut()
            .map_err(|e| PeerError::Conflict(e.clone()))?;
        let issued = peers.issue(name)?;
        if let Err(e) = peers.save(&self.state_dir) {
            let _ = peers.revoke(&issued.0.id);
            return Err(e);
        }
        guard.stamp = file_stamp(&self.state_dir);
        Ok(issued)
    }

    /// Revoke (and save) a peer by id or name; its sockets close.
    pub fn revoke(&self, which: &str) -> Result<PeerInfo, PeerError> {
        let revoked = {
            let mut guard = self.lock();
            let peers = guard
                .peers
                .as_mut()
                .map_err(|e| PeerError::Conflict(e.clone()))?;
            let revoked = peers.revoke(which)?;
            peers.save(&self.state_dir)?;
            guard.stamp = file_stamp(&self.state_dir);
            revoked
        };
        self.changes.send_modify(|n| *n += 1);
        Ok(revoked)
    }

    pub fn list(&self) -> Result<Vec<(PeerInfo, Option<u64>)>, PeerError> {
        self.lock()
            .peers
            .as_ref()
            .map(Peers::list)
            .map_err(|e| PeerError::Conflict(e.clone()))
    }

    /// Bumped on every revocation.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_verify_revoke_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut peers = Peers::load(dir.path()).unwrap();
        let (info, credential) = peers.issue("laptop").unwrap();
        assert!(credential.starts_with(CREDENTIAL_PREFIX));
        assert!(is_peer_credential(&credential));
        assert_eq!(peers.verify(&credential).unwrap(), info);
        peers.save(dir.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(PEERS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Digests only on disk.
        let text = fs::read_to_string(dir.path().join(PEERS_FILE)).unwrap();
        let secret = credential.rsplit('_').next().unwrap();
        assert!(!text.contains(secret));
        let mut reloaded = Peers::load(dir.path()).unwrap();
        assert_eq!(reloaded.verify(&credential).unwrap().name, "laptop");
        assert!(reloaded.list()[0].1.is_some());
        assert_eq!(reloaded.revoke("laptop").unwrap().id, info.id);
        assert!(reloaded.verify(&credential).is_none());
        assert!(reloaded.revoke(&info.id).is_err());
    }

    #[test]
    fn wrong_or_malformed_credentials_are_refused() {
        let mut peers = Peers::default();
        let (_, credential) = peers.issue("a").unwrap();
        let mut forged = credential.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == '0' { '1' } else { '0' });
        assert!(peers.verify(&forged).is_none());
        for bad in ["", "fwpeer_", "fwpeer_zz_zz", "deadbeef", &credential[1..]] {
            assert!(peers.verify(bad).is_none(), "{bad}");
        }
        // A 64-hex hub token is never mistaken for one.
        assert!(!is_peer_credential(&"a".repeat(64)));
    }

    #[test]
    fn names_are_validated_and_unique() {
        let mut peers = Peers::default();
        peers.issue("box-1").unwrap();
        assert!(matches!(peers.issue("box-1"), Err(PeerError::Conflict(_))));
        for bad in ["", "a b", "x/y", &"n".repeat(65)] {
            assert!(
                matches!(peers.issue(bad), Err(PeerError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_malformed_file_refuses_rather_than_forgets() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PEERS_FILE), b"{nope").unwrap();
        assert!(Peers::load(dir.path()).is_err());
    }

    #[test]
    fn peer_scope_refuses_administration() {
        for refused in [
            "/api/token/rotate",
            "/api/login-codes",
            "/api/peers",
            "/api/peers/abc",
            "/api/machines",
            "/api/machines/b/test",
            "/api/workspaces/import",
            "/api/code-workspaces",
            "/api/plugins",
            "/api/plugins/p/actions/a",
            "/api/settings",
            "/api/config",
            "/api/tls",
            "/api/tls/reload",
            "/api/shares",
            "/api/shares/abc",
            "/api/recordings",
            "/api/recordings/x.cast",
            "/api/perms",
            "/api/perms/allow",
            "/api/sessions/abc/record/start",
            "/api/sessions/abc/record/stop",
            // Autopilot is read-only for peers.
            "/api/flywheel/autopilot/start",
            "/api/flywheel/autopilot/decide",
            // So is steering a live run.
            "/api/flywheel/steer/say",
            "/api/flywheel/steer/interrupt",
            // And writing a task.
            "/api/flywheel/tasks",
            "/m/b/api/sessions",
            "/",
            "/login",
        ] {
            assert!(peer_forbidden(refused), "{refused}");
        }
        for allowed in [
            "/api/sessions",
            "/api/sessions/abc/ws",
            "/api/events",
            "/api/workspaces",
            "/api/workspaces/abc/layout",
            "/api/whoami",
            "/api/peer",
            "/api/flywheel/panel",
            "/api/flywheel/autopilot",
            // Search, transcripts, templates, and usage: a machine's
            // search overlay, launcher, and cost chips through the proxy.
            "/api/search",
            "/api/sessions/abc/transcript",
            "/api/templates",
            "/api/workspaces/abc/templates",
            "/api/workspaces/abc/templates/review",
            "/api/usage",
            // Fan-outs keep their own rules (not settings-scoped).
            "/api/fanouts",
            "/api/fanouts/abc/pick",
        ] {
            assert!(!peer_forbidden(allowed), "{allowed}");
        }
    }
}
