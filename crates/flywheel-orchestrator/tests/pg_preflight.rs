//! Tests for the guided Postgres preflight (`flywheel_orchestrator::pg_preflight`).
//!
//! The pooler-mode decision and the report formatter are unit-tested without
//! a database. The integration cases run against the throwaway container from
//! flywheel-core's test support and skip cleanly when Docker is unavailable.
//! The fail-loud case (a configured-but-unreachable target) needs no server:
//! it uses a closed local port.

#[path = "../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

use flywheel_core::store::CURRENT_SCHEMA_VERSION;
use flywheel_orchestrator::pg_preflight::{
    CheckStatus, PreflightCheck, PreflightOptions, PreflightOutcome, SessionProbe, dsn_port,
    format_report, pooler_mode_verdict, provisioning_sql, run_postgres_preflight,
};
use pg_container::Postgres;

fn status(outcome: &PreflightOutcome, name: &str) -> CheckStatus {
    outcome
        .status_of(name)
        .unwrap_or_else(|| panic!("no check named {name:?} in {:?}", outcome.checks))
}

fn observed(value: &str) -> SessionProbe {
    SessionProbe::Observed(Some(value.to_owned()))
}

// --- pooler-mode decision (no database) -------------------------------------------------------

#[test]
fn pooler_mode_ok_when_session_state_survives_on_session_port() {
    let check = pooler_mode_verdict("postgresql://h:5432/db", &observed("session-ok"));
    assert_eq!(check.status, CheckStatus::Ok);
    assert_eq!(
        check.detail,
        "session state persisted and the port is not a known transaction-pooler port"
    );
    assert_eq!(check.remedy, None);
}

#[test]
fn pooler_mode_blocks_when_session_state_lost() {
    // A failed probe is the one trustworthy behavioral signal -- it blocks
    // regardless of port.
    for probe in [observed(""), SessionProbe::Observed(None)] {
        let check = pooler_mode_verdict("postgresql://h:5432/db", &probe);
        assert_eq!(check.status, CheckStatus::Block);
        assert!(
            check.detail.contains("transaction-mode pooler"),
            "{}",
            check.detail
        );
        assert!(check.remedy.as_deref().is_some_and(|r| r.contains("5432")));
    }
}

#[test]
fn pooler_mode_blocks_on_supabase_transaction_port_even_if_probe_passes() {
    // A lone idle client can make a transaction pooler pass the GUC probe;
    // the unambiguous 6543 port still blocks.
    let check = pooler_mode_verdict("postgresql://h:6543/db", &observed("session-ok"));
    assert_eq!(check.status, CheckStatus::Block);
    assert_eq!(
        check.detail,
        "port 6543 is the Supabase Supavisor transaction pooler; flywheel needs a session \
         connection (prepared statements and LISTEN/NOTIFY will fail)"
    );
    assert!(check.remedy.as_deref().is_some_and(|r| r.contains("5432")));
}

#[test]
fn pooler_mode_warns_when_the_probe_itself_fails() {
    let check = pooler_mode_verdict(
        "postgresql://h:5432/db",
        &SessionProbe::Failed("permission denied".into()),
    );
    assert_eq!(check.status, CheckStatus::Warn);
    assert_eq!(
        check.detail,
        "could not verify session mode: permission denied; ensure this is a session or direct \
         connection, not a transaction-mode pooler"
    );
    assert_eq!(check.remedy, None);
}

#[test]
fn dsn_port_parses_and_tolerates_missing() {
    assert_eq!(dsn_port("postgresql://h:6543/db"), Some(6543));
    assert_eq!(dsn_port("host=h port=6543 dbname=db"), Some(6543));
    // A URL without a port resolves to the libpq default the connection
    // will actually use; it is never a transaction-pooler hint.
    assert_eq!(dsn_port("postgresql://h/db"), Some(5432));
    assert_eq!(dsn_port("host=h dbname=db"), None);
    assert_eq!(dsn_port("not a dsn at all"), None);
}

// --- report formatter -------------------------------------------------------------------------

#[test]
fn format_report_renders_status_and_remedy() {
    let report = format_report(&[
        PreflightCheck::new("connection", CheckStatus::Ok, "reached the server"),
        PreflightCheck::new("privileges", CheckStatus::Block, "role lacks CREATE")
            .with_remedy("GRANT ..."),
    ]);
    assert!(report.starts_with("Postgres preflight:\n"));
    assert!(report.contains("[OK  ] connection: reached the server"));
    assert!(report.contains("[BLOCK] privileges: role lacks CREATE"));
    assert!(report.contains("fix: GRANT ..."));
}

#[test]
fn format_report_indents_multiline_remedy() {
    let report = format_report(&[
        PreflightCheck::new("privileges", CheckStatus::Block, "x").with_remedy("line1\nline2")
    ]);
    assert!(report.contains("fix: line1"));
    // Continuation lines align under the first, not re-prefixed with "fix:".
    assert!(report.contains("\n              line2"));
    assert_eq!(report.matches("fix:").count(), 1);
    let warn = format_report(&[PreflightCheck::new("x", CheckStatus::Warn, "w")]);
    assert!(warn.contains("[WARN] x: w"));
}

#[test]
fn provisioning_sql_is_idempotent_and_grants_the_role() {
    let sql = provisioning_sql("flywheel", "flywheel_app");
    assert!(sql.contains("CREATE SCHEMA IF NOT EXISTS \"flywheel\";"));
    assert!(sql.contains("GRANT ALL ON SCHEMA \"flywheel\" TO \"flywheel_app\";"));
}

// --- fail loud (no server) --------------------------------------------------------------------

#[test]
fn unreachable_server_blocks_on_connection_without_leaking_the_dsn() {
    // A closed local port: connection is refused fast, no live server needed.
    let dsn = "postgresql://flywheel:hunter2@127.0.0.1:1/flywheel?connect_timeout=1";
    let outcome = run_postgres_preflight(dsn, "flywheel", PreflightOptions::default());
    assert!(outcome.blocked());
    assert_eq!(status(&outcome, "postgres extra"), CheckStatus::Ok);
    let connection = outcome.check("connection").unwrap();
    assert_eq!(connection.status, CheckStatus::Block);
    assert!(
        connection.detail.starts_with("could not connect: "),
        "{}",
        connection.detail
    );
    assert!(
        !connection.detail.contains("hunter2"),
        "{}",
        connection.detail
    );
    assert!(!connection.detail.contains(dsn), "{}", connection.detail);
    assert!(
        connection
            .remedy
            .as_deref()
            .is_some_and(|r| r.contains("6543"))
    );
    // Short-circuited: nothing after the connection check ran.
    assert_eq!(outcome.checks.len(), 2);
    assert!(!outcome.provisioned);
    // The override does not provision an unreachable target either.
    let forced = run_postgres_preflight(
        dsn,
        "flywheel",
        PreflightOptions {
            provision: true,
            allow_unverified: true,
        },
    );
    assert!(!forced.provisioned);
    assert!(forced.check("provision").is_none());
}

#[test]
fn malformed_dsn_blocks_on_connection() {
    let outcome =
        run_postgres_preflight("not a dsn at all", "flywheel", PreflightOptions::default());
    assert_eq!(status(&outcome, "connection"), CheckStatus::Block);
    assert!(
        outcome
            .check("connection")
            .unwrap()
            .detail
            .contains("could not connect: ")
    );
}

// --- integration against the real container ---------------------------------------------------

#[test]
fn preflight_fresh_schema_all_ok() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_preflight");
    let outcome = run_postgres_preflight(&pg.dsn(), &schema, PreflightOptions::default());
    assert!(!outcome.blocked(), "{}", format_report(&outcome.checks));
    assert_eq!(status(&outcome, "postgres extra"), CheckStatus::Ok);
    assert_eq!(status(&outcome, "connection"), CheckStatus::Ok);
    assert_eq!(status(&outcome, "pooler mode"), CheckStatus::Ok);
    assert_eq!(status(&outcome, "privileges"), CheckStatus::Ok);
    // Not yet initialized -> the version check is informational, not a block.
    assert_eq!(status(&outcome, "schema version"), CheckStatus::Ok);
    assert_eq!(
        outcome.check("privileges").unwrap().detail,
        format!("role 'postgres' can CREATE SCHEMA '{schema}' in 'postgres'")
    );
    assert_eq!(
        outcome.check("schema version").unwrap().detail,
        format!(
            "schema '{schema}' is not yet initialized; it will be created at version \
             {CURRENT_SCHEMA_VERSION} on first use"
        )
    );
    assert!(!outcome.provisioned);
}

#[test]
fn preflight_provision_creates_schema_at_current_version() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_provision");
    let outcome = run_postgres_preflight(
        &pg.dsn(),
        &schema,
        PreflightOptions {
            provision: true,
            allow_unverified: false,
        },
    );
    assert!(outcome.provisioned, "{}", format_report(&outcome.checks));
    assert_eq!(status(&outcome, "provision"), CheckStatus::Ok);
    assert_eq!(
        outcome.check("provision").unwrap().detail,
        format!("schema '{schema}' provisioned at version {CURRENT_SCHEMA_VERSION}")
    );

    // A second preflight now sees an initialized schema at the current
    // version, owned by a role that can create objects in it.
    let again = run_postgres_preflight(&pg.dsn(), &schema, PreflightOptions::default());
    let version_check = again.check("schema version").unwrap();
    assert_eq!(version_check.status, CheckStatus::Ok);
    assert_eq!(
        version_check.detail,
        format!("schema '{schema}' is at version {CURRENT_SCHEMA_VERSION} (current)")
    );
    assert_eq!(
        again.check("privileges").unwrap().detail,
        format!("role 'postgres' can create objects in schema '{schema}'")
    );
}

#[test]
fn preflight_incompatible_schema_version_blocks() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_incompat");
    // Stand up just enough of a foreign schema to look initialized at a
    // version the store would refuse.
    pg.admin()
        .batch_execute(&format!(
            "CREATE SCHEMA \"{schema}\";
             CREATE TABLE \"{schema}\".schema_version (id INT PRIMARY KEY, version INT NOT NULL);
             INSERT INTO \"{schema}\".schema_version (id, version) VALUES (1, 99);"
        ))
        .unwrap();
    let outcome = run_postgres_preflight(&pg.dsn(), &schema, PreflightOptions::default());
    assert!(outcome.blocked());
    let version_check = outcome.check("schema version").unwrap();
    assert_eq!(version_check.status, CheckStatus::Block);
    assert_eq!(
        version_check.detail,
        format!(
            "schema '{schema}' is at version 99, which the store (version \
             {CURRENT_SCHEMA_VERSION}) will refuse to open"
        )
    );
    let remedy = version_check.remedy.as_deref().unwrap();
    assert!(remedy.contains("DROP SCHEMA"), "{remedy}");
    assert!(remedy.contains(&schema), "{remedy}");
    // Blocked without the override: provisioning is withheld.
    let withheld = run_postgres_preflight(
        &pg.dsn(),
        &schema,
        PreflightOptions {
            provision: true,
            allow_unverified: false,
        },
    );
    assert!(!withheld.provisioned);
    assert!(withheld.check("provision").is_none());
    // With the override the store is still refused, so provision blocks too.
    let forced = run_postgres_preflight(
        &pg.dsn(),
        &schema,
        PreflightOptions {
            provision: true,
            allow_unverified: true,
        },
    );
    assert!(!forced.provisioned);
    let prov = forced.check("provision").unwrap();
    assert_eq!(prov.status, CheckStatus::Block);
    assert!(
        prov.detail.contains("bootstrap failed: "),
        "{}",
        prov.detail
    );
    assert!(
        prov.detail.contains("store must be re-created"),
        "{}",
        prov.detail
    );
}

#[test]
fn preflight_migratable_schema_version_warns() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_migratable");
    pg.admin()
        .batch_execute(&format!(
            "CREATE SCHEMA \"{schema}\";
             CREATE TABLE \"{schema}\".schema_version (id INT PRIMARY KEY, version INT NOT NULL);
             INSERT INTO \"{schema}\".schema_version (id, version) VALUES (1, 11);"
        ))
        .unwrap();
    let outcome = run_postgres_preflight(&pg.dsn(), &schema, PreflightOptions::default());
    assert!(!outcome.blocked());
    let version_check = outcome.check("schema version").unwrap();
    assert_eq!(version_check.status, CheckStatus::Warn);
    assert_eq!(
        version_check.detail,
        format!(
            "schema '{schema}' is at version 11; it will be migrated in place to \
             {CURRENT_SCHEMA_VERSION} on first use"
        )
    );
}

#[test]
fn preflight_privileges_block_for_unprivileged_role() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let role = format!("fw_preflight_lowpriv_{}", std::process::id());
    let schema = pg.fresh_schema("fw_lowpriv");
    let mut admin = pg.admin();
    // A fresh role has CONNECT (via PUBLIC) but no CREATE on the database, so
    // it cannot create the absent target schema.
    admin
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS \"{role}\"; CREATE ROLE \"{role}\" LOGIN PASSWORD 'probe-pw'"
        ))
        .unwrap();
    let lowpriv_dsn = pg.dsn_with_credentials(&role, "probe-pw");
    let outcome = run_postgres_preflight(&lowpriv_dsn, &schema, PreflightOptions::default());
    admin
        .batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
        .unwrap();

    assert!(outcome.blocked(), "{}", format_report(&outcome.checks));
    assert_eq!(status(&outcome, "connection"), CheckStatus::Ok);
    let priv_check = outcome.check("privileges").unwrap();
    assert_eq!(priv_check.status, CheckStatus::Block);
    assert_eq!(
        priv_check.detail,
        format!(
            "role '{role}' cannot CREATE SCHEMA in database 'postgres' and schema '{schema}' \
             does not exist"
        )
    );
    // The block hands back the exact provisioning SQL to copy-paste.
    let remedy = priv_check.remedy.as_deref().unwrap();
    assert!(remedy.contains("CREATE SCHEMA IF NOT EXISTS"), "{remedy}");
    assert!(remedy.contains("GRANT ALL ON SCHEMA"), "{remedy}");
    assert!(remedy.contains(&role), "{remedy}");
    // The report renders the multi-line remedy under one "fix:" label.
    let report = format_report(&outcome.checks);
    assert_eq!(report.matches("fix:").count(), 1);
    assert!(report.contains(&format!(
        "              GRANT ALL ON SCHEMA \"{schema}\" TO \"{role}\";"
    )));
}

#[test]
fn preflight_privileges_block_on_existing_schema_without_grants() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let role = format!("fw_preflight_nogrant_{}", std::process::id());
    let schema = pg.fresh_schema("fw_nogrant");
    let mut admin = pg.admin();
    admin
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS \"{role}\"; CREATE ROLE \"{role}\" LOGIN PASSWORD 'probe-pw';
             CREATE SCHEMA \"{schema}\";"
        ))
        .unwrap();
    let lowpriv_dsn = pg.dsn_with_credentials(&role, "probe-pw");
    let outcome = run_postgres_preflight(&lowpriv_dsn, &schema, PreflightOptions::default());
    admin
        .batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
        .unwrap();
    let priv_check = outcome.check("privileges").unwrap();
    assert_eq!(priv_check.status, CheckStatus::Block);
    assert_eq!(
        priv_check.detail,
        format!("role '{role}' lacks CREATE/USAGE on existing schema '{schema}'")
    );
    assert_eq!(
        priv_check.remedy.as_deref().unwrap(),
        format!(
            "run as a privileged role:\n{}",
            provisioning_sql(&schema, &role)
        )
    );
}
