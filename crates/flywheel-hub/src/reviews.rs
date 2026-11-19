//! Review records: who reviewed which commits of a repo (`base..head`),
//! with what verdict, findings by severity, and the full report, kept in
//! `<state-dir>/reviews.db` once and never changed (a re-review is a new
//! record, `follow_up_of` the one it answers; the operator may retract
//! one, which keeps the row and its event). A record is the reviewer's
//! claim: the hub checks only that the commits exist and says who
//! claimed what.
//!
//! **Review coverage** (`check_reviewed`, `fw hub review check`) answers
//! whether every commit of a range is covered: some review whose range
//! holds it ends, through its follow-ups, READY; no review of it ends
//! NOT_READY later than that; and the READY one is not by the session
//! whose branch the commit is on (its author).

use crate::sqlite_store::LazyDb;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Per commit, other commits with the same patch.
pub type Twins = HashMap<String, Vec<String>>;
/// Records relevant to some commits, and per commit those holding it.
type Relevant = (Vec<Review>, HashMap<String, Vec<String>>);

/// The review store's file in the state dir.
pub const REVIEWS_FILE: &str = "reviews.db";
const SCHEMA: i64 = 1;
/// Commits one review may cover.
pub const MAX_COMMITS: usize = 5000;
/// Uncovered commits an answer lists (the rest counted).
pub const MAX_LISTED: usize = 200;
/// The longest note, reviewer label, or retraction reason.
pub const NOTE_CHARS: usize = 2000;
const LABEL_CHARS: usize = 120;
/// Records one query returns at most.
pub const QUERY_MAX: usize = 1000;

/// A reviewer's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Ready,
    ReadyWithChanges,
    NotReady,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::ReadyWithChanges => "READY_WITH_CHANGES",
            Self::NotReady => "NOT_READY",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().replace('-', "_").as_str() {
            "READY" => Ok(Self::Ready),
            "READY_WITH_CHANGES" => Ok(Self::ReadyWithChanges),
            "NOT_READY" => Ok(Self::NotReady),
            _ => Err(format!(
                "verdict {raw:?}: READY, READY_WITH_CHANGES, or NOT_READY"
            )),
        }
    }
}

/// Findings counted by severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    #[serde(default)]
    pub critical: u32,
    #[serde(default)]
    pub high: u32,
    #[serde(default)]
    pub medium: u32,
    #[serde(default)]
    pub low: u32,
    #[serde(default)]
    pub info: u32,
}

impl Findings {
    pub fn total(&self) -> u32 {
        self.critical + self.high + self.medium + self.low + self.info
    }

    /// `critical=0,high=1,...` (any order, any subset).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut found = Self::default();
        for part in raw.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
            let (key, n) = part
                .split_once('=')
                .ok_or_else(|| format!("findings {part:?}: like high=1"))?;
            let n: u32 = n
                .trim()
                .parse()
                .map_err(|_| format!("findings {part:?}: a count"))?;
            match key.trim().to_ascii_lowercase().as_str() {
                "critical" => found.critical = n,
                "high" => found.high = n,
                "medium" => found.medium = n,
                "low" => found.low = n,
                "info" => found.info = n,
                other => {
                    return Err(format!(
                        "severity {other:?}: critical, high, medium, low, or info"
                    ));
                }
            }
        }
        Ok(found)
    }
}

/// Who took a record back, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retraction {
    pub at: i64,
    pub reason: String,
}

/// A review record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// `r` and 15 hex.
    pub id: String,
    /// The repo's common git dir (one repo, all its worktrees).
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    /// The session that reviewed (its id), or `agent:<label>` for a
    /// subagent a session recorded it for, or `operator`.
    pub reviewer: String,
    /// The session (id) or `operator` that recorded it.
    pub recorded_by: String,
    /// The session whose branch `head` is on, when the hub knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub verdict: Verdict,
    pub findings: Findings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<crate::reports::ReportRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Commits it covers.
    pub commits: u32,
    /// Unix ms.
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retracted: Option<Retraction>,
}

impl Review {
    pub fn short(&self) -> String {
        self.id.chars().take(8).collect()
    }
}

/// Which records to read: newest first.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReviewQuery {
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// A commit the record covers.
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Why a commit is not covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Uncovered {
    NoReview,
    /// A review of it ends NOT_READY (its chain's tip).
    NotReady {
        review: String,
    },
    /// A review of it ends READY_WITH_CHANGES with no READY follow-up.
    ReadyWithChanges {
        review: String,
    },
    /// Only its author's own review says READY.
    SelfReview {
        review: String,
    },
}

/// One commit's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitCoverage {
    pub sha: String,
    pub subject: String,
    /// The READY review (chain tip) that covers it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Covered through a reviewed commit with the same patch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    pub uncovered: Option<Uncovered>,
}

/// A range's coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub ok: bool,
    pub commits: u32,
    pub covered: u32,
    /// Uncovered commits, oldest first (at most [`MAX_LISTED`]).
    pub uncovered: Vec<CommitCoverage>,
    pub uncovered_count: u32,
}

/// What [`ReviewStore::record`] stores.
#[derive(Debug, Clone)]
pub struct NewReview {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub reviewer: String,
    pub recorded_by: String,
    pub author: Option<String>,
    pub verdict: Verdict,
    pub findings: Findings,
    pub report: Option<crate::reports::ReportRef>,
    pub follow_up_of: Option<String>,
    pub note: Option<String>,
    /// The commits `base..head` (oldest first).
    pub commits: Vec<String>,
    pub at: i64,
}

fn migrate(conn: &rusqlite::Connection, from: i64) -> rusqlite::Result<()> {
    if from < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS reviews (
                 id           TEXT PRIMARY KEY,
                 repo         TEXT NOT NULL,
                 at           INTEGER NOT NULL,
                 reviewer     TEXT NOT NULL,
                 follow_up_of TEXT,
                 body         TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS reviews_repo ON reviews (repo, at);
             CREATE INDEX IF NOT EXISTS reviews_follow ON reviews (follow_up_of);
             CREATE TABLE IF NOT EXISTS review_commits (
                 review TEXT NOT NULL,
                 repo   TEXT NOT NULL,
                 sha    TEXT NOT NULL,
                 PRIMARY KEY (review, sha)
             );
             CREATE INDEX IF NOT EXISTS review_commits_sha ON review_commits (repo, sha);",
        )?;
    }
    Ok(())
}

/// The review records of one hub.
#[derive(Debug)]
pub struct ReviewStore {
    db: LazyDb,
}

fn cap(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

fn read(body: &str) -> Option<Review> {
    serde_json::from_str(body).ok()
}

impl ReviewStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            db: LazyDb::new(state_dir.join(REVIEWS_FILE), SCHEMA, migrate),
        }
    }

    /// Keep `new` (its id from `id`); the record as kept.
    pub fn record(&self, new: NewReview, id: String) -> Result<Review, String> {
        if new.commits.len() > MAX_COMMITS {
            return Err(format!(
                "{}..{} has {} commits; a review covers at most {MAX_COMMITS}",
                new.base,
                new.head,
                new.commits.len()
            ));
        }
        let review = Review {
            id,
            repo: new.repo,
            base: new.base,
            head: new.head,
            reviewer: cap(&new.reviewer, LABEL_CHARS),
            recorded_by: new.recorded_by,
            author: new.author,
            verdict: new.verdict,
            findings: new.findings,
            report: new.report,
            follow_up_of: new.follow_up_of,
            note: new
                .note
                .as_deref()
                .map(|n| cap(n, NOTE_CHARS))
                .filter(|n| !n.is_empty()),
            commits: u32::try_from(new.commits.len()).unwrap_or(u32::MAX),
            at: new.at,
            retracted: None,
        };
        let repo = review.repo.to_string_lossy().into_owned();
        let body = serde_json::to_string(&review).map_err(|e| e.to_string())?;
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO reviews (id, repo, at, reviewer, follow_up_of, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    review.id,
                    repo,
                    review.at,
                    review.reviewer,
                    review.follow_up_of,
                    body
                ],
            )?;
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO review_commits (review, repo, sha) VALUES (?1, ?2, ?3)",
            )?;
            for sha in &new.commits {
                insert.execute(params![review.id, repo, sha])?;
            }
            Ok(())
        })?;
        Ok(review)
    }

    /// A record by id or unique id prefix (4+).
    pub fn get(&self, reference: &str) -> Result<Option<Review>, String> {
        let reference = reference.trim();
        if reference.len() < 4 {
            return Err("name a review by its id (4+ characters)".into());
        }
        let found: Vec<Review> = self.db.with(|conn| {
            let mut statement =
                conn.prepare_cached("SELECT body FROM reviews WHERE id LIKE ?1 || '%' LIMIT 2")?;
            let rows = statement
                .query_map(params![reference], |row| row.get::<_, String>(0))?
                .filter_map(Result::ok)
                .filter_map(|b| read(&b))
                .collect();
            Ok(rows)
        })?;
        match found.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            _ => Err(format!(
                "{reference} matches more than one review: give more of its id"
            )),
        }
    }

    /// Records `query` names, newest first.
    pub fn query(&self, query: &ReviewQuery) -> Result<Vec<Review>, String> {
        let limit = query.limit.unwrap_or(100).clamp(1, QUERY_MAX) as i64;
        let repo = query
            .repo
            .as_ref()
            .map(|r| r.to_string_lossy().into_owned());
        self.db.with(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT body FROM reviews
                 WHERE (?1 IS NULL OR repo = ?1)
                   AND (?2 IS NULL OR id IN (SELECT review FROM review_commits WHERE sha = ?2))
                   AND (?3 IS NULL OR reviewer = ?3)
                   AND (?4 IS NULL OR at >= ?4)
                 ORDER BY at DESC, id DESC LIMIT ?5",
            )?;
            let rows = statement
                .query_map(
                    params![repo, query.sha, query.reviewer, query.since, limit],
                    |row| row.get::<_, String>(0),
                )?
                .filter_map(Result::ok)
                .filter_map(|b| read(&b))
                .collect();
            Ok(rows)
        })
    }

    /// Mark `id` retracted (the row stays).
    pub fn retract(&self, id: &str, reason: &str, at: i64) -> Result<Review, String> {
        let mut review = self.get(id)?.ok_or_else(|| format!("no review {id}"))?;
        if review.retracted.is_some() {
            return Err(format!("review {} was retracted already", review.short()));
        }
        review.retracted = Some(Retraction {
            at,
            reason: cap(reason, NOTE_CHARS),
        });
        let body = serde_json::to_string(&review).map_err(|e| e.to_string())?;
        self.db.with(|conn| {
            conn.execute(
                "UPDATE reviews SET body = ?2 WHERE id = ?1",
                params![review.id, body],
            )
        })?;
        Ok(review)
    }

    /// The newest `limit` commits reviews of `repo` hold (patch twins'
    /// candidates).
    pub fn reviewed_shas(&self, repo: &Path, limit: usize) -> Result<Vec<String>, String> {
        let repo = repo.to_string_lossy().into_owned();
        self.db.with(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT DISTINCT c.sha FROM review_commits c JOIN reviews r ON r.id = c.review
                 WHERE c.repo = ?1 ORDER BY r.at DESC LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![repo, limit as i64], |row| row.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
    }

    /// Whether `id` exists, and its repo.
    pub fn repo_of(&self, id: &str) -> Result<Option<String>, String> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT repo FROM reviews WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
        })
    }

    /// Every record of `repo` that covers one of `shas`, and every
    /// follow-up below them (not retracted).
    fn relevant(&self, repo: &str, shas: &[String]) -> Result<Relevant, String> {
        self.db.with(|conn| {
            let mut covering: HashMap<String, Vec<String>> = HashMap::new();
            let mut ids: HashSet<String> = HashSet::new();
            let mut by_sha = conn
                .prepare_cached("SELECT review FROM review_commits WHERE repo = ?1 AND sha = ?2")?;
            for sha in shas {
                let reviews: Vec<String> = by_sha
                    .query_map(params![repo, sha], |row| row.get::<_, String>(0))?
                    .filter_map(Result::ok)
                    .collect();
                ids.extend(reviews.iter().cloned());
                covering.insert(sha.clone(), reviews);
            }
            // Follow-up chains, any depth.
            let mut follow =
                conn.prepare_cached("SELECT id FROM reviews WHERE follow_up_of = ?1")?;
            let mut frontier: Vec<String> = ids.iter().cloned().collect();
            while let Some(id) = frontier.pop() {
                for child in follow
                    .query_map(params![id], |row| row.get::<_, String>(0))?
                    .filter_map(Result::ok)
                {
                    if ids.insert(child.clone()) {
                        frontier.push(child);
                    }
                }
            }
            let mut body = conn.prepare_cached("SELECT body FROM reviews WHERE id = ?1")?;
            let mut reviews = Vec::new();
            for id in &ids {
                if let Some(review) = body
                    .query_row(params![id], |row| row.get::<_, String>(0))
                    .optional()?
                    .and_then(|b| read(&b))
                    .filter(|r| r.retracted.is_none())
                {
                    reviews.push(review);
                }
            }
            Ok((reviews, covering))
        })
    }

    /// Each of `shas`: the READY review (chain tip) covering it, or why
    /// none does.
    fn judge(
        &self,
        repo: &str,
        shas: &[String],
    ) -> Result<HashMap<String, Result<String, Uncovered>>, String> {
        let (reviews, covering) = self.relevant(repo, shas)?;
        let by_id: HashMap<&str, &Review> = reviews.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut children: BTreeMap<&str, Vec<&Review>> = BTreeMap::new();
        for r in &reviews {
            if let Some(parent) = r.follow_up_of.as_deref() {
                children.entry(parent).or_default().push(r);
            }
        }
        // A chain's tip: the newest follow-up, followed down.
        let tip = |start: &Review| -> Review {
            let mut at = start;
            let mut seen = HashSet::new();
            while seen.insert(at.id.clone()) {
                match children
                    .get(at.id.as_str())
                    .and_then(|c| c.iter().max_by_key(|r| (r.at, r.id.clone())))
                {
                    Some(next) => at = next,
                    None => break,
                }
            }
            at.clone()
        };
        let mut out = HashMap::new();
        for sha in shas {
            let tips: Vec<Review> = covering
                .get(sha)
                .into_iter()
                .flatten()
                .filter_map(|id| by_id.get(id.as_str()))
                .map(|r| tip(r))
                .collect();
            let own = |t: &Review| t.author.as_deref() == Some(t.reviewer.as_str());
            let newest_ready = tips
                .iter()
                .filter(|t| t.verdict == Verdict::Ready && !own(t))
                .max_by_key(|t| t.at);
            let newest_not = tips
                .iter()
                .filter(|t| t.verdict == Verdict::NotReady)
                .max_by_key(|t| t.at);
            let answer = match (newest_ready, newest_not) {
                _ if tips.is_empty() => Err(Uncovered::NoReview),
                (Some(ready), Some(not)) if not.at > ready.at => Err(Uncovered::NotReady {
                    review: not.id.clone(),
                }),
                (Some(ready), _) => Ok(ready.id.clone()),
                (None, Some(not)) => Err(Uncovered::NotReady {
                    review: not.id.clone(),
                }),
                (None, None) => match tips.iter().find(|t| t.verdict == Verdict::Ready) {
                    Some(mine) => Err(Uncovered::SelfReview {
                        review: mine.id.clone(),
                    }),
                    None => Err(Uncovered::ReadyWithChanges {
                        review: tips
                            .iter()
                            .max_by_key(|t| t.at)
                            .map(|t| t.id.clone())
                            .unwrap_or_default(),
                    }),
                },
            };
            out.insert(sha.clone(), answer);
        }
        Ok(out)
    }

    /// Coverage of `commits` (oldest first, with subjects) of `repo`'s
    /// `base..head`. `same_patch`: for commits no review holds, the
    /// reviewed commits with the same patch (the caller computes them
    /// only when asked; a covered one covers it too, `via`).
    pub fn coverage(
        &self,
        repo: &Path,
        base: &str,
        head: &str,
        commits: &[(String, String)],
        same_patch: &dyn Fn(&[String]) -> Twins,
    ) -> Result<Coverage, String> {
        let repo_key = repo.to_string_lossy().into_owned();
        let shas: Vec<String> = commits.iter().map(|(s, _)| s.clone()).collect();
        let judged = self.judge(&repo_key, &shas)?;
        let mut answers: Vec<CommitCoverage> = commits
            .iter()
            .map(|(sha, subject)| {
                let (review, uncovered) = match judged.get(sha) {
                    Some(Ok(review)) => (Some(review.clone()), None),
                    Some(Err(why)) => (None, Some(why.clone())),
                    None => (None, Some(Uncovered::NoReview)),
                };
                CommitCoverage {
                    sha: sha.clone(),
                    subject: subject.clone(),
                    review,
                    via: None,
                    uncovered,
                }
            })
            .collect();
        let missing: Vec<String> = answers
            .iter()
            .filter(|a| a.uncovered == Some(Uncovered::NoReview))
            .map(|a| a.sha.clone())
            .collect();
        if !missing.is_empty() {
            let twins = same_patch(&missing);
            let all: Vec<String> = twins.values().flatten().cloned().collect();
            let twin_judged = if all.is_empty() {
                HashMap::new()
            } else {
                self.judge(&repo_key, &all)?
            };
            for answer in answers
                .iter_mut()
                .filter(|a| a.uncovered == Some(Uncovered::NoReview))
            {
                if let Some((twin, review)) =
                    twins.get(&answer.sha).into_iter().flatten().find_map(|t| {
                        match twin_judged.get(t) {
                            Some(Ok(review)) => Some((t.clone(), review.clone())),
                            _ => None,
                        }
                    })
                {
                    answer.via = Some(twin);
                    answer.review = Some(review);
                    answer.uncovered = None;
                }
            }
        }
        let total = answers.len();
        let uncovered: Vec<CommitCoverage> = answers
            .into_iter()
            .filter(|a| a.uncovered.is_some())
            .collect();
        let uncovered_count = uncovered.len();
        Ok(Coverage {
            repo: repo.to_path_buf(),
            base: base.to_owned(),
            head: head.to_owned(),
            ok: uncovered_count == 0,
            commits: u32::try_from(total).unwrap_or(u32::MAX),
            covered: u32::try_from(total - uncovered_count).unwrap_or(u32::MAX),
            uncovered: uncovered.into_iter().take(MAX_LISTED).collect(),
            uncovered_count: u32::try_from(uncovered_count).unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: ReviewStore,
        n: std::cell::Cell<i64>,
    }

    const REPO: &str = "/r/.git";

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = ReviewStore::new(dir.path());
            Self {
                _dir: dir,
                store,
                n: std::cell::Cell::new(0),
            }
        }

        /// Record `verdict` over `commits` (by `reviewer`, of `author`'s
        /// branch), following `follows`; its id.
        fn review(
            &self,
            commits: &[&str],
            verdict: Verdict,
            reviewer: &str,
            author: Option<&str>,
            follows: Option<&str>,
        ) -> String {
            self.n.set(self.n.get() + 1);
            let id = format!("r{:015}", self.n.get());
            self.store
                .record(
                    NewReview {
                        repo: PathBuf::from(REPO),
                        base: "b".into(),
                        head: commits.last().unwrap().to_string(),
                        reviewer: reviewer.into(),
                        recorded_by: reviewer.into(),
                        author: author.map(str::to_owned),
                        verdict,
                        findings: Findings::default(),
                        report: None,
                        follow_up_of: follows.map(str::to_owned),
                        note: None,
                        commits: commits.iter().map(|c| (*c).to_owned()).collect(),
                        at: self.n.get(),
                    },
                    id.clone(),
                )
                .unwrap();
            id
        }

        fn check(&self, commits: &[&str]) -> Coverage {
            self.check_twins(commits, &|_| HashMap::new())
        }

        fn check_twins(&self, commits: &[&str], twins: &dyn Fn(&[String]) -> Twins) -> Coverage {
            let commits: Vec<(String, String)> = commits
                .iter()
                .map(|c| ((*c).to_owned(), format!("subject {c}")))
                .collect();
            self.store
                .coverage(Path::new(REPO), "b", "h", &commits, twins)
                .unwrap()
        }
    }

    fn why(coverage: &Coverage, sha: &str) -> Option<Uncovered> {
        coverage
            .uncovered
            .iter()
            .find(|c| c.sha == sha)
            .and_then(|c| c.uncovered.clone())
    }

    #[test]
    fn a_ready_review_covers_its_range_and_nothing_else() {
        let f = Fixture::new();
        f.review(&["c1", "c2"], Verdict::Ready, "rev", None, None);
        let got = f.check(&["c1", "c2", "c3"]);
        assert!(!got.ok);
        assert_eq!(got.covered, 2);
        assert_eq!(why(&got, "c3"), Some(Uncovered::NoReview));
        assert!(f.check(&["c1", "c2"]).ok);
    }

    #[test]
    fn a_ready_follow_up_covers_what_was_not_ready() {
        let f = Fixture::new();
        let first = f.review(&["c1", "c2"], Verdict::NotReady, "rev", None, None);
        let got = f.check(&["c1", "c2"]);
        assert_eq!(
            why(&got, "c1"),
            Some(Uncovered::NotReady {
                review: first.clone()
            })
        );
        let changes = f.review(
            &["c3"],
            Verdict::ReadyWithChanges,
            "rev",
            None,
            Some(&first),
        );
        let got = f.check(&["c1", "c2", "c3"]);
        assert_eq!(
            why(&got, "c1"),
            Some(Uncovered::ReadyWithChanges {
                review: changes.clone()
            })
        );
        f.review(&["c4"], Verdict::Ready, "rev", None, Some(&changes));
        assert!(
            f.check(&["c1", "c2", "c3", "c4"]).ok,
            "the chain ends READY"
        );
    }

    #[test]
    fn a_later_not_ready_uncovers_and_a_self_review_never_covers() {
        let f = Fixture::new();
        f.review(&["c1"], Verdict::Ready, "a", None, None);
        let later = f.review(&["c1"], Verdict::NotReady, "b", None, None);
        assert_eq!(
            why(&f.check(&["c1"]), "c1"),
            Some(Uncovered::NotReady { review: later })
        );
        let own = f.review(&["c9"], Verdict::Ready, "w1", Some("w1"), None);
        assert_eq!(
            why(&f.check(&["c9"]), "c9"),
            Some(Uncovered::SelfReview { review: own })
        );
        f.review(&["c9"], Verdict::Ready, "lead", Some("w1"), None);
        assert!(f.check(&["c9"]).ok, "another reviewer covers it");
    }

    #[test]
    fn a_retracted_review_covers_nothing_and_its_row_stays() {
        let f = Fixture::new();
        let id = f.review(&["c1"], Verdict::Ready, "rev", None, None);
        f.store.retract(&id, "wrong range", 99).unwrap();
        assert!(!f.check(&["c1"]).ok);
        let kept = f.store.get(&id).unwrap().unwrap();
        assert_eq!(kept.retracted.unwrap().reason, "wrong range");
        assert!(f.store.retract(&id, "again", 100).is_err());
    }

    #[test]
    fn a_patch_twin_covers_only_when_asked_and_only_if_it_is_covered() {
        let f = Fixture::new();
        f.review(&["old1"], Verdict::Ready, "rev", None, None);
        f.review(&["old2"], Verdict::NotReady, "rev", None, None);
        let twins = |missing: &[String]| {
            missing
                .iter()
                .map(|m| {
                    let twin = if m == "new1" { "old1" } else { "old2" };
                    (m.clone(), vec![twin.to_owned()])
                })
                .collect::<HashMap<_, _>>()
        };
        assert!(!f.check(&["new1"]).ok);
        let got = f.check_twins(&["new1", "new2"], &twins);
        assert_eq!(got.covered, 1);
        assert_eq!(why(&got, "new2"), Some(Uncovered::NoReview));
    }

    #[test]
    fn records_are_read_by_sha_reviewer_and_repo() {
        let f = Fixture::new();
        f.review(&["c1", "c2"], Verdict::Ready, "a", None, None);
        f.review(&["c3"], Verdict::Ready, "b", None, None);
        let by_sha = f
            .store
            .query(&ReviewQuery {
                sha: Some("c2".into()),
                ..ReviewQuery::default()
            })
            .unwrap();
        assert_eq!(by_sha.len(), 1);
        assert_eq!(by_sha[0].commits, 2);
        let by_reviewer = f
            .store
            .query(&ReviewQuery {
                reviewer: Some("b".into()),
                repo: Some(PathBuf::from(REPO)),
                ..ReviewQuery::default()
            })
            .unwrap();
        assert_eq!(by_reviewer.len(), 1);
        assert_eq!(
            Verdict::parse("ready-with-changes").unwrap(),
            Verdict::ReadyWithChanges
        );
        assert_eq!(Findings::parse("high=2, info=1").unwrap().total(), 3);
        assert!(Findings::parse("severe=1").is_err());
    }
}
