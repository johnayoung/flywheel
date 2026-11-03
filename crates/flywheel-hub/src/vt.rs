//! Server-side terminal emulation for one session.
//!
//! Every byte a session's PTY emits is fed through an [`Emulator`] (an
//! `alacritty_terminal` grid + parser) before it is broadcast, so the server
//! always knows what the terminal looks like: both screens, scrollback,
//! cursor, SGR pen, DEC modes, title, palette. Attaching clients get a
//! [`Emulator::snapshot`] -- escape sequences that rebuild that state in a
//! freshly reset xterm-compatible terminal -- instead of a raw byte replay,
//! which loses modes set before the ring's horizon, can start inside an
//! escape sequence, and re-runs terminal queries.
//!
//! The emulator is also the session's only answerer of terminal queries
//! (device attributes, cursor position, mode and colour reports; see
//! [`Emulator::take_replies`]): a detached session still gets its replies,
//! and two viewers do not answer twice. Clients must not answer them.
//!
//! Output is untrusted, so everything it can make the emulator retain is
//! bounded (see [`Guarded`]): OSC strings, titles and the title stack,
//! hyperlinks, combining marks per cell. The kitty keyboard protocol is not
//! emulated: the browser terminal (xterm.js 6) does not implement it, so
//! reporting support would make programs send keys it cannot encode.
//!
//! Two pieces of state `alacritty_terminal` keeps private -- the scroll
//! region and the SI/SO active charset -- are mirrored by a second, tiny
//! `vte` parser that applies the same rules (see [`Tracked`]).
//!
//! Known gaps: custom tab stops are not replayed (rarely changed; apps that
//! set them re-set them); `?47`/`?1047` alternate screens are not emulated
//! (`alacritty_terminal` implements `?1049`, which is what `xterm-256color`
//! terminfo emits); blink (SGR 5) and overline (SGR 53) are not stored, so
//! a snapshot drops them; DECRQSS is not answered.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte;
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor, Processor, StdSyncHandler};
use serde::Serialize;
use std::io::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

mod colors;
mod guard;
mod snapshot;
mod text;

pub use colors::ViewerColors;
pub use text::{Lines, SearchText};

use guard::{Guarded, OscScan, Tracked};
use snapshot::Writer;
use text::{
    HistoryText, HistoryWatch, recent_lines, row_text, sanitize, update_history_text, wrapped,
};

/// Default scrollback retained per session, in lines.
pub const DEFAULT_SCROLLBACK_LINES: usize = 5000;
/// Upper bound on retained history cells (lines x columns) per session, so a
/// very wide terminal keeps fewer lines rather than more memory. A cell is
/// 24 bytes, plus a per-cell allocation only for cells carrying combining
/// marks (at most [`MAX_ZEROWIDTH`]) or a non-default underline colour.
pub const HISTORY_CELL_BUDGET: usize = 1_000_000;
/// Default `lines` for [`TextSource::Recent`].
pub const DEFAULT_RECENT_LINES: usize = 100;
/// Default byte budget for the scrollback part of a [`Emulator::snapshot`]
/// (the screen itself is always included): the oldest whole lines beyond it
/// are left out, so attaching costs a bounded transfer on any link.
pub const SNAPSHOT_HISTORY_BYTES: usize = 2 * 1024 * 1024;
/// Combining (zero-width) characters kept per cell; more are dropped (xterm
/// keeps at most 10).
pub const MAX_ZEROWIDTH: usize = 8;
/// Longest OSC string the parsers buffer; the rest of a longer one is not
/// fed to them (the sequence still ends at its terminator).
const MAX_OSC_BYTES: usize = 64 * 1024;
/// Longest title kept, in bytes.
const MAX_TITLE_BYTES: usize = 4096;
/// Titles kept by `CSI 22 t` (xterm keeps 10).
const TITLE_STACK_DEPTH: usize = 10;
/// Longest hyperlink URI / id kept; longer links are dropped (the text stays).
const MAX_LINK_URI_BYTES: usize = 2048;
const MAX_LINK_ID_BYTES: usize = 256;

/// Flags that change how a cell is drawn (everything but layout markers).
const STYLE_FLAGS: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);
/// Flags under which even a blank cell is visible.
const VISIBLE_BLANK_FLAGS: Flags = Flags::INVERSE
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::STRIKEOUT);

#[derive(Debug, Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Receives the emulator's events: the window title, and replies to
/// terminal queries (`PtyWrite`), collected for [`Emulator::take_replies`].
/// Clipboard and bell events are the clients' business (they see the same
/// bytes live); clipboard reads are never answered.
#[derive(Clone, Default)]
struct Listener {
    title: Arc<Mutex<Option<String>>>,
    replies: Arc<Mutex<Vec<u8>>>,
}

impl Listener {
    fn title(&self) -> Option<String> {
        self.title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn reply(&self, text: &str) {
        self.replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(text.as_bytes());
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(t) => {
                *self.title.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
            }
            Event::ResetTitle => {
                *self.title.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
            Event::PtyWrite(text) => self.reply(&text),
            _ => {}
        }
    }
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn truncated(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Names of the modes [`ScreenText::modes`] reports.
pub const MODE_NAMES: [(TermMode, &str); 11] = [
    (TermMode::APP_CURSOR, "app_cursor"),
    (TermMode::APP_KEYPAD, "app_keypad"),
    (TermMode::BRACKETED_PASTE, "bracketed_paste"),
    (TermMode::MOUSE_REPORT_CLICK, "mouse_click"),
    (TermMode::MOUSE_DRAG, "mouse_drag"),
    (TermMode::MOUSE_MOTION, "mouse_motion"),
    (TermMode::SGR_MOUSE, "sgr_mouse"),
    (TermMode::UTF8_MOUSE, "utf8_mouse"),
    (TermMode::FOCUS_IN_OUT, "focus_events"),
    (TermMode::INSERT, "insert"),
    (TermMode::ORIGIN, "origin"),
];

/// Which rendered text [`Emulator::text`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSource {
    /// The rows on screen now (the active screen, top to bottom).
    Visible,
    /// The last N logical lines (soft-wrapped rows joined) of the active
    /// screen's scrollback plus screen, trailing blank lines dropped.
    Recent { lines: usize },
}

/// Cursor facts for [`ScreenText`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorView {
    /// 0-based screen row.
    pub row: usize,
    /// 0-based column.
    pub col: usize,
    pub visible: bool,
}

/// Rendered plain text of a session's terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenText {
    pub cols: usize,
    pub rows: usize,
    pub alt_screen: bool,
    pub title: Option<String>,
    pub cursor: CursorView,
    /// Input-affecting modes the program has set (see [`MODE_NAMES`]).
    pub modes: Vec<&'static str>,
    /// Trailing whitespace trimmed.
    pub lines: Vec<String>,
}

/// One session's terminal state.
pub struct Emulator {
    term: Term<Listener>,
    parser: Processor<StdSyncHandler>,
    tracker: vte::Parser,
    tracked: Tracked,
    listener: Listener,
    /// The title stack (see [`Guarded`]).
    titles: Vec<Option<String>>,
    /// What colour queries answer where the program set no colour.
    colors: ViewerColors,
    osc: OscScan,
    config: Config,
    scrollback_lines: usize,
    history: HistoryWatch,
    history_text: HistoryText,
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.term.columns())
            .field("rows", &self.term.screen_lines())
            .field("history", &self.history_lines())
            .finish_non_exhaustive()
    }
}

fn history_limit(scrollback_lines: usize, cols: usize) -> usize {
    scrollback_lines.min(HISTORY_CELL_BUDGET / cols.max(1))
}

impl Emulator {
    /// A blank `cols` x `rows` terminal keeping up to `scrollback_lines`
    /// lines of history (fewer on very wide terminals, see
    /// [`HISTORY_CELL_BUDGET`]).
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        let size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        let config = Config {
            scrolling_history: history_limit(scrollback_lines, size.cols),
            // Not emulated: see the module docs.
            kitty_keyboard: false,
            ..Config::default()
        };
        let listener = Listener::default();
        Self {
            term: Term::new(config.clone(), &size, listener.clone()),
            parser: Processor::new(),
            tracker: vte::Parser::new(),
            tracked: Tracked::new(size.rows),
            listener,
            titles: Vec::new(),
            colors: ViewerColors::default(),
            osc: OscScan::default(),
            config,
            scrollback_lines,
            history: HistoryWatch::default(),
            history_text: HistoryText::default(),
        }
    }

    /// Set the colours that colour queries answer where the program set
    /// none (the attached viewer's theme).
    pub fn set_viewer_colors(&mut self, colors: ViewerColors) {
        self.colors = colors;
    }

    /// Feed PTY output. Replies to any terminal queries in it are queued
    /// for [`Emulator::take_replies`].
    pub fn process(&mut self, bytes: &[u8]) {
        let mut run = None;
        for (i, &byte) in bytes.iter().enumerate() {
            match (self.osc.keep(byte), run) {
                (true, None) => run = Some(i),
                (false, Some(start)) => {
                    self.feed(&bytes[start..i]);
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(start) = run {
            self.feed(&bytes[start..]);
        }
        // A synchronized update (`CSI ? 2026 h`) buffers output until its
        // end or a timeout; nothing else drives the timeout here.
        if self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.parser.stop_sync(&mut Guarded {
                term: &mut self.term,
                titles: &mut self.titles,
                listener: &self.listener,
                colors: &self.colors,
                history: &mut self.history,
            });
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.tracker.advance(&mut self.tracked, bytes);
        self.parser.advance(
            &mut Guarded {
                term: &mut self.term,
                titles: &mut self.titles,
                listener: &self.listener,
                colors: &self.colors,
                history: &mut self.history,
            },
            bytes,
        );
    }

    /// Bytes the terminal answers queries with (device attributes, cursor
    /// position, mode and colour reports), to be written to the program's
    /// input, in order. Drained by the call.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(
            &mut *self
                .listener
                .replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Apply output buffered by an unfinished synchronized update: it has
    /// already been sent to live clients, so it is part of the state.
    fn flush(&mut self) {
        if self.parser.sync_bytes_count() > 0 || self.parser.sync_timeout().sync_timeout().is_some()
        {
            self.parser.stop_sync(&mut Guarded {
                term: &mut self.term,
                titles: &mut self.titles,
                listener: &self.listener,
                colors: &self.colors,
                history: &mut self.history,
            });
        }
    }

    /// Resize (reflowing the primary screen) and re-derive the history
    /// bound for the new width.
    ///
    /// Growing taller follows xterm.js (the browser's terminal), so the
    /// rows here stay the rows a viewer sees: scrollback is pulled back
    /// into view only while the cursor is on the bottom row; otherwise the
    /// new rows are blank, below. (`Term` always pulls scrollback, which
    /// put old lines -- an answered approval, a stale busy line -- on this
    /// screen but not on the viewer's, shifting every row number.)
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.flush();
        let size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        let (old_cols, old_rows) = self.size();
        if size.rows > old_rows && !self.alt_screen() {
            let grid = self.term.grid();
            let at_bottom = grid.cursor.point.line.0 + 1 >= old_rows as i32;
            let pulled = grid.history_size().min(size.rows - old_rows);
            // Rows first, as `Term` itself does (then columns, below).
            self.term.resize(Size {
                cols: old_cols,
                rows: size.rows,
            });
            if !at_bottom && pulled > 0 {
                let grid = self.term.grid_mut();
                // Blank rows, like `Term`'s own resize (not the pen's colors).
                let template = std::mem::take(&mut grid.cursor.template);
                grid.scroll_up(&(Line(0)..Line(size.rows as i32)), pulled);
                grid.cursor.template = template;
                let back = pulled as i32;
                grid.cursor.point.line = Line((grid.cursor.point.line.0 - back).max(0));
                grid.saved_cursor.point.line = Line((grid.saved_cursor.point.line.0 - back).max(0));
            }
        }
        self.term.resize(size);
        self.tracked.resize(size.rows);
        // Reflowed (and maybe pulled back into view or cut): read afresh.
        self.history.epoch += 1;
        let limit = history_limit(self.scrollback_lines, size.cols);
        if limit != self.config.scrolling_history {
            self.config.scrolling_history = limit;
            self.term.set_options(self.config.clone());
        }
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// Lines of scrollback retained by the active screen (the alternate
    /// screen keeps none).
    pub fn history_lines(&self) -> usize {
        self.term.grid().history_size()
    }

    /// The window title the program set (OSC 0/2), if any.
    pub fn title(&self) -> Option<String> {
        self.listener.title()
    }

    /// Whether the alternate screen is up.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether the program enabled bracketed paste (`CSI ? 2004 h`).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Escape sequences that rebuild this terminal in a freshly reset
    /// xterm-compatible one of the same size: the primary screen's
    /// scrollback and rows, then (when up) the alternate screen, then the
    /// active screen's cursor, saved cursor, charsets, scroll region, SGR
    /// pen, DEC modes, cursor style, title, and palette changes.
    ///
    /// Scrollback is included newest first, in whole logical lines, while
    /// it fits in `history_budget` bytes; the screen always is.
    pub fn snapshot(&mut self, history_budget: usize) -> Vec<u8> {
        self.flush();
        let mut w = Writer::default();
        // RIS: a client that did not reset itself starts clean too.
        w.raw(b"\x1bc");
        if self.alt_screen() {
            // The primary grid is private while the alternate screen is up.
            // Leaving the alternate screen is a plain swap; re-entering
            // resets the alternate grid (restored from the copy) and sets
            // the primary's saved cursor to its cursor -- which entering did
            // already and nothing changed since: an inactive grid is only
            // touched by resize, which clamps both alike.
            let alt = self.term.grid().clone();
            self.term.swap_alt();
            let primary = self.term.grid();
            w.rows(primary, primary.topmost_line(), history_budget);
            w.cup(primary.cursor.point);
            w.pen(&primary.cursor.template);
            self.term.swap_alt();
            *self.term.grid_mut() = alt;
            // Save the primary cursor, switch to a cleared alternate screen.
            w.raw(b"\x1b[?1049h\x1b[H");
            w.plain();
            w.rows(self.term.grid(), Line(0), 0);
        } else {
            let grid = self.term.grid();
            w.rows(grid, grid.topmost_line(), history_budget);
        }
        self.write_state(&mut w);
        w.out
    }

    /// Escape sequences that put a terminal mirroring this one (same
    /// screen contents) back into this one's cursor, saved cursor,
    /// charsets, scroll region, SGR pen, and DEC modes -- after a client
    /// drew over it (`fw hub attach` drawing its status bar). Ends showing
    /// the cursor when the program shows it.
    pub fn restore_sequence(&mut self) -> Vec<u8> {
        self.flush();
        let mut w = Writer::default();
        self.write_state(&mut w);
        if self.term.mode().contains(TermMode::SHOW_CURSOR) {
            w.raw(b"\x1b[?25h");
        }
        w.out
    }

    fn write_state(&self, w: &mut Writer) {
        let grid = self.term.grid();
        let mode = *self.term.mode();
        let cols = grid.columns();

        // Saved cursor (DECSC: position, pen, charsets).
        let saved = &grid.saved_cursor;
        w.cup(saved.point);
        w.pen(&saved.template);
        w.charsets(&saved.charsets);
        w.raw(b"\x1b7");
        // Cells hold mapped characters: re-print them through ASCII.
        w.charsets(&alacritty_terminal::grid::Charsets::default());

        let (top, bottom) = self.tracked.region;
        if (top, bottom) != (0, grid.screen_lines()) {
            let _ = write!(w.out, "\x1b[{};{}r", top + 1, bottom);
        }
        let origin = mode.contains(TermMode::ORIGIN);
        if origin {
            w.raw(b"\x1b[?6h");
        }
        let cursor = grid.cursor.point;
        let row_base = if origin { top } else { 0 };
        let cursor_row = usize::try_from(cursor.line.0)
            .unwrap_or(0)
            .saturating_sub(row_base);
        if grid.cursor.input_needs_wrap && cursor.column.0 + 1 == cols {
            // Re-print the last cell so the client is in the same
            // pending-wrap state.
            let row = &grid[cursor.line];
            let mut col = cursor.column.0;
            if row[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) && col > 0 {
                col -= 1;
            }
            let _ = write!(w.out, "\x1b[{};{}H", cursor_row + 1, col + 1);
            w.cell(&row[Column(col)]);
        } else {
            let _ = write!(w.out, "\x1b[{};{}H", cursor_row + 1, cursor.column.0 + 1);
        }
        w.charsets(&grid.cursor.charsets);
        if self.tracked.shifted_out {
            w.raw(b"\x0e");
        }
        w.pen(&grid.cursor.template);
        w.link(grid.cursor.template.hyperlink());

        // Modes (only those differing from a reset terminal).
        let decset = [
            (TermMode::APP_CURSOR, 1),
            (TermMode::MOUSE_REPORT_CLICK, 1000),
            (TermMode::MOUSE_DRAG, 1002),
            (TermMode::MOUSE_MOTION, 1003),
            (TermMode::FOCUS_IN_OUT, 1004),
            (TermMode::UTF8_MOUSE, 1005),
            (TermMode::SGR_MOUSE, 1006),
            (TermMode::BRACKETED_PASTE, 2004),
        ];
        for (flag, number) in decset {
            if mode.contains(flag) {
                let _ = write!(w.out, "\x1b[?{number}h");
            }
        }
        if !mode.contains(TermMode::SHOW_CURSOR) {
            w.raw(b"\x1b[?25l");
        }
        if !mode.contains(TermMode::LINE_WRAP) {
            w.raw(b"\x1b[?7l");
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            w.raw(b"\x1b=");
        }
        if mode.contains(TermMode::INSERT) {
            w.raw(b"\x1b[4h");
        }
        if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            w.raw(b"\x1b[20h");
        }

        let style = self.term.cursor_style();
        if style != self.config.default_cursor_style {
            let base = match style.shape {
                CursorShape::Underline => 3,
                CursorShape::Beam => 5,
                CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 1,
            };
            let code = if style.blinking { base } else { base + 1 };
            let _ = write!(w.out, "\x1b[{code} q");
        }

        if let Some(title) = self.title() {
            w.raw(b"\x1b]2;");
            w.raw(sanitize(&title).as_bytes());
            w.raw(b"\x07");
        }

        let colors = self.term.colors();
        for index in 0..256 {
            if let Some(rgb) = colors[index] {
                let _ = write!(
                    w.out,
                    "\x1b]4;{index};rgb:{:02x}/{:02x}/{:02x}\x07",
                    rgb.r, rgb.g, rgb.b
                );
            }
        }
        for (named, osc) in [
            (NamedColor::Foreground, 10),
            (NamedColor::Background, 11),
            (NamedColor::Cursor, 12),
        ] {
            if let Some(rgb) = colors[named] {
                let _ = write!(
                    w.out,
                    "\x1b]{osc};rgb:{:02x}/{:02x}/{:02x}\x07",
                    rgb.r, rgb.g, rgb.b
                );
            }
        }
    }

    /// Rendered plain text of the active screen.
    pub fn text(&mut self, source: TextSource) -> ScreenText {
        self.flush();
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let lines = match source {
            TextSource::Visible => (0..rows)
                .map(|r| row_text(grid, Line(r as i32)).trim_end().to_owned())
                .collect(),
            TextSource::Recent { lines } => recent_lines(grid, lines),
        };
        let cursor = grid.cursor.point;
        ScreenText {
            cols: grid.columns(),
            rows,
            alt_screen: self.alt_screen(),
            title: self.title(),
            cursor: CursorView {
                row: usize::try_from(cursor.line.0).unwrap_or(0),
                col: cursor.column.0,
                visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            },
            modes: MODE_NAMES
                .iter()
                .filter(|(flag, _)| self.term.mode().contains(*flag))
                .map(|(_, name)| *name)
                .collect(),
            lines,
        }
    }

    /// Every logical line the terminal retains, for search: the primary
    /// screen's scrollback plus rows (also while the alternate screen is
    /// up), oldest first, trailing blank lines dropped -- the same lines,
    /// counted from the bottom, a client replaying [`Emulator::snapshot`]
    /// holds -- and the alternate screen's rows when it is up.
    ///
    /// History rows read by an earlier call are reused (output only adds
    /// rows at the bottom of history and drops them at the top), so a
    /// session printing steadily costs the rows it printed, not its whole
    /// scrollback, per search.
    pub fn search_text(&mut self) -> SearchText {
        self.flush();
        if !self.alt_screen() {
            return SearchText {
                primary: self.primary_lines(),
                alternate: None,
            };
        }
        // The primary grid is private while the alternate screen is up:
        // swap to it and back, as `snapshot` does.
        let alt = self.term.grid().clone();
        let alternate = recent_lines(&alt, usize::MAX);
        self.term.swap_alt();
        let primary = self.primary_lines();
        self.term.swap_alt();
        *self.term.grid_mut() = alt;
        SearchText {
            primary,
            alternate: Some(alternate),
        }
    }

    /// [`recent_lines`] of the primary grid (the active one when called),
    /// through [`HistoryText`].
    fn primary_lines(&mut self) -> Lines {
        let limit = self.config.scrolling_history;
        let grid = self.term.grid();
        let cache = &mut self.history_text;
        update_history_text(cache, grid, self.history, limit);
        let mut out = cache.lines.clone();
        let mut pending = cache.open.concat();
        let mut open = !cache.open.is_empty();
        for line in 0..grid.screen_lines() as i32 {
            let line = Line(line);
            pending.push_str(&row_text(grid, line));
            open = true;
            if !wrapped(grid, line) {
                out.push(pending.trim_end());
                pending.clear();
                open = false;
            }
        }
        if open {
            out.push(pending.trim_end());
        }
        // Blank rows below the last output (back into history, after a
        // clear).
        while out.last().is_some_and(str::is_empty) {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests;
