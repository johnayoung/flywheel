//! Port of the Python `flywheel_orchestrator._store_registry` module.
//!
//! The registries of core and claim store backends: the names an operator can
//! select in `[store] backend`, each bound to a builder. The two families
//! (core store, claim store) share one builder signature so the factory in
//! [`crate::store_factory`] dispatches by name with no per-backend branch;
//! each registry owns the unknown-name failure for its family.
//!
//! Only `sqlite` is registered here. A `postgres` backend is added later by
//! its own module calling [`Registry::register`] on both families with a
//! [`BackendSpec`] whose `extra` is `Some("postgres")`; nothing in this file
//! changes when that lands.
//!
//! The sqlite *core* store is not named here either: `flywheel_core`'s
//! `SqliteStore` is passed in by the caller as a [`SqliteCoreOpener`] when
//! the registries are assembled ([`StoreRegistries::with_sqlite`]), which
//! keeps this module decoupled from the core store's constructor.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flywheel_core::store::{HarnessStore, StoreError};

use crate::claims::{ClaimStore, SqliteClaimStore};
use crate::store_factory::StoreConfigError;

/// The `[store]` section of a policy as the factory consumes it.
///
/// `backend` is the registry name (`"sqlite"` by default); `db_path` is the
/// resolved SQLite file (flag > policy > default, resolved by the caller);
/// `schema` is the optional Postgres schema name a `postgres` builder passes
/// through. The DSN is never part of the config -- it is resolved from the
/// environment at build time and never echoed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    pub backend: String,
    pub db_path: PathBuf,
    pub schema: Option<String>,
}

impl StoreConfig {
    /// The default backend: sqlite at `db_path`.
    pub fn sqlite(db_path: impl Into<PathBuf>) -> Self {
        Self {
            backend: DEFAULT_BACKEND.to_owned(),
            db_path: db_path.into(),
            schema: None,
        }
    }
}

/// The backend selected when no `[store]` section is present.
pub const DEFAULT_BACKEND: &str = "sqlite";

/// A process-environment snapshot the builders read DSNs from. A plain map
/// so tests inject one without touching the real environment.
pub type Environ = BTreeMap<String, String>;

/// Snapshot the real process environment into an [`Environ`].
pub fn environ_from_process() -> Environ {
    std::env::vars().collect()
}

/// Everything a builder receives: the config and the environment. Both
/// families dispatch on this one shape.
#[derive(Debug, Clone, Copy)]
pub struct BuildRequest<'a> {
    pub config: &'a StoreConfig,
    pub environ: &'a Environ,
}

/// Opens the core SQLite store at a path. Supplied by the caller (the
/// product shell) so this crate never names `flywheel_core`'s concrete
/// `SqliteStore` type.
pub type SqliteCoreOpener =
    Arc<dyn Fn(&Path) -> Result<Arc<dyn HarnessStore>, StoreError> + Send + Sync>;

/// A core-store builder: `BuildRequest -> Arc<dyn HarnessStore>`.
pub type CoreStoreBuilder =
    Arc<dyn Fn(&BuildRequest<'_>) -> Result<Arc<dyn HarnessStore>, StoreConfigError> + Send + Sync>;

/// A claim-store builder: `BuildRequest -> Arc<dyn ClaimStore>`.
pub type ClaimStoreBuilder =
    Arc<dyn Fn(&BuildRequest<'_>) -> Result<Arc<dyn ClaimStore>, StoreConfigError> + Send + Sync>;

/// One registered backend: its name, a one-line summary for operator
/// listings, the optional extra it depends on (informational: `None` for
/// built-ins with no optional dependency), and its builder.
#[derive(Clone)]
pub struct BackendSpec<B> {
    pub name: String,
    pub summary: String,
    pub extra: Option<String>,
    pub build: B,
}

impl<B> BackendSpec<B> {
    pub fn new(
        name: impl Into<String>,
        summary: impl Into<String>,
        extra: Option<&str>,
        build: B,
    ) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            extra: extra.map(str::to_owned),
            build,
        }
    }
}

impl<B> fmt::Debug for BackendSpec<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendSpec")
            .field("name", &self.name)
            .field("summary", &self.summary)
            .field("extra", &self.extra)
            .finish_non_exhaustive()
    }
}

/// A registry failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A name was requested that no backend in this family is registered
    /// for. The message lists the registered names so an operator reading a
    /// config error sees the valid choices immediately.
    #[error("unknown {family} {name:?}; registered: {}", known_display(.known))]
    Unknown {
        family: String,
        name: String,
        known: Vec<String>,
    },
    /// A second registration under an already-registered name; built-ins win
    /// a collision and are never silently replaced.
    #[error("{family} {name:?} is already registered")]
    Duplicate { family: String, name: String },
}

fn known_display(known: &[String]) -> String {
    if known.is_empty() {
        "(none)".to_owned()
    } else {
        known
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A name -> [`BackendSpec`] table for one backend family.
#[derive(Clone)]
pub struct Registry<B> {
    family: String,
    specs: BTreeMap<String, BackendSpec<B>>,
}

impl<B> fmt::Debug for Registry<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("family", &self.family)
            .field("names", &self.names())
            .finish()
    }
}

impl<B> Registry<B> {
    /// An empty registry for `family` (the noun used in error messages,
    /// e.g. `"store"` or `"claim store"`).
    pub fn new(family: impl Into<String>) -> Self {
        Self {
            family: family.into(),
            specs: BTreeMap::new(),
        }
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// Register a backend; refuses a duplicate name.
    pub fn register(&mut self, spec: BackendSpec<B>) -> Result<(), RegistryError> {
        if self.specs.contains_key(&spec.name) {
            return Err(RegistryError::Duplicate {
                family: self.family.clone(),
                name: spec.name,
            });
        }
        self.specs.insert(spec.name.clone(), spec);
        Ok(())
    }

    /// Registered names in sorted order.
    pub fn names(&self) -> Vec<String> {
        self.specs.keys().cloned().collect()
    }

    /// Every registered spec in name order.
    pub fn specs(&self) -> impl Iterator<Item = &BackendSpec<B>> {
        self.specs.values()
    }

    /// The spec registered under `name`.
    pub fn resolve(&self, name: &str) -> Result<&BackendSpec<B>, RegistryError> {
        self.specs.get(name).ok_or_else(|| RegistryError::Unknown {
            family: self.family.clone(),
            name: name.to_owned(),
            known: self.names(),
        })
    }
}

/// The two families selected by the same `[store] backend` knob, so a
/// postgres deployment routes both onto postgres and no orchestrator
/// coordination state leaks to sqlite.
#[derive(Debug, Clone)]
pub struct StoreRegistries {
    pub core: Registry<CoreStoreBuilder>,
    pub claims: Registry<ClaimStoreBuilder>,
}

impl StoreRegistries {
    /// Two empty registries (`"store"` and `"claim store"`).
    pub fn empty() -> Self {
        Self {
            core: Registry::new("store"),
            claims: Registry::new("claim store"),
        }
    }

    /// The shipped registries: `sqlite` in both families. The core sqlite
    /// store is opened through `core_sqlite_opener` (the seam described in
    /// the module docs); the claim store is this crate's own
    /// [`SqliteClaimStore`].
    pub fn with_sqlite(core_sqlite_opener: SqliteCoreOpener) -> Self {
        let mut registries = Self::empty();
        let core_builder: CoreStoreBuilder = Arc::new(move |request: &BuildRequest<'_>| {
            (core_sqlite_opener)(&request.config.db_path).map_err(StoreConfigError::from)
        });
        let claim_builder: ClaimStoreBuilder = Arc::new(build_sqlite_claim_store);
        // Fresh registries cannot already hold the name.
        let _ = registries.core.register(BackendSpec::new(
            DEFAULT_BACKEND,
            "local SQLite file (default)",
            None,
            core_builder,
        ));
        let _ = registries.claims.register(BackendSpec::new(
            DEFAULT_BACKEND,
            "local SQLite file (default)",
            None,
            claim_builder,
        ));
        registries
    }
}

/// Build the sqlite claim store (the registry's `sqlite` target):
/// `SqliteClaimStore::open(db_path)`, the same file the core sqlite store
/// binds (each owns its own tables). The environment is unused but part of
/// the shared builder signature.
pub fn build_sqlite_claim_store(
    request: &BuildRequest<'_>,
) -> Result<Arc<dyn ClaimStore>, StoreConfigError> {
    let store = SqliteClaimStore::open(&request.config.db_path)?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_name_lists_registered_choices() {
        let mut registry: Registry<u8> = Registry::new("store");
        registry
            .register(BackendSpec::new("sqlite", "s", None, 1))
            .unwrap();
        registry
            .register(BackendSpec::new("postgres", "p", Some("postgres"), 2))
            .unwrap();
        let err = registry.resolve("mysql").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown store \"mysql\"; registered: \"postgres\", \"sqlite\""
        );
        assert_eq!(registry.names(), vec!["postgres", "sqlite"]);
    }

    #[test]
    fn empty_registry_says_none() {
        let registry: Registry<u8> = Registry::new("claim store");
        let err = registry.resolve("sqlite").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown claim store \"sqlite\"; registered: (none)"
        );
    }

    #[test]
    fn duplicate_registration_is_refused() {
        let mut registry: Registry<u8> = Registry::new("store");
        registry
            .register(BackendSpec::new("sqlite", "s", None, 1))
            .unwrap();
        let err = registry
            .register(BackendSpec::new("sqlite", "again", None, 2))
            .unwrap_err();
        assert_eq!(
            err,
            RegistryError::Duplicate {
                family: "store".to_owned(),
                name: "sqlite".to_owned()
            }
        );
        assert_eq!(registry.resolve("sqlite").unwrap().build, 1);
    }

    #[test]
    fn with_sqlite_registers_sqlite_in_both_families() {
        let opener: SqliteCoreOpener = Arc::new(|_path: &Path| {
            Err(StoreError::backend(std::io::Error::other(
                "not opened in this test",
            )))
        });
        let registries = StoreRegistries::with_sqlite(opener);
        assert_eq!(registries.core.names(), vec!["sqlite"]);
        assert_eq!(registries.claims.names(), vec!["sqlite"]);
        let spec = registries.claims.resolve("sqlite").unwrap();
        assert_eq!(spec.extra, None);
        assert_eq!(spec.summary, "local SQLite file (default)");
    }

    #[test]
    fn a_later_postgres_entry_registers_without_touching_this_module() {
        let opener: SqliteCoreOpener =
            Arc::new(|_path: &Path| Err(StoreError::backend(std::io::Error::other("unused"))));
        let mut registries = StoreRegistries::with_sqlite(opener);
        let builder: ClaimStoreBuilder = Arc::new(|_request: &BuildRequest<'_>| {
            Err(StoreConfigError::MissingDsn {
                backend: "postgres".to_owned(),
            })
        });
        registries
            .claims
            .register(BackendSpec::new(
                "postgres",
                "durable Postgres backend",
                Some("postgres"),
                builder,
            ))
            .unwrap();
        assert_eq!(registries.claims.names(), vec!["postgres", "sqlite"]);
        assert_eq!(
            registries
                .claims
                .resolve("postgres")
                .unwrap()
                .extra
                .as_deref(),
            Some("postgres")
        );
    }
}
