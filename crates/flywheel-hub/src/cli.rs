//! The `fw hub <verb>` entry: `serve`, `url`, and `token`.

use crate::auth::{LOGIN_CODE_TTL, Token};
use crate::client;
use crate::hub::{Hub, HubOptions};
use crate::paths::default_state_dir;
use crate::remote::{
    LoginTarget, RuntimeInfo, Tailscale, detect_tailscale, interface_addresses, login_targets,
    normalize_public_url, process_alive, qr_lines, tailscale_program,
};
use crate::server::{ServerOptions, code_login_url, serve};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the CLI waits for the running hub to answer.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default listen port.
pub const DEFAULT_PORT: u16 = 7420;

#[derive(Debug, Parser)]
#[command(
    name = "fw hub",
    about = "The flywheel hub: a web workspace of server-owned agent terminals"
)]
struct HubCli {
    #[command(subcommand)]
    command: HubCommand,
}

#[derive(Debug, Subcommand)]
enum HubCommand {
    /// Run the hub server in the foreground and print the login URLs.
    Serve(ServeArgs),
    /// Print one-time login URLs for the running hub (loopback, LAN,
    /// tailnet, public), optionally as a QR code for a phone. The
    /// long-lived token is never shown.
    Url(UrlArgs),
    /// Print the hub's access token (created on first use), or rotate it.
    Token(TokenArgs),
}

#[derive(Debug, clap::Args)]
struct UrlArgs {
    /// Also draw the most remote-friendly URL as a QR code.
    #[arg(long)]
    qr: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct TokenArgs {
    /// Replace the token with a fresh one. A running hub switches at once:
    /// every browser login, one-time code, and open terminal connection of
    /// the old token stops working. Prints nothing; `fw hub token` shows
    /// the new one.
    #[arg(long)]
    rotate: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct StateDirArg {
    /// State directory [default: $XDG_STATE_HOME/flywheel/hub or
    /// ~/.local/state/flywheel/hub].
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Address to listen on. Anything but loopback exposes shells to the
    /// network: prefer `tailscale serve` for remote access.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    bind: IpAddr,
    /// Port to listen on (0 picks a free port).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Extra browser origin to accept (e.g. https://box.tailnet.ts.net);
    /// repeatable. Same-origin is always accepted.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    allow_origins: Vec<String>,
    /// A URL browsers reach this hub at through a proxy (e.g.
    /// https://box.tailnet.ts.net); repeatable. Accepted as an origin,
    /// listed first by `fw hub url`, and given `Secure` cookies when
    /// https. `tailscale serve` URLs proxying to this port are detected
    /// automatically.
    #[arg(long = "public-url", value_name = "URL")]
    public_urls: Vec<String>,
    /// Per-session scrollback kept by the server's terminal emulator, in
    /// lines (capped at 1M cells, so wide terminals keep fewer).
    #[arg(long, value_name = "LINES", default_value_t = crate::vt::DEFAULT_SCROLLBACK_LINES)]
    scrollback_lines: usize,
    #[command(flatten)]
    state: StateDirArg,
}

fn resolve_state_dir(arg: &StateDirArg) -> Result<PathBuf, String> {
    match &arg.state_dir {
        Some(dir) => Ok(dir.clone()),
        None => default_state_dir()
            .ok_or_else(|| "cannot resolve a state dir: set HOME or pass --state-dir".to_owned()),
    }
}

/// Route `fw hub <args>` (`args` excludes `hub`); returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = run(args, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// [`main`] against injected streams.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // argv[0] becomes clap's bin name, so usage reads `fw hub ...`.
    let argv = std::iter::once("fw hub".to_owned()).chain(args.iter().cloned());
    let cli = match HubCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let rendered = e.render().to_string();
            let _ = if e.use_stderr() {
                err.write_all(rendered.as_bytes())
            } else {
                out.write_all(rendered.as_bytes())
            };
            return code;
        }
    };
    match cli.command {
        HubCommand::Token(args) => token(&args, out, err),
        HubCommand::Url(args) => url_cmd(&args, out, err),
        HubCommand::Serve(args) => serve_cmd(args, out, err),
    }
}

fn token(args: &TokenArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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

/// The running hub for `state_dir`, if its runtime file names a live
/// process.
fn running_hub(state_dir: &Path) -> Result<Option<RuntimeInfo>, String> {
    let info = RuntimeInfo::read(state_dir).map_err(|e| {
        format!(
            "runtime file {}: {e}",
            state_dir.join(crate::remote::RUNTIME_FILE).display()
        )
    })?;
    Ok(info.filter(|info| process_alive(info.pid)))
}

/// Rotate through the running hub (so it switches without a restart), or
/// in the token file when none runs.
fn rotate(state_dir: &Path) -> Result<String, String> {
    if let Some(info) = running_hub(state_dir)? {
        let token = Token::load(state_dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the running hub's token file is missing".to_owned())?;
        match client::request(
            info.connect_addr(),
            "POST",
            "/api/token/rotate",
            token.expose(),
            None,
            CLIENT_TIMEOUT,
        ) {
            Ok(response) if response.status == 204 => {
                return Ok(
                    "token rotated; the running hub signed out every browser and closed every terminal connection (log in again via `fw hub url`)"
                        .to_owned(),
                );
            }
            Ok(response) => {
                return Err(format!(
                    "the running hub (pid {}) refused the rotation: {} {}",
                    info.pid,
                    response.status,
                    response.error_message()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "cannot reach the running hub (pid {}) at {}: {e}",
                    info.pid,
                    info.connect_addr()
                ));
            }
        }
    }
    Token::rotate(state_dir).map_err(|e| e.to_string())?;
    Ok("token rotated (no hub is running; the next `fw hub serve` uses it)".to_owned())
}

fn url_cmd(args: &UrlArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match mint_remote_code(&args.state) {
        Ok((info, code)) => {
            let tailscale =
                tailscale_program().and_then(|p| detect_tailscale(&p, info.listen.port()));
            let targets = login_targets(
                info.listen,
                &info.public_urls,
                tailscale.as_ref(),
                &local_interfaces(info.listen),
            );
            print_login(out, err, &targets, &code, args.qr);
            print_tailscale_hint(err, tailscale.as_ref(), &targets, info.listen.port());
            0
        }
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

/// Ask the running hub for a one-time login code.
fn mint_remote_code(state: &StateDirArg) -> Result<(RuntimeInfo, String), String> {
    let dir = resolve_state_dir(state)?;
    let info = running_hub(&dir)?.ok_or_else(|| {
        format!(
            "no hub is running for state dir {} (start one with `fw hub serve`)",
            dir.display()
        )
    })?;
    let token = Token::load(&dir)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the running hub's token file is missing".to_owned())?;
    let response = client::request(
        info.connect_addr(),
        "POST",
        "/api/login-codes",
        token.expose(),
        None,
        CLIENT_TIMEOUT,
    )
    .map_err(|e| {
        format!(
            "cannot reach the running hub (pid {}) at {}: {e}",
            info.pid,
            info.connect_addr()
        )
    })?;
    if response.status != 200 {
        return Err(format!(
            "the running hub refused to mint a login code: {} {}",
            response.status,
            response.error_message()
        ));
    }
    let code = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned))
        .ok_or_else(|| "the running hub sent no login code".to_owned())?;
    Ok((info, code))
}

/// The login URLs for `code`, then (with `qr`) the first remote one as a
/// QR code.
fn print_login(
    out: &mut dyn Write,
    err: &mut dyn Write,
    targets: &[LoginTarget],
    code: &str,
    qr: bool,
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
    if !qr {
        return;
    }
    let Some(remote) = targets.iter().find(|t| t.remote) else {
        let _ = writeln!(
            err,
            "fw hub: no QR code: the hub listens on loopback only, which a phone cannot reach (use `tailscale serve`, --public-url, or `fw hub serve --bind`)"
        );
        return;
    };
    let url = code_login_url(&remote.base, code);
    match qr_lines(&url) {
        Ok(lines) => {
            let _ = writeln!(
                out,
                "
scan to log in ({}):",
                remote.label
            );
            for line in lines {
                let _ = writeln!(out, "  {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: {e}");
        }
    }
}

/// Point at `tailscale serve` when the tailnet is up but nothing serves the
/// hub over HTTPS on it.
fn print_tailscale_hint(
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
fn browse_host(addr: SocketAddr) -> String {
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
fn local_interfaces(listen: SocketAddr) -> Vec<(String, IpAddr)> {
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
fn route_address(unspecified: IpAddr) -> Option<IpAddr> {
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

fn serve_cmd(args: ServeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let state_dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    let public_urls = match args
        .public_urls
        .iter()
        .map(|u| normalize_public_url(u))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(urls) => urls,
        Err(message) => {
            let _ = writeln!(err, "fw hub: --public-url {message}");
            return 2;
        }
    };
    let mut options = HubOptions::new(state_dir);
    options.scrollback_lines = args.scrollback_lines;
    let hub = match Hub::open(options) {
        Ok(hub) => hub,
        Err(e) => {
            let _ = writeln!(err, "fw hub: {e}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hub-http")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "fw hub: failed to start the async runtime: {e}");
            return 1;
        }
    };
    let bind = SocketAddr::new(args.bind, args.port);
    let listener = match runtime.block_on(tokio::net::TcpListener::bind(bind)) {
        Ok(listener) => listener,
        Err(e) => {
            let _ = writeln!(err, "fw hub: cannot listen on {bind}: {e}");
            return 1;
        }
    };
    let local = listener.local_addr().unwrap_or(bind);
    if !args.bind.is_loopback() {
        let _ = writeln!(
            err,
            "fw hub: WARNING: listening on {local}, reachable beyond this machine. \
             Anyone with the token gets a shell; prefer the default loopback bind \
             plus `tailscale serve`."
        );
    }
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, local.port()));
    let base = format!("http://{}", browse_host(local));
    let _ = writeln!(out, "flywheel hub listening on {base}");
    let _ = writeln!(out, "state dir: {}", hub.state_dir().display());
    let targets = login_targets(
        local,
        &public_urls,
        tailscale.as_ref(),
        &local_interfaces(local),
    );
    match hub.mint_login_code() {
        Ok(code) => {
            let qr = targets.iter().any(|t| t.remote);
            print_login(out, err, &targets, &code, qr);
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: cannot mint a login code: {e}");
        }
    }
    print_tailscale_hint(err, tailscale.as_ref(), &targets, local.port());
    let _ = out.flush();

    let runtime_info = RuntimeInfo {
        pid: std::process::id(),
        listen: local,
        public_urls: public_urls.clone(),
    };
    if let Err(e) = runtime_info.write(hub.state_dir()) {
        let _ = writeln!(
            err,
            "fw hub: cannot write the runtime file (`fw hub url` and `fw hub token --rotate` \
             will not find this hub): {e}"
        );
    }
    let mut allowed_origins = args.allow_origins;
    allowed_origins.extend(public_urls);
    if let Some(ts) = &tailscale {
        allowed_origins.extend(ts.serve_urls.iter().cloned());
    }
    let server_options = ServerOptions { allowed_origins };
    let state_dir = hub.state_dir().to_path_buf();
    let result = runtime.block_on(serve(listener, hub, server_options, shutdown_signal()));
    RuntimeInfo::remove_if_owned(&state_dir, runtime_info.pid);
    match result {
        Ok(()) => {
            let _ = writeln!(out, "flywheel hub stopped");
            0
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: server error: {e}");
            1
        }
    }
}

/// Resolves on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn token_verb_creates_then_reprints_the_same_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("hub");
        let state_arg = state.to_str().unwrap();
        let (code, first, err) = run_args(&["token", "--state-dir", state_arg]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(first.trim().len(), 64);
        let (_, second, _) = run_args(&["token", "--state-dir", state_arg]);
        assert_eq!(first, second);
    }

    #[test]
    fn usage_errors_exit_2_and_help_exits_0() {
        let (code, _, err) = run_args(&["bogus"]);
        assert_eq!(code, 2);
        assert!(err.contains("bogus"));
        let (code, _, err) = run_args(&[]);
        assert_eq!(code, 2, "{err}");
        let (code, out, _) = run_args(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("Usage: fw hub"), "{out}");
        assert!(out.contains("serve") && out.contains("token"));
        let (code, _, _) = run_args(&["serve", "--port", "notaport"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn route_address_is_never_loopback_or_unspecified() {
        // Offline boxes may have no route; when there is one it is usable
        // from another device.
        if let Some(ip) = route_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)) {
            assert!(!ip.is_loopback() && !ip.is_unspecified() && ip.is_ipv4());
        }
    }

    #[test]
    fn browse_host_maps_unspecified_to_loopback() {
        assert_eq!(
            browse_host("0.0.0.0:7420".parse().unwrap()),
            "127.0.0.1:7420"
        );
        assert_eq!(browse_host("[::1]:80".parse().unwrap()), "[::1]:80");
        assert_eq!(
            browse_host("192.168.1.5:9".parse().unwrap()),
            "192.168.1.5:9"
        );
    }
}
