//! File implementation of the [`TelemetrySink`] contract from `crate::store`.
//!
//! Writes one append-only JSONL file per run at
//! `<logs_root>/runs/<run_id>.jsonl` (default logs root
//! [`DEFAULT_LOGS_ROOT`]): one JSON object per line, in emission order.
//! File write order is the canonical observability ordering for the run;
//! the sink never rewrites, reorders, or truncates a file it has appended
//! to.
//!
//! Durability semantics: each append is a single `write_all` of the whole
//! line on an unbuffered handle, so a line reaches the OS as soon as
//! `append_telemetry` returns. No fsync: telemetry loss on power failure is
//! acceptable per the data taxonomy; the relational ledger remains the
//! source of truth for state.
//!
//! Concurrency: distinct runs write disjoint files keyed by `run_id`, so
//! concurrent runs never contend. Creating the `runs/` directory is
//! idempotent and safe when multiple sinks race on first append. Within one
//! sink instance a lock serializes appends so interleaved callers cannot
//! split a line.
//!
//! Wire compatibility: every line is byte-identical to what the Python
//! `FileTelemetrySink` writes for the same record (`json.dumps(obj,
//! ensure_ascii=False)` separators, key order, Python float `repr`, aware
//! UTC `isoformat()` timestamps), so the Python and Rust audit readers can
//! read each other's files. The encoder is exposed as [`python_json_dumps`]
//! for the other read-side renderers that must match Python output.
//!
//! Like the run database, run files are verbatim and sensitive by default;
//! redaction is read-time (see `crate::redaction`).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::event_serde::format_timestamp;
use crate::store::{JsonObject, StoreError, TelemetryRecord, TelemetrySink};
use crate::task::python_float_repr;

/// The spec's default logs root, relative to the working directory.
pub const DEFAULT_LOGS_ROOT: &str = ".flywheel/logs";

/// Subdirectory of the logs root holding the per-run JSONL files.
pub const RUNS_DIRNAME: &str = "runs";

/// A `run_id` that cannot form a safe single-segment filename.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("run_id {0:?} is not a valid file key")]
pub struct InvalidRunId(pub String);

/// Reject run ids that cannot form a safe single-segment filename: empty,
/// `.`/`..`, or containing a path separator of either flavor.
pub fn validate_run_id(run_id: &str) -> Result<(), InvalidRunId> {
    if run_id.is_empty()
        || run_id.contains('/')
        || run_id.contains('\\')
        || run_id == "."
        || run_id == ".."
    {
        return Err(InvalidRunId(run_id.to_owned()));
    }
    Ok(())
}

/// JSONL-file [`TelemetrySink`].
///
/// One instance serves any number of runs: appends are routed to
/// `<logs_root>/runs/<record.run_id>.jsonl` and the file handle is cached
/// per run for the lifetime of the sink (or until [`close`](Self::close)).
/// Files are opened in append mode, so a second sink, or a sink re-created
/// after `close`, continues the same file without rewriting prior lines.
///
/// `close()` releases every cached handle and is idempotent; dropping the
/// sink closes it.
#[derive(Debug)]
pub struct FileTelemetrySink {
    runs_dir: PathBuf,
    handles: Mutex<HashMap<String, File>>,
}

impl Default for FileTelemetrySink {
    fn default() -> Self {
        Self::new(DEFAULT_LOGS_ROOT)
    }
}

impl FileTelemetrySink {
    pub fn new(logs_root: impl AsRef<Path>) -> Self {
        Self {
            runs_dir: logs_root.as_ref().join(RUNS_DIRNAME),
            handles: Mutex::new(HashMap::new()),
        }
    }

    /// Directory holding the per-run JSONL files.
    pub fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    /// Return the JSONL path for `run_id` without creating it.
    pub fn path_for(&self, run_id: &str) -> Result<PathBuf, InvalidRunId> {
        validate_run_id(run_id)?;
        Ok(self.runs_dir.join(format!("{run_id}.jsonl")))
    }

    /// Release every cached file handle. Idempotent.
    pub fn close(&self) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.clear();
    }

    fn append_line(&self, run_id: &str, line: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for(run_id).map_err(StoreError::backend)?;
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let handle = match handles.entry(run_id.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                std::fs::create_dir_all(&self.runs_dir)?;
                let file = OpenOptions::new().append(true).create(true).open(&path)?;
                entry.insert(file)
            }
        };
        handle.write_all(line)?;
        handle.flush()?;
        Ok(())
    }
}

impl TelemetrySink for FileTelemetrySink {
    /// Append `record` as one JSON line to the run's file.
    ///
    /// The whole line (including the trailing newline) is handed to the OS
    /// in one write before returning; appends are strictly ordered per run
    /// within this sink instance.
    fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
        let mut line = telemetry_line(record);
        line.push('\n');
        self.append_line(&record.run_id, line.as_bytes())
    }
}

impl Drop for FileTelemetrySink {
    fn drop(&mut self) {
        self.close();
    }
}

/// Project a [`TelemetryRecord`] onto its JSONL line shape.
///
/// Timestamps become ISO-8601 strings; the payload object is persisted
/// verbatim. `run_id` is included so each line is self-contained even when
/// the file is moved or concatenated by external tooling. Key order is the
/// Python writer's: `kind`, `ts`, `run_id`, `attempt_number`,
/// `iteration_number`, `payload`.
pub fn telemetry_record_to_json(record: &TelemetryRecord) -> JsonObject {
    let mut obj = Map::with_capacity(6);
    obj.insert("kind".to_owned(), Value::String(record.kind.clone()));
    obj.insert("ts".to_owned(), Value::String(format_timestamp(&record.ts)));
    obj.insert("run_id".to_owned(), Value::String(record.run_id.clone()));
    obj.insert(
        "attempt_number".to_owned(),
        record.attempt_number.map_or(Value::Null, Value::from),
    );
    obj.insert(
        "iteration_number".to_owned(),
        record.iteration_number.map_or(Value::Null, Value::from),
    );
    obj.insert("payload".to_owned(), Value::Object(record.payload.clone()));
    obj
}

/// The exact line (without the trailing newline) the sink appends for
/// `record`: Python's `json.dumps(obj, ensure_ascii=False)`.
pub fn telemetry_line(record: &TelemetryRecord) -> String {
    python_json_dumps(
        &Value::Object(telemetry_record_to_json(record)),
        &JsonStyle::SINK_LINE,
    )
}

// --- Python json.dumps parity ---------------------------------------------------

/// The knobs of Python's `json.dumps` that flywheel's on-disk and
/// operator-facing JSON surfaces vary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonStyle {
    /// Text between array items and object members (`", "` by default).
    pub item_separator: &'static str,
    /// Text between an object key and its value (`": "` by default).
    pub key_separator: &'static str,
    /// Escape every non-ASCII character as `\uXXXX` (Python's default).
    pub ensure_ascii: bool,
    /// Emit object members in code-point key order instead of insertion
    /// order.
    pub sort_keys: bool,
}

impl JsonStyle {
    /// `json.dumps(obj)` with every default.
    pub const PYTHON_DEFAULT: JsonStyle = JsonStyle {
        item_separator: ", ",
        key_separator: ": ",
        ensure_ascii: true,
        sort_keys: false,
    };

    /// `json.dumps(obj, ensure_ascii=False)`: the sink's line format.
    pub const SINK_LINE: JsonStyle = JsonStyle {
        ensure_ascii: false,
        ..JsonStyle::PYTHON_DEFAULT
    };

    /// `json.dumps(obj, sort_keys=True)`: the audit CLI's NDJSON format.
    pub const SORTED: JsonStyle = JsonStyle {
        sort_keys: true,
        ..JsonStyle::PYTHON_DEFAULT
    };

    /// `json.dumps(obj, sort_keys=True, separators=(",", ":"))`: the
    /// canonical compact form (also `crate::loaders::canonical_json`).
    pub const COMPACT_SORTED: JsonStyle = JsonStyle {
        item_separator: ",",
        key_separator: ":",
        ensure_ascii: true,
        sort_keys: true,
    };
}

impl Default for JsonStyle {
    fn default() -> Self {
        JsonStyle::PYTHON_DEFAULT
    }
}

/// Serialize `value` exactly as Python's `json.dumps` would under `style`.
///
/// Integers print verbatim, floats use Python's `repr` (`1e+16`, `1e-05`,
/// `100.0`), strings use Python's escaping rules (`\n`, `\u00XX` for other
/// control characters, `\uXXXX` with surrogate pairs for non-ASCII when
/// `ensure_ascii` is set), and object keys keep insertion order unless
/// `sort_keys` is set.
pub fn python_json_dumps(value: &Value, style: &JsonStyle) -> String {
    let mut out = String::new();
    write_value(value, style, &mut out);
    out
}

fn write_value(value: &Value, style: &JsonStyle, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&python_float_repr(n.as_f64().unwrap_or(0.0)));
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Value::String(s) => write_python_string(s, style.ensure_ascii, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_separator);
                }
                write_value(item, style, out);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = obj.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_separator);
                }
                write_python_string(key, style.ensure_ascii, out);
                out.push_str(style.key_separator);
                write_value(item, style, out);
            }
            out.push('}');
        }
    }
}

/// Python `json.dumps` string encoding (`py_encode_basestring` or its
/// `_ascii` variant).
fn write_python_string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{00}'..='\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ' '..='~' => out.push(ch),
            _ if !ensure_ascii => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::sync::{Arc, Barrier};

    fn ts(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn record(run_id: &str, kind: &str, payload: Value) -> TelemetryRecord {
        let mut record = TelemetryRecord::new(run_id, ts("2026-06-11T12:00:00+00:00"), kind);
        record.payload = payload.as_object().unwrap().clone();
        record.attempt_number = Some(1);
        record.iteration_number = Some(1);
        record
    }

    fn lines(path: &Path) -> Vec<Value> {
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.ends_with('\n'));
        raw.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // --- Byte parity with the Python writer ---------------------------------

    /// The exact bytes `flywheel_core.telemetry_file.FileTelemetrySink`
    /// wrote for three records (captured from the Python implementation).
    const PYTHON_LINE_1: &str = r#"{"kind": "message_turn", "ts": "2026-06-11T12:00:00+00:00", "run_id": "run-1", "attempt_number": 1, "iteration_number": 1, "payload": {"text": "hello"}}"#;
    const PYTHON_LINE_2: &str = "{\"kind\": \"harness.iteration_start\", \"ts\": \"2026-06-11T12:00:01.123456+00:00\", \"run_id\": \"run-1\", \"attempt_number\": null, \"iteration_number\": null, \"payload\": {\"u\": \"\u{e9}\u{1F600} \\u0001\\t\\\"q\\\"\\\\ /\", \"f\": [1e+16, 1e-05, 2.5, 100.0, -0.0, 0.30000000000000004], \"i\": [0, -7, 9007199254740993], \"n\": null, \"b\": [true, false], \"nested\": {\"z\": {\"y\": []}, \"a\": {}}}}";
    const PYTHON_LINE_3: &str = r#"{"kind": "domain.landed", "ts": "2026-06-11T12:00:00+00:00", "run_id": "run-1", "attempt_number": 2, "iteration_number": null, "payload": {"strategy": "merge"}}"#;

    fn python_fixture_records() -> Vec<TelemetryRecord> {
        let mut second = TelemetryRecord::new(
            "run-1",
            ts("2026-06-11T12:00:01.123456+00:00"),
            "harness.iteration_start",
        );
        second.payload = json!({
            "u": "\u{e9}\u{1F600} \u{01}\t\"q\"\\ /",
            "f": [1e16, 1e-05, 2.5, 100.0, -0.0, 0.30000000000000004],
            "i": [0, -7, 9007199254740993u64],
            "n": null,
            "b": [true, false],
            "nested": {"z": {"y": []}, "a": {}},
        })
        .as_object()
        .unwrap()
        .clone();
        let mut third =
            TelemetryRecord::new("run-1", ts("2026-06-11T12:00:00+00:00"), "domain.landed");
        third.payload = json!({"strategy": "merge"}).as_object().unwrap().clone();
        third.attempt_number = Some(2);
        vec![
            record("run-1", "message_turn", json!({"text": "hello"})),
            second,
            third,
        ]
    }

    #[test]
    fn writer_output_is_byte_identical_to_python_writer() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path());
        for record in python_fixture_records() {
            sink.append_telemetry(&record).unwrap();
        }
        let written = std::fs::read_to_string(sink.path_for("run-1").unwrap()).unwrap();
        let expected = format!("{PYTHON_LINE_1}\n{PYTHON_LINE_2}\n{PYTHON_LINE_3}\n");
        assert_eq!(written, expected);
    }

    #[test]
    fn python_written_lines_parse_into_equal_records() {
        for (line, expected) in [PYTHON_LINE_1, PYTHON_LINE_2, PYTHON_LINE_3]
            .into_iter()
            .zip(python_fixture_records())
        {
            let parsed: TelemetryRecord = serde_json::from_str(line).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(telemetry_line(&parsed), line);
        }
    }

    #[test]
    fn python_json_dumps_matches_cpython_styles() {
        let value = json!({"b": [1, 2.0, null, true, 1e-05], "a": {"z": "\u{e9}\u{7f}\u{01}\n\"/\\", "y": []}});
        assert_eq!(
            python_json_dumps(&value, &JsonStyle::PYTHON_DEFAULT),
            r#"{"b": [1, 2.0, null, true, 1e-05], "a": {"z": "\u00e9\u007f\u0001\n\"/\\", "y": []}}"#
        );
        assert_eq!(
            python_json_dumps(&value, &JsonStyle::SINK_LINE),
            "{\"b\": [1, 2.0, null, true, 1e-05], \"a\": {\"z\": \"\u{e9}\u{7f}\\u0001\\n\\\"/\\\\\", \"y\": []}}"
        );
        assert_eq!(
            python_json_dumps(&value, &JsonStyle::SORTED),
            r#"{"a": {"y": [], "z": "\u00e9\u007f\u0001\n\"/\\"}, "b": [1, 2.0, null, true, 1e-05]}"#
        );
        assert_eq!(
            python_json_dumps(&value, &JsonStyle::COMPACT_SORTED),
            crate::loaders::canonical_json(&value)
        );
        assert_eq!(
            python_json_dumps(&json!({"a": "\u{1F600}"}), &JsonStyle::PYTHON_DEFAULT),
            r#"{"a": "\ud83d\ude00"}"#
        );
        assert_eq!(JsonStyle::default(), JsonStyle::PYTHON_DEFAULT);
    }

    // --- File layout --------------------------------------------------------

    #[test]
    fn default_logs_root_and_path_for() {
        let sink = FileTelemetrySink::default();
        assert_eq!(
            sink.path_for("run-x").unwrap(),
            PathBuf::from(".flywheel/logs/runs/run-x.jsonl")
        );
        assert_eq!(sink.runs_dir(), Path::new(".flywheel/logs/runs"));
    }

    #[test]
    fn append_writes_jsonl_under_runs_dir_in_emission_order() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path());
        for n in 0..50 {
            sink.append_telemetry(&record("run-1", "tick", json!({"n": n})))
                .unwrap();
        }
        let path = dir.path().join("runs").join("run-1.jsonl");
        assert!(path.is_file());
        let payloads: Vec<Value> = lines(&path)
            .into_iter()
            .map(|l| l["payload"].clone())
            .collect();
        assert_eq!(
            payloads,
            (0..50).map(|n| json!({"n": n})).collect::<Vec<_>>()
        );
        assert_eq!(
            lines(&path)[0],
            json!({"kind": "tick", "ts": "2026-06-11T12:00:00+00:00", "run_id": "run-1",
                   "attempt_number": 1, "iteration_number": 1, "payload": {"n": 0}})
        );
    }

    #[test]
    fn each_line_is_visible_before_append_returns_and_none_is_null() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path());
        let mut record = record("run-1", "message_turn", json!({"n": 0}));
        record.attempt_number = None;
        record.iteration_number = None;
        sink.append_telemetry(&record).unwrap();
        let line = &lines(&sink.path_for("run-1").unwrap())[0];
        assert_eq!(line["payload"], json!({"n": 0}));
        assert_eq!(line["attempt_number"], Value::Null);
        assert_eq!(line["iteration_number"], Value::Null);
    }

    #[test]
    fn new_sink_appends_and_close_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let sink = FileTelemetrySink::new(dir.path());
            sink.append_telemetry(&record("run-1", "k", json!({"n": 0})))
                .unwrap();
        }
        let sink = FileTelemetrySink::new(dir.path());
        sink.append_telemetry(&record("run-1", "k", json!({"n": 1})))
            .unwrap();
        sink.close();
        sink.close();
        sink.append_telemetry(&record("run-1", "k", json!({"n": 2})))
            .unwrap();
        sink.close();
        let payloads: Vec<Value> = lines(&sink.path_for("run-1").unwrap())
            .into_iter()
            .map(|l| l["payload"].clone())
            .collect();
        assert_eq!(
            payloads,
            vec![json!({"n": 0}), json!({"n": 1}), json!({"n": 2})]
        );
    }

    // --- Concurrency --------------------------------------------------------

    #[test]
    fn concurrent_runs_write_disjoint_files() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(FileTelemetrySink::new(dir.path()));
        let threads: Vec<_> = ["run-a", "run-b"]
            .into_iter()
            .map(|run_id| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for n in 0..25 {
                        sink.append_telemetry(&record(run_id, "tick", json!({"n": n})))
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for run_id in ["run-a", "run-b"] {
            let lines = lines(&sink.path_for(run_id).unwrap());
            assert!(lines.iter().all(|l| l["run_id"] == run_id));
            let payloads: Vec<Value> = lines.into_iter().map(|l| l["payload"].clone()).collect();
            assert_eq!(
                payloads,
                (0..25).map(|n| json!({"n": n})).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn two_sinks_race_on_first_append_in_same_root() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = ["run-a", "run-b"]
            .into_iter()
            .map(|run_id| {
                let root = dir.path().to_path_buf();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let sink = FileTelemetrySink::new(&root);
                    barrier.wait();
                    sink.append_telemetry(&record(run_id, "k", json!({})))
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        assert!(dir.path().join("runs/run-a.jsonl").is_file());
        assert!(dir.path().join("runs/run-b.jsonl").is_file());
    }

    // --- Run-id safety ------------------------------------------------------

    #[test]
    fn unsafe_run_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path());
        for bad in ["", ".", "..", "a/b", "a\\b", "../escape"] {
            assert_eq!(
                sink.path_for(bad).unwrap_err(),
                InvalidRunId(bad.to_owned())
            );
            let err = sink
                .append_telemetry(&record(bad, "k", json!({})))
                .unwrap_err();
            assert!(matches!(err, StoreError::Backend(_)));
            assert_eq!(
                err.to_string(),
                format!("store backend: run_id {bad:?} is not a valid file key")
            );
        }
        assert!(!dir.path().join("runs").exists());
    }
}
