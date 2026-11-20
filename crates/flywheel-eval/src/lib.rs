//! flywheel eval: measure whether a change to the agent setup (CLAUDE.md,
//! skills, MCP config, prompts, model, flywheel version) makes agents
//! better or worse.
//!
//! * A **set** is a frozen, versioned list of past tasks, each with a base
//!   commit, a known-good reference commit, and a held-out oracle proven to
//!   fail at the base and pass at the reference.
//! * A **variant** is a declared agent setup, resolved and hashed per trial.
//! * A **trial** runs one task under one variant: `fw worker --task --base`
//!   in a throwaway clone holding no history past the base, with its own
//!   flywheel home; the oracle grades the final commit afterwards.
//! * The **report** compares variants paired by task and says `tie` when a
//!   difference is inside the measured noise.
//!
//! Built on the orchestrator (home, held-out registrations, stores); the
//! eval tables are this crate's own ([`store`]). Design:
//! `docs/eval.md`.

pub mod db;
pub mod git;
pub mod mine;
pub mod mirror;
pub mod oracle;
pub mod proc;
pub mod set;
pub mod store;
pub mod validate;

#[cfg(test)]
mod testutil;
