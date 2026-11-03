//! Cross-agent handoff: send what one session shows -- selected terminal
//! text, a transcript message, a suggested command, its branch's diff --
//! to another agent session (on this hub or another machine's) as one
//! prompt under a provenance header (`From <session> on <branch>: ...`).
//!
//! Two steps, so the user reads and edits the prompt before it goes:
//!
//! 1. **Compose** on the hub that owns the *source* session
//!    ([`Hub::handoff_compose`]): exactly the content the user picked (the
//!    hub adds only the header and a fence around it), cut to a size cap
//!    with a notice saying so.
//! 2. **Deliver** on the hub that owns the *target* session
//!    ([`Hub::handoff_deliver`]): the (edited) prompt is pasted as a
//!    bracketed paste, then Enter -- the same path as every paste -- and
//!    `handoff.sent` is announced to plugins.
//!
//! The browser and `fw hub handoff` reach either hub through this hub's
//! `/m/<machine>` proxy, so a handoff crosses machines without either hub
//! knowing the other. Asking an agent of the other kind to review a
//! session's work lives in [`review`].
//!
//! (Not to be confused with [`crate::handoff`]: the live handoff of every
//! terminal to a new hub process.)

pub mod review;

use crate::agent::SessionKind;
use crate::hub::{Hub, HubError, SessionView};
use crate::review::Tools;
use crate::state::StateError;
use crate::transcript::{CommandFilter, PasteOutcome};
use serde::{Deserialize, Serialize};

/// Content kept in a composed prompt by default, in bytes.
pub const DEFAULT_CONTENT_BYTES: usize = 32 * 1024;
/// The most a compose request may ask to keep.
pub const MAX_CONTENT_BYTES: usize = 96 * 1024;
/// The least (a smaller cap would keep nothing useful).
pub const MIN_CONTENT_BYTES: usize = 256;
/// Largest prompt delivered (the composed one plus the user's edits).
pub const MAX_PROMPT_BYTES: usize = 128 * 1024;
/// Largest selection accepted from a client.
pub const MAX_SELECTION_BYTES: usize = 1024 * 1024;

/// What to hand off from the source session.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Text the user selected (in its terminal, or out of its transcript).
    Selection {
        text: String,
        /// Where it was selected: `terminal` (default) or `transcript`.
        #[serde(default)]
        from: Option<String>,
    },
    /// One message of its agent transcript (the viewer's / a search
    /// hit's index).
    Transcript { index: u64 },
    /// One of its suggested code blocks, by `seq`.
    Command { seq: u64 },
    /// Its newest suggested shell command.
    LastCommand,
    /// Its branch against its base (`git diff <base>...HEAD`), or one
    /// file's part of it.
    Diff {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        old_path: Option<String>,
        #[serde(default)]
        base: Option<String>,
    },
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Selection { .. } => "selection",
            Self::Transcript { .. } => "transcript",
            Self::Command { .. } => "command",
            Self::LastCommand => "last_command",
            Self::Diff { .. } => "diff",
        }
    }
}

/// A compose request.
#[derive(Debug, Clone, Deserialize)]
pub struct ComposeRequest {
    pub source: Source,
    /// Content bytes kept ([`DEFAULT_CONTENT_BYTES`] unless given).
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// The target is on another machine: the header names this one.
    #[serde(default)]
    pub cross_machine: bool,
}

/// Where a handoff comes from, as its header and `handoff.sent` say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub session_id: String,
    pub session: String,
    pub kind: SessionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The source hub's host name (set when composed for another
    /// machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// A composed prompt, for the user to read and edit.
#[derive(Debug, Clone, Serialize)]
pub struct Composed {
    pub from: Provenance,
    /// `selection`, `transcript`, `command`, `last_command`, `diff`.
    pub source: &'static str,
    /// What the header says it is ("selected terminal text").
    pub what: String,
    pub prompt: String,
    /// The content's size, and what of it the prompt keeps.
    pub content_bytes: usize,
    pub kept_bytes: usize,
    pub truncated: bool,
}

/// A deliver request (the composed prompt, maybe edited).
#[derive(Debug, Clone, Deserialize)]
pub struct Delivery {
    pub prompt: String,
    #[serde(default)]
    pub from: Option<Provenance>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

/// What a delivery did.
#[derive(Debug, Clone, Serialize)]
pub struct Delivered {
    pub to: TargetRef,
    pub paste: PasteOutcome,
}

/// A session a handoff went to.
#[derive(Debug, Clone, Serialize)]
pub struct TargetRef {
    pub id: String,
    pub name: String,
    pub kind: SessionKind,
}

/// The fence that keeps `content` one block: backticks, one more than its
/// longest run (at least three).
pub fn fence_for(content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for ch in content.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// `text` cut to at most `max` bytes: on a character boundary, and back to
/// the last line break when there is one (no half line). Whether it was
/// cut.
pub fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    match kept.rfind('\n') {
        Some(at) if at > 0 => (&kept[..at], true),
        _ => (kept, true),
    }
}

/// `From <session> on <branch> (machine <host>): <what>`; a session with
/// no branch is `in <its directory's name>`.
pub fn header(from: &Provenance, dir_name: Option<&str>, what: &str) -> String {
    let mut line = format!("From {}", from.session);
    match (&from.branch, dir_name) {
        (Some(branch), _) => line.push_str(&format!(" on {branch}")),
        (None, Some(dir)) if !dir.is_empty() => line.push_str(&format!(" in {dir}")),
        _ => {}
    }
    if let Some(machine) = from.machine.as_deref().filter(|m| !m.is_empty()) {
        line.push_str(&format!(" (machine {machine})"));
    }
    line.push_str(": ");
    line.push_str(what);
    line
}

/// The prompt: header, the content in one fenced block (`lang` its info
/// string), and a notice when it was cut. Returns it with the bytes kept.
pub fn assemble(
    header: &str,
    lang: &str,
    content: &str,
    max: usize,
    after: Option<&str>,
) -> (String, usize, bool) {
    let content = content.trim_end_matches(['\n', '\r']);
    let (kept, truncated) = cut(content, max);
    let fence = fence_for(kept);
    let mut prompt = format!("{header}\n\n{fence}{lang}\n{kept}\n{fence}\n");
    if truncated {
        prompt.push_str(&format!(
            "\n[truncated by fw hub: kept the first {} of {} bytes]\n",
            kept.len(),
            content.len()
        ));
    }
    if let Some(after) = after {
        prompt.push('\n');
        prompt.push_str(after);
        prompt.push('\n');
    }
    (prompt, kept.len(), truncated)
}

/// The size cap a request asks for, within bounds.
fn content_cap(asked: Option<usize>) -> Result<usize, HubError> {
    match asked {
        None => Ok(DEFAULT_CONTENT_BYTES),
        Some(n) if (MIN_CONTENT_BYTES..=MAX_CONTENT_BYTES).contains(&n) => Ok(n),
        Some(n) => Err(StateError::Invalid(format!(
            "max_bytes {n} is outside {MIN_CONTENT_BYTES}..={MAX_CONTENT_BYTES}"
        ))
        .into()),
    }
}

/// A session's branch: its git badge's, else its worktree's, else read
/// from git now (a badge not refreshed yet). Blocking.
pub(crate) fn branch_of(view: &SessionView) -> Option<String> {
    view.git
        .as_ref()
        .and_then(|g| g.branch.clone())
        .or_else(|| view.record.worktree.as_ref().map(|w| w.branch.clone()))
        .or_else(|| {
            view.record
                .cwd
                .is_dir()
                .then(|| crate::git::status(&view.record.cwd, None).ok())
                .flatten()
                .and_then(|s| s.branch)
        })
}

fn provenance(view: &SessionView, cross_machine: bool) -> Provenance {
    Provenance {
        session_id: view.record.id.clone(),
        session: view.record.name.clone(),
        kind: view.record.kind,
        branch: branch_of(view),
        machine: cross_machine
            .then(crate::machines::host_name)
            .filter(|h| !h.is_empty()),
    }
}

fn dir_name(view: &SessionView) -> Option<String> {
    view.record
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

impl Hub {
    /// Compose the prompt handing `request.source` of session `id` to
    /// another agent (nothing is sent). Blocking (a diff runs git).
    pub fn handoff_compose(
        &self,
        tools: &Tools,
        id: &str,
        request: &ComposeRequest,
    ) -> Result<Composed, HubError> {
        let max = content_cap(request.max_bytes)?;
        let view = self.session(id)?;
        let from = provenance(&view, request.cross_machine);
        let (what, lang, content) = self.handoff_content(tools, &view, &request.source)?;
        if content.trim().is_empty() {
            return Err(
                StateError::Invalid("nothing to hand off: the content is empty".into()).into(),
            );
        }
        let header = header(&from, dir_name(&view).as_deref(), &what);
        let (prompt, kept, truncated) = assemble(&header, &lang, &content, max, None);
        Ok(Composed {
            from,
            source: request.source.name(),
            what,
            prompt,
            content_bytes: content.trim_end_matches(['\n', '\r']).len(),
            kept_bytes: kept,
            truncated,
        })
    }

    /// What the header calls the content, its fence's info string, and
    /// the content itself.
    fn handoff_content(
        &self,
        tools: &Tools,
        view: &SessionView,
        source: &Source,
    ) -> Result<(String, String, String), HubError> {
        let record = &view.record;
        match source {
            Source::Selection { text, from } => {
                if text.len() > MAX_SELECTION_BYTES {
                    return Err(StateError::Invalid(format!(
                        "the selection is larger than {MAX_SELECTION_BYTES} bytes"
                    ))
                    .into());
                }
                let what = match from.as_deref().unwrap_or("terminal") {
                    "terminal" => "selected terminal text",
                    "transcript" => "an excerpt of its transcript",
                    other => {
                        return Err(StateError::Invalid(format!(
                            "selection from {other:?}: terminal or transcript"
                        ))
                        .into());
                    }
                };
                Ok((what.to_owned(), "text".to_owned(), text.clone()))
            }
            Source::Transcript { index } => {
                let page = self.transcript_page(&record.id, Some(*index), 0, 0)?;
                let message = page
                    .messages
                    .into_iter()
                    .find(|m| m.meta.index == *index)
                    .ok_or_else(|| {
                        StateError::NotFound(format!(
                            "message {index} of {}'s transcript (gone, or not written yet)",
                            record.label()
                        ))
                    })?;
                let role = match message.meta.role {
                    crate::agentlog::Role::User => "a prompt",
                    crate::agentlog::Role::Assistant => "a reply",
                };
                let what = match (&message.meta.kind, &message.meta.tool) {
                    (crate::search::MessageKind::Tool, Some(tool)) => {
                        format!("a {tool} tool call from its transcript")
                    }
                    _ => format!("{role} from its transcript"),
                };
                Ok((what, "text".to_owned(), message.text))
            }
            Source::Command { seq } => {
                let list = self.commands(
                    &record.id,
                    CommandFilter {
                        all: true,
                        limit: usize::MAX,
                    },
                )?;
                let command = list
                    .commands
                    .into_iter()
                    .find(|c| c.seq == *seq)
                    .ok_or_else(|| {
                        StateError::NotFound(format!("suggested block {seq} of {}", record.label()))
                    })?;
                let what = if command.shell {
                    "a command it suggested"
                } else {
                    "a code block it suggested"
                };
                let lang = command
                    .lang
                    .clone()
                    .unwrap_or_else(|| if command.shell { "sh" } else { "text" }.to_owned());
                Ok((what.to_owned(), lang, command.text))
            }
            Source::LastCommand => {
                let list = self.commands(
                    &record.id,
                    CommandFilter {
                        all: false,
                        limit: 1,
                    },
                )?;
                let command = list.commands.into_iter().next().ok_or_else(|| {
                    StateError::Conflict(format!(
                        "{} has suggested no shell command yet",
                        record.label()
                    ))
                })?;
                let lang = command.lang.clone().unwrap_or_else(|| "sh".to_owned());
                Ok((
                    "the last command it suggested".to_owned(),
                    lang,
                    command.text,
                ))
            }
            Source::Diff {
                path,
                old_path,
                base,
            } => {
                let (_, target) = self.review_target(tools, &record.id, base.as_deref())?;
                let diff = crate::review::branch_diff(
                    tools,
                    &target,
                    path.as_deref(),
                    old_path.as_deref(),
                )
                .map_err(StateError::Conflict)?;
                if diff.text.trim().is_empty() {
                    return Err(StateError::Conflict(match path {
                        Some(path) => format!("{path} has no committed change vs {}", diff.base),
                        None => format!("nothing committed beyond {} yet", diff.base),
                    })
                    .into());
                }
                let what = match path {
                    Some(path) => format!(
                        "its diff of {path} vs {} (git diff {}...HEAD -- {path})",
                        diff.base, diff.base
                    ),
                    None => format!(
                        "its branch's diff vs {} (git diff {}...HEAD)",
                        diff.base, diff.base
                    ),
                };
                Ok((what, "diff".to_owned(), diff.text))
            }
        }
    }

    /// Paste a handoff into agent session `to`, then Enter, and announce
    /// `handoff.sent`.
    pub fn handoff_deliver(&self, to: &str, delivery: &Delivery) -> Result<Delivered, HubError> {
        let view = self.session(to)?;
        let record = &view.record;
        if !record.runs_agent() {
            return Err(StateError::Invalid(format!(
                "{} is not an agent session: hand off to a Claude Code or Codex session (fw hub send types into any)",
                record.label()
            ))
            .into());
        }
        if let Some(from) = &delivery.from
            && from.session_id == record.id
            && from.machine.is_none()
        {
            return Err(StateError::Invalid(format!(
                "{} is the session it comes from: pick another agent",
                record.label()
            ))
            .into());
        }
        if delivery.prompt.len() > MAX_PROMPT_BYTES {
            return Err(StateError::Invalid(format!(
                "the prompt is {} bytes; at most {MAX_PROMPT_BYTES} are sent",
                delivery.prompt.len()
            ))
            .into());
        }
        let source = delivery
            .source
            .as_deref()
            .filter(|s| {
                [
                    "selection",
                    "transcript",
                    "command",
                    "last_command",
                    "diff",
                    "review_findings",
                ]
                .contains(s)
            })
            .map(str::to_owned);
        let pasted = self.paste(&record.id, &delivery.prompt, true);
        let from_label = delivery
            .from
            .as_ref()
            .map_or_else(|| "(unnamed)".to_owned(), |f| f.session.clone());
        eprintln!(
            "fw hub: handoff {from_label} -> {}: {} bytes{} -> {}",
            record.label(),
            delivery.prompt.len(),
            source
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default(),
            match &pasted {
                Ok(_) => "pasted".to_owned(),
                Err(e) => format!("failed: {e}"),
            }
        );
        let paste = pasted?;
        if let Some(session) = self.event_session(&record.id) {
            self.emit_lifecycle(
                "handoff.sent",
                serde_json::json!({
                    "from": delivery.from,
                    "to": session,
                    "source": source,
                    "bytes": delivery.prompt.len(),
                    "truncated": delivery.truncated,
                }),
            );
        }
        Ok(Delivered {
            to: TargetRef {
                id: record.id.clone(),
                name: record.name.clone(),
                kind: record.kind,
            },
            paste,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(branch: Option<&str>, machine: Option<&str>) -> Provenance {
        Provenance {
            session_id: "s1".into(),
            session: "claude-1".into(),
            kind: SessionKind::Claude,
            branch: branch.map(str::to_owned),
            machine: machine.map(str::to_owned),
        }
    }

    #[test]
    fn the_header_names_the_session_its_branch_and_a_remote_machine() {
        assert_eq!(
            header(
                &from(Some("hub/34"), None),
                Some("repo"),
                "selected terminal text"
            ),
            "From claude-1 on hub/34: selected terminal text"
        );
        assert_eq!(
            header(&from(None, None), Some("notes"), "x"),
            "From claude-1 in notes: x"
        );
        assert_eq!(
            header(&from(Some("main"), Some("box")), None, "x"),
            "From claude-1 on main (machine box): x"
        );
    }

    #[test]
    fn a_fence_outruns_every_backtick_run_inside() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("a ```sh\nx\n``` b"), "````");
        assert_eq!(fence_for("`````"), "``````");
    }

    #[test]
    fn cutting_keeps_whole_lines_and_characters() {
        assert_eq!(cut("short", 10), ("short", false));
        assert_eq!(cut("one\ntwo\nthree", 9), ("one\ntwo", true));
        // No line break inside the cap: cut on a character boundary.
        let (kept, cut_) = cut("ééééé", 5);
        assert!(cut_);
        assert_eq!(kept, "éé");
    }

    #[test]
    fn a_cut_prompt_says_how_much_it_kept() {
        let content = "line\n".repeat(100);
        let (prompt, kept, truncated) = assemble("From a on b: c", "text", &content, 52, None);
        assert!(truncated);
        assert_eq!(kept, 49);
        assert!(prompt.starts_with("From a on b: c\n\n```text\nline\n"));
        assert!(prompt.ends_with("```\n\n[truncated by fw hub: kept the first 49 of 499 bytes]\n"));
        let (whole, _, truncated) = assemble("h", "diff", "+x\n", 1000, Some("Then: y"));
        assert!(!truncated);
        assert_eq!(whole, "h\n\n```diff\n+x\n```\n\nThen: y\n");
    }
}
