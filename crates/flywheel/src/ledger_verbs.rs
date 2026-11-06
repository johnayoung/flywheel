//! The audit record of the cwd project: `fw audit --project` and `fw purge`.
//!
//! * `fw audit --project [--json]` -- every audit-ledger row of the project
//!   at the working directory, oldest first (newest last): one
//!   `transcript` row per finalized run transcript (`uri`, `sha256`,
//!   `bytes`, `run_id`) and every `audit_log` row (`autopilot.cycle`,
//!   `autopilot.decision`, `autopilot.approve`, `autopilot.deny`, ...).
//!   `--json` prints NDJSON: one object per row with `kind`, `at`
//!   (RFC 3339), and the kind's fields. Reads whichever store the project
//!   is configured for (sqlite or postgres).
//! * `fw purge` -- delete every transcript body of the cwd project now
//!   (after the retention sweep); ledger rows stay.
//! * `fw purge --project [--yes]` -- delete the whole project dir
//!   (`$FLYWHEEL_HOME/projects/<id>/`) after a confirmation; unattended
//!   (stdin not a terminal) it requires `--yes`.
//!
//! Ledger rows are append-only: nothing here deletes one. (Deleting a
//! project dir removes its sqlite store file with it; a postgres store's
//! rows stay.)

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

use flywheel_core::store::{HarnessStore, LedgerFilter, StoreError};
use flywheel_core::transcripts::{purge_bodies, sweep_expired};
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::policy::{ProjectBinding, WorkPolicy};
use flywheel_orchestrator::workflow::{load_effective_policy, resolve_db_path};

use crate::tui::open_store_bundle;

pub const PURGE_VERB: &str = "purge";

/// `fw audit --project --help`.
pub const AUDIT_PROJECT_USAGE: &str = "usage: fw audit --project [--json]

Print the audit record of the project at the working directory, oldest
first: one 'transcript' row per finalized run transcript (uri, sha256,
bytes, run_id) and every ledger row (autopilot.cycle, autopilot.decision,
autopilot.approve, autopilot.deny, ...). Rows are kept forever.

options:
  --json   NDJSON: one object per row with kind, at (RFC 3339), and the
           kind's fields
";

/// `fw purge --help`.
pub const PURGE_USAGE: &str = "usage: fw purge [--project [--yes]]

Delete the run transcript bodies of the project at the working directory
(every file under $FLYWHEEL_HOME/projects/<id>/transcripts/). Their audit
rows stay ('fw audit --project' still lists them). Bodies older than
transcript_retention_days are also deleted automatically at worker start.

options:
  --project   delete the whole project dir ($FLYWHEEL_HOME/projects/<id>/:
              store, tasks, specs, transcripts, worktrees) after a
              confirmation
  --yes, -y   skip the confirmation (required when stdin is not a terminal)
";

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help")
}

/// The cwd's home project policy (store, project dir), or an error naming
/// why there is none.
fn project_policy() -> Result<(WorkPolicy, ProjectBinding), String> {
    let policy = load_effective_policy(Access::ReadOnly)
        .map_err(|e| format!("policy error: {e}"))?
        .ok_or_else(|| "no project at the working directory".to_owned())?;
    let project = policy
        .project
        .clone()
        .ok_or_else(|| "no home project at the working directory".to_owned())?;
    Ok((policy, project))
}

// --- fw audit --project -----------------------------------------------------------------

/// One audit row as `fw audit --project` prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub kind: String,
    pub at: DateTime<Utc>,
    /// Every kind-specific field (`run_id` included when the row has one).
    pub fields: Map<String, Value>,
}

impl AuditRow {
    /// The NDJSON object: `kind`, `at`, then the kind's fields.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("kind".into(), Value::String(self.kind.clone()));
        object.insert(
            "at".into(),
            Value::String(self.at.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        );
        for (key, value) in &self.fields {
            if key != "kind" && key != "at" {
                object.insert(key.clone(), value.clone());
            }
        }
        Value::Object(object)
    }

    /// One human-readable line.
    pub fn to_line(&self) -> String {
        let at = self.at.to_rfc3339_opts(SecondsFormat::Secs, true);
        let text = |key: &str| self.fields.get(key).and_then(Value::as_str).unwrap_or("");
        if self.kind == "transcript" {
            let sha = text("sha256");
            return format!(
                "{at}  transcript  run={} bytes={} sha256={} {}",
                text("run_id"),
                self.fields.get("bytes").cloned().unwrap_or(Value::Null),
                sha.get(..12).unwrap_or(sha),
                text("uri"),
            );
        }
        let detail = Value::Object(self.fields.clone()).to_string();
        format!("{at}  {}  {detail}", self.kind)
    }
}

/// Every audit row of `store`'s partition, oldest first: transcript rows
/// and ledger rows merged by time (ledger rows first on a tie).
pub fn audit_rows(store: &dyn HarnessStore) -> Result<Vec<AuditRow>, StoreError> {
    let mut rows: Vec<(DateTime<Utc>, u8, usize, AuditRow)> = Vec::new();
    for (n, entry) in store
        .list_ledger_entries(&LedgerFilter::default())?
        .into_iter()
        .enumerate()
    {
        let mut fields = Map::new();
        if let Some(run_id) = entry.run_id {
            fields.insert("run_id".into(), Value::String(run_id));
        }
        for (key, value) in entry.payload {
            fields.entry(key).or_insert(value);
        }
        rows.push((
            entry.at,
            0,
            n,
            AuditRow {
                kind: entry.kind,
                at: entry.at,
                fields,
            },
        ));
    }
    for (n, record) in store.list_transcripts()?.into_iter().enumerate() {
        let mut fields = Map::new();
        fields.insert("run_id".into(), Value::String(record.run_id));
        fields.insert("uri".into(), Value::String(record.uri));
        fields.insert("sha256".into(), Value::String(record.sha256));
        fields.insert("bytes".into(), Value::from(record.bytes));
        rows.push((
            record.created_at,
            1,
            n,
            AuditRow {
                kind: "transcript".into(),
                at: record.created_at,
                fields,
            },
        ));
    }
    rows.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    Ok(rows.into_iter().map(|(_, _, _, row)| row).collect())
}

/// `fw audit --project [--json]` (`args` is the audit argv; `--project`
/// is among them).
pub fn run_audit_project(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if wants_help(args) {
        let _ = out.write_all(AUDIT_PROJECT_USAGE.as_bytes());
        return 0;
    }
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--project" => {}
            "--json" => json = true,
            other => {
                let _ = writeln!(
                    err,
                    "{AUDIT_PROJECT_USAGE}fw audit: error: unexpected argument with --project: {other}"
                );
                return 2;
            }
        }
    }
    let (policy, _project) = match project_policy() {
        Ok(found) => found,
        Err(message) => {
            let _ = writeln!(err, "fw audit: {message}");
            return 2;
        }
    };
    let db_path = resolve_db_path(None, Some(&policy));
    let bundle = match open_store_bundle(Some(&policy), &db_path) {
        Ok(bundle) => bundle,
        Err(message) => {
            let _ = writeln!(err, "fw audit: {message}");
            return 2;
        }
    };
    let rows = match audit_rows(bundle.core.as_ref()) {
        Ok(rows) => rows,
        Err(failure) => {
            let _ = writeln!(err, "fw audit: reading the audit record: {failure}");
            return 1;
        }
    };
    for row in &rows {
        let line = if json {
            row.to_json().to_string()
        } else {
            row.to_line()
        };
        if writeln!(out, "{line}").is_err() {
            return 1;
        }
    }
    if rows.is_empty() && !json {
        let _ = writeln!(err, "fw audit: no audit rows for this project yet");
    }
    0
}

// --- fw purge ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct PurgeArgs {
    project: bool,
    yes: bool,
}

fn parse_purge(args: &[String]) -> Result<PurgeArgs, String> {
    let mut parsed = PurgeArgs::default();
    for arg in args {
        match arg.as_str() {
            "--project" => parsed.project = true,
            "--yes" | "-y" => parsed.yes = true,
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    if parsed.yes && !parsed.project {
        return Err("--yes only applies to --project".into());
    }
    Ok(parsed)
}

/// The transcript dirs of a project: `transcripts/` plus the pre-ledger
/// `logs/runs/` location run files used to land in.
fn transcript_dirs(project: &ProjectBinding) -> [PathBuf; 2] {
    let paths = project.paths();
    [paths.transcripts(), paths.logs().join("runs")]
}

/// Run `fw purge ...` (`args` excludes the verb).
pub fn run_purge(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        let mut input = stdin.lock();
        purge_with(args, Some(&mut input), out, err)
    } else {
        purge_with(args, None, out, err)
    }
}

/// [`run_purge`] with the confirmation input injected (`None`: unattended).
pub fn purge_with(
    args: &[String],
    confirm: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if wants_help(args) {
        let _ = out.write_all(PURGE_USAGE.as_bytes());
        return 0;
    }
    let parsed = match parse_purge(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(err, "{PURGE_USAGE}fw purge: error: {message}");
            return 2;
        }
    };
    let (policy, project) = match project_policy() {
        Ok(found) => found,
        Err(message) => {
            let _ = writeln!(err, "fw purge: {message}");
            return 2;
        }
    };
    if parsed.project {
        return purge_project(&project, parsed.yes, confirm, out, err);
    }
    let mut code = 0;
    let mut total = 0usize;
    let swept = sweep_expired(
        &project.paths().transcripts(),
        policy.transcript_retention_days,
        SystemTime::now(),
    );
    match swept {
        Ok(deleted) => total += deleted.len(),
        Err(failure) => {
            let _ = writeln!(err, "fw purge: retention sweep failed: {failure}");
            code = 1;
        }
    }
    for dir in transcript_dirs(&project) {
        match purge_bodies(&dir) {
            Ok(deleted) => total += deleted.len(),
            Err(failure) => {
                let _ = writeln!(err, "fw purge: {}: {failure}", dir.display());
                code = 1;
            }
        }
    }
    let _ = writeln!(
        out,
        "deleted {total} transcript bod{} of project {} from {} (audit rows kept)",
        if total == 1 { "y" } else { "ies" },
        project.id,
        project.paths().transcripts().display()
    );
    code
}

fn purge_project(
    project: &ProjectBinding,
    yes: bool,
    confirm: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let dir = project.dir.clone();
    if !yes {
        let Some(input) = confirm else {
            let _ = writeln!(
                err,
                "fw purge: refusing to delete {} without confirmation (stdin is not a terminal); \
                 pass --yes",
                dir.display()
            );
            return 2;
        };
        let _ = write!(
            out,
            "delete {} (project {}: store, tasks, specs, transcripts, worktrees)? [y/N] ",
            dir.display(),
            project.id
        );
        let _ = out.flush();
        let mut answer = String::new();
        if input.read_line(&mut answer).is_err()
            || !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        {
            let _ = writeln!(out, "nothing deleted");
            return 1;
        }
    }
    let files = count_files(&dir);
    if let Err(failure) = std::fs::remove_dir_all(&dir)
        && failure.kind() != std::io::ErrorKind::NotFound
    {
        let _ = writeln!(err, "fw purge: deleting {}: {failure}", dir.display());
        return 1;
    }
    // The project's worktrees lived in that dir: drop their stale git
    // registrations (best effort).
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&project.repo_root)
        .args(["worktree", "prune"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = writeln!(
        out,
        "deleted project dir {} ({files} file(s)) of project {}",
        dir.display(),
        project.id
    );
    0
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => count_files(&entry.path()),
            Ok(_) => 1,
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::store::{LedgerEntry, LedgerStore, TranscriptRecord};
    use flywheel_core::store_memory::InMemoryStore;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn rows_merge_by_time_and_print_as_ndjson() {
        let store = InMemoryStore::new();
        store
            .append_transcript(&TranscriptRecord {
                run_id: "run-1".into(),
                uri: "file:///h/projects/p/transcripts/run-1.jsonl".into(),
                sha256: "ab".repeat(32),
                bytes: 42,
                created_at: at(2),
                id: None,
            })
            .unwrap();
        let mut payload = Map::new();
        payload.insert("cycle".into(), Value::from(1));
        store
            .append_ledger_entry(&LedgerEntry::new("autopilot.cycle", at(1), payload))
            .unwrap();
        let rows = audit_rows(&store).unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, ["autopilot.cycle", "transcript"]);
        let json = rows[1].to_json();
        assert_eq!(json["kind"], "transcript");
        assert_eq!(json["bytes"], 42);
        assert_eq!(json["run_id"], "run-1");
        assert_eq!(json["sha256"], "ab".repeat(32));
        let stamp = json["at"].as_str().unwrap();
        assert_eq!(DateTime::parse_from_rfc3339(stamp).unwrap(), at(2));
        assert_eq!(rows[0].to_json()["cycle"], 1);
        assert!(rows[1].to_line().contains("transcript  run=run-1 bytes=42"));
    }

    #[test]
    fn purge_flags_parse_and_yes_needs_project() {
        let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        assert_eq!(parse_purge(&args(&[])).unwrap(), PurgeArgs::default());
        assert_eq!(
            parse_purge(&args(&["--project", "-y"])).unwrap(),
            PurgeArgs {
                project: true,
                yes: true
            }
        );
        assert!(parse_purge(&args(&["--yes"])).is_err());
        assert!(parse_purge(&args(&["--all"])).is_err());
    }
}
