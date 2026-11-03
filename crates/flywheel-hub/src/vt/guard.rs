//! The parser's bounded view of the terminal ([`Guarded`]) and the state
//! `alacritty_terminal` keeps private, mirrored ([`OscScan`], [`Tracked`]).

use super::colors::ViewerColors;
use super::text::HistoryWatch;
use super::{
    Listener, MAX_LINK_ID_BYTES, MAX_LINK_URI_BYTES, MAX_OSC_BYTES, MAX_TITLE_BYTES, MAX_ZEROWIDTH,
    TITLE_STACK_DEPTH, truncated,
};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink as LinkSpec,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode,
    Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use unicode_width::UnicodeWidthChar;

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
pub(crate) struct Guarded<'a> {
    pub(crate) term: &'a mut Term<Listener>,
    pub(crate) titles: &'a mut Vec<Option<String>>,
    pub(crate) listener: &'a Listener,
    pub(crate) colors: &'a ViewerColors,
    pub(crate) history: &'a mut HistoryWatch,
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
        self.history.epoch += 1;
        <Term<Listener> as Handler>::reset_state(self.term);
    }

    fn linefeed(&mut self) {
        self.history.scrolled(1);
        <Term<Listener> as Handler>::linefeed(self.term);
    }

    fn newline(&mut self) {
        self.history.scrolled(1);
        <Term<Listener> as Handler>::newline(self.term);
    }

    fn scroll_up(&mut self, count: usize) {
        self.history.scrolled(count.min(self.term.screen_lines()));
        <Term<Listener> as Handler>::scroll_up(self.term, count);
    }

    fn delete_lines(&mut self, count: usize) {
        self.history.scrolled(count.min(self.term.screen_lines()));
        <Term<Listener> as Handler>::delete_lines(self.term, count);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::Saved => self.history.epoch += 1,
            // On the primary screen, ED 2 scrolls the screen into history.
            _ => self.history.scrolled(self.term.screen_lines()),
        }
        <Term<Listener> as Handler>::clear_screen(self.term, mode);
    }

    fn input(&mut self, c: char) {
        if self.term.grid().cursor.input_needs_wrap {
            self.history.scrolled(1);
        }
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
        bell();
        substitute();
        set_horizontal_tabstop();
        scroll_down(count: usize);
        insert_blank_lines(count: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
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
pub(crate) enum OscScan {
    #[default]
    Other,
    Escape,
    Osc(usize),
    /// Past [`MAX_OSC_BYTES`]: bytes are dropped until the terminator.
    Discard,
}

impl OscScan {
    /// Advance over `byte`; returns whether the parsers should see it.
    pub(crate) fn keep(&mut self, byte: u8) -> bool {
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
pub(crate) struct Tracked {
    rows: usize,
    /// Scroll region rows, 0-based, end exclusive.
    pub(crate) region: (usize, usize),
    pub(crate) shifted_out: bool,
}

impl Tracked {
    pub(crate) fn new(rows: usize) -> Self {
        Self {
            rows,
            region: (0, rows),
            shifted_out: false,
        }
    }

    pub(crate) fn resize(&mut self, rows: usize) {
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
