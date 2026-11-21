//! Review records on the hub (see [`crate::reviews`]): recorded by an
//! agent (`record_review`) or the operator (`fw hub review record`),
//! read by SHA, and checked for coverage (`check_reviewed`, `fw hub
//! review check`). Each record, retraction, and check is a `verdict.*`
//! event in the log.

use super::{Hub, HubError};
use crate::git;
use crate::reviews::{Coverage, Findings, NewReview, Review, ReviewQuery, Verdict};
use crate::state::{SessionRecord, StateError, new_id};
use crate::telemetry::now_ms;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Reviewed commits looked at for patch twins.
const TWIN_CANDIDATES: usize = 500;

/// A review to record, as the tool, API, and CLI give it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ReviewInput {
    /// Any path inside the repo (an agent's default: its directory).
    #[serde(default)]
    pub repo: String,
    pub base: String,
    pub head: String,
    pub verdict: String,
    #[serde(default)]
    pub findings: Findings,
    /// A path to the full review.
    #[serde(default)]
    pub report: Option<String>,
    #[serde(default)]
    pub follow_up_of: Option<String>,
    /// For a subagent the caller ran: its label (recorded `agent:<label>`).
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Hub {
    pub fn review_store(&self) -> &crate::reviews::ReviewStore {
        &self.reviews
    }

    /// The directories agent session `caller` works in (reports and repos
    /// it may name).
    fn work_roots(&self, caller: &SessionRecord) -> Vec<PathBuf> {
        let folders: Vec<PathBuf> = self
            .store()
            .workspace(&caller.workspace_id)
            .map(|w| w.folders.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default();
        crate::reports::roots_of(
            &caller.cwd,
            caller.worktree.as_ref().map(|w| w.path.as_path()),
            &folders,
        )
    }

    /// `raw` as a repo for `caller` (`None`: the operator): relative to its
    /// cwd, inside the directories it works in; the repo's common dir.
    fn review_repo(
        &self,
        caller: Option<&SessionRecord>,
        raw: &str,
    ) -> Result<(PathBuf, PathBuf), HubError> {
        let raw = raw.trim();
        let given = match (caller, raw) {
            (Some(c), "") => c.cwd.clone(),
            (Some(c), r) if Path::new(r).is_relative() => c.cwd.join(r),
            (None, "") => {
                return Err(StateError::Invalid("name the repo (a path inside it)".into()).into());
            }
            (_, r) => PathBuf::from(r),
        };
        let dir = given
            .canonicalize()
            .map_err(|e| StateError::Invalid(format!("repo {}: {e}", given.display())))?;
        if let Some(caller) = caller {
            let inside = self
                .work_roots(caller)
                .iter()
                .filter_map(|r| r.canonicalize().ok())
                .any(|root| dir.starts_with(root));
            if !inside {
                return Err(StateError::Invalid(format!(
                    "{} is outside the directories this session works in",
                    dir.display()
                ))
                .into());
            }
        }
        let common = git::common_dir(&dir).map_err(StateError::Invalid)?;
        Ok((dir, common))
    }

    /// The session whose worktree branch holds `head` in repo `common`
    /// (its tip first).
    fn review_author(&self, common: &Path, head: &str) -> Option<String> {
        let sessions: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.worktree.is_some())
            .cloned()
            .collect();
        let mut containing = None;
        for session in sessions {
            let worktree = session.worktree.as_ref()?;
            // Same repo: its folder's .git is the common dir (no git run per
            // session).
            if worktree.repo.join(".git").canonicalize().ok().as_deref() != Some(common) {
                continue;
            }
            let branch = format!("refs/heads/{}", worktree.branch);
            if git::commit_id(&worktree.repo, &branch).ok().as_deref() == Some(head) {
                return Some(session.id);
            }
            if containing.is_none() && git::is_ancestor(&worktree.repo, head, &branch) {
                containing = Some(session.id);
            }
        }
        containing
    }

    /// Record a review. `caller`: the agent session recording it (`None`:
    /// the operator).
    pub fn record_review(
        &self,
        caller: Option<&str>,
        input: &ReviewInput,
    ) -> Result<Review, HubError> {
        let record = match caller {
            Some(id) => Some(self.store().session(id)?.clone()),
            None => None,
        };
        let verdict = Verdict::parse(&input.verdict).map_err(StateError::Invalid)?;
        let (dir, repo) = self.review_repo(record.as_ref(), &input.repo)?;
        let base = git::commit_id(&dir, &input.base).map_err(StateError::Invalid)?;
        let head = git::commit_id(&dir, &input.head).map_err(StateError::Invalid)?;
        if base == head || !git::is_ancestor(&dir, &base, &head) {
            return Err(StateError::Invalid(format!(
                "{} is not an ancestor of {}: a review covers base..head",
                &base[..12],
                &head[..12]
            ))
            .into());
        }
        let commits = git::range_commits(&dir, &base, &head, crate::reviews::MAX_COMMITS)
            .map_err(StateError::Invalid)?;
        let follow_up_of = match input
            .follow_up_of
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
        {
            Some(reference) => {
                let earlier = self
                    .reviews
                    .get(reference)
                    .map_err(StateError::Invalid)?
                    .ok_or_else(|| StateError::NotFound(format!("review {reference}")))?;
                if earlier.repo != repo {
                    return Err(StateError::Invalid(format!(
                        "review {} is of another repo",
                        earlier.short()
                    ))
                    .into());
                }
                Some(earlier.id)
            }
            None => None,
        };
        let me = caller.unwrap_or("operator").to_owned();
        let reviewer = match input
            .reviewer
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            Some(label) => format!("agent:{label}"),
            None => me.clone(),
        };
        let id = format!("r{}", &new_id()[..15]);
        let report = match input
            .report
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            Some(path) => {
                let path = match &record {
                    Some(r) if Path::new(path).is_relative() => r.cwd.join(path),
                    _ => PathBuf::from(path),
                };
                let roots = record.as_ref().map(|r| self.work_roots(r));
                Some(
                    crate::reports::keep(self.state_dir(), "reviews", &id, &path, roots.as_deref())
                        .map_err(StateError::Invalid)?,
                )
            }
            None => None,
        };
        let author = self.review_author(&repo, &head);
        let review = self
            .reviews
            .record(
                NewReview {
                    repo,
                    base,
                    head,
                    reviewer,
                    recorded_by: me,
                    author,
                    verdict,
                    findings: input.findings,
                    report,
                    follow_up_of,
                    note: input.note.clone(),
                    commits: commits.into_iter().map(|(sha, _)| sha).collect(),
                    at: now_ms(),
                },
                id,
            )
            .map_err(HubError::Internal)?;
        self.log_hub_event(
            "verdict.recorded",
            caller,
            serde_json::json!({
                "id": review.id,
                "repo": review.repo,
                "base": review.base,
                "head": review.head,
                "verdict": review.verdict,
                "findings": review.findings,
                "reviewer": review.reviewer,
                "follow_up_of": review.follow_up_of,
                "commits": review.commits,
            }),
        );
        Ok(review)
    }

    /// Records `query` names (its repo, if a path, resolved to the repo).
    pub fn reviews(&self, query: &ReviewQuery) -> Result<Vec<Review>, HubError> {
        let mut query = query.clone();
        if let Some(repo) = &query.repo {
            query.repo = Some(git::common_dir(repo).map_err(StateError::Invalid)?);
        }
        if let (Some(sha), Some(repo)) = (&query.sha, &query.repo) {
            let dir = repo.parent().unwrap_or(repo);
            if let Ok(full) = git::commit_id(dir, sha).or_else(|_| git::commit_id(repo, sha)) {
                query.sha = Some(full);
            }
        }
        self.reviews.query(&query).map_err(HubError::Internal)
    }

    /// Coverage of `range` (`BASE..HEAD`) in the repo `repo` names, for
    /// `caller` (`None`: the operator).
    pub fn check_reviewed(
        &self,
        caller: Option<&str>,
        repo: &str,
        range: &str,
        patch_equivalent: bool,
    ) -> Result<Coverage, HubError> {
        let record = match caller {
            Some(id) => Some(self.store().session(id)?.clone()),
            None => None,
        };
        let (dir, common) = self.review_repo(record.as_ref(), repo)?;
        let (base, head) = range
            .split_once("..")
            .filter(|(b, h)| !b.is_empty() && !h.is_empty() && !h.starts_with('.'))
            .ok_or_else(|| StateError::Invalid(format!("range {range:?}: BASE..HEAD")))?;
        let base = git::commit_id(&dir, base).map_err(StateError::Invalid)?;
        let head = git::commit_id(&dir, head).map_err(StateError::Invalid)?;
        let commits = git::range_commits(&dir, &base, &head, crate::reviews::MAX_COMMITS)
            .map_err(StateError::Invalid)?;
        let store = &self.reviews;
        let twins = |missing: &[String]| -> HashMap<String, Vec<String>> {
            if !patch_equivalent {
                return HashMap::new();
            }
            let candidates = store
                .reviewed_shas(&common, TWIN_CANDIDATES)
                .unwrap_or_default();
            let theirs = git::patch_ids(&dir, &candidates);
            let mine = git::patch_ids(&dir, missing);
            let mut by_patch: HashMap<&str, Vec<String>> = HashMap::new();
            for (sha, patch) in &theirs {
                by_patch
                    .entry(patch.as_str())
                    .or_default()
                    .push(sha.clone());
            }
            mine.iter()
                .filter_map(|(sha, patch)| {
                    by_patch.get(patch.as_str()).map(|twins| {
                        (
                            sha.clone(),
                            twins.iter().filter(|t| *t != sha).cloned().collect(),
                        )
                    })
                })
                .collect()
        };
        let coverage = store
            .coverage(&common, &base, &head, &commits, &twins)
            .map_err(HubError::Internal)?;
        self.log_hub_event(
            "verdict.coverage",
            caller,
            serde_json::json!({
                "repo": coverage.repo,
                "base": coverage.base,
                "head": coverage.head,
                "ok": coverage.ok,
                "commits": coverage.commits,
                "uncovered": coverage.uncovered.iter().take(20).map(|c| c.sha.clone()).collect::<Vec<_>>(),
                "uncovered_count": coverage.uncovered_count,
                "by": caller.unwrap_or("operator"),
            }),
        );
        Ok(coverage)
    }

    /// Retract a record (the operator).
    pub fn retract_review(&self, id: &str, reason: &str) -> Result<Review, HubError> {
        if reason.trim().is_empty() {
            return Err(StateError::Invalid("say why it is retracted".into()).into());
        }
        let review = self
            .reviews
            .retract(id, reason, now_ms())
            .map_err(StateError::Invalid)?;
        self.log_hub_event(
            "verdict.retracted",
            None,
            serde_json::json!({ "id": review.id, "repo": review.repo, "reason": reason.trim() }),
        );
        Ok(review)
    }

    /// The `record_review` agent tool.
    pub fn agent_record_review(
        &self,
        caller: &str,
        input: &ReviewInput,
    ) -> Result<String, HubError> {
        let review = self.record_review(Some(caller), input)?;
        Ok(format!(
            "Recorded review {} of {}..{} ({} commit{}): {}{}{}. Leads find it by SHA \
             (check_reviewed); records cannot change: record a follow-up instead.",
            review.short(),
            &review.base[..12],
            &review.head[..12],
            review.commits,
            if review.commits == 1 { "" } else { "s" },
            review.verdict.as_str(),
            if review.findings.total() > 0 {
                format!(", {} findings", review.findings.total())
            } else {
                String::new()
            },
            if review.author.as_deref() == Some(review.reviewer.as_str()) {
                " (you are its author: it does not count as coverage)"
            } else {
                ""
            }
        ))
    }

    /// The `check_reviewed` agent tool.
    pub fn agent_check_reviewed(
        &self,
        caller: &str,
        repo: &str,
        range: &str,
        patch_equivalent: bool,
    ) -> Result<String, HubError> {
        let coverage = self.check_reviewed(Some(caller), repo, range, patch_equivalent)?;
        Ok(coverage_text(&coverage))
    }
}

/// A review an agent asks for (the `request_review` tool).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ReviewRequest {
    /// Any path inside one of the caller's repos (default: its directory).
    #[serde(default)]
    pub repo: String,
    pub base: String,
    pub head: String,
    /// What the change claims to do, and what to check hardest.
    #[serde(default)]
    pub focus: Option<String>,
    /// The reviewer session's name (default `review-<head>`).
    #[serde(default)]
    pub name: Option<String>,
}

impl Hub {
    /// The `request_review` agent tool: start a reviewer for `base..head`
    /// as `caller`'s child, in a fresh worktree at `head`, told to record
    /// its verdict and end its turn with it, so `caller` reads the verdict
    /// (through its children watch) and never the diff.
    pub fn agent_request_review(
        self: &std::sync::Arc<Self>,
        caller: &str,
        request: &ReviewRequest,
    ) -> Result<String, HubError> {
        let record = self.store().session(caller)?.clone();
        let (dir, common) = self.review_repo(Some(&record), &request.repo)?;
        let base = git::commit_id(&dir, &request.base).map_err(StateError::Invalid)?;
        let head = git::commit_id(&dir, &request.head).map_err(StateError::Invalid)?;
        if base == head || !git::is_ancestor(&dir, &base, &head) {
            return Err(StateError::Invalid(format!(
                "{} is not an ancestor of {}: a review covers base..head",
                &base[..12],
                &head[..12]
            ))
            .into());
        }
        let commits = git::range_commits(&dir, &base, &head, crate::reviews::MAX_COMMITS)
            .map_err(StateError::Invalid)?
            .len();
        let author = self
            .review_author(&common, &head)
            .and_then(|id| self.store().session(&id).ok().map(|s| s.name.clone()));
        // The reviewer's worktree is cut in the workspace folder holding
        // this repo (the caller's own when that is it).
        let workspace = self.workspace(&record.workspace_id)?;
        let own = git::common_dir(&record.cwd).ok();
        let folder = (own.as_deref() != Some(common.as_path()))
            .then(|| {
                workspace
                    .folders
                    .iter()
                    .find(|f| git::common_dir(&f.path).ok().as_deref() == Some(common.as_path()))
            })
            .flatten()
            .map(|f| f.path.clone());
        let short = &head[..8];
        let name = request
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map_or_else(|| format!("review-{short}"), str::to_owned);
        let task = format!("review-{short}-{}", &new_id()[..4]);
        let prompt = review_prompt(
            &base,
            &head,
            commits,
            author.as_deref(),
            request.focus.as_deref(),
        );
        let opened = self.agent_start(
            caller,
            super::AgentStart {
                name,
                prompt,
                folder,
                worktree: Some(crate::state::WorktreeRequest {
                    task,
                    base: Some(head.clone()),
                }),
                model: None,
            },
        )?;
        let range = format!("{}..{}", &base[..12], &head[..12]);
        Ok(match opened {
            super::Opened::Started(view) => format!(
                "Started reviewer {} (id {}) on {range} ({commits} commit{}). Its verdict \
                 arrives as its closing message through your children watch; do not read the \
                 diff yourself. check_reviewed {range} confirms coverage once it records.",
                view.record.name,
                view.record.id,
                if commits == 1 { "" } else { "s" },
            ),
            super::Opened::Waiting { request, .. } => format!(
                "The reviewer on {range} waits for the operator's approval (request {}); \
                 keep working.",
                request.id
            ),
        })
    }
}

/// A reviewer's first prompt for `base..head`.
fn review_prompt(
    base: &str,
    head: &str,
    commits: usize,
    author: Option<&str>,
    focus: Option<&str>,
) -> String {
    let by = author.map_or_else(String::new, |a| format!(" by {a}"));
    let focus = focus
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map_or_else(String::new, |f| {
            format!("\n\nWhat it claims, and what to check hardest:\n{f}")
        });
    format!(
        "Review {commits} commit{s} {base}..{head}{by}. Your worktree is checked out at the head; \
         you are the reviewer, not the author: do not edit, commit, or push.{focus}\n\n\
         1. Read the change: git log --oneline {base}..{head}; git diff {base}..{head}.\n\
         2. Check it does what it claims, and look for correctness bugs, missing or weak \
         tests, and broken project rules (CLAUDE.md). Run the targeted tests or commands that \
         prove or disprove a finding; do not run a full suite unless the change needs it.\n\
         3. Write the full review to a file outside the repo (/tmp/review-{short}.md) and call \
         mcp__flywheel__record_review with base {base}, head {head}, verdict READY, \
         READY_WITH_CHANGES, or NOT_READY, findings counted by severity, and that report.\n\
         4. End your turn with the verdict on the first line, then each finding that matters \
         with file:line and why, at most ~1500 characters in all. That closing message is all \
         your starter reads.",
        s = if commits == 1 { "" } else { "s" },
        short = &head[..8],
    )
}

/// Coverage in words.
pub fn coverage_text(coverage: &Coverage) -> String {
    use crate::reviews::Uncovered;
    if coverage.ok {
        return format!(
            "ok: all {} commit{} of {}..{} are covered by READY reviews.",
            coverage.commits,
            if coverage.commits == 1 { "" } else { "s" },
            &coverage.base[..12.min(coverage.base.len())],
            &coverage.head[..12.min(coverage.head.len())]
        );
    }
    let mut text = format!(
        "NOT ok: {} of {} commits uncovered in {}..{}:\n",
        coverage.uncovered_count,
        coverage.commits,
        &coverage.base[..12.min(coverage.base.len())],
        &coverage.head[..12.min(coverage.head.len())]
    );
    for commit in &coverage.uncovered {
        let why = match &commit.uncovered {
            Some(Uncovered::NoReview) | None => "no review".to_owned(),
            Some(Uncovered::NotReady { review }) => format!("NOT_READY ({review})"),
            Some(Uncovered::ReadyWithChanges { review }) => {
                format!("READY_WITH_CHANGES with no READY follow-up ({review})")
            }
            Some(Uncovered::SelfReview { review }) => {
                format!("reviewed only by its author ({review})")
            }
        };
        text.push_str(&format!(
            "  {} {}: {why}\n",
            &commit.sha[..12.min(commit.sha.len())],
            commit.subject
        ));
    }
    if coverage.uncovered_count as usize > coverage.uncovered.len() {
        text.push_str(&format!(
            "  and {} more\n",
            coverage.uncovered_count as usize - coverage.uncovered.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::review_prompt;

    #[test]
    fn a_reviewer_is_told_the_range_to_stay_read_only_record_and_end_with_its_verdict() {
        let (base, head) = ("a".repeat(40), "b".repeat(40));
        let prompt = review_prompt(&base, &head, 2, Some("c1"), Some("adds X"));
        assert!(
            prompt.starts_with(&format!("Review 2 commits {base}..{head} by c1.")),
            "{prompt}"
        );
        assert!(prompt.contains("do not edit, commit, or push"), "{prompt}");
        assert!(prompt.contains("check hardest:\nadds X"), "{prompt}");
        assert!(
            prompt.contains("mcp__flywheel__record_review with base"),
            "{prompt}"
        );
        assert!(prompt.contains("verdict on the first line"), "{prompt}");
        assert!(prompt.contains("/tmp/review-bbbbbbbb.md"), "{prompt}");
        let bare = review_prompt(&base, &head, 1, None, None);
        assert!(
            bare.starts_with(&format!("Review 1 commit {base}..{head}. ")),
            "{bare}"
        );
    }
}
