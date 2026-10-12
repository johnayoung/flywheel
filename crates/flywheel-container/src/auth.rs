//! How the in-container Claude CLI authenticates (spec 00044).
//!
//! A first-class, validated alternative to hand-building the container env:
//! prefer a Claude Pro/Max **subscription** session over an API key. Three
//! modes, each via a constructor; the fields are an implementation detail.
//!
//! - [`ClaudeAuth::oauth_token`]: a long-lived subscription OAuth token from
//!   `claude setup-token`, passed as `CLAUDE_CODE_OAUTH_TOKEN`. No API key,
//!   no on-disk state, no refresh. The recommended container path.
//! - [`ClaudeAuth::session`]: bind-mount the host's existing `~/.claude`
//!   credentials (a prior `claude /login`) read-write so OAuth refresh
//!   persists; sets `HOME` so the CLI finds them.
//! - [`ClaudeAuth::api_key`]: a raw `ANTHROPIC_API_KEY` (not a subscription).
//!
//! Guard: `ANTHROPIC_API_KEY` takes precedence over the subscription token in
//! the CLI's auth order, so a subscription mode refuses to coexist with one
//! in the container env, otherwise the key would silently win.

use crate::docker::{DEFAULT_AGENT_HOME, VolumeMount};
use std::collections::BTreeMap;
use std::fmt;

/// The env var carrying a subscription OAuth token.
pub const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// The env var carrying a raw API key.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("ClaudeAuth::oauth_token requires a non-empty token")]
    EmptyToken,
    #[error("ClaudeAuth::api_key requires a non-empty key")]
    EmptyKey,
    #[error(
        "{API_KEY_ENV} is set in the container env alongside a subscription auth; the CLI \
         prefers the key over the subscription, so the session would never be used. Remove \
         {API_KEY_ENV} to authenticate with the subscription."
    )]
    ApiKeyConflict,
}

/// The env + mounts an auth mode contributes to the container.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeAuth {
    env: BTreeMap<String, String>,
    mounts: Vec<VolumeMount>,
    /// True for subscription modes (oauth/session): `ANTHROPIC_API_KEY` must
    /// be absent from the container env or it would override the session.
    subscription: bool,
}

impl fmt::Debug for ClaudeAuth {
    /// Env values are secrets; only the key names are rendered.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeAuth")
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("mounts", &self.mounts)
            .field("subscription", &self.subscription)
            .finish()
    }
}

/// Expand a leading `~` / `~/` against `HOME`; an unresolvable path is
/// returned unchanged (the same contract as Python's `expanduser`).
fn expand_user(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_owned();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_owned();
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{}{rest}", home.trim_end_matches('/')),
        _ => path.to_owned(),
    }
}

impl ClaudeAuth {
    /// Subscription OAuth token (`claude setup-token`) via env. Recommended.
    pub fn oauth_token(token: impl Into<String>) -> Result<Self, AuthError> {
        let token = token.into();
        if token.is_empty() {
            return Err(AuthError::EmptyToken);
        }
        Ok(Self {
            env: BTreeMap::from([(OAUTH_TOKEN_ENV.to_owned(), token)]),
            mounts: Vec::new(),
            subscription: true,
        })
    }

    /// Bind-mount the host's `~/.claude` (an existing `claude /login`).
    ///
    /// Mounted read-write so OAuth refresh persists; `HOME` is set so the CLI
    /// resolves credentials there. The container user must be UID-matched to
    /// the host owner of the dir (the strategy's `--user` default).
    pub fn session(claude_dir: Option<&str>) -> Self {
        let host = expand_user(claude_dir.unwrap_or("~/.claude"));
        let sandbox = format!("{DEFAULT_AGENT_HOME}/.claude");
        Self {
            env: BTreeMap::from([("HOME".to_owned(), DEFAULT_AGENT_HOME.to_owned())]),
            mounts: vec![VolumeMount::new(host, sandbox)],
            subscription: true,
        }
    }

    /// A raw `ANTHROPIC_API_KEY` (not a subscription session).
    pub fn api_key(key: impl Into<String>) -> Result<Self, AuthError> {
        let key = key.into();
        if key.is_empty() {
            return Err(AuthError::EmptyKey);
        }
        Ok(Self {
            env: BTreeMap::from([(API_KEY_ENV.to_owned(), key)]),
            mounts: Vec::new(),
            subscription: false,
        })
    }

    pub fn is_subscription(&self) -> bool {
        self.subscription
    }

    /// Validate against `base_env` and return the (env, mounts) to add.
    ///
    /// Fails if a subscription mode would be overridden by an
    /// `ANTHROPIC_API_KEY` already in the container env.
    pub fn resolve(
        &self,
        base_env: &BTreeMap<String, String>,
    ) -> Result<(BTreeMap<String, String>, Vec<VolumeMount>), AuthError> {
        if self.subscription && base_env.contains_key(API_KEY_ENV) {
            return Err(AuthError::ApiKeyConflict);
        }
        Ok((self.env.clone(), self.mounts.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn oauth_token_sets_env_no_mounts() {
        let (resolved, mounts) = ClaudeAuth::oauth_token("tok-123")
            .unwrap()
            .resolve(&BTreeMap::new())
            .unwrap();
        assert_eq!(resolved, env(&[(OAUTH_TOKEN_ENV, "tok-123")]));
        assert!(mounts.is_empty());
    }

    #[test]
    fn session_mounts_credentials_and_sets_home() {
        let (resolved, mounts) = ClaudeAuth::session(None).resolve(&BTreeMap::new()).unwrap();
        assert_eq!(resolved, env(&[("HOME", DEFAULT_AGENT_HOME)]));
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host_path, expand_user("~/.claude"));
        assert!(mounts[0].host_path.ends_with("/.claude"));
        assert!(!mounts[0].host_path.starts_with('~'));
        assert_eq!(
            mounts[0].sandbox_path,
            format!("{DEFAULT_AGENT_HOME}/.claude")
        );
        assert!(!mounts[0].readonly);
    }

    #[test]
    fn session_honors_custom_claude_dir() {
        let (_, mounts) = ClaudeAuth::session(Some("/tmp/creds"))
            .resolve(&BTreeMap::new())
            .unwrap();
        assert_eq!(mounts[0].host_path, "/tmp/creds");
    }

    #[test]
    fn expand_user_only_rewrites_a_leading_tilde() {
        assert_eq!(expand_user("/abs/~"), "/abs/~");
        assert_eq!(expand_user("~other/x"), "~other/x");
        assert!(!expand_user("~").starts_with('~'));
    }

    #[test]
    fn api_key_sets_env() {
        let (resolved, mounts) = ClaudeAuth::api_key("sk-ant-x")
            .unwrap()
            .resolve(&BTreeMap::new())
            .unwrap();
        assert_eq!(resolved, env(&[(API_KEY_ENV, "sk-ant-x")]));
        assert!(mounts.is_empty());
    }

    #[test]
    fn empty_token_or_key_rejected() {
        assert_eq!(
            ClaudeAuth::oauth_token("").unwrap_err(),
            AuthError::EmptyToken
        );
        assert_eq!(ClaudeAuth::api_key("").unwrap_err(), AuthError::EmptyKey);
    }

    #[test]
    fn subscription_rejects_api_key_in_env() {
        let base = env(&[(API_KEY_ENV, "sk-ant-x")]);
        assert_eq!(
            ClaudeAuth::oauth_token("tok")
                .unwrap()
                .resolve(&base)
                .unwrap_err(),
            AuthError::ApiKeyConflict
        );
        assert_eq!(
            ClaudeAuth::session(None).resolve(&base).unwrap_err(),
            AuthError::ApiKeyConflict
        );
        assert!(AuthError::ApiKeyConflict.to_string().contains(API_KEY_ENV));
    }

    #[test]
    fn api_key_mode_coexists_with_api_key_env() {
        let base = env(&[(API_KEY_ENV, "sk-ant-x")]);
        let (resolved, _) = ClaudeAuth::api_key("sk-ant-x")
            .unwrap()
            .resolve(&base)
            .unwrap();
        assert_eq!(resolved, base);
    }

    #[test]
    fn debug_redacts_secret_values() {
        let rendered = format!("{:?}", ClaudeAuth::oauth_token("super-secret").unwrap());
        assert!(rendered.contains(OAUTH_TOKEN_ENV));
        assert!(!rendered.contains("super-secret"));
    }
}
