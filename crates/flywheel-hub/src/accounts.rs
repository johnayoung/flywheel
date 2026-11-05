//! Claude Code accounts: named long-lived tokens (`claude setup-token`) a
//! Claude session can run under instead of the CLI's own login, so a work
//! and a personal subscription share one `~/.claude` (settings, MCP,
//! transcripts, resume) and differ only in who is billed.
//!
//! Each token is its own file, `<state-dir>/claude-accounts/<name>.token`
//! (0600, the directory 0700), never in `hub.toml` and never served: the
//! API lists names only. A session launched with an account gets the token
//! as `CLAUDE_CODE_OAUTH_TOKEN`, read at every spawn (a relaunch or
//! restore picks up a replaced token), with the variables that would
//! outrank it (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`) removed.

use crate::util::{FileMode, write_atomic};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The directory under the state dir.
pub const DIR: &str = "claude-accounts";
/// The variable Claude Code reads the token from.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Variables that outrank [`TOKEN_ENV`] in Claude Code's auth order; a
/// session with an account runs without them.
pub const OUTRANKING_ENV: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];
/// Longest account name.
pub const MAX_NAME_CHARS: usize = 40;
/// Largest token accepted.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// An account, as listed (never its token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub name: String,
    /// When its token was last written.
    pub updated: DateTime<Utc>,
}

/// Why an account operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    Invalid(String),
    NotFound(String),
    Exists(String),
    Io(String),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::NotFound(m) | Self::Exists(m) | Self::Io(m) => f.write_str(m),
        }
    }
}

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR)
}

fn file(state_dir: &Path, name: &str) -> PathBuf {
    dir(state_dir).join(format!("{name}.token"))
}

/// A usable account name: 1-40 lowercase letters, digits, `-`, `_`,
/// starting with a letter or digit.
pub fn validate_name(name: &str) -> Result<&str, AccountError> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if ok {
        Ok(name)
    } else {
        Err(AccountError::Invalid(format!(
            "account name {name:?}: 1-{MAX_NAME_CHARS} lowercase letters, digits, - and _, \
             starting with a letter or digit"
        )))
    }
}

/// `raw` as a token: trimmed, one printable word.
fn clean_token(raw: &str) -> Result<&str, AccountError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(AccountError::Invalid(
            "the token is empty: paste the one `claude setup-token` prints".into(),
        ));
    }
    if token.len() > MAX_TOKEN_BYTES {
        return Err(AccountError::Invalid(format!(
            "the token is longer than {MAX_TOKEN_BYTES} bytes"
        )));
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AccountError::Invalid(
            "the token has spaces or control characters in it: paste the token alone".into(),
        ));
    }
    Ok(token)
}

/// Every account, by name.
pub fn list(state_dir: &Path) -> Vec<Account> {
    let Ok(entries) = std::fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    let mut accounts: Vec<Account> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".token")?
                .to_owned();
            validate_name(&name).ok()?;
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some(Account {
                name,
                updated: meta.modified().ok()?.into(),
            })
        })
        .collect();
    accounts.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    accounts
}

/// Whether account `name` exists.
pub fn exists(state_dir: &Path, name: &str) -> bool {
    validate_name(name).is_ok() && file(state_dir, name).is_file()
}

/// Store `token` as account `name` (an existing one only with `replace`).
pub fn add(state_dir: &Path, name: &str, token: &str, replace: bool) -> Result<(), AccountError> {
    validate_name(name)?;
    let token = clean_token(token)?;
    let path = file(state_dir, name);
    if path.exists() && !replace {
        return Err(AccountError::Exists(format!(
            "account {name:?} exists: replace its token instead (CLI: --replace)"
        )));
    }
    let dir = dir(state_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| AccountError::Io(format!("{}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| AccountError::Io(format!("{}: {e}", dir.display())))?;
    }
    write_atomic(&path, format!("{token}\n").as_bytes(), FileMode::Private)
        .map_err(|e| AccountError::Io(format!("{}: {e}", path.display())))
}

/// Delete account `name`.
pub fn remove(state_dir: &Path, name: &str) -> Result<(), AccountError> {
    validate_name(name)?;
    let path = file(state_dir, name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(AccountError::NotFound(format!("no account {name:?}")))
        }
        Err(e) => Err(AccountError::Io(format!("{}: {e}", path.display()))),
    }
}

/// Account `name`'s token, for a spawn.
pub fn token(state_dir: &Path, name: &str) -> Result<String, AccountError> {
    validate_name(name)?;
    let path = file(state_dir, name);
    let raw = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AccountError::NotFound(format!(
            "no Claude account {name:?} (add it: `claude setup-token`, then \
             `fw hub account add {name}`)"
        )),
        _ => AccountError::Io(format!("{}: {e}", path.display())),
    })?;
    Ok(clean_token(&raw)?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_private_files_listed_by_name_only() {
        let state = tempfile::tempdir().unwrap();
        assert!(list(state.path()).is_empty());
        add(state.path(), "work", "  sk-ant-oat01-abc\n", false).unwrap();
        add(state.path(), "personal", "sk-ant-oat01-xyz", false).unwrap();
        let names: Vec<String> = list(state.path()).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["personal", "work"]);
        assert_eq!(token(state.path(), "work").unwrap(), "sk-ant-oat01-abc");
        assert!(exists(state.path(), "work"));
        assert!(!exists(state.path(), "../work"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&file(state.path(), "work")), 0o600);
            assert_eq!(mode(&dir(state.path())), 0o700);
        }
        assert!(matches!(
            add(state.path(), "work", "other", false),
            Err(AccountError::Exists(_))
        ));
        add(state.path(), "work", "sk-new", true).unwrap();
        assert_eq!(token(state.path(), "work").unwrap(), "sk-new");
        remove(state.path(), "work").unwrap();
        assert!(matches!(
            token(state.path(), "work"),
            Err(AccountError::NotFound(_))
        ));
        assert!(matches!(
            remove(state.path(), "work"),
            Err(AccountError::NotFound(_))
        ));
    }

    #[test]
    fn names_and_tokens_are_checked() {
        let state = tempfile::tempdir().unwrap();
        for bad in ["", "Work", "../x", "-x", "a b", &"x".repeat(41)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        for good in ["work", "acme-2", "p_1"] {
            assert!(validate_name(good).is_ok(), "{good:?}");
        }
        for bad in ["", "   ", "two words", "a\u{1b}b"] {
            assert!(add(state.path(), "x", bad, false).is_err(), "{bad:?}");
        }
    }
}
