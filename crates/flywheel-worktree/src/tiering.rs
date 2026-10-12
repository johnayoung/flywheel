//! Risk-tiered landing: classify a DONE branch's diff and route its landing.
//!
//! Spec 00080. With `[[submit.tiers]]` configured, the landing strategy
//! applied to each DONE task is a mechanical function of its diff, not a
//! repo-global constant: the changed files are classified at submit time
//! against operator-declared path rules -- tier 0 lands direct (`merge`),
//! tier 1 lands on the phase integration branch (`phase`, spec 00079), tier
//! 2 goes to a PR requiring human approval (`pr`). The highest tier of any
//! touched file wins and an unmatched file defaults to tier 1 (D-3), so a
//! newly invented path is never a silent direct-merge lane.
//!
//! Classification is deliberately agent-external: [`classify_changed_files`]
//! consumes the [`SubmitTierRule`] rules the WORKER process loaded from the
//! operator's checkout, never any copy in the task branch or worktree -- an
//! agent that rewrites its worktree's `flywheel.toml` to declare everything
//! tier 0 changes nothing, and the policy-file path itself always classifies
//! at the highest tier (criterion 4). `protected_paths` remains the
//! distinct, absolute refuse-list: every routed strategy re-evaluates it
//! first, so a protected file parks regardless of its computed tier (D-1,
//! criterion 5).
//!
//! Port of the Python `flywheel_worktree.tiering` module. Deviation: a
//! non-DONE run is handed straight to the inherited park/destroy path
//! without classifying its diff -- the Python derived and witnessed a
//! routing decision for a failed run that never routed anywhere.

use crate::submit_registry::{
    MERGE_STRATEGY, PHASE_STRATEGY, PR_STRATEGY, SubmitBuildRequest, SubmitRegistryError,
    SubmitStrategyRegistry, SubmitStrategySpec,
};
use crate::submitter::{
    GitError, GitWorktreeSubmitter, LedgerWriteError, SubmitError, build_merge_submitter, git,
    protected_path_matches,
};
use chrono::Utc;
use flywheel_core::events::{
    DomainEvent, DomainEventPayload, LandingRouted, PARK_KIND_SUBMIT_ERROR,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::StoreError;
use flywheel_orchestrator::policy::{DEFAULT_POLICY_FILENAME, SUBMIT_TIER_DEFAULT, SubmitTierRule};
use flywheel_orchestrator::strategy::{
    LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    SubmitStrategy,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Tier-to-strategy routing (spec 00080): the registry name each winning
/// tier lands through. The vocabulary is closed -- policy validation pins
/// tiers to this domain -- and the mapping is deliberately not configurable:
/// loosening a route is a spec change, not a knob.
pub const TIER_ROUTE_STRATEGIES: [(u32, &str); 3] =
    [(0, MERGE_STRATEGY), (1, PHASE_STRATEGY), (2, PR_STRATEGY)];

/// The most restrictive tier: where the policy file itself always lands.
pub const HIGHEST_TIER: u32 = 2;

/// The registry name the tier router registers under.
pub const TIER_STRATEGY: &str = "tiered";

/// The strategy a winning tier routes to, `None` outside the closed
/// vocabulary.
pub fn tier_route_strategy(tier: u32) -> Option<&'static str> {
    TIER_ROUTE_STRATEGIES
        .iter()
        .find(|(t, _)| *t == tier)
        .map(|(_, name)| *name)
}

/// One classified diff: every changed file's tier and the winning route.
///
/// `per_file_tiers` maps each changed file (repo-relative, the same
/// merge-base-scoped diff the protected-path gate reads) to its computed
/// tier; `winning_tier` is the highest of those; `strategy` is the
/// [`TIER_ROUTE_STRATEGIES`] registry name the decision routes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDecision {
    pub per_file_tiers: BTreeMap<String, u32>,
    pub winning_tier: u32,
    pub strategy: String,
}

/// Why a diff could not be classified. Both fail closed: the router parks
/// the run as `submit-error` rather than guessing a route.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyError {
    #[error("cannot classify an empty changed-file set")]
    EmptyFileSet,
    #[error("tier rule declares tier {0}, outside the closed vocabulary 0/1/2")]
    UnknownTier(i64),
}

/// Classify `files` against `rules`; highest tier wins (spec 00080).
///
/// Per file: the highest tier of any rule whose glob matches
/// ([`protected_path_matches`] semantics, `**` crosses directories -- the
/// `protected_paths` vocabulary); a file matching no rule classifies at
/// [`SUBMIT_TIER_DEFAULT`] (tier 1, D-3). The `policy_file` path -- the
/// source of the rules themselves -- always classifies at the highest tier
/// regardless of what any rule says about it (criterion 4): a diff that
/// edits the rules can never use those edits to cheapen its own route.
/// `files` must be non-empty: an empty diff has no tier and the caller
/// routes it to the plain merge no-op path instead.
pub fn classify_changed_files(
    files: &[String],
    rules: &[SubmitTierRule],
    policy_file: &str,
) -> Result<TierDecision, ClassifyError> {
    if files.is_empty() {
        return Err(ClassifyError::EmptyFileSet);
    }
    let default_tier = tier_in_vocabulary(SUBMIT_TIER_DEFAULT)?;
    let mut per_file: BTreeMap<String, u32> = BTreeMap::new();
    for path in files {
        if path == policy_file {
            per_file.insert(path.clone(), HIGHEST_TIER);
            continue;
        }
        let mut matched: Option<u32> = None;
        for rule in rules {
            if rule
                .paths
                .iter()
                .any(|pattern| protected_path_matches(path, pattern))
            {
                let tier = tier_in_vocabulary(rule.tier)?;
                matched = Some(matched.map_or(tier, |m| m.max(tier)));
            }
        }
        per_file.insert(path.clone(), matched.unwrap_or(default_tier));
    }
    let winning = per_file.values().copied().max().unwrap_or(default_tier);
    let strategy = tier_route_strategy(winning)
        .ok_or(ClassifyError::UnknownTier(i64::from(winning)))?
        .to_owned();
    Ok(TierDecision {
        per_file_tiers: per_file,
        winning_tier: winning,
        strategy,
    })
}

fn tier_in_vocabulary(tier: i64) -> Result<u32, ClassifyError> {
    u32::try_from(tier)
        .ok()
        .filter(|t| tier_route_strategy(*t).is_some())
        .ok_or(ClassifyError::UnknownTier(tier))
}

/// Provision like the merge strategy; route each landing by its tier.
///
/// Provisioning, landability, and every non-landing concern come from the
/// wrapped [`GitWorktreeSubmitter`] unchanged -- worktrees always fork from
/// the true base, so a diff classifies against the base the operator
/// actually reviews against. Only the landing differs: the branch's
/// merge-base-scoped changed files are classified against the worker-loaded
/// tier rules, the decision is recorded on the run's ledger as a
/// [`LandingRouted`] audit witness (before the routed strategy runs, so a
/// subsequent park still leaves the decision queryable), and the whole
/// submit request is delegated to the winning tier's strategy: `merge` runs
/// the wrapped submitter's own landing ladder, `phase` and `pr` run the
/// registry-built delegates. Each delegate re-runs its own full verify
/// ladder including the protected-path refusal, so protection outranks
/// every tier by construction (criterion 5). A diff that cannot be derived
/// fails closed: the error parks the run as `submit-error`.
///
/// A tier-1 (phase) route lands a true-base-forked branch onto the phase
/// integration branch; when that branch has already advanced, the
/// delegate's rebase-once-then-reverify rung carries it, so nothing lands
/// unverified against the exact ref it lands on.
#[derive(Clone)]
pub struct TierRoutingSubmitter {
    inner: GitWorktreeSubmitter,
    /// The worker-loaded `[[submit.tiers]]` rules.
    pub tier_rules: Vec<SubmitTierRule>,
    delegates: BTreeMap<String, Arc<dyn SubmitStrategy>>,
    /// The repo-relative policy file path that always classifies at the
    /// highest tier.
    pub policy_file: String,
}

impl fmt::Debug for TierRoutingSubmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TierRoutingSubmitter")
            .field("inner", &self.inner)
            .field("tier_rules", &self.tier_rules)
            .field("delegates", &self.delegates.keys().collect::<Vec<_>>())
            .field("policy_file", &self.policy_file)
            .finish()
    }
}

impl std::ops::Deref for TierRoutingSubmitter {
    type Target = GitWorktreeSubmitter;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl TierRoutingSubmitter {
    /// Wrap `inner` (the merge route) with `tier_rules` and the `phase` /
    /// `pr` `delegates`, keyed by registry name. A tier whose delegate is
    /// absent lands through `inner`.
    pub fn new(
        inner: GitWorktreeSubmitter,
        tier_rules: Vec<SubmitTierRule>,
        delegates: BTreeMap<String, Arc<dyn SubmitStrategy>>,
    ) -> Self {
        Self {
            inner,
            tier_rules,
            delegates,
            policy_file: DEFAULT_POLICY_FILENAME.to_owned(),
        }
    }

    pub fn with_policy_file(mut self, policy_file: impl Into<String>) -> Self {
        self.policy_file = policy_file.into();
        self
    }

    pub fn inner(&self) -> &GitWorktreeSubmitter {
        &self.inner
    }

    /// The delegate registered under `name`, if any.
    pub fn delegate(&self, name: &str) -> Option<&Arc<dyn SubmitStrategy>> {
        self.delegates.get(name)
    }

    /// Registered delegate names, sorted.
    pub fn delegate_names(&self) -> Vec<&str> {
        self.delegates.keys().map(String::as_str).collect()
    }

    fn log(&self, message: impl AsRef<str>) {
        (self.inner.log)(message.as_ref());
    }

    /// Repo-relative paths the branch touches vs the true base.
    ///
    /// The same merge-base-scoped diff (`base...branch`) the protected-path
    /// gate reads, so classification and protection judge the identical
    /// file set. Fail closed: an underivable diff is an error (the submit
    /// wrapper parks the run as `submit-error`) rather than a guessed route.
    pub fn changed_files(&self, branch: &str, base: &str) -> Result<Vec<String>, GitError> {
        let range = format!("{base}...{branch}");
        let res = git(&self.inner.repo_root, &["diff", "--name-only", &range])?;
        if !res.ok() {
            return Err(GitError::Failed {
                cwd: self.inner.repo_root.clone(),
                args: format!(
                    "diff --name-only {range} (tier classification cannot derive the changed \
                     files of {branch})"
                ),
                stderr: res.stderr.trim().to_owned(),
            });
        }
        Ok(res
            .stdout
            .lines()
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Classify and route. Every leaf returns an outcome; an `Err` is the
    /// Python "raise" that [`SubmitStrategy::submit`] contains.
    pub fn submit_inner(&self, req: &SubmitRequest) -> Result<SubmitOutcome, SubmitError> {
        if req.status != Status::Done {
            // Nothing routes: the inherited path parks (or destroys) the
            // worktree per [sandbox.retention] on_failure.
            return self.inner.submit_inner(req);
        }
        let task_id = &req.task_id;
        let phase = self.inner.phase_of(req.task_file.as_deref());
        let branch = self.inner.branch(task_id, &phase);
        let base = self.inner.landing_base_for(&phase)?;
        let changed = self.changed_files(&branch, &base)?;
        if changed.is_empty() {
            // Nothing to classify: an empty diff is the merge path's
            // no-op/cleanup case, identical to the un-tiered behavior.
            return self.inner.submit_inner(req);
        }
        let decision = classify_changed_files(&changed, &self.tier_rules, &self.policy_file)
            .map_err(|err| SubmitError::Runtime(std::io::Error::other(err.to_string())))?;
        self.record_landing_routed(&req.run_id, &decision);
        self.log(format!(
            "tier routing {task_id}: {} changed file(s), winning tier {} -> {} strategy",
            changed.len(),
            decision.winning_tier,
            decision.strategy
        ));
        match self.delegates.get(&decision.strategy) {
            Some(delegate) => Ok(delegate.submit(req)),
            None => self.inner.submit_inner(req),
        }
    }

    /// Append the queryable `landing_routed` audit witness (criterion 7).
    ///
    /// Recorded at classification time -- before the routed strategy runs
    /// -- so the decision outlives a subsequent park. Best-effort exactly
    /// like the submitter's own witnesses: a missing store handle, a
    /// declined preflight, or any store error is logged, never raised, and
    /// the append is guarded by the same disk/inode preflight.
    pub fn record_landing_routed(&self, run_id: &str, decision: &TierDecision) {
        let Some(store) = &self.inner.store else {
            return;
        };
        let payload = DomainEventPayload::LandingRouted(LandingRouted {
            winning_tier: decision.winning_tier,
            strategy: decision.strategy.clone(),
            per_file_tiers: decision.per_file_tiers.clone(),
        });
        let mut append = || -> Result<(), StoreError> {
            let Some(lifecycle) = store.load_lifecycle(run_id)? else {
                self.log(format!(
                    "cannot record landing-routed event: no lifecycle for {run_id}"
                ));
                return Ok(());
            };
            let event = DomainEvent::new(run_id, Utc::now(), payload.clone());
            store.append_domain_event(&event, lifecycle.version)?;
            Ok(())
        };
        let result = match &self.inner.ledger_guard {
            Some(guard) => guard.guard(&self.inner.repo_root, run_id, &mut append),
            None => append().map_err(LedgerWriteError::Store),
        };
        if let Err(err) = result {
            self.log(format!(
                "failed to record landing-routed event for {run_id} ({err})"
            ));
        }
    }
}

impl SubmitStrategy for TierRoutingSubmitter {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        self.inner.prepare_sandbox(request)
    }

    /// Route a DONE task's landing by its diff's tier, or park the
    /// worktree. Never fails: a classification or git failure is logged,
    /// witnessed as a `submit-error` park, and the worktree left parked.
    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        match self.submit_inner(request) {
            Ok(outcome) => outcome,
            Err(err) => {
                let worktree = self.inner.worktree(&request.task_id);
                self.log(format!(
                    "submit error for {} ({}: {err}); worktree left parked at {}",
                    request.task_id,
                    err.kind(),
                    worktree.display()
                ));
                let detail = format!(
                    "submit raised {}: {err}; worktree left parked at {}",
                    err.kind(),
                    worktree.display()
                );
                self.inner.record_landing_park(
                    &request.run_id,
                    &flywheel_core::events::LandingParked {
                        park_kind: PARK_KIND_SUBMIT_ERROR.to_owned(),
                        detail: detail.clone(),
                        ..Default::default()
                    },
                );
                SubmitOutcome::parked(PARK_KIND_SUBMIT_ERROR, detail)
            }
        }
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        self.inner.is_landable(request)
    }
}

/// Build the tier-routing submitter for a policy with `[[submit.tiers]]`.
///
/// The `phase` and `pr` delegates are resolved through `registry` with the
/// identical request (routing goes through the existing registry), so each
/// routed landing runs the exact strategy an operator would have gotten by
/// configuring it repo-globally; the merge route is the router's own
/// wrapped landing, built with the same `[submit]` knobs as
/// [`build_merge_submitter`]. A request without a policy, or with no tier
/// rules, cannot build: the worker selects this builder only when the
/// loaded policy carries rules.
pub fn build_tier_submitter(
    registry: &SubmitStrategyRegistry,
    request: &SubmitBuildRequest,
) -> Result<TierRoutingSubmitter, SubmitRegistryError> {
    let Some(policy) = request.policy.as_deref() else {
        return Err(SubmitRegistryError::build(
            TIER_STRATEGY,
            "[[submit.tiers]] routing requires a loaded policy",
        ));
    };
    if policy.submit_tiers.is_empty() {
        return Err(SubmitRegistryError::build(
            TIER_STRATEGY,
            "no [[submit.tiers]] rules are configured",
        ));
    }
    let mut delegates: BTreeMap<String, Arc<dyn SubmitStrategy>> = BTreeMap::new();
    for (_, name) in TIER_ROUTE_STRATEGIES {
        if name == MERGE_STRATEGY {
            continue;
        }
        delegates.insert(name.to_owned(), registry.build(name, request)?);
    }
    let submitter = TierRoutingSubmitter::new(
        build_merge_submitter(request),
        policy.submit_tiers.clone(),
        delegates,
    );
    (request.log)(&format!(
        "landing strategy: tiered ({} rule(s); 0=merge 1=phase 2=pr, unmatched=tier \
         {SUBMIT_TIER_DEFAULT})",
        policy.submit_tiers.len()
    ));
    Ok(submitter)
}

/// Register the tier router on `registry` under [`TIER_STRATEGY`]; a
/// second registration is a [`SubmitRegistryError::Duplicate`].
pub fn register_tier_strategy(
    registry: &mut SubmitStrategyRegistry,
) -> Result<(), SubmitRegistryError> {
    registry.register(SubmitStrategySpec::new(
        TIER_STRATEGY,
        "route each landing by its diff's risk tier ([[submit.tiers]]): 0=merge 1=phase 2=pr",
        Arc::new(|registry, request| {
            Ok(Arc::new(build_tier_submitter(registry, request)?) as Arc<dyn SubmitStrategy>)
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::{PhaseBranchSubmitter, phase_branch};
    use crate::pr::test_support::{ScriptedGh, init_repo_with_remote};
    use crate::pr::{GitPullRequestSubmitter, register_pr_strategy};
    use crate::submit_registry::builtin_registry;
    use crate::submitter::{Logger, test_support};
    use flywheel_core::events::{LifecycleInitialized, PARK_KIND_PROTECTED_PATHS};
    use flywheel_core::store::DomainEventStore;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_orchestrator::policy::WorkPolicy;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn rule(tier: i64, paths: &[&str]) -> SubmitTierRule {
        SubmitTierRule {
            tier,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    fn capture_log() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Logger = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        (log, lines)
    }

    /// A ledger seeded with lifecycles for `run_ids` so witnesses append.
    fn ledger(run_ids: &[&str]) -> Arc<InMemoryStore> {
        let store = InMemoryStore::new();
        for run_id in run_ids {
            let seed = DomainEvent::new(
                *run_id,
                Utc::now(),
                DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                    task_id: "t".into(),
                    ..LifecycleInitialized::default()
                }),
            );
            store.append_domain_event(&seed, 0).unwrap();
        }
        Arc::new(store)
    }

    fn routed(store: &InMemoryStore, run_id: &str) -> Vec<LandingRouted> {
        store
            .list_domain_events(run_id)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::LandingRouted(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn park_kinds(store: &InMemoryStore, run_id: &str) -> Vec<String> {
        store
            .list_domain_events(run_id)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::LandingParked(p) => Some(p.park_kind),
                _ => None,
            })
            .collect()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        gh: Arc<ScriptedGh>,
        store: Arc<InMemoryStore>,
    }

    impl Fixture {
        fn new(run_ids: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            init_repo_with_remote(&repo);
            Self {
                _dir: dir,
                repo,
                gh: ScriptedGh::new(""),
                store: ledger(run_ids),
            }
        }

        fn shared(&self, protected: &[&str], log: Logger) -> GitWorktreeSubmitter {
            let worktrees = self.repo.join(".flywheel").join("worktrees");
            fs::create_dir_all(&worktrees).unwrap();
            GitWorktreeSubmitter::new(
                &self.repo,
                self.repo.join(".flywheel").join("tasks"),
                worktrees,
                "main",
                self.repo.join(".flywheel").join(".merge.lock"),
                log,
            )
            .with_store(Arc::clone(&self.store) as Arc<dyn crate::submitter::LandingLedger>)
            .with_protected_paths(protected.iter().copied())
        }

        /// The router with real `phase` and `pr` delegates sharing its
        /// layout, protected paths, and ledger.
        fn router(&self, rules: Vec<SubmitTierRule>, protected: &[&str]) -> TierRoutingSubmitter {
            self.router_with_log(rules, protected, test_support::quiet())
        }

        fn router_with_log(
            &self,
            rules: Vec<SubmitTierRule>,
            protected: &[&str],
            log: Logger,
        ) -> TierRoutingSubmitter {
            let mut delegates: BTreeMap<String, Arc<dyn SubmitStrategy>> = BTreeMap::new();
            delegates.insert(
                PHASE_STRATEGY.into(),
                Arc::new(PhaseBranchSubmitter::new(
                    self.shared(protected, Arc::clone(&log)),
                )),
            );
            delegates.insert(
                PR_STRATEGY.into(),
                Arc::new(
                    GitPullRequestSubmitter::new(self.shared(protected, Arc::clone(&log)))
                        .with_gh(self.gh.clone()),
                ),
            );
            TierRoutingSubmitter::new(self.shared(protected, log), rules, delegates)
        }

        fn rev(&self, r: &str) -> String {
            test_support::rev(&self.repo, r)
        }

        fn tree(&self, r: &str) -> Vec<String> {
            test_support::git_ok(&self.repo, &["ls-tree", "-r", "--name-only", r])
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// Provision `task_id`, commit `files`, submit DONE under `run_id`.
        fn land(
            &self,
            s: &TierRoutingSubmitter,
            task_id: &str,
            files: &[(&str, &str)],
            run_id: &str,
        ) -> (SubmitOutcome, PathBuf) {
            let (tf, wt) = self.provision(s, task_id, files);
            let mut req = test_support::submit_req(&tf, task_id, &wt, Status::Done, &["true"]);
            req.run_id = run_id.into();
            (s.submit(&req), wt)
        }

        fn provision(
            &self,
            s: &TierRoutingSubmitter,
            task_id: &str,
            files: &[(&str, &str)],
        ) -> (PathBuf, PathBuf) {
            let tf = test_support::task_file(&self.repo, "01-phase", task_id, "true");
            let wt = s
                .prepare_worktree(&test_support::sandbox_req(&tf, task_id))
                .unwrap();
            for (name, body) in files {
                let target = wt.join(name);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(&target, body).unwrap();
            }
            test_support::git_ok(&wt, &["add", "-A"]);
            test_support::git_ok(&wt, &["commit", "-q", "-m", &format!("feat: {task_id}")]);
            (tf, wt)
        }
    }

    // --- the pure classifier ---------------------------------------------------------------

    #[test]
    fn classify_highest_tier_wins_per_file_and_per_diff() {
        let rules = vec![rule(0, &["docs/**"]), rule(2, &["docs/security/**"])];
        let decision = classify_changed_files(
            &files(&["docs/a.md", "docs/security/threat.md"]),
            &rules,
            DEFAULT_POLICY_FILENAME,
        )
        .unwrap();
        assert_eq!(
            decision.per_file_tiers,
            BTreeMap::from([
                ("docs/a.md".to_owned(), 0),
                ("docs/security/threat.md".to_owned(), 2)
            ])
        );
        assert_eq!(decision.winning_tier, 2);
        assert_eq!(decision.strategy, "pr");
    }

    #[test]
    fn classify_unmatched_file_defaults_to_tier_1() {
        let decision = classify_changed_files(
            &files(&["src/new_lane.py"]),
            &[rule(0, &["docs/**"])],
            DEFAULT_POLICY_FILENAME,
        )
        .unwrap();
        assert_eq!(
            decision.per_file_tiers,
            BTreeMap::from([("src/new_lane.py".to_owned(), 1)])
        );
        assert_eq!(decision.winning_tier, 1);
        assert_eq!(decision.strategy, "phase");
        assert_eq!(SUBMIT_TIER_DEFAULT, 1);
    }

    #[test]
    fn classify_policy_file_forced_to_highest_tier() {
        // Even a rule that names the policy file tier 0 cannot cheapen it.
        let decision = classify_changed_files(
            &files(&["flywheel.toml", "docs/a.md"]),
            &[rule(0, &["**"])],
            DEFAULT_POLICY_FILENAME,
        )
        .unwrap();
        assert_eq!(decision.per_file_tiers["flywheel.toml"], HIGHEST_TIER);
        assert_eq!(decision.per_file_tiers["docs/a.md"], 0);
        assert_eq!(decision.strategy, "pr");
        // A relocated policy file is honored by name.
        let moved = classify_changed_files(
            &files(&["flywheel.toml", "cfg/policy.toml"]),
            &[rule(0, &["**"])],
            "cfg/policy.toml",
        )
        .unwrap();
        assert_eq!(moved.per_file_tiers["flywheel.toml"], 0);
        assert_eq!(moved.per_file_tiers["cfg/policy.toml"], 2);
    }

    #[test]
    fn classify_empty_file_set_is_an_error() {
        assert_eq!(
            classify_changed_files(&[], &[], DEFAULT_POLICY_FILENAME),
            Err(ClassifyError::EmptyFileSet)
        );
        assert!(ClassifyError::EmptyFileSet.to_string().contains("empty"));
    }

    #[test]
    fn classify_rejects_a_tier_outside_the_vocabulary() {
        let err = classify_changed_files(
            &files(&["docs/a.md"]),
            &[rule(7, &["docs/**"])],
            DEFAULT_POLICY_FILENAME,
        )
        .unwrap_err();
        assert_eq!(err, ClassifyError::UnknownTier(7));
        let negative = classify_changed_files(
            &files(&["docs/a.md"]),
            &[rule(-1, &["docs/**"])],
            DEFAULT_POLICY_FILENAME,
        )
        .unwrap_err();
        assert_eq!(negative, ClassifyError::UnknownTier(-1));
        // A non-matching rule's tier is never consulted.
        assert!(
            classify_changed_files(
                &files(&["src/x.py"]),
                &[rule(7, &["docs/**"])],
                DEFAULT_POLICY_FILENAME,
            )
            .is_ok()
        );
        assert_eq!(tier_route_strategy(3), None);
        assert_eq!(tier_route_strategy(0), Some("merge"));
    }

    // --- criterion 1: tier 0 lands direct; no phase branch, no PR ------------------------

    #[test]
    fn tier0_diff_lands_direct_on_base() {
        let f = Fixture::new(&["r1"]);
        let (log, lines) = capture_log();
        let s = f.router_with_log(vec![rule(0, &["docs/**"])], &[], log);

        let (outcome, wt) = f.land(&s, "t-docs", &[("docs/a.md", "text")], "r1");

        assert!(outcome.is_landed(), "{outcome:?}");
        assert!(f.tree("main").contains(&"docs/a.md".to_owned()));
        assert!(!test_support::branch_exists(
            &f.repo,
            &phase_branch("01-phase")
        ));
        assert!(f.gh.calls().is_empty());
        assert!(!wt.exists());
        assert!(
            lines.lock().unwrap().contains(
                &"tier routing t-docs: 1 changed file(s), winning tier 0 -> merge strategy"
                    .to_owned()
            ),
            "{:?}",
            lines.lock().unwrap()
        );
    }

    // --- criterion 2: any tier-2 file wins; base and phase branch untouched ---------------

    #[test]
    fn tier2_file_among_tier0_routes_to_pr() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![rule(0, &["docs/**"]), rule(2, &["scripts/**"])], &[]);
        let base_before = f.rev("main");

        let (outcome, _) = f.land(
            &s,
            "t-mixed",
            &[
                ("docs/a.md", "text"),
                ("docs/b.md", "text"),
                ("scripts/gate.sh", "x"),
            ],
            "r1",
        );

        assert_eq!(
            outcome,
            SubmitOutcome::landed("pr", crate::pr::test_support::CREATED_URL)
        );
        assert!(f.gh.commands().contains(&"pr create".to_owned()));
        assert_eq!(f.rev("main"), base_before);
        assert!(!test_support::branch_exists(
            &f.repo,
            &phase_branch("01-phase")
        ));
        let witness = routed(&f.store, "r1");
        assert_eq!(witness.len(), 1);
        assert_eq!(witness[0].winning_tier, 2);
        assert_eq!(witness[0].per_file_tiers["scripts/gate.sh"], 2);
        assert_eq!(witness[0].per_file_tiers["docs/a.md"], 0);
    }

    // --- criterion 3: unmatched defaults to tier 1 (phase branch) ------------------------

    #[test]
    fn unmatched_diff_lands_on_phase_branch_not_base() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![rule(0, &["docs/**"])], &[]);
        let base_before = f.rev("main");

        let (outcome, _) = f.land(&s, "t-new", &[("src/new_lane.py", "pass")], "r1");

        assert!(outcome.is_landed(), "{outcome:?}");
        let integration = phase_branch("01-phase");
        assert!(test_support::branch_exists(&f.repo, &integration));
        assert_eq!(f.rev("main"), base_before);
        assert!(f.tree(&integration).contains(&"src/new_lane.py".to_owned()));
        assert!(f.gh.calls().is_empty());
    }

    #[test]
    fn second_unmatched_task_stacks_via_rebase_rung() {
        // Tier-routed worktrees fork from the true base, so the second
        // task's branch does not contain the advanced integration branch;
        // the phase delegate's rebase-once-then-reverify rung carries it.
        let f = Fixture::new(&["r1", "r2"]);
        let s = f.router(vec![rule(0, &["docs/**"])], &[]);

        let (first, _) = f.land(&s, "t-one", &[("src/one.py", "pass")], "r1");
        let (second, _) = f.land(&s, "t-two", &[("src/two.py", "pass")], "r2");

        assert!(first.is_landed() && second.is_landed());
        let tree = f.tree(&phase_branch("01-phase"));
        assert!(tree.contains(&"src/one.py".to_owned()));
        assert!(tree.contains(&"src/two.py".to_owned()));
    }

    // --- criterion 4: the policy file classifies worker-side, at the top tier -----------

    #[test]
    fn policy_file_edit_routes_to_pr_despite_worktree_rules() {
        let f = Fixture::new(&["r1"]);
        // The WORKER-loaded rules declare everything tier 0 -- the cheapest
        // possible configuration -- and the branch additionally rewrites its
        // worktree policy copy to say the same. Neither cheapens the route.
        let s = f.router(vec![rule(0, &["**"])], &[]);
        let base_before = f.rev("main");

        let (outcome, _) = f.land(
            &s,
            "t-rules",
            &[
                (
                    "flywheel.toml",
                    "[[submit.tiers]]\ntier = 0\npaths = [\"**\"]\n",
                ),
                ("docs/a.md", "text"),
            ],
            "r1",
        );

        assert!(outcome.is_landed());
        assert!(f.gh.commands().contains(&"pr create".to_owned()));
        assert_eq!(f.rev("main"), base_before);
        assert!(!test_support::branch_exists(
            &f.repo,
            &phase_branch("01-phase")
        ));
    }

    // --- criterion 5: protected paths park regardless of tier --------------------------

    #[test]
    fn protected_path_parks_regardless_of_tier() {
        let f = Fixture::new(&["r1"]);
        // The same path is both tier 2 and protected: protection is refusal,
        // not routing -- the routed strategy's protected gate parks before
        // any push.
        let s = f.router(vec![rule(2, &["ci/**"])], &["ci/**"]);
        let base_before = f.rev("main");

        let (outcome, wt) = f.land(&s, "t-ci", &[("ci/gate.yml", "x")], "r1");

        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_PROTECTED_PATHS)
        );
        assert_eq!(park_kinds(&f.store, "r1"), [PARK_KIND_PROTECTED_PATHS]);
        assert!(!f.gh.commands().contains(&"pr create".to_owned()));
        assert_eq!(f.rev("main"), base_before);
        assert!(!test_support::branch_exists(
            &f.repo,
            &phase_branch("01-phase")
        ));
        assert!(wt.exists());
        // The routing decision itself is still recorded (criterion 7): the
        // park is diagnosable together with the route that led to it.
        let witness = routed(&f.store, "r1");
        assert_eq!(witness.len(), 1);
        assert_eq!(witness[0].strategy, "pr");
    }

    #[test]
    fn protected_path_parks_on_the_merge_and_phase_routes_too() {
        let f = Fixture::new(&["r0", "r1"]);
        let s = f.router(vec![rule(0, &["ci/**"])], &["ci/**"]);
        let (outcome, _) = f.land(&s, "t-merge", &[("ci/gate.yml", "x")], "r0");
        assert!(outcome.is_parked());
        assert_eq!(park_kinds(&f.store, "r0"), [PARK_KIND_PROTECTED_PATHS]);
        assert_eq!(routed(&f.store, "r0")[0].strategy, "merge");

        let s = f.router(vec![], &["ci/**"]);
        let (outcome, _) = f.land(&s, "t-phase", &[("ci/other.yml", "x")], "r1");
        assert!(outcome.is_parked());
        assert_eq!(park_kinds(&f.store, "r1"), [PARK_KIND_PROTECTED_PATHS]);
        assert_eq!(routed(&f.store, "r1")[0].strategy, "phase");
        assert!(!test_support::branch_exists(
            &f.repo,
            &phase_branch("01-phase")
        ));
    }

    // --- criterion 7: the decision is recorded on the run's ledger ----------------------

    #[test]
    fn landing_routed_event_recorded_per_tier() {
        let f = Fixture::new(&["r0", "r1", "r2"]);
        let s = f.router(vec![rule(0, &["docs/**"]), rule(2, &["scripts/**"])], &[]);

        f.land(&s, "t0", &[("docs/a.md", "text")], "r0");
        f.land(&s, "t1", &[("src/x.py", "pass")], "r1");
        f.land(&s, "t2", &[("scripts/y.sh", "x")], "r2");

        let r0 = routed(&f.store, "r0");
        assert_eq!(r0.len(), 1);
        assert_eq!(r0[0].winning_tier, 0);
        assert_eq!(r0[0].strategy, "merge");
        assert_eq!(
            r0[0].per_file_tiers,
            BTreeMap::from([("docs/a.md".to_owned(), 0)])
        );
        let r1 = routed(&f.store, "r1");
        assert_eq!(r1[0].winning_tier, 1);
        assert_eq!(r1[0].strategy, "phase");
        let r2 = routed(&f.store, "r2");
        assert_eq!(r2[0].winning_tier, 2);
        assert_eq!(r2[0].strategy, "pr");
        // The ledger write is best-effort: an unknown run logs, never fails.
        let (log, lines) = capture_log();
        let s = f.router_with_log(vec![], &[], log);
        s.record_landing_routed(
            "r-unknown",
            &TierDecision {
                per_file_tiers: BTreeMap::new(),
                winning_tier: 1,
                strategy: "phase".into(),
            },
        );
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            ["cannot record landing-routed event: no lifecycle for r-unknown"]
        );
    }

    // --- non-DONE, empty diffs, and failures --------------------------------------------

    #[test]
    fn failed_run_parks_without_classifying() {
        let f = Fixture::new(&["r1"]);
        let (log, lines) = capture_log();
        let s = f.router_with_log(vec![rule(0, &["docs/**"])], &[], log);
        let (tf, wt) = f.provision(&s, "t-fail", &[("docs/a.md", "text")]);
        let mut req = test_support::submit_req(&tf, "t-fail", &wt, Status::Failed, &["true"]);
        req.run_id = "r1".into();

        let outcome = s.submit(&req);

        assert!(matches!(outcome, SubmitOutcome::NoOp { .. }));
        assert!(wt.exists());
        assert!(routed(&f.store, "r1").is_empty());
        assert!(
            !lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("tier routing"))
        );
    }

    #[test]
    fn empty_diff_takes_the_merge_no_op_path() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![rule(2, &["**"])], &[]);
        let tf = test_support::task_file(&f.repo, "01-phase", "t-empty", "true");
        let wt = s
            .prepare_worktree(&test_support::sandbox_req(&tf, "t-empty"))
            .unwrap();
        let mut req = test_support::submit_req(&tf, "t-empty", &wt, Status::Done, &["true"]);
        req.run_id = "r1".into();

        let outcome = s.submit(&req);

        assert!(matches!(outcome, SubmitOutcome::NoOp { .. }), "{outcome:?}");
        assert!(!wt.exists());
        assert!(routed(&f.store, "r1").is_empty());
        assert!(f.gh.calls().is_empty());
    }

    #[test]
    fn underivable_diff_fails_closed_as_a_submit_error_park() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![rule(0, &["**"])], &[]);
        let tf = test_support::task_file(&f.repo, "t-gone", "t-gone", "true");
        // No worktree was ever provisioned: the branch does not exist, so
        // the diff cannot be derived and no route is guessed.
        let mut req = test_support::submit_req(
            &tf,
            "t-gone",
            &f.repo.join("nowhere"),
            Status::Done,
            &["true"],
        );
        req.run_id = "r1".into();
        let outcome = s.submit(&req);
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
                if park_kind == PARK_KIND_SUBMIT_ERROR
                    && detail.contains("tier classification cannot derive the changed files")),
            "{outcome:?}"
        );
        assert_eq!(park_kinds(&f.store, "r1"), [PARK_KIND_SUBMIT_ERROR]);
        assert!(routed(&f.store, "r1").is_empty());
    }

    #[test]
    fn invalid_rule_tier_fails_closed_as_a_submit_error_park() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![rule(9, &["**"])], &[]);
        let (outcome, wt) = f.land(&s, "t-bad", &[("docs/a.md", "text")], "r1");
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
                if park_kind == PARK_KIND_SUBMIT_ERROR
                    && detail.starts_with("submit raised RuntimeError: cannot start the grader runtime: tier rule declares tier 9")),
            "{outcome:?}"
        );
        assert!(wt.exists());
        assert!(f.gh.calls().is_empty());
    }

    #[test]
    fn missing_delegate_falls_back_to_the_wrapped_landing() {
        let f = Fixture::new(&["r1"]);
        let s = TierRoutingSubmitter::new(
            f.shared(&[], test_support::quiet()),
            vec![rule(2, &["**"])],
            BTreeMap::new(),
        );
        assert!(s.delegate_names().is_empty());
        assert!(s.delegate("pr").is_none());
        let (outcome, _) = f.land(&s, "t-direct", &[("docs/a.md", "text")], "r1");
        assert!(outcome.is_landed());
        assert_eq!(routed(&f.store, "r1")[0].strategy, "pr");
        assert!(f.tree("main").contains(&"docs/a.md".to_owned()));
        assert!(format!("{s:?}").contains("policy_file: \"flywheel.toml\""));
    }

    #[test]
    fn is_landable_delegates_to_the_worktree_probe() {
        let f = Fixture::new(&["r1"]);
        let s = f.router(vec![], &[]);
        let tf = test_support::task_file(&f.repo, "01-phase", "t-probe", "true");
        let wt = s
            .prepare_worktree(&test_support::sandbox_req(&tf, "t-probe"))
            .unwrap();
        let req = test_support::submit_req(&tf, "t-probe", &wt, Status::Done, &["true"]);
        assert!(!s.is_landable(&req).landable);
        assert_eq!(
            s.changed_files("flywheel/01-phase/t-probe", "main")
                .unwrap(),
            Vec::<String>::new()
        );
    }

    // --- the builder resolves delegates through the registry --------------------------

    fn request(tmp: &Path, log: Logger) -> SubmitBuildRequest {
        SubmitBuildRequest::new(
            tmp.join("repo"),
            tmp.join("tasks"),
            tmp.join("worktrees"),
            "main",
            tmp.join(".merge.lock"),
            log,
        )
    }

    #[test]
    fn build_tier_submitter_from_policy_through_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let (log, lines) = capture_log();
        let policy = WorkPolicy {
            submit_tiers: vec![rule(0, &["docs/**"])],
            submit_verify: Some("true".into()),
            ..WorkPolicy::default()
        };
        let req = request(tmp.path(), log).with_policy(Arc::new(policy.clone()));
        let mut registry = builtin_registry();
        register_pr_strategy(&mut registry).unwrap();
        register_tier_strategy(&mut registry).unwrap();
        assert_eq!(registry.names(), ["merge", "phase", "pr", "tiered"]);

        let s = build_tier_submitter(&registry, &req).unwrap();
        assert_eq!(s.tier_rules, policy.submit_tiers);
        assert_eq!(s.delegate_names(), ["phase", "pr"]);
        assert_eq!(s.verify_command.as_deref(), Some("true"));
        let logged = lines.lock().unwrap().clone();
        assert!(logged.iter().any(|l| l.contains("landing strategy: phase")));
        assert!(
            logged
                .iter()
                .any(|l| l == "landing strategy: pr (remote=origin base=main)")
        );
        assert_eq!(
            logged.last().unwrap(),
            "landing strategy: tiered (1 rule(s); 0=merge 1=phase 2=pr, unmatched=tier 1)"
        );
        let _strategy = registry.build(TIER_STRATEGY, &req).unwrap();
        assert!(matches!(
            register_tier_strategy(&mut registry),
            Err(SubmitRegistryError::Duplicate { .. })
        ));
    }

    #[test]
    fn build_tier_submitter_requires_a_policy_with_rules_and_a_pr_delegate() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = builtin_registry();
        let bare = request(tmp.path(), test_support::quiet());
        let err = build_tier_submitter(&registry, &bare).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot build submit strategy \"tiered\": [[submit.tiers]] routing requires a loaded policy"
        );
        let no_rules = bare.clone().with_policy(Arc::new(WorkPolicy::default()));
        let err = build_tier_submitter(&registry, &no_rules).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot build submit strategy \"tiered\": no [[submit.tiers]] rules are configured"
        );
        // Without the pr strategy registered the tier-2 delegate cannot be
        // resolved: loud, never a silent merge lane.
        let with_rules = bare.with_policy(Arc::new(WorkPolicy {
            submit_tiers: vec![rule(0, &["docs/**"])],
            ..WorkPolicy::default()
        }));
        let err = build_tier_submitter(&registry, &with_rules).unwrap_err();
        assert!(matches!(err, SubmitRegistryError::Unknown { ref name, .. } if name == "pr"));
    }
}
