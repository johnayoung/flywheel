//! A digest's contents: the journal's rows over its span and who waits on
//! you now, per workspace, with the top "needs you" items and their links;
//! and how a push and the CLI say it.
//!
//! Only names (sessions, workspaces, fan-outs, folders, autopilot tasks),
//! counts, and amounts: never terminal output, prompts, commands, what a
//! blocked agent asks, a gate's instruction, grader output, or paths. The
//! names come from journal rows (already scrubbed) or live state, which is
//! scrubbed here the same way ([`crate::activity::scrub`]).

use super::schedule;
use crate::activity::{Row, scrub};
use crate::util::dollars;
use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The most "needs you" items a digest lists.
pub const NEEDS_YOU: usize = 3;
/// Longest push message (characters; ntfy takes 4 KiB).
const PUSH_MAX_CHARS: usize = 1800;
/// Workspaces a push lists one line each (the rest are counted).
const PUSH_WORKSPACES: usize = 6;

/// What a digest (or one workspace's share) counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub turns_finished: u64,
    /// Agents blocked when the digest was made.
    pub blocked_now: u64,
    pub fanouts_graded: u64,
    /// Pull requests by their latest state in the span.
    pub prs_red: u64,
    pub prs_green: u64,
    pub prs_merged: u64,
    pub autopilot_decisions: u64,
    /// Manual gates waiting when the digest was made.
    pub gates_waiting: u64,
    pub queue_stalls: u64,
    /// Agents' list-price spend since local midnight / Monday 00:00.
    pub spend_today_usd: f64,
    pub spend_week_usd: f64,
}

impl Counts {
    /// One line of the nonzero counts (`3 turns finished, 1 blocked, ...`).
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        let mut add = |n: u64, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        add(self.turns_finished, "turn finished", "turns finished");
        add(self.blocked_now, "blocked", "blocked");
        add(self.fanouts_graded, "fan-out graded", "fan-outs graded");
        add(self.prs_red, "PR red", "PRs red");
        add(self.prs_green, "PR green", "PRs green");
        add(self.prs_merged, "PR merged", "PRs merged");
        add(
            self.autopilot_decisions,
            "autopilot decision",
            "autopilot decisions",
        );
        add(self.gates_waiting, "gate waiting", "gates waiting");
        add(self.queue_stalls, "queue stall", "queue stalls");
        if self.spend_today_usd > 0.0 || self.spend_week_usd > 0.0 {
            parts.push(format!(
                "{} today, {} this week",
                dollars(self.spend_today_usd),
                dollars(self.spend_week_usd)
            ));
        }
        parts.join(", ")
    }
}

/// A session named in a digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Named {
    pub session_id: String,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

/// A fan-out graded in the span (its latest grading).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanoutLine {
    pub fanout_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<String>,
    pub passing: u64,
    pub candidates: u64,
}

impl FanoutLine {
    fn says(&self) -> String {
        let lead = match (&self.winner, &self.leader) {
            (Some(w), _) => format!("winner {w}"),
            (None, Some(l)) => format!("{l} leads"),
            (None, None) => "none passing".to_owned(),
        };
        format!(
            "{}: {lead} ({}/{})",
            self.name, self.passing, self.candidates
        )
    }
}

/// One workspace's share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDigest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub workspace: String,
    #[serde(flatten)]
    pub counts: Counts,
    /// Blocked now.
    #[serde(default)]
    pub blocked: Vec<Named>,
    #[serde(default)]
    pub fanouts: Vec<FanoutLine>,
}

/// Where a "needs you" item opens: the hub page's fragment, and the full
/// URL when the hub has one a phone reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// `#session=<id>`, `#gate=<run id>`, `#digest=<id>`.
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Link {
    pub fn new(hash: String, hub_url: Option<&str>) -> Self {
        let url = hub_url.map(|base| format!("{}/{hash}", base.trim_end_matches('/')));
        Self { hash, url }
    }
}

/// Something waiting on a person, most pressing first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeedsYou {
    /// `gate`, `blocked`, `pr_red`, `queue_stalled`.
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    pub link: Link,
}

/// What a delivery reached.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// `push`, `browser`, `plugin`, as delivered.
    pub channels: Vec<String>,
    /// Why the push did not go (its host and the failure; never the URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_error: Option<String>,
    /// Nothing in it and `quiet_if_empty`: not delivered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// Channels configured that nothing received, each `channel: why`
    /// (no browser open, no plugin listening, push not set up).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unheard: Vec<String>,
}

/// One digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// `dg-<slot, unix s>` (scheduled), `dg-m<unix ms>` (sent by hand),
    /// `preview`.
    pub id: String,
    /// `scheduled`, `manual`, `preview`.
    pub trigger: String,
    /// `morning`, `afternoon`, `evening` (the slot's local time), or
    /// `manual`.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// The IANA zone "today" and "this week" are counted in.
    pub timezone: String,
    pub totals: Counts,
    pub workspaces: Vec<WorkspaceDigest>,
    pub needs_you: Vec<NeedsYou>,
    /// Nothing happened in the span and nobody waits.
    pub empty: bool,
    /// The hub page (`<url>/#digest=<id>`), when the hub has a public URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

// --- what the hub has now --------------------------------------------------

/// An agent blocked now.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveBlocked {
    pub session_id: String,
    pub session: String,
    pub workspace_id: String,
    pub since: DateTime<Utc>,
}

/// A manual gate waiting (a Flywheel folder's run parked on approval).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveGate {
    pub workspace_id: Option<String>,
    /// The folder's name (never its path).
    pub folder: String,
    pub task_id: String,
    pub run_id: String,
    pub since: Option<DateTime<Utc>>,
}

/// An open pull request whose checks fail now.
#[derive(Debug, Clone, PartialEq)]
pub struct LivePr {
    pub session_id: String,
    pub session: String,
    pub workspace_id: String,
    pub number: u64,
}

/// A prompt queue stalled now.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveStall {
    pub session_id: String,
    pub session: String,
    pub workspace_id: String,
    pub at: DateTime<Utc>,
}

/// The hub's state when the digest is made.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    /// Every workspace: (id, name).
    pub workspaces: Vec<(String, String)>,
    /// Each workspace folder's name and its workspace's id (autopilot rows
    /// name the folder).
    pub folders: Vec<(String, String)>,
    pub blocked: Vec<LiveBlocked>,
    pub gates: Vec<LiveGate>,
    pub red_prs: Vec<LivePr>,
    pub stalled: Vec<LiveStall>,
}

/// What a digest is made of.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub id: String,
    pub trigger: &'a str,
    pub label: String,
    pub slot: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub tz: Tz,
    /// Journal rows from [`rows_from`] to `until`, oldest first.
    pub rows: &'a [Row],
    pub live: &'a Live,
    pub hub_url: Option<&'a str>,
}

/// The earliest row a digest of `since..until` reads: its span, and the
/// week its spend is counted over.
pub fn rows_from(tz: &Tz, since: DateTime<Utc>, until: DateTime<Utc>) -> DateTime<Utc> {
    since.min(schedule::week_start(tz, until))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrState {
    Red,
    Green,
    Merged,
}

/// A pull request's latest state and its workspace (id, name).
type PrEntry = (PrState, Option<String>, Option<String>);

struct Builder {
    workspaces: Vec<WorkspaceDigest>,
    /// Activity in the span other than live state (what makes it not
    /// empty).
    happened: bool,
}

impl Builder {
    /// The entry for a workspace by id (else name); created on first use.
    fn entry(&mut self, id: Option<&str>, name: Option<&str>) -> Option<&mut WorkspaceDigest> {
        if id.is_none() && name.is_none() {
            return None;
        }
        let at = self.workspaces.iter().position(|w| match id {
            Some(id) => w.workspace_id.as_deref() == Some(id),
            None => w.workspace_id.is_none() && Some(w.workspace.as_str()) == name,
        });
        let at = match at {
            Some(at) => at,
            None => {
                self.workspaces.push(WorkspaceDigest {
                    workspace_id: id.map(str::to_owned),
                    workspace: scrub(name.or(id).unwrap_or_default()),
                    counts: Counts::default(),
                    blocked: Vec::new(),
                    fanouts: Vec::new(),
                });
                self.workspaces.len() - 1
            }
        };
        Some(&mut self.workspaces[at])
    }
}

fn data_str(row: &Row, key: &str) -> Option<String> {
    row.data.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn data_u64(row: &Row, key: &str) -> u64 {
    row.data.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn add_counts(total: &mut Counts, part: &Counts) {
    total.turns_finished += part.turns_finished;
    total.blocked_now += part.blocked_now;
    total.fanouts_graded += part.fanouts_graded;
    total.prs_red += part.prs_red;
    total.prs_green += part.prs_green;
    total.prs_merged += part.prs_merged;
    total.autopilot_decisions += part.autopilot_decisions;
    total.gates_waiting += part.gates_waiting;
    total.queue_stalls += part.queue_stalls;
    total.spend_today_usd += part.spend_today_usd;
    total.spend_week_usd += part.spend_week_usd;
}

fn cents(usd: f64) -> f64 {
    (usd * 10_000.0).round() / 10_000.0
}

/// Make the digest `inputs` describe.
pub fn build(inputs: &Inputs<'_>) -> Report {
    let Inputs {
        since,
        until,
        tz,
        live,
        hub_url,
        ..
    } = inputs;
    let (since, until) = (*since, *until);
    let mut b = Builder {
        workspaces: live
            .workspaces
            .iter()
            .map(|(id, name)| WorkspaceDigest {
                workspace_id: Some(id.clone()),
                workspace: scrub(name),
                counts: Counts::default(),
                blocked: Vec::new(),
                fanouts: Vec::new(),
            })
            .collect(),
        happened: false,
    };
    // Hub-level counts (autopilot in a folder no workspace has).
    let mut unplaced = Counts::default();
    let folder_ws = |folder: &str| -> Option<String> {
        live.folders
            .iter()
            .find(|(name, _)| name == folder)
            .map(|(_, ws)| ws.clone())
    };
    let today = schedule::day_start(tz, until);
    let week = schedule::week_start(tz, until);
    // (workspace key, PR number or session) -> (latest state, workspace).
    let mut prs: BTreeMap<(String, String), PrEntry> = BTreeMap::new();
    for row in inputs.rows.iter().filter(|r| r.at < until) {
        let ws_id = row.workspace_id.as_deref();
        let ws_name = row.workspace.as_deref();
        if row.kind == "usage" {
            let cost = row
                .data
                .get("cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if row.at >= since && cost > 0.0 {
                b.happened = true;
            }
            let (t, w) = (
                if row.at >= today { cost } else { 0.0 },
                if row.at >= week { cost } else { 0.0 },
            );
            match b.entry(ws_id, ws_name) {
                Some(entry) => {
                    entry.counts.spend_today_usd += t;
                    entry.counts.spend_week_usd += w;
                }
                None => {
                    unplaced.spend_today_usd += t;
                    unplaced.spend_week_usd += w;
                }
            }
            continue;
        }
        if row.at < since {
            continue;
        }
        match row.kind.as_str() {
            "turn.finished" => {
                b.happened = true;
                if let Some(entry) = b.entry(ws_id, ws_name) {
                    entry.counts.turns_finished += 1;
                } else {
                    unplaced.turns_finished += 1;
                }
            }
            "fanout.finished" => {
                let Some(fanout_id) = data_str(row, "fanout_id") else {
                    continue;
                };
                b.happened = true;
                let line = FanoutLine {
                    fanout_id,
                    name: data_str(row, "name").unwrap_or_else(|| "fan-out".to_owned()),
                    winner: data_str(row, "winner"),
                    leader: data_str(row, "leader"),
                    passing: data_u64(row, "passing"),
                    candidates: data_u64(row, "candidates"),
                };
                if let Some(entry) = b.entry(ws_id, ws_name) {
                    entry.fanouts.retain(|f| f.fanout_id != line.fanout_id);
                    entry.fanouts.push(line);
                }
            }
            "pr.checks_failed" | "pr.checks_passed" | "pr.merged" => {
                b.happened = true;
                let state = match row.kind.as_str() {
                    "pr.checks_failed" => PrState::Red,
                    "pr.checks_passed" => PrState::Green,
                    _ => PrState::Merged,
                };
                let pr = row
                    .data
                    .get("number")
                    .and_then(Value::as_u64)
                    .map(|n| format!("#{n}"))
                    .or_else(|| row.session_id.clone())
                    .unwrap_or_default();
                let key = (ws_id.or(ws_name).unwrap_or_default().to_owned(), pr);
                let merged = prs.get(&key).is_some_and(|(s, _, _)| *s == PrState::Merged);
                prs.insert(
                    key,
                    (
                        if merged { PrState::Merged } else { state },
                        row.workspace_id.clone(),
                        row.workspace.clone(),
                    ),
                );
            }
            "autopilot.decision" => {
                b.happened = true;
                let ws = data_str(row, "folder").and_then(|f| folder_ws(&f));
                match ws.as_deref().and_then(|id| b.entry(Some(id), None)) {
                    Some(entry) => entry.counts.autopilot_decisions += 1,
                    None => unplaced.autopilot_decisions += 1,
                }
            }
            "queue.stalled" => {
                b.happened = true;
                match b.entry(ws_id, ws_name) {
                    Some(entry) => entry.counts.queue_stalls += 1,
                    None => unplaced.queue_stalls += 1,
                }
            }
            _ => {}
        }
    }
    for ((_, _), (state, ws_id, ws_name)) in prs {
        let counts = match b.entry(ws_id.as_deref(), ws_name.as_deref()) {
            Some(entry) => &mut entry.counts,
            None => &mut unplaced,
        };
        match state {
            PrState::Red => counts.prs_red += 1,
            PrState::Green => counts.prs_green += 1,
            PrState::Merged => counts.prs_merged += 1,
        }
    }
    for f in &mut b.workspaces {
        f.counts.fanouts_graded = f.fanouts.len() as u64;
    }
    // Now: blocked agents and waiting gates.
    let mut blocked: Vec<&LiveBlocked> = live.blocked.iter().collect();
    blocked.sort_by_key(|s| s.since);
    for s in &blocked {
        if let Some(entry) = b.entry(Some(&s.workspace_id), None) {
            entry.counts.blocked_now += 1;
            entry.blocked.push(Named {
                session_id: s.session_id.clone(),
                session: scrub(&s.session),
                since: Some(s.since),
            });
        }
    }
    for g in &live.gates {
        match g
            .workspace_id
            .as_deref()
            .and_then(|id| b.entry(Some(id), None))
        {
            Some(entry) => entry.counts.gates_waiting += 1,
            None => unplaced.gates_waiting += 1,
        }
    }
    let ws_name = |id: &str| -> Option<String> {
        live.workspaces
            .iter()
            .find(|(w, _)| w == id)
            .map(|(_, name)| scrub(name))
    };
    // Needs you: gates, then blocked agents (longest waiting first), red
    // pull requests, stalled queues.
    let mut needs: Vec<NeedsYou> = Vec::new();
    let mut gates: Vec<&LiveGate> = live.gates.iter().collect();
    gates.sort_by_key(|g| g.since.unwrap_or(until));
    for g in gates {
        needs.push(NeedsYou {
            kind: "gate".into(),
            title: scrub(&format!(
                "approve autopilot task {} in {}",
                g.task_id, g.folder
            )),
            workspace: g.workspace_id.as_deref().and_then(ws_name),
            since: g.since,
            link: Link::new(format!("#gate={}", g.run_id), *hub_url),
        });
    }
    for s in &blocked {
        needs.push(NeedsYou {
            kind: "blocked".into(),
            title: format!("{} is blocked", scrub(&s.session)),
            workspace: ws_name(&s.workspace_id),
            since: Some(s.since),
            link: Link::new(format!("#session={}", s.session_id), *hub_url),
        });
    }
    for p in &live.red_prs {
        needs.push(NeedsYou {
            kind: "pr_red".into(),
            title: format!("CI fails on PR #{} ({})", p.number, scrub(&p.session)),
            workspace: ws_name(&p.workspace_id),
            since: None,
            link: Link::new(format!("#session={}", p.session_id), *hub_url),
        });
    }
    let mut stalled: Vec<&LiveStall> = live.stalled.iter().collect();
    stalled.sort_by_key(|s| s.at);
    for s in stalled {
        needs.push(NeedsYou {
            kind: "queue_stalled".into(),
            title: format!("{}'s prompt queue stalled", scrub(&s.session)),
            workspace: ws_name(&s.workspace_id),
            since: Some(s.at),
            link: Link::new(format!("#session={}", s.session_id), *hub_url),
        });
    }
    let waiting = !needs.is_empty();
    needs.truncate(NEEDS_YOU);
    // Totals, rounded; workspaces with nothing to say dropped, the ones
    // waiting on you first.
    let mut totals = unplaced;
    for w in &mut b.workspaces {
        w.counts.spend_today_usd = cents(w.counts.spend_today_usd);
        w.counts.spend_week_usd = cents(w.counts.spend_week_usd);
        add_counts(&mut totals, &w.counts);
    }
    totals.spend_today_usd = cents(totals.spend_today_usd);
    totals.spend_week_usd = cents(totals.spend_week_usd);
    let mut workspaces: Vec<WorkspaceDigest> = b
        .workspaces
        .into_iter()
        .filter(|w| w.counts != Counts::default() || !w.fanouts.is_empty())
        .collect();
    workspaces.sort_by(|a, b| {
        let urgent = |w: &WorkspaceDigest| w.counts.blocked_now + w.counts.gates_waiting;
        urgent(b)
            .cmp(&urgent(a))
            .then_with(|| a.workspace.to_lowercase().cmp(&b.workspace.to_lowercase()))
    });
    let link = Link::new(format!("#digest={}", inputs.id), *hub_url);
    Report {
        id: inputs.id.clone(),
        trigger: inputs.trigger.to_owned(),
        label: inputs.label.clone(),
        slot: inputs.slot,
        since,
        until,
        timezone: tz.name().to_owned(),
        totals,
        workspaces,
        needs_you: needs,
        empty: !b.happened && !waiting,
        url: link.url,
        delivery: None,
    }
}

impl WorkspaceDigest {
    /// `name: 3 turns finished, ...; blocked: a, b; fan-out x: winner b (3/4)`.
    pub fn line(&self) -> String {
        let mut out = format!("{}: ", self.workspace);
        let counts = self.counts.line();
        out.push_str(if counts.is_empty() { "-" } else { &counts });
        if !self.blocked.is_empty() {
            let names: Vec<&str> = self.blocked.iter().map(|s| s.session.as_str()).collect();
            out.push_str(&format!("; blocked: {}", names.join(", ")));
        }
        for f in &self.fanouts {
            out.push_str(&format!("; fan-out {}", f.says()));
        }
        out
    }
}

impl Report {
    /// The title a push and a notification carry.
    pub fn title(&self) -> String {
        match self.trigger.as_str() {
            "scheduled" => format!("flywheel {} digest", self.label),
            _ => "flywheel digest".to_owned(),
        }
    }

    /// The headline: every count, or that nothing happened.
    pub fn headline(&self) -> String {
        let line = self.totals.line();
        if line.is_empty() {
            "Nothing happened, and nothing waits on you.".to_owned()
        } else {
            line
        }
    }

    /// The push for this digest: names and counts, the top items as action
    /// buttons when the hub has a URL a phone reaches.
    pub fn push(&self) -> crate::notify::Push {
        let mut message = self.headline();
        if !self.needs_you.is_empty() {
            message.push_str("\nNeeds you:");
            for (n, item) in self.needs_you.iter().enumerate() {
                message.push_str(&format!("\n{}. {}", n + 1, item.title));
                if let Some(ws) = &item.workspace {
                    message.push_str(&format!(" ({ws})"));
                }
            }
        }
        for w in self.workspaces.iter().take(PUSH_WORKSPACES) {
            message.push('\n');
            message.push_str(&w.line());
        }
        if self.workspaces.len() > PUSH_WORKSPACES {
            message.push_str(&format!(
                "\n+{} more workspaces",
                self.workspaces.len() - PUSH_WORKSPACES
            ));
        }
        let urgent = self.totals.blocked_now + self.totals.gates_waiting > 0;
        crate::notify::Push {
            title: crate::notify::bounded(&self.title(), 120),
            message: crate::notify::bounded(&message, PUSH_MAX_CHARS),
            priority: if urgent { "high" } else { "default" },
            tags: "flywheel,digest".into(),
            click: self.url.clone(),
            actions: self
                .needs_you
                .iter()
                .filter_map(|item| item.link.url.clone().map(|url| (item.title.clone(), url)))
                .collect(),
        }
    }

    /// The plain-text digest `fw hub digest` prints (times in `tz`).
    pub fn text(&self, tz: &Tz) -> String {
        // The year too when the span crosses one.
        let format = if self.since.with_timezone(tz).year() == self.until.with_timezone(tz).year() {
            "%a %d %b %H:%M"
        } else {
            "%a %d %b %Y %H:%M"
        };
        let local = |at: DateTime<Utc>| at.with_timezone(tz).format(format).to_string();
        let mut out = format!(
            "{} ({} - {}, {})\n{}\n",
            self.title(),
            local(self.since),
            local(self.until),
            self.timezone,
            self.headline()
        );
        if !self.needs_you.is_empty() {
            out.push_str("\nNeeds you:\n");
            for (n, item) in self.needs_you.iter().enumerate() {
                let ws = item
                    .workspace
                    .as_deref()
                    .map(|w| format!(" ({w})"))
                    .unwrap_or_default();
                let link = item.link.url.as_deref().unwrap_or(&item.link.hash);
                out.push_str(&format!("  {}. {}{ws}  {link}\n", n + 1, item.title));
            }
        }
        if !self.workspaces.is_empty() {
            out.push('\n');
            for w in &self.workspaces {
                out.push_str(&format!("  {}\n", w.line()));
            }
        }
        if let Some(delivery) = &self.delivery {
            out.push('\n');
            if delivery.quiet {
                out.push_str("Not sent: nothing in it (digest.quiet_if_empty).\n");
            } else {
                let to = if delivery.channels.is_empty() {
                    "nowhere".to_owned()
                } else {
                    delivery.channels.join(", ")
                };
                out.push_str(&format!("Sent to: {to}\n"));
                for why in &delivery.unheard {
                    out.push_str(&format!("Not received: {why}\n"));
                }
            }
            if let Some(e) = &delivery.push_error {
                out.push_str(&format!("Push failed: {e}\n"));
            }
        }
        out
    }
}
