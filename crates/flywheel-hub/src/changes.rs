//! Adopted worktrees: a git worktree made outside the hub (plain `git
//! worktree add`, an older session) that the hub tracks from now on, as
//! if `start_session` had made it: its uncommitted, untracked, and
//! unmerged counts, and the close rules (see [`crate::close`]) that never
//! remove work. A **change** is the record: the worktree, its branch and
//! base, and the session working in it, if any. It is the first piece of
//! the hub's change record; nothing else hangs off it yet.
//!
//! A session whose directory is inside the worktree gets it as its own
//! (its `worktree`), so every worktree path the hub has (badges, end
//! losses, removal refusals) covers it unchanged. A session elsewhere is
//! only linked.

use serde::{Deserialize, Serialize};

/// Changes one hub tracks.
pub const MAX_CHANGES: usize = 2000;

/// How a change's base was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseFrom {
    /// Given by whoever adopted it.
    Given,
    /// The branch's reflog: what it was created from.
    Reflog,
    /// The branch's upstream.
    Upstream,
    /// The repo's checked-out branch.
    RepoBranch,
}

impl BaseFrom {
    pub fn words(self) -> &'static str {
        match self {
            Self::Given => "as given",
            Self::Reflog => "from the branch's reflog (what it was created from)",
            Self::Upstream => "from the branch's upstream",
            Self::RepoBranch => "the repo's checked-out branch (nothing better was known)",
        }
    }
}

/// A tracked worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// `c` and 7 hex.
    pub id: String,
    pub worktree: crate::git::WorktreeRef,
    pub base_from: BaseFrom,
    /// The session working in it, if any (its id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Who adopted it: a session's id, or `operator`.
    pub by: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// A change with what its worktree holds now.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeView {
    #[serde(flatten)]
    pub change: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<crate::git::RemovalCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
}
