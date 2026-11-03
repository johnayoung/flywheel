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
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, Hyperlink as LinkSpec,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor,
    PrivateMode, Processor, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, StdSyncHandler,
    TabulationClearMode,
};
use serde::Serialize;
use std::io::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

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

/// The colours a viewer's terminal shows wherever the program has set none:
/// the 16 ANSI colours plus the default foreground, background, and cursor.
/// Terminal colour queries (OSC 4/10/11/12) are answered from these, so a
/// program that adapts to the background (light or dark) matches what the
/// viewer actually sees. The web UI reports its theme on attach and on
/// every theme change or focus; the last report wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerColors {
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
}

const fn rgb(v: u32) -> Rgb {
    Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    }
}

impl ViewerColors {
    /// The web app's dark terminal theme (`TERM_THEMES.dark` in
    /// `web/app.js`): what a session answers before any viewer reported.
    pub const DARK: Self = Self {
        ansi: [
            rgb(0x22241f),
            rgb(0xef5a4c),
            rgb(0xa3c76d),
            rgb(0xe3b341),
            rgb(0x6fa3d6),
            rgb(0xc98bd1),
            rgb(0x62c2b5),
            rgb(0xd9d3c3),
            rgb(0x5d5a51),
            rgb(0xff7a66),
            rgb(0xbfe08a),
            rgb(0xf4cd68),
            rgb(0x93bff0),
            rgb(0xe0a9e6),
            rgb(0x86ddd0),
            rgb(0xf6f1e4),
        ],
        foreground: rgb(0xe6e0cf),
        background: rgb(0x0c0d0c),
        cursor: rgb(0xff5a1f),
    };

    /// Parse a viewer's report: `#rrggbb` colours (16 ANSI ones).
    pub fn from_hex(
        foreground: &str,
        background: &str,
        cursor: &str,
        ansi: &[String],
    ) -> Result<Self, String> {
        let parse = |what: &str, text: &str| {
            parse_hex_color(text).ok_or_else(|| format!("{what}: {text:?} is not #rrggbb"))
        };
        if ansi.len() != 16 {
            return Err(format!("ansi: expected 16 colours, got {}", ansi.len()));
        }
        let mut colors = [Rgb::default(); 16];
        for (i, text) in ansi.iter().enumerate() {
            colors[i] = parse(&format!("ansi[{i}]"), text)?;
        }
        Ok(Self {
            ansi: colors,
            foreground: parse("foreground", foreground)?,
            background: parse("background", background)?,
            cursor: parse("cursor", cursor)?,
        })
    }

    /// The colour for palette / dynamic-colour `index` when the program has
    /// not set one: these colours, then xterm.js 6's 256-colour cube and
    /// greys (which no theme changes).
    fn color(&self, index: usize) -> Rgb {
        match index {
            0..16 => self.ansi[index],
            16..232 => {
                let level = |n: usize| [0u8, 95, 135, 175, 215, 255][n % 6];
                let i = index - 16;
                Rgb {
                    r: level(i / 36),
                    g: level(i / 6),
                    b: level(i),
                }
            }
            232..256 => {
                let v = u8::try_from(8 + (index - 232) * 10).unwrap_or(u8::MAX);
                Rgb { r: v, g: v, b: v }
            }
            _ if index == NamedColor::Background as usize => self.background,
            _ if index == NamedColor::Cursor as usize => self.cursor,
            _ => self.foreground,
        }
    }
}

impl Default for ViewerColors {
    fn default() -> Self {
        Self::DARK
    }
}

/// `#rrggbb` (case-insensitive) as a colour.
fn parse_hex_color(text: &str) -> Option<Rgb> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(rgb(v))
}

/// The parser's view of the terminal: `Term` with what output can make it
/// retain bounded, and colour queries answered.
///
/// * titles are cut to [`MAX_TITLE_BYTES`] and the title stack is kept here,
///   [`TITLE_STACK_DEPTH`] deep (`Term`'s is 4096 deep, each entry a clone of
///   the title: output could make it allocate thousands of times its size);
/// * combining characters beyond [`MAX_ZEROWIDTH`] on one cell are dropped
///   (`Term` keeps every one);
/// * over-long hyperlinks are dropped;
/// * OSC 4/10/11/12 queries are answered from the palette, falling back to
///   the viewer's [`ViewerColors`] (`Term` would ask a renderer for colours
///   it does not know).
///
/// Everything else is forwarded unchanged.
struct Guarded<'a> {
    term: &'a mut Term<Listener>,
    titles: &'a mut Vec<Option<String>>,
    listener: &'a Listener,
    colors: &'a ViewerColors,
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $name(&mut self, $($arg: $ty),*) {
                <Term<Listener> as Handler>::$name(self.term, $($arg),*)
            }
        )*
    };
}

impl Handler for Guarded<'_> {
    fn set_title(&mut self, title: Option<String>) {
        let title = title.map(|t| truncated(t, MAX_TITLE_BYTES));
        <Term<Listener> as Handler>::set_title(self.term, title);
    }

    fn push_title(&mut self) {
        if self.titles.len() >= TITLE_STACK_DEPTH {
            self.titles.remove(0);
        }
        self.titles.push(self.listener.title());
    }

    fn pop_title(&mut self) {
        if let Some(title) = self.titles.pop() {
            self.set_title(title);
        }
    }

    fn reset_state(&mut self) {
        self.titles.clear();
        <Term<Listener> as Handler>::reset_state(self.term);
    }

    fn input(&mut self, c: char) {
        if c.width() == Some(0) {
            // The cell `Term::input` attaches a zero-width character to.
            let grid = self.term.grid();
            let cursor = &grid.cursor;
            let mut column = cursor.point.column;
            if !cursor.input_needs_wrap {
                column.0 = column.0.saturating_sub(1);
            }
            let row = &grid[cursor.point.line];
            if row[column].flags.contains(Flags::WIDE_CHAR_SPACER) {
                column.0 = column.0.saturating_sub(1);
            }
            if row[column]
                .zerowidth()
                .is_some_and(|z| z.len() >= MAX_ZEROWIDTH)
            {
                return;
            }
        }
        <Term<Listener> as Handler>::input(self.term, c);
    }

    fn set_hyperlink(&mut self, link: Option<LinkSpec>) {
        let link = link.filter(|l| {
            l.uri.len() <= MAX_LINK_URI_BYTES
                && l.id.as_ref().is_none_or(|id| id.len() <= MAX_LINK_ID_BYTES)
        });
        <Term<Listener> as Handler>::set_hyperlink(self.term, link);
    }

    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        if index >= alacritty_terminal::term::color::COUNT {
            return;
        }
        let rgb = self.term.colors()[index].unwrap_or_else(|| self.colors.color(index));
        self.listener.reply(&format!(
            "\x1b]{prefix};rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}{terminator}",
            rgb.r, rgb.g, rgb.b
        ));
    }

    forward! {
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(count: usize);
        move_up(count: usize);
        move_down(count: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(col: usize);
        move_backward(col: usize);
        move_down_and_cr(row: usize);
        move_up_and_cr(row: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(count: usize);
        scroll_down(count: usize);
        insert_blank_lines(count: usize);
        delete_lines(count: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_screen(mode: ClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        text_area_size_pixels();
        text_area_size_chars();
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(to_pop: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

/// Where the byte stream is relative to an OSC string, so an endless one
/// can be cut off before the parsers buffer it (`vte` with `std` buffers OSC
/// strings without limit). Mirrors `vte`'s transitions: ESC enters the
/// escape state from anywhere, `]` there starts an OSC string, and BEL,
/// CAN, SUB or ESC ends one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum OscScan {
    #[default]
    Other,
    Escape,
    Osc(usize),
    /// Past [`MAX_OSC_BYTES`]: bytes are dropped until the terminator.
    Discard,
}

impl OscScan {
    /// Advance over `byte`; returns whether the parsers should see it.
    fn keep(&mut self, byte: u8) -> bool {
        match *self {
            Self::Other => {
                if byte == 0x1b {
                    *self = Self::Escape;
                }
                true
            }
            Self::Escape => {
                *self = match byte {
                    b']' => Self::Osc(0),
                    // C0 controls (but CAN/SUB) execute without leaving the
                    // escape state; DEL is ignored there.
                    0x00..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f => Self::Escape,
                    _ => Self::Other,
                };
                true
            }
            Self::Osc(len) => {
                *self = match byte {
                    0x07 | 0x18 | 0x1a => Self::Other,
                    0x1b => Self::Escape,
                    _ if len >= MAX_OSC_BYTES => {
                        *self = Self::Discard;
                        return false;
                    }
                    _ => Self::Osc(len + 1),
                };
                true
            }
            Self::Discard => match byte {
                0x07 | 0x18 | 0x1a => {
                    *self = Self::Other;
                    true
                }
                0x1b => {
                    *self = Self::Escape;
                    true
                }
                _ => false,
            },
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
    /// The title stack (see [`Guarded`]).
    titles: Vec<Option<String>>,
    /// What colour queries answer where the program set no colour.
    colors: ViewerColors,
    osc: OscScan,
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
    /// position (with the default pen and no link) in logical lines:
    /// soft-wrapped rows continue by autowrap (so the client keeps them as
    /// one line), and lines end in CR LF. Lines entirely above the screen
    /// are included newest first while they fit in `history_budget` bytes.
    fn rows(&mut self, grid: &Grid<Cell>, from: Line, history_budget: usize) {
        debug_assert!(self.pen == Pen::default() && self.link.is_none());
        // Each logical line ends with the default pen and no link (see
        // `plain`), so lines render independently: bottom up, to stop at
        // the budget.
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut spent = 0usize;
        let mut last = grid.bottommost_line();
        let mut line = last;
        loop {
            let starts = line == from || !(wrapped(grid, line - 1) && row_end(grid, line) > 0);
            if starts {
                let mut piece = Writer::default();
                piece.logical_line(grid, line, last);
                if last < Line(0) {
                    spent += piece.out.len() + 2;
                    if spent > history_budget {
                        break;
                    }
                }
                lines.push(piece.out);
                if line == from {
                    break;
                }
                last = line - 1;
            }
            line -= 1;
        }
        for (i, piece) in lines.iter().rev().enumerate() {
            if i > 0 {
                self.raw(b"\r\n");
            }
            self.raw(piece);
        }
    }

    /// Rows `first..=last` (one logical line), then the default pen.
    fn logical_line(&mut self, grid: &Grid<Cell>, first: Line, last: Line) {
        let mut line = first;
        while line <= last {
            let row = &grid[line];
            for col in 0..row_end(grid, line) {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                self.cell(cell);
            }
            line += 1;
        }
        self.plain();
    }
}

/// Cells of a row worth writing: all of a soft-wrapped row, else up to the
/// last non-blank one.
fn row_end(grid: &Grid<Cell>, line: Line) -> usize {
    let cols = grid.columns();
    if wrapped(grid, line) {
        return cols;
    }
    let row = &grid[line];
    (0..cols)
        .rev()
        .find(|&c| !is_blank(&row[Column(c)]))
        .map_or(0, |c| c + 1)
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
        client.process(&source.snapshot(SNAPSHOT_HISTORY_BYTES));
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
        let first = snapshotted.snapshot(SNAPSHOT_HISTORY_BYTES);
        assert_eq!(
            first,
            snapshotted.snapshot(SNAPSHOT_HISTORY_BYTES),
            "snapshots are stable"
        );
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
    fn growing_taller_pulls_scrollback_only_with_the_cursor_on_the_bottom_row() {
        let visible = |e: &mut Emulator| e.text(TextSource::Visible).lines;
        let mut lines = String::new();
        for i in 0..30 {
            lines.push_str(&format!("line {i}\r\n"));
        }
        // An inline TUI: its cursor sits above a footer row, not at the
        // bottom. xterm.js adds blank rows below; the screen must match.
        let mut e = emulator();
        e.process(lines.as_bytes());
        e.process(b"\x1b[3;1H");
        let before = visible(&mut e);
        let history = e.history_lines();
        e.resize(COLS, ROWS + 5);
        let after = visible(&mut e);
        assert_eq!(after[..usize::from(ROWS)], before[..], "{after:#?}");
        assert!(
            after[usize::from(ROWS)..]
                .iter()
                .all(|r| r.trim().is_empty())
        );
        assert_eq!(e.history_lines(), history);
        e.process(b"X");
        assert!(
            visible(&mut e)[2].starts_with('X'),
            "{:#?}",
            visible(&mut e)
        );
        // Cursor on the bottom row (a shell prompt): scrollback comes
        // back above, as in xterm.js.
        let mut e = emulator();
        e.process(lines.as_bytes());
        let before = visible(&mut e);
        e.resize(COLS, ROWS + 5);
        let after = visible(&mut e);
        assert_eq!(after[5..], before[..], "{after:#?}");
        assert_eq!(after[0], "line 16");
        assert_eq!(e.history_lines(), history - 5);
        // Taller and wider at once keeps the same rows.
        let mut e = emulator();
        e.process(lines.as_bytes());
        e.process(b"\x1b[3;1H");
        let before = visible(&mut e);
        e.resize(COLS + 10, ROWS + 5);
        assert_eq!(visible(&mut e)[..usize::from(ROWS)], before[..]);
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
        let snapshot = String::from_utf8(e.snapshot(SNAPSHOT_HISTORY_BYTES)).unwrap();
        let osc = snapshot.find("\x1b]2;").unwrap();
        let end = osc + snapshot[osc..].find('\x07').unwrap();
        assert!(!snapshot[osc + 4..end].contains('\x1b'));
    }

    #[test]
    fn kitty_keyboard_pushes_are_inert_and_cannot_panic() {
        // `Term` 0.26 removes from the (empty) title stack once the kitty
        // stack is 4096 deep: a panic on the reader thread.
        let mut e = emulator();
        e.process(&b"\x1b[>1u".repeat(5000));
        e.process(b"\x1b[?u\x1b[?1049h\x1b[>1u\x1b[=3;1u");
        assert_eq!(e.take_replies(), b"", "kitty support must not be reported");
        let snapshot = e.snapshot(SNAPSHOT_HISTORY_BYTES);
        assert!(!String::from_utf8_lossy(&snapshot).contains("\x1b[="));
        let mut client = replayed(&mut e);
        assert_same(&mut e, &mut client);
    }

    #[test]
    fn titles_and_the_title_stack_are_bounded() {
        let mut e = emulator();
        let huge = "t".repeat(256 * 1024);
        e.process(format!("\x1b]2;{huge}\x07").as_bytes());
        e.process(&b"\x1b[22t".repeat(4096));
        assert_eq!(e.title().map(|t| t.len()), Some(MAX_TITLE_BYTES));
        assert_eq!(e.titles.len(), TITLE_STACK_DEPTH);
        let kept: usize = e.titles.iter().flatten().map(String::len).sum();
        assert!(kept <= TITLE_STACK_DEPTH * MAX_TITLE_BYTES, "{kept}");
        // The stack still works: push, retitle, pop restores.
        e.process(b"\x1b]2;one\x07\x1b[22t\x1b]2;two\x07");
        assert_eq!(e.title().as_deref(), Some("two"));
        e.process(b"\x1b[23t");
        assert_eq!(e.title().as_deref(), Some("one"));
        e.process(b"\x1bc");
        assert!(e.titles.is_empty());
    }

    #[test]
    fn an_endless_osc_string_is_cut_off_and_output_resumes_after_it() {
        let mut scan = OscScan::default();
        let osc: Vec<u8> = [&b"\x1b]2;"[..], &vec![b'x'; MAX_OSC_BYTES + 10]].concat();
        let kept = osc.iter().filter(|&&b| scan.keep(b)).count();
        assert_eq!(kept, 4 + MAX_OSC_BYTES - 2, "{scan:?}");
        assert_eq!(scan, OscScan::Discard);
        assert!(scan.keep(0x07));
        assert_eq!(scan, OscScan::Other);
        // Escape-state controls and CSI bytes do not start an OSC.
        for b in b"\x1b\x05]" {
            scan.keep(*b);
        }
        assert_eq!(scan, OscScan::Osc(0));
        scan.keep(0x18);
        for b in b"\x1b[31m]" {
            scan.keep(*b);
        }
        assert_eq!(scan, OscScan::Other);

        let mut e = emulator();
        e.process(&osc);
        e.process(&vec![b'y'; 1024 * 1024]);
        e.process(b"\x1b\\after");
        assert_eq!(e.title().map(|t| t.len()), Some(MAX_TITLE_BYTES));
        assert_eq!(e.text(TextSource::Visible).lines[0], "after");
    }

    #[test]
    fn combining_marks_per_cell_are_bounded() {
        let mut e = emulator();
        let mut bytes = b"a".to_vec();
        bytes.extend("\u{301}".repeat(200_000).as_bytes());
        // Wide-char spacer target, and a cursor parked in pending wrap.
        bytes.extend("\r\n\u{4f60}".as_bytes());
        bytes.extend("\u{302}".repeat(100).as_bytes());
        e.process(&bytes);
        let grid = e.term.grid();
        let marks = |line: i32, col: usize| {
            grid[Line(line)][Column(col)]
                .zerowidth()
                .map_or(0, <[char]>::len)
        };
        assert_eq!(marks(0, 0), MAX_ZEROWIDTH);
        assert_eq!(marks(1, 0), MAX_ZEROWIDTH);
        assert!(e.snapshot(SNAPSHOT_HISTORY_BYTES).len() < 1024);
        let mut client = replayed(&mut e);
        assert_same(&mut e, &mut client);
    }

    #[test]
    fn over_long_hyperlinks_are_dropped() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_URI_BYTES));
        let mut e = fed(format!("\x1b]8;;{long}\x1b\\x\x1b]8;;https://ok\x1b\\y").as_bytes());
        let row = &e.term.grid()[Line(0)];
        assert!(row[Column(0)].hyperlink().is_none());
        assert_eq!(
            row[Column(1)]
                .hyperlink()
                .map(|l| l.uri().to_owned())
                .as_deref(),
            Some("https://ok")
        );
        let mut client = replayed(&mut e);
        assert_same(&mut e, &mut client);
    }

    #[test]
    fn queries_are_answered_by_the_emulator() {
        let mut e = fed(b"abc");
        e.process(b"\x1b[6n\x1b[5n\x1b[c\x1b[?2004$p");
        assert_eq!(
            String::from_utf8(e.take_replies()).unwrap(),
            "\x1b[1;4R\x1b[0n\x1b[?6c\x1b[?2004;2$y"
        );
        assert!(e.take_replies().is_empty(), "drained");
        // Colours: the web theme until the program sets one.
        e.process(b"\x1b]11;?\x07\x1b]4;1;?\x1b\\\x1b]4;100;?\x07");
        assert_eq!(
            String::from_utf8(e.take_replies()).unwrap(),
            "\x1b]11;rgb:0c0c/0d0d/0c0c\x07\x1b]4;1;rgb:efef/5a5a/4c4c\x1b\\\
             \x1b]4;100;rgb:8787/8787/0000\x07"
        );
        e.process(b"\x1b]11;rgb:12/34/56\x07\x1b]11;?\x07");
        assert_eq!(
            String::from_utf8(e.take_replies()).unwrap(),
            "\x1b]11;rgb:1212/3434/5656\x07"
        );
        let dark = ViewerColors::DARK;
        assert_eq!(dark.color(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            dark.color(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        // Snapshots carry no queries, so replaying one asks nothing.
        let mut client = replayed(&mut e);
        assert!(client.take_replies().is_empty());
    }

    #[test]
    fn colour_queries_follow_the_viewer_theme() {
        let mut e = fed(b"");
        let ansi: Vec<String> = (0..16).map(|i| format!("#0000{i:02x}")).collect();
        let light = ViewerColors::from_hex("#23241F", "#f2eee4", "#e2470c", &ansi).unwrap();
        e.set_viewer_colors(light);
        e.process(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b]4;3;?\x07\x1b]4;16;?\x07");
        assert_eq!(
            String::from_utf8(e.take_replies()).unwrap(),
            "\x1b]10;rgb:2323/2424/1f1f\x07\x1b]11;rgb:f2f2/eeee/e4e4\x07\
             \x1b]12;rgb:e2e2/4747/0c0c\x07\x1b]4;3;rgb:0000/0000/0303\x07\
             \x1b]4;16;rgb:0000/0000/0000\x07"
        );
        // A colour the program set still wins over the viewer's.
        e.process(b"\x1b]11;rgb:12/34/56\x07\x1b]11;?\x07");
        assert_eq!(
            String::from_utf8(e.take_replies()).unwrap(),
            "\x1b]11;rgb:1212/3434/5656\x07"
        );
        for bad in [
            ViewerColors::from_hex("#fff", "#000000", "#000000", &ansi),
            ViewerColors::from_hex("#ffffff", "000000", "#000000", &ansi),
            ViewerColors::from_hex("#ffffff", "#000000", "#00000g", &ansi),
            ViewerColors::from_hex("#ffffff", "#000000", "#000000", &ansi[..15]),
        ] {
            assert!(bad.is_err());
        }
    }

    #[test]
    fn the_snapshot_keeps_the_newest_history_within_its_budget() {
        let mut source = Emulator::new(COLS, ROWS, 1000);
        for i in 0..500 {
            source.process(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8).as_bytes());
        }
        source.process("w".repeat(usize::from(COLS) * 3 + 5).as_bytes());
        let full = source.snapshot(usize::MAX);
        let budget = 2000;
        let small = source.snapshot(budget);
        let screen_only = source.snapshot(0);
        assert!(screen_only.len() < small.len() && small.len() < full.len());
        assert!(small.len() < screen_only.len() + budget);
        for snapshot in [&small, &screen_only] {
            let mut client = Emulator::new(COLS, ROWS, 1000);
            client.process(snapshot);
            assert!(client.history_lines() < source.history_lines());
            assert_eq!(
                client.text(TextSource::Visible),
                source.text(TextSource::Visible)
            );
            // Whole logical lines, newest kept.
            let recent = client.text(TextSource::Recent { lines: 1000 }).lines;
            let all = source.text(TextSource::Recent { lines: 1000 }).lines;
            assert_eq!(recent[..], all[all.len() - recent.len()..]);
        }
        let mut client = Emulator::new(COLS, ROWS, 1000);
        client.process(&full);
        assert_same(&mut source, &mut client);
    }
}
