//! flywheel orchestrator: drives **many** tasks on top of `flywheel-core`.
//!
//! Selection over a prerequisite DAG, work sources, claims and leases,
//! multi-worker coordination, phases, the autopilot intake daemon. Owns its
//! own store tables (`task_claims`, ledgers, snapshots). Cross-task concepts
//! live here and never leak into core.
//!
//! Authoritative specs: `docs/orchestration.md`, `docs/work-sources.md`,
//! `docs/strategy.md`, `docs/held-out-gate.md`, `docs/configuration.md`,
//! `docs/autopilot.md`.

pub mod autopilot;
pub mod autopilot_activity;
pub mod autopilot_run;
pub mod autopilot_supervised_run;
pub mod claims;
pub mod github;
pub mod github_ci;
pub mod github_review;
pub mod held_out_gate;
pub mod history;
pub mod init_agent;
pub mod orchestrate;
pub mod pg_preflight;
pub mod policy;
pub mod rollup;
pub mod skills;
pub mod source_registry;
pub mod sources;
pub mod store_factory;
pub mod store_registry;
pub mod strategy;
pub mod supervision_policy;
pub mod surface_lint;
pub mod triage;
pub mod triage_receipt;
pub mod triage_run;
pub mod work_graph;
pub mod workflow;
