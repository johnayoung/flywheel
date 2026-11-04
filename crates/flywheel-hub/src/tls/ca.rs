//! The local CA and the leaves it signs.

#[allow(clippy::wildcard_imports)]
use super::*;

/// The hub's local CA: its certificate, key, and constraints.
pub struct LocalCa {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    pub(super) key: rcgen::KeyPair,
    constraints: Constraints,
    pub(super) fingerprint: String,
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

pub(super) fn to_offset(t: SystemTime) -> Result<time::OffsetDateTime, String> {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970".to_owned())?
        .as_secs();
    time::OffsetDateTime::from_unix_timestamp(i64::try_from(secs).unwrap_or(i64::MAX))
        .map_err(|e| format!("bad time: {e}"))
}

pub(super) fn from_timestamp(secs: i64) -> SystemTime {
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
        extra: &ExtraNames,
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
    pub fn rotate(state_dir: &Path, names: &Names, extra: &ExtraNames) -> Result<Self, String> {
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
    #[cfg(test)]
    pub(crate) fn sign_unchecked(&self, names: &Names, now: SystemTime) -> Result<Leaf, String> {
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
pub(super) fn parsed_constraints(
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
    pub(super) key: Arc<CertifiedKey>,
    /// The names it carries.
    pub names: Names,
    /// Names asked for that the CA may not sign for (left out).
    pub outside: Names,
    /// Everything asked for (to tell whether the names changed).
    pub(super) requested: Names,
    pub expires: SystemTime,
    pub(super) ca_fingerprint: String,
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
