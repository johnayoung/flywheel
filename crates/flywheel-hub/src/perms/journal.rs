//! `<state-dir>/permission-requests.jsonl` (0600): one line each time an
//! agent blocked on a permission the hub could read -- when, which repo,
//! which agent and tool, and the narrow rules proposed for it (never the
//! full command or file content). Read back as suggestions: the requests
//! a repo blocks on most.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub const FILE: &str = "permission-requests.jsonl";
/// Kept this long.
pub const KEEP_DAYS: i64 = 30;
/// Past this size the file is rewritten with only what is kept.
const TRIM_BYTES: u64 = 512 * 1024;
/// Most entries kept by a trim.
const KEEP_ENTRIES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds.
    pub at: i64,
    /// The repo (main checkout) or folder.
    pub folder: String,
    pub agent: String,
    pub tool: String,
    /// Claude Code rules (strings) or Codex prefix-rule patterns (lists of
    /// words): the default proposal.
    pub rules: Vec<serde_json::Value>,
    pub session: String,
}

#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    lock: Mutex<()>,
}

/// A request a repo blocks on, with how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub agent: String,
    pub tool: String,
    pub rules: Vec<serde_json::Value>,
    pub count: usize,
    /// Distinct sessions it blocked.
    pub sessions: usize,
    pub last_at: i64,
}

impl Journal {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(FILE),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry (errors go to the hub log: a missed entry only
    /// thins out suggestions).
    pub fn append(&self, entry: &Entry) {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = self.append_locked(entry) {
            eprintln!(
                "fw hub: cannot record a permission request in {}: {e}",
                self.path.display()
            );
        }
    }

    fn append_locked(&self, entry: &Entry) -> std::io::Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        file.write_all(line.as_bytes())?;
        if file.metadata()?.len() > TRIM_BYTES {
            drop(file);
            let kept = self.kept(entry.at);
            let mut text = String::new();
            for e in &kept {
                text.push_str(&serde_json::to_string(e)?);
                text.push('\n');
            }
            crate::auth::write_private(&self.path, text.as_bytes())?;
        }
        Ok(())
    }

    /// Entries younger than [`KEEP_DAYS`] at `now`, the newest
    /// [`KEEP_ENTRIES`].
    fn kept(&self, now: i64) -> Vec<Entry> {
        let mut all = self.read();
        all.retain(|e| now - e.at <= KEEP_DAYS * 86_400);
        let skip = all.len().saturating_sub(KEEP_ENTRIES);
        all.split_off(skip)
    }

    /// Every readable entry, oldest first.
    pub fn read(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The requests `folder` blocked on since `since` (unix seconds), most
    /// frequent first; `keep` drops ones already handled.
    pub fn suggestions(
        &self,
        folder: &str,
        since: i64,
        keep: impl Fn(&Entry) -> bool,
    ) -> Vec<Suggestion> {
        let mut groups: Vec<(Suggestion, Vec<String>)> = Vec::new();
        for entry in self.read() {
            if entry.folder != folder || entry.at < since || entry.rules.is_empty() || !keep(&entry)
            {
                continue;
            }
            match groups
                .iter_mut()
                .find(|(s, _)| s.agent == entry.agent && s.rules == entry.rules)
            {
                Some((s, sessions)) => {
                    s.count += 1;
                    s.last_at = s.last_at.max(entry.at);
                    if !sessions.contains(&entry.session) {
                        sessions.push(entry.session.clone());
                    }
                }
                None => groups.push((
                    Suggestion {
                        agent: entry.agent.clone(),
                        tool: entry.tool.clone(),
                        rules: entry.rules.clone(),
                        count: 1,
                        sessions: 0,
                        last_at: entry.at,
                    },
                    vec![entry.session.clone()],
                )),
            }
        }
        let mut out: Vec<Suggestion> = groups
            .into_iter()
            .map(|(mut s, sessions)| {
                s.sessions = sessions.len();
                s
            })
            .collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_at.cmp(&a.last_at)));
        out
    }
}
