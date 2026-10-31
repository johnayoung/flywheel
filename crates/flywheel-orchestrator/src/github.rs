//! GitHub Issues work source: a tracker-backed [`WorkSource`].
//!
//! Maps labeled GitHub issues to flywheel tasks through the `gh` CLI (no
//! extra dependencies; auth is whatever `gh auth` already holds). The board
//! is the inbound control plane; flywheel's store stays the authoritative
//! execution record, and outcomes are projected back as issue comments.
//!
//! Issue -> Task compilation:
//!
//! * `id` is `gh-<number>` (stable, referenceable from other issues'
//!   `prerequisites`).
//! * `goal` is the issue title, unless overridden by a spec block.
//! * An issue MAY embed a fenced spec block whose info string is exactly
//!   `flywheel`:
//!
//!   ````text
//!   ```flywheel
//!   {
//!     "goal": "One-sentence diff test.",
//!     "graders": [{"type": "command", "run": "uv run pytest"}],
//!     "context": {"relevant": ["src/thing.py"]},
//!     "tags": ["loop"],
//!     "prerequisites": ["gh-12"]
//!   }
//!   ```
//!   ````
//!
//!   Every key is optional. `graders` falls back to the repo policy's
//!   default graders; an issue with no graders from either source is **not
//!   runnable** and is skipped, so the readiness gate stays mechanical
//!   without forcing every ticket author to write graders.
//! * The issue body is carried in `context.notes` (unless the spec block
//!   sets its own) and the issue URL in `context.references`, so the agent
//!   sees the full ticket text.
//!
//! Outbound: [`GithubWorkSource::report`] posts a comment with the run id,
//! terminal status, and the final attempt's grader receipts. When the run
//! reached `Done` and the policy says `done_action = "close"`, the issue is
//! closed with that comment instead. Ticket writes go through the harness's
//! reporting path, never through the agent, so "agent claims are untrusted"
//! extends to ticket transitions.
//!
//! This module also hosts the seam the two sibling GitHub adapters share:
//! the [`GhRunner`] subprocess boundary (with [`ProcessGhRunner`] as the
//! real implementation), [`run_gh`], [`format_report_body`],
//! [`emit_truncation_warning`], and [`register_github_sources`], which
//! binds all three `[source] kind`s into a [`SourceRegistry`].
//!
//! Authoritative spec: `docs/work-sources.md` (`github`).

use crate::github_ci::build_github_ci_source;
use crate::github_review::build_github_review_source;
use crate::source_registry::{
    GITHUB_CI_KIND, GITHUB_KIND, GITHUB_REVIEW_KIND, SourceBuildContext, SourceRegistry,
    SourceRegistryError,
};
use crate::sources::{LogSink, StopEventSink, WorkItem, WorkReport, WorkSource, WorkSourceError};
use crate::triage_receipt::{
    RECEIPT_KEY, SPEC_FENCE_CLOSE, SPEC_FENCE_OPEN, content_hash, is_fail_first, parse_receipt,
    py_splitlines,
};
use flywheel_core::loaders::load_graders;
use flywheel_core::task::{Context, Grader, Task};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

/// Provenance `source_kind` for emitted items and `source_syncs` rows.
/// (The registry kind is `github`; the provenance tag is `github_issue`.)
pub const GITHUB_ISSUE_SOURCE_KIND: &str = "github_issue";

/// Stop-event kind recorded when a one-page listing fills its cap.
/// Mirrors the orchestrator's `STOP_SOURCE_TRUNCATION` ledger kind.
pub const STOP_SOURCE_TRUNCATION: &str = "source-truncation";
/// Stop-event kind recorded when an item is dropped for having no graders.
/// Mirrors the orchestrator's `STOP_ZERO_GRADER_DROP` ledger kind.
pub const STOP_ZERO_GRADER_DROP: &str = "zero-grader-drop";

/// The `gh issue list --json` fields the adapter reads.
pub const ISSUE_LIST_FIELDS: &str = "number,title,body,url";

/// gh defaults to 30; one page of 200 keeps a realistic board in one call.
/// Shared with the CI adapter (`gh run list` defaults to 20).
pub const LIST_LIMIT: usize = 200;

// --- The gh subprocess seam ---------------------------------------------------

/// What one `gh` invocation produced.
///
/// `exit_code` is `None` when the process was terminated by a signal
/// rather than exiting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GhOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl GhOutput {
    /// A successful invocation that printed `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
        }
    }
}

/// The `gh` subprocess boundary: run `gh <argv>` and report what it did.
///
/// Production uses [`ProcessGhRunner`]; tests drive the sources with a
/// scripted runner returning canned output. `argv` is everything after the
/// `gh` executable. An `Err` means the process could not be run at all
/// (binary missing, spawn failure); a non-zero exit is reported through
/// [`GhOutput::exit_code`] and turned into the branded [`WorkSourceError`]
/// by [`run_gh`].
pub trait GhRunner: Send + Sync {
    fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError>;
}

/// The real runner: spawns the `gh` binary on `PATH` and captures its
/// output.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessGhRunner;

impl GhRunner for ProcessGhRunner {
    fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
        let output = std::process::Command::new("gh")
            .args(argv)
            .output()
            .map_err(|err| {
                WorkSourceError::new(format!("gh {} failed to start: {err}", argv.join(" ")))
            })?;
        Ok(GhOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
        })
    }
}

/// Run `gh <argv>` through `runner` and return stdout; fail closed on any
/// non-zero exit with the branded `gh <argv> failed (exit N): <detail>`
/// message (stderr, else stdout, else `(no output)`).
pub fn run_gh(runner: &dyn GhRunner, argv: &[&str]) -> Result<String, WorkSourceError> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let output = runner.run_gh(&argv)?;
    if output.exit_code == Some(0) {
        return Ok(output.stdout);
    }
    let exit = match output.exit_code {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    };
    let stderr = output.stderr.trim();
    let stdout = output.stdout.trim();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "(no output)"
    };
    Err(WorkSourceError::new(format!(
        "gh {} failed (exit {exit}): {detail}",
        argv.join(" ")
    )))
}

// --- Shared helpers for every GitHub adapter -----------------------------------

/// Emit the uniform one-page truncation warning through `log`.
///
/// The shared seam for every GitHub source: a bounded single-page listing
/// that fills its cap signals dropped items rather than silently
/// truncating. `source` names the source tag (e.g. `github`), `what` names
/// the listed unit. A side channel only: a `None` log is a no-op and the
/// returned work sequence is never affected.
pub fn emit_truncation_warning(log: Option<&LogSink>, source: &str, what: &str) {
    if let Some(log) = log {
        log(&format!(
            "[{source}] {what} listing truncated at one page; some items were not read this pass"
        ));
    }
}

/// Render the comment body every GitHub adapter posts back: the run id and
/// terminal status, the error (when any), and a markdown table of the
/// grader receipts.
pub fn format_report_body(report: &WorkReport) -> String {
    let mut lines = vec![format!(
        "flywheel: run `{}` for task `{}` finished with status `{}`.",
        report.run_id,
        report.task_id,
        report.status.as_str()
    )];
    if !report.error.is_empty() {
        lines.push(format!("\nError: {}", report.error));
    }
    if report.graders.is_empty() {
        lines.push("\nNo grader receipts were recorded for this run.".to_string());
    } else {
        lines.push("\n| ordinal | grader | type | passed |".to_string());
        lines.push("| ------- | ------ | ---- | ------ |".to_string());
        for receipt in &report.graders {
            lines.push(format!(
                "| {} | {} | {} | {} |",
                receipt.ordinal,
                receipt.name.as_deref().unwrap_or("-"),
                receipt.grader_type.as_str(),
                if receipt.passed { "yes" } else { "no" }
            ));
        }
    }
    lines.join("\n")
}

/// Python's `type(x).__name__` for the value a JSON value decodes to, so
/// payload-shape errors read the same as the Python adapter's.
pub(crate) fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness of a JSON value (`x or ""` semantics).
pub(crate) fn py_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python's `str(x)` for a JSON scalar; containers render as compact JSON.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `str(obj.get(key) or "")`: the field as text, empty for a missing or
/// falsy value.
pub(crate) fn str_or_empty(obj: &Map<String, Value>, key: &str) -> String {
    match obj.get(key) {
        Some(value) if py_truthy(Some(value)) => py_str(value),
        _ => String::new(),
    }
}

/// Decode a `gh ... list` page: empty stdout is an empty page, anything
/// but a JSON array is a hard error naming the command.
pub(crate) fn parse_list_payload(
    stdout: &str,
    command: &str,
) -> Result<Vec<Value>, WorkSourceError> {
    let text = if stdout.trim().is_empty() {
        "[]"
    } else {
        stdout
    };
    let payload: Value = serde_json::from_str(text)
        .map_err(|err| WorkSourceError::new(format!("{command} returned invalid JSON: {err}")))?;
    match payload {
        Value::Array(entries) => Ok(entries),
        other => Err(WorkSourceError::new(format!(
            "{command} returned {}, expected a list",
            py_type_name(&other)
        ))),
    }
}

/// `sha256(text)` as lowercase hex.
pub(crate) fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

// --- The issue spec block ------------------------------------------------------

/// Parse the issue body's ```` ```flywheel ```` fenced block, if any.
///
/// Returns `None` when no block is present. A present-but-invalid block is
/// an error: a ticket author who wrote a spec block meant it, so silent
/// fallback to defaults would mask their error. A block nested past the
/// JSON parser's recursion limit (an attacker-authored body) is reported
/// as an invalid block rather than escaping the per-item guard.
pub fn extract_spec_block(
    body: &str,
    source: &str,
) -> Result<Option<Map<String, Value>>, WorkSourceError> {
    let lines = py_splitlines(body);
    let Some(open_idx) = lines.iter().position(|l| l.trim() == SPEC_FENCE_OPEN) else {
        return Ok(None);
    };
    let mut block: Vec<&str> = Vec::new();
    for line in &lines[open_idx + 1..] {
        if line.trim() != SPEC_FENCE_CLOSE {
            block.push(line);
            continue;
        }
        let data: Value = serde_json::from_str(&block.join("\n")).map_err(|err| {
            if err.to_string().contains("recursion limit exceeded") {
                WorkSourceError::new(format!(
                    "{source}: flywheel spec block nests too deeply to parse"
                ))
            } else {
                WorkSourceError::new(format!(
                    "{source}: invalid JSON in flywheel spec block: {err}"
                ))
            }
        })?;
        return match data {
            Value::Object(map) => Ok(Some(map)),
            other => Err(WorkSourceError::new(format!(
                "{source}: flywheel spec block must be a JSON object, got {}",
                py_type_name(&other)
            ))),
        };
    }
    Err(WorkSourceError::new(format!(
        "{source}: flywheel spec block is not closed by a ``` fence"
    )))
}

/// Why the trust rule skips this issue, or `None` when it may schedule.
///
/// The drain-side triage-trust gate (the *policy*; the receipt schema and
/// content-hash *definition* live in [`crate::triage_receipt`]). Returns a
/// human-readable reason for every failure mode (no spec block, no receipt
/// in the block, a malformed or passing receipt, or a receipt whose
/// `content_hash` no longer matches the issue's current human content) and
/// `None` only when the block carries a fail-first receipt matching the
/// current content hash.
pub fn triage_skip_reason(
    spec: Option<&Map<String, Value>>,
    title: &str,
    body: &str,
) -> Option<String> {
    let Some(spec) = spec else {
        return Some(
            "trust mode requires a flywheel spec block carrying a triage receipt, \
             but the issue has none"
                .to_string(),
        );
    };
    let raw = match spec.get(RECEIPT_KEY) {
        None | Some(Value::Null) => {
            return Some(
                "trust mode requires a triage receipt in the flywheel spec block, \
                 but none is present"
                    .to_string(),
            );
        }
        Some(raw) => raw,
    };
    let Some(receipt) = parse_receipt(raw) else {
        return Some("the flywheel spec block's triage receipt is malformed".to_string());
    };
    if !is_fail_first(&receipt) {
        return Some(
            "the triage receipt is not fail-first (exit_code 0); it records a command \
             that already passed, so there is nothing to schedule"
                .to_string(),
        );
    }
    if receipt.content_hash != content_hash(title, body) {
        return Some(
            "the triage receipt is stale -- its content_hash no longer matches the \
             issue's current title/body (edited after triage)"
                .to_string(),
        );
    }
    None
}

/// A spec-block list field as strings: absent/null is empty, a list is
/// stringified entry by entry, a bare string is one entry, anything else
/// is a shape error naming the field.
fn spec_str_list(
    obj: &Map<String, Value>,
    key: &str,
    what: &str,
    source: &str,
) -> Result<Vec<String>, WorkSourceError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(entries)) => Ok(entries.iter().map(py_str).collect()),
        Some(Value::String(s)) if s.is_empty() => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(other) => Err(WorkSourceError::new(format!(
            "{source}: spec '{what}' must be a list, got {}",
            py_type_name(other)
        ))),
    }
}

fn context_from_spec(spec: &Map<String, Value>, source: &str) -> Result<Context, WorkSourceError> {
    let raw = match spec.get("context") {
        Some(Value::Object(map)) => map,
        None | Some(Value::Null) => return Ok(Context::default()),
        Some(other) if !py_truthy(Some(other)) => return Ok(Context::default()),
        Some(other) => {
            return Err(WorkSourceError::new(format!(
                "{source}: spec 'context' must be an object, got {}",
                py_type_name(other)
            )));
        }
    };
    Ok(Context {
        relevant: spec_str_list(raw, "relevant", "context.relevant", source)?,
        references: spec_str_list(raw, "references", "context.references", source)?,
        constraints: spec_str_list(raw, "constraints", "context.constraints", source)?,
        non_goals: spec_str_list(raw, "non_goals", "context.non_goals", source)?,
        edge_cases: spec_str_list(raw, "edge_cases", "context.edge_cases", source)?,
        notes: str_or_empty(raw, "notes"),
    })
}

/// `owner/repo#123` -> `123` (the shape this source itself emits).
fn issue_number_from_ref(source_ref: &str) -> Result<&str, WorkSourceError> {
    let number = source_ref.split_once('#').map(|(_, number)| number);
    match number {
        Some(number) if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(number)
        }
        _ => Err(WorkSourceError::new(format!(
            "malformed github source_ref {source_ref:?}; expected 'owner/repo#<number>'"
        ))),
    }
}

/// Sort key by issue number, tolerant of malformed payloads: an issue with
/// a missing or non-integer `number` (which compilation rejects and skips)
/// sorts as `0` rather than crashing the ordering.
fn issue_sort_key(issue: &Map<String, Value>) -> i64 {
    issue.get("number").and_then(Value::as_i64).unwrap_or(0)
}

// --- done_action -------------------------------------------------------------

/// What `report` does to an issue whose run reached `Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoneAction {
    /// Post the outcome as a comment (the default).
    #[default]
    Comment,
    /// Close the issue with the outcome as the closing comment.
    Close,
}

impl DoneAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            DoneAction::Comment => "comment",
            DoneAction::Close => "close",
        }
    }
}

impl fmt::Display for DoneAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DoneAction {
    type Err = WorkSourceError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "comment" => Ok(DoneAction::Comment),
            "close" => Ok(DoneAction::Close),
            other => Err(WorkSourceError::new(format!(
                "done_action must be 'comment' or 'close', got {other:?}"
            ))),
        }
    }
}

// --- The source ----------------------------------------------------------------

/// Labeled open issues in one GitHub repo, compiled to flywheel tasks.
///
/// `runner` is the `gh` subprocess seam: production uses the real CLI,
/// tests inject a scripted runner returning canned stdout. `log` (when
/// provided) receives one line per skipped issue so non-runnable tickets
/// are visible rather than silently absent.
///
/// `require_triage_receipt` is the opt-in trust rule (default off, so the
/// source behaves identically when absent). When on, an issue is scheduled
/// only when its `flywheel` spec block carries a triage receipt (see
/// [`crate::triage_receipt`]) whose `content_hash` matches the issue's
/// current human content and whose `exit_code` is fail-first (non-zero).
/// Every issue that fails that gate (no block, no receipt, a malformed or
/// passing receipt, or a hash gone stale after a title/body edit) is
/// skipped loudly through `log` and produces no [`WorkItem`], and the
/// `[[defaults.graders]]` fallback deliberately does NOT rescue it.
///
/// One uncompilable payload never aborts the whole board: the bad ticket
/// is logged (the line carries its `source_ref`/payload so it is
/// recoverable) and every other issue still returns. A listing that
/// produced no compilable item yet hit malformed payloads is a hard error,
/// not "no work", so a fully broken board never silently reads as empty.
pub struct GithubWorkSource {
    repo: String,
    label: String,
    default_graders: Vec<Grader>,
    done_action: DoneAction,
    require_triage_receipt: bool,
    runner: Arc<dyn GhRunner>,
    log: Option<LogSink>,
    stop_sink: Option<StopEventSink>,
}

impl fmt::Debug for GithubWorkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubWorkSource")
            .field("repo", &self.repo)
            .field("label", &self.label)
            .field("default_graders", &self.default_graders.len())
            .field("done_action", &self.done_action)
            .field("require_triage_receipt", &self.require_triage_receipt)
            .finish()
    }
}

/// Reject an empty `owner/repo` at construction so a misconfigured source
/// fails at build time rather than on its first `gh` call.
pub(crate) fn require_repo(repo: &str) -> Result<(), WorkSourceError> {
    if repo.is_empty() {
        return Err(WorkSourceError::new(
            "repo must be a non-empty 'owner/repo' string",
        ));
    }
    Ok(())
}

impl GithubWorkSource {
    /// A source over `repo`'s open issues carrying `label`, driven by the
    /// real `gh` binary, with comment-only reporting and the trust rule
    /// off.
    pub fn new(repo: impl Into<String>, label: impl Into<String>) -> Result<Self, WorkSourceError> {
        let repo = repo.into();
        let label = label.into();
        require_repo(&repo)?;
        if label.is_empty() {
            return Err(WorkSourceError::new("label must be a non-empty string"));
        }
        Ok(Self {
            repo,
            label,
            default_graders: Vec::new(),
            done_action: DoneAction::Comment,
            require_triage_receipt: false,
            runner: Arc::new(ProcessGhRunner),
            log: None,
            stop_sink: None,
        })
    }

    /// The `[[defaults.graders]]` fallback for issues whose spec block
    /// declares none.
    pub fn with_default_graders(mut self, graders: Vec<Grader>) -> Self {
        self.default_graders = graders;
        self
    }

    pub fn with_done_action(mut self, done_action: DoneAction) -> Self {
        self.done_action = done_action;
        self
    }

    /// Turn the triage-trust rule on or off.
    pub fn with_require_triage_receipt(mut self, require: bool) -> Self {
        self.require_triage_receipt = require;
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

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn done_action(&self) -> DoneAction {
        self.done_action
    }

    pub fn require_triage_receipt(&self) -> bool {
        self.require_triage_receipt
    }

    pub fn default_graders(&self) -> &[Grader] {
        &self.default_graders
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

    fn compile_issue(
        &self,
        issue: &Map<String, Value>,
    ) -> Result<Option<WorkItem>, WorkSourceError> {
        let Some(number) = issue.get("number").and_then(Value::as_i64) else {
            return Err(WorkSourceError::new(format!(
                "github issue payload missing integer 'number': {}",
                Value::Object(issue.clone())
            )));
        };
        let source_ref = format!("{}#{number}", self.repo);
        let title = str_or_empty(issue, "title");
        let body = str_or_empty(issue, "body");
        let url = str_or_empty(issue, "url");

        // A present-but-invalid block still fails here, even in trust mode:
        // an author who wrote a block meant it.
        let spec_block = extract_spec_block(&body, &source_ref)?;

        if self.require_triage_receipt
            && let Some(reason) = triage_skip_reason(spec_block.as_ref(), &title, &body)
        {
            // Loud skip: surfaced through the log seam, no WorkItem, and the
            // [[defaults.graders]] fallback deliberately does not fire.
            self.log(&format!("[github] skipping {source_ref}: {reason}"));
            return Ok(None);
        }

        let spec = spec_block.unwrap_or_default();

        let graders = match spec.get("graders") {
            Some(raw) if !raw.is_null() => load_graders(raw, &source_ref)?,
            _ => self.default_graders.clone(),
        };
        if graders.is_empty() {
            let detail = format!(
                "{source_ref}: no graders in the issue's flywheel block and no default \
                 grader policy -- not runnable"
            );
            self.log(&format!("[github] skipping {detail}"));
            self.stop(STOP_ZERO_GRADER_DROP, &detail);
            return Ok(None);
        }

        let mut context = context_from_spec(&spec, &source_ref)?;
        if context.notes.is_empty() && !body.is_empty() {
            context.notes = body.clone();
        }
        if !url.is_empty() && !context.references.contains(&url) {
            context.references.push(url.clone());
        }

        let goal = match spec.get("goal") {
            Some(raw) if py_truthy(Some(raw)) => py_str(raw),
            _ => title.clone(),
        }
        .trim()
        .to_string();
        let prerequisites = spec_str_list(&spec, "prerequisites", "prerequisites", &source_ref)?;
        let tags = spec_str_list(&spec, "tags", "tags", &source_ref)?;

        let task = Task {
            id: format!("gh-{number}"),
            goal,
            graders,
            tags,
            context,
            budgets: Default::default(),
        };
        task.validate()
            .map_err(|exc| WorkSourceError::new(format!("{source_ref}: {exc}")))?;
        Ok(Some(WorkItem {
            task,
            prerequisites,
            source_ref,
            local_path: None,
            phase: None,
            source_kind: Some(GITHUB_ISSUE_SOURCE_KIND.to_string()),
            source_version: Some(sha256_hex(&body)),
            source_url: Some(url),
            priority: 0,
            required_capabilities: Default::default(),
            conflict_keys: Default::default(),
        }))
    }
}

impl WorkSource for GithubWorkSource {
    fn source_kind(&self) -> &str {
        GITHUB_ISSUE_SOURCE_KIND
    }

    /// The source's locus for `source_syncs`: the `owner/repo`.
    fn source_name(&self) -> String {
        self.repo.clone()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let limit = LIST_LIMIT.to_string();
        let stdout = run_gh(
            &*self.runner,
            &[
                "issue",
                "list",
                "--repo",
                &self.repo,
                "--label",
                &self.label,
                "--state",
                "open",
                "--json",
                ISSUE_LIST_FIELDS,
                "--limit",
                &limit,
            ],
        )?;
        let issues = parse_list_payload(&stdout, "gh issue list")?;
        if issues.len() == LIST_LIMIT {
            emit_truncation_warning(self.log.as_ref(), "github", "issue");
            self.stop(
                STOP_SOURCE_TRUNCATION,
                &format!(
                    "issue listing truncated at one page ({LIST_LIMIT} items); \
                     some items were not read this pass"
                ),
            );
        }
        let mut objects: Vec<&Map<String, Value>> =
            issues.iter().filter_map(Value::as_object).collect();
        objects.sort_by_key(|issue| issue_sort_key(issue));

        let mut items = Vec::new();
        let mut first_skip: Option<WorkSourceError> = None;
        for issue in objects {
            match self.compile_issue(issue) {
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(exc) => {
                    // One uncompilable payload must not abort the whole
                    // board: log the bad ticket (the message carries its
                    // source_ref / payload so it is recoverable) and go on.
                    self.log(&format!("[github] skipping malformed issue: {exc}"));
                    if first_skip.is_none() {
                        first_skip = Some(exc);
                    }
                }
            }
        }
        // A listing that produced no compilable item yet hit malformed
        // payloads is a hard error, not "no work": a fully broken board
        // never silently reads as empty.
        if items.is_empty()
            && let Some(exc) = first_skip
        {
            return Err(exc);
        }
        Ok(items)
    }

    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError> {
        let number = issue_number_from_ref(&report.source_ref)?;
        let body = format_report_body(report);
        if report.status == flywheel_core::lifecycle::Status::Done
            && self.done_action == DoneAction::Close
        {
            run_gh(
                &*self.runner,
                &[
                    "issue",
                    "close",
                    number,
                    "--repo",
                    &self.repo,
                    "--comment",
                    &body,
                ],
            )?;
            return Ok(());
        }
        run_gh(
            &*self.runner,
            &[
                "issue", "comment", number, "--repo", &self.repo, "--body", &body,
            ],
        )?;
        Ok(())
    }
}

// --- Registry wiring -------------------------------------------------------------

/// A `[source]` boolean that defaults to `false` and rejects non-booleans.
pub(crate) fn optional_bool(
    context: &SourceBuildContext,
    kind: &str,
    key: &str,
) -> Result<bool, SourceRegistryError> {
    match context.settings.get(key) {
        None => Ok(false),
        Some(toml::Value::Boolean(value)) => Ok(*value),
        Some(other) => Err(SourceRegistryError::Config {
            kind: kind.to_string(),
            detail: format!("[source] {key} must be a boolean, got {}", other.type_str()),
        }),
    }
}

/// A `[source]` string with a default, rejecting non-strings.
pub(crate) fn optional_string<'a>(
    context: &'a SourceBuildContext,
    kind: &str,
    key: &str,
    default: &'a str,
) -> Result<&'a str, SourceRegistryError> {
    match context.settings.get(key) {
        None => Ok(default),
        Some(toml::Value::String(value)) => Ok(value),
        Some(other) => Err(SourceRegistryError::Config {
            kind: kind.to_string(),
            detail: format!("[source] {key} must be a string, got {}", other.type_str()),
        }),
    }
}

pub(crate) fn build_error(kind: &str, source: WorkSourceError) -> SourceRegistryError {
    SourceRegistryError::Build {
        kind: kind.to_string(),
        source,
    }
}

/// The factory behind the `github` kind: `[source] repo` and `label` are
/// required, `done_action` defaults to `comment`, `require_triage_receipt`
/// to `false`; the context's default graders and sinks are wired in.
pub fn build_github_source(
    context: &SourceBuildContext,
) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
    Ok(Box::new(github_source_from_context(context)?))
}

/// The concrete source [`build_github_source`] boxes, so the parsed
/// `[source]` knobs stay observable.
///
/// # Errors
///
/// A missing or mistyped `[source]` key, or a source that fails
/// construction.
pub(crate) fn github_source_from_context(
    context: &SourceBuildContext,
) -> Result<GithubWorkSource, SourceRegistryError> {
    let repo = context.required_string(GITHUB_KIND, "repo")?;
    let label = context.required_string(GITHUB_KIND, "label")?;
    let done_action = optional_string(context, GITHUB_KIND, "done_action", "comment")?
        .parse::<DoneAction>()
        .map_err(|err| SourceRegistryError::Config {
            kind: GITHUB_KIND.to_string(),
            detail: format!("[source] {err}"),
        })?;
    let require_triage_receipt = optional_bool(context, GITHUB_KIND, "require_triage_receipt")?;
    let mut source = GithubWorkSource::new(repo, label)
        .map_err(|err| build_error(GITHUB_KIND, err))?
        .with_default_graders(context.default_graders.clone())
        .with_done_action(done_action)
        .with_require_triage_receipt(require_triage_receipt);
    if let Some(log) = &context.log {
        source = source.with_log(Arc::clone(log));
    }
    if let Some(sink) = &context.stop_sink {
        source = source.with_stop_sink(Arc::clone(sink));
    }
    Ok(source)
}

/// Register the three GitHub kinds (`github`, `github_ci`,
/// `github_review`) into `registry` with the shared [`crate::source_registry::SourceFactory`]
/// signature. Re-registering replaces in place, so calling this on the
/// default registry is idempotent.
pub fn register_github_sources(registry: &mut SourceRegistry) {
    registry.register(
        GITHUB_KIND,
        "labeled open GitHub issues via the gh CLI",
        Arc::new(build_github_source),
    );
    registry.register(
        GITHUB_CI_KIND,
        "failed GitHub Actions runs via the gh CLI",
        Arc::new(build_github_ci_source),
    );
    registry.register(
        GITHUB_REVIEW_KIND,
        "unresolved GitHub PR review threads via the gh CLI",
        Arc::new(build_github_review_source),
    );
}

#[cfg(test)]
pub(crate) mod test_support {
    //! The scripted `gh` runner the three adapters' tests share.

    use super::{GhOutput, GhRunner};
    use crate::sources::{GraderReceipt, LogSink, StopEventSink, WorkSourceError};
    use flywheel_core::task::{CommandGrader, Grader, GraderType};
    use std::sync::{Arc, Mutex};

    /// Records every argv and answers with canned output (or a scripted
    /// failure) so the real `gh` binary is never invoked.
    pub struct FakeGh {
        pub stdout: String,
        pub exit_code: Option<i32>,
        pub stderr: String,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeGh {
        pub fn new(stdout: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                stdout: stdout.into(),
                exit_code: Some(0),
                stderr: String::new(),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// A runner whose every call exits non-zero with `stderr`.
        pub fn failing(stderr: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                stdout: String::new(),
                exit_code: Some(1),
                stderr: stderr.into(),
                calls: Mutex::new(Vec::new()),
            })
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        /// The single recorded argv.
        pub fn only_call(&self) -> Vec<String> {
            let calls = self.calls();
            assert_eq!(calls.len(), 1, "expected exactly one gh call: {calls:?}");
            calls.into_iter().next().unwrap()
        }
    }

    impl GhRunner for FakeGh {
        fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            Ok(GhOutput {
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
                exit_code: self.exit_code,
            })
        }
    }

    /// The value following `flag` in `argv`.
    pub fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let idx = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"));
        &argv[idx + 1]
    }

    pub fn capture_log() -> (LogSink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let sink: LogSink = Arc::new(move |line: &str| {
            sink_lines.lock().unwrap().push(line.to_string());
        });
        (sink, lines)
    }

    pub type StopEvents = Arc<Mutex<Vec<(String, String, String)>>>;

    pub fn capture_stop_events() -> (StopEventSink, StopEvents) {
        let events: StopEvents = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let sink: StopEventSink = Arc::new(move |kind: &str, subject: &str, detail: &str| {
            sink_events.lock().unwrap().push((
                kind.to_string(),
                subject.to_string(),
                detail.to_string(),
            ));
        });
        (sink, events)
    }

    pub fn command_grader(run: &str) -> Grader {
        Grader::Command(CommandGrader::new(run).unwrap())
    }

    pub fn grader_run(grader: &Grader) -> &str {
        match grader {
            Grader::Command(g) => &g.run,
            other => panic!("expected a command grader, got {other:?}"),
        }
    }

    pub fn receipt(ordinal: u32, name: Option<&str>, passed: bool) -> GraderReceipt {
        GraderReceipt {
            ordinal,
            grader_type: GraderType::Command,
            name: name.map(str::to_string),
            passed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::source_registry::default_registry;
    use crate::sources::GraderReceipt;
    use crate::triage_receipt::RECEIPT_KEY;
    use flywheel_core::lifecycle::Status;
    use serde_json::json;

    fn issue(number: i64, title: &str, body: impl AsRef<str>) -> Value {
        json!({
            "number": number,
            "title": title,
            "body": body.as_ref(),
            "url": "https://github.com/octo/widgets/issues/1",
        })
    }

    fn default_issue(number: i64) -> Value {
        issue(number, "Fix the widget", "")
    }

    fn source(gh: &Arc<FakeGh>) -> GithubWorkSource {
        GithubWorkSource::new("octo/widgets", "flywheel")
            .unwrap()
            .with_runner(Arc::clone(gh) as Arc<dyn GhRunner>)
    }

    fn with_defaults(gh: &Arc<FakeGh>) -> GithubWorkSource {
        source(gh).with_default_graders(vec![command_grader("true")])
    }

    fn ids(items: &[WorkItem]) -> Vec<&str> {
        items.iter().map(WorkItem::id).collect()
    }

    fn report(status: Status, graders: Vec<GraderReceipt>) -> WorkReport {
        WorkReport {
            task_id: "gh-12".into(),
            source_ref: "octo/widgets#12".into(),
            run_id: "run-abc".into(),
            status,
            error: String::new(),
            graders,
        }
    }

    const SPEC_BODY: &str = "Some prose describing the bug.

```flywheel
{
  \"goal\": \"Widget retries flaky fetches.\",
  \"graders\": [{\"type\": \"command\", \"run\": \"uv run pytest tests/widget\"}],
  \"context\": {\"relevant\": [\"src/widget.py\"]},
  \"tags\": [\"reliability\"],
  \"prerequisites\": [\"gh-7\"]
}
```

More prose after the block.
";

    fn full_page() -> String {
        let issues: Vec<Value> = (1..=200)
            .map(|n| issue(n, "Fix the widget", "Please."))
            .collect();
        Value::Array(issues).to_string()
    }

    fn below_cap() -> String {
        let issues: Vec<Value> = (1..200)
            .map(|n| issue(n, "Fix the widget", "Please."))
            .collect();
        Value::Array(issues).to_string()
    }

    // --- inbound: list_work ------------------------------------------------

    #[test]
    fn list_work_queries_open_labeled_issues() {
        let gh = FakeGh::new("[]");
        assert!(source(&gh).list_work().unwrap().is_empty());
        let argv = gh.only_call();
        assert_eq!(&argv[..2], ["issue", "list"]);
        assert_eq!(arg_after(&argv, "--repo"), "octo/widgets");
        assert_eq!(arg_after(&argv, "--label"), "flywheel");
        assert_eq!(arg_after(&argv, "--state"), "open");
        assert_eq!(arg_after(&argv, "--json"), "number,title,body,url");
        assert_eq!(arg_after(&argv, "--limit"), "200");
    }

    #[test]
    fn spec_block_compiles_fully() {
        let gh = FakeGh::new(json!([issue(12, "Fix the widget", SPEC_BODY)]).to_string());
        let items = source(&gh).list_work().unwrap();
        let [item] = items.as_slice() else {
            panic!("expected one item, got {items:?}");
        };
        assert_eq!(item.task.id, "gh-12");
        assert_eq!(item.task.goal, "Widget retries flaky fetches.");
        assert_eq!(item.source_ref, "octo/widgets#12");
        assert!(item.local_path.is_none());
        assert_eq!(item.prerequisites, ["gh-7"]);
        assert_eq!(item.task.tags, ["reliability"]);
        assert_eq!(
            grader_run(&item.task.graders[0]),
            "uv run pytest tests/widget"
        );
        assert_eq!(item.task.context.relevant, ["src/widget.py"]);
        assert!(
            item.task
                .context
                .notes
                .contains("Some prose describing the bug.")
        );
        assert!(
            item.task
                .context
                .references
                .contains(&"https://github.com/octo/widgets/issues/1".to_string())
        );
        assert_eq!(item.source_kind.as_deref(), Some("github_issue"));
        assert_eq!(
            item.source_url.as_deref(),
            Some("https://github.com/octo/widgets/issues/1")
        );
        assert_eq!(
            item.source_version.as_deref(),
            Some(sha256_hex(SPEC_BODY).as_str())
        );
    }

    #[test]
    fn issue_without_spec_uses_title_and_default_graders() {
        let gh = FakeGh::new(json!([issue(3, "Add caching", "Please.")]).to_string());
        let items = with_defaults(&gh).list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].task.id, "gh-3");
        assert_eq!(items[0].task.goal, "Add caching");
        assert_eq!(grader_run(&items[0].task.graders[0]), "true");
        assert_eq!(items[0].task.context.notes, "Please.");
    }

    #[test]
    fn issue_without_any_graders_is_skipped_and_logged() {
        let gh = FakeGh::new(json!([issue(4, "Vague wish", "")]).to_string());
        let (log, lines) = capture_log();
        let src = source(&gh).with_log(log);
        assert!(src.list_work().unwrap().is_empty());
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("octo/widgets#4"));
        assert!(lines[0].contains("not runnable"));
    }

    #[test]
    fn items_sort_by_issue_number() {
        let gh = FakeGh::new(json!([default_issue(20), default_issue(5)]).to_string());
        let items = with_defaults(&gh).list_work().unwrap();
        assert_eq!(ids(&items), ["gh-5", "gh-20"]);
    }

    #[test]
    fn invalid_spec_json_raises_instead_of_silent_fallback() {
        let body = "```flywheel\n{not json}\n```";
        let gh = FakeGh::new(json!([issue(9, "Fix the widget", body)]).to_string());
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("octo/widgets#9"), "{err}");
        assert!(err.contains("invalid JSON"), "{err}");
    }

    #[test]
    fn deeply_nested_spec_block_is_an_invalid_block_not_a_crash() {
        let depth = 20000;
        let body = format!(
            "```flywheel\n{}{}\n```",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let gh = FakeGh::new(json!([issue(9, "Fix the widget", body)]).to_string());
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("octo/widgets#9"), "{err}");
        assert!(err.contains("too deeply"), "{err}");
    }

    #[test]
    fn unclosed_spec_fence_raises() {
        let gh = FakeGh::new(json!([issue(9, "Fix the widget", "```flywheel\n{}")]).to_string());
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("not closed"), "{err}");
    }

    #[test]
    fn non_object_spec_block_raises() {
        let gh =
            FakeGh::new(json!([issue(9, "Fix the widget", "```flywheel\n[1]\n```")]).to_string());
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("must be a JSON object, got list"), "{err}");
    }

    #[test]
    fn invalid_list_payload_raises() {
        let gh = FakeGh::new("not json at all");
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("invalid JSON"), "{err}");
        let gh = FakeGh::new(json!({"unexpected": "object"}).to_string());
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.contains("returned dict, expected a list"), "{err}");
    }

    #[test]
    fn empty_stdout_is_an_empty_page() {
        let gh = FakeGh::new("");
        assert!(source(&gh).list_work().unwrap().is_empty());
    }

    #[test]
    fn full_page_emits_one_truncation_warning() {
        let gh = FakeGh::new(full_page());
        let (log, lines) = capture_log();
        let items = with_defaults(&gh).with_log(log).list_work().unwrap();
        assert_eq!(items.len(), 200);
        let lines = lines.lock().unwrap();
        let warnings: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("truncated at one page"))
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("[github]"));
        assert!(warnings[0].contains("some items were not read this pass"));
    }

    #[test]
    fn below_cap_emits_no_truncation_warning() {
        let gh = FakeGh::new(below_cap());
        let (log, lines) = capture_log();
        with_defaults(&gh).with_log(log).list_work().unwrap();
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .all(|l| !l.contains("truncated"))
        );
    }

    #[test]
    fn truncation_warning_and_stop_sink_are_side_channels() {
        let page = full_page();
        let (log, _lines) = capture_log();
        let (sink, _events) = capture_stop_events();
        let with_sinks = with_defaults(&FakeGh::new(page.clone()))
            .with_log(log)
            .with_stop_sink(sink)
            .list_work()
            .unwrap();
        let without = with_defaults(&FakeGh::new(page)).list_work().unwrap();
        assert_eq!(ids(&with_sinks), ids(&without));
        assert_eq!(without.len(), 200);
    }

    #[test]
    fn full_page_records_one_source_truncation_on_sink() {
        let gh = FakeGh::new(full_page());
        let (sink, events) = capture_stop_events();
        let src = with_defaults(&gh).with_stop_sink(sink);
        assert_eq!(src.list_work().unwrap().len(), 200);
        let events = events.lock().unwrap();
        let truncations: Vec<_> = events
            .iter()
            .filter(|(kind, _, _)| kind == "source-truncation")
            .collect();
        assert_eq!(truncations.len(), 1);
        let (_, subject, detail) = truncations[0];
        assert_eq!(subject, &src.source_name());
        assert!(detail.contains("truncated at one page"));
        assert!(detail.contains("some items were not read this pass"));
    }

    #[test]
    fn below_cap_records_no_source_truncation_on_sink() {
        let gh = FakeGh::new(below_cap());
        let (sink, events) = capture_stop_events();
        with_defaults(&gh).with_stop_sink(sink).list_work().unwrap();
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn zero_grader_issue_records_zero_grader_drop_on_sink() {
        let gh = FakeGh::new(json!([issue(4, "Vague wish", "")]).to_string());
        let (sink, events) = capture_stop_events();
        let src = source(&gh).with_stop_sink(sink);
        assert!(src.list_work().unwrap().is_empty());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let (kind, subject, detail) = &events[0];
        assert_eq!(kind, "zero-grader-drop");
        assert_eq!(subject, "octo/widgets");
        assert!(detail.contains("octo/widgets#4"));
        assert!(detail.contains("not runnable"));
    }

    #[test]
    fn failed_listing_raises_and_never_returns_empty() {
        let gh = FakeGh::failing("boom");
        let err = source(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("gh issue list"), "{err}");
        assert!(err.contains("failed (exit 1): boom"), "{err}");
    }

    #[test]
    fn run_gh_details_fall_back_to_stdout_then_no_output() {
        struct Quiet(Option<i32>, &'static str);
        impl GhRunner for Quiet {
            fn run_gh(&self, _argv: &[String]) -> Result<GhOutput, WorkSourceError> {
                Ok(GhOutput {
                    stdout: self.1.to_string(),
                    stderr: String::new(),
                    exit_code: self.0,
                })
            }
        }
        let err = run_gh(&Quiet(Some(2), " out "), &["x"])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "gh x failed (exit 2): out");
        let err = run_gh(&Quiet(None, ""), &["x", "y"])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "gh x y failed (exit signal): (no output)");
        assert_eq!(run_gh(&Quiet(Some(0), "fine"), &["x"]).unwrap(), "fine");
    }

    // --- skip-bad-item: one malformed payload never aborts the board ---------

    const BAD_JSON_BODY: &str = "```flywheel\n{not valid json}\n```";

    #[test]
    fn missing_number_issue_is_skipped_rest_return() {
        let payload = json!([
            issue(5, "Fix it", "Please."),
            {"title": "no number here", "body": "Please.", "url": ""},
            issue(20, "Fix it", "Please."),
        ]);
        let gh = FakeGh::new(payload.to_string());
        let (log, lines) = capture_log();
        let items = with_defaults(&gh).with_log(log).list_work().unwrap();
        assert_eq!(ids(&items), ["gh-5", "gh-20"]);
        let lines = lines.lock().unwrap();
        let skips: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("skipping malformed issue"))
            .collect();
        assert_eq!(skips.len(), 1);
        assert!(skips[0].contains("number"));
    }

    #[test]
    fn invalid_spec_block_issue_is_skipped_rest_return() {
        let payload = json!([
            issue(5, "Fix it", "Please."),
            issue(9, "Fix it", BAD_JSON_BODY),
            issue(20, "Fix it", "Please."),
        ]);
        let gh = FakeGh::new(payload.to_string());
        let (log, lines) = capture_log();
        let items = with_defaults(&gh).with_log(log).list_work().unwrap();
        assert_eq!(ids(&items), ["gh-5", "gh-20"]);
        let lines = lines.lock().unwrap();
        let skips: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("skipping malformed issue"))
            .collect();
        assert_eq!(skips.len(), 1);
        assert!(skips[0].contains("octo/widgets#9"));
    }

    #[test]
    fn one_bad_item_never_reads_as_empty_and_needs_no_log() {
        let payload = json!([
            issue(5, "Fix it", "Please."),
            issue(9, "Fix it", BAD_JSON_BODY)
        ]);
        let items = with_defaults(&FakeGh::new(payload.to_string()))
            .list_work()
            .unwrap();
        assert_eq!(ids(&items), ["gh-5"]);
    }

    #[test]
    fn all_bad_listing_still_aborts_loudly() {
        let payload = json!([issue(9, "Fix it", BAD_JSON_BODY)]);
        let err = with_defaults(&FakeGh::new(payload.to_string()))
            .list_work()
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("octo/widgets#9"), "{err}");
        assert!(err.contains("invalid JSON"), "{err}");
    }

    #[test]
    fn non_object_entries_are_ignored_and_bad_graders_are_branded() {
        let payload = json!([
            "junk",
            issue(5, "Fix it", "Please."),
            issue(
                6,
                "Fix it",
                "```flywheel\n{\"graders\": [{\"type\": \"nope\"}]}\n```"
            ),
        ]);
        let gh = FakeGh::new(payload.to_string());
        let (log, lines) = capture_log();
        let items = with_defaults(&gh).with_log(log).list_work().unwrap();
        assert_eq!(ids(&items), ["gh-5"]);
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("octo/widgets#6"), "{}", lines[0]);
    }

    #[test]
    fn spec_context_shape_errors_are_branded_and_scalars_coerce() {
        let body = "```flywheel\n{\"context\": 5}\n```";
        let gh = FakeGh::new(json!([issue(1, "T", body)]).to_string());
        let err = with_defaults(&gh).list_work().unwrap_err().to_string();
        assert_eq!(
            err,
            "octo/widgets#1: spec 'context' must be an object, got int"
        );

        let body = "```flywheel\n{\"context\": {\"relevant\": 7}}\n```";
        let gh = FakeGh::new(json!([issue(1, "T", body)]).to_string());
        let err = with_defaults(&gh).list_work().unwrap_err().to_string();
        assert_eq!(
            err,
            "octo/widgets#1: spec 'context.relevant' must be a list, got int"
        );

        let body = "```flywheel\n{\"goal\": 12, \"tags\": [1, true], \"prerequisites\": \"gh-2\", \
                    \"context\": {\"notes\": \"own notes\", \"references\": \
                    [\"https://github.com/octo/widgets/issues/1\"]}}\n```";
        let gh = FakeGh::new(json!([issue(1, "T", body)]).to_string());
        let items = with_defaults(&gh).list_work().unwrap();
        assert_eq!(items[0].task.goal, "12");
        assert_eq!(items[0].task.tags, ["1", "True"]);
        assert_eq!(items[0].prerequisites, ["gh-2"]);
        assert_eq!(items[0].task.context.notes, "own notes");
        // The URL is not appended twice when the spec already lists it.
        assert_eq!(items[0].task.context.references.len(), 1);
    }

    #[test]
    fn invalid_compiled_task_is_branded_with_source_ref() {
        let body = "```flywheel\n{\"goal\": \"   \"}\n```";
        let gh = FakeGh::new(json!([issue(1, "", body)]).to_string());
        let err = with_defaults(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("octo/widgets#1: goal"), "{err}");
    }

    // --- trust rule OFF: unchanged ------------------------------------------

    fn block(spec: Value) -> String {
        format!("```flywheel\n{spec}\n```")
    }

    fn receipt_for(title: &str, prose: &str, exit_code: i64) -> Value {
        json!({
            "command": "uv run pytest tests/widget",
            "exit_code": exit_code,
            "base_sha": "abc123",
            "content_hash": content_hash(title, prose),
        })
    }

    fn triaged_body(prose: &str, receipt: Option<Value>, graders: Option<Value>) -> String {
        let mut spec = Map::new();
        if let Some(receipt) = receipt {
            spec.insert(RECEIPT_KEY.into(), receipt);
        }
        if let Some(graders) = graders {
            spec.insert("graders".into(), graders);
        }
        format!("{prose}\n\n{}", block(Value::Object(spec)))
    }

    fn trusting(gh: &Arc<FakeGh>) -> GithubWorkSource {
        source(gh).with_require_triage_receipt(true)
    }

    const WIDGET_GRADERS: &str = r#"[{"type": "command", "run": "uv run pytest tests/widget"}]"#;

    fn widget_graders() -> Value {
        serde_json::from_str(WIDGET_GRADERS).unwrap()
    }

    #[test]
    fn trust_off_ignores_receipt_entirely() {
        let body = triaged_body("Widget drops retries.", None, Some(widget_graders()));
        let gh = FakeGh::new(json!([issue(6, "Fix the widget", body)]).to_string());
        let items = source(&gh).list_work().unwrap();
        assert_eq!(items[0].task.id, "gh-6");
        assert_eq!(
            grader_run(&items[0].task.graders[0]),
            "uv run pytest tests/widget"
        );
    }

    // --- trust rule ON ------------------------------------------------------

    #[test]
    fn valid_block_and_matching_receipt_yields_workitem() {
        let title = "Fix the widget";
        let prose = "Widget drops retries under load.";
        let body = triaged_body(
            prose,
            Some(receipt_for(title, prose, 1)),
            Some(widget_graders()),
        );
        let gh = FakeGh::new(json!([issue(12, title, body)]).to_string());
        let (log, lines) = capture_log();
        let items = trusting(&gh).with_log(log).list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].task.id, "gh-12");
        assert_eq!(
            grader_run(&items[0].task.graders[0]),
            "uv run pytest tests/widget"
        );
        assert_eq!(items[0].source_ref, "octo/widgets#12");
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_receipt_without_block_graders_uses_defaults() {
        let title = "Fix the widget";
        let prose = "Widget drops retries.";
        let body = triaged_body(prose, Some(receipt_for(title, prose, 1)), None);
        let gh = FakeGh::new(json!([issue(8, title, body)]).to_string());
        let items = trusting(&gh)
            .with_default_graders(vec![command_grader("true")])
            .list_work()
            .unwrap();
        assert_eq!(items[0].task.id, "gh-8");
        assert_eq!(grader_run(&items[0].task.graders[0]), "true");
    }

    fn skip_line(gh: &Arc<FakeGh>) -> String {
        let (log, lines) = capture_log();
        let src = trusting(gh)
            .with_default_graders(vec![command_grader("true")])
            .with_log(log);
        assert!(src.list_work().unwrap().is_empty());
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines[0].clone()
    }

    #[test]
    fn missing_block_skipped_even_with_defaults() {
        let gh = FakeGh::new(json!([issue(4, "Vague wish", "Please.")]).to_string());
        let line = skip_line(&gh);
        assert!(line.contains("octo/widgets#4"));
        assert!(line.contains("trust mode"));
    }

    #[test]
    fn missing_receipt_skipped_even_with_defaults() {
        let body = triaged_body("Widget drops retries.", None, Some(widget_graders()));
        let gh = FakeGh::new(json!([issue(5, "Fix the widget", body)]).to_string());
        let line = skip_line(&gh);
        assert!(line.contains("octo/widgets#5"));
        assert!(line.contains("receipt"));
    }

    #[test]
    fn stale_receipt_after_body_edit_skipped() {
        let title = "Fix the widget";
        let original = "Widget drops retries.";
        let receipt = receipt_for(title, original, 1);
        let edited = format!("{original}\n\nEDIT: also handle request timeouts.");
        let body = format!("{edited}\n\n{}", block(json!({RECEIPT_KEY: receipt})));
        let gh = FakeGh::new(json!([issue(7, title, body)]).to_string());
        assert!(skip_line(&gh).contains("stale"));
    }

    #[test]
    fn title_edit_also_staleness_skips() {
        let prose = "Widget drops retries.";
        let receipt = receipt_for("Fix the widget", prose, 1);
        let body = format!("{prose}\n\n{}", block(json!({RECEIPT_KEY: receipt})));
        let gh = FakeGh::new(json!([issue(7, "Fix the widget NOW", body)]).to_string());
        assert!(skip_line(&gh).contains("stale"));
    }

    #[test]
    fn passing_receipt_is_not_fail_first_and_is_skipped() {
        let title = "Fix the widget";
        let prose = "Widget drops retries.";
        let body = format!(
            "{prose}\n\n{}",
            block(json!({RECEIPT_KEY: receipt_for(title, prose, 0)}))
        );
        let gh = FakeGh::new(json!([issue(9, title, body)]).to_string());
        assert!(skip_line(&gh).contains("fail-first"));
    }

    #[test]
    fn malformed_receipt_is_skipped() {
        let title = "Fix the widget";
        let prose = "Widget drops retries.";
        let receipt = json!({
            "command": "uv run pytest",
            "exit_code": "nonzero",
            "base_sha": "abc123",
            "content_hash": content_hash(title, prose),
        });
        let body = format!("{prose}\n\n{}", block(json!({RECEIPT_KEY: receipt})));
        let gh = FakeGh::new(json!([issue(10, title, body)]).to_string());
        assert!(skip_line(&gh).contains("malformed"));
    }

    #[test]
    fn trust_skip_does_not_require_a_log_sink() {
        let gh = FakeGh::new(json!([issue(4, "Vague wish", "Please.")]).to_string());
        let src = trusting(&gh).with_default_graders(vec![command_grader("true")]);
        assert!(src.list_work().unwrap().is_empty());
    }

    #[test]
    fn present_but_invalid_block_still_raises_in_trust_mode() {
        let gh = FakeGh::new(
            json!([issue(9, "Fix the widget", "```flywheel\n{not json}\n```")]).to_string(),
        );
        let err = trusting(&gh).list_work().unwrap_err().to_string();
        assert!(err.starts_with("octo/widgets#9"), "{err}");
        assert!(err.contains("invalid JSON"), "{err}");
    }

    #[test]
    fn trust_mode_schedules_only_the_receipted_issue() {
        let title = "Fix the widget";
        let prose = "Widget drops retries.";
        let good = issue(
            2,
            title,
            triaged_body(
                prose,
                Some(receipt_for(title, prose, 1)),
                Some(json!([{"type": "command", "run": "uv run pytest"}])),
            ),
        );
        let bare = issue(11, "Vague wish", "Please.");
        let gh = FakeGh::new(json!([good, bare]).to_string());
        let (log, lines) = capture_log();
        let items = trusting(&gh)
            .with_default_graders(vec![command_grader("true")])
            .with_log(log)
            .list_work()
            .unwrap();
        assert_eq!(ids(&items), ["gh-2"]);
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("octo/widgets#11"));
    }

    // --- outbound: report ---------------------------------------------------

    #[test]
    fn report_comments_with_receipts() {
        let gh = FakeGh::new("");
        let receipts = vec![receipt(0, Some("tests"), true), receipt(1, None, false)];
        source(&gh)
            .report(&report(Status::FailedValidation, receipts))
            .unwrap();
        let argv = gh.only_call();
        assert_eq!(&argv[..3], ["issue", "comment", "12"]);
        assert_eq!(arg_after(&argv, "--repo"), "octo/widgets");
        let body = arg_after(&argv, "--body");
        assert!(body.contains("run `run-abc`"));
        assert!(body.contains("`failed_validation`"));
        assert!(body.contains("| 0 | tests | command | yes |"));
        assert!(body.contains("| 1 | - | command | no |"));
    }

    #[test]
    fn report_body_renders_error_and_empty_receipts() {
        let mut rep = report(Status::Failed, vec![]);
        rep.error = "agent crashed".into();
        let body = format_report_body(&rep);
        assert_eq!(
            body,
            "flywheel: run `run-abc` for task `gh-12` finished with status `failed`.\n\
             \nError: agent crashed\n\
             \nNo grader receipts were recorded for this run."
        );
    }

    #[test]
    fn report_closes_on_done_when_policy_says_close() {
        let gh = FakeGh::new("");
        let receipts = vec![receipt(0, Some("tests"), true), receipt(1, None, false)];
        source(&gh)
            .with_done_action(DoneAction::Close)
            .report(&report(Status::Done, receipts))
            .unwrap();
        let argv = gh.only_call();
        assert_eq!(&argv[..3], ["issue", "close", "12"]);
        assert_eq!(arg_after(&argv, "--repo"), "octo/widgets");
        let body = arg_after(&argv, "--comment");
        assert!(body.contains("run `run-abc`"));
        assert!(body.contains("`done`"));
        assert!(body.contains("| 0 | tests | command | yes |"));
        assert!(body.contains("| 1 | - | command | no |"));
    }

    #[test]
    fn report_comments_on_done_by_default_and_on_non_done_under_close() {
        let gh = FakeGh::new("");
        source(&gh).report(&report(Status::Done, vec![])).unwrap();
        assert_eq!(&gh.only_call()[..3], ["issue", "comment", "12"]);

        let gh = FakeGh::new("");
        source(&gh)
            .with_done_action(DoneAction::Close)
            .report(&report(Status::Failed, vec![]))
            .unwrap();
        assert_eq!(&gh.only_call()[..3], ["issue", "comment", "12"]);
    }

    #[test]
    fn report_with_malformed_source_ref_raises() {
        let gh = FakeGh::new("");
        let mut rep = report(Status::Done, vec![]);
        rep.source_ref = "no-number-here".into();
        let err = source(&gh).report(&rep).unwrap_err().to_string();
        assert!(err.contains("malformed github source_ref"), "{err}");
        rep.source_ref = "octo/widgets#12x".into();
        assert!(source(&gh).report(&rep).is_err());
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn report_propagates_gh_failure() {
        let gh = FakeGh::failing("rate limited");
        let err = source(&gh)
            .report(&report(Status::Done, vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("gh issue comment 12"), "{err}");
    }

    #[test]
    fn done_action_parses_and_rejects_unknown_values() {
        assert_eq!(
            "comment".parse::<DoneAction>().unwrap(),
            DoneAction::Comment
        );
        assert_eq!("close".parse::<DoneAction>().unwrap(), DoneAction::Close);
        let err = "merge".parse::<DoneAction>().unwrap_err().to_string();
        assert_eq!(
            err,
            "done_action must be 'comment' or 'close', got \"merge\""
        );
        assert_eq!(DoneAction::Close.to_string(), "close");
    }

    #[test]
    fn construction_validates_repo_and_label() {
        assert!(GithubWorkSource::new("", "flywheel").is_err());
        assert!(GithubWorkSource::new("octo/widgets", "").is_err());
        let src = GithubWorkSource::new("octo/widgets", "flywheel").unwrap();
        assert_eq!(src.source_kind(), "github_issue");
        assert_eq!(src.source_name(), "octo/widgets");
        assert_eq!(src.repo(), "octo/widgets");
        assert_eq!(src.label(), "flywheel");
        assert_eq!(src.done_action(), DoneAction::Comment);
        assert!(!src.require_triage_receipt());
        assert!(src.default_graders().is_empty());
        assert!(format!("{src:?}").contains("octo/widgets"));
    }

    // --- registry wiring ----------------------------------------------------

    fn settings(pairs: &[(&str, toml::Value)]) -> toml::Table {
        let mut table = toml::Table::new();
        for (key, value) in pairs {
            table.insert((*key).to_string(), value.clone());
        }
        table
    }

    fn s(text: &str) -> toml::Value {
        toml::Value::String(text.to_string())
    }

    #[test]
    fn register_github_sources_adds_all_three_kinds_in_order() {
        let mut registry = default_registry();
        register_github_sources(&mut registry);
        assert_eq!(
            registry.names(),
            ["directory", "github", "github_ci", "github_review"]
        );
        // Idempotent: a second registration replaces in place.
        register_github_sources(&mut registry);
        assert_eq!(registry.names().len(), 4);
    }

    #[test]
    fn github_factory_reads_settings_and_default_graders() {
        let mut registry = default_registry();
        register_github_sources(&mut registry);
        let ctx = SourceBuildContext::new(settings(&[
            ("kind", s("github")),
            ("repo", s("octo/widgets")),
            ("label", s("flywheel")),
            ("done_action", s("close")),
            ("require_triage_receipt", toml::Value::Boolean(true)),
        ]))
        .with_default_graders(vec![command_grader("true")]);
        let built = registry.build_from_context(&ctx).unwrap();
        assert_eq!(built.source_kind(), "github_issue");
        assert_eq!(built.source_name(), "octo/widgets");

        // The concrete builder exposes the parsed knobs.
        let concrete = build_github_source_concrete(&ctx);
        assert_eq!(concrete.done_action(), DoneAction::Close);
        assert!(concrete.require_triage_receipt());
        assert_eq!(concrete.default_graders().len(), 1);
    }

    fn build_github_source_concrete(ctx: &SourceBuildContext) -> GithubWorkSource {
        let repo = ctx.required_string(GITHUB_KIND, "repo").unwrap();
        let label = ctx.required_string(GITHUB_KIND, "label").unwrap();
        GithubWorkSource::new(repo, label)
            .unwrap()
            .with_default_graders(ctx.default_graders.clone())
            .with_done_action(
                optional_string(ctx, GITHUB_KIND, "done_action", "comment")
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
            .with_require_triage_receipt(
                optional_bool(ctx, GITHUB_KIND, "require_triage_receipt").unwrap(),
            )
    }

    #[test]
    fn github_factory_rejects_missing_or_mistyped_settings() {
        let mut registry = default_registry();
        register_github_sources(&mut registry);
        let err = registry
            .build(
                "github",
                &SourceBuildContext::new(settings(&[("repo", s("o/r"))])),
            )
            .err()
            .expect("build must fail")
            .to_string();
        assert_eq!(
            err,
            "cannot build github source: [source] label is required"
        );

        let err = registry
            .build(
                "github",
                &SourceBuildContext::new(settings(&[
                    ("repo", s("o/r")),
                    ("label", s("x")),
                    ("done_action", s("merge")),
                ])),
            )
            .err()
            .expect("build must fail")
            .to_string();
        assert!(
            err.contains("done_action must be 'comment' or 'close'"),
            "{err}"
        );

        let err = registry
            .build(
                "github",
                &SourceBuildContext::new(settings(&[
                    ("repo", s("o/r")),
                    ("label", s("x")),
                    ("require_triage_receipt", s("yes")),
                ])),
            )
            .err()
            .expect("build must fail")
            .to_string();
        assert!(
            err.contains("require_triage_receipt must be a boolean, got string"),
            "{err}"
        );

        let err = registry
            .build(
                "github",
                &SourceBuildContext::new(settings(&[("repo", s("")), ("label", s("x"))])),
            )
            .err()
            .expect("build must fail");
        assert!(matches!(err, SourceRegistryError::Build { .. }), "{err}");
    }

    #[test]
    fn github_factory_wires_log_and_stop_sinks() {
        let (log, lines) = capture_log();
        let (sink, events) = capture_stop_events();
        let ctx = SourceBuildContext::new(settings(&[
            ("repo", s("octo/widgets")),
            ("label", s("flywheel")),
        ]))
        .with_log(log)
        .with_stop_sink(sink);
        // Build through the factory, then swap the runner on a concrete
        // source built the same way to exercise the sinks end to end.
        assert!(build_github_source(&ctx).is_ok());
        let gh = FakeGh::new(json!([issue(4, "Vague wish", "")]).to_string());
        let src = GithubWorkSource::new("octo/widgets", "flywheel")
            .unwrap()
            .with_log(Arc::clone(ctx.log.as_ref().unwrap()))
            .with_stop_sink(Arc::clone(ctx.stop_sink.as_ref().unwrap()))
            .with_runner(gh as Arc<dyn GhRunner>);
        assert!(src.list_work().unwrap().is_empty());
        assert_eq!(lines.lock().unwrap().len(), 1);
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[test]
    fn process_runner_reports_a_missing_binary_as_an_error() {
        // Exercise the real runner's spawn-failure path without needing
        // gh: an argv is passed through verbatim and a missing executable
        // surfaces as a WorkSourceError rather than a panic.
        struct MissingBinary;
        impl GhRunner for MissingBinary {
            fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
                std::process::Command::new("flywheel-definitely-missing-binary")
                    .args(argv)
                    .output()
                    .map(|_| GhOutput::ok(""))
                    .map_err(|err| {
                        WorkSourceError::new(format!(
                            "gh {} failed to start: {err}",
                            argv.join(" ")
                        ))
                    })
            }
        }
        let err = run_gh(&MissingBinary, &["issue", "list"])
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("gh issue list failed to start"), "{err}");
        // The shipped runner is a unit value that can be constructed freely.
        let _runner: Arc<dyn GhRunner> = Arc::new(ProcessGhRunner);
    }

    // --- mutation-survey pins (graders fallback, goal shape, close errors,
    // factory wiring) -------------------------------------------------------

    #[test]
    fn null_spec_graders_fall_back_to_the_default_policy() {
        let body = block(json!({"goal": "Fix it", "graders": null}));
        let gh = FakeGh::new(json!([issue(3, "Fix the widget", body)]).to_string());
        let items = with_defaults(&gh).list_work().unwrap();
        assert_eq!(ids(&items), ["gh-3"]);
        let runs: Vec<&str> = items[0].task.graders.iter().map(grader_run).collect();
        assert_eq!(runs, ["true"]);
    }

    #[test]
    fn goal_is_trimmed_from_the_spec_block_and_from_the_title() {
        let spec_body = block(json!({"goal": "  Retry flaky fetches.\n"}));
        let gh = FakeGh::new(
            json!([
                issue(1, "ignored title", spec_body),
                issue(2, "  Fix the widget  ", "")
            ])
            .to_string(),
        );
        let items = with_defaults(&gh).list_work().unwrap();
        let goals: Vec<&str> = items.iter().map(|i| i.task.goal.as_str()).collect();
        assert_eq!(goals, ["Retry flaky fetches.", "Fix the widget"]);
    }

    #[test]
    fn report_close_propagates_gh_failure() {
        let gh = FakeGh::failing("issue already locked");
        let err = source(&gh)
            .with_done_action(DoneAction::Close)
            .report(&report(Status::Done, vec![]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh issue close 12"), "{err}");
        assert!(err.contains("issue already locked"), "{err}");
    }

    #[test]
    fn factory_wires_done_action_and_trust_flag_from_settings() {
        let ctx = SourceBuildContext::new(settings(&[
            ("repo", s("octo/widgets")),
            ("label", s("flywheel")),
            ("done_action", s("close")),
            ("require_triage_receipt", toml::Value::Boolean(true)),
        ]));
        let built = github_source_from_context(&ctx).unwrap();
        assert_eq!(built.done_action(), DoneAction::Close);
        assert!(built.require_triage_receipt());

        let ctx = SourceBuildContext::new(settings(&[
            ("repo", s("octo/widgets")),
            ("label", s("flywheel")),
        ]));
        let built = github_source_from_context(&ctx).unwrap();
        assert_eq!(built.done_action(), DoneAction::Comment);
        assert!(!built.require_triage_receipt());
    }
}
