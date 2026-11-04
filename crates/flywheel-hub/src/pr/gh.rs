//! What `gh` says about a branch's pull request, its checks, a failed
//! job's log, and the repository's merge methods -- parsed into plain
//! data, its failures sorted into why (not installed, not signed in, rate
//! limited, not a GitHub repository, failed).

use crate::review::Tools;
use crate::review::runner::{self, Output, RunError, Spec};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// One read (`pr view`, `pr checks`, `repo view`).
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const READ_LIMIT: usize = 1024 * 1024;
/// A failed job's log: read at most this much (the tail of what was read
/// is kept).
const LOG_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const LOG_READ_LIMIT: usize = 16 * 1024 * 1024;
/// A git read deciding whether to ask GitHub at all.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest reason kept from gh's own words.
const REASON_CHARS: usize = 300;

/// Why gh could not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// gh is not installed.
    Missing,
    /// gh is not signed in (`gh auth login`).
    Unauthenticated,
    /// GitHub's API rate limit was hit: the hub waits before asking again.
    RateLimited,
    /// The repository has no GitHub remote gh knows.
    NotGithub,
    /// Anything else (a network error, a timeout).
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GhProblem {
    pub kind: ProblemKind,
    /// One or two lines, for a person.
    pub reason: String,
}

impl GhProblem {
    pub(crate) fn new(kind: ProblemKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    fn missing() -> Self {
        Self::new(
            ProblemKind::Missing,
            "gh (GitHub CLI) is not installed: https://cli.github.com",
        )
    }
}

impl std::fmt::Display for GhProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// gh's words cut to a few lines.
fn short(text: &str) -> String {
    let text = plain_lines(text);
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() <= REASON_CHARS {
        return joined;
    }
    let mut cut: String = joined.chars().take(REASON_CHARS - 3).collect();
    cut.push_str("...");
    cut
}

/// [`plain`] per line, the lines kept.
fn plain_lines(text: &str) -> String {
    text.lines().map(plain).collect::<Vec<_>>().join("\n")
}

/// Sort a failed gh run into why.
pub(crate) fn classify(out: &Output) -> GhProblem {
    let said = out.error_text();
    let lower = said.to_ascii_lowercase();
    if out.timed_out {
        return GhProblem::new(ProblemKind::Failed, "gh timed out talking to GitHub");
    }
    if out.code == Some(4)
        || lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("authentication required")
    {
        return GhProblem::new(
            ProblemKind::Unauthenticated,
            "gh is not signed in: run `gh auth login` (or set GH_TOKEN for the hub)",
        );
    }
    if lower.contains("rate limit") {
        return GhProblem::new(
            ProblemKind::RateLimited,
            format!("GitHub's API rate limit was hit: {}", short(&said)),
        );
    }
    if lower.contains("none of the git remotes")
        || lower.contains("no git remotes")
        || lower.contains("not a git repository")
    {
        return GhProblem::new(
            ProblemKind::NotGithub,
            "no GitHub remote: pull requests need the branch pushed to GitHub",
        );
    }
    GhProblem::new(ProblemKind::Failed, short(&said))
}

impl From<RunError> for GhProblem {
    fn from(e: RunError) -> Self {
        match e {
            RunError::NotFound(_) => Self::missing(),
            other => Self::new(ProblemKind::Failed, other.to_string()),
        }
    }
}

/// gh, resolved on `PATH` (or the configured path), in `dir`.
pub(crate) struct Gh<'a> {
    pub program: String,
    pub dir: &'a Path,
}

impl<'a> Gh<'a> {
    /// `None`-free: gh missing is a [`GhProblem`].
    pub(crate) fn find(tools: &Tools, dir: &'a Path) -> Result<Self, GhProblem> {
        let program = crate::agent::which(&tools.gh).ok_or_else(GhProblem::missing)?;
        Ok(Self {
            program: program.to_string_lossy().into_owned(),
            dir,
        })
    }

    pub(crate) fn run(
        &self,
        args: &[String],
        limit: usize,
        timeout: Duration,
    ) -> Result<Output, GhProblem> {
        Ok(runner::run(&Spec {
            program: &self.program,
            dir: self.dir,
            args: args.to_vec(),
            stdin: None,
            timeout,
            limit,
        })?)
    }

    fn read(&self, args: &[&str]) -> Result<Output, GhProblem> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        self.run(&args, READ_LIMIT, READ_TIMEOUT)
    }

    /// The pull request whose head is `branch` in this repository (open
    /// first, else the latest closed or merged one); `None`: there is
    /// none. Listed by head (`gh pr list --head`), never `gh pr view
    /// BRANCH`: gh reads `123` or `#123` there as a pull request number,
    /// so a branch named like an issue would bind to another pull request.
    pub(crate) fn pr_view(&self, branch: &str) -> Result<Option<PullRequest>, GhProblem> {
        let out = self.read(&[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "30",
            "--json",
            "number,title,url,state,isDraft,reviewDecision,mergeable,mergeStateStatus,\
             headRefName,baseRefName,headRefOid,isCrossRepository",
        ])?;
        if !out.success() {
            return Err(classify(&out));
        }
        let raw: Vec<RawPr> = serde_json::from_slice(&out.stdout).map_err(|e| {
            GhProblem::new(
                ProblemKind::Failed,
                format!("gh pr list answered something unexpected: {e}"),
            )
        })?;
        Ok(pick_pr(raw, branch))
    }

    /// Pull request `number`'s checks (none reported: empty).
    pub(crate) fn checks(&self, number: u64) -> Result<Vec<Check>, GhProblem> {
        let out = self.read(&[
            "pr",
            "checks",
            &number.to_string(),
            "--json",
            "name,state,bucket,link,startedAt,completedAt,workflow,description",
        ])?;
        // Exit 1 (some failed) and 8 (some pending) still print the list.
        if let Ok(raw) = serde_json::from_slice::<Vec<RawCheck>>(&out.stdout) {
            return Ok(raw.into_iter().map(Check::from).collect());
        }
        let said = out.error_text().to_ascii_lowercase();
        if said.contains("no checks reported") {
            return Ok(Vec::new());
        }
        if out.success() {
            return Err(GhProblem::new(
                ProblemKind::Failed,
                "gh pr checks answered something unexpected",
            ));
        }
        Err(classify(&out))
    }

    /// The repository's allowed merge methods.
    pub(crate) fn merge_methods(&self) -> Result<MergeMethods, GhProblem> {
        let out = self.read(&[
            "repo",
            "view",
            "--json",
            "mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed,viewerDefaultMergeMethod",
        ])?;
        if !out.success() {
            return Err(classify(&out));
        }
        let raw: RawRepo = serde_json::from_slice(&out.stdout).map_err(|e| {
            GhProblem::new(
                ProblemKind::Failed,
                format!("gh repo view answered something unexpected: {e}"),
            )
        })?;
        Ok(raw.into())
    }

    /// The failed steps' log of `check`'s job (or run): the argv shown,
    /// the bytes read, and whether more was there than read.
    pub(crate) fn failed_log(&self, check: &Check) -> Result<(String, Vec<u8>, bool), GhProblem> {
        let args = log_args(check).ok_or_else(|| {
            GhProblem::new(
                ProblemKind::Failed,
                format!(
                    "{} is not a GitHub Actions check: open its link for the log",
                    check.name
                ),
            )
        })?;
        let display = runner::display("gh", &args);
        let out = self.run(&args, LOG_READ_LIMIT, LOG_TIMEOUT)?;
        if !out.success() {
            return Err(classify(&out));
        }
        Ok((display, out.stdout, out.truncated))
    }
}

/// `gh run view` args for a check's failed log (`None`: not an Actions
/// check).
pub(crate) fn log_args(check: &Check) -> Option<Vec<String>> {
    let mut args = vec!["run".to_owned(), "view".to_owned()];
    match (check.job_id, check.run_id) {
        (Some(job), _) => {
            args.push("--job".into());
            args.push(job.to_string());
        }
        (None, Some(run)) => args.push(run.to_string()),
        (None, None) => return None,
    }
    args.push("--log-failed".into());
    Some(args)
}

// --- pull requests ---------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    /// From a fork: not this checkout's branch (as `gh pr view BRANCH`
    /// sees it).
    #[serde(default)]
    is_cross_repository: bool,
}

/// The pull request `branch` heads: exactly that head, in this
/// repository; open first, else the newest (gh lists newest first).
fn pick_pr(raw: Vec<RawPr>, branch: &str) -> Option<PullRequest> {
    let mine: Vec<RawPr> = raw
        .into_iter()
        .filter(|p| p.head_ref_name == branch && !p.is_cross_repository)
        .collect();
    let open = mine
        .iter()
        .position(|p| p.state.trim().eq_ignore_ascii_case("open"));
    let mut mine = mine;
    let chosen = match open {
        Some(i) => mine.swap_remove(i),
        None if mine.is_empty() => return None,
        None => mine.swap_remove(0),
    };
    Some(chosen.into())
}

/// A pull request as the hub shows it (GitHub's enum words lowercased).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `open`, `closed`, or `merged`.
    pub state: String,
    pub draft: bool,
    /// `approved`, `changes_requested`, `review_required`; `None`: no
    /// review needed or given.
    pub review: Option<String>,
    /// `mergeable`, `conflicting`, `unknown` (GitHub still computing).
    pub mergeable: Option<String>,
    /// GitHub's merge state: `clean`, `blocked`, `behind`, `dirty`,
    /// `unstable`, `draft`, `has_hooks`, `unknown`.
    pub merge_state: Option<String>,
    pub head: String,
    pub base: String,
    pub head_oid: String,
}

fn lower(word: Option<String>) -> Option<String> {
    word.map(|w| w.trim().to_ascii_lowercase())
        .filter(|w| !w.is_empty())
}

impl From<RawPr> for PullRequest {
    fn from(raw: RawPr) -> Self {
        Self {
            number: raw.number,
            title: plain(&raw.title),
            url: web_link(&raw.url).unwrap_or_default(),
            state: lower(Some(raw.state)).unwrap_or_else(|| "open".into()),
            draft: raw.is_draft,
            review: lower(raw.review_decision),
            mergeable: lower(raw.mergeable),
            merge_state: lower(raw.merge_state_status),
            head: plain(&raw.head_ref_name),
            base: plain(&raw.base_ref_name),
            head_oid: plain(&raw.head_ref_oid),
        }
    }
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// `open`, `draft`, `merged`, `closed`: what a badge says.
    pub fn shown_state(&self) -> &str {
        if self.is_open() && self.draft {
            "draft"
        } else {
            &self.state
        }
    }
}

// --- checks ----------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    workflow: String,
    #[serde(default)]
    description: String,
}

/// gh's category of a check's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Pass,
    Fail,
    Pending,
    Skipping,
    Cancel,
}

impl Bucket {
    fn parse(bucket: &str, state: &str) -> Self {
        match bucket.trim().to_ascii_lowercase().as_str() {
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            "pending" => Self::Pending,
            "skipping" => Self::Skipping,
            "cancel" => Self::Cancel,
            // An older gh without `bucket`: from the state.
            _ => match state.trim().to_ascii_uppercase().as_str() {
                "SUCCESS" | "NEUTRAL" => Self::Pass,
                "SKIPPED" => Self::Skipping,
                "CANCELLED" => Self::Cancel,
                "FAILURE" | "ERROR" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
                | "STALE" => Self::Fail,
                _ => Self::Pending,
            },
        }
    }

    /// It counts against the pull request (failed or cancelled).
    pub fn failing(self) -> bool {
        matches!(self, Self::Fail | Self::Cancel)
    }
}

/// One CI check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub bucket: Bucket,
    /// gh's state word (`SUCCESS`, `FAILURE`, `IN_PROGRESS`, ...).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Finished: how long it ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// GitHub Actions: the workflow run and job (for logs and re-runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
}

/// gh gives a never-set time as year 1.
fn time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let at = DateTime::parse_from_rfc3339(raw?.trim()).ok()?;
    (at.timestamp() > 0).then(|| at.with_timezone(&Utc))
}

/// The number after `marker` in `link`.
fn id_after(link: &str, marker: &str) -> Option<u64> {
    let at = link.find(marker)? + marker.len();
    let digits: String = link[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn non_empty(text: String) -> Option<String> {
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

impl From<RawCheck> for Check {
    fn from(raw: RawCheck) -> Self {
        let started_at = time(raw.started_at.as_deref());
        let completed_at = time(raw.completed_at.as_deref());
        let duration_secs = match (started_at, completed_at) {
            (Some(s), Some(c)) if c >= s => u64::try_from((c - s).num_seconds()).ok(),
            _ => None,
        };
        let actions = raw.link.contains("/actions/runs/");
        Self {
            bucket: Bucket::parse(&raw.bucket, &raw.state),
            run_id: actions
                .then(|| id_after(&raw.link, "/actions/runs/"))
                .flatten(),
            job_id: actions.then(|| id_after(&raw.link, "/job/")).flatten(),
            name: plain(&raw.name),
            workflow: non_empty(plain(&raw.workflow)),
            state: plain(&raw.state),
            link: web_link(&raw.link),
            description: non_empty(plain(&raw.description)),
            started_at,
            completed_at,
            duration_secs,
        }
    }
}

/// The checks at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rollup {
    /// No checks (or every one skipped).
    None,
    Pending,
    Failing,
    Passing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ChecksSummary {
    pub total: usize,
    pub passing: usize,
    /// Failed or cancelled.
    pub failing: usize,
    pub pending: usize,
    pub skipped: usize,
    /// Failing when any fails (even with others still running), else
    /// pending while any runs, else passing.
    pub rollup: Rollup,
}

impl ChecksSummary {
    pub fn of(checks: &[Check]) -> Self {
        let count = |f: fn(Bucket) -> bool| checks.iter().filter(|c| f(c.bucket)).count();
        let passing = count(|b| b == Bucket::Pass);
        let failing = count(Bucket::failing);
        let pending = count(|b| b == Bucket::Pending);
        let skipped = count(|b| b == Bucket::Skipping);
        let rollup = if failing > 0 {
            Rollup::Failing
        } else if pending > 0 {
            Rollup::Pending
        } else if passing > 0 {
            Rollup::Passing
        } else {
            Rollup::None
        };
        Self {
            total: checks.len(),
            passing,
            failing,
            pending,
            skipped,
            rollup,
        }
    }
}

// --- merge methods -----------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepo {
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
    #[serde(default)]
    viewer_default_merge_method: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

/// What the repository allows, and the method its merge button offers
/// this user first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergeMethods {
    pub allowed: Vec<MergeMethod>,
    pub default: Option<MergeMethod>,
}

impl From<RawRepo> for MergeMethods {
    fn from(raw: RawRepo) -> Self {
        let mut allowed = Vec::new();
        if raw.merge_commit_allowed {
            allowed.push(MergeMethod::Merge);
        }
        if raw.squash_merge_allowed {
            allowed.push(MergeMethod::Squash);
        }
        if raw.rebase_merge_allowed {
            allowed.push(MergeMethod::Rebase);
        }
        let default = match lower(raw.viewer_default_merge_method).as_deref() {
            Some("merge") => Some(MergeMethod::Merge),
            Some("squash") => Some(MergeMethod::Squash),
            Some("rebase") => Some(MergeMethod::Rebase),
            _ => None,
        };
        Self { allowed, default }
    }
}

impl MergeMethods {
    /// The repository's default when allowed, else the first allowed.
    pub fn pick(&self) -> Option<MergeMethod> {
        self.default
            .filter(|d| self.allowed.contains(d))
            .or_else(|| self.allowed.first().copied())
    }
}

// --- git: is the branch on a remote ------------------------------------------

/// Whether `branch` is pushed (some remote has it) and whether it is a
/// remote's default branch (no pull request is opened from it).
pub(crate) fn remote_facts(tools: &Tools, dir: &Path, branch: &str) -> Option<(bool, bool)> {
    let out = runner::run(&Spec {
        program: &tools.git,
        dir,
        args: vec![
            "--no-optional-locks".into(),
            "for-each-ref".into(),
            "--format=%(refname:short)%00%(symref:short)".into(),
            "refs/remotes".into(),
        ],
        stdin: None,
        timeout: GIT_TIMEOUT,
        limit: READ_LIMIT,
    })
    .ok()?;
    if !out.success() {
        return None;
    }
    let text = out.stdout_text();
    let suffix = format!("/{branch}");
    let mut pushed = false;
    let mut default = false;
    for line in text.lines() {
        let (name, target) = line.split_once('\0').unwrap_or((line, ""));
        if target.is_empty() && name.ends_with(&suffix) {
            pushed = true;
        }
        if !target.is_empty() && target.ends_with(&suffix) {
            default = true;
        }
    }
    Some((pushed, default))
}

// --- logs ---------------------------------------------------------------------

/// A failed log made readable: GitHub's `job<TAB>step<TAB>time line`
/// prefixes become a `== step ==` line per step, timestamps dropped.
pub(crate) fn tidy_log(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut step = String::new();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let (job, this_step, rest) = match (parts.next(), parts.next(), parts.next()) {
            (Some(job), Some(step), Some(rest)) => (job, step, rest),
            _ => {
                lines.push(plain(line));
                continue;
            }
        };
        let header = format!("{} / {}", plain(job), plain(this_step));
        if header != step {
            lines.push(format!("== {header} =="));
            step = header;
        }
        // `2024-05-01T10:00:00.1234567Z message`
        let text = match rest.split_once(' ') {
            Some((stamp, text))
                if stamp.len() >= 20 && stamp.ends_with('Z') && stamp.as_bytes()[4] == b'-' =>
            {
                text
            }
            _ => rest,
        };
        lines.push(plain(text.trim_start_matches('\u{feff}')));
    }
    lines
}

/// `text` without terminal control: ANSI/VT escape sequences (CSI colour
/// codes, OSC title and clipboard writes, bracketed-paste markers) and
/// every other control character but a tab are dropped. What gh relays
/// from GitHub (a log, a check's name) is shown in terminals and pasted
/// into agents: it must not drive either.
pub(crate) fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: to BEL or ESC \.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // ESC + intermediates + one final byte (charset picks).
                Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                    chars.next();
                }
                _ => {}
            },
            // C1 CSI: parameters then a final byte.
            '\u{9b}' => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `link` when it is a web address (`http(s)://`); anything else (a
/// `javascript:` URL) is not shown as a link.
fn web_link(link: &str) -> Option<String> {
    let link = plain(link).trim().to_owned();
    let lower = link.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(link)
}

/// The last lines of `lines` within `max_lines` and `max_bytes`: the
/// text and how many lines it holds.
pub(crate) fn tail(lines: &[String], max_lines: usize, max_bytes: usize) -> (String, usize) {
    let mut taken = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if taken.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        taken.push(line.as_str());
    }
    // One line longer than the whole budget: its end.
    if taken.is_empty()
        && max_lines > 0
        && let Some(last) = lines.last()
    {
        let mut start = last.len().saturating_sub(max_bytes);
        while !last.is_char_boundary(start) {
            start += 1;
        }
        return (last[start..].to_owned(), 1);
    }
    taken.reverse();
    (taken.join("\n"), taken.len())
}
