//! Decomposition of the resolved [`WorkPolicy`] into the plain primitives
//! the runner consumes (specs 00037/00039/00066 and the checkpoint-nudge
//! wiring).
//!
//! Keeps the policy-type boundary intact: the orchestrator owns the policy
//! type, only primitives cross the [`RunTask`](super::run_task::RunTask)
//! seam. A `None` policy (library callers) decomposes the `fast` defaults so
//! the construction is identical to the historical hardcoded one.

use super::run_task::AgentPrimitives;
use crate::policy::{
    DEFAULT_CHECKPOINT_NUDGE_SECONDS, SandboxPolicy, WorkPolicy, resolve_agent_env,
    resolve_grader_env,
};
use flywheel_core::deadline_config::DeadlineConfig;
use flywheel_core::harness::HarnessConfig;
use std::collections::BTreeMap;

/// Decompose the resolved `[sandbox]` policy into agent-side primitives.
///
/// `[sandbox.env]` resolves as declared `pass` names forwarded from
/// `environ` (present-only -- an absent name is dropped, never
/// blank-substituted) merged with the inline `set` literals, which win on
/// key collision. Both empty (the fast default and a `None` policy)
/// resolves to an empty env.
pub fn sandbox_agent_primitives(
    policy: Option<&WorkPolicy>,
    environ: &BTreeMap<String, String>,
) -> AgentPrimitives {
    let default_sandbox;
    let sandbox: &SandboxPolicy = match policy {
        Some(policy) => &policy.sandbox,
        None => {
            default_sandbox = SandboxPolicy::default();
            &default_sandbox
        }
    };
    let cap = &sandbox.capabilities;
    AgentPrimitives {
        agent_id: policy.and_then(|p| p.agent_id.clone()),
        agent_transport: policy.and_then(|p| p.agent_transport.clone()),
        permission_mode: sandbox.permission_mode.clone(),
        skills: cap.skills.clone(),
        allowed_tools: cap.allowed_tools.clone(),
        denied_tools: cap.denied_tools.clone(),
        setting_sources: cap.setting_sources.clone(),
        mcp_servers: cap.mcp_servers.clone(),
        mcp_strict: cap.mcp_strict,
        exec_enabled: sandbox.exec.enabled,
        exec_auto_allow: sandbox.exec.auto_allow,
        agent_env: resolve_agent_env(&sandbox.env, Some(environ)),
        grader_env: resolve_grader_env(&sandbox.env, Some(environ)),
    }
}

/// The per-run cumulative ceilings from `[sandbox.limits]` (`0` =
/// unenforced). A `None` policy decomposes the `fast` default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SandboxLimitPrimitives {
    pub max_cost_usd: f64,
    pub max_tokens: u64,
    pub wall_clock_seconds: u64,
}

pub fn sandbox_limit_primitives(policy: Option<&WorkPolicy>) -> SandboxLimitPrimitives {
    let default_sandbox;
    let sandbox: &SandboxPolicy = match policy {
        Some(policy) => &policy.sandbox,
        None => {
            default_sandbox = SandboxPolicy::default();
            &default_sandbox
        }
    };
    SandboxLimitPrimitives {
        max_cost_usd: sandbox.limits.max_cost_usd,
        max_tokens: u64::try_from(sandbox.limits.max_tokens).unwrap_or(0),
        wall_clock_seconds: u64::try_from(sandbox.limits.wall_clock_seconds).unwrap_or(0),
    }
}

/// The repo-owned attempt budgets (spec 00066): the `[deadlines]` table and
/// `[sandbox.limits] rubric_judge_max_turns` (`None` keeps the harness
/// default).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AttemptBudgetPrimitives {
    pub deadlines: DeadlineConfig,
    pub rubric_judge_max_turns: Option<u32>,
}

pub fn attempt_budget_primitives(policy: Option<&WorkPolicy>) -> AttemptBudgetPrimitives {
    match policy {
        None => AttemptBudgetPrimitives::default(),
        Some(policy) => AttemptBudgetPrimitives {
            deadlines: policy.deadlines.clone(),
            rubric_judge_max_turns: policy
                .sandbox
                .limits
                .rubric_judge_max_turns
                .and_then(|v| u32::try_from(v).ok()),
        },
    }
}

/// The `[worker] checkpoint_nudge_seconds` threshold; the default-on
/// [`DEFAULT_CHECKPOINT_NUDGE_SECONDS`] for a `None` policy, and a policy
/// `0` (disabled) reaches the harness as-is.
pub fn checkpoint_nudge_seconds(policy: Option<&WorkPolicy>) -> f64 {
    policy.map_or(DEFAULT_CHECKPOINT_NUDGE_SECONDS, |p| {
        p.worker_checkpoint_nudge_seconds
    })
}

/// The policy-derived [`HarnessConfig`] every run starts from (the
/// per-run landability gate and progress probe are bound by the drive).
pub fn harness_config_from_policy(
    policy: Option<&WorkPolicy>,
    max_retries: u32,
    grader_env: Option<BTreeMap<String, String>>,
) -> HarnessConfig {
    let limits = sandbox_limit_primitives(policy);
    let budgets = attempt_budget_primitives(policy);
    let mut config = HarnessConfig {
        max_retries,
        max_cost_usd: limits.max_cost_usd,
        max_tokens: limits.max_tokens,
        wall_clock_seconds: limits.wall_clock_seconds,
        grader_env,
        deadlines: budgets.deadlines,
        checkpoint_nudge_seconds: checkpoint_nudge_seconds(policy),
        ..HarnessConfig::default()
    };
    if let Some(turns) = budgets.rubric_judge_max_turns {
        config.rubric_judge_max_turns = turns;
    }
    if let Some(agent_id) = policy.and_then(|p| p.agent_id.clone()) {
        config.agent_id = agent_id;
    }
    config
}
