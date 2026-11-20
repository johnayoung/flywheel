//! Hub sessions as set tasks, where that is sound.
//!
//! A recorded agent session becomes a candidate only when **all** hold:
//!
//! | rule | why |
//! | --- | --- |
//! | its change branch merged into the default branch | a reference exists |
//! | exactly one operator prompt carries requirements; later ones are approvals or "continue" | otherwise the task is the conversation, and replaying the first prompt measures something else |
//! | the merged change touches code, not only docs or styles | an oracle needs a test surface |
//!
//! The candidate's task is the first prompt as its goal and the files the
//! session read before its first edit as `context.relevant`. Its oracle
//! is not derived from the session: it must be blind-authored (`/fw-verify`
//! against the prompt and the reference) into the draft, then validated
//! like every other task.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// What the hub and git say about one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    pub session_id: String,
    /// Its change branch (`fw.change`).
    pub branch: String,
    /// The branch's merge-base with the default branch when the session
    /// started.
    pub base: String,
    /// The branch tip that merged (empty when it did not).
    pub merged_tip: String,
    /// The operator's prompts, in order.
    pub prompts: Vec<String>,
    /// Files the session read before its first edit, in order.
    pub read_before_edit: Vec<String>,
    /// Files the merged change touches.
    pub changed_files: Vec<String>,
}

/// Why a session is or is not a sound task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Soundness {
    pub sound: bool,
    pub reasons: Vec<String>,
}

/// Whether a later prompt only approves or continues (carries no new
/// requirement).
pub fn is_approval(prompt: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*(?:ok(?:ay)?|yes|yep|go(?: ahead| on)?|continue|proceed|approved?|lgtm|looks good|ship it|do it|sounds good|thanks?|thank you|keep going|carry on|merge it|push it|y)\s*[.!]*\s*$",
        )
        .expect("static regex")
    });
    re.is_match(prompt)
}

fn is_code(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let doc_like = [".md", ".txt", ".css", ".svg", ".png", ".rst", ".adoc"];
    !doc_like.iter().any(|ext| lower.ends_with(ext))
}

/// Judge one session against the rules.
pub fn judge(f: &SessionFacts) -> Soundness {
    let mut reasons = Vec::new();
    if f.merged_tip.is_empty() {
        reasons.push(format!(
            "its branch {} never merged: no reference",
            f.branch
        ));
    }
    match f.prompts.split_first() {
        None => reasons.push("no operator prompt".into()),
        Some((_, rest)) => {
            let steering: Vec<&String> = rest.iter().filter(|p| !is_approval(p)).collect();
            if !steering.is_empty() {
                reasons.push(format!(
                    "{} later operator prompt(s) add requirements (first: {:?}): the task is the conversation",
                    steering.len(),
                    steering[0].chars().take(80).collect::<String>()
                ));
            }
        }
    }
    if !f.changed_files.is_empty() && !f.changed_files.iter().any(|p| is_code(p)) {
        reasons.push("the change touches only docs or styles: no test surface".into());
    }
    Soundness {
        sound: reasons.is_empty(),
        reasons,
    }
}

/// The task a sound session becomes (`sess-<id8>`).
pub fn task_json(f: &SessionFacts) -> serde_json::Value {
    let id = format!("sess-{}", &f.session_id[..f.session_id.len().min(8)]);
    serde_json::json!({
        "id": id,
        "goal": f.prompts.first().cloned().unwrap_or_default(),
        "context": {
            "relevant": f.read_before_edit,
            "notes": format!("From hub session {} (branch {}).", f.session_id, f.branch),
        },
    })
}

/// Git's side of a session's facts: its branch tip, whether that merged
/// into `default_branch`, the base (the parent of the oldest commit on the
/// tip's first-parent chain made at or after `started`), and the files the
/// change touches.
pub fn git_facts(
    repo: &std::path::Path,
    branch: &str,
    default_branch: &str,
    started: chrono::DateTime<chrono::Utc>,
) -> Result<(String, String, Vec<String>), String> {
    let tip = crate::git::rev_parse(repo, branch)
        .map_err(|_| format!("branch {branch} is gone from {}", repo.display()))?;
    let merged = crate::git::is_ancestor(repo, &tip, default_branch);
    let log = crate::git::run(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H|%P|%ct",
            "-n",
            "500",
            &tip,
        ],
    )
    .map_err(|e| e.to_string())?;
    let mut base = None;
    for line in log.lines() {
        let mut parts = line.split('|');
        let (_, parents, time) = (parts.next(), parts.next().unwrap_or(""), parts.next());
        let Some(time) = time.and_then(|t| t.trim().parse::<i64>().ok()) else {
            break;
        };
        // A root commit has no parent: nothing before the session to start from.
        let Some(parent) = parents.split_whitespace().next() else {
            break;
        };
        if time < started.timestamp() {
            break;
        }
        base = Some(parent.to_owned());
    }
    let base =
        base.ok_or_else(|| format!("branch {branch} has no commit made during the session"))?;
    let changed = crate::git::run(repo, &["diff", "--name-only", &base, &tip])
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::to_owned)
        .collect();
    Ok((base, if merged { tip } else { String::new() }, changed))
}

/// Add a sound session's task to the draft at `set` (no oracle yet: it
/// must be blind-authored before validation passes).
pub fn add_to_draft(
    set: &crate::set::SetDir,
    manifest: &mut crate::set::SetManifest,
    facts: &SessionFacts,
) -> Result<crate::set::SetTask, crate::set::SetError> {
    let verdict = judge(facts);
    if !verdict.sound {
        return Err(crate::set::SetError::Invalid(format!(
            "session {} is not a sound task: {}",
            facts.session_id,
            verdict.reasons.join("; ")
        )));
    }
    let json = task_json(facts);
    let id = json["id"].as_str().expect("set").to_owned();
    crate::set::write_json(&set.task_file(&id), &json)?;
    let task = crate::set::SetTask {
        id: id.clone(),
        source: crate::set::Source::Session,
        tier: crate::set::Tier::Uncalibrated,
        split: crate::set::Split::Dev,
        kind: crate::set::TaskKind::Build,
        base: facts.base.clone(),
        reference: facts.merged_tip.clone(),
        files_digest: String::new(),
        parallel_safe: None,
        prior_cost_usd: None,
        oracle_secs: None,
        prerequisites: Default::default(),
        provenance: crate::set::Provenance {
            session_id: Some(facts.session_id.clone()),
            ..Default::default()
        },
    };
    manifest.tasks.retain(|t| t.id != id);
    manifest.tasks.push(task.clone());
    manifest.tasks.sort_by(|a, b| a.id.cmp(&b.id));
    set.save(manifest)?;
    Ok(task)
}

/// Claude Code's transcript lines of one session -> its operator prompts,
/// and the files it read before its first edit.
pub fn from_transcript(text: &str) -> (Vec<String>, Vec<String>) {
    let mut prompts = Vec::new();
    let mut reads = Vec::new();
    let mut edited = false;
    for line in text.lines() {
        let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = &e["message"]["content"];
        match e["type"].as_str() {
            Some("user") => {
                let text = match content {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Array(items)
                        if items.iter().all(|i| i["type"] == "text") =>
                    {
                        Some(
                            items
                                .iter()
                                .filter_map(|i| i["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                    _ => None,
                };
                if let Some(t) = text.filter(|t| {
                    !t.trim().is_empty()
                        && !t.starts_with("<command-")
                        && !t.starts_with("<local-command")
                }) {
                    prompts.push(t);
                }
            }
            Some("assistant") => {
                for item in content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| i["type"] == "tool_use")
                {
                    let name = item["name"].as_str().unwrap_or("");
                    if ["Edit", "Write", "MultiEdit", "NotebookEdit"].contains(&name) {
                        edited = true;
                    } else if !edited
                        && matches!(name, "Read")
                        && let Some(path) = item["input"]["file_path"].as_str()
                        && !reads.iter().any(|r| r == path)
                    {
                        reads.push(path.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    (prompts, reads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> SessionFacts {
        SessionFacts {
            session_id: "a1b2c3d4e5".into(),
            branch: "feat/x".into(),
            base: "b".repeat(40),
            merged_tip: "c".repeat(40),
            prompts: vec![
                "Add a --dry-run flag to fw reland.".into(),
                "ok".into(),
                "continue".into(),
            ],
            read_before_edit: vec!["crates/flywheel/src/cli.rs".into()],
            changed_files: vec!["crates/flywheel/src/cli.rs".into(), "docs/cli.md".into()],
        }
    }

    #[test]
    fn git_facts_find_the_sessions_base_and_whether_it_merged() {
        let repo = crate::testutil::TempRepo::new();
        let before = repo.commit("before", &[("a.rs", "1")]);
        let started = chrono::Utc::now() - chrono::Duration::seconds(1);
        repo.git(&["checkout", "-q", "-b", "feat/x"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        repo.commit("work 1", &[("a.rs", "2")]);
        let tip = repo.commit("work 2", &[("b.rs", "1")]);
        let (base, merged, changed) = git_facts(repo.path(), "feat/x", "main", started).unwrap();
        assert_eq!(base, before);
        assert!(merged.is_empty(), "not merged yet");
        assert_eq!(changed, ["a.rs", "b.rs"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "-q", "--ff-only", "feat/x"]);
        let (_, merged, _) = git_facts(repo.path(), "feat/x", "main", started).unwrap();
        assert_eq!(merged, tip);
        assert!(git_facts(repo.path(), "gone", "main", started).is_err());
    }

    #[test]
    fn a_sound_session_joins_the_draft_without_an_oracle_and_fails_validation_until_one_is_authored()
     {
        let evals = tempfile::tempdir().unwrap();
        let set = crate::set::SetDir::draft(evals.path(), "core");
        let mut manifest = crate::set::SetManifest {
            name: "core".into(),
            version: None,
            project_id: "p".into(),
            repo: "/r".into(),
            created_at: chrono::Utc::now(),
            tasks: Vec::new(),
            hash: None,
        };
        let task = add_to_draft(&set, &mut manifest, &facts()).unwrap();
        assert_eq!(task.source, crate::set::Source::Session);
        assert!(set.task_file(&task.id).is_file());
        assert!(!set.registration(&task.id).exists());
        let mut steered = facts();
        steered.prompts.push("and also X".into());
        assert!(add_to_draft(&set, &mut manifest, &steered).is_err());
    }

    #[test]
    fn a_transcript_gives_prompts_and_reads_before_the_first_edit() {
        let lines = [
            r#"{"type":"user","message":{"content":"Add a flag."}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/w/a.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"..."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/w/a.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/w/b.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"ok"}]}}"#,
        ];
        let (prompts, reads) = from_transcript(&lines.join("\n"));
        assert_eq!(prompts, ["Add a flag.", "ok"]);
        assert_eq!(reads, ["/w/a.rs"]);
    }

    #[test]
    fn a_single_intent_merged_code_change_is_sound() {
        let s = judge(&facts());
        assert!(s.sound, "{s:?}");
        let task = task_json(&facts());
        assert_eq!(task["id"], "sess-a1b2c3d4");
        assert_eq!(task["goal"], "Add a --dry-run flag to fw reland.");
        assert_eq!(task["context"]["relevant"][0], "crates/flywheel/src/cli.rs");
    }

    #[test]
    fn steered_unmerged_and_docs_only_sessions_are_refused_with_reasons() {
        let mut steered = facts();
        steered.prompts.push("also make it print JSON".into());
        let s = judge(&steered);
        assert!(
            !s.sound && s.reasons[0].contains("add requirements"),
            "{s:?}"
        );
        let mut unmerged = facts();
        unmerged.merged_tip.clear();
        assert!(judge(&unmerged).reasons[0].contains("never merged"));
        let mut docs = facts();
        docs.changed_files = vec!["docs/a.md".into(), "web/app.css".into()];
        assert!(judge(&docs).reasons[0].contains("only docs"));
        assert!(is_approval("Looks good!"));
        assert!(!is_approval("ok but rename it"));
    }
}
