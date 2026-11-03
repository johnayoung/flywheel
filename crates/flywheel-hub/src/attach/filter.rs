//! What `fw hub attach` passes from a session to the real terminal.
//!
//! Output goes through byte for byte, except:
//!
//! * **Terminal queries are dropped** (device attributes, cursor position
//!   and status reports, mode and setting requests, colour and clipboard
//!   reads, size reports, ENQ, kitty keyboard flags). The hub's emulator
//!   answers every query once per session; were the real terminal to
//!   answer too, its reply would arrive as typed input. Kitty keyboard
//!   pushes are dropped with the query: the hub does not emulate the
//!   protocol, so keys must stay legacy-encoded for every viewer.
//! * **With the status bar on**, the session's PTY is one row shorter than
//!   the terminal and the bar's row sits below a scroll region. Sequences
//!   that would reach past the session's rows are clamped to them
//!   (`DECSTBM` bottoms, `CUP`/`HVP`/`VPA` rows), a full or soft reset is
//!   followed by the scroll region again, and anything that may have
//!   erased the bar (erase in display, reset, alternate screen switches,
//!   `DECALN`, `DECCOLM`) marks it damaged so it is redrawn.
//!
//! The filter is a streaming parser: a sequence split across reads is held
//! until it completes (bounded: an over-long control string is passed
//! through as it comes), and so is a trailing partial UTF-8 character, so
//! between feeds the terminal is always at a character boundary -- where
//! the bar can be drawn.

/// Longest CSI / escape sequence held (longer: passed through as is).
const MAX_SEQUENCE: usize = 512;
/// Longest control string (OSC, DCS header) held to decide on it.
const MAX_STRING: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const ENQ: u8 = 0x05;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Str {
    Osc,
    Dcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC (with any intermediates in `held`).
    Escape,
    Csi,
    /// Holding a control string to decide on it.
    Held(Str),
    /// Inside a control string being passed through (`drop`: discarded).
    Passing {
        drop: bool,
    },
    /// ESC seen inside a passed-through string: `\` ends it.
    PassingEsc {
        drop: bool,
    },
    /// ESC seen inside a held string.
    HeldEsc(Str),
}

/// The bytes to write, and whether the status bar's row may have been
/// overwritten.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    pub out: Vec<u8>,
    pub bar_damaged: bool,
}

#[derive(Debug)]
pub struct OutputFilter {
    state: State,
    held: Vec<u8>,
    /// A trailing partial UTF-8 character from the last feed.
    carry: Vec<u8>,
    /// The session's rows when the status bar is below them.
    rows: Option<u16>,
}

impl OutputFilter {
    /// A filter for a terminal whose status bar (when `rows` is set) sits
    /// below the session's `rows`.
    pub fn new(rows: Option<u16>) -> Self {
        Self {
            state: State::Ground,
            held: Vec::new(),
            carry: Vec::new(),
            rows,
        }
    }

    /// The session's rows above the bar, or `None` without a bar.
    pub fn set_rows(&mut self, rows: Option<u16>) {
        self.rows = rows.filter(|r| *r > 0);
    }

    /// Whether nothing is held: the terminal is between sequences and
    /// characters.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground && self.carry.is_empty()
    }

    /// Forget anything held (a new snapshot follows).
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.held.clear();
        self.carry.clear();
    }

    pub fn feed(&mut self, input: &[u8]) -> Filtered {
        let mut f = Filtered::default();
        let carried = std::mem::take(&mut self.carry);
        for &byte in carried.iter().chain(input) {
            self.byte(byte, &mut f);
        }
        if self.state == State::Ground {
            let keep = partial_utf8_tail(&f.out);
            if keep > 0 {
                let at = f.out.len() - keep;
                self.carry = f.out.split_off(at);
            }
        }
        f
    }

    fn byte(&mut self, byte: u8, f: &mut Filtered) {
        match self.state {
            State::Ground => match byte {
                ESC => self.start(byte, State::Escape),
                ENQ => {}
                _ => f.out.push(byte),
            },
            State::Escape => match byte {
                b'[' if self.held.len() == 1 => {
                    self.held.push(byte);
                    self.state = State::Csi;
                }
                b']' if self.held.len() == 1 => self.hold_string(byte, Str::Osc),
                b'P' if self.held.len() == 1 => self.hold_string(byte, Str::Dcs),
                b'X' | b'^' | b'_' if self.held.len() == 1 => {
                    // SOS / PM / APC (kitty graphics, ...): not inspected,
                    // passed through as they come.
                    self.held.push(byte);
                    f.out.append(&mut self.held);
                    self.state = State::Passing { drop: false };
                }
                0x20..=0x2f => self.hold(byte, f),
                0x30..=0x7e => {
                    self.held.push(byte);
                    self.escape_done(f);
                }
                ESC => {
                    f.out.append(&mut self.held);
                    self.start(byte, State::Escape);
                }
                CAN | SUB => {
                    self.held.clear();
                    f.out.push(byte);
                    self.state = State::Ground;
                }
                // C0 controls inside a sequence act at once.
                _ => f.out.push(byte),
            },
            State::Csi => match byte {
                0x40..=0x7e => {
                    self.held.push(byte);
                    self.csi_done(f);
                }
                0x20..=0x3f => self.hold(byte, f),
                ESC => {
                    f.out.append(&mut self.held);
                    self.start(byte, State::Escape);
                }
                CAN | SUB => {
                    self.held.clear();
                    f.out.push(byte);
                    self.state = State::Ground;
                }
                _ => f.out.push(byte),
            },
            State::Held(kind) => match byte {
                BEL if kind == Str::Osc => {
                    self.held.push(byte);
                    self.string_done(kind, f);
                }
                ESC => {
                    self.held.push(byte);
                    self.state = State::HeldEsc(kind);
                }
                CAN | SUB => {
                    self.held.clear();
                    f.out.push(byte);
                    self.state = State::Ground;
                }
                _ => {
                    self.held.push(byte);
                    if kind == Str::Dcs && (0x40..=0x7e).contains(&byte) && self.held.len() > 2 {
                        // The DCS header is complete: decide now.
                        let drop = dcs_is_query(&self.held[2..]);
                        if drop {
                            self.held.clear();
                        } else {
                            f.out.append(&mut self.held);
                        }
                        self.state = State::Passing { drop };
                    } else if self.held.len() > MAX_STRING {
                        f.out.append(&mut self.held);
                        self.state = State::Passing { drop: false };
                    }
                }
            },
            State::HeldEsc(kind) => {
                if byte == b'\\' {
                    self.held.push(byte);
                    self.string_done(kind, f);
                } else {
                    // The string was cut off by a new sequence.
                    self.held.pop();
                    f.out.append(&mut self.held);
                    self.state = State::Ground;
                    self.start(ESC, State::Escape);
                    self.byte(byte, f);
                }
            }
            State::Passing { drop } => match byte {
                BEL => {
                    if !drop {
                        f.out.push(byte);
                    }
                    self.state = State::Ground;
                }
                ESC => self.state = State::PassingEsc { drop },
                CAN | SUB => {
                    f.out.push(byte);
                    self.state = State::Ground;
                }
                _ => {
                    if !drop {
                        f.out.push(byte);
                    }
                }
            },
            State::PassingEsc { drop } => {
                if byte == b'\\' {
                    if !drop {
                        f.out.extend_from_slice(b"\x1b\\");
                    }
                    self.state = State::Ground;
                } else {
                    if !drop {
                        // ESC ends the string; what follows is a sequence.
                        f.out.extend_from_slice(b"\x1b\\");
                    }
                    self.state = State::Ground;
                    self.start(ESC, State::Escape);
                    self.byte(byte, f);
                }
            }
        }
    }

    fn start(&mut self, byte: u8, state: State) {
        self.held.clear();
        self.held.push(byte);
        self.state = state;
    }

    fn hold_string(&mut self, byte: u8, kind: Str) {
        self.held.push(byte);
        self.state = State::Held(kind);
    }

    /// Add to a held sequence; one grown absurdly long goes through as is.
    fn hold(&mut self, byte: u8, f: &mut Filtered) {
        self.held.push(byte);
        if self.held.len() > MAX_SEQUENCE {
            f.out.append(&mut self.held);
            self.state = State::Ground;
        }
    }

    fn escape_done(&mut self, f: &mut Filtered) {
        let seq = std::mem::take(&mut self.held);
        self.state = State::Ground;
        f.out.extend_from_slice(&seq);
        match &seq[1..] {
            // RIS: everything reset, the scroll region included.
            b"c" => {
                f.bar_damaged = true;
                self.region(f);
            }
            // DECALN fills the screen.
            b"#8" => f.bar_damaged = true,
            _ => {}
        }
    }

    fn region(&self, f: &mut Filtered) {
        if let Some(rows) = self.rows {
            f.out
                .extend_from_slice(format!("\x1b[1;{rows}r").as_bytes());
        }
    }

    fn csi_done(&mut self, f: &mut Filtered) {
        let seq = std::mem::take(&mut self.held);
        self.state = State::Ground;
        let csi = Csi::parse(&seq[2..]);
        if csi.is_query() {
            return;
        }
        let Some(rows) = self.rows else {
            f.out.extend_from_slice(&seq);
            return;
        };
        match (csi.prefix, csi.intermediates, csi.finale) {
            // DECSTBM: keep the bottom margin above the bar.
            (None, b"", b'r') => {
                let top = csi.param(0).unwrap_or(1).max(1);
                let bottom = match csi.param(1) {
                    Some(b) if b >= 1 && b <= u32::from(rows) => b,
                    _ => u32::from(rows),
                };
                f.out
                    .extend_from_slice(format!("\x1b[{top};{bottom}r").as_bytes());
            }
            // CUP / HVP: no row below the session's.
            (None, b"", b'H' | b'f') => match csi.param(0) {
                Some(row) if row > u32::from(rows) => {
                    let col = csi.param(1).map(|c| format!(";{c}")).unwrap_or_default();
                    let finale = char::from(csi.finale);
                    f.out
                        .extend_from_slice(format!("\x1b[{rows}{col}{finale}").as_bytes());
                }
                _ => f.out.extend_from_slice(&seq),
            },
            // VPA.
            (None, b"", b'd') => match csi.param(0) {
                Some(row) if row > u32::from(rows) => {
                    f.out.extend_from_slice(format!("\x1b[{rows}d").as_bytes());
                }
                _ => f.out.extend_from_slice(&seq),
            },
            // ED / DECSED (all but "to the cursor").
            (None | Some(b'?'), b"", b'J') => {
                f.out.extend_from_slice(&seq);
                if csi.param(0) != Some(1) {
                    f.bar_damaged = true;
                }
            }
            // Alternate screens, DECCOLM.
            (Some(b'?'), b"", b'h' | b'l') => {
                f.out.extend_from_slice(&seq);
                if csi
                    .params()
                    .any(|p| matches!(p, Some(3 | 47 | 1047 | 1049)))
                {
                    f.bar_damaged = true;
                }
            }
            // DECSTR (soft reset) resets the margins.
            (None, b"!", b'p') => {
                f.out.extend_from_slice(&seq);
                f.bar_damaged = true;
                self.region(f);
            }
            _ => f.out.extend_from_slice(&seq),
        }
    }

    fn string_done(&mut self, kind: Str, f: &mut Filtered) {
        let seq = std::mem::take(&mut self.held);
        self.state = State::Ground;
        let body = string_body(&seq);
        let drop = match kind {
            Str::Osc => osc_is_query(body),
            Str::Dcs => dcs_is_query(body),
        };
        if !drop {
            f.out.extend_from_slice(&seq);
        }
    }
}

/// A control string's content between its introducer and terminator.
fn string_body(seq: &[u8]) -> &[u8] {
    let body = &seq[2.min(seq.len())..];
    body.strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(&[BEL]))
        .unwrap_or(body)
}

/// OSC colour or clipboard reads: 4 (`index;?` pairs), 10-19 (`?` specs),
/// 52 (`selection;?`).
fn osc_is_query(body: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    let mut parts = text.split(';');
    let Some(code) = parts.next().and_then(|c| c.parse::<u32>().ok()) else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    match code {
        4 => {
            !rest.is_empty()
                && rest.len().is_multiple_of(2)
                && rest.iter().skip(1).step_by(2).all(|s| *s == "?")
        }
        10..=19 => !rest.is_empty() && rest.iter().all(|s| *s == "?"),
        52 => rest.len() == 2 && rest[1] == "?",
        _ => false,
    }
}

/// DECRQSS (`DCS $ q`) and XTGETTCAP (`DCS + q`); `header` starts after
/// `ESC P`.
fn dcs_is_query(header: &[u8]) -> bool {
    let rest: Vec<u8> = header
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_digit() || *b == b';')
        .collect();
    rest.starts_with(b"$q") || rest.starts_with(b"+q")
}

/// One parsed CSI sequence (after `ESC [`).
struct Csi<'a> {
    prefix: Option<u8>,
    params: &'a [u8],
    intermediates: &'a [u8],
    finale: u8,
}

impl<'a> Csi<'a> {
    fn parse(body: &'a [u8]) -> Self {
        let (finale, body) = body.split_last().map_or((0, body), |(f, b)| (*f, b));
        let (prefix, body) = match body.first() {
            Some(&p @ (b'<' | b'=' | b'>' | b'?')) => (Some(p), &body[1..]),
            _ => (None, body),
        };
        let split = body
            .iter()
            .position(|b| (0x20..=0x2f).contains(b))
            .unwrap_or(body.len());
        Self {
            prefix,
            params: &body[..split],
            intermediates: &body[split..],
            finale,
        }
    }

    /// Each `;`-separated parameter (`None`: empty or not a number).
    fn params(&self) -> impl Iterator<Item = Option<u32>> + '_ {
        self.params
            .split(|b| *b == b';')
            .map(|p| std::str::from_utf8(p).ok()?.parse().ok())
    }

    fn param(&self, index: usize) -> Option<u32> {
        self.params().nth(index).flatten()
    }

    fn is_query(&self) -> bool {
        match (self.prefix, self.intermediates, self.finale) {
            // DA1 / DA2 / DA3.
            (None | Some(b'>') | Some(b'='), b"", b'c') => true,
            // DSR (status, cursor position) / DEC DSR.
            (None | Some(b'?'), b"", b'n') => true,
            // DECRQM (ANSI and DEC modes).
            (None | Some(b'?'), b"$", b'p') => true,
            // XTVERSION.
            (Some(b'>'), b"", b'q') => true,
            // Kitty keyboard: query, push, pop, set.
            (Some(b'?' | b'>' | b'<' | b'='), b"", b'u') => true,
            // XTQMODKEYS.
            (Some(b'?'), b"", b'm') => true,
            // DECRQCRA (checksum of a rectangle).
            (None, b"*", b'y') => true,
            // DECREQTPARM.
            (None, b"", b'x') => true,
            // Window and size reports.
            (None, b"", b't') => matches!(
                self.param(0),
                Some(11 | 13 | 14 | 15 | 16 | 18 | 19 | 20 | 21)
            ),
            _ => false,
        }
    }
}

/// How many trailing bytes of `out` are an incomplete UTF-8 character.
fn partial_utf8_tail(out: &[u8]) -> usize {
    let n = out.len();
    for back in 1..=3.min(n) {
        let b = out[n - back];
        if b & 0b1100_0000 == 0b1000_0000 {
            continue; // continuation byte: keep looking for the lead
        }
        let need = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(filter: &mut OutputFilter, input: &[u8]) -> Vec<u8> {
        filter.feed(input).out
    }

    #[test]
    fn plain_output_and_ordinary_sequences_pass_untouched() {
        let mut f = OutputFilter::new(None);
        let text = "hello \x1b[1;31mred\x1b[0m \u{4e16}\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b(0q\x1b(B\r\n";
        assert_eq!(run(&mut f, text.as_bytes()), text.as_bytes());
        assert!(f.at_boundary());
    }

    #[test]
    fn queries_are_dropped_wherever_they_are_split() {
        let input = b"a\x1b[c\x1b[>c\x1b[6n\x1b[?6n\x1b[?2004$p\x1b[>q\x1b[?u\x1b[>1u\x1b[14t\x1b[22;0t\x05\
            \x1b]11;?\x07\x1b]4;1;?;2;?\x1b\\\x1b]52;c;?\x07\x1b]52;c;aGk=\x07\x1bP$qm\x1b\\\x1bP+q544e\x1b\\b";
        let expected = b"a\x1b[22;0t\x1b]52;c;aGk=\x07b".to_vec();
        let mut whole = OutputFilter::new(None);
        assert_eq!(run(&mut whole, input), expected);
        // Byte at a time: the same.
        let mut split = OutputFilter::new(None);
        let mut out = Vec::new();
        for b in input {
            out.extend(run(&mut split, std::slice::from_ref(b)));
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn colour_sets_are_not_queries() {
        let mut f = OutputFilter::new(None);
        let set = b"\x1b]11;#000000\x07\x1b]4;1;rgb:ff/00/00\x07";
        assert_eq!(run(&mut f, set), set);
    }

    #[test]
    fn with_a_bar_rows_and_margins_stay_above_it() {
        let mut f = OutputFilter::new(Some(23));
        assert_eq!(run(&mut f, b"\x1b[r"), b"\x1b[1;23r");
        assert_eq!(run(&mut f, b"\x1b[5;40r"), b"\x1b[5;23r");
        assert_eq!(run(&mut f, b"\x1b[2;10r"), b"\x1b[2;10r");
        assert_eq!(run(&mut f, b"\x1b[999;999H"), b"\x1b[23;999H");
        assert_eq!(run(&mut f, b"\x1b[30f"), b"\x1b[23f");
        assert_eq!(run(&mut f, b"\x1b[3;4H"), b"\x1b[3;4H");
        assert_eq!(run(&mut f, b"\x1b[99d"), b"\x1b[23d");
        let reset = f.feed(b"\x1bc");
        assert_eq!(reset.out, b"\x1bc\x1b[1;23r");
        assert!(reset.bar_damaged);
        assert!(f.feed(b"\x1b[2J").bar_damaged);
        assert!(!f.feed(b"\x1b[1J").bar_damaged);
        assert!(f.feed(b"\x1b[?1049h").bar_damaged);
        assert!(!f.feed(b"\x1b[?25l").bar_damaged);
        // Without a bar nothing is rewritten.
        f.set_rows(None);
        assert_eq!(run(&mut f, b"\x1b[r\x1b[999;1H"), b"\x1b[r\x1b[999;1H");
    }

    #[test]
    fn a_split_character_is_held_to_the_next_feed() {
        let mut f = OutputFilter::new(None);
        let text = "\u{4e16}".as_bytes();
        assert_eq!(run(&mut f, &text[..2]), b"");
        assert!(!f.at_boundary());
        assert_eq!(run(&mut f, &text[2..]), text);
        assert!(f.at_boundary());
        let mut f = OutputFilter::new(None);
        assert_eq!(run(&mut f, b"x\x1b["), b"x");
        assert!(!f.at_boundary());
        assert_eq!(run(&mut f, b"1m"), b"\x1b[1m");
    }

    #[test]
    fn long_strings_pass_through_and_end_cleanly() {
        let mut f = OutputFilter::new(None);
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'A', MAX_STRING * 2));
        input.extend(b"\x07after");
        assert_eq!(run(&mut f, &input), input);
        assert!(f.at_boundary());
        // An APC (kitty graphics) is never inspected.
        let apc = b"\x1b_Gf=24;AAAA\x1b\\x";
        assert_eq!(run(&mut f, apc), apc);
    }

    #[test]
    fn a_string_cut_off_by_a_sequence_keeps_the_sequence() {
        let mut f = OutputFilter::new(None);
        assert_eq!(run(&mut f, b"\x1b]0;t\x1b[1mX"), b"\x1b]0;t\x1b[1mX");
    }
}
