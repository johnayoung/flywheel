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
//! postgres DSN, partition = the project id); `--policy FILE` the legacy
//! policy's store; `--db PATH` that sqlite file. The two explicit forms skip
//! project resolution and bind the documented `default` partition.

use std::io::Write;
use std::path::{Path, PathBuf};

use flywheel_core::store::SchemaUpgrade;
use flywheel_core::store_postgres::{DEFAULT_SCHEMA, PostgresStore};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_orchestrator::claims::SqliteClaimStore;
use flywheel_orchestrator::claims_postgres::PostgresClaimStore;
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::store_factory::{PG_DSN_ENV, resolve_postgres_dsn};
use flywheel_orchestrator::workflow::{
    load_effective_policy, load_policy_file, resolve_db_path, store_partition,
};

pub const DB_VERB: &str = "db";

/// `fw db --help`.
pub const DB_USAGE: &str = "usage: fw db upgrade [--db PATH] [--policy FILE]

Migrate the store in place to this flywheel's schema version. Store verbs
refuse a store whose schema is behind and name this command; nothing is
migrated implicitly. Both the run tables and the orchestrator tables are
upgraded, each in one transaction; existing rows are assigned the store's
project id (the project_id partition column). Safe to re-run.

Default: the store of the project at the working directory (sqlite under
$FLYWHEEL_HOME/projects/<id>/, or the configured postgres DSN).

options:
  --db PATH       upgrade this sqlite file (partition 'default')
  --policy FILE   upgrade the store a legacy policy file selects
                  (partition 'default')
";

#[derive(Debug, Default)]
struct UpgradeArgs {
    db: Option<String>,
    policy: Option<String>,
}

fn parse_upgrade(args: &[String]) -> Result<UpgradeArgs, String> {
    let mut parsed = UpgradeArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let slot = match flag {
            "--db" => &mut parsed.db,
            "--policy" => &mut parsed.policy,
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
    Ok(parsed)
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
        Some(other) => {
            let _ = writeln!(err, "{DB_USAGE}fw db: error: unknown subcommand: {other}");
            2
        }
    }
}

fn run_upgrade(args: &UpgradeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // An explicit --db or --policy names the store, so the cwd project is
    // not resolved (and the partition is the documented default); otherwise
    // the cwd repo's home project supplies the store and the partition.
    let policy_path = args.policy.as_deref().map(Path::new);
    let loaded = if args.db.is_some() || policy_path.is_some() {
        load_policy_file(policy_path)
    } else {
        load_effective_policy(None, Access::ReadOnly)
    };
    let policy = match loaded {
        Ok(policy) => policy,
        Err(failure) => {
            let _ = writeln!(err, "fw: policy error: {failure}");
            return 2;
        }
    };
    let partition = store_partition(policy.as_ref());
    let outcome = match policy.as_ref().map(|p| p.store_backend.as_str()) {
        Some("postgres") => upgrade_postgres(policy.as_ref(), &partition),
        _ => {
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            upgrade_sqlite(&db_path, &partition)
        }
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

fn upgrade_postgres(policy: Option<&WorkPolicy>, partition: &str) -> Result<Vec<String>, String> {
    let dsn = policy
        .and_then(|p| p.store_dsn.as_ref())
        .map(|dsn| dsn.expose().to_owned())
        .or_else(|| {
            resolve_postgres_dsn(&flywheel_orchestrator::store_registry::environ_from_process())
        })
        .ok_or_else(|| {
            format!(
                "store backend is postgres but no DSN is configured (store = ... or {PG_DSN_ENV})"
            )
        })?;
    let schema = policy
        .and_then(|p| p.store_schema.clone())
        .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
    let core = PostgresStore::upgrade(&dsn, &schema, partition).map_err(|e| e.to_string())?;
    let claims =
        PostgresClaimStore::upgrade(&dsn, &schema, partition).map_err(|e| e.to_string())?;
    Ok(report(
        &format!("postgres schema '{schema}'"),
        partition,
        &core,
        &claims,
    ))
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
    fn parses_db_and_policy_in_both_spellings() {
        let parsed = parse_upgrade(&strings(&["--db", "a.sqlite", "--policy=p.toml"])).unwrap();
        assert_eq!(parsed.db.as_deref(), Some("a.sqlite"));
        assert_eq!(parsed.policy.as_deref(), Some("p.toml"));
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
