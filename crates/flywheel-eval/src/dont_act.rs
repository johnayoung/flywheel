//! "Don't act" tasks: a landed task replayed at its own reference commit.
//! The work is already done; the right move is to check that and change
//! nothing. Built from a validated task of a draft.
//!
//! The task passes when, all three:
//!
//! 1. **no change**: no commit on top of the trial's starting point (its
//!    base plus the variant overlay) -- the generated first oracle command;
//! 2. **still green**: the original oracle passes;
//! 3. **inspected**: the transcript the harness kept (the agent cannot
//!    write it) shows a read of a file the task names and a command that
//!    builds or tests ([`inspected`]); checked by the trial, recorded as
//!    the `inspected` check.
//!
//! Validation inverts V1/V2: the oracle passes at the base, and fails on a
//! commit made on top of it.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::mine::PROJECT_HOME_TOKEN;
use crate::set::{
    Provenance, SetDir, SetError, SetManifest, SetTask, Source, TaskKind, Tier, files_under, io,
};

/// The id suffix of a task's don't-act twin.
pub const SUFFIX: &str = "-dont-act";

/// The first oracle command: nothing committed past the overlay commit,
/// which sits directly on `base`.
pub fn no_change_command(base: &str) -> String {
    format!(
        "test \"$(git rev-list --count {base}..HEAD --invert-grep --grep='^eval: variant overlay')\" = 0 \
         || {{ echo 'the agent committed a change on a task that needed none'; exit 1; }}"
    )
}

/// Add `from`'s don't-act twin to the draft at `set`.
pub fn generate(set: &SetDir, manifest: &mut SetManifest, from: &str) -> Result<SetTask, SetError> {
    if manifest.version.is_some() {
        return Err(SetError::Invalid(format!(
            "{} is frozen; add to a draft",
            manifest.label()
        )));
    }
    let original = manifest
        .task(from)
        .cloned()
        .ok_or_else(|| SetError::Invalid(format!("no task {from} in {}", manifest.label())))?;
    if original.kind == TaskKind::DontAct || original.reference.is_empty() {
        return Err(SetError::Invalid(format!(
            "{from} has no known-good reference to replay at"
        )));
    }
    match set.proof(from)? {
        Some(p) if p.passed => {}
        _ => {
            return Err(SetError::Invalid(format!(
                "validate {from} first: its oracle must be proven before its twin is built"
            )));
        }
    }
    let id = format!("{from}{SUFFIX}");
    // The task, its specs and oracle files: the original's.
    let task_text =
        std::fs::read_to_string(set.task_file(from)).map_err(io(&set.task_file(from)))?;
    let mut task: serde_json::Value = serde_json::from_str(&task_text)
        .map_err(|e| SetError::Invalid(format!("task {from}: {e}")))?;
    task["id"] = serde_json::json!(id);
    crate::set::write_json(&set.task_file(&id), &task)?;
    for (src, dst) in [
        (set.spec_dir(from), set.spec_dir(&id)),
        (
            set.oracle_dir(from).join("home"),
            set.oracle_dir(&id).join("home"),
        ),
    ] {
        if dst.exists() {
            std::fs::remove_dir_all(&dst).map_err(io(&dst))?;
        }
        for rel in files_under(&src)? {
            let to = dst.join(&rel);
            std::fs::create_dir_all(to.parent().expect("has a parent")).map_err(io(&dst))?;
            std::fs::copy(src.join(&rel), &to).map_err(io(&to))?;
        }
    }
    let reg_text =
        std::fs::read_to_string(set.registration(from)).map_err(io(&set.registration(from)))?;
    let reg: serde_json::Value = serde_json::from_str(&reg_text)
        .map_err(|e| SetError::Invalid(format!("registration {from}: {e}")))?;
    let mut graders = match reg {
        serde_json::Value::Array(g) => g,
        other => other["graders"].as_array().cloned().unwrap_or_default(),
    };
    graders.insert(
        0,
        serde_json::json!({"type": "command", "name": "no-change", "run": no_change_command(&original.reference)}),
    );
    crate::set::write_json(
        &set.registration(&id),
        &serde_json::json!({"graders": graders}),
    )?;
    let twin = SetTask {
        id: id.clone(),
        source: Source::DontAct,
        tier: Tier::Uncalibrated,
        kind: TaskKind::DontAct,
        base: original.reference.clone(),
        reference: original.reference.clone(),
        files_digest: String::new(),
        parallel_safe: None,
        oracle_secs: None,
        prerequisites: Default::default(),
        provenance: Provenance {
            origin_task: Some(from.to_owned()),
            ..original.provenance.clone()
        },
        ..original
    };
    manifest.tasks.retain(|t| t.id != id);
    manifest.tasks.push(twin.clone());
    manifest.tasks.sort_by(|a, b| a.id.cmp(&b.id));
    set.save(manifest)?;
    Ok(twin)
}

/// The files a task names (`context.relevant`, each entry's first word).
pub fn relevant_paths(task_json: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(task_json).unwrap_or_default();
    value["context"]["relevant"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str())
                .filter_map(|s| s.split_whitespace().next())
                .filter(|p| !p.contains(PROJECT_HOME_TOKEN) && !p.starts_with("http"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the transcripts in `dir` show the agent inspected the work: a
/// read (Read/Grep/Glob or a shell read) of a file in `relevant` (any file
/// when the task names none), and a command that builds or tests.
pub fn inspected(dir: &Path, relevant: &[String]) -> Result<(bool, String), String> {
    static VERIFY: OnceLock<Regex> = OnceLock::new();
    let verify = VERIFY.get_or_init(|| {
        Regex::new(
            r"cargo (?:test|build|check|clippy|nextest)|scripts/check\.sh|go test|npm (?:test|run)|pnpm |yarn |pytest|make\b|node --test|node [^ ]*test",
        )
        .expect("static regex")
    });
    let mut read = false;
    let mut ran = false;
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    for f in files {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        for line in text.lines() {
            let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if e["kind"] != "tool.started" {
                continue;
            }
            let inner = &e["payload"]["payload"];
            let name = inner["tool_name"].as_str().unwrap_or("");
            let input = inner["tool_input"].to_string();
            let command = inner["tool_input"]["command"].as_str().unwrap_or("");
            let names_relevant =
                relevant.is_empty() || relevant.iter().any(|r| input.contains(r.as_str()));
            if (matches!(name, "Read" | "Grep" | "Glob")
                || (name == "Bash" && !verify.is_match(command)))
                && names_relevant
            {
                read = true;
            }
            if name == "Bash" && verify.is_match(command) {
                ran = true;
            }
        }
    }
    let detail = match (read, ran) {
        (true, true) => "read the named files and ran a build or test".to_owned(),
        (false, true) => "ran a build or test but read none of the named files".to_owned(),
        (true, false) => "read the named files but never built or tested".to_owned(),
        (false, false) => "neither read the named files nor built or tested".to_owned(),
    };
    Ok((read && ran, detail))
}

#[cfg(test)]
mod tests;
