//! The hub's audit sink: hub actions (`hub.session.start`, `hub.merge`,
//! ...; see `flywheel_hub::audit`) appended to the `audit_log` of the
//! project each touched, where `fw audit --project` reads them.
//!
//! Project resolution is read-only: a folder records only when it is in a
//! git repo that is already a flywheel project (its id in the repo's git
//! config, its project dir under `$FLYWHEEL_HOME`) and resolving it would
//! not mint an id (a copy of a project repo is not that project). Anything
//! else -- not a git repo, a repo flywheel never ran in -- records nothing
//! and creates nothing: no project dir, no `.git/config` write. Opening a
//! hub session is not using flywheel in that repo; the first `fw` verb run
//! there makes it a project, and hub actions are recorded from then on.

use std::path::{Path, PathBuf};

use flywheel_core::store::LedgerEntry;
use flywheel_hub::audit::{AuditRecord, AuditSink, Recorded};
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::home::{Home, worktree_toplevel, would_mint};
use flywheel_orchestrator::workflow::{known_project, project_policy_at, resolve_db_path};
use serde_json::Value;

use crate::tui::open_store_bundle;

/// Appends hub actions to their projects' stores.
#[derive(Debug, Default)]
pub struct HubAudit {
    /// The home projects resolve under; `None` reads `$FLYWHEEL_HOME`
    /// (default `~/.flywheel`) on each record.
    home: Option<PathBuf>,
}

impl HubAudit {
    /// A sink resolving projects under `$FLYWHEEL_HOME`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink resolving projects under the home at `root` (tests,
    /// embedders).
    pub fn with_home(root: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(root.into()),
        }
    }

    fn home(&self) -> Result<Home, String> {
        match &self.home {
            Some(root) => Ok(Home::at(root)),
            None => Home::from_env().map_err(|e| e.to_string()),
        }
    }
}

/// The id of the project `folder` is already in, never creating one.
pub fn existing_project(home: &Home, folder: &Path) -> Option<String> {
    if !folder.is_dir() {
        return None;
    }
    let id = known_project(home, folder)?;
    match would_mint(home, folder) {
        Ok(false) => Some(id),
        _ => None,
    }
}

impl AuditSink for HubAudit {
    fn record(&self, record: &AuditRecord) -> Result<Recorded, String> {
        let home = self.home()?;
        if existing_project(&home, &record.folder).is_none() {
            return Ok(Recorded::NoProject);
        }
        let (policy, _warnings) = project_policy_at(&home, &record.folder, Access::ReadOnly)
            .map_err(|e| e.to_string())?;
        let project = policy
            .project
            .as_ref()
            .map(|p| p.id.clone())
            .ok_or_else(|| format!("{} resolves to no project", record.folder.display()))?;
        let db_path = resolve_db_path(None, Some(&policy));
        let bundle = open_store_bundle(Some(&policy), &db_path)?;
        let mut payload = record.fields.clone();
        if let Ok(repo) = worktree_toplevel(&record.folder) {
            payload.insert("repo".into(), Value::from(repo.display().to_string()));
        }
        bundle
            .core
            .append_ledger_entry(&LedgerEntry::new(record.kind, record.at, payload))
            .map_err(|e| format!("appending to the audit record: {e}"))?;
        Ok(Recorded::Project(project))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::store::{LedgerFilter, LedgerStore};
    use flywheel_orchestrator::home::{resolve_project, stored_project_id};
    use serde_json::{Map, json};

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn record(folder: &Path) -> AuditRecord {
        let mut fields = Map::new();
        fields.insert("session_id".into(), json!("s1"));
        fields.insert("session_kind".into(), json!("shell"));
        AuditRecord {
            kind: flywheel_hub::audit::KIND_SESSION_START,
            at: chrono::Utc::now(),
            folder: folder.to_path_buf(),
            fields,
        }
    }

    #[test]
    fn a_repo_that_is_no_project_records_nothing_and_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home_root = tmp.path().join("home");
        let sink = HubAudit::with_home(&home_root);

        // Not a git repo.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(sink.record(&record(&plain)), Ok(Recorded::NoProject));
        // A directory that is gone.
        assert_eq!(
            sink.record(&record(&tmp.path().join("gone"))),
            Ok(Recorded::NoProject)
        );
        // A git repo flywheel never ran in.
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        assert_eq!(sink.record(&record(&repo)), Ok(Recorded::NoProject));
        assert_eq!(stored_project_id(&repo), Ok(None), "no id minted");
        assert!(!home_root.exists(), "no home state written");
    }

    #[test]
    fn a_project_repo_records_in_its_store_and_a_copy_does_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home_root = tmp.path().join("home");
        let home = Home::at(&home_root);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        git(&repo, &["init", "-q"]);
        let project = resolve_project(&home, &repo).unwrap();

        let sink = HubAudit::with_home(&home_root);
        assert_eq!(
            sink.record(&record(&repo.join("sub"))),
            Ok(Recorded::Project(project.id.clone()))
        );
        let (policy, _) = project_policy_at(&home, &repo, Access::ReadOnly).unwrap();
        let bundle =
            open_store_bundle(Some(&policy), &resolve_db_path(None, Some(&policy))).unwrap();
        let rows = bundle
            .core
            .list_ledger_entries(&LedgerFilter::kind_prefix("hub."))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "hub.session.start");
        assert_eq!(rows[0].payload["session_id"], "s1");
        let toplevel = worktree_toplevel(&repo).unwrap();
        assert_eq!(
            rows[0].payload["repo"],
            json!(toplevel.display().to_string())
        );

        // A copy of the repo (same id in its git config): not that project.
        let copy = tmp.path().join("copy");
        let status = std::process::Command::new("cp")
            .arg("-R")
            .arg(&repo)
            .arg(&copy)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(sink.record(&record(&copy)), Ok(Recorded::NoProject));
        assert_eq!(
            stored_project_id(&copy).unwrap().as_deref(),
            Some(project.id.as_str()),
            "the copy's id is left alone"
        );
    }
}
