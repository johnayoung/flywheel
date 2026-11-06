//! Hub actions as audit rows in the store of the project they touched.
//!
//! The hub knows no store: the embedding binary plugs in an [`AuditSink`]
//! (`fw` implements it against each project's store, the same seam shape
//! as [`crate::flywheel::FlywheelProvider`]). The hub turns the lifecycle
//! events it already emits ([`crate::plugins::EVENTS`]) into
//! [`AuditRecord`]s ([`from_lifecycle`]) -- one path, never a second
//! emission site -- and hands them to an [`AuditRecorder`]: a channel and
//! one thread, so a store write never blocks a hub action, and a failed
//! write is logged and dropped, never surfaced to the action.
//!
//! Each record names the folder it touched (a session's worktree, else
//! its start directory; a review's directory; a fan-out's folder). The
//! sink resolves that folder's project; a folder in no known project
//! records nothing. Kinds: [`KINDS`].

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A session was opened.
pub const KIND_SESSION_START: &str = "hub.session.start";
/// A session's process exited (exit status, and the agent's last reply
/// in short as `summary`).
pub const KIND_SESSION_END: &str = "hub.session.end";
/// A session's worktree branch was merged into its base.
pub const KIND_MERGE: &str = "hub.merge";
/// A review action: a reviewer asked for, or a commit, push, or rebase
/// (`action`).
pub const KIND_REVIEW: &str = "hub.review";
/// A pull request was opened.
pub const KIND_PR: &str = "hub.pr";
/// A verified fan-out finished grading.
pub const KIND_FANOUT: &str = "hub.fanout";
/// Text was handed off into a session.
pub const KIND_HANDOFF: &str = "hub.handoff";
/// A gate approved from the hub (a run's manual gate not of autopilot's
/// work, or an agent's tool permission).
pub const KIND_APPROVE: &str = "hub.approve";
/// A run's manual gate (not of autopilot's work) denied from the hub.
pub const KIND_DENY: &str = "hub.deny";
/// Every hub audit kind.
pub const KINDS: &[&str] = &[
    KIND_SESSION_START,
    KIND_SESSION_END,
    KIND_MERGE,
    KIND_REVIEW,
    KIND_PR,
    KIND_FANOUT,
    KIND_HANDOFF,
    KIND_APPROVE,
    KIND_DENY,
];

/// Longest `summary` kept (characters, on one line).
pub const SUMMARY_CHARS: usize = 280;
/// How long a shutdown waits for queued records to be written.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// One hub action to record.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    /// One of [`KINDS`].
    pub kind: &'static str,
    pub at: DateTime<Utc>,
    /// The folder the action touched: whose project's store records it.
    pub folder: PathBuf,
    /// The row's fields (`session_id`, `session_kind`, `cwd`, `worktree`,
    /// and the action's own: `branch`, `commit`, `url`, ...).
    pub fields: Map<String, Value>,
}

/// What became of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// Appended to this project's store.
    Project(String),
    /// The folder is in no project the home knows: nothing written.
    NoProject,
}

/// Writes hub actions to the store of the project they touched.
/// Implemented by the embedding binary; called only on the recorder's own
/// thread (it may block on the store).
pub trait AuditSink: Send + Sync + 'static {
    /// Append `record` to the store of the project `record.folder` is in.
    /// Must not create a project (no project dir, no repo config write)
    /// for a folder that is not one already.
    fn record(&self, record: &AuditRecord) -> Result<Recorded, String>;
}

/// The queue between hub actions and the sink.
pub struct AuditRecorder {
    sender: Mutex<Option<mpsc::Sender<AuditRecord>>>,
    /// Signalled once every queued record was handed to the sink.
    drained: Mutex<Option<mpsc::Receiver<()>>>,
}

impl std::fmt::Debug for AuditRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditRecorder").finish_non_exhaustive()
    }
}

impl AuditRecorder {
    /// Start the recording thread.
    pub fn start(sink: Arc<dyn AuditSink>) -> std::io::Result<Self> {
        let (sender, records) = mpsc::channel::<AuditRecord>();
        let (done, drained) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("hub-audit".into())
            .spawn(move || {
                for record in records {
                    write(sink.as_ref(), &record);
                }
                let _ = done.send(());
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            drained: Mutex::new(Some(drained)),
        })
    }

    /// Queue `record` (never blocks; dropped with a log line once closed).
    pub fn submit(&self, record: AuditRecord) {
        let sender = self.sender.lock().unwrap_or_else(PoisonError::into_inner);
        match sender.as_ref() {
            Some(sender) => {
                if sender.send(record).is_err() {
                    eprintln!("fw hub: audit: the recorder stopped; a row was not written");
                }
            }
            None => eprintln!(
                "fw hub: audit: {} after shutdown was not recorded",
                record.kind
            ),
        }
    }

    /// Stop taking records and wait (up to `timeout`) for the queued ones
    /// to be written. Idempotent.
    pub fn close(&self, timeout: Duration) {
        drop(
            self.sender
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let drained = self
            .drained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(drained) = drained
            && drained.recv_timeout(timeout).is_err()
        {
            eprintln!(
                "fw hub: audit: rows still queued after {}s were not written",
                timeout.as_secs()
            );
        }
    }
}

fn write(sink: &dyn AuditSink, record: &AuditRecord) {
    // A sink that panics loses that row, never the recorder.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.record(record)));
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!(
            "fw hub: audit: {} for {}: not recorded: {e}",
            record.kind,
            record.folder.display()
        ),
        Err(_) => eprintln!(
            "fw hub: audit: {} for {}: not recorded: the sink panicked",
            record.kind,
            record.folder.display()
        ),
    }
}

/// `text` on one line, at most [`SUMMARY_CHARS`] characters (`None` when
/// blank).
pub fn summary(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= SUMMARY_CHARS {
        return Some(line);
    }
    let mut cut: String = line.chars().take(SUMMARY_CHARS - 3).collect();
    cut.push_str("...");
    Some(cut)
}

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A session's own fields (`session_id`, `session_name`, `session_kind`,
/// `workspace`, `cwd`, `worktree`, `branch`) and the folder it touched.
fn session_fields(session: &Value, fields: &mut Map<String, Value>) -> Option<PathBuf> {
    let id = text(session.get("id"))?;
    fields.insert("session_id".into(), Value::from(id));
    for (from, to) in [
        ("name", "session_name"),
        ("kind", "session_kind"),
        ("workspace", "workspace"),
        ("cwd", "cwd"),
        ("branch", "branch"),
    ] {
        if let Some(value) = text(session.get(from)) {
            fields.insert(to.into(), Value::from(value));
        }
    }
    let worktree = session.get("worktree").filter(|w| w.is_object());
    let worktree_path = worktree.and_then(|w| text(w.get("path")));
    if let Some(worktree) = worktree {
        if let Some(path) = worktree_path {
            fields.insert("worktree".into(), Value::from(path));
        }
        if let Some(branch) = text(worktree.get("branch")) {
            fields.insert("branch".into(), Value::from(branch));
        }
        if let Some(base) = text(worktree.get("base")) {
            fields.insert("base".into(), Value::from(base));
        }
    }
    worktree_path
        .or_else(|| text(session.get("cwd")))
        .map(PathBuf::from)
}

fn copy(payload: &Value, keys: &[(&str, &str)], fields: &mut Map<String, Value>) {
    for (from, to) in keys {
        if let Some(value) = payload.get(*from).filter(|v| !v.is_null()) {
            fields.insert((*to).into(), value.clone());
        }
    }
}

/// The audit record of lifecycle event `event` (`None`: not a hub action
/// the audit record keeps, or a payload naming no folder). `summary` is
/// the ended session's last agent reply, for `session.exited`.
pub fn from_lifecycle(
    event: &str,
    payload: &Value,
    summary_text: Option<&str>,
    at: DateTime<Utc>,
) -> Option<AuditRecord> {
    let mut fields = Map::new();
    let (kind, folder) = match event {
        "session.created" => (
            KIND_SESSION_START,
            session_fields(payload.get("session")?, &mut fields)?,
        ),
        "session.exited" => {
            let folder = session_fields(payload.get("session")?, &mut fields)?;
            let exit = payload.get("exit");
            fields.insert(
                "exit_code".into(),
                exit.and_then(|e| e.get("code"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            fields.insert(
                "signal".into(),
                exit.and_then(|e| e.get("signal"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            if payload.get("hub_stopping").and_then(Value::as_bool) == Some(true) {
                fields.insert("hub_stopping".into(), Value::Bool(true));
            }
            if let Some(summary) = summary_text.and_then(summary) {
                fields.insert("summary".into(), Value::from(summary));
            }
            (KIND_SESSION_END, folder)
        }
        "review.merged" | "review.pr_opened" | "review.committed" | "review.pushed"
        | "review.rebased" => {
            let session_folder = session_fields(payload.get("session")?, &mut fields);
            copy(
                payload,
                &[
                    ("branch", "branch"),
                    ("base", "base"),
                    ("head", "commit"),
                    ("url", "url"),
                ],
                &mut fields,
            );
            if let Some(outcome) = text(payload.get("summary")).and_then(summary) {
                fields.insert("summary".into(), Value::from(outcome));
            }
            let kind = match event {
                "review.merged" => KIND_MERGE,
                "review.pr_opened" => KIND_PR,
                other => {
                    let action = other.trim_start_matches("review.");
                    let action = match action {
                        "committed" => "commit",
                        "pushed" => "push",
                        _ => "rebase",
                    };
                    fields.insert("action".into(), Value::from(action));
                    KIND_REVIEW
                }
            };
            let dir = text(payload.get("dir")).map(PathBuf::from);
            (kind, dir.or(session_folder)?)
        }
        "review.requested" => {
            let author_folder = session_fields(payload.get("author")?, &mut fields);
            fields.insert("action".into(), Value::from("request"));
            if let Some(reviewer) = payload.get("reviewer") {
                copy(
                    reviewer,
                    &[("id", "reviewer_session_id"), ("kind", "reviewer_kind")],
                    &mut fields,
                );
            }
            copy(
                payload,
                &[("reused", "reused"), ("branch", "branch"), ("base", "base")],
                &mut fields,
            );
            let dir = text(payload.get("dir")).map(PathBuf::from);
            (KIND_REVIEW, dir.or(author_folder)?)
        }
        "handoff.sent" => {
            let folder = session_fields(payload.get("to")?, &mut fields)?;
            if let Some(from) = payload.get("from").filter(|f| f.is_object()) {
                fields.insert("from".into(), from.clone());
            }
            copy(
                payload,
                &[
                    ("source", "source"),
                    ("bytes", "bytes"),
                    ("truncated", "truncated"),
                ],
                &mut fields,
            );
            (KIND_HANDOFF, folder)
        }
        "fanout.finished" => {
            let fanout = payload.get("fanout")?;
            copy(
                fanout,
                &[
                    ("id", "fanout_id"),
                    ("name", "name"),
                    ("workspace", "workspace"),
                    ("repo", "repo_dir"),
                    ("base", "base"),
                    ("base_sha", "base_sha"),
                    ("graded_at", "graded_at"),
                    ("leader", "leader"),
                    ("winner", "winner"),
                    ("counts", "counts"),
                ],
                &mut fields,
            );
            let candidates: Vec<Value> = fanout
                .get("candidates")
                .and_then(Value::as_array)
                .map(|all| {
                    all.iter()
                        .map(|c| {
                            let mut row = Map::new();
                            copy(
                                c,
                                &[
                                    ("label", "label"),
                                    ("session_id", "session_id"),
                                    ("kind", "session_kind"),
                                    ("verdict", "verdict"),
                                    ("passed", "passed"),
                                    ("branch", "branch"),
                                    ("head", "commit"),
                                    ("graders_passed", "graders_passed"),
                                    ("graders_total", "graders_total"),
                                ],
                                &mut row,
                            );
                            Value::Object(row)
                        })
                        .collect()
                })
                .unwrap_or_default();
            fields.insert("candidates".into(), Value::Array(candidates));
            let folder = text(fanout.get("folder")).or_else(|| text(fanout.get("repo")))?;
            (KIND_FANOUT, PathBuf::from(folder))
        }
        "perms.allowed" => {
            let session_folder = payload
                .get("session")
                .filter(|s| s.is_object())
                .and_then(|s| session_fields(s, &mut fields));
            fields.insert("subject".into(), Value::from("permission"));
            copy(
                payload,
                &[
                    ("agent", "agent"),
                    ("file", "file"),
                    ("rules", "rules"),
                    ("dangerous", "dangerous"),
                ],
                &mut fields,
            );
            let folder = text(payload.get("folder")).map(PathBuf::from);
            (KIND_APPROVE, folder.or(session_folder)?)
        }
        _ => return None,
    };
    Some(AuditRecord {
        kind,
        at,
        folder,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn session() -> Value {
        json!({
            "id": "s1", "name": "sh 1", "kind": "shell", "state": "running",
            "workspace_id": "w1", "workspace": "repo", "cwd": "/repo",
            "worktree": null, "branch": "main", "agent": null,
        })
    }

    #[test]
    fn session_start_and_end_carry_the_session_and_exit_status() {
        let start = from_lifecycle(
            "session.created",
            &json!({ "session": session() }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(start.kind, KIND_SESSION_START);
        assert_eq!(start.folder, PathBuf::from("/repo"));
        assert_eq!(start.fields["session_id"], "s1");
        assert_eq!(start.fields["session_kind"], "shell");
        assert_eq!(start.fields["cwd"], "/repo");
        assert_eq!(start.fields["branch"], "main");

        let payload = json!({
            "session": session(),
            "exit": { "code": null, "signal": 1 },
            "hub_stopping": true,
        });
        let end = from_lifecycle(
            "session.exited",
            &payload,
            Some("Done.\n\nAll   tests pass."),
            at(),
        )
        .unwrap();
        assert_eq!(end.kind, KIND_SESSION_END);
        assert_eq!(end.fields["exit_code"], Value::Null);
        assert_eq!(end.fields["signal"], 1);
        assert_eq!(end.fields["hub_stopping"], true);
        assert_eq!(end.fields["summary"], "Done. All tests pass.");
    }

    #[test]
    fn a_worktree_session_records_against_its_worktree() {
        let mut s = session();
        s["worktree"] = json!({
            "repo": "/repo", "path": "/repo.wt/fix", "branch": "fix", "base": "main",
        });
        let merge = from_lifecycle(
            "review.merged",
            &json!({
                "session": s, "dir": "/repo.wt/fix", "head": "abc123",
                "url": null, "summary": "merged fix into main", "branch": "fix", "base": "main",
            }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(merge.kind, KIND_MERGE);
        assert_eq!(merge.folder, PathBuf::from("/repo.wt/fix"));
        assert_eq!(merge.fields["worktree"], "/repo.wt/fix");
        assert_eq!(merge.fields["commit"], "abc123");
        assert_eq!(merge.fields["branch"], "fix");
        assert_eq!(merge.fields["base"], "main");
        assert!(!merge.fields.contains_key("url"));
    }

    #[test]
    fn review_actions_pr_handoff_fanout_and_permissions_map_to_their_kinds() {
        let pr = from_lifecycle(
            "review.pr_opened",
            &json!({ "session": session(), "dir": "/repo", "url": "https://x/pr/1", "head": "c1" }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(
            (pr.kind, &pr.fields["url"]),
            (KIND_PR, &json!("https://x/pr/1"))
        );

        let push = from_lifecycle(
            "review.pushed",
            &json!({ "session": session(), "dir": "/repo" }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(
            (push.kind, &push.fields["action"]),
            (KIND_REVIEW, &json!("push"))
        );

        let asked = from_lifecycle(
            "review.requested",
            &json!({
                "author": session(),
                "reviewer": { "id": "s2", "kind": "codex" },
                "reused": false, "dir": "/repo", "branch": "fix", "base": "main",
            }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(asked.kind, KIND_REVIEW);
        assert_eq!(asked.fields["action"], "request");
        assert_eq!(asked.fields["reviewer_session_id"], "s2");

        let handoff = from_lifecycle(
            "handoff.sent",
            &json!({ "from": null, "to": session(), "source": "file", "bytes": 12 }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(
            (handoff.kind, &handoff.fields["bytes"]),
            (KIND_HANDOFF, &json!(12))
        );

        let fanout = from_lifecycle(
            "fanout.finished",
            &json!({ "fanout": {
                "id": "f1", "name": "try", "folder": "/repo", "repo": "/repo", "base": "main",
                "winner": "a", "counts": { "passed": 1 },
                "candidates": [{ "label": "a", "verdict": "pass", "passed": true,
                                 "head": "c9", "diff": "big", "tail": "out" }],
            }}),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(fanout.kind, KIND_FANOUT);
        assert_eq!(fanout.folder, PathBuf::from("/repo"));
        assert_eq!(fanout.fields["winner"], "a");
        let candidate = &fanout.fields["candidates"][0];
        assert_eq!(candidate["commit"], "c9");
        assert!(candidate.get("diff").is_none() && candidate.get("tail").is_none());

        let perms = from_lifecycle(
            "perms.allowed",
            &json!({ "session": null, "agent": "claude", "rules": ["Bash(ls)"],
                     "folder": "/repo", "dangerous": false }),
            None,
            at(),
        )
        .unwrap();
        assert_eq!(
            (perms.kind, &perms.fields["subject"]),
            (KIND_APPROVE, &json!("permission"))
        );
    }

    #[test]
    fn other_events_and_folderless_payloads_record_nothing() {
        assert!(
            from_lifecycle(
                "share.created",
                &json!({ "session": session() }),
                None,
                at()
            )
            .is_none()
        );
        assert!(from_lifecycle("session.created", &json!({}), None, at()).is_none());
    }

    #[test]
    fn summaries_are_one_capped_line() {
        assert_eq!(summary("  \n "), None);
        let long = "word ".repeat(200);
        let cut = summary(&long).unwrap();
        assert_eq!(cut.chars().count(), SUMMARY_CHARS);
        assert!(cut.ends_with("..."));
    }

    struct Collect(Mutex<Vec<&'static str>>);

    impl AuditSink for Collect {
        fn record(&self, record: &AuditRecord) -> Result<Recorded, String> {
            if record.kind == KIND_DENY {
                return Err("store down".into());
            }
            self.0.lock().unwrap().push(record.kind);
            Ok(Recorded::NoProject)
        }
    }

    #[test]
    fn the_recorder_writes_in_order_survives_a_failed_write_and_drains_on_close() {
        let sink = Arc::new(Collect(Mutex::new(Vec::new())));
        let recorder = AuditRecorder::start(Arc::clone(&sink) as Arc<dyn AuditSink>).unwrap();
        for kind in [KIND_SESSION_START, KIND_DENY, KIND_SESSION_END] {
            recorder.submit(AuditRecord {
                kind,
                at: at(),
                folder: PathBuf::from("/repo"),
                fields: Map::new(),
            });
        }
        recorder.close(FLUSH_TIMEOUT);
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![KIND_SESSION_START, KIND_SESSION_END]
        );
        // Closed: later records are dropped, a second close is a no-op.
        recorder.submit(AuditRecord {
            kind: KIND_MERGE,
            at: at(),
            folder: PathBuf::from("/repo"),
            fields: Map::new(),
        });
        recorder.close(FLUSH_TIMEOUT);
        assert_eq!(sink.0.lock().unwrap().len(), 2);
    }
}
