//! Writing a screen as escape sequences ([`Writer`]): what a snapshot
//! replays into a freshly reset terminal.

use super::text::{printable, sanitize, wrapped};
use super::{STYLE_FLAGS, VISIBLE_BLANK_FLAGS};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{CharsetIndex, Color, NamedColor, StandardCharset};
use std::io::Write as _;

/// The SGR state a cell is drawn with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pen {
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
    pub(crate) fn of(cell: &Cell) -> Self {
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
pub(crate) fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\t')
        && Pen::of(cell).bg == Pen::default().bg
        && !cell.flags.intersects(VISIBLE_BLANK_FLAGS)
        && cell.zerowidth().is_none()
        && cell.hyperlink().is_none()
}

/// Accumulates snapshot bytes, emitting SGR / OSC 8 only on change.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
    pen: Pen,
    link: Option<Hyperlink>,
}

impl Writer {
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// Absolute cursor position.
    pub(crate) fn cup(&mut self, point: Point) {
        let row = usize::try_from(point.line.0).unwrap_or(0);
        let _ = write!(self.out, "\x1b[{};{}H", row + 1, point.column.0 + 1);
    }

    pub(crate) fn pen(&mut self, cell: &Cell) {
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

    pub(crate) fn link(&mut self, link: Option<Hyperlink>) {
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

    pub(crate) fn cell(&mut self, cell: &Cell) {
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
    pub(crate) fn plain(&mut self) {
        self.link(None);
        if self.pen != Pen::default() {
            self.raw(b"\x1b[0m");
            self.pen = Pen::default();
        }
    }

    pub(crate) fn charsets(&mut self, charsets: &alacritty_terminal::grid::Charsets) {
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
    pub(crate) fn rows(&mut self, grid: &Grid<Cell>, from: Line, history_budget: usize) {
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

impl Writer {
    /// One screen row drawn alone (a frame's row): like a one-row logical
    /// line, but a wide character's leading spacer (it did not fit at the
    /// end, so it starts the next row) is drawn as the blank it shows.
    pub(crate) fn screen_row(&mut self, grid: &Grid<Cell>, line: Line) {
        let row = &grid[line];
        for col in 0..row_end(grid, line) {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                let mut blank = cell.clone();
                blank.flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
                blank.c = ' ';
                self.cell(&blank);
                continue;
            }
            self.cell(cell);
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
pub(crate) fn color(out: &mut Vec<u8>, color: Color, base: u8, bright: u8, ext: u8) {
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
