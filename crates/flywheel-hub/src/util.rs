//! Small helpers the CLI verbs and file watchers share.

use serde_json::Value;
use std::io::Write;
use std::path::Path;

/// Print `fw hub: <message>` to `err`; the failing exit code (1).
pub(crate) fn fail(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "fw hub: {message}");
    1
}

/// `item[field]` as a string, `""` when missing or not a string.
pub(crate) fn str_field<'a>(item: &'a Value, field: &str) -> &'a str {
    item.get(field).and_then(Value::as_str).unwrap_or("")
}

/// `"1 file"`, `"3 files"`.
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
