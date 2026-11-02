//! Server-side terminal emulation for one session.
//!
//! Every byte a session's PTY emits is fed through an [`Emulator`] (an
//! `alacritty_terminal` grid + parser) before it is broadcast, so the server
//! always knows what the terminal looks like: both screens, scrollback,
//! cursor, SGR pen, DEC modes, title, palette. Attaching clients get a
//! [`Emulator::snapshot`] -- escape sequences that rebuild that state in a
//! freshly reset xterm-compatible terminal -- instead of a raw byte replay,
//! which loses modes set before the ring's horizon, can start inside an
//! escape sequence, and re-runs terminal queries (making the client answer
//! them into the PTY a second time).
//!
//! Two pieces of state `alacritty_terminal` keeps private -- the scroll
//! region and the SI/SO active charset -- are mirrored by a second, tiny
//! `vte` parser that applies the same rules (see [`Tracked`]).
//!
//! Known gaps: custom tab stops are not replayed (rarely changed; apps that
//! set them re-set them), and `?47`/`?1047` alternate screens are not
//! emulated (`alacritty_terminal` implements `?1049`, which is what
//! `xterm-256color` terminfo emits).

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte;
use alacritty_terminal::vte::ansi::{
    CharsetIndex, Color, CursorShape, NamedColor, Processor, StandardCharset, StdSyncHandler,
};
use serde::Serialize;
use std::io::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Default scrollback retained per session, in lines.
pub const DEFAULT_SCROLLBACK_LINES: usize = 5000;
/// Upper bound on retained history cells (lines x columns) per session, so a
/// very wide terminal keeps fewer lines rather than more memory (a cell is
/// ~24 bytes: about 24 MiB of history at most).
pub const HISTORY_CELL_BUDGET: usize = 1_000_000;
/// Default `lines` for [`TextSource::Recent`].
pub const DEFAULT_RECENT_LINES: usize = 100;

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

/// Receives the emulator's events. Only the window title is kept: replies
/// to terminal queries (`PtyWrite`) are the attached clients' job -- they see
/// the same bytes live -- and clipboard/bell events are theirs too.
#[derive(Clone, Default)]
struct Listener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut title = self.title.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            Event::Title(t) => *title = Some(t),
            Event::ResetTitle => *title = None,
            _ => {}
        }
    }
}

/// State `alacritty_terminal` does not expose, tracked from the same byte
/// stream with its rules: DECSTBM (`CSI top;bottom r`, reset by RIS and by a
/// resize) and the SI/SO-selected charset (reset by RIS).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    rows: usize,
    /// Scroll region rows, 0-based, end exclusive.
    region: (usize, usize),
    shifted_out: bool,
}

impl Tracked {
    fn new(rows: usize) -> Self {
        Self {
            rows,
            region: (0, rows),
            shifted_out: false,
        }
    }

    fn resize(&mut self, rows: usize) {
        self.rows = rows;
        self.region = (0, rows);
    }
}

impl vte::Perform for Tracked {
    fn execute(&mut self, byte: u8) {
        match byte {
            0x0E => self.shifted_out = true,
            0x0F => self.shifted_out = false,
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore || action != 'r' || !intermediates.is_empty() {
            return;
        }
        let mut params = params.iter();
        let top = match params.next() {
            Some(&[p, ..]) if p != 0 => usize::from(p),
            _ => 1,
        };
        let bottom = params
            .next()
            .map(|p| usize::from(p[0]))
            .filter(|&p| p != 0)
            .unwrap_or(self.rows);
        if top >= bottom {
            return;
        }
        self.region = ((top - 1).min(self.rows), bottom.min(self.rows));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if byte == b'c' && intermediates.is_empty() {
            let rows = self.rows;
            *self = Self::new(rows);
        }
    }
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
    config: Config,
    scrollback_lines: usize,
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
            kitty_keyboard: true,
            ..Config::default()
        };
        let listener = Listener::default();
        Self {
            term: Term::new(config.clone(), &size, listener.clone()),
            parser: Processor::new(),
            tracker: vte::Parser::new(),
            tracked: Tracked::new(size.rows),
            listener,
            config,
            scrollback_lines,
        }
    }

    /// Feed PTY output.
    pub fn process(&mut self, bytes: &[u8]) {
        self.tracker.advance(&mut self.tracked, bytes);
        self.parser.advance(&mut self.term, bytes);
        // A synchronized update (`CSI ? 2026 h`) buffers output until its
        // end or a timeout; nothing else drives the timeout here.
        if self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.parser.stop_sync(&mut self.term);
        }
    }

    /// Apply output buffered by an unfinished synchronized update: it has
    /// already been sent to live clients, so it is part of the state.
    fn flush(&mut self) {
        if self.parser.sync_bytes_count() > 0 || self.parser.sync_timeout().sync_timeout().is_some()
        {
            self.parser.stop_sync(&mut self.term);
        }
    }

    /// Resize (reflowing the primary screen) and re-derive the history
    /// bound for the new width.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.flush();
        let size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        self.term.resize(size);
        self.tracked.resize(size.rows);
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
        self.listener
            .title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the alternate screen is up.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Escape sequences that rebuild this terminal in a freshly reset
    /// xterm-compatible one of the same size: the primary screen's
    /// scrollback and rows, then (when up) the alternate screen, then the
    /// active screen's cursor, saved cursor, charsets, scroll region, SGR
    /// pen, DEC modes, cursor style, title, and palette changes.
    pub fn snapshot(&mut self) -> Vec<u8> {
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
            w.lines(primary, primary.topmost_line());
            w.cup(primary.cursor.point);
            w.pen(&primary.cursor.template);
            self.term.swap_alt();
            *self.term.grid_mut() = alt;
            // Save the primary cursor, switch to a cleared alternate screen.
            w.raw(b"\x1b[?1049h\x1b[H");
            w.lines(self.term.grid(), Line(0));
        } else {
            let grid = self.term.grid();
            w.lines(grid, grid.topmost_line());
        }
        self.write_state(&mut w);
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
        let kitty = kitty_flags(mode);
        if kitty != 0 {
            let _ = write!(w.out, "\x1b[={kitty};1u");
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
}

fn kitty_flags(mode: TermMode) -> u8 {
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .into_iter()
    .filter(|(flag, _)| mode.contains(*flag))
    .map(|(_, bit)| bit)
    .sum()
}

/// A title without control characters (it is replayed inside an OSC).
fn sanitize(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control())
        .take(4096)
        .collect()
}

fn wrapped(grid: &Grid<Cell>, line: Line) -> bool {
    grid[line][grid.last_column()]
        .flags
        .contains(Flags::WRAPLINE)
}

/// A row's characters (wide-char spacers skipped, tabs as spaces).
fn row_text(grid: &Grid<Cell>, line: Line) -> String {
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
fn recent_lines(grid: &Grid<Cell>, count: usize) -> Vec<String> {
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

fn printable(c: char) -> char {
    // A tab is stored in the cell it started at; the cells it skipped
    // follow, so replaying the tab itself would shift them.
    if c == '\t' || c.is_control() { ' ' } else { c }
}

/// The SGR state a cell is drawn with.
#[derive(Debug, Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE_FLAGS,
            underline: cell.underline_color(),
        }
    }
}

/// Whether a trailing cell can be dropped from a row (a reset terminal's
/// erased cell looks the same).
fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\t')
        && Pen::of(cell).bg == Pen::default().bg
        && !cell.flags.intersects(VISIBLE_BLANK_FLAGS)
        && cell.zerowidth().is_none()
        && cell.hyperlink().is_none()
}

/// Accumulates snapshot bytes, emitting SGR / OSC 8 only on change.
#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    pen: Pen,
    link: Option<Hyperlink>,
}

impl Writer {
    fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// Absolute cursor position.
    fn cup(&mut self, point: Point) {
        let row = usize::try_from(point.line.0).unwrap_or(0);
        let _ = write!(self.out, "\x1b[{};{}H", row + 1, point.column.0 + 1);
    }

    fn pen(&mut self, cell: &Cell) {
        let pen = Pen::of(cell);
        if pen == self.pen {
            return;
        }
        self.out.extend_from_slice(b"\x1b[0");
        let f = pen.flags;
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if f.contains(flag) {
                self.out.push(b';');
                self.out.extend_from_slice(code.as_bytes());
            }
        }
        let underline = if f.contains(Flags::DOUBLE_UNDERLINE) {
            Some("4:2")
        } else if f.contains(Flags::UNDERCURL) {
            Some("4:3")
        } else if f.contains(Flags::DOTTED_UNDERLINE) {
            Some("4:4")
        } else if f.contains(Flags::DASHED_UNDERLINE) {
            Some("4:5")
        } else if f.contains(Flags::UNDERLINE) {
            Some("4")
        } else {
            None
        };
        if let Some(code) = underline {
            self.out.push(b';');
            self.out.extend_from_slice(code.as_bytes());
        }
        color(&mut self.out, pen.fg, 30, 90, 38);
        color(&mut self.out, pen.bg, 40, 100, 48);
        if let Some(c) = pen.underline {
            match c {
                Color::Spec(rgb) => {
                    let _ = write!(self.out, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
                }
                Color::Indexed(i) => {
                    let _ = write!(self.out, ";58;5;{i}");
                }
                Color::Named(n) => {
                    if let Some(i) = named_index(n) {
                        let _ = write!(self.out, ";58;5;{i}");
                    }
                }
            }
        }
        self.out.push(b'm');
        self.pen = pen;
    }

    fn link(&mut self, link: Option<Hyperlink>) {
        if link == self.link {
            return;
        }
        match &link {
            Some(l) => {
                let _ = write!(
                    self.out,
                    "\x1b]8;id={};{}\x1b\\",
                    sanitize(l.id()),
                    sanitize(l.uri())
                );
            }
            None => self.raw(b"\x1b]8;;\x1b\\"),
        }
        self.link = link;
    }

    fn cell(&mut self, cell: &Cell) {
        self.pen(cell);
        self.link(cell.hyperlink());
        let mut buf = [0u8; 4];
        self.raw(printable(cell.c).encode_utf8(&mut buf).as_bytes());
        if let Some(zerowidth) = cell.zerowidth() {
            for &c in zerowidth {
                self.raw(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    /// Back to the default pen and no link (before a line break, so a
    /// scroll does not fill the new line with the pen's background).
    fn plain(&mut self) {
        self.link(None);
        if self.pen != Pen::default() {
            self.raw(b"\x1b[0m");
            self.pen = Pen::default();
        }
    }

    fn charsets(&mut self, charsets: &alacritty_terminal::grid::Charsets) {
        for (index, intro) in [
            (CharsetIndex::G0, b'('),
            (CharsetIndex::G1, b')'),
            (CharsetIndex::G2, b'*'),
            (CharsetIndex::G3, b'+'),
        ] {
            let set = match charsets[index] {
                StandardCharset::Ascii => b'B',
                StandardCharset::SpecialCharacterAndLineDrawing => b'0',
            };
            self.raw(&[0x1b, intro, set]);
        }
    }

    /// Rows `from..=bottom` of `grid`, written from the cursor's current
    /// position: soft-wrapped rows continue by autowrap (so the client
    /// keeps them as one logical line), others end in CR LF.
    fn lines(&mut self, grid: &Grid<Cell>, from: Line) {
        let cols = grid.columns();
        let mut previous_wrapped = false;
        let mut first = true;
        let mut line = from;
        while line <= grid.bottommost_line() {
            let row = &grid[line];
            let is_wrapped = wrapped(grid, line);
            let end = if is_wrapped {
                cols
            } else {
                (0..cols)
                    .rev()
                    .find(|&c| !is_blank(&row[Column(c)]))
                    .map_or(0, |c| c + 1)
            };
            // A wrapped row's content continues by autowrap.
            let continues = previous_wrapped && end > 0;
            if !first && !continues {
                self.plain();
                self.raw(b"\r\n");
            }
            for col in 0..end {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                self.cell(cell);
            }
            previous_wrapped = is_wrapped;
            first = false;
            line += 1;
        }
        self.plain();
    }
}

fn named_index(named: NamedColor) -> Option<u8> {
    let n = named as usize;
    match n {
        0..=15 => u8::try_from(n).ok(),
        // Dim variants are a rendering of the base colour.
        _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&n) => {
            u8::try_from(n - NamedColor::DimBlack as usize).ok()
        }
        // Foreground/background/cursor and the bright/dim foregrounds are
        // the default colour.
        _ => None,
    }
}

/// `;<sgr>` for a foreground (`base` 30, `bright` 90, `ext` 38) or
/// background (40/100/48) colour; nothing for the default.
fn color(out: &mut Vec<u8>, color: Color, base: u8, bright: u8, ext: u8) {
    match color {
        Color::Named(named) => {
            if let Some(i) = named_index(named) {
                let code = if i < 8 { base + i } else { bright + i - 8 };
                let _ = write!(out, ";{code}");
            }
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{ext};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: u16 = 40;
    const ROWS: u16 = 10;

    fn emulator() -> Emulator {
        Emulator::new(COLS, ROWS, 200)
    }

    fn fed(bytes: &[u8]) -> Emulator {
        let mut e = emulator();
        e.process(bytes);
        e
    }

    /// What a client that reset itself and replayed `source`'s snapshot
    /// holds.
    fn replayed(source: &mut Emulator) -> Emulator {
        let (cols, rows) = source.size();
        let mut client = Emulator::new(
            u16::try_from(cols).unwrap(),
            u16::try_from(rows).unwrap(),
            source.scrollback_lines,
        );
        client.process(&source.snapshot());
        client
    }

    type CellKey = Option<(
        char,
        Color,
        Color,
        Flags,
        Vec<char>,
        Option<Color>,
        Option<String>,
    )>;

    fn cell_key(cell: &Cell) -> CellKey {
        if is_blank(cell) {
            return None;
        }
        let layout = Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        Some((
            printable(cell.c),
            cell.fg,
            cell.bg,
            cell.flags & (STYLE_FLAGS | layout | Flags::WRAPLINE),
            cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
            cell.underline_color(),
            // Ids without an explicit one come from a process-wide counter.
            cell.hyperlink().map(|l| l.uri().to_owned()),
        ))
    }

    /// Everything a client-visible difference could come from, on the
    /// active screen.
    fn assert_same(a: &mut Emulator, b: &mut Emulator) {
        a.flush();
        b.flush();
        let (ga, gb) = (a.term.grid(), b.term.grid());
        assert_eq!(ga.history_size(), gb.history_size(), "history size");
        assert_eq!(ga.columns(), gb.columns());
        let mut line = ga.topmost_line();
        while line <= ga.bottommost_line() {
            for col in 0..ga.columns() {
                let (ca, cb) = (&ga[line][Column(col)], &gb[line][Column(col)]);
                assert_eq!(cell_key(ca), cell_key(cb), "cell {line:?},{col}");
            }
            line += 1;
        }
        assert_eq!(ga.cursor.point, gb.cursor.point, "cursor");
        assert_eq!(
            ga.cursor.input_needs_wrap, gb.cursor.input_needs_wrap,
            "pending wrap"
        );
        assert_eq!(
            Pen::of(&ga.cursor.template),
            Pen::of(&gb.cursor.template),
            "pen"
        );
        assert_eq!(ga.cursor.charsets, gb.cursor.charsets, "charsets");
        assert_eq!(ga.saved_cursor.point, gb.saved_cursor.point, "saved cursor");
        assert_eq!(*a.term.mode(), *b.term.mode(), "modes");
        assert_eq!(a.tracked, b.tracked, "scroll region / shift state");
        assert_eq!(a.term.cursor_style(), b.term.cursor_style(), "cursor style");
        assert_eq!(a.title(), b.title(), "title");
        for i in 0..259 {
            assert_eq!(a.term.colors()[i], b.term.colors()[i], "colour {i}");
        }
    }

    fn rich_shell_output() -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..30 {
            out.extend(format!("line {i} \x1b[1;31mred\x1b[0m \x1b[38;5;208mindexed\x1b[0m \x1b[48;2;10;20;30mrgb bg\x1b[0m\r\n").as_bytes());
        }
        // Soft-wrapped long line, wide chars, a combining mark, a tab.
        out.extend("w".repeat(95).as_bytes());
        out.extend("\r\n\u{4f60}\u{597d} e\u{301} a\tb\r\n".as_bytes());
        // Underline styles + colour, italic, dim, inverse, strike, hidden.
        out.extend(b"\x1b[4:3;58;2;255;0;0mcurly\x1b[0m \x1b[3;2mit\x1b[7minv\x1b[9mst\x1b[8mhid\x1b[0m\r\n");
        // Hyperlink, background-coloured trailing blanks, then a prompt.
        out.extend(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ \x1b[44m   \x1b[0m\r\n$ ");
        out.extend(b"\x1b[1;32m");
        out
    }

    #[test]
    fn a_replayed_primary_screen_matches_cell_for_cell() {
        let mut source = fed(&rich_shell_output());
        assert!(source.history_lines() > 0);
        let mut client = replayed(&mut source);
        assert_same(&mut source, &mut client);
        // And keeps matching as output continues (pen carried over).
        for e in [&mut source, &mut client] {
            e.process(b"green\x1b[0m\r\nmore\r\n");
        }
        assert_same(&mut source, &mut client);
    }

    fn full_screen_app() -> Vec<u8> {
        let mut out = rich_shell_output();
        out.extend(b"\x1b[0mvim file\r\n");
        // Enter the alternate screen and set what a TUI typically does.
        out.extend(b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h");
        out.extend(
            b"\x1b]2;vim - file\x07\x1b[6 q\x1b]4;1;rgb:12/34/56\x07\x1b]11;rgb:00/00/20\x07",
        );
        out.extend(b"\x1b[H\x1b[2J\x1b[44;37m title bar \x1b[K\x1b[0m");
        for row in 2..ROWS {
            out.extend(format!("\x1b[{row};1H~ row {row}").as_bytes());
        }
        // A scroll region, origin mode, a saved cursor, line drawing.
        out.extend(b"\x1b[3;8r\x1b[?6h\x1b[2;5H\x1b7\x1b[4;2H\x1b(0lqqk\x1b(B\x1b[?25l");
        out.extend(b"\x1b[1;35m");
        out
    }

    #[test]
    fn a_replayed_alternate_screen_matches_and_restores_the_primary() {
        let mut source = fed(&full_screen_app());
        assert!(source.alt_screen());
        let mut client = replayed(&mut source);
        assert_same(&mut source, &mut client);
        let text = client.text(TextSource::Visible);
        assert_eq!(text.title.as_deref(), Some("vim - file"));
        assert!(text.modes.contains(&"bracketed_paste"), "{:?}", text.modes);
        // Continue identically: scroll inside the region (origin-relative),
        // restore the saved cursor, then leave the alternate screen.
        let tail: &[u8] = b"\x1b[6;1Hx\n\n\nscrolled\x1b8saved\x1b[?6l\x1b[r\x1b[?1049lback\r\n";
        for e in [&mut source, &mut client] {
            e.process(tail);
        }
        assert!(!source.alt_screen());
        assert_same(&mut source, &mut client);
    }

    #[test]
    fn snapshotting_does_not_perturb_the_session() {
        let mut snapshotted = fed(&full_screen_app());
        let mut control = fed(&full_screen_app());
        let first = snapshotted.snapshot();
        assert_eq!(first, snapshotted.snapshot(), "snapshots are stable");
        let tail: &[u8] = b"\x1b8more\x1b[?1049l\x1b[0mafter\r\n";
        for e in [&mut snapshotted, &mut control] {
            e.process(tail);
        }
        assert_same(&mut snapshotted, &mut control);
        // The primary screen survived the snapshot's screen swap intact.
        let text = snapshotted.text(TextSource::Recent { lines: 3 });
        assert_eq!(text.lines, ["link", "$ vim file", "after"]);
    }

    #[test]
    fn pending_wrap_and_shifted_charsets_survive_replay() {
        let mut source = fed(format!("{}\x1b)0\x0e", "z".repeat(usize::from(COLS))).as_bytes());
        assert!(source.term.grid().cursor.input_needs_wrap);
        let mut client = replayed(&mut source);
        assert_same(&mut source, &mut client);
        for e in [&mut source, &mut client] {
            e.process(b"qx\x0fqx");
        }
        assert_same(&mut source, &mut client);
        let text = client.text(TextSource::Visible);
        assert_eq!(text.lines[1], "\u{2500}\u{2502}qx");
    }

    #[test]
    fn an_unfinished_synchronized_update_is_part_of_the_snapshot() {
        let mut source = fed(b"before\x1b[?2026h\r\nduring");
        let mut client = replayed(&mut source);
        assert_eq!(client.text(TextSource::Visible).lines[1], "during");
        assert_same(&mut source, &mut client);
    }

    #[test]
    fn rendered_text_joins_soft_wraps_and_drops_trailing_blank_rows() {
        let long = "a".repeat(usize::from(COLS) + 5);
        let mut e = fed(format!("one\r\n{long}\r\nthree   \r\n").as_bytes());
        let recent = e.text(TextSource::Recent { lines: 10 });
        assert_eq!(
            recent.lines,
            vec!["one".to_owned(), long, "three".to_owned()]
        );
        let last = e.text(TextSource::Recent { lines: 1 });
        assert_eq!(last.lines, vec!["three".to_owned()]);
        let visible = e.text(TextSource::Visible);
        assert_eq!(visible.lines.len(), usize::from(ROWS));
        assert_eq!(visible.lines[0], "one");
        assert_eq!(visible.lines[2], "aaaaa");
        assert_eq!(
            visible.cursor,
            CursorView {
                row: 4,
                col: 0,
                visible: true
            }
        );
        assert!(!visible.alt_screen);
    }

    #[test]
    fn history_is_bounded_by_lines_and_by_cells() {
        let mut e = Emulator::new(COLS, ROWS, 50);
        for i in 0..500 {
            e.process(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(e.history_lines(), 50);
        // Wide enough that the cell budget, not the line count, binds.
        let mut wide = Emulator::new(1000, ROWS, 5000);
        for i in 0..3000 {
            wide.process(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(wide.history_lines(), HISTORY_CELL_BUDGET / 1000);
        wide.resize(2000, ROWS);
        assert!(wide.history_lines() <= HISTORY_CELL_BUDGET / 2000);
    }

    #[test]
    fn resize_resets_the_tracked_scroll_region() {
        let mut e = fed(b"\x1b[2;5r");
        assert_eq!(e.tracked.region, (1, 5));
        e.resize(COLS, 20);
        assert_eq!(e.tracked.region, (0, 20));
        e.process(b"\x1b[4;3r"); // invalid: ignored, like the terminal
        assert_eq!(e.tracked.region, (0, 20));
        e.process(b"\x1b[3r\x1bc");
        assert_eq!(e.tracked.region, (0, 20));
    }

    #[test]
    fn a_title_cannot_inject_control_sequences() {
        let mut e = fed(b"\x1b]2;evil\x1b[31m\x07");
        let snapshot = String::from_utf8(e.snapshot()).unwrap();
        let osc = snapshot.find("\x1b]2;").unwrap();
        let end = osc + snapshot[osc..].find('\x07').unwrap();
        assert!(!snapshot[osc + 4..end].contains('\x1b'));
    }
}
