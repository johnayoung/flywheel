//! Client-side networking shared by the scripting endpoint
//! ([`crate::endpoint`]), the machine proxy ([`crate::machines`]), and the
//! notify webhook ([`crate::notify`]): rustls client configs on ring (never
//! a process-wide default provider), the WebSocket connect, and how a
//! failed request and an API error body read, and the pinning verifier
//! machines and `--url` hubs use.

use rustls::pki_types::CertificateDer;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use url::Url;

/// A failed HTTP request for people: what failed, then every distinct
/// cause down the chain (a TLS reason or "Connection refused" sits a few
/// sources down; the first is often only hyper's "client error
/// (Connect)"). Never the URL: only the sources are shown, and reqwest
/// names the URL in the top error alone (a webhook's URL is a secret).
pub fn describe(error: &reqwest::Error) -> String {
    let what = if error.is_connect() {
        "cannot connect"
    } else if error.is_timeout() {
        "timed out"
    } else if error.is_builder() {
        "cannot send"
    } else {
        "request failed"
    };
    let mut causes: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !causes.contains(&text) {
            causes.push(text);
        }
        source = cause.source();
    }
    if causes.is_empty() {
        what.to_owned()
    } else {
        format!("{what}: {}", causes.join(": "))
    }
}

/// An API answer's error: its `{"error": ...}` message, else the body's
/// text (at most 300 characters).
pub fn error_text(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            String::from_utf8_lossy(body)
                .trim()
                .chars()
                .take(300)
                .collect()
        })
}

/// Anything a WebSocket can run over (plain TCP or TLS).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A client WebSocket (to a hub, or a machine through it).
pub type Socket = WebSocketStream<Box<dyn Io>>;

/// The crypto provider every client config is built on.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The platform's trusted roots; an error naming `what` needs them (an
/// https hub, machine, notify URL) when there are none.
pub fn native_roots(what: &str) -> Result<rustls::RootCertStore, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(format!(
            "no trusted root certificates found on this system for {what}"
        ));
    }
    Ok(roots)
}

/// A client config verifying servers against `roots`.
pub fn roots_config(roots: rustls::RootCertStore) -> Result<rustls::ClientConfig, String> {
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// A client config verifying servers with `verifier` (built on
/// `provider`, which it must share).
pub fn verifier_config(
    provider: Arc<rustls::crypto::CryptoProvider>,
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
) -> Result<rustls::ClientConfig, String> {
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

/// Verifies a server's certificate: against the system roots plus any
/// extra ones (a hub's local CA), or -- with a pin -- by the SHA-256 of
/// its own certificate (a self-signed hub: like an SSH host key, the pin
/// is the trust; names and expiry are not checked) or of a certificate it
/// chains to (a hub's local CA, `fw hub tls ca`: then the chain is
/// verified against that CA alone, names, expiry, and the CA's name
/// constraints included). Either way it records the fingerprint to pin
/// (the top of the chain presented), so a refusal can name it.
#[derive(Debug)]
struct PinVerifier {
    roots: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    pin: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let presented = sha256_hex(end_entity.as_ref());
        // What to pin: the top of the chain presented (a hub's local CA
        // outlives its short-lived leaves), else the certificate itself.
        let top = intermediates
            .last()
            .map_or_else(|| presented.clone(), |c| sha256_hex(c.as_ref()));
        *self.observed.lock().unwrap_or_else(PoisonError::into_inner) = Some(top.clone());
        let matches = |pin: &str, hex: &str| {
            bool::from(subtle::ConstantTimeEq::ct_eq(
                pin.as_bytes(),
                hex.as_bytes(),
            ))
        };
        match (&self.pin, &self.roots) {
            (Some(pin), _) => {
                if matches(pin, &presented) {
                    return Ok(rustls::client::danger::ServerCertVerified::assertion());
                }
                let Some(anchor) = intermediates
                    .iter()
                    .find(|c| matches(pin, &sha256_hex(c.as_ref())))
                else {
                    // Name what to pin: the CA a short-lived leaf chains
                    // to, not the leaf (it changes at every renewal).
                    let what = if intermediates.is_empty() {
                        "certificate fingerprint"
                    } else {
                        "its CA's fingerprint"
                    };
                    return Err(rustls::Error::General(format!(
                        "{what} {} does not match the pinned {}",
                        crate::machines::display_fingerprint(&top),
                        crate::machines::display_fingerprint(pin)
                    )));
                };
                let mut store = rustls::RootCertStore::empty();
                store
                    .add(anchor.clone().into_owned())
                    .map_err(|e| rustls::Error::General(format!("the pinned CA: {e}")))?;
                let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(store),
                    Arc::clone(&self.provider),
                )
                .build()
                .map_err(|e| rustls::Error::General(format!("the pinned CA: {e}")))?;
                let rest: Vec<CertificateDer<'_>> = intermediates
                    .iter()
                    .filter(|c| c.as_ref() != anchor.as_ref())
                    .cloned()
                    .collect();
                verifier.verify_server_cert(end_entity, &rest, server_name, ocsp_response, now)
            }
            (None, Some(roots)) => {
                roots.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            }
            (None, None) => Err(rustls::Error::General("no trust anchors".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A client config verifying with [`PinVerifier`]: `pin` (lowercase hex)
/// when given, else the system roots plus `extra_roots`; an error naming
/// `what` when there is nothing to trust. `observed` gets the fingerprint
/// of each certificate presented.
pub fn pinned_config(
    pin: Option<&str>,
    extra_roots: &[CertificateDer<'static>],
    observed: Arc<Mutex<Option<String>>>,
    what: &str,
) -> Result<rustls::ClientConfig, String> {
    let provider = provider();
    let roots = match pin {
        Some(_) => None,
        None => {
            let mut store = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = store.add(cert);
            }
            for cert in extra_roots {
                store
                    .add(cert.clone())
                    .map_err(|e| format!("a trusted CA certificate: {e}"))?;
            }
            if store.is_empty() {
                return Err(format!(
                    "no trusted root certificates found on this system for {what}"
                ));
            }
            Some(
                rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(store),
                    Arc::clone(&provider),
                )
                .build()
                .map_err(|e| format!("TLS setup failed: {e}"))?,
            )
        }
    };
    let verifier = PinVerifier {
        roots,
        pin: pin.map(str::to_owned),
        observed,
        provider: Arc::clone(&provider),
    };
    let mut config = verifier_config(provider, Arc::new(verifier))?;
    // No session resumption: a resumed session skips the certificate (and
    // so the pin) altogether, which would keep a hub "online" under a pin
    // its rotated CA no longer matches.
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

/// The config reqwest wants even for plain http (never used to verify).
pub fn plain_config() -> Result<rustls::ClientConfig, String> {
    roots_config(rustls::RootCertStore::empty())
}

/// Why [`connect_ws`] failed, for the caller to word.
#[derive(Debug)]
pub enum WsError {
    /// The URL or credential cannot form a request.
    Invalid(String),
    /// TCP did not connect.
    Connect(String),
    /// The TLS handshake failed.
    Tls(String),
    /// The WebSocket handshake failed (`Http`: the server answered, not
    /// upgraded).
    Handshake(tungstenite::Error),
}

/// Open a WebSocket to `url` (`http`/`https`, switched to `ws`/`wss`) with
/// `Authorization: Bearer <bearer>`, over TLS with `tls` when given; frames
/// and messages up to `max_message` bytes. The caller bounds the time.
pub async fn connect_ws(
    url: &Url,
    bearer: &str,
    tls: Option<&Arc<rustls::ClientConfig>>,
    max_message: usize,
) -> Result<Socket, WsError> {
    let mut url = url.clone();
    let secure = url.scheme() == "https";
    url.set_scheme(if secure { "wss" } else { "ws" })
        .map_err(|()| WsError::Invalid("cannot form the WebSocket URL".into()))?;
    let host = url
        .host_str()
        .ok_or_else(|| WsError::Invalid("the URL has no host".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WsError::Invalid("the URL has no port".into()))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| WsError::Invalid(format!("bad WebSocket request: {e}")))?;
    let bearer = format!("Bearer {bearer}")
        .parse()
        .map_err(|_| WsError::Invalid("the credential is not a valid header value".into()))?;
    request.headers_mut().insert("authorization", bearer);
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| WsError::Connect(e.to_string()))?;
    let _ = tcp.set_nodelay(true);
    let stream: Box<dyn Io> = match tls {
        Some(config) => {
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| WsError::Invalid(format!("bad TLS server name {host:?}: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::clone(config))
                .connect(name, tcp)
                .await
                .map_err(|e| WsError::Tls(e.to_string()))?;
            Box::new(tls)
        }
        None => Box::new(tcp),
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(max_message))
        .max_frame_size(Some(max_message));
    let (socket, _) = tokio_tungstenite::client_async_with_config(request, stream, Some(config))
        .await
        .map_err(WsError::Handshake)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_url_without_a_host_or_port_is_refused_before_connecting() {
        let url = Url::parse("unix:/tmp/x").unwrap();
        let Err(err) = connect_ws(&url, "t", None, 1024).await else {
            panic!("connected without a host");
        };
        assert!(matches!(err, WsError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn configs_build_without_a_process_wide_provider() {
        plain_config().unwrap();
        roots_config(rustls::RootCertStore::empty()).unwrap();
    }
}
