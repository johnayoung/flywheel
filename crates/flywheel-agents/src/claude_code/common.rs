//! Shared Claude normalization: usage coercion and stop-reason mapping.

use crate::fold::number_as_i64;
use crate::models::{StopReason, Usage};
use serde_json::Value;

/// flywheel's four canonical token counters.
pub const USAGE_KEYS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
];

/// Project a native usage mapping onto the four canonical counters.
///
/// When any canonical key is present the result is zero-filled across all
/// four; otherwise it is empty. Bools and non-numbers are excluded.
pub fn coerce_usage(raw: Option<&Value>) -> Usage {
    let Some(Value::Object(map)) = raw else {
        return Usage::new();
    };
    let mut picked = Usage::new();
    for key in USAGE_KEYS {
        if let Some(n) = map.get(key).and_then(number_as_i64) {
            picked.insert(key.to_string(), n);
        }
    }
    if picked.is_empty() {
        return picked;
    }
    USAGE_KEYS
        .iter()
        .map(|key| (key.to_string(), picked.get(*key).copied().unwrap_or(0)))
        .collect()
}

/// Result subtypes are the most reliable stop signal in `--print` mode;
/// assistant `stop_reason` strings are the fallback.
pub fn normalize_stop(
    subtype: Option<&str>,
    native_stop: Option<&str>,
    is_error: bool,
) -> StopReason {
    match subtype {
        Some("success") => return StopReason::Completed,
        Some("error_max_turns") => return StopReason::MaxTurns,
        Some("error_during_execution") => return StopReason::Error,
        _ => {}
    }
    match native_stop {
        Some("end_turn") | Some("stop_sequence") => return StopReason::Completed,
        Some("max_tokens") => return StopReason::MaxTokens,
        _ => {}
    }
    if is_error {
        StopReason::Error
    } else {
        StopReason::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_zero_fills_canonical_keys() {
        let usage = coerce_usage(Some(&json!({"input_tokens": 12, "extra": 5})));
        assert_eq!(usage["input_tokens"], 12);
        assert_eq!(usage["output_tokens"], 0);
        assert_eq!(usage["cache_creation_input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 0);
        assert_eq!(usage.len(), 4);
    }

    #[test]
    fn usage_empty_when_no_canonical_key() {
        assert!(coerce_usage(Some(&json!({"extra": 5}))).is_empty());
        assert!(coerce_usage(Some(&json!("x"))).is_empty());
        assert!(coerce_usage(None).is_empty());
        assert!(coerce_usage(Some(&json!({"input_tokens": true}))).is_empty());
    }

    #[test]
    fn stop_precedence_subtype_then_native() {
        assert_eq!(
            normalize_stop(Some("success"), Some("max_tokens"), true),
            StopReason::Completed
        );
        assert_eq!(
            normalize_stop(Some("error_max_turns"), None, false),
            StopReason::MaxTurns
        );
        assert_eq!(
            normalize_stop(Some("error_during_execution"), None, false),
            StopReason::Error
        );
        assert_eq!(
            normalize_stop(Some("weird"), Some("end_turn"), false),
            StopReason::Completed
        );
        assert_eq!(
            normalize_stop(None, Some("stop_sequence"), false),
            StopReason::Completed
        );
        assert_eq!(
            normalize_stop(None, Some("max_tokens"), false),
            StopReason::MaxTokens
        );
        assert_eq!(
            normalize_stop(None, Some("tool_use"), true),
            StopReason::Error
        );
        assert_eq!(normalize_stop(None, None, false), StopReason::Unknown);
    }
}
