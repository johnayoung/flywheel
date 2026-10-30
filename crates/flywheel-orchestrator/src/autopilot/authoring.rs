//! Autopilot authoring: compile a selected finding into validated,
//! grader-bearing tasks (spec 00058).

use super::{AutopilotInvoker, Finding, extract_json_object, non_blank_str, trimmed_str};
use crate::autopilot_context::prompt_with_digest;
use flywheel_core::loaders::load_task_data;
use flywheel_core::task::{Grader, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

// --- Authoring: compile a finding into grader-bearing tasks ---------------------------
//
// The integrity crux of the phase. Each selected finding is compiled --
// entirely by an agent following the fw-spec / fw-plan contracts headlessly
// -- into one or more validated flywheel tasks. The authoritative grader on
// every emitted task must be either a pre-existing committed repo check or
// a registered held-out oracle, never a check the same task's own diff
// creates (criterion #8: the "entirely by agents" path must not emit
// self-attestation wearing a grader's clothes). A finding that cannot be
// lowered to such a task is DROPPED with a recorded reason, never written
// as a goal-only or grader-less stub (criterion #1).

/// The authoritative grader points at a check already committed in the repo.
pub const GRADER_SOURCE_REPO_COMMAND: &str = "repo_command";
/// The authoritative grader is a registered held-out oracle (out of the
/// agent's reach), declared by an absolute path outside the agent's
/// worktree.
pub const GRADER_SOURCE_HELD_OUT: &str = "held_out_oracle";

/// Where an emitted task's authoritative grader comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderSource {
    RepoCommand,
    HeldOutOracle,
}

impl GraderSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            GraderSource::RepoCommand => GRADER_SOURCE_REPO_COMMAND,
            GraderSource::HeldOutOracle => GRADER_SOURCE_HELD_OUT,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            GRADER_SOURCE_REPO_COMMAND => Some(GraderSource::RepoCommand),
            GRADER_SOURCE_HELD_OUT => Some(GraderSource::HeldOutOracle),
            _ => None,
        }
    }
}

/// Default turn budget for the agents-backed authoring invoker.
pub const DEFAULT_AUTHORING_MAX_TURNS: u32 = 120;

/// Commit-before-done discipline that `/fw-plan` bakes into hand-authored
/// tasks (fw-plan template). The autopilot authoring prompt does not, so
/// every emitted task gets it injected deterministically -- a task that
/// finishes with uncommitted work is verified green against a dirty tree
/// yet silently fails to land (the worker parks it). Phrasing mirrors the
/// fw-plan example constraint.
pub const COMMIT_BEFORE_DONE_CONSTRAINT: &str =
    "Commit the change with a clear message before reporting done";

/// One validated, grader-bearing task autopilot authored from a finding.
///
/// `task` is the compiled core [`Task`] (it loaded through the
/// authoritative validator and carries at least one grader).
/// `authoritative_grader` is the run string of the out-of-band command
/// grader the work lands on; `grader_source` records whether it is a
/// pre-existing repo check or a held-out oracle; `grader_target` is the
/// pre-existing check it resolves to. `creates_files` are the files the
/// task's own diff is expected to create -- recorded so the
/// self-attestation guard can prove the authoritative grader does not
/// target any of them. `assumptions` are the ambiguities the authoring
/// agent resolved itself (D-2), recorded with the emitted task rather than
/// edited into a spec.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedTask {
    pub finding: Finding,
    pub task: Task,
    pub authoritative_grader: String,
    pub grader_source: GraderSource,
    pub grader_target: String,
    pub creates_files: Vec<String>,
    pub assumptions: Vec<String>,
    pub held_out_oracle_path: Option<PathBuf>,
    pub prerequisites: Vec<String>,
}

impl EmittedTask {
    /// An emitted task with no creates/assumptions/prerequisites.
    pub fn new(
        finding: Finding,
        task: Task,
        authoritative_grader: impl Into<String>,
        grader_source: GraderSource,
        grader_target: impl Into<String>,
    ) -> Self {
        Self {
            finding,
            task,
            authoritative_grader: authoritative_grader.into(),
            grader_source,
            grader_target: grader_target.into(),
            creates_files: Vec::new(),
            assumptions: Vec::new(),
            held_out_oracle_path: None,
            prerequisites: Vec::new(),
        }
    }
}

/// A finding autopilot could not compile to a grader-bearing task.
///
/// Recorded with a `reason` instead of emitting a vaguer or grader-less
/// task (criterion #1 / D-2). The loop persists the reason so a clean repo
/// or an ungradeable finding is auditable rather than silently
/// filler-filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedFinding {
    pub finding: Finding,
    pub reason: String,
}

/// The outcome of compiling one or more findings: emitted tasks + drops.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AuthoringResult {
    pub emitted: Vec<EmittedTask>,
    pub dropped: Vec<DroppedFinding>,
}

impl AuthoringResult {
    fn dropped(finding: &Finding, reason: impl Into<String>) -> Self {
        Self {
            emitted: Vec::new(),
            dropped: vec![DroppedFinding {
                finding: finding.clone(),
                reason: reason.into(),
            }],
        }
    }
}

/// Build the headless authoring prompt for one selected finding.
///
/// Carries the load-bearing integrity rule: the authoritative grader must
/// be a pre-existing committed check (or a held-out oracle), never a check
/// this task's own diff creates. Ambiguity is resolved as a recorded
/// assumption, not by emitting a vaguer task (D-2).
pub fn authoring_prompt(finding: &Finding, repo_root: &Path) -> String {
    let evidence = if finding.evidence.is_empty() {
        "  (none)".to_owned()
    } else {
        finding
            .evidence
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are autopilot's headless authoring agent. Compile the finding below into \
         one or more flywheel tasks following the fw-spec and fw-plan contracts -- a \
         one-sentence goal and the strongest out-of-band grader you can express.\n\n\
         Repository: {}\n\
         Finding (Tier {}, {}): {}\n\
         Detail: {}\n\
         Evidence:\n{evidence}\n\n\
         HARD INTEGRITY RULES:\n\
         - Every emitted task must carry at least one grader.\n\
         - The authoritative grader MUST be either (a) a command that invokes a check \
         already committed in this repo (a test/lint/build command that exists BEFORE \
         this task runs), or (b) a registered held-out oracle by absolute path. It must \
         NEVER be a brand-new check this task's own diff creates -- that is \
         self-attestation.\n\
         - Resolve any ambiguity a human would be asked about as a recorded assumption; \
         never emit a vaguer task to dodge a question.\n\
         - If you cannot express an out-of-band grader for this finding, do not invent \
         one: set \"dropped\" to a one-sentence reason and emit no task.\n\n\
         Respond with exactly one fenced JSON block:\n\
         ```json\n\
         {{\n\
         \x20 \"tasks\": [\n\
         \x20   {{\n\
         \x20     \"task\": {{\"id\": \"kebab-id\", \"goal\": \"one sentence\", \
         \"graders\": [{{\"type\": \"command\", \"run\": \"<existing repo check>\"}}], \
         \"tags\": [], \"context\": {{}}}},\n\
         \x20     \"authoritative_grader\": \"<run string matching a command grader \
         above>\",\n\
         \x20     \"grader_source\": \"{GRADER_SOURCE_REPO_COMMAND}\",\n\
         \x20     \"grader_target\": \"<repo-relative path to the pre-existing check \
         the grader runs>\",\n\
         \x20     \"creates_files\": [\"<paths this task's diff will create>\"],\n\
         \x20     \"assumptions\": [\"<ambiguity you resolved yourself>\"]\n\
         \x20   }}\n\
         \x20 ],\n\
         \x20 \"dropped\": \"\"\n\
         }}\n\
         ```",
        repo_root.display(),
        finding.tier.value(),
        finding.tier.name(),
        finding.title,
        finding.detail,
    )
}

/// Inject the commit-before-done constraint on an authored task (in place).
///
/// Idempotent: a no-op when the task already carries a constraint
/// mentioning a commit, so an agent that authored its own commit discipline
/// is not duplicated. Constraints are advisory prompt text (the agent sees
/// them under `## Constraints`); the hard enforcement is the
/// landable-change gate.
fn ensure_commit_before_done(task: &mut Task) {
    if task
        .context
        .constraints
        .iter()
        .any(|c| c.to_lowercase().contains("commit"))
    {
        return;
    }
    task.context
        .constraints
        .push(COMMIT_BEFORE_DONE_CONSTRAINT.to_owned());
}

/// The string entries of a JSON array (non-strings skipped); empty when
/// the value is not an array.
fn string_entries(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Validate one authored-task entry into an [`EmittedTask`] or a drop
/// reason.
///
/// The checks are the deterministic enforcement of criteria #1 and #8 -- a
/// self-authored or grader-less entry is dropped, never emitted.
fn validate_task_entry(
    finding: &Finding,
    entry: &Value,
    repo_root: &Path,
) -> Result<EmittedTask, String> {
    let Value::Object(entry) = entry else {
        return Err("authored entry is not an object".to_owned());
    };
    let Some(task_data @ Value::Object(task_map)) = entry.get("task") else {
        return Err("authored entry has no task object".to_owned());
    };
    let mut task = load_task_data(task_data, &format!("autopilot:{}", finding.id))
        .map_err(|err| format!("authored task failed validation: {err}"))?;
    if task.graders.is_empty() {
        return Err("authored task carries no grader (criterion #1)".to_owned());
    }
    ensure_commit_before_done(&mut task);

    let Some(authoritative) = non_blank_str(entry, "authoritative_grader").map(str::to_owned)
    else {
        return Err("authored task names no authoritative grader".to_owned());
    };
    let is_command_run = task.graders.iter().any(|grader| match grader {
        Grader::Command(command) => command.run == authoritative,
        _ => false,
    });
    if !is_command_run {
        return Err("authoritative grader is not a command grader on the emitted task".to_owned());
    }

    let source = match entry.get("grader_source") {
        Some(Value::String(s)) => GraderSource::parse(s)
            .ok_or_else(|| format!("unknown grader_source {}", py_repr_str(s)))?,
        other => {
            return Err(format!(
                "unknown grader_source {}",
                other
                    .map(Value::to_string)
                    .unwrap_or_else(|| "None".to_owned())
            ));
        }
    };
    let Some(target) = non_blank_str(entry, "grader_target").map(str::to_owned) else {
        return Err("authoritative grader names no pre-existing target".to_owned());
    };

    let creates = string_entries(entry.get("creates_files"));

    // Self-attestation guard (criterion #8): the authoritative grader must
    // not name -- nor resolve to -- a file this task's own diff creates.
    for created in &creates {
        if created.is_empty() {
            continue;
        }
        if authoritative.contains(created.as_str()) {
            return Err(
                "authoritative grader names a file this task's own diff creates \
                 (self-attestation)"
                    .to_owned(),
            );
        }
        if *created == target {
            return Err(
                "authoritative grader target is a file this task's own diff creates \
                 (self-attestation)"
                    .to_owned(),
            );
        }
    }

    let held_out_oracle_path = match source {
        GraderSource::RepoCommand => {
            // The grader must point at a check that pre-exists in the repo
            // -- proof it is out-of-band, not created by the run it grades.
            if !repo_root.join(&target).exists() {
                return Err(
                    "authoritative grader target does not pre-exist in the repo (not an \
                     out-of-band check)"
                        .to_owned(),
                );
            }
            None
        }
        GraderSource::HeldOutOracle => {
            let oracle = PathBuf::from(&target);
            if !oracle.is_absolute() || !oracle.exists() {
                return Err("held-out oracle target is not an existing absolute path".to_owned());
            }
            Some(oracle)
        }
    };

    let assumptions = string_entries(entry.get("assumptions"));

    // `prerequisites` is an orchestration-layer edge core drops on load, so
    // read it straight from the authored task source (mirroring the
    // directory work source) and carry it on the emitted task for emission
    // to preserve.
    let prerequisites = string_entries(task_map.get("prerequisites"));

    Ok(EmittedTask {
        finding: finding.clone(),
        task,
        authoritative_grader: authoritative,
        grader_source: source,
        grader_target: target,
        creates_files: creates,
        assumptions,
        held_out_oracle_path,
        prerequisites,
    })
}

/// Python `repr()` of a string: single-quoted unless it contains a single
/// quote and no double quote.
fn py_repr_str(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "\\'"))
    }
}

/// Compile one finding into emitted tasks (or a recorded drop).
///
/// Drives the authoring agent through the injectable seam, parses its
/// structured output, and validates every authored task against criteria
/// #1 and #8. A finding whose agent call fails, whose response is
/// unparseable, or that yields no valid grader-bearing task is dropped with
/// a recorded reason rather than written.
pub async fn author_finding(
    finding: &Finding,
    repo_root: &Path,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    author_finding_with(finding, repo_root, None, invoker).await
}

/// [`author_finding`] with the cycle's shared repo digest appended to the
/// authoring prompt ([`prompt_with_digest`]).
pub async fn author_finding_with(
    finding: &Finding,
    repo_root: &Path,
    digest: Option<&str>,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    let prompt = prompt_with_digest(authoring_prompt(finding, repo_root), digest);
    let text = match invoker.invoke(&prompt).await {
        Ok(text) => text,
        Err(err) => {
            return AuthoringResult::dropped(finding, format!("authoring agent error: {err}"));
        }
    };
    let data = match extract_json_object(&text) {
        Ok(data) => data,
        Err(err) => {
            return AuthoringResult::dropped(
                finding,
                format!("unparseable authoring response: {err}"),
            );
        }
    };

    let entries: &[Value] = match data.get("tasks") {
        Some(Value::Array(entries)) => entries,
        _ => &[],
    };
    let mut emitted = Vec::new();
    let mut drop_reasons = Vec::new();
    for entry in entries {
        match validate_task_entry(finding, entry, repo_root) {
            Ok(task) => emitted.push(task),
            Err(reason) => drop_reasons.push(reason),
        }
    }

    if !emitted.is_empty() {
        return AuthoringResult {
            emitted,
            dropped: Vec::new(),
        };
    }

    // No valid task: prefer the agent's own drop reason, else the first
    // validation failure, else a generic note.
    let reason = trimmed_str(&data, "dropped")
        .or_else(|| drop_reasons.into_iter().next())
        .unwrap_or_else(|| "no grader-bearing task authored".to_owned());
    AuthoringResult::dropped(finding, reason)
}

/// Author every finding in order; aggregate emitted tasks and drops.
pub async fn author_findings(
    findings: &[Finding],
    repo_root: &Path,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    let mut result = AuthoringResult::default();
    for finding in findings {
        let one = author_finding(finding, repo_root, invoker).await;
        result.emitted.extend(one.emitted);
        result.dropped.extend(one.dropped);
    }
    result
}
