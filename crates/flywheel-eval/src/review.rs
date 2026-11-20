//! Transcript review: a cheap reader agent per trial, over failures **and**
//! passes, with a frozen brief ([`ANALYSIS_MD`], hashed into the run), and
//! one coordinator grouping what they found.
//!
//! Readers judge nothing on their own: a reader that calls a pass a fake
//! with high confidence makes it **suspect**, and only the operator's
//! confirmation voids it (`fw eval review decide`). Agent claims --
//! a reader's included -- are untrusted.
//!
//! Coverage: every failure; every pass up to [`ALL_PASSES_UP_TO`] per run,
//! then a seeded [`PASS_SAMPLE`] share of each (variant, task) cell's
//! passes, at least one each.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::TrialRow;

/// The reader's brief. Frozen: its hash is recorded with every review, so
/// a changed brief is a changed evaluator.
pub const ANALYSIS_MD: &str = include_str!("../analysis.md");
/// Every pass is read while a run has at most this many.
pub const ALL_PASSES_UP_TO: usize = 200;
/// The share of passes read beyond that.
pub const PASS_SAMPLE: f64 = 0.25;
/// The reader's envelope fences.
pub const OPEN: &str = "<!-- EVAL_REVIEW -->";
pub const CLOSE: &str = "<!-- /EVAL_REVIEW -->";
/// The verdicts a reader may give.
pub const VERDICTS: [&str; 7] = [
    "clean",
    "fake_pass",
    "discovery_bug",
    "access_bug",
    "spec_ambiguity",
    "harness_bug",
    "gave_up",
];

/// The brief's hash.
pub fn analysis_hash() -> String {
    hex::encode(Sha256::digest(ANALYSIS_MD.as_bytes()))
}

/// One reader's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderVerdict {
    pub verdict: String,
    /// `low`, `medium`, or `high`.
    pub confidence: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// A transcript file and line, or `final.diff`.
    pub at: String,
    pub quote: String,
}

/// Parse a reader's answer: exactly one envelope holding a valid verdict.
pub fn parse(text: &str) -> Result<ReaderVerdict, String> {
    let opens = text.matches(OPEN).count();
    let closes = text.matches(CLOSE).count();
    if opens == 0 {
        return Err("no review envelope".into());
    }
    if opens > 1 || closes > 1 {
        return Err(format!("{opens} review envelopes"));
    }
    let start = text.find(OPEN).expect("counted") + OPEN.len();
    let end = text.find(CLOSE).ok_or("an unclosed review envelope")?;
    if end < start {
        return Err("a review envelope closed before it opened".into());
    }
    let v: ReaderVerdict = serde_json::from_str(text[start..end].trim())
        .map_err(|e| format!("a malformed review envelope: {e}"))?;
    if !VERDICTS.contains(&v.verdict.as_str()) {
        return Err(format!(
            "verdict {:?} is not one of {}",
            v.verdict,
            VERDICTS.join(", ")
        ));
    }
    if !["low", "medium", "high"].contains(&v.confidence.as_str()) {
        return Err(format!(
            "confidence {:?}: low, medium, or high",
            v.confidence
        ));
    }
    Ok(v)
}

/// Which trials a review reads: every failure, and passes per the
/// coverage rule. Only finished, valid-so-far trials are read.
pub fn pick(trials: &[TrialRow], seed: u64) -> Vec<String> {
    let graded: Vec<&TrialRow> = trials
        .iter()
        .filter(|t| t.status() == crate::store::TrialStatus::Done && t.oracle.is_some())
        .collect();
    let mut out: Vec<String> = graded
        .iter()
        .filter(|t| t.oracle.as_deref() != Some("pass"))
        .map(|t| t.trial_id.clone())
        .collect();
    let passes: Vec<&&TrialRow> = graded
        .iter()
        .filter(|t| t.oracle.as_deref() == Some("pass"))
        .collect();
    if passes.len() <= ALL_PASSES_UP_TO {
        out.extend(passes.iter().map(|t| t.trial_id.clone()));
    } else {
        let mut cells: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for t in passes {
            cells
                .entry((t.variant_hash.clone(), t.task_id.clone()))
                .or_default()
                .push(t.trial_id.clone());
        }
        for ids in cells.values_mut() {
            ids.sort_by_key(|id| hex::encode(Sha256::digest(format!("{seed}:{id}").as_bytes())));
            let take = ((ids.len() as f64 * PASS_SAMPLE).ceil() as usize).max(1);
            out.extend(ids.iter().take(take).cloned());
        }
    }
    out.sort();
    out
}

/// What a reader is told about one trial.
pub fn prompt(trial: &TrialRow, dir: &Path, goal: &str) -> String {
    format!(
        "{ANALYSIS_MD}\n\n# This trial\n\n\
         - task goal: {goal}\n\
         - oracle verdict: {oracle} (the held-out oracle on the final commit; authoritative)\n\
         - final commit: {where_} ({final_commit})\n\
         - flywheel verdict: {fly}\n\
         - turns: {turns}, cost: ${cost:.2}\n\
         - deterministic trace checks: {trace}\n\
         - files (read them; paths are relative to your working directory {dir}): \
         `transcripts/*.jsonl` (every event of the run), `final.diff` (the change), \
         `worker.log`, `oracle.log`\n",
        oracle = trial.oracle.as_deref().unwrap_or("none"),
        where_ = trial.final_where.as_deref().unwrap_or("-"),
        final_commit = trial.final_commit.as_deref().unwrap_or("-"),
        fly = trial.flywheel_status.as_deref().unwrap_or("-"),
        turns = trial.turns,
        cost = trial.cost_usd,
        trace = trial.trace_json,
        dir = dir.display(),
    )
}

/// Runs one reader (production: a cold agent session).
pub trait Reader: Send + Sync {
    /// The reader's final answer text for `prompt`, run in `dir`.
    fn read(&self, prompt: &str, dir: &Path) -> Result<String, String>;
    /// The model the reader runs.
    fn model(&self) -> String;
}

/// A cold Claude Code session that may only read files: no user layers,
/// no skills, `Read`/`Grep`/`Glob` only, a turn cap.
pub struct AgentReader {
    session: flywheel_core::agents_invoke::ColdSession,
    model: String,
}

impl AgentReader {
    pub fn new(model: &str, max_turns: u32, command_override: Option<Vec<String>>) -> Self {
        let mut configuration = flywheel_agents::AgentConfiguration::new("claude-code");
        configuration.model_id = Some(model.to_owned());
        configuration.max_turns = Some(max_turns);
        configuration.permission_policy = flywheel_agents::PermissionPolicy::Auto;
        configuration.command_override = command_override;
        let opts = &mut configuration.adapter_options;
        opts.insert("skills".into(), serde_json::Value::Null);
        opts.insert("tools".into(), serde_json::json!(["Read", "Grep", "Glob"]));
        opts.insert("setting_sources".into(), serde_json::json!(["project"]));
        Self {
            session: flywheel_core::agents_invoke::ColdSession::with_configuration(configuration)
                .with_timeout_seconds(Some(900.0)),
            model: model.to_owned(),
        }
    }
}

impl Reader for AgentReader {
    fn read(&self, prompt: &str, dir: &Path) -> Result<String, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime
            .block_on(self.session.run(prompt, dir, None))
            .map(|out| out.text)
            .map_err(|e| e.to_string())
    }

    fn model(&self) -> String {
        self.model.clone()
    }
}

/// Read one trial: up to two tries (a malformed envelope is the reader's
/// infrastructure failing, not a verdict).
pub fn review_one(
    reader: &dyn Reader,
    trial: &TrialRow,
    dir: &Path,
    goal: &str,
) -> Result<ReaderVerdict, String> {
    let text = prompt(trial, dir, goal);
    let mut last = String::new();
    for _ in 0..2 {
        match reader.read(&text, dir).and_then(|answer| parse(&answer)) {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// What a verdict does to a trial's validity: a high-confidence fake on a
/// pass makes it suspect; nothing else changes it.
pub fn suspect(trial: &TrialRow, v: &ReaderVerdict) -> bool {
    trial.oracle.as_deref() == Some("pass") && v.verdict == "fake_pass" && v.confidence == "high"
}

/// The coordinator's brief: group the readers' findings by root cause.
pub fn coordinator_prompt(findings: &[(String, ReaderVerdict)]) -> String {
    let mut body = String::from(
        "You coordinate transcript reviews of an eval run. Below is every reader's verdict, \
         one per trial. Group the findings by root cause (one heading per cause, the trial \
         ids under it, the strongest evidence quoted), most trials first; then list the \
         passes a reader doubted. Plain markdown, no preamble. Readers can be wrong: say \
         where the evidence is thin.\n\n",
    );
    for (trial, v) in findings {
        body.push_str(&format!(
            "- {trial}: {} ({}) {} | {}\n",
            v.verdict,
            v.confidence,
            v.notes.replace('\n', " "),
            v.evidence
                .iter()
                .map(|e| format!("{}: {}", e.at, e.quote.replace('\n', " ")))
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    body
}

/// Where a run's coordinator summary goes.
pub fn summary_path(run_dir: &Path) -> PathBuf {
    run_dir.join("review.md")
}

#[cfg(test)]
mod tests;
