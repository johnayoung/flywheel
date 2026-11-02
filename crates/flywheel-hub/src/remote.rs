//! Reaching the hub from other devices: where it listens (the runtime file
//! `fw hub serve` leaves in the state dir), the URLs a phone or laptop can
//! log in at -- public URLs, `tailscale serve`, the tailnet, LAN
//! interfaces, loopback -- and the terminal QR code that carries one.

use crate::agent::which;
use crate::auth::write_private;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The runtime file inside the state dir: present while a hub serves.
pub const RUNTIME_FILE: &str = "server.json";
/// The lock file inside the state dir: a serving hub holds an exclusive
/// lock on it, so one state dir has at most one hub, and a runtime file
/// nobody holds the lock for is stale.
pub const LOCK_FILE: &str = "serve.lock";
/// How long a `tailscale` CLI call may take before it is abandoned.
pub const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);

/// What a running hub records about itself for the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    /// The bound address (`0.0.0.0` / `::` when listening everywhere).
    pub listen: SocketAddr,
    /// Configured public URLs (`--public-url`), normalized origins.
    #[serde(default)]
    pub public_urls: Vec<String>,
    /// When it started serving (`fw hub status` uptime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Where its output goes when it runs detached (`fw hub start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<PathBuf>,
}

impl RuntimeInfo {
    /// Where a client on this machine connects.
    pub fn connect_addr(&self) -> SocketAddr {
        let ip = match self.listen.ip() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        SocketAddr::new(ip, self.listen.port())
    }

    /// Record `self` (0600) in `state_dir`.
    pub fn write(&self, state_dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_private(&state_dir.join(RUNTIME_FILE), &json)
    }

    /// The runtime file in `state_dir`, if any (a malformed one is an
    /// error).
    pub fn read(state_dir: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(state_dir.join(RUNTIME_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove the runtime file if it is still `pid`'s (a newer hub on the
    /// same state dir may have replaced it).
    pub fn remove_if_owned(state_dir: &Path, pid: u32) {
        if let Ok(Some(info)) = Self::read(state_dir)
            && info.pid == pid
        {
            let _ = std::fs::remove_file(state_dir.join(RUNTIME_FILE));
        }
    }
}

/// The exclusive serve lock on a state dir, held while a hub serves it
/// (released when dropped or when the process dies, however it dies).
#[derive(Debug)]
pub struct ServeLock {
    _file: std::fs::File,
}

fn open_lock_file(state_dir: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(state_dir.join(LOCK_FILE))
}

impl ServeLock {
    /// Take the lock; `None` when another hub holds it.
    pub fn acquire(state_dir: &Path) -> std::io::Result<Option<Self>> {
        let file = open_lock_file(state_dir)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// The hub serving `state_dir`, if one does: its runtime file, trusted only
/// while the serve lock is held. A runtime file left by a hub that died
/// without cleaning up is removed (under the lock, so a hub starting
/// meanwhile cannot lose its own).
pub fn running_hub(state_dir: &Path) -> std::io::Result<Option<RuntimeInfo>> {
    if !state_dir.join(RUNTIME_FILE).exists() {
        return Ok(None);
    }
    match ServeLock::acquire(state_dir)? {
        Some(_probe) => {
            match std::fs::remove_file(state_dir.join(RUNTIME_FILE)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            Ok(None)
        }
        None => RuntimeInfo::read(state_dir),
    }
}

/// Normalize a `--public-url` to its origin: `http(s)://host[:port]`,
/// lowercase, no trailing slash. Paths, queries, and credentials are
/// refused (the hub is served from `/`).
pub fn normalize_public_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let (scheme, authority) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("{raw:?}: expected http://host[:port] or https://host[:port]"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("{raw:?}: the scheme must be http or https"));
    }
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@') || c.is_whitespace())
    {
        return Err(format!(
            "{raw:?}: give only the origin (scheme://host[:port]); the hub is served from /"
        ));
    }
    let authority = authority.to_ascii_lowercase();
    let default = if scheme == "https" { ":443" } else { ":80" };
    let authority = authority.strip_suffix(default).unwrap_or(&authority);
    Ok(format!("{scheme}://{authority}"))
}

/// What the local `tailscale` reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tailscale {
    /// This node's MagicDNS name (`box.tail1234.ts.net`), when enabled.
    pub dns_name: Option<String>,
    /// This node's tailnet addresses (100.x, fd7a:...).
    pub ips: Vec<IpAddr>,
    /// `tailscale serve` URLs whose root proxies to the hub's port.
    pub serve_urls: Vec<String>,
}

/// The `tailscale` CLI: on `PATH`, else the macOS app bundle.
pub fn tailscale_program() -> Option<PathBuf> {
    which("tailscale").or_else(|| {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        app.is_file().then(|| app.to_path_buf())
    })
}

/// Ask `program` (the `tailscale` CLI) about this node and its serve
/// config for `port`. `None` when it fails, times out, or the node is not
/// connected.
pub fn detect_tailscale(program: &Path, port: u16) -> Option<Tailscale> {
    let status = run_json(program, &["status", "--json"])?;
    let mut tailscale = parse_status(&status)?;
    if let Some(serve) = run_json(program, &["serve", "status", "--json"]) {
        tailscale.serve_urls = serve_urls_for_port(&serve, port);
    }
    Some(tailscale)
}

/// Run `program args`, returning its stdout parsed as JSON, within
/// [`TAILSCALE_TIMEOUT`].
fn run_json(program: &Path, args: &[&str]) -> Option<serde_json::Value> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(4 * 1024 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + TAILSCALE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// `tailscale status --json` -> this node, if it is connected.
pub fn parse_status(status: &serde_json::Value) -> Option<Tailscale> {
    if status.get("BackendState").and_then(|v| v.as_str()) != Some("Running") {
        return None;
    }
    let node = status.get("Self")?;
    let dns_name = node
        .get("DNSName")
        .and_then(|v| v.as_str())
        .map(|name| name.trim_end_matches('.').to_ascii_lowercase())
        .filter(|name| !name.is_empty());
    let ips = node
        .get("TailscaleIPs")
        .and_then(|v| v.as_array())
        .map(|ips| {
            ips.iter()
                .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default();
    Some(Tailscale {
        dns_name,
        ips,
        serve_urls: Vec::new(),
    })
}

/// `tailscale serve status --json` -> the URLs whose `/` handler proxies
/// to `127.0.0.1:<port>` (or `localhost`), background and foreground
/// configs alike.
pub fn serve_urls_for_port(serve: &serde_json::Value, port: u16) -> Vec<String> {
    let mut configs = vec![serve];
    if let Some(foreground) = serve.get("Foreground").and_then(|v| v.as_object()) {
        configs.extend(foreground.values());
    }
    let mut urls = Vec::new();
    for config in configs {
        let Some(web) = config.get("Web").and_then(|v| v.as_object()) else {
            continue;
        };
        for (host_port, server) in web {
            let proxied = server
                .get("Handlers")
                .and_then(|h| h.get("/"))
                .and_then(|h| h.get("Proxy"))
                .and_then(|p| p.as_str())
                .is_some_and(|target| proxies_to_port(target, port));
            if !proxied {
                continue;
            }
            let Some((host, listen_port)) = host_port.rsplit_once(':') else {
                continue;
            };
            // A port listed under TCP with HTTP (not HTTPS) serves plain
            // HTTP (`tailscale serve --http`); everything else is HTTPS.
            let tcp = config.get("TCP").and_then(|t| t.get(listen_port));
            let http = tcp.and_then(|t| t.get("HTTP")).and_then(|v| v.as_bool()) == Some(true)
                && tcp.and_then(|t| t.get("HTTPS")).and_then(|v| v.as_bool()) != Some(true);
            let scheme = if http { "http" } else { "https" };
            if let Ok(url) = normalize_public_url(&format!("{scheme}://{host}:{listen_port}"))
                && !urls.contains(&url)
            {
                urls.push(url);
            }
        }
    }
    urls
}

fn proxies_to_port(target: &str, port: u16) -> bool {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https+insecure://"))
        .unwrap_or(target);
    let authority = rest.split('/').next().unwrap_or_default();
    let Some((host, target_port)) = authority.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]") && target_port.parse() == Ok(port)
}

/// Up IPv4 addresses of this machine's non-loopback interfaces, with the
/// interface name, skipping container/VM bridges (`docker0`, `br-*`,
/// `veth*`, `virbr*`, `cni*`).
pub fn interface_addresses() -> Vec<(String, IpAddr)> {
    let skip = |name: &str| {
        ["docker", "br-", "veth", "virbr", "cni", "flannel"]
            .iter()
            .any(|p| name.starts_with(p))
    };
    raw_interface_addresses()
        .into_iter()
        .filter(|(name, ip)| {
            !skip(name)
                && match ip {
                    IpAddr::V4(v4) => {
                        !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified()
                    }
                    IpAddr::V6(_) => false,
                }
        })
        .collect()
}

#[cfg(unix)]
fn raw_interface_addresses() -> Vec<(String, IpAddr)> {
    let mut found = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list freed below; every
    // pointer read is checked for null and only read while the list lives.
    unsafe {
        if libc::getifaddrs(&raw mut head) != 0 {
            return found;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
                continue;
            }
            let flags = entry.ifa_flags;
            let up = libc::IFF_UP as libc::c_uint;
            let loopback = libc::IFF_LOOPBACK as libc::c_uint;
            if flags & up == 0 || flags & loopback != 0 {
                continue;
            }
            if libc::c_int::from((*entry.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let sin = &*entry.ifa_addr.cast::<libc::sockaddr_in>();
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let name = std::ffi::CStr::from_ptr(entry.ifa_name)
                .to_string_lossy()
                .into_owned();
            found.push((name, IpAddr::V4(ip)));
        }
        libc::freeifaddrs(head);
    }
    found
}

#[cfg(not(unix))]
fn raw_interface_addresses() -> Vec<(String, IpAddr)> {
    Vec::new()
}

/// A URL to log in at, and where it reaches from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginTarget {
    /// `public`, `tailnet https`, `tailnet`, `lan (eth0)`, `this machine`.
    pub label: String,
    /// The origin (`scheme://host[:port]`).
    pub base: String,
    /// Reachable from another device (QR-worthy).
    pub remote: bool,
}

fn http_base(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

/// Every URL the hub listening on `listen` can be logged into at, most
/// remote-friendly first: public URLs, `tailscale serve` URLs, tailnet
/// addresses and MagicDNS name, LAN addresses, then loopback.
pub fn login_targets(
    listen: SocketAddr,
    public_urls: &[String],
    tailscale: Option<&Tailscale>,
    interfaces: &[(String, IpAddr)],
) -> Vec<LoginTarget> {
    let mut targets: Vec<LoginTarget> = Vec::new();
    let mut push = |label: String, base: String, remote: bool| {
        if !targets.iter().any(|t| t.base == base) {
            targets.push(LoginTarget {
                label,
                base,
                remote,
            });
        }
    };
    for url in public_urls {
        push("public".into(), url.clone(), true);
    }
    if let Some(ts) = tailscale {
        for url in &ts.serve_urls {
            push("tailnet https (tailscale serve)".into(), url.clone(), true);
        }
    }
    let port = listen.port();
    let ip = listen.ip();
    if !ip.is_loopback() {
        let tailnet_ips: Vec<IpAddr> = tailscale.map(|t| t.ips.clone()).unwrap_or_default();
        let mut candidates: Vec<(String, IpAddr)> = Vec::new();
        if ip.is_unspecified() {
            candidates.extend(interfaces.iter().cloned());
            // A tailnet address no interface carries is not reachable here
            // (userspace-networking tailscale): only listed when the
            // interfaces could not be enumerated at all.
            if interfaces.is_empty() {
                candidates.extend(
                    tailnet_ips
                        .iter()
                        .filter(|t| t.is_ipv4())
                        .map(|t| ("tailscale".to_owned(), *t)),
                );
            }
        } else {
            let name = interfaces
                .iter()
                .find(|(_, a)| *a == ip)
                .map_or_else(|| "bound address".to_owned(), |(n, _)| n.clone());
            candidates.push((name, ip));
        }
        let (tailnet, lan): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|(_, a)| tailnet_ips.contains(a));
        for (_, address) in &tailnet {
            if let Some(name) = tailscale.and_then(|t| t.dns_name.as_deref()) {
                push(
                    "tailnet (MagicDNS)".into(),
                    format!("http://{name}:{port}"),
                    true,
                );
            }
            push("tailnet".into(), http_base(*address, port), true);
        }
        for (name, address) in lan {
            push(format!("lan ({name})"), http_base(address, port), true);
        }
    }
    if ip.is_loopback() || ip.is_unspecified() {
        let local = if ip.is_ipv6() && !ip.is_unspecified() {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };
        push("this machine".into(), http_base(local, port), false);
    }
    targets
}

/// `text` as a QR code drawn with Unicode half blocks, two modules per
/// character cell, with explicit black-on-white colours so it scans on dark
/// and light terminals alike. One string per terminal line.
pub fn qr_lines(text: &str) -> Result<Vec<String>, String> {
    use qrcode::{Color, QrCode};
    const QUIET: usize = 4;
    let code = QrCode::new(text.as_bytes()).map_err(|e| format!("QR encoding failed: {e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET;
    let dark = |x: usize, y: usize| {
        x >= QUIET
            && y >= QUIET
            && x < width + QUIET
            && y < width + QUIET
            && colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
    };
    let mut lines = Vec::with_capacity(size.div_ceil(2));
    for y in (0..size).step_by(2) {
        let mut line = String::from("\x1b[38;5;16;48;5;231m");
        for x in 0..size {
            line.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '\u{2588}',
                (true, false) => '\u{2580}',
                (false, true) => '\u{2584}',
                (false, false) => ' ',
            });
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_file_round_trips_and_is_removed_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(RuntimeInfo::read(dir.path()).unwrap(), None);
        let info = RuntimeInfo {
            pid: 42,
            listen: "0.0.0.0:7420".parse().unwrap(),
            public_urls: vec!["https://box.ts.net".into()],
            started_at: None,
            log_file: None,
        };
        info.write(dir.path()).unwrap();
        assert_eq!(RuntimeInfo::read(dir.path()).unwrap(), Some(info.clone()));
        assert_eq!(info.connect_addr(), "127.0.0.1:7420".parse().unwrap());
        RuntimeInfo::remove_if_owned(dir.path(), 43);
        assert!(dir.path().join(RUNTIME_FILE).exists());
        RuntimeInfo::remove_if_owned(dir.path(), 42);
        assert!(!dir.path().join(RUNTIME_FILE).exists());
        std::fs::write(dir.path().join(RUNTIME_FILE), "{").unwrap();
        assert!(RuntimeInfo::read(dir.path()).is_err());
        let v6 = RuntimeInfo {
            listen: "[::]:9".parse().unwrap(),
            ..info
        };
        assert_eq!(v6.connect_addr(), "[::1]:9".parse().unwrap());
    }

    #[test]
    fn serve_lock_is_exclusive_and_gates_the_runtime_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(running_hub(dir.path()).unwrap(), None);
        let info = RuntimeInfo {
            pid: std::process::id(),
            listen: "127.0.0.1:7420".parse().unwrap(),
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
        };
        // A runtime file nobody holds the lock for is stale: ignored and
        // removed (a crashed hub, even if its pid was reused).
        info.write(dir.path()).unwrap();
        assert_eq!(running_hub(dir.path()).unwrap(), None);
        assert!(!dir.path().join(RUNTIME_FILE).exists());
        let lock = ServeLock::acquire(dir.path()).unwrap().expect("free");
        assert!(ServeLock::acquire(dir.path()).unwrap().is_none(), "held");
        info.write(dir.path()).unwrap();
        assert_eq!(running_hub(dir.path()).unwrap(), Some(info.clone()));
        drop(lock);
        assert!(
            ServeLock::acquire(dir.path()).unwrap().is_some(),
            "released"
        );
        assert_eq!(running_hub(dir.path()).unwrap(), None);
    }

    #[test]
    fn public_urls_normalize_to_origins() {
        assert_eq!(
            normalize_public_url("HTTPS://Box.Tail1.ts.net/").unwrap(),
            "https://box.tail1.ts.net"
        );
        assert_eq!(
            normalize_public_url("https://box.ts.net:443").unwrap(),
            "https://box.ts.net"
        );
        assert_eq!(
            normalize_public_url("http://10.0.0.2:7420").unwrap(),
            "http://10.0.0.2:7420"
        );
        for bad in [
            "box.ts.net",
            "ftp://box",
            "https://",
            "https://box/hub",
            "https://u@box",
            "https://box?x",
        ] {
            assert!(normalize_public_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_tailscale_status_and_serve_config() {
        let status = json!({
            "BackendState": "Running",
            "Self": {"DNSName": "Box.tail1234.ts.net.", "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]}
        });
        let ts = parse_status(&status).unwrap();
        assert_eq!(ts.dns_name.as_deref(), Some("box.tail1234.ts.net"));
        assert_eq!(ts.ips.len(), 2);
        assert!(parse_status(&json!({"BackendState": "Stopped", "Self": {}})).is_none());

        let serve = json!({
            "TCP": {"443": {"HTTPS": true}, "8080": {"HTTP": true}},
            "Web": {
                "box.tail1234.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:7420"}}},
                "box.tail1234.ts.net:8080": {"Handlers": {"/": {"Proxy": "http://localhost:7420/"}}},
                "box.tail1234.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:9999"}}},
                "box.tail1234.ts.net:9443": {"Handlers": {"/hub": {"Proxy": "http://127.0.0.1:7420"}}}
            },
            "Foreground": {"s1": {
                "TCP": {"10000": {"HTTPS": true}},
                "Web": {"box.tail1234.ts.net:10000": {"Handlers": {"/": {"Proxy": "127.0.0.1:7420"}}}}
            }}
        });
        assert_eq!(
            serve_urls_for_port(&serve, 7420),
            vec![
                "https://box.tail1234.ts.net",
                "http://box.tail1234.ts.net:8080",
                "https://box.tail1234.ts.net:10000",
            ]
        );
        assert!(serve_urls_for_port(&json!({}), 7420).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detects_tailscale_through_its_cli() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("tailscale");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
case "$1 $2" in
  "status --json") echo '{"BackendState":"Running","Self":{"DNSName":"box.t.ts.net.","TailscaleIPs":["100.64.0.9"]}}' ;;
  "serve status") echo '{"Web":{"box.t.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:7420"}}}}}' ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ts = detect_tailscale(&fake, 7420).unwrap();
        assert_eq!(ts.dns_name.as_deref(), Some("box.t.ts.net"));
        assert_eq!(ts.serve_urls, vec!["https://box.t.ts.net"]);
        assert!(detect_tailscale(&fake, 1).unwrap().serve_urls.is_empty());

        let hang = dir.path().join("hang");
        std::fs::write(&hang, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert!(detect_tailscale(&hang, 7420).is_none());
        assert!(started.elapsed() < TAILSCALE_TIMEOUT + Duration::from_secs(2));
        assert!(detect_tailscale(&dir.path().join("missing"), 7420).is_none());
    }

    #[test]
    fn interfaces_exclude_loopback_and_bridges() {
        for (name, ip) in interface_addresses() {
            assert!(!ip.is_loopback() && ip.is_ipv4(), "{name} {ip}");
            assert!(!name.starts_with("docker"));
        }
    }

    #[test]
    fn login_targets_order_remote_first() {
        let ts = Tailscale {
            dns_name: Some("box.t.ts.net".into()),
            ips: vec!["100.64.0.9".parse().unwrap()],
            serve_urls: vec!["https://box.t.ts.net".into()],
        };
        let interfaces = vec![
            ("eth0".to_owned(), "192.168.1.5".parse().unwrap()),
            ("tailscale0".to_owned(), "100.64.0.9".parse().unwrap()),
        ];
        let everywhere = login_targets(
            "0.0.0.0:7420".parse().unwrap(),
            &["https://hub.example.com".into()],
            Some(&ts),
            &interfaces,
        );
        let bases: Vec<&str> = everywhere.iter().map(|t| t.base.as_str()).collect();
        assert_eq!(
            bases,
            vec![
                "https://hub.example.com",
                "https://box.t.ts.net",
                "http://box.t.ts.net:7420",
                "http://100.64.0.9:7420",
                "http://192.168.1.5:7420",
                "http://127.0.0.1:7420",
            ]
        );
        assert_eq!(everywhere[4].label, "lan (eth0)");
        assert!(!everywhere[5].remote);

        // Tailscale reports 100.64.0.9 but no interface has it (userspace
        // networking): not reachable on the bind, not listed.
        let lan_only = login_targets(
            "0.0.0.0:7420".parse().unwrap(),
            &[],
            Some(&ts),
            &interfaces[..1],
        );
        assert!(
            lan_only.iter().all(|t| !t.base.contains("100.64.0.9")),
            "{lan_only:?}"
        );
        let unknown = login_targets("0.0.0.0:7420".parse().unwrap(), &[], Some(&ts), &[]);
        assert!(unknown.iter().any(|t| t.base == "http://100.64.0.9:7420"));

        let loopback = login_targets("127.0.0.1:7420".parse().unwrap(), &[], None, &interfaces);
        assert_eq!(loopback.len(), 1);
        assert!(!loopback[0].remote);

        let one = login_targets("192.168.1.5:7420".parse().unwrap(), &[], None, &interfaces);
        assert_eq!(one.len(), 1);
        assert_eq!(
            (one[0].label.as_str(), one[0].base.as_str()),
            ("lan (eth0)", "http://192.168.1.5:7420")
        );
        let tailnet_only = login_targets(
            "100.64.0.9:7420".parse().unwrap(),
            &[],
            Some(&ts),
            &interfaces,
        );
        let labels: Vec<&str> = tailnet_only.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "tailnet https (tailscale serve)",
                "tailnet (MagicDNS)",
                "tailnet"
            ]
        );
    }

    #[test]
    fn qr_code_is_square_half_blocks() {
        let lines = qr_lines("http://192.168.1.5:7420/login?code=0123456789abcdefghjk").unwrap();
        let strip = |l: &str| {
            l.trim_start_matches("\x1b[38;5;16;48;5;231m")
                .trim_end_matches("\x1b[0m")
                .chars()
                .count()
        };
        let width = strip(&lines[0]);
        assert!(lines.iter().all(|l| strip(l) == width));
        assert_eq!(lines.len(), width.div_ceil(2));
        // Scan it: rebuild the module grid from the half blocks, draw it
        // at 4 px per module, and decode.
        let text = "http://192.168.1.5:7420/login?code=0123456789abcdefghjk";
        let mut modules = vec![vec![false; width]; lines.len() * 2];
        for (row, line) in lines.iter().enumerate() {
            let line = line
                .trim_start_matches("\x1b[38;5;16;48;5;231m")
                .trim_end_matches("\x1b[0m");
            for (x, c) in line.chars().enumerate() {
                let (top, bottom) = match c {
                    '\u{2588}' => (true, true),
                    '\u{2580}' => (true, false),
                    '\u{2584}' => (false, true),
                    _ => (false, false),
                };
                modules[row * 2][x] = top;
                modules[row * 2 + 1][x] = bottom;
            }
        }
        const PX: usize = 4;
        let mut image =
            rqrr::PreparedImage::prepare_from_greyscale(width * PX, modules.len() * PX, |x, y| {
                if modules[y / PX][x / PX] { 0 } else { 255 }
            });
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, decoded) = grids[0].decode().unwrap();
        assert_eq!(decoded, text);
    }
}
