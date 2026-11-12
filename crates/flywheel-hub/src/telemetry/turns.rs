//! A session's turns, each read at a glance instead of as its list of
//! calls: where the turn's time went by kind of work (see
//! [`super::station`]), a strip of its calls along its own clock, the
//! files it read and edited, the commands it ran and how they failed, what
//! it cost, and how it ended. Folded from the turns' events; the agent's
//! transcript stays the record of what was said.

use super::event::{Event, Outcome, What};
use super::reduce::{WorkItem, WorkSource, WorkStatus};
use super::shape;
use super::station::{self, Station};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Segments a turn's strip keeps; past it, neighbours of one station merge.
pub const STRIP_MAX: usize = 160;
/// Files edited and commands listed per turn.
const LIST_MAX: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Turn {
    pub id: String,
    pub title: String,
    pub status: WorkStatus,
    pub started: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    /// Its first events are older than those read: its counts are short.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    pub calls: u32,
    pub errors: u32,
    /// Time its finished calls took (parallel calls each count).
    pub tool_ms: u64,
    /// What it cost, when the session reported its cost before and after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The context window in use as it ended (tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    pub compacted: bool,
    /// Times it waited on a person (a permission prompt, a question).
    pub waits: u32,
    pub subagents: u32,
    /// By kind of work, the most time first.
    pub stations: Vec<StationUse>,
    pub strip: Vec<Segment>,
    /// Distinct files read.
    pub read: u32,
    /// Distinct files edited, and the most edited of them.
    pub edited: u32,
    pub edits: Vec<FileEdits>,
    /// Shell commands by shape, the most time first.
    pub commands: Vec<CommandUse>,
    /// The last call that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// The end of its closing message, or the error it failed on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ending: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StationUse {
    pub station: Station,
    pub calls: u32,
    pub ms: u64,
    pub errors: u32,
}

/// One call (or a run of calls at one station) on the turn's clock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub station: Station,
    /// Milliseconds after the turn started.
    pub at: u64,
    /// `None`: still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    /// Made in a subagent.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sub: bool,
    /// Calls it stands for.
    pub calls: u32,
    /// What its (first) call acted on: `name: summary`.
    pub what: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileEdits {
    pub path: String,
    pub edits: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandUse {
    pub shape: String,
    pub calls: u32,
    pub errors: u32,
    pub ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What one turn is gathering while its events are read.
#[derive(Default)]
struct Fold {
    calls: u32,
    errors: u32,
    tool_ms: u64,
    cost_before: Option<f64>,
    cost_last: Option<f64>,
    context: Option<u64>,
    compacted: bool,
    waits: u32,
    subagents: HashSet<String>,
    stations: BTreeMap<Station, StationUse>,
    strip: Vec<Segment>,
    /// Running calls: id to their strip index.
    open: HashMap<String, usize>,
    read: HashSet<String>,
    edits: HashMap<String, u32>,
    commands: HashMap<String, CommandUse>,
    failure: Option<Failure>,
    ending: Option<String>,
    error: Option<String>,
    seen: bool,
}

/// The turns of `work` (newest first, at most `limit`), folded from the
/// session's `events` (oldest first). `complete`: the events start at the
/// session's first, so no turn is partial.
pub fn fold(work: &[WorkItem], events: &[Event], limit: usize, complete: bool) -> Vec<Turn> {
    let by_id: HashMap<&str, &WorkItem> = work.iter().map(|w| (w.id.as_str(), w)).collect();
    let root_of = |id: &str| -> Option<&str> {
        let mut at = by_id.get(id)?;
        for _ in 0..64 {
            match at.parent.as_deref().and_then(|p| by_id.get(p)) {
                Some(up) => at = up,
                None => break,
            }
        }
        Some(at.id.as_str())
    };
    let mut turns: Vec<&WorkItem> = work
        .iter()
        .filter(|w| w.source == WorkSource::Turn)
        .collect();
    turns.sort_by_key(|w| w.started);
    let keep: HashSet<&str> = turns
        .iter()
        .rev()
        .take(limit)
        .map(|w| w.id.as_str())
        .collect();
    // An event naming no work item belongs to the turn running at its time.
    let turn_at = |ts: i64| -> Option<&str> {
        turns
            .iter()
            .enumerate()
            .rev()
            .find(|(i, w)| {
                ts >= w.started
                    && match w.finished {
                        Some(end) => ts <= end,
                        None => turns.get(i + 1).is_none_or(|next| ts < next.started),
                    }
            })
            .map(|(_, w)| w.id.as_str())
    };

    let mut folds: HashMap<&str, Fold> = HashMap::new();
    let mut cost_seen: Option<f64> = None;
    for event in events {
        let root = match event.work.as_deref() {
            Some(work) => root_of(work),
            None => turn_at(event.ts),
        };
        let started = root
            .and_then(|r| by_id.get(r))
            .map_or(event.ts, |w| w.started);
        let Some(root) = root.filter(|r| keep.contains(r)) else {
            if let What::Usage {
                cost_usd: Some(c), ..
            } = &event.what
            {
                cost_seen = Some(*c);
            }
            continue;
        };
        let fold = folds.entry(root).or_default();
        if !fold.seen {
            fold.seen = true;
            fold.cost_before = cost_seen;
        }
        let at = (event.ts - started).max(0) as u64;
        let sub = event.agent.is_some();
        match &event.what {
            What::ToolStarted {
                id, name, summary, ..
            } => {
                fold.calls += 1;
                let station = station::of(name, summary.as_deref());
                fold.open.insert(id.clone(), fold.strip.len());
                fold.strip.push(Segment {
                    station,
                    at,
                    ms: None,
                    failed: false,
                    sub,
                    calls: 1,
                    what: what(name, summary.as_deref()),
                });
            }
            What::ToolFinished {
                id,
                name,
                summary,
                outcome,
                duration_ms,
                error,
                ..
            } => {
                let station = station::of(name, summary.as_deref());
                let failed = *outcome != Outcome::Ok;
                let ms = duration_ms.unwrap_or(0);
                match fold.open.remove(id) {
                    Some(index) => {
                        let segment = &mut fold.strip[index];
                        segment.ms = Some(duration_ms.unwrap_or(at.saturating_sub(segment.at)));
                        segment.failed = failed;
                    }
                    None => {
                        // Its start is older than the events read.
                        fold.calls += 1;
                        fold.strip.push(Segment {
                            station,
                            at: at.saturating_sub(ms),
                            ms: Some(ms),
                            failed,
                            sub,
                            calls: 1,
                            what: what(name, summary.as_deref()),
                        });
                    }
                }
                fold.tool_ms += ms;
                let use_ = fold.stations.entry(station).or_insert(StationUse {
                    station,
                    calls: 0,
                    ms: 0,
                    errors: 0,
                });
                use_.calls += 1;
                use_.ms += ms;
                if failed {
                    fold.errors += 1;
                    use_.errors += 1;
                    fold.failure = Some(Failure {
                        tool: name.clone(),
                        summary: summary.clone(),
                        error: error.clone(),
                    });
                }
                let path = summary.as_deref().unwrap_or_default();
                match station {
                    Station::Files if name != "Grep" && !path.is_empty() && is_path(path) => {
                        fold.read.insert(path.to_owned());
                    }
                    Station::Edits | Station::Memory if !path.is_empty() && is_path(path) => {
                        if station == Station::Edits || name != "Read" {
                            *fold.edits.entry(path.to_owned()).or_default() += 1;
                        }
                    }
                    _ => {}
                }
                let mut kind = shape::of(name, summary.as_deref());
                if matches!(station, Station::Shell | Station::Wait | Station::Network)
                    && !kind.is_empty()
                {
                    if shape::is_poll(&kind) {
                        kind = kind.replacen(shape::POLL, "wait loop", 1);
                    }
                    let command = fold.commands.entry(kind.clone()).or_insert(CommandUse {
                        shape: kind,
                        calls: 0,
                        errors: 0,
                        ms: 0,
                    });
                    command.calls += 1;
                    command.ms += ms;
                    command.errors += u32::from(failed);
                }
            }
            What::WaitStarted { .. } => fold.waits += 1,
            What::SubagentStarted { id, .. } => {
                fold.subagents.insert(id.clone());
            }
            What::Compact { .. } => fold.compacted = true,
            What::ContextUsage { used, .. } => fold.context = Some(*used),
            What::Usage {
                cost_usd: Some(c), ..
            } => {
                fold.cost_last = Some(*c);
                cost_seen = Some(*c);
            }
            What::TurnFinished { ending } => fold.ending.clone_from(ending),
            What::TurnFailed { error, detail } => {
                fold.error = Some(match detail {
                    Some(detail) => format!("{error}: {detail}"),
                    None => error.clone(),
                });
            }
            _ => {}
        }
    }

    turns
        .iter()
        .rev()
        .take(limit)
        .map(|w| {
            let fold = folds.remove(w.id.as_str()).unwrap_or_default();
            // Its first events were not read: older than the window, or
            // (with no events at all) never seen though it made calls.
            let partial =
                !complete && w.tools > 0 && events.first().is_none_or(|first| first.ts > w.started);
            finish(w, fold, partial)
        })
        .collect()
}

fn finish(w: &WorkItem, fold: Fold, partial: bool) -> Turn {
    let mut stations: Vec<StationUse> = fold.stations.into_values().collect();
    stations.sort_by(|a, b| b.ms.cmp(&a.ms).then(b.calls.cmp(&a.calls)));
    let mut edits: Vec<FileEdits> = fold
        .edits
        .iter()
        .map(|(path, edits)| FileEdits {
            path: path.clone(),
            edits: *edits,
        })
        .collect();
    edits.sort_by(|a, b| b.edits.cmp(&a.edits).then(a.path.cmp(&b.path)));
    let edited = edits.len() as u32;
    edits.truncate(LIST_MAX);
    let mut commands: Vec<CommandUse> = fold.commands.into_values().collect();
    commands.sort_by(|a, b| b.ms.cmp(&a.ms).then(b.calls.cmp(&a.calls)));
    commands.truncate(LIST_MAX);
    let cost_usd = match (fold.cost_before, fold.cost_last) {
        (Some(before), Some(last)) if last >= before => Some(last - before),
        (None, Some(last)) if !partial => Some(last),
        _ => None,
    };
    Turn {
        id: w.id.clone(),
        title: w.title.clone(),
        status: w.status,
        started: w.started,
        finished: w.finished,
        partial,
        calls: fold.calls.max(w.tools),
        errors: fold.errors.max(w.tool_errors),
        tool_ms: fold.tool_ms.max(w.tool_ms),
        cost_usd,
        context: fold.context,
        compacted: fold.compacted,
        waits: fold.waits,
        subagents: fold.subagents.len() as u32,
        stations,
        strip: merge(fold.strip),
        read: fold.read.len() as u32,
        edited,
        edits,
        commands,
        failure: fold.failure,
        ending: fold.ending,
        error: fold.error,
    }
}

/// `Read: /repo/src/main.rs`.
fn what(name: &str, summary: Option<&str>) -> String {
    match summary {
        Some(s) if !s.is_empty() => format!("{name}: {s}"),
        _ => name.to_owned(),
    }
}

fn is_path(text: &str) -> bool {
    !text.contains(' ') || text.starts_with('/')
}

/// The strip in time order, cut to [`STRIP_MAX`] by merging neighbours at
/// one station (finished ones; a running call stays its own).
fn merge(mut strip: Vec<Segment>) -> Vec<Segment> {
    strip.sort_by_key(|s| s.at);
    if strip.len() <= STRIP_MAX {
        return strip;
    }
    let mut out: Vec<Segment> = Vec::with_capacity(STRIP_MAX);
    let mut gap = 0u64;
    loop {
        out.clear();
        for segment in strip.iter() {
            let joins = out.last().is_some_and(|last: &Segment| {
                last.station == segment.station
                    && last.ms.is_some()
                    && segment.ms.is_some()
                    && segment.at <= last.at + last.ms.unwrap_or(0) + gap
            });
            if joins {
                let last = out.last_mut().expect("joined to a segment");
                let end =
                    (segment.at + segment.ms.unwrap_or(0)).max(last.at + last.ms.unwrap_or(0));
                last.ms = Some(end - last.at);
                last.failed |= segment.failed;
                last.sub &= segment.sub;
                last.calls += segment.calls;
            } else {
                out.push(segment.clone());
            }
        }
        if out.len() <= STRIP_MAX || gap > 1 << 40 {
            break;
        }
        gap = (gap * 4).max(1_000);
    }
    if out.len() > STRIP_MAX {
        out.truncate(STRIP_MAX);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, parent: Option<&str>, started: i64, finished: Option<i64>) -> WorkItem {
        WorkItem {
            id: id.into(),
            parent: parent.map(Into::into),
            source: if parent.is_some() {
                WorkSource::Subagent
            } else {
                WorkSource::Turn
            },
            title: format!("{id} title"),
            status: if finished.is_some() {
                WorkStatus::Done
            } else {
                WorkStatus::Active
            },
            agent: parent.map(|_| "sub-1".into()),
            kind: None,
            started,
            finished,
            tools: 0,
            tool_errors: 0,
            tool_ms: 0,
            tokens: None,
            doing: None,
        }
    }

    fn ev(seq: u64, ts: i64, work: &str, what: What) -> Event {
        Event {
            host: "h".into(),
            session: "s".into(),
            seq,
            ts,
            agent: None,
            work: Some(work.into()),
            item: None,
            what,
        }
    }

    fn call(id: &str, name: &str, summary: &str) -> What {
        What::ToolStarted {
            id: id.into(),
            name: name.into(),
            summary: Some(summary.into()),
            subagent: None,
        }
    }

    fn done(id: &str, name: &str, summary: &str, ms: u64, outcome: Outcome) -> What {
        What::ToolFinished {
            id: id.into(),
            name: name.into(),
            summary: Some(summary.into()),
            outcome,
            duration_ms: Some(ms),
            error: (outcome != Outcome::Ok).then(|| "exit 101".into()),
            tokens: None,
        }
    }

    fn cost(c: f64) -> What {
        What::Usage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(c),
        }
    }

    #[test]
    fn a_turn_reads_as_where_its_time_went_what_it_touched_and_how_it_ended() {
        let work = vec![
            item("t1", None, 1_000, Some(2_000)),
            item("t2", None, 10_000, Some(60_000)),
            item("a1", Some("t2"), 12_000, Some(20_000)),
        ];
        let mut sub_call = ev(9, 13_000, "a1", call("c5", "Read", "/r/b.rs"));
        sub_call.agent = Some("sub-1".into());
        let events = vec![
            ev(1, 1_500, "t1", cost(1.0)),
            ev(2, 10_100, "t2", call("c1", "Read", "/r/a.rs")),
            ev(
                3,
                10_200,
                "t2",
                done("c1", "Read", "/r/a.rs", 100, Outcome::Ok),
            ),
            ev(4, 10_300, "t2", call("c2", "Edit", "/r/a.rs")),
            ev(
                5,
                10_400,
                "t2",
                done("c2", "Edit", "/r/a.rs", 100, Outcome::Ok),
            ),
            ev(6, 11_000, "t2", call("c3", "Bash", "cargo test -p x")),
            ev(
                7,
                41_000,
                "t2",
                done("c3", "Bash", "cargo test -p x", 30_000, Outcome::Error),
            ),
            ev(
                8,
                12_000,
                "a1",
                What::SubagentStarted {
                    id: "sub-1".into(),
                    kind: None,
                },
            ),
            sub_call,
            ev(10, 50_000, "t2", call("c4", "Bash", "sleep 60")),
            ev(11, 55_000, "t2", cost(3.5)),
            ev(
                12,
                59_000,
                "t2",
                What::TurnFinished {
                    ending: Some("Tests fail on x.".into()),
                },
            ),
        ];
        let turns = fold(&work, &events, 10, true);
        assert_eq!(turns.len(), 2);
        let t2 = &turns[0];
        assert_eq!(t2.id, "t2");
        assert_eq!(t2.calls, 5);
        assert_eq!(t2.errors, 1);
        assert_eq!(t2.cost_usd, Some(2.5));
        assert_eq!(t2.ending.as_deref(), Some("Tests fail on x."));
        assert_eq!(t2.read, 1, "the subagent's read is still running");
        assert_eq!(t2.edited, 1);
        assert_eq!(t2.edits[0].path, "/r/a.rs");
        assert_eq!(t2.subagents, 1);
        assert_eq!(t2.stations[0].station, Station::Shell);
        assert_eq!(t2.stations[0].errors, 1);
        assert_eq!(t2.commands[0].shape, "cargo test");
        assert_eq!(
            t2.failure.as_ref().unwrap().error.as_deref(),
            Some("exit 101")
        );
        let strip: Vec<(Station, u64, Option<u64>, bool, bool)> = t2
            .strip
            .iter()
            .map(|s| (s.station, s.at, s.ms, s.failed, s.sub))
            .collect();
        assert_eq!(
            strip,
            [
                (Station::Files, 100, Some(100), false, false),
                (Station::Edits, 300, Some(100), false, false),
                (Station::Shell, 1_000, Some(30_000), true, false),
                (Station::Files, 3_000, None, false, true),
                (Station::Wait, 40_000, None, false, false),
            ]
        );
        let t1 = &turns[1];
        assert_eq!(t1.cost_usd, Some(1.0));
        assert!(!t1.partial);
    }

    #[test]
    fn a_turn_older_than_the_events_read_is_partial_and_unpriced() {
        let mut old = item("t1", None, 1_000, Some(5_000));
        old.tools = 4;
        let work = vec![old, item("t2", None, 6_000, None)];
        let events = vec![
            ev(7, 3_000, "t1", done("c1", "Bash", "ls", 10, Outcome::Ok)),
            ev(8, 4_000, "t1", cost(2.0)),
            ev(9, 7_000, "t2", cost(2.5)),
        ];
        let turns = fold(&work, &events, 10, false);
        assert!(turns[1].partial);
        assert_eq!(turns[1].calls, 4, "the reducer's count where it is more");
        assert_eq!(turns[1].cost_usd, None);
        assert!(!turns[0].partial);
        assert_eq!(turns[0].cost_usd, Some(0.5));
    }

    #[test]
    fn a_long_strip_merges_neighbours_at_one_station() {
        let strip: Vec<Segment> = (0..1000u64)
            .map(|i| Segment {
                station: if i % 100 < 50 {
                    Station::Files
                } else {
                    Station::Shell
                },
                at: i * 10,
                ms: Some(5),
                failed: i == 7,
                sub: false,
                calls: 1,
                what: String::new(),
            })
            .collect();
        let merged = merge(strip);
        assert!(merged.len() <= STRIP_MAX, "{}", merged.len());
        assert_eq!(merged.iter().map(|s| s.calls).sum::<u32>(), 1000);
        assert!(merged[0].failed);
    }
}
