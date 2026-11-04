//! "While you were away": what the journal's rows over a span come to, plus
//! who is blocked now; one per hub, merged across machines.

use super::Row;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A session in a digest (blocked now).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestSession {
    pub session_id: String,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// What it waits on (as the sidebar shows it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// A fan-out graded in the span (its latest grading).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestFanout {
    pub fanout_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The picked winner, else the leader (candidate labels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<String>,
    pub passing: u64,
    pub candidates: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// One workspace's share of a digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestWorkspace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub workspace: String,
    pub turns_finished: u64,
    pub commands_suggested: u64,
    pub cost_usd: f64,
    pub events: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// What happened over `since..until`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// Agent turns that finished.
    pub turns_finished: u64,
    /// Agents blocked now (not "became blocked": still waiting).
    pub blocked_now: Vec<DigestSession>,
    pub fanouts_graded: Vec<DigestFanout>,
    /// Code blocks agents suggested.
    pub commands_suggested: u64,
    /// List-price cost of the agents' usage in the span (priced models).
    pub cost_usd: f64,
    /// Sessions that exited.
    pub exited: u64,
    /// Review actions (commit, push, PR, rebase, merge).
    pub reviews: u64,
    /// Rows in the span.
    pub events: u64,
    pub workspaces: Vec<DigestWorkspace>,
}

fn data_u64(row: &Row, key: &str) -> u64 {
    row.data.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn data_str(row: &Row, key: &str) -> Option<String> {
    row.data.get(key).and_then(Value::as_str).map(str::to_owned)
}

impl Digest {
    /// The digest of `rows` (oldest first, already within the span).
    pub fn of(rows: &[Row], since: DateTime<Utc>, until: DateTime<Utc>) -> Self {
        let mut digest = Self {
            since,
            until,
            turns_finished: 0,
            blocked_now: Vec::new(),
            fanouts_graded: Vec::new(),
            commands_suggested: 0,
            cost_usd: 0.0,
            exited: 0,
            reviews: 0,
            events: 0,
            workspaces: Vec::new(),
        };
        for row in rows {
            digest.events += 1;
            let (turns, commands, cost) = match row.kind.as_str() {
                "turn.finished" => (1, 0, 0.0),
                "commands.suggested" => (0, data_u64(row, "count"), 0.0),
                "usage" => (
                    0,
                    0,
                    row.data
                        .get("cost_usd")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                ),
                "session.exited" => {
                    digest.exited += 1;
                    (0, 0, 0.0)
                }
                "fanout.finished" => {
                    let Some(fanout_id) = data_str(row, "fanout_id") else {
                        continue;
                    };
                    // A later grading of the same fan-out replaces it.
                    digest.fanouts_graded.retain(|f| f.fanout_id != fanout_id);
                    digest.fanouts_graded.push(DigestFanout {
                        fanout_id,
                        name: data_str(row, "name").unwrap_or_default(),
                        workspace_id: row.workspace_id.clone(),
                        workspace: row.workspace.clone(),
                        winner: data_str(row, "winner"),
                        leader: data_str(row, "leader"),
                        passing: data_u64(row, "passing"),
                        candidates: data_u64(row, "candidates"),
                        machine: None,
                    });
                    (0, 0, 0.0)
                }
                kind if kind.starts_with("review.") && kind != "review.requested" => {
                    digest.reviews += 1;
                    (0, 0, 0.0)
                }
                _ => (0, 0, 0.0),
            };
            digest.turns_finished += turns;
            digest.commands_suggested += commands;
            digest.cost_usd += cost;
            let workspace = row.workspace.clone().unwrap_or_default();
            let at = match digest
                .workspaces
                .iter()
                .position(|w| w.workspace_id == row.workspace_id && w.workspace == workspace)
            {
                Some(at) => at,
                None => {
                    digest.workspaces.push(DigestWorkspace {
                        workspace_id: row.workspace_id.clone(),
                        workspace,
                        turns_finished: 0,
                        commands_suggested: 0,
                        cost_usd: 0.0,
                        events: 0,
                        machine: None,
                    });
                    digest.workspaces.len() - 1
                }
            };
            let entry = &mut digest.workspaces[at];
            entry.turns_finished += turns;
            entry.commands_suggested += commands;
            entry.cost_usd += cost;
            entry.events += 1;
        }
        digest.cost_usd = round_cents(digest.cost_usd);
        for w in &mut digest.workspaces {
            w.cost_usd = round_cents(w.cost_usd);
        }
        digest
    }

    /// Nothing happened and nobody waits.
    pub fn is_empty(&self) -> bool {
        self.events == 0 && self.blocked_now.is_empty()
    }

    /// Fold `other` (machine `machine`'s digest) into this one.
    pub fn merge(&mut self, mut other: Digest, machine: &str) {
        let label = Some(machine.to_owned());
        self.turns_finished += other.turns_finished;
        self.commands_suggested += other.commands_suggested;
        self.cost_usd = round_cents(self.cost_usd + other.cost_usd);
        self.exited += other.exited;
        self.reviews += other.reviews;
        self.events += other.events;
        for s in &mut other.blocked_now {
            s.machine.clone_from(&label);
        }
        for f in &mut other.fanouts_graded {
            f.machine.clone_from(&label);
        }
        for w in &mut other.workspaces {
            w.machine.clone_from(&label);
        }
        self.blocked_now.append(&mut other.blocked_now);
        self.fanouts_graded.append(&mut other.fanouts_graded);
        self.workspaces.append(&mut other.workspaces);
    }
}

/// `usd` to the hundredth of a cent (sums of many small costs drift).
pub fn round_cents(usd: f64) -> f64 {
    (usd * 10_000.0).round() / 10_000.0
}
