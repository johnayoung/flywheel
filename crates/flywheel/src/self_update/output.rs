//! The step lines (human) or JSON Lines (`--json`) `fw self-update` prints.
//!
//! Every run reports each of [`STEPS`] exactly once, in order, then one
//! summary: `self-update: <outcome>` (human) or one `{"summary":{...}}`
//! object (`--json`). A step the run never reached reports `skipped`.

use serde_json::{Value, json};
use std::io::Write;

/// The fixed step order.
pub const STEPS: [Step; 8] = [
    Step::Preflight,
    Step::Build,
    Step::Drain,
    Step::Backup,
    Step::Install,
    Step::DbUpgrade,
    Step::Hub,
    Step::Restart,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Preflight,
    Build,
    Drain,
    Backup,
    Install,
    DbUpgrade,
    Hub,
    Restart,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Build => "build",
            Self::Drain => "drain",
            Self::Backup => "backup",
            Self::Install => "install",
            Self::DbUpgrade => "db-upgrade",
            Self::Hub => "hub",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Skipped,
    Failed,
    Reverted,
    Planned,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Reverted => "reverted",
            Self::Planned => "planned",
        }
    }
}

/// The run's final outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Updated,
    Current,
    Planned,
    Refused,
    Failed,
    UpdatedHubPending,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Current => "current",
            Self::Planned => "planned",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::UpdatedHubPending => "updated-hub-pending",
        }
    }

    /// The process exit code this outcome maps to.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Updated | Self::Current | Self::Planned => 0,
            Self::Failed => 1,
            Self::Refused => 3,
            Self::UpdatedHubPending => 4,
        }
    }
}

/// `{version, commit|null, sha256}`: one side of the update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinaryInfo {
    pub version: Option<String>,
    pub commit: Option<String>,
    pub sha256: Option<String>,
}

impl BinaryInfo {
    fn to_json(&self) -> Value {
        json!({"version": self.version, "commit": self.commit, "sha256": self.sha256})
    }
}

/// `{kind:"checkout", path, commit}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub path: String,
    pub commit: Option<String>,
}

/// Everything the summary reports. The drain / store / hub / restart
/// collections stay empty until a run performs those steps.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub source: Option<SourceInfo>,
    pub from: BinaryInfo,
    pub to: BinaryInfo,
    pub drained: Vec<Value>,
    pub not_stopped: Vec<Value>,
    pub unidentified_claims: Vec<Value>,
    pub backups: Vec<Value>,
    pub upgraded: Vec<Value>,
    pub hub: Option<Value>,
    pub restarted: Vec<Value>,
    pub manual: Vec<Value>,
}

impl Summary {
    pub fn to_json(&self, outcome: Outcome) -> Value {
        let source = self
            .source
            .as_ref()
            .map(|s| json!({"kind": "checkout", "path": s.path, "commit": s.commit}));
        json!({"summary": {
            "outcome": outcome.name(),
            "source": source,
            "from": self.from.to_json(),
            "to": self.to.to_json(),
            "drained": self.drained,
            "not_stopped": self.not_stopped,
            "unidentified_claims": self.unidentified_claims,
            "backups": self.backups,
            "upgraded": self.upgraded,
            "hub": self.hub,
            "restarted": self.restarted,
            "manual": self.manual,
        }})
    }
}

/// Collapse a detail to one line so a step stays one output line.
fn one_line(detail: &str) -> String {
    detail.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Prints each step as it is reported and fills every unreached one with
/// `skipped` before the summary.
pub struct Reporter<'a> {
    out: &'a mut dyn Write,
    json: bool,
    next: usize,
}

impl<'a> Reporter<'a> {
    pub fn new(out: &'a mut dyn Write, json: bool) -> Self {
        Self { out, json, next: 0 }
    }

    /// Report `step`. Steps before it that were never reported print as
    /// `skipped` first; a step already reported (or one before it) is a
    /// programming error and is ignored so the stream stays well-formed.
    pub fn step(&mut self, step: Step, status: Status, detail: Option<&str>) {
        let Some(index) = STEPS.iter().position(|s| *s == step) else {
            return;
        };
        if index < self.next {
            return;
        }
        while self.next < index {
            let skipped = STEPS[self.next];
            self.emit(skipped, Status::Skipped, None);
            self.next += 1;
        }
        self.emit(step, status, detail);
        self.next += 1;
    }

    fn emit(&mut self, step: Step, status: Status, detail: Option<&str>) {
        let detail = detail.map(one_line).filter(|d| !d.is_empty());
        if self.json {
            let line = json!({"step": step.name(), "status": status.name(), "detail": detail});
            let _ = writeln!(self.out, "{line}");
        } else {
            match detail {
                Some(detail) => {
                    let _ = writeln!(
                        self.out,
                        "self-update: {} {} -- {detail}",
                        step.name(),
                        status.name()
                    );
                }
                None => {
                    let _ = writeln!(self.out, "self-update: {} {}", step.name(), status.name());
                }
            }
        }
        let _ = self.out.flush();
    }

    /// Report every remaining step `status`, then the summary.
    pub fn finish(mut self, rest: Status, outcome: Outcome, summary: &Summary) -> i32 {
        while self.next < STEPS.len() {
            let step = STEPS[self.next];
            self.emit(step, rest, None);
            self.next += 1;
        }
        if self.json {
            let _ = writeln!(self.out, "{}", summary.to_json(outcome));
        } else {
            for entry in &summary.manual {
                let pid = entry.get("pid").cloned().unwrap_or(Value::Null);
                let command = entry.get("command").and_then(Value::as_str).unwrap_or("");
                let _ = writeln!(self.out, "manual: {pid} {command}");
            }
            for entry in &summary.not_stopped {
                let field = |key: &str| match entry.get(key) {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let _ = writeln!(
                    self.out,
                    "not-stopped: {} {} {}",
                    field("pid"),
                    field("kind"),
                    field("cwd")
                );
            }
            for entry in &summary.unidentified_claims {
                let field = |key: &str| match entry.get(key) {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let _ = writeln!(
                    self.out,
                    "unidentified-claim: {} {} {} {}",
                    field("project_id"),
                    field("task_id"),
                    field("worker_id"),
                    field("lease_expires_at")
                );
            }
            let _ = writeln!(self.out, "self-update: {}", outcome.name());
        }
        let _ = self.out.flush();
        outcome.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(json: bool, f: impl FnOnce(&mut Reporter<'_>)) -> (String, i32) {
        let mut buf = Vec::new();
        let code = {
            let mut reporter = Reporter::new(&mut buf, json);
            f(&mut reporter);
            reporter.finish(Status::Skipped, Outcome::Refused, &Summary::default())
        };
        (String::from_utf8(buf).unwrap(), code)
    }

    #[test]
    fn step_names_are_the_fixed_order() {
        let names: Vec<&str> = STEPS.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            [
                "preflight",
                "build",
                "drain",
                "backup",
                "install",
                "db-upgrade",
                "hub",
                "restart"
            ]
        );
    }

    #[test]
    fn outcomes_map_to_exit_codes() {
        assert_eq!(Outcome::Updated.exit_code(), 0);
        assert_eq!(Outcome::Current.exit_code(), 0);
        assert_eq!(Outcome::Planned.exit_code(), 0);
        assert_eq!(Outcome::Failed.exit_code(), 1);
        assert_eq!(Outcome::Refused.exit_code(), 3);
        assert_eq!(Outcome::UpdatedHubPending.exit_code(), 4);
        assert_eq!(Outcome::UpdatedHubPending.name(), "updated-hub-pending");
    }

    #[test]
    fn human_mode_prints_one_line_per_step_then_the_outcome() {
        let (text, code) = render(false, |r| {
            r.step(Step::Preflight, Status::Failed, Some("dirty\ntree"));
        });
        assert_eq!(code, 3);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 9);
        assert_eq!(lines[0], "self-update: preflight failed -- dirty tree");
        assert_eq!(lines[1], "self-update: build skipped");
        assert_eq!(lines[7], "self-update: restart skipped");
        assert_eq!(lines[8], "self-update: refused");
    }

    #[test]
    fn unreported_steps_before_a_reported_one_are_skipped_and_order_holds() {
        let (text, _) = render(true, |r| {
            r.step(Step::Preflight, Status::Ok, None);
            r.step(Step::Install, Status::Ok, None);
            // Out of order: ignored.
            r.step(Step::Build, Status::Ok, None);
        });
        let objects: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(objects.len(), 9);
        let steps: Vec<(&str, &str)> = objects[..8]
            .iter()
            .map(|o| (o["step"].as_str().unwrap(), o["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            steps,
            [
                ("preflight", "ok"),
                ("build", "skipped"),
                ("drain", "skipped"),
                ("backup", "skipped"),
                ("install", "ok"),
                ("db-upgrade", "skipped"),
                ("hub", "skipped"),
                ("restart", "skipped"),
            ]
        );
        let summary = &objects[8]["summary"];
        assert_eq!(summary["outcome"], "refused");
        for key in [
            "source",
            "from",
            "to",
            "drained",
            "not_stopped",
            "unidentified_claims",
            "backups",
            "upgraded",
            "hub",
            "restarted",
            "manual",
        ] {
            assert!(summary.get(key).is_some(), "summary misses {key}");
        }
        assert!(objects[0]["detail"].is_null());
    }
}
