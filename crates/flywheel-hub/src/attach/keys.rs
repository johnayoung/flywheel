//! What `fw hub attach` does with typed bytes: everything goes to the
//! session verbatim (escape sequences, bracketed pastes, mouse reports)
//! except the prefix key (Ctrl+B by default, tmux-like) and the command
//! key after it. Inside a bracketed paste the prefix is just pasted.

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// A command the user typed after the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `d`: leave the session running and exit.
    Detach,
    /// `s`: pick another session.
    Switch,
    /// `t`: status bar on / off.
    ToggleBar,
    /// `r`: re-attach for a fresh snapshot (a garbled screen).
    Redraw,
    /// `?`: show the key help.
    Help,
    /// Anything else (the byte).
    Unbound(u8),
}

/// One step of typed input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send(Vec<u8>),
    Run(Command),
}

/// The prefix key: a control character, written `C-b`, `^b`, or `Ctrl-b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub byte: u8,
}

impl Prefix {
    #[cfg(test)]
    pub const DEFAULT: Self = Self { byte: 0x02 };

    pub fn parse(raw: &str) -> Result<Self, String> {
        let text = raw.trim();
        let lower = text.to_ascii_lowercase();
        let key = ["ctrl-", "ctrl+", "c-", "^"]
            .iter()
            .find_map(|p| lower.strip_prefix(p).map(|_| &text[p.len()..]))
            .ok_or_else(|| {
                format!("--prefix {raw:?}: expected a control key such as C-b, C-a, ^]")
            })?;
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(format!("--prefix {raw:?}: one key after the Ctrl"));
        };
        let byte = match c.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 - b'a' + 1,
            '@' | ' ' => 0x00,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '[' => return Err(format!("--prefix {raw:?}: C-[ is Escape")),
            _ => return Err(format!("--prefix {raw:?}: no such control key")),
        };
        if matches!(byte, 0x09 | 0x0d | 0x0a | 0x08) {
            return Err(format!(
                "--prefix {raw:?}: that is Tab/Enter/Backspace; pick another"
            ));
        }
        Ok(Self { byte })
    }

    /// `C-b`.
    pub fn label(self) -> String {
        match self.byte {
            0x00 => "C-@".into(),
            0x01..=0x1a => format!("C-{}", char::from(b'a' + self.byte - 1)),
            0x1c => "C-\\".into(),
            0x1d => "C-]".into(),
            0x1e => "C-^".into(),
            0x1f => "C-_".into(),
            other => format!("0x{other:02x}"),
        }
    }
}

/// Splits typed bytes into what the session gets and prefix commands.
#[derive(Debug)]
pub struct Keys {
    prefix: Prefix,
    armed: bool,
    in_paste: bool,
    /// The last few bytes seen (paste markers split across reads).
    recent: Vec<u8>,
}

impl Keys {
    pub fn new(prefix: Prefix) -> Self {
        Self {
            prefix,
            armed: false,
            in_paste: false,
            recent: Vec::with_capacity(PASTE_START.len()),
        }
    }

    /// Whether the prefix was typed and the command key is awaited.
    pub fn armed(&self) -> bool {
        self.armed
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut send = Vec::new();
        for &byte in bytes {
            if self.armed {
                self.armed = false;
                if byte == self.prefix.byte {
                    send.push(byte);
                    self.track(byte);
                    continue;
                }
                if !send.is_empty() {
                    actions.push(Action::Send(std::mem::take(&mut send)));
                }
                actions.push(Action::Run(match byte {
                    b'd' => Command::Detach,
                    b's' => Command::Switch,
                    b't' => Command::ToggleBar,
                    b'r' => Command::Redraw,
                    b'?' => Command::Help,
                    other => Command::Unbound(other),
                }));
                continue;
            }
            if byte == self.prefix.byte && !self.in_paste {
                self.armed = true;
                continue;
            }
            send.push(byte);
            self.track(byte);
        }
        if !send.is_empty() {
            actions.push(Action::Send(send));
        }
        actions
    }

    fn track(&mut self, byte: u8) {
        if self.recent.len() == PASTE_START.len() {
            self.recent.remove(0);
        }
        self.recent.push(byte);
        if self.recent == PASTE_START {
            self.in_paste = true;
        } else if self.recent == PASTE_END {
            self.in_paste = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::new(Prefix::DEFAULT)
    }

    #[test]
    fn ordinary_input_goes_through_verbatim() {
        let mut k = keys();
        let input = b"ls -la\r\x1b[A\x1b[<0;10;5M\x03\x7f";
        assert_eq!(k.feed(input), vec![Action::Send(input.to_vec())]);
    }

    #[test]
    fn prefix_commands_split_the_input() {
        let mut k = keys();
        assert_eq!(
            k.feed(b"ab\x02dcd"),
            vec![
                Action::Send(b"ab".to_vec()),
                Action::Run(Command::Detach),
                Action::Send(b"cd".to_vec()),
            ]
        );
        // The prefix twice sends one.
        assert_eq!(k.feed(b"\x02\x02x"), vec![Action::Send(b"\x02x".to_vec())]);
        // Split across reads.
        assert_eq!(k.feed(b"\x02"), vec![]);
        assert!(k.armed());
        assert_eq!(k.feed(b"s"), vec![Action::Run(Command::Switch)]);
        assert_eq!(k.feed(b"\x02z"), vec![Action::Run(Command::Unbound(b'z'))]);
    }

    #[test]
    fn a_prefix_inside_a_bracketed_paste_is_pasted() {
        let mut k = keys();
        let paste = b"\x1b[200~a\x02db\x1b[201~";
        assert_eq!(k.feed(paste), vec![Action::Send(paste.to_vec())]);
        // Markers split across reads count too.
        let mut k = keys();
        assert_eq!(k.feed(b"\x1b[20"), vec![Action::Send(b"\x1b[20".to_vec())]);
        assert_eq!(k.feed(b"0~\x02d"), vec![Action::Send(b"0~\x02d".to_vec())]);
        assert_eq!(
            k.feed(b"\x1b[201~\x02d"),
            vec![
                Action::Send(b"\x1b[201~".to_vec()),
                Action::Run(Command::Detach)
            ]
        );
    }

    #[test]
    fn prefixes_parse_like_tmux() {
        assert_eq!(Prefix::parse("C-b").unwrap(), Prefix::DEFAULT);
        assert_eq!(Prefix::parse("^A").unwrap().byte, 0x01);
        assert_eq!(Prefix::parse("Ctrl-]").unwrap().byte, 0x1d);
        assert!(Prefix::parse("ctrl+space").is_err());
        for bad in ["b", "C-", "C-[", "C-i", "C-m", "C-é"] {
            assert!(Prefix::parse(bad).is_err(), "{bad} accepted");
        }
        assert_eq!(Prefix::parse("C-a").unwrap().label(), "C-a");
        assert_eq!(Prefix::DEFAULT.label(), "C-b");
    }
}
