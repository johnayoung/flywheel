//! The flywheel-orchestrator integration tests, one binary: each module is a former
//! `tests/<name>.rs` file. Shared helpers are wired once here.

#[path = "../../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

mod autopilot_emit_dedupe;
mod claims_contract;
mod claims_postgres;
mod pg_preflight;
