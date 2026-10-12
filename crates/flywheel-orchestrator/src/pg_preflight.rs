//! Guided, failsafe Postgres bring-up for `flywheel init`.
//!
//! The store's runtime bootstrap is idempotent and correct, but on a managed
//! Postgres (Supabase, RDS, a PgBouncer/Supavisor pool) the *first worker
//! run* is a terrible place to discover that the DSN points at a
//! transaction-mode pooler, that the role cannot create the schema, or that
//! an existing schema is at a version the store will refuse. Those failures
//! are silent-until-runtime; this module surfaces them at init time.
//!
//! The engine runs an ordered list of checks over one connection and returns
//! structured [`PreflightCheck`] results. The init workflow prints them and,
//! when any check is [`CheckStatus::Block`] and the operator did not pass
//! `--allow-unverified`, aborts init without writing `flywheel.toml` -- so a
//! misconfigured target never yields a config that looks ready.
//!
//! Every check is read-only except the optional provision step
//! ([`PreflightOptions::provision`]), which constructs a [`PostgresStore`] so
//! its bootstrap DDL runs during init rather than at the first worker run.
//!
//! The Python module's first check verifies the optional `psycopg` extra is
//! importable; the Rust driver is compiled in, so that check always passes
//! and is kept only so reports line up across implementations.

use std::fmt;
use std::time::Duration;

use flywheel_core::store::CURRENT_SCHEMA_VERSION;
use flywheel_core::store_postgres::{PostgresStore, describe_error};
use postgres::{Client, Config, NoTls};

/// The schema versions the store migrates forward in place (v11 -> v12, an
/// additive nullable column). Mirrors the store bootstrap; kept in sync here
/// so the version check can say "will auto-migrate" instead of "will be
/// refused".
pub const IN_PLACE_MIGRATABLE_FROM: &[u32] = &[11];

/// Bounded wait for the preflight's one connection attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Ports that unambiguously mean a transaction-mode pooler. Supabase's
/// Supavisor splits session (5432) and transaction (6543) onto distinct
/// ports, so 6543 is a reliable block. A generic PgBouncer port (6432) is
/// NOT listed: it carries no mode information (it can be session or
/// transaction mode), so blocking it would false-positive on a valid session
/// pool. The behavioral probe is the only other signal -- and only in its
/// failing direction (see [`pooler_mode_verdict`]).
pub fn transaction_pooler_port_hint(port: u16) -> Option<&'static str> {
    match port {
        6543 => Some("the Supabase Supavisor transaction pooler"),
        _ => None,
    }
}

const SESSION_REMEDY: &str = "use a session connection: for Supabase the Session pooler (port \
     5432) or the direct connection; never the Transaction pooler (6543)";

const CONNECTION_REMEDY: &str = "verify the host, port, and credentials; for Supabase use the \
     Session pooler (port 5432) or the direct connection, never the Transaction pooler (6543)";

/// The value the session-state probe writes and expects to read back.
const PROBE_VALUE: &str = "session-ok";

/// One check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Block,
}

impl CheckStatus {
    /// The Python wire string (`"ok"` / `"warn"` / `"block"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Block => "block",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            CheckStatus::Ok => "OK  ",
            CheckStatus::Warn => "WARN",
            CheckStatus::Block => "BLOCK",
        }
    }
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One named readiness check and its verdict.
///
/// `remedy` is a copy-pasteable fix (a shell command or SQL) shown beneath a
/// warn/block line; `None` when nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    pub remedy: Option<String>,
}

impl PreflightCheck {
    pub fn new(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
            remedy: None,
        }
    }

    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

/// The ordered check results plus whether provisioning ran and succeeded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightOutcome {
    pub checks: Vec<PreflightCheck>,
    pub provisioned: bool,
}

impl PreflightOutcome {
    fn from_checks(checks: Vec<PreflightCheck>) -> Self {
        Self {
            checks,
            provisioned: false,
        }
    }

    /// Whether any check blocks.
    pub fn blocked(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Block)
    }

    /// The check named `name`, if it ran.
    pub fn check(&self, name: &str) -> Option<&PreflightCheck> {
        self.checks.iter().find(|c| c.name == name)
    }

    /// The status of the check named `name`, if it ran.
    pub fn status_of(&self, name: &str) -> Option<CheckStatus> {
        self.check(name).map(|c| c.status)
    }
}

/// Knobs for [`run_postgres_preflight`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreflightOptions {
    /// Run the store's bootstrap DDL now, after the checks.
    pub provision: bool,
    /// Provision even when a check blocked (the operator's override).
    pub allow_unverified: bool,
}

/// Strip the DSN and its password from a driver error string and collapse
/// whitespace.
pub fn sanitize(message: &str, dsn: &str, password: Option<&str>) -> String {
    let mut out = message.replace(dsn, "<dsn>");
    if let Some(password) = password.filter(|p| !p.is_empty()) {
        out = out.replace(password, "<password>");
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The port the DSN resolves to, when it parses: an explicit port, or the
/// libpq default (5432) that a URL-form DSN implies. `None` for a
/// `key=value` DSN that names no port, or an unparseable string.
pub fn dsn_port(dsn: &str) -> Option<u16> {
    dsn.parse::<Config>()
        .ok()
        .and_then(|config| config.get_ports().first().copied())
}

/// The password named by `dsn`, when it parses and carries one.
pub fn dsn_password(dsn: &str) -> Option<String> {
    dsn.parse::<Config>().ok().and_then(|config| {
        config
            .get_password()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    })
}

/// Run the ordered Postgres readiness checks against `dsn`.
///
/// The connection-dependent checks (pooler mode, privileges, schema version)
/// share one short-lived connection. A failure that prevents later checks
/// (unreachable server) short-circuits with a single blocking result.
/// Provisioning runs last and only when requested and not blocked (unless
/// `allow_unverified` overrode the block).
pub fn run_postgres_preflight(
    dsn: &str,
    schema: &str,
    options: PreflightOptions,
) -> PreflightOutcome {
    let mut checks = vec![PreflightCheck::new(
        "postgres extra",
        CheckStatus::Ok,
        "postgres driver is built in",
    )];
    let password = dsn_password(dsn);

    let mut client = match connect(dsn) {
        Ok(client) => client,
        Err(message) => {
            checks.push(
                PreflightCheck::new(
                    "connection",
                    CheckStatus::Block,
                    format!(
                        "could not connect: {}",
                        sanitize(&message, dsn, password.as_deref())
                    ),
                )
                .with_remedy(CONNECTION_REMEDY),
            );
            return PreflightOutcome::from_checks(checks);
        }
    };
    checks.push(PreflightCheck::new(
        "connection",
        CheckStatus::Ok,
        "reached the server",
    ));

    checks.push(check_pooler_mode(&mut client, dsn, password.as_deref()));
    checks.push(check_privileges(
        &mut client,
        schema,
        dsn,
        password.as_deref(),
    ));
    checks.push(check_schema_version(
        &mut client,
        schema,
        dsn,
        password.as_deref(),
    ));
    drop(client);

    let outcome = PreflightOutcome::from_checks(checks);
    if options.provision && (!outcome.blocked() || options.allow_unverified) {
        let prov = provision(dsn, schema, password.as_deref());
        let provisioned = prov.status == CheckStatus::Ok;
        let mut checks = outcome.checks;
        checks.push(prov);
        return PreflightOutcome {
            checks,
            provisioned,
        };
    }
    outcome
}

/// One autocommit connection with the preflight's bounded connect timeout,
/// or the rendered driver error.
fn connect(dsn: &str) -> Result<Client, String> {
    let mut config: Config = dsn.parse().map_err(|err| describe_error(&err))?;
    config.connect_timeout(CONNECT_TIMEOUT);
    config.connect(NoTls).map_err(|err| describe_error(&err))
}

/// What the session-state probe observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionProbe {
    /// `current_setting` read back this value in a later statement (`None`
    /// when the setting was NULL / absent).
    Observed(Option<String>),
    /// The probe itself failed; carries the sanitized driver message.
    Failed(String),
}

/// Flag connections that cannot hold session state.
///
/// flywheel needs a session connection: the driver issues server-side
/// prepared statements and the store's reactive layer uses `LISTEN/NOTIFY`
/// -- both break on a transaction-mode pooler that rebinds backends per
/// transaction. Detecting that from one client connection is only partly
/// possible, and this check is deliberately honest about the limit:
///
/// * The behavioral probe (set a session GUC, read it back in the next
///   autocommit statement) is trusted **only when it fails** -- a value that
///   does not survive proves the pool is transaction-scoped. A lone idle
///   client often reuses one backend, so a *passing* probe does NOT prove
///   session mode and is not treated as proof.
/// * Supabase's Supavisor puts the transaction pooler on a dedicated port
///   (6543), which is an unambiguous, reliable block.
///
/// Anything else is reported OK with a detail that says exactly what was
/// verified.
pub fn pooler_mode_verdict(dsn: &str, probe: &SessionProbe) -> PreflightCheck {
    let port = dsn_port(dsn);
    let port_hint = port.and_then(transaction_pooler_port_hint);

    let (state_lost, probe_error) = match probe {
        SessionProbe::Observed(value) => (value.as_deref() != Some(PROBE_VALUE), None),
        SessionProbe::Failed(message) => (false, Some(message.as_str())),
    };

    // Strongest signal: state demonstrably did not persist.
    if state_lost {
        return PreflightCheck::new(
            "pooler mode",
            CheckStatus::Block,
            "session state did NOT persist across statements -- a transaction-mode pooler; \
             prepared statements and LISTEN/NOTIFY will fail at runtime",
        )
        .with_remedy(SESSION_REMEDY);
    }

    // Reliable for the realistic dangerous case (Supabase 6543).
    if let (Some(port), Some(hint)) = (port, port_hint) {
        return PreflightCheck::new(
            "pooler mode",
            CheckStatus::Block,
            format!(
                "port {port} is {hint}; flywheel needs a session connection (prepared \
                 statements and LISTEN/NOTIFY will fail)"
            ),
        )
        .with_remedy(SESSION_REMEDY);
    }

    if let Some(error) = probe_error {
        return PreflightCheck::new(
            "pooler mode",
            CheckStatus::Warn,
            format!(
                "could not verify session mode: {error}; ensure this is a session or direct \
                 connection, not a transaction-mode pooler"
            ),
        );
    }

    PreflightCheck::new(
        "pooler mode",
        CheckStatus::Ok,
        "session state persisted and the port is not a known transaction-pooler port",
    )
}

/// Run the session-state probe over `client` (two autocommit statements) and
/// render the verdict.
fn check_pooler_mode(client: &mut Client, dsn: &str, password: Option<&str>) -> PreflightCheck {
    let probe = (|| -> Result<Option<String>, postgres::Error> {
        client.execute(
            "SELECT set_config('flywheel.preflight_probe', $1, false)",
            &[&PROBE_VALUE],
        )?;
        let row = client.query_one(
            "SELECT current_setting('flywheel.preflight_probe', true)",
            &[],
        )?;
        row.try_get(0)
    })();
    let probe = match probe {
        Ok(value) => SessionProbe::Observed(value),
        Err(err) => SessionProbe::Failed(sanitize(&describe_error(&err), dsn, password)),
    };
    pooler_mode_verdict(dsn, &probe)
}

/// The exact SQL to make `role` able to own objects in `schema`.
///
/// Run as a Postgres superuser / privileged role (e.g. `postgres` in
/// Supabase). `CREATE SCHEMA` is idempotent, and the grant targets the
/// least-privilege role from the operator's DSN. Identifiers are double-
/// quoted; the engine only ever passes validated schema names and a role read
/// back from the live connection, so neither can inject.
pub fn provisioning_sql(schema: &str, role: &str) -> String {
    format!(
        "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\nGRANT ALL ON SCHEMA \"{schema}\" TO \"{role}\";"
    )
}

/// Verify the connecting role can create/own the target schema.
///
/// If the schema exists, the role needs `CREATE`+`USAGE` on it; if it does
/// not, the role needs `CREATE` on the database to `CREATE SCHEMA`. The
/// remedy names the exact `GRANT`.
fn check_privileges(
    client: &mut Client,
    schema: &str,
    dsn: &str,
    password: Option<&str>,
) -> PreflightCheck {
    match privileges_verdict(client, schema) {
        Ok(check) => check,
        Err(err) => PreflightCheck::new(
            "privileges",
            CheckStatus::Block,
            format!(
                "could not verify privileges: {}",
                sanitize(&describe_error(&err), dsn, password)
            ),
        ),
    }
}

fn privileges_verdict(
    client: &mut Client,
    schema: &str,
) -> Result<PreflightCheck, postgres::Error> {
    let identity = client.query_one("SELECT current_user, current_database()", &[])?;
    let role: String = identity.try_get(0)?;
    let database: String = identity.try_get(1)?;

    // pg_namespace, not information_schema.schemata: the latter hides every
    // schema the role holds no privilege on, which would misreport an
    // existing-but-ungranted schema as absent.
    let schema_exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1::text)",
            &[&schema],
        )?
        .try_get(0)?;

    if schema_exists {
        let row = client.query_one(
            "SELECT has_schema_privilege($1::name, 'CREATE'), has_schema_privilege($1::name, 'USAGE')",
            &[&schema],
        )?;
        let can_create: bool = row.try_get(0)?;
        let can_use: bool = row.try_get(1)?;
        if can_create && can_use {
            return Ok(PreflightCheck::new(
                "privileges",
                CheckStatus::Ok,
                format!("role '{role}' can create objects in schema '{schema}'"),
            ));
        }
        return Ok(PreflightCheck::new(
            "privileges",
            CheckStatus::Block,
            format!("role '{role}' lacks CREATE/USAGE on existing schema '{schema}'"),
        )
        .with_remedy(format!(
            "run as a privileged role:\n{}",
            provisioning_sql(schema, &role)
        )));
    }

    let can_create_schema: bool = client
        .query_one(
            "SELECT has_database_privilege($1::name, 'CREATE')",
            &[&database],
        )?
        .try_get(0)?;
    if can_create_schema {
        return Ok(PreflightCheck::new(
            "privileges",
            CheckStatus::Ok,
            format!("role '{role}' can CREATE SCHEMA '{schema}' in '{database}'"),
        ));
    }
    Ok(PreflightCheck::new(
        "privileges",
        CheckStatus::Block,
        format!(
            "role '{role}' cannot CREATE SCHEMA in database '{database}' and schema '{schema}' \
             does not exist"
        ),
    )
    .with_remedy(format!(
        "run as a privileged role (e.g. Supabase 'postgres'):\n{}",
        provisioning_sql(schema, &role)
    )))
}

/// Preview whether the store would accept an existing schema.
///
/// A fresh target (no schema, or no `schema_version` row) is fine -- the
/// first worker (or `--provision`) bootstraps it. An existing schema at the
/// current version is fine; at a migratable version it warns; at any other
/// version it blocks, because the store refuses such a schema at open time
/// and the operator should see that before queuing work.
fn check_schema_version(
    client: &mut Client,
    schema: &str,
    dsn: &str,
    password: Option<&str>,
) -> PreflightCheck {
    match schema_version_verdict(client, schema) {
        Ok(check) => check,
        Err(err) => PreflightCheck::new(
            "schema version",
            CheckStatus::Block,
            format!(
                "could not read the schema version: {}",
                sanitize(&describe_error(&err), dsn, password)
            ),
        ),
    }
}

fn schema_version_verdict(
    client: &mut Client,
    schema: &str,
) -> Result<PreflightCheck, postgres::Error> {
    let initialized: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1::text AND table_name = 'schema_version')",
            &[&schema],
        )?
        .try_get(0)?;
    if !initialized {
        return Ok(PreflightCheck::new(
            "schema version",
            CheckStatus::Ok,
            format!(
                "schema '{schema}' is not yet initialized; it will be created at version \
                 {CURRENT_SCHEMA_VERSION} on first use"
            ),
        ));
    }

    // The schema name reached here only through the caller's validation
    // (an identifier-safe pattern), so quoting it is safe.
    let row = client.query_opt(
        &format!("SELECT version FROM \"{schema}\".schema_version WHERE id = 1"),
        &[],
    )?;
    let observed: Option<i32> = match row {
        Some(row) => Some(row.try_get(0)?),
        None => None,
    };
    let observed_text = observed.map_or_else(|| "None".to_owned(), |v| v.to_string());

    if observed == Some(CURRENT_SCHEMA_VERSION as i32) {
        return Ok(PreflightCheck::new(
            "schema version",
            CheckStatus::Ok,
            format!("schema '{schema}' is at version {observed_text} (current)"),
        ));
    }
    if observed.is_some_and(|v| IN_PLACE_MIGRATABLE_FROM.iter().any(|m| *m as i32 == v)) {
        return Ok(PreflightCheck::new(
            "schema version",
            CheckStatus::Warn,
            format!(
                "schema '{schema}' is at version {observed_text}; it will be migrated in place \
                 to {CURRENT_SCHEMA_VERSION} on first use"
            ),
        ));
    }
    Ok(PreflightCheck::new(
        "schema version",
        CheckStatus::Block,
        format!(
            "schema '{schema}' is at version {observed_text}, which the store (version \
             {CURRENT_SCHEMA_VERSION}) will refuse to open"
        ),
    )
    .with_remedy(format!(
        "DROP SCHEMA \"{schema}\" CASCADE;  -- destroys existing flywheel run history in this \
         schema, then re-init"
    )))
}

/// Run the store's bootstrap DDL now (create schema + tables).
///
/// Constructs a [`PostgresStore`], which runs the idempotent bootstrap, then
/// closes it. Any failure becomes a blocking result so init does not report
/// success on a half-provisioned target.
fn provision(dsn: &str, schema: &str, password: Option<&str>) -> PreflightCheck {
    let result = PostgresStore::connect(dsn, schema).and_then(PostgresStore::close);
    match result {
        Ok(()) => PreflightCheck::new(
            "provision",
            CheckStatus::Ok,
            format!("schema '{schema}' provisioned at version {CURRENT_SCHEMA_VERSION}"),
        ),
        Err(err) => PreflightCheck::new(
            "provision",
            CheckStatus::Block,
            format!(
                "bootstrap failed: {}",
                sanitize(&err.to_string(), dsn, password)
            ),
        ),
    }
}

/// Render the checks as an aligned, human-readable block.
pub fn format_report(checks: &[PreflightCheck]) -> String {
    let mut lines = vec!["Postgres preflight:".to_owned()];
    for check in checks {
        lines.push(format!(
            "  [{}] {}: {}",
            check.status.glyph(),
            check.name,
            check.detail
        ));
        if let Some(remedy) = &check.remedy {
            let mut remedy_lines = remedy.lines();
            lines.push(format!(
                "         fix: {}",
                remedy_lines.next().unwrap_or("")
            ));
            // Continuation lines (e.g. a multi-statement SQL block) align
            // under the first so the block is copy-pasteable.
            for extra in remedy_lines {
                lines.push(format!("              {extra}"));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_wire_strings_match_python() {
        assert_eq!(CheckStatus::Ok.as_str(), "ok");
        assert_eq!(CheckStatus::Warn.to_string(), "warn");
        assert_eq!(CheckStatus::Block.as_str(), "block");
    }

    #[test]
    fn sanitize_masks_dsn_and_password_and_collapses_whitespace() {
        let dsn = "postgresql://u:hunter2@h:5432/db";
        assert_eq!(
            sanitize(
                "failed  for postgresql://u:hunter2@h:5432/db\n with hunter2",
                dsn,
                Some("hunter2")
            ),
            "failed for <dsn> with <password>"
        );
        assert_eq!(sanitize("x  y", dsn, Some("")), "x y");
    }

    #[test]
    fn outcome_blocked_when_any_check_blocks() {
        let mut outcome =
            PreflightOutcome::from_checks(vec![PreflightCheck::new("a", CheckStatus::Warn, "w")]);
        assert!(!outcome.blocked());
        outcome
            .checks
            .push(PreflightCheck::new("b", CheckStatus::Block, "b"));
        assert!(outcome.blocked());
        assert_eq!(outcome.status_of("b"), Some(CheckStatus::Block));
        assert_eq!(outcome.status_of("zzz"), None);
    }

    #[test]
    fn dsn_password_is_extracted_only_when_present() {
        assert_eq!(
            dsn_password("postgresql://u:hunter2@h:5432/db").as_deref(),
            Some("hunter2")
        );
        assert_eq!(dsn_password("postgresql://u@h:5432/db"), None);
        assert_eq!(dsn_password("not a dsn at all"), None);
    }
}
