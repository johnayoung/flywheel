//! What an agent asks the operator for when its turn ends on them.
//!
//! Two sources. An agent can declare it (the hub's `ask_operator` tool,
//! [`super::mcp`]): a reply, a decision between options, or a command
//! only the operator can run. Else the hub reads the turn's closing
//! message: one whose last paragraph asks a question waits on an answer.
//! Only questions are inferred. A command handed over in prose is too
//! often a suggestion rather than a request, so asking someone to run
//! one is the agent's to declare.

use super::summary;
use serde::{Deserialize, Serialize};

use super::event::AskKind;

/// What a session waits on the operator for, once its turn has ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub kind: AskKind,
    /// The ask in one line.
    pub text: String,
    /// The command to run (`run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The choices (`decide`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// `declared` (the agent's tool) or `inferred` (its closing message).
    pub source: String,
    /// When the turn that asked ended (or, while it runs, when it asked).
    pub since: i64,
}

/// How many trailing sentences of the last paragraph may hold the
/// question.
const QUESTION_WINDOW: usize = 3;

/// The question a closing message ends on, in one line: its last
/// paragraph outside code fences, from the first of its last few
/// sentences that asks something.
pub fn question(ending: &str) -> Option<String> {
    let mut prose = String::new();
    let mut fenced = false;
    for line in ending.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    let last = prose
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .last()?;
    let sentences = sentences(last);
    let from = sentences.len().saturating_sub(QUESTION_WINDOW);
    let first = (from..sentences.len()).find(|&i| asks(sentences[i]))?;
    let text = sentences[first..].join(" ");
    Some(tail_line(&text))
}

/// The sentences of a paragraph, each with its closing mark.
fn sentences(paragraph: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = paragraph.as_bytes();
    for (i, c) in paragraph.char_indices() {
        if matches!(c, '.' | '?' | '!') {
            let next = bytes.get(i + 1).copied();
            if next.is_none_or(|b| b.is_ascii_whitespace()) {
                let s = paragraph[start..=i].trim();
                if !s.is_empty() {
                    out.push(s);
                }
                start = i + 1;
            }
        }
    }
    let rest = paragraph[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// A sentence that asks: it ends on a question mark once Markdown's
/// closing emphasis and brackets are set aside.
fn asks(sentence: &str) -> bool {
    sentence
        .trim_end_matches(['*', '_', '`', ')', '"', '\'', ' '])
        .ends_with('?')
}

/// One scrubbed line, cut at its start when long (a question's end is
/// what matters).
fn tail_line(text: &str) -> String {
    let line = summary::text(&text.replace('\n', " "));
    if text.chars().count() <= summary::SUMMARY_CAP {
        return line;
    }
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let skip = collapsed
        .chars()
        .count()
        .saturating_sub(summary::SUMMARY_CAP - 1);
    let tail: String = collapsed.chars().skip(skip).collect();
    format!("\u{2026}{}", summary::text(&tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closing_question_is_an_ask() {
        assert_eq!(
            question("Done: the board is gone.\n\nShould I delete the mockup branch too?")
                .as_deref(),
            Some("Should I delete the mockup branch too?")
        );
        // The question may be followed by a sentence or two.
        assert_eq!(
            question("Want me to start with step 0? I'd run the spec first. It is small.")
                .as_deref(),
            Some("Want me to start with step 0? I'd run the spec first. It is small.")
        );
        assert_eq!(
            question("**Should I commit the mockup?**").as_deref(),
            Some("**Should I commit the mockup?**")
        );
    }

    #[test]
    fn a_report_is_not_an_ask() {
        assert_eq!(
            question("All green. Nothing else is pending from me."),
            None
        );
        // A question earlier in the message, then a closing report.
        assert_eq!(
            question("Why did it fail?\n\nIt was the lock. Fixed and pushed."),
            None
        );
        // Code is not prose: a `?` inside a fence asks nothing.
        assert_eq!(question("Run:\n```\ntest -f x || echo missing?\n```"), None);
        assert_eq!(question(""), None);
    }

    #[test]
    fn a_long_question_keeps_its_end() {
        let long = format!("{} is that right?", "word ".repeat(80));
        let got = question(&long).unwrap();
        assert!(got.starts_with('\u{2026}'), "{got}");
        assert!(got.ends_with("is that right?"), "{got}");
        assert!(got.chars().count() <= summary::SUMMARY_CAP + 1);
    }
}
