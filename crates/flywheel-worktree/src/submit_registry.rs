//! The registry of submit (landing) strategies.
//!
//! Names the landing strategies an operator can select in
//! `[submit] strategy` and binds each to a factory beside its
//! implementation: [`crate::submitter::build_merge_submitter`] and
//! [`crate::phase::build_phase_submitter`] ship here as the built-ins; the
//! `pr` strategy (`crate::pr`) and the tier router (`crate::tiering`)
//! register themselves against the same registry through
//! [`SubmitStrategyRegistry::register`] without this module naming them, so
//! adding a strategy never edits this file.
//!
//! Port of the Python `flywheel_worktree._submit_registry` module. The
//! Python resolved a `"module:attr"` target lazily to break the
//! `worker <-> pr` import cycle; Rust modules have no such cycle, so a spec
//! carries its factory closure directly.
//!
//! # The factory signature
//!
//! Every strategy is built through one [`SubmitFactory`]:
//!
//! ```text
//! Fn(&SubmitStrategyRegistry, &SubmitBuildRequest)
//!     -> Result<Arc<dyn SubmitStrategy>, SubmitRegistryError>
//! ```
//!
//! The registry is passed so a composite strategy (the tier router, which
//! delegates to `merge`/`phase`/`pr` by tier) can build its delegates through
//! the same table it was resolved from. [`SubmitBuildRequest`] is the shared
//! argument set every factory accepts, so the worker dispatches by name with
//! no per-strategy branch; a factory reads its own knobs (`[submit] remote`,
//! `pr_base`, `verify`, the recovery-agent bounds) off `request.policy`.

use crate::submitter::{ConflictResolver, LandingLedger};
use flywheel_orchestrator::held_out_gate::HeldOutGraderSource;
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::sources::LogSink;
use flywheel_orchestrator::strategy::SubmitStrategy;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// The family label used in registry error messages.
pub const SUBMIT_FAMILY: &str = "submit strategy";

/// The fast-forward merge strategy (the default).
pub const MERGE_STRATEGY: &str = "merge";
/// The pull-request strategy (registered by `crate::pr`).
pub const PR_STRATEGY: &str = "pr";
/// The per-phase integration-branch strategy.
pub const PHASE_STRATEGY: &str = "phase";

/// The shared construction inputs every submit-strategy factory accepts.
///
/// Mirrors the keyword signature the Python builders shared: the repo
/// layout, the resolved landing base, the merge lock, the log sink, the
/// `[submit] protected_paths` and `[sandbox] setup` knobs, the
/// `[sandbox.retention]` dispositions, and the optional seams (run ledger,
/// resolved grader env, held-out gate source, conflict resolver). `policy`
/// is the loaded `flywheel.toml`, `None` for a bare construction; a factory
/// that needs a required policy field reports [`SubmitRegistryError::Build`].
#[derive(Clone)]
pub struct SubmitBuildRequest {
    pub policy: Option<Arc<WorkPolicy>>,
    pub repo_root: PathBuf,
    pub tasks_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    pub phase_base: String,
    pub lock_path: PathBuf,
    pub log: LogSink,
    pub protected_paths: Vec<String>,
    pub setup_command: Option<String>,
    pub on_done: String,
    pub on_failure: String,
    pub store: Option<Arc<dyn LandingLedger>>,
    pub grader_env: Option<BTreeMap<String, String>>,
    pub held_out_source: Option<Arc<dyn HeldOutGraderSource>>,
    pub resolve_conflict: Option<ConflictResolver>,
}

impl SubmitBuildRequest {
    /// A request with the required layout fields; every optional seam is
    /// unset and the retention dispositions take their defaults
    /// (`on_done = "destroy"`, `on_failure = "park"`).
    pub fn new(
        repo_root: impl Into<PathBuf>,
        tasks_dir: impl Into<PathBuf>,
        worktrees_dir: impl Into<PathBuf>,
        phase_base: impl Into<String>,
        lock_path: impl Into<PathBuf>,
        log: LogSink,
    ) -> Self {
        Self {
            policy: None,
            repo_root: repo_root.into(),
            tasks_dir: tasks_dir.into(),
            worktrees_dir: worktrees_dir.into(),
            phase_base: phase_base.into(),
            lock_path: lock_path.into(),
            log,
            protected_paths: Vec::new(),
            setup_command: None,
            on_done: "destroy".to_owned(),
            on_failure: "park".to_owned(),
            store: None,
            grader_env: None,
            held_out_source: None,
            resolve_conflict: None,
        }
    }

    pub fn with_policy(mut self, policy: Arc<WorkPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_protected_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.protected_paths = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_setup_command(mut self, command: impl Into<String>) -> Self {
        self.setup_command = Some(command.into());
        self
    }

    pub fn with_retention(
        mut self,
        on_done: impl Into<String>,
        on_failure: impl Into<String>,
    ) -> Self {
        self.on_done = on_done.into();
        self.on_failure = on_failure.into();
        self
    }

    pub fn with_store(mut self, store: Arc<dyn LandingLedger>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_grader_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.grader_env = Some(env);
        self
    }

    pub fn with_held_out_source(mut self, source: Arc<dyn HeldOutGraderSource>) -> Self {
        self.held_out_source = Some(source);
        self
    }

    pub fn with_resolve_conflict(mut self, resolver: ConflictResolver) -> Self {
        self.resolve_conflict = Some(resolver);
        self
    }
}

impl fmt::Debug for SubmitBuildRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubmitBuildRequest")
            .field("policy", &self.policy.is_some())
            .field("repo_root", &self.repo_root)
            .field("tasks_dir", &self.tasks_dir)
            .field("worktrees_dir", &self.worktrees_dir)
            .field("phase_base", &self.phase_base)
            .field("lock_path", &self.lock_path)
            .field("protected_paths", &self.protected_paths)
            .field("setup_command", &self.setup_command)
            .field("on_done", &self.on_done)
            .field("on_failure", &self.on_failure)
            .field("store", &self.store.is_some())
            .field("grader_env", &self.grader_env.is_some())
            .field("held_out_source", &self.held_out_source.is_some())
            .field("resolve_conflict", &self.resolve_conflict.is_some())
            .finish()
    }
}

/// Builds one named strategy from the shared request. See the module docs
/// for the contract.
pub type SubmitFactory = Arc<
    dyn Fn(
            &SubmitStrategyRegistry,
            &SubmitBuildRequest,
        ) -> Result<Arc<dyn SubmitStrategy>, SubmitRegistryError>
        + Send
        + Sync,
>;

/// One named landing strategy: its name, a short human label for
/// diagnostics, and the factory that builds it.
#[derive(Clone)]
pub struct SubmitStrategySpec {
    pub name: String,
    pub summary: String,
    pub factory: SubmitFactory,
}

impl SubmitStrategySpec {
    pub fn new(
        name: impl Into<String>,
        summary: impl Into<String>,
        factory: SubmitFactory,
    ) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            factory,
        }
    }
}

impl fmt::Debug for SubmitStrategySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubmitStrategySpec")
            .field("name", &self.name)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

/// The two kinds of registry failure, branded in exactly one place: an
/// unknown name lists the names that *are* registered; a factory that
/// cannot build (a missing required policy field) names the strategy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitRegistryError {
    #[error("unknown {family} {name:?}; registered: {}", render_known(.known))]
    Unknown {
        family: String,
        name: String,
        known: Vec<String>,
    },
    #[error("{family} {name:?} is already registered")]
    Duplicate { family: String, name: String },
    #[error("cannot build {family} {name:?}: {detail}")]
    Build {
        family: String,
        name: String,
        detail: String,
    },
}

impl SubmitRegistryError {
    /// A build failure for `name`.
    pub fn build(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Build {
            family: SUBMIT_FAMILY.to_owned(),
            name: name.into(),
            detail: detail.into(),
        }
    }
}

fn render_known(known: &[String]) -> String {
    if known.is_empty() {
        return "(none)".to_owned();
    }
    known
        .iter()
        .map(|k| format!("{k:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The name -> factory table. Insertion-ordered so `names()` is stable for
/// diagnostics; a name registers at most once (built-ins win a collision
/// through [`register_if_absent`](Self::register_if_absent)).
#[derive(Debug, Clone, Default)]
pub struct SubmitStrategyRegistry {
    specs: Vec<SubmitStrategySpec>,
}

impl SubmitStrategyRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `spec`; a second registration of the same name is a
    /// [`SubmitRegistryError::Duplicate`].
    pub fn register(&mut self, spec: SubmitStrategySpec) -> Result<(), SubmitRegistryError> {
        if self.contains(&spec.name) {
            return Err(SubmitRegistryError::Duplicate {
                family: SUBMIT_FAMILY.to_owned(),
                name: spec.name,
            });
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Register `spec` unless its name is already taken; returns whether it
    /// was added. The discovery-style path: a built-in keeps its slot.
    pub fn register_if_absent(&mut self, spec: SubmitStrategySpec) -> bool {
        if self.contains(&spec.name) {
            return false;
        }
        self.specs.push(spec);
        true
    }

    pub fn contains(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// Registered names in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn specs(&self) -> &[SubmitStrategySpec] {
        &self.specs
    }

    /// The spec registered under `name`.
    pub fn resolve(&self, name: &str) -> Result<&SubmitStrategySpec, SubmitRegistryError> {
        self.specs
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| SubmitRegistryError::Unknown {
                family: SUBMIT_FAMILY.to_owned(),
                name: name.to_owned(),
                known: self.names().into_iter().map(str::to_owned).collect(),
            })
    }

    /// Resolve `name` and build it from `request`.
    pub fn build(
        &self,
        name: &str,
        request: &SubmitBuildRequest,
    ) -> Result<Arc<dyn SubmitStrategy>, SubmitRegistryError> {
        let spec = self.resolve(name)?;
        (spec.factory)(self, request)
    }
}

/// The registry with the strategies this crate's landing core ships:
/// `merge` and `phase`. The `pr` strategy and the tier router add
/// themselves through [`SubmitStrategyRegistry::register`] from their own
/// modules; the worker daemon assembles the full table.
pub fn builtin_registry() -> SubmitStrategyRegistry {
    let mut registry = SubmitStrategyRegistry::new();
    registry
        .register(SubmitStrategySpec::new(
            MERGE_STRATEGY,
            "fast-forward merge into the phase base (default)",
            Arc::new(|_registry, request| {
                Ok(Arc::new(crate::submitter::build_merge_submitter(request))
                    as Arc<dyn SubmitStrategy>)
            }),
        ))
        .expect("fresh registry has no merge entry");
    registry
        .register(SubmitStrategySpec::new(
            PHASE_STRATEGY,
            "land each task onto a per-phase integration branch",
            Arc::new(|_registry, request| {
                Ok(Arc::new(crate::phase::build_phase_submitter(request))
                    as Arc<dyn SubmitStrategy>)
            }),
        ))
        .expect("fresh registry has no phase entry");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_orchestrator::strategy::{
        PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    };
    use std::sync::Mutex;

    fn request(tmp: &std::path::Path) -> (SubmitBuildRequest, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogSink = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        let req = SubmitBuildRequest::new(
            tmp.join("repo"),
            tmp.join("tasks"),
            tmp.join("worktrees"),
            "main",
            tmp.join(".merge.lock"),
            log,
        )
        .with_protected_paths(["conftest.py", ".github/**"])
        .with_setup_command("echo setup");
        (req, lines)
    }

    struct Nothing;

    impl SubmitStrategy for Nothing {
        fn prepare_sandbox(&self, _: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
            Ok(SandboxHandle::new("/nowhere"))
        }
        fn submit(&self, _: &SubmitRequest) -> SubmitOutcome {
            SubmitOutcome::no_op("nothing")
        }
    }

    #[test]
    fn builtin_names_are_merge_then_phase() {
        let registry = builtin_registry();
        assert_eq!(registry.names(), vec!["merge", "phase"]);
        assert!(registry.contains(MERGE_STRATEGY));
        assert!(registry.contains(PHASE_STRATEGY));
        assert!(!registry.contains(PR_STRATEGY));
        assert_eq!(registry.specs().len(), 2);
    }

    #[test]
    fn resolve_unknown_lists_registered_choices() {
        let registry = builtin_registry();
        let err = registry.resolve("nope").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("\"merge\""), "{message}");
        assert!(message.contains("\"phase\""), "{message}");
        assert!(message.contains("nope"), "{message}");
        let empty = SubmitStrategyRegistry::new();
        assert!(
            empty
                .resolve("x")
                .unwrap_err()
                .to_string()
                .contains("(none)")
        );
    }

    #[test]
    fn merge_builds_from_a_policy_free_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (req, _lines) = request(tmp.path());
        let strategy = builtin_registry().build(MERGE_STRATEGY, &req).unwrap();
        // The merge backend is a GitWorktreeSubmitter whose landability
        // default is the git probe, not the always-landable trait default:
        // a request against a non-repo sandbox is reported not landable.
        let sub = crate::submitter::build_merge_submitter(&req);
        assert_eq!(sub.repo_root, req.repo_root);
        assert_eq!(sub.tasks_dir, req.tasks_dir);
        assert_eq!(sub.worktrees_dir, req.worktrees_dir);
        assert_eq!(sub.phase_base, "main");
        assert_eq!(sub.lock_path, req.lock_path);
        assert_eq!(sub.protected_paths, vec!["conftest.py", ".github/**"]);
        assert_eq!(sub.setup_command.as_deref(), Some("echo setup"));
        assert!(sub.verify_command.is_none());
        assert_eq!(sub.recovery_agent_max_turns, 0);
        drop(strategy);
    }

    #[test]
    fn phase_builds_and_logs_the_landing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (req, lines) = request(tmp.path());
        let _strategy = builtin_registry().build(PHASE_STRATEGY, &req).unwrap();
        let logged = lines.lock().unwrap().join("\n");
        assert!(logged.contains("landing strategy: phase"), "{logged}");
        assert!(logged.contains("true base main never advanced"), "{logged}");
    }

    #[test]
    fn merge_reads_policy_knobs() {
        let tmp = tempfile::tempdir().unwrap();
        let (req, _lines) = request(tmp.path());
        let policy = WorkPolicy {
            submit_verify: Some("scripts/check.sh".to_owned()),
            submit_recovery_agent_max_turns: 12,
            submit_recovery_agent_max_wall_seconds: 60.0,
            model: Some("claude-test".to_owned()),
            ..WorkPolicy::default()
        };
        let req = req.with_policy(Arc::new(policy));
        let sub = crate::submitter::build_merge_submitter(&req);
        assert_eq!(sub.verify_command.as_deref(), Some("scripts/check.sh"));
        assert_eq!(sub.recovery_agent_max_turns, 12);
        assert_eq!(sub.recovery_agent_max_wall_seconds, 60.0);
        assert_eq!(sub.recovery_agent_model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn third_party_registers_without_editing_the_module() {
        let mut registry = builtin_registry();
        registry
            .register(SubmitStrategySpec::new(
                PR_STRATEGY,
                "push the branch and open a pull request",
                Arc::new(|registry, _req| {
                    // A composite may resolve its delegates through the
                    // registry it was handed.
                    assert!(registry.contains(MERGE_STRATEGY));
                    Ok(Arc::new(Nothing) as Arc<dyn SubmitStrategy>)
                }),
            ))
            .unwrap();
        assert_eq!(registry.names(), vec!["merge", "phase", "pr"]);
        let tmp = tempfile::tempdir().unwrap();
        let (req, _lines) = request(tmp.path());
        let built = registry.build(PR_STRATEGY, &req).unwrap();
        assert!(matches!(
            built.submit(&SubmitRequest::new(
                flywheel_core::task::Task::new("g", Vec::new()),
                "run",
                flywheel_core::lifecycle::Status::Done,
                "/sb",
            )),
            SubmitOutcome::NoOp { .. }
        ));
        // A duplicate name is refused; register_if_absent keeps the built-in.
        let dup = registry.register(SubmitStrategySpec::new(
            MERGE_STRATEGY,
            "shadow",
            Arc::new(|_, _| Ok(Arc::new(Nothing) as Arc<dyn SubmitStrategy>)),
        ));
        assert!(matches!(dup, Err(SubmitRegistryError::Duplicate { .. })));
        assert!(!registry.register_if_absent(SubmitStrategySpec::new(
            MERGE_STRATEGY,
            "shadow",
            Arc::new(|_, _| Ok(Arc::new(Nothing) as Arc<dyn SubmitStrategy>)),
        )));
        assert_eq!(
            registry.resolve(MERGE_STRATEGY).unwrap().summary,
            "fast-forward merge into the phase base (default)"
        );
    }

    #[test]
    fn build_error_is_branded() {
        let err = SubmitRegistryError::build("pr", "[submit] remote is required");
        assert_eq!(
            err.to_string(),
            "cannot build submit strategy \"pr\": [submit] remote is required"
        );
        let tmp = tempfile::tempdir().unwrap();
        let (req, _lines) = request(tmp.path());
        let rendered = format!("{req:?}");
        assert!(rendered.contains("policy: false"));
        assert!(rendered.contains("on_failure: \"park\""));
    }
}
