//! A session's pull request view, a failing check's log, asking the agent
//! to fix CI, and the guarded actions (re-run failed checks, mark ready,
//! merge): planned, then run against the plan's fingerprint.

use super::gh::{Check, ChecksSummary, Gh, GhProblem, MergeMethod, PullRequest, tail, tidy_log};
use super::watch::{Entry, Found, Key, Reason};
use crate::hub::{Hub, HubError, SessionView};
use crate::queue::Delivery;
use crate::review::runner;
use crate::review::{AgentRef, Ran};
use crate::state::StateError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A failed log as shown: its last lines within these.
const LOG_LINES: usize = 400;
const LOG_BYTES: usize = 64 * 1024;
/// The log tail pasted into the agent.
const FIX_LINES: usize = 150;
const FIX_BYTES: usize = 12 * 1024;
/// Failing checks named in a fix prompt.
const FIX_NAMES: usize = 10;
/// The same ask (agent, pull request, head commit, check) pasted within
/// this is not pasted again.
const FIX_REPEAT: Duration = Duration::from_secs(5 * 60);
/// One action command.
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);
/// Output kept per command run.
const RAN_BYTES: usize = 8 * 1024;

/// A session's pull request as the PR section shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PrView {
    pub session_id: String,
    pub session: String,
    pub dir: PathBuf,
    pub branch: String,
    /// Who "Ask agent to fix CI" asks (`None`: no agent runs there).
    pub agent: Option<AgentRef>,
    /// The branch's pull request (`None`: none, or not known yet).
    pub pr: Option<PullRequest>,
    pub checks: Vec<Check>,
    pub summary: Option<ChecksSummary>,
    /// Why gh could not answer the last time (the rest is older, if any).
    pub problem: Option<GhProblem>,
    /// Why GitHub is not asked (not pushed, a default branch).
    pub note: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
    /// Seconds until the hub asks again on its own.
    pub next_check_secs: Option<u64>,
    /// A refresh was asked for but GitHub was asked this many seconds ago
    /// by one: the cache answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throttled_secs: Option<u64>,
}

/// A failing check's log tail.
#[derive(Debug, Clone, Serialize)]
pub struct LogView {
    pub check: String,
    /// What read it (`gh run view --job N --log-failed`).
    pub command: String,
    pub link: Option<String>,
    /// The last [`LOG_LINES`] lines, secrets redacted.
    pub text: String,
    /// Lines in the log, and how many `text` holds.
    pub lines: usize,
    pub shown: usize,
    /// The log was longer than the hub reads: `text` is the tail of its
    /// first 16 MiB.
    pub truncated: bool,
}

/// What "Ask agent to fix CI" pasted or queued.
#[derive(Debug, Clone, Serialize)]
pub struct FixReport {
    pub agent: AgentRef,
    pub check: String,
    pub prompt: String,
    /// Set once sent: pasted now, or queued while the agent works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

/// A guarded pull request action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrAction {
    /// `gh run rerun <run> --failed` for each failing check's run.
    Rerun,
    /// `gh pr ready`.
    Ready,
    /// `gh pr merge` with the repository's method, pinned to the head
    /// commit planned against.
    Merge,
}

impl PrAction {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "rerun" => Some(Self::Rerun),
            "ready" => Some(Self::Ready),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rerun => "rerun",
            Self::Ready => "ready",
            Self::Merge => "merge",
        }
    }
}

/// What an action would do.
#[derive(Debug, Clone, Serialize)]
pub struct PrPlan {
    pub action: &'static str,
    pub summary: String,
    pub details: Vec<String>,
    pub warnings: Vec<String>,
    /// The commands, as they read in a shell (run in `dir`).
    pub commands: Vec<String>,
    pub dir: PathBuf,
    /// Hand back to run it: the pull request state it was planned on.
    pub fingerprint: String,
    /// Why it should not run now.
    pub refusal: Option<String>,
    /// The refusal can be overridden (merge with checks failing or a
    /// review missing) by typing [`Self::force_word`] back.
    pub forcible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_word: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<MergeMethod>,
    /// What forcing does and does not skip (shown when forcing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_note: Option<String>,
    #[serde(skip)]
    argv: Vec<Vec<String>>,
}

/// What an action did.
#[derive(Debug, Clone, Serialize)]
pub struct PrOutcome {
    pub action: &'static str,
    pub ok: bool,
    pub summary: String,
    pub ran: Vec<Ran>,
    /// The pull request as read right after.
    pub pr: Option<PullRequest>,
}

fn conflict(message: impl Into<String>) -> HubError {
    StateError::Conflict(message.into()).into()
}

fn fingerprint(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fence longer than any backtick run in `text`.
fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// How every fix-CI prompt for pull request `number` begins (a queued
/// one is found by it).
fn fix_marker(number: u64) -> String {
    format!("From fw hub: CI failed on pull request #{number} (")
}

/// Why `key` shows no pull request.
fn no_pr(key: &Key, entry: &Entry) -> String {
    match (&entry.problem, &entry.note, &entry.found) {
        (Some(p), _, _) => p.reason.clone(),
        (None, Some(note), _) => note.clone(),
        (None, None, Some(Found::None)) => {
            format!("{} has no pull request: open one first", key.branch)
        }
        _ => "GitHub was not asked yet: refresh".into(),
    }
}

fn short_sha(oid: &str) -> &str {
    &oid[..oid.len().min(10)]
}

/// The check `name` names (exact, else one case-insensitive match).
fn find_check<'a>(checks: &'a [Check], name: &str) -> Result<&'a Check, HubError> {
    if let Some(c) = checks.iter().find(|c| c.name == name) {
        return Ok(c);
    }
    let like: Vec<&Check> = checks
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(name))
        .collect();
    match like.as_slice() {
        [one] => Ok(one),
        _ => Err(StateError::NotFound(format!("check {name:?}")).into()),
    }
}

impl Hub {
    /// A session's view and the cache key of its branch.
    fn pr_target(&self, id: &str) -> Result<(SessionView, Key), HubError> {
        let view = self.session(id)?;
        let key = Key::of(&view.record, view.git.as_ref()).ok_or_else(|| {
            conflict(format!(
                "{} is not on a git branch (or its branch is not read yet: try again in a moment)",
                view.record.label()
            ))
        })?;
        self.pr_watch().note_session(&key, &view.record.id);
        Ok((view, key))
    }

    /// The cache entry for `key`, asking GitHub first when it never was.
    fn pr_entry(&self, key: &Key, refresh: bool) -> Entry {
        if !refresh
            && let Some(entry) = self.pr_watch().entry(key)
            && (entry.found.is_some() || entry.problem.is_some())
        {
            return entry;
        }
        self.pr_refresh(
            key,
            if refresh {
                Reason::Manual
            } else {
                Reason::Nudge
            },
        )
    }

    /// A session's pull request, checks, and why gh could not answer (if
    /// so); `refresh` asks GitHub now (a refresh within a few seconds of
    /// the last answers from the cache). Blocking.
    pub fn pr_view(&self, id: &str, refresh: bool) -> Result<PrView, HubError> {
        let (view, key) = self.pr_target(id)?;
        let throttled = refresh.then(|| self.pr_watch().throttled(&key)).flatten();
        let entry = self.pr_entry(&key, refresh);
        let snap = entry.snap();
        Ok(PrView {
            session_id: view.record.id.clone(),
            session: view.record.name.clone(),
            agent: self.review_agent(&view.record, &key.dir),
            dir: key.dir.clone(),
            branch: key.branch.clone(),
            pr: snap.map(|s| s.pr.clone()),
            checks: snap.map(|s| s.checks.clone()).unwrap_or_default(),
            summary: snap.map(|s| s.summary),
            problem: entry.problem.clone(),
            note: entry.note.clone(),
            checked_at: entry.checked_at,
            next_check_secs: entry
                .due
                .map(|d| d.saturating_duration_since(Instant::now()).as_secs()),
            throttled_secs: throttled.map(|t| t.as_secs()),
        })
    }

    /// The failed log of check `name` on a session's pull request: its
    /// last lines, secrets redacted. Blocking.
    pub fn pr_log(&self, id: &str, name: &str) -> Result<LogView, HubError> {
        let (_, key) = self.pr_target(id)?;
        let entry = self.pr_entry(&key, false);
        let snap = entry.snap().ok_or_else(|| conflict(no_pr(&key, &entry)))?;
        let check = find_check(&snap.checks, name)?;
        self.pr_check_log(&key, check)
            .map_err(|p| conflict(p.reason))
    }

    fn pr_check_log(&self, key: &Key, check: &Check) -> Result<LogView, GhProblem> {
        let watch = self.pr_watch();
        let cache_key = format!(
            "{}:{}:{}",
            key.dir.display(),
            check.run_id.unwrap_or(0),
            check.job_id.unwrap_or(0)
        );
        if let Some(view) = watch.cached_log(&cache_key) {
            return Ok(view);
        }
        let tools = watch.tools();
        let gh = Gh::find(&tools, &key.dir)?;
        let (command, bytes, truncated) = gh.failed_log(check)?;
        let raw = String::from_utf8_lossy(&bytes);
        let lines = tidy_log(&raw);
        let (text, shown) = tail(&lines, LOG_LINES, LOG_BYTES);
        let view = LogView {
            check: check.name.clone(),
            command,
            link: check.link.clone(),
            text: crate::activity::redact(&text),
            lines: lines.len(),
            shown,
            truncated,
        };
        watch.keep_log(cache_key, view.clone());
        Ok(view)
    }

    /// Ask the session's agent to fix CI: the failing check (`check`, else
    /// the first failing one), the tail of its failed log under a
    /// provenance header, secrets redacted; with `run`, pasted now when
    /// the agent is at rest, else queued for when its turn finishes.
    pub fn pr_fix(&self, id: &str, check: Option<&str>, run: bool) -> Result<FixReport, HubError> {
        let (view, key) = self.pr_target(id)?;
        let entry = self.pr_entry(&key, false);
        let snap = entry.snap().ok_or_else(|| conflict(no_pr(&key, &entry)))?;
        if !snap.pr.is_open() {
            return Err(conflict(format!(
                "pull request #{} is {}: nothing to fix",
                snap.pr.number, snap.pr.state
            )));
        }
        let failing: Vec<&Check> = snap.checks.iter().filter(|c| c.bucket.failing()).collect();
        let chosen = match check {
            Some(name) => {
                let found = find_check(&snap.checks, name)?;
                if !found.bucket.failing() {
                    return Err(conflict(format!("check {:?} is not failing", found.name)));
                }
                found
            }
            None => failing
                .iter()
                .find(|c| c.run_id.is_some())
                .or_else(|| failing.first())
                .copied()
                .ok_or_else(|| {
                    conflict(format!(
                        "no failing checks on pull request #{}",
                        snap.pr.number
                    ))
                })?,
        };
        let agent = self.review_agent(&view.record, &key.dir).ok_or_else(|| {
            conflict(format!(
                "no agent session runs in {} to ask",
                key.dir.display()
            ))
        })?;
        let watch = self.pr_watch();
        if let Some(ago) = watch.asked_lately(
            &agent.id,
            snap.pr.number,
            &snap.pr.head_oid,
            &chosen.name,
            FIX_REPEAT,
        ) {
            return Err(conflict(format!(
                "{} was asked to fix {} on pull request #{} at {} {} ago already: \
                 give it time to push, or ask again in its terminal",
                agent.name,
                chosen.name,
                snap.pr.number,
                short_sha(&snap.pr.head_oid),
                if ago.as_secs() < 60 {
                    format!("{}s", ago.as_secs())
                } else {
                    format!("{} min", ago.as_secs() / 60)
                }
            )));
        }
        let log = self.pr_check_log(&key, chosen);
        let prompt = fix_prompt(&snap.pr, &failing, chosen, &key.branch, log);
        let mut report = FixReport {
            agent,
            check: chosen.name.clone(),
            prompt,
            delivery: None,
        };
        if run {
            // One ask per pull request waits in the queue: a newer one
            // replaces it (its log is the latest).
            let queued = self.queued_fix_asks(&report.agent.id, snap.pr.number);
            let delivery = match queued.first() {
                Some(item) => self
                    .queue_edit(
                        &report.agent.id,
                        item,
                        &crate::queue::ItemEdit {
                            text: Some(report.prompt.clone()),
                            when: None,
                        },
                    )
                    .map(|view| Delivery::Queued {
                        item: item.clone(),
                        why: "it replaces the ask already queued".into(),
                        pending: view.pending,
                    }),
                None => self.send_or_queue(&report.agent.id, &report.prompt),
            };
            if let Ok(Delivery::Pasted { .. }) = &delivery {
                watch.note_asked(
                    &report.agent.id,
                    snap.pr.number,
                    &snap.pr.head_oid,
                    &chosen.name,
                );
            }
            eprintln!(
                "fw hub: pr #{} ({}): asked {} to fix {} -> {}",
                snap.pr.number,
                key.branch,
                report.agent.name,
                chosen.name,
                match &delivery {
                    Ok(Delivery::Pasted { .. }) => "pasted".to_owned(),
                    Ok(Delivery::Queued { why, .. }) => format!("queued ({why})"),
                    Err(e) => format!("failed: {e}"),
                }
            );
            report.delivery = Some(delivery?);
        }
        Ok(report)
    }

    /// What `action` would do (nothing runs). Blocking.
    pub fn pr_plan(&self, id: &str, action: PrAction) -> Result<PrPlan, HubError> {
        let (_, key) = self.pr_target(id)?;
        let entry = self.pr_entry(&key, false);
        Ok(self.pr_plan_on(&key, &entry, action))
    }

    fn pr_plan_on(&self, key: &Key, entry: &Entry, action: PrAction) -> PrPlan {
        let mut plan = PrPlan {
            action: action.as_str(),
            summary: String::new(),
            details: Vec::new(),
            warnings: Vec::new(),
            commands: Vec::new(),
            dir: key.dir.clone(),
            fingerprint: String::new(),
            refusal: None,
            forcible: false,
            force_word: None,
            method: None,
            force_note: None,
            argv: Vec::new(),
        };
        let Some(snap) = entry.snap() else {
            plan.summary = if entry.problem.is_some() {
                format!("The pull request of {} is not known", key.branch)
            } else {
                format!("No pull request for {}", key.branch)
            };
            plan.refusal = Some(no_pr(key, entry));
            return plan;
        };
        let pr = &snap.pr;
        let n = pr.number.to_string();
        let mut add = |args: Vec<String>, plan: &mut PrPlan| {
            plan.commands.push(format!(
                "(cd {} && {})",
                runner::quote(&key.dir.to_string_lossy()),
                runner::display("gh", &args)
            ));
            plan.argv.push(args);
        };
        if let Some(problem) = &entry.problem {
            plan.warnings
                .push(format!("what is shown may be stale: {}", problem.reason));
        }
        match action {
            PrAction::Rerun => {
                let failing: Vec<&Check> =
                    snap.checks.iter().filter(|c| c.bucket.failing()).collect();
                let runs: BTreeSet<u64> = failing.iter().filter_map(|c| c.run_id).collect();
                plan.summary = format!(
                    "Re-run the failed jobs of {} on pull request #{n}",
                    crate::util::plural(runs.len(), "workflow run")
                );
                plan.details = failing.iter().map(|c| c.name.clone()).collect();
                for run in &runs {
                    add(
                        vec![
                            "run".into(),
                            "rerun".into(),
                            run.to_string(),
                            "--failed".into(),
                        ],
                        &mut plan,
                    );
                }
                if !pr.is_open() {
                    plan.refusal = Some(format!("pull request #{n} is {}", pr.state));
                } else if failing.is_empty() {
                    plan.refusal = Some("no check is failing".into());
                } else if runs.is_empty() {
                    plan.refusal = Some(
                        "the failing checks are not GitHub Actions runs: re-run them where they run"
                            .into(),
                    );
                } else if failing.len() > failing.iter().filter(|c| c.run_id.is_some()).count() {
                    plan.warnings
                        .push("some failing checks are not GitHub Actions runs: not re-run".into());
                }
                let runs: Vec<String> = runs.iter().map(u64::to_string).collect();
                let mut parts = vec!["rerun", &n, &pr.head_oid];
                parts.extend(runs.iter().map(String::as_str));
                plan.fingerprint = fingerprint(&parts);
            }
            PrAction::Ready => {
                plan.summary = format!("Mark pull request #{n} ready for review");
                add(vec!["pr".into(), "ready".into(), n.clone()], &mut plan);
                if !pr.is_open() {
                    plan.refusal = Some(format!("pull request #{n} is {}", pr.state));
                } else if !pr.draft {
                    plan.refusal = Some(format!("pull request #{n} is ready for review already"));
                }
                plan.fingerprint = fingerprint(&[
                    "ready",
                    &n,
                    &pr.head_oid,
                    if pr.draft { "draft" } else { "ready" },
                ]);
            }
            PrAction::Merge => self.plan_merge(key, snap, &mut plan, &mut add),
        }
        plan
    }

    fn plan_merge(
        &self,
        key: &Key,
        snap: &super::watch::Snap,
        plan: &mut PrPlan,
        add: &mut impl FnMut(Vec<String>, &mut PrPlan),
    ) {
        let pr = &snap.pr;
        let n = pr.number.to_string();
        let watch = self.pr_watch();
        let methods = match watch.fresh_methods(key) {
            Some(m) => Ok(m),
            None => Gh::find(&watch.tools(), &key.dir)
                .and_then(|gh| gh.merge_methods())
                .inspect(|m| watch.keep_methods(key, m.clone())),
        };
        let method = methods
            .as_ref()
            .ok()
            .and_then(super::gh::MergeMethods::pick);
        plan.method = method;
        plan.summary = format!(
            "Merge pull request #{n} ({} into {}){}",
            pr.head,
            pr.base,
            method
                .map(|m| format!(" by {}", m.as_str()))
                .unwrap_or_default()
        );
        plan.details = vec![pr.title.clone()];
        if let Some(m) = method {
            add(
                vec![
                    "pr".into(),
                    "merge".into(),
                    n.clone(),
                    m.flag().into(),
                    "--match-head-commit".into(),
                    pr.head_oid.clone(),
                ],
                plan,
            );
        }
        plan.warnings.push(format!(
            "it merges on GitHub: {} in your checkouts moves only when you pull",
            pr.base
        ));
        // Never overridden here.
        let hard = if !pr.is_open() {
            Some(format!("pull request #{n} is {} already", pr.state))
        } else if pr.draft {
            Some(format!(
                "pull request #{n} is a draft: mark it ready for review first"
            ))
        } else if pr.mergeable.as_deref() == Some("conflicting") {
            Some(format!(
                "pull request #{n} conflicts with {}: rebase the branch onto it (Review: Rebase) and push",
                pr.base
            ))
        } else {
            match &methods {
                Err(problem) => Some(problem.reason.clone()),
                Ok(_) if method.is_none() => Some("the repository allows no merge method".into()),
                Ok(_) => None,
            }
        };
        // Overridden by typing the pull request's number back.
        let mut soft = Vec::new();
        let failing: Vec<&str> = snap
            .checks
            .iter()
            .filter(|c| c.bucket.failing())
            .map(|c| c.name.as_str())
            .collect();
        if !failing.is_empty() {
            soft.push(format!(
                "{} failing ({})",
                crate::util::plural(failing.len(), "check"),
                failing.join(", ")
            ));
        }
        if snap.summary.pending > 0 {
            soft.push(format!(
                "{} still running",
                crate::util::plural(snap.summary.pending, "check")
            ));
        }
        match pr.review.as_deref() {
            Some("changes_requested") => soft.push("changes were requested".into()),
            Some("review_required") => soft.push("a review is required".into()),
            _ => {}
        }
        if let Some(why) = hard {
            plan.refusal = Some(why);
        } else if !soft.is_empty() {
            plan.refusal = Some(format!("not merged while {}", soft.join("; ")));
            plan.forcible = true;
            plan.force_word = Some(n.clone());
            plan.force_note = Some(
                "forcing skips the hub's checks only: GitHub's branch protection still applies (no --admin)"
                    .into(),
            );
        }
        let rollup = format!("{:?}", snap.summary.rollup);
        plan.fingerprint = fingerprint(&[
            "merge",
            &n,
            &pr.head_oid,
            method.map_or("", MergeMethod::as_str),
            &pr.state,
            if pr.draft { "draft" } else { "" },
            pr.review.as_deref().unwrap_or(""),
            pr.mergeable.as_deref().unwrap_or(""),
            &rollup,
        ]);
    }

    /// Run `action` as planned (`confirm`: the plan's fingerprint),
    /// against the pull request read afresh; a forcible refusal runs only
    /// with `force` equal to its force word. Logged. Blocking.
    pub fn pr_run(
        &self,
        id: &str,
        action: PrAction,
        confirm: &str,
        force: Option<&str>,
    ) -> Result<PrOutcome, HubError> {
        let (view, key) = self.pr_target(id)?;
        let entry = self.pr_refresh(&key, Reason::Nudge);
        let plan = self.pr_plan_on(&key, &entry, action);
        let label = view.record.label();
        let refuse = |why: String| {
            eprintln!("fw hub: pr {label}: {}: refused: {why}", action.as_str());
            conflict(why)
        };
        // Acting needs GitHub read now, not a cached answer (a rate-limit
        // pause, a network error).
        if let Some(problem) = &entry.problem {
            return Err(refuse(format!(
                "GitHub could not be read just now, so nothing runs on what may be stale: {}",
                problem.reason
            )));
        }
        if plan.fingerprint.is_empty() || plan.fingerprint != confirm.trim() {
            return Err(refuse(
                "the pull request changed since this was planned (or the plan is not this one): look again"
                    .into(),
            ));
        }
        let mut forced = false;
        if let Some(why) = &plan.refusal {
            let word = plan.force_word.as_deref();
            if plan.forcible && word.is_some() && force.map(str::trim) == word {
                forced = true;
            } else if plan.forcible {
                return Err(refuse(format!(
                    "{why} (to merge anyway, force it with the pull request's number, {})",
                    word.unwrap_or_default()
                )));
            } else {
                return Err(refuse(why.clone()));
            }
        }
        let tools = self.pr_watch().tools();
        let gh = Gh::find(&tools, &key.dir).map_err(|p| conflict(p.reason))?;
        let mut ran = Vec::new();
        let mut ok = true;
        for (args, display) in plan.argv.iter().zip(&plan.commands) {
            let out = gh
                .run(args, RAN_BYTES, ACTION_TIMEOUT)
                .map_err(|p| conflict(p.reason))?;
            let mut output = out.stdout_text().trim().to_owned();
            let err = out.stderr.trim();
            if !err.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(err);
            }
            if output.len() > RAN_BYTES {
                let mut cut = RAN_BYTES;
                while !output.is_char_boundary(cut) {
                    cut -= 1;
                }
                output.truncate(cut);
            }
            let success = out.success();
            ran.push(Ran {
                display: display.clone(),
                code: out.code,
                output: crate::activity::redact(&output),
            });
            if !success {
                ok = false;
                break;
            }
        }
        let summary = if ok {
            plan.summary.clone()
        } else {
            format!(
                "{} failed: {}",
                plan.summary,
                ran.last().map(|r| r.output.as_str()).unwrap_or_default()
            )
        };
        eprintln!(
            "fw hub: pr {label}: {}{}: {} -> {}",
            action.as_str(),
            if forced { " (forced)" } else { "" },
            plan.commands.join("; "),
            if ok { "ok" } else { "failed" }
        );
        // What it changed, and the events it makes.
        self.pr_watch().nudge_dir(&key.dir);
        let after = self.pr_refresh(&key, Reason::Nudge);
        Ok(PrOutcome {
            action: action.as_str(),
            ok,
            summary,
            ran,
            pr: after.snap().map(|s| s.pr.clone()),
        })
    }
}

impl Hub {
    /// Pending queue items of session `id` asking it to fix CI on pull
    /// request `number`.
    fn queued_fix_asks(&self, id: &str, number: u64) -> Vec<String> {
        let marker = fix_marker(number);
        self.store()
            .queue(id)
            .map(|q| {
                q.items
                    .iter()
                    .filter(|i| {
                        matches!(i.state, crate::queue::ItemState::Pending)
                            && i.text.starts_with(&marker)
                    })
                    .map(|i| i.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remove the fix-CI asks still queued for `pr` in the sessions on
    /// `key`: its checks are no longer failing (or it is closed or
    /// merged), so they would only send the agent after a stale failure.
    pub(crate) fn drop_stale_fix_asks(&self, key: &Key, pr: &PullRequest) {
        let marker = fix_marker(pr.number);
        let candidates: Vec<String> = self
            .store()
            .queues()
            .iter()
            .filter(|(_, q)| {
                q.items.iter().any(|i| {
                    matches!(i.state, crate::queue::ItemState::Pending)
                        && i.text.starts_with(&marker)
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in candidates {
            let Ok(view) = self.session(&id) else {
                continue;
            };
            if Key::of(&view.record, view.git.as_ref()).as_ref() != Some(key) {
                continue;
            }
            for item in self.queued_fix_asks(&id, pr.number) {
                if self.queue_remove(&id, &item).is_ok() {
                    eprintln!(
                        "fw hub: pr #{} ({}): dropped the queued fix-CI ask for {} (checks {}, {})",
                        pr.number,
                        key.branch,
                        view.record.label(),
                        if pr.is_open() {
                            "no longer failing"
                        } else {
                            "settled"
                        },
                        pr.shown_state()
                    );
                }
            }
        }
    }
}

/// The prompt asking the agent to fix CI.
fn fix_prompt(
    pr: &PullRequest,
    failing: &[&Check],
    chosen: &Check,
    branch: &str,
    log: Result<LogView, GhProblem>,
) -> String {
    let names: Vec<String> = failing
        .iter()
        .take(FIX_NAMES)
        .map(|c| match &c.workflow {
            Some(w) => format!("{} ({w})", c.name),
            None => c.name.clone(),
        })
        .collect();
    let more = failing.len().saturating_sub(FIX_NAMES);
    let mut prompt = format!(
        "From fw hub: CI failed on pull request #{} ({}), branch {branch} at {}.\n\
         Failing checks: {}{}.\n\n",
        pr.number,
        pr.url,
        short_sha(&pr.head_oid),
        names.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    );
    match log {
        Ok(log) => {
            let lines: Vec<String> = log.text.lines().map(str::to_owned).collect();
            let (text, shown) = tail(&lines, FIX_LINES, FIX_BYTES);
            let fence = fence_for(&text);
            prompt.push_str(&format!(
                "The last {shown} of {} lines of the failed log of \"{}\" (`{}`; secrets redacted by the hub{}):\n\n\
                 {fence}text\n{text}\n{fence}\n\n",
                log.lines,
                chosen.name,
                log.command,
                if log.truncated {
                    "; the log was longer than the hub reads"
                } else {
                    ""
                }
            ));
        }
        Err(problem) => {
            prompt.push_str(&format!(
                "No log of \"{}\" could be read ({}){}.\n\n",
                chosen.name,
                problem.reason,
                chosen
                    .link
                    .as_deref()
                    .map(|l| format!(": see {l}"))
                    .unwrap_or_default()
            ));
        }
    }
    prompt.push_str(
        "Find the cause and fix it in this work tree; run the failing check locally if you can. \
         Then commit and push the fix. Do not merge the pull request or re-run CI yourself.",
    );
    prompt
}
