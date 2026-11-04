//! The terminal fonts draw what agent TUIs print: every non-ASCII
//! character captured from real Claude Code sessions (tests/fixtures/
//! claude-glyphs.txt), and whole blocks TUIs draw from (box drawing,
//! blocks, braille, arrows, geometric shapes, technical symbols,
//! dingbats), is in a vendored font's character map -- never left to
//! whatever a device has (a tofu box on most phones). Read from the
//! embedded woff2 files themselves; the stylesheets' "FW Symbols"
//! unicode-range is checked against its font's real map.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

fn web(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("web").join(rel)
}

/// WOFF2's UIntBase128.
fn base128(data: &[u8], at: &mut usize) -> u32 {
    let mut value: u32 = 0;
    for _ in 0..5 {
        let byte = data[*at];
        *at += 1;
        assert!(value & 0xFE00_0000 == 0, "UIntBase128 overflow");
        value = (value << 7) | u32::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("UIntBase128 longer than 5 bytes");
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

/// The codepoints a woff2 font maps (its `cmap` table: untransformed in
/// WOFF2, so it is read straight out of the decompressed table stream).
fn woff2_codepoints(file: &str) -> BTreeSet<u32> {
    // WOFF2's known-table index (spec section 5.1).
    const KNOWN: [&[u8; 4]; 63] = [
        b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
        b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
        b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
        b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
        b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
        b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
        b"Gloc", b"Feat", b"Sill",
    ];
    let path = web(&format!("vendor/fonts/{file}"));
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&data[..4], b"wOF2", "{file} is not woff2");
    let tables = u16::from_be_bytes([data[12], data[13]]);
    let compressed = be32(&data, 20) as usize;
    let mut at = 48;
    // (tag, offset in the stream, length there)
    let mut directory = Vec::new();
    let mut offset = 0usize;
    for _ in 0..tables {
        let flags = data[at];
        at += 1;
        let tag: [u8; 4] = if flags & 0x3F == 0x3F {
            let tag = data[at..at + 4].try_into().unwrap();
            at += 4;
            tag
        } else {
            *KNOWN[usize::from(flags & 0x3F)]
        };
        let version = flags >> 6;
        let original = base128(&data, &mut at) as usize;
        let transformed = if &tag == b"glyf" || &tag == b"loca" {
            version == 0
        } else {
            version != 0
        };
        let length = if transformed {
            base128(&data, &mut at) as usize
        } else {
            original
        };
        directory.push((tag, offset, length));
        offset += length;
    }
    let mut stream = Vec::new();
    brotli_decompressor::Decompressor::new(&data[at..at + compressed], 4096)
        .read_to_end(&mut stream)
        .unwrap_or_else(|e| panic!("{file}: brotli: {e}"));
    assert_eq!(stream.len(), offset, "{file}: table stream length");
    let (_, start, length) = directory
        .iter()
        .find(|(tag, _, _)| tag == b"cmap")
        .unwrap_or_else(|| panic!("{file} has no cmap"));
    let cmap = ttf_parser::cmap::Table::parse(&stream[*start..start + length])
        .unwrap_or_else(|| panic!("{file}: unreadable cmap"));
    let mut codepoints = BTreeSet::new();
    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        subtable.codepoints(|cp| {
            if subtable.glyph_index(cp).is_some_and(|g| g.0 != 0) {
                codepoints.insert(cp);
            }
        });
    }
    assert!(
        codepoints.len() > 100,
        "{file}: {} codepoints",
        codepoints.len()
    );
    codepoints
}

/// `U+XXXX`, `U+XXXX-YYYY` items of a CSS unicode-range.
fn unicode_range(value: &str) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let hex = item
            .strip_prefix("U+")
            .unwrap_or_else(|| panic!("bad range item {item:?}"));
        let (from, to) = hex.split_once('-').unwrap_or((hex, hex));
        let from = u32::from_str_radix(from, 16).unwrap();
        let to = u32::from_str_radix(to, 16).unwrap();
        assert!(from <= to, "{item}");
        set.extend(from..=to);
    }
    set
}

/// A stylesheet's "FW Symbols" faces: (font file, unicode-range).
fn symbol_faces(css_file: &str) -> Vec<(String, BTreeSet<u32>)> {
    let css = std::fs::read_to_string(web(css_file)).unwrap();
    let mut faces = Vec::new();
    for block in css.split("@font-face").skip(1) {
        let body = &block[..block.find('}').unwrap()];
        if !body.contains("font-family: \"FW Symbols\"") {
            continue;
        }
        let src = body.split("url(/assets/vendor/fonts/").nth(1).unwrap();
        let file = src[..src.find(')').unwrap()].to_owned();
        let range = body.split("unicode-range:").nth(1).unwrap();
        let range = &range[..range.find(';').unwrap()];
        faces.push((file, unicode_range(range)));
    }
    faces
}

const TERM: &str = "jetbrains-mono.woff2";

#[test]
fn symbol_faces_name_only_what_their_fonts_draw() {
    let term = woff2_codepoints(TERM);
    for css in ["app.css", "viewer.css"] {
        let faces = symbol_faces(css);
        assert_eq!(faces.len(), 2, "{css}: the core and extended symbol faces");
        for (file, range) in &faces {
            // A range wider than the font sends the browser for a face
            // that cannot draw the character. (The subset's map may hold a
            // few more: characters sharing a kept glyph, most of them
            // JetBrains Mono's, which the range leaves to it.)
            let map = woff2_codepoints(file);
            let undrawn: Vec<String> = range
                .difference(&map)
                .map(|cp| format!("U+{cp:04X}"))
                .collect();
            assert!(
                undrawn.is_empty(),
                "{css}: {file}'s unicode-range names characters it lacks: {undrawn:?}"
            );
            // Only what JetBrains Mono lacks: one look for every cell.
            assert!(range.is_disjoint(&term), "{css}: {file} overlaps {TERM}");
        }
        assert!(faces[0].1.is_disjoint(&faces[1].1), "{css}: faces overlap");
    }
}

#[test]
fn every_character_real_claude_sessions_print_is_drawn() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-glyphs.txt");
    let text = std::fs::read_to_string(&fixture).unwrap();
    let term = woff2_codepoints(TERM);
    // The face every agent's first screen needs is preloaded: these must
    // not wait for the lazily fetched extended face.
    let core = woff2_codepoints("juliamono-symbols.woff2");
    let mut missing = Vec::new();
    let mut seen = 0;
    for line in text.lines().filter(|l| l.starts_with("U+")) {
        let hex = line[2..].split_whitespace().next().unwrap();
        let cp = u32::from_str_radix(hex, 16).unwrap();
        seen += 1;
        if !term.contains(&cp) && !core.contains(&cp) {
            missing.push(line.to_owned());
        }
    }
    assert!(seen >= 30, "fixture holds {seen} characters");
    assert!(
        missing.is_empty(),
        "no vendored glyph for:\n{}",
        missing.join("\n")
    );
}

#[test]
fn tui_blocks_are_covered_whole() {
    let mut drawn = woff2_codepoints(TERM);
    drawn.extend(woff2_codepoints("juliamono-symbols.woff2"));
    drawn.extend(woff2_codepoints("juliamono-symbols-ext.woff2"));
    for (name, from, to) in [
        ("Arrows", 0x2190, 0x21FF),
        ("Miscellaneous Technical", 0x2300, 0x23FF),
        ("Box Drawing", 0x2500, 0x257F),
        ("Block Elements", 0x2580, 0x259F),
        ("Geometric Shapes", 0x25A0, 0x25FF),
        ("Miscellaneous Symbols", 0x2600, 0x26FF),
        ("Dingbats", 0x2700, 0x27BF),
        ("Braille Patterns", 0x2800, 0x28FF),
    ] {
        let missing: Vec<String> = (from..=to)
            .filter(|cp| !drawn.contains(cp))
            .map(|cp| format!("U+{cp:04X}"))
            .collect();
        assert!(missing.is_empty(), "{name}: no glyph for {missing:?}");
    }
}

#[test]
fn every_terminal_font_stack_falls_back_to_the_symbols() {
    let stacks = [
        ("app.css", "--term:"),
        ("term-view.js", "fontFamily:"),
        ("app-layout.js", "const TERM_FONT ="),
        ("app-layout.js", "martian:"),
        ("app-layout.js", "system:"),
    ];
    for (file, key) in stacks {
        let text = std::fs::read_to_string(web(file)).unwrap();
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with(key))
            .unwrap_or_else(|| panic!("{file}: no {key}"));
        assert!(
            line.contains("\"FW Symbols\""),
            "{file}: {key} lacks FW Symbols"
        );
    }
}
