//! `flywheel worker --task ID [--task ID ...] [--with-prereqs]`: a worker
//! scoped to named tasks.
//!
//! The selection rules live in the orchestrator
//! ([`flywheel_orchestrator::orchestrate::TaskScope`]); this module is the
//! worker's wiring around them: the start-up refusal (exit 2, nothing
//! claimed), the per-cycle "settled?" test that stops the daemon loop
//! instead of idling, the forwarding of the scope to pool members, and the
//! one-line summary plus exit code (0 only when every named task verified).

use crate::daemon::WorkerArgs;
use chrono::{DateTime, DurationRound, SecondsFormat, TimeDelta, Utc};
use flywheel_core::store::HarnessStore;
use flywheel_orchestrator::orchestrate::{
    ScopeError, ScopePlan, ScopeRequest, ScopeVerdict, TaskScope, evaluate_scope, plan_scope,
};
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::sources::DirectoryWorkSource;
use flywheel_orchestrator::store_registry::environ_from_process;
use flywheel_orchestrator::workflow::{default_store_registries, open_stores_for};
use std::path::Path;
use std::sync::Arc;

/// The internal flag a pool member receives its supervisor's scope start
/// on (exact-match only, absent from `--help`).
pub const SCOPE_SINCE_FLAG: &str = "--scope-since";

/// A resolved `--task` scope over the worker's store and task tree.
pub struct WorkerScope {
    pub plan: ScopePlan,
    pub scope: TaskScope,
    source: DirectoryWorkSource,
    control: Arc<dyn HarnessStore>,
}

impl std::fmt::Debug for WorkerScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerScope")
            .field("plan", &self.plan)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// The scope start a fresh scope takes: now, floored to the millisecond so
/// it survives the RFC 3339 round trip to pool members unchanged.
fn scope_start() -> DateTime<Utc> {
    let now = Utc::now();
    now.duration_trunc(TimeDelta::milliseconds(1))
        .unwrap_or(now)
}

impl WorkerScope {
    /// `None` without `--task`; else the validated scope, or the operator
    /// message that refuses it (exit 2, nothing claimed).
    pub fn open(
        args: &WorkerArgs,
        policy: Option<&WorkPolicy>,
        tasks_dir: &Path,
        db_path: &Path,
        repo_root: &Path,
    ) -> Result<Option<Self>, String> {
        if args.tasks.is_empty() {
            return Ok(None);
        }
        let since = match &args.scope_since {
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|e| format!("{SCOPE_SINCE_FLAG} {raw:?}: {e}"))?,
            None => scope_start(),
        };
        let stores = open_stores_for(
            policy,
            db_path,
            &default_store_registries(),
            &environ_from_process(),
        )
        .map_err(|e| format!("store error: {e}"))?;
        let source = DirectoryWorkSource::new(tasks_dir);
        let request = ScopeRequest {
            tasks: args.tasks.clone(),
            with_prerequisites: args.with_prereqs,
            worker_capabilities: policy
                .map(|p| p.execution_capabilities.clone())
                .unwrap_or_default(),
        };
        let plan = plan_scope(&source, stores.core.as_ref(), &request, Some(repo_root)).map_err(
            |err| match err {
                ScopeError::UnmetPrerequisites(_) => format!(
                    "{err}. Run them first, or pass --with-prereqs to run them in this scope."
                ),
                other => other.to_string(),
            },
        )?;
        let scope = plan.scope(since);
        Ok(Some(Self {
            plan,
            scope,
            source,
            control: stores.core,
        }))
    }

    /// How many tasks the scope may run (named plus pulled-in).
    pub fn len(&self) -> usize {
        self.scope.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scope.is_empty()
    }

    /// The start-up line: what the worker is scoped to.
    pub fn describe(&self) -> String {
        let mut line = format!("scoped to {}", self.plan.named.join(", "));
        if !self.plan.added.is_empty() {
            line.push_str(&format!(
                " (with prerequisites {})",
                self.plan.added.join(", ")
            ));
        }
        line
    }

    /// Where every named task stands now.
    pub fn verdict(&self) -> Result<ScopeVerdict, ScopeError> {
        evaluate_scope(&self.source, self.control.as_ref(), &self.plan, &self.scope)
    }

    /// The flags that hand this scope to a pool member: the named tasks
    /// (the `=` spelling, so an id can never read as an option), the
    /// prerequisite switch, and this scope's start.
    pub fn member_flags(&self, args: &WorkerArgs) -> Vec<String> {
        let mut flags: Vec<String> = args.tasks.iter().map(|t| format!("--task={t}")).collect();
        if args.with_prereqs {
            flags.push("--with-prereqs".to_owned());
        }
        flags.push(format!(
            "{SCOPE_SINCE_FLAG}={}",
            self.scope
                .since()
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        ));
        flags
    }
}

/// The exit code a settled (or stopped) scope reports: `loop_code` when
/// the loop itself failed, else 0 only when every named task verified.
pub fn exit_code(loop_code: i32, verdict: &ScopeVerdict) -> i32 {
    if loop_code != 0 {
        loop_code
    } else if verdict.all_verified() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_orchestrator::orchestrate::ScopeOutcome;

    #[test]
    fn exit_codes_follow_the_verdict_unless_the_loop_failed() {
        let verified = ScopeVerdict {
            outcomes: vec![("a".into(), ScopeOutcome::Verified)],
        };
        let failed = ScopeVerdict {
            outcomes: vec![
                ("a".into(), ScopeOutcome::Verified),
                (
                    "b".into(),
                    ScopeOutcome::Failed {
                        reason: "grader unit failed".into(),
                    },
                ),
            ],
        };
        assert_eq!(exit_code(0, &verified), 0);
        assert_eq!(exit_code(0, &failed), 1);
        assert_eq!(exit_code(2, &verified), 2);
        assert_eq!(
            failed.summary(),
            "scope: 1/2 verified -- a verified; b failed (grader unit failed)"
        );
    }

    #[test]
    fn the_scope_start_round_trips_through_the_member_flag() {
        let at = scope_start();
        let text = at.to_rfc3339_opts(SecondsFormat::Millis, true);
        let back = DateTime::parse_from_rfc3339(&text)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(back, at);
    }
}
