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

use super::event::{AskKind, HoldKind};

/// Shortest `context` a decision or an approval carries: a sentence or
/// two on the situation and why the recommendation.
pub const MIN_CONTEXT_CHARS: usize = 40;

/// Why an `ask_operator` call's `input` is not an ask the operator can
/// decide from the card alone; `None` when it is. A choice or a yes-or-no
/// needs what each answer means and the agent's recommendation, the way a
/// question tool's options each carry a description and the recommended
/// one comes first. The hub records no ask for a refused call, and the
/// tool says why, so the agent asks again whole.
pub fn ask_problem(input: &serde_json::Value) -> Option<String> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if text("text").or_else(|| text("question")).is_none() {
        return Some("give text: the ask in one plain sentence".into());
    }
    let kind = AskKind::parse(text("kind").unwrap_or("answer"));
    let approval = text("hold").and_then(HoldKind::parse) == Some(HoldKind::AwaitingApproval);
    if kind == AskKind::Decide {
        let options = input
            .get("options")
            .and_then(serde_json::Value::as_array)
            .map_or(0, |o| {
                o.iter()
                    .filter(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
                    .count()
            });
        if options < 2 {
            return Some("a decide ask gives at least 2 options, recommended first".into());
        }
    }
    if kind == AskKind::Decide || approval {
        let what = if approval {
            "an approval"
        } else {
            "a decide ask"
        };
        if text("context").is_none_or(|c| c.chars().count() < MIN_CONTEXT_CHARS) {
            return Some(format!(
                "{what} gives context: the situation, what each answer leads to, and why you \
                 recommend yours, so the operator decides from the card alone"
            ));
        }
        if text("recommendation").is_none() {
            return Some(format!(
                "{what} gives recommendation: the answer you would pick"
            ));
        }
    }
    None
}

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
    /// The successor's first prompt (`handoff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The hold's kind the agent named (else read from `kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<HoldKind>,
    /// The agent's grouping key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// The answer the agent recommends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// What the operator needs to answer it without the conversation:
    /// the agent's (`ask_operator`'s `context`), else the closing message
    /// of the turn that asked (bounded at capture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The session the ask concerns (one the asker works with), as the
    /// agent named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
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
