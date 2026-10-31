//! Claude-specific fault recognition.
//!
//! Two recognized forms of the session-limit refusal:
//!
//! - `"Claude AI usage limit reached|<epoch>"`: carries the absolute reset
//!   instant (epoch seconds, UTC) on `resets_at_epoch`.
//! - Any other `"usage limit reached"` text (e.g. the human `"resets 6pm"`
//!   variant): reported without `resets_at_epoch`; the wall-clock form is
//!   relative to the observer's timezone, so the caller derives the absolute
//!   reset with its own clock.

use crate::models::{AgentFault, FaultEvidence};
use regex::Regex;
use std::sync::OnceLock;

fn pipe_epoch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)usage limit reached\s*\|\s*(\d+)").expect("static regex"))
}

fn limit_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)usage limit reached").expect("static regex"))
}

pub fn classify_claude_fault(evidence: &FaultEvidence) -> Option<AgentFault> {
    let sources = [
        Some(evidence.final_text.as_str()),
        evidence.stderr.as_deref(),
    ];
    for source in sources.into_iter().flatten().filter(|s| !s.is_empty()) {
        if let Some(captures) = pipe_epoch_re().captures(source) {
            let resets_at = captures.get(1).and_then(|m| m.as_str().parse::<f64>().ok());
            return Some(AgentFault {
                kind: "session_limit".into(),
                message: "Claude usage limit reached (pipe-epoch refusal)".into(),
                resets_at_epoch: resets_at,
            });
        }
        if limit_marker_re().is_match(source) {
            return Some(AgentFault {
                kind: "session_limit".into(),
                message: "Claude usage limit reached".into(),
                resets_at_epoch: None,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(final_text: &str, stderr: Option<&str>) -> FaultEvidence {
        FaultEvidence {
            final_text: final_text.into(),
            stderr: stderr.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn pipe_epoch_refusal_carries_reset_instant() {
        let fault =
            classify_claude_fault(&evidence("Claude AI usage limit reached|1751990400", None))
                .unwrap();
        assert_eq!(fault.kind, "session_limit");
        assert_eq!(fault.resets_at_epoch, Some(1751990400.0));
    }

    #[test]
    fn pipe_epoch_tolerates_whitespace_around_the_pipe() {
        let fault = classify_claude_fault(&evidence(
            "Claude AI usage limit reached | 1751990400",
            None,
        ))
        .unwrap();
        assert_eq!(fault.resets_at_epoch, Some(1_751_990_400.0));
    }

    #[test]
    fn marker_without_epoch_has_no_reset() {
        let fault = classify_claude_fault(&evidence(
            "Claude AI usage limit reached -- resets 6pm",
            None,
        ))
        .unwrap();
        assert_eq!(fault.kind, "session_limit");
        assert_eq!(fault.resets_at_epoch, None);
    }

    #[test]
    fn match_in_stderr_with_empty_final_text() {
        let fault = classify_claude_fault(&evidence(
            "",
            Some("Claude AI usage limit reached|1751990400"),
        ))
        .unwrap();
        assert_eq!(fault.resets_at_epoch, Some(1751990400.0));
    }

    #[test]
    fn no_match_returns_none() {
        assert!(
            classify_claude_fault(&evidence(
                "all changes applied",
                Some("warning: slow network")
            ))
            .is_none()
        );
    }

    #[test]
    fn case_insensitive_marker() {
        assert!(classify_claude_fault(&evidence("USAGE LIMIT REACHED", None)).is_some());
    }
}
