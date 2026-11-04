//! Serving: the certificate resolver and its refresh.

#[allow(clippy::wildcard_imports)]
use super::*;

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
    extra: ExtraNames,
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
        extra: &ExtraNames,
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
            extra: extra.clone(),
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
    pub fn extra_names(&self) -> &ExtraNames {
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

/// What the CA's public constraints (see [`Constraints::public`]) mean, one
/// line each for the opted-in ones and the others (empty when none).
pub fn public_warnings(public: &[NameSpec], extra: &ExtraNames) -> Vec<String> {
    let list = |specs: Vec<&NameSpec>| {
        specs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (allowed, other): (Vec<&NameSpec>, Vec<&NameSpec>) =
        public.iter().partition(|s| extra.public().contains(s));
    let mut lines = Vec::new();
    if !allowed.is_empty() {
        lines.push(format!(
            "WARNING: this CA may also sign for the public {} (allowed with \
             --tls-allow-public-name): every device that installs it trusts this machine's CA key \
             for them and every name under them; drop the opt-in and `fw hub tls rotate` unless \
             you own them and mean it",
            list(allowed)
        ));
    }
    if !other.is_empty() {
        lines.push(format!(
            "WARNING: this CA may also sign for the public {}, which the current settings do \
             not opt in to (--tls-allow-public-name): a CA from before the opt-in was needed or \
             was dropped; run `fw hub tls rotate` and remove the old CA from every device",
            list(other)
        ));
    }
    lines
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
            "public_names": serving
                .extra_names()
                .public()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "expires": expires,
        }),
        _ => json!({ "mode": "files", "expires": expires }),
    }
}
