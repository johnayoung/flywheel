//! Views and ranking: a candidate's phase and verdict, the ranked board,
//! and the `fanout.finished` plugin event.

use super::promote;
use super::promote::TaskOrigin;
use super::request::tail_from_line_start;
use super::{Candidate, Cleaned, Grade, GraderSpec, Pending, Winner};
use crate::agent::SessionKind;
use crate::status::{AgentStatus, StatusView};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::PathBuf;

/// Where a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Its agent is working on the prompt.
    Working,
    /// Its agent waits on a person.
    Blocked,
    /// Its agent has not reported a finished turn yet (starting, idle).
    Waiting,
    Queued,
    Grading,
    Graded,
    /// Its agent is no longer running and it was never graded.
    Ended,
    /// Its worktree was removed (after the pick).
    Removed,
}

/// What the board says of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Graded, every grader passed, nothing changed since.
    Pass,
    /// Graded and failed (a grader, a protected path, or grading itself).
    Fail,
    /// Graded, but its worktree changed since: re-grade.
    Stale,
    /// A grading is queued or running.
    Pending,
    /// Not graded yet.
    None,
}

/// A candidate on the board.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub label: String,
    pub session_id: String,
    /// The session's name (`None`: removed).
    pub session: Option<String>,
    pub kind: SessionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub branch: String,
    pub path: PathBuf,
    pub phase: Phase,
    pub verdict: Verdict,
    /// 1 = best.
    pub rank: usize,
    /// Why it stands where it does.
    pub why: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<StatusView>,
    /// The session's state (`running`, `exited`, `stopped`; `None`:
    /// removed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<Grade>,
    /// Why the grade no longer describes the worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// The worktree's HEAD now (`None`: unknown or gone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Spend so far (live while the session exists, else as last graded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub tokens: u64,
    pub gradings: u32,
    pub fixes: u32,
    pub winner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<Cleaned>,
    /// It could be picked now (fresh pass, clean, no winner yet).
    pub pickable: bool,
}

/// Counts over a fan-out's candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub candidates: usize,
    pub passing: usize,
    pub failing: usize,
    /// Queued or grading.
    pub grading: usize,
    /// Agents still at work (working, blocked, waiting).
    pub running: usize,
}

/// A fan-out with its ranked board.
#[derive(Debug, Clone, Serialize)]
pub struct FanoutView {
    pub id: String,
    pub name: String,
    pub workspace_id: String,
    pub workspace: Option<String>,
    /// The workspace folder the worktrees were cut from (what a launch
    /// names `folder`).
    pub folder: PathBuf,
    /// Deprecated mirror of `folder`.
    pub repo: PathBuf,
    pub base: String,
    pub base_sha: String,
    pub prompt: String,
    pub sent_prompt: String,
    pub graders: Vec<GraderSpec>,
    pub timeout_secs: u64,
    pub protect: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<Winner>,
    /// Best first.
    pub candidates: Vec<CandidateView>,
    pub counts: Counts,
    /// The best candidate, when it passes.
    pub leader: Option<String>,
    /// Nothing is left to wait for: no agent at work, no grading queued.
    pub settled: bool,
    pub graded_at: Option<DateTime<Utc>>,
    /// Its latest grading is unseen: Needs you lists it, and it counts as
    /// one done in its workspace ([`crate::fanout::Fanout::needs_you`]).
    pub needs_you: bool,
    /// What runs the graders (`None`: this hub cannot grade).
    pub runner: Option<String>,
    /// What the board should say of the fan-out as a whole (the graders
    /// pass on the base unchanged, ...).
    pub warnings: Vec<String>,
    /// A list entry: no grader output, worktree reads up to
    /// half a minute old (`GET /api/fanouts/{id}` has the full board).
    pub summary: bool,
    /// The flywheel task it was seeded from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// The flywheel task files written from it, and where each is now.
    pub promoted: Vec<promote::PromotedView>,
}

impl FanoutView {
    /// Drop what only a board needs (grader output).
    pub fn summarize(&mut self) {
        for candidate in &mut self.candidates {
            if let Some(grade) = &mut candidate.grade {
                for run in &mut grade.graders {
                    run.stdout.clear();
                    run.stderr.clear();
                }
            }
        }
        self.summary = true;
    }
}

/// A candidate's phase from its record and session.
pub fn phase_of(
    candidate: &Candidate,
    session: Option<(&str, bool, Option<AgentStatus>)>,
) -> Phase {
    if candidate.cleaned.is_some() {
        return Phase::Removed;
    }
    if let Some(pending) = &candidate.pending {
        return if pending.started_at.is_some() {
            Phase::Grading
        } else {
            Phase::Queued
        };
    }
    if candidate.grade.is_some() {
        return Phase::Graded;
    }
    match session {
        Some(("running", true, status)) => match status {
            Some(AgentStatus::Working) => Phase::Working,
            Some(AgentStatus::Blocked) => Phase::Blocked,
            _ => Phase::Waiting,
        },
        _ => Phase::Ended,
    }
}

/// The verdict from a candidate's grade, pending grading, and staleness.
pub fn verdict_of(candidate: &Candidate, stale: Option<&str>) -> Verdict {
    match (&candidate.grade, &candidate.pending) {
        (_, Some(_)) => Verdict::Pending,
        (Some(_), None) if stale.is_some() => Verdict::Stale,
        (Some(grade), None) if grade.passed => Verdict::Pass,
        (Some(_), None) => Verdict::Fail,
        (None, None) => Verdict::None,
    }
}

fn money(cost: Option<f64>) -> String {
    match cost {
        Some(cost) => crate::util::dollars(cost),
        None => "unpriced".to_owned(),
    }
}

/// Two costs as the board shows them, with as many more decimals as it
/// takes to tell them apart (cents round two cheap runs to one figure:
/// "$0.0060 vs $0.0060" explains nothing).
fn money_apart(a: Option<f64>, b: Option<f64>) -> (String, String) {
    let (shown_a, shown_b) = (money(a), money(b));
    let (Some(x), Some(y)) = (a, b) else {
        return (shown_a, shown_b);
    };
    if shown_a != shown_b || x == y {
        return (shown_a, shown_b);
    }
    (5..=8)
        .map(|digits| (format!("${x:.digits$}"), format!("${y:.digits$}")))
        .find(|(p, q)| p != q)
        .unwrap_or((format!("${x:.8}"), format!("${y:.8}")))
}

/// What `v` had cost when it was graded (what ranking compares).
fn graded_cost(v: &CandidateView) -> Option<f64> {
    v.grade.as_ref().and_then(|g| g.cost_usd)
}

/// Order the board and say why: passing first (smaller diff, then lower
/// cost when graded), then failing (more graders passed, smaller diff,
/// lower cost),
/// then stale, pending, and ungraded ones in launch order. Fills `rank`
/// and `why`.
pub fn rank(views: &mut [CandidateView]) {
    let diff = |v: &CandidateView| v.grade.as_ref().map_or(u64::MAX, |g| g.diff.lines());
    let noop = |v: &CandidateView| v.grade.as_ref().is_some_and(Grade::no_change);
    let passed = |v: &CandidateView| v.grade.as_ref().map_or(0, Grade::graders_passed);
    // The cost recorded with its grade, not the live one: usage keeps
    // arriving after a turn ends, and a leader must not change (after
    // `fanout.finished` named it) until a grading changes.
    let cost = |v: &CandidateView| graded_cost(v).unwrap_or(f64::INFINITY);
    let launch = |v: &CandidateView| {
        v.label
            .trim_start_matches('c')
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    };
    views.sort_by(|a, b| {
        a.verdict
            .cmp(&b.verdict)
            .then_with(|| match a.verdict {
                // A pass that changes nothing has nothing to merge: last.
                Verdict::Pass => noop(a)
                    .cmp(&noop(b))
                    .then_with(|| diff(a).cmp(&diff(b)))
                    .then_with(|| cost(a).total_cmp(&cost(b))),
                Verdict::Fail => passed(b)
                    .cmp(&passed(a))
                    .then_with(|| diff(a).cmp(&diff(b)))
                    .then_with(|| cost(a).total_cmp(&cost(b))),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| launch(a).cmp(&launch(b)))
    });
    let passing = views.iter().filter(|v| v.verdict == Verdict::Pass).count();
    for i in 0..views.len() {
        let mut why = Vec::new();
        let v = &views[i];
        let graders = v.grade.as_ref().map_or(0, |g| g.graders.len());
        match v.verdict {
            Verdict::Pass => {
                why.push(format!(
                    "passes all {graders} grader{}",
                    if graders == 1 { "" } else { "s" }
                ));
                if noop(v) {
                    why.push("changes nothing vs base: nothing to merge".to_owned());
                }
            }
            Verdict::Fail => {
                if let Some(failure) = v.grade.as_ref().and_then(Grade::failure) {
                    why.push(failure);
                }
                if let Some(grade) = &v.grade {
                    why.push(format!("{}/{graders} graders pass", grade.graders_passed()));
                }
            }
            Verdict::Stale => {
                why.push(format!(
                    "{}: re-grade",
                    v.stale.as_deref().unwrap_or("changed since graded")
                ));
            }
            Verdict::Pending => why.push(
                if v.phase == Phase::Grading {
                    "grading now"
                } else {
                    "queued for grading"
                }
                .to_owned(),
            ),
            Verdict::None => why.push(
                match v.phase {
                    Phase::Working => "its agent is still working",
                    Phase::Blocked => "its agent waits on you",
                    Phase::Waiting => "waiting for its agent to report a finished turn",
                    Phase::Removed => "its worktree was removed",
                    _ => "its agent ended without a finished turn: grade it by hand",
                }
                .to_owned(),
            ),
        }
        if let Some(grade) = &v.grade
            && matches!(v.verdict, Verdict::Pass | Verdict::Fail)
        {
            why.push(format!(
                "{} line{} in {} file{} vs base",
                grade.diff.lines(),
                if grade.diff.lines() == 1 { "" } else { "s" },
                grade.diff.files,
                if grade.diff.files == 1 { "" } else { "s" }
            ));
        }
        if matches!(v.verdict, Verdict::Pass | Verdict::Fail) {
            if let Some(above) = i.checked_sub(1).map(|j| &views[j])
                && above.verdict == v.verdict
            {
                why.push(versus(v, above, "below", &diff, &cost, &passed));
            } else if let Some(below) = views.get(i + 1)
                && below.verdict == v.verdict
            {
                why.push(versus(v, below, "ahead of", &diff, &cost, &passed));
            }
        }
        if v.verdict == Verdict::Pass && i == 0 && passing > 1 {
            why.push(format!("best of {passing} passing"));
        }
        views[i].why = why;
        views[i].rank = i + 1;
    }
}

/// Why `v` stands `relation` (`below` / `ahead of`) `other` of the same
/// verdict.
fn versus(
    v: &CandidateView,
    other: &CandidateView,
    relation: &str,
    diff: &dyn Fn(&CandidateView) -> u64,
    cost: &dyn Fn(&CandidateView) -> f64,
    passed: &dyn Fn(&CandidateView) -> usize,
) -> String {
    let who = &other.label;
    let noop = |c: &CandidateView| c.grade.as_ref().is_some_and(Grade::no_change);
    if v.verdict == Verdict::Pass && noop(v) != noop(other) {
        return if noop(v) {
            format!("{relation} {who}: it changes nothing")
        } else {
            format!("{relation} {who}: {who} changes nothing")
        };
    }
    if v.verdict == Verdict::Fail && passed(v) != passed(other) {
        let word = if passed(v) > passed(other) {
            "more"
        } else {
            "fewer"
        };
        return format!(
            "{relation} {who}: {word} graders pass ({} vs {})",
            passed(v),
            passed(other)
        );
    }
    if diff(v) != diff(other) {
        let word = if diff(v) < diff(other) {
            "smaller"
        } else {
            "larger"
        };
        return format!(
            "{relation} {who}: {word} diff ({} vs {} lines)",
            diff(v),
            diff(other)
        );
    }
    if cost(v) != cost(other) {
        let word = if cost(v) < cost(other) {
            "cheaper"
        } else {
            "costs more"
        };
        let (mine, theirs) = money_apart(graded_cost(v), graded_cost(other));
        return format!("{relation} {who}: same diff, {word} when graded ({mine} vs {theirs})");
    }
    format!("{relation} {who}: same diff and cost (launch order)")
}

// --- the plugin event -------------------------------------------------------

/// Most characters of a failing grader's output a `fanout.finished` event
/// carries (the board and `fw hub fanout show` have the rest).
pub const FINISHED_TAIL_CHARS: usize = 400;

/// The `fanout.finished` plugin event's payload: a settled fan-out's ranked
/// board without grader output beyond a short tail of the first failing
/// grader. Shape (`payload.fanout`):
///
/// ```json
/// {"id", "name", "workspace_id", "workspace", "repo", "base", "base_sha",
///  "graded_at", "leader": "c2" | null, "winner": {"label","head","base","at"} | null,
///  "counts": {"candidates","passing","failing","grading","running"},
///  "warnings": [..],
///  "candidates": [{"label", "rank", "verdict": "pass"|"fail"|"stale"|"pending"|"none",
///    "passed", "kind", "model", "session_id", "session", "branch", "path", "head",
///    "graders_passed", "graders_total", "diff": {"files","insertions","deletions","untracked"},
///    "failure": "fails `unit` (exit 1)" | null, "tail": "..." | null,
///    "cost_usd", "tokens", "why": [..], "pickable"}]}
/// ```
///
/// Best first; `tail` is at most [`FINISHED_TAIL_CHARS`] characters of the
/// first failing grader's stderr (its stdout when stderr is empty).
pub fn finished_payload(view: &FanoutView) -> serde_json::Value {
    let candidates: Vec<serde_json::Value> = view
        .candidates
        .iter()
        .map(|c| {
            let grade = c.grade.as_ref();
            let tail = grade.and_then(Grade::first_failure).and_then(|run| {
                let text = [&run.stderr, &run.stdout]
                    .into_iter()
                    .map(|t| t.trim_end())
                    .find(|t| !t.is_empty())?;
                Some(tail_from_line_start(text, FINISHED_TAIL_CHARS).to_owned())
            });
            serde_json::json!({
                "label": c.label,
                "rank": c.rank,
                "verdict": c.verdict,
                "passed": c.verdict == Verdict::Pass,
                "kind": c.kind,
                "model": c.model,
                "session_id": c.session_id,
                "session": c.session,
                "branch": c.branch,
                "path": c.path,
                "head": grade.and_then(|g| g.head.clone()).or_else(|| c.head.clone()),
                "graders_passed": grade.map_or(0, Grade::graders_passed),
                "graders_total": grade.map_or(0, |g| g.graders.len()),
                "diff": grade.map(|g| &g.diff),
                "failure": grade.and_then(Grade::failure),
                "tail": tail,
                "cost_usd": c.cost_usd,
                "tokens": c.tokens,
                "why": c.why,
                "pickable": c.pickable,
            })
        })
        .collect();
    serde_json::json!({
        "fanout": {
            "id": view.id,
            "name": view.name,
            "workspace_id": view.workspace_id,
            "workspace": view.workspace,
            "folder": view.folder,
            "repo": view.repo,
            "base": view.base,
            "base_sha": view.base_sha,
            "graded_at": view.graded_at,
            "leader": view.leader,
            "winner": view.winner,
            "counts": view.counts,
            "warnings": view.warnings,
            "candidates": candidates,
        }
    })
}
