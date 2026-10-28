//! Loop-path signal detection over a unified-diff string.
//!
//! Pure string analysis: the input is a unified-diff text, the output is the
//! set of watched loop-path signals it contains. No file IO, no git
//! invocation. Implements the trigger set of the in-loop verification gate
//! (`docs/loop.md`, "In-loop verification gate").
//!
//! Only **added** content is considered: lines beginning with `+` and not
//! the `+++` file header. A watched symbol that appears only in a context
//! line or a removed line is not a hit. Operating on the **net cumulative**
//! diff text the caller hands in means an add-then-revert within one phase
//! naturally nets no new symbol.
//!
//! The watched files are the Rust crate's (`crates/flywheel-core/src/`
//! `lifecycle.rs`, `task.rs`, `grader_*.rs`, `store.rs`,
//! `invoker_client.rs`, and the canonical DDL under
//! `crates/flywheel-core/schema/*.sql`). Their Python-era counterparts
//! (`lifecycle.py`, `task.py`, `grader_*.py`, `store_protocols.py`,
//! `invoker_client.py`, `_schema/*.sql`) stay recognized so a diff that
//! still touches them is judged by the same trigger set.
//!
//! The detection is intentionally an over-approximation: file/symbol
//! presence cannot prove a new loop path exists. Signal 4 is scoped to its
//! decidable core -- a newly added required method on a store trait in
//! `store.rs` (a bodiless `fn ...;`, which only a trait can declare), or a
//! `def` on a Protocol in `store_protocols.py` -- and explicitly does NOT
//! fire on any new function in the harness/orchestrator. The opt-out
//! artifact (`loop-path-exempt.md`) is parsed by the orchestrator's archive
//! gate, not here.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::LazyLock;

/// Watched signals from the gate's trigger-set table. The numeric suffix in
/// each doc comment mirrors the table row in `docs/loop.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopPathSignal {
    /// Signal 1: new `Status`/`Outcome` enum member or transition-rule
    /// entry in `lifecycle.rs` (or `lifecycle.py`).
    StatusOrTransition,
    /// Signal 2: new `ADD COLUMN` / table / column definition in a
    /// `schema/*.sql` (or `_schema/*.sql`) file the live store binds to.
    SchemaColumnOrTable,
    /// Signal 3: new `Grader` union variant in `task.rs` (or `task.py`) or a
    /// new `grader_*.rs` (or `grader_*.py`) module the harness dispatches.
    GraderVariant,
    /// Signal 4: new required method on a store trait in `store.rs` (or a
    /// `def` on a Protocol in `store_protocols.py`): the decidable core of
    /// the store-contract/resolver-entry signal.
    ProtocolMethod,
    /// Signal 5: new `CONTROL_COMMAND_*` constant in `invoker_client.rs`
    /// (or `invoker_client.py`).
    ControlCommandVerb,
}

impl LoopPathSignal {
    pub const ALL: [LoopPathSignal; 5] = [
        LoopPathSignal::StatusOrTransition,
        LoopPathSignal::SchemaColumnOrTable,
        LoopPathSignal::GraderVariant,
        LoopPathSignal::ProtocolMethod,
        LoopPathSignal::ControlCommandVerb,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            LoopPathSignal::StatusOrTransition => "status_or_transition",
            LoopPathSignal::SchemaColumnOrTable => "schema_column_or_table",
            LoopPathSignal::GraderVariant => "grader_variant",
            LoopPathSignal::ProtocolMethod => "protocol_method",
            LoopPathSignal::ControlCommandVerb => "control_command_verb",
        }
    }
}

impl fmt::Display for LoopPathSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- Public entry point -----------------------------------------------------

/// Return the set of watched loop-path signals present in `diff`.
///
/// `diff` is a unified-diff text. An empty or signal-free diff returns an
/// empty set; the function never fails on malformed input -- unparseable
/// segments are silently skipped, which is the correct behavior for an
/// over-approximating gate (false negatives only hurt coverage, never
/// archive-correctness, because every signal it does fire is verified by
/// either the verify-task gate or the audit re-check).
pub fn detect_loop_path_signals(diff: &str) -> BTreeSet<LoopPathSignal> {
    let mut signals = BTreeSet::new();
    for section in parse_diff(diff) {
        let path = section.new_path.as_str();
        let added = &section.added_lines;
        let name = basename(path);

        if is_schema_sql_path(path) && detect_schema_column_or_table(added) {
            signals.insert(LoopPathSignal::SchemaColumnOrTable);
        }
        if section.is_new_file && is_grader_module_path(path) {
            signals.insert(LoopPathSignal::GraderVariant);
        }
        let fired = match name {
            "lifecycle.rs" => detect_rust_status_or_transition(added)
                .then_some(LoopPathSignal::StatusOrTransition),
            "lifecycle.py" => {
                detect_status_or_transition(added).then_some(LoopPathSignal::StatusOrTransition)
            }
            "task.rs" => {
                detect_rust_grader_variant_in_task(added).then_some(LoopPathSignal::GraderVariant)
            }
            "task.py" => {
                detect_grader_variant_in_task(added).then_some(LoopPathSignal::GraderVariant)
            }
            "store.rs" => detect_rust_trait_method(added).then_some(LoopPathSignal::ProtocolMethod),
            "store_protocols.py" => {
                detect_protocol_method(added).then_some(LoopPathSignal::ProtocolMethod)
            }
            "invoker_client.rs" => detect_rust_control_command_verb(added)
                .then_some(LoopPathSignal::ControlCommandVerb),
            "invoker_client.py" => {
                detect_control_command_verb(added).then_some(LoopPathSignal::ControlCommandVerb)
            }
            _ => None,
        };
        signals.extend(fired);
    }
    signals
}

// --- Diff parsing -----------------------------------------------------------

/// One file's worth of added content extracted from a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSection {
    new_path: String,
    is_new_file: bool,
    added_lines: Vec<String>,
}

fn header_path(line: &str) -> &str {
    line.split('\t').next().unwrap_or("").trim()
}

/// Split a unified diff into per-file added-content sections.
///
/// Only the post-image path (the `+++` header) and added lines (those
/// beginning with `+` but not the `+++` header) are extracted. Context
/// lines, removed lines, hunk headers, and `\ No newline at end of file`
/// markers are ignored. A section's `is_new_file` flag is set when the
/// matching `---` header pointed at `/dev/null` (the canonical "file
/// creation" marker in a unified diff).
///
/// Hunk extents are tracked from the `@@ -a,b +c,d @@` counts so that a
/// removed `-- comment` line (rendered `--- comment`) or an added `++ x`
/// line (rendered `+++ x`) inside a hunk is attributed as body content, not
/// mistaken for a file header. When counts are off (hand-written diffs),
/// a `---` line is still accepted as a header if it is followed by a `+++`
/// line and then a hunk header, a `diff --git` separator, or the end of
/// the text -- the only shape a file boundary can take.
fn parse_diff(diff: &str) -> Vec<FileSection> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut sections: Vec<FileSection> = Vec::new();
    let mut current: Option<usize> = None;
    let mut pending_old_is_null = false;
    let mut old_left: usize = 0;
    let mut new_left: usize = 0;
    let mut expect_new_header = false;

    let header_pair_ahead = |i: usize| {
        lines
            .get(i + 1)
            .is_some_and(|next| next.starts_with("+++ "))
            && lines
                .get(i + 2)
                .is_none_or(|after| hunk_counts(after).is_some() || after.starts_with("diff --git"))
    };

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git") {
            current = None;
            pending_old_is_null = false;
            old_left = 0;
            new_left = 0;
            expect_new_header = false;
            continue;
        }

        if let Some((old, new)) = hunk_counts(line) {
            old_left = old;
            new_left = new;
            expect_new_header = false;
            continue;
        }

        let in_hunk = old_left > 0 || new_left > 0;

        if let Some(rest) = line.strip_prefix("--- ")
            && (!in_hunk || header_pair_ahead(i))
        {
            pending_old_is_null = header_path(rest) == "/dev/null";
            current = None;
            old_left = 0;
            new_left = 0;
            expect_new_header = true;
            continue;
        }

        if let Some(rest) = line.strip_prefix("+++ ")
            && (!in_hunk || expect_new_header)
        {
            expect_new_header = false;
            let new = header_path(rest);
            if new == "/dev/null" {
                // File deletion: no added content to attribute.
                current = None;
                pending_old_is_null = false;
                continue;
            }
            let new = new.strip_prefix("b/").unwrap_or(new);
            sections.push(FileSection {
                new_path: new.to_string(),
                is_new_file: pending_old_is_null,
                added_lines: Vec::new(),
            });
            current = Some(sections.len() - 1);
            pending_old_is_null = false;
            continue;
        }
        expect_new_header = false;

        if line.starts_with('\\') {
            // "\ No newline at end of file" consumes no hunk lines.
            continue;
        }
        if in_hunk {
            if line.starts_with('+') {
                new_left = new_left.saturating_sub(1);
            } else if line.starts_with('-') {
                old_left = old_left.saturating_sub(1);
            } else {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
            }
        }

        let Some(index) = current else {
            continue;
        };
        if let Some(added) = line.strip_prefix('+') {
            sections[index].added_lines.push(added.to_string());
        }
    }

    sections
}

static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@").expect("static regex"));

/// `(old_count, new_count)` of a `@@ -a,b +c,d @@` hunk header; an omitted
/// count means one line.
fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    let caps = HUNK_HEADER_RE.captures(line)?;
    let count = |idx: usize| caps.get(idx).map_or(Some(1), |m| m.as_str().parse().ok());
    Some((count(1)?, count(2)?))
}

// --- Path predicates --------------------------------------------------------

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A `.sql` file directly inside a `schema/` (the Rust crate's
/// `crates/flywheel-core/schema/`) or `_schema/` (the Python package's)
/// directory: the canonical DDL the live store binds to.
fn is_schema_sql_path(path: &str) -> bool {
    if !path.ends_with(".sql") {
        return false;
    }
    let mut components = path.rsplit('/');
    components.next();
    matches!(components.next(), Some("schema" | "_schema"))
}

static GRADER_MODULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^grader_[a-z_][a-z_0-9]*\.(?:rs|py)$").expect("static regex"));

fn is_grader_module_path(path: &str) -> bool {
    GRADER_MODULE_RE.is_match(basename(path))
}

// --- Signal detectors -------------------------------------------------------

// Signal 1: a Status/Outcome enum member is an indented `UPPER_NAME = "..."`
// at class body indentation; transition-rule entries are indented
// `Status.NAME:` / `Outcome.NAME:` dict keys (`_VALID_EDGES`-style). The
// indentation requirement (`^\s+`) excludes module-level constants such as
// `_REQUIRES_ERROR` and rules out local-variable renames inside methods
// (those use lowercase identifiers).
static ENUM_MEMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s+[A-Z][A-Z_0-9]*\s*=\s*["']"#).expect("static regex"));
static TRANSITION_ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(?:Status|Outcome)\.[A-Z_][A-Z_0-9]*\s*:").expect("static regex")
});

fn detect_status_or_transition(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .filter(|line| !is_python_comment_or_blank(line))
        .any(|line| ENUM_MEMBER_RE.is_match(line) || TRANSITION_ENTRY_RE.is_match(line))
}

// Signal 2: a new SQL column or table. `ADD COLUMN` and `CREATE TABLE` are
// unambiguous; column-definition lines added directly inside an existing
// `CREATE TABLE` block are caught too (the phase-08 pattern, where
// `awaiting_manual_ordinal INTEGER` was added as a new column inside the
// existing `lifecycles` definition).
static ADD_COLUMN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bADD\s+COLUMN\b").expect("static regex"));
static CREATE_TABLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bCREATE\s+TABLE\b").expect("static regex"));
static COLUMN_DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s+[a-z_][a-z_0-9]*\s+(?:TEXT|INTEGER|REAL|BLOB|DATETIME|BOOLEAN|NUMERIC|DATE|TIMESTAMP|VARCHAR|CHAR|BIGINT|SMALLINT|FLOAT|DOUBLE)\b",
    )
    .expect("static regex")
});

fn detect_schema_column_or_table(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .filter(|line| {
            let stripped = line.trim_start();
            !stripped.is_empty() && !stripped.starts_with("--")
        })
        .any(|line| {
            ADD_COLUMN_RE.is_match(line)
                || CREATE_TABLE_RE.is_match(line)
                || COLUMN_DEF_RE.is_match(line)
        })
}

// Signal 3 (task.py side): a new `class XxxGrader` definition, or a
// modification of the top-level `Grader = ...` union assignment. Either is
// a decisive marker that the discriminated union has a new variant.
static NEW_GRADER_CLASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*class\s+[A-Za-z_]\w*Grader\b").expect("static regex"));
static GRADER_UNION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Grader\s*=").expect("static regex"));

fn detect_grader_variant_in_task(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .filter(|line| !is_python_comment_or_blank(line))
        .any(|line| NEW_GRADER_CLASS_RE.is_match(line) || GRADER_UNION_RE.is_match(line))
}

// Signal 4 (scoped, decidable core): a newly added `def` at Protocol body
// indentation (4 spaces) in `store_protocols.py`. The name must start with
// a lowercase letter, which excludes dunders (`__init__` etc.) so a new
// `StoreConflictError` subclass with an `__init__` does not false-positive.
static PROTOCOL_DEF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {4}def\s+[a-z][a-z_0-9]*\s*\(").expect("static regex"));

fn detect_protocol_method(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .any(|line| PROTOCOL_DEF_RE.is_match(line))
}

// Signal 5: a new top-level `CONTROL_COMMAND_*` constant in
// `invoker_client.py`. The verbs are module-level, so anchor at column 0.
static CONTROL_CMD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^CONTROL_COMMAND_[A-Z][A-Z_0-9]*\s*[:=]").expect("static regex"));

fn detect_control_command_verb(added_lines: &[String]) -> bool {
    added_lines.iter().any(|line| CONTROL_CMD_RE.is_match(line))
}

// --- Rust signal detectors ---------------------------------------------------

// Signal 1 (lifecycle.rs): adding a `Status`/`Outcome` member adds its enum
// variant line (`    Reviewing,` at item-body indentation) and, because
// the matches are exhaustive, a `Status::Reviewing => ...` arm (the
// `as_str` mapping or a `valid_targets` row). A new transition edge adds a
// `valid_targets` row or a `Status::X,` target line inside an existing
// row's array. The array-entry form also matches a status list elsewhere in
// the file (an over-approximation the opt-out artifact downgrades).
static RUST_ENUM_VARIANT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {4}[A-Z][A-Za-z0-9]*,\s*$").expect("static regex"));
static RUST_STATUS_ARM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s+(?:Status|Outcome)::[A-Z][A-Za-z0-9]*(?:\s*\|\s*(?:Status|Outcome)::[A-Z][A-Za-z0-9]*)*\s*=>",
    )
    .expect("static regex")
});
static RUST_STATUS_ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(?:Status|Outcome)::[A-Z][A-Za-z0-9]*,\s*$").expect("static regex")
});

fn detect_rust_status_or_transition(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .filter(|line| !is_rust_comment_or_blank(line))
        .any(|line| {
            RUST_ENUM_VARIANT_RE.is_match(line)
                || RUST_STATUS_ARM_RE.is_match(line)
                || RUST_STATUS_ENTRY_RE.is_match(line)
        })
}

// Signal 3 (task.rs): a new `struct XxxGrader` definition, or a new
// `Xxx(XxxGrader)` variant line in the `Grader` enum (or its private
// deserialization mirror).
static RUST_GRADER_STRUCT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+[A-Za-z_]\w*Grader\b").expect("static regex")
});
static RUST_GRADER_VARIANT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+[A-Z][A-Za-z0-9]*\(\s*[A-Za-z_]\w*Grader\s*\),?\s*$").expect("static regex")
});

fn detect_rust_grader_variant_in_task(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .filter(|line| !is_rust_comment_or_blank(line))
        .any(|line| RUST_GRADER_STRUCT_RE.is_match(line) || RUST_GRADER_VARIANT_RE.is_match(line))
}

// Signal 4 (store.rs, scoped decidable core): a newly added required trait
// method -- an `fn` at trait-body indentation (4 spaces, no `pub`, which an
// inherent method would carry) whose signature ends in `;`. Only a trait
// can declare a bodiless `fn`, so this excludes the test module's helper
// functions and trait *impls* (`fn fmt(...) {`), whose signatures end in
// `{`. A signature spanning several added lines is followed to its end.
static RUST_TRAIT_FN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {4}fn\s+[a-z][a-z_0-9]*\s*[(<]").expect("static regex"));

fn detect_rust_trait_method(added_lines: &[String]) -> bool {
    let mut lines = added_lines
        .iter()
        .filter(|line| !is_rust_comment_or_blank(line));
    while let Some(line) = lines.next() {
        if !RUST_TRAIT_FN_RE.is_match(line) {
            continue;
        }
        let mut current = line.trim_end();
        loop {
            if current.ends_with(';') {
                return true;
            }
            if current.ends_with('{') || current.ends_with('}') {
                break;
            }
            match lines.next() {
                Some(next) => current = next.trim_end(),
                None => return false,
            }
        }
    }
    false
}

// Signal 5 (invoker_client.rs): a new top-level `CONTROL_COMMAND_*` const.
static RUST_CONTROL_CMD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:pub(?:\([^)]*\))?\s+)?const\s+CONTROL_COMMAND_[A-Z][A-Z_0-9]*\s*:")
        .expect("static regex")
});

fn detect_rust_control_command_verb(added_lines: &[String]) -> bool {
    added_lines
        .iter()
        .any(|line| RUST_CONTROL_CMD_RE.is_match(line))
}

// --- Helpers ----------------------------------------------------------------

fn is_rust_comment_or_blank(line: &str) -> bool {
    let stripped = line.trim_start();
    stripped.is_empty() || stripped.starts_with("//") || stripped.starts_with("/*")
}

fn is_python_comment_or_blank(line: &str) -> bool {
    let stripped = line.trim_start();
    stripped.is_empty() || stripped.starts_with('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn only(signal: LoopPathSignal) -> BTreeSet<LoopPathSignal> {
        BTreeSet::from([signal])
    }

    // --- Signal 1: Status/Outcome member or transition-rule entry ---

    #[test]
    fn status_enum_member_addition_fires_signal_1() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -16,6 +16,7 @@",
            "     FAILED = \"failed\"",
            "     INTERRUPTED = \"interrupted\"",
            "+    REVIEWING = \"reviewing\"",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::StatusOrTransition)
        );
    }

    #[test]
    fn outcome_enum_member_addition_fires_signal_1() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -25,3 +25,4 @@",
            "     INTERNAL_ERROR = \"internal_error\"",
            "+    REJECTED = 'rejected'",
        ]);
        assert!(detect_loop_path_signals(&d).contains(&LoopPathSignal::StatusOrTransition));
    }

    #[test]
    fn new_transition_rule_entry_fires_signal_1() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -58,3 +58,5 @@",
            "     Status.DONE: frozenset(),",
            "     Status.FAILED: frozenset(),",
            "+    Status.REVIEWING: frozenset({Status.DONE}),",
            " }",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::StatusOrTransition)
        );
    }

    #[test]
    fn local_variable_rename_in_lifecycle_does_not_fire() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -220,7 +220,7 @@",
            "         if not self.attempts:",
            "             return 0",
            "-        tail = self.attempts[-1]",
            "-        if tail.outcome not in _FAILED_OUTCOMES:",
            "+        last = self.attempts[-1]",
            "+        if last.outcome not in _FAILED_OUTCOMES:",
            "             return 0",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    // --- Signal 2: ADD COLUMN / new table / column DDL ---

    #[test]
    fn add_column_fires_signal_2() {
        let d = diff(&[
            "diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql",
            "--- a/src/flywheel/_schema/persistence-schema.sql",
            "+++ b/src/flywheel/_schema/persistence-schema.sql",
            "@@ -78,3 +78,4 @@",
            "   awaiting_manual_ordinal INTEGER",
            " );",
            "+ALTER TABLE lifecycles ADD COLUMN review_notes TEXT;",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
    }

    #[test]
    fn new_create_table_fires_signal_2() {
        let d = diff(&[
            "diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql",
            "--- a/src/flywheel/_schema/persistence-schema.sql",
            "+++ b/src/flywheel/_schema/persistence-schema.sql",
            "@@ -235,3 +235,8 @@",
            "   version INTEGER NOT NULL",
            " );",
            "+create table if not exists reviews (",
            "+  id    INTEGER PRIMARY KEY AUTOINCREMENT,",
            "+  run_id TEXT NOT NULL,",
            "+  verdict TEXT NOT NULL",
            "+);",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
    }

    #[test]
    fn inline_column_addition_inside_create_table_fires_signal_2() {
        let d = diff(&[
            "diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql",
            "--- a/src/flywheel/_schema/persistence-schema.sql",
            "+++ b/src/flywheel/_schema/persistence-schema.sql",
            "@@ -70,5 +70,6 @@",
            "   blocked_requires_json   TEXT,",
            "   task_content_hash       TEXT,",
            "+  review_state            TEXT,",
            "   awaiting_manual_ordinal INTEGER",
            " );",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
    }

    #[test]
    fn sql_comment_only_change_does_not_fire_signal_2() {
        let d = diff(&[
            "diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql",
            "--- a/src/flywheel/_schema/persistence-schema.sql",
            "+++ b/src/flywheel/_schema/persistence-schema.sql",
            "@@ -1,3 +1,4 @@",
            " -- Flywheel loop persistence schema.",
            "+-- (Clarified header: described CREATE TABLE convention better.)",
            " --",
            " -- Tables mirror the conceptual model: tasks is ...",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn removed_sql_comment_inside_hunk_is_not_a_file_header() {
        // A removed `-- comment` line renders as `--- comment` inside the
        // hunk. The Python parser mistook it for a file header and dropped
        // every added line that followed; the column below must still fire.
        let d = diff(&[
            "diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql",
            "--- a/src/flywheel/_schema/persistence-schema.sql",
            "+++ b/src/flywheel/_schema/persistence-schema.sql",
            "@@ -70,5 +70,5 @@",
            "   task_content_hash       TEXT,",
            "--- old comment about the next column",
            "+++ new comment spelled with two pluses",
            "+  review_state            TEXT,",
            " );",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
    }

    #[test]
    fn plain_unified_diff_without_git_separators_splits_files() {
        // Exact hunk counts, as `diff -u` emits them.
        let exact = diff(&[
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -16,1 +16,2 @@",
            "     INTERRUPTED = \"interrupted\"",
            "+    REVIEWING = \"reviewing\"",
            "--- a/src/flywheel/invoker_client.py",
            "+++ b/src/flywheel/invoker_client.py",
            "@@ -67,1 +67,2 @@",
            " CONTROL_COMMAND_REJECT: str = \"reject\"",
            "+CONTROL_COMMAND_REVIEW: str = \"review\"",
        ]);
        // Overstated counts (a hand-written diff): the header triple
        // `---`, `+++`, `@@` is still recognized as a file boundary.
        let sloppy = exact.replace("@@ -16,1 +16,2 @@", "@@ -16,6 +16,7 @@");
        for d in [exact, sloppy] {
            assert_eq!(
                detect_loop_path_signals(&d),
                BTreeSet::from([
                    LoopPathSignal::StatusOrTransition,
                    LoopPathSignal::ControlCommandVerb
                ])
            );
        }
    }

    #[test]
    fn hunk_counts_parse_optional_lengths() {
        assert_eq!(hunk_counts("@@ -1,3 +1,4 @@ class Foo:"), Some((3, 4)));
        assert_eq!(hunk_counts("@@ -0,0 +1 @@"), Some((0, 1)));
        assert_eq!(hunk_counts("@@ -5 +5,0 @@"), Some((1, 0)));
        assert_eq!(hunk_counts("@@"), None);
        assert_eq!(hunk_counts("@@ -x +y @@"), None);
    }

    #[test]
    fn no_newline_marker_is_not_counted_as_a_hunk_line() {
        let d = diff(&[
            "diff --git a/x/_schema/s.sql b/x/_schema/s.sql",
            "--- a/x/_schema/s.sql",
            "+++ b/x/_schema/s.sql",
            "@@ -1,1 +1,2 @@",
            "-x INTEGER",
            "\\ No newline at end of file",
            "+x INTEGER,",
            "+  y TEXT",
            "\\ No newline at end of file",
            "--- a/x/lifecycle.py",
            "+++ b/x/lifecycle.py",
            "@@ -1,0 +1,1 @@",
            "+    NEW = \"new\"",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            BTreeSet::from([
                LoopPathSignal::SchemaColumnOrTable,
                LoopPathSignal::StatusOrTransition
            ])
        );
    }

    // --- Signal 3: new Grader union variant ---

    #[test]
    fn new_grader_class_in_task_fires_signal_3() {
        let d = diff(&[
            "diff --git a/src/flywheel/task.py b/src/flywheel/task.py",
            "--- a/src/flywheel/task.py",
            "+++ b/src/flywheel/task.py",
            "@@ -76,2 +76,12 @@",
            " Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader",
            "+",
            "+",
            "+@dataclass(kw_only=True)",
            "+class LoopGrader:",
            "+    run: str",
            "+    name: str | None = None",
            "+    type: Literal[\"loop\"] = \"loop\"",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::GraderVariant)
        );
    }

    #[test]
    fn modified_grader_union_assignment_fires_signal_3() {
        let d = diff(&[
            "diff --git a/src/flywheel/task.py b/src/flywheel/task.py",
            "--- a/src/flywheel/task.py",
            "+++ b/src/flywheel/task.py",
            "@@ -76,1 +76,1 @@",
            "-Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader",
            "+Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader | LoopGrader",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::GraderVariant)
        );
    }

    #[test]
    fn new_grader_module_file_creation_fires_signal_3() {
        let d = diff(&[
            "diff --git a/src/flywheel/grader_loop.py b/src/flywheel/grader_loop.py",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/src/flywheel/grader_loop.py",
            "@@ -0,0 +1,3 @@",
            "+def run_loop_grader(spec):",
            "+    \"\"\"Execute a loop grader.\"\"\"",
            "+    return True",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::GraderVariant)
        );
    }

    #[test]
    fn editing_existing_grader_module_does_not_fire_signal_3() {
        let d = diff(&[
            "diff --git a/src/flywheel/grader_command.py b/src/flywheel/grader_command.py",
            "--- a/src/flywheel/grader_command.py",
            "+++ b/src/flywheel/grader_command.py",
            "@@ -1,1 +1,1 @@",
            "-# header",
            "+# header (rephrased)",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn deleted_grader_module_does_not_fire_signal_3() {
        let d = diff(&[
            "diff --git a/src/flywheel/grader_loop.py b/src/flywheel/grader_loop.py",
            "deleted file mode 100644",
            "--- a/src/flywheel/grader_loop.py",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-def run_loop_grader(spec):",
            "-    return True",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    // --- Signal 4: new Protocol method in store_protocols.py ---

    #[test]
    fn new_protocol_method_in_store_protocols_fires_signal_4() {
        let d = diff(&[
            "diff --git a/src/flywheel/store_protocols.py b/src/flywheel/store_protocols.py",
            "--- a/src/flywheel/store_protocols.py",
            "+++ b/src/flywheel/store_protocols.py",
            "@@ -310,2 +310,6 @@",
            "     def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...",
            "+",
            "+    def list_awaiting_approval(",
            "+        self,",
            "+    ) -> list[Lifecycle]: ...",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::ProtocolMethod)
        );
    }

    #[test]
    fn dunder_addition_in_store_protocols_does_not_fire_signal_4() {
        let d = diff(&[
            "diff --git a/src/flywheel/store_protocols.py b/src/flywheel/store_protocols.py",
            "--- a/src/flywheel/store_protocols.py",
            "+++ b/src/flywheel/store_protocols.py",
            "@@ -85,2 +85,6 @@",
            "         self.task_id = task_id",
            "+",
            "+",
            "+class NewConflictError(StoreConflictError):",
            "+    def __init__(self, run_id: str) -> None:",
            "+        super().__init__(f\"new conflict {run_id!r}\")",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn new_def_in_harness_does_not_fire_signal_4() {
        let d = diff(&[
            "diff --git a/src/flywheel/harness.py b/src/flywheel/harness.py",
            "--- a/src/flywheel/harness.py",
            "+++ b/src/flywheel/harness.py",
            "@@ -1,3 +1,7 @@",
            " from __future__ import annotations",
            "+",
            "+",
            "+def resolve_review(lifecycle):",
            "+    return lifecycle",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    // --- Signal 5: new CONTROL_COMMAND_* constant ---

    #[test]
    fn new_control_command_constant_fires_signal_5() {
        let d = diff(&[
            "diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py",
            "--- a/src/flywheel/invoker_client.py",
            "+++ b/src/flywheel/invoker_client.py",
            "@@ -67,2 +67,3 @@",
            " CONTROL_COMMAND_REJECT: str = \"reject\"",
            "+CONTROL_COMMAND_RESUBMIT: str = \"resubmit\"",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::ControlCommandVerb)
        );
    }

    #[test]
    fn unrelated_constant_in_invoker_client_does_not_fire_signal_5() {
        let d = diff(&[
            "diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py",
            "--- a/src/flywheel/invoker_client.py",
            "+++ b/src/flywheel/invoker_client.py",
            "@@ -54,2 +54,3 @@",
            " DEFAULT_CONTROL_POLL_INTERVAL: float = 0.25",
            "+DEFAULT_RETRY_BACKOFF: float = 0.5",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    // --- Combined / edge cases ---

    #[test]
    fn multiple_signals_in_one_phase_diff() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -16,2 +16,3 @@",
            "     INTERRUPTED = \"interrupted\"",
            "+    REVIEWING = \"reviewing\"",
            "diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py",
            "--- a/src/flywheel/invoker_client.py",
            "+++ b/src/flywheel/invoker_client.py",
            "@@ -67,2 +67,3 @@",
            " CONTROL_COMMAND_REJECT: str = \"reject\"",
            "+CONTROL_COMMAND_REVIEW: str = \"review\"",
        ]);
        assert_eq!(
            detect_loop_path_signals(&d),
            BTreeSet::from([
                LoopPathSignal::StatusOrTransition,
                LoopPathSignal::ControlCommandVerb
            ])
        );
    }

    #[test]
    fn empty_and_garbage_diffs_return_empty_set() {
        assert!(detect_loop_path_signals("").is_empty());
        assert!(detect_loop_path_signals("+    REVIEWING = \"reviewing\"\n").is_empty());
        assert!(detect_loop_path_signals("+++ \n@@\n+CONTROL_COMMAND_X = 1").is_empty());
    }

    #[test]
    fn removed_status_member_does_not_fire() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -17,2 +17,1 @@",
            "     INTERRUPTED = \"interrupted\"",
            "-    OLD_STATE = \"old_state\"",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn context_line_with_watched_symbol_does_not_fire() {
        // The modified Status.FAILED line matches the transition pattern
        // because the detector is intentionally over-approximating; the
        // context Status.DONE line must not contribute anything more.
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -57,3 +57,3 @@",
            "     Status.DONE: frozenset(),",
            "-    Status.FAILED: frozenset(),",
            "+    Status.FAILED: frozenset(),  # trailing comment tweak",
            " }",
        ]);
        assert!(detect_loop_path_signals(&d).is_subset(&only(LoopPathSignal::StatusOrTransition)));
    }

    #[test]
    fn pure_docstring_addition_in_lifecycle_does_not_fire() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -28,2 +28,3 @@",
            " class LifecycleTransitionError(ValueError):",
            "     \"\"\"Raised when a Lifecycle transition violates docs/task-lifecycle.md.\"\"\"",
            "+    # Updated wording for the failure message.",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn add_then_revert_within_one_phase_nets_no_signal() {
        let d = diff(&[
            "diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py",
            "--- a/src/flywheel/lifecycle.py",
            "+++ b/src/flywheel/lifecycle.py",
            "@@ -1,1 +1,1 @@",
            "-from dataclasses import dataclass, field",
            "+from dataclasses import dataclass, field  # (cumulative no-op)",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn non_watched_file_with_status_pattern_does_not_fire() {
        let d = diff(&[
            "diff --git a/tests/fixtures/sample.py b/tests/fixtures/sample.py",
            "--- a/tests/fixtures/sample.py",
            "+++ b/tests/fixtures/sample.py",
            "@@ -1,1 +1,2 @@",
            " # sample fixture",
            "+    REVIEWING = \"reviewing\"",
        ]);
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn header_paths_strip_tab_suffixes_and_crlf() {
        let d = "diff --git a/x/lifecycle.py b/x/lifecycle.py\r\n--- a/x/lifecycle.py\t2026-01-01\r\n+++ b/x/lifecycle.py\t2026-01-02\r\n@@ -1,1 +1,2 @@\r\n+    NEW = \"new\"\r\n";
        let sections = parse_diff(d);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].new_path, "x/lifecycle.py");
        assert!(!sections[0].is_new_file);
        assert_eq!(sections[0].added_lines, vec!["    NEW = \"new\""]);
        assert_eq!(
            detect_loop_path_signals(d),
            only(LoopPathSignal::StatusOrTransition)
        );
    }

    // --- Rust-crate equivalents ---

    const LIFECYCLE_RS: &str = "crates/flywheel-core/src/lifecycle.rs";

    fn rust_diff(path: &str, hunk: &str, body: &[&str]) -> String {
        let mut lines = vec![
            format!("diff --git a/{path} b/{path}"),
            format!("--- a/{path}"),
            format!("+++ b/{path}"),
            hunk.to_owned(),
        ];
        lines.extend(body.iter().map(|l| (*l).to_owned()));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        diff(&refs)
    }

    #[test]
    fn rust_status_member_addition_fires_signal_1() {
        let d = rust_diff(
            LIFECYCLE_RS,
            "@@ -24,6 +24,7 @@ pub enum Status {",
            &[
                "     Done,",
                "     Failed,",
                "     Interrupted,",
                "+    Reviewing,",
                " }",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::StatusOrTransition)
        );
        let arm = rust_diff(
            LIFECYCLE_RS,
            "@@ -54,3 +54,4 @@ impl Status {",
            &[
                "             Status::Failed => \"failed\",",
                "+            Status::Reviewing => \"reviewing\",",
                "             Status::Interrupted => \"interrupted\",",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&arm),
            only(LoopPathSignal::StatusOrTransition)
        );
    }

    #[test]
    fn rust_transition_edge_fires_signal_1() {
        // A new target inside an existing `valid_targets` row.
        let d = rust_diff(
            LIFECYCLE_RS,
            "@@ -94,2 +94,3 @@ impl Status {",
            &[
                "             Status::AwaitingApproval => &[Status::Done, Status::FailedValidation],",
                "-            Status::FailedValidation => &[Status::Ready, Status::Failed],",
                "+            Status::FailedValidation => &[Status::Ready, Status::Failed, Status::Interrupted],",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::StatusOrTransition)
        );
        let multi_line = rust_diff(
            LIFECYCLE_RS,
            "@@ -81,5 +81,6 @@ impl Status {",
            &[
                "             Status::Running => &[",
                "                 Status::Validating,",
                "                 Status::Failed,",
                "+                Status::AwaitingApproval,",
                "                 Status::Interrupted,",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&multi_line),
            only(LoopPathSignal::StatusOrTransition)
        );
    }

    #[test]
    fn rust_lifecycle_refactor_and_comments_do_not_fire() {
        let d = rust_diff(
            LIFECYCLE_RS,
            "@@ -320,4 +320,5 @@ impl Lifecycle {",
            &[
                "-        let tail = self.attempts.last();",
                "+        // The newest attempt decides whether budget remains.",
                "+        let last = self.attempts.last();",
                "+        let Some(last) = last else { return 0 };",
            ],
        );
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn rust_schema_sql_fires_signal_2_only_in_the_schema_dir() {
        let d = rust_diff(
            "crates/flywheel-core/schema/persistence-schema.sql",
            "@@ -78,3 +78,4 @@",
            &[
                "   awaiting_manual_ordinal INTEGER",
                " );",
                "+ALTER TABLE lifecycles ADD COLUMN review_notes TEXT;",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
        let postgres = d.replace("persistence-schema.sql", "persistence-schema-postgres.sql");
        assert_eq!(
            detect_loop_path_signals(&postgres),
            only(LoopPathSignal::SchemaColumnOrTable)
        );
        let fixture = d.replace(
            "crates/flywheel-core/schema/persistence-schema.sql",
            "crates/flywheel-core/tests/fixtures/v3-store.sql",
        );
        assert!(detect_loop_path_signals(&fixture).is_empty());
    }

    #[test]
    fn rust_grader_variant_and_module_fire_signal_3() {
        let d = rust_diff(
            "crates/flywheel-core/src/task.rs",
            "@@ -312,5 +312,6 @@ pub enum Grader {",
            &[
                "     Manual(ManualGrader),",
                "     Transcript(TranscriptGrader),",
                "+    Review(ReviewGrader),",
                " }",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::GraderVariant)
        );
        let strukt = rust_diff(
            "crates/flywheel-core/src/task.rs",
            "@@ -290,0 +291,4 @@",
            &[
                "+#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]",
                "+pub struct ReviewGrader {",
                "+    pub reviewers: Vec<String>,",
                "+}",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&strukt),
            only(LoopPathSignal::GraderVariant)
        );
        let module = diff(&[
            "diff --git a/crates/flywheel-core/src/grader_review.rs b/crates/flywheel-core/src/grader_review.rs",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/crates/flywheel-core/src/grader_review.rs",
            "@@ -0,0 +1,1 @@",
            "+//! Review grader.",
        ]);
        assert_eq!(
            detect_loop_path_signals(&module),
            only(LoopPathSignal::GraderVariant)
        );
        // A non-grader edit to task.rs does not fire.
        let field = rust_diff(
            "crates/flywheel-core/src/task.rs",
            "@@ -190,3 +190,4 @@ pub struct CommandGrader {",
            &["     pub run: String,", "+    pub cwd: Option<String>,"],
        );
        assert!(detect_loop_path_signals(&field).is_empty());
    }

    #[test]
    fn rust_store_trait_method_fires_signal_4() {
        let d = rust_diff(
            "crates/flywheel-core/src/store.rs",
            "@@ -712,3 +712,8 @@ pub trait AttemptStore {",
            &[
                "     fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError>;",
                "+",
                "+    /// Every review recorded against the run.",
                "+    fn list_reviews(",
                "+        &self,",
                "+        run_id: &str,",
                "+    ) -> Result<Vec<Review>, StoreError>;",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::ProtocolMethod)
        );
        let one_line = rust_diff(
            "crates/flywheel-core/src/store.rs",
            "@@ -800,1 +800,2 @@ pub trait ControlCommandStore {",
            &[
                "     fn delete_command(&self, command_id: i64) -> Result<(), StoreError>;",
                "+    fn purge_commands(&self, run_id: &str) -> Result<u64, StoreError>;",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&one_line),
            only(LoopPathSignal::ProtocolMethod)
        );
    }

    #[test]
    fn rust_store_helpers_impls_and_tests_do_not_fire_signal_4() {
        let d = rust_diff(
            "crates/flywheel-core/src/store.rs",
            "@@ -950,3 +950,16 @@ mod tests {",
            &[
                "+    fn seed_review(run_id: &str) -> Review {",
                "+        Review::new(run_id)",
                "+    }",
                "+",
                "+    // fn not_a_contract(&self) -> u32;",
                "+",
                "+impl SpendSummary {",
                "+    pub fn is_zero(&self) -> bool {",
                "+        self.total_cost_usd == 0.0",
                "+    }",
                "+    fn fmt_short(",
                "+        &self,",
                "+    ) -> String {",
            ],
        );
        assert!(detect_loop_path_signals(&d).is_empty());
    }

    #[test]
    fn rust_control_command_const_fires_signal_5() {
        let d = rust_diff(
            "crates/flywheel-core/src/invoker_client.rs",
            "@@ -87,1 +87,2 @@",
            &[
                " pub const CONTROL_COMMAND_REJECT: &str = \"reject\";",
                "+pub const CONTROL_COMMAND_RESUBMIT: &str = \"resubmit\";",
            ],
        );
        assert_eq!(
            detect_loop_path_signals(&d),
            only(LoopPathSignal::ControlCommandVerb)
        );
        let usage = rust_diff(
            "crates/flywheel-core/src/invoker_client.rs",
            "@@ -140,1 +140,2 @@",
            &["+        CONTROL_COMMAND_REJECT => apply_reject(command),"],
        );
        assert!(detect_loop_path_signals(&usage).is_empty());
    }

    #[test]
    fn signal_serde_and_display_use_snake_case() {
        for signal in LoopPathSignal::ALL {
            let json = serde_json::to_string(&signal).unwrap();
            assert_eq!(json, format!("\"{}\"", signal.as_str()));
            let back: LoopPathSignal = serde_json::from_str(&json).unwrap();
            assert_eq!(back, signal);
            assert_eq!(signal.to_string(), signal.as_str());
        }
    }
}
