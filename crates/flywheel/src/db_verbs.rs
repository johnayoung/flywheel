//! `fw db upgrade`: migrate a project's store in place to this build's
//! schema.
//!
//! Every store verb refuses a store whose recorded schema version is behind
//! this build (naming this verb) rather than migrating it implicitly. The
//! upgrade migrates both halves of the store -- the core run tables and the
//! orchestrator's coordination tables -- each in its own transaction, from
//! any older supported version, filling the `project_id` partition column of
//! existing rows with the project the store is bound to. It is idempotent: a
//! second run, or a run against a store whose tables are already partitioned
//! but whose version row is behind, succeeds and only fixes the version.
//!
//! Which store: the cwd project's (sqlite at
//! `$FLYWHEEL_HOME/projects/<id>/flywheel.sqlite`, or the configured
//! postgres DSN, partition = the project id); `--project ID` that known
//! project's store, resolved from the home without the cwd (partition =
//! ID, the same store and result as running inside its repo); `--db PATH`
//! that sqlite file, which skips project resolution and binds the
//! documented `default` partition.
//!
//! `fw db status [--project ID | --db PATH] [--json]` reads, without
//! creating or changing anything, the recorded run and orchestrator schema
//! versions of that store next to this build's targets -- the
//! machine-readable surface `fw self-update` asks a freshly built `fw`
//! for, so planning never trusts the running binary's constants.

use std::io::Write;
use std::path::{Path, PathBuf};

use flywheel_core::store::{CURRENT_SCHEMA_VERSION, SchemaUpgrade};
use flywheel_core::store_postgres::{DEFAULT_SCHEMA, PostgresStore};
use flywheel_core::store_sqlite::{Recorded, SqliteStore, VERSION_TABLE, recorded_version};
use flywheel_orchestrator::claims::{
    CURRENT_ORCH_SCHEMA_VERSION, ORCH_VERSION_TABLE, SqliteClaimStore,
};
use flywheel_orchestrator::claims_postgres::PostgresClaimStore;
use flywheel_orchestrator::config::{
    Access, ConfigFlags, Interaction, StoreSetting, env_dsn_from_process, load_config, redact_dsn,
};
use flywheel_orchestrator::home::{self, Home, Project};
use flywheel_orchestrator::store_factory::{PG_DSN_ENV, resolve_postgres_dsn};
use flywheel_orchestrator::workflow::{load_effective_policy, resolve_db_path, store_partition};

pub const DB_VERB: &str = "db";

/// `fw db --help`.
pub const DB_USAGE: &str = "usage: fw db upgrade [--project ID | --db PATH]
       fw db status [--project ID | --db PATH] [--json]

Migrate the store in place to this flywheel's schema version. Store verbs
refuse a store whose schema is behind and name this command; nothing is
migrated implicitly. Both the run tables and the orchestrator tables are
upgraded, each in one transaction; existing rows are assigned the store's
project id (the project_id partition column). Safe to re-run.

Default: the store of the project at the working directory (sqlite under
$FLYWHEEL_HOME/projects/<id>/, or the configured postgres DSN).

status reads the store's recorded run and orchestrator schema versions and
this build's targets without creating or changing anything.

options:
  --project ID    the store of that known project (fw project ls), partition ID,
                  resolved without the working directory
  --db PATH       this sqlite file (partition 'default')
  --json          status only: one JSON object
";

#[derive(Debug, Default)]
struct UpgradeArgs {
    db: Option<String>,
    project: Option<String>,
    json: bool,
}

/// Parse `upgrade` (`allow_json = false`) or `status` flags.
fn parse_args(args: &[String], allow_json: bool) -> Result<UpgradeArgs, String> {
    let mut parsed = UpgradeArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if allow_json && arg == "--json" {
            parsed.json = true;
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let slot = match flag {
            "--db" => &mut parsed.db,
            "--project" => &mut parsed.project,
            other => return Err(format!("unrecognized argument: {other}")),
        };
        let value = match inline {
            Some(value) => value,
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} expects a value"))?,
        };
        if value.is_empty() {
            return Err(format!("{flag} expects a non-empty value"));
        }
        *slot = Some(value);
    }
    if parsed.db.is_some() && parsed.project.is_some() {
        return Err("--db and --project are mutually exclusive".to_owned());
    }
    Ok(parsed)
}

fn parse_upgrade(args: &[String]) -> Result<UpgradeArgs, String> {
    parse_args(args, false)
}

/// Run `fw db ...` (`args` excludes the verb).
pub fn run_db(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            let target: &mut dyn Write = if args.is_empty() { err } else { out };
            let _ = target.write_all(DB_USAGE.as_bytes());
            if args.is_empty() { 2 } else { 0 }
        }
        Some("upgrade") => {
            let rest = &args[1..];
            if rest.iter().any(|a| a == "-h" || a == "--help") {
                let _ = out.write_all(DB_USAGE.as_bytes());
                return 0;
            }
            match parse_upgrade(rest) {
                Ok(parsed) => run_upgrade(&parsed, out, err),
                Err(message) => {
                    let _ = writeln!(err, "{DB_USAGE}fw db upgrade: error: {message}");
                    2
                }
            }
        }
        Some("status") => {
            let rest = &args[1..];
            if rest.iter().any(|a| a == "-h" || a == "--help") {
                let _ = out.write_all(DB_USAGE.as_bytes());
                return 0;
            }
            match parse_args(rest, true) {
                Ok(parsed) => run_status(&parsed, out, err),
                Err(message) => {
                    let _ = writeln!(err, "{DB_USAGE}fw db status: error: {message}");
                    2
                }
            }
        }
        Some(other) => {
            let _ = writeln!(err, "{DB_USAGE}fw db: error: unknown subcommand: {other}");
            2
        }
    }
}

/// A store and the partition its rows belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreTarget {
    Sqlite { path: PathBuf, partition: String },
    Postgres { dsn: String, partition: String },
}

impl StoreTarget {
    fn partition(&self) -> &str {
        match self {
            Self::Sqlite { partition, .. } | Self::Postgres { partition, .. } => partition,
        }
    }
}

/// The store `fw db` operates on: `--project ID`'s, `--db PATH`, or the
/// cwd project's. `Err((message, exit code))`.
fn resolve_target(args: &UpgradeArgs, err: &mut dyn Write) -> Result<StoreTarget, (String, i32)> {
    if let Some(id) = args.project.as_deref() {
        let home = Home::from_env().map_err(|e| (e.to_string(), 2))?;
        let (target, warnings) = project_store(&home, id).map_err(|m| (m, 2))?;
        for warning in warnings {
            let _ = writeln!(err, "{warning}");
        }
        return Ok(target);
    }
    // An explicit --db names the store, so the cwd project is not resolved
    // (and the partition is the documented default); otherwise the cwd
    // repo's home project supplies the store and the partition.
    let policy = if args.db.is_some() {
        None
    } else {
        load_effective_policy(Access::ReadOnly)
            .map_err(|failure| (format!("policy error: {failure}"), 2))?
    };
    let partition = store_partition(policy.as_ref());
    if policy.as_ref().map(|p| p.store_backend.as_str()) == Some("postgres") {
        let dsn = policy
            .as_ref()
            .and_then(|p| p.store_dsn.as_ref())
            .map(|dsn| dsn.expose().to_owned())
            .or_else(|| {
                resolve_postgres_dsn(&flywheel_orchestrator::store_registry::environ_from_process())
            })
            .ok_or_else(|| {
                (
                    format!(
                        "store backend is postgres but no DSN is configured (store = ... or \
                         {PG_DSN_ENV})"
                    ),
                    1,
                )
            })?;
        return Ok(StoreTarget::Postgres { dsn, partition });
    }
    Ok(StoreTarget::Sqlite {
        path: resolve_db_path(args.db.as_deref(), policy.as_ref()),
        partition,
    })
}

/// The store of the known project `id` and the config warnings, resolved
/// from the home alone -- its meta.json, the user and personal config, the
/// repo file at its recorded path (honored only when trusted), and
/// `FLYWHEEL_PG_DSN` -- writing nothing. Partition = `id`.
pub fn project_store(home: &Home, id: &str) -> Result<(StoreTarget, Vec<String>), String> {
    let paths = home.project(id);
    let meta = home::read_meta(&paths)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no project {id} in {}", home.projects_dir().display()))?;
    let repo_root = meta.path.clone();
    let project = Project {
        id: id.to_owned(),
        paths: paths.clone(),
        meta,
    };
    let loaded = load_config(
        home,
        &project,
        &repo_root,
        Access::ReadOnly,
        &ConfigFlags::default(),
        env_dsn_from_process(),
        Interaction::Unattended,
    )
    .map_err(|e| e.to_string())?;
    let target = match &loaded.config.store.value {
        StoreSetting::Sqlite => StoreTarget::Sqlite {
            path: paths.db(),
            partition: id.to_owned(),
        },
        StoreSetting::Postgres(dsn) => StoreTarget::Postgres {
            dsn: dsn.expose().to_owned(),
            partition: id.to_owned(),
        },
    };
    Ok((target, loaded.warnings))
}

fn run_upgrade(args: &UpgradeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let target = match resolve_target(args, err) {
        Ok(target) => target,
        Err((message, code)) => {
            let _ = writeln!(err, "fw: {message}");
            return code;
        }
    };
    let partition = target.partition().to_owned();
    let outcome = match &target {
        StoreTarget::Postgres { dsn, .. } => upgrade_postgres(dsn, &partition),
        StoreTarget::Sqlite { path, .. } => upgrade_sqlite(path, &partition),
    };
    match outcome {
        Ok(report) => {
            for line in report {
                let _ = writeln!(out, "{line}");
            }
            0
        }
        Err(message) => {
            let _ = writeln!(err, "fw: db upgrade: {message}");
            1
        }
    }
}

fn upgrade_sqlite(db_path: &Path, partition: &str) -> Result<Vec<String>, String> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let core = SqliteStore::upgrade(db_path, partition).map_err(|e| e.to_string())?;
    let claims = SqliteClaimStore::upgrade(db_path, partition).map_err(|e| e.to_string())?;
    Ok(report(
        &format!("sqlite {}", PathBuf::from(db_path).display()),
        partition,
        &core,
        &claims,
    ))
}

fn upgrade_postgres(dsn: &str, partition: &str) -> Result<Vec<String>, String> {
    let schema = DEFAULT_SCHEMA.to_owned();
    let core = PostgresStore::upgrade(dsn, &schema, partition).map_err(|e| e.to_string())?;
    let claims = PostgresClaimStore::upgrade(dsn, &schema, partition).map_err(|e| e.to_string())?;
    Ok(report(
        &format!("postgres schema '{schema}'"),
        partition,
        &core,
        &claims,
    ))
}

/// The versions a store records: `None` for a sentinel that is missing or
/// unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Versions {
    pub run: Option<u32>,
    pub orchestrator: Option<u32>,
}

fn version_of(recorded: Recorded) -> Option<u32> {
    match recorded {
        Recorded::Missing => None,
        Recorded::Version(v) => v,
    }
}

/// The versions the sqlite file at `path` records, read through a
/// read-only connection (nothing is created, migrated, or checkpointed);
/// `Ok(None)` when no file exists.
pub fn sqlite_versions(path: &Path) -> Result<Option<Versions>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let fail = |e: rusqlite::Error| format!("cannot read {}: {e}", path.display());
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(fail)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    let run = recorded_version(&conn, VERSION_TABLE).map_err(fail)?;
    let orchestrator = recorded_version(&conn, ORCH_VERSION_TABLE).map_err(fail)?;
    if run == Recorded::Missing && orchestrator == Recorded::Missing {
        return Ok(None);
    }
    Ok(Some(Versions {
        run: version_of(run),
        orchestrator: version_of(orchestrator),
    }))
}

/// The versions the Postgres store at `dsn` records; `Ok(None)` when it
/// holds neither sentinel. Unreachable is an error naming the redacted DSN.
pub fn postgres_versions(dsn: &str) -> Result<Option<Versions>, String> {
    let recorded =
        PostgresStore::recorded_versions(dsn, DEFAULT_SCHEMA, &[VERSION_TABLE, ORCH_VERSION_TABLE])
            .map_err(|e| format!("cannot reach postgres store {}: {e}", redact_dsn(dsn)))?;
    let (run, orchestrator) = (recorded[0], recorded[1]);
    if run == Recorded::Missing && orchestrator == Recorded::Missing {
        return Ok(None);
    }
    Ok(Some(Versions {
        run: version_of(run),
        orchestrator: version_of(orchestrator),
    }))
}

fn versions_json(versions: Option<Versions>) -> serde_json::Value {
    versions.map_or(
        serde_json::Value::Null,
        |v| serde_json::json!({"run": v.run, "orchestrator": v.orchestrator}),
    )
}

fn run_status(args: &UpgradeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let target = match resolve_target(args, err) {
        Ok(target) => target,
        Err((message, code)) => {
            let _ = writeln!(err, "fw: {message}");
            return code;
        }
    };
    let (kind, location, current) = match &target {
        StoreTarget::Sqlite { path, .. } => {
            ("sqlite", path.display().to_string(), sqlite_versions(path))
        }
        StoreTarget::Postgres { dsn, .. } => ("postgres", redact_dsn(dsn), postgres_versions(dsn)),
    };
    let current = match current {
        Ok(current) => current,
        Err(message) => {
            let _ = writeln!(err, "fw: db status: {message}");
            return 1;
        }
    };
    if args.json {
        let body = serde_json::json!({
            "project_id": target.partition(),
            "store": kind,
            "location": location,
            "current": versions_json(current),
            "target": {"run": CURRENT_SCHEMA_VERSION, "orchestrator": CURRENT_ORCH_SCHEMA_VERSION},
        });
        let _ = writeln!(out, "{body}");
        return 0;
    }
    let show = |v: Option<u32>| v.map_or_else(|| "none".to_owned(), |v| v.to_string());
    let line = match current {
        None => format!(
            "{kind} {location} (project {}): no store yet; this build creates run schema \
             {CURRENT_SCHEMA_VERSION}, orchestrator schema {CURRENT_ORCH_SCHEMA_VERSION}",
            target.partition()
        ),
        Some(v) => format!(
            "{kind} {location} (project {}): run schema {} (this build {CURRENT_SCHEMA_VERSION}), \
             orchestrator schema {} (this build {CURRENT_ORCH_SCHEMA_VERSION})",
            target.partition(),
            show(v.run),
            show(v.orchestrator)
        ),
    };
    let _ = writeln!(out, "{line}");
    0
}

fn describe(label: &str, upgrade: &SchemaUpgrade) -> String {
    let keyed = if upgrade.partitioned.is_empty() {
        String::new()
    } else {
        format!(
            "; partitioned by project_id: {}",
            upgrade.partitioned.join(", ")
        )
    };
    match upgrade.from {
        None => format!("{label}: created at schema {}", upgrade.to),
        Some(from) if from == upgrade.to && upgrade.partitioned.is_empty() => {
            format!("{label}: already at schema {}", upgrade.to)
        }
        Some(from) => format!("{label}: schema {from} -> {}{keyed}", upgrade.to),
    }
}

fn report(
    target: &str,
    partition: &str,
    core: &SchemaUpgrade,
    claims: &SchemaUpgrade,
) -> Vec<String> {
    vec![
        format!("upgraded {target} (project {partition})"),
        format!("  {}", describe("run store", core)),
        format!("  {}", describe("orchestrator store", claims)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_db_in_both_spellings_and_refuses_the_removed_policy_flag() {
        let parsed = parse_upgrade(&strings(&["--db", "a.sqlite"])).unwrap();
        assert_eq!(parsed.db.as_deref(), Some("a.sqlite"));
        let parsed = parse_upgrade(&strings(&["--db=b.sqlite"])).unwrap();
        assert_eq!(parsed.db.as_deref(), Some("b.sqlite"));
        assert!(parse_upgrade(&strings(&["--policy=p.toml"])).is_err());
        assert!(parse_upgrade(&strings(&["--db"])).is_err());
        assert!(parse_upgrade(&strings(&["--db="])).is_err());
        assert!(parse_upgrade(&strings(&["--force"])).is_err());
    }

    #[test]
    fn help_and_bad_subcommands() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run_db(&strings(&["--help"]), &mut out, &mut err), 0);
        assert!(String::from_utf8_lossy(&out).contains("fw db upgrade"));
        assert_eq!(run_db(&strings(&["downgrade"]), &mut out, &mut err), 2);
        assert_eq!(run_db(&[], &mut out, &mut err), 2);
    }

    #[test]
    fn upgrades_an_explicit_sqlite_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested").join("flywheel.sqlite");
        let lines = upgrade_sqlite(&db, "default").unwrap();
        assert!(lines[1].contains("created at schema"), "{lines:?}");
        let again = upgrade_sqlite(&db, "default").unwrap();
        assert!(again[1].contains("already at schema"), "{again:?}");
        assert!(again[2].contains("already at schema"), "{again:?}");
        SqliteStore::open(&db).unwrap();
        SqliteClaimStore::open(&db).unwrap();
    }

    #[test]
    fn describe_names_the_re_keyed_tables() {
        let upgrade = SchemaUpgrade {
            from: Some(12),
            to: 13,
            partitioned: strings(&["tasks", "lifecycles"]),
        };
        assert_eq!(
            describe("run store", &upgrade),
            "run store: schema 12 -> 13; partitioned by project_id: tasks, lifecycles"
        );
    }
}
