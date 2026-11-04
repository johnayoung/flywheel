//! Screen frames ([`Emulator::frame`]): what changed on the screen since
//! a viewer's last frame, as escape sequences -- the rows that differ,
//! history rows that scrolled in meanwhile, and the cursor and modes --
//! instead of every byte the program wrote. A viewer on a slow link gets a
//! bounded number of these a second (see `server/terminal_ws.rs`).

use super::Emulator;
use super::snapshot::Writer;
use super::text::{fingerprint, row_address, row_text, wrapped};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use std::hash::{Hash, Hasher};
use std::io::Write as _;

/// History rows one frame sends at most (newest first): output faster
/// than this per frame leaves a gap in the viewer's scrollback (the hub
/// keeps every row; a reattach replays them).
pub const FRAME_HISTORY_ROWS: usize = 500;
/// History bytes one frame sends at most.
const FRAME_HISTORY_BYTES: usize = 64 * 1024;

/// What a viewer's terminal shows, as far as frames know: the state the
/// last frame left it in. A fresh one (or [`FrameCursor::forget`]) knows
/// nothing, so the next frame repaints every row.
#[derive(Debug, Clone, Default)]
pub struct FrameCursor {
    known: bool,
    size: (usize, usize),
    alt: bool,
    epoch: u64,
    scrolls: u64,
    history: usize,
    /// The newest history row's cells' address and fingerprint.
    anchor: usize,
    anchor_print: u64,
    /// Each screen row's rendered bytes, hashed.
    rows: Vec<u64>,
    /// Saved cursor and modes, hashed.
    modes: u64,
    /// Cursor, pen, region, hashed.
    cursor: u64,
}

impl FrameCursor {
    /// Whether the viewer's screen is known (the next frame is a diff).
    pub fn known(&self) -> bool {
        self.known
    }

    /// Know nothing of the viewer's screen any more (it drew something
    /// else): the next frame repaints it.
    pub fn forget(&mut self) {
        self.known = false;
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Puts a viewer's terminal in a known drawing state: synchronized update
/// on, default pen, no link, no origin mode, full scroll region (cursor
/// home), replace mode, ASCII charsets, SI.
const PREFIX: &[u8] =
    b"\x1b[?2026h\x1b[0m\x1b]8;;\x1b\\\x1b[?6l\x1b[r\x1b[4l\x1b(B\x1b)B\x1b*B\x1b+B\x0f";
/// Ends the synchronized update.
const SUFFIX: &[u8] = b"\x1b[?2026l";
/// Every mode a frame's mode part may set, back at its reset value (the
/// part then sets those in force), plus the default cursor style, an empty
/// title, and the default palette.
const MODES_RESET: &[u8] = b"\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\
\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[?7h\x1b>\x1b[20l\x1b[0 q\x1b]2;\x07\x1b]104\x07\
\x1b]110\x07\x1b]111\x07\x1b]112\x07";

impl Emulator {
    /// One screen row as escape sequences (from column 1, default pen at
    /// both ends, no line break).
    fn row_bytes(&self, line: Line) -> Vec<u8> {
        let mut w = Writer::default();
        w.screen_row(self.term.grid(), line);
        w.out
    }

    /// Rows scrolled into the primary screen's history since `cursor`'s
    /// frame, when that can be told (else `None`: more than history holds,
    /// or the rows were rewritten).
    fn scrolled_since(&self, cursor: &FrameCursor) -> Option<usize> {
        let grid = self.term.grid();
        let history = grid.history_size();
        let bound = self.history.scrolls().saturating_sub(cursor.scrolls);
        if bound == 0 || history == 0 {
            return Some(0);
        }
        if cursor.history == 0 {
            // Nothing was in history: everything there now is new.
            return Some(history);
        }
        let bound = usize::try_from(bound).unwrap_or(usize::MAX);
        (0..=bound.min(history - 1)).find(|&s| {
            let line = Line(-1 - s as i32);
            row_address(grid, line) == cursor.anchor
                && fingerprint(&row_text(grid, line), wrapped(grid, line)) == cursor.anchor_print
        })
    }

    /// What changed since `cursor`'s last frame, as escape sequences that
    /// bring a viewer showing that frame to this terminal's screen, cursor
    /// and modes; `None` when nothing did. Updates `cursor`. A cursor that
    /// knows nothing (or a switch of screens, a resize, a reset) repaints
    /// every row without clearing the viewer's scrollback.
    ///
    /// Rows that scrolled into history since are sent too, so they reach
    /// the viewer's scrollback (at most [`FRAME_HISTORY_ROWS`]); what
    /// leaves the screen otherwise (a full-screen program redrawing) is
    /// only repainted. Soft wraps across rows are not carried (each row is
    /// drawn alone): a viewer that resizes gets a full repaint anyway.
    pub fn frame(&mut self, cursor: &mut FrameCursor) -> Option<Vec<u8>> {
        self.flush();
        let (cols, rows) = self.size();
        let alt = self.alt_screen();
        let epoch = self.history.epoch;
        let history = self.term.grid().history_size();
        let blind = !cursor.known
            || cursor.alt != alt
            || cursor.size != (cols, rows)
            || cursor.epoch != epoch;

        let row_bytes: Vec<Vec<u8>> = (0..rows).map(|r| self.row_bytes(Line(r as i32))).collect();
        let row_prints: Vec<u64> = row_bytes.iter().map(|b| hash_bytes(b)).collect();
        let mut modes = Writer::default();
        self.write_saved(&mut modes);
        self.write_modes(&mut modes);
        modes.plain();
        let modes_print = hash_bytes(&modes.out);
        let mut place = Writer::default();
        self.write_cursor(&mut place);
        if self
            .term
            .mode()
            .contains(alacritty_terminal::term::TermMode::INSERT)
        {
            place.raw(b"\x1b[4h");
        }
        let place_print = hash_bytes(&place.out);

        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(PREFIX);
        let mut drew = false;
        if blind {
            out.extend_from_slice(if alt { b"\x1b[?1049h" } else { b"\x1b[?1049l" });
            if cursor.known && !alt && !cursor.alt && history == 0 && cursor.history > 0 {
                // Its history was cleared (a reset, ED 3): so is theirs.
                out.extend_from_slice(b"\x1b[3J");
            }
            for (r, bytes) in row_bytes.iter().enumerate() {
                let _ = write!(out, "\x1b[{};1H\x1b[2K", r + 1);
                out.extend_from_slice(bytes);
            }
            drew = true;
        } else {
            let scrolled = if alt {
                Some(0)
            } else {
                self.scrolled_since(cursor)
            };
            // Unknown: everything in history may be new.
            let scrolled = scrolled.unwrap_or(history);
            if scrolled > 0 {
                let send = scrolled.min(FRAME_HISTORY_ROWS).min(history);
                // The rows that scrolled off are the ones the viewer has
                // at the top: scroll them into its history in place.
                let in_place = scrolled == send
                    && scrolled <= rows
                    && (0..scrolled).all(|i| {
                        let line = Line(i as i32 - scrolled as i32);
                        hash_bytes(&self.row_bytes(line)) == cursor.rows[i]
                    });
                if in_place {
                    let _ = write!(out, "\x1b[{rows};1H");
                    out.extend(std::iter::repeat_n(b'\n', scrolled));
                    let blank = hash_bytes(&[]);
                    cursor.rows.drain(..scrolled);
                    cursor.rows.extend(std::iter::repeat_n(blank, scrolled));
                } else {
                    // Written from the top, the history rows then the
                    // screen scroll the former into the viewer's history.
                    let mut lines: Vec<Vec<u8>> = Vec::new();
                    let mut spent = 0;
                    for back in 1..=send {
                        let bytes = self.row_bytes(Line(-(back as i32)));
                        spent += bytes.len() + 6;
                        if spent > FRAME_HISTORY_BYTES {
                            break;
                        }
                        lines.push(bytes);
                    }
                    out.extend_from_slice(b"\x1b[H");
                    for (i, bytes) in lines.iter().rev().chain(row_bytes.iter()).enumerate() {
                        if i > 0 {
                            out.extend_from_slice(b"\r\n");
                        }
                        out.extend_from_slice(b"\x1b[2K");
                        out.extend_from_slice(bytes);
                    }
                    cursor.rows.clone_from(&row_prints);
                    drew = true;
                }
            }
            for (r, bytes) in row_bytes.iter().enumerate() {
                if cursor.rows.get(r) != Some(&row_prints[r]) {
                    let _ = write!(out, "\x1b[{};1H\x1b[2K", r + 1);
                    out.extend_from_slice(bytes);
                    drew = true;
                }
            }
            if scrolled > 0 {
                drew = true;
            }
        }
        let modes_changed = blind || modes_print != cursor.modes;
        if modes_changed {
            out.extend_from_slice(MODES_RESET);
            out.extend_from_slice(&modes.out);
        }
        let changed = drew || modes_changed || place_print != cursor.cursor;

        let grid = self.term.grid();
        *cursor = FrameCursor {
            known: true,
            size: (cols, rows),
            alt,
            epoch,
            scrolls: self.history.scrolls(),
            history,
            anchor: if history > 0 {
                row_address(grid, Line(-1))
            } else {
                0
            },
            anchor_print: if history > 0 {
                fingerprint(&row_text(grid, Line(-1)), wrapped(grid, Line(-1)))
            } else {
                0
            },
            rows: row_prints,
            modes: modes_print,
            cursor: place_print,
        };
        if !changed {
            return None;
        }
        out.extend_from_slice(&place.out);
        out.extend_from_slice(SUFFIX);
        Some(out)
    }

    /// Bring `cursor` to this terminal's state without drawing: the viewer
    /// got there by other means (every output byte since its last frame).
    pub fn sync_cursor(&mut self, cursor: &mut FrameCursor) {
        let _ = self.frame(cursor);
    }
}
