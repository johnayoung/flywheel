//! Names a leaf is issued for and the constraints a local CA carries.

#[allow(clippy::wildcard_imports)]
use super::*;

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

/// The DNS suffixes `tls_names` takes without an opt-in: names under them
/// are private (mDNS, home and internal networks, loopback, a tailnet's
/// MagicDNS), so a CA constrained to one cannot vouch for a public site.
pub const PRIVATE_DNS_SUFFIXES: &[&str] = &[
    "local",
    "lan",
    "home.arpa",
    "internal",
    "localhost",
    "ts.net",
];

/// The networks `tls_names` takes without an opt-in: RFC 1918, loopback,
/// link-local, CGNAT (100.64/10, Tailscale), and IPv6 loopback, link-local
/// and ULA (fc00::/7, Tailscale's included).
const PRIVATE_NETS: &[(IpAddr, u8)] = &[
    (IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8),
    (IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)), 12),
    (IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16),
    (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8),
    (IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0)), 16),
    (IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), 10),
    (IpAddr::V6(Ipv6Addr::LOCALHOST), 128),
    (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)), 10),
    (IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0)), 7),
];

/// The private-namespace rule, as error messages name it.
pub const PRIVATE_RULE: &str = "names under .local, .lan, .home.arpa, .internal, .localhost or \
     <tailnet>.ts.net, and private (10/8, 172.16/12, 192.168/16), loopback, link-local, CGNAT \
     (100.64/10) or ULA (fc00::/7) addresses and networks";

/// Whether a DNS name is in a private namespace: `localhost`, or at least
/// one label under a [`PRIVATE_DNS_SUFFIXES`] entry (never `ts.net`
/// itself: that would cover every tailnet).
pub fn private_dns(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || PRIVATE_DNS_SUFFIXES.iter().any(|suffix| {
            name.strip_suffix(suffix)
                .and_then(|rest| rest.strip_suffix('.'))
                .is_some_and(|rest| !rest.is_empty())
        })
}

/// Whether the network `ip/prefix` lies wholly inside a private range.
pub fn private_net(ip: IpAddr, prefix: u8) -> bool {
    let ip = ip.to_canonical();
    // An IPv4-mapped address stands for its IPv4 one (prefix shifted).
    let prefix = if ip.is_ipv4() && prefix > 32 {
        prefix.saturating_sub(96)
    } else {
        prefix
    };
    PRIVATE_NETS.iter().any(|(net, wider)| {
        net.is_ipv4() == ip.is_ipv4() && prefix >= *wider && mask(ip, *wider) == *net
    })
}

/// Whether `spec` is in a private namespace (see [`PRIVATE_RULE`]).
pub fn is_private(spec: &NameSpec) -> bool {
    match spec {
        NameSpec::Dns(name) => private_dns(name),
        NameSpec::Ip(ip) => private_net(*ip, if ip.is_ipv4() { 32 } else { 128 }),
        NameSpec::Net(ip, prefix) => private_net(*ip, *prefix),
    }
}

/// The extra names the local CA and its leaf are for (`--tls-name`,
/// `[server] tls_names`), checked: each is private (see [`PRIVATE_RULE`])
/// or opted in by name (`--tls-allow-public-name`, `[server]
/// tls_public_names`). The only way extra names reach a CA.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraNames {
    pub(super) names: Vec<NameSpec>,
    pub(super) public: Vec<NameSpec>,
}

impl ExtraNames {
    /// `names`, refusing a public one not in `allow_public`, and an
    /// opt-in for a name that is not in `names`. One message per problem.
    pub fn new(names: Vec<NameSpec>, allow_public: Vec<NameSpec>) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        for spec in &names {
            if !is_private(spec) && !allow_public.contains(spec) {
                let name = spec.to_string();
                errors.push(format!(
                    "{name:?} is a public name: the hub's CA (its key on this machine) could then \
                     mint certificates for it that every device with the CA installed trusts. \
                     Without an opt-in only private names are taken: {PRIVATE_RULE}. If {name} \
                     is yours and you mean it, also pass --tls-allow-public-name {name} (hub.toml \
                     [server] tls_public_names)"
                ));
            }
        }
        for spec in &allow_public {
            if !names.contains(spec) {
                errors.push(format!(
                    "{spec} is opted in as public (--tls-allow-public-name, tls_public_names) but \
                     is not a --tls-name (tls_names) entry: the opt-in allows a name listed \
                     there, it does not add one"
                ));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let public = names.iter().filter(|s| !is_private(s)).cloned().collect();
        Ok(Self { names, public })
    }

    /// Every extra name.
    pub fn names(&self) -> &[NameSpec] {
        &self.names
    }

    /// The extra names a CA or leaf may take: each private or opted in.
    /// [`ExtraNames::new`] refuses any other, so this is all of them; the
    /// filter holds the line should one ever get in some other way.
    fn permitted(&self) -> impl Iterator<Item = &NameSpec> {
        self.names
            .iter()
            .filter(|spec| is_private(spec) || self.public.contains(spec))
    }

    /// The public ones among them (each opted in).
    pub fn public(&self) -> &[NameSpec] {
        &self.public
    }
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
    extra: &ExtraNames,
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
    for spec in extra.permitted() {
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
pub fn discover_names(listen: SocketAddr, extra: &ExtraNames) -> Names {
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
    pub fn for_names(names: &Names, extra: &ExtraNames) -> Self {
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
        for spec in extra.permitted() {
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

    /// The constraints outside the private namespaces (see
    /// [`PRIVATE_RULE`]) other than this machine's own names and addresses
    /// (its host name, a public address of its own: those in `names` that
    /// are not `extra` names): what a device installing the CA would trust
    /// it for beyond this network. Each as the [`NameSpec`] that names it
    /// (`ip/32` reads as the address).
    pub fn public(&self, names: &Names, extra: &ExtraNames) -> Vec<NameSpec> {
        let mut own = names.clone();
        for spec in extra.names() {
            match spec {
                NameSpec::Dns(name) => {
                    own.dns.remove(name);
                }
                NameSpec::Ip(ip) => {
                    own.ips.remove(ip);
                }
                NameSpec::Net(..) => {}
            }
        }
        let dns = self
            .dns
            .iter()
            .filter(|name| !private_dns(name) && !own.dns.contains(*name))
            .map(|name| NameSpec::Dns(name.clone()));
        let nets = self.nets.iter().filter_map(|(ip, prefix)| {
            let full = if ip.is_ipv4() { 32 } else { 128 };
            let spec = if *prefix == full {
                NameSpec::Ip(*ip)
            } else {
                NameSpec::Net(*ip, *prefix)
            };
            let own_ip = *prefix == full && own.ips.contains(ip);
            (!private_net(*ip, *prefix) && !own_ip).then_some(spec)
        });
        dns.chain(nets).collect()
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

    pub(super) fn to_rcgen(&self) -> rcgen::NameConstraints {
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
pub(super) fn prefix_of(mask: &[u8]) -> Option<u8> {
    let ones: u32 = mask.iter().map(|b| b.count_ones()).sum();
    let contiguous = mask
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1))
        .skip_while(|bit| *bit)
        .all(|bit| !bit);
    contiguous.then(|| u8::try_from(ones).ok()).flatten()
}
