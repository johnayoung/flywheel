//! The agent's input line as its screen shows it: the fail-safe under the
//! typed-bytes draft model, which cannot know everything that reaches the
//! line (text typed while a dialog was up and kept by the agent after it
//! closed, history recall, a draft from before a hub restart).

use crate::agent::SessionKind;

fn is_rule(row: &str) -> bool {
    let row = row.trim();
    row.chars().count() >= 20 && row.chars().all(|c| c == '─')
}

/// A dialog's footer is on the last rows: the input line is not shown.
fn dialog_up(rows: &[&str]) -> bool {
    rows.iter()
        .rev()
        .filter(|r| !r.trim().is_empty())
        .take(4)
        .any(|r| r.to_lowercase().contains("esc to cancel"))
}

/// Characters of `rows` as one text, whitespace runs as one space.
fn chars<'a>(rows: impl IntoIterator<Item = &'a str>) -> usize {
    let words: Vec<&str> = rows.into_iter().flat_map(str::split_whitespace).collect();
    words.join(" ").chars().count()
}

/// Claude Code's prompt box: the last two rule lines on screen with a
/// `❯` row right under the first; what follows the `❯` (and the rows
/// down to the second rule) is the unsent text.
fn claude(rows: &[&str]) -> Option<usize> {
    let bottom = rows.iter().rposition(|r| is_rule(r))?;
    let top = rows[..bottom].iter().rposition(|r| is_rule(r))?;
    let body = &rows[top + 1..bottom];
    let first = body.first()?.strip_prefix('❯')?;
    Some(chars(
        std::iter::once(first).chain(body[1..].iter().copied()),
    ))
}

/// Codex's composer: the last `›` row (an approval overlay's `› 1. Yes`
/// option is not one).
fn codex(rows: &[&str]) -> Option<usize> {
    let text = rows.iter().rev().find_map(|r| r.strip_prefix('›'))?.trim();
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && text[digits..].starts_with(". ") {
        return None;
    }
    Some(chars([text]))
}

/// Characters sitting unsent in the input line of a `kind` agent whose
/// screen shows `rows` (dim text left out: a placeholder is not typed
/// text). `None`: the screen does not show an input line this reads.
pub(crate) fn unsent_on_screen(kind: SessionKind, rows: &[String]) -> Option<usize> {
    let rows: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    if dialog_up(&rows) {
        return None;
    }
    match kind {
        SessionKind::Claude => claude(&rows),
        SessionKind::Codex => codex(&rows),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn fixture(name: &str) -> Vec<String> {
        let path = format!("{}/detect/fixtures/{name}.txt", env!("CARGO_MANIFEST_DIR"));
        rows(&std::fs::read_to_string(path).unwrap())
    }

    #[test]
    fn claudes_prompt_box_is_read_empty_or_holding_text() {
        for name in [
            "claude-idle",
            "claude-done",
            "claude-working",
            "claude-question-cancelled",
        ] {
            assert_eq!(
                unsent_on_screen(SessionKind::Claude, &fixture(name)),
                Some(0),
                "{name}"
            );
        }
        let rule = "─".repeat(60);
        let held = rows(&format!(
            "● Done.\n\n{rule}\n❯ Is Wintermute doing LP activity?\n{rule}\n  ⏵⏵ bypass permissions on · 1 shell still running\n"
        ));
        assert_eq!(unsent_on_screen(SessionKind::Claude, &held), Some(32));
        let two_lines = rows(&format!("{rule}\n❯ one\n  two three\n{rule}\n"));
        assert_eq!(unsent_on_screen(SessionKind::Claude, &two_lines), Some(13));
    }

    #[test]
    fn a_dialog_or_a_screen_without_the_box_is_not_read() {
        for name in [
            "claude-ask-question",
            "claude-ask-question-narrow",
            "claude-permission-bash",
            "claude-permission-write",
            "claude-trust",
            "claude-transcript-view",
        ] {
            assert_eq!(
                unsent_on_screen(SessionKind::Claude, &fixture(name)),
                None,
                "{name}"
            );
        }
        for name in ["codex-approval-exec", "codex-approval-patch", "codex-trust"] {
            assert_eq!(
                unsent_on_screen(SessionKind::Codex, &fixture(name)),
                None,
                "{name}"
            );
        }
        assert_eq!(
            unsent_on_screen(SessionKind::Shell, &rows("$ ls -la")),
            None
        );
    }

    #[test]
    fn codexs_composer_is_read_empty_or_holding_text() {
        // Its placeholder is dim: left out of the rows read.
        let empty = rows("  /status - show current session configuration\n›\n  ? for shortcuts\n");
        assert_eq!(unsent_on_screen(SessionKind::Codex, &empty), Some(0));
        let held = rows("• Done.\n› fix the tests\n  ? for shortcuts\n");
        assert_eq!(unsent_on_screen(SessionKind::Codex, &held), Some(13));
    }
}
