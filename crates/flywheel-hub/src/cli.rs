//! The `fw hub <verb>` entry: `serve`, `url`, `token`, and `report` (the
//! status hook agent sessions run).

use crate::auth::{LOGIN_CODE_TTL, Token};
use crate::endpoint::Endpoint;
use crate::flywheel::{FlywheelMonitor, MonitorTiming};
use crate::hub::{Hub, HubError, HubOptions, RestoreOutcome};
use crate::notify::{Notifier, NotifyConfig};
use crate::paths::default_state_dir;
use crate::remote::{
    LoginTarget, RuntimeInfo, Served, Tailscale, detect_tailscale, interface_addresses,
    login_targets_served, normalize_public_url, qr_lines, tailscale_program,
};
use crate::server::{ServerOptions, code_login_url, serve_gated};
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
    /// Replace the running hub with a new process (binary) without ending
    /// any session: a live handoff of every terminal, its screen, and its
    /// state. Exit code 0 handed off, 4 refused (nothing changed).
    Upgrade(crate::upgrade::UpgradeArgs),
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
    /// Open a session in a workspace; prints its id. Agents take a first
    /// prompt, model, and permission options, or a saved --template.
    New(script::NewArgs),
    /// A workspace's launch templates: ls, save, rm.
    Template(crate::template_cli::TemplateArgs),
    /// Type text into a session (pasted, bracketed-paste aware).
    Send(script::SendArgs),
    /// Print a session's screen text.
    Read(script::ReadArgs),
    /// Why a session shows its status: what set it (hook, detection
    /// manifest rule, heuristic), the rule its live screen matches now
    /// with the evidence lines, and every rule tried. --file explains a
    /// saved screen against this machine's manifests (no hub needed).
    Explain(crate::detect::explain::ExplainArgs),
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
    /// Built-in https (`serve --tls auto`): `tls ca` prints the local CA's
    /// fingerprint and constraints (and writes its certificate), `tls
    /// rotate` replaces it.
    Tls(crate::tls_cli::TlsArgs),
    /// Report an agent session's status to its hub (run by the agent's
    /// hooks inside a hub session; reads FLYWHEEL_HUB_SESSION,
    /// FLYWHEEL_HUB_REPORT_SOCKET, FLYWHEEL_HUB_REPORT_SECRET).
    Report(ReportArgs),
    /// Print the code blocks an agent session suggested, exactly as
    /// written (shell commands unless --all), newest first; or copy the
    /// newest with OSC 52.
    Commands(crate::commands_cli::CommandsArgs),
    /// Full-text search across sessions: every terminal's scrollback and
    /// screen, agent transcripts (prompts, replies, tool calls), and
    /// suggested commands; hits grouped by session, newest first. Exit 0
    /// with hits, 1 with none.
    Search(crate::search_cli::SearchArgs),
    /// A session's git work: its branch against its base (commits,
    /// changed files with +/- counts), and staged, unstaged, and untracked
    /// changes apart; --diff PATH prints one file's diff.
    Review(crate::review_cli::ReviewArgs),
    /// Hand part of one session to another agent session (here or on a
    /// machine) as one prompt under a provenance header (`From <session>
    /// on <branch>: ...`): a selection (--selection-file), its branch
    /// diff (--diff), a suggested command (--last-command, --command), or
    /// a transcript message (--message); pasted, then Enter. Capped, with
    /// a truncation notice.
    Handoff(crate::agent_handoff_cli::HandoffArgs),
    /// Ask an agent of the other kind (Claude <-> Codex; hub.toml
    /// review.reviewer) to review a session's branch read-only, in its
    /// worktree (launched, or the paired reviewer asked again);
    /// --findings prints its answer, --send-findings pastes it into the
    /// author.
    AskReview(crate::agent_handoff_cli::AskReviewArgs),
    /// Prompt queues: prompts typed into an agent session one at a time,
    /// each once the agent is done (never while it is blocked, someone
    /// types into it, or a grading or review uses it): add, ls, rm,
    /// clear, move, pause, resume.
    Queue(crate::queue_cli::QueueArgs),
    /// Fast-forward a worktree session's base onto its branch in the main
    /// checkout (git merge --ff-only), after printing exactly what runs.
    /// Refused when the main checkout is not on the base or has
    /// uncommitted changes, or the branch is not a fast-forward (--rebase
    /// rebases it onto the base first).
    Merge(crate::review_cli::MergeArgs),
    /// Verified fan-out: one prompt to 2-6 agents in sibling worktrees cut
    /// from one commit, graded by commands the hub runs out of band;
    /// ranked board, pick the winner (merged into the base), clean up.
    Fanout(crate::fanout_cli::FanoutArgs),
    /// Other hubs shown and proxied by this one: add, ls, rm, test.
    /// The session-scripting verbs (ls, new, send, ..., merge, open,
    /// fanout) take --machine NAME to reach one; url, share and record do
    /// not.
    Machine(crate::machine_cli::MachineArgs),
    /// Credentials other hubs hold to reach this one: issue, ls, revoke.
    Peer(crate::machine_cli::PeerArgs),
    /// <state-dir>/hub.toml: check validates it, show prints the settings
    /// in force and where each came from (flag, file, default).
    Config(crate::config_cli::ConfigArgs),
    /// Local plugins (<state-dir>/plugins/<name>/plugin.toml) run on hub
    /// events and as palette actions: ls, enable, disable, logs.
    Plugin(crate::plugin_cli::PluginArgs),
    /// A read-only live view link to a session (expiring, revocable):
    /// whoever opens it watches that terminal -- everything it shows --
    /// and cannot type. `share ls`, `share revoke ID|NAME|--all`.
    Share(crate::share_cli::ShareArgs),
    /// Record a session to an asciinema v2 .cast file (start, stop), and
    /// list, download (get), and delete recordings.
    Record(crate::share_cli::RecordArgs),
    /// Check this machine for the hub: fw/flywheel on PATH (a stale or
    /// foreign `fw` shadowing this one), claude/codex and the status
    /// hooks, state dir permissions, config files, the port, tailscale,
    /// the browser opener, systemd --user, the clock, and disk space; each
    /// finding with a fix. Read-only. Exit 0 nothing failed, 1 otherwise.
    Doctor(crate::doctor::DoctorArgs),
}

// `--machine` is refused here (a peer credential mints no login codes):
// hidden from help so help matches what the verb takes.
#[derive(Debug, clap::Args)]
#[command(mut_arg("machine", |arg| arg.hide(true)))]
struct UrlArgs {
    /// Also draw the most remote-friendly URL as a QR code.
    #[arg(long)]
    qr: bool,
    #[command(flatten)]
    endpoint: crate::endpoint::EndpointArgs,
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
    /// Take over from the hub running for this state dir without ending
    /// its sessions (live handoff; `fw hub upgrade` runs this). Its
    /// listening socket is kept, so --bind/--port are ignored.
    #[arg(long)]
    handoff: bool,
    /// With --handoff: how long the old hub may stay frozen waiting for
    /// each step of this one, in seconds (`fw hub upgrade --timeout`).
    #[arg(long, value_name = "SECS", hide = true, requires = "handoff")]
    handoff_timeout: Option<u64>,
}

/// How the hub serves: shared by `serve`, `start`, and `install-service`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct HubArgs {
    /// Address to listen on [default: 127.0.0.1, or hub.toml
    /// server.bind]. Anything but loopback exposes shells to the network:
    /// prefer `tailscale serve` for remote access.
    #[arg(long)]
    bind: Option<IpAddr>,
    /// Port to listen on (0 picks a free port) [default: 7420, or hub.toml
    /// server.port].
    #[arg(long)]
    port: Option<u16>,
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
    /// Built-in https on the same port: `auto` (a local CA, name-constrained
    /// to this hub's names and addresses, signs short-lived certificates;
    /// phones install it from http://<lan-address>:<port>/ca), `off`, or
    /// `CERT,KEY` (PEM files you supply) [default: off, or hub.toml
    /// server.tls]. Plain http from the network is then redirected to
    /// https, except /ca; loopback keeps http.
    #[arg(long, value_name = "auto|off|CERT,KEY", value_parser = parse_tls_mode)]
    tls: Option<crate::tls::TlsMode>,
    /// An extra name for the local CA and its certificate: a DNS name, an
    /// IP address, or an IP network the CA may sign for (e.g.
    /// 192.168.0.0/16, so a new DHCP lease is covered); repeatable. Names
    /// added after the CA was made need `fw hub tls rotate`.
    #[arg(long = "tls-name", value_name = "NAME", value_parser = crate::tls::parse_name)]
    tls_names: Vec<crate::tls::NameSpec>,
    /// Per-session scrollback kept by the server's terminal emulator, in
    /// lines (capped at 1M cells, so wide terminals keep fewer)
    /// [default: 5000, or hub.toml sessions.scrollback_lines].
    #[arg(long, value_name = "LINES")]
    scrollback_lines: Option<usize>,
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
    /// Size a recording part reaches before it rotates to the next, e.g.
    /// 64MiB (64KiB to 4GiB) [default: 64MiB, or hub.toml
    /// recordings.part_cap].
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    record_part_cap: Option<u64>,
    /// Size of the recordings directory past which the oldest finished
    /// recordings are deleted, e.g. 1GiB [default: 1GiB, or hub.toml
    /// recordings.dir_cap].
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    record_dir_cap: Option<u64>,
    /// Expiry of a share link that names none, e.g. 1h [default: 1h, or
    /// hub.toml shares.default_expiry].
    #[arg(long, value_name = "DURATION", value_parser = crate::share::parse_ttl)]
    share_default_expiry: Option<Duration>,
    /// Longest expiry a share link may have, e.g. 1d (60s to 30d)
    /// [default: 30d, or hub.toml shares.max_expiry].
    #[arg(long, value_name = "DURATION", value_parser = crate::share::parse_ttl)]
    share_max_expiry: Option<Duration>,
    #[command(flatten)]
    pub(crate) state: StateDirArg,
}

fn parse_tls_mode(raw: &str) -> Result<crate::tls::TlsMode, String> {
    Ok(match crate::tls::parse_mode(raw)? {
        // Absolute: a detached hub runs from `/`.
        crate::tls::TlsMode::Files { cert, key } => crate::tls::TlsMode::Files {
            cert: std::path::absolute(&cert).unwrap_or(cert),
            key: std::path::absolute(&key).unwrap_or(key),
        },
        mode => mode,
    })
}

fn parse_branch_prefix(raw: &str) -> Result<String, String> {
    crate::git::validate_prefix(raw)
}

fn parse_size(raw: &str) -> Result<u64, String> {
    crate::record::parse_size(raw)
}

impl HubArgs {
    /// These options as `serve` arguments, with `state_dir` (absolute)
    /// in place of the one given.
    pub(crate) fn to_argv(&self, state_dir: &Path) -> Vec<String> {
        // Only what was given: the rest comes from hub.toml (re-read by
        // the started hub) or the defaults.
        let mut argv = Vec::new();
        if let Some(bind) = self.bind {
            argv.extend(["--bind".to_owned(), bind.to_string()]);
        }
        if let Some(port) = self.port {
            argv.extend(["--port".to_owned(), port.to_string()]);
        }
        for origin in &self.allow_origins {
            argv.extend(["--allow-origin".to_owned(), origin.clone()]);
        }
        for url in &self.public_urls {
            argv.extend(["--public-url".to_owned(), url.clone()]);
        }
        for proxy in &self.trusted_proxies {
            argv.extend(["--trusted-proxy".to_owned(), proxy.to_string()]);
        }
        if let Some(mode) = &self.tls {
            argv.extend(["--tls".to_owned(), mode.to_string()]);
        }
        for name in &self.tls_names {
            argv.extend(["--tls-name".to_owned(), name.to_string()]);
        }
        if let Some(lines) = self.scrollback_lines {
            argv.extend(["--scrollback-lines".to_owned(), lines.to_string()]);
        }
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
        if let Some(bytes) = self.record_part_cap {
            argv.extend(["--record-part-cap".to_owned(), bytes.to_string()]);
        }
        if let Some(bytes) = self.record_dir_cap {
            argv.extend(["--record-dir-cap".to_owned(), bytes.to_string()]);
        }
        if let Some(ttl) = self.share_default_expiry {
            argv.extend([
                "--share-default-expiry".to_owned(),
                ttl.as_secs().to_string(),
            ]);
        }
        if let Some(ttl) = self.share_max_expiry {
            argv.extend(["--share-max-expiry".to_owned(), ttl.as_secs().to_string()]);
        }
        argv.extend([
            "--state-dir".to_owned(),
            state_dir.to_string_lossy().into_owned(),
        ]);
        argv
    }

    /// What was given on the command line, for [`crate::config`].
    pub(crate) fn flags(&self) -> crate::config::Flags {
        crate::config::Flags {
            bind: self.bind,
            port: self.port,
            public_urls: self.public_urls.clone(),
            allow_origins: self.allow_origins.clone(),
            trusted_proxies: self.trusted_proxies.clone(),
            tls: self.tls.clone(),
            tls_names: self.tls_names.clone(),
            scrollback_lines: self.scrollback_lines,
            no_restore: self.no_restore,
            no_agent_resume: self.no_agent_resume,
            no_flywheel: self.no_flywheel,
            import_root: self.import_root.clone(),
            worktree_branch_prefix: self.worktree_branch_prefix.clone(),
            record_part_cap: self.record_part_cap,
            record_dir_cap: self.record_dir_cap,
            share_default_expiry: self.share_default_expiry,
            share_max_expiry: self.share_max_expiry,
        }
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

/// Most async worker threads the hub runs. Its async work is many small
/// wakeups (terminal output frames, pushes, short requests; anything slow
/// runs on the blocking pool): measured with 50 sessions and 3 browsers,
/// a worker per core (32 here) spent more time parking and waking workers
/// than serving, and its per-thread malloc arenas grew the RSS.
pub const MAX_RUNTIME_WORKERS: usize = 4;

/// Async workers for this machine: one per core, at most
/// [`MAX_RUNTIME_WORKERS`].
fn runtime_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(2, std::num::NonZeroUsize::get)
        .clamp(1, MAX_RUNTIME_WORKERS)
}

/// The `fw hub` command tree as clap builds it (the docs drift check walks
/// it: every verb and flag must be in `docs/hub/reference.md`).
#[cfg(test)]
pub(crate) fn command() -> clap::Command {
    <HubCli as clap::CommandFactory>::command()
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
    /// Runs verified fan-outs' graders (`None`: fan-outs cannot start or
    /// grade).
    pub graders: Option<Arc<dyn crate::fanout::GraderRunner>>,
}

impl std::fmt::Debug for HubExtensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubExtensions")
            .field("flywheel", &self.flywheel.is_some())
            .field("graders", &self.graders.as_ref().map(|g| g.name()))
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
        HubCommand::Tls(args) => crate::tls_cli::main(&args, out, err),
        HubCommand::Url(args) => url_cmd(&args, out, err),
        HubCommand::Serve(args) => serve_cmd(args, out, err, extensions),
        HubCommand::Commands(args) => crate::commands_cli::run(&args, out, err),
        HubCommand::Search(args) => crate::search_cli::run(&args, out, err),
        HubCommand::Review(args) => crate::review_cli::review(&args, out, err),
        HubCommand::Merge(args) => crate::review_cli::merge(&args, out, err),
        HubCommand::Fanout(args) => crate::fanout_cli::main(&args, out, err),
        // Routed before parsing (see `report_main`); reached only via help.
        HubCommand::Report(_) => 0,
        HubCommand::Start(args) => lifecycle::start(&args, out, err),
        HubCommand::Stop(args) => lifecycle::stop(&args, out, err),
        HubCommand::Upgrade(args) => crate::upgrade::upgrade(&args, out, err),
        HubCommand::Status(args) => lifecycle::status(&args, out, err),
        HubCommand::Logs(args) => lifecycle::logs(&args, out, err),
        HubCommand::InstallService(args) => lifecycle::install_service(&args, out, err),
        HubCommand::Ls(args) => script::ls(&args, out, err),
        HubCommand::New(args) => script::new(&args, out, err),
        HubCommand::Template(args) => crate::template_cli::main(&args, out, err),
        HubCommand::Send(args) => script::send(&args, out, err),
        HubCommand::Read(args) => script::read(&args, out, err),
        HubCommand::Explain(args) => crate::detect::explain::run(&args, out, err),
        HubCommand::Wait(args) => crate::wait::wait(&args, out, err),
        HubCommand::Attach(args) => crate::attach::attach(&args, out, err),
        HubCommand::Kill(args) => script::kill(&args, out, err),
        HubCommand::Relaunch(args) => script::relaunch(&args, out, err),
        HubCommand::Import(args) => script::import(&args, out, err),
        HubCommand::Open(args) => browser::open(&args, out, err),
        HubCommand::Machine(args) => crate::machine_cli::machine(&args, out, err),
        HubCommand::Peer(args) => crate::machine_cli::peer(&args, out, err),
        HubCommand::Config(args) => crate::config_cli::main(&args, out, err),
        HubCommand::Plugin(args) => crate::plugin_cli::main(&args, out, err),
        HubCommand::Share(args) => crate::share_cli::share(&args, out, err),
        HubCommand::Record(args) => crate::share_cli::record(&args, out, err),
        HubCommand::Doctor(args) => crate::doctor::doctor(&args, out, err),
        HubCommand::Handoff(args) => crate::agent_handoff_cli::handoff(&args, out, err),
        HubCommand::AskReview(args) => crate::agent_handoff_cli::ask_review(&args, out, err),
        HubCommand::Queue(args) => crate::queue_cli::main(&args, out, err),
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

fn url_cmd(args: &UrlArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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
fn login_qrs(remote: &LoginTarget, code: &str) -> Vec<(String, String)> {
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
fn draw_qr(out: &mut dyn Write, err: &mut dyn Write, url: &str, title: &str) {
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
fn print_tls(
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
    if let Err(message) = hub_args.public_url_origins() {
        let _ = writeln!(err, "fw hub: {message}");
        return 2;
    }
    if let Err(e) = crate::paths::ensure_private_dir(&state_dir) {
        let _ = writeln!(err, "fw hub: state dir {}: {e}", state_dir.display());
        return 1;
    }
    // `<state-dir>/hub.toml` under the flags given (flag > file > default).
    let config = match crate::config::ConfigWatch::open(
        &state_dir,
        hub_args.flags(),
        crate::config::home_dir(),
    ) {
        Ok(config) => Arc::new(config),
        Err(message) => {
            let _ = writeln!(
                err,
                "fw hub: {message}\nfw hub: fix it (`fw hub config check`) or move it aside; not starting"
            );
            return 2;
        }
    };
    let effective = config.started().clone();
    let public_urls = effective.public_urls.clone();
    crate::install_crypto_provider();
    let notify = match NotifyConfig::load(&state_dir, hub_args.notify_url.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "fw hub: push notifications: {message}");
            return 2;
        }
    };
    let mut options = HubOptions::new(state_dir);
    options.scrollback_lines = effective.scrollback_lines;
    options.report_command = report_command();
    options.agent_resume = effective.agent_resume;
    options.worktree_branch_prefix = effective.worktree_branch_prefix.clone();
    options.import_root = effective.import_root.clone();
    options.agent_defaults = effective.agent_defaults.clone();
    options.record_limits = effective.record_limits;
    options.share_policy = effective.share_policy;
    options.reviewer = effective.reviewer;
    options.queue_typing_quiet = effective.queue_typing_quiet;
    // One hub per state dir: the hub holds `<state-dir>/serve.lock` for
    // its whole life (a second would overwrite the first's runtime file and
    // report socket, leaving `url` / `token --rotate` and agent hooks unable
    // to reach the first).
    let serve_args = hub_args.to_argv(
        &std::path::absolute(&options.state_dir).unwrap_or_else(|_| options.state_dir.clone()),
    );
    let mut taken_over = None;
    let opened = if args.handoff {
        let step = args.handoff_timeout.map(Duration::from_secs);
        match crate::upgrade::take_over(options, step, err) {
            Ok((hub, taken)) => {
                taken_over = Some(taken);
                Ok(hub)
            }
            Err(code) => return code,
        }
    } else {
        Hub::open(options)
    };
    let hub = match opened {
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
        .worker_threads(runtime_workers())
        .thread_name("hub-http")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "fw hub: failed to start the async runtime: {e}");
            return 1;
        }
    };
    let bind = SocketAddr::new(effective.bind, effective.port);
    let listening = match taken_over.as_mut() {
        Some(taken) => taken.listener(&runtime),
        None => runtime.block_on(tokio::net::TcpListener::bind(bind)),
    };
    let listener = match listening {
        Ok(listener) => listener,
        Err(e) => {
            let _ = writeln!(err, "fw hub: cannot listen on {bind}: {e}");
            return 1;
        }
    };
    let local = listener.local_addr().unwrap_or(bind);
    if !effective.bind.is_loopback() {
        let advice = if effective.tls.is_on() {
            "Traffic from the network is https (--tls); plain http from it reaches only /ca."
        } else {
            "Traffic is plain http: prefer the default loopback bind plus `tailscale serve`, \
             or add --tls auto."
        };
        let _ = writeln!(
            err,
            "fw hub: WARNING: listening on {local}, reachable beyond this machine. \
             Anyone with the token gets a shell. {advice}"
        );
    }
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
    // A handoff completes first: the old hub is frozen until then.
    let handed_over = match taken_over.take() {
        Some(taken) => match taken.finish(&hub) {
            Ok(said) => Some(said),
            Err(message) => {
                let _ = writeln!(err, "fw hub: {message}");
                return 1;
            }
        },
        None => None,
    };
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, local.port()));
    // Built-in TLS: the CA and a leaf for every name this hub is reached
    // at, before anything listens for it.
    let tls = match &effective.tls {
        crate::tls::TlsMode::Off => None,
        mode => {
            let names = crate::tls::hub_names(
                local,
                &crate::machines::host_name(),
                &interface_addresses(),
                tailscale.as_ref(),
                &effective.tls_names,
            );
            match crate::tls::TlsServing::start(
                mode,
                hub.state_dir(),
                local,
                &names,
                &effective.tls_names,
            ) {
                Ok(tls) => tls,
                Err(e) => {
                    let _ = writeln!(err, "fw hub: tls: {e}");
                    return 1;
                }
            }
        }
    };
    let served = match (&tls, &effective.tls) {
        (None, _) => Served::Http,
        (Some(_), crate::tls::TlsMode::Auto) => Served::Https { ca_page: true },
        (Some(_), _) => Served::Https { ca_page: false },
    };
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
    if let Some(tls) = &tls {
        print_tls(out, err, tls, local);
    }
    if let Some(said) = handed_over {
        // Sessions came along running: nothing to restore.
        let _ = writeln!(out, "{said}");
    } else if !effective.restore {
        match hub.forgo_restore() {
            Ok(0) => {
                let _ = writeln!(out, "session restore off (--no-restore / sessions.restore)");
            }
            Ok(n) => {
                let _ = writeln!(
                    out,
                    "session restore off (--no-restore / sessions.restore): {n} session{} stay{} stopped until relaunched",
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
        let targets = login_targets_served(
            local,
            &public_urls,
            tailscale.as_ref(),
            &local_interfaces(local),
            served,
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

    let gate = crate::handoff::Gate::new();
    let handoff_server = {
        use std::os::fd::AsRawFd;
        crate::handoff::HandoffServer::start(crate::handoff::Source {
            hub: Arc::clone(&hub),
            gate: gate.clone(),
            listener_fd: listener.as_raw_fd(),
            listen: local,
            serve_args,
            log_file: args.log_file.clone(),
        })
    };
    let handoff_server = match handoff_server {
        Ok(server) => Some(server),
        Err(e) => {
            let _ = writeln!(
                err,
                "fw hub: live handoff off (`fw hub upgrade` will restart instead): {e}"
            );
            None
        }
    };
    // Written last: `fw hub start` treats it as the hub being ready.
    let runtime_info = RuntimeInfo {
        pid: std::process::id(),
        listen: local,
        public_urls: public_urls.clone(),
        started_at: Some(Utc::now()),
        log_file: args.log_file.clone(),
        tls: match served {
            Served::Http => None,
            Served::Https { ca_page: true } => Some("auto".to_owned()),
            Served::Https { ca_page: false } => Some("files".to_owned()),
        },
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
    let mut allowed_origins = effective.allow_origins.clone();
    allowed_origins.extend(public_urls);
    if let Some(ts) = &tailscale {
        allowed_origins.extend(ts.serve_urls.iter().cloned());
    }
    let flywheel = match (&extensions.flywheel, effective.flywheel) {
        (Some(provider), true) => {
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
    let review_tools = crate::review::Tools::default();
    let fanouts =
        crate::fanout::Fanouts::new(&hub, extensions.graders.clone(), review_tools.clone());
    let fanouts = match fanouts.start() {
        Ok(()) => Some(fanouts),
        Err(e) => {
            let _ = writeln!(err, "fw hub: fan-outs off: {e}");
            None
        }
    };
    // Plugins: local executables on hub events and as palette actions.
    let plugins = Arc::new(crate::plugins::PluginHost::new(
        hub.state_dir(),
        crate::plugins::HostOptions {
            max_concurrent: effective.plugins_max_concurrent,
            base_env: std::env::vars_os().collect(),
            enabled: effective.plugins_enabled,
        },
    ));
    {
        let _enter = runtime.enter();
        plugins.watch(&hub);
    }
    // hub.toml edits: live keys in force at once, the UI told.
    if let Err(e) = config.attach(&hub, Some(Arc::clone(&plugins))) {
        let _ = writeln!(
            err,
            "fw hub: {} will not be re-read while running: {e}",
            config.path().display()
        );
    }
    let server_options = ServerOptions {
        allowed_origins,
        trusted_proxies: effective.trusted_proxies.clone(),
        notifier,
        flywheel,
        review_tools,
        fanouts,
        config: Some(Arc::clone(&config)),
        plugins: Some(plugins),
        tls: tls.clone(),
    };
    if let Some(tls) = &tls {
        let _enter = runtime.enter();
        tls.watch();
    }
    let state_dir = hub.state_dir().to_path_buf();
    let result = runtime.block_on(serve_gated(
        crate::handoff::GatedListener::new(listener, gate),
        Arc::clone(&hub),
        server_options,
        shutdown.wait(),
    ));
    if let Some(server) = &handoff_server {
        server.remove();
    }
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

    /// Every flag and positional of every verb says what it is in
    /// `--help` (a blank line there is a doc comment forgotten).
    #[test]
    fn every_argument_has_help_text() {
        fn walk(cmd: &clap::Command, path: &str, blank: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if id == "help" || id == "version" || arg.is_hide_set() {
                    continue;
                }
                let help = arg.get_help().or(arg.get_long_help());
                if help.is_none_or(|h| h.to_string().trim().is_empty()) {
                    blank.push(format!("{path} {id}"));
                }
            }
            for sub in cmd.get_subcommands() {
                if sub.is_hide_set() {
                    continue;
                }
                walk(sub, &format!("{path} {}", sub.get_name()), blank);
            }
        }
        let mut blank = Vec::new();
        walk(&command(), "fw hub", &mut blank);
        assert!(
            blank.is_empty(),
            "arguments with no help:\n{}",
            blank.join("\n")
        );
    }

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
    fn the_login_code_never_rides_a_plain_http_qr() {
        let targets = crate::remote::login_targets_served(
            "0.0.0.0:7600".parse().unwrap(),
            &[],
            None,
            &[("eth0".into(), "192.168.1.5".parse().unwrap())],
            Served::Https { ca_page: true },
        );
        let lan = targets.iter().find(|t| t.remote).unwrap();
        let qrs = login_qrs(lan, "SECRETCODE");
        let urls: Vec<&str> = qrs.iter().map(|(url, _)| url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "http://192.168.1.5:7600/ca",
                "https://192.168.1.5:7600/login?code=SECRETCODE"
            ]
        );
        for (url, _) in &qrs {
            assert!(
                !url.contains("SECRETCODE") || url.starts_with("https://"),
                "{url}"
            );
        }
        // Without the local CA: one QR, the login itself.
        let plain = crate::remote::login_targets_served(
            "0.0.0.0:7600".parse().unwrap(),
            &[],
            None,
            &[("eth0".into(), "192.168.1.5".parse().unwrap())],
            Served::Http,
        );
        let lan = plain.iter().find(|t| t.remote).unwrap();
        assert_eq!(login_qrs(lan, "C").len(), 1);
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
    fn recording_and_share_flags_reach_the_detached_serve() {
        let cli = HubCli::try_parse_from([
            "fw hub",
            "serve",
            "--record-part-cap",
            "128KiB",
            "--record-dir-cap",
            "1GiB",
            "--share-default-expiry",
            "10m",
            "--share-max-expiry",
            "1d",
        ])
        .unwrap();
        let HubCommand::Serve(args) = cli.command else {
            panic!("not serve");
        };
        let mut again = vec!["fw hub".to_owned(), "serve".to_owned()];
        again.extend(args.hub.to_argv(Path::new("/state")));
        let HubCommand::Serve(reparsed) = HubCli::try_parse_from(again).unwrap().command else {
            panic!("not serve");
        };
        let flags = reparsed.hub.flags();
        assert_eq!(flags.record_part_cap, Some(128 * 1024));
        assert_eq!(flags.record_dir_cap, Some(1 << 30));
        assert_eq!(flags.share_default_expiry, Some(Duration::from_secs(600)));
        assert_eq!(flags.share_max_expiry, Some(Duration::from_secs(86_400)));
        assert!(HubCli::try_parse_from(["fw hub", "serve", "--record-part-cap", "lots"]).is_err());
    }

    #[test]
    fn recordings_and_share_links_have_no_machine() {
        let (code, _, err) = run_args(&["record", "ls", "--machine", "desktop"]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("no --machine"), "{err}");
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
        // `url` refuses --machine, so its help does not offer it.
        let (code, out, _) = run_args(&["url", "--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("--url") && !out.contains("--machine"), "{out}");
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
            tls: None,
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
