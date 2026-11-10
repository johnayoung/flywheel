//! `fw worker --account NAME`: the worker's Claude agents authenticate with
//! a named account's token instead of the CLI's own login.
//!
//! The token lives where `fw hub account add` writes it
//! ([`Home::account_token_path`]); this module mirrors the hub's contract
//! (`flywheel_hub::accounts`: name rule, token variable, outranking
//! variables) without depending on the hub, and a test in the `flywheel`
//! crate pins the two together. The worker resolves the account once at
//! startup only to refuse a bad one; the token file itself is read at every
//! agent spawn ([`AgentCredential::apply`]), so a replaced token reaches the
//! next attempt. The token never leaves the agent's environment: not argv,
//! not logs, not the store.

use crate::home::Home;
use flywheel_core::agents_invoke::AgentCredential;

/// The variable Claude Code reads the account token from.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Variables that outrank [`TOKEN_ENV`] in Claude Code's auth order; an
/// agent under an account runs without them.
pub const OUTRANKING_ENV: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];
/// Longest account name.
pub const MAX_NAME_CHARS: usize = 40;

/// A usable account name: 1-40 lowercase letters, digits, `-`, `_`,
/// starting with a letter or digit (the hub's rule; never a path).
pub fn validate_name(name: &str) -> Result<&str, String> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if ok {
        Ok(name)
    } else {
        Err(format!(
            "--account {name:?} is not a valid account name: 1-{MAX_NAME_CHARS} lowercase \
             letters, digits, - and _, starting with a letter or digit"
        ))
    }
}

/// The credential account `name` runs agents under: a valid name whose
/// token file exists and holds a token now. The file is re-read at every
/// spawn; this check only refuses a worker that could never authenticate.
pub fn resolve_credential(home: &Home, name: &str) -> Result<AgentCredential, String> {
    let name = validate_name(name)?;
    let credential = AgentCredential {
        env: TOKEN_ENV.to_owned(),
        token_file: home.account_token_path(name),
        removed_env: OUTRANKING_ENV.iter().map(|v| (*v).to_owned()).collect(),
    };
    if !credential.token_file.is_file() {
        return Err(format!(
            "--account {name:?}: no Claude account {name:?} (no token file at {}; add it: \
             `claude setup-token`, then `fw hub account add {name}`)",
            credential.token_file.display()
        ));
    }
    credential
        .read_token()
        .map_err(|e| format!("--account {name:?}: {e}"))?;
    Ok(credential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_account_names_follow_the_hub_rule_and_never_name_a_path() {
        for good in ["work", "a", "0x", "per_sonal-2", &"a".repeat(40)] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "../x",
            "x/y",
            "Work",
            "-x",
            "_x",
            "a b",
            &"a".repeat(41),
            "..",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn worker_account_resolves_the_home_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        let missing = resolve_credential(&home, "work").unwrap_err();
        assert!(missing.contains("no Claude account"), "{missing}");
        let invalid = resolve_credential(&home, "../work").unwrap_err();
        assert!(invalid.contains("not a valid account name"), "{invalid}");

        let path = dir.path().join("hub/claude-accounts/work.token");
        assert_eq!(home.account_token_path("work"), path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\n").unwrap();
        let empty = resolve_credential(&home, "work").unwrap_err();
        assert!(empty.contains("empty"), "{empty}");

        std::fs::write(&path, "sk-ant-oat01-secret\n").unwrap();
        let credential = resolve_credential(&home, "work").unwrap();
        assert_eq!(credential.env, TOKEN_ENV);
        assert_eq!(credential.token_file, path);
        assert_eq!(credential.removed_env, OUTRANKING_ENV);
        assert_eq!(credential.read_token().unwrap(), "sk-ant-oat01-secret");
        assert!(!format!("{credential:?}").contains("secret"));
    }
}
