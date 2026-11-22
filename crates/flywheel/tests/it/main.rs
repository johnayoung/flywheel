//! The flywheel integration tests, one binary: each module is a former
//! `tests/<name>.rs` file. Shared helpers are wired once here.

#[path = "../../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;
#[path = "../../../flywheel-hub/tests/support/script.rs"]
mod script;
#[path = "../support/mod.rs"]
mod support;

mod account_pauses_in_loop;
mod audit_record;
mod closed_stdout;
mod config_trust;
mod eval_cli;
mod eval_trial;
mod exec_cli;
mod gate_decision_cli;
mod home_project;
mod hub_attach;
mod hub_audit;
mod hub_autopilot;
mod hub_fanout;
mod hub_flywheel;
mod hub_handoff;
mod hub_lifecycle;
mod hub_login_rolling;
mod hub_promote;
mod hub_queue;
mod hub_record_handoff;
mod hub_slowloris;
mod hub_status;
mod nested_locks;
mod pauses_verb;
mod reland_cli;
mod reland_in_loop;
mod self_update_drain;
mod self_update_hub;
mod self_update_install;
mod self_update_stores;
mod stop_reason_in_loop;
mod store_partition;
mod worker_account;
mod worker_scope;
mod worker_shutdown;
