//! Matching: a compiled query ([`Matcher`]) scanned over a corpus of
//! units joined by newlines, and the snippets shown around a hit.

use super::{
    BEFORE_CHARS, CLOCK_EVERY, MATCH_CHARS, MAX_QUERY_BYTES, REGEX_SIZE_LIMIT, SNIPPET_CHARS,
};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::VecDeque;
use std::time::Instant;

/// A compiled query.
#[derive(Debug, Clone)]
pub struct Matcher {
    re: Regex,
}

impl Matcher {
    /// `query` as a literal (`regex` false) or a `regex`-crate pattern;
    /// case-insensitive unless `case_sensitive`. `^` and `$` match at line
    /// boundaries.
    pub fn new(query: &str, regex: bool, case_sensitive: bool) -> Result<Self, String> {
        if query.is_empty() {
            return Err("the query is empty".into());
        }
        if query.len() > MAX_QUERY_BYTES {
            return Err(format!("the query is longer than {MAX_QUERY_BYTES} bytes"));
        }
        let pattern = if regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| match e {
                regex::Error::CompiledTooBig(_) => format!(
                    "the pattern is too large (compiled over {} KiB); make it simpler",
                    REGEX_SIZE_LIMIT / 1024
                ),
                other => format!("invalid regex: {other}"),
            })?;
        Ok(Self { re })
    }

    /// The non-empty matches in `text`, as byte ranges.
    pub fn find_iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.re
            .find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| (m.start(), m.end()))
    }

    /// The first non-empty match in `text` starting at or after byte `at`
    /// (anchors and word boundaries see the text before `at`).
    pub fn find_at(&self, text: &str, mut at: usize) -> Option<(usize, usize)> {
        while at <= text.len() {
            let m = self.re.find_at(text, at)?;
            if !m.is_empty() {
                return Some((m.start(), m.end()));
            }
            // Step over the empty match by one character.
            at = m.start() + text[m.start()..].chars().next()?.len_utf8();
        }
        None
    }

    /// Whether `text` has a non-empty match.
    pub fn is_match(&self, text: &str) -> bool {
        self.find_iter(text).next().is_some()
    }
}

/// Units of text (lines, messages, blocks) joined by `\n`, scanned with
/// one regex pass.
#[derive(Debug, Default, Clone)]
pub(crate) struct Corpus {
    pub(crate) text: String,
    /// Where each unit starts in `text`.
    pub(crate) starts: Vec<usize>,
}

impl Corpus {
    pub(crate) fn from_units<I: IntoIterator<Item = S>, S: AsRef<str>>(units: I) -> Self {
        let mut corpus = Self::default();
        for unit in units {
            corpus.push(unit.as_ref());
        }
        corpus
    }

    pub(crate) fn push(&mut self, unit: &str) {
        if !self.starts.is_empty() {
            self.text.push('\n');
        }
        self.starts.push(self.text.len());
        self.text.push_str(unit);
    }

    pub(crate) fn len(&self) -> usize {
        self.starts.len()
    }

    fn end_of(&self, unit: usize) -> usize {
        self.starts
            .get(unit + 1)
            .map_or(self.text.len(), |next| next - 1)
    }

    pub(crate) fn unit(&self, unit: usize) -> &str {
        &self.text[self.starts[unit]..self.end_of(unit)]
    }

    /// The unit holding byte `offset` (a separator belongs to the unit
    /// before it).
    pub(crate) fn unit_of(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1)
    }

    /// Drop the first `n` units.
    pub(crate) fn drop_front(&mut self, n: usize) {
        if n >= self.starts.len() {
            self.text.clear();
            self.starts.clear();
            return;
        }
        let cut = self.starts[n];
        self.text.drain(..cut);
        self.starts.drain(..n);
        for start in &mut self.starts {
            *start -= cut;
        }
    }
}

/// A matched unit: its index and its first match (bytes within the unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnitHit {
    pub(crate) unit: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// Matches in the unit.
    pub(crate) matches: usize,
}

/// What [`scan`] found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// The last `keep` matched units, oldest first.
    pub(crate) hits: VecDeque<UnitHit>,
    /// Units with a match (all of them, not only those kept).
    pub(crate) units: usize,
    /// The whole text was scanned (the budget did not run out).
    pub(crate) complete: bool,
}

/// Scan `corpus`, keeping the last `keep` matched units.
///
/// A match never runs across units: one that would (a pattern with `\n`
/// or `\s` spanning the separator) is retried inside its unit alone, so
/// what is reported is a match the unit itself holds.
pub(crate) fn scan(corpus: &Corpus, matcher: &Matcher, keep: usize, deadline: Instant) -> Scan {
    let mut found = Scan {
        complete: true,
        ..Scan::default()
    };
    let mut last: Option<UnitHit> = None;
    let mut record = |found: &mut Scan, unit: usize, start: usize, end: usize| {
        if let Some(hit) = last.as_mut()
            && hit.unit == unit
        {
            hit.matches += 1;
            return;
        }
        if let Some(done) = last.take() {
            keep_last(&mut found.hits, done, keep);
        }
        let base = corpus.starts[unit];
        last = Some(UnitHit {
            unit,
            start: start - base,
            end: end - base,
            matches: 1,
        });
        found.units += 1;
    };
    let text = corpus.text.as_str();
    let mut pos = 0;
    let mut steps = 0usize;
    while pos <= text.len() {
        steps += 1;
        if steps.is_multiple_of(CLOCK_EVERY) && Instant::now() >= deadline {
            found.complete = false;
            break;
        }
        let Some((start, end)) = matcher.find_at(text, pos) else {
            break;
        };
        let unit = corpus.unit_of(start);
        let unit_end = corpus.end_of(unit);
        if end <= unit_end {
            record(&mut found, unit, start, end);
            pos = end;
            continue;
        }
        // The match crosses into the next unit: look again inside this one.
        let mut inner = start;
        while inner < unit_end {
            let Some((s, e)) = matcher.find_at(&text[..unit_end], inner) else {
                break;
            };
            record(&mut found, unit, s, e);
            inner = e;
        }
        pos = unit_end + 1;
    }
    if let Some(done) = last {
        keep_last(&mut found.hits, done, keep);
    }
    found
}

fn keep_last(hits: &mut VecDeque<UnitHit>, hit: UnitHit, keep: usize) {
    if keep == 0 {
        return;
    }
    if hits.len() == keep {
        hits.pop_front();
    }
    hits.push_back(hit);
}

/// A match in context, on one line: `before` + `match` + `after`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snippet {
    /// Up to [`BEFORE_CHARS`] characters before the match, `…`-prefixed
    /// when cut.
    pub before: String,
    #[serde(rename = "match")]
    pub matched: String,
    /// The rest of the line, `…`-suffixed when cut.
    pub after: String,
    /// 0-based line of the match within its unit (a message or block).
    pub line: usize,
}

/// Control characters (tabs, newlines, escapes) shown as spaces.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The last `n` characters of `text`, and whether it was cut.
fn tail_chars(text: &str, n: usize) -> (&str, bool) {
    match text.char_indices().rev().nth(n.saturating_sub(1)) {
        Some((i, _)) if n > 0 && i > 0 => (&text[i..], true),
        _ if n == 0 => ("", !text.is_empty()),
        _ => (text, false),
    }
}

/// The first `n` characters of `text`, and whether it was cut.
pub(crate) fn head_chars(text: &str, n: usize) -> (&str, bool) {
    match text.char_indices().nth(n) {
        Some((i, _)) => (&text[..i], true),
        None => (text, false),
    }
}

/// The snippet of `unit`'s match at bytes `start..end`.
pub fn snippet(unit: &str, start: usize, end: usize) -> Snippet {
    let line_start = unit[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = unit[start..].find('\n').map_or(unit.len(), |i| start + i);
    // A match running past its line is shown up to the line's end (one
    // that starts on a newline, as itself).
    let end = if end.min(line_end) > start {
        end.min(line_end)
    } else {
        end
    };
    let (before, cut_before) = tail_chars(&unit[line_start..start], BEFORE_CHARS);
    let before = if cut_before {
        format!("…{}", clean(before))
    } else {
        clean(before).trim_start().to_owned()
    };
    let (matched, cut_match) = head_chars(&unit[start..end], MATCH_CHARS);
    let mut matched = clean(matched);
    if cut_match {
        matched.push('…');
    }
    let room = SNIPPET_CHARS
        .saturating_sub(before.chars().count() + matched.chars().count())
        .max(40);
    let rest = if end < line_end {
        &unit[end..line_end]
    } else {
        ""
    };
    let (after, cut_after) = head_chars(rest, room);
    let mut after = clean(after).trim_end().to_owned();
    if cut_after {
        after.push('…');
    }
    Snippet {
        before,
        matched,
        after,
        line: unit[..start].matches('\n').count(),
    }
}
