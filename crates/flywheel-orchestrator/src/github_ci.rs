//! GitHub CI-failure work source: a red build becomes a graded task.
//!
//! A second GitHub adapter, distinct from [`crate::github`] (labeled
//! issues). This one lists a repo's FAILED continuous-integration runs
//! through the `gh` CLI (reusing the same injectable [`GhRunner`] seam) and
//! compiles each into a [`WorkItem`] the orchestrator drives to green. The
//! two adapters never mix: issues and CI failures have different listing,
//! keying, and write-back, so they live behind separate `[source] kind`
//! selectors (spec 00052 decision D-1).
//!
//! Run -> Task compilation:
//!
//! * `id` is `ci-<digest(workflow, branch)>`: STABLE for a given
//!   (workflow, branch) pair across polls, so a persistently-broken branch
//!   is ONE work item, not a new one every poll/push (D-3). It is NOT keyed
//!   on the per-run `databaseId` (that advances as the head moves, which
//!   would queue an unbounded stream of duplicates).
//! * `graders` come solely from the operator's policy `[[defaults.graders]]`.
//!   A CI run carries no embedded spec block, so a run that resolves to
//!   ZERO graders is **not runnable**: it is skipped and logged rather than
//!   run ungated or graded by a fabricated check the operator never
//!   declared (D-4). The authoritative "fixed" signal is the harness
//!   running those default graders out-of-band, never the GitHub check
//!   status, never a re-run (D-2).
//! * `source_version` tracks the failing head sha + conclusion, so a
//!   failure that moves to a new commit is detected even though the item
//!   id is stable.
//! * `source_url` locates the run; `source_kind` marks the CI provenance.
//!
//! A single malformed run row (a non-object entry, a missing identity
//! string, a definition that fails validation) is **skipped, counted, and
//! logged** rather than aborting the whole listing: one uncompilable run
//! must not hide every other failing run. The skip is recorded in
//! [`GithubCiWorkSource::last_skipped_count`] and one `log` line names the
//! offending run. But a parse break must never masquerade as "CI is
//! green": if the page held rows yet *nothing* compiled because rows were
//! dropped, `list_work` fails with the first skip's error instead of
//! returning an empty list.
//!
//! Outbound: [`GithubCiWorkSource::report`] posts the run's grader receipts
//! back as a comment on the failing commit (never an issue mutation, never
//! a check-status flip), so a terminal outcome leaves an audit trail on
//! GitHub (D-5).
//!
//! Authoritative spec: `docs/work-sources.md` (`github_ci`).

use crate::github::{
    GhRunner, LIST_LIMIT, ProcessGhRunner, STOP_SOURCE_TRUNCATION, STOP_ZERO_GRADER_DROP,
    build_error, emit_truncation_warning, format_report_body, optional_string, parse_list_payload,
    py_type_name, require_repo, run_gh, sha256_hex, str_or_empty,
};
use crate::source_registry::{GITHUB_CI_KIND, SourceBuildContext, SourceRegistryError};
use crate::sources::{LogSink, StopEventSink, WorkItem, WorkReport, WorkSource, WorkSourceError};
use flywheel_core::task::{Context, Grader, Task};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Provenance `source_kind` for emitted items and `source_syncs` rows.
pub const GITHUB_CI_SOURCE_KIND: &str = "github_ci";

/// The `gh run list --json` fields the adapter reads. `workflowName` +
/// `headBranch` form the stable key; `headSha` + `conclusion` form
/// `source_version`; `createdAt` orders runs so dedup keeps the most
/// recent per key.
pub const RUN_LIST_FIELDS: &str =
    "databaseId,workflowName,headBranch,headSha,conclusion,url,displayTitle,event,createdAt";

/// The default failure filter: gh's `--status failure` selects runs whose
/// conclusion is a hard failure. Operators may widen it (e.g. `timed_out`).
pub const DEFAULT_FAILURE_FILTER: &str = "failure";

/// Stable item id for a (workflow, branch) pair (D-3).
///
/// A digest keeps the id well-formed regardless of the characters a
/// workflow name or branch ref carries, and stable across polls so a
/// persistent failure is one item, not a new id each pass.
pub fn ci_item_id(workflow: &str, branch: &str) -> String {
    let digest = sha256_hex(&format!("{workflow}\u{0}{branch}"));
    format!("ci-{}", &digest[..16])
}

/// Read a required non-empty string field or fail with the branded error.
///
/// A run row missing the identity fields the adapter keys on is malformed
/// output, not "no work": surfacing it keeps a parse break from reading as
/// a green repo.
fn required_str(run: &Map<String, Value>, key: &str) -> Result<String, WorkSourceError> {
    match run.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(WorkSourceError::new(format!(
            "gh run list payload missing string {key:?}: {}",
            Value::Object(run.clone())
        ))),
    }
}

/// Failed CI runs in one GitHub repo, compiled to flywheel tasks.
///
/// `runner` is the `gh` subprocess seam: production uses the real CLI,
/// tests inject a scripted runner returning canned stdout.
/// `failure_filter` is the `gh run list --status` value (default
/// `"failure"`); an operator may widen it to capture other failing
/// conclusions. `log` (when provided) receives one line per skipped run so
/// non-runnable failures are visible.
///
/// A run row that cannot be compiled into a valid [`WorkItem`] is skipped
/// rather than aborting the listing: it is counted in
/// [`GithubCiWorkSource::last_skipped_count`] and named in one `log` line,
/// mirroring [`crate::sources::DirectoryWorkSource`]. The count is what
/// lets a reconciler tell a dropped item ("investigate it") from an empty
/// source ("no work"); the two must never look alike.
pub struct GithubCiWorkSource {
    repo: String,
    default_graders: Vec<Grader>,
    failure_filter: String,
    runner: Arc<dyn GhRunner>,
    log: Option<LogSink>,
    stop_sink: Option<StopEventSink>,
    last_skipped_count: AtomicUsize,
}

impl fmt::Debug for GithubCiWorkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubCiWorkSource")
            .field("repo", &self.repo)
            .field("default_graders", &self.default_graders.len())
            .field("failure_filter", &self.failure_filter)
            .field("last_skipped_count", &self.last_skipped_count())
            .finish()
    }
}

impl GithubCiWorkSource {
    /// A source over `repo`'s failed runs (filter `failure`), driven by the
    /// real `gh` binary.
    pub fn new(repo: impl Into<String>) -> Result<Self, WorkSourceError> {
        let repo = repo.into();
        require_repo(&repo)?;
        Ok(Self {
            repo,
            default_graders: Vec::new(),
            failure_filter: DEFAULT_FAILURE_FILTER.to_string(),
            runner: Arc::new(ProcessGhRunner),
            log: None,
            stop_sink: None,
            last_skipped_count: AtomicUsize::new(0),
        })
    }

    /// The `[[defaults.graders]]` every CI item is graded by.
    pub fn with_default_graders(mut self, graders: Vec<Grader>) -> Self {
        self.default_graders = graders;
        self
    }

    /// The `gh run list --status` value; must be non-empty.
    pub fn with_failure_filter(
        mut self,
        failure_filter: impl Into<String>,
    ) -> Result<Self, WorkSourceError> {
        let failure_filter = failure_filter.into();
        if failure_filter.is_empty() {
            return Err(WorkSourceError::new(
                "failure_filter must be a non-empty string",
            ));
        }
        self.failure_filter = failure_filter;
        Ok(self)
    }

    /// Replace the `gh` subprocess seam.
    pub fn with_runner(mut self, runner: Arc<dyn GhRunner>) -> Self {
        self.runner = runner;
        self
    }

    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    pub fn with_stop_sink(mut self, sink: StopEventSink) -> Self {
        self.stop_sink = Some(sink);
        self
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn failure_filter(&self) -> &str {
        &self.failure_filter
    }

    pub fn default_graders(&self) -> &[Grader] {
        &self.default_graders
    }

    /// Number of run rows the most recent [`WorkSource::list_work`]
    /// skipped because they could not be compiled into a valid item
    /// (malformed output, missing identity field, failed validation). The
    /// no-grader readiness gate (D-4) is a policy decision, not a parse
    /// break, so it is logged but not counted here. Reset by every
    /// listing.
    pub fn last_skipped_count(&self) -> usize {
        self.last_skipped_count.load(Ordering::SeqCst)
    }

    fn log(&self, line: &str) {
        if let Some(log) = &self.log {
            log(line);
        }
    }

    fn stop(&self, kind: &str, detail: &str) {
        if let Some(sink) = &self.stop_sink {
            sink(kind, &self.repo, detail);
        }
    }

    fn compile_run(
        &self,
        run: &Map<String, Value>,
        workflow: &str,
        branch: &str,
    ) -> Result<Option<WorkItem>, WorkSourceError> {
        let head_sha = required_str(run, "headSha")?;
        let conclusion = str_or_empty(run, "conclusion");
        let url = str_or_empty(run, "url");
        let display_title = str_or_empty(run, "displayTitle");
        let event = str_or_empty(run, "event");

        let item_id = ci_item_id(workflow, branch);

        let graders = self.default_graders.clone();
        if graders.is_empty() {
            let detail = format!(
                "{item_id} ({workflow} on {branch}): no default grader policy -- not runnable"
            );
            self.log(&format!("[github_ci] skipping {detail}"));
            self.stop(STOP_ZERO_GRADER_DROP, &detail);
            return Ok(None);
        }

        let mut notes_lines = vec![
            format!("CI workflow '{workflow}' is failing on branch '{branch}'."),
            format!(
                "Failing head: {head_sha} (conclusion: {}).",
                if conclusion.is_empty() {
                    "unknown"
                } else {
                    &conclusion
                }
            ),
        ];
        if !display_title.is_empty() {
            notes_lines.push(format!("Run title: {display_title}"));
        }
        if !event.is_empty() {
            notes_lines.push(format!("Triggering event: {event}"));
        }
        let context = Context {
            references: if url.is_empty() {
                Vec::new()
            } else {
                vec![url.clone()]
            },
            notes: notes_lines.join("\n"),
            ..Context::default()
        };

        let task = Task {
            id: item_id.clone(),
            goal: format!("Fix the failing CI workflow '{workflow}' on branch '{branch}'."),
            graders,
            tags: Vec::new(),
            context,
            budgets: Default::default(),
        };
        task.validate()
            .map_err(|exc| WorkSourceError::new(format!("{item_id}: {exc}")))?;

        Ok(Some(WorkItem {
            task,
            prerequisites: Vec::new(),
            source_ref: format!("{}@{head_sha}", self.repo),
            local_path: None,
            phase: None,
            source_kind: Some(GITHUB_CI_SOURCE_KIND.to_string()),
            source_version: Some(sha256_hex(&format!("{head_sha}\u{0}{conclusion}"))),
            source_url: Some(url),
            priority: 0,
            required_capabilities: Default::default(),
            conflict_keys: Default::default(),
        }))
    }
}

/// Tracks the rows a listing dropped: the count feeds
/// `last_skipped_count`; the first error is held so an all-dropped page
/// re-raises rather than reading as "CI is green".
struct Skips {
    count: usize,
    first: Option<WorkSourceError>,
}

impl Skips {
    fn record(&mut self, source: &GithubCiWorkSource, error: WorkSourceError) {
        self.count += 1;
        source.log(&format!("[github_ci] skipping malformed run -- {error}"));
        if self.first.is_none() {
            self.first = Some(error);
        }
    }
}

impl WorkSource for GithubCiWorkSource {
    fn source_kind(&self) -> &str {
        GITHUB_CI_SOURCE_KIND
    }

    /// The source's locus for `source_syncs` (D-6): the `owner/repo`.
    fn source_name(&self) -> String {
        self.repo.clone()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let limit = LIST_LIMIT.to_string();
        let stdout = run_gh(
            &*self.runner,
            &[
                "run",
                "list",
                "--repo",
                &self.repo,
                "--status",
                &self.failure_filter,
                "--json",
                RUN_LIST_FIELDS,
                "--limit",
                &limit,
            ],
        )?;
        let runs = parse_list_payload(&stdout, "gh run list")?;

        // A page filled to the cap may have dropped overflow runs: gh's
        // REST `run list` exposes no `hasNextPage`, so a full page is the
        // only available truncation evidence. Measured on the raw page,
        // before the (workflow, branch) dedup collapses it.
        if runs.len() == LIST_LIMIT {
            emit_truncation_warning(self.log.as_ref(), "github_ci", "run");
            self.stop(
                STOP_SOURCE_TRUNCATION,
                &format!(
                    "run listing truncated at one page ({LIST_LIMIT} runs); \
                     some runs were not read this pass"
                ),
            );
        }

        let mut skips = Skips {
            count: 0,
            first: None,
        };

        // Dedup to one item per (workflow, branch), keeping the most recent
        // run (D-3): multiple failed runs of the same workflow on the same
        // branch are one persistent failure, not several work items. The
        // ordered map yields items sorted by (workflow, branch).
        let mut latest: BTreeMap<(String, String), &Map<String, Value>> = BTreeMap::new();
        for run in &runs {
            let Some(obj) = run.as_object() else {
                skips.record(
                    self,
                    WorkSourceError::new(format!(
                        "gh run list entry is {}, expected an object: {run}",
                        py_type_name(run)
                    )),
                );
                continue;
            };
            let identity = required_str(obj, "workflowName").and_then(|workflow| {
                let branch = required_str(obj, "headBranch")?;
                required_str(obj, "headSha")?;
                Ok((workflow, branch))
            });
            let key = match identity {
                Ok(key) => key,
                Err(exc) => {
                    skips.record(self, exc);
                    continue;
                }
            };
            let created = str_or_empty(obj, "createdAt");
            let newer = match latest.get(&key) {
                None => true,
                Some(prior) => created >= str_or_empty(prior, "createdAt"),
            };
            if newer {
                latest.insert(key, obj);
            }
        }

        let mut items = Vec::new();
        for ((workflow, branch), run) in &latest {
            match self.compile_run(run, workflow, branch) {
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(exc) => skips.record(self, exc),
            }
        }

        self.last_skipped_count.store(skips.count, Ordering::SeqCst);

        // A parse break must never masquerade as "CI is green": if the page
        // held rows yet nothing compiled because rows were dropped, surface
        // the first skip's error rather than returning an empty list.
        if items.is_empty()
            && let Some(first) = skips.first
        {
            return Err(first);
        }
        Ok(items)
    }

    /// Post the run's grader receipts as a comment on the failing commit.
    ///
    /// A CI run is not an issue to close (D-5): the audit trail goes onto
    /// the commit the failure was listed against, parsed back out of the
    /// item's `source_ref` (`owner/repo@<sha>`). No issue is mutated and no
    /// check status is flipped; the grade stays the harness's out-of-band
    /// graders.
    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError> {
        let (repo, head_sha) = match report.source_ref.rsplit_once('@') {
            Some((repo, sha)) if !repo.is_empty() && !sha.is_empty() => (repo, sha),
            _ => {
                return Err(WorkSourceError::new(format!(
                    "malformed github_ci source_ref {:?}; expected 'owner/repo@<sha>'",
                    report.source_ref
                )));
            }
        };
        let body = format_report_body(report);
        run_gh(
            &*self.runner,
            &[
                "api",
                "--method",
                "POST",
                &format!("repos/{repo}/commits/{head_sha}/comments"),
                "-f",
                &format!("body={body}"),
            ],
        )?;
        Ok(())
    }
}

/// The factory behind the `github_ci` kind: `[source] repo` is required,
/// `failure_filter` defaults to `failure`; the context's default graders
/// and sinks are wired in.
pub fn build_github_ci_source(
    context: &SourceBuildContext,
) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
    let repo = context.required_string(GITHUB_CI_KIND, "repo")?;
    let failure_filter = optional_string(
        context,
        GITHUB_CI_KIND,
        "failure_filter",
        DEFAULT_FAILURE_FILTER,
    )?;
    let mut source = GithubCiWorkSource::new(repo)
        .and_then(|source| source.with_failure_filter(failure_filter))
        .map_err(|err| build_error(GITHUB_CI_KIND, err))?
        .with_default_graders(context.default_graders.clone());
    if let Some(log) = &context.log {
        source = source.with_log(Arc::clone(log));
    }
    if let Some(sink) = &context.stop_sink {
        source = source.with_stop_sink(Arc::clone(sink));
    }
    Ok(Box::new(source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::test_support::*;
    use crate::source_registry::default_registry;
    use crate::sources::GraderReceipt;
    use flywheel_core::lifecycle::Status;
    use serde_json::json;

    struct RunSpec {
        workflow: &'static str,
        branch: &'static str,
        head_sha: String,
        conclusion: &'static str,
        url: &'static str,
        created_at: &'static str,
    }

    impl Default for RunSpec {
        fn default() -> Self {
            Self {
                workflow: "CI",
                branch: "main",
                head_sha: "abc123".to_string(),
                conclusion: "failure",
                url: "https://github.com/octo/widgets/actions/runs/1",
                created_at: "2026-06-25T00:00:00Z",
            }
        }
    }

    fn run(spec: RunSpec) -> Value {
        json!({
            "workflowName": spec.workflow,
            "headBranch": spec.branch,
            "headSha": spec.head_sha,
            "conclusion": spec.conclusion,
            "url": spec.url,
            "databaseId": 1,
            "createdAt": spec.created_at,
            "displayTitle": "Fix the widget",
            "event": "push",
        })
    }

    fn run_on(workflow: &'static str, branch: &'static str) -> Value {
        run(RunSpec {
            workflow,
            branch,
            ..RunSpec::default()
        })
    }

    fn without(mut value: Value, key: &str) -> Value {
        value.as_object_mut().unwrap().remove(key);
        value
    }

    fn source(gh: &Arc<FakeGh>) -> GithubCiWorkSource {
        GithubCiWorkSource::new("octo/widgets")
            .unwrap()
            .with_default_graders(vec![command_grader("uv run pytest")])
            .with_runner(Arc::clone(gh) as Arc<dyn GhRunner>)
    }

    fn page(runs: Vec<Value>) -> String {
        Value::Array(runs).to_string()
    }

    fn goals(items: &[WorkItem]) -> Vec<&str> {
        items.iter().map(|i| i.task.goal.as_str()).collect()
    }

    // --- inbound: list_work ------------------------------------------------

    #[test]
    fn list_work_queries_failed_runs() {
        let gh = FakeGh::new("[]");
        assert!(source(&gh).list_work().unwrap().is_empty());
        let argv = gh.only_call();
        assert_eq!(&argv[..2], ["run", "list"]);
        assert_eq!(arg_after(&argv, "--repo"), "octo/widgets");
        assert_eq!(arg_after(&argv, "--status"), "failure");
        assert_eq!(arg_after(&argv, "--json"), RUN_LIST_FIELDS);
        assert_eq!(arg_after(&argv, "--limit"), "200");
    }

    #[test]
    fn failure_filter_is_configurable() {
        let gh = FakeGh::new("[]");
        source(&gh)
            .with_failure_filter("timed_out")
            .unwrap()
            .list_work()
            .unwrap();
        assert_eq!(arg_after(&gh.only_call(), "--status"), "timed_out");
        assert!(source(&gh).with_failure_filter("").is_err());
    }

    #[test]
    fn one_item_per_failed_run() {
        let gh = FakeGh::new(page(vec![run_on("CI", "main"), run_on("Lint", "main")]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items.len(), 2);
        let goals = goals(&items);
        assert!(goals.iter().any(|g| g.contains("'CI'")));
        assert!(goals.iter().any(|g| g.contains("'Lint'")));
    }

    #[test]
    fn default_graders_become_the_task_graders_and_context_is_filled() {
        let grader = command_grader("uv run pytest");
        let gh = FakeGh::new(page(vec![run(RunSpec::default())]));
        let items = source(&gh)
            .with_default_graders(vec![grader.clone()])
            .list_work()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].task.graders, vec![grader]);
        assert_eq!(
            items[0].task.goal,
            "Fix the failing CI workflow 'CI' on branch 'main'."
        );
        assert_eq!(
            items[0].task.context.notes,
            "CI workflow 'CI' is failing on branch 'main'.\n\
             Failing head: abc123 (conclusion: failure).\n\
             Run title: Fix the widget\n\
             Triggering event: push"
        );
        assert_eq!(
            items[0].task.context.references,
            ["https://github.com/octo/widgets/actions/runs/1"]
        );
        assert_eq!(items[0].source_ref, "octo/widgets@abc123");
        assert_eq!(items[0].task.id, ci_item_id("CI", "main"));
        assert!(items[0].task.id.starts_with("ci-"));
        assert_eq!(items[0].task.id.len(), 19);
    }

    #[test]
    fn run_without_default_graders_is_skipped_and_logged_and_not_counted() {
        let gh = FakeGh::new(page(vec![run_on("CI", "main")]));
        let (log, lines) = capture_log();
        let (sink, events) = capture_stop_events();
        let src = source(&gh)
            .with_default_graders(vec![])
            .with_log(log)
            .with_stop_sink(sink);
        assert!(src.list_work().unwrap().is_empty());
        assert_eq!(src.last_skipped_count(), 0);
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("not runnable"));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "zero-grader-drop");
        assert_eq!(events[0].1, "octo/widgets");
        assert!(events[0].2.contains("CI on main"));
    }

    #[test]
    fn stable_id_across_two_passes_for_same_failure() {
        let first = FakeGh::new(page(vec![run(RunSpec {
            head_sha: "aaa".into(),
            ..RunSpec::default()
        })]));
        let second = FakeGh::new(page(vec![run(RunSpec {
            head_sha: "bbb".into(),
            ..RunSpec::default()
        })]));
        let item1 = source(&first).list_work().unwrap().remove(0);
        let item2 = source(&second).list_work().unwrap().remove(0);
        assert_eq!(item1.task.id, item2.task.id);
        assert_ne!(item1.source_version, item2.source_version);
    }

    #[test]
    fn different_workflow_or_branch_gives_distinct_ids() {
        let gh = FakeGh::new(page(vec![run_on("CI", "main"), run_on("CI", "dev")]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items.len(), 2);
        assert_ne!(items[0].task.id, items[1].task.id);
        // Sorted by (workflow, branch): dev before main.
        assert_eq!(items[0].source_ref, items[1].source_ref);
        assert!(items[0].task.goal.contains("'dev'"));
    }

    #[test]
    fn duplicate_runs_collapse_to_one_item_keeping_the_most_recent() {
        let gh = FakeGh::new(page(vec![
            run(RunSpec {
                head_sha: "old".into(),
                created_at: "2026-06-24T00:00:00Z",
                ..RunSpec::default()
            }),
            run(RunSpec {
                head_sha: "new".into(),
                created_at: "2026-06-25T00:00:00Z",
                ..RunSpec::default()
            }),
        ]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items.len(), 1);
        let newest = source(&FakeGh::new(page(vec![run(RunSpec {
            head_sha: "new".into(),
            ..RunSpec::default()
        })])))
        .list_work()
        .unwrap();
        assert_eq!(items[0].source_version, newest[0].source_version);
        assert_eq!(items[0].source_ref, "octo/widgets@new");
    }

    #[test]
    fn provenance_stamps() {
        let gh = FakeGh::new(page(vec![run(RunSpec {
            url: "https://github.com/octo/widgets/actions/runs/42",
            ..RunSpec::default()
        })]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items[0].source_kind.as_deref(), Some("github_ci"));
        assert_eq!(
            items[0].source_url.as_deref(),
            Some("https://github.com/octo/widgets/actions/runs/42")
        );
        assert_eq!(
            items[0].source_version.as_deref(),
            Some(sha256_hex("abc123\u{0}failure").as_str())
        );
    }

    fn distinct_runs(count: usize) -> Vec<Value> {
        (0..count)
            .map(|n| {
                json!({
                    "workflowName": format!("CI-{n}"),
                    "headBranch": format!("b-{n}"),
                    "headSha": format!("sha-{n}"),
                    "conclusion": "failure",
                    "url": "https://github.com/octo/widgets/actions/runs/1",
                    "databaseId": 1,
                    "createdAt": "2026-06-25T00:00:00Z",
                    "displayTitle": "Fix the widget",
                    "event": "push",
                })
            })
            .collect()
    }

    #[test]
    fn full_page_emits_one_truncation_warning_and_stop_event() {
        let gh = FakeGh::new(page(distinct_runs(200)));
        let (log, lines) = capture_log();
        let (sink, events) = capture_stop_events();
        let items = source(&gh)
            .with_log(log)
            .with_stop_sink(sink)
            .list_work()
            .unwrap();
        assert_eq!(items.len(), 200);
        let lines = lines.lock().unwrap();
        let warnings: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("truncated at one page"))
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("[github_ci]"));
        assert!(warnings[0].contains("some items were not read this pass"));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "source-truncation");
        assert!(
            events[0]
                .2
                .contains("run listing truncated at one page (200 runs)")
        );
    }

    #[test]
    fn below_cap_emits_no_truncation_warning() {
        let gh = FakeGh::new(page(distinct_runs(199)));
        let (log, lines) = capture_log();
        source(&gh).with_log(log).list_work().unwrap();
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .all(|l| !l.contains("truncated"))
        );
    }

    #[test]
    fn truncation_warning_is_a_side_channel() {
        let page = page(distinct_runs(200));
        let (log, _lines) = capture_log();
        let with_log = source(&FakeGh::new(page.clone()))
            .with_log(log)
            .list_work()
            .unwrap();
        let without = source(&FakeGh::new(page)).list_work().unwrap();
        let ids = |items: &[WorkItem]| items.iter().map(|i| i.task.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&with_log), ids(&without));
        assert_eq!(without.len(), 200);
    }

    #[test]
    fn failed_listing_raises_and_never_returns_empty() {
        let gh = FakeGh::failing("boom");
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("gh run list"), "{err}");
        assert!(err.contains("failed (exit 1): boom"), "{err}");
    }

    #[test]
    fn invalid_and_non_list_payloads_raise() {
        let err = source(&FakeGh::new("not json at all"))
            .list_work()
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh run list returned invalid JSON"), "{err}");
        let err = source(&FakeGh::new(json!({"unexpected": "object"}).to_string()))
            .list_work()
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected a list"), "{err}");
    }

    #[test]
    fn run_missing_identity_field_raises_when_nothing_else_compiles() {
        let gh = FakeGh::new(page(vec![without(run(RunSpec::default()), "headSha")]));
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("headSha"), "{err}");
        assert!(
            err.starts_with("gh run list payload missing string"),
            "{err}"
        );
    }

    // --- skip-bad-item -------------------------------------------------------

    #[test]
    fn one_bad_row_is_skipped_and_the_valid_runs_still_surface() {
        let bad = without(run_on("Broken", "main"), "headSha");
        let gh = FakeGh::new(page(vec![
            run_on("CI", "main"),
            bad,
            run_on("Lint", "main"),
        ]));
        let (log, lines) = capture_log();
        let src = source(&gh).with_log(log);
        let items = src.list_work().unwrap();
        assert_eq!(items.len(), 2);
        let goals = goals(&items);
        assert!(goals.iter().any(|g| g.contains("'CI'")));
        assert!(goals.iter().any(|g| g.contains("'Lint'")));
        assert!(!goals.iter().any(|g| g.contains("'Broken'")));
        assert_eq!(src.last_skipped_count(), 1);
        let lines = lines.lock().unwrap();
        let skips: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("skipping malformed run"))
            .collect();
        assert_eq!(skips.len(), 1);
        assert!(skips[0].contains("headSha"));
        assert!(skips[0].contains("[github_ci]"));
    }

    #[test]
    fn non_object_row_is_skipped_not_fatal() {
        let gh = FakeGh::new(page(vec![run_on("CI", "main"), json!("not-an-object")]));
        let src = source(&gh);
        assert_eq!(src.list_work().unwrap().len(), 1);
        assert_eq!(src.last_skipped_count(), 1);
    }

    #[test]
    fn count_resets_to_zero_on_a_clean_pass() {
        let bad = without(run_on("Broken", "main"), "headSha");
        let dirty = FakeGh::new(page(vec![run_on("CI", "main"), bad]));
        let src = source(&dirty);
        src.list_work().unwrap();
        assert_eq!(src.last_skipped_count(), 1);

        let clean = FakeGh::new(page(vec![run_on("CI", "main")]));
        let src = src.with_runner(clean as Arc<dyn GhRunner>);
        src.list_work().unwrap();
        assert_eq!(src.last_skipped_count(), 0);
    }

    #[test]
    fn all_rows_dropped_reraises_so_a_parse_break_is_not_green() {
        let gh = FakeGh::new(page(vec![
            without(run_on("CI", "main"), "headSha"),
            without(run_on("Lint", "main"), "workflowName"),
        ]));
        let src = source(&gh);
        let err = src.list_work().unwrap_err().to_string();
        assert!(err.contains("headSha"), "{err}");
        assert_eq!(src.last_skipped_count(), 2);
    }

    #[test]
    fn no_grader_skip_is_not_counted_as_a_parse_break() {
        let gh = FakeGh::new(page(vec![run_on("CI", "main")]));
        let src = source(&gh).with_default_graders(vec![]);
        assert!(src.list_work().unwrap().is_empty());
        assert_eq!(src.last_skipped_count(), 0);
    }

    // --- outbound: report ------------------------------------------------------

    fn report(source_ref: &str) -> WorkReport {
        WorkReport {
            task_id: "ci-x".into(),
            source_ref: source_ref.into(),
            run_id: "run-abc".into(),
            status: Status::Done,
            error: String::new(),
            graders: vec![GraderReceipt {
                ordinal: 0,
                grader_type: flywheel_core::task::GraderType::Command,
                name: Some("tests".into()),
                passed: true,
            }],
        }
    }

    #[test]
    fn report_comments_on_failing_commit() {
        let gh = FakeGh::new("");
        source(&gh).report(&report("octo/widgets@abc123")).unwrap();
        let argv = gh.only_call();
        assert_eq!(argv[0], "api");
        assert_eq!(&argv[1..3], ["--method", "POST"]);
        assert!(argv.contains(&"repos/octo/widgets/commits/abc123/comments".to_string()));
        assert!(!argv.iter().any(|a| a == "issue"));
        let body = arg_after(&argv, "-f");
        assert!(body.starts_with("body="));
        assert!(body.contains("run `run-abc`"));
        assert!(body.contains("| 0 | tests | command | yes |"));
    }

    #[test]
    fn report_malformed_source_ref_raises() {
        let gh = FakeGh::new("");
        for bad in ["no-at-sign", "@abc", "octo/widgets@"] {
            let err = source(&gh).report(&report(bad)).unwrap_err().to_string();
            assert!(err.contains("malformed github_ci source_ref"), "{err}");
        }
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn construction_validates_repo() {
        assert!(GithubCiWorkSource::new("").is_err());
        let src = GithubCiWorkSource::new("octo/widgets").unwrap();
        assert_eq!(src.source_kind(), "github_ci");
        assert_eq!(src.source_name(), "octo/widgets");
        assert_eq!(src.repo(), "octo/widgets");
        assert_eq!(src.failure_filter(), "failure");
        assert!(src.default_graders().is_empty());
        assert!(format!("{src:?}").contains("failure"));
    }

    // --- registry wiring ---------------------------------------------------------

    #[test]
    fn github_ci_factory_reads_settings() {
        let mut registry = default_registry();
        crate::github::register_github_sources(&mut registry);
        let mut settings = toml::Table::new();
        settings.insert("kind".into(), toml::Value::String("github_ci".into()));
        settings.insert("repo".into(), toml::Value::String("octo/widgets".into()));
        settings.insert(
            "failure_filter".into(),
            toml::Value::String("timed_out".into()),
        );
        let built = registry
            .build_from_context(&SourceBuildContext::new(settings.clone()))
            .unwrap();
        assert_eq!(built.source_kind(), "github_ci");
        assert_eq!(built.source_name(), "octo/widgets");

        settings.remove("repo");
        let err = registry
            .build("github_ci", &SourceBuildContext::new(settings.clone()))
            .err()
            .expect("build must fail")
            .to_string();
        assert_eq!(
            err,
            "cannot build github_ci source: [source] repo is required"
        );

        settings.insert("repo".into(), toml::Value::String("octo/widgets".into()));
        settings.insert("failure_filter".into(), toml::Value::Integer(1));
        let err = registry
            .build("github_ci", &SourceBuildContext::new(settings.clone()))
            .err()
            .expect("build must fail")
            .to_string();
        assert!(
            err.contains("failure_filter must be a string, got integer"),
            "{err}"
        );

        settings.insert("failure_filter".into(), toml::Value::String(String::new()));
        let err = registry
            .build("github_ci", &SourceBuildContext::new(settings))
            .err()
            .expect("build must fail");
        assert!(matches!(err, SourceRegistryError::Build { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("failure_filter must be a non-empty")
        );
    }
}
