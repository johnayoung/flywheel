//! TLS for every Postgres connection flywheel opens.
//!
//! The Rust driver (`postgres` / `tokio-postgres`) understands only
//! `sslmode=disable|prefer|require` and has no notion of root or client
//! certificates, while operators write libpq DSNs (the Python store hands
//! them to psycopg, i.e. libpq, verbatim). This module is the single place
//! that bridges the two: [`configure`] strips the libpq TLS keywords out of
//! the DSN, resolves them the way libpq does, and returns the driver
//! [`Config`] plus a rustls connector whose certificate verification
//! matches the requested mode.
//!
//! libpq semantics reproduced here:
//!
//! - `sslmode` defaults to `prefer`; `allow` is treated as `prefer` (the
//!   driver cannot try plaintext first, and TLS-when-offered is strictly
//!   safer than the libpq ordering).
//! - `disable` / `allow` / `prefer` / `require` verify the server chain
//!   (verify-ca) only when a root certificate file exists -- `sslrootcert`
//!   or the default `~/.postgresql/root.crt` -- and otherwise encrypt
//!   without verification.
//! - `verify-ca` checks the chain against the roots; `verify-full` also
//!   checks the server name. Both refuse to start without a root file.
//! - `sslrootcert=system` uses the platform trust store and forces
//!   `verify-full`; any weaker explicit `sslmode` is rejected.
//! - `sslcert` / `sslkey` (default `~/.postgresql/postgresql.crt` /
//!   `.key`) present a client certificate when the certificate file exists.
//! - Unset DSN keywords fall back to `PGSSLMODE`, `PGSSLROOTCERT`,
//!   `PGSSLCERT`, `PGSSLKEY`.
//!
//! Both DSN syntaxes are accepted: `postgresql://...?sslmode=...` URLs and
//! `key=value` connection strings.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use postgres::Client;
use postgres::config::{Config, SslMode as DriverSslMode};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres_rustls::MakeRustlsConnect;

/// The libpq `sslmode` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> Result<Self, PgTlsError> {
        Ok(match value {
            "disable" => Self::Disable,
            "allow" => Self::Allow,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            other => {
                return Err(PgTlsError(format!(
                    "invalid sslmode value: \"{other}\" (expected disable, allow, prefer, \
                     require, verify-ca or verify-full)"
                )));
            }
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    fn driver(self) -> DriverSslMode {
        match self {
            Self::Disable => DriverSslMode::Disable,
            Self::Allow | Self::Prefer => DriverSslMode::Prefer,
            Self::Require | Self::VerifyCa | Self::VerifyFull => DriverSslMode::Require,
        }
    }
}

/// How the server certificate is checked once TLS is negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Encrypt only; any certificate is accepted (handshake signatures are
    /// still checked).
    None,
    /// The chain must lead to a trusted root; the name is not checked.
    Chain,
    /// Chain and server name (libpq `verify-full`).
    Full,
}

/// A DSN or certificate problem found before any network traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgTlsError(String);

impl fmt::Display for PgTlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PgTlsError {}

/// The libpq TLS keywords pulled out of a DSN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TlsKeywords {
    sslmode: Option<String>,
    sslrootcert: Option<String>,
    sslcert: Option<String>,
    sslkey: Option<String>,
}

impl TlsKeywords {
    /// Claim `key`/`value` when it is one of ours; `false` leaves it to the
    /// driver.
    fn take(&mut self, key: &str, value: String) -> bool {
        let slot = match key {
            "sslmode" => &mut self.sslmode,
            "sslrootcert" => &mut self.sslrootcert,
            "sslcert" => &mut self.sslcert,
            "sslkey" => &mut self.sslkey,
            _ => return false,
        };
        *slot = Some(value);
        true
    }
}

/// The resolved TLS plan for one DSN; exposed for diagnostics and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPlan {
    pub mode: SslMode,
    pub verification: Verification,
    /// Where trusted roots come from: a PEM file, or `None` with
    /// [`TlsPlan::system_roots`] for the platform store.
    pub root_cert: Option<PathBuf>,
    pub system_roots: bool,
    /// Client certificate and key, when one is presented.
    pub client_cert: Option<(PathBuf, PathBuf)>,
}

/// Parse `dsn`, resolve its TLS settings against the process environment,
/// and return the driver config with a matching rustls connector.
pub fn configure(dsn: &str) -> Result<(Config, MakeRustlsConnect), PgTlsError> {
    configure_with(dsn, &|name| std::env::var(name).ok())
}

/// [`configure`] with an injected environment lookup.
pub fn configure_with(
    dsn: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Config, MakeRustlsConnect), PgTlsError> {
    let (driver_dsn, keywords) = split_dsn(dsn)?;
    let plan = plan(&keywords, env)?;
    let mut config: Config = driver_dsn
        .parse()
        .map_err(|err: postgres::Error| PgTlsError(format!("invalid connection string: {err}")))?;
    config.ssl_mode(plan.mode.driver());
    let tls = MakeRustlsConnect::new(client_config(&plan)?);
    Ok((config, tls))
}

/// The driver config for `dsn` with the libpq TLS keywords stripped, for
/// callers that only inspect it (host, port, password); no TLS resolution.
pub fn parse_config(dsn: &str) -> Result<Config, PgTlsError> {
    let (driver_dsn, _) = split_dsn(dsn)?;
    driver_dsn
        .parse()
        .map_err(|err: postgres::Error| PgTlsError(format!("invalid connection string: {err}")))
}

/// Resolve the TLS plan for `dsn` without building a connector.
pub fn plan_for(dsn: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<TlsPlan, PgTlsError> {
    let (_, keywords) = split_dsn(dsn)?;
    plan(&keywords, env)
}

/// Open one blocking client for `dsn` with TLS per its `sslmode`.
pub fn connect(dsn: &str) -> Result<Client, ConnectError> {
    let (config, tls) = configure(dsn).map_err(ConnectError::Tls)?;
    config.connect(tls).map_err(ConnectError::Postgres)
}

/// Replace `client` with a fresh connection when the driver has observed it
/// closed (server restart, failover, a dropped idle socket), running
/// `session_sql` (e.g. the `search_path` pin) on the new session first.
///
/// The call that hit the dead socket has already failed; this heals the
/// next one, the same contract as a pool discarding a broken connection.
pub fn ensure_open(client: &mut Client, dsn: &str, session_sql: &str) -> Result<(), ConnectError> {
    if !client.is_closed() {
        return Ok(());
    }
    let mut fresh = connect(dsn)?;
    fresh
        .batch_execute(session_sql)
        .map_err(ConnectError::Postgres)?;
    *client = fresh;
    Ok(())
}

/// Why [`connect`] failed.
#[derive(Debug)]
pub enum ConnectError {
    Tls(PgTlsError),
    Postgres(postgres::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tls(err) => err.fmt(f),
            Self::Postgres(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tls(err) => Some(err),
            Self::Postgres(err) => Some(err),
        }
    }
}

fn plan(
    keywords: &TlsKeywords,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<TlsPlan, PgTlsError> {
    let pick = |own: &Option<String>, var: &str| {
        own.clone()
            .or_else(|| env(var))
            .filter(|value| !value.is_empty())
    };
    let explicit_mode = pick(&keywords.sslmode, "PGSSLMODE")
        .map(|value| SslMode::parse(&value))
        .transpose()?;
    let rootcert = pick(&keywords.sslrootcert, "PGSSLROOTCERT");
    let home = env("HOME").map(PathBuf::from);
    let default_file = |name: &str| home.as_ref().map(|h| h.join(".postgresql").join(name));

    let client_cert = client_cert(
        pick(&keywords.sslcert, "PGSSLCERT")
            .map(PathBuf::from)
            .or_else(|| default_file("postgresql.crt")),
        pick(&keywords.sslkey, "PGSSLKEY")
            .map(PathBuf::from)
            .or_else(|| default_file("postgresql.key")),
    )?;

    if rootcert.as_deref() == Some("system") {
        if let Some(mode) = explicit_mode
            && mode != SslMode::VerifyFull
        {
            return Err(PgTlsError(format!(
                "weak sslmode \"{}\" may not be used with sslrootcert=system (use \"verify-full\")",
                mode.as_str()
            )));
        }
        return Ok(TlsPlan {
            mode: SslMode::VerifyFull,
            verification: Verification::Full,
            root_cert: None,
            system_roots: true,
            client_cert,
        });
    }

    let mode = explicit_mode.unwrap_or(SslMode::Prefer);
    let root_path = rootcert
        .map(PathBuf::from)
        .or_else(|| default_file("root.crt"));
    let root_present = root_path.as_deref().is_some_and(Path::is_file);
    let verification = match mode {
        SslMode::VerifyFull | SslMode::VerifyCa if !root_present => {
            let shown = root_path.as_deref().map_or_else(
                || "~/.postgresql/root.crt".to_owned(),
                |p| p.display().to_string(),
            );
            return Err(PgTlsError(format!(
                "root certificate file \"{shown}\" does not exist; either provide the file, \
                 use the system's trusted roots with sslrootcert=system, or change sslmode \
                 to disable server certificate verification"
            )));
        }
        SslMode::VerifyFull => Verification::Full,
        _ if root_present => Verification::Chain,
        _ => Verification::None,
    };
    Ok(TlsPlan {
        mode,
        verification,
        root_cert: root_present.then_some(root_path).flatten(),
        system_roots: false,
        client_cert,
    })
}

/// The client certificate pair, when the certificate file exists (libpq
/// silently skips a missing certificate but refuses a certificate without
/// its key).
fn client_cert(
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
) -> Result<Option<(PathBuf, PathBuf)>, PgTlsError> {
    let Some(cert) = cert.filter(|path| path.is_file()) else {
        return Ok(None);
    };
    match key.filter(|path| path.is_file()) {
        Some(key) => Ok(Some((cert, key))),
        None => Err(PgTlsError(format!(
            "certificate present, but not private key file for \"{}\"",
            cert.display()
        ))),
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn client_config(plan: &TlsPlan) -> Result<ClientConfig, PgTlsError> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|err| PgTlsError(format!("TLS setup failed: {err}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = match plan.verification {
        Verification::None => Arc::new(AcceptAnyCertificate {
            algorithms: provider.signature_verification_algorithms,
        }),
        Verification::Chain | Verification::Full => {
            let roots = Arc::new(root_store(plan)?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, Arc::clone(&provider))
                .build()
                .map_err(|err| PgTlsError(format!("TLS setup failed: {err}")))?;
            if plan.verification == Verification::Full {
                webpki
            } else {
                Arc::new(ChainOnly { inner: webpki })
            }
        }
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let mut config = match &plan.client_cert {
        Some((cert, key)) => {
            let chain = load_certs(cert)?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|err| {
                PgTlsError(format!(
                    "could not load private key file \"{}\": {err}",
                    key.display()
                ))
            })?;
            builder.with_client_auth_cert(chain, key).map_err(|err| {
                PgTlsError(format!(
                    "client certificate \"{}\" rejected: {err}",
                    cert.display()
                ))
            })?
        }
        None => builder.with_no_client_auth(),
    };
    // libpq 17+ always offers the "postgresql" ALPN protocol; servers that
    // predate it ignore the extension, and direct TLS negotiation needs it.
    config.alpn_protocols = vec![b"postgresql".to_vec()];
    Ok(config)
}

fn root_store(plan: &TlsPlan) -> Result<RootCertStore, PgTlsError> {
    let mut roots = RootCertStore::empty();
    if plan.system_roots {
        let loaded = rustls_native_certs::load_native_certs();
        if loaded.certs.is_empty() {
            let reasons: Vec<String> = loaded.errors.iter().map(ToString::to_string).collect();
            return Err(PgTlsError(format!(
                "sslrootcert=system: no trusted roots found in the platform store ({})",
                reasons.join("; ")
            )));
        }
        roots.add_parsable_certificates(loaded.certs);
        return Ok(roots);
    }
    let path = plan
        .root_cert
        .as_deref()
        .ok_or_else(|| PgTlsError("no root certificate file to verify against".to_owned()))?;
    let (added, _) = roots.add_parsable_certificates(load_certs(path)?);
    if added == 0 {
        return Err(PgTlsError(format!(
            "root certificate file \"{}\" holds no usable certificates",
            path.display()
        )));
    }
    Ok(roots)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, PgTlsError> {
    let unreadable = |err: &dyn fmt::Display| {
        PgTlsError(format!(
            "could not read certificate file \"{}\": {err}",
            path.display()
        ))
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|err| unreadable(&err))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| unreadable(&err))?;
    if certs.is_empty() {
        return Err(unreadable(&"no PEM certificates found"));
    }
    Ok(certs)
}

/// libpq `sslmode=require` without a root file: encrypt, trust any
/// certificate, but still verify the handshake signatures so the session
/// is bound to the presented key.
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// libpq `verify-ca`: the full webpki chain check, with a name mismatch
/// (reported only after the chain validated) forgiven.
#[derive(Debug)]
struct ChainOnly {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Split `dsn` into the driver-parsable remainder and our TLS keywords.
fn split_dsn(dsn: &str) -> Result<(String, TlsKeywords), PgTlsError> {
    let trimmed = dsn.trim();
    if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
        Ok(split_url(trimmed))
    } else {
        split_key_value(trimmed)
    }
}

fn split_url(url: &str) -> (String, TlsKeywords) {
    let mut keywords = TlsKeywords::default();
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_owned(), keywords);
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            !keywords.take(&percent_decode(key), percent_decode(value))
        })
        .collect();
    if kept.is_empty() {
        (base.to_owned(), keywords)
    } else {
        (format!("{base}?{}", kept.join("&")), keywords)
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parse a libpq `key = value` connection string, claiming our keywords
/// and re-serializing the rest with every value single-quoted.
fn split_key_value(dsn: &str) -> Result<(String, TlsKeywords), PgTlsError> {
    let mut keywords = TlsKeywords::default();
    let mut kept: Vec<String> = Vec::new();
    let mut chars = dsn.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next() != Some('=') {
            return Err(PgTlsError(format!(
                "invalid connection string: missing \"=\" after \"{key}\""
            )));
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\\') => match chars.next() {
                        Some(c) => value.push(c),
                        None => break,
                    },
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => {
                        return Err(PgTlsError(
                            "invalid connection string: unterminated quoted value".to_owned(),
                        ));
                    }
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        value.push(escaped);
                    }
                } else {
                    value.push(c);
                }
            }
        }
        if !keywords.take(&key, value.clone()) {
            let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
            kept.push(format!("{key}='{escaped}'"));
        }
    }
    Ok((kept.join(" "), keywords))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn url_keywords_are_stripped_and_the_rest_kept() {
        let (rest, kw) = split_url(
            "postgresql://u:p@db:5432/app?sslmode=verify-full&application_name=fw&sslrootcert=%2Ftmp%2Fca.pem",
        );
        assert_eq!(rest, "postgresql://u:p@db:5432/app?application_name=fw");
        assert_eq!(kw.sslmode.as_deref(), Some("verify-full"));
        assert_eq!(kw.sslrootcert.as_deref(), Some("/tmp/ca.pem"));
    }

    #[test]
    fn url_without_remaining_query_drops_the_question_mark() {
        let (rest, kw) = split_url("postgres://h/db?sslmode=require");
        assert_eq!(rest, "postgres://h/db");
        assert_eq!(kw.sslmode.as_deref(), Some("require"));
    }

    #[test]
    fn key_value_strings_handle_quotes_and_escapes() {
        let (rest, kw) = split_key_value(
            "host=db  user = 'o\\'neil' password='a b' sslmode=verify-ca sslrootcert='/c a.pem'",
        )
        .unwrap();
        assert_eq!(rest, "host='db' user='o\\'neil' password='a b'");
        assert_eq!(kw.sslmode.as_deref(), Some("verify-ca"));
        assert_eq!(kw.sslrootcert.as_deref(), Some("/c a.pem"));
        let config: Config = rest.parse().unwrap();
        assert_eq!(config.get_user(), Some("o'neil"));
        assert_eq!(config.get_password(), Some(&b"a b"[..]));
    }

    #[test]
    fn key_value_rejects_malformed_input() {
        assert!(split_key_value("host").is_err());
        assert!(split_key_value("host='db").is_err());
    }

    #[test]
    fn default_is_prefer_without_verification() {
        let plan = plan_for("postgresql://h/db", &no_env).unwrap();
        assert_eq!(plan.mode, SslMode::Prefer);
        assert_eq!(plan.verification, Verification::None);
    }

    #[test]
    fn environment_fills_unset_keywords_but_dsn_wins() {
        let env = |name: &str| (name == "PGSSLMODE").then(|| "disable".to_owned());
        assert_eq!(plan_for("host=h", &env).unwrap().mode, SslMode::Disable);
        assert_eq!(
            plan_for("host=h sslmode=require", &env).unwrap().mode,
            SslMode::Require
        );
    }

    #[test]
    fn verify_modes_need_a_root_file() {
        let err = plan_for("host=h sslmode=verify-full", &no_env).unwrap_err();
        assert!(err.to_string().contains("root certificate file"), "{err}");
        let err = plan_for(
            "host=h sslmode=verify-ca sslrootcert=/nonexistent/ca.pem",
            &no_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("/nonexistent/ca.pem"), "{err}");
    }

    #[test]
    fn a_present_root_file_upgrades_require_to_chain_verification() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, "x").unwrap();
        let dsn = format!("host=h sslmode=require sslrootcert={}", ca.display());
        let plan = plan_for(&dsn, &no_env).unwrap();
        assert_eq!(plan.verification, Verification::Chain);
        let dsn = format!("host=h sslmode=verify-full sslrootcert={}", ca.display());
        assert_eq!(
            plan_for(&dsn, &no_env).unwrap().verification,
            Verification::Full
        );
    }

    #[test]
    fn default_root_file_under_home_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".postgresql")).unwrap();
        std::fs::write(dir.path().join(".postgresql/root.crt"), "x").unwrap();
        let home = dir.path().display().to_string();
        let env = move |name: &str| (name == "HOME").then(|| home.clone());
        let plan = plan_for("host=h sslmode=verify-ca", &env).unwrap();
        assert_eq!(plan.verification, Verification::Chain);
        assert_eq!(
            plan.root_cert.as_deref(),
            Some(dir.path().join(".postgresql/root.crt").as_path())
        );
    }

    #[test]
    fn system_roots_force_verify_full_and_reject_weaker_modes() {
        let plan = plan_for("host=h sslrootcert=system", &no_env).unwrap();
        assert_eq!(plan.mode, SslMode::VerifyFull);
        assert!(plan.system_roots);
        let err = plan_for("host=h sslrootcert=system sslmode=require", &no_env).unwrap_err();
        assert!(err.to_string().contains("weak sslmode"), "{err}");
    }

    #[test]
    fn client_cert_without_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("client.crt");
        std::fs::write(&cert, "x").unwrap();
        let dsn = format!(
            "host=h sslcert={} sslkey={}",
            cert.display(),
            dir.path().join("missing.key").display()
        );
        let err = plan_for(&dsn, &no_env).unwrap_err();
        assert!(err.to_string().contains("not private key"), "{err}");
    }

    #[test]
    fn missing_client_cert_is_skipped() {
        let plan = plan_for("host=h sslcert=/nonexistent/client.crt", &no_env).unwrap();
        assert_eq!(plan.client_cert, None);
    }

    #[test]
    fn invalid_sslmode_is_rejected() {
        let err = plan_for("host=h sslmode=sometimes", &no_env).unwrap_err();
        assert!(err.to_string().contains("invalid sslmode"), "{err}");
    }

    #[test]
    fn configure_maps_modes_onto_the_driver() {
        let (config, _) = configure_with("host=h sslmode=disable", &no_env).unwrap();
        assert_eq!(config.get_ssl_mode(), DriverSslMode::Disable);
        let (config, _) = configure_with("postgresql://h/db?sslmode=allow", &no_env).unwrap();
        assert_eq!(config.get_ssl_mode(), DriverSslMode::Prefer);
        let (config, _) = configure_with("host=h sslmode=require", &no_env).unwrap();
        assert_eq!(config.get_ssl_mode(), DriverSslMode::Require);
    }
}
