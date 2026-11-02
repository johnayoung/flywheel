//! Hub authentication: the state-dir token, the login cookie, and the
//! same-origin check.
//!
//! A hub serves shells over HTTP -- remote code execution by design -- so
//! every route except the login page requires the token. The token is 256
//! random bits, hex-encoded, stored `0600` in the state dir, and compared in
//! constant time. Browsers trade it once (`/login?token=...`) for an
//! `HttpOnly; SameSite=Strict` cookie; scripts send `Authorization: Bearer`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use subtle::ConstantTimeEq;

/// Token file name inside the state dir.
pub const TOKEN_FILE: &str = "token";
/// The login cookie's name.
pub const COOKIE_NAME: &str = "fw_hub";
/// Login cookie lifetime: 30 days (rotating the token invalidates it sooner).
pub const COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
/// Random bytes in a token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Token file failures.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("token file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("token file {path} is malformed (expected {expected} hex characters)")]
    Malformed { path: PathBuf, expected: usize },
    #[error("system random source failed: {0}")]
    Random(String),
}

/// The hub's shared secret.
#[derive(Clone)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// A fresh random token.
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| TokenError::Random(e.to_string()))?;
        Ok(Self(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        ))
    }

    /// The token text (for the login URL and `fw hub token`).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented credential.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        // Token length is public (fixed); only content must not leak.
        expected.len() == presented.len() && bool::from(expected.ct_eq(presented))
    }

    /// Load the token from `state_dir`, creating it (mode 0600) if absent.
    pub fn load_or_create(state_dir: &Path) -> Result<Self, TokenError> {
        let path = state_dir.join(TOKEN_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let text = text.trim();
                let expected = TOKEN_BYTES * 2;
                if text.len() != expected || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(TokenError::Malformed { path, expected });
                }
                Ok(Self(text.to_ascii_lowercase()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let token = Self::generate()?;
                write_private(&path, format!("{}\n", token.0).as_bytes())
                    .map_err(|source| TokenError::Io { path, source })?;
                Ok(token)
            }
            Err(source) => Err(TokenError::Io { path, source }),
        }
    }
}

/// Write `bytes` to `path` atomically with mode 0600 (temp file + rename).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            // A pre-existing temp file keeps its old mode; force 0600.
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// The login cookie value from a `Cookie` header, if present.
pub fn cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value.trim().trim_matches('"'))
    })
}

/// The bearer credential from an `Authorization` header, if present.
pub fn bearer_value(authorization: &str) -> Option<&str> {
    let (scheme, value) = authorization.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(value.trim())
}

/// The `Set-Cookie` value that logs a browser in.
pub fn login_cookie(token: &Token, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE_SECS}{}",
        token.expose(),
        if secure { "; Secure" } else { "" }
    )
}

/// The `Set-Cookie` value that logs a browser out.
pub fn logout_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Whether a request's `Origin` is acceptable for a state-changing or
/// WebSocket request.
///
/// * no `Origin` header: a non-browser client (curl, the CLI); allowed --
///   the credential check still applies;
/// * `Origin` whose `host[:port]` equals the request's `Host`: same-origin;
/// * `Origin` listed verbatim in `allowed` (e.g. a reverse proxy's public
///   origin): allowed;
/// * anything else, including the opaque `null` origin: rejected.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, allowed: &[String]) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let origin = origin.trim().trim_end_matches('/');
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let Some(host) = host else {
        return false;
    };
    normalize_authority(authority, scheme) == normalize_authority(host.trim(), scheme)
}

/// Lowercase an authority and drop the scheme's default port.
fn normalize_authority(authority: &str, scheme: &str) -> String {
    let lower = authority.to_ascii_lowercase();
    let default = if scheme.eq_ignore_ascii_case("https") {
        ":443"
    } else {
        ":80"
    };
    lower
        .strip_suffix(default)
        .map(str::to_owned)
        .unwrap_or(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_256_bit_hex_and_distinct() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.expose(), b.expose());
        assert_eq!(format!("{a:?}"), "Token(<redacted>)");
    }

    #[test]
    fn matches_is_exact() {
        let t = Token::generate().unwrap();
        assert!(t.matches(t.expose()));
        assert!(!t.matches(""));
        assert!(!t.matches(&t.expose()[..63]));
        let mut flipped = t.expose().to_owned();
        let last = if flipped.ends_with('0') { "1" } else { "0" };
        flipped.replace_range(63.., last);
        assert!(!t.matches(&flipped));
    }

    #[test]
    fn token_file_is_created_private_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let created = Token::load_or_create(dir.path()).unwrap();
        let reloaded = Token::load_or_create(dir.path()).unwrap();
        assert_eq!(created.expose(), reloaded.expose());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn malformed_token_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(TOKEN_FILE), "short\n").unwrap();
        assert!(matches!(
            Token::load_or_create(dir.path()),
            Err(TokenError::Malformed { .. })
        ));
    }

    #[test]
    fn parses_cookie_and_bearer_headers() {
        assert_eq!(
            cookie_value("a=1; fw_hub=abc; b=2", COOKIE_NAME),
            Some("abc")
        );
        assert_eq!(cookie_value("fw_hubx=abc", COOKIE_NAME), None);
        assert_eq!(bearer_value("Bearer xyz"), Some("xyz"));
        assert_eq!(bearer_value("bearer  xyz "), Some("xyz"));
        assert_eq!(bearer_value("Basic xyz"), None);
    }

    #[test]
    fn login_cookie_is_http_only_and_strict() {
        let t = Token::generate().unwrap();
        let c = login_cookie(&t, false);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict"));
        assert!(!c.contains("Secure"));
        assert!(login_cookie(&t, true).ends_with("; Secure"));
        assert!(logout_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn origin_rules() {
        let host = Some("127.0.0.1:7420");
        assert!(origin_allowed(None, host, &[]));
        assert!(origin_allowed(Some("http://127.0.0.1:7420"), host, &[]));
        assert!(origin_allowed(Some("HTTP://127.0.0.1:7420/"), host, &[]));
        assert!(!origin_allowed(Some("http://evil.example"), host, &[]));
        assert!(!origin_allowed(Some("http://127.0.0.1:9999"), host, &[]));
        assert!(!origin_allowed(Some("null"), host, &[]));
        assert!(!origin_allowed(Some("file://127.0.0.1:7420"), host, &[]));
        assert!(!origin_allowed(Some("http://127.0.0.1:7420"), None, &[]));
        assert!(origin_allowed(
            Some("https://box.ts.net"),
            Some("box.ts.net:443"),
            &[]
        ));
        let allow = vec!["https://hub.example.com".to_owned()];
        assert!(origin_allowed(
            Some("https://hub.example.com"),
            host,
            &allow
        ));
    }
}
