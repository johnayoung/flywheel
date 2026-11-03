//! The colours a viewer's terminal shows where a program has set none.

use alacritty_terminal::vte::ansi::{NamedColor, Rgb};

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

pub(crate) const fn rgb(v: u32) -> Rgb {
    Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    }
}

impl ViewerColors {
    /// The web app's dark terminal theme (`TERM_THEMES.dark` in
    /// `web/app-base.js`): what a session answers before any viewer reported.
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
    pub(crate) fn color(&self, index: usize) -> Rgb {
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
