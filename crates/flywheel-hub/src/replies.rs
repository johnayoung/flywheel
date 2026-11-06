//! Agent replies: the assistant's text messages of an agent session, as
//! the agent wrote them (Markdown, without the terminal's margins, hard
//! wraps, or bullet), for copying out whole or block by block.
//!
//! What a reply is: one assistant text message of the transcript -- a
//! Claude Code line's text (its text parts joined by a blank line), or a
//! Codex `output_text` message. A turn that talks between its tool calls
//! writes several; each is a reply of its own, and the newest is the "last
//! reply" (once the turn ends, its final text). Thinking and tool calls
//! are not replies.
//!
//! The hub reads them in the same tail as suggested commands
//! ([`crate::transcript`]), with the same fallbacks: a Claude Code Stop
//! hook's `last_assistant_message` and a Codex `notify` payload's
//! `last-assistant-message` stand in until the transcript line is read,
//! and that line then takes their place (same `seq`; the fallback may have
//! been cut short to fit a report) rather than being added again. A reply
//! is known by its conversation and message id, so a file read again adds
//! nothing twice. Each session keeps its newest [`MAX_REPLIES`], each cut
//! at [`MAX_REPLY_BYTES`] (`truncated`), saved with its commands.
//!
//! This module is the pure part: [`ReplyLog`] (the bounded, deduplicated
//! list), [`blocks`] (a reply split into Markdown blocks for per-block
//! copy), and [`match_key`] (what the web client matches a reply's
//! rendering in a terminal against).

use crate::commands::{CommandSource, closes, fenced_blocks, opening_fence};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

/// Replies kept per session (oldest dropped first).
pub const MAX_REPLIES: usize = 100;
/// Longest reply kept whole; a longer one is cut (at a character
/// boundary) and marked `truncated`.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Reply keys remembered per session against re-reading a replaced file.
const MAX_KEYS: usize = 4096;
/// Letters and digits in a [`match_key`].
pub const MATCH_KEY_CHARS: usize = 64;

/// One assistant reply of an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReply {
    /// Per-session sequence number, increasing in arrival order (a
    /// fallback replaced by its transcript line keeps its own).
    pub seq: u64,
    /// The agent conversation it came from.
    pub conversation: Option<String>,
    /// Claude's line `uuid`, Codex's message id (else `@<offset>`), or the
    /// fallback's turn id.
    pub message_id: String,
    /// When the agent wrote it (RFC 3339, as the agent recorded it).
    pub timestamp: Option<String>,
    /// The reply's Markdown exactly as written.
    pub text: String,
    /// `text` was cut at [`MAX_REPLY_BYTES`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub source: CommandSource,
}

impl AgentReply {
    /// A fallback (hook or notify payload) its transcript line replaces.
    fn is_fallback(&self) -> bool {
        matches!(self.source, CommandSource::Hook | CommandSource::Notify)
    }
}

/// A reply as the API gives it: with its blocks and match key.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyView {
    #[serde(flatten)]
    pub reply: AgentReply,
    pub blocks: Vec<Block>,
    pub match_key: String,
}

impl From<AgentReply> for ReplyView {
    fn from(reply: AgentReply) -> Self {
        Self {
            blocks: blocks(&reply.text),
            match_key: match_key(&reply.text),
            reply,
        }
    }
}

/// One session's replies: the newest [`MAX_REPLIES`], oldest first.
#[derive(Debug, Default)]
pub struct ReplyLog {
    entries: VecDeque<AgentReply>,
    next_seq: u64,
    keys: HashSet<(String, String)>,
    key_order: VecDeque<(String, String)>,
}

impl ReplyLog {
    /// The log saved as `entries` (oldest first) and `next_seq`.
    pub fn restore(entries: Vec<AgentReply>, next_seq: u64) -> Self {
        let mut log = Self::default();
        let skip = entries.len().saturating_sub(MAX_REPLIES);
        for reply in entries.into_iter().skip(skip) {
            log.remember(key_of(reply.conversation.as_deref(), &reply.message_id));
            log.next_seq = log.next_seq.max(reply.seq + 1);
            log.entries.push_back(reply);
        }
        log.next_seq = log.next_seq.max(next_seq);
        log
    }

    /// What to save: the entries (oldest first) and the next `seq`.
    pub fn saved(&self) -> (Vec<AgentReply>, u64) {
        (self.entries.iter().cloned().collect(), self.next_seq)
    }

    /// The newest `limit` replies, newest first.
    pub fn newest(&self, limit: usize) -> impl Iterator<Item = &AgentReply> {
        self.entries.iter().rev().take(limit)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add reply `message_id` of `conversation`; returns the entry added,
    /// or the fallback it replaced (updated, same `seq`), or `None` when it
    /// is known already or empty.
    ///
    /// A transcript or rollout reply replaces the oldest fallback of its
    /// conversation whose text it begins with. A fallback is not added
    /// when the newest reply of its conversation was read from the
    /// transcript and begins with its text (the line came first).
    pub fn add(
        &mut self,
        conversation: Option<&str>,
        message_id: &str,
        timestamp: Option<&str>,
        text: &str,
        source: CommandSource,
    ) -> Option<AgentReply> {
        if text.trim().is_empty() {
            return None;
        }
        let key = key_of(conversation, message_id);
        if self.keys.contains(&key) {
            return None;
        }
        let (text, truncated) = cap(text);
        let fallback = matches!(source, CommandSource::Hook | CommandSource::Notify);
        let same = |r: &AgentReply| r.conversation.as_deref() == conversation;
        if fallback {
            let read = self
                .entries
                .iter()
                .rev()
                .find(|r| same(r))
                .is_some_and(|r| !r.is_fallback() && begins_with(&r.text, &text));
            if read {
                return None;
            }
        } else if let Some(at) = self
            .entries
            .iter()
            .position(|r| same(r) && r.is_fallback() && begins_with(&text, &r.text))
        {
            self.remember(key);
            let entry = &mut self.entries[at];
            entry.message_id = message_id.to_owned();
            entry.timestamp = timestamp.map(str::to_owned);
            entry.text = text;
            entry.truncated = truncated;
            entry.source = source;
            return Some(entry.clone());
        }
        self.remember(key);
        let reply = AgentReply {
            seq: self.next_seq,
            conversation: conversation.map(str::to_owned),
            message_id: message_id.to_owned(),
            timestamp: timestamp.map(str::to_owned),
            text,
            truncated,
            source,
        };
        self.next_seq += 1;
        if self.entries.len() == MAX_REPLIES {
            self.entries.pop_front();
        }
        self.entries.push_back(reply.clone());
        Some(reply)
    }

    fn remember(&mut self, key: (String, String)) {
        if self.keys.contains(&key) {
            return;
        }
        if self.key_order.len() == MAX_KEYS
            && let Some(old) = self.key_order.pop_front()
        {
            self.keys.remove(&old);
        }
        self.keys.insert(key.clone());
        self.key_order.push_back(key);
    }
}

/// Keep at most [`MAX_REPLIES`] of `added` and `more` (the newest); an
/// entry updated again replaces its earlier copy.
pub fn push_bounded(added: &mut Vec<AgentReply>, more: AgentReply) {
    added.retain(|r| r.seq != more.seq);
    added.push(more);
    if added.len() > MAX_REPLIES {
        added.drain(..added.len() - MAX_REPLIES);
    }
}

/// A reply's identity: conversation and message.
fn key_of(conversation: Option<&str>, message_id: &str) -> (String, String) {
    (
        conversation.unwrap_or_default().to_owned(),
        message_id.to_owned(),
    )
}

/// `text` cut to [`MAX_REPLY_BYTES`] at a character boundary, and whether
/// it was cut.
fn cap(text: &str) -> (String, bool) {
    if text.len() <= MAX_REPLY_BYTES {
        return (text.to_owned(), false);
    }
    let mut cut = MAX_REPLY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_owned(), true)
}

/// Whether `text` begins with `start` (trailing whitespace aside: a
/// payload's text may be trimmed or cut).
fn begins_with(text: &str, start: &str) -> bool {
    let start = start.trim_end();
    !start.is_empty() && text.starts_with(start)
}

/// What kind of Markdown block a [`Block`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Paragraph,
    Heading,
    /// A bullet or numbered list (its items, nested lists, and code
    /// blocks indented under them; blank lines between items included).
    List,
    /// A fenced code block.
    Code,
    Table,
    Quote,
}

/// One Markdown block of a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Block {
    pub kind: BlockKind,
    /// The block's Markdown exactly as written: its lines, without the
    /// newline after the last.
    pub text: String,
    /// A code block's language (its info string's first word).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// A code block's content without its fence lines (what its Copy
    /// takes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// The block a line starts (or belongs to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Blank,
    Fence,
    Heading,
    ListItem,
    Table,
    Quote,
    /// A thematic break (`---`): ends a block, is none.
    Rule,
    Text,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Whether `line` (indentation removed) starts a list item: `-`, `*`, or
/// `+`, or 1-9 digits and `.` or `)`, then a space or the line's end.
fn is_list_item(line: &str) -> bool {
    let bytes = line.as_bytes();
    let after = match bytes.first() {
        Some(b'-' | b'*' | b'+') => 1,
        Some(b'0'..=b'9') => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return false;
            }
            digits + 1
        }
        _ => return false,
    };
    matches!(bytes.get(after), None | Some(b' ' | b'\t'))
}

/// `---`, `***`, `___` (three or more, spaces between allowed).
fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(ch) = trimmed
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '*' | '_'))
    else {
        return false;
    };
    trimmed.chars().all(|c| c == ch || c == ' ')
        && trimmed.chars().filter(|&c| c == ch).count() >= 3
}

fn is_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && matches!(line.as_bytes().get(hashes), None | Some(b' ' | b'\t'))
}

fn classify(line: &str) -> LineKind {
    if line.trim().is_empty() {
        return LineKind::Blank;
    }
    if opening_fence(line).is_some() {
        return LineKind::Fence;
    }
    let indent = indent_of(line);
    let rest = &line[indent..];
    if indent < 4 && is_heading(rest) {
        LineKind::Heading
    } else if indent < 4 && is_rule(rest) {
        LineKind::Rule
    } else if is_list_item(rest) {
        LineKind::ListItem
    } else if rest.starts_with('|') {
        LineKind::Table
    } else if rest.starts_with('>') {
        LineKind::Quote
    } else {
        LineKind::Text
    }
}

/// A block being gathered: its kind and byte range in the text.
struct Open {
    kind: BlockKind,
    start: usize,
    end: usize,
}

/// `markdown` split into blocks, in order: paragraphs, headings, lists,
/// fenced code (never split at a blank line inside it), tables, and quotes.
/// Blank lines and thematic breaks separate blocks and belong to none.
pub fn blocks(markdown: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut open: Option<Open> = None;
    // The fence the current block is inside of (a code block, or one
    // indented under a list item).
    let mut fence = None;
    let mut blank_before = false;
    let mut offset = 0;
    let flush = |open: &mut Option<Open>, out: &mut Vec<Block>| {
        if let Some(block) = open.take() {
            out.push(block_of(markdown, block));
        }
    };
    for raw in markdown.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let end = start + line.len();
        if let Some(inside) = fence {
            if let Some(block) = open.as_mut() {
                block.end = end;
            }
            if closes(line, inside) {
                fence = None;
                if open.as_ref().is_some_and(|b| b.kind == BlockKind::Code) {
                    flush(&mut open, &mut out);
                }
            }
            continue;
        }
        let kind = classify(line);
        if kind == LineKind::Blank {
            blank_before = true;
            // A list goes on past a blank line (a loose list); nothing
            // else does.
            if open.as_ref().is_some_and(|b| b.kind != BlockKind::List) {
                flush(&mut open, &mut out);
            }
            continue;
        }
        let indent = indent_of(line);
        let current = open.as_ref().map(|b| b.kind);
        let goes_on = match current {
            None | Some(BlockKind::Heading | BlockKind::Code) => false,
            Some(BlockKind::List) => {
                if blank_before {
                    // After a blank: another item, or something indented
                    // under one.
                    indent >= 2 || kind == LineKind::ListItem
                } else {
                    // Lazy continuation, nested items, indented fences;
                    // a heading, a rule, or a fence at the margin ends it.
                    let ends = matches!(kind, LineKind::Heading | LineKind::Rule)
                        || (kind == LineKind::Fence && indent < 2);
                    !ends
                }
            }
            Some(BlockKind::Paragraph) => kind == LineKind::Text,
            Some(BlockKind::Table) => kind == LineKind::Table,
            Some(BlockKind::Quote) => matches!(kind, LineKind::Quote | LineKind::Text),
        };
        blank_before = false;
        if goes_on {
            if let Some(block) = open.as_mut() {
                block.end = end;
            }
            if kind == LineKind::Fence {
                fence = opening_fence(line).map(|(f, _)| f);
            }
            continue;
        }
        flush(&mut open, &mut out);
        let kind = match kind {
            LineKind::Rule | LineKind::Blank => continue,
            LineKind::Fence => {
                fence = opening_fence(line).map(|(f, _)| f);
                BlockKind::Code
            }
            LineKind::Heading => BlockKind::Heading,
            LineKind::ListItem => BlockKind::List,
            LineKind::Table => BlockKind::Table,
            LineKind::Quote => BlockKind::Quote,
            LineKind::Text => BlockKind::Paragraph,
        };
        open = Some(Open { kind, start, end });
    }
    flush(&mut open, &mut out);
    out
}

fn block_of(markdown: &str, open: Open) -> Block {
    let text = markdown[open.start..open.end].to_owned();
    let (lang, code) = if open.kind == BlockKind::Code {
        // An unclosed fence still being written reads as no block: its
        // content is what is there so far.
        match fenced_blocks(&text).into_iter().next() {
            Some(fenced) => (fenced.lang, Some(fenced.text)),
            None => (None, Some(String::new())),
        }
    } else {
        (None, None)
    };
    Block {
        kind: open.kind,
        text,
        lang,
        code,
    }
}

/// What a reply's rendering in a terminal is matched against: the first
/// [`MATCH_KEY_CHARS`] letters and digits of its text, leaving out what
/// agents do not draw (fence lines, a link's target). Markup, bullets,
/// indentation, and wrapping are not letters or digits, so a terminal's
/// rows (bullet and hanging indent included, joined in order) reduced the
/// same way begin with the same characters.
pub fn match_key(text: &str) -> String {
    let mut key = String::new();
    let mut count = 0;
    let mut fence = None;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match fence {
            Some(inside) => {
                if closes(line, inside) {
                    fence = None;
                    continue;
                }
            }
            None => {
                if let Some((open, _)) = opening_fence(line) {
                    fence = Some(open);
                    continue;
                }
            }
        }
        let mut rest = line;
        while !rest.is_empty() {
            // `[text](target)`: the target is skipped.
            let (shown, after) = match rest.find("](") {
                Some(at) => {
                    let target = &rest[at + 2..];
                    let skip = target.find(')').map_or(target.len(), |end| end + 1);
                    (&rest[..at], &target[skip..])
                }
                None => (rest, ""),
            };
            for c in shown.chars().filter(|c| c.is_alphanumeric()) {
                key.push(c);
                count += 1;
                if count == MATCH_KEY_CHARS {
                    return key;
                }
            }
            rest = after;
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(markdown: &str) -> Vec<(BlockKind, String)> {
        blocks(markdown)
            .into_iter()
            .map(|b| (b.kind, b.text))
            .collect()
    }

    #[test]
    fn a_slack_message_splits_into_its_paragraphs_and_list() {
        let reply = "Hey team, quick update on the **release**:\nit ships Friday.\n\n- API frozen\n- docs: `fw docs`\n  (in review)\n- tests green\n\nThanks!";
        assert_eq!(
            kinds(reply),
            [
                (
                    BlockKind::Paragraph,
                    "Hey team, quick update on the **release**:\nit ships Friday.".into()
                ),
                (
                    BlockKind::List,
                    "- API frozen\n- docs: `fw docs`\n  (in review)\n- tests green".into()
                ),
                (BlockKind::Paragraph, "Thanks!".into()),
            ]
        );
    }

    #[test]
    fn fenced_code_is_never_split_at_its_blank_lines() {
        let reply =
            "Run:\n\n```bash\nmake\n\n\nmake install\n```\nDone.\n~~~\nunclosed\n\nstill code";
        let got = blocks(reply);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[1].kind, BlockKind::Code);
        assert_eq!(got[1].text, "```bash\nmake\n\n\nmake install\n```");
        assert_eq!(got[1].lang.as_deref(), Some("bash"));
        assert_eq!(got[1].code.as_deref(), Some("make\n\n\nmake install"));
        assert_eq!(
            (got[2].kind, got[2].text.as_str()),
            (BlockKind::Paragraph, "Done.")
        );
        assert_eq!(got[3].kind, BlockKind::Code);
        assert_eq!(got[3].text, "~~~\nunclosed\n\nstill code");
        assert_eq!(got[3].code.as_deref(), Some("unclosed\n\nstill code"));
    }

    #[test]
    fn headings_tables_quotes_and_loose_lists_are_blocks() {
        let reply = "# Plan\nFirst para\n## Steps\n1. one\n\n2. two\n   ```sh\n   ls\n\n   pwd\n   ```\n3) three\n---\n| a | b |\n|---|---|\n| 1 | 2 |\n> quoted\nlazy\n\n* star\n+ plus\n\n    indented after blank\n\nend\r\n";
        assert_eq!(
            kinds(reply),
            [
                (BlockKind::Heading, "# Plan".into()),
                (BlockKind::Paragraph, "First para".into()),
                (BlockKind::Heading, "## Steps".into()),
                (
                    BlockKind::List,
                    "1. one\n\n2. two\n   ```sh\n   ls\n\n   pwd\n   ```\n3) three".into()
                ),
                (BlockKind::Table, "| a | b |\n|---|---|\n| 1 | 2 |".into()),
                (BlockKind::Quote, "> quoted\nlazy".into()),
                (
                    BlockKind::List,
                    "* star\n+ plus\n\n    indented after blank".into()
                ),
                (BlockKind::Paragraph, "end".into()),
            ]
        );
        // Not list items or headings: emphasis, a hashtag, a year.
        assert_eq!(
            kinds("**bold** start\n#tag\n2026 was long")[0].0,
            BlockKind::Paragraph
        );
        assert_eq!(kinds("**bold** start\n#tag\n2026 was long").len(), 1);
        assert!(blocks("").is_empty());
        assert!(blocks("\n\n---\n").is_empty());
    }

    #[test]
    fn every_block_is_an_exact_slice_of_the_reply() {
        let reply = "a\r\nb\r\n\r\n- x\r\n```\r\ncode\r\n```\r\n";
        for block in blocks(reply) {
            assert!(reply.contains(&block.text), "{block:?}");
            assert!(!block.text.ends_with('\r'), "{block:?}");
        }
    }

    #[test]
    fn match_keys_ignore_markup_wrapping_fences_and_link_targets() {
        let markdown = "Hey **team**, see [the docs](https://example.com/x) and:\n\n```bash\nls -la\n```\n- one";
        assert_eq!(match_key(markdown), "Heyteamseethedocsandlslaone");
        // How Claude Code draws it: bullet, hanging indent, hard wraps at
        // the pane's width, emphasis and fences gone.
        let rendered = "● Hey team, see the docs\n  and:\n\n  ls -la\n\n  - one";
        let reduced: String = rendered.chars().filter(|c| c.is_alphanumeric()).collect();
        assert_eq!(reduced, match_key(markdown));
        let long = "word ".repeat(100);
        assert_eq!(match_key(&long).chars().count(), MATCH_KEY_CHARS);
        assert_eq!(match_key("[unclosed](http"), "unclosed");
    }

    fn add(log: &mut ReplyLog, id: &str, text: &str, source: CommandSource) -> Option<u64> {
        log.add(Some("c1"), id, None, text, source).map(|r| r.seq)
    }

    #[test]
    fn replies_are_deduplicated_bounded_and_capped() {
        let mut log = ReplyLog::default();
        assert_eq!(
            add(&mut log, "a", "first", CommandSource::Transcript),
            Some(0)
        );
        assert_eq!(add(&mut log, "a", "first", CommandSource::Transcript), None);
        assert_eq!(add(&mut log, "b", "  \n", CommandSource::Transcript), None);
        // Another conversation's same id is another reply.
        assert!(
            log.add(Some("c2"), "a", None, "x", CommandSource::Transcript)
                .is_some()
        );
        for n in 0..MAX_REPLIES + 5 {
            add(
                &mut log,
                &format!("m{n}"),
                "text",
                CommandSource::Transcript,
            );
        }
        assert_eq!(log.len(), MAX_REPLIES);
        let newest: Vec<u64> = log.newest(2).map(|r| r.seq).collect();
        let last = (MAX_REPLIES + 6) as u64;
        assert_eq!(newest, [last, last - 1]);
        let big = format!("{}é", "x".repeat(MAX_REPLY_BYTES - 1));
        let reply = log
            .add(Some("c1"), "big", None, &big, CommandSource::Transcript)
            .unwrap();
        assert!(reply.truncated);
        assert_eq!(reply.text.len(), MAX_REPLY_BYTES - 1);
        // Restored, it keeps its seqs and still knows what it read.
        let (entries, next) = log.saved();
        let mut back = ReplyLog::restore(entries, next);
        assert_eq!(
            add(&mut back, "big", "again", CommandSource::Transcript),
            None
        );
        assert_eq!(
            add(&mut back, "new", "n", CommandSource::Transcript),
            Some(next)
        );
    }

    #[test]
    fn a_fallback_is_replaced_by_its_transcript_line_or_skipped_after_it() {
        let mut log = ReplyLog::default();
        add(&mut log, "u1", "Looking.", CommandSource::Transcript);
        // The Stop hook's reply (cut short) comes before its line...
        assert_eq!(
            add(&mut log, "stop-1", "Done: all", CommandSource::Hook),
            Some(1)
        );
        // ...which takes its place.
        let line = log
            .add(
                Some("c1"),
                "u2",
                Some("t"),
                "Done: all green.",
                CommandSource::Transcript,
            )
            .unwrap();
        assert_eq!((line.seq, line.source), (1, CommandSource::Transcript));
        assert_eq!(line.text, "Done: all green.");
        assert_eq!(log.len(), 2);
        // A hook after its line adds nothing.
        assert_eq!(
            add(
                &mut log,
                "stop-2",
                "Done: all green.\n",
                CommandSource::Hook
            ),
            None
        );
        // A notify reply for another turn is added; the rollout's line for
        // it replaces it.
        assert_eq!(
            add(&mut log, "turn-9", "Next.", CommandSource::Notify),
            Some(2)
        );
        assert_eq!(
            add(&mut log, "@120", "Next.", CommandSource::Rollout),
            Some(2)
        );
        assert_eq!(log.len(), 3);
        // A fallback of another conversation is not this one's line.
        log.add(Some("c2"), "stop-3", None, "Same", CommandSource::Hook);
        assert_eq!(
            add(&mut log, "u5", "Same", CommandSource::Transcript),
            Some(4)
        );
    }
}
