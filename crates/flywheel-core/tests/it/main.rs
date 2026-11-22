//! The flywheel-core integration tests, one binary: each module is a former
//! `tests/<name>.rs` file. Shared helpers are wired once here.

#[path = "../support/pg_container.rs"]
mod pg_container;

mod module_purity;
mod pg_tls;
mod store_contract;
mod store_postgres;
