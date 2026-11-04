//! Small helpers the CLI verbs and file watchers share.

use serde::Serialize;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

/// Write `value` as pretty JSON on its own line (a `--json` answer).
pub(crate) fn write_json<T: Serialize + ?Sized>(
    out: &mut dyn Write,
    value: &T,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
}

/// [`write_json`], for a verb whose exit code does not depend on the write.
pub(crate) fn print_json<T: Serialize + ?Sized>(out: &mut dyn Write, value: &T) {
    let _ = write_json(out, value);
}

/// Run a verb whose reader may go away (`fw hub activity | head -1`): once
/// a write to `out` meets a closed pipe, that write (and every later one)
/// still fails, so the verb stops writing, but what it then says on `err`
/// about it is dropped and it exits 0, as a filter killed by SIGPIPE ends
/// quietly. The one closed-pipe policy of every verb (a daemon's stderr
/// is its log, so `serve` does not run under it).
pub(crate) fn quiet_on_closed_pipe(
    out: &mut dyn Write,
    err: &mut dyn Write,
    verb: impl FnOnce(&mut dyn Write, &mut dyn Write) -> i32,
) -> i32 {
    let closed = std::cell::Cell::new(false);
    let code = verb(
        &mut PipeOut {
            inner: out,
            closed: &closed,
        },
        &mut PipeErr {
            inner: err,
            closed: &closed,
        },
    );
    if closed.get() { 0 } else { code }
}

/// `out` under [`quiet_on_closed_pipe`]: a closed pipe is noted.
struct PipeOut<'a> {
    inner: &'a mut dyn Write,
    closed: &'a std::cell::Cell<bool>,
}

impl PipeOut<'_> {
    fn note<T>(&self, result: std::io::Result<T>) -> std::io::Result<T> {
        if let Err(e) = &result
            && e.kind() == std::io::ErrorKind::BrokenPipe
        {
            self.closed.set(true);
        }
        result
    }
}

impl Write for PipeOut<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed.get() {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let result = self.inner.write(buf);
        self.note(result)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.closed.get() {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let result = self.inner.flush();
        self.note(result)
    }
}

/// `err` under [`quiet_on_closed_pipe`]: silent once `out` is closed.
struct PipeErr<'a> {
    inner: &'a mut dyn Write,
    closed: &'a std::cell::Cell<bool>,
}

impl Write for PipeErr<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed.get() {
            return Ok(buf.len());
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        self.inner.flush()
    }
}

/// Print `fw hub: <message>` to `err`; the failing exit code (1).
pub(crate) fn fail(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "fw hub: {message}");
    1
}

/// `item[field]` as a string, `""` when missing or not a string.
pub(crate) fn str_field<'a>(item: &'a Value, field: &str) -> &'a str {
    item.get(field).and_then(Value::as_str).unwrap_or("")
}

/// `item[field]` as an array, empty when missing or not an array.
pub(crate) fn list<'a>(item: &'a Value, field: &str) -> &'a [Value] {
    item.get(field)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Ask on the terminal; `false` unless the answer is yes.
pub(crate) fn ask(question: &str, err: &mut dyn Write) -> bool {
    let _ = write!(err, "{question} [y/N] ");
    let _ = err.flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
}

/// Whether to go ahead with a guarded action: `--yes`, else a yes on the
/// terminal (never asked under `--json` or without a terminal).
pub(crate) fn approved(yes: bool, json: bool, question: &str, err: &mut dyn Write) -> bool {
    if yes {
        return true;
    }
    if json || !std::io::stdin().is_terminal() {
        return false;
    }
    ask(question, err)
}

/// One word as a shell reads it back verbatim: as is when every character
/// is plain, else single-quoted (a leading `~`, `^` or `=` is quoted too:
/// shells expand those at a word's start). For showing a command a person
/// can paste (a fix line, a planned git call): nothing is run through it.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && !word.starts_with(['~', '^', '='])
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,^~".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// [`shell_quote`] of a path.
pub fn shell_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// A size as people read it: `512 B`, `1.5 KiB`, `64.0 MiB`, `1.0 GiB`
/// (a config value is written [`crate::record::format_size`]'s way).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `"1 file"`, `"3 files"`.
/// A duration in seconds: plain seconds, or numbers each with a unit
/// (`s m h d w`, any case), summed (`1h30m`). `Err` says what is wrong;
/// callers check the range.
pub fn parse_duration(raw: &str) -> Result<u64, String> {
    let text = raw.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err("empty".into());
    }
    if let Ok(seconds) = text.parse::<u64>() {
        return Ok(seconds);
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 7 * 86_400,
            _ => return Err("units are s, m, h, d, w".into()),
        };
        let n: u64 = digits
            .parse()
            .map_err(|_| "a number comes before each unit".to_owned())?;
        digits.clear();
        total = total.saturating_add(n.saturating_mul(unit));
    }
    if !digits.is_empty() {
        return Err("the last number has no unit".into());
    }
    Ok(total)
}

/// A dollar amount as every surface writes it (CLI rows, activity rows,
/// and the web UI's `fmt.js` `dollars`): `$0`,
/// `$0.0040` (sub-cent spend stays visible), `$0.42`, `$120`.
pub fn dollars(usd: f64) -> String {
    if usd <= 0.0 || !usd.is_finite() {
        "$0".to_owned()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else if usd < 100.0 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.0}")
    }
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Which file `meta` describes: device and inode, so a file replaced at
/// the same path (rotated, rewritten by rename) is told apart. Off unix
/// there is no such id: every file reads `(0, 0)`.
pub fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

/// [`file_identity`] of the file at `path` (following symlinks), `None`
/// when it cannot be read.
pub fn path_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| file_identity(&meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes `room` bytes, then reports a closed pipe.
    struct Pipe {
        room: usize,
        got: Vec<u8>,
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.room == 0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            let n = buf.len().min(self.room);
            self.room -= n;
            self.got.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_ends_a_verb_quietly_and_nothing_else_does() {
        let verb = |out: &mut dyn Write, err: &mut dyn Write| {
            for i in 0..100 {
                if let Err(e) = writeln!(out, "line {i}") {
                    return fail(err, &e.to_string());
                }
            }
            writeln!(err, "done").unwrap();
            3
        };
        let (mut out, mut err) = (
            Pipe {
                room: 12,
                got: Vec::new(),
            },
            Vec::new(),
        );
        assert_eq!(quiet_on_closed_pipe(&mut out, &mut err, verb), 0);
        assert_eq!(out.got, b"line 0\nline ");
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
        // An open pipe: the verb's own code and stderr.
        let (mut out, mut err) = (
            Pipe {
                room: usize::MAX,
                got: Vec::new(),
            },
            Vec::new(),
        );
        assert_eq!(quiet_on_closed_pipe(&mut out, &mut err, verb), 3);
        assert_eq!(err, b"done\n");
        // Any other write error is the verb's to report.
        let mut full = std::io::Cursor::new([0u8; 4]);
        let mut err = Vec::new();
        assert_eq!(
            quiet_on_closed_pipe(&mut full, &mut err, |out, err| {
                match writeln!(out, "too long") {
                    Ok(()) => 0,
                    Err(e) => fail(err, &e.to_string()),
                }
            }),
            1
        );
        assert!(!err.is_empty());
    }

    #[test]
    fn helpers() {
        let mut err = Vec::new();
        assert_eq!(fail(&mut err, "no"), 1);
        assert_eq!(String::from_utf8(err).unwrap(), "fw hub: no\n");
        let v = serde_json::json!({"a": "x", "b": 1});
        assert_eq!(
            (str_field(&v, "a"), str_field(&v, "b"), str_field(&v, "c")),
            ("x", "", "")
        );
        assert_eq!(
            (plural(1, "file"), plural(2, "file")),
            ("1 file".into(), "2 files".into())
        );
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, "1").unwrap();
        let before = path_identity(&a).unwrap();
        std::fs::write(dir.path().join("b"), "2").unwrap();
        std::fs::rename(dir.path().join("b"), &a).unwrap();
        assert_ne!(
            path_identity(&a).unwrap(),
            before,
            "a replaced file is another file"
        );
        assert_eq!(path_identity(&dir.path().join("missing")), None);
    }
}
