//! Per-iteration prompt builder.
//!
//! Composes the agent prompt for a single iteration from a [`Task`], the
//! current [`Lifecycle`] state, and an [`IterationInputs`] bundle that
//! carries per-run parameters (the configured retry ceiling, carried-forward
//! reviewer findings, an optional recovery handoff) that are not on the Task
//! or Lifecycle themselves.
//!
//! Deterministic: the same `(task, lifecycle, iteration_inputs)` triple
//! produces byte-identical output -- no timestamps, no PIDs, no env-var
//! injection, no IO. The harness owns timing and delivery; this module owns
//! shape. The rendered text is hashed into `prompt_template_hash` on the
//! attempt's agent context, so every template here is byte-for-byte the
//! Python original.
//!
//! The prompt surfaces:
//!
//! - The task's `goal` and provided `context` fields.
//! - The task's `graders` -- including non-`command` types so the agent
//!   knows non-deterministic verification will follow.
//! - The lifecycle's current status, retry budget, last error, and full
//!   attempts history so the agent does not re-attempt already-failed
//!   approaches blindly.
//! - The iteration envelope contract from `docs/loop.md` -- the closed-enum
//!   `intent` (`verify`/`blocked`/`continue`/`abort`), the
//!   `<!-- LOOP_STATUS -->` fencing, and the mandatory `requires` predicate
//!   array that an `intent=blocked` envelope must carry (with the steer to
//!   use `abort` for an unsatisfiable/moot task that has nothing to re-check).
//!
//! Pure module: no IO, no SDK, no logging, no template-file reads.

use crate::envelope::{CLOSING_FENCE, Intent, OPENING_FENCE};
use crate::lifecycle::Lifecycle;
use crate::task::{Grader, Task};
use serde::{Deserialize, Serialize};

/// A single rubric finding carried forward into the next iteration.
///
/// Pure data: produced by the harness (which queries the grader-results
/// store) and consumed by [`build_iteration_prompt`] to render the
/// `# Reviewer feedback` section.
///
/// `ordinal` mirrors the grader's index in `task.graders` so the renderer
/// can deterministically interleave rubric and manual findings by
/// `(attempt_number, ordinal)`. It defaults to `0` so callers that only
/// build rubric lists in their preferred order keep that order (the sort is
/// stable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubricFindings {
    pub grader_name: String,
    pub attempt_number: u32,
    pub summary: String,
    #[serde(default)]
    pub ordinal: u32,
}

impl RubricFindings {
    pub fn new(
        grader_name: impl Into<String>,
        attempt_number: u32,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            grader_name: grader_name.into(),
            attempt_number,
            summary: summary.into(),
            ordinal: 0,
        }
    }

    pub fn with_ordinal(mut self, ordinal: u32) -> Self {
        self.ordinal = ordinal;
        self
    }
}

/// A single manual-grader rejection carried forward into the next
/// iteration.
///
/// Pure data sibling of [`RubricFindings`]: built by the harness from
/// failing `grader_type='manual'` receipts (operator rejections) and rendered
/// in the `# Reviewer feedback` section with an operator-distinguishing label
/// so the agent can tell a human "no" apart from a rubric verdict.
///
/// `ordinal` is the grader's index in `task.graders` (matching the manual
/// gate ordinal the resolver persisted to the receipt) and feeds the
/// renderer's `(attempt_number, ordinal)` ordering when rubric and manual
/// findings appear on the same attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualFinding {
    pub grader_name: String,
    pub attempt_number: u32,
    pub summary: String,
    #[serde(default)]
    pub ordinal: u32,
}

impl ManualFinding {
    pub fn new(
        grader_name: impl Into<String>,
        attempt_number: u32,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            grader_name: grader_name.into(),
            attempt_number,
            summary: summary.into(),
            ordinal: 0,
        }
    }

    pub fn with_ordinal(mut self, ordinal: u32) -> Self {
        self.ordinal = ordinal;
        self
    }
}

/// Structured handoff summary from a prior attempt that was terminated by
/// the harness's context-recovery policy.
///
/// Pure data: produced from a fresh-context summarizer call
/// (`crate::recovery_summarizer`) when the working attempt approaches the
/// agent's context-window capacity, then consumed by
/// [`build_iteration_prompt`] to render the `# Recovery handoff` section on
/// the recovery attempt's prompt so the resuming agent rebuilds situational
/// context from the summary rather than re-discovering it.
///
/// The four fields mirror the structured-handoff shape exactly (work done,
/// work remaining, key decisions, suggested next step).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecoveryHandoff {
    pub work_done: String,
    pub work_remaining: String,
    pub key_decisions: String,
    pub suggested_next_step: String,
}

/// Per-iteration parameters not carried by the Task or Lifecycle.
///
/// Kept as a struct so future fields (grader-result snapshots, prior
/// envelope intent, harness-supplied tool budgets) can be added without
/// changing the [`build_iteration_prompt`] signature.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IterationInputs {
    pub max_retries: u32,
    #[serde(default)]
    pub prior_rubric_findings: Vec<RubricFindings>,
    #[serde(default)]
    pub prior_manual_findings: Vec<ManualFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_handoff: Option<RecoveryHandoff>,
}

impl IterationInputs {
    /// Inputs with only the retry ceiling set: no carried-forward findings
    /// and no recovery handoff.
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            prior_rubric_findings: Vec::new(),
            prior_manual_findings: Vec::new(),
            recovery_handoff: None,
        }
    }
}

/// Render the iteration prompt as a single string.
///
/// The output is byte-identical for byte-identical inputs. Sections are
/// emitted in a fixed order, list-typed context fields are walked in
/// insertion order, and the envelope contract block is built from
/// [`Intent::ALL`] order rather than any set iteration.
pub fn build_iteration_prompt(
    task: &Task,
    lifecycle: &Lifecycle,
    iteration_inputs: &IterationInputs,
) -> String {
    let mut sections: Vec<String> = vec![section_goal(task)];

    if let Some(context_section) = section_context(task) {
        sections.push(context_section);
    }

    if let Some(handoff) = &iteration_inputs.recovery_handoff {
        sections.push(section_recovery_handoff(handoff));
    }

    if !iteration_inputs.prior_rubric_findings.is_empty()
        || !iteration_inputs.prior_manual_findings.is_empty()
    {
        sections.push(section_reviewer_feedback(
            &iteration_inputs.prior_rubric_findings,
            &iteration_inputs.prior_manual_findings,
        ));
    }

    sections.push(section_graders(task));
    sections.push(section_lifecycle(lifecycle, iteration_inputs));
    sections.push(section_envelope_contract());

    let mut prompt = sections.join("\n\n");
    prompt.push('\n');
    prompt
}

fn section_goal(task: &Task) -> String {
    format!("# Goal\n\n{}", task.goal)
}

fn section_context(task: &Task) -> Option<String> {
    let ctx = &task.context;
    let mut blocks: Vec<String> = Vec::new();
    if !ctx.relevant.is_empty() {
        blocks.push(bulleted("Relevant files", &ctx.relevant));
    }
    if !ctx.references.is_empty() {
        blocks.push(bulleted("References", &ctx.references));
    }
    if !ctx.constraints.is_empty() {
        blocks.push(bulleted("Constraints", &ctx.constraints));
    }
    if !ctx.non_goals.is_empty() {
        blocks.push(bulleted("Non-goals", &ctx.non_goals));
    }
    if !ctx.edge_cases.is_empty() {
        blocks.push(bulleted("Edge cases", &ctx.edge_cases));
    }
    if !ctx.notes.trim().is_empty() {
        blocks.push(format!("## Notes\n\n{}", ctx.notes));
    }
    if blocks.is_empty() {
        return None;
    }
    Some(format!("# Context\n\n{}", blocks.join("\n\n")))
}

fn bulleted(heading: &str, items: &[String]) -> String {
    let bullets = items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("## {heading}\n\n{bullets}")
}

/// Render the `# Recovery handoff` section from a structured summary
/// produced by the harness's context-recovery summarizer.
///
/// Field order and headings are fixed for byte-determinism. An empty field
/// renders as the `"(none recorded)"` placeholder so a missing subsection is
/// explicit in the prompt instead of collapsing into adjacent text.
fn section_recovery_handoff(handoff: &RecoveryHandoff) -> String {
    const PLACEHOLDER: &str = "(none recorded)";
    let or_placeholder = |value: &str| -> String {
        if value.is_empty() {
            PLACEHOLDER.to_owned()
        } else {
            value.to_owned()
        }
    };
    format!(
        "# Recovery handoff\n\n\
         A prior attempt approached the agent's context-window capacity \
         and was finalized by the harness. The structured summary below \
         is your starting point on a fresh context — continue from here \
         rather than re-running the prior attempt's discovery.\n\n\
         ## Work done\n\n{}\n\n\
         ## Work remaining\n\n{}\n\n\
         ## Key decisions\n\n{}\n\n\
         ## Suggested next step\n\n{}",
        or_placeholder(&handoff.work_done),
        or_placeholder(&handoff.work_remaining),
        or_placeholder(&handoff.key_decisions),
        or_placeholder(&handoff.suggested_next_step),
    )
}

/// A borrowed view over either finding kind so the reviewer-feedback
/// renderer can sort rubric and manual findings together.
#[derive(Clone, Copy)]
enum FindingRef<'a> {
    Rubric(&'a RubricFindings),
    Manual(&'a ManualFinding),
}

impl FindingRef<'_> {
    fn sort_key(self) -> (u32, u32) {
        match self {
            FindingRef::Rubric(f) => (f.attempt_number, f.ordinal),
            FindingRef::Manual(f) => (f.attempt_number, f.ordinal),
        }
    }

    fn bullet(self) -> String {
        match self {
            FindingRef::Manual(f) => {
                let summary = if f.summary.is_empty() {
                    "(no feedback provided)"
                } else {
                    f.summary.as_str()
                };
                format!("- manual `{}` (operator): {summary}", f.grader_name)
            }
            FindingRef::Rubric(f) => {
                let summary = if f.summary.is_empty() {
                    "(no summary provided)"
                } else {
                    f.summary.as_str()
                };
                format!("- rubric `{}`: {summary}", f.grader_name)
            }
        }
    }
}

/// Render the `# Reviewer feedback` section from rubric and manual findings.
///
/// Findings are grouped under `## attempt #N` subheadings in ascending
/// `attempt_number` order; within each attempt, bullets are emitted in
/// ascending `ordinal` order. The sort is stable, so two findings sharing a
/// `(attempt_number, ordinal)` key keep the order in which they were
/// supplied -- rubric findings before manual findings, each in caller order.
///
/// Rubric findings render as ``- rubric `name`: <summary>``; manual findings
/// render as ``- manual `name` (operator): <feedback>``. An empty summary
/// substitutes `"(no feedback provided)"` (manual) or
/// `"(no summary provided)"` (rubric).
fn section_reviewer_feedback(
    rubric_findings: &[RubricFindings],
    manual_findings: &[ManualFinding],
) -> String {
    let mut findings: Vec<FindingRef<'_>> = rubric_findings
        .iter()
        .map(FindingRef::Rubric)
        .chain(manual_findings.iter().map(FindingRef::Manual))
        .collect();
    findings.sort_by_key(|finding| finding.sort_key());

    let mut lines: Vec<String> = vec!["# Reviewer feedback".to_owned()];
    let mut current_attempt: Option<u32> = None;
    for finding in findings {
        let (attempt_number, _) = finding.sort_key();
        if current_attempt != Some(attempt_number) {
            current_attempt = Some(attempt_number);
            lines.push(String::new());
            lines.push(format!("## attempt #{attempt_number}"));
            lines.push(String::new());
        }
        lines.push(finding.bullet());
    }
    lines.join("\n")
}

fn section_graders(task: &Task) -> String {
    let mut lines: Vec<String> = vec![
        "# Verification".to_owned(),
        String::new(),
        "When you emit `intent=verify`, the harness runs these graders in".to_owned(),
        "list order. All must pass for the task to reach `done`.".to_owned(),
        String::new(),
    ];
    for (index, grader) in task.graders.iter().enumerate() {
        lines.push(grader_line(index + 1, grader));
    }
    lines.join("\n")
}

fn grader_line(index: usize, grader: &Grader) -> String {
    match grader {
        Grader::Command(g) => {
            let label = g.name.as_deref().unwrap_or("command");
            format!("{index}. command `{label}`: `{}`", g.run)
        }
        Grader::Rubric(g) => {
            let label = g.name.as_deref().unwrap_or("rubric");
            let assertions = g
                .assertions
                .iter()
                .map(|a| format!("   - {a}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{index}. rubric `{label}` (LLM-judged, agent cannot self-verify):\n{assertions}"
            )
        }
        Grader::Manual(g) => {
            let label = g.name.as_deref().unwrap_or("manual");
            format!(
                "{index}. manual `{label}` (operator approval required): {}",
                g.instruction
            )
        }
        Grader::Transcript(g) => {
            let label = g.name.as_deref().unwrap_or("transcript");
            let mut caps: Vec<String> = Vec::new();
            if let Some(max_turns) = g.max_turns {
                caps.push(format!("max_turns={max_turns}"));
            }
            if let Some(max_total_tokens) = g.max_total_tokens {
                caps.push(format!("max_total_tokens={max_total_tokens}"));
            }
            if let Some(max_wall_seconds) = g.max_wall_seconds {
                caps.push(format!("max_wall_seconds={max_wall_seconds}"));
            }
            format!(
                "{index}. transcript `{label}` (hard run-time caps): {}",
                caps.join(", ")
            )
        }
    }
}

fn section_lifecycle(lifecycle: &Lifecycle, iteration_inputs: &IterationInputs) -> String {
    let mut lines: Vec<String> = vec![
        "# Lifecycle state".to_owned(),
        String::new(),
        format!("Status: {}", lifecycle.status.as_str()),
        format!("Run id: {}", lifecycle.run_id),
        format!(
            "Retries used: {} / {}",
            lifecycle.retries, iteration_inputs.max_retries
        ),
    ];
    if !lifecycle.error.is_empty() {
        lines.push(format!("Last error: {}", lifecycle.error));
    }

    if lifecycle.attempts.is_empty() {
        lines.push(String::new());
        lines.push("No prior attempts on this task — this is the first iteration.".to_owned());
    } else {
        lines.push(String::new());
        lines.push("## Prior attempts".to_owned());
        lines.push(String::new());
        for attempt in &lifecycle.attempts {
            let outcome = attempt
                .outcome
                .map_or("pending", |outcome| outcome.as_str());
            let mut entry = format!(
                "- attempt #{} run={} outcome={outcome}",
                attempt.number, attempt.run_id
            );
            if !attempt.error.is_empty() {
                entry.push_str(&format!(" error={}", attempt.error));
            }
            lines.push(entry);
        }
        lines.push(String::new());
        lines.push("Do not blindly repeat approaches that already failed above.".to_owned());
    }

    lines.join("\n")
}

fn section_envelope_contract() -> String {
    let intent_list = Intent::ALL
        .iter()
        .map(|intent| format!("\"{}\"", intent.as_str()))
        .collect::<Vec<_>>()
        .join(" | ");
    let intent_bullets = Intent::ALL
        .iter()
        .map(|intent| format!("- `{}`: {}", intent.as_str(), intent_description(*intent)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Iteration envelope (required)\n\n\
         Every iteration MUST end with exactly one envelope, fenced like so:\n\n\
         {OPENING_FENCE}\n\
         {{\"intent\": {intent_list}, \"reason\": \"...\"}}\n\
         {CLOSING_FENCE}\n\n\
         `intent` is a closed enum — only these four values are accepted:\n\n\
         {intent_bullets}\n\n\
         ## `blocked` requires a `requires` array\n\n\
         An `intent=blocked` envelope MUST additionally carry a non-empty \
         `requires` array naming the machine-checkable predicate(s) that would \
         unblock it; the harness persists them and auto-resumes the lifecycle \
         once they all hold. Use `blocked` ONLY when such a predicate exists. \
         The three recognized shapes (and only these) are:\n\n\
         - `{{\"type\": \"command_grader\", \"name\": \"<a command grader on this \
         task>\"}}`\n\
         - `{{\"type\": \"file_exists\", \"path\": \"<repo-relative path>\", \
         \"present\": true}}`\n\
         - `{{\"type\": \"env_var_set\", \"name\": \"<ENV_VAR>\"}}`\n\n\
         Example:\n\n\
         {OPENING_FENCE}\n\
         {{\"intent\": \"blocked\", \"reason\": \"needs the integration database up\", \
         \"requires\": [{{\"type\": \"env_var_set\", \"name\": \"DATABASE_URL\"}}]}}\n\
         {CLOSING_FENCE}\n\n\
         If you cannot proceed and there is NOTHING for the harness to \
         re-check — the task's premise is false or already satisfied, the work \
         is already on the base, or no correct change is possible — emit \
         `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` \
         envelope with no `requires` array is a protocol failure that discards \
         your reasoning.\n\n\
         Missing, malformed, duplicated, or truncated envelopes are protocol \
         failures and may end the run."
    )
}

const fn intent_description(intent: Intent) -> &'static str {
    match intent {
        Intent::Verify => {
            "work claims to be complete; the harness runs the graders above. \
             Emit only when you believe every grader will pass."
        }
        Intent::Blocked => {
            "you cannot proceed until a concrete, RE-CHECKABLE external \
             prerequisite is met (a named command grader passing, a file \
             appearing, an env var being set). This intent REQUIRES a non-empty \
             `requires` array (see below) and pauses the lifecycle until the \
             harness re-checks those predicates. Do NOT use `blocked` for a task \
             you simply cannot complete — use `abort`."
        }
        Intent::Continue => "more iterations needed; carry context forward.",
        Intent::Abort => {
            "you cannot complete the task and there is nothing for the harness to \
             re-check — e.g. the task's premise is false or already satisfied, the \
             work is already on the base, or no correct change is possible without \
             fabricating one. The harness records your `reason` and stops (no \
             retry). Put your full diagnosis in `reason`."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{Attempt, Outcome, Status};
    use crate::task::{CommandGrader, Context, ManualGrader, RubricGrader, TranscriptGrader};
    use chrono::{DateTime, Utc};

    // Expected renderings generated by running the Python
    // `flywheel_core.prompt.build_iteration_prompt` on the identical inputs
    // the fixtures below construct. These pin byte parity: the prompt is
    // hashed into `prompt_template_hash`, so any drift here is a real
    // behavior change, not a cosmetic one.

    const EXPECTED_MINIMAL: &str = r##"# Goal

Add exponential backoff to the HTTP client.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`

# Lifecycle state

Status: pending
Run id: run-1
Retries used: 0 / 2

No prior attempts on this task — this is the first iteration.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_BRIEFED: &str = r##"# Goal

HTTP client retries 5xx and timeout failures with exponential backoff.

# Context

## Relevant files

- src/flywheel/http/client.py
- src/flywheel/http/config.py

## References

- src/flywheel/db/retry.py — mirror this structure

## Constraints

- Use stdlib + existing deps; no new packages

## Non-goals

- Don't touch tests outside tests/http

## Edge cases

- Respect Retry-After when the server provides it

## Notes

Backoff base delay is 100ms. See ADR-014 for the curve.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`
2. rubric `semantics` (LLM-judged, agent cannot self-verify):
   - Retries on 5xx and timeout errors only
   - Respects max_retries config (default 3)
3. transcript `budget` (hard run-time caps): max_turns=20, max_total_tokens=500000, max_wall_seconds=900
4. manual `ops-sign-off` (operator approval required): Confirm jitter algorithm fits upstream rate limits

# Lifecycle state

Status: pending
Run id: run-abc
Retries used: 0 / 3

No prior attempts on this task — this is the first iteration.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_UNNAMED: &str = r##"# Goal

Do a thing.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `command`: `uv run pytest`
2. rubric `rubric` (LLM-judged, agent cannot self-verify):
   - one
3. manual `manual` (operator approval required): Check it
4. transcript `transcript` (hard run-time caps): max_wall_seconds=30.5

# Lifecycle state

Status: pending
Run id: run-1
Retries used: 0 / 1

No prior attempts on this task — this is the first iteration.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_RETRY: &str = r##"# Goal

Add exponential backoff to the HTTP client.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`

# Lifecycle state

Status: ready
Run id: run-1
Retries used: 2 / 3

## Prior attempts

- attempt #1 run=run-1 outcome=validation_failed error=pytest failed: tests/http/test_retry.py::test_backoff
- attempt #2 run=run-1 outcome=validation_failed error=pytest failed: jitter clipped above ceiling
- attempt #3 run=run-1 outcome=pending

Do not blindly repeat approaches that already failed above.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_LAST_ERROR: &str = r##"# Goal

Add exponential backoff to the HTTP client.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`

# Lifecycle state

Status: failed_validation
Run id: run-2
Retries used: 1 / 3
Last error: grader command exited 1

## Prior attempts

- attempt #1 run=run-2 outcome=agent_error error=protocol failure: missing envelope

Do not blindly repeat approaches that already failed above.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_REVIEWER: &str = r##"# Goal

HTTP client retries 5xx and timeout failures with exponential backoff.

# Context

## Relevant files

- src/flywheel/http/client.py
- src/flywheel/http/config.py

## References

- src/flywheel/db/retry.py — mirror this structure

## Constraints

- Use stdlib + existing deps; no new packages

## Non-goals

- Don't touch tests outside tests/http

## Edge cases

- Respect Retry-After when the server provides it

## Notes

Backoff base delay is 100ms. See ADR-014 for the curve.

# Reviewer feedback

## attempt #1

- rubric `semantics-empty`: (no summary provided)
- rubric `semantics-attempt-1`: attempt-1 rubric
- manual `confirm-attempt-1` (operator): attempt-1 manual

## attempt #2

- manual `confirm-empty` (operator): (no feedback provided)
- rubric `semantics-attempt-2`: attempt-2 rubric
- manual `confirm-attempt-2` (operator): attempt-2 manual

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`
2. rubric `semantics` (LLM-judged, agent cannot self-verify):
   - Retries on 5xx and timeout errors only
   - Respects max_retries config (default 3)
3. transcript `budget` (hard run-time caps): max_turns=20, max_total_tokens=500000, max_wall_seconds=900
4. manual `ops-sign-off` (operator approval required): Confirm jitter algorithm fits upstream rate limits

# Lifecycle state

Status: pending
Run id: run-1
Retries used: 0 / 3

No prior attempts on this task — this is the first iteration.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_RECOVERY: &str = r##"# Goal

Add exponential backoff to the HTTP client.

# Recovery handoff

A prior attempt approached the agent's context-window capacity and was finalized by the harness. The structured summary below is your starting point on a fresh context — continue from here rather than re-running the prior attempt's discovery.

## Work done

Added backoff() in src/flywheel/http/client.py; tests/http/test_backoff.py written.

## Work remaining

Wire Retry-After parsing; run the full suite.

## Key decisions

(none recorded)

## Suggested next step

Run `uv run pytest tests/http` and fix the two failing cases.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`

# Lifecycle state

Status: ready
Run id: run-1
Retries used: 0 / 3

## Prior attempts

- attempt #1 run=run-1 outcome=recovered error=context window approaching capacity

Do not blindly repeat approaches that already failed above.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;

    const EXPECTED_BLOCKED_RESUME: &str = r##"# Goal

Add exponential backoff to the HTTP client.

# Verification

When you emit `intent=verify`, the harness runs these graders in
list order. All must pass for the task to reach `done`.

1. command `tests`: `uv run pytest tests/http`

# Lifecycle state

Status: ready
Run id: run-1
Retries used: 0 / 3

## Prior attempts

- attempt #1 run=run-1 outcome=cancelled error=needs the integration database up

Do not blindly repeat approaches that already failed above.

# Iteration envelope (required)

Every iteration MUST end with exactly one envelope, fenced like so:

<!-- LOOP_STATUS -->
{"intent": "verify" | "blocked" | "continue" | "abort", "reason": "..."}
<!-- /LOOP_STATUS -->

`intent` is a closed enum — only these four values are accepted:

- `verify`: work claims to be complete; the harness runs the graders above. Emit only when you believe every grader will pass.
- `blocked`: you cannot proceed until a concrete, RE-CHECKABLE external prerequisite is met (a named command grader passing, a file appearing, an env var being set). This intent REQUIRES a non-empty `requires` array (see below) and pauses the lifecycle until the harness re-checks those predicates. Do NOT use `blocked` for a task you simply cannot complete — use `abort`.
- `continue`: more iterations needed; carry context forward.
- `abort`: you cannot complete the task and there is nothing for the harness to re-check — e.g. the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible without fabricating one. The harness records your `reason` and stops (no retry). Put your full diagnosis in `reason`.

## `blocked` requires a `requires` array

An `intent=blocked` envelope MUST additionally carry a non-empty `requires` array naming the machine-checkable predicate(s) that would unblock it; the harness persists them and auto-resumes the lifecycle once they all hold. Use `blocked` ONLY when such a predicate exists. The three recognized shapes (and only these) are:

- `{"type": "command_grader", "name": "<a command grader on this task>"}`
- `{"type": "file_exists", "path": "<repo-relative path>", "present": true}`
- `{"type": "env_var_set", "name": "<ENV_VAR>"}`

Example:

<!-- LOOP_STATUS -->
{"intent": "blocked", "reason": "needs the integration database up", "requires": [{"type": "env_var_set", "name": "DATABASE_URL"}]}
<!-- /LOOP_STATUS -->

If you cannot proceed and there is NOTHING for the harness to re-check — the task's premise is false or already satisfied, the work is already on the base, or no correct change is possible — emit `abort` with your diagnosis in `reason`, NOT `blocked`. A `blocked` envelope with no `requires` array is a protocol failure that discards your reasoning.

Missing, malformed, duplicated, or truncated envelopes are protocol failures and may end the run.
"##;
    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-22T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cmd(run: &str, name: Option<&str>) -> Grader {
        Grader::Command(CommandGrader {
            run: run.to_owned(),
            name: name.map(str::to_owned),
        })
    }

    fn minimal_task() -> Task {
        Task::new(
            "Add exponential backoff to the HTTP client.",
            vec![cmd("uv run pytest tests/http", Some("tests"))],
        )
        .with_id("add-retry-logic")
    }

    fn briefed_task() -> Task {
        let mut task = Task::new(
            "HTTP client retries 5xx and timeout failures with exponential backoff.",
            vec![
                cmd("uv run pytest tests/http", Some("tests")),
                Grader::Rubric(RubricGrader {
                    assertions: vec![
                        "Retries on 5xx and timeout errors only".into(),
                        "Respects max_retries config (default 3)".into(),
                    ],
                    rubric: None,
                    name: Some("semantics".into()),
                    judge_model: None,
                    retry_on_fail: true,
                }),
                Grader::Transcript(TranscriptGrader {
                    max_turns: Some(20),
                    max_total_tokens: Some(500_000),
                    max_wall_seconds: Some(900.0),
                    name: Some("budget".into()),
                }),
                Grader::Manual(ManualGrader {
                    instruction: "Confirm jitter algorithm fits upstream rate limits".into(),
                    name: Some("ops-sign-off".into()),
                }),
            ],
        )
        .with_id("add-retry-logic");
        task.context = Context {
            relevant: vec![
                "src/flywheel/http/client.py".into(),
                "src/flywheel/http/config.py".into(),
            ],
            references: vec!["src/flywheel/db/retry.py — mirror this structure".into()],
            constraints: vec!["Use stdlib + existing deps; no new packages".into()],
            non_goals: vec!["Don't touch tests outside tests/http".into()],
            edge_cases: vec!["Respect Retry-After when the server provides it".into()],
            notes: "Backoff base delay is 100ms. See ADR-014 for the curve.".into(),
        };
        task
    }

    fn unnamed_task() -> Task {
        Task::new(
            "Do a thing.",
            vec![
                cmd("uv run pytest", None),
                Grader::Rubric(RubricGrader::new(vec!["one".into()]).unwrap()),
                Grader::Manual(ManualGrader::new("Check it").unwrap()),
                Grader::Transcript(TranscriptGrader {
                    max_wall_seconds: Some(30.5),
                    ..Default::default()
                }),
            ],
        )
        .with_id("unnamed")
    }

    fn attempt(number: u32, outcome: Option<Outcome>, error: &str, run_id: &str) -> Attempt {
        let mut attempt = Attempt::new(number, fixed_now(), run_id);
        attempt.ended_at = outcome.map(|_| fixed_now());
        attempt.outcome = outcome;
        attempt.error = error.to_owned();
        attempt
    }

    fn lifecycle(task: &Task, run_id: &str) -> Lifecycle {
        Lifecycle::new(task.id.clone()).with_run_id(run_id)
    }

    fn retried_lifecycle(task: &Task) -> Lifecycle {
        let mut lc = lifecycle(task, "run-1");
        lc.status = Status::Ready;
        lc.retries = 2;
        lc.attempts = vec![
            attempt(
                1,
                Some(Outcome::ValidationFailed),
                "pytest failed: tests/http/test_retry.py::test_backoff",
                "run-1",
            ),
            attempt(
                2,
                Some(Outcome::ValidationFailed),
                "pytest failed: jitter clipped above ceiling",
                "run-1",
            ),
            attempt(3, None, "", "run-1"),
        ];
        lc
    }

    fn mixed_findings_inputs() -> IterationInputs {
        IterationInputs {
            max_retries: 3,
            prior_rubric_findings: vec![
                RubricFindings::new("semantics-attempt-2", 2, "attempt-2 rubric").with_ordinal(1),
                RubricFindings::new("semantics-attempt-1", 1, "attempt-1 rubric").with_ordinal(1),
                RubricFindings::new("semantics-empty", 1, ""),
            ],
            prior_manual_findings: vec![
                ManualFinding::new("confirm-attempt-2", 2, "attempt-2 manual").with_ordinal(3),
                ManualFinding::new("confirm-attempt-1", 1, "attempt-1 manual").with_ordinal(3),
                ManualFinding::new("confirm-empty", 2, ""),
            ],
            recovery_handoff: None,
        }
    }

    // --- byte-parity fixtures (expected strings generated by Python) ------

    #[test]
    fn parity_minimal_task_first_iteration() {
        let task = minimal_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(2));
        assert_eq!(prompt, EXPECTED_MINIMAL);
    }

    #[test]
    fn parity_fully_briefed_task_every_context_field_and_grader_type() {
        let task = briefed_task();
        let prompt =
            build_iteration_prompt(&task, &lifecycle(&task, "run-abc"), &IterationInputs::new(3));
        assert_eq!(prompt, EXPECTED_BRIEFED);
    }

    #[test]
    fn parity_unnamed_graders_fall_back_to_type_labels() {
        let task = unnamed_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(1));
        assert_eq!(prompt, EXPECTED_UNNAMED);
    }

    #[test]
    fn parity_prior_attempt_feedback_with_pending_attempt() {
        let task = minimal_task();
        let prompt = build_iteration_prompt(&task, &retried_lifecycle(&task), &IterationInputs::new(3));
        assert_eq!(prompt, EXPECTED_RETRY);
    }

    #[test]
    fn parity_last_error_surfaces_on_failed_validation() {
        let task = minimal_task();
        let mut lc = lifecycle(&task, "run-2");
        lc.status = Status::FailedValidation;
        lc.retries = 1;
        lc.error = "grader command exited 1".into();
        lc.attempts = vec![attempt(
            1,
            Some(Outcome::AgentError),
            "protocol failure: missing envelope",
            "run-2",
        )];
        let prompt = build_iteration_prompt(&task, &lc, &IterationInputs::new(3));
        assert_eq!(prompt, EXPECTED_LAST_ERROR);
    }

    #[test]
    fn parity_reviewer_feedback_rubric_and_manual_interleaved() {
        let task = briefed_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &mixed_findings_inputs());
        assert_eq!(prompt, EXPECTED_REVIEWER);
    }

    #[test]
    fn parity_recovery_handoff_with_empty_slot_placeholder() {
        let task = minimal_task();
        let mut lc = lifecycle(&task, "run-1");
        lc.status = Status::Ready;
        lc.attempts = vec![attempt(
            1,
            Some(Outcome::Recovered),
            "context window approaching capacity",
            "run-1",
        )];
        let inputs = IterationInputs {
            recovery_handoff: Some(RecoveryHandoff {
                work_done: "Added backoff() in src/flywheel/http/client.py; tests/http/test_backoff.py written.".into(),
                work_remaining: "Wire Retry-After parsing; run the full suite.".into(),
                key_decisions: String::new(),
                suggested_next_step: "Run `uv run pytest tests/http` and fix the two failing cases.".into(),
            }),
            ..IterationInputs::new(3)
        };
        let prompt = build_iteration_prompt(&task, &lc, &inputs);
        assert_eq!(prompt, EXPECTED_RECOVERY);
    }

    #[test]
    fn parity_blocked_requires_resume() {
        // After a `blocked` envelope the harness finalizes the attempt
        // `cancelled` with the envelope reason, parks at `interrupted`, and
        // the recheck sweep applies `interrupted -> ready` (clearing the
        // requires snapshot). The resume prompt shows the cancelled attempt.
        let task = minimal_task();
        let mut lc = lifecycle(&task, "run-1");
        lc.status = Status::Ready;
        lc.attempts = vec![attempt(
            1,
            Some(Outcome::Cancelled),
            "needs the integration database up",
            "run-1",
        )];
        assert_eq!(lc.blocked_requires_json, None);
        let prompt = build_iteration_prompt(&task, &lc, &IterationInputs::new(3));
        assert_eq!(prompt, EXPECTED_BLOCKED_RESUME);
    }

    // --- behavioral tests ported from test_prompt.py ------------------------

    #[test]
    fn builder_is_deterministic_for_identical_inputs() {
        let task = briefed_task();
        let lc = lifecycle(&task, "run-abc");
        let inputs = IterationInputs::new(3);
        let first = build_iteration_prompt(&task, &lc, &inputs);
        let second = build_iteration_prompt(&task, &lc, &inputs);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn builder_omits_context_section_when_all_fields_are_empty() {
        let task = minimal_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(2));
        assert!(!prompt.contains("# Context"));
        assert!(prompt.contains(&task.goal));
        assert!(prompt.contains(OPENING_FENCE));
        for forbidden in [
            "Relevant files",
            "References",
            "Constraints",
            "Non-goals",
            "Edge cases",
            "Notes",
        ] {
            assert!(!prompt.contains(forbidden), "hallucinated heading {forbidden:?}");
        }
    }

    #[test]
    fn whitespace_only_notes_do_not_render_a_notes_block() {
        let mut task = minimal_task();
        task.context.notes = "   \n\t".into();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(2));
        assert!(!prompt.contains("# Context"));
    }

    #[test]
    fn envelope_contract_names_fences_and_every_intent() {
        let task = minimal_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(1));
        assert!(prompt.contains(OPENING_FENCE));
        assert!(prompt.contains(CLOSING_FENCE));
        for intent in Intent::ALL {
            assert!(prompt.contains(&format!("\"{}\"", intent.as_str())), "{intent}");
        }
        let lowered = prompt.to_lowercase();
        assert!(lowered.contains("closed enum"));
        assert!(lowered.contains("required"));
    }

    #[test]
    fn blocked_intent_documents_requires_contract() {
        let task = minimal_task();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(1));
        for needle in ["requires", "command_grader", "file_exists", "env_var_set", "\"requires\"", "abort"] {
            assert!(prompt.contains(needle), "{needle}");
        }
        let lowered = prompt.to_lowercase();
        assert!(lowered.contains("premise is false") || lowered.contains("already satisfied"));
    }

    #[test]
    fn first_attempt_prompt_differs_from_retry_prompt_for_same_task() {
        let task = minimal_task();
        let inputs = IterationInputs::new(3);
        let fresh = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        let retry = build_iteration_prompt(&task, &retried_lifecycle(&task), &inputs);
        assert_ne!(fresh, retry);
        assert!(fresh.contains("first iteration"));
        assert!(fresh.contains("Retries used: 0 / 3"));
        assert!(retry.contains("Prior attempts"));
        assert!(retry.contains("Retries used: 2 / 3"));
        assert!(retry.contains("jitter clipped above ceiling"));
        assert!(retry.contains("Do not blindly repeat"));
        assert!(retry.contains("outcome=pending"));
    }

    #[test]
    fn long_notes_are_not_silently_truncated() {
        let long_notes = "Sentence one. ".repeat(500);
        let mut task = Task::new("Do a thing.", vec![cmd("true", None)]).with_id("long-notes");
        task.context.notes = long_notes.clone();
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &IterationInputs::new(1));
        assert!(prompt.contains(&long_notes));
    }

    #[test]
    fn empty_finding_lists_are_byte_identical_to_bare_inputs() {
        let task = briefed_task();
        let lc = lifecycle(&task, "run-1");
        let bare = build_iteration_prompt(&task, &lc, &IterationInputs::new(3));
        let explicit = build_iteration_prompt(
            &task,
            &lc,
            &IterationInputs {
                max_retries: 3,
                prior_rubric_findings: vec![],
                prior_manual_findings: vec![],
                recovery_handoff: None,
            },
        );
        assert_eq!(bare, explicit);
        assert!(!bare.contains("# Reviewer feedback"));
        assert!(!bare.contains("# Recovery handoff"));
    }

    #[test]
    fn multiple_findings_same_attempt_keep_supplied_order_when_ordinals_tie() {
        let task = briefed_task();
        let inputs = IterationInputs {
            prior_rubric_findings: vec![
                RubricFindings::new("semantics", 1, "first"),
                RubricFindings::new("aaa-alpha-first-alphabetically", 1, "second"),
            ],
            ..IterationInputs::new(3)
        };
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        assert_eq!(prompt.matches("## attempt #1").count(), 1);
        let first = prompt.find("- rubric `semantics`: first").unwrap();
        let second = prompt
            .find("- rubric `aaa-alpha-first-alphabetically`: second")
            .unwrap();
        assert!(first < second);
    }

    #[test]
    fn findings_across_attempts_render_in_ascending_attempt_order() {
        let task = briefed_task();
        let inputs = IterationInputs {
            prior_rubric_findings: vec![
                RubricFindings::new("semantics", 2, "second-attempt finding"),
                RubricFindings::new("semantics", 1, "first-attempt finding"),
            ],
            ..IterationInputs::new(3)
        };
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        let one = prompt.find("## attempt #1").unwrap();
        let two = prompt.find("## attempt #2").unwrap();
        assert!(one < two);
        assert!(prompt.find("first-attempt finding").unwrap() < prompt.find("second-attempt finding").unwrap());
        assert_eq!(prompt.matches("## attempt #1").count(), 1);
        assert_eq!(prompt.matches("## attempt #2").count(), 1);
    }

    #[test]
    fn reviewer_feedback_appears_between_context_and_verification() {
        let task = briefed_task();
        let inputs = IterationInputs {
            prior_rubric_findings: vec![RubricFindings::new("semantics", 1, "something to review")],
            ..IterationInputs::new(3)
        };
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        let context = prompt.find("# Context").unwrap();
        let feedback = prompt.find("# Reviewer feedback").unwrap();
        let verification = prompt.find("# Verification").unwrap();
        assert!(context < feedback && feedback < verification);
    }

    #[test]
    fn manual_only_findings_render_operator_label_without_rubric_bullets() {
        let task = briefed_task();
        let inputs = IterationInputs {
            prior_manual_findings: vec![
                ManualFinding::new(
                    "confirm-migration",
                    1,
                    "The migration drops a column still read by the billing service.",
                )
                .with_ordinal(3),
            ],
            ..IterationInputs::new(3)
        };
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        assert!(prompt.contains("# Reviewer feedback"));
        assert!(prompt.contains(
            "- manual `confirm-migration` (operator): The migration drops a column still read by the billing service."
        ));
        assert!(!prompt.contains("- rubric"));
    }

    #[test]
    fn recovery_handoff_precedes_reviewer_feedback() {
        let task = briefed_task();
        let inputs = IterationInputs {
            recovery_handoff: Some(RecoveryHandoff {
                work_done: "wd".into(),
                work_remaining: "wr".into(),
                key_decisions: "kd".into(),
                suggested_next_step: "ns".into(),
            }),
            ..mixed_findings_inputs()
        };
        let prompt = build_iteration_prompt(&task, &lifecycle(&task, "run-1"), &inputs);
        let context = prompt.find("# Context").unwrap();
        let handoff = prompt.find("# Recovery handoff").unwrap();
        let feedback = prompt.find("# Reviewer feedback").unwrap();
        assert!(context < handoff && handoff < feedback);
        assert!(!prompt.contains("(none recorded)"));
    }

    #[test]
    fn transcript_grader_with_no_caps_renders_empty_cap_list() {
        // Not constructible through validation, but the renderer must not
        // panic on a struct built directly.
        let line = grader_line(
            2,
            &Grader::Transcript(TranscriptGrader::default()),
        );
        assert_eq!(line, "2. transcript `transcript` (hard run-time caps): ");
    }

    #[test]
    fn iteration_inputs_json_round_trip() {
        let inputs = mixed_findings_inputs();
        let json = serde_json::to_string(&inputs).unwrap();
        let back: IterationInputs = serde_json::from_str(&json).unwrap();
        assert_eq!(back, inputs);
        let bare: IterationInputs = serde_json::from_str(r#"{"max_retries":2}"#).unwrap();
        assert_eq!(bare, IterationInputs::new(2));
        let finding: RubricFindings = serde_json::from_str(
            r#"{"grader_name":"g","attempt_number":1,"summary":"s"}"#,
        )
        .unwrap();
        assert_eq!(finding.ordinal, 0);
    }
}
