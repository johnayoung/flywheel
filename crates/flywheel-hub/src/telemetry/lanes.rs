//! The fleet in lanes: every session placed by what it needs from the
//! operator now, built from its telemetry snapshot alone (plus whether a
//! finished turn was looked at, which the hub knows).
//!
//! * `needs`: blocked on a dialog, asking, or its last turn failed.
//! * `moving`: working, and not merely waiting.
//! * `waiting`: working but waiting on time: rate limited, or one tool
//!   call running for [`WAIT_AFTER_MS`] or longer (a build, a test run,
//!   a poll).
//! * `finished`: its turn is done and nobody has looked since.
//! * `idle`: looked at, idle, or ended.
//!
//! Captured runs come as their batches (one card each): moving while a
//! run works, else finished. Cards in a lane are grouped by project, the
//! most urgent group first.

use super::reduce::Snapshot;
use super::store::BatchRollup;
use serde::Serialize;

/// A tool call running this long is a wait, not progress (ms).
pub const WAIT_AFTER_MS: i64 = 45_000;
/// Most cards one lane lists (its count is all of them).
pub const LANE_CARDS_MAX: usize = 60;

/// The lanes, in order.
pub const LANES: [(&str, &str); 5] = [
    ("needs", "Needs you"),
    ("moving", "Moving"),
    ("waiting", "Waiting on time"),
    ("finished", "Finished"),
    ("idle", "Idle"),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lane {
    pub id: &'static str,
    pub title: &'static str,
    /// Every session and batch in it.
    pub count: u64,
    /// The first [`LANE_CARDS_MAX`] of them, grouped by project.
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    pub project: String,
    pub cards: Vec<Card>,
}

/// One session or batch in a lane, and why it is there.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Card {
    /// The line that says why it is in its lane.
    pub why: String,
    /// Since when that holds (unix ms).
    pub since: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchRollup>,
}

/// What the hub knows of a session beside its telemetry.
#[derive(Debug, Clone, Default)]
pub struct Known {
    /// Its project (the hub's workspace name), when the hub has it.
    pub project: Option<String>,
    /// Its finished turn was looked at (`None`: the hub cannot tell).
    pub seen: Option<bool>,
}

/// Which lane a session is in at `now`, and why.
pub fn place(snap: &Snapshot, seen: Option<bool>, now: i64) -> (&'static str, String, i64) {
    match snap.status.as_str() {
        "blocked" => {
            let on = match (&snap.blocked_on, &snap.blocked_tool) {
                (_, Some(tool)) => format!("permission: {tool}"),
                (Some(on), None) => serde_json::to_value(on)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "a dialog".to_owned()),
                (None, None) => "a dialog".to_owned(),
            };
            ("needs", format!("blocked on {on}"), snap.since)
        }
        "asking" => (
            "needs",
            snap.ask
                .as_ref()
                .map_or_else(|| "asks you something".to_owned(), |a| a.text.clone()),
            snap.since,
        ),
        "failed" => (
            "needs",
            format!(
                "turn failed: {}",
                snap.last_error.as_deref().unwrap_or("an error")
            ),
            snap.since,
        ),
        "working" => {
            let limited = snap
                .limited_at
                .filter(|at| snap.turn_started.is_none_or(|t| *at >= t));
            if let Some(at) = limited {
                return ("waiting", "rate limited".to_owned(), at);
            }
            if let Some(tool) = snap
                .tool
                .as_ref()
                .filter(|t| now - t.since >= WAIT_AFTER_MS)
            {
                let what = tool.summary.as_deref().unwrap_or(&tool.name);
                return ("waiting", format!("{}: {what}", tool.name), tool.since);
            }
            let doing = snap
                .plan
                .as_ref()
                .and_then(|p| p.current.clone())
                .or_else(|| {
                    snap.tool.as_ref().map(|t| match &t.summary {
                        Some(summary) => format!("{}: {summary}", t.name),
                        None => t.name.clone(),
                    })
                })
                .or_else(|| snap.turn_title.clone())
                .unwrap_or_else(|| "working".to_owned());
            ("moving", doing, snap.since)
        }
        "done" if seen != Some(true) => (
            "finished",
            snap.turn_title
                .clone()
                .unwrap_or_else(|| "its turn finished".to_owned()),
            snap.since,
        ),
        "done" => ("idle", "looked at".to_owned(), snap.since),
        "ended" => ("idle", "ended".to_owned(), snap.since),
        _ => ("idle", "no prompt yet".to_owned(), snap.since),
    }
}

/// Which lane a batch is in, and why.
fn place_batch(batch: &BatchRollup) -> (&'static str, String) {
    let count = |status: &str| {
        batch
            .counts
            .iter()
            .find(|c| c.status == status)
            .map_or(0, |c| c.sessions)
    };
    let working = count("working");
    let runs = if batch.sessions == 1 { "run" } else { "runs" };
    let mut why = if working > 0 {
        format!("{working} of {} {runs} working", batch.sessions)
    } else {
        format!("{} {runs} done", batch.sessions)
    };
    if batch.over_budget {
        why.push_str(", over budget");
    }
    (if working > 0 { "moving" } else { "finished" }, why)
}

/// A project from a session's directory: the repository a worktree
/// belongs to (`repo.worktrees/task` is `repo`), else the folder's name.
pub fn project_of(dir: Option<&str>) -> String {
    let Some(dir) = dir
        .map(|d| d.trim_end_matches('/'))
        .filter(|d| !d.is_empty())
    else {
        return "elsewhere".to_owned();
    };
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    // A worker's sandbox: `$FLYWHEEL_HOME/projects/<id>/worktrees/<task>`.
    if parts
        .windows(2)
        .any(|w| w[0] == ".flywheel" && w[1] == "projects")
    {
        return "fw worker".to_owned();
    }
    if let Some(repo) = parts
        .iter()
        .rev()
        .find_map(|p| p.strip_suffix(".worktrees"))
    {
        return repo.to_owned();
    }
    parts
        .last()
        .map_or_else(|| dir.to_owned(), |p| (*p).to_owned())
}

/// The lanes of `sessions` (with what the hub knows of each) and the
/// captured runs' `batches`, at `now`.
pub fn lanes(sessions: Vec<(Snapshot, Known)>, batches: Vec<BatchRollup>, now: i64) -> Vec<Lane> {
    let mut placed: Vec<Vec<(String, Card)>> = vec![Vec::new(); LANES.len()];
    let at = |id: &str| {
        LANES
            .iter()
            .position(|(l, _)| *l == id)
            .unwrap_or(LANES.len() - 1)
    };
    for (snap, known) in sessions {
        let (lane, why, since) = place(&snap, known.seen, now);
        let project = known
            .project
            .unwrap_or_else(|| project_of(snap.dir.as_deref()));
        placed[at(lane)].push((
            project,
            Card {
                why,
                since,
                session: Some(snap),
                batch: None,
            },
        ));
    }
    for batch in batches {
        let (lane, why) = place_batch(&batch);
        placed[at(lane)].push((
            "captured runs".to_owned(),
            Card {
                why,
                since: batch.last_ts,
                session: None,
                batch: Some(batch),
            },
        ));
    }
    LANES
        .iter()
        .zip(placed)
        .map(|((id, title), mut cards)| {
            let count = cards.len() as u64;
            // The longest waiting first where a person is owed something;
            // else the latest.
            if *id == "needs" || *id == "finished" {
                cards.sort_by_key(|(_, c)| c.since);
            } else {
                cards.sort_by_key(|(_, c)| std::cmp::Reverse(c.since));
            }
            cards.truncate(LANE_CARDS_MAX);
            let mut groups: Vec<Group> = Vec::new();
            for (project, card) in cards {
                match groups.iter_mut().find(|g| g.project == project) {
                    Some(group) => group.cards.push(card),
                    None => groups.push(Group {
                        project,
                        cards: vec![card],
                    }),
                }
            }
            Lane {
                id,
                title,
                count,
                groups,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::reduce::ToolNow;

    fn snap(session: &str, status: &str, since: i64) -> Snapshot {
        let mut snap: Snapshot = serde_json::from_value(serde_json::json!({
            "host": "h", "session": session, "seq": 1, "status": status, "since": since,
            "last_ts": since, "turns": 1, "turn_tools": 0, "open_tools": 0, "tools": 0,
            "tool_errors": 0, "denied": 0, "subagents_active": 0, "subagents_total": 0,
            "errors": 0, "blocked_ms": 0, "waits": 0, "awaiting_ms": 0, "compactions": 0
        }))
        .unwrap();
        snap.dir = Some("/home/me/code/flywheel.worktrees/fix-lanes".to_owned());
        snap
    }

    #[test]
    fn each_status_finds_its_lane() {
        let now = 1_000_000;
        assert_eq!(place(&snap("a", "blocked", 0), None, now).0, "needs");
        assert_eq!(place(&snap("a", "asking", 0), None, now).0, "needs");
        assert_eq!(place(&snap("a", "failed", 0), None, now).0, "needs");
        let mut working = snap("a", "working", 0);
        working.turn_title = Some("Fix the lanes".into());
        assert_eq!(
            place(&working, None, now),
            ("moving", "Fix the lanes".into(), 0)
        );
        // A long tool call is a wait; a short one is progress.
        working.tool = Some(ToolNow {
            name: "Bash".into(),
            summary: Some("cargo test".into()),
            since: now - WAIT_AFTER_MS,
        });
        assert_eq!(
            place(&working, None, now),
            ("waiting", "Bash: cargo test".into(), now - WAIT_AFTER_MS)
        );
        working.tool.as_mut().unwrap().since = now - 1_000;
        assert_eq!(place(&working, None, now).0, "moving");
        // Rate limited during this turn.
        working.turn_started = Some(10);
        working.limited_at = Some(20);
        assert_eq!(place(&working, None, now).0, "waiting");
        // ...but not by a limit an earlier turn hit.
        working.limited_at = Some(5);
        assert_eq!(place(&working, None, now).0, "moving");
        // Done: finished until somebody looks.
        assert_eq!(place(&snap("a", "done", 0), None, now).0, "finished");
        assert_eq!(place(&snap("a", "done", 0), Some(false), now).0, "finished");
        assert_eq!(place(&snap("a", "done", 0), Some(true), now).0, "idle");
        assert_eq!(place(&snap("a", "ended", 0), None, now).0, "idle");
        assert_eq!(place(&snap("a", "idle", 0), None, now).0, "idle");
    }

    #[test]
    fn projects_are_repositories_not_worktrees() {
        assert_eq!(project_of(Some("/c/flywheel.worktrees/fix")), "flywheel");
        assert_eq!(
            project_of(Some("/c/flywheel.worktrees/fix/crates/hub")),
            "flywheel"
        );
        assert_eq!(project_of(Some("/c/infrared/")), "infrared");
        assert_eq!(project_of(None), "elsewhere");
        assert_eq!(
            project_of(Some("/h/.flywheel/projects/8b/worktrees/landing-queue")),
            "fw worker"
        );
    }

    #[test]
    fn cards_are_grouped_by_project_with_the_longest_wait_first() {
        let mut other = snap("c", "asking", 50);
        other.dir = Some("/c/infrared".into());
        let sessions = vec![
            (snap("a", "asking", 300), Known::default()),
            (other, Known::default()),
            (snap("b", "asking", 100), Known::default()),
            (
                snap("d", "done", 10),
                Known {
                    project: Some("my workspace".into()),
                    seen: Some(false),
                },
            ),
        ];
        let lanes = lanes(sessions, Vec::new(), 1_000);
        let ids: Vec<&str> = lanes.iter().map(|l| l.id).collect();
        assert_eq!(ids, ["needs", "moving", "waiting", "finished", "idle"]);
        let needs = &lanes[0];
        assert_eq!(needs.count, 3);
        let groups: Vec<(&str, Vec<&str>)> = needs
            .groups
            .iter()
            .map(|g| {
                (
                    g.project.as_str(),
                    g.cards
                        .iter()
                        .map(|c| c.session.as_ref().unwrap().session.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            [("infrared", vec!["c"]), ("flywheel", vec!["b", "a"])]
        );
        assert_eq!(lanes[3].groups[0].project, "my workspace");
    }
}
