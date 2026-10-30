//! The `Task` / `Grader` / `Context` shape and validation rules.
//!
//! Authoritative spec: `docs/task-schema.md`.
//!
//! This module is **pure**: no filesystem, no IO, no environment access.
//! `Task` is input-source agnostic -- direct construction is a first-class
//! API and loaders (`crate::loaders`) are optional conveniences. No field may
//! name a path, file, payload, or source.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};

/// Raised when a task or grader violates `docs/task-schema.md`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

impl ValidationError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// The closed set of grader discriminators, in cost order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderType {
    Command,
    Transcript,
    Rubric,
    Manual,
}

impl GraderType {
    pub const fn as_str(self) -> &'static str {
        match self {
            GraderType::Command => "command",
            GraderType::Transcript => "transcript",
            GraderType::Rubric => "rubric",
            GraderType::Manual => "manual",
        }
    }

    /// Execution order: cost-cheapest first (`docs/task-schema.md`).
    pub const COST_ORDER: [GraderType; 4] = [
        GraderType::Command,
        GraderType::Transcript,
        GraderType::Rubric,
        GraderType::Manual,
    ];
}

impl fmt::Display for GraderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for GraderType {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "command" => Ok(GraderType::Command),
            "transcript" => Ok(GraderType::Transcript),
            "rubric" => Ok(GraderType::Rubric),
            "manual" => Ok(GraderType::Manual),
            other => Err(ValidationError::new(format!(
                "unknown grader type {other:?}; expected one of {KNOWN_GRADER_TYPES:?}"
            ))),
        }
    }
}

/// Grader discriminator strings as they appear in task sources.
pub const KNOWN_GRADER_TYPES: [&str; 4] = ["command", "rubric", "manual", "transcript"];

/// A seconds value that remembers whether its source wrote an integer or a
/// float, so content hashes, serialized task versions, and rendered prompts
/// stay byte-identical to the Python implementation (`30` vs `30.0`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seconds {
    Int(i64),
    Float(f64),
}

impl Seconds {
    pub fn as_f64(self) -> f64 {
        match self {
            Seconds::Int(n) => n as f64,
            Seconds::Float(f) => f,
        }
    }
}

impl From<i64> for Seconds {
    fn from(n: i64) -> Self {
        Seconds::Int(n)
    }
}

impl From<f64> for Seconds {
    fn from(f: f64) -> Self {
        Seconds::Float(f)
    }
}

impl From<Seconds> for serde_json::Value {
    fn from(s: Seconds) -> Self {
        match s {
            Seconds::Int(n) => n.into(),
            Seconds::Float(f) => f.into(),
        }
    }
}

/// Renders exactly as Python's `str()` of the source number.
impl fmt::Display for Seconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Seconds::Int(n) => write!(f, "{n}"),
            Seconds::Float(x) => f.write_str(&python_float_repr(*x)),
        }
    }
}

/// Python `float.__repr__`: shortest round-trip digits, fixed notation when
/// the decimal exponent is in `[-4, 16)`, otherwise `d.ddde[+-]XX`.
pub fn python_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    // `{:e}` yields the shortest round-trip digits as `[-]d[.ddd]e[-]N`.
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // value = 0.<digits> * 10^decpt
    let decpt = exp + 1;
    let mut out = String::from(sign);
    if decpt <= -4 || decpt > 16 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let decpt = decpt.unsigned_abs() as usize;
        if decpt < digits.len() {
            out.push_str(&digits[..decpt]);
            out.push('.');
            out.push_str(&digits[decpt..]);
        } else {
            out.push_str(&digits);
            for _ in digits.len()..decpt {
                out.push('0');
            }
            out.push_str(".0");
        }
    }
    out
}

/// `command` -- deterministic shell check. Pass = exit 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandGrader {
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl CommandGrader {
    pub fn new(run: impl Into<String>) -> Result<Self, ValidationError> {
        let grader = Self {
            run: run.into(),
            name: None,
        };
        grader.validate()?;
        Ok(grader)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.run.is_empty() {
            return Err(ValidationError::new(
                "command grader requires a non-empty 'run'",
            ));
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

/// `rubric` -- LLM-judged semantic check over natural-language assertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubricGrader {
    pub assertions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
    #[serde(default = "default_true")]
    pub retry_on_fail: bool,
}

impl RubricGrader {
    pub fn new(assertions: Vec<String>) -> Result<Self, ValidationError> {
        let grader = Self {
            assertions,
            rubric: None,
            name: None,
            judge_model: None,
            retry_on_fail: true,
        };
        grader.validate()?;
        Ok(grader)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.assertions.is_empty() {
            return Err(ValidationError::new(
                "rubric grader requires a non-empty 'assertions' list",
            ));
        }
        Ok(())
    }
}

/// `manual` -- declared human-approval gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualGrader {
    pub instruction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ManualGrader {
    pub fn new(instruction: impl Into<String>) -> Result<Self, ValidationError> {
        let grader = Self {
            instruction: instruction.into(),
            name: None,
        };
        grader.validate()?;
        Ok(grader)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.instruction.is_empty() {
            return Err(ValidationError::new(
                "manual grader requires a non-empty 'instruction'",
            ));
        }
        Ok(())
    }
}

/// `transcript` -- path-level constraint over the run record. At least one
/// limit must be set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TranscriptGrader {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_seconds: Option<Seconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl TranscriptGrader {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.max_turns.is_none()
            && self.max_total_tokens.is_none()
            && self.max_wall_seconds.is_none()
        {
            return Err(ValidationError::new(
                "transcript grader requires at least one of \
                 max_turns, max_total_tokens, max_wall_seconds",
            ));
        }
        Ok(())
    }
}

/// Discriminated union of the four grader kinds. The JSON shape carries a
/// `type` tag; deserialization validates required fields so downstream code
/// can match on the variant without re-checking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", try_from = "RawGrader")]
pub enum Grader {
    Command(CommandGrader),
    Rubric(RubricGrader),
    Manual(ManualGrader),
    Transcript(TranscriptGrader),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawGrader {
    Command(CommandGrader),
    Rubric(RubricGrader),
    Manual(ManualGrader),
    Transcript(TranscriptGrader),
}

impl TryFrom<RawGrader> for Grader {
    type Error = ValidationError;
    fn try_from(raw: RawGrader) -> Result<Self, Self::Error> {
        let grader = match raw {
            RawGrader::Command(g) => Grader::Command(g),
            RawGrader::Rubric(g) => Grader::Rubric(g),
            RawGrader::Manual(g) => Grader::Manual(g),
            RawGrader::Transcript(g) => Grader::Transcript(g),
        };
        grader.validate()?;
        Ok(grader)
    }
}

impl Grader {
    pub const fn grader_type(&self) -> GraderType {
        match self {
            Grader::Command(_) => GraderType::Command,
            Grader::Rubric(_) => GraderType::Rubric,
            Grader::Manual(_) => GraderType::Manual,
            Grader::Transcript(_) => GraderType::Transcript,
        }
    }

    /// The optional short label, whichever variant carries it.
    pub fn name(&self) -> Option<&str> {
        match self {
            Grader::Command(g) => g.name.as_deref(),
            Grader::Rubric(g) => g.name.as_deref(),
            Grader::Manual(g) => g.name.as_deref(),
            Grader::Transcript(g) => g.name.as_deref(),
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Grader::Command(g) => g.validate(),
            Grader::Rubric(g) => g.validate(),
            Grader::Manual(g) => g.validate(),
            Grader::Transcript(g) => g.validate(),
        }
    }
}

/// Briefing material the agent reads upfront.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Context {
    pub relevant: Vec<String>,
    pub references: Vec<String>,
    pub constraints: Vec<String>,
    pub non_goals: Vec<String>,
    pub edge_cases: Vec<String>,
    pub notes: String,
}

impl Context {
    pub fn is_empty(&self) -> bool {
        self.relevant.is_empty()
            && self.references.is_empty()
            && self.constraints.is_empty()
            && self.non_goals.is_empty()
            && self.edge_cases.is_empty()
            && self.notes.is_empty()
    }
}

/// Optional per-task execution-budget overrides for the heavyweight tail.
///
/// Every field defaults to `None` -- inherit the harness/policy value -- so a
/// default-constructed instance changes nothing. Seconds fields mirror the
/// `[deadlines]` semantics: a positive number is the ceiling, `0` is the
/// explicit unbounded opt-out for THIS task.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskBudgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_iteration_seconds: Option<Seconds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rubric_judge_seconds: Option<Seconds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rubric_judge_max_turns: Option<u64>,
}

impl TaskBudgets {
    pub fn validate(&self) -> Result<(), ValidationError> {
        for (name, value) in [
            ("agent_iteration_seconds", self.agent_iteration_seconds),
            ("rubric_judge_seconds", self.rubric_judge_seconds),
        ] {
            let Some(value) = value.map(Seconds::as_f64) else {
                continue;
            };
            if !value.is_finite() {
                return Err(ValidationError::new(format!(
                    "budgets.{name} must be a number of seconds"
                )));
            }
            if value < 0.0 {
                return Err(ValidationError::new(format!(
                    "budgets.{name} must be >= 0 (0 opts this task out of \
                     the ceiling entirely)"
                )));
            }
        }
        if let Some(turns) = self.rubric_judge_max_turns
            && turns < 1
        {
            return Err(ValidationError::new(
                "budgets.rubric_judge_max_turns must be a positive integer",
            ));
        }
        Ok(())
    }
}

/// Mint the default task id: `task-<uuid4 hex>`.
pub fn default_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4().simple())
}

/// A single unit of work: the desired outcome and how to verify it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    #[serde(default = "default_task_id")]
    pub id: String,
    pub goal: String,
    #[serde(default)]
    pub graders: Vec<Grader>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub context: Context,
    #[serde(default)]
    pub budgets: TaskBudgets,
}

impl Task {
    /// Construct a task with a generated id and empty optional fields.
    pub fn new(goal: impl Into<String>, graders: Vec<Grader>) -> Self {
        Self {
            id: default_task_id(),
            goal: goal.into(),
            graders,
            tags: Vec::new(),
            context: Context::default(),
            budgets: TaskBudgets::default(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.id.is_empty() {
            return Err(ValidationError::new("id must be a non-empty string"));
        }
        if self.id.chars().any(char::is_whitespace) {
            return Err(ValidationError::new(format!(
                "id {:?} must not contain whitespace",
                self.id
            )));
        }
        validate_id_path_safe(&self.id)?;
        if self.goal.trim().is_empty() {
            return Err(ValidationError::new("goal must be a non-empty string"));
        }
        for grader in &self.graders {
            grader.validate()?;
        }
        self.budgets.validate()
    }

    /// Whether the task declares at least one manual-approval gate.
    pub fn has_manual_gate(&self) -> bool {
        self.graders.iter().any(|g| matches!(g, Grader::Manual(_)))
    }
}

/// Reject a task id that is not a relative, traversal-free path.
///
/// `id` is joined into filesystem paths and git ref names downstream
/// (`worktrees_dir / id`, `sandbox_root / id`, `flywheel/<phase>/<id>`). An
/// absolute or `..`-bearing id would let those joins escape their base. Pure
/// string validation -- no filesystem access.
///
/// `/`-separated segments are allowed (a nested id maps to a nested branch
/// inside the worktree root); each segment must be non-empty, must not be
/// the parent-directory marker `..`, and must contain only `[A-Za-z0-9._-]`.
pub fn validate_id_path_safe(task_id: &str) -> Result<(), ValidationError> {
    for segment in task_id.split('/') {
        if segment.is_empty() {
            return Err(ValidationError::new(format!(
                "id {task_id:?} must be a relative path with no empty \
                 segments (no leading, trailing, or doubled '/')"
            )));
        }
        if segment == ".." {
            return Err(ValidationError::new(format!(
                "id {task_id:?} must not contain a '..' path segment"
            )));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        {
            return Err(ValidationError::new(format!(
                "id {task_id:?} segment {segment:?} must contain only [A-Za-z0-9._-]"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(run: &str) -> Grader {
        Grader::Command(CommandGrader::new(run).unwrap())
    }

    #[test]
    fn default_id_has_task_prefix_and_hex() {
        let t = Task::new("do the thing", vec![]);
        assert!(t.id.starts_with("task-"));
        assert_eq!(t.id.len(), 5 + 32);
        t.validate().unwrap();
    }

    #[test]
    fn rejects_empty_goal_and_whitespace_id() {
        let t = Task::new("   ", vec![]);
        assert!(t.validate().is_err());
        let t = Task::new("x", vec![]).with_id("has space");
        assert_eq!(
            t.validate().unwrap_err().0,
            "id \"has space\" must not contain whitespace"
        );
    }

    #[test]
    fn rejects_path_escaping_ids() {
        for bad in ["/abs", "a//b", "a/", "../x", "a/../b", "a\\b", "a:b", "é"] {
            assert!(
                Task::new("g", vec![]).with_id(bad).validate().is_err(),
                "{bad} should be rejected"
            );
        }
        for good in ["a", "a/b", "a.b-c_d", "phase-01/task.2"] {
            Task::new("g", vec![]).with_id(good).validate().unwrap();
        }
    }

    #[test]
    fn grader_constructors_enforce_required_fields() {
        assert!(CommandGrader::new("").is_err());
        assert!(RubricGrader::new(vec![]).is_err());
        assert!(ManualGrader::new("").is_err());
        assert!(TranscriptGrader::default().validate().is_err());
        assert!(
            TranscriptGrader {
                max_turns: Some(3),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn grader_json_round_trips_with_type_tag() {
        let g = cmd("uv run pytest");
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["type"], "command");
        assert_eq!(json["run"], "uv run pytest");
        assert!(json.get("name").is_none());
        let back: Grader = serde_json::from_value(json).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn grader_deserialize_validates() {
        let err = serde_json::from_str::<Grader>(r#"{"type":"command","run":""}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty 'run'"), "{err}");
        assert!(serde_json::from_str::<Grader>(r#"{"type":"bogus"}"#).is_err());
        let r: Grader = serde_json::from_str(r#"{"type":"rubric","assertions":["a"]}"#).unwrap();
        match r {
            Grader::Rubric(r) => assert!(r.retry_on_fail),
            _ => panic!(),
        }
    }

    #[test]
    fn budgets_validation() {
        let mut b = TaskBudgets::default();
        b.validate().unwrap();
        b.agent_iteration_seconds = Some(Seconds::Int(0));
        b.validate().unwrap();
        b.agent_iteration_seconds = Some(Seconds::Float(-1.0));
        assert!(b.validate().is_err());
        b.agent_iteration_seconds = None;
        b.rubric_judge_max_turns = Some(0);
        assert!(b.validate().is_err());
    }

    #[test]
    fn task_deserialize_ignores_prerequisites() {
        let t: Task = serde_json::from_str(
            r#"{"id":"t1","goal":"g","prerequisites":["x"],"graders":[{"type":"transcript","max_turns":2}]}"#,
        )
        .unwrap();
        t.validate().unwrap();
        assert_eq!(t.graders.len(), 1);
        assert!(t.context.is_empty());
    }
}

#[cfg(test)]
mod seconds_tests {
    use super::*;

    #[test]
    fn seconds_preserve_int_versus_float_through_json() {
        let int: TranscriptGrader = serde_json::from_str(r#"{"max_wall_seconds": 30}"#).unwrap();
        let float: TranscriptGrader =
            serde_json::from_str(r#"{"max_wall_seconds": 30.0}"#).unwrap();
        assert_eq!(int.max_wall_seconds, Some(Seconds::Int(30)));
        assert_eq!(float.max_wall_seconds, Some(Seconds::Float(30.0)));
        assert_eq!(
            serde_json::to_string(&int).unwrap(),
            r#"{"max_wall_seconds":30}"#
        );
        assert_eq!(
            serde_json::to_string(&float).unwrap(),
            r#"{"max_wall_seconds":30.0}"#
        );
        assert_eq!(Seconds::Int(900).to_string(), "900");
        assert_eq!(Seconds::Float(900.0).to_string(), "900.0");
        assert_eq!(Seconds::Float(1e16).to_string(), "1e+16");
        assert_eq!(Seconds::Int(7).as_f64(), 7.0);
    }
}
