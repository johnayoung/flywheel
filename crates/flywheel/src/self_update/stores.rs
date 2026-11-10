//! The store half of `fw self-update`: plan, back up, upgrade, restore.
//!
//! * Plan (after the build, before the drain): every project in `fw
//!   project ls` is asked, through the NEWLY BUILT `fw db status --project
//!   ID --json`, for its store's recorded and target schema versions, so
//!   targets never come from the running binary. A store that is behind is
//!   planned; a Postgres store that is behind without `--upgrade-postgres`,
//!   an unreachable one, or one newer than the build refuses the run.
//! * Backup (after the drain): each planned SQLite store is snapshotted
//!   with `VACUUM INTO` (a consistent read that includes committed rows
//!   still in the WAL) to `projects/<id>/backups/<UTC>-run<N>-orch<M>.sqlite`,
//!   converted to a single self-contained file, and only then published by
//!   rename; the 5 newest per project are kept.
//! * Upgrade (after the install): the installed `fw db upgrade --project
//!   ID` -- the same store and partition as `fw db upgrade` in that repo.
//! * Restore (revert): the SQLite online-backup API copies a backup back
//!   into the live file under SQLite's own locking.

use crate::db_verbs::{Versions, sqlite_versions};
use flywheel_orchestrator::home::{Home, list_projects};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Backups kept per project.
pub const BACKUPS_KEPT: usize = 5;

/// Which backend a planned store lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Sqlite(PathBuf),
    /// The redacted DSN (for messages only).
    Postgres(String),
}

/// One store that will be upgraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub project_id: String,
    pub backend: Backend,
    pub from: Versions,
    pub to: (u32, u32),
}

impl Planned {
    pub fn store_name(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite(_) => "sqlite",
            Backend::Postgres(_) => "postgres",
        }
    }

    /// The summary's `upgraded` entry.
    pub fn to_json(&self, backup: Option<&Path>) -> Value {
        json!({
            "project_id": self.project_id,
            "store": self.store_name(),
            "from": {"run": self.from.run, "orchestrator": self.from.orchestrator},
            "to": {"run": self.to.0, "orchestrator": self.to.1},
            "backup": backup.map(|p| p.display().to_string()),
        })
    }

    /// `<id> (sqlite run 14 -> 15, orchestrator 9 -> 11)`.
    pub fn describe(&self) -> String {
        format!(
            "{} ({} run {} -> {}, orchestrator {} -> {})",
            self.project_id,
            self.store_name(),
            show(self.from.run),
            self.to.0,
            show(self.from.orchestrator),
            self.to.1
        )
    }
}

fn show(v: Option<u32>) -> String {
    v.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

/// A child `fw` confined to this home: `FLYWHEEL_HOME` pinned, cwd the
/// home root (the verbs used here never read the cwd).
fn fw_command(fw: &Path, home: &Home, args: &[&str]) -> Command {
    let mut command = Command::new(fw);
    command
        .args(args)
        .current_dir(home.root())
        .env("FLYWHEEL_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

/// Plan the stores to upgrade, asking `new_fw` (the built artifact) for
/// each project's versions. `Err` is a refusal (exit 3) listing every
/// project that blocks the run.
pub fn plan(home: &Home, new_fw: &Path, upgrade_postgres: bool) -> Result<Vec<Planned>, String> {
    let projects =
        list_projects(home).map_err(|e| format!("cannot list this home's projects: {e}"))?;
    let mut planned = Vec::new();
    let mut refusals = Vec::new();
    for project in projects {
        let id = project.id.as_str();
        let output = fw_command(new_fw, home, &["db", "status", "--project", id, "--json"])
            .output()
            .map_err(|e| format!("cannot run {} db status: {e}", new_fw.display()))?;
        if !output.status.success() {
            refusals.push(format!(
                "project {id}: store unreadable or unreachable: {}",
                last_line(&output.stderr)
            ));
            continue;
        }
        let status: Value = match serde_json::from_slice(&output.stdout) {
            Ok(status) => status,
            Err(e) => {
                refusals.push(format!(
                    "project {id}: the new fw printed an unreadable db status: {e}"
                ));
                continue;
            }
        };
        let target = (
            json_u32(&status["target"]["run"]),
            json_u32(&status["target"]["orchestrator"]),
        );
        let (Some(run_target), Some(orch_target)) = target else {
            refusals.push(format!(
                "project {id}: the new fw reported no target schema versions"
            ));
            continue;
        };
        if status["current"].is_null() {
            continue; // no store yet: nothing to back up or upgrade
        }
        let from = Versions {
            run: json_u32(&status["current"]["run"]),
            orchestrator: json_u32(&status["current"]["orchestrator"]),
        };
        let location = status["location"].as_str().unwrap_or_default().to_owned();
        if from.run.is_some_and(|v| v > run_target)
            || from.orchestrator.is_some_and(|v| v > orch_target)
        {
            refusals.push(format!(
                "project {id}: store {location} is newer than the build (run {}, orchestrator \
                 {}; the build has {run_target}, {orch_target})",
                show(from.run),
                show(from.orchestrator)
            ));
            continue;
        }
        if from.run == Some(run_target) && from.orchestrator == Some(orch_target) {
            continue;
        }
        let backend = match status["store"].as_str() {
            Some("postgres") => {
                if !upgrade_postgres {
                    refusals.push(format!(
                        "project {id}: postgres store {location} is behind the build (run {} -> \
                         {run_target}, orchestrator {} -> {orch_target}); take your own backup, \
                         then pass --upgrade-postgres",
                        show(from.run),
                        show(from.orchestrator)
                    ));
                    continue;
                }
                Backend::Postgres(location)
            }
            _ => Backend::Sqlite(PathBuf::from(location)),
        };
        planned.push(Planned {
            project_id: id.to_owned(),
            backend,
            from,
            to: (run_target, orch_target),
        });
    }
    if refusals.is_empty() {
        Ok(planned)
    } else {
        Err(refusals.join("; "))
    }
}

fn json_u32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// `projects/<id>/backups`.
pub fn backups_dir(home: &Home, project_id: &str) -> PathBuf {
    home.project(project_id).root().join("backups")
}

/// Whether `name` is a backup file name this module writes.
fn is_backup_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".sqlite") else {
        return false;
    };
    let Some((stamp, rest)) = stem.split_once("-run") else {
        return false;
    };
    let Some((run, orch)) = rest.split_once("-orch") else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    stamp.len() == 16
        && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").is_ok()
        && digits(run)
        && digits(orch)
}

/// Every backup in `dir`, oldest first (the UTC stamp sorts by name).
pub fn list_backups(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_backup_name(n))
        .collect();
    names.sort();
    names.into_iter().map(|n| dir.join(n)).collect()
}

/// Remove all but the [`BACKUPS_KEPT`] newest backups in `dir`.
fn prune(dir: &Path) -> Result<(), String> {
    let all = list_backups(dir);
    let excess = all.len().saturating_sub(BACKUPS_KEPT);
    for old in &all[..excess] {
        std::fs::remove_file(old).map_err(|e| format!("cannot prune {}: {e}", old.display()))?;
    }
    Ok(())
}

/// Snapshot the planned SQLite store into its project's `backups/` and
/// return the published path. The file exists, complete and alone, before
/// this returns; nothing in the store changes.
pub fn backup(home: &Home, planned: &Planned) -> Result<PathBuf, String> {
    let Backend::Sqlite(source) = &planned.backend else {
        return Err(format!(
            "project {}: postgres stores are not backed up",
            planned.project_id
        ));
    };
    let dir = backups_dir(home, &planned.project_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let suffix = format!(
        "-run{}-orch{}.sqlite",
        planned.from.run.unwrap_or(0),
        planned.from.orchestrator.unwrap_or(0)
    );
    let mut final_path = None;
    for _ in 0..3 {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let candidate = dir.join(format!("{stamp}{suffix}"));
        if !candidate.exists() {
            final_path = Some(candidate);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    let final_path = final_path.ok_or_else(|| {
        format!(
            "cannot pick a free backup name in {} (one per second)",
            dir.display()
        )
    })?;
    let file_name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let cleanup = |tmp: &Path| {
        for path in [
            tmp.to_path_buf(),
            sidecar(tmp, "-wal"),
            sidecar(tmp, "-shm"),
            sidecar(tmp, "-journal"),
        ] {
            let _ = std::fs::remove_file(path);
        }
    };
    cleanup(&tmp);
    let made = snapshot(source, &tmp, planned.from);
    if let Err(why) = made {
        cleanup(&tmp);
        return Err(format!(
            "project {}: backup of {} failed: {why}",
            planned.project_id,
            source.display()
        ));
    }
    let published = std::fs::rename(&tmp, &final_path)
        .and_then(|()| std::fs::File::open(&dir).and_then(|d| d.sync_all()));
    if let Err(e) = published {
        cleanup(&tmp);
        return Err(format!(
            "project {}: cannot publish backup {}: {e}",
            planned.project_id,
            final_path.display()
        ));
    }
    prune(&dir)?;
    Ok(final_path)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `VACUUM INTO` `tmp` from `source` (read-only), then make `tmp` a
/// rollback-journal file that needs no sidecar, check it, and fsync it.
fn snapshot(source: &Path, tmp: &Path, expected: Versions) -> Result<(), String> {
    use rusqlite::{Connection, OpenFlags};
    let busy = std::time::Duration::from_secs(5);
    {
        let conn = Connection::open_with_flags(
            source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;
        conn.busy_timeout(busy).map_err(|e| e.to_string())?;
        conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])
            .map_err(|e| format!("VACUUM INTO: {e}"))?;
    }
    {
        let conn = Connection::open(tmp).map_err(|e| e.to_string())?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(format!("snapshot stayed in journal mode {mode}"));
        }
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if check != "ok" {
            return Err(format!("snapshot failed integrity_check: {check}"));
        }
    }
    let got = sqlite_versions(tmp)?;
    if got != Some(expected) {
        return Err(format!(
            "snapshot records {got:?}, the store recorded {expected:?} when planned"
        ));
    }
    std::fs::File::open(tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot fsync {}: {e}", tmp.display()))
}

/// Upgrade one project's store with the installed `fw db upgrade
/// --project ID`.
pub fn upgrade(installed_fw: &Path, home: &Home, planned: &Planned) -> Result<(), String> {
    let id = planned.project_id.as_str();
    let output = fw_command(installed_fw, home, &["db", "upgrade", "--project", id])
        .output()
        .map_err(|e| format!("cannot run {} db upgrade: {e}", installed_fw.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "project {id}: fw db upgrade failed: {}",
            last_line(&output.stderr)
        ))
    }
}

/// Whether the planned SQLite store no longer records its pre-run
/// versions (an unreadable store counts as changed, so it is restored).
pub fn changed(planned: &Planned) -> bool {
    match &planned.backend {
        Backend::Sqlite(path) => sqlite_versions(path).ok() != Some(Some(planned.from)),
        Backend::Postgres(_) => true,
    }
}

/// Copy `backup` back into the planned SQLite store through the online
/// backup API, then confirm it records the pre-run versions.
pub fn restore(planned: &Planned, backup: &Path) -> Result<(), String> {
    let Backend::Sqlite(path) = &planned.backend else {
        return Err(format!(
            "project {}: postgres store {} has no backup and stays upgraded",
            planned.project_id,
            match &planned.backend {
                Backend::Postgres(dsn) => dsn.as_str(),
                Backend::Sqlite(_) => "",
            }
        ));
    };
    let fail = |e: rusqlite::Error| {
        format!(
            "project {}: restore of {} from {} failed: {e}",
            planned.project_id,
            path.display(),
            backup.display()
        )
    };
    {
        let mut conn = rusqlite::Connection::open(path).map_err(fail)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(fail)?;
        conn.restore(
            rusqlite::MAIN_DB,
            backup,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .map_err(fail)?;
    }
    match sqlite_versions(path) {
        Ok(Some(v)) if v == planned.from => Ok(()),
        other => Err(format!(
            "project {}: after restore {} records {other:?}, expected {:?}",
            planned.project_id,
            path.display(),
            planned.from
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_round_trip_and_reject_strangers() {
        assert!(is_backup_name("20261110T120000Z-run14-orch9.sqlite"));
        for bad in [
            "20261110T120000Z-run14-orch9.sqlite.tmp",
            ".20261110T120000Z-run14-orch9.sqlite.tmp-1",
            "20261110T120000Z-run-orch9.sqlite",
            "2026-11-10-run14-orch9.sqlite",
            "notes.sqlite",
            "20261199T120000Z-run14-orch9.sqlite",
        ] {
            assert!(!is_backup_name(bad), "{bad}");
        }
    }

    #[test]
    fn prune_keeps_the_five_newest_backups_only() {
        let dir = tempfile::tempdir().unwrap();
        for second in 0..7 {
            let name = format!("2026111{second}T000000Z-run14-orch9.sqlite");
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::write(dir.path().join("keep-me.txt"), b"x").unwrap();
        prune(dir.path()).unwrap();
        let left: Vec<String> = list_backups(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), BACKUPS_KEPT);
        assert_eq!(left[0], "20261112T000000Z-run14-orch9.sqlite");
        assert!(dir.path().join("keep-me.txt").exists());
    }
}
