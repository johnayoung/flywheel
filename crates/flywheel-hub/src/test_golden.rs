//! Golden files for tests: what a consumer made of the captured fixtures
//! under `tests/fixtures`, compared byte for byte. `FW_HUB_BLESS=1`
//! rewrites them (only when the change in output is intended).

use std::path::{Path, PathBuf};

/// A fixture under `tests/fixtures`.
pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The agent transcripts consumers are checked against: (file, format is Codex).
pub(crate) const TRANSCRIPTS: &[(&str, bool)] = &[
    ("claude-usage.jsonl", false),
    ("claude-tools.jsonl", false),
    ("codex-rollout.jsonl", true),
];

/// Assert `actual` is golden file `name` (or write it when blessing).
pub(crate) fn assert_golden(name: &str, actual: &str) {
    let path = fixture("golden").join(name);
    if std::env::var_os("FW_HUB_BLESS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read golden {}: {e}", path.display()));
    assert!(
        actual == want,
        "{name} differs from its golden file\n--- want\n{want}\n--- got\n{actual}"
    );
}
