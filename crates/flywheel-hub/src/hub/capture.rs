//! Runs the hub did not start: `claude -p`, Agent SDK runs, a Claude Code
//! session in a plain terminal. `fw hub capture install` puts http hooks
//! in the user's Claude Code settings ([`crate::agent::capture_settings`]);
//! every Claude Code process on the machine then POSTs its hooks to
//! `/api/telemetry/capture` with the capture token, and each run becomes
//! a session on the fleet board (`cc-<conversation>`), marked captured.
//!
//! A run started inside a hub session (an agent's or a terminal's)
//! inherits that session's id from its environment: it is counted under
//! that session. The capture token is what authenticates the run; the
//! session it names only places it.
//! A launcher may name a batch (`FLYWHEEL_BATCH`) and what the batch may
//! spend (`FLYWHEEL_BATCH_BUDGET`, USD). The hub's own Claude Code
//! sessions report through their own hooks; what they also send here is
//! dropped. A run's tokens and cost are priced from its transcript when a
//! turn ends.

use super::Hub;
use crate::auth;
use crate::telemetry::{Draft, SessionLinks, What, claude, now_ms, summary};
use crate::usage::{self, SessionUsage};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

/// The capture token's file in the state dir.
pub const CAPTURE_TOKEN_FILE: &str = "capture-token";
/// Its length in bytes (hex: twice that).
const TOKEN_BYTES: usize = 32;
/// The prefix of a captured run's session id on the board.
pub const CAPTURED_PREFIX: &str = "cc-";
/// The longest batch label kept.
const BATCH_MAX_CHARS: usize = 80;
/// The most a transcript read for pricing may hold (bytes, each file).
const TRANSCRIPT_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The most subagent transcripts priced beside a run's own.
const SUBAGENTS_MAX: usize = 256;

/// What a capture hook's request says beside its payload.
#[derive(Debug, Default, Clone)]
pub struct CaptureFrom {
    /// The hub session it ran under (`FLYWHEEL_HUB_SESSION`).
    pub parent: Option<String>,
    pub batch: Option<String>,
    pub budget: Option<String>,
}

impl Hub {
    /// The capture token, created (mode 0600) on first use.
    pub fn capture_token(&self) -> Result<String, String> {
        let mut held = self
            .capture
            .token
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = held.as_ref() {
            return Ok(token.clone());
        }
        let path = self.options.state_dir.join(CAPTURE_TOKEN_FILE);
        let token = match std::fs::read_to_string(&path) {
            Ok(text)
                if text.trim().len() == TOKEN_BYTES * 2
                    && text.trim().bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                text.trim().to_ascii_lowercase()
            }
            Ok(_) | Err(_) => {
                let token = auth::random_hex(TOKEN_BYTES).map_err(|e| e.to_string())?;
                auth::write_private(&path, format!("{token}\n").as_bytes())
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                token
            }
        };
        *held = Some(token.clone());
        Ok(token)
    }

    /// The hooks `fw hub capture install` puts in the user's settings.
    pub fn capture_hooks(&self) -> Result<serde_json::Value, String> {
        let url = self
            .agent_url()
            .ok_or("the hub is not listening yet: try again in a moment")?;
        Ok(crate::agent::capture_settings(&url, &self.capture_token()?))
    }

    /// One capture hook's POST: `token` is what it presented.
    pub fn capture_report(
        &self,
        token: &str,
        from: CaptureFrom,
        payload: serde_json::Value,
    ) -> Result<(), CaptureRefused> {
        let expected = self.capture_token().map_err(CaptureRefused::Hub)?;
        if !auth::Token::from_secret(expected).matches(token) {
            return Err(CaptureRefused::Token);
        }
        let str_of = |key: &str| {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let event = str_of("hook_event_name")
            .ok_or(CaptureRefused::Payload("names no hook_event_name"))?
            .to_owned();
        let conversation = str_of("session_id")
            .filter(|id| {
                id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .ok_or(CaptureRefused::Payload(
                "names no session_id (letters, digits, and dashes)",
            ))?
            .to_owned();
        // The hub session it ran under, when this hub has it.
        let parent = from
            .parent
            .filter(|session| !session.starts_with('$') && self.store().session(session).is_ok());
        if let Some(parent) = &parent
            && self.is_own_conversation(parent, &conversation)
        {
            // The hub session itself: its own hooks report it.
            return Ok(());
        }
        let id = format!("{CAPTURED_PREFIX}{conversation}");
        let now = now_ms();
        let mut drafts = claude::hook(&event, &payload, now);
        if self
            .telemetry
            .snapshot(&id)
            .is_none_or(|s| s.harness.is_none())
        {
            let cwd = str_of("cwd");
            let links = SessionLinks {
                opened_by: parent,
                captured: true,
                batch: from.batch.as_deref().and_then(batch_label),
                budget_usd: from.budget.as_deref().and_then(budget_of),
                ..SessionLinks::default()
            };
            drafts.insert(
                0,
                Draft::new(
                    now,
                    What::SessionStarted {
                        harness: "claude".to_owned(),
                        conversation: Some(conversation.clone()),
                        model: None,
                        label: Some(summary::text(
                            &cwd.map(Path::new)
                                .and_then(Path::file_name)
                                .and_then(|n| n.to_str())
                                .map_or_else(|| "claude".to_owned(), str::to_owned),
                        )),
                        dir: cwd.map(summary::text),
                        links: Some(links),
                    },
                ),
            );
        }
        if matches!(event.as_str(), "Stop" | "StopFailure" | "SessionEnd")
            && let Some(path) = str_of("transcript_path").and_then(transcript_of)
        {
            // Ahead of the hook's own: a session's end stays its last word.
            let at = usize::from(matches!(
                drafts.first().map(|d| &d.what),
                Some(What::SessionStarted { links: Some(_), .. })
            ));
            let totals = priced(&path, &conversation, now);
            drafts.splice(at..at, totals);
        }
        self.telemetry.record(&id, drafts);
        Ok(())
    }

    /// Whether `conversation` is hub session `session`'s own Claude Code
    /// conversation (the one it started with, or one it moved to since).
    fn is_own_conversation(&self, session: &str, conversation: &str) -> bool {
        let started = self
            .store()
            .session(session)
            .ok()
            .and_then(|r| r.agent_session_id.clone());
        started.as_deref() == Some(conversation)
            || self
                .telemetry
                .snapshot(session)
                .and_then(|s| s.conversation)
                .as_deref()
                == Some(conversation)
    }
}

/// Why a capture POST was refused.
#[derive(Debug)]
pub enum CaptureRefused {
    Token,
    Payload(&'static str),
    Hub(String),
}

/// A launcher's batch label, one line and bounded.
fn batch_label(raw: &str) -> Option<String> {
    let line: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(BATCH_MAX_CHARS)
        .collect();
    (!line.is_empty() && !line.starts_with('$')).then(|| summary::text(&line))
}

/// A budget in USD: a positive finite number (`$` allowed).
fn budget_of(raw: &str) -> Option<f64> {
    raw.trim()
        .trim_start_matches('$')
        .parse::<f64>()
        .ok()
        .filter(|b| b.is_finite() && *b > 0.0)
}

/// The transcript a hook names, when it is a Claude Code transcript (a
/// `.jsonl` under a `projects` dir) that exists.
fn transcript_of(raw: &str) -> Option<PathBuf> {
    let path = PathBuf::from(raw);
    let fits = path.is_absolute()
        && path.extension().is_some_and(|e| e == "jsonl")
        && path.components().any(|c| c.as_os_str() == "projects")
        && path.is_file();
    fits.then_some(path)
}

/// A run's totals and last context window from its transcript and its
/// subagents' (`<conversation>/subagents/*.jsonl` beside it).
fn priced(main: &Path, conversation: &str, now: i64) -> Vec<Draft> {
    let mut usage = SessionUsage::default();
    read_usage(main, &mut usage);
    let main_last = usage.view(chrono::Local::now().date_naive()).last_turn;
    let subagents = main.with_file_name(conversation).join("subagents");
    if let Ok(entries) = std::fs::read_dir(&subagents) {
        for entry in entries.flatten().take(SUBAGENTS_MAX) {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                read_usage(&path, &mut usage);
            }
        }
    }
    if usage.is_empty() {
        return Vec::new();
    }
    let view = usage.view(chrono::Local::now().date_naive());
    let tokens = &view.total.tokens;
    let mut drafts = vec![Draft::new(
        now,
        What::Usage {
            input: tokens.input,
            output: tokens.output,
            cache_read: tokens.cache_read,
            cache_write: tokens.cache_write_5m + tokens.cache_write_1h,
            cost_usd: view.total.priced.then_some(view.total.cost_usd),
        },
    )];
    if let Some(last) = main_last {
        let t = &last.tokens;
        let used = t.input + t.cache_read + t.cache_write_5m + t.cache_write_1h;
        drafts.push(Draft::new(
            now,
            What::SessionStarted {
                harness: "claude".to_owned(),
                conversation: None,
                model: Some(last.model.clone()),
                label: None,
                dir: None,
                links: None,
            },
        ));
        drafts.push(Draft::new(
            now,
            What::ContextUsage {
                used,
                window: super::telemetry::assumed_window(used),
                exact: false,
            },
        ));
    }
    drafts
}

/// Count the usage on a transcript's lines.
fn read_usage(path: &Path, usage: &mut SessionUsage) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    if file
        .metadata()
        .is_ok_and(|m| m.len() > TRANSCRIPT_MAX_BYTES)
    {
        return;
    }
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"usage\"") {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(turn) = usage::claude_turn(&value)
        {
            usage.observe(&turn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_labels_and_budgets_are_read_leniently() {
        assert_eq!(
            batch_label("  post-trade \n analyst "),
            Some("post-trade analyst".into())
        );
        // An unset variable Claude Code left as written.
        assert_eq!(batch_label("$FLYWHEEL_BATCH"), None);
        assert_eq!(batch_label("   "), None);
        assert_eq!(
            batch_label(&"x".repeat(500)).unwrap().chars().count(),
            BATCH_MAX_CHARS
        );
        assert_eq!(budget_of("$20"), Some(20.0));
        assert_eq!(budget_of("2.5"), Some(2.5));
        assert_eq!(budget_of("-1"), None);
        assert_eq!(budget_of("NaN"), None);
        assert_eq!(budget_of("$FLYWHEEL_BATCH_BUDGET"), None);
    }

    #[test]
    fn only_a_claude_transcript_is_priced() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects").join("p");
        std::fs::create_dir_all(&projects).unwrap();
        let ok = projects.join("a.jsonl");
        std::fs::write(&ok, "").unwrap();
        assert_eq!(transcript_of(ok.to_str().unwrap()), Some(ok.clone()));
        let other = dir.path().join("a.jsonl");
        std::fs::write(&other, "").unwrap();
        assert_eq!(transcript_of(other.to_str().unwrap()), None);
        assert_eq!(transcript_of("relative/projects/a.jsonl"), None);
        assert_eq!(
            transcript_of(projects.join("missing.jsonl").to_str().unwrap()),
            None
        );
    }
}
