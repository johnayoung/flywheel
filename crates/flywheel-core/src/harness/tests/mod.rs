//! Harness contract tests: a scripted `InvokeFn` returning canned
//! `IterationResult`s stands in for the agent (the injectable invoker seam),
//! the in-memory store persists state, and a list sink captures telemetry.

mod graders;
mod guards;
mod mirroring;
mod ops;
mod recovery;
mod run;
mod support;
mod teardown;
mod validate;
