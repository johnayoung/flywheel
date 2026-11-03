//! The status board's side of a live handoff: every tracked agent
//! session's status, seen-by clients, and spawn key (its report secret and
//! generation) move to the next hub process as they are. Viewers do not:
//! their connections end with the old process and reconnect.

use super::{AgentStatus, Entry, Raw, SpawnKey, StatusBoard};
use crate::auth::Token;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// One tracked session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EntryExport {
    pub session_id: String,
    pub workspace_id: String,
    pub status: AgentStatus,
    pub heuristic: bool,
    pub detail: Option<String>,
    pub since: DateTime<Utc>,
    pub seen_by: Vec<String>,
    pub secret: String,
    pub generation: u64,
    pub changes: u64,
}

/// The whole board, minus viewers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BoardExport {
    pub entries: Vec<EntryExport>,
    pub next_generation: u64,
}

impl StatusBoard {
    pub(crate) fn handoff_export(&self) -> BoardExport {
        let mut entries: Vec<EntryExport> = self
            .entries
            .iter()
            .map(|(id, e)| {
                let mut seen_by: Vec<String> = e.seen_by.iter().cloned().collect();
                seen_by.sort();
                EntryExport {
                    session_id: id.clone(),
                    workspace_id: e.workspace_id.clone(),
                    status: e.raw.status,
                    heuristic: e.raw.heuristic,
                    detail: e.detail.clone(),
                    since: e.since,
                    seen_by,
                    secret: e.key.secret.expose().to_owned(),
                    generation: e.key.generation,
                    changes: e.changes,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        BoardExport {
            entries,
            next_generation: self.next_generation,
        }
    }

    /// A board holding exactly what `export` carried.
    pub(crate) fn handoff_import(export: BoardExport) -> Self {
        let mut board = Self {
            next_generation: export.next_generation,
            ..Self::default()
        };
        for e in export.entries {
            board.next_generation = board.next_generation.max(e.generation);
            board.entries.insert(
                e.session_id,
                Entry {
                    workspace_id: e.workspace_id,
                    raw: Raw {
                        status: e.status,
                        heuristic: e.heuristic,
                    },
                    detail: e.detail,
                    since: e.since,
                    seen_by: e.seen_by.into_iter().collect::<HashSet<_>>(),
                    key: SpawnKey {
                        secret: Token::from_secret(e.secret),
                        generation: e.generation,
                    },
                    changes: e.changes,
                },
            );
        }
        board
    }

    /// The spawn generation of a tracked session.
    pub(crate) fn generation(&self, session_id: &str) -> Option<u64> {
        self.entries.get(session_id).map(|e| e.key.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Audience, Signal};

    #[test]
    fn a_board_survives_export_and_import() {
        let mut board = StatusBoard::default();
        let key = board.track("s1", "w1").unwrap();
        board.track("s2", "w1").unwrap();
        assert!(board.apply("s1", key.generation, Signal::Blocked, Some("may I?")));
        let viewer = board.connect("browser-a");
        board.view(viewer, Some("s2"));
        let export = board.handoff_export();
        let json = serde_json::to_string(&export).unwrap();
        let mut copy = StatusBoard::handoff_import(serde_json::from_str(&json).unwrap());
        assert_eq!(copy.handoff_export(), export);
        assert_eq!(
            copy.authenticate("s1", key.secret.expose()),
            Some(key.generation)
        );
        let view = copy.status("s1", Audience::Anyone).unwrap();
        assert_eq!(view.status, AgentStatus::Blocked);
        assert_eq!(view.detail.as_deref(), Some("may I?"));
        // A later spawn still gets a fresh, higher generation.
        let next = copy.track("s3", "w1").unwrap();
        assert!(next.generation > key.generation);
    }
}
