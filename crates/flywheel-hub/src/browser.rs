//! `fw hub open`: sign a browser on this machine into the running hub with
//! a fresh one-time code (optionally straight onto a session) and open it.
//!
//! The opener: `$BROWSER` when set; on WSL `wslview`, else Windows'
//! `cmd.exe /c start`, else `xdg-open`; on macOS `open`; elsewhere
//! `xdg-open`. The URL is always printed too, so a failed opener still
//! leaves something to click.

use crate::cli::{StateDirArg, browse_host, resolve_state_dir};
use crate::script::{HubConn, resolve};
use crate::server::code_login_url;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long an opener may take before it is left running and taken as
/// having worked (`xdg-open` can stay until the browser exits).
const OPENER_WAIT: Duration = Duration::from_secs(10);
/// Where WSL keeps Windows' `cmd.exe` when Windows' PATH is not appended.
const WSL_CMD: &str = "/mnt/c/Windows/System32/cmd.exe";

#[derive(Debug, clap::Args)]
pub(crate) struct OpenArgs {
    /// Open straight onto this session (id, unique id prefix, or name).
    #[arg(long)]
    session: Option<String>,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn open(args: &OpenArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let url = match login_url(args) {
        Ok(url) => url,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    let _ = writeln!(out, "{url}");
    let _ = out.flush();
    let host = Host::current();
    let candidates = openers(&url, &host);
    if candidates.is_empty() {
        let _ = writeln!(
            err,
            "fw hub: no way to open a browser here (set BROWSER, or install xdg-open{}); open the URL above",
            if host.wsl { " or wslview" } else { "" }
        );
        return 1;
    }
    let mut failures = Vec::new();
    for argv in &candidates {
        match run_opener(argv) {
            Ok(()) => return 0,
            Err(e) => failures.push(format!("{}: {e}", argv[0])),
        }
    }
    let _ = writeln!(
        err,
        "fw hub: could not open a browser ({}); open the URL above",
        failures.join("; ")
    );
    1
}

/// A one-time login URL for the hub on this machine (loopback when it
/// listens everywhere), onto `--session` when given.
fn login_url(args: &OpenArgs) -> Result<String, String> {
    let conn = HubConn::connect(&resolve_state_dir(&args.state)?)?;
    let fragment = match &args.session {
        Some(query) => {
            let sessions = conn.sessions()?;
            let session = resolve(&sessions, query, "session")?;
            let id = session.get("id").and_then(Value::as_str).unwrap_or("");
            format!("#session={id}")
        }
        None => String::new(),
    };
    let minted = conn.call("POST", "/api/login-codes", None)?;
    let code = minted
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "the running hub sent no login code".to_owned())?;
    let base = format!("http://{}", browse_host(conn.info.listen));
    Ok(format!("{}{fragment}", code_login_url(&base, code)))
}

/// What choosing an opener looks at.
#[derive(Debug, Clone, Default)]
pub(crate) struct Host {
    /// `$BROWSER`.
    browser: Option<String>,
    /// Running under WSL.
    wsl: bool,
    macos: bool,
    /// Programs found on `PATH` (by name), as resolved paths.
    found: Vec<(String, PathBuf)>,
    /// `cmd.exe` at its usual WSL mount when not on `PATH`.
    wsl_cmd: Option<PathBuf>,
}

impl Host {
    fn current() -> Self {
        let wsl = is_wsl();
        let found = ["wslview", "cmd.exe", "xdg-open", "open"]
            .into_iter()
            .filter_map(|name| crate::agent::which(name).map(|path| (name.to_owned(), path)))
            .collect();
        Self {
            browser: std::env::var("BROWSER")
                .ok()
                .filter(|b| !b.trim().is_empty()),
            wsl,
            macos: cfg!(target_os = "macos"),
            found,
            wsl_cmd: (wsl && Path::new(WSL_CMD).is_file()).then(|| PathBuf::from(WSL_CMD)),
        }
    }

    fn has(&self, name: &str) -> Option<String> {
        self.found
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, path)| path.to_string_lossy().into_owned())
    }
}

/// Running under Windows Subsystem for Linux.
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// The commands to try, in order, to open `url`.
pub(crate) fn openers(url: &str, host: &Host) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();
    // `$BROWSER`: a `:`-separated list of commands, each taking the URL as
    // its last argument or at `%s`.
    if let Some(browser) = &host.browser {
        for command in browser.split(':').filter(|c| !c.trim().is_empty()) {
            let mut argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
            if argv.iter().any(|a| a.contains("%s")) {
                for arg in &mut argv {
                    *arg = arg.replace("%s", url);
                }
            } else {
                argv.push(url.to_owned());
            }
            candidates.push(argv);
        }
    }
    if host.wsl {
        if let Some(wslview) = host.has("wslview") {
            candidates.push(vec![wslview, url.to_owned()]);
        }
        if let Some(cmd) = host.has("cmd.exe").or_else(|| {
            host.wsl_cmd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
        }) {
            // `start`'s first quoted argument is a window title, so the URL
            // goes unquoted -- with cmd's metacharacters escaped.
            candidates.push(vec![cmd, "/c".into(), "start".into(), cmd_escape(url)]);
        }
    }
    if host.macos
        && let Some(open) = host.has("open")
    {
        candidates.push(vec![open, url.to_owned()]);
    }
    if let Some(xdg) = host.has("xdg-open") {
        candidates.push(vec![xdg, url.to_owned()]);
    }
    candidates
}

/// `text` safe as one unquoted `cmd.exe` word: `& | < > ^ ( ) %` and `!`
/// caret-escaped, so `a=1&b=2` reaches `start` whole instead of running
/// `b=2` as a second command.
pub(crate) fn cmd_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!') {
            out.push('^');
        }
        out.push(c);
    }
    out
}

/// Run one opener: success when it exits 0, or is still running after
/// [`OPENER_WAIT`] (it is left to finish).
fn run_opener(argv: &[String]) -> Result<(), String> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // cmd.exe warns about (and cannot start in) a Linux working directory.
    if Path::new(&argv[0])
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe"))
        && Path::new("/mnt/c").is_dir()
    {
        command.current_dir("/mnt/c");
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + OPENER_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("exited with {status}")),
            Ok(None) if Instant::now() >= deadline => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:7420/login?code=ABC&x=1#session=ff";

    fn host(wsl: bool, found: &[&str]) -> Host {
        Host {
            wsl,
            found: found
                .iter()
                .map(|n| ((*n).to_owned(), PathBuf::from(format!("/bin/{n}"))))
                .collect(),
            ..Host::default()
        }
    }

    #[test]
    fn wsl_prefers_wslview_then_cmd_then_xdg_open() {
        let all = openers(URL, &host(true, &["wslview", "cmd.exe", "xdg-open"]));
        assert_eq!(all[0], ["/bin/wslview", URL]);
        assert_eq!(
            all[1],
            [
                "/bin/cmd.exe",
                "/c",
                "start",
                "http://127.0.0.1:7420/login?code=ABC^&x=1#session=ff"
            ]
        );
        assert_eq!(all[2], ["/bin/xdg-open", URL]);
        // cmd.exe off PATH: its usual mount.
        let mut mounted = host(true, &[]);
        mounted.wsl_cmd = Some(PathBuf::from(WSL_CMD));
        assert_eq!(openers(URL, &mounted)[0][0], WSL_CMD);
        // Not WSL: Windows openers are never tried.
        assert_eq!(
            openers(URL, &host(false, &["wslview", "cmd.exe", "xdg-open"])),
            [vec!["/bin/xdg-open".to_owned(), URL.to_owned()]]
        );
        assert!(openers(URL, &host(false, &[])).is_empty());
    }

    #[test]
    fn browser_variable_comes_first_with_its_placeholder() {
        let mut h = host(false, &["xdg-open"]);
        h.browser = Some("firefox --new-tab:chromium --app=%s".into());
        let all = openers(URL, &h);
        assert_eq!(all[0], ["firefox", "--new-tab", URL]);
        assert_eq!(all[1], ["chromium".to_owned(), format!("--app={URL}")]);
        assert_eq!(all[2][0], "/bin/xdg-open");
    }

    #[test]
    fn cmd_metacharacters_are_caret_escaped() {
        assert_eq!(
            cmd_escape("a&b|c<d>e^f(g)h%i!j"),
            "a^&b^|c^<d^>e^^f^(g^)h^%i^!j"
        );
        assert_eq!(cmd_escape("plain/url?x=1"), "plain/url?x=1");
    }

    #[test]
    fn openers_succeed_on_exit_zero_and_report_failures() {
        assert!(run_opener(&["true".into()]).is_ok());
        assert!(
            run_opener(&["false".into()])
                .unwrap_err()
                .contains("exited")
        );
        assert!(run_opener(&["/definitely/not/a/program".into()]).is_err());
    }
}
