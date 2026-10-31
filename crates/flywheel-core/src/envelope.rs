//! Iteration envelope parser.
//!
//! Extracts and validates the `<!-- LOOP_STATUS -->` envelope emitted at the
//! end of every agent iteration (see `docs/loop.md`). The envelope is
//! untrusted protocol input: malformed JSON, missing fences, duplicate
//! envelopes, truncation, and unknown intents are first-class outcomes,
//! never silent fallbacks.
//!
//! Pure module: string in, typed result out. No IO, no SDK, no logging.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const OPENING_FENCE: &str = "<!-- LOOP_STATUS -->";
pub const CLOSING_FENCE: &str = "<!-- /LOOP_STATUS -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Verify,
    Blocked,
    Continue,
    Abort,
}

impl Intent {
    pub const ALL: [Intent; 4] = [
        Intent::Verify,
        Intent::Blocked,
        Intent::Continue,
        Intent::Abort,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Intent::Verify => "verify",
            Intent::Blocked => "blocked",
            Intent::Continue => "continue",
            Intent::Abort => "abort",
        }
    }

    fn parse(s: &str) -> Option<Intent> {
        Intent::ALL.into_iter().find(|i| i.as_str() == s)
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Valid intent strings, sorted (for error messages).
pub const VALID_INTENTS: [&str; 4] = ["abort", "blocked", "continue", "verify"];
/// Valid `requires` predicate types, sorted (for error messages).
pub const VALID_REQUIREMENT_TYPES: [&str; 3] = ["command_grader", "env_var_set", "file_exists"];

/// A predicate describing what would unblock a `blocked` lifecycle. Exactly
/// three shapes are recognized in v1.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockedRequirement {
    /// A named command grader on the task must pass at recheck time.
    CommandGrader { name: String },
    /// A path must exist (`present=true`) or be absent (`present=false`).
    FileExists {
        path: String,
        #[serde(default = "default_present")]
        present: bool,
    },
    /// An environment variable must be set to a non-empty value.
    EnvVarSet { name: String },
}

fn default_present() -> bool {
    true
}

impl BlockedRequirement {
    pub const fn type_name(&self) -> &'static str {
        match self {
            BlockedRequirement::CommandGrader { .. } => "command_grader",
            BlockedRequirement::FileExists { .. } => "file_exists",
            BlockedRequirement::EnvVarSet { .. } => "env_var_set",
        }
    }
}

/// A well-formed envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidEnvelope {
    pub intent: Intent,
    pub reason: Option<String>,
    #[serde(default)]
    pub requires: Vec<BlockedRequirement>,
}

/// The closed set of parse outcomes: every input maps to exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvelopeResult {
    Valid(ValidEnvelope),
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

impl EnvelopeResult {
    pub const fn kind(&self) -> &'static str {
        match self {
            EnvelopeResult::Valid(_) => "valid",
            EnvelopeResult::Missing => "missing",
            EnvelopeResult::Truncated { .. } => "truncated",
            EnvelopeResult::Duplicate { .. } => "duplicate",
            EnvelopeResult::Malformed { .. } => "malformed",
        }
    }

    pub fn as_valid(&self) -> Option<&ValidEnvelope> {
        match self {
            EnvelopeResult::Valid(v) => Some(v),
            _ => None,
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

fn malformed(reason: impl Into<String>, inner: &str) -> EnvelopeResult {
    EnvelopeResult::Malformed {
        reason: reason.into(),
        offending: Some(truncate(inner)),
    }
}

fn parse_name_field(
    entry: &serde_json::Map<String, Value>,
    kind: &str,
    inner: &str,
) -> Result<String, EnvelopeResult> {
    let Some(name) = entry.get("name") else {
        return Err(malformed(
            format!("envelope 'requires' {kind} entry missing required 'name' field"),
            inner,
        ));
    };
    let Value::String(name) = name else {
        return Err(malformed(
            format!(
                "envelope 'requires' {kind} 'name' must be a string, got {}",
                json_type_name(name)
            ),
            inner,
        ));
    };
    if name.is_empty() {
        return Err(malformed(
            format!("envelope 'requires' {kind} 'name' must be a non-empty string"),
            inner,
        ));
    }
    Ok(name.clone())
}

fn parse_requirement(entry: &Value, inner: &str) -> Result<BlockedRequirement, EnvelopeResult> {
    let Value::Object(entry) = entry else {
        return Err(malformed(
            format!(
                "envelope 'requires' entries must each be a JSON object, got {}",
                json_type_name(entry)
            ),
            inner,
        ));
    };
    let Some(raw_type) = entry.get("type") else {
        return Err(malformed(
            "envelope 'requires' entry missing required 'type' field",
            inner,
        ));
    };
    let type_str = match raw_type {
        Value::String(s) if VALID_REQUIREMENT_TYPES.contains(&s.as_str()) => s.as_str(),
        other => {
            return Err(malformed(
                format!(
                    "envelope 'requires' entry 'type' must be one of {:?}, got {}",
                    VALID_REQUIREMENT_TYPES, other
                ),
                inner,
            ));
        }
    };
    match type_str {
        "command_grader" => {
            let name = parse_name_field(entry, "command_grader", inner)?;
            Ok(BlockedRequirement::CommandGrader { name })
        }
        "file_exists" => {
            let Some(path) = entry.get("path") else {
                return Err(malformed(
                    "envelope 'requires' file_exists entry missing required 'path' field",
                    inner,
                ));
            };
            let Value::String(path) = path else {
                return Err(malformed(
                    format!(
                        "envelope 'requires' file_exists 'path' must be a string, got {}",
                        json_type_name(path)
                    ),
                    inner,
                ));
            };
            if path.is_empty() {
                return Err(malformed(
                    "envelope 'requires' file_exists 'path' must be a non-empty string",
                    inner,
                ));
            }
            let present = match entry.get("present") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(malformed(
                        format!(
                            "envelope 'requires' file_exists 'present' must be a bool when provided, got {}",
                            json_type_name(other)
                        ),
                        inner,
                    ));
                }
            };
            Ok(BlockedRequirement::FileExists {
                path: path.clone(),
                present,
            })
        }
        _ => {
            let name = parse_name_field(entry, "env_var_set", inner)?;
            Ok(BlockedRequirement::EnvVarSet { name })
        }
    }
}

fn parse_requires(
    data: &serde_json::Map<String, Value>,
    inner: &str,
) -> Result<Vec<BlockedRequirement>, EnvelopeResult> {
    let Some(raw) = data.get("requires") else {
        return Err(malformed(
            "envelope payload with intent='blocked' missing required 'requires' field",
            inner,
        ));
    };
    let Value::Array(raw) = raw else {
        return Err(malformed(
            format!(
                "envelope 'requires' must be a list when intent='blocked', got {}",
                json_type_name(raw)
            ),
            inner,
        ));
    };
    if raw.is_empty() {
        return Err(malformed(
            "envelope 'requires' must contain at least one predicate when intent='blocked'",
            inner,
        ));
    }
    raw.iter()
        .map(|entry| parse_requirement(entry, inner))
        .collect()
}

/// Parse the loop-status envelope out of an agent iteration's output.
///
/// The contract is closed: every input maps to exactly one variant of
/// [`EnvelopeResult`]. Unknown intents, embedded fences, and partial fences
/// are all distinguishable failure modes rather than coerced into a default.
pub fn parse_envelope(output: &str) -> EnvelopeResult {
    let open_positions = all_occurrences(output, OPENING_FENCE);
    let close_positions = all_occurrences(output, CLOSING_FENCE);

    if open_positions.is_empty() && close_positions.is_empty() {
        return EnvelopeResult::Missing;
    }
    if open_positions.is_empty() {
        return EnvelopeResult::Malformed {
            reason: "closing fence found without a matching opening fence".into(),
            offending: None,
        };
    }
    if open_positions.len() > 1 {
        return EnvelopeResult::Duplicate {
            count: open_positions.len(),
        };
    }
    if close_positions.is_empty() {
        return EnvelopeResult::Truncated {
            detail: "opening fence found without a matching closing fence".into(),
        };
    }
    if close_positions.len() > 1 {
        return EnvelopeResult::Malformed {
            reason: "multiple closing fences for a single opening fence".into(),
            offending: None,
        };
    }
    let open_pos = open_positions[0];
    let close_pos = close_positions[0];
    if close_pos < open_pos {
        return EnvelopeResult::Malformed {
            reason: "closing fence appears before opening fence".into(),
            offending: None,
        };
    }
    let inner = &output[open_pos + OPENING_FENCE.len()..close_pos];

    // serde_json bounds recursion depth, so deeply nested untrusted output
    // surfaces as an ordinary decode error instead of a stack overflow.
    let data: Value = match serde_json::from_str(inner) {
        Ok(v) => v,
        Err(e) => {
            return malformed(format!("envelope payload is not valid JSON: {e}"), inner);
        }
    };
    let Value::Object(data) = data else {
        return malformed(
            format!(
                "envelope payload must be a JSON object, got {}",
                json_type_name(&data)
            ),
            inner,
        );
    };
    let Some(raw_intent) = data.get("intent") else {
        return malformed("envelope payload missing required 'intent' field", inner);
    };
    let intent = match raw_intent {
        Value::String(s) => Intent::parse(s),
        _ => None,
    };
    let Some(intent) = intent else {
        return malformed(
            format!(
                "envelope 'intent' must be one of {:?}, got {}",
                VALID_INTENTS, raw_intent
            ),
            inner,
        );
    };
    let reason = match data.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return malformed(
                format!(
                    "envelope 'reason' must be a string when present, got {}",
                    json_type_name(other)
                ),
                inner,
            );
        }
    };
    let requires = if intent == Intent::Blocked {
        match parse_requires(&data, inner) {
            Ok(r) => r,
            Err(e) => return e,
        }
    } else {
        Vec::new()
    };
    EnvelopeResult::Valid(ValidEnvelope {
        intent,
        reason,
        requires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(payload: &str) -> String {
        format!("prose before\n{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}\ntrailing")
    }

    #[test]
    fn valid_verify() {
        let r = parse_envelope(&wrap(r#"{"intent":"verify","reason":"done"}"#));
        assert_eq!(
            r,
            EnvelopeResult::Valid(ValidEnvelope {
                intent: Intent::Verify,
                reason: Some("done".into()),
                requires: vec![],
            })
        );
    }

    #[test]
    fn requires_ignored_on_non_blocked() {
        let r = parse_envelope(&wrap(r#"{"intent":"continue","requires":"junk"}"#));
        assert_eq!(r.as_valid().unwrap().requires, vec![]);
    }

    #[test]
    fn missing_truncated_duplicate() {
        assert_eq!(parse_envelope("no fences"), EnvelopeResult::Missing);
        assert!(matches!(
            parse_envelope(&format!("{OPENING_FENCE} {{")),
            EnvelopeResult::Truncated { .. }
        ));
        assert_eq!(
            parse_envelope(&format!("{OPENING_FENCE}{OPENING_FENCE}{CLOSING_FENCE}")),
            EnvelopeResult::Duplicate { count: 2 }
        );
        assert_eq!(
            parse_envelope(CLOSING_FENCE),
            EnvelopeResult::Malformed {
                reason: "closing fence found without a matching opening fence".into(),
                offending: None
            }
        );
        assert!(matches!(
            parse_envelope(&format!("{CLOSING_FENCE}{OPENING_FENCE}")),
            EnvelopeResult::Malformed { .. }
        ));
    }

    #[test]
    fn malformed_payloads() {
        let cases = [
            ("not json", "not valid JSON"),
            ("[1]", "must be a JSON object, got array"),
            ("{}", "missing required 'intent'"),
            (r#"{"intent":"finish"}"#, "must be one of"),
            (r#"{"intent":7}"#, "must be one of"),
            (
                r#"{"intent":"verify","reason":3}"#,
                "'reason' must be a string",
            ),
            (r#"{"intent":"blocked"}"#, "missing required 'requires'"),
            (r#"{"intent":"blocked","requires":{}}"#, "must be a list"),
            (
                r#"{"intent":"blocked","requires":[]}"#,
                "at least one predicate",
            ),
            (
                r#"{"intent":"blocked","requires":[1]}"#,
                "must each be a JSON object, got number",
            ),
            (
                r#"{"intent":"blocked","requires":[{}]}"#,
                "missing required 'type'",
            ),
            (
                r#"{"intent":"blocked","requires":[{"type":"x"}]}"#,
                "'type' must be one of",
            ),
            (
                r#"{"intent":"blocked","requires":[{"type":"command_grader"}]}"#,
                "missing required 'name'",
            ),
            (
                r#"{"intent":"blocked","requires":[{"type":"command_grader","name":""}]}"#,
                "non-empty string",
            ),
            (
                r#"{"intent":"blocked","requires":[{"type":"file_exists","path":"p","present":"y"}]}"#,
                "'present' must be a bool",
            ),
            (
                r#"{"intent":"blocked","requires":[{"type":"env_var_set","name":1}]}"#,
                "must be a string, got number",
            ),
        ];
        for (payload, expect) in cases {
            match parse_envelope(&wrap(payload)) {
                EnvelopeResult::Malformed { reason, offending } => {
                    assert!(reason.contains(expect), "{payload}: {reason}");
                    assert_eq!(offending.as_deref(), Some(payload));
                }
                other => panic!("{payload}: expected malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn blocked_with_all_three_predicates() {
        let r = parse_envelope(&wrap(
            r#"{"intent":"blocked","requires":[
                {"type":"command_grader","name":"tests"},
                {"type":"file_exists","path":"a.txt"},
                {"type":"file_exists","path":"b.txt","present":false},
                {"type":"env_var_set","name":"TOKEN"}]}"#,
        ));
        let v = r.as_valid().unwrap();
        assert_eq!(v.intent, Intent::Blocked);
        assert_eq!(
            v.requires,
            vec![
                BlockedRequirement::CommandGrader {
                    name: "tests".into()
                },
                BlockedRequirement::FileExists {
                    path: "a.txt".into(),
                    present: true
                },
                BlockedRequirement::FileExists {
                    path: "b.txt".into(),
                    present: false
                },
                BlockedRequirement::EnvVarSet {
                    name: "TOKEN".into()
                },
            ]
        );
        // Requirements round-trip through JSON with the same tagged shape.
        let json = serde_json::to_string(&v.requires).unwrap();
        let back: Vec<BlockedRequirement> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v.requires);
    }

    #[test]
    fn a_second_closing_fence_is_malformed() {
        let payload = r#"{"intent":"verify"}"#;
        for output in [
            format!("{OPENING_FENCE}{payload}{CLOSING_FENCE}{CLOSING_FENCE}"),
            format!("{OPENING_FENCE}{payload}{CLOSING_FENCE} prose {CLOSING_FENCE}"),
        ] {
            assert_eq!(
                parse_envelope(&output),
                EnvelopeResult::Malformed {
                    reason: "multiple closing fences for a single opening fence".into(),
                    offending: None
                }
            );
        }
    }

    #[test]
    fn intent_matching_is_exact() {
        for payload in [r#"{"intent":"VERIFY"}"#, r#"{"intent":"Verify"}"#] {
            assert!(
                matches!(
                    parse_envelope(&wrap(payload)),
                    EnvelopeResult::Malformed { ref reason, .. } if reason.contains("must be one of")
                ),
                "{payload}"
            );
        }
    }

    #[test]
    fn a_null_reason_is_absent() {
        let r = parse_envelope(&wrap(r#"{"intent":"verify","reason":null}"#));
        assert_eq!(
            r,
            EnvelopeResult::Valid(ValidEnvelope {
                intent: Intent::Verify,
                reason: None,
                requires: vec![],
            })
        );
    }

    #[test]
    fn file_exists_requires_a_non_empty_path() {
        let payload = r#"{"intent":"blocked","requires":[{"type":"file_exists","path":""}]}"#;
        match parse_envelope(&wrap(payload)) {
            EnvelopeResult::Malformed { reason, .. } => {
                assert!(
                    reason.contains("'path' must be a non-empty string"),
                    "{reason}"
                );
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_persisted_file_exists_predicate_defaults_to_present() {
        let req: BlockedRequirement =
            serde_json::from_str(r#"{"type":"file_exists","path":"p"}"#).unwrap();
        assert_eq!(
            req,
            BlockedRequirement::FileExists {
                path: "p".into(),
                present: true
            }
        );
    }

    #[test]
    fn deep_nesting_is_malformed_not_a_crash() {
        let deep = "[".repeat(100_000);
        let r = parse_envelope(&wrap(&deep));
        match r {
            EnvelopeResult::Malformed { offending, .. } => {
                assert_eq!(offending.unwrap().len(), 200);
            }
            other => panic!("{other:?}"),
        }
    }
}
