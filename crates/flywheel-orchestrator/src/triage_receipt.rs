//! Triage receipt: the schema + content-hash the github drain trusts.
//!
//! A *triage receipt* attests that an issue's human-authored content was
//! seen by a triage run which produced a fail-first grader: a command that
//! exits non-zero against a named base. The receipt is embedded in the
//! issue's ```` ```flywheel ```` spec block under the [`RECEIPT_KEY`] key.
//! The drain-side trust rule (a [`crate::github::GithubWorkSource`] built
//! with `require_triage_receipt`) schedules an issue only when its
//! receipt's `content_hash` matches the issue's *current* human content, so
//! a human edit to the title or prose after triage invalidates a stale
//! receipt.
//!
//! The content hash covers the issue title plus the body **with the
//! `flywheel` fenced block stripped**, so the machine-managed spec block
//! (which carries the receipt itself) never feeds its own hash. Stripping
//! is normalized with `trim` at the boundaries so a stray trailing
//! separator added when the block was embedded does not perturb the hash.
//!
//! This module is the single home of both the receipt schema and the
//! content-hash definition; the triage engine (the write side) uses the
//! same definitions, so the side that mints a receipt and the side that
//! trusts it can never drift. The line splitting reproduces Python's
//! `str.splitlines` boundaries so a receipt minted by the Python engine
//! hashes identically here.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The key under which a receipt lives inside the `flywheel` spec block.
pub const RECEIPT_KEY: &str = "triage_receipt";

/// The opening fence line of the `flywheel` spec block (after trimming).
pub const SPEC_FENCE_OPEN: &str = "```flywheel";
/// The closing fence line of the `flywheel` spec block (after trimming).
pub const SPEC_FENCE_CLOSE: &str = "```";

/// A parsed, well-typed triage receipt.
///
/// `command` is the fail-first grader command triage ran; `exit_code` its
/// exit status (non-zero is the fail-first invariant, see
/// [`is_fail_first`]); `base_sha` the commit the command was run against;
/// `content_hash` the [`content_hash`] of the issue's human content at
/// triage time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageReceipt {
    pub command: String,
    pub exit_code: i64,
    pub base_sha: String,
    pub content_hash: String,
}

/// Split `text` at exactly the boundaries Python's `str.splitlines` uses
/// (`\n`, `\r`, `\r\n`, `\x0b`, `\x0c`, `\x1c`, `\x1d`, `\x1e`, `\u{85}`,
/// `\u{2028}`, `\u{2029}`), dropping a trailing empty segment the same way.
pub fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        let is_break = matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..idx]);
        let mut next = idx + ch.len_utf8();
        if ch == '\r'
            && let Some(&(nidx, '\n')) = chars.peek()
        {
            chars.next();
            next = nidx + 1;
        }
        start = next;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Return `body` with the first `flywheel` fenced block removed.
///
/// Removes the region from the opening ```` ```flywheel ```` fence line
/// through its closing ```` ``` ```` fence line, inclusive. An
/// unterminated block (no closing fence) drops everything from the open
/// fence to the end. A body with no block is returned with only its
/// boundary whitespace normalized. The result is `trim`-normalized so the
/// blank separator that typically precedes an embedded block does not leak
/// into the hashed material.
pub fn strip_spec_block(body: &str) -> String {
    let lines = py_splitlines(body);
    let Some(open_idx) = lines.iter().position(|l| l.trim() == SPEC_FENCE_OPEN) else {
        return body.trim().to_string();
    };
    let close_idx = lines
        .iter()
        .skip(open_idx + 1)
        .position(|l| l.trim() == SPEC_FENCE_CLOSE)
        .map(|offset| open_idx + 1 + offset);
    let remaining: Vec<&str> = match close_idx {
        Some(close) => lines[..open_idx]
            .iter()
            .chain(lines[close + 1..].iter())
            .copied()
            .collect(),
        None => lines[..open_idx].to_vec(),
    };
    remaining.join("\n").trim().to_string()
}

/// The canonical content hash of an issue's human-authored content.
///
/// Defined as the SHA-256 of the issue `title` joined with the `body`
/// **after the `flywheel` spec block is stripped** ([`strip_spec_block`]).
/// Both the write side (the triage engine, which mints the receipt) and
/// the drain side call this one function, so a receipt minted before the
/// block was embedded hashes identically to the same issue re-read with
/// the block present.
///
/// Line breaks are canonicalized to `\n` (every [`py_splitlines`] boundary,
/// including `\r\n`) before hashing: stripping an embedded block rejoins the
/// body's lines with `\n`, so without this a CRLF body (GitHub's web editor)
/// would mint a receipt that reads as drifted the moment it is embedded.
pub fn content_hash(title: &str, body: &str) -> String {
    let human = strip_spec_block(body);
    let material = format!("{}\n{}", title.trim(), py_splitlines(&human).join("\n"));
    hex::encode(Sha256::digest(material.as_bytes()))
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(text.to_string())
}

/// Parse a spec block's [`RECEIPT_KEY`] value into a [`TriageReceipt`].
///
/// Returns `None` when `value` is not a well-formed receipt object: not an
/// object, or any of the four fields missing or wrong-typed (`command` and
/// `base_sha`/`content_hash` must be non-empty strings; `exit_code` must
/// be an integer, so a JSON boolean or float is rejected). Validity of the
/// *contents* (fail-first, hash currency) is the caller's decision, not a
/// parse concern.
pub fn parse_receipt(value: &Value) -> Option<TriageReceipt> {
    if !value.is_object() {
        return None;
    }
    let command = non_empty_str(value, "command")?;
    let exit_code = value.get("exit_code")?.as_i64()?;
    let base_sha = non_empty_str(value, "base_sha")?;
    let content_hash = non_empty_str(value, "content_hash")?;
    Some(TriageReceipt {
        command,
        exit_code,
        base_sha,
        content_hash,
    })
}

/// Whether `receipt` records a fail-first grader (non-zero exit).
///
/// A receipt whose `exit_code` is `0` records a command that *passed* at
/// triage time: there is nothing to fix, so it is not a valid basis for
/// scheduling work.
pub fn is_fail_first(receipt: &TriageReceipt) -> bool {
    receipt.exit_code != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sha256_hex(text: &str) -> String {
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    #[test]
    fn splitlines_matches_python_boundaries() {
        assert_eq!(py_splitlines("a\nb\r\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n"), ["a"]);
        assert_eq!(py_splitlines("a\n\n"), ["a", ""]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("x\u{2028}y\u{85}z"), ["x", "y", "z"]);
    }

    #[test]
    fn strip_removes_block_and_normalizes_boundaries() {
        let body = "Prose here.\n\n```flywheel\n{\"goal\": \"x\"}\n```\n\nAfter.\n";
        assert_eq!(strip_spec_block(body), "Prose here.\n\n\nAfter.");
        assert_eq!(strip_spec_block("  no block \n"), "no block");
        // An unterminated block drops everything from the fence onward.
        assert_eq!(
            strip_spec_block("Prose.\n\n```flywheel\n{\"goal\": \"x\"}"),
            "Prose."
        );
        // A fence with surrounding whitespace still counts.
        assert_eq!(strip_spec_block("P\n  ```flywheel  \n{}\n ``` \nQ"), "P\nQ");
    }

    #[test]
    fn content_hash_is_stable_across_block_embedding() {
        let title = "Fix the widget";
        let prose = "Widget drops retries.";
        let embedded = format!("{prose}\n\n```flywheel\n{{\"triage_receipt\": {{}}}}\n```");
        assert_eq!(content_hash(title, prose), content_hash(title, &embedded));
        assert_eq!(
            content_hash(title, prose),
            sha256_hex("Fix the widget\nWidget drops retries.")
        );
        assert_ne!(content_hash("Other", prose), content_hash(title, prose));
        assert_eq!(content_hash("  t  ", "  b  "), sha256_hex("t\nb"));
    }

    #[test]
    fn parse_receipt_requires_all_four_well_typed_fields() {
        let good = json!({
            "command": "uv run pytest",
            "exit_code": 1,
            "base_sha": "abc123",
            "content_hash": "deadbeef",
        });
        let receipt = parse_receipt(&good).expect("well-formed");
        assert_eq!(receipt.command, "uv run pytest");
        assert_eq!(receipt.exit_code, 1);
        assert_eq!(receipt.base_sha, "abc123");
        assert_eq!(receipt.content_hash, "deadbeef");
        assert!(is_fail_first(&receipt));

        let mut passing = good.clone();
        passing["exit_code"] = json!(0);
        assert!(!is_fail_first(&parse_receipt(&passing).unwrap()));

        for (key, bad) in [
            ("command", json!("")),
            ("command", json!(3)),
            ("exit_code", json!("nonzero")),
            ("exit_code", json!(true)),
            ("exit_code", json!(1.5)),
            ("base_sha", json!("   ")),
            ("content_hash", json!(null)),
        ] {
            let mut broken = good.clone();
            broken[key] = bad;
            assert!(parse_receipt(&broken).is_none(), "{key} must reject");
        }
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("base_sha");
        assert!(parse_receipt(&missing).is_none());
        assert!(parse_receipt(&json!("not an object")).is_none());
        assert!(parse_receipt(&json!([1])).is_none());
    }
}
