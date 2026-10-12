//! Recovery summarizer runner -- fresh-session structured handoff.
//!
//! When the harness's context-recovery policy fires, the loop needs a
//! structured handoff that lets a fresh attempt resume without re-running
//! the prior attempt's discovery. This module produces that handoff via a
//! separate, fresh-context summarizer call -- exactly mirroring how the
//! rubric judge runs: the summarizer is never the working-agent invoker
//! (which is near capacity and degraded) and is fully driven by an injected
//! seam ([`SummarizerInvoke`]).
//!
//! Layered surface:
//!
//! * [`parse_handoff`] is pure -- no IO, no agent calls. The result taxonomy
//!   ([`HandoffResult`]) is closed so every input maps to exactly one
//!   variant (missing / truncated / duplicate / malformed / valid).
//! * [`build_summarizer_prompt`] renders the summarizer prompt; its text is
//!   byte-identical to the Python original.
//! * [`run_recovery_summarizer`] is the public async runner over the
//!   [`SummarizerInvoke`] seam; [`AgentsSummarizerInvoke`] is the shipped
//!   production invoker (a cold agent session through the `flywheel-agents`
//!   runtime with cwd set to the worktree, the full skill surface, auto
//!   permissions, and a turn cap -- no session inheritance). Tests inject a
//!   scripted one.
//!
//! Error contract: any failure to produce a parseable handoff -- an invoke
//! that fails, an empty/missing envelope, malformed JSON, missing fields --
//! is a [`RecoverySummarizerError`]. The runner never silently returns an
//! empty handoff; the harness routes the error through `internal_error`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::agents_invoke::ColdSession;
pub use crate::prompt::RecoveryHandoff;
use crate::task::Task;

// --- Handoff envelope -------------------------------------------------------

pub const OPENING_FENCE: &str = "<!-- RECOVERY_HANDOFF -->";
pub const CLOSING_FENCE: &str = "<!-- /RECOVERY_HANDOFF -->";

const REQUIRED_FIELDS: [&str; 4] = [
    "work_done",
    "work_remaining",
    "key_decisions",
    "suggested_next_step",
];

/// The closed set of parse outcomes: every input maps to exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffResult {
    Valid {
        handoff: RecoveryHandoff,
    },
    Missing,
    Truncated {
        detail: String,
    },
    Duplicate {
        count: usize,
    },
    Malformed {
        reason: String,
        offending: Option<String>,
    },
}

impl HandoffResult {
    pub const fn kind(&self) -> &'static str {
        match self {
            HandoffResult::Valid { .. } => "valid",
            HandoffResult::Missing => "missing",
            HandoffResult::Truncated { .. } => "truncated",
            HandoffResult::Duplicate { .. } => "duplicate",
            HandoffResult::Malformed { .. } => "malformed",
        }
    }

    pub fn as_valid(&self) -> Option<&RecoveryHandoff> {
        match self {
            HandoffResult::Valid { handoff } => Some(handoff),
            _ => None,
        }
    }

    /// The operator-facing reason for a non-valid result; `None` for
    /// [`HandoffResult::Valid`].
    pub fn failure_reason(&self) -> Option<String> {
        match self {
            HandoffResult::Valid { .. } => None,
            HandoffResult::Missing => Some("missing handoff envelope".to_owned()),
            HandoffResult::Truncated { detail } => {
                Some(format!("truncated handoff envelope: {detail}"))
            }
            HandoffResult::Duplicate { count } => {
                Some(format!("duplicate handoff envelopes (count={count})"))
            }
            HandoffResult::Malformed { reason, .. } => {
                Some(format!("malformed handoff envelope: {reason}"))
            }
        }
    }
}

const OFFENDING_LIMIT: usize = 200;

fn all_occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut start = 0;
    while let Some(i) = haystack[start..].find(needle) {
        positions.push(start + i);
        start += i + needle.len();
    }
    positions
}

fn truncate(payload: &str) -> String {
    payload.trim().chars().take(OFFENDING_LIMIT).collect()
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn malformed(reason: impl Into<String>, inner: &str) -> HandoffResult {
    HandoffResult::Malformed {
        reason: reason.into(),
        offending: Some(truncate(inner)),
    }
}

/// Parse the recovery-handoff envelope out of a summarizer response.
///
/// The contract is closed: every input maps to exactly one variant of
/// [`HandoffResult`]. Missing fences, duplicate fences, truncation, JSON
/// decode failure, missing fields, and any field-level shape violation are
/// all distinguishable failure modes rather than coerced into a default.
pub fn parse_handoff(text: &str) -> HandoffResult {
    let open_positions = all_occurrences(text, OPENING_FENCE);
    let close_positions = all_occurrences(text, CLOSING_FENCE);

    if open_positions.is_empty() {
        return HandoffResult::Missing;
    }
    if close_positions.is_empty() {
        return HandoffResult::Truncated {
            detail: "opening fence found without a matching closing fence".into(),
        };
    }
    if open_positions.len() > 1 || close_positions.len() > 1 {
        return HandoffResult::Duplicate {
            count: open_positions.len().max(close_positions.len()),
        };
    }

    let open_pos = open_positions[0];
    let close_pos = close_positions[0];
    if close_pos < open_pos {
        return HandoffResult::Truncated {
            detail: "closing fence appears before opening fence".into(),
        };
    }

    let inner = &text[open_pos + OPENING_FENCE.len()..close_pos];

    // serde_json bounds recursion depth (128 levels), so deeply nested
    // untrusted summarizer output surfaces as a decode error instead of a
    // stack overflow. Report it with the same reason the Python parser uses
    // for its RecursionError path so operators see one message.
    let data: Value = match serde_json::from_str(inner) {
        Ok(v) => v,
        Err(e) if e.to_string().starts_with("recursion limit exceeded") => {
            return malformed("handoff payload nests too deeply to parse", inner);
        }
        Err(e) => {
            return malformed(format!("handoff payload is not valid JSON: {e}"), inner);
        }
    };

    let Value::Object(data) = data else {
        return malformed(
            format!(
                "handoff payload must be a JSON object, got {}",
                json_type_name(&data)
            ),
            inner,
        );
    };

    let mut values: [&str; 4] = [""; 4];
    for (slot, field_name) in values.iter_mut().zip(REQUIRED_FIELDS) {
        let Some(raw) = data.get(field_name) else {
            return malformed(
                format!("handoff payload missing required '{field_name}' field"),
                inner,
            );
        };
        let Value::String(raw) = raw else {
            return malformed(
                format!(
                    "handoff '{field_name}' must be a string, got {}",
                    json_type_name(raw)
                ),
                inner,
            );
        };
        *slot = raw.as_str();
    }

    HandoffResult::Valid {
        handoff: RecoveryHandoff {
            work_done: values[0].to_owned(),
            work_remaining: values[1].to_owned(),
            key_decisions: values[2].to_owned(),
            suggested_next_step: values[3].to_owned(),
        },
    }
}

// --- Runner -----------------------------------------------------------------

/// Raised when the recovery summarizer cannot produce a parseable handoff.
///
/// Carries `reason` so the harness can render
/// `recovery summarizer failed: <reason>` without re-introspecting the
/// error. Invoker failures are wrapped into this type; parse failures
/// (missing / malformed / duplicate / truncated) are also surfaced through
/// it. The harness routes this error through `internal_error` -- no
/// recovery is attempted with an empty handoff.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("recovery summarizer failed: {reason}")]
pub struct RecoverySummarizerError {
    pub reason: String,
}

impl RecoverySummarizerError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Any failure an invoker can report. A [`RecoverySummarizerError`] inside
/// the box is re-raised verbatim by the runner; anything else is wrapped
/// with its `Display` text as the reason.
pub type SummarizerInvokeError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The boxed future a [`SummarizerInvoke`] returns.
pub type SummarizerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, SummarizerInvokeError>> + Send + 'a>>;

/// The summarizer seam: given the rendered prompt and the worktree the
/// summarizer should run in, produce the agent's full text response.
///
/// The production implementation starts a cold agent session (no session
/// inheritance from the working agent) with cwd set to `worktree`. Tests
/// supply a scripted implementation. Any `Fn(String, PathBuf) -> Fut`
/// closure whose future is `Send + 'static` implements the trait directly.
pub trait SummarizerInvoke: Send + Sync {
    fn invoke<'a>(&'a self, prompt: &'a str, worktree: &'a Path) -> SummarizerFuture<'a>;
}

impl<F, Fut> SummarizerInvoke for F
where
    F: Fn(String, PathBuf) -> Fut + Send + Sync,
    Fut: Future<Output = Result<String, SummarizerInvokeError>> + Send + 'static,
{
    fn invoke<'a>(&'a self, prompt: &'a str, worktree: &'a Path) -> SummarizerFuture<'a> {
        Box::pin(self(prompt.to_owned(), worktree.to_path_buf()))
    }
}

/// The envelope contract appended to every summarizer prompt.
pub const HANDOFF_CONTRACT: &str = "# Handoff envelope

End your response with exactly one fenced JSON block of the form:

<!-- RECOVERY_HANDOFF -->
{\"work_done\": \"<...>\", \"work_remaining\": \"<...>\", \"key_decisions\": \"<...>\", \"suggested_next_step\": \"<...>\"}
<!-- /RECOVERY_HANDOFF -->

Fields (all required, strings; empty string accepted when a slot has nothing to report):
- `work_done`: concrete progress made by the prior attempt (files touched, behaviors implemented, tests added).
- `work_remaining`: what still has to happen for the task goal to be reached.
- `key_decisions`: design choices, trade-offs, or constraints the resuming agent must respect.
- `suggested_next_step`: the single next action the resuming agent should take.

Emit the envelope exactly once. Missing, malformed, duplicate, or
truncated envelopes are treated as summarizer-infrastructure failures
and abort recovery.
";

/// Render the summarizer prompt from the task goal, the cumulative
/// diff/artifacts, and the recent transcript tail. Byte-deterministic.
pub fn build_summarizer_prompt(
    task: &Task,
    cumulative_diff: &str,
    transcript_tail: &str,
) -> String {
    format!(
        "# Role\n\n\
         You are the recovery summarizer for a coding agent whose prior \
         attempt approached its context-window capacity and was \
         force-finalized. Produce a structured handoff that lets a fresh \
         agent resume without re-discovering what the prior attempt \
         already learned.\n\n\
         # Goal\n\n{}\n\n\
         # Cumulative diff / artifacts\n\n{cumulative_diff}\n\n\
         # Recent transcript tail\n\n{transcript_tail}\n\n\
         {HANDOFF_CONTRACT}",
        task.goal
    )
}

/// Produce a structured recovery handoff from the prior attempt's state.
///
/// Assembles the summarizer prompt from the task goal, the cumulative
/// diff/artifacts, and the recent transcript tail; runs `summarizer_invoke`
/// against `worktree`; parses the response with [`parse_handoff`] and
/// returns the structured handoff.
///
/// A missing or empty `worktree`, an invoker failure, or any non-valid parse
/// result is a [`RecoverySummarizerError`] -- the runner never silently
/// returns an empty handoff. An invoker that fails with a
/// [`RecoverySummarizerError`] of its own has it re-raised verbatim rather
/// than wrapped a second time.
pub async fn run_recovery_summarizer(
    task: &Task,
    transcript_tail: &str,
    cumulative_diff: &str,
    worktree: Option<&Path>,
    summarizer_invoke: &dyn SummarizerInvoke,
) -> Result<RecoveryHandoff, RecoverySummarizerError> {
    let worktree = match worktree {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => return Err(RecoverySummarizerError::new("worktree not available")),
    };

    let prompt = build_summarizer_prompt(task, cumulative_diff, transcript_tail);

    let response = match summarizer_invoke.invoke(&prompt, worktree).await {
        Ok(response) => response,
        Err(err) => {
            return Err(match err.downcast::<RecoverySummarizerError>() {
                Ok(typed) => *typed,
                Err(other) => RecoverySummarizerError::new(other.to_string()),
            });
        }
    };

    match parse_handoff(&response) {
        HandoffResult::Valid { handoff } => Ok(handoff),
        other => Err(RecoverySummarizerError::new(
            other
                .failure_reason()
                .unwrap_or_else(|| "unexpected handoff variant".to_owned()),
        )),
    }
}

/// The shipped summarizer: a cold session through the `flywheel-agents`
/// runtime.
///
/// Every call starts fresh (no session inheritance from the working agent)
/// in the supplied worktree with the full skill surface, auto permissions,
/// and the session's turn cap; `summarizer_model` of `None` falls through to
/// the agent's own default. A failed run (crash, timeout, policy) is a
/// summarizer-infrastructure failure routed through
/// [`RecoverySummarizerError`] by the runner.
pub struct AgentsSummarizerInvoke {
    session: ColdSession,
}

impl std::fmt::Debug for AgentsSummarizerInvoke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentsSummarizerInvoke")
            .field("session", &self.session)
            .finish()
    }
}

impl AgentsSummarizerInvoke {
    /// A summarizer for `agent_id` on `summarizer_model`, capped at
    /// `summarizer_max_turns`.
    pub fn new(agent_id: &str, summarizer_model: Option<&str>, summarizer_max_turns: u32) -> Self {
        Self::with_session(ColdSession::new(
            agent_id,
            summarizer_model,
            summarizer_max_turns,
        ))
    }

    /// A summarizer over an explicitly configured [`ColdSession`].
    pub fn with_session(session: ColdSession) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &ColdSession {
        &self.session
    }
}

impl SummarizerInvoke for AgentsSummarizerInvoke {
    fn invoke<'a>(&'a self, prompt: &'a str, worktree: &'a Path) -> SummarizerFuture<'a> {
        Box::pin(async move {
            let output = self.session.run(prompt, worktree, None).await?;
            Ok(output.text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // Expected summarizer prompt generated by running the Python
    // `flywheel_core.recovery_summarizer._build_summarizer_prompt` on the
    // identical inputs `parity_summarizer_prompt` passes.

    const EXPECTED_SUMMARIZER_PROMPT: &str = r##"# Role

You are the recovery summarizer for a coding agent whose prior attempt approached its context-window capacity and was force-finalized. Produce a structured handoff that lets a fresh agent resume without re-discovering what the prior attempt already learned.

# Goal

Implement widget foo.

# Cumulative diff / artifacts

diff --git a/x b/x
+line

# Recent transcript tail

agent did stuff


# Handoff envelope

End your response with exactly one fenced JSON block of the form:

<!-- RECOVERY_HANDOFF -->
{"work_done": "<...>", "work_remaining": "<...>", "key_decisions": "<...>", "suggested_next_step": "<...>"}
<!-- /RECOVERY_HANDOFF -->

Fields (all required, strings; empty string accepted when a slot has nothing to report):
- `work_done`: concrete progress made by the prior attempt (files touched, behaviors implemented, tests added).
- `work_remaining`: what still has to happen for the task goal to be reached.
- `key_decisions`: design choices, trade-offs, or constraints the resuming agent must respect.
- `suggested_next_step`: the single next action the resuming agent should take.

Emit the envelope exactly once. Missing, malformed, duplicate, or
truncated envelopes are treated as summarizer-infrastructure failures
and abort recovery.
"##;
    fn wrap(payload: &str) -> String {
        format!("{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}")
    }

    fn payload(work_done: &str, work_remaining: &str, key_decisions: &str, next: &str) -> String {
        serde_json::json!({
            "work_done": work_done,
            "work_remaining": work_remaining,
            "key_decisions": key_decisions,
            "suggested_next_step": next,
        })
        .to_string()
    }

    fn well_formed() -> String {
        payload(
            "implemented module X",
            "wire harness",
            "fresh-context summarizer",
            "run pytest",
        )
    }

    fn task(goal: &str) -> Task {
        Task::new(goal, vec![])
    }

    /// Scripted invoker: returns canned responses in order and records
    /// every `(prompt, worktree)` it was handed.
    struct Scripted {
        responses: Mutex<VecDeque<String>>,
        calls: Mutex<Vec<(String, PathBuf)>>,
    }

    impl Scripted {
        fn new(responses: impl IntoIterator<Item = String>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, PathBuf)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SummarizerInvoke for Scripted {
        fn invoke<'a>(&'a self, prompt: &'a str, worktree: &'a Path) -> SummarizerFuture<'a> {
            self.calls
                .lock()
                .unwrap()
                .push((prompt.to_owned(), worktree.to_path_buf()));
            let next = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted summarizer ran out of responses");
            Box::pin(async move { Ok(next) })
        }
    }

    /// Invoker that always fails with the supplied error.
    struct Raising(Mutex<Option<SummarizerInvokeError>>);

    impl SummarizerInvoke for Raising {
        fn invoke<'a>(&'a self, _prompt: &'a str, _worktree: &'a Path) -> SummarizerFuture<'a> {
            let err = self
                .0
                .lock()
                .unwrap()
                .take()
                .expect("raising summarizer fires once");
            Box::pin(async move { Err(err) })
        }
    }

    async fn run(
        invoker: &dyn SummarizerInvoke,
        worktree: Option<&Path>,
    ) -> Result<RecoveryHandoff, RecoverySummarizerError> {
        run_recovery_summarizer(&task("Implement feature foo."), "t", "d", worktree, invoker).await
    }

    // --- parse_handoff: well-formed ----------------------------------------

    #[test]
    fn parse_well_formed() {
        let result = parse_handoff(&wrap(&well_formed()));
        assert_eq!(result.kind(), "valid");
        let handoff = result.as_valid().unwrap();
        assert_eq!(handoff.work_done, "implemented module X");
        assert_eq!(handoff.work_remaining, "wire harness");
        assert_eq!(handoff.key_decisions, "fresh-context summarizer");
        assert_eq!(handoff.suggested_next_step, "run pytest");
        assert_eq!(result.failure_reason(), None);
    }

    #[test]
    fn parse_accepts_empty_string_fields() {
        // Structurally valid; the prompt renderer substitutes
        // "(none recorded)" downstream.
        let result = parse_handoff(&wrap(&payload("", "", "", "")));
        assert_eq!(result.as_valid().unwrap(), &RecoveryHandoff::default());
    }

    #[test]
    fn parse_ignores_extra_json_fields() {
        let text = wrap(
            r#"{"work_done":"wd","work_remaining":"wr","key_decisions":"kd","suggested_next_step":"ns","unexpected":7}"#,
        );
        assert_eq!(parse_handoff(&text).as_valid().unwrap().work_done, "wd");
    }

    #[test]
    fn parse_tolerates_prose_around_the_envelope() {
        let text = format!("thinking...\n{}\ntrailing", wrap(&well_formed()));
        assert_eq!(parse_handoff(&text).kind(), "valid");
    }

    // --- parse_handoff: failure modes --------------------------------------

    #[test]
    fn parse_missing_variants() {
        assert_eq!(parse_handoff(""), HandoffResult::Missing);
        assert_eq!(parse_handoff("just chatter"), HandoffResult::Missing);
        // Unlike the loop-status envelope, a closing fence on its own is
        // "missing" here: the summarizer never opened an envelope.
        let closing_only = format!("{}\n{CLOSING_FENCE}", well_formed());
        assert_eq!(parse_handoff(&closing_only), HandoffResult::Missing);
    }

    #[test]
    fn parse_truncated_variants() {
        let open_only = format!("{OPENING_FENCE}\n{}", well_formed());
        assert_eq!(
            parse_handoff(&open_only),
            HandoffResult::Truncated {
                detail: "opening fence found without a matching closing fence".into()
            }
        );
        let reversed = format!("{CLOSING_FENCE}{OPENING_FENCE}");
        assert_eq!(
            parse_handoff(&reversed),
            HandoffResult::Truncated {
                detail: "closing fence appears before opening fence".into()
            }
        );
    }

    #[test]
    fn parse_duplicate_fence_pairs() {
        let text = format!(
            "{}\n{}",
            wrap(&well_formed()),
            wrap(&payload("injected", "wr", "kd", "ns"))
        );
        assert_eq!(parse_handoff(&text), HandoffResult::Duplicate { count: 2 });
        let lopsided = format!("{OPENING_FENCE}{CLOSING_FENCE}{CLOSING_FENCE}");
        assert_eq!(
            parse_handoff(&lopsided),
            HandoffResult::Duplicate { count: 2 }
        );
    }

    #[test]
    fn parse_malformed_payloads() {
        let cases = [
            (r#"{"work_done": "wd","#.to_owned(), "valid JSON"),
            ("[1, 2, 3]".to_owned(), "JSON object, got array"),
            (
                r#"{"work_remaining":"wr","key_decisions":"kd","suggested_next_step":"ns"}"#.to_owned(),
                "missing required 'work_done' field",
            ),
            (
                r#"{"work_done":"wd","key_decisions":"kd","suggested_next_step":"ns"}"#.to_owned(),
                "missing required 'work_remaining' field",
            ),
            (
                r#"{"work_done":"wd","work_remaining":"wr","suggested_next_step":"ns"}"#.to_owned(),
                "missing required 'key_decisions' field",
            ),
            (
                r#"{"work_done":"wd","work_remaining":"wr","key_decisions":"kd"}"#.to_owned(),
                "missing required 'suggested_next_step' field",
            ),
            (
                r#"{"work_done":42,"work_remaining":"wr","key_decisions":"kd","suggested_next_step":"ns"}"#.to_owned(),
                "'work_done' must be a string, got number",
            ),
        ];
        for (inner, expect) in cases {
            match parse_handoff(&wrap(&inner)) {
                HandoffResult::Malformed { reason, offending } => {
                    assert!(reason.contains(expect), "{inner}: {reason}");
                    assert_eq!(offending.as_deref(), Some(inner.as_str()));
                }
                other => panic!("{inner}: expected malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_deeply_nested_payload_is_malformed_not_a_crash() {
        let depth = 20_000;
        let inner = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        match parse_handoff(&wrap(&inner)) {
            HandoffResult::Malformed { reason, offending } => {
                assert!(reason.contains("deep"), "{reason}");
                assert_eq!(offending.unwrap().len(), OFFENDING_LIMIT);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn handoff_result_json_shape_carries_kind_tag() {
        let valid = parse_handoff(&wrap(&well_formed()));
        let json = serde_json::to_value(&valid).unwrap();
        assert_eq!(json["kind"], "valid");
        assert_eq!(json["handoff"]["work_done"], "implemented module X");
        let back: HandoffResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, valid);
        assert_eq!(
            serde_json::to_value(HandoffResult::Missing).unwrap(),
            serde_json::json!({"kind": "missing"})
        );
    }

    // --- build_summarizer_prompt -------------------------------------------

    #[test]
    fn parity_summarizer_prompt() {
        let prompt = build_summarizer_prompt(
            &task("Implement widget foo."),
            "diff --git a/x b/x\n+line",
            "agent did stuff\n",
        );
        assert_eq!(prompt, EXPECTED_SUMMARIZER_PROMPT);
    }

    // --- run_recovery_summarizer: happy path -------------------------------

    #[tokio::test]
    async fn runner_returns_structured_handoff() {
        let summarizer = Scripted::new([wrap(&well_formed())]);
        let handoff = run_recovery_summarizer(
            &task("Implement widget foo."),
            "agent did stuff",
            "diff --git a b",
            Some(Path::new("/tmp/wt")),
            &summarizer,
        )
        .await
        .unwrap();
        assert_eq!(handoff.work_done, "implemented module X");
        assert_eq!(handoff.work_remaining, "wire harness");
        assert_eq!(handoff.key_decisions, "fresh-context summarizer");
        assert_eq!(handoff.suggested_next_step, "run pytest");
        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, PathBuf::from("/tmp/wt"));
    }

    #[tokio::test]
    async fn runner_prompt_contains_goal_diff_transcript_and_contract() {
        let summarizer = Scripted::new([wrap(&well_formed())]);
        run_recovery_summarizer(
            &task("GOAL_MARKER_implement_widget"),
            "TRANSCRIPT_TAIL_MARKER",
            "DIFF_MARKER_artifacts",
            Some(Path::new("/tmp/wt")),
            &summarizer,
        )
        .await
        .unwrap();
        let prompt = &summarizer.calls()[0].0;
        for needle in [
            "GOAL_MARKER_implement_widget",
            "DIFF_MARKER_artifacts",
            "TRANSCRIPT_TAIL_MARKER",
            OPENING_FENCE,
            CLOSING_FENCE,
        ] {
            assert!(prompt.contains(needle), "{needle}");
        }
        assert_eq!(
            prompt,
            &build_summarizer_prompt(
                &task("GOAL_MARKER_implement_widget"),
                "DIFF_MARKER_artifacts",
                "TRANSCRIPT_TAIL_MARKER"
            )
        );
    }

    #[tokio::test]
    async fn closure_implements_the_seam() {
        let canned = wrap(&well_formed());
        let invoker = move |prompt: String, worktree: PathBuf| {
            let canned = canned.clone();
            async move {
                assert!(prompt.starts_with("# Role"));
                assert_eq!(worktree, PathBuf::from("/tmp/wt-closure"));
                Ok(canned)
            }
        };
        let handoff = run(&invoker, Some(Path::new("/tmp/wt-closure")))
            .await
            .unwrap();
        assert_eq!(handoff.suggested_next_step, "run pytest");
    }

    // --- run_recovery_summarizer: failures wrap into the typed error -------

    #[tokio::test]
    async fn runner_wraps_invoker_failure() {
        let raising = Raising(Mutex::new(Some("network down".into())));
        let err = run(&raising, Some(Path::new("/tmp/wt"))).await.unwrap_err();
        assert_eq!(err.reason, "network down");
        assert_eq!(err.to_string(), "recovery summarizer failed: network down");
    }

    #[tokio::test]
    async fn runner_does_not_double_wrap_typed_error() {
        let inner = RecoverySummarizerError::new("inner reason");
        let raising = Raising(Mutex::new(Some(Box::new(inner.clone()))));
        let err = run(&raising, Some(Path::new("/tmp/wt"))).await.unwrap_err();
        assert_eq!(err, inner);
    }

    #[tokio::test]
    async fn runner_maps_every_parse_failure_to_a_reason() {
        let dup = format!("{}\n{}", wrap(&well_formed()), wrap(&well_formed()));
        let cases = [
            (String::new(), "missing handoff envelope"),
            (
                wrap(r#"{"work_done": "wd", "work_remaining""#),
                "malformed handoff envelope",
            ),
            (
                format!("{OPENING_FENCE}\n{}", well_formed()),
                "truncated handoff envelope",
            ),
            (dup, "duplicate handoff envelopes (count=2)"),
            (
                wrap(r#"{"work_done":"wd","work_remaining":"wr","key_decisions":"kd"}"#),
                "'suggested_next_step'",
            ),
        ];
        for (response, expect) in cases {
            let summarizer = Scripted::new([response.clone()]);
            let err = run(&summarizer, Some(Path::new("/tmp/wt")))
                .await
                .unwrap_err();
            assert!(err.reason.contains(expect), "{response:?}: {}", err.reason);
        }
    }

    // --- the shipped summarizer through the real runtime + fake CLI -------

    fn agents_summarizer(
        dir: &Path,
        scenario: &str,
        model: Option<&str>,
    ) -> AgentsSummarizerInvoke {
        use crate::agents_invoke::test_support::{fake_configuration, write_fake_claude};
        let script = write_fake_claude(dir);
        let mut configuration = fake_configuration(&script, scenario);
        configuration.model_id = model.map(str::to_owned);
        configuration.max_turns = Some(8);
        AgentsSummarizerInvoke::with_session(
            ColdSession::with_configuration(configuration).with_timeout_seconds(Some(30.0)),
        )
    }

    #[tokio::test]
    async fn agents_summarizer_runs_a_cold_session_on_the_configured_model() {
        let dir = tempfile::tempdir().unwrap();
        let summarizer = agents_summarizer(dir.path(), "handoff", Some("summarizer-model"));
        let handoff = run_recovery_summarizer(
            &task("Implement widget foo."),
            "tail",
            "diff",
            Some(dir.path()),
            &summarizer,
        )
        .await
        .unwrap();
        assert_eq!(handoff.work_done, "model=summarizer-model");
        assert_eq!(handoff.work_remaining, "wire harness");
        assert_eq!(handoff.suggested_next_step, "run tests");
        assert_eq!(summarizer.session().configuration().max_turns, Some(8));
    }

    #[tokio::test]
    async fn agents_summarizer_failures_route_through_the_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let no_envelope = agents_summarizer(dir.path(), "no_verdict", None);
        let err = run(&no_envelope, Some(dir.path())).await.unwrap_err();
        assert_eq!(err.reason, "missing handoff envelope");
        let crash = agents_summarizer(dir.path(), "crash", None);
        let err = run(&crash, Some(dir.path())).await.unwrap_err();
        assert!(err.reason.contains("agent_exit"), "{}", err.reason);
    }

    #[tokio::test]
    async fn runner_rejects_missing_or_empty_worktree_before_invoking() {
        for worktree in [None, Some(Path::new(""))] {
            let summarizer = Scripted::new([wrap(&well_formed())]);
            let err = run(&summarizer, worktree).await.unwrap_err();
            assert_eq!(err.reason, "worktree not available");
            assert!(summarizer.calls().is_empty());
        }
    }
}
