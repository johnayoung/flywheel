//! The `fw hub <verb>` entry: `serve`, `url`, `token`, and `report` (the
//! status hook agent sessions run).

use crate::auth::{LOGIN_CODE_TTL, Token};
use crate::flywheel::{FlywheelMonitor, MonitorTiming};
use crate::hub::{Hub, HubError, HubOptions, RestoreOutcome};
use crate::notify::{Notifier, NotifyConfig};
use crate::paths::default_state_dir;
use crate::remote::{
    LoginTarget, RuntimeInfo, Tailscale, detect_tailscale, interface_addresses, login_targets,
    normalize_public_url, qr_lines, tailscale_program,
};
use crate::server::{ServerOptions, code_login_url, serve};
use crate::{browser, lifecycle, script};
use chrono::{SecondsFormat, Utc};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Start the hub detached (output to the state dir's rotating
    /// hub.log); returns once it is serving and prints the login URLs.
    Start(lifecycle::StartArgs),
    /// Stop the running hub gracefully; its running sessions are recorded
    /// stopped and come back on the next start.
    Stop(lifecycle::StopArgs),
    /// Is the hub running: pid, uptime, URL, workspace and session counts.
    /// Exit code 0 running, 3 not running, 1 running but unreachable.
    Status(lifecycle::StatusArgs),
    /// Print the detached hub's log (hub.log), optionally following it.
    Logs(lifecycle::LogsArgs),
    /// Write a systemd --user unit that runs the hub (printed; not
    /// enabled).
    InstallService(lifecycle::InstallServiceArgs),
    /// List workspaces and their sessions with state.
    Ls(script::LsArgs),
    /// Open a session in a workspace; prints its id.
    New(script::NewArgs),
    /// Type text into a session (pasted, bracketed-paste aware).
    Send(script::SendArgs),
    /// Print a session's screen text.
    Read(script::ReadArgs),
    /// Wait until a session is done, blocked, idle, working, or has
    /// exited. Exit code 0 met, 124 --timeout, 1 error.
    Wait(crate::wait::WaitArgs),
    /// Attach this terminal to a session (a picker without SESSION):
    /// raw passthrough, prefix C-b then d detach, s switch, t status bar.
    Attach(crate::attach::AttachArgs),
    /// Hang up a session's processes (the record stays, as exited).
    Kill(script::KillArgs),
    /// Start an ended or stopped session again in place (same id, name,
    /// command, and folder; a fresh terminal).
    Relaunch(script::RelaunchArgs),
    /// Import VS Code multi-root workspace files (*.code-workspace) as
    /// workspaces: their folders (git repos or plain directories) and
    /// folder names; missing folders are reported and skipped.
    Import(script::ImportArgs),
    /// Open the running hub in a browser on this machine, signed in with
    /// a fresh one-time code (also printed).
    Open(browser::OpenArgs),
    /// Print one-time login URLs for the running hub (loopback, LAN,
    /// tailnet, public), optionally as a QR code for a phone. The
    /// long-lived token is never shown.
    Url(UrlArgs),
    /// Print the hub's access token (created on first use), or rotate it.
    Token(TokenArgs),
    /// Report an agent session's status to its hub (run by the agent's
    /// hooks inside a hub session; reads FLYWHEEL_HUB_SESSION,
    /// FLYWHEEL_HUB_REPORT_SOCKET, FLYWHEEL_HUB_REPORT_SECRET).
    Report(ReportArgs),
    /// Print the code blocks an agent session suggested, exactly as
    /// written (shell commands unless --all), newest first; or copy the
    /// newest with OSC 52.
    Commands(crate::commands_cli::CommandsArgs),
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
struct ReportArgs {
    /// The agent sending it.
    #[arg(long, value_parser = ["claude", "codex"])]
    source: String,
    /// The agent's event (a Claude Code hook event, or `notify`).
    #[arg(long)]
    event: String,
    /// Event qualifier (a Claude Code notification type).
    #[arg(long)]
    detail: Option<String>,
    /// Read the event's JSON payload from stdin (Claude Code hooks).
    #[arg(long)]
    stdin: bool,
    /// The event's JSON payload (Codex passes it as the last argument).
    payload: Option<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct StateDirArg {
    /// State directory [default: $XDG_STATE_HOME/flywheel/hub or
    /// ~/.local/state/flywheel/hub].
    #[arg(long, value_name = "DIR")]
    pub(crate) state_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    #[command(flatten)]
    hub: HubArgs,
    /// Run detached: send stdout and stderr to this rotating log file and
    /// print no login codes into it (`fw hub start` passes it).
    #[arg(long, value_name = "PATH", hide = true)]
    log_file: Option<PathBuf>,
    /// Print no one-time login code (its output is a log or journal);
    /// `fw hub url` mints one (`install-service` passes it).
    #[arg(long, hide = true)]
    no_login_code: bool,
}

/// How the hub serves: shared by `serve`, `start`, and `install-service`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct HubArgs {
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
    /// Address of a reverse proxy on another machine whose
    /// X-Forwarded-For/-Host/-Proto are believed (as a local proxy's
    /// always are), so clients behind it are rate-limited by their own
    /// address; repeatable.
    #[arg(long = "trusted-proxy", value_name = "IP")]
    trusted_proxies: Vec<IpAddr>,
    /// Per-session scrollback kept by the server's terminal emulator, in
    /// lines (capped at 1M cells, so wide terminals keep fewer).
    #[arg(long, value_name = "LINES", default_value_t = crate::vt::DEFAULT_SCROLLBACK_LINES)]
    scrollback_lines: usize,
    /// Do not restore the sessions the last clean shutdown stopped: they
    /// stay stopped, on later starts too, until relaunched (`fw hub
    /// relaunch` or the UI).
    #[arg(long)]
    no_restore: bool,
    /// Restore agent sessions as fresh shells instead of resuming their
    /// conversations (Claude Code `--resume`, `codex resume`).
    #[arg(long)]
    no_agent_resume: bool,
    /// Turn off the Flywheel panel (verified-loop status for the
    /// flywheel-adopted folders of your workspaces).
    #[arg(long)]
    no_flywheel: bool,
    /// Where the web UI's "Import VS Code workspace" looks for
    /// *.code-workspace files (3 levels deep) [default: ~/code].
    #[arg(long, value_name = "DIR")]
    import_root: Option<PathBuf>,
    /// Push a notification to this ntfy-compatible URL (e.g.
    /// https://ntfy.sh/<topic>) when an agent becomes blocked or finishes
    /// a turn nobody has seen; overrides `url` in <state-dir>/notify.toml.
    #[arg(long, value_name = "URL")]
    notify_url: Option<String>,
    /// Prefix of the branches session worktrees are cut on
    /// (`<prefix><task>`, e.g. `agent/`) [default: none].
    #[arg(long, value_name = "PREFIX", value_parser = parse_branch_prefix)]
    worktree_branch_prefix: Option<String>,
    #[command(flatten)]
    pub(crate) state: StateDirArg,
}

fn parse_branch_prefix(raw: &str) -> Result<String, String> {
    crate::git::validate_prefix(raw)
}

impl HubArgs {
    /// These options as `serve` arguments, with `state_dir` (absolute)
    /// in place of the one given.
    pub(crate) fn to_argv(&self, state_dir: &Path) -> Vec<String> {
        let mut argv = vec![
            "--bind".to_owned(),
            self.bind.to_string(),
            "--port".to_owned(),
            self.port.to_string(),
        ];
        for origin in &self.allow_origins {
            argv.extend(["--allow-origin".to_owned(), origin.clone()]);
        }
        for url in &self.public_urls {
            argv.extend(["--public-url".to_owned(), url.clone()]);
        }
        for proxy in &self.trusted_proxies {
            argv.extend(["--trusted-proxy".to_owned(), proxy.to_string()]);
        }
        argv.extend([
            "--scrollback-lines".to_owned(),
            self.scrollback_lines.to_string(),
        ]);
        if self.no_restore {
            argv.push("--no-restore".to_owned());
        }
        if self.no_agent_resume {
            argv.push("--no-agent-resume".to_owned());
        }
        if self.no_flywheel {
            argv.push("--no-flywheel".to_owned());
        }
        if let Some(root) = &self.import_root {
            // Absolute: the detached hub runs from `/`.
            let root = std::path::absolute(root).unwrap_or_else(|_| root.clone());
            argv.extend([
                "--import-root".to_owned(),
                root.to_string_lossy().into_owned(),
            ]);
        }
        if let Some(url) = &self.notify_url {
            argv.extend(["--notify-url".to_owned(), url.clone()]);
        }
        if let Some(prefix) = &self.worktree_branch_prefix {
            argv.extend(["--worktree-branch-prefix".to_owned(), prefix.clone()]);
        }
        argv.extend([
            "--state-dir".to_owned(),
            state_dir.to_string_lossy().into_owned(),
        ]);
        argv
    }

    /// The `--public-url`s as origins (an error names the bad one).
    pub(crate) fn public_url_origins(&self) -> Result<Vec<String>, String> {
        self.public_urls
            .iter()
            .map(|u| normalize_public_url(u))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| format!("--public-url {message}"))
    }
}

pub(crate) fn resolve_state_dir(arg: &StateDirArg) -> Result<PathBuf, String> {
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

/// What the embedding binary plugs into `fw hub serve`.
#[derive(Clone, Default)]
pub struct HubExtensions {
    /// Reads flywheel state for the Flywheel panel (`None`: no panel).
    pub flywheel: Option<Arc<dyn crate::flywheel::FlywheelProvider>>,
}

impl std::fmt::Debug for HubExtensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubExtensions")
            .field("flywheel", &self.flywheel.is_some())
            .finish()
    }
}

/// [`main`] against injected streams.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_with(args, out, err, &HubExtensions::default())
}

/// [`run`] with the embedding binary's [`HubExtensions`].
pub fn run_with(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    extensions: &HubExtensions,
) -> i32 {
    if args.first().map(String::as_str) == Some("report") {
        return report_main(&args[1..], &mut std::io::stdin().lock(), &EnvReport, err);
    }
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
        HubCommand::Serve(args) => serve_cmd(args, out, err, extensions),
        HubCommand::Commands(args) => crate::commands_cli::run(&args, out, err),
        // Routed before parsing (see `report_main`); reached only via help.
        HubCommand::Report(_) => 0,
        HubCommand::Start(args) => lifecycle::start(&args, out, err),
        HubCommand::Stop(args) => lifecycle::stop(&args, out, err),
        HubCommand::Status(args) => lifecycle::status(&args, out, err),
        HubCommand::Logs(args) => lifecycle::logs(&args, out, err),
        HubCommand::InstallService(args) => lifecycle::install_service(&args, out, err),
        HubCommand::Ls(args) => script::ls(&args, out, err),
        HubCommand::New(args) => script::new(&args, out, err),
        HubCommand::Send(args) => script::send(&args, out, err),
        HubCommand::Read(args) => script::read(&args, out, err),
        HubCommand::Wait(args) => crate::wait::wait(&args, out, err),
        HubCommand::Attach(args) => crate::attach::attach(&args, out, err),
        HubCommand::Kill(args) => script::kill(&args, out, err),
        HubCommand::Relaunch(args) => script::relaunch(&args, out, err),
        HubCommand::Import(args) => script::import(&args, out, err),
        HubCommand::Open(args) => browser::open(&args, out, err),
    }
}

/// Where `report` finds its session: the environment a hub gives agent
/// sessions.
trait ReportEnv {
    fn var(&self, name: &str) -> Option<String>;
}

struct EnvReport;

impl ReportEnv for EnvReport {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

/// `fw hub report ...`. Agent hooks run this, so it never prints to stdout
/// (Claude Code parses hook stdout) and never exits 2 (which would make a
/// `Stop` hook keep the agent going): failures are exit 1 plus a line on
/// stderr, which the agent shows as a non-blocking hook error.
fn report_main(
    args: &[String],
    stdin: &mut dyn std::io::Read,
    env: &dyn ReportEnv,
    err: &mut dyn Write,
) -> i32 {
    #[derive(Debug, Parser)]
    #[command(
        name = "fw hub report",
        about = "Report an agent session's status to its hub (for the agent's hooks only: \
                 run inside a hub session, it reads FLYWHEEL_HUB_SESSION, \
                 FLYWHEEL_HUB_REPORT_SOCKET and FLYWHEEL_HUB_REPORT_SECRET; no --state-dir)"
    )]
    struct ReportCli {
        #[command(flatten)]
        args: ReportArgs,
    }
    let argv = std::iter::once("fw hub report".to_owned()).chain(args.iter().cloned());
    let cli = match ReportCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = err.write_all(e.render().to_string().as_bytes());
            return if e.use_stderr() { 1 } else { 0 };
        }
    };
    match send_report(cli.args, stdin, env) {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(err, "fw hub report: {message}");
            1
        }
    }
}

fn send_report(
    args: ReportArgs,
    stdin: &mut dyn std::io::Read,
    env: &dyn ReportEnv,
) -> Result<(), String> {
    use crate::report;
    let need = |name: &str| {
        env.var(name)
            .ok_or_else(|| format!("{name} is not set: not running inside a hub agent session"))
    };
    let session = need(report::SESSION_ENV)?;
    let socket = need(report::SOCKET_ENV)?;
    let secret = need(report::SECRET_ENV)?;
    let raw = match (&args.payload, args.stdin) {
        (Some(payload), _) => Some(payload.clone().into_bytes()),
        (None, true) => {
            let mut bytes = Vec::new();
            stdin
                .take(16 * report::MAX_REPORT_BYTES as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("reading stdin: {e}"))?;
            Some(bytes)
        }
        (None, false) => None,
    };
    // A payload that is not JSON is dropped, not fatal: the event itself
    // still carries the status.
    let payload = raw
        .filter(|bytes| !bytes.iter().all(u8::is_ascii_whitespace))
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .map(|value| report::fit_payload(value, report::MAX_REPORT_BYTES / 2));
    let message = report::Report {
        session,
        secret,
        source: args.source,
        event: args.event,
        detail: args.detail,
        payload,
    };
    report::send(std::path::Path::new(&socket), &message).map_err(|e| e.to_string())
}

/// The argv prefix that reaches this binary's `hub` verb, for agent hooks.
fn report_command() -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    Some(vec![exe.to_string_lossy().into_owned(), "hub".to_owned()])
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
fn rotate(state_dir: &Path) -> Result<String, String> {
    if let Some(hub) = script::HubConn::running(state_dir)? {
        hub.with_timeout(CLIENT_TIMEOUT)
            .call("POST", "/api/token/rotate", None)?;
        return Ok(
            "token rotated; the running hub signed out every browser and closed every terminal connection (log in again via `fw hub url`)"
                .to_owned(),
        );
    }
    Token::rotate(state_dir).map_err(|e| e.to_string())?;
    Ok("token rotated (no hub is running; the next `fw hub serve` uses it)".to_owned())
}

fn url_cmd(args: &UrlArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let minted = resolve_state_dir(&args.state).and_then(|dir| mint_remote_code(&dir));
    match minted {
        Ok((info, code)) => {
            let qr = if args.qr { Qr::Draw } else { Qr::No };
            show_login(&info, &code, qr, out, err);
            0
        }
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
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
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, info.listen.port()));
    let targets = login_targets(
        info.listen,
        &info.public_urls,
        tailscale.as_ref(),
        &local_interfaces(info.listen),
    );
    let qr = match qr {
        Qr::Auto => auto_qr(&targets),
        qr => qr,
    };
    print_login(out, err, &targets, code, qr);
    print_tailscale_hint(err, tailscale.as_ref(), &targets, info.listen.port());
}

/// Draw a QR code when a remote device could use it and stdout can show it.
fn auto_qr(targets: &[LoginTarget]) -> Qr {
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
    let hub = script::HubConn::connect(dir)?.with_timeout(CLIENT_TIMEOUT);
    let body = hub.call("POST", "/api/login-codes", None)?;
    let code = body
        .get("code")
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "the running hub sent no login code".to_owned())?;
    Ok((hub.info, code))
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
fn print_login(
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

fn serve_cmd(
    args: ServeArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
    extensions: &HubExtensions,
) -> i32 {
    // Detached (`fw hub start`): everything this process writes to stdout
    // and stderr goes to the rotating log from here on. Declared first so
    // it is dropped last, after the final message.
    #[cfg(unix)]
    let _log = match &args.log_file {
        Some(path) => match crate::daemon::LogRedirect::install(path) {
            Ok(redirect) => Some(redirect),
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot log to {}: {e}", path.display());
                return 1;
            }
        },
        None => None,
    };
    #[cfg(not(unix))]
    if args.log_file.is_some() {
        let _ = writeln!(err, "fw hub: running detached is supported on unix only");
        return 1;
    }
    let detached = args.log_file.is_some();
    if detached {
        let _ = writeln!(
            out,
            "=== flywheel hub (pid {}) starting at {} ===",
            std::process::id(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
        );
    }
    let hub_args = args.hub;
    let state_dir = match resolve_state_dir(&hub_args.state) {
        Ok(dir) => dir,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    let public_urls = match hub_args.public_url_origins() {
        Ok(urls) => urls,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 2;
        }
    };
    if let Err(e) = crate::paths::ensure_private_dir(&state_dir) {
        let _ = writeln!(err, "fw hub: state dir {}: {e}", state_dir.display());
        return 1;
    }
    crate::install_crypto_provider();
    let notify = match NotifyConfig::load(&state_dir, hub_args.notify_url.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "fw hub: push notifications: {message}");
            return 2;
        }
    };
    let mut options = HubOptions::new(state_dir);
    options.scrollback_lines = hub_args.scrollback_lines;
    options.report_command = report_command();
    options.agent_resume = !hub_args.no_agent_resume;
    if let Some(prefix) = &hub_args.worktree_branch_prefix {
        options.worktree_branch_prefix = prefix.clone();
    }
    if let Some(root) = &hub_args.import_root {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let root = crate::state::expand_home(root, home.as_deref());
        options.import_root = Some(std::path::absolute(&root).unwrap_or(root));
    }
    // One hub per state dir: the hub holds `<state-dir>/serve.lock` for
    // its whole life (a second would overwrite the first's runtime file and
    // report socket, leaving `url` / `token --rotate` and agent hooks unable
    // to reach the first).
    let hub = match Hub::open(options) {
        Ok(hub) => hub,
        Err(HubError::InUse(dir)) => {
            let at = RuntimeInfo::read(&dir)
                .ok()
                .flatten()
                .map(|info| format!(" (pid {}, {})", info.pid, info.listen))
                .unwrap_or_default();
            let _ = writeln!(
                err,
                "fw hub: another hub is already serving state dir {}{at}; stop it or pass a different --state-dir",
                dir.display()
            );
            return 1;
        }
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
    let bind = SocketAddr::new(hub_args.bind, hub_args.port);
    let listener = match runtime.block_on(tokio::net::TcpListener::bind(bind)) {
        Ok(listener) => listener,
        Err(e) => {
            let _ = writeln!(err, "fw hub: cannot listen on {bind}: {e}");
            return 1;
        }
    };
    let local = listener.local_addr().unwrap_or(bind);
    if !hub_args.bind.is_loopback() {
        let _ = writeln!(
            err,
            "fw hub: WARNING: listening on {local}, reachable beyond this machine. \
             Anyone with the token gets a shell; prefer the default loopback bind \
             plus `tailscale serve`."
        );
    }
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, local.port()));
    let base = format!("http://{}", browse_host(local));
    if local.ip().is_unspecified() {
        let _ = writeln!(
            out,
            "flywheel hub listening on {local} (all interfaces; {base} from this machine)"
        );
    } else {
        let _ = writeln!(out, "flywheel hub listening on {base}");
    }
    let _ = writeln!(out, "state dir: {}", hub.state_dir().display());
    // Taken before anything is restored: a SIGTERM from here on (`fw hub
    // start` giving up, `systemctl stop`) shuts down gracefully -- every
    // restored session recorded stopped again -- instead of killing the
    // hub mid-restore and leaving them recorded running (a crash).
    let shutdown = {
        let _enter = runtime.enter();
        match ShutdownSignal::install() {
            Ok(signal) => signal,
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot handle shutdown signals: {e}");
                return 1;
            }
        }
    };
    if hub_args.no_restore {
        match hub.forgo_restore() {
            Ok(0) => {
                let _ = writeln!(out, "session restore off (--no-restore)");
            }
            Ok(n) => {
                let _ = writeln!(
                    out,
                    "session restore off (--no-restore): {n} session{} stay{} stopped until relaunched",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "s" } else { "" }
                );
            }
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot record --no-restore: {e}");
            }
        }
    } else {
        report_restore(&hub.restore_sessions(), out, err);
    }
    if detached || args.no_login_code {
        // One-time codes stay out of log files and journals: `fw hub
        // start` printed one to the operator's terminal.
        let _ = writeln!(
            out,
            "log in with a one-time URL from `fw hub url` (or `fw hub url --qr` for a phone)"
        );
    } else {
        let targets = login_targets(
            local,
            &public_urls,
            tailscale.as_ref(),
            &local_interfaces(local),
        );
        match hub.mint_login_code() {
            Ok(code) => print_login(out, err, &targets, &code, auto_qr(&targets)),
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot mint a login code: {e}");
            }
        }
        print_tailscale_hint(err, tailscale.as_ref(), &targets, local.port());
    }
    let _ = out.flush();

    // Written last: `fw hub start` treats it as the hub being ready.
    let runtime_info = RuntimeInfo {
        pid: std::process::id(),
        listen: local,
        public_urls: public_urls.clone(),
        started_at: Some(Utc::now()),
        log_file: args.log_file.clone(),
    };
    if let Err(e) = runtime_info.write(hub.state_dir()) {
        let _ = writeln!(
            err,
            "fw hub: cannot write the runtime file (`fw hub url`, `status`, `stop`, and the \
             scripting verbs will not find this hub): {e}"
        );
    }
    let notifier = match notify {
        Some(mut config) => {
            // A push's click opens the hub where a phone reaches it.
            config.click_base = public_urls
                .first()
                .or_else(|| tailscale.as_ref().and_then(|ts| ts.serve_urls.first()))
                .cloned();
            match Notifier::new(config) {
                Ok(notifier) => {
                    let _enter = runtime.enter();
                    notifier.watch(&hub);
                    let config = notifier.config();
                    let _ = writeln!(
                        out,
                        "push notifications: {} -> {} (debounce {}s)",
                        config.events().join(", "),
                        config.host(),
                        config.debounce.as_secs()
                    );
                    Some(notifier)
                }
                Err(e) => {
                    let _ = writeln!(err, "fw hub: push notifications off: {e}");
                    None
                }
            }
        }
        None => None,
    };
    let mut allowed_origins = hub_args.allow_origins;
    allowed_origins.extend(public_urls);
    if let Some(ts) = &tailscale {
        allowed_origins.extend(ts.serve_urls.iter().cloned());
    }
    let flywheel = match (&extensions.flywheel, hub_args.no_flywheel) {
        (Some(provider), false) => {
            let monitor = FlywheelMonitor::new(Arc::clone(provider), MonitorTiming::default());
            match monitor.watch(&hub) {
                Ok(()) => Some(monitor),
                Err(e) => {
                    let _ = writeln!(err, "fw hub: Flywheel panel off: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    let server_options = ServerOptions {
        allowed_origins,
        trusted_proxies: hub_args.trusted_proxies,
        notifier,
        flywheel,
    };
    let state_dir = hub.state_dir().to_path_buf();
    let result = runtime.block_on(serve(
        listener,
        Arc::clone(&hub),
        server_options,
        shutdown.wait(),
    ));
    // Still under the serve lock (the hub holds it until dropped).
    RuntimeInfo::remove_if_owned(&state_dir, runtime_info.pid);
    drop(hub);
    match result {
        Ok(()) => {
            let _ = writeln!(
                out,
                "flywheel hub stopped at {}",
                Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
            );
            0
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: server error: {e}");
            1
        }
    }
}

/// Say what restoring the last clean shutdown's sessions did.
fn report_restore(outcomes: &[RestoreOutcome], out: &mut dyn Write, err: &mut dyn Write) {
    for outcome in outcomes {
        match &outcome.result {
            Ok(how) => {
                let note = outcome
                    .note
                    .as_deref()
                    .map(|n| format!("; {n}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "restored session {:?} ({}) in {} ({}{note})",
                    outcome.name,
                    outcome.id,
                    outcome.cwd.display(),
                    how.as_str()
                );
            }
            Err(message) => {
                let _ = writeln!(
                    err,
                    "fw hub: cannot restore session {:?} ({}): {message}; it stays stopped",
                    outcome.name, outcome.id
                );
            }
        }
    }
}

/// Ctrl-C or SIGTERM, hooked when installed (so either one arriving
/// before the server runs is held, not fatal) and awaited by the server.
struct ShutdownSignal {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl ShutdownSignal {
    /// Hook the signals; call inside the runtime.
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                interrupt: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Resolves on the first of them (including one that arrived earlier).
    async fn wait(self) {
        #[cfg(unix)]
        {
            let Self {
                mut term,
                mut interrupt,
            } = self;
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            let _ = tokio::signal::ctrl_c().await;
        }
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
    fn notify_url_is_forwarded_to_the_detached_serve() {
        let cli = HubCli::try_parse_from([
            "fw hub",
            "serve",
            "--notify-url",
            "https://ntfy.sh/topic",
            "--port",
            "0",
        ])
        .unwrap();
        let HubCommand::Serve(args) = cli.command else {
            panic!("not serve");
        };
        let argv = args.hub.to_argv(Path::new("/state"));
        let at = argv.iter().position(|a| a == "--notify-url").unwrap();
        assert_eq!(argv[at + 1], "https://ntfy.sh/topic");
        // What `start` passes on parses back the same.
        let mut again = vec!["fw hub".to_owned(), "serve".to_owned()];
        again.extend(argv);
        let HubCommand::Serve(reparsed) = HubCli::try_parse_from(again).unwrap().command else {
            panic!("not serve");
        };
        assert_eq!(
            reparsed.hub.notify_url.as_deref(),
            Some("https://ntfy.sh/topic")
        );
    }

    #[test]
    fn serve_refuses_a_bad_notify_config_before_starting() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("hub");
        let (code, _, err) = run_args(&[
            "serve",
            "--port",
            "0",
            "--notify-url",
            "ftp://nope/x",
            "--state-dir",
            state.to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{err}");
        assert!(err.contains("push notifications"), "{err}");
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

    struct FakeEnv(Vec<(&'static str, String)>);

    impl ReportEnv for FakeEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn report_args(args: &[&str], stdin: &str, env: &FakeEnv) -> (i32, String) {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut err = Vec::new();
        let code = report_main(&args, &mut stdin.as_bytes(), env, &mut err);
        (code, String::from_utf8(err).unwrap())
    }

    #[test]
    fn report_sends_the_event_and_payload_and_never_exits_2() {
        use crate::report::{Report, ReportServer, SECRET_ENV, SESSION_ENV, SOCKET_ENV};
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let got: Arc<Mutex<Vec<Report>>> = Arc::default();
        let sink = Arc::clone(&got);
        let server = ReportServer::bind(
            dir.path(),
            Arc::new(move |r: Report| {
                if r.secret != "s3cret" {
                    return Err("bad secret".into());
                }
                sink.lock().unwrap().push(r);
                Ok(())
            }),
        )
        .unwrap();
        let socket = server.path().to_string_lossy().into_owned();
        let env = |secret: &str| {
            FakeEnv(vec![
                (SESSION_ENV, "sess".into()),
                (SOCKET_ENV, socket.clone()),
                (SECRET_ENV, secret.into()),
            ])
        };

        let (code, err) = report_args(
            &[
                "--source",
                "claude",
                "--event",
                "Notification",
                "--detail",
                "permission_prompt",
                "--stdin",
            ],
            r#"{"session_id":"abc","message":"Claude needs your permission to use Bash"}"#,
            &env("s3cret"),
        );
        assert_eq!(code, 0, "{err}");
        let (code, _) = report_args(
            &[
                "--source",
                "codex",
                "--event",
                "notify",
                r#"{"type":"agent-turn-complete"}"#,
            ],
            "",
            &env("s3cret"),
        );
        assert_eq!(code, 0);
        // Payload that is not JSON: the event still goes through.
        let (code, _) = report_args(
            &["--source", "claude", "--event", "Stop", "--stdin"],
            "not json",
            &env("s3cret"),
        );
        assert_eq!(code, 0);
        {
            let got = got.lock().unwrap();
            assert_eq!(got.len(), 3);
            assert_eq!(got[0].session, "sess");
            assert_eq!(got[0].detail.as_deref(), Some("permission_prompt"));
            assert_eq!(got[0].payload.as_ref().unwrap()["session_id"], "abc");
            assert_eq!(
                got[1].payload.as_ref().unwrap()["type"],
                "agent-turn-complete"
            );
            assert_eq!(got[2].payload, None);
        }

        // Refusals, missing env, and usage errors are exit 1, never 2.
        let (code, err) = report_args(
            &["--source", "claude", "--event", "Stop"],
            "",
            &env("wrong"),
        );
        assert_eq!(code, 1);
        assert!(err.contains("bad secret"), "{err}");
        let (code, err) = report_args(
            &["--source", "claude", "--event", "Stop"],
            "",
            &FakeEnv(vec![]),
        );
        assert_eq!(code, 1);
        assert!(err.contains("FLYWHEEL_HUB_SESSION is not set"), "{err}");
        let (code, _) = report_args(
            &["--source", "emacs", "--event", "Stop"],
            "",
            &env("s3cret"),
        );
        assert_eq!(code, 1);
        let (code, _) = report_args(&[], "", &env("s3cret"));
        assert_eq!(code, 1);
        drop(server);
        let (code, err) = report_args(
            &["--source", "claude", "--event", "Stop"],
            "",
            &env("s3cret"),
        );
        assert_eq!(code, 1, "{err}");
    }

    #[test]
    fn a_second_serve_on_one_state_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("hub");
        crate::paths::ensure_private_dir(&state).unwrap();
        let _held = crate::remote::ServeLock::acquire(&state).unwrap().unwrap();
        RuntimeInfo {
            pid: 4242,
            listen: "127.0.0.1:47533".parse().unwrap(),
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
        }
        .write(&state)
        .unwrap();
        let (code, out, err) = run_args(&[
            "serve",
            "--port",
            "0",
            "--state-dir",
            state.to_str().unwrap(),
        ]);
        assert_eq!(code, 1, "{out}{err}");
        assert!(
            err.contains("another hub is already serving") && err.contains("pid 4242"),
            "{err}"
        );
        // The first hub's runtime file is untouched.
        let info = RuntimeInfo::read(&state).unwrap().unwrap();
        assert_eq!(info.pid, 4242);
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
