//! The `Strategy` seam: how a finished task's work leaves the sandbox.
//!
//! Core owns only the contract and the do-nothing default. Real landing
//! strategies (git worktree merge, pull request, container) live in
//! consumer crates. This module is pure: no subprocess, git, filesystem, or
//! network access -- [`NoOpStrategy`] performs no side effects at all.

use crate::task::Task;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Per-task state a strategy threads from `setup` through `submit` and
/// `cleanup`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyContext {
    pub task_id: String,
    /// The branch / ref the strategy is working on (empty when none).
    #[serde(rename = "ref")]
    pub reference: String,
    pub metadata: BTreeMap<String, String>,
}

impl StrategyContext {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            ..Default::default()
        }
    }
}

/// What `submit` produced: the ref the work landed on (or was proposed
/// from), whether anything was actually submitted, and strategy-specific
/// metadata (PR URLs, merge commits, ...).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyResult {
    #[serde(rename = "ref")]
    pub reference: String,
    pub submitted: bool,
    pub metadata: BTreeMap<String, String>,
}

/// A strategy step failed. `message` is operator-facing; `source` carries
/// the underlying cause when one exists.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StrategyError {
    pub message: String,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl StrategyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl From<std::io::Error> for StrategyError {
    fn from(err: std::io::Error) -> Self {
        Self::with_source(err.to_string(), err)
    }
}

/// The landing contract. Implementations may hold state behind interior
/// mutability; every method takes `&self` so a strategy can be shared.
pub trait Strategy: Send + Sync {
    /// Prepare whatever `task` needs to run in (a worktree, a branch, ...).
    fn setup(&self, task: &Task) -> Result<StrategyContext, StrategyError>;
    /// Land or propose the finished work described by `ctx`.
    fn submit(&self, ctx: &StrategyContext) -> Result<StrategyResult, StrategyError>;
    /// Release whatever `setup` acquired. Must be safe to call repeatedly.
    fn cleanup(&self, ctx: &StrategyContext) -> Result<(), StrategyError>;
}

/// Default [`Strategy`] that performs no git, filesystem, or network work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoOpStrategy;

impl Strategy for NoOpStrategy {
    fn setup(&self, task: &Task) -> Result<StrategyContext, StrategyError> {
        Ok(StrategyContext::new(task.id.clone()))
    }

    fn submit(&self, ctx: &StrategyContext) -> Result<StrategyResult, StrategyError> {
        Ok(StrategyResult {
            reference: ctx.reference.clone(),
            ..Default::default()
        })
    }

    fn cleanup(&self, _ctx: &StrategyContext) -> Result<(), StrategyError> {
        Ok(())
    }
}

/// The slug [`derive_slug`] yields for unusable input.
pub const SLUG_FALLBACK: &str = "task";

static CONVENTIONAL_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z]+(?:\([^)]+\))?!?:\s*").expect("static regex"));

/// Extract a branch-name slug from a (conventional) commit message.
///
/// Total: empty, whitespace-only, or otherwise unparsable input yields
/// [`SLUG_FALLBACK`] rather than failing.
pub fn derive_slug(commit: &str) -> String {
    let text = commit.trim();
    if text.is_empty() {
        return SLUG_FALLBACK.to_owned();
    }
    let first_line = text.split('\n').next().unwrap_or("").trim();
    if first_line.is_empty() {
        return SLUG_FALLBACK.to_owned();
    }
    let body = CONVENTIONAL_PREFIX.replace(first_line, "");
    let body = body.trim();
    if body.is_empty() {
        return SLUG_FALLBACK.to_owned();
    }
    let mut slug = String::with_capacity(body.len());
    let mut last_dash = false;
    for ch in body.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        SLUG_FALLBACK.to_owned()
    } else {
        slug.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CommandGrader, Grader};

    fn task() -> Task {
        Task::new(
            "trivial goal",
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id("t1")
    }

    #[test]
    fn noop_strategy_is_object_safe() {
        let strategy: Box<dyn Strategy> = Box::new(NoOpStrategy);
        let ctx = strategy.setup(&task()).unwrap();
        assert_eq!(ctx.task_id, "t1");
    }

    #[test]
    fn noop_setup_returns_typed_context_carrying_task_id() {
        let ctx = NoOpStrategy.setup(&task()).unwrap();
        assert_eq!(ctx.task_id, "t1");
        assert_eq!(ctx.reference, "");
        assert!(ctx.metadata.is_empty());
    }

    #[test]
    fn noop_submit_returns_deterministic_result_with_empty_ref() {
        let ctx = NoOpStrategy.setup(&task()).unwrap();
        let first = NoOpStrategy.submit(&ctx).unwrap();
        let second = NoOpStrategy.submit(&ctx).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.reference, "");
        assert!(!first.submitted);
        assert!(first.metadata.is_empty());
    }

    #[test]
    fn noop_cleanup_is_safe_and_repeatable() {
        let ctx = NoOpStrategy.setup(&task()).unwrap();
        NoOpStrategy.cleanup(&ctx).unwrap();
        NoOpStrategy.cleanup(&ctx).unwrap();
    }

    #[test]
    fn noop_strategy_performs_no_filesystem_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = NoOpStrategy.setup(&task()).unwrap();
        NoOpStrategy.submit(&ctx).unwrap();
        NoOpStrategy.cleanup(&ctx).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn context_and_result_serialize_ref_key() {
        let ctx = StrategyContext {
            reference: "refs/heads/x".into(),
            ..StrategyContext::new("t")
        };
        let json = serde_json::to_value(&ctx).unwrap();
        assert_eq!(json["ref"], "refs/heads/x");
        assert_eq!(json["task_id"], "t");
        let back: StrategyContext = serde_json::from_value(json).unwrap();
        assert_eq!(back, ctx);
        let res: StrategyResult =
            serde_json::from_str(r#"{"ref": "main", "submitted": true}"#).unwrap();
        assert_eq!(res.reference, "main");
        assert!(res.submitted);
    }

    #[test]
    fn derive_slug_strips_conventional_commit_prefix() {
        assert_eq!(derive_slug("feat: add login flow"), "add-login-flow");
        assert_eq!(
            derive_slug("fix(auth): handle expired tokens"),
            "handle-expired-tokens"
        );
        assert_eq!(
            derive_slug("refactor!: drop deprecated api"),
            "drop-deprecated-api"
        );
    }

    #[test]
    fn derive_slug_uses_only_first_line() {
        assert_eq!(
            derive_slug("feat: add login flow\n\nLonger body that should be ignored."),
            "add-login-flow"
        );
    }

    #[test]
    fn derive_slug_handles_non_conventional_messages() {
        assert_eq!(derive_slug("Just a plain message"), "just-a-plain-message");
    }

    #[test]
    fn derive_slug_total_on_empty_input() {
        assert_eq!(derive_slug(""), "task");
        assert_eq!(derive_slug("   "), "task");
        assert_eq!(derive_slug("\n\t  \n"), "task");
        assert_eq!(derive_slug("feat:   "), "task");
        assert_eq!(derive_slug("chore:"), "task");
        assert_eq!(derive_slug("feat: !!!"), "task");
    }

    #[test]
    fn derive_slug_collapses_repeated_separators_and_lowercases() {
        assert_eq!(
            derive_slug("feat: hello   world!! again"),
            "hello-world-again"
        );
        assert_eq!(
            derive_slug("Feat(Scope): Mixed CASE -- Title_With_Underscores"),
            "mixed-case-title-with-underscores"
        );
    }
}
