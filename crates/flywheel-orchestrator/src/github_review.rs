//! GitHub PR review-thread work source: a reviewer's request becomes work.
//!
//! A third GitHub adapter, distinct from [`crate::github`] (labeled
//! issues) and [`crate::github_ci`] (failed CI runs). This one lists a
//! repo's open pull requests' UNRESOLVED review threads through the same
//! injectable [`GhRunner`] seam, but via `gh api graphql`, because the
//! `isResolved` thread state the candidate filter depends on is
//! GraphQL-only (spec 00053 D-2), and compiles each unresolved thread into
//! a [`WorkItem`] the orchestrator drives an agent to address. The three
//! adapters never mix: issues, CI failures, and review threads have
//! different listing, keying, and write-back, so each lives behind its own
//! `[source] kind` selector (D-1).
//!
//! Thread -> Task compilation:
//!
//! * `id` is `prc-<digest(thread_node_id)>`: STABLE for a given review
//!   thread across polls and across appended replies (D-3), so one thread
//!   is ONE work item, not a fresh one every time a reviewer comments. It
//!   is NOT keyed on a comment id or comment count (those advance on every
//!   reply, which would re-queue the same concern endlessly).
//! * `graders` come solely from the operator's policy
//!   `[[defaults.graders]]`, NEVER from the thread's `isResolved` state and
//!   never compiled from the review comment text (D-4). A review comment is
//!   subjective and not itself a check, so a thread that resolves to ZERO
//!   graders is **not runnable**: it is skipped and logged rather than run
//!   ungated or graded by a fabricated rubric. Resolution status is read
//!   ONLY to select candidate threads; it is never the verdict.
//! * `source_version` digests the thread's comment bodies + `isResolved` +
//!   `isOutdated` + latest comment timestamp, so a new reply changes the
//!   change-token even though the item id is stable.
//! * `source_url` locates the thread; `source_kind` marks the review
//!   provenance; `source_ref` encodes `{repo}#{pr}#{thread_id}` so the
//!   report path can recover both the PR (to post on) and the thread (to
//!   reference) (D-6).
//!
//! Malformed `gh` output is read at two grains. A broken top-level
//! envelope (invalid JSON, a non-object payload, a thread missing its node
//! id so it cannot even be named, or a non-zero `gh` exit) fails the
//! listing rather than returning an empty list: a parse break or auth
//! failure must never masquerade as "every thread is resolved". But a
//! single *identifiable* thread node (its node id is in hand) that fails
//! to compile (malformed comments substructure, an invalid compiled task)
//! is dropped one at a time: it is counted and logged by node id, and the
//! surviving valid threads on the page still return, so one bad node never
//! blanks the whole listing.
//!
//! Outbound: [`GithubReviewWorkSource::report`] posts the run's grader
//! receipts as a PR comment via `gh pr comment` (the PR parsed back out of
//! `source_ref`). It issues NO resolve/reply mutation: the harness never
//! resolves a thread, so the exact state that must stay untrusted cannot
//! be flipped by the report path (D-5).
//!
//! Authoritative spec: `docs/work-sources.md` (`github_review`).

use crate::github::{
    GhRunner, ProcessGhRunner, STOP_SOURCE_TRUNCATION, STOP_ZERO_GRADER_DROP, build_error,
    emit_truncation_warning, format_report_body, py_str, py_truthy, py_type_name, run_gh,
    sha256_hex, str_or_empty,
};
use crate::source_registry::{GITHUB_REVIEW_KIND, SourceBuildContext, SourceRegistryError};
use crate::sources::{LogSink, StopEventSink, WorkItem, WorkReport, WorkSource, WorkSourceError};
use flywheel_core::task::{Context, Grader, Task};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Provenance `source_kind` for emitted items and `source_syncs` rows.
pub const GITHUB_REVIEW_SOURCE_KIND: &str = "github_review";

/// One bounded page per axis. A truncated page is LOGGED (and recorded on
/// the stop ledger), never read as "no work".
pub const PR_PAGE: usize = 50;
pub const THREAD_PAGE: usize = 50;
pub const COMMENT_PAGE: usize = 50;

/// The single GraphQL query: open PRs and their review threads, with each
/// thread's comments (body, author, timestamp, url). `pageInfo.hasNextPage`
/// on every connection lets the adapter detect, and log, truncation.
pub fn graphql_query() -> String {
    format!(
        "query($owner: String!, $name: String!) {{
  repository(owner: $owner, name: $name) {{
    pullRequests(
      states: OPEN
      first: {PR_PAGE}
      orderBy: {{field: UPDATED_AT, direction: DESC}}
    ) {{
      nodes {{
        number
        reviewThreads(first: {THREAD_PAGE}) {{
          nodes {{
            id
            isResolved
            isOutdated
            comments(first: {COMMENT_PAGE}) {{
              nodes {{
                body
                createdAt
                url
                author {{ login }}
              }}
              pageInfo {{ hasNextPage }}
            }}
          }}
          pageInfo {{ hasNextPage }}
        }}
      }}
      pageInfo {{ hasNextPage }}
    }}
  }}
}}"
    )
}

/// Stable item id for a review thread, keyed on its GraphQL node id (D-3).
///
/// A digest keeps the id well-formed regardless of the node id's
/// characters and stable across polls and appended replies, so one thread
/// is one item.
pub fn review_item_id(thread_node_id: &str) -> String {
    format!("prc-{}", &sha256_hex(thread_node_id)[..16])
}

/// Read a value that must be a JSON object or fail with the branded error.
///
/// A missing or wrong-typed node in the GraphQL response is malformed
/// output, not "no work": surfacing it keeps a parse break from reading as
/// a repo with no unresolved threads.
fn require_object<'a>(
    value: Option<&'a Value>,
    what: &str,
) -> Result<&'a Map<String, Value>, WorkSourceError> {
    match value {
        Some(Value::Object(map)) => Ok(map),
        other => Err(WorkSourceError::new(format!(
            "gh api graphql payload {what} is {}, expected an object",
            py_type_name(other.unwrap_or(&Value::Null))
        ))),
    }
}

/// Read a GraphQL connection's `nodes` list or fail with the branded error.
fn nodes<'a>(
    connection: &'a Map<String, Value>,
    what: &str,
) -> Result<&'a [Value], WorkSourceError> {
    match connection.get("nodes") {
        Some(Value::Array(nodes)) => Ok(nodes),
        other => Err(WorkSourceError::new(format!(
            "gh api graphql payload {what}.nodes is {}, expected a list",
            py_type_name(other.unwrap_or(&Value::Null))
        ))),
    }
}

/// Whether a GraphQL connection has an unread next page.
fn truncated(connection: &Map<String, Value>) -> bool {
    connection
        .get("pageInfo")
        .and_then(Value::as_object)
        .is_some_and(|page| py_truthy(page.get("hasNextPage")))
}

/// `owner/repo#<pr>#<thread_id>` -> `(repo, pr, thread_id)` (D-6).
fn parse_review_source_ref(source_ref: &str) -> Result<(&str, &str, &str), WorkSourceError> {
    let parts: Vec<&str> = source_ref.split('#').collect();
    match parts.as_slice() {
        [repo, pr, thread]
            if !repo.is_empty()
                && !pr.is_empty()
                && pr.bytes().all(|b| b.is_ascii_digit())
                && !thread.is_empty() =>
        {
            Ok((repo, pr, thread))
        }
        _ => Err(WorkSourceError::new(format!(
            "malformed github_review source_ref {source_ref:?}; \
             expected 'owner/repo#<pr_number>#<thread_node_id>'"
        ))),
    }
}

/// Unresolved PR review threads in one GitHub repo, compiled to tasks.
///
/// `runner` is the `gh` subprocess seam: production uses the real CLI,
/// tests inject a scripted runner returning canned stdout. `log` (when
/// provided) receives one line per skipped thread (no default grader
/// policy, or a malformed identifiable node) and one line when a listing
/// page truncates, so non-runnable threads and dropped work are visible
/// rather than silently absent.
pub struct GithubReviewWorkSource {
    repo: String,
    default_graders: Vec<Grader>,
    runner: Arc<dyn GhRunner>,
    log: Option<LogSink>,
    stop_sink: Option<StopEventSink>,
    last_skipped_count: AtomicUsize,
}

impl fmt::Debug for GithubReviewWorkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubReviewWorkSource")
            .field("repo", &self.repo)
            .field("default_graders", &self.default_graders.len())
            .field("last_skipped_count", &self.last_skipped_count())
            .finish()
    }
}

impl GithubReviewWorkSource {
    /// A source over `repo`'s unresolved review threads, driven by the
    /// real `gh` binary. `repo` must be a non-empty `owner/repo`.
    pub fn new(repo: impl Into<String>) -> Result<Self, WorkSourceError> {
        let repo = repo.into();
        if repo.is_empty() || !repo.contains('/') {
            return Err(WorkSourceError::new(
                "repo must be a non-empty 'owner/repo' string",
            ));
        }
        Ok(Self {
            repo,
            default_graders: Vec::new(),
            runner: Arc::new(ProcessGhRunner),
            log: None,
            stop_sink: None,
            last_skipped_count: AtomicUsize::new(0),
        })
    }

    /// The `[[defaults.graders]]` every review item is graded by.
    pub fn with_default_graders(mut self, graders: Vec<Grader>) -> Self {
        self.default_graders = graders;
        self
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

    pub fn default_graders(&self) -> &[Grader] {
        &self.default_graders
    }

    /// Number of identifiable thread nodes the most recent
    /// [`WorkSource::list_work`] dropped because they could not be
    /// compiled. Reset by every listing; the no-grader gate is not counted.
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

    /// Log a one-page truncation and record one durable stop event.
    ///
    /// Both are pure side channels: the returned work sequence is
    /// unchanged whether or not a sink is wired.
    fn note_truncation(&self, what: &str) {
        emit_truncation_warning(self.log.as_ref(), "github_review", what);
        self.stop(
            STOP_SOURCE_TRUNCATION,
            &format!("{what} listing truncated at one page; some items were not read this pass"),
        );
    }

    fn compile_identified_thread(
        &self,
        thread: &Map<String, Value>,
        node_id: &str,
        pr_number: i64,
    ) -> Result<Option<WorkItem>, WorkSourceError> {
        // Resolution state is the candidate FILTER only, never the verdict
        // (D-4): skip resolved threads, but do not derive a grade from it.
        if py_truthy(thread.get("isResolved")) {
            return Ok(None);
        }
        let is_outdated = py_truthy(thread.get("isOutdated"));
        let comments_conn = require_object(thread.get("comments"), "reviewThread.comments")?;
        if truncated(comments_conn) {
            self.note_truncation(&format!("comments on a review thread on PR #{pr_number}"));
        }
        let comments = nodes(comments_conn, "reviewThread.comments")?;

        let mut bodies = Vec::new();
        let mut authors = Vec::new();
        let mut timestamps = Vec::new();
        let mut thread_url = String::new();
        for comment in comments {
            let comment = require_object(Some(comment), "reviewThread.comment")?;
            let login = comment
                .get("author")
                .and_then(Value::as_object)
                .map(|author| str_or_empty(author, "login"))
                .filter(|login| !login.is_empty())
                .unwrap_or_else(|| "(ghost)".to_string());
            bodies.push(str_or_empty(comment, "body"));
            authors.push(login);
            timestamps.push(str_or_empty(comment, "createdAt"));
            if thread_url.is_empty() {
                thread_url = str_or_empty(comment, "url");
            }
        }

        let item_id = review_item_id(node_id);

        let graders = self.default_graders.clone();
        if graders.is_empty() {
            let detail = format!(
                "{item_id} (thread on PR #{pr_number}): no default grader policy -- not runnable"
            );
            self.log(&format!("[github_review] skipping {detail}"));
            self.stop(STOP_ZERO_GRADER_DROP, &detail);
            return Ok(None);
        }

        let mut notes_lines = vec![
            format!(
                "Unresolved review thread on pull request #{pr_number} in {}{}",
                self.repo,
                if is_outdated {
                    " (the diff it annotates is outdated)."
                } else {
                    "."
                }
            ),
            String::new(),
            "Reviewer thread (most recent last):".to_string(),
        ];
        for (login, body) in authors.iter().zip(&bodies) {
            notes_lines.push(format!("{login}: {body}"));
        }
        let context = Context {
            references: if thread_url.is_empty() {
                Vec::new()
            } else {
                vec![thread_url.clone()]
            },
            notes: notes_lines.join("\n"),
            ..Context::default()
        };

        let task = Task {
            id: item_id.clone(),
            goal: format!(
                "Address the unresolved review thread on pull request #{pr_number} in {}.",
                self.repo
            ),
            graders,
            tags: Vec::new(),
            context,
            budgets: Default::default(),
        };
        task.validate()
            .map_err(|exc| WorkSourceError::new(format!("{item_id}: {exc}")))?;

        let latest_ts = timestamps.iter().max().cloned().unwrap_or_default();
        let resolved_text = py_str(thread.get("isResolved").unwrap_or(&Value::Null));
        let outdated_text = if is_outdated { "True" } else { "False" };
        let mut seed_parts = bodies.clone();
        seed_parts.push(resolved_text);
        seed_parts.push(outdated_text.to_string());
        seed_parts.push(latest_ts);
        let version_seed = seed_parts.join("\u{0}");

        Ok(Some(WorkItem {
            task,
            prerequisites: Vec::new(),
            source_ref: format!("{}#{pr_number}#{node_id}", self.repo),
            local_path: None,
            phase: None,
            source_kind: Some(GITHUB_REVIEW_SOURCE_KIND.to_string()),
            source_version: Some(sha256_hex(&version_seed)),
            source_url: Some(thread_url),
            priority: 0,
            required_capabilities: Default::default(),
            conflict_keys: Default::default(),
        }))
    }
}

impl WorkSource for GithubReviewWorkSource {
    fn source_kind(&self) -> &str {
        GITHUB_REVIEW_SOURCE_KIND
    }

    /// The source's locus for `source_syncs` (D-6): the `owner/repo`.
    fn source_name(&self) -> String {
        self.repo.clone()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let (owner, name) = self.repo.split_once('/').unwrap_or((&self.repo, ""));
        let query = format!("query={}", graphql_query());
        let owner_arg = format!("owner={owner}");
        let name_arg = format!("name={name}");
        let stdout = run_gh(
            &*self.runner,
            &[
                "api", "graphql", "-f", &query, "-F", &owner_arg, "-F", &name_arg,
            ],
        )?;
        let text = if stdout.trim().is_empty() {
            "{}"
        } else {
            &stdout
        };
        let payload: Value = serde_json::from_str(text).map_err(|err| {
            WorkSourceError::new(format!("gh api graphql returned invalid JSON: {err}"))
        })?;

        let root = require_object(Some(&payload), "root")?;
        let data = require_object(root.get("data"), "data")?;
        let repository = require_object(data.get("repository"), "data.repository")?;
        let pull_requests = require_object(
            repository.get("pullRequests"),
            "data.repository.pullRequests",
        )?;
        let pr_nodes = nodes(pull_requests, "pullRequests")?;
        if truncated(pull_requests) {
            self.note_truncation("open pull requests");
        }

        let mut items = Vec::new();
        let mut skipped = 0usize;
        for pr in pr_nodes {
            let pr_obj = require_object(Some(pr), "pullRequest")?;
            let Some(number) = pr_obj.get("number").and_then(Value::as_i64) else {
                return Err(WorkSourceError::new(format!(
                    "gh api graphql pull request missing integer 'number': {pr}"
                )));
            };
            let what = format!("pullRequest(#{number}).reviewThreads");
            let review_threads = require_object(pr_obj.get("reviewThreads"), &what)?;
            if truncated(review_threads) {
                self.note_truncation(&format!("review threads on PR #{number}"));
            }
            for thread in nodes(review_threads, &what)? {
                let thread_obj = require_object(Some(thread), "reviewThread")?;
                let node_id = match thread_obj.get("id") {
                    Some(Value::String(id)) if !id.is_empty() => id,
                    _ => {
                        // No stable id means the node cannot be named or
                        // keyed: an identity break, not a skippable node.
                        return Err(WorkSourceError::new(format!(
                            "gh api graphql review thread missing string 'id': {thread}"
                        )));
                    }
                };
                // Past this point the node is identifiable, so any compile
                // failure is demoted to a per-node skip naming the thread,
                // leaving the surviving valid threads intact.
                match self.compile_identified_thread(thread_obj, node_id, number) {
                    Ok(Some(item)) => items.push(item),
                    Ok(None) => {}
                    Err(exc) => {
                        skipped += 1;
                        self.log(&format!(
                            "[github_review] skipping malformed review thread node: \
                             review thread node {node_id:?} on PR #{number}: {exc}"
                        ));
                    }
                }
            }
        }
        self.last_skipped_count.store(skipped, Ordering::SeqCst);
        if skipped > 0 {
            self.log(&format!(
                "[github_review] skipped {skipped} malformed review thread node(s) this pass on {}",
                self.repo
            ));
        }
        Ok(items)
    }

    /// Post the run's grader receipts as a PR comment (D-5).
    ///
    /// The PR number is parsed back out of the item's `source_ref`
    /// (`owner/repo#<pr>#<thread_id>`); GraphQL node ids contain no `#`,
    /// so the delimiter is unambiguous. No resolve/reply mutation is ever
    /// issued: the harness never flips a thread's resolution state, which
    /// is the one signal that must stay untrusted.
    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError> {
        let (repo, pr_number, _) = parse_review_source_ref(&report.source_ref)?;
        let body = format_report_body(report);
        run_gh(
            &*self.runner,
            &["pr", "comment", pr_number, "--repo", repo, "--body", &body],
        )?;
        Ok(())
    }
}

/// The factory behind the `github_review` kind: `[source] repo` is
/// required; the context's default graders and sinks are wired in.
pub fn build_github_review_source(
    context: &SourceBuildContext,
) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
    let repo = context.required_string(GITHUB_REVIEW_KIND, "repo")?;
    let mut source = GithubReviewWorkSource::new(repo)
        .map_err(|err| build_error(GITHUB_REVIEW_KIND, err))?
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

    fn comment(body: &str, login: &str, created_at: &str) -> Value {
        json!({
            "body": body,
            "createdAt": created_at,
            "url": "https://github.com/octo/widgets/pull/7#discussion_r1",
            "author": {"login": login},
        })
    }

    fn default_comment() -> Value {
        comment("please add a test", "reviewer", "2026-06-25T00:00:00Z")
    }

    fn thread_with(
        node_id: &str,
        is_resolved: bool,
        comments: Vec<Value>,
        comment_truncated: bool,
    ) -> Value {
        json!({
            "id": node_id,
            "isResolved": is_resolved,
            "isOutdated": false,
            "comments": {
                "nodes": comments,
                "pageInfo": {"hasNextPage": comment_truncated},
            },
        })
    }

    fn thread(node_id: &str) -> Value {
        thread_with(node_id, false, vec![default_comment()], false)
    }

    fn payload_with(
        pr_number: i64,
        threads: Vec<Value>,
        pr_truncated: bool,
        thread_truncated: bool,
    ) -> String {
        json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": [
                            {
                                "number": pr_number,
                                "reviewThreads": {
                                    "nodes": threads,
                                    "pageInfo": {"hasNextPage": thread_truncated},
                                },
                            }
                        ],
                        "pageInfo": {"hasNextPage": pr_truncated},
                    }
                }
            }
        })
        .to_string()
    }

    fn payload(threads: Vec<Value>) -> String {
        payload_with(7, threads, false, false)
    }

    fn source(gh: &Arc<FakeGh>) -> GithubReviewWorkSource {
        GithubReviewWorkSource::new("octo/widgets")
            .unwrap()
            .with_default_graders(vec![command_grader("uv run pytest")])
            .with_runner(Arc::clone(gh) as Arc<dyn GhRunner>)
    }

    fn ids(items: &[WorkItem]) -> Vec<String> {
        items.iter().map(|i| i.task.id.clone()).collect()
    }

    // --- inbound: list_work ------------------------------------------------

    #[test]
    fn list_work_queries_graphql() {
        let gh = FakeGh::new(payload(vec![]));
        assert!(source(&gh).list_work().unwrap().is_empty());
        let argv = gh.only_call();
        assert_eq!(&argv[..2], ["api", "graphql"]);
        let joined = argv.join(" ");
        assert!(joined.contains("owner=octo"));
        assert!(joined.contains("name=widgets"));
        assert!(joined.contains("isResolved"));
        let query = arg_after(&argv, "-f");
        assert!(query.starts_with("query=query($owner: String!, $name: String!)"));
        assert!(query.contains("first: 50"));
        assert!(query.contains("states: OPEN"));
        assert!(query.ends_with('}'));
    }

    #[test]
    fn only_unresolved_threads_become_items() {
        let gh = FakeGh::new(payload(vec![
            thread_with("RT_open", false, vec![default_comment()], false),
            thread_with("RT_done", true, vec![default_comment()], false),
        ]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].task.id.starts_with("prc-"));
        assert_eq!(items[0].task.id, review_item_id("RT_open"));
        assert_eq!(items[0].task.id.len(), 20);
    }

    #[test]
    fn default_graders_become_the_task_graders() {
        let grader = command_grader("uv run pytest");
        let gh = FakeGh::new(payload(vec![thread("RT_thread1")]));
        let items = source(&gh)
            .with_default_graders(vec![grader.clone()])
            .list_work()
            .unwrap();
        assert_eq!(items[0].task.graders, vec![grader]);
        assert_eq!(
            items[0].task.goal,
            "Address the unresolved review thread on pull request #7 in octo/widgets."
        );
    }

    #[test]
    fn thread_without_default_graders_is_skipped_and_logged() {
        let gh = FakeGh::new(payload(vec![thread("RT_thread1")]));
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
        assert!(events[0].2.contains("thread on PR #7"));
    }

    #[test]
    fn stable_id_across_polls_including_after_reply() {
        let first = FakeGh::new(payload(vec![thread("RT_x")]));
        let second = FakeGh::new(payload(vec![thread_with(
            "RT_x",
            false,
            vec![
                default_comment(),
                comment("still not addressed", "reviewer", "2026-06-25T01:00:00Z"),
            ],
            false,
        )]));
        let item1 = source(&first).list_work().unwrap().remove(0);
        let item2 = source(&second).list_work().unwrap().remove(0);
        assert_eq!(item1.task.id, item2.task.id);
        assert_ne!(item1.source_version, item2.source_version);
    }

    #[test]
    fn source_version_matches_the_python_seed() {
        let gh = FakeGh::new(payload(vec![thread("RT_x")]));
        let items = source(&gh).list_work().unwrap();
        let seed = "please add a test\u{0}False\u{0}False\u{0}2026-06-25T00:00:00Z";
        assert_eq!(
            items[0].source_version.as_deref(),
            Some(sha256_hex(seed).as_str())
        );
    }

    #[test]
    fn different_thread_node_id_gives_distinct_ids() {
        let gh = FakeGh::new(payload(vec![thread("RT_a"), thread("RT_b")]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items.len(), 2);
        assert_ne!(items[0].task.id, items[1].task.id);
    }

    #[test]
    fn context_carries_full_thread_and_url() {
        let gh = FakeGh::new(payload(vec![thread_with(
            "RT_thread1",
            false,
            vec![
                comment("please add a test", "alice", "2026-06-25T00:00:00Z"),
                comment("and rename it", "bob", "2026-06-25T00:00:00Z"),
            ],
            false,
        )]));
        let items = source(&gh).list_work().unwrap();
        let notes = &items[0].task.context.notes;
        assert_eq!(
            notes,
            "Unresolved review thread on pull request #7 in octo/widgets.\n\
             \n\
             Reviewer thread (most recent last):\n\
             alice: please add a test\n\
             bob: and rename it"
        );
        assert_eq!(
            items[0].task.context.references,
            ["https://github.com/octo/widgets/pull/7#discussion_r1"]
        );
    }

    #[test]
    fn outdated_threads_and_ghost_authors_are_rendered() {
        let mut t = thread_with(
            "RT_o",
            false,
            vec![json!({"body": "fix", "createdAt": "", "url": "", "author": null})],
            false,
        );
        t["isOutdated"] = json!(true);
        let gh = FakeGh::new(payload(vec![t]));
        let items = source(&gh).list_work().unwrap();
        let notes = &items[0].task.context.notes;
        assert!(notes.starts_with(
            "Unresolved review thread on pull request #7 in octo/widgets \
             (the diff it annotates is outdated)."
        ));
        assert!(notes.ends_with("(ghost): fix"));
        assert!(items[0].task.context.references.is_empty());
        assert_eq!(items[0].source_url.as_deref(), Some(""));
    }

    #[test]
    fn provenance_stamps() {
        let gh = FakeGh::new(payload_with(42, vec![thread("RT_z")], false, false));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items[0].source_kind.as_deref(), Some("github_review"));
        assert!(
            items[0]
                .source_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://github.com/"))
        );
        assert!(
            items[0]
                .source_version
                .as_deref()
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(items[0].source_ref, "octo/widgets#42#RT_z");
    }

    #[test]
    fn each_truncated_axis_emits_one_shared_warning_and_stop_event() {
        let gh = FakeGh::new(payload_with(
            7,
            vec![thread_with("RT_t", false, vec![default_comment()], true)],
            true,
            true,
        ));
        let (log, lines) = capture_log();
        let (sink, events) = capture_stop_events();
        let items = source(&gh)
            .with_log(log)
            .with_stop_sink(sink)
            .list_work()
            .unwrap();
        assert_eq!(items.len(), 1);
        let lines = lines.lock().unwrap();
        let warnings: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("truncated at one page"))
            .collect();
        assert_eq!(warnings.len(), 3);
        assert!(warnings.iter().all(|l| l.contains("[github_review]")));
        assert!(
            warnings
                .iter()
                .all(|l| l.contains("some items were not read this pass"))
        );
        let whats = warnings
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(whats.contains("open pull requests"));
        assert!(whats.contains("review threads on PR #7"));
        assert!(whats.contains("comments on a review thread on PR #7"));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|(kind, subject, _)| kind == "source-truncation" && subject == "octo/widgets")
        );
    }

    #[test]
    fn no_truncation_emits_no_warning() {
        let gh = FakeGh::new(payload(vec![thread("RT_t")]));
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
        let page = payload_with(
            7,
            vec![thread_with("RT_t", false, vec![default_comment()], true)],
            true,
            true,
        );
        let (log, _lines) = capture_log();
        let with_log = source(&FakeGh::new(page.clone()))
            .with_log(log)
            .list_work()
            .unwrap();
        let without = source(&FakeGh::new(page)).list_work().unwrap();
        assert_eq!(ids(&with_log), ids(&without));
        assert_eq!(without.len(), 1);
    }

    #[test]
    fn failed_listing_raises_and_never_returns_empty() {
        let gh = FakeGh::failing("boom");
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("gh api graphql"), "{err}");
        assert!(err.contains("failed (exit 1): boom"), "{err}");
    }

    #[test]
    fn invalid_and_non_object_payloads_raise() {
        let err = source(&FakeGh::new("not json at all"))
            .list_work()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("gh api graphql returned invalid JSON"),
            "{err}"
        );
        let err = source(&FakeGh::new(json!([1, 2, 3]).to_string()))
            .list_work()
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "gh api graphql payload root is list, expected an object"
        );
        let err = source(&FakeGh::new(""))
            .list_work()
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "gh api graphql payload data is NoneType, expected an object"
        );
        let err = source(&FakeGh::new(
            json!({"data": {"repository": {"pullRequests": {"nodes": "x"}}}}).to_string(),
        ))
        .list_work()
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "gh api graphql payload pullRequests.nodes is str, expected a list"
        );
    }

    #[test]
    fn pull_request_missing_number_raises() {
        let text = json!({
            "data": {"repository": {"pullRequests": {
                "nodes": [{"reviewThreads": {"nodes": []}}],
                "pageInfo": {"hasNextPage": false},
            }}}
        })
        .to_string();
        let err = source(&FakeGh::new(text))
            .list_work()
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing integer 'number'"), "{err}");
    }

    #[test]
    fn thread_missing_node_id_raises() {
        let mut bad = thread("RT_a");
        bad.as_object_mut().unwrap().remove("id");
        let gh = FakeGh::new(payload(vec![bad]));
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("missing string 'id'"), "{err}");
    }

    // --- skip-bad-item: one identifiable node is skipped -------------------------

    fn bad_thread(node_id: &str) -> Value {
        json!({
            "id": node_id,
            "isResolved": false,
            "isOutdated": false,
            "comments": "not-an-object",
        })
    }

    #[test]
    fn one_bad_node_is_skipped_and_the_valid_threads_still_return() {
        let gh = FakeGh::new(payload(vec![
            thread("RT_a"),
            bad_thread("RT_bad"),
            thread("RT_b"),
        ]));
        let (log, lines) = capture_log();
        let src = source(&gh).with_log(log);
        let items = src.list_work().unwrap();
        assert_eq!(items.len(), 2);
        let refs: Vec<&str> = items.iter().map(|i| i.source_ref.as_str()).collect();
        assert_eq!(refs, ["octo/widgets#7#RT_a", "octo/widgets#7#RT_b"]);
        assert_eq!(src.last_skipped_count(), 1);
        let lines = lines.lock().unwrap();
        let skips: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("skipping malformed review thread"))
            .collect();
        assert_eq!(skips.len(), 1);
        assert!(skips[0].contains("RT_bad"));
        assert!(skips[0].contains("reviewThread.comments is str, expected an object"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("skipped 1 malformed review thread node"))
        );
    }

    #[test]
    fn skip_does_not_crash_when_no_log_is_configured() {
        let gh = FakeGh::new(payload(vec![thread("RT_a"), bad_thread("RT_bad")]));
        let items = source(&gh).list_work().unwrap();
        let refs: Vec<&str> = items.iter().map(|i| i.source_ref.as_str()).collect();
        assert_eq!(refs, ["octo/widgets#7#RT_a"]);
    }

    #[test]
    fn all_nodes_bad_yields_empty_but_does_not_raise() {
        let gh = FakeGh::new(payload(vec![bad_thread("RT_x"), bad_thread("RT_y")]));
        let (log, lines) = capture_log();
        let src = source(&gh).with_log(log);
        assert!(src.list_work().unwrap().is_empty());
        assert_eq!(src.last_skipped_count(), 2);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("skipped 2 malformed review thread node"))
        );
    }

    #[test]
    fn missing_node_id_is_an_identity_break_and_still_aborts() {
        let mut no_id = thread("RT_a");
        no_id.as_object_mut().unwrap().remove("id");
        let gh = FakeGh::new(payload(vec![no_id, thread("RT_b")]));
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("missing string 'id'"), "{err}");
    }

    #[test]
    fn malformed_comment_node_is_a_per_thread_skip() {
        let gh = FakeGh::new(payload(vec![
            thread_with("RT_c", false, vec![json!("not a comment")], false),
            thread("RT_ok"),
        ]));
        let src = source(&gh);
        let items = src.list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_ref, "octo/widgets#7#RT_ok");
        assert_eq!(src.last_skipped_count(), 1);
    }

    // --- outbound: report ----------------------------------------------------------

    fn report(source_ref: &str) -> WorkReport {
        WorkReport {
            task_id: "prc-x".into(),
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
    fn report_comments_on_pr_and_never_resolves() {
        let gh = FakeGh::new("");
        source(&gh).report(&report("octo/widgets#7#RT_x")).unwrap();
        let argv = gh.only_call();
        assert_eq!(&argv[..3], ["pr", "comment", "7"]);
        assert_eq!(arg_after(&argv, "--repo"), "octo/widgets");
        let body = arg_after(&argv, "--body");
        assert!(body.contains("run `run-abc`"));
        assert!(body.contains("| 0 | tests | command | yes |"));
        assert!(!argv.iter().any(|a| a.contains("resolveReviewThread")));
    }

    #[test]
    fn report_malformed_source_ref_raises() {
        let gh = FakeGh::new("");
        for bad in [
            "octo/widgets#7",
            "octo/widgets#x#RT",
            "#7#RT",
            "octo/widgets#7#",
        ] {
            let err = source(&gh).report(&report(bad)).unwrap_err().to_string();
            assert!(err.contains("malformed github_review source_ref"), "{err}");
        }
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn construction_validates_repo_shape() {
        assert!(GithubReviewWorkSource::new("").is_err());
        assert!(GithubReviewWorkSource::new("no-slash").is_err());
        let src = GithubReviewWorkSource::new("octo/widgets").unwrap();
        assert_eq!(src.source_kind(), "github_review");
        assert_eq!(src.source_name(), "octo/widgets");
        assert_eq!(src.repo(), "octo/widgets");
        assert!(src.default_graders().is_empty());
        assert_eq!(src.last_skipped_count(), 0);
        assert!(format!("{src:?}").contains("octo/widgets"));
    }

    // --- registry wiring -------------------------------------------------------------

    #[test]
    fn github_review_factory_reads_settings() {
        let mut registry = default_registry();
        crate::github::register_github_sources(&mut registry);
        let mut settings = toml::Table::new();
        settings.insert("kind".into(), toml::Value::String("github_review".into()));
        settings.insert("repo".into(), toml::Value::String("octo/widgets".into()));
        let built = registry
            .build_from_context(&SourceBuildContext::new(settings.clone()))
            .unwrap();
        assert_eq!(built.source_kind(), "github_review");
        assert_eq!(built.source_name(), "octo/widgets");

        settings.insert("repo".into(), toml::Value::String("no-slash".into()));
        let err = registry
            .build("github_review", &SourceBuildContext::new(settings))
            .err()
            .expect("build must fail");
        assert!(matches!(err, SourceRegistryError::Build { .. }), "{err}");

        let err = registry
            .build("github_review", &SourceBuildContext::default())
            .err()
            .expect("build must fail")
            .to_string();
        assert_eq!(
            err,
            "cannot build github_review source: [source] repo is required"
        );
    }

    // --- mutation-survey pins (thread url, version seed, compile failures,
    // report errors) -----------------------------------------------------------

    #[test]
    fn thread_url_is_the_first_comment_url() {
        let first = json!({
            "body": "please add a test",
            "createdAt": "2026-06-25T00:00:00Z",
            "url": "https://github.com/octo/widgets/pull/7#discussion_r1",
            "author": {"login": "alice"},
        });
        let reply = json!({
            "body": "done?",
            "createdAt": "2026-06-26T00:00:00Z",
            "url": "https://github.com/octo/widgets/pull/7#discussion_r2",
            "author": {"login": "bob"},
        });
        let gh = FakeGh::new(payload(vec![thread_with(
            "RT_x",
            false,
            vec![first, reply],
            false,
        )]));
        let items = source(&gh).list_work().unwrap();
        assert_eq!(
            items[0].source_url.as_deref(),
            Some("https://github.com/octo/widgets/pull/7#discussion_r1")
        );
        assert_eq!(
            items[0].task.context.references,
            ["https://github.com/octo/widgets/pull/7#discussion_r1"]
        );
    }

    #[test]
    fn source_version_seed_uses_the_latest_timestamp_and_the_outdated_flag() {
        let mut node = thread_with(
            "RT_x",
            false,
            vec![
                comment("later", "alice", "2026-06-26T00:00:00Z"),
                comment("earlier", "bob", "2026-06-25T00:00:00Z"),
            ],
            false,
        );
        node["isOutdated"] = json!(true);
        let gh = FakeGh::new(payload(vec![node]));
        let items = source(&gh).list_work().unwrap();
        let seed = "later\u{0}earlier\u{0}False\u{0}True\u{0}2026-06-26T00:00:00Z";
        assert_eq!(
            items[0].source_version.as_deref(),
            Some(sha256_hex(seed).as_str())
        );
    }

    #[test]
    fn invalid_default_grader_is_a_per_node_skip() {
        let bad = Grader::Command(flywheel_core::task::CommandGrader {
            run: String::new(),
            name: None,
        });
        let gh = FakeGh::new(payload(vec![thread("RT_x")]));
        let (log, lines) = capture_log();
        let src = source(&gh).with_default_graders(vec![bad]).with_log(log);
        assert!(src.list_work().unwrap().is_empty());
        assert_eq!(src.last_skipped_count(), 1);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("\"RT_x\"") && l.contains("non-empty 'run'"))
        );
    }

    #[test]
    fn report_propagates_gh_failure() {
        let gh = FakeGh::failing("pull request is locked");
        let err = source(&gh)
            .report(&report("octo/widgets#7#RT_x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh pr comment 7"), "{err}");
        assert!(err.contains("pull request is locked"), "{err}");
    }
}
