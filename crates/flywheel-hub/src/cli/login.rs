//! `fw hub token` / `url` and what `start` and `serve` print: login
//! URLs, QR codes, TLS and tailscale hints, the addresses a hub is reached at.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn token(args: &TokenArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = resolve_state_dir(&args.state).and_then(|dir| {
        crate::paths::ensure_private_dir(&dir)
            .map_err(|e| format!("state dir {}: {e}", dir.display()))?;
        if args.rotate {
            rotate(&dir).map(|message| {
                let _ = writeln!(err, "fw hub: {message}");
            })
        } else {
            let token = Token::load_or_create(&dir).map_err(|e| e.to_string())?;
            let _ = writeln!(out, "{}", token.expose());
            Ok(())
        }
    });
    match result {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

/// The hub serving `state_dir`, if one holds its serve lock.
pub(crate) fn running_hub(state_dir: &Path) -> Result<Option<RuntimeInfo>, String> {
    crate::remote::running_hub(state_dir).map_err(|e| {
        format!(
            "runtime file {}: {e}",
            state_dir.join(crate::remote::RUNTIME_FILE).display()
        )
    })
}

/// Rotate through the running hub (so it switches without a restart), or
/// in the token file when none runs.
pub(super) fn rotate(state_dir: &Path) -> Result<String, String> {
    if let Some(hub) = Endpoint::running(state_dir)? {
        hub.with_timeout(CLIENT_TIMEOUT)
            .blocking_call("POST", "/api/token/rotate", None)?;
        return Ok(
            "token rotated; the running hub signed out every browser and closed every terminal connection (log in again via `fw hub url`)"
                .to_owned(),
        );
    }
    Token::rotate(state_dir).map_err(|e| e.to_string())?;
    Ok("token rotated (no hub is running; the next `fw hub serve` uses it)".to_owned())
}

pub(super) fn url_cmd(args: &UrlArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if let Some(machine) = &args.endpoint.machine {
        // This hub holds only a peer credential there, which never mints
        // login codes.
        let _ = writeln!(
            err,
            "fw hub: url has no --machine: a login code for {machine} comes from that machine \
             (`fw hub url` there); in this hub's browser UI its sessions are already here"
        );
        return 2;
    }
    let minted = Endpoint::resolve(&args.endpoint).and_then(|hub| {
        let hub = hub.with_timeout(CLIENT_TIMEOUT);
        let code = mint_login_code(&hub)?;
        Ok((hub, code))
    });
    let (hub, code) = match minted {
        Ok(minted) => minted,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    let qr = if args.qr { Qr::Draw } else { Qr::No };
    match hub.local_info() {
        Some(info) => show_login(info, &code, qr, out, err),
        // A hub elsewhere: its own URL is the one to log in at.
        None => {
            let target = LoginTarget {
                label: "url".to_owned(),
                base: hub.base().to_owned(),
                remote: true,
                ca_page: false,
            };
            print_login(out, err, &[target], &code, qr);
        }
    }
    0
}

/// Print the login URLs for `code` on the hub `info` describes (`qr`:
/// [`Qr::Auto`] draws one only when it can be scanned from elsewhere and
/// stdout is a terminal).
pub(crate) fn show_login(
    info: &RuntimeInfo,
    code: &str,
    qr: Qr,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let (targets, tailscale) = hub_targets(info);
    let qr = match qr {
        Qr::Auto => auto_qr(&targets),
        qr => qr,
    };
    print_login(out, err, &targets, code, qr);
    print_tailscale_hint(err, tailscale.as_ref(), &targets, info.listen.port());
}

/// Every origin the hub `info` describes is reached at, most remote-friendly
/// first, and the tailnet it found.
pub(crate) fn hub_targets(info: &RuntimeInfo) -> (Vec<LoginTarget>, Option<Tailscale>) {
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, info.listen.port()));
    let targets = login_targets_served(
        info.listen,
        &info.public_urls,
        tailscale.as_ref(),
        &local_interfaces(info.listen),
        info.served(),
    );
    (targets, tailscale)
}

/// Draw a QR code when a remote device could use it and stdout can show it.
pub(super) fn auto_qr(targets: &[LoginTarget]) -> Qr {
    if !targets.iter().any(|t| t.remote) {
        Qr::No
    } else if std::io::stdout().is_terminal() {
        Qr::Draw
    } else {
        Qr::NotATerminal
    }
}

/// Ask the hub serving `dir` for a one-time login code.
pub(crate) fn mint_remote_code(dir: &Path) -> Result<(RuntimeInfo, String), String> {
    let hub = Endpoint::local(dir)?.with_timeout(CLIENT_TIMEOUT);
    let code = mint_login_code(&hub)?;
    let info = hub
        .local_info()
        .cloned()
        .ok_or_else(|| "the hub's runtime file vanished".to_owned())?;
    Ok((info, code))
}

/// A one-time login code from `hub`.
pub(crate) fn mint_login_code(hub: &Endpoint) -> Result<String, String> {
    let body = hub.blocking_call("POST", "/api/login-codes", None)?;
    body.get("code")
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "the hub sent no login code".to_owned())
}

/// Whether [`print_login`] draws the QR code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Qr {
    No,
    Draw,
    /// [`Qr::Draw`] when a remote URL exists and stdout is a terminal.
    Auto,
    /// Wanted, but the output is not a terminal (a log or journal): say
    /// how to get one instead of writing colour escapes into it.
    NotATerminal,
}

/// The login URLs for `code`, then (per `qr`) the first remote one as a
/// QR code.
pub(super) fn print_login(
    out: &mut dyn Write,
    err: &mut dyn Write,
    targets: &[LoginTarget],
    code: &str,
    qr: Qr,
) {
    let minutes = LOGIN_CODE_TTL.as_secs() / 60;
    let _ = writeln!(
        out,
        "log in with a one-time code (single use, valid {minutes} minutes; `fw hub url` mints another):"
    );
    let width = targets.iter().map(|t| t.label.len()).max().unwrap_or(0);
    for target in targets {
        let _ = writeln!(
            out,
            "  {:width$}  {}",
            target.label,
            code_login_url(&target.base, code)
        );
    }
    if qr == Qr::No {
        return;
    }
    let Some(remote) = targets.iter().find(|t| t.remote) else {
        let _ = writeln!(
            err,
            "fw hub: no QR code: the hub listens on loopback only, which a phone cannot reach (use `tailscale serve`, --public-url, or `fw hub serve --bind`)"
        );
        return;
    };
    if qr == Qr::NotATerminal {
        let _ = writeln!(
            out,
            "(QR code not drawn: output is not a terminal; `fw hub url --qr` draws one)"
        );
        return;
    }
    // The hub's own TLS with a local CA: a phone that does not trust the CA
    // yet installs it from the CA page, which is plain http (it cannot be
    // https before the trust exists) -- so the login code never goes there:
    // an active attacker on the LAN could rewrite that page and spend the
    // code first. The CA page gets a QR of its own, without the code; the
    // login QR is https only.
    for (url, title) in login_qrs(remote, code) {
        draw_qr(out, err, &url, &title);
    }
}

/// The QR codes (URL, title) that log a phone in at `remote` with `code`.
/// With the hub's local CA, a CA-page QR without the code comes first.
pub(super) fn login_qrs(remote: &LoginTarget, code: &str) -> Vec<(String, String)> {
    let mut qrs = Vec::new();
    if let Some(ca) = remote.ca_base() {
        qrs.push((
            format!("{ca}/ca"),
            format!(
                "first time on this device? scan to install this hub's CA ({}, plain http: \
                 compare the fingerprint with `fw hub tls ca`; no login code in it)",
                remote.label
            ),
        ));
    }
    qrs.push((
        code_login_url(&remote.base, code),
        format!("scan to log in ({})", remote.label),
    ));
    qrs
}

/// `url` as a QR code under `title`.
pub(super) fn draw_qr(out: &mut dyn Write, err: &mut dyn Write, url: &str, title: &str) {
    match qr_lines(url) {
        Ok(lines) => {
            let _ = writeln!(out, "\n{title}:");
            for line in lines {
                let _ = writeln!(out, "  {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: {e}");
        }
    }
}

/// What built-in TLS serves: the CA's fingerprint and where a device gets
/// it, or the operator's files; names left out of the certificate.
pub(super) fn print_tls(
    out: &mut dyn Write,
    err: &mut dyn Write,
    tls: &crate::tls::TlsServing,
    local: SocketAddr,
) {
    let Some(ca) = tls.ca() else {
        let _ = writeln!(
            out,
            "https: on this port with your certificate (plain http from the network redirects to https)"
        );
        return;
    };
    let _ = writeln!(
        out,
        "https: on this port, local CA SHA-256 {} (name-constrained to this hub)",
        crate::machines::display_fingerprint(ca.fingerprint())
    );
    let urls = crate::tls::ca_page_urls(local);
    if urls.is_empty() {
        let _ = writeln!(
            out,
            "  listening on loopback only: bind a LAN address (--bind 0.0.0.0) for devices to reach /ca"
        );
    } else {
        let _ = writeln!(
            out,
            "  devices install it once from {} (`fw hub url --qr` leads a phone there)",
            urls.join(" or ")
        );
    }
    if let Some((_, outside)) = tls.names()
        && !outside.is_empty()
    {
        let _ = writeln!(err, "{}", crate::tls::outside_warning(&outside));
    }
    let wide = ca.constraints().too_wide();
    if !wide.is_empty() {
        let _ = writeln!(err, "{}", crate::tls::too_wide_warning(&wide));
    }
}

/// Point at `tailscale serve` when the tailnet is up but nothing serves the
/// hub over HTTPS on it.
pub(super) fn print_tailscale_hint(
    err: &mut dyn Write,
    tailscale: Option<&Tailscale>,
    targets: &[LoginTarget],
    port: u16,
) {
    let Some(ts) = tailscale else {
        return;
    };
    if !ts.serve_urls.is_empty() || targets.iter().any(|t| t.base.starts_with("https://")) {
        return;
    }
    let at = ts
        .dns_name
        .as_deref()
        .map(|name| format!(" at https://{name}"))
        .unwrap_or_default();
    let _ = writeln!(
        err,
        "fw hub: tailscale is up: `tailscale serve --bg http://127.0.0.1:{port}` serves this hub over HTTPS on your tailnet{at} (recommended for phones)"
    );
}

/// The host a browser should use to reach `addr`.
pub(crate) fn browse_host(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        addr.ip()
    };
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{}", addr.port()),
        IpAddr::V6(v6) => format!("[{v6}]:{}", addr.port()),
    }
}

/// This machine's interface addresses; where they cannot be listed, the
/// default-route address.
pub(super) fn local_interfaces(listen: SocketAddr) -> Vec<(String, IpAddr)> {
    let found = interface_addresses();
    if !found.is_empty() || !listen.ip().is_unspecified() {
        return found;
    }
    route_address(listen.ip())
        .map(|ip| vec![("default route".to_owned(), ip)])
        .unwrap_or_default()
}

/// This machine's address on its default route, for a URL another device
/// can open when listening on an unspecified address. A UDP `connect` only
/// selects a route; nothing is sent.
pub(super) fn route_address(unspecified: IpAddr) -> Option<IpAddr> {
    let (bind, probe): (SocketAddr, SocketAddr) = match unspecified {
        IpAddr::V4(_) => (
            (Ipv4Addr::UNSPECIFIED, 0).into(),
            (Ipv4Addr::new(192, 0, 2, 1), 9).into(),
        ),
        IpAddr::V6(_) => (
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
            (std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 9).into(),
        ),
    };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect(probe).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}
