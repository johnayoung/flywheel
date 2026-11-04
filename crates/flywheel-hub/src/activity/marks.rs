//! Each client's presence: whether someone is at it now (any of its
//! events sockets says so), and when it was last active -- the start of
//! its "while you were away" digest.

use super::MARKS_FILE;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Clients remembered (the longest-gone are dropped past it).
pub const MAX_CLIENTS: usize = 256;
/// Least time between two saves while a client stays present.
const SAVE_EVERY: chrono::Duration = chrono::Duration::seconds(60);

#[derive(Debug, Default, Serialize, Deserialize)]
struct MarksFile {
    #[serde(default)]
    clients: BTreeMap<String, DateTime<Utc>>,
}

/// A client came back after being away: the digest covers
/// `since..until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Return {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

/// Every client's last-active time and live presence.
#[derive(Debug)]
pub struct Marks {
    path: PathBuf,
    last_active: HashMap<String, DateTime<Utc>>,
    /// Connections (per client) where someone is at the page now.
    present: HashMap<String, HashSet<u64>>,
    saved_at: Option<DateTime<Utc>>,
    dirty: bool,
}

impl Marks {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(MARKS_FILE);
        let file: MarksFile = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            last_active: file.clients.into_iter().collect(),
            present: HashMap::new(),
            saved_at: None,
            dirty: false,
        }
    }

    /// When `client` was last active: now while it is present, `None` for
    /// a client never seen.
    pub fn last_active(&self, client: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.is_present(client) {
            return Some(now);
        }
        self.last_active.get(client).copied()
    }

    pub fn is_present(&self, client: &str) -> bool {
        self.present.get(client).is_some_and(|c| !c.is_empty())
    }

    /// Connection `conn` of `client` says whether someone is at it. A
    /// client becoming present after `away_min` or more away returns the
    /// span it missed.
    pub fn set_present(
        &mut self,
        client: &str,
        conn: u64,
        present: bool,
        now: DateTime<Utc>,
        away_min: chrono::Duration,
    ) -> Option<Return> {
        let was = self.is_present(client);
        let conns = self.present.entry(client.to_owned()).or_default();
        if present {
            conns.insert(conn);
        } else {
            conns.remove(&conn);
        }
        let is = !conns.is_empty();
        if !is {
            self.present.remove(client);
        }
        let back = match (was, is) {
            (false, true) => self
                .last_active
                .get(client)
                .copied()
                .filter(|since| now - *since >= away_min)
                .map(|since| Return { since, until: now }),
            _ => None,
        };
        if was || is {
            self.touch(client, now);
        }
        back
    }

    /// Connection `conn` of `client` closed.
    pub fn disconnect(&mut self, client: &str, conn: u64, now: DateTime<Utc>) {
        if self.present.get(client).is_some_and(|c| c.contains(&conn)) {
            self.set_present(client, conn, false, now, chrono::Duration::MAX);
        }
    }

    /// Mark `client` active now (the CLI's `--since last-seen`).
    pub fn touch(&mut self, client: &str, now: DateTime<Utc>) {
        self.last_active.insert(client.to_owned(), now);
        self.dirty = true;
        if self.last_active.len() > MAX_CLIENTS {
            let mut by_age: Vec<(String, DateTime<Utc>)> = self
                .last_active
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            by_age.sort_by_key(|(_, at)| *at);
            let excess = self.last_active.len() - MAX_CLIENTS;
            for (client, _) in by_age.into_iter().take(excess) {
                if !self.is_present(&client) {
                    self.last_active.remove(&client);
                }
            }
        }
    }

    /// Save if anything changed (and, while clients stay present, at most
    /// once a minute unless `force`).
    pub fn save(&mut self, now: DateTime<Utc>, force: bool) {
        if !self.dirty {
            return;
        }
        if !force && self.saved_at.is_some_and(|at| now - at < SAVE_EVERY) {
            return;
        }
        // Present clients are active up to now.
        let present: Vec<String> = self.present.keys().cloned().collect();
        for client in present {
            self.last_active.insert(client, now);
        }
        let file = MarksFile {
            clients: self
                .last_active
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).unwrap_or_default();
        match crate::auth::write_private(&self.path, &bytes) {
            Ok(()) => {
                self.saved_at = Some(now);
                self.dirty = false;
            }
            Err(e) => eprintln!("fw hub: cannot save {}: {e}", self.path.display()),
        }
    }
}
