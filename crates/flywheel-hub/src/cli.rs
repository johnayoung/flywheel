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

mod login;
mod serve;
#[allow(clippy::wildcard_imports)]
pub(crate) use login::*;
use serve::*;

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
    /// stopped and come back on the next start. Exit code 3 when no hub
    /// runs.
    Stop(lifecycle::StopArgs),
    /// Replace the running hub with a new process (binary) without ending
    /// any session: a live handoff of every terminal, its screen, and its
    /// state. Exit codes: 0 handed off (or restarted with
    /// --allow-restart), 1 failed (the hub carries on or was rolled back),
    /// 3 no hub running, 4 refused as incompatible (nothing changed), 5
    /// busy (nothing changed; retry).
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
    /// What happened while you were away: turns finished, agents blocked
    /// now, fan-outs graded, commands suggested, cost; then the activity
    /// timeline (this hub and its machines), newest first.
    Activity(crate::activity_cli::ActivityArgs),
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
    /// Agent permission rules for a folder or session: show every Claude
    /// Code and Codex source with where each rule comes from, allow a
    /// narrow rule (previewed, confirmed, dangerous ones typed back),
    /// suggest rules for the prompts a repo blocks on most, set Codex's
    /// trust and approval settings.
    Perms(crate::perms_cli::PermsArgs),
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
    /// An extra name for the local CA and its certificate: a private DNS
    /// name (under .local, .lan, .home.arpa, .internal, .localhost, or
    /// <tailnet>.ts.net), a private IP address, or a private IP network the
    /// CA may sign for (e.g. 192.168.0.0/16, so a new DHCP lease is
    /// covered); repeatable. Names added after the CA was made need `fw hub
    /// tls rotate`.
    #[arg(long = "tls-name", value_name = "NAME", value_parser = crate::tls::parse_name)]
    tls_names: Vec<crate::tls::NameSpec>,
    /// Allow this public --tls-name (a domain or address you own): every
    /// device that installs the CA then trusts this machine's CA key for
    /// it; `fw hub tls ca` and /ca say so. Repeatable, one per name.
    #[arg(long = "tls-allow-public-name", value_name = "NAME", value_parser = crate::tls::parse_name)]
    tls_public_names: Vec<crate::tls::NameSpec>,
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
        for name in &self.tls_public_names {
            argv.extend(["--tls-allow-public-name".to_owned(), name.to_string()]);
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
            tls_public_names: self.tls_public_names.clone(),
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
    // `serve`'s stderr is the daemon's log: never silenced.
    if matches!(cli.command, HubCommand::Serve(_)) {
        return dispatch(cli.command, out, err, extensions);
    }
    crate::util::quiet_on_closed_pipe(out, err, |out, err| {
        dispatch(cli.command, out, err, extensions)
    })
}

/// Passes stderr through, noting whether a verb said no hub is running
/// (only the start of the stream is kept to look in).
struct NoHubWatch<'a> {
    inner: &'a mut dyn Write,
    seen: Vec<u8>,
}

impl NoHubWatch<'_> {
    const KEEP: usize = 64 * 1024;

    fn said_no_hub(&self) -> bool {
        let needle = crate::endpoint::NO_HUB_PREFIX.as_bytes();
        self.seen.windows(needle.len()).any(|w| w == needle)
    }
}

impl Write for NoHubWatch<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let room = Self::KEEP.saturating_sub(self.seen.len());
        self.seen.extend_from_slice(&buf[..n.min(room)]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Run a verb; a failure (exit 1) that said no hub is running exits
/// [`crate::endpoint::EXIT_NOT_RUNNING`] instead, whichever verb it was,
/// so a script can tell "no hub" from a real failure.
fn dispatch(
    command: HubCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
    extensions: &HubExtensions,
) -> i32 {
    let mut watch = NoHubWatch {
        inner: err,
        seen: Vec::new(),
    };
    let code = dispatch_verb(command, out, &mut watch, extensions);
    if code == 1 && watch.said_no_hub() {
        crate::endpoint::EXIT_NOT_RUNNING
    } else {
        code
    }
}

fn dispatch_verb(
    command: HubCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
    extensions: &HubExtensions,
) -> i32 {
    match command {
        HubCommand::Token(args) => token(&args, out, err),
        HubCommand::Tls(args) => crate::tls_cli::main(&args, out, err),
        HubCommand::Url(args) => url_cmd(&args, out, err),
        HubCommand::Serve(args) => serve_cmd(args, out, err, extensions),
        HubCommand::Commands(args) => crate::commands_cli::run(&args, out, err),
        HubCommand::Search(args) => crate::search_cli::run(&args, out, err),
        HubCommand::Activity(args) => crate::activity_cli::run(&args, out, err),
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
        HubCommand::Perms(args) => crate::perms_cli::main(&args, out, err),
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

    /// Every verb that needs the running hub and finds none exits 3 with
    /// the one wording, client verbs included; a verb failing otherwise
    /// keeps exit 1.
    #[test]
    fn no_hub_is_exit_three_for_every_verb_that_needs_one() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_str().unwrap();
        for verb in [
            &["status"][..],
            &["stop"],
            &["upgrade"],
            &["url"],
            &["ls"],
            &["open"],
            &["read", "x"],
            &["send", "x", "hi"],
            &["kill", "x"],
            &["activity"],
        ] {
            let mut args = verb.to_vec();
            args.extend(["--state-dir", state]);
            let (code, out, err) = run_args(&args);
            let said = format!("{out}{err}");
            assert_eq!(code, crate::endpoint::EXIT_NOT_RUNNING, "{verb:?}: {said}");
            assert!(
                said.contains(crate::endpoint::NO_HUB_PREFIX),
                "{verb:?}: {said}"
            );
        }
        // Offline verbs succeed and say so; failures that are not "no
        // hub" keep exit 1.
        let (code, out, _) = run_args(&["config", "show", "--state-dir", state]);
        assert_eq!(code, 0);
        assert!(out.contains(crate::endpoint::NO_HUB_PREFIX), "{out}");
        let mut sink = Vec::new();
        let mut watch = NoHubWatch {
            inner: &mut sink,
            seen: Vec::new(),
        };
        let _ = writeln!(watch, "fw hub: something else failed");
        assert!(!watch.said_no_hub());
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
