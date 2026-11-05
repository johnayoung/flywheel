//! Past agent conversations of a folder, to resume in a new session: Claude
//! Code's transcripts (`<config>/projects/<dir>/<id>.jsonl`, `<dir>` named
//! after the folder) and Codex's rollouts (`<home>/sessions/YYYY/MM/DD/
//! rollout-*.jsonl` whose `session_meta` names the folder). Newest first,
//! each with what it was about: its title (Claude's `/rename` or its own
//! summary), else its first prompt.

use crate::agent::{self, SessionKind};
use crate::agentlog::{self, Event, Format, Role};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// Conversations listed by default, and at most.
pub const DEFAULT_LIMIT: usize = 30;
pub const MAX_LIMIT: usize = 200;
/// Bytes of a conversation read for its title and first prompt.
const HEAD_BYTES: u64 = 1024 * 1024;
/// Rollout files opened when looking for a folder's Codex conversations.
const MAX_ROLLOUTS_SCANNED: usize = 4000;
/// Characters of a title or prompt kept.
const MAX_TITLE_CHARS: usize = 160;

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conversation {
    pub id: String,
    pub kind: SessionKind,
    /// When its file last changed.
    pub modified: DateTime<Utc>,
    /// Its title: a `/rename`, else the agent's own summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its first prompt, on one line, cut.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub bytes: u64,
}

/// `folder`'s Claude Code conversations, newest first (at most `limit`).
/// Conversations with no prompt yet (nothing to resume) are left out.
pub fn claude(config_dir: &Path, folder: &Path, limit: usize) -> Vec<Conversation> {
    let dir = config_dir
        .join("projects")
        .join(agent::claude_project_dir_name(folder));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(DateTime<Utc>, u64, String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path
                .file_name()?
                .to_str()?
                .strip_suffix(".jsonl")?
                .to_owned();
            if !agent::is_conversation_id(&id) {
                return None;
            }
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((meta.modified().ok()?.into(), meta.len(), id, path))
        })
        .collect();
    files.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .filter_map(|(modified, bytes, id, path)| {
            let (title, prompt) = claude_head(&path);
            (title.is_some() || prompt.is_some()).then_some(Conversation {
                id,
                kind: SessionKind::Claude,
                modified,
                title,
                prompt,
                bytes,
            })
        })
        .take(limit)
        .collect()
}

/// `folder`'s Codex conversations, newest first (at most `limit`): the
/// rollouts whose `session_meta` names it, searched newest day first.
pub fn codex(codex_home: &Path, folder: &Path, limit: usize) -> Vec<Conversation> {
    let mut found = Vec::new();
    let mut scanned = 0;
    for day in newest_days(&codex_home.join("sessions")) {
        let Ok(entries) = std::fs::read_dir(&day) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            })
            .collect();
        files.sort_unstable_by(|a, b| b.cmp(a));
        for path in files {
            if found.len() >= limit || scanned >= MAX_ROLLOUTS_SCANNED {
                break;
            }
            scanned += 1;
            if let Some(conversation) = codex_conversation(&path, folder) {
                found.push(conversation);
            }
        }
        if found.len() >= limit || scanned >= MAX_ROLLOUTS_SCANNED {
            break;
        }
    }
    // A day's names sort by start; a later resumed one changed since.
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    found
}

/// The newest conversation of `kind` in `folder`.
pub fn latest(
    kind: SessionKind,
    claude_config_dir: Option<&Path>,
    codex_home: Option<&Path>,
    folder: &Path,
) -> Option<Conversation> {
    match kind {
        SessionKind::Claude => claude(claude_config_dir?, folder, 1).into_iter().next(),
        SessionKind::Codex => codex(codex_home?, folder, 1).into_iter().next(),
        SessionKind::Shell => None,
    }
}

/// `sessions/YYYY/MM/DD` directories, newest first, at most
/// [`agent::MAX_ROLLOUT_DAYS`].
fn newest_days(sessions: &Path) -> Vec<PathBuf> {
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_unstable_by(|a, b| b.cmp(a));
        dirs
    };
    subdirs(sessions)
        .into_iter()
        .flat_map(|year| subdirs(&year))
        .flat_map(|month| subdirs(&month))
        .take(agent::MAX_ROLLOUT_DAYS)
        .collect()
}

/// The JSON lines of `path`'s first [`HEAD_BYTES`] (a longer first line is
/// read whole: Codex's `session_meta` carries its instructions).
fn head_lines(path: &Path) -> Vec<Value> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file.take(agentlog::MAX_LINE_BYTES as u64));
    let mut lines = Vec::new();
    let mut read = 0u64;
    let mut buf = Vec::new();
    while read < HEAD_BYTES {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n as u64,
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&buf) {
            lines.push(value);
        }
    }
    lines
}

/// A title or prompt on one line, cut to [`MAX_TITLE_CHARS`].
fn one_line(text: &str) -> Option<String> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    if flat.chars().count() <= MAX_TITLE_CHARS {
        return Some(flat);
    }
    let mut cut: String = flat.chars().take(MAX_TITLE_CHARS - 1).collect();
    cut.push('…');
    Some(cut)
}

/// Words the user typed, not context a CLI injected (`<command-name>`,
/// `<system-reminder>`, Codex's `<environment_context>`, AGENTS.md).
fn typed_prompt(text: &str) -> bool {
    let text = text.trim_start();
    !text.is_empty() && !text.starts_with('<') && !text.starts_with("# AGENTS.md instructions")
}

/// The first user prompt among `lines`, parsed as `format`.
fn first_prompt(format: Format, lines: &[Value]) -> Option<String> {
    lines.iter().enumerate().find_map(|(at, value)| {
        if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        agentlog::parse(format, value, at as u64)
            .events
            .into_iter()
            .find_map(|event| match event {
                Event::Text {
                    role: Role::User,
                    parts,
                    ..
                } => {
                    let text: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
                    let text = text.join("\n");
                    typed_prompt(&text).then(|| one_line(&text)).flatten()
                }
                _ => None,
            })
    })
}

/// A Claude transcript's title (the last `/rename`, else its last
/// summary) and first prompt.
fn claude_head(path: &Path) -> (Option<String>, Option<String>) {
    let lines = head_lines(path);
    let mut custom = None;
    let mut summary = None;
    for value in &lines {
        let text = |key: &str| value.get(key).and_then(Value::as_str).and_then(one_line);
        match value.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = text("customTitle").or(custom),
            Some("ai-title") => summary = text("aiTitle").or(summary),
            Some("summary") => summary = text("summary").or(summary),
            _ => {}
        }
    }
    (custom.or(summary), first_prompt(Format::Claude, &lines))
}

/// A rollout, if its `session_meta` says it ran in `folder`.
fn codex_conversation(path: &Path, folder: &Path) -> Option<Conversation> {
    let meta = std::fs::metadata(path).ok()?;
    let lines = head_lines(path);
    let session = lines
        .first()
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("session_meta"))?
        .get("payload")?;
    let cwd = session.get("cwd").and_then(Value::as_str)?;
    if Path::new(cwd) != folder {
        return None;
    }
    let id = session.get("id").and_then(Value::as_str)?;
    if !agent::is_conversation_id(id) {
        return None;
    }
    let prompt = first_prompt(Format::Codex, &lines);
    prompt.as_ref()?;
    Some(Conversation {
        id: id.to_owned(),
        kind: SessionKind::Codex,
        modified: meta.modified().ok()?.into(),
        title: None,
        prompt,
        bytes: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write(path: &Path, lines: &[Value], age_secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, text).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn claude_user(id: &str, text: &str) -> Value {
        serde_json::json!({
            "type": "user", "uuid": id, "sessionId": "s",
            "message": { "role": "user", "content": text }
        })
    }

    #[test]
    fn claude_conversations_are_listed_newest_first_with_their_title_or_prompt() {
        let config = tempfile::tempdir().unwrap();
        let folder = Path::new("/home/me/code/app");
        let dir = config
            .path()
            .join("projects")
            .join(agent::claude_project_dir_name(folder));
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let empty = "33333333-3333-4333-8333-333333333333";
        write(
            &dir.join(format!("{a}.jsonl")),
            &[
                serde_json::json!({"type": "user", "uuid": "m", "isMeta": true,
                    "message": {"role": "user", "content": "Caveat: meta"}}),
                claude_user("c", "<command-name>/clear</command-name>"),
                claude_user("u", "Fix   the\nflaky test"),
            ],
            300,
        );
        write(
            &dir.join(format!("{b}.jsonl")),
            &[
                claude_user("u", "Add a CLI"),
                serde_json::json!({"type": "ai-title", "aiTitle": "CLI work"}),
                serde_json::json!({"type": "custom-title", "customTitle": "my cli"}),
            ],
            10,
        );
        write(
            &dir.join(format!("{empty}.jsonl")),
            &[serde_json::json!({"type": "file-history-snapshot"})],
            1,
        );
        let list = claude(config.path(), folder, 10);
        let ids: Vec<&str> = list.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [b, a]);
        assert_eq!(list[0].title.as_deref(), Some("my cli"));
        assert_eq!(list[0].prompt.as_deref(), Some("Add a CLI"));
        assert_eq!(list[1].title, None);
        assert_eq!(list[1].prompt.as_deref(), Some("Fix the flaky test"));
        assert_eq!(claude(config.path(), folder, 1).len(), 1);
        assert!(claude(config.path(), Path::new("/elsewhere"), 10).is_empty());
        assert_eq!(
            latest(SessionKind::Claude, Some(config.path()), None, folder).map(|c| c.id),
            Some(b.to_owned())
        );
    }

    #[test]
    fn codex_conversations_are_the_rollouts_of_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let folder = Path::new("/home/me/code/app");
        let rollout = |day: &str, id: &str, cwd: &str, age: u64| {
            let path = home
                .path()
                .join("sessions")
                .join(day)
                .join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
            write(
                &path,
                &[
                    serde_json::json!({"type": "session_meta",
                        "payload": {"id": id, "cwd": cwd, "base_instructions": {"text": "x"}}}),
                    serde_json::json!({"type": "response_item", "payload": {"type": "message",
                        "role": "user", "content": [{"type": "input_text",
                        "text": "# AGENTS.md instructions for /home/me/code/app"}]}}),
                    serde_json::json!({"type": "response_item", "payload": {"type": "message",
                        "role": "user", "content": [{"type": "input_text",
                        "text": "<environment_context>cwd</environment_context>"}]}}),
                    serde_json::json!({"type": "response_item", "payload": {"type": "message",
                        "role": "user", "content": [{"type": "input_text",
                        "text": format!("task {id}")}]}}),
                ],
                age,
            );
        };
        let old = "0190a000-0000-7000-8000-000000000001";
        let new = "0190a000-0000-7000-8000-000000000002";
        let other = "0190a000-0000-7000-8000-000000000003";
        rollout("2026/01/01", old, "/home/me/code/app", 500);
        rollout("2026/01/02", new, "/home/me/code/app", 5);
        rollout("2026/01/02", other, "/home/me/code/other", 1);
        let list = codex(home.path(), folder, 10);
        let ids: Vec<&str> = list.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [new, old]);
        assert_eq!(list[0].prompt.as_deref(), Some(&*format!("task {new}")));
        assert_eq!(
            latest(SessionKind::Codex, None, Some(home.path()), folder).map(|c| c.id),
            Some(new.to_owned())
        );
    }

    #[test]
    fn long_prompts_are_cut_on_one_line() {
        let long = "word ".repeat(100);
        let cut = one_line(&long).unwrap();
        assert_eq!(cut.chars().count(), MAX_TITLE_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(one_line(" \n "), None);
    }
}
