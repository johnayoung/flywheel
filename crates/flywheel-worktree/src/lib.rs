//! flywheel worktree: built on the orchestrator -- the git-worktree submit
//! strategies (merge, phase, pr, risk tiers) and the reference worker daemon
//! spawned by `flywheel worker`.
//!
//! The worker never commits to the operator's branch: landing is the
//! `SubmitStrategy`'s job (FF-merge or PR); phase bookkeeping lives in the
//! `refs/flywheel/loop-base/<phase>` ref namespace. Nothing lands that was
//! not verified against the exact base it lands on.
//!
//! Authoritative specs: `docs/strategy.md`, `docs/sandbox.md`, `docs/cli.md`.

pub mod daemon;
pub mod disk_preflight;
pub mod phase;
pub mod pool;
pub mod pr;
pub mod retention;
pub mod submit_registry;
pub mod submitter;
pub mod tiering;
pub mod trailers;
