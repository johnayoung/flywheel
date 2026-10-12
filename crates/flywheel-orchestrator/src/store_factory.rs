//! Port of the Python `flywheel_orchestrator._store_factory` module.
//!
//! The single construction point for the core store and the claim store a
//! `[store]` config selects. Both are routed by name through
//! [`StoreRegistries`], so adding a backend is a registration, never a branch
//! here. The postgres DSN is never part of the config -- it is resolved from
//! the environment (`FLYWHEEL_PG_DSN`, falling back to `DATABASE_URL`) and
//! never echoed back in error messages.
//!
//! The core-store seam: `flywheel_core`'s concrete `SqliteStore` is not named
//! in this crate. The caller builds the registries with
//! [`StoreRegistries::with_sqlite`], passing a [`SqliteCoreOpener`] closure
//! (for example `Arc::new(|path| SqliteStore::open(path).map(|s| Arc::new(s)
//! as Arc<dyn HarnessStore>))`), and every factory function here takes those
//! registries.

use std::sync::Arc;

use flywheel_core::store::{HarnessStore, StoreError};

use crate::claims::{ClaimStore, ClaimStoreError};
use crate::store_registry::{BuildRequest, Environ, RegistryError, StoreRegistries};

pub use crate::store_registry::{
    DEFAULT_BACKEND, SqliteCoreOpener, StoreConfig, environ_from_process,
};

/// The flywheel-specific DSN variable; wins over the fallback silently.
pub const PG_DSN_ENV: &str = "FLYWHEEL_PG_DSN";
/// The 12-factor fallback DSN variable.
pub const PG_DSN_FALLBACK_ENV: &str = "DATABASE_URL";

/// The configured store backend cannot be constructed. Messages never
/// contain a DSN value.
#[derive(Debug, thiserror::Error)]
pub enum StoreConfigError {
    /// `[store] backend` names a backend no registry knows.
    #[error(transparent)]
    UnknownBackend(#[from] RegistryError),
    /// A DSN-backed backend was selected with neither DSN variable set.
    #[error(
        "store backend is {backend} but neither {PG_DSN_ENV} nor {PG_DSN_FALLBACK_ENV} is set; \
         export one with a {backend} connection string"
    )]
    MissingDsn { backend: String },
    /// A DSN-backed backend was selected but the build does not ship it.
    #[error("store backend is {backend} but the {extra} backend is not available in this build")]
    MissingExtra { backend: String, extra: String },
    /// The configured store could not be reached during the startup probe.
    /// Names the backend and the DSN sources tried, never the DSN value.
    #[error(
        "store backend is {backend} but the database is unreachable; checked {PG_DSN_ENV} then \
         {PG_DSN_FALLBACK_ENV} for the DSN"
    )]
    Unreachable {
        backend: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// The core store failed to open.
    #[error(transparent)]
    Core(#[from] StoreError),
    /// The claim store failed to open.
    #[error(transparent)]
    Claims(#[from] ClaimStoreError),
}

/// Resolve the postgres DSN from `environ`, or `None`.
///
/// `FLYWHEEL_PG_DSN` wins over `DATABASE_URL` silently. Empty or
/// whitespace-only values count as unset so an `export FLYWHEEL_PG_DSN=`
/// typo cannot select an empty DSN -- the fallback (or the fail-fast error)
/// applies instead.
pub fn resolve_postgres_dsn(environ: &Environ) -> Option<String> {
    [PG_DSN_ENV, PG_DSN_FALLBACK_ENV]
        .into_iter()
        .filter_map(|name| environ.get(name))
        .find(|value| !value.trim().is_empty())
        .cloned()
}

/// The core store and the claim store a config selects, opened together.
#[derive(Clone)]
pub struct StoreBundle {
    pub core: Arc<dyn HarnessStore>,
    pub claims: Arc<dyn ClaimStore>,
}

impl std::fmt::Debug for StoreBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreBundle").finish_non_exhaustive()
    }
}

/// Construct the core store `config` selects, routing `config.backend`
/// through the core registry (which owns the unknown-backend failure).
pub fn build_store(
    registries: &StoreRegistries,
    config: &StoreConfig,
    environ: &Environ,
) -> Result<Arc<dyn HarnessStore>, StoreConfigError> {
    let spec = registries.core.resolve(&config.backend)?;
    (spec.build)(&BuildRequest { config, environ })
}

/// Construct the claim store `config` selects, the claim-store peer of
/// [`build_store`]: a postgres config lands claims/leases in postgres with
/// the same fail-fast DSN preconditions the core store enforces.
pub fn build_claim_store(
    registries: &StoreRegistries,
    config: &StoreConfig,
    environ: &Environ,
) -> Result<Arc<dyn ClaimStore>, StoreConfigError> {
    let spec = registries.claims.resolve(&config.backend)?;
    (spec.build)(&BuildRequest { config, environ })
}

/// Open both stores for `config`: the orchestrate loop's entry point.
pub fn open_stores(
    registries: &StoreRegistries,
    config: &StoreConfig,
    environ: &Environ,
) -> Result<StoreBundle, StoreConfigError> {
    Ok(StoreBundle {
        core: build_store(registries, config, environ)?,
        claims: build_claim_store(registries, config, environ)?,
    })
}

/// Fail loud at startup when a configured non-sqlite store is unusable.
///
/// A no-op for the sqlite backend (no file is created). For any other
/// backend this probes the configured core store once, before any run or
/// claim activity, so a missing DSN or an unreachable server terminates the
/// process instead of silently falling back to sqlite. The probe store is
/// dropped immediately; it is not the store the caller runs with.
pub fn preflight_store(
    registries: &StoreRegistries,
    config: &StoreConfig,
    environ: &Environ,
) -> Result<(), StoreConfigError> {
    if config.backend == DEFAULT_BACKEND {
        return Ok(());
    }
    match build_store(registries, config, environ) {
        Ok(store) => {
            drop(store);
            Ok(())
        }
        // The registry's and the builder's own messages already name the
        // backend and both DSN sources.
        Err(err @ StoreConfigError::UnknownBackend(_))
        | Err(err @ StoreConfigError::MissingDsn { .. })
        | Err(err @ StoreConfigError::MissingExtra { .. })
        | Err(err @ StoreConfigError::Unreachable { .. }) => Err(err),
        Err(StoreConfigError::Core(source)) => Err(StoreConfigError::Unreachable {
            backend: config.backend.clone(),
            source: Box::new(source),
        }),
        Err(StoreConfigError::Claims(source)) => Err(StoreConfigError::Unreachable {
            backend: config.backend.clone(),
            source: Box::new(source),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_registry::{BackendSpec, CoreStoreBuilder};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Mutex;

    fn env(pairs: &[(&str, &str)]) -> Environ {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn resolve_dsn_primary_wins_silently() {
        let environ = env(&[
            (PG_DSN_ENV, "postgresql://primary"),
            (PG_DSN_FALLBACK_ENV, "postgresql://fallback"),
        ]);
        assert_eq!(
            resolve_postgres_dsn(&environ).as_deref(),
            Some("postgresql://primary")
        );
    }

    #[test]
    fn resolve_dsn_falls_back_to_database_url() {
        let environ = env(&[(PG_DSN_FALLBACK_ENV, "postgresql://fallback")]);
        assert_eq!(
            resolve_postgres_dsn(&environ).as_deref(),
            Some("postgresql://fallback")
        );
    }

    #[test]
    fn resolve_dsn_none_when_unset_and_whitespace_is_unset() {
        assert_eq!(resolve_postgres_dsn(&Environ::new()), None);
        let environ = env(&[(PG_DSN_ENV, "   "), (PG_DSN_FALLBACK_ENV, "")]);
        assert_eq!(resolve_postgres_dsn(&environ), None);
        let environ = env(&[(PG_DSN_ENV, ""), (PG_DSN_FALLBACK_ENV, "postgresql://x")]);
        assert_eq!(
            resolve_postgres_dsn(&environ).as_deref(),
            Some("postgresql://x")
        );
    }

    /// A registries value whose core sqlite opener records the path it was
    /// handed instead of opening a real core store (the core `SqliteStore`
    /// is not this module's concern).
    fn recording_registries(seen: Arc<Mutex<Vec<std::path::PathBuf>>>) -> StoreRegistries {
        let opener: SqliteCoreOpener = Arc::new(move |path: &Path| {
            seen.lock().unwrap().push(path.to_path_buf());
            Err(StoreError::backend(std::io::Error::other(
                "core store opener stub for the factory tests",
            )))
        });
        StoreRegistries::with_sqlite(opener)
    }

    #[test]
    fn sqlite_config_opens_claim_store_at_path_and_ignores_dsn_env() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("flywheel.db");
        let registries = recording_registries(Arc::new(Mutex::new(Vec::new())));
        let environ = env(&[(PG_DSN_ENV, "postgresql://ignored")]);
        let config = StoreConfig::sqlite(&db_path);
        let claims = build_claim_store(&registries, &config, &environ).unwrap();
        assert!(db_path.exists());
        let claim = claims
            .acquire_claim(
                "task-a",
                "worker-1",
                chrono::Utc::now(),
                30.0,
                &BTreeSet::new(),
            )
            .unwrap();
        assert!(claim.is_some());
    }

    #[test]
    fn build_store_routes_db_path_to_the_core_opener() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registries = recording_registries(seen.clone());
        let config = StoreConfig::sqlite("/tmp/some.db");
        let Err(err) = build_store(&registries, &config, &Environ::new()) else {
            panic!("the recording opener always fails");
        };
        assert!(matches!(err, StoreConfigError::Core(_)), "{err}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[std::path::PathBuf::from("/tmp/some.db")]
        );
    }

    #[test]
    fn unknown_backend_fails_through_the_registry() {
        let registries = recording_registries(Arc::new(Mutex::new(Vec::new())));
        let config = StoreConfig {
            backend: "mysql".to_owned(),
            db_path: "/tmp/x.db".into(),
            schema: None,
        };
        let Err(err) = build_claim_store(&registries, &config, &Environ::new()) else {
            panic!("mysql is not registered");
        };
        assert_eq!(
            err.to_string(),
            "unknown claim store \"mysql\"; registered: \"sqlite\""
        );
    }

    #[test]
    fn preflight_is_a_noop_for_sqlite_and_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("never.db");
        let registries = recording_registries(Arc::new(Mutex::new(Vec::new())));
        preflight_store(&registries, &StoreConfig::sqlite(&db_path), &Environ::new()).unwrap();
        assert!(!db_path.exists());
    }

    #[test]
    fn preflight_surfaces_missing_dsn_and_wraps_connection_failures() {
        let mut registries = recording_registries(Arc::new(Mutex::new(Vec::new())));
        let builder: CoreStoreBuilder =
            Arc::new(
                |request: &BuildRequest<'_>| match resolve_postgres_dsn(request.environ) {
                    None => Err(StoreConfigError::MissingDsn {
                        backend: "postgres".to_owned(),
                    }),
                    Some(dsn) => Err(StoreConfigError::Core(StoreError::backend(
                        std::io::Error::other(format!("connection refused for {dsn}")),
                    ))),
                },
            );
        registries
            .core
            .register(BackendSpec::new(
                "postgres",
                "durable Postgres backend",
                Some("postgres"),
                builder,
            ))
            .unwrap();
        let config = StoreConfig {
            backend: "postgres".to_owned(),
            db_path: "/tmp/unused.db".into(),
            schema: Some("flywheel".to_owned()),
        };

        let missing = preflight_store(&registries, &config, &Environ::new()).unwrap_err();
        let message = missing.to_string();
        assert!(message.contains(PG_DSN_ENV) && message.contains(PG_DSN_FALLBACK_ENV));
        assert!(message.starts_with("store backend is postgres"));

        let secret = "postgresql://user:hunter2@db.example/flywheel";
        let environ = env(&[(PG_DSN_FALLBACK_ENV, secret)]);
        let unreachable = preflight_store(&registries, &config, &environ).unwrap_err();
        assert!(matches!(unreachable, StoreConfigError::Unreachable { .. }));
        assert!(!unreachable.to_string().contains("hunter2"));
        assert!(unreachable.to_string().contains("unreachable"));
    }

    #[test]
    fn open_stores_builds_the_core_store_first_and_surfaces_its_failure() {
        // The core sqlite store is the caller's (passed in as the opener), so
        // this module proves the bundle's construction order: the core store
        // is built before the claim store, and its failure is surfaced as-is
        // without the claim store being opened (no file appears).
        let registries = recording_registries(Arc::new(Mutex::new(Vec::new())));
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bundle.db");
        let config = StoreConfig::sqlite(&db_path);
        let Err(err) = open_stores(&registries, &config, &Environ::new()) else {
            panic!("the recording opener always fails");
        };
        assert!(matches!(err, StoreConfigError::Core(_)), "{err}");
        assert!(!db_path.exists());
    }
}
