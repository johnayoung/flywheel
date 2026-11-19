//! Holds and the decision record.
//!
//! A **hold** is anything a session (or a flywheel run) waits on the
//! operator for: an `ask_operator` ask or a closing question, an agent's
//! start or end request, a harness permission prompt, a manual gate. Each
//! has a [`HoldKind`] and a `cause`, the key Needs you groups by, so the
//! operator answers twelve start requests from one lead as one decision.
//!
//! A **decision** is a hold's answer, kept: the question, the agent's
//! recommendation, the answer, who gave it, when, for which session, and
//! the batch it was part of. [`DecisionStore`] keeps them in
//! `<state-dir>/decisions.db` (SQLite, 0600), outliving restarts and live
//! handoffs. Each one recorded is also the plugin event `decision.made`
//! (its answer capped, never its question), which is how it reaches the
//! activity journal and anything that folds the hub's event bus.
//!
//! The hub side (listing session holds, answering them, recording each
//! answer where it is given) is in `crate::hub::holds`; manual gates are
//! added from the Flywheel monitor in `crate::server::holds_api`.

pub use crate::telemetry::HoldKind;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// The decision store's file in the state dir.
pub const DECISIONS_FILE: &str = "decisions.db";
/// The schema this hub writes (`PRAGMA user_version`).
const SCHEMA: i64 = 1;
/// The longest question, recommendation, or answer kept (characters).
pub const TEXT_CAP: usize = 2000;
/// The longest cause kept (characters).
pub const CAUSE_CAP: usize = 120;
/// The most decisions one query returns.
pub const QUERY_MAX: usize = 1000;
/// How many one query returns by default.
pub const QUERY_DEFAULT: usize = 100;
/// The answer that leaves a hold out of a batch.
pub const SKIP: &str = "skip";

/// Something the operator is waited on for, now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// Stable while the hold stands: `ask:<session>:<since>`,
    /// `start:<session>:<start>`, `end:<session>:<end>`,
    /// `perm:<session>:<request>`, `gate:<run>:<gate>:<attempt>`.
    pub id: String,
    pub kind: HoldKind,
    /// What groups it with like holds (within its kind).
    pub cause: String,
    /// The session that waits (the asker), when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// The flywheel folder a gate is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// What is asked, in one line.
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// The answers it takes from Needs you: `approve` / `decline`, `yes`,
    /// the agent's options; empty: any text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<String>,
    /// When it began (Unix ms).
    pub since: i64,
}

/// Holds of one kind and cause, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldGroup {
    pub kind: HoldKind,
    pub cause: String,
    pub holds: Vec<Hold>,
}

/// Group `holds` by kind and cause: groups by kind (in [`HoldKind::ALL`]
/// order), then oldest first; holds in a group oldest first.
pub fn group(mut holds: Vec<Hold>) -> Vec<HoldGroup> {
    holds.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.id.cmp(&b.id)));
    let mut groups: Vec<HoldGroup> = Vec::new();
    for hold in holds {
        match groups
            .iter_mut()
            .find(|g| g.kind == hold.kind && g.cause == hold.cause)
        {
            Some(group) => group.holds.push(hold),
            None => groups.push(HoldGroup {
                kind: hold.kind,
                cause: hold.cause.clone(),
                holds: vec![hold],
            }),
        }
    }
    groups.sort_by_key(|g| (g.kind, g.holds[0].since));
    groups
}

/// A cause from free text: lowercase, whitespace collapsed, closing
/// punctuation dropped, capped. Two asks worded the same group together.
pub fn cause_of(text: &str) -> String {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let joined = words.join(" ");
    let trimmed = joined.trim_end_matches(['?', '.', '!', ':', ' ']);
    cap(trimmed, CAUSE_CAP)
}

/// `text` cut to `max` characters (an ellipsis marks a cut).
pub fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// A manual gate parked in `folder` as a hold (`None`: a decision is
/// already queued on it).
pub fn gate_hold(folder: &Path, gate: &crate::autopilot::PilotGate) -> Option<Hold> {
    if gate.pending.is_some() {
        return None;
    }
    let label = folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Some(Hold {
        id: format!("gate:{}:{}:{}", gate.run_id, gate.gate, gate.attempt),
        kind: HoldKind::Gate,
        cause: format!("gates in {label}"),
        session: None,
        session_name: None,
        folder: Some(folder.to_path_buf()),
        question: cap(
            &format!("{}: {}", gate.task_id, gate.instruction.trim()),
            TEXT_CAP,
        ),
        recommendation: None,
        answers: vec!["approve".into(), "deny".into()],
        since: gate.since.map_or(0, |t| t.timestamp_millis()),
    })
}

/// A gate answer: approve, or deny with optional feedback after a colon
/// (`deny: the test is weak`).
pub fn gate_answer(answer: &str) -> Result<(bool, Option<String>), String> {
    let (word, rest) = answer.split_once(':').map_or((answer, ""), |(w, r)| (w, r));
    let feedback = Some(rest.trim().to_owned()).filter(|f| !f.is_empty());
    match word.trim().to_ascii_lowercase().as_str() {
        "approve" | "approved" | "yes" if feedback.is_none() => Ok((true, None)),
        "deny" | "denied" | "decline" | "reject" | "no" => Ok((false, feedback)),
        _ => Err(format!(
            "{answer:?}: answer approve, or deny (deny: FEEDBACK for the next attempt)"
        )),
    }
}

/// The decision on a gate the hub enqueued: `hold` as shown (else made
/// from what was decided).
pub fn gate_decision(
    hold: Option<Hold>,
    decided: &crate::autopilot::GateDecided,
    feedback: Option<&str>,
) -> NewDecision {
    let id = format!(
        "gate:{}:{}:{}",
        decided.run_id, decided.gate, decided.attempt
    );
    let answer = match (decided.kind.as_str(), feedback) {
        ("approve", _) => "approved".to_owned(),
        (_, Some(feedback)) => format!("denied: {feedback}"),
        _ => "denied".to_owned(),
    };
    NewDecision {
        hold: id,
        kind: Some(HoldKind::Gate),
        cause: hold
            .as_ref()
            .map_or_else(|| "gates".to_owned(), |h| h.cause.clone()),
        session: None,
        session_name: None,
        question: hold.map_or_else(
            || format!("{}: gate {}", decided.task_id, decided.gate),
            |h| h.question,
        ),
        recommendation: None,
        answer,
        by: "operator".into(),
        batch: None,
    }
}

/// A decision to record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewDecision {
    pub hold: String,
    pub kind: Option<HoldKind>,
    pub cause: String,
    pub session: Option<String>,
    pub session_name: Option<String>,
    pub question: String,
    pub recommendation: Option<String>,
    pub answer: String,
    /// `operator`, or `agent:<name>` for an agent's message that answered.
    pub by: String,
    /// The batch it was answered in (Needs you's group answer).
    pub batch: Option<String>,
}

/// A hold's answer, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: i64,
    pub hold: String,
    pub kind: HoldKind,
    pub cause: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    pub answer: String,
    pub by: String,
    /// When (Unix ms).
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
}

/// Which decisions to read: newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DecisionQuery {
    #[serde(default)]
    pub kind: Option<HoldKind>,
    /// A session id (or name: matched against both).
    #[serde(default)]
    pub session: Option<String>,
    /// At or after (Unix ms).
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The decision record: one SQLite file, opened lazily (a hub whose file
/// cannot be opened keeps running and says why on each use).
#[derive(Debug)]
pub struct DecisionStore {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl DecisionStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(DECISIONS_FILE),
            conn: Mutex::new(None),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(open(&self.path)?);
        }
        let conn = guard.as_ref().expect("opened above");
        f(conn).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Keep `new`; the decision as kept.
    pub fn record(&self, new: &NewDecision, at: i64) -> Result<Decision, String> {
        let kind = new.kind.unwrap_or(HoldKind::NeedsInput);
        let decision = Decision {
            id: 0,
            hold: cap(&new.hold, 512),
            kind,
            cause: cap(&new.cause, CAUSE_CAP),
            session: new.session.clone(),
            session_name: new.session_name.clone(),
            question: cap(&new.question, TEXT_CAP),
            recommendation: new
                .recommendation
                .as_deref()
                .map(|r| cap(r, TEXT_CAP))
                .filter(|r| !r.is_empty()),
            answer: cap(&new.answer, TEXT_CAP),
            by: cap(&new.by, 200),
            at,
            batch: new.batch.clone(),
        };
        let id = self.with(|conn| {
            conn.execute(
                "INSERT INTO decisions
                     (hold, kind, cause, session, session_name, question, recommendation,
                      answer, by, at, batch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    decision.hold,
                    decision.kind.as_str(),
                    decision.cause,
                    decision.session,
                    decision.session_name,
                    decision.question,
                    decision.recommendation,
                    decision.answer,
                    decision.by,
                    decision.at,
                    decision.batch,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })?;
        Ok(Decision { id, ..decision })
    }

    /// The decisions `query` names, newest first.
    pub fn query(&self, query: &DecisionQuery) -> Result<Vec<Decision>, String> {
        let limit = query.limit.unwrap_or(QUERY_DEFAULT).clamp(1, QUERY_MAX);
        let kind = query.kind.map(HoldKind::as_str);
        self.with(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT id, hold, kind, cause, session, session_name, question, recommendation,
                        answer, by, at, batch
                 FROM decisions
                 WHERE (?1 IS NULL OR kind = ?1)
                   AND (?2 IS NULL OR session = ?2 OR session_name = ?2)
                   AND (?3 IS NULL OR at >= ?3)
                 ORDER BY at DESC, id DESC
                 LIMIT ?4",
            )?;
            let rows = statement.query_map(
                params![kind, query.session, query.since, limit as i64],
                |row| {
                    let kind: String = row.get(2)?;
                    Ok(Decision {
                        id: row.get(0)?,
                        hold: row.get(1)?,
                        kind: HoldKind::parse(&kind).unwrap_or(HoldKind::NeedsInput),
                        cause: row.get(3)?,
                        session: row.get(4)?,
                        session_name: row.get(5)?,
                        question: row.get(6)?,
                        recommendation: row.get(7)?,
                        answer: row.get(8)?,
                        by: row.get(9)?,
                        at: row.get(10)?,
                        batch: row.get(11)?,
                    })
                },
            )?;
            rows.collect()
        })
    }

    /// The newest decision on hold `hold`, if any.
    pub fn of_hold(&self, hold: &str) -> Result<Option<i64>, String> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id FROM decisions WHERE hold = ?1 ORDER BY id DESC LIMIT 1",
                params![hold],
                |row| row.get(0),
            )
            .optional()
        })
    }
}

fn open(path: &Path) -> Result<Connection, String> {
    let fail = |e: rusqlite::Error| format!("{}: {e}", path.display());
    let fresh = !path.exists();
    let conn = Connection::open(path).map_err(fail)?;
    if fresh {
        restrict(path);
    }
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(fail)?;
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(fail)?;
    if found > SCHEMA {
        return Err(format!(
            "{} was written by a newer hub (schema {found}, this one reads {SCHEMA})",
            path.display()
        ));
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS decisions (
             id             INTEGER PRIMARY KEY AUTOINCREMENT,
             hold           TEXT NOT NULL,
             kind           TEXT NOT NULL,
             cause          TEXT NOT NULL,
             session        TEXT,
             session_name   TEXT,
             question       TEXT NOT NULL,
             recommendation TEXT,
             answer         TEXT NOT NULL,
             by             TEXT NOT NULL,
             at             INTEGER NOT NULL,
             batch          TEXT
         );
         CREATE INDEX IF NOT EXISTS decisions_at ON decisions (at);
         CREATE INDEX IF NOT EXISTS decisions_session ON decisions (session, at);
         CREATE INDEX IF NOT EXISTS decisions_kind ON decisions (kind, at);
         CREATE INDEX IF NOT EXISTS decisions_hold ON decisions (hold);",
    )
    .map_err(fail)?;
    conn.pragma_update(None, "user_version", SCHEMA)
        .map_err(fail)?;
    Ok(conn)
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

/// Who answered, from a prompt: an agent's message (the hub's signed
/// sender line, `From <name> ...: a message from that agent session`) is
/// `agent:<name>`; anything else is the operator's.
pub fn by_of_prompt(prompt: &str, known: impl Fn(&str) -> bool) -> String {
    let first = prompt.lines().next().unwrap_or("");
    if let Some(rest) = first.strip_prefix("From ")
        && first.ends_with("agent session")
    {
        // The name runs up to " on <branch>", " in <dir>", " (machine",
        // or ": "; a name may hold any of those words, so the longest
        // prefix that names a known session wins.
        let mut best = None;
        for (at, _) in rest.char_indices() {
            if at > 0 && known(&rest[..at]) {
                best = Some(&rest[..at]);
            }
        }
        if let Some(name) = best {
            return format!("agent:{name}");
        }
    }
    "operator".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(id: &str, kind: HoldKind, cause: &str, since: i64) -> Hold {
        Hold {
            id: id.into(),
            kind,
            cause: cause.into(),
            session: None,
            session_name: None,
            folder: None,
            question: id.into(),
            recommendation: None,
            answers: Vec::new(),
            since,
        }
    }

    #[test]
    fn holds_group_by_kind_then_cause_oldest_first() {
        let groups = group(vec![
            hold("s2", HoldKind::AwaitingApproval, "start from lead", 30),
            hold("p1", HoldKind::Permission, "Bash cargo", 5),
            hold("s1", HoldKind::AwaitingApproval, "start from lead", 10),
            hold("a1", HoldKind::NeedsInput, "which db", 40),
            hold("s3", HoldKind::AwaitingApproval, "start from other", 20),
        ]);
        let shape: Vec<(HoldKind, &str, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (
                    g.kind,
                    g.cause.as_str(),
                    g.holds.iter().map(|h| h.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (HoldKind::NeedsInput, "which db", vec!["a1"]),
                (
                    HoldKind::AwaitingApproval,
                    "start from lead",
                    vec!["s1", "s2"]
                ),
                (HoldKind::AwaitingApproval, "start from other", vec!["s3"]),
                (HoldKind::Permission, "Bash cargo", vec!["p1"]),
            ]
        );
    }

    #[test]
    fn like_worded_asks_share_a_cause() {
        assert_eq!(
            cause_of("Should I push   the branch?"),
            cause_of("should i push the branch")
        );
        assert_ne!(cause_of("push it?"), cause_of("merge it?"));
        assert_eq!(cause_of(&"x".repeat(500)).chars().count(), CAUSE_CAP);
    }

    #[test]
    fn decisions_are_kept_and_read_back_by_kind_session_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = DecisionStore::new(dir.path());
        let new = |hold: &str, kind, session: &str| NewDecision {
            hold: hold.into(),
            kind: Some(kind),
            cause: "c".into(),
            session: Some(session.into()),
            session_name: Some(format!("{session}-name")),
            question: "q?".into(),
            recommendation: Some("yes".into()),
            answer: "yes".into(),
            by: "operator".into(),
            batch: None,
        };
        let first = store
            .record(&new("start:a:1", HoldKind::AwaitingApproval, "a"), 100)
            .unwrap();
        store
            .record(&new("ask:b:2", HoldKind::NeedsInput, "b"), 200)
            .unwrap();
        store
            .record(&new("start:a:3", HoldKind::AwaitingApproval, "a"), 300)
            .unwrap();
        assert!(first.id > 0);
        assert_eq!(first.recommendation.as_deref(), Some("yes"));
        let all = store.query(&DecisionQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|d| d.at).collect::<Vec<_>>(),
            vec![300, 200, 100]
        );
        let starts = store
            .query(&DecisionQuery {
                kind: Some(HoldKind::AwaitingApproval),
                ..DecisionQuery::default()
            })
            .unwrap();
        assert_eq!(starts.len(), 2);
        let by_name = store
            .query(&DecisionQuery {
                session: Some("b-name".into()),
                ..DecisionQuery::default()
            })
            .unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].hold, "ask:b:2");
        let recent = store
            .query(&DecisionQuery {
                since: Some(200),
                limit: Some(1),
                ..DecisionQuery::default()
            })
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].at, 300);
        assert_eq!(store.of_hold("ask:b:2").unwrap(), Some(by_name[0].id));
        // A second store on the same file (the next hub process) reads them.
        let again = DecisionStore::new(dir.path());
        assert_eq!(again.query(&DecisionQuery::default()).unwrap().len(), 3);
    }

    #[test]
    fn an_agents_signed_message_is_its_answer_else_the_operators() {
        let known = |n: &str| n == "lead" || n == "lead in x";
        assert_eq!(
            by_of_prompt(
                "From lead in x on b: a message from that agent session\n\nyes",
                known
            ),
            "agent:lead in x"
        );
        assert_eq!(
            by_of_prompt(
                "From lead in flywheel: a message from that agent session\n\ngo",
                known
            ),
            "agent:lead"
        );
        assert_eq!(by_of_prompt("From lead: hello", known), "operator");
        assert_eq!(by_of_prompt("yes, push it", known), "operator");
    }
}
