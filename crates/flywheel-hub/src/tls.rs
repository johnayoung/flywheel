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
//! re-issued when those change and before it expires.
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

// --- names ------------------------------------------------------------------

/// An extra name for the certificate (`[server] tls_names`, `--tls-name`):
/// a DNS name, an IP address, or an IP network (`10.0.0.0/8`: the CA may
/// sign for any address in it; the leaf names none of it by itself).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NameSpec {
    Dns(String),
    Ip(IpAddr),
    Net(IpAddr, u8),
}

impl fmt::Display for NameSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns(name) => f.write_str(name),
            Self::Ip(ip) => write!(f, "{ip}"),
            Self::Net(ip, prefix) => write!(f, "{ip}/{prefix}"),
        }
    }
}

/// One `tls_names` entry.
pub fn parse_name(raw: &str) -> Result<NameSpec, String> {
    let raw = raw.trim();
    if let Some((addr, prefix)) = raw.split_once('/') {
        let ip: IpAddr = addr
            .parse()
            .map_err(|_| format!("{raw:?}: not an IP network (e.g. 10.0.0.0/8)"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = prefix
            .parse()
            .ok()
            .filter(|p| *p <= max)
            .ok_or_else(|| format!("{raw:?}: the prefix must be 0..={max}"))?;
        if prefix < if ip.is_ipv4() { 8 } else { 16 } {
            return Err(format!(
                "{raw:?}: too wide a network for a hub's CA (at most /8, or /16 for IPv6)"
            ));
        }
        return Ok(NameSpec::Net(mask(ip, prefix), prefix));
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(NameSpec::Ip(ip));
    }
    let name = dns_name(raw)
        .ok_or_else(|| format!("{raw:?}: not a DNS name, IP address, or IP network"))?;
    if !constrainable(&name) {
        return Err(format!(
            "{raw:?}: a single-label name would let the CA vouch for every name under it (a whole \
             TLD, as X.509 name constraints read it); use a dotted name such as {name}.lan or \
             {name}.local"
        ));
    }
    Ok(NameSpec::Dns(name))
}

/// Whether a CA may be name-constrained to `name` (and so to every name
/// under it): anything with a dot, and `localhost` (reserved for loopback,
/// subdomains included). A single label such as `dev` or `box` would cover
/// a public TLD.
pub fn constrainable(name: &str) -> bool {
    name == "localhost" || name.contains('.')
}

/// `raw` as a lowercase DNS name (letters, digits, hyphens; dot-separated
/// labels of 1-63), or `None`. No wildcards: a hub has its own names.
pub fn dns_name(raw: &str) -> Option<String> {
    let name = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        // An all-numeric name would read as an address.
        && name.bytes().any(|b| b.is_ascii_alphabetic());
    ok.then_some(name)
}

/// The names a certificate is for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    pub dns: BTreeSet<String>,
    pub ips: BTreeSet<IpAddr>,
}

impl Names {
    pub fn is_empty(&self) -> bool {
        self.dns.is_empty() && self.ips.is_empty()
    }

    /// Every name, DNS names first.
    pub fn list(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(ToString::to_string))
            .collect()
    }
}

/// The names this hub is reached at: loopback and `localhost`, `host` when
/// it has a dot (`<host>.local` when it has none), every address in `interfaces`, the
/// bound address when specific, the tailnet's name and addresses, and the
/// extra names (an extra network adds no name).
pub fn hub_names(
    listen: SocketAddr,
    host: &str,
    interfaces: &[(String, IpAddr)],
    tailscale: Option<&Tailscale>,
    extra: &[NameSpec],
) -> Names {
    let mut names = Names::default();
    names.dns.insert("localhost".to_owned());
    names.ips.insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
    names.ips.insert(IpAddr::V6(Ipv6Addr::LOCALHOST));
    // A bare single-label host name is left out: as a dNSName constraint it
    // would cover every name under it -- a whole TLD when the machine is
    // called `dev` or `box`. `<host>.local` (mDNS, reserved) stands in.
    if let Some(host) = dns_name(host) {
        if host.contains('.') {
            names.dns.insert(host);
        } else {
            names.dns.insert(format!("{host}.local"));
        }
    }
    names.ips.extend(interfaces.iter().map(|(_, ip)| *ip));
    let ip = listen.ip().to_canonical();
    if !ip.is_unspecified() {
        names.ips.insert(ip);
    }
    if let Some(ts) = tailscale {
        if let Some(name) = ts.dns_name.as_deref().and_then(dns_name) {
            names.dns.insert(name);
        }
        names.ips.extend(ts.ips.iter().copied());
    }
    for spec in extra {
        match spec {
            NameSpec::Dns(name) => {
                names.dns.insert(name.clone());
            }
            NameSpec::Ip(ip) => {
                names.ips.insert(*ip);
            }
            NameSpec::Net(..) => {}
        }
    }
    names
}

/// [`hub_names`] for this machine now (asks `tailscale`, so it may take a
/// few seconds: call it off the async runtime).
pub fn discover_names(listen: SocketAddr, extra: &[NameSpec]) -> Names {
    let tailscale = crate::remote::tailscale_program()
        .and_then(|p| crate::remote::detect_tailscale(&p, listen.port()));
    hub_names(
        listen,
        &crate::machines::host_name(),
        &crate::remote::interface_addresses(),
        tailscale.as_ref(),
        extra,
    )
}

// --- constraints ------------------------------------------------------------

/// What a CA may sign for: DNS names (each with its subdomains, as X.509
/// dNSName constraints read) and IP networks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Constraints {
    pub dns: Vec<String>,
    pub nets: Vec<(IpAddr, u8)>,
}

impl Constraints {
    /// The constraints a new CA gets for `names` and the extra networks:
    /// each name, each address exactly, and all of loopback.
    pub fn for_names(names: &Names, extra: &[NameSpec]) -> Self {
        let mut dns: BTreeSet<String> = names
            .dns
            .iter()
            .filter(|name| constrainable(name))
            .cloned()
            .collect();
        dns.insert("localhost".to_owned());
        let mut nets: BTreeSet<(IpAddr, u8)> = names
            .ips
            .iter()
            .map(|ip| (*ip, if ip.is_ipv4() { 32 } else { 128 }))
            .collect();
        nets.insert((IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8));
        nets.insert((IpAddr::V6(Ipv6Addr::LOCALHOST), 128));
        for spec in extra {
            match spec {
                NameSpec::Dns(name) => {
                    if constrainable(name) {
                        dns.insert(name.clone());
                    }
                }
                NameSpec::Ip(ip) => {
                    nets.insert((*ip, if ip.is_ipv4() { 32 } else { 128 }));
                }
                NameSpec::Net(ip, prefix) => {
                    nets.insert((*ip, *prefix));
                }
            }
        }
        // An address inside a wider network listed too adds nothing.
        let all: Vec<(IpAddr, u8)> = nets.iter().copied().collect();
        let nets = all
            .iter()
            .copied()
            .filter(|(ip, prefix)| {
                !all.iter().any(|(net, wider)| {
                    wider < prefix
                        && net.is_ipv4() == ip.is_ipv4()
                        && mask(*ip, *wider) == mask(*net, *wider)
                })
            })
            .collect();
        Self {
            dns: dns.into_iter().collect(),
            nets,
        }
    }

    pub fn permits_dns(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.dns.iter().any(|c| {
            name == *c
                || name
                    .strip_suffix(c.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        })
    }

    /// DNS constraints that cover a whole TLD (a single label other than
    /// `localhost`): a CA made before such names were refused.
    pub fn too_wide(&self) -> Vec<String> {
        self.dns
            .iter()
            .filter(|name| !constrainable(name))
            .cloned()
            .collect()
    }

    pub fn permits_ip(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|(net, prefix)| {
            net.is_ipv4() == ip.is_ipv4() && mask(ip, *prefix) == mask(*net, *prefix)
        })
    }

    /// `names` split into those inside and those outside.
    pub fn split(&self, names: &Names) -> (Names, Names) {
        let mut inside = Names::default();
        let mut outside = Names::default();
        for name in &names.dns {
            let set = if self.permits_dns(name) {
                &mut inside.dns
            } else {
                &mut outside.dns
            };
            set.insert(name.clone());
        }
        for ip in &names.ips {
            let set = if self.permits_ip(*ip) {
                &mut inside.ips
            } else {
                &mut outside.ips
            };
            set.insert(*ip);
        }
        (inside, outside)
    }

    /// Every constraint as text (`name`, `ip/prefix`).
    pub fn list(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.nets.iter().map(|(ip, p)| format!("{ip}/{p}")))
            .collect()
    }

    fn to_rcgen(&self) -> rcgen::NameConstraints {
        let mut permitted: Vec<rcgen::GeneralSubtree> = self
            .dns
            .iter()
            .map(|d| rcgen::GeneralSubtree::DnsName(d.clone()))
            .collect();
        permitted.extend(self.nets.iter().map(|(ip, prefix)| {
            rcgen::GeneralSubtree::IpAddress(rcgen::CidrSubnet::from_addr_prefix(*ip, *prefix))
        }));
        rcgen::NameConstraints {
            permitted_subtrees: permitted,
            excluded_subtrees: Vec::new(),
        }
    }
}

/// Where a device on the network opens the CA page of a hub listening on
/// `listen`: its LAN addresses (none on a loopback-only bind).
pub fn ca_page_urls(listen: SocketAddr) -> Vec<String> {
    let ip = listen.ip().to_canonical();
    let ips: Vec<IpAddr> = if ip.is_loopback() {
        Vec::new()
    } else if ip.is_unspecified() {
        crate::remote::interface_addresses()
            .into_iter()
            .map(|(_, ip)| ip)
            .collect()
    } else {
        vec![ip]
    };
    ips.into_iter()
        .map(|ip| match ip {
            IpAddr::V4(v4) => format!("http://{v4}:{}/ca", listen.port()),
            IpAddr::V6(v6) => format!("http://[{v6}]:{}/ca", listen.port()),
        })
        .collect()
}

/// `ip` with all but its first `prefix` bits cleared.
fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let m = u32::MAX
                .checked_shl(32 - u32::from(prefix.min(32)))
                .unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(bits & m))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let m = u128::MAX
                .checked_shl(128 - u32::from(prefix.min(128)))
                .unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(bits & m))
        }
    }
}

/// A network's prefix length from its mask bytes (contiguous masks only).
fn prefix_of(mask: &[u8]) -> Option<u8> {
    let ones: u32 = mask.iter().map(|b| b.count_ones()).sum();
    let contiguous = mask
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1))
        .skip_while(|bit| *bit)
        .all(|bit| !bit);
    contiguous.then(|| u8::try_from(ones).ok()).flatten()
}

// --- the local CA -----------------------------------------------------------

/// The hub's local CA: its certificate, key, and constraints.
pub struct LocalCa {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key: rcgen::KeyPair,
    constraints: Constraints,
    fingerprint: String,
    created: SystemTime,
    expires: SystemTime,
}

impl fmt::Debug for LocalCa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the key.
        f.debug_struct("LocalCa")
            .field("fingerprint", &self.fingerprint)
            .field("constraints", &self.constraints)
            .finish_non_exhaustive()
    }
}

fn tls_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(TLS_DIR)
}

/// Where the local CA's certificate lives for `state_dir`.
pub fn ca_cert_path(state_dir: &Path) -> PathBuf {
    tls_dir(state_dir).join(CA_CERT_FILE)
}

fn ca_key_path(state_dir: &Path) -> PathBuf {
    tls_dir(state_dir).join(CA_KEY_FILE)
}

fn rcgen_error(what: &str, e: rcgen::Error) -> String {
    format!("{what}: {e}")
}

fn to_offset(t: SystemTime) -> Result<time::OffsetDateTime, String> {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970".to_owned())?
        .as_secs();
    time::OffsetDateTime::from_unix_timestamp(i64::try_from(secs).unwrap_or(i64::MAX))
        .map_err(|e| format!("bad time: {e}"))
}

fn from_timestamp(secs: i64) -> SystemTime {
    match u64::try_from(secs) {
        Ok(s) => UNIX_EPOCH + Duration::from_secs(s),
        Err(_) => UNIX_EPOCH,
    }
}

/// A random positive 128-bit serial number.
fn serial() -> Result<rcgen::SerialNumber, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    bytes[0] &= 0x7f;
    bytes[0] |= 0x01;
    Ok(rcgen::SerialNumber::from_slice(&bytes))
}

/// SHA-256 of `der`, lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl LocalCa {
    /// A new CA constrained to `constraints`, valid from `now` (not
    /// written anywhere).
    pub fn generate(constraints: Constraints, now: SystemTime) -> Result<Self, String> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| rcgen_error("cannot make the CA key", e))?;
        let mut params = rcgen::CertificateParams::default();
        let mut dn = rcgen::DistinguishedName::new();
        let host = crate::machines::host_name();
        let host = dns_name(&host).unwrap_or_else(|| "this machine".to_owned());
        dn.push(
            rcgen::DnType::CommonName,
            format!(
                "flywheel hub CA ({host}, {})",
                chrono::DateTime::<chrono::Utc>::from(now).format("%Y-%m-%d")
            ),
        );
        dn.push(rcgen::DnType::OrganizationName, "flywheel hub");
        params.distinguished_name = dn;
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        params.name_constraints = Some(constraints.to_rcgen());
        params.not_before = to_offset(now - CLOCK_SKEW)?;
        params.not_after = to_offset(now + CA_VALIDITY)?;
        params.serial_number = Some(serial()?);
        let cert = params
            .self_signed(&key)
            .map_err(|e| rcgen_error("cannot make the CA certificate", e))?;
        Self::from_parts(cert.pem(), key)
    }

    fn from_parts(cert_pem: String, key: rcgen::KeyPair) -> Result<Self, String> {
        use rustls::pki_types::pem::PemObject;
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| format!("the CA certificate is not PEM: {e}"))?;
        let (_, parsed) = x509_parser::parse_x509_certificate(cert_der.as_ref())
            .map_err(|e| format!("the CA certificate does not parse: {e}"))?;
        if parsed.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
            return Err("the CA key does not match its certificate".into());
        }
        let constraints = parsed_constraints(&parsed)?
            .ok_or_else(|| "the CA certificate has no name constraints".to_owned())?;
        let created = from_timestamp(parsed.validity().not_before.timestamp());
        let expires = from_timestamp(parsed.validity().not_after.timestamp());
        drop(parsed);
        Ok(Self {
            fingerprint: fingerprint(cert_der.as_ref()),
            cert_pem,
            cert_der,
            key,
            constraints,
            created: created + CLOCK_SKEW,
            expires,
        })
    }

    /// The CA in `state_dir`, if there is one.
    pub fn load(state_dir: &Path) -> Result<Option<Self>, String> {
        let cert_path = ca_cert_path(state_dir);
        let cert_pem = match std::fs::read_to_string(&cert_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", cert_path.display())),
        };
        let key_path = ca_key_path(state_dir);
        let key_pem = std::fs::read_to_string(&key_path)
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
        let key = rcgen::KeyPair::from_pem(&key_pem)
            .map_err(|e| rcgen_error(&key_path.display().to_string(), e))?;
        Self::from_parts(cert_pem, key)
            .map(Some)
            .map_err(|e| format!("{}: {e}", cert_path.display()))
    }

    /// Write this CA as `state_dir`'s (key first, both 0600, each
    /// atomically), replacing any other.
    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        let dir = tls_dir(state_dir);
        crate::paths::ensure_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let key_path = ca_key_path(state_dir);
        write_private(&key_path, self.key.serialize_pem().as_bytes())
            .map_err(|e| format!("{}: {e}", key_path.display()))?;
        let cert_path = ca_cert_path(state_dir);
        write_private(&cert_path, self.cert_pem.as_bytes())
            .map_err(|e| format!("{}: {e}", cert_path.display()))
    }

    /// `state_dir`'s CA, made (for `names` and the extra networks) and
    /// saved when it has none; `true` when it was just made.
    pub fn load_or_create(
        state_dir: &Path,
        names: &Names,
        extra: &[NameSpec],
    ) -> Result<(Self, bool), String> {
        if let Some(ca) = Self::load(state_dir)? {
            return Ok((ca, false));
        }
        let ca = Self::generate(Constraints::for_names(names, extra), SystemTime::now())?;
        ca.save(state_dir)?;
        Ok((ca, true))
    }

    /// Replace `state_dir`'s CA with a new one for `names`: every device
    /// that trusted the old one must install this one, and the old key is
    /// gone (nothing new is ever signed by it).
    pub fn rotate(state_dir: &Path, names: &Names, extra: &[NameSpec]) -> Result<Self, String> {
        let ca = Self::generate(Constraints::for_names(names, extra), SystemTime::now())?;
        ca.save(state_dir)?;
        Ok(ca)
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// SHA-256 of the certificate, lowercase hex.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn constraints(&self) -> &Constraints {
        &self.constraints
    }

    pub fn created(&self) -> SystemTime {
        self.created
    }

    pub fn expires(&self) -> SystemTime {
        self.expires
    }

    /// A leaf for every name in `names` the constraints permit (the rest
    /// are reported in [`Leaf::outside`]), valid [`LEAF_VALIDITY`] from
    /// `now`, chained to this CA.
    pub fn issue(&self, names: &Names, now: SystemTime) -> Result<Leaf, String> {
        let (inside, outside) = self.constraints.split(names);
        if inside.is_empty() {
            return Err("no name of this hub is inside its CA's constraints".into());
        }
        self.sign(&inside, now, outside, names.clone())
    }

    /// Sign a leaf for exactly `names` (constraints not consulted: a test
    /// of what verifiers make of a leaf outside them).
    pub fn sign_unchecked(&self, names: &Names, now: SystemTime) -> Result<Leaf, String> {
        self.sign(names, now, Names::default(), names.clone())
    }

    fn sign(
        &self,
        inside: &Names,
        now: SystemTime,
        outside: Names,
        requested: Names,
    ) -> Result<Leaf, String> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| rcgen_error("cannot make a leaf key", e))?;
        let mut params = rcgen::CertificateParams::default();
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "flywheel hub");
        dn.push(rcgen::DnType::OrganizationName, "flywheel hub");
        params.distinguished_name = dn;
        params.subject_alt_names = inside
            .dns
            .iter()
            .map(|name| {
                name.clone()
                    .try_into()
                    .map(rcgen::SanType::DnsName)
                    .map_err(|e: rcgen::Error| format!("{name:?}: {e}"))
            })
            .chain(
                inside
                    .ips
                    .iter()
                    .map(|ip| Ok(rcgen::SanType::IpAddress(*ip))),
            )
            .collect::<Result<_, String>>()?;
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let not_after = (now + LEAF_VALIDITY).min(self.expires);
        params.not_before = to_offset(now - CLOCK_SKEW)?;
        params.not_after = to_offset(not_after)?;
        params.serial_number = Some(serial()?);
        let issuer = rcgen::Issuer::from_ca_cert_der(&self.cert_der, &self.key)
            .map_err(|e| rcgen_error("cannot read the CA certificate", e))?;
        let cert = params
            .signed_by(&key, &issuer)
            .map_err(|e| rcgen_error("cannot sign the leaf", e))?;
        let chain = vec![cert.der().clone(), self.cert_der.clone()];
        let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let certified = CertifiedKey::from_der(chain, private, &crate::net::provider())
            .map_err(|e| format!("the leaf does not load: {e}"))?;
        Ok(Leaf {
            key: Arc::new(certified),
            names: inside.clone(),
            outside,
            requested,
            expires: not_after,
            ca_fingerprint: self.fingerprint.clone(),
        })
    }

    /// An Apple configuration profile (`.mobileconfig`) installing this CA
    /// as a root (iOS: Settings > Profile Downloaded, then Settings >
    /// General > About > Certificate Trust Settings).
    pub fn mobileconfig(&self) -> String {
        let fp = &self.fingerprint;
        let uuid = |salt: &str| {
            let h = fingerprint(format!("{salt}{fp}").as_bytes());
            format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )
            .to_ascii_uppercase()
        };
        let data = crate::commands_cli::base64(self.cert_der.as_ref());
        let name = crate::web::escape_html(&ca_common_name(self).unwrap_or_default());
        let short = &fp[..16];
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>PayloadCertificateFileName</key>
      <string>flywheel-hub-ca.crt</string>
      <key>PayloadContent</key>
      <data>{data}</data>
      <key>PayloadDescription</key>
      <string>Trusts this flywheel hub's certificates. Name-constrained: it cannot vouch for any other site.</string>
      <key>PayloadDisplayName</key>
      <string>{name}</string>
      <key>PayloadIdentifier</key>
      <string>dev.flywheel.hub.ca.{short}.cert</string>
      <key>PayloadType</key>
      <string>com.apple.security.root</string>
      <key>PayloadUUID</key>
      <string>{cert_uuid}</string>
      <key>PayloadVersion</key>
      <integer>1</integer>
    </dict>
  </array>
  <key>PayloadDescription</key>
  <string>Install to reach your flywheel hub over https. SHA-256 {display}</string>
  <key>PayloadDisplayName</key>
  <string>{name}</string>
  <key>PayloadIdentifier</key>
  <string>dev.flywheel.hub.ca.{short}</string>
  <key>PayloadRemovalDisallowed</key>
  <false/>
  <key>PayloadType</key>
  <string>Configuration</string>
  <key>PayloadUUID</key>
  <string>{profile_uuid}</string>
  <key>PayloadVersion</key>
  <integer>1</integer>
</dict>
</plist>
"#,
            cert_uuid = uuid("cert"),
            profile_uuid = uuid("profile"),
            display = crate::machines::display_fingerprint(fp),
        )
    }
}

/// The CA certificate's common name.
pub fn ca_common_name(ca: &LocalCa) -> Option<String> {
    let (_, parsed) = x509_parser::parse_x509_certificate(ca.cert_der.as_ref()).ok()?;
    parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(str::to_owned)
}

/// The name constraints of a parsed certificate: `None` when it has none.
fn parsed_constraints(
    cert: &x509_parser::certificate::X509Certificate<'_>,
) -> Result<Option<Constraints>, String> {
    use x509_parser::extensions::GeneralName;
    let Some(ext) = cert
        .name_constraints()
        .map_err(|e| format!("bad name constraints: {e}"))?
    else {
        return Ok(None);
    };
    let mut constraints = Constraints::default();
    for subtree in ext.value.permitted_subtrees.iter().flatten() {
        match &subtree.base {
            GeneralName::DNSName(name) => constraints.dns.push(name.to_ascii_lowercase()),
            GeneralName::IPAddress(bytes) if bytes.len() == 8 => {
                let addr: [u8; 4] = bytes[..4].try_into().unwrap_or_default();
                let prefix = prefix_of(&bytes[4..]).ok_or("a non-contiguous IPv4 mask")?;
                constraints.nets.push((IpAddr::V4(addr.into()), prefix));
            }
            GeneralName::IPAddress(bytes) if bytes.len() == 32 => {
                let addr: [u8; 16] = bytes[..16].try_into().unwrap_or_default();
                let prefix = prefix_of(&bytes[16..]).ok_or("a non-contiguous IPv6 mask")?;
                constraints.nets.push((IpAddr::V6(addr.into()), prefix));
            }
            _ => {}
        }
    }
    Ok(Some(constraints))
}

// --- a leaf -------------------------------------------------------------------

/// An issued certificate, ready to serve.
#[derive(Clone)]
pub struct Leaf {
    key: Arc<CertifiedKey>,
    /// The names it carries.
    pub names: Names,
    /// Names asked for that the CA may not sign for (left out).
    pub outside: Names,
    /// Everything asked for (to tell whether the names changed).
    requested: Names,
    pub expires: SystemTime,
    ca_fingerprint: String,
}

impl fmt::Debug for Leaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leaf")
            .field("names", &self.names)
            .field("outside", &self.outside)
            .finish_non_exhaustive()
    }
}

impl Leaf {
    /// The chain served: the leaf, then the CA.
    pub fn chain(&self) -> &[CertificateDer<'static>] {
        &self.key.cert
    }
}

// --- serving ------------------------------------------------------------------

/// Hands every handshake the current certificate.
#[derive(Debug)]
struct Resolver {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self.current.read().unwrap_or_else(PoisonError::into_inner),
        ))
    }
}

/// When the operator's files last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp(Vec<(Option<SystemTime>, u64)>);

impl Stamp {
    fn of(paths: &[&Path]) -> Self {
        Self(
            paths
                .iter()
                .map(|p| match std::fs::metadata(p) {
                    Ok(meta) => (meta.modified().ok(), meta.len()),
                    Err(_) => (None, 0),
                })
                .collect(),
        )
    }
}

enum Source {
    Auto {
        state_dir: PathBuf,
        ca: Arc<LocalCa>,
        leaf: Leaf,
    },
    Files {
        cert: PathBuf,
        key: PathBuf,
        stamp: Stamp,
        expires: Option<SystemTime>,
    },
}

/// What one [`TlsServing::refresh`] did, for the hub's log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    /// A new certificate is being served.
    pub reissued: bool,
    /// Names left out of it (outside the CA's constraints), when that
    /// changed.
    pub outside: Option<Names>,
    /// The CA on disk changed (`fw hub tls rotate`).
    pub new_ca: bool,
}

/// The hub's TLS: the server config every https connection uses and the
/// certificate behind it, kept current.
pub struct TlsServing {
    config: Arc<rustls::ServerConfig>,
    /// Bumped when the CA changes (a rotation): connections made under the
    /// old one are closed, so rotation revokes them too.
    ca_epoch: tokio::sync::watch::Sender<u64>,
    resolver: Arc<Resolver>,
    source: Mutex<Source>,
    extra: Vec<NameSpec>,
    /// Where the hub listens (its names follow from it).
    listen: SocketAddr,
}

impl fmt::Debug for TlsServing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsServing").finish_non_exhaustive()
    }
}

fn server_config(resolver: Arc<Resolver>) -> Result<Arc<rustls::ServerConfig>, String> {
    let mut config = rustls::ServerConfig::builder_with_provider(crate::net::provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // No session resumption: every connection presents the current
    // certificate, so a rotated CA is never vouched for by a session from
    // before the rotation. (A P-256 handshake costs well under a
    // millisecond.)
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Arc::new(config))
}

/// The certificate chain and key in PEM files `cert` and `key`.
fn load_files(cert: &Path, key: &Path) -> Result<(CertifiedKey, Option<SystemTime>), String> {
    use rustls::pki_types::pem::PemObject;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!("{}: no certificate in it", cert.display()));
    }
    let private =
        PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let expires = x509_parser::parse_x509_certificate(chain[0].as_ref())
        .ok()
        .map(|(_, c)| from_timestamp(c.validity().not_after.timestamp()));
    let certified = CertifiedKey::from_der(chain, private, &crate::net::provider())
        .map_err(|e| format!("{} and {}: {e}", cert.display(), key.display()))?;
    Ok((certified, expires))
}

impl TlsServing {
    /// TLS for `mode` (`None` when off). `auto` makes the state dir's CA
    /// when it has none (constrained to `names` and the extra networks)
    /// and issues a leaf for `names`.
    pub fn start(
        mode: &TlsMode,
        state_dir: &Path,
        listen: SocketAddr,
        names: &Names,
        extra: &[NameSpec],
    ) -> Result<Option<Arc<Self>>, String> {
        let (certified, source) = match mode {
            TlsMode::Off => return Ok(None),
            TlsMode::Auto => {
                let (ca, _) = LocalCa::load_or_create(state_dir, names, extra)?;
                let leaf = ca.issue(names, SystemTime::now())?;
                (
                    Arc::clone(&leaf.key),
                    Source::Auto {
                        state_dir: state_dir.to_owned(),
                        ca: Arc::new(ca),
                        leaf,
                    },
                )
            }
            TlsMode::Files { cert, key } => {
                let (certified, expires) = load_files(cert, key)?;
                (
                    Arc::new(certified),
                    Source::Files {
                        stamp: Stamp::of(&[cert, key]),
                        cert: cert.clone(),
                        key: key.clone(),
                        expires,
                    },
                )
            }
        };
        let resolver = Arc::new(Resolver {
            current: RwLock::new(certified),
        });
        Ok(Some(Arc::new(Self {
            config: server_config(Arc::clone(&resolver))?,
            ca_epoch: tokio::sync::watch::Sender::new(0),
            resolver,
            source: Mutex::new(source),
            extra: extra.to_vec(),
            listen,
        })))
    }

    /// The rustls config every https connection is accepted with.
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.config)
    }

    pub fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        tokio_rustls::TlsAcceptor::from(self.server_config())
    }

    /// The CA generation, bumped by every rotation the hub picks up.
    pub fn ca_epoch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.ca_epoch.subscribe()
    }

    /// Where the hub listens.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// The extra names it was started with.
    pub fn extra_names(&self) -> &[NameSpec] {
        &self.extra
    }

    /// The local CA (`auto` only).
    pub fn ca(&self) -> Option<Arc<LocalCa>> {
        match &*self.source.lock().unwrap_or_else(PoisonError::into_inner) {
            Source::Auto { ca, .. } => Some(Arc::clone(ca)),
            Source::Files { .. } => None,
        }
    }

    /// The certificate chain served now.
    pub fn chain(&self) -> Vec<CertificateDer<'static>> {
        self.resolver
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .cert
            .clone()
    }

    /// The names the served certificate carries, and those left out
    /// (`auto` only).
    pub fn names(&self) -> Option<(Names, Names)> {
        match &*self.source.lock().unwrap_or_else(PoisonError::into_inner) {
            Source::Auto { leaf, .. } => Some((leaf.names.clone(), leaf.outside.clone())),
            Source::Files { .. } => None,
        }
    }

    /// When the served certificate expires.
    pub fn expires(&self) -> Option<SystemTime> {
        match &*self.source.lock().unwrap_or_else(PoisonError::into_inner) {
            Source::Auto { leaf, .. } => Some(leaf.expires),
            Source::Files { expires, .. } => *expires,
        }
    }

    /// Bring the certificate up to date at `now`: `auto` re-reads the CA
    /// on disk (a rotation) and re-issues the leaf when `names` (when
    /// known) differ from the last ones asked for, the CA changed, or it
    /// expires within [`RENEW_BEFORE`]; files are re-read when they
    /// changed. `force` re-issues (re-reads) regardless. On an error the
    /// certificate in service stays.
    pub fn refresh(
        &self,
        names: Option<&Names>,
        now: SystemTime,
        force: bool,
    ) -> Result<Refreshed, String> {
        let mut source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        let mut done = Refreshed::default();
        match &mut *source {
            Source::Auto {
                state_dir,
                ca,
                leaf,
            } => {
                let on_disk = LocalCa::load(state_dir)?;
                match on_disk {
                    Some(disk) if disk.fingerprint != ca.fingerprint => {
                        *ca = Arc::new(disk);
                        done.new_ca = true;
                    }
                    Some(_) => {}
                    // Deleted under it: make one again rather than serve a
                    // CA nobody can download.
                    None => {
                        let wanted = names.unwrap_or(&leaf.requested);
                        let (fresh, _) = LocalCa::load_or_create(state_dir, wanted, &self.extra)?;
                        *ca = Arc::new(fresh);
                        done.new_ca = true;
                    }
                }
                let wanted = names.unwrap_or(&leaf.requested).clone();
                let expiring = leaf
                    .expires
                    .duration_since(now)
                    .map_or(true, |left| left < RENEW_BEFORE);
                if force
                    || done.new_ca
                    || expiring
                    || wanted != leaf.requested
                    || leaf.ca_fingerprint != ca.fingerprint
                {
                    let fresh = ca.issue(&wanted, now)?;
                    if fresh.outside != leaf.outside || done.new_ca {
                        done.outside = Some(fresh.outside.clone());
                    }
                    *self
                        .resolver
                        .current
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = Arc::clone(&fresh.key);
                    *leaf = fresh;
                    done.reissued = true;
                }
                if done.new_ca {
                    self.ca_epoch.send_modify(|epoch| *epoch += 1);
                }
            }
            Source::Files {
                cert,
                key,
                stamp,
                expires,
            } => {
                let now_stamp = Stamp::of(&[cert.as_path(), key.as_path()]);
                if force || now_stamp != *stamp {
                    let (certified, until) = load_files(cert, key)?;
                    *self
                        .resolver
                        .current
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = Arc::new(certified);
                    *stamp = now_stamp;
                    *expires = until;
                    done.reissued = true;
                }
            }
        }
        Ok(done)
    }

    /// Keep it current on the running runtime: every [`REFRESH_PERIOD`],
    /// re-discover the hub's names and
    /// [`refresh`](Self::refresh); what changed goes to stderr (the hub's
    /// log). Ends when `self` is the last reference left.
    pub fn watch(self: &Arc<Self>) {
        let listen = self.listen;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(serving) = weak.upgrade() else {
                    return;
                };
                let outcome = tokio::task::spawn_blocking(move || {
                    let names = discover_names(listen, &serving.extra);
                    serving.refresh(Some(&names), SystemTime::now(), false)
                })
                .await;
                match outcome {
                    Ok(Ok(done)) => log_refresh(&done),
                    Ok(Err(e)) => eprintln!("fw hub: tls: cannot renew the certificate: {e}"),
                    Err(e) => eprintln!("fw hub: tls: renewal failed: {e}"),
                }
            }
        });
    }
}

/// Say what a refresh changed (stderr: the hub's log).
pub fn log_refresh(done: &Refreshed) {
    if done.new_ca {
        eprintln!(
            "fw hub: tls: the local CA changed (rotated); devices must install the new one (/ca)"
        );
    }
    if let Some(outside) = &done.outside {
        if outside.is_empty() {
            if done.reissued {
                eprintln!("fw hub: tls: certificate re-issued; every name of this hub is in it");
            }
        } else {
            eprintln!("{}", outside_warning(outside));
        }
    }
}

/// The warning for names left out of the certificate.
pub fn outside_warning(outside: &Names) -> String {
    format!(
        "fw hub: tls: WARNING: {} outside this hub's CA name constraints, so not in its \
         certificate (https to {} fails): `fw hub tls rotate` makes a CA that covers it (every \
         device then re-installs the CA), or add a covering network with [server] tls_names \
         before rotating",
        outside.list().join(", "),
        if outside.list().len() == 1 {
            "it"
        } else {
            "them"
        }
    )
}

/// The warning for a CA (made by an earlier version) constrained to a
/// single-label name, which X.509 reads as that whole TLD.
pub fn too_wide_warning(wide: &[String]) -> String {
    format!(
        "fw hub: tls: WARNING: this hub's CA is name-constrained to {}, which covers every name \
         under it (a whole TLD): run `fw hub tls rotate` (every device then re-installs the CA) \
         and remove the old CA from devices",
        wide.join(", ")
    )
}

/// The status JSON (`GET /api/tls`, `fw hub tls ca` with a running hub).
pub fn summary(serving: Option<&TlsServing>) -> serde_json::Value {
    use serde_json::json;
    let Some(serving) = serving else {
        return json!({ "mode": "off" });
    };
    let expires = serving.expires().map(|t| {
        chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    });
    match (serving.ca(), serving.names()) {
        (Some(ca), Some((names, outside))) => json!({
            "mode": "auto",
            "ca_fingerprint": crate::machines::display_fingerprint(ca.fingerprint()),
            "constraints": ca.constraints().list(),
            "names": names.list(),
            "outside": outside.list(),
            "expires": expires,
        }),
        _ => json!({ "mode": "files", "expires": expires }),
    }
}

#[cfg(test)]
mod tests;
