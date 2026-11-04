use super::snapshot::{Pen, is_blank};
use super::text::printable;
use super::*;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::{Color, Rgb};

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
    out.extend(
        b"\x1b[4:3;58;2;255;0;0mcurly\x1b[0m \x1b[3;2mit\x1b[7minv\x1b[9mst\x1b[8mhid\x1b[0m\r\n",
    );
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
    out.extend(b"\x1b]2;vim - file\x07\x1b[6 q\x1b]4;1;rgb:12/34/56\x07\x1b]11;rgb:00/00/20\x07");
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
fn search_text_holds_every_primary_line_also_under_the_alternate_screen() {
    let mut e = Emulator::new(10, 4, 100);
    for i in 0..20 {
        e.process(format!("row {i}\r\n").as_bytes());
    }
    // A soft-wrapped line is one logical line.
    e.process(b"0123456789abcdef\r\n");
    let text = e.search_text();
    assert_eq!(text.alternate, None);
    assert_eq!(text.primary.len(), 21, "{:?}", text.primary);
    assert_eq!(&text.primary[0], "row 0");
    assert_eq!(&text.primary[20], "0123456789abcdef");
    e.process(b"\x1b[?1049h\x1b[Hfull screen app");
    let text = e.search_text();
    assert_eq!(text.primary.len(), 21);
    assert_eq!(&text.primary[20], "0123456789abcdef");
    assert_eq!(text.alternate.unwrap()[0], "full screen app");
    // Reading it changed nothing on screen.
    assert_eq!(
        e.text(TextSource::Visible).lines[..2],
        ["full scree", "n app"]
    );
    assert!(e.alt_screen());
    e.process(b"\x1b[?1049l");
    assert_eq!(e.search_text().primary.len(), 21);
}

/// [`Emulator::search_text`] read from scratch (no reuse).
fn search_text_afresh(e: &mut Emulator) -> SearchText {
    e.flush();
    if !e.alt_screen() {
        return SearchText {
            primary: recent_lines(e.term.grid(), usize::MAX)
                .into_iter()
                .collect(),
            alternate: None,
        };
    }
    let alt = e.term.grid().clone();
    let alternate = recent_lines(&alt, usize::MAX);
    e.term.swap_alt();
    let primary = recent_lines(e.term.grid(), usize::MAX)
        .into_iter()
        .collect();
    e.term.swap_alt();
    *e.term.grid_mut() = alt;
    SearchText {
        primary,
        alternate: Some(alternate),
    }
}

#[test]
fn search_text_reuses_history_and_matches_a_fresh_read_through_any_output() {
    // A fixed xorshift: the same run every time.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut e = Emulator::new(12, 5, 30);
    let mut counter = 0u32;
    let (mut reused_reads, mut full_reads) = (0, 0);
    for step in 0..3000 {
        let op = next(100);
        let mut out = String::new();
        match op {
            // Lines: short, wrapping, wide, blank.
            0..=49 => {
                for _ in 0..=next(12) {
                    counter += 1;
                    match next(5) {
                        0 => out
                            .push_str(&format!("{counter} {}\r\n", "w".repeat(next(30) as usize))),
                        1 => out.push_str(&format!(
                            "{counter} \u{6f22}\u{5b57}\u{6f22}\u{5b57}\u{6f22}\u{5b57}\r\n"
                        )),
                        2 => out.push_str("\r\n"),
                        3 => out.push_str(&format!("{counter}   \r\n")),
                        _ => out.push_str(&format!("line {counter}\r\n")),
                    }
                }
            }
            // A partial line, then an overwrite of it.
            50..=57 => out.push_str(&format!("part {counter}\rPART")),
            58..=61 => out.push_str(&format!("\x1b[{}S", 1 + next(8))),
            62..=64 => out.push_str(&format!("\x1b[H\x1b[{}M", 1 + next(4))),
            65..=68 => out.push_str(&format!(
                "\x1b[1;{}r\x1b[5;1Hregion {counter}\r\n\r\n",
                2 + next(3)
            )),
            69..=72 => out.push_str("\x1b[r\x1b[5;1H"),
            73..=75 => out.push_str("\x1b[2J"),
            76 => out.push_str("\x1b[3J"),
            77 => out.push_str("\x1bc"),
            78..=80 => out.push_str(&format!("\x1b[?1049h\x1b[Halt {counter}\r\nmore\r\n")),
            81..=83 => out.push_str("\x1b[?1049l"),
            84..=86 => out.push_str("\x1b[A\x1b[2Kup"),
            87..=88 => {
                let (cols, rows) = (8 + next(8) as u16, 3 + next(5) as u16);
                e.resize(cols, rows);
            }
            // A flood: more than the history holds.
            89..=90 => {
                for _ in 0..40 {
                    counter += 1;
                    out.push_str(&format!("flood {counter}\r\n"));
                }
            }
            _ => {}
        }
        e.process(out.as_bytes());
        if next(3) == 0 || step % 50 == 0 {
            let reused = e.search_text();
            if e.history_lines() > e.history_text.read {
                reused_reads += 1;
            } else if e.history_lines() > 0 {
                full_reads += 1;
            }
            let fresh = search_text_afresh(&mut e);
            assert_eq!(reused, fresh, "step {step} (op {op}, {out:?})");
        }
    }
    // Both paths ran, many times.
    assert!(
        reused_reads > 200 && full_reads > 50,
        "{reused_reads} reused, {full_reads} full"
    );
}

#[test]
fn search_text_reads_only_rows_scrolled_in_since_the_last_read() {
    let mut e = Emulator::new(80, 24, 5000);
    for i in 0..6000 {
        e.process(format!("line {i} of a long build log\r\n").as_bytes());
    }
    let first = e.search_text();
    assert_eq!(e.history_text.read, 5000);
    assert_eq!(
        first.primary.last().unwrap(),
        "line 5999 of a long build log"
    );
    // Nothing new: nothing read (the screen is always read).
    e.search_text();
    assert_eq!(e.history_text.read, 0);
    for i in 6000..6010 {
        e.process(format!("line {i} of a long build log\r\n").as_bytes());
    }
    let text = e.search_text();
    assert_eq!(e.history_text.read, 10);
    assert_eq!(text, search_text_afresh(&mut e));
    // 5000 history rows plus 23 screen rows above the empty cursor row.
    assert_eq!(&text.primary[0], "line 987 of a long build log");
    // A resize reflows: everything is read again.
    e.resize(100, 24);
    e.search_text();
    assert!(e.history_text.read > 4000, "{}", e.history_text.read);
}

/// Timing of a search read over a full scrollback, from scratch and
/// reused: `cargo test --release -p flywheel-hub --lib
/// bench_search_text -- --ignored --nocapture`.
#[test]
#[ignore = "a benchmark: run by hand in release"]
fn bench_search_text() {
    let mut e = Emulator::new(120, 40, DEFAULT_SCROLLBACK_LINES);
    let line = |i: usize| {
        format!(
            "compiling crate foo v0.1.{i} (/src/foo) {}\r\n",
            "x".repeat(i % 60)
        )
    };
    for i in 0..6000 {
        e.process(line(i).as_bytes());
    }
    let rounds = 50u32;
    let started = Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(search_text_afresh(&mut e));
    }
    let fresh = started.elapsed() / rounds;
    let mut i = 6000;
    let started = Instant::now();
    for _ in 0..rounds {
        // About what a chatty build prints between two searches.
        for _ in 0..100 {
            e.process(line(i).as_bytes());
            i += 1;
        }
        std::hint::black_box(e.search_text());
    }
    let reused = started.elapsed() / rounds;
    let started = Instant::now();
    for _ in 0..rounds {
        for _ in 0..100 {
            e.process(line(i).as_bytes());
            i += 1;
        }
    }
    let printing = started.elapsed() / rounds;
    eprintln!(
        "search text over {DEFAULT_SCROLLBACK_LINES} lines: afresh {fresh:?}; after 100 new \
         lines, reused {:?} (printing them alone {printing:?})",
        reused.saturating_sub(printing)
    );
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
    // What docs/hub/reference.md sizes memory by (24 B a cell): a session's grid
    // stays under the budget's bytes whatever it prints.
    assert_eq!(std::mem::size_of::<Cell>(), 24);
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

// --- frames -----------------------------------------------------------------

/// The screen as a viewer sees it: every cell of the active screen (soft
/// wraps aside: frames draw rows alone), the cursor, pen, modes, region,
/// cursor style, title (empty is none), and palette.
fn assert_same_view(a: &mut Emulator, b: &mut Emulator) {
    a.flush();
    b.flush();
    let (ga, gb) = (a.term.grid(), b.term.grid());
    assert_eq!(
        (ga.columns(), ga.screen_lines()),
        (gb.columns(), gb.screen_lines())
    );
    for line in 0..ga.screen_lines() as i32 {
        for col in 0..ga.columns() {
            let (ca, cb) = (&ga[Line(line)][Column(col)], &gb[Line(line)][Column(col)]);
            let strip = |k: CellKey| {
                k.map(|(c, fg, bg, f, z, u, l)| {
                    (
                        c,
                        fg,
                        bg,
                        f - Flags::WRAPLINE - Flags::LEADING_WIDE_CHAR_SPACER,
                        z,
                        u,
                        l,
                    )
                })
            };
            assert_eq!(
                strip(cell_key(ca)),
                strip(cell_key(cb)),
                "cell {line},{col}"
            );
        }
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
    let title = |e: &Emulator| e.title().filter(|t| !t.is_empty());
    assert_eq!(title(a), title(b), "title");
    for i in 0..259 {
        assert_eq!(a.term.colors()[i], b.term.colors()[i], "colour {i}");
    }
}

/// Feed `steps` to a source, sending a frame to a viewer after each; the
/// viewer must show what the source does after every frame. Returns both
/// and the frame bytes sent.
fn framed(steps: &[Vec<u8>]) -> (Emulator, Emulator, usize) {
    let mut source = Emulator::new(COLS, ROWS, 1000);
    let mut viewer = Emulator::new(COLS, ROWS, 1000);
    let mut cursor = FrameCursor::default();
    let mut sent = 0;
    for (i, step) in steps.iter().enumerate() {
        source.process(step);
        if let Some(frame) = source.frame(&mut cursor) {
            sent += frame.len();
            viewer.process(&frame);
        }
        let _ = viewer.take_replies();
        assert!(cursor.known());
        assert_same_view(&mut source, &mut viewer);
        let _ = i;
    }
    (source, viewer, sent)
}

fn recent(e: &mut Emulator) -> Vec<String> {
    e.text(TextSource::Recent { lines: 10_000 }).lines
}

#[test]
fn frames_carry_scrolling_output_into_the_viewers_history() {
    // A log a line or a few at a time (the rows scroll in place), then
    // bursts taller than the screen (written out whole).
    let mut steps = Vec::new();
    for i in 0..60 {
        steps.push(format!("\x1b[32mline {i}\x1b[0m tail\r\n").into_bytes());
        if i % 7 == 0 {
            steps.push(format!("a{i}\r\nb{i}\r\nc{i}\r\n").into_bytes());
        }
    }
    let mut burst = Vec::new();
    for i in 0..25 {
        burst.extend(format!("burst {i}\r\n").as_bytes());
    }
    steps.push(burst);
    steps.push(b"$ ".to_vec());
    let (mut source, mut viewer, _) = framed(&steps);
    assert_eq!(recent(&mut viewer), recent(&mut source));
    assert_eq!(viewer.history_lines(), source.history_lines());
}

#[test]
fn an_unchanged_screen_sends_no_frame_and_a_spinner_one_row() {
    let mut source = Emulator::new(COLS, ROWS, 1000);
    let mut viewer = Emulator::new(COLS, ROWS, 1000);
    let mut cursor = FrameCursor::default();
    source.process(b"\x1b[1mtitle\x1b[0m\r\nsome text\r\n\r\n");
    viewer.process(&source.frame(&mut cursor).expect("first frame"));
    assert!(source.frame(&mut cursor).is_none(), "nothing changed");
    let mut sizes = Vec::new();
    for s in ["|", "/", "-", "\\"] {
        source.process(format!("\r\x1b[2K\x1b[38;5;208m{s}\x1b[0m Working... (3s)").as_bytes());
        let frame = source.frame(&mut cursor).expect("a spinner frame");
        sizes.push(frame.len());
        viewer.process(&frame);
        assert_same_view(&mut source, &mut viewer);
    }
    // The prefix, one row and the cursor: far less than the screen.
    assert!(sizes.iter().all(|&n| n < 200), "{sizes:?}");
}

#[test]
fn frames_follow_screens_modes_regions_titles_and_resets() {
    let steps: Vec<Vec<u8>> = vec![
        b"prompt $ ls\r\nfile1 file2\r\n".to_vec(),
        // A full-screen program: alternate screen, modes, a title.
        b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b]2;vim\x07\x1b[H\x1b[2Jvim buffer\x1b[5;3H\x1b[7mstatus\x1b[0m".to_vec(),
        b"\x1b[2;10r\x1b[10;1Hscrolled\nin region\n".to_vec(),
        b"\x1b[?25l\x1b[3 q\x1b]4;1;rgb:12/34/56\x07".to_vec(),
        // Back: modes off again, the primary screen as it was.
        b"\x1b[r\x1b[?25h\x1b[0 q\x1b[?1000l\x1b[?1006l\x1b[?2004l\x1b>\x1b[?1l\x1b[?1049l\x1b]104\x07\x1b]2;shell\x07".to_vec(),
        b"\x1b7\x1b[4;4H\x1b[31msaved\x1b8 after\r\n".to_vec(),
        b"\x1b(0lqqk\x1b(B \x0e\x0f done\r\n".to_vec(),
        b"\x1b[4hinsert\x1b[4l".to_vec(),
        // A full-width row leaves the cursor pending a wrap.
        "x".repeat(usize::from(COLS)).into_bytes(),
        // A reset clears history too.
        b"\x1bcafter reset\r\n".to_vec(),
        "\u{4f60}\u{597d} e\u{301} \x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\r\n".as_bytes().to_vec(),
    ];
    let (_source, viewer, _) = framed(&steps);
    assert_eq!(
        viewer.history_lines(),
        0,
        "the reset cleared the viewer's history"
    );
}

#[test]
fn a_frame_after_a_resize_or_a_forgotten_viewer_repaints_without_a_reset() {
    let mut source = Emulator::new(COLS, ROWS, 1000);
    let mut viewer = Emulator::new(COLS, ROWS, 1000);
    let mut cursor = FrameCursor::default();
    for i in 0..30 {
        source.process(format!("old {i}\r\n").as_bytes());
    }
    viewer.process(&source.snapshot(SNAPSHOT_HISTORY_BYTES));
    // The viewer has the screen by a snapshot: a cursor that never drew
    // repaints it, keeping the history the viewer had.
    source.process(b"new\r\n");
    viewer.process(&source.frame(&mut cursor).expect("repaint"));
    assert_same_view(&mut source, &mut viewer);
    assert!(viewer.history_lines() >= 20, "{}", viewer.history_lines());
    source.resize(30, 12);
    viewer.resize(30, 12);
    source.process(b"wider no, taller\r\n");
    viewer.process(&source.frame(&mut cursor).expect("resized"));
    assert_same_view(&mut source, &mut viewer);
    cursor.forget();
    viewer.process(b"\x1b[5;5Hscribble\x1b[?25l");
    viewer.process(&source.frame(&mut cursor).expect("forgotten"));
    assert_same_view(&mut source, &mut viewer);
}

#[test]
fn raw_bytes_then_a_synced_cursor_keep_frames_exact() {
    // A viewer that got the raw bytes (a quiet link): the cursor is
    // brought along without drawing, and later frames diff against it.
    let mut source = Emulator::new(COLS, ROWS, 1000);
    let mut viewer = Emulator::new(COLS, ROWS, 1000);
    let mut cursor = FrameCursor::default();
    viewer.process(&source.snapshot(SNAPSHOT_HISTORY_BYTES));
    source.sync_cursor(&mut cursor);
    for i in 0..40 {
        let bytes = format!("raw {i}\r\n\x1b[2;2Hx{i}\x1b[{};1H", 10).into_bytes();
        source.process(&bytes);
        if i % 3 == 0 {
            viewer.process(&bytes);
            source.sync_cursor(&mut cursor);
        } else if let Some(frame) = source.frame(&mut cursor) {
            viewer.process(&frame);
        }
        assert_same_view(&mut source, &mut viewer);
    }
}

#[test]
fn frames_track_random_output() {
    // Pseudo-random mixes of text, colours, cursor moves, erases, scroll
    // regions, inserts and deletes, wide characters and line feeds.
    let mut seed: u64 = 0x5eed_f00d;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    let pieces: &[&str] = &[
        "hello",
        "\r\n",
        "\n",
        "\x1b[31m",
        "\x1b[0m",
        "\x1b[1;44m",
        "\x1b[K",
        "\x1b[2J",
        "\x1b[H",
        "\x1b[3;7H",
        "\x1b[L",
        "\x1b[M",
        "\x1b[@",
        "\x1b[P",
        "\x1b[2;8r",
        "\x1b[r",
        "\u{4f60}",
        "\t",
        "\x1b[S",
        "\x1b[T",
        "\x1b[?25l",
        "\x1b[?25h",
        "\x1b[10;40H",
        "wrap me around please",
        "\x1bM",
        "\x1b[J",
        "\x1b[1K",
        "\x1b[38;2;1;2;3m",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x08",
    ];
    for _ in 0..8 {
        let mut steps = Vec::new();
        for _ in 0..60 {
            let mut step = Vec::new();
            for _ in 0..=next(6) {
                step.extend_from_slice(pieces[next(pieces.len() as u64) as usize].as_bytes());
            }
            steps.push(step);
        }
        framed(&steps);
    }
}

#[test]
fn frames_of_a_busy_screen_cost_less_than_its_bytes() {
    // An agent redrawing its status line many times between frames: the
    // frame carries the last state only.
    let mut steps = Vec::new();
    for i in 0..20 {
        let mut step = Vec::new();
        for j in 0..50 {
            step.extend(format!("\r\x1b[2K\x1b[38;5;208m*\x1b[0m Working {i}.{j} \x1b[2m(esc to interrupt)\x1b[0m").as_bytes());
        }
        steps.push(step);
    }
    let raw: usize = steps.iter().map(Vec::len).sum();
    let (_, _, sent) = framed(&steps);
    assert!(sent * 10 < raw, "frames {sent} B vs raw {raw} B");
}
