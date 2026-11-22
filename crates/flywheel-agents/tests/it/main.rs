//! The flywheel-agents integration tests, one binary: each module is a former
//! `tests/<name>.rs` file. Shared helpers are wired once here.

#[path = "../common/mod.rs"]
mod common;

mod claude_fold;
mod claude_normalizer;
mod claude_normalizer_edges;
mod claude_plan;
mod claude_plan_edges;
mod codex_normalizer;
mod contract_claude;
mod contract_claude_control;
mod contract_codex;
mod hosts;
mod probes;
mod process_groups;
