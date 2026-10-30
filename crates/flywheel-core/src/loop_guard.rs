//! Pure detector for repeated-failure (STUCK) and thrash (THRASH) loops.
//!
//! The harness feeds each iteration's `signals.tool_interactions` into
//! [`LoopGuard::observe`] in arrival order; a returned [`LoopGuardVerdict`]
//! tells the harness to abort the attempt and which transition to route to.
//! The hang watchdog and harness wiring live in the harness; the
//! `hang_timeout_seconds` knob is carried here as pure data only.
//!
//! See `docs/loop.md` for the surrounding design. The module is pure: no
//! IO, no clock, no async.
//!
//! The input digest is `sha256` over [`stable_encode`], a deterministic
//! structural encoding that is byte-identical to the Python
//! `flywheel_core.loop_guard._stable_encode`, so digests recorded by either
//! implementation agree.

use crate::invoker::ToolInteraction;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Write as _;

/// Closed set of detector kinds.
///
/// `Stuck` is the repeated-failure block (FR-1) and `Thrash` is the
/// identical-tuple repetition within a window (FR-2). The hang watchdog is
/// orthogonal and produced by the harness, not this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardVerdictKind {
    Stuck,
    Thrash,
}

impl LoopGuardVerdictKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            LoopGuardVerdictKind::Stuck => "stuck",
            LoopGuardVerdictKind::Thrash => "thrash",
        }
    }
}

impl fmt::Display for LoopGuardVerdictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One detector trip.
///
/// `tool_name` and `input_digest` identify the offending tuple so the
/// harness can name it in audit events and operator surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopGuardVerdict {
    pub kind: LoopGuardVerdictKind,
    pub reason: String,
    pub tool_name: String,
    pub input_digest: String,
}

/// Threshold knobs. Each is independently disablable via `None` or `0`.
///
/// Defaults for the deterministic detectors are conservative on -- they key
/// on byte-identical input digests, so a false positive requires the agent
/// to literally repeat itself. `hang_timeout_seconds` ships disabled: no
/// grounded value exists yet (see `docs/loop.md`), so this module carries
/// the knob as pure data and the harness watchdog reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopGuardConfig {
    pub repeated_tool_failure_threshold: Option<u32>,
    pub thrash_repeat_threshold: Option<u32>,
    pub thrash_window: Option<u32>,
    pub hang_timeout_seconds: Option<f64>,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        LoopGuardConfig {
            repeated_tool_failure_threshold: Some(3),
            thrash_repeat_threshold: Some(4),
            thrash_window: Some(12),
            hang_timeout_seconds: None,
        }
    }
}

/// `Some(threshold)` when a knob is enabled (present and positive).
fn enabled(knob: Option<u32>) -> Option<u32> {
    knob.filter(|v| *v > 0)
}

/// Python `repr(float)`: shortest round-trip digits, fixed notation for
/// exponents in `-4..16`, otherwise scientific with a signed, at least
/// two-digit exponent.
fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let sci = format!("{value:e}");
    let (mantissa, exp) = sci
        .split_once('e')
        .expect("LowerExp always carries an exponent");
    let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = (exp + 1) as usize;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let _ = write!(out, "{:02}", exp.abs());
    }
    out
}

fn encode_str_into(out: &mut String, s: &str) {
    // Length is the Python `len(str)`: code points, not bytes.
    let _ = write!(out, "S:{}:{}", s.chars().count(), s);
}

fn encode_into(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push('N'),
        Value::Bool(true) => out.push('T'),
        Value::Bool(false) => out.push('F'),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "I:{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "I:{u}");
            } else {
                let f = n
                    .as_f64()
                    .expect("a serde_json number is an integer or a float");
                let _ = write!(out, "X:{}", python_float_repr(f));
            }
        }
        Value::String(s) => encode_str_into(out, s),
        Value::Array(items) => {
            out.push_str("L[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_into(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => encode_map_into(out, map),
    }
}

fn encode_map_into(out: &mut String, map: &Map<String, Value>) {
    // Keys are sorted by their own stable encoding, so key-order noise in
    // the source mapping never splits a digest.
    let mut items: Vec<(String, String)> = map
        .iter()
        .map(|(k, v)| {
            let mut key = String::new();
            encode_str_into(&mut key, k);
            (key, stable_encode(v))
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    out.push_str("D{");
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out.push('}');
}

/// Deterministic structural encoding of a tool-input value.
///
/// Byte-identical structured inputs collide and key-order noise in
/// mappings does not split them. The encoding is tagged (`N`, `T`/`F`,
/// `I:`, `X:`, `S:<len>:`, `L[...]`, `D{k=v,...}`) and matches the Python
/// implementation byte-for-byte: string lengths count code points and
/// floats use Python `repr` formatting.
pub fn stable_encode(value: &Value) -> String {
    let mut out = String::new();
    encode_into(&mut out, value);
    out
}

/// [`stable_encode`] for a JSON object (the shape of every tool input).
pub fn stable_encode_map(map: &Map<String, Value>) -> String {
    let mut out = String::new();
    encode_map_into(&mut out, map);
    out
}

fn digest_encoded(encoded: &str) -> String {
    hex::encode(Sha256::digest(encoded.as_bytes()))
}

/// Lowercase hex sha256 of [`stable_encode`].
pub fn value_digest(value: &Value) -> String {
    digest_encoded(&stable_encode(value))
}

/// Lowercase hex sha256 of [`stable_encode_map`] -- the `input_digest` the
/// harness keys `blocked_implicit` and thrash detection on.
pub fn tool_input_digest(tool_input: &Map<String, Value>) -> String {
    digest_encoded(&stable_encode_map(tool_input))
}

type Key = (String, String);

/// Per-attempt accumulator over tool interactions.
///
/// Construct one per attempt and feed each iteration's
/// `signals.tool_interactions` to [`LoopGuard::observe`] in arrival order.
/// State accumulates across calls so detectors spanning multiple iterations
/// work correctly; the guard is not designed to be reset or reused across
/// attempts -- the harness builds a fresh one per attempt.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    config: LoopGuardConfig,
    last_failing_key: Option<Key>,
    consecutive_failures: u32,
    recent: VecDeque<Key>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        LoopGuard::new(LoopGuardConfig::default())
    }
}

impl LoopGuard {
    pub fn new(config: LoopGuardConfig) -> Self {
        let capacity = enabled(config.thrash_window).map_or(1, |w| w as usize);
        LoopGuard {
            config,
            last_failing_key: None,
            consecutive_failures: 0,
            recent: VecDeque::with_capacity(capacity),
        }
    }

    pub fn config(&self) -> &LoopGuardConfig {
        &self.config
    }

    /// Feed one iteration's tool interactions in arrival order.
    ///
    /// Returns the first verdict produced by walking the sequence; the
    /// repeated-failure (STUCK) detector is evaluated before the thrash
    /// (THRASH) detector for the same interaction, so a call that is both
    /// failing-and-repeating and thrashing returns STUCK (FR-6 precedence).
    /// Empty sequences are a no-op and never produce a verdict.
    pub fn observe(&mut self, interactions: &[ToolInteraction]) -> Option<LoopGuardVerdict> {
        for interaction in interactions {
            let key: Key = (
                interaction.tool_name.clone(),
                tool_input_digest(&interaction.tool_input),
            );
            if let Some(stuck) = self.update_repeated_failure(&key, interaction.is_failure()) {
                return Some(stuck);
            }
            if let Some(thrash) = self.update_thrash(key) {
                return Some(thrash);
            }
        }
        None
    }

    fn update_repeated_failure(&mut self, key: &Key, is_failure: bool) -> Option<LoopGuardVerdict> {
        let threshold = enabled(self.config.repeated_tool_failure_threshold)?;

        if is_failure {
            if self.last_failing_key.as_ref() == Some(key) {
                self.consecutive_failures += 1;
            } else {
                self.last_failing_key = Some(key.clone());
                self.consecutive_failures = 1;
            }
        } else {
            // A non-failure (success, or pending result with no result
            // block) breaks the consecutive failing streak per FR-1.
            self.last_failing_key = None;
            self.consecutive_failures = 0;
        }

        if self.consecutive_failures >= threshold {
            let (tool_name, digest) = key;
            return Some(LoopGuardVerdict {
                kind: LoopGuardVerdictKind::Stuck,
                reason: format!(
                    "{} consecutive failing calls to '{tool_name}' with identical input \
                     (digest {}...) reached repeated_tool_failure_threshold={threshold}",
                    self.consecutive_failures,
                    &digest[..12],
                ),
                tool_name: tool_name.clone(),
                input_digest: digest.clone(),
            });
        }
        None
    }

    fn update_thrash(&mut self, key: Key) -> Option<LoopGuardVerdict> {
        let threshold = enabled(self.config.thrash_repeat_threshold)?;
        let window = enabled(self.config.thrash_window)?;

        // Keep the trailing `window - 1` prior calls, count this call among
        // them, then append it: the window is this call plus its
        // predecessors (`window >= 1`, `enabled` filters zero).
        while self.recent.len() >= window as usize {
            self.recent.pop_front();
        }
        let occurrences = 1 + self.recent.iter().filter(|entry| **entry == key).count();
        let verdict = (occurrences >= threshold as usize).then(|| {
            let (tool_name, digest) = &key;
            LoopGuardVerdict {
                kind: LoopGuardVerdictKind::Thrash,
                reason: format!(
                    "{occurrences} repeats of '{tool_name}' with identical input \
                     (digest {}...) within trailing {window}-call window reached \
                     thrash_repeat_threshold={threshold}",
                    &digest[..12],
                ),
                tool_name: tool_name.clone(),
                input_digest: digest.clone(),
            }
        });
        self.recent.push_back(key);
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoker::ToolResultObservation;
    use serde_json::json;

    fn input(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("not an object: {other}"),
        }
    }

    fn interaction(id: &str, tool_input: Value, result: Option<Option<bool>>) -> ToolInteraction {
        ToolInteraction {
            tool_use_id: id.to_string(),
            tool_name: "Bash".to_string(),
            tool_input: input(tool_input),
            result: result.map(|is_error| ToolResultObservation {
                tool_use_id: id.to_string(),
                is_error,
                content: None,
            }),
        }
    }

    fn failing(id: &str, tool_input: Value) -> ToolInteraction {
        interaction(id, tool_input, Some(Some(true)))
    }

    fn succeeding(id: &str, tool_input: Value) -> ToolInteraction {
        interaction(id, tool_input, Some(Some(false)))
    }

    fn pending(id: &str) -> ToolInteraction {
        interaction(id, json!({"cmd": "ls"}), None)
    }

    fn ls() -> Value {
        json!({"cmd": "ls"})
    }

    fn config(stuck: Option<u32>, thrash: Option<u32>, window: Option<u32>) -> LoopGuardConfig {
        LoopGuardConfig {
            repeated_tool_failure_threshold: stuck,
            thrash_repeat_threshold: thrash,
            thrash_window: window,
            hang_timeout_seconds: None,
        }
    }

    // --- stable encoding parity with the Python implementation ---

    #[test]
    fn stable_encode_matches_python_byte_for_byte() {
        // Expected encodings and digests were produced by running the
        // Python `_stable_encode` / `_digest` on the same inputs.
        let cases: Vec<(Value, &str, &str)> = vec![
            (
                json!({"cmd": "ls"}),
                "D{S:3:cmd=S:2:ls}",
                "a88f544530b9c20efe02d0491114ba97590aec1adc65f26b6546d8b9d33355c8",
            ),
            (
                json!({"a": 1, "b": 2}),
                "D{S:1:a=I:1,S:1:b=I:2}",
                "03fb1e88511021d2302576d93403f97497a4dc0efd858a733ea115672ca2e2af",
            ),
            (
                json!({"b": 2, "a": 1}),
                "D{S:1:a=I:1,S:1:b=I:2}",
                "03fb1e88511021d2302576d93403f97497a4dc0efd858a733ea115672ca2e2af",
            ),
            (
                json!({}),
                "D{}",
                "ad1dc21dfec58caffbb688952139a162b5f2c9b8b3027a521f30e9e752674dd4",
            ),
            (
                json!([]),
                "L[]",
                "d27cbf07d603f889c14a4f228ccd5996f02e0244ce4c890558b9ff483be3bb41",
            ),
            (
                Value::Null,
                "N",
                "8ce86a6ae65d3692e7305e2c58ac62eebd97d3d943e093f577da25c36988246b",
            ),
            (
                json!(true),
                "T",
                "e632b7095b0bf32c260fa4c539e9fd7b852d0de454e9be26f24d0d6f91d069d3",
            ),
            (
                json!(false),
                "F",
                "f67ab10ad4e4c53121b6a5fe4da9c10ddee905b978d3788d2723d7bfacbe28a9",
            ),
            (
                json!(0),
                "I:0",
                "881e9f6198042359f1628d82b75927bef5d6c874c05b565522b8f25e247c62c5",
            ),
            (
                json!(-7),
                "I:-7",
                "f118a554002c5afbb2fc3ca564bcaf9df83bb3c54e49c03dc794b21be24b8b5b",
            ),
            (
                json!(1.0),
                "X:1.0",
                "b97e9b865c5b536ed9d9ad582fdd308c0be115f396932700dc9a858ca14352f3",
            ),
            (
                json!(0.1),
                "X:0.1",
                "807650f96f379d486dbd518d5a0c094351e8bf8a88b942d26c66373fc038fa51",
            ),
            (
                json!(1e16),
                "X:1e+16",
                "c0e665121898dddb9fcd492d328e645710216ff9472d5694e1a820d8970b2e67",
            ),
            (
                json!(1e-5),
                "X:1e-05",
                "b8aa2a27546cd190584f665c5ea2e6ea5f47f2d3d2cb330aecb89cae3888aebc",
            ),
            (
                json!(123456789012345678.0),
                "X:1.2345678901234568e+17",
                "d92ac9f482d536583b5d8ee6c429a64b5b1629d6f6b8816566dbc17f86f63cc0",
            ),
            (
                json!(-0.0),
                "X:-0.0",
                "d325e6d7b34d20f4f7aa37acf78dfa6fcf37a23495a59b88a036e7aa3122f2fc",
            ),
            (
                json!("héllo"),
                "S:5:héllo",
                "92232a89eb4cdc35a147dd682d7184af1defeaf7e5fa8194c10bd08fe3bad44a",
            ),
            (
                json!(""),
                "S:0:",
                "0942d35c0438a7116a1029b78ce38cda296acd647a790b2a51d06bdd1a8a4708",
            ),
            (
                json!({"nested": [1, 2.5, "x", null, {"k": [true]}], "z": "last", "A": "upper"}),
                "D{S:1:A=S:5:upper,S:1:z=S:4:last,S:6:nested=L[I:1,X:2.5,S:1:x,N,D{S:1:k=L[T]}]}",
                "d9bb52988c643ca67038738af5aea3acaee0291b230ca1769aab1af9e2101dbc",
            ),
            (
                json!({"10": 1, "9": 2, "a": 3}),
                "D{S:1:9=I:2,S:1:a=I:3,S:2:10=I:1}",
                "e84c47d187f43ebb5d3dc958218c99dd1787c478164b76eaaa13dfff3db613e8",
            ),
            (
                json!([1, [2, [3]]]),
                "L[I:1,L[I:2,L[I:3]]]",
                "2df5edaa9367e5051ec5f5188591c64a6d6f8d431ee4fd89618af13a05b91cda",
            ),
            (
                json!(9223372036854775808u64),
                "I:9223372036854775808",
                "cc3bbf6550f6a97a76652f8c00f4b4f1f89f2552c788c7ab0cc7e2d2cdb43195",
            ),
        ];
        for (value, encoded, digest) in cases {
            assert_eq!(stable_encode(&value), encoded, "{value}");
            assert_eq!(value_digest(&value), digest, "{value}");
            if let Value::Object(map) = &value {
                assert_eq!(stable_encode_map(map), encoded);
                assert_eq!(tool_input_digest(map), digest);
            }
        }
    }

    #[test]
    fn python_float_repr_edges() {
        let cases = [
            (0.0, "0.0"),
            (1e-4, "0.0001"),
            (1e15, "1000000000000000.0"),
            (123456.789, "123456.789"),
            (-1.5e-7, "-1.5e-07"),
            (2.5e100, "2.5e+100"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float_repr(value), expected);
        }
    }

    // --- FR-1: repeated-failure (STUCK) ---

    #[test]
    fn three_consecutive_identical_failures_trip_stuck_at_threshold_three() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        let verdict = guard
            .observe(&[
                failing("u1", ls()),
                failing("u2", ls()),
                failing("u3", ls()),
            ])
            .unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Stuck);
        assert_eq!(verdict.tool_name, "Bash");
        assert_eq!(verdict.input_digest, tool_input_digest(&input(ls())));
        assert_eq!(
            verdict.reason,
            "3 consecutive failing calls to 'Bash' with identical input (digest a88f544530b9...) \
             reached repeated_tool_failure_threshold=3"
        );
    }

    #[test]
    fn repeated_failure_accumulates_across_observe_calls() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        assert!(guard.observe(&[failing("u1", ls())]).is_none());
        assert!(guard.observe(&[failing("u2", ls())]).is_none());
        let verdict = guard.observe(&[failing("u3", ls())]).unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Stuck);
    }

    #[test]
    fn alternating_failures_do_not_trip_stuck() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        let a = json!({"cmd": "a"});
        let b = json!({"cmd": "b"});
        let stream = [
            failing("u1", a.clone()),
            failing("u2", b.clone()),
            failing("u3", a),
            failing("u4", b),
        ];
        assert!(guard.observe(&stream).is_none());
    }

    #[test]
    fn success_in_the_middle_breaks_the_consecutive_failing_run() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        let stream = [
            failing("u1", ls()),
            failing("u2", ls()),
            succeeding("u3", ls()),
            failing("u4", ls()),
            failing("u5", ls()),
        ];
        assert!(guard.observe(&stream).is_none());
    }

    #[test]
    fn different_tool_name_with_same_input_is_a_different_key() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        let mut other = failing("u2", ls());
        other.tool_name = "Read".into();
        assert!(
            guard
                .observe(&[failing("u1", ls()), other, failing("u3", ls())])
                .is_none()
        );
    }

    #[test]
    fn pending_result_does_not_count_for_repeated_failure() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        assert!(
            guard
                .observe(&[pending("u1"), pending("u1"), pending("u1")])
                .is_none()
        );
        // An explicit `is_error: null` is likewise not a failure.
        let unknown = interaction("u1", ls(), Some(None));
        assert!(
            guard
                .observe(&[unknown.clone(), unknown.clone(), unknown])
                .is_none()
        );
    }

    #[test]
    fn key_order_noise_does_not_split_digests() {
        let mut guard = LoopGuard::new(config(Some(3), None, None));
        let stream = [
            failing("u1", json!({"a": 1, "b": 2})),
            failing("u2", json!({"b": 2, "a": 1})),
            failing("u3", json!({"a": 1, "b": 2})),
        ];
        let verdict = guard.observe(&stream).unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Stuck);
    }

    // --- FR-2: thrash (THRASH) ---

    #[test]
    fn thrash_trips_on_identical_successes_within_window() {
        let mut guard = LoopGuard::new(config(None, Some(3), Some(5)));
        let stream: Vec<ToolInteraction> =
            (0..3).map(|i| succeeding(&format!("u{i}"), ls())).collect();
        let verdict = guard.observe(&stream).unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Thrash);
        assert_eq!(verdict.tool_name, "Bash");
        assert_eq!(
            verdict.reason,
            "3 repeats of 'Bash' with identical input (digest a88f544530b9...) within trailing \
             5-call window reached thrash_repeat_threshold=3"
        );
    }

    #[test]
    fn identical_tuple_outside_window_does_not_trip_thrash() {
        let mut guard = LoopGuard::new(config(None, Some(3), Some(3)));
        let target = json!({"cmd": "target"});
        let stream = [
            succeeding("u1", target.clone()),
            succeeding("u2", target.clone()),
            succeeding("u3", json!({"cmd": "decoy-a"})),
            succeeding("u4", json!({"cmd": "decoy-b"})),
            succeeding("u5", json!({"cmd": "decoy-c"})),
            // The trailing 3-call window now holds only decoys.
            succeeding("u6", target),
        ];
        assert!(guard.observe(&stream).is_none());
        assert_eq!(guard.recent.len(), 3);
    }

    #[test]
    fn thrash_window_counts_the_current_call_and_its_window_minus_one_predecessors() {
        let target = json!({"cmd": "target"});
        // window 3: target, decoy, target -> both targets share the window.
        let mut guard = LoopGuard::new(config(None, Some(2), Some(3)));
        let verdict = guard
            .observe(&[
                succeeding("u1", target.clone()),
                succeeding("u2", json!({"cmd": "decoy"})),
                succeeding("u3", target.clone()),
            ])
            .unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Thrash);
        assert!(verdict.reason.starts_with("2 repeats of 'Bash'"));

        // window 3: the first target is the 4th-most-recent call -> no trip.
        let mut guard = LoopGuard::new(config(None, Some(2), Some(3)));
        let verdict = guard.observe(&[
            succeeding("u1", target.clone()),
            succeeding("u2", json!({"cmd": "decoy-a"})),
            succeeding("u3", json!({"cmd": "decoy-b"})),
            succeeding("u4", target.clone()),
        ]);
        assert!(verdict.is_none());
        assert_eq!(guard.recent.len(), 3);

        // window 1 holds only the current call: threshold 1 trips at once.
        let mut guard = LoopGuard::new(config(None, Some(1), Some(1)));
        assert!(guard.observe(&[succeeding("u1", target)]).is_some());
        assert_eq!(guard.recent.len(), 1);
    }

    #[test]
    fn pending_results_still_count_toward_thrash_repetition() {
        let mut guard = LoopGuard::new(config(None, Some(3), Some(5)));
        let verdict = guard
            .observe(&[pending("u1"), pending("u1"), pending("u1")])
            .unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Thrash);
    }

    // --- FR-6: precedence ---

    #[test]
    fn precedence_stuck_wins_when_both_detectors_would_trip() {
        let mut guard = LoopGuard::new(config(Some(3), Some(3), Some(10)));
        let stream: Vec<ToolInteraction> =
            (0..3).map(|i| failing(&format!("u{i}"), ls())).collect();
        let verdict = guard.observe(&stream).unwrap();
        assert_eq!(verdict.kind, LoopGuardVerdictKind::Stuck);
    }

    // --- Edge cases ---

    #[test]
    fn empty_interactions_is_a_noop() {
        let mut guard = LoopGuard::default();
        assert!(guard.observe(&[]).is_none());
        assert_eq!(guard.config(), &LoopGuardConfig::default());
    }

    #[test]
    fn default_thresholds_are_three_four_twelve() {
        let cfg = LoopGuardConfig::default();
        assert_eq!(cfg.repeated_tool_failure_threshold, Some(3));
        assert_eq!(cfg.thrash_repeat_threshold, Some(4));
        assert_eq!(cfg.thrash_window, Some(12));
        assert_eq!(cfg.hang_timeout_seconds, None);
        let partial: LoopGuardConfig = serde_json::from_value(json!({"thrash_window": 0})).unwrap();
        assert_eq!(partial.thrash_window, Some(0));
        assert_eq!(partial.repeated_tool_failure_threshold, Some(3));
    }

    // --- FR-5: each disabled threshold never trips ---

    #[test]
    fn disabled_knobs_never_trip() {
        let failing_50: Vec<ToolInteraction> =
            (0..50).map(|i| failing(&format!("u{i}"), ls())).collect();
        let succeeding_20: Vec<ToolInteraction> = (0..20)
            .map(|i| succeeding(&format!("u{i}"), ls()))
            .collect();
        for cfg in [config(None, None, None), config(Some(0), None, None)] {
            assert!(LoopGuard::new(cfg).observe(&failing_50).is_none());
        }
        for cfg in [
            config(None, Some(0), Some(5)),
            config(None, Some(3), Some(0)),
            config(None, None, Some(5)),
            config(None, Some(3), None),
        ] {
            let mut guard = LoopGuard::new(cfg);
            assert!(guard.observe(&succeeding_20).is_none());
            assert!(guard.recent.is_empty());
        }
    }

    // --- Config carries the watchdog knob as pure data ---

    #[test]
    fn hang_timeout_seconds_is_a_pure_data_field() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            hang_timeout_seconds: Some(0.5),
            ..LoopGuardConfig::default()
        });
        assert!(guard.observe(&[succeeding("u1", ls())]).is_none());
    }

    #[test]
    fn verdict_round_trips_through_json() {
        let verdict = LoopGuardVerdict {
            kind: LoopGuardVerdictKind::Thrash,
            reason: "r".into(),
            tool_name: "Bash".into(),
            input_digest: "d".into(),
        };
        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["kind"], "thrash");
        let back: LoopGuardVerdict = serde_json::from_value(json).unwrap();
        assert_eq!(back, verdict);
        assert_eq!(LoopGuardVerdictKind::Stuck.to_string(), "stuck");
    }
}
