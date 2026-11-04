//! Built-in TLS (`fw hub serve --tls auto|off|CERT,KEY`, hub.toml
//! `[server] tls`): https on the hub's own port, so a phone on the LAN gets
//! a secure context (service worker, notifications, clipboard) without
//! Tailscale.
//!
//! `auto` keeps a **local CA** in `<state-dir>/tls/` (ECDSA P-256; its key
//! is 0600 and never leaves that directory) and serves a short-lived leaf
//! it signs for every name the hub is reached at: loopback, `localhost`,
//! this machine's host name (`<host>.local` for a single-label one: a bare
//! label as a constraint would cover a whole TLD), its LAN addresses, its
//! tailnet name and addresses, and `[server] tls_names`. The leaf is
//! re-issued when those change and before it expires. `tls_names` takes
//! private names only ([`PRIVATE_RULE`]); a public one needs an opt-in by
//! name (`--tls-allow-public-name`, `[server] tls_public_names`), and
//! everything that shows the CA then warns of it.
//!
//! **The CA is name-constrained** (X.509 nameConstraints, critical) to
//! exactly those names and addresses as they were when it was created:
//! installing it on a phone lets it vouch for this hub and nothing else --
//! a stolen CA key cannot impersonate any other site to that phone. An
//! address added later (a new DHCP lease) is outside the constraints: it is
//! left out of the leaf (with a warning) until `fw hub tls rotate` makes a
//! new CA that covers it -- and every device re-installs it. Rotation is
//! also revocation: the old key is deleted, so nothing new is ever signed
//! by the old CA.
//!
//! `CERT,KEY` serves PEM files you supply (re-read when they change); there
//! is no local CA then.

use crate::auth::write_private;
use crate::remote::Tailscale;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The directory inside the state dir holding the local CA.
pub const TLS_DIR: &str = "tls";
/// The CA certificate (PEM) inside [`TLS_DIR`].
pub const CA_CERT_FILE: &str = "ca-cert.pem";
/// The CA private key (PEM, 0600) inside [`TLS_DIR`].
pub const CA_KEY_FILE: &str = "ca-key.pem";
/// How long a local CA is valid.
pub const CA_VALIDITY: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
/// How long a leaf is valid: short, so a leaked leaf key soon stops
/// mattering; re-issued automatically.
pub const LEAF_VALIDITY: Duration = Duration::from_secs(14 * 24 * 3600);
/// A leaf with less than this left is re-issued.
pub const RENEW_BEFORE: Duration = Duration::from_secs(5 * 24 * 3600);
/// How often the running hub re-checks its names, its certificate's age,
/// and the files on disk.
pub const REFRESH_PERIOD: Duration = Duration::from_secs(60);
/// Backdating of `notBefore`, for clocks a little behind this one.
const CLOCK_SKEW: Duration = Duration::from_secs(3600);

// --- mode -------------------------------------------------------------------

/// How the hub serves TLS.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// Plain http only (the default).
    #[default]
    Off,
    /// A local, name-constrained CA and leaves it signs.
    Auto,
    /// PEM files supplied by the operator (chain first, then key).
    Files { cert: PathBuf, key: PathBuf },
}

impl TlsMode {
    pub fn is_on(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Auto => f.write_str("auto"),
            Self::Files { cert, key } => write!(f, "{},{}", cert.display(), key.display()),
        }
    }
}

/// `auto`, `off`, or `CERT,KEY` (two PEM file paths).
pub fn parse_mode(raw: &str) -> Result<TlsMode, String> {
    let raw = raw.trim();
    match raw.to_ascii_lowercase().as_str() {
        "auto" | "on" => return Ok(TlsMode::Auto),
        "off" | "" => return Ok(TlsMode::Off),
        _ => {}
    }
    let Some((cert, key)) = raw.split_once(',') else {
        return Err(format!(
            "{raw:?}: expected auto, off, or CERT,KEY (a PEM certificate chain and its key)"
        ));
    };
    let (cert, key) = (cert.trim(), key.trim());
    if cert.is_empty() || key.is_empty() || key.contains(',') {
        return Err(format!(
            "{raw:?}: expected CERT,KEY (two PEM file paths, comma-separated)"
        ));
    }
    Ok(TlsMode::Files {
        cert: PathBuf::from(cert),
        key: PathBuf::from(key),
    })
}

mod ca;
mod names;
mod serving;
pub use ca::*;
pub use names::*;
pub use serving::*;

#[cfg(test)]
mod tests;
