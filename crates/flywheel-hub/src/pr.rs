//! Pull requests and CI for hub sessions: after "Open PR" the work goes on
//! on GitHub, and this brings it back. For a session whose branch has a
//! pull request, the hub knows its state (open, draft, merged, closed),
//! review decision, mergeability, and CI checks; it shows a badge on the
//! session's row and a PR section in its Review drawer, fetches a failing
//! check's log, asks the session's agent to fix CI (or queues the ask
//! while it works), re-runs failed checks, marks the PR ready, and merges
//! it -- the last three planned first and confirmed by fingerprint, like
//! every review action.
//!
//! Everything goes through `gh` (the GitHub CLI, signed in by the user)
//! via the review runner ([`crate::review::runner`]: argv only, no
//! terminal, capped output, deadlines, at most 4 git/gh at once). No `gh`,
//! or `gh` not signed in: nothing is asked, and the view says why.
//!
//! [`PrWatch`] caches what GitHub said per (directory, branch) and asks
//! again on a slow timer (sooner while checks run), after review actions
//! (push, open PR) and its own, and on a manual refresh -- rate-limited
//! hub-wide, and paused for a while after GitHub's rate limit is hit.
//! Transitions it sees announce plugin events (`pr.checks_failed`,
//! `pr.checks_passed`, `pr.review_changes_requested`, `pr.merged`), which
//! the activity journal and the browsers' notifications pick up. The
//! first look at a pull request (a hub start) announces nothing.
//!
//! Operator doc: `docs/hub/workflows.md` (Pull requests and CI).

mod actions;
mod gh;
mod watch;

pub use actions::{FixReport, LogView, PrAction, PrOutcome, PrPlan, PrView};
pub use gh::{
    Bucket, Check, ChecksSummary, GhProblem, MergeMethod, ProblemKind, PullRequest, Rollup,
};
pub use watch::{PrBrief, PrTiming, PrWatch};

pub(crate) use watch::spawn_watcher;

#[cfg(test)]
mod tests;
