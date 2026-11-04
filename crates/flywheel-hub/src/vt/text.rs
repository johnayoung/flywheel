//! Rendered text: rows, recent lines, and the search text with its cache
//! of the primary screen's history.

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

/// What may have changed the primary screen's history, counted where the
/// handler sees it, so [`Emulator::search_text`] can reuse the history rows
/// it read last time (history rows never change once there; they only
/// scroll in at the bottom and out at the top).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HistoryWatch {
    /// Bumped where history may be rewritten or dropped (reset, ED 3, a
    /// resize): rows read before it are not reused.
    pub(crate) epoch: u64,
    /// An upper bound on rows scrolled into history: every linefeed, wrap,
    /// scroll and screen clear counts, also off the bottom of a scroll
    /// region or on the alternate screen (where nothing scrolls).
    scrolls: u64,
}

impl HistoryWatch {
    pub(crate) fn scrolled(&mut self, rows: usize) {
        self.scrolls = self.scrolls.saturating_add(rows as u64);
    }

    /// The upper bound on rows scrolled so far (see the field).
    pub(crate) fn scrolls(&self) -> u64 {
        self.scrolls
    }
}

/// The primary screen's history as search text, kept between
/// [`Emulator::search_text`] calls: only rows scrolled in since are read.
#[derive(Debug, Default)]
pub(crate) struct HistoryText {
    /// The logical lines wholly in history (trimmed), oldest first.
    pub(crate) lines: Lines,
    /// How many rows each of `lines` spans.
    line_rows: std::collections::VecDeque<usize>,
    /// The rows of the logical line still open at the bottom of history
    /// (each soft-wraps into the next; it ends on screen or later).
    pub(crate) open: Vec<String>,
    /// Every history row's fingerprint (text and wrap), oldest first:
    /// `line_rows` plus `open` rows.
    rows: std::collections::VecDeque<u64>,
    /// The newest row's cell storage (rows move through history as
    /// buffers, so this finds where it went).
    anchor: usize,
    /// The [`HistoryWatch`] when read, and the grid size.
    watch: HistoryWatch,
    size: (usize, usize),
    /// Rows read by the last update (the test of the reuse).
    pub(crate) read: usize,
}

impl HistoryText {
    fn clear(&mut self) {
        self.lines = Lines::default();
        self.line_rows.clear();
        self.open.clear();
        self.rows.clear();
    }

    /// Add history row `line` (the newest so far).
    fn push_row(&mut self, grid: &Grid<Cell>, line: Line) {
        let (text, wraps) = (row_text(grid, line), wrapped(grid, line));
        self.rows.push_back(fingerprint(&text, wraps));
        self.open.push(text);
        if !wraps {
            let joined = self.open.concat();
            self.lines.push(joined.trim_end());
            self.line_rows.push_back(self.open.len());
            self.open.clear();
        }
        self.read += 1;
    }

    /// Forget the `n` oldest rows (scrolled out of history). A logical line
    /// losing only some of its rows is read again from what is left.
    fn drop_rows(&mut self, grid: &Grid<Cell>, mut n: usize) {
        self.rows.drain(..n.min(self.rows.len()));
        // Whole lines first, in one cut.
        let mut whole = 0;
        while let Some(&first) = self.line_rows.get(whole)
            && first <= n
        {
            n -= first;
            whole += 1;
        }
        self.lines.drop_front(whole);
        self.line_rows.drain(..whole);
        if n > 0 {
            if self.line_rows.is_empty() {
                self.open.drain(..n.min(self.open.len()));
                return;
            }
            let first = self.line_rows[0];
            let left = first - n;
            let top = grid.topmost_line();
            let text: String = (0..left as i32).map(|i| row_text(grid, top + i)).collect();
            self.lines.replace_front(text.trim_end());
            self.line_rows[0] = left;
            self.read += left;
        }
    }
}

pub(crate) fn fingerprint(text: &str, wraps: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    wraps.hash(&mut hasher);
    hasher.finish()
}
/// The address of a row's cells: stable while the row moves through the
/// grid's ring (rows are swapped and rotated, not copied).
pub(crate) fn row_address(grid: &Grid<Cell>, line: Line) -> usize {
    std::ptr::from_ref(&grid[line][Column(0)]) as usize
}

/// Bring `cache` up to `grid`'s history: reuse what was read before when
/// its newest row is found again (and its oldest kept row checks out),
/// reading only the rows that scrolled in since; else read them all.
pub(crate) fn update_history_text(
    cache: &mut HistoryText,
    grid: &Grid<Cell>,
    watch: HistoryWatch,
    limit: usize,
) {
    let history = grid.history_size();
    let size = (grid.columns(), grid.screen_lines());
    let shift = (cache.watch.epoch == watch.epoch && cache.size == size)
        .then(|| history_shift(cache, grid, watch.scrolls - cache.watch.scrolls, limit))
        .flatten();
    cache.read = 0;
    let scrolled = match shift {
        Some(scrolled) => {
            let dropped = cache.rows.len() + scrolled - history;
            cache.drop_rows(grid, dropped);
            scrolled
        }
        None => {
            cache.clear();
            history
        }
    };
    for back in (1..=scrolled).rev() {
        cache.push_row(grid, Line(-(back as i32)));
    }
    cache.anchor = if history > 0 {
        row_address(grid, Line(-1))
    } else {
        0
    };
    cache.watch = watch;
    cache.size = size;
}

/// Rows scrolled into history since `cache` was read, when that can be
/// told: at most `bound`, and fewer than history holds (past that a
/// recycled row could be taken for the anchor).
fn history_shift(
    cache: &HistoryText,
    grid: &Grid<Cell>,
    bound: u64,
    limit: usize,
) -> Option<usize> {
    let history = grid.history_size();
    if cache.rows.is_empty() {
        // Nothing was in history: everything there now is new.
        return Some(history);
    }
    if bound >= limit as u64 || history == 0 {
        return None;
    }
    let bound = usize::try_from(bound).unwrap_or(usize::MAX);
    let scrolled = (0..=bound.min(history - 1))
        .find(|&s| row_address(grid, Line(-1 - s as i32)) == cache.anchor)?;
    let dropped = (cache.rows.len() + scrolled).checked_sub(history)?;
    // The newest row kept and the oldest kept must read as they did.
    let reads_as = |line: Line, expected: Option<&u64>| {
        expected.is_some_and(|&e| fingerprint(&row_text(grid, line), wrapped(grid, line)) == e)
    };
    if !reads_as(Line(-1 - scrolled as i32), cache.rows.back()) {
        return None;
    }
    if dropped < cache.rows.len() && !reads_as(grid.topmost_line(), cache.rows.get(dropped)) {
        return None;
    }
    Some(scrolled)
}

/// Logical lines joined by `\n`, with where each starts: the form search
/// scans (one regex pass over `text`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lines {
    pub text: String,
    /// Where each line starts in `text`.
    pub starts: Vec<usize>,
}

impl Lines {
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Line `i`.
    pub fn get(&self, i: usize) -> Option<&str> {
        let start = *self.starts.get(i)?;
        let end = self
            .starts
            .get(i + 1)
            .map_or(self.text.len(), |next| next - 1);
        Some(&self.text[start..end])
    }

    pub fn last(&self) -> Option<&str> {
        self.len().checked_sub(1).and_then(|i| self.get(i))
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    pub fn push(&mut self, line: &str) {
        if !self.starts.is_empty() {
            self.text.push('\n');
        }
        self.starts.push(self.text.len());
        self.text.push_str(line);
    }

    /// Drop the last line.
    pub fn pop(&mut self) {
        let Some(start) = self.starts.pop() else {
            return;
        };
        // With its separator.
        self.text.truncate(start.saturating_sub(1));
        if self.starts.is_empty() {
            self.text.clear();
        }
    }

    /// Drop the first `n` lines.
    pub fn drop_front(&mut self, n: usize) {
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

    /// Replace the first line.
    fn replace_front(&mut self, line: &str) {
        let Some(old) = self.get(0).map(str::len) else {
            self.push(line);
            return;
        };
        self.text.replace_range(..old, line);
        let delta = line.len() as isize - old as isize;
        for start in self.starts.iter_mut().skip(1) {
            *start = start.saturating_add_signed(delta);
        }
    }
}

impl std::ops::Index<usize> for Lines {
    type Output = str;

    fn index(&self, i: usize) -> &str {
        self.get(i).expect("line index in range")
    }
}

impl<S: AsRef<str>> FromIterator<S> for Lines {
    fn from_iter<I: IntoIterator<Item = S>>(lines: I) -> Self {
        let mut out = Self::default();
        for line in lines {
            out.push(line.as_ref());
        }
        out
    }
}

/// What [`Emulator::search_text`](super::Emulator::search_text) returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchText {
    /// The primary screen's logical lines: scrollback then rows.
    pub primary: Lines,
    /// The alternate screen's rows, when it is up.
    pub alternate: Option<Vec<String>>,
}

/// A title without control characters (it is replayed inside an OSC).
pub(crate) fn sanitize(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control())
        .take(4096)
        .collect()
}

pub(crate) fn wrapped(grid: &Grid<Cell>, line: Line) -> bool {
    grid[line][grid.last_column()]
        .flags
        .contains(Flags::WRAPLINE)
}

/// A row's characters (wide-char spacers skipped, tabs as spaces).
pub(crate) fn row_text(grid: &Grid<Cell>, line: Line) -> String {
    let row = &grid[line];
    let mut text = String::with_capacity(grid.columns());
    for col in 0..grid.columns() {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(printable(cell.c));
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth);
        }
    }
    text
}

/// The last `count` logical lines, newest last.
pub(crate) fn recent_lines(grid: &Grid<Cell>, count: usize) -> Vec<String> {
    let top = grid.topmost_line();
    let mut out: Vec<String> = Vec::new();
    let mut pieces: Vec<String> = Vec::new();
    let mut line = grid.bottommost_line();
    while line >= top && out.len() < count {
        pieces.push(row_text(grid, line));
        let starts_logical = line == top || !wrapped(grid, line - 1);
        if starts_logical {
            let joined: String = pieces.drain(..).rev().collect();
            let joined = joined.trim_end().to_owned();
            // Skip the blank rows below the last output.
            if !(out.is_empty() && joined.is_empty()) {
                out.push(joined);
            }
        }
        line -= 1;
    }
    out.reverse();
    out
}

pub(crate) fn printable(c: char) -> char {
    // A tab is stored in the cell it started at; the cells it skipped
    // follow, so replaying the tab itself would shift them.
    if c == '\t' || c.is_control() { ' ' } else { c }
}
