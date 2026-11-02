//! `fw hub open`: sign a browser on this machine into the running hub with
//! a fresh one-time code (optionally straight onto a session) and open it.
//!
//! The code never goes on an opener's command line (any local user can
//! read `/proc/<pid>/cmdline`, and a browser keeps its argv for life): the
//! opener is given a 0600 redirect page in the state dir instead, which
//! sends the browser on to the login URL (Jupyter's approach).
//!
//! The opener: `$BROWSER` when set; on WSL `wslview`, else Windows'
//! `cmd.exe /c start` (given the page's `\\wsl.localhost\...` path), else
//! `xdg-open`; on macOS `open`; elsewhere `xdg-open`. Only an opener that
//! cannot be started is skipped for the next: one that ran and failed may
//! still have opened a tab, and a second would only find the code spent.
//! The URL is always printed too, so a failed opener still leaves
//! something to click.

use crate::auth::{LOGIN_CODE_TTL, write_private};
use crate::cli::{StateDirArg, browse_host, resolve_state_dir};
use crate::script::{HubConn, resolve};
use crate::server::code_login_url;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How long an opener may take before it is left running and taken as
/// having worked (`xdg-open` can stay until the browser exits).
const OPENER_WAIT: Duration = Duration::from_secs(10);
/// Where WSL keeps Windows' `cmd.exe` when Windows' PATH is not appended.
const WSL_CMD: &str = "/mnt/c/Windows/System32/cmd.exe";
/// Redirect pages are `open-<id>.html` in the state dir.
const PAGE_PREFIX: &str = "open-";
const PAGE_SUFFIX: &str = ".html";

#[derive(Debug, clap::Args)]
pub(crate) struct OpenArgs {
    /// Open straight onto this session (id, unique id prefix, or name).
    #[arg(long)]
    session: Option<String>,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn open(args: &OpenArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let prepared = resolve_state_dir(&args.state).and_then(|state_dir| {
        let url = login_url(&state_dir, args)?;
        Ok((state_dir, url))
    });
    let (state_dir, url) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    let _ = writeln!(out, "{url}");
    let _ = out.flush();
    let page = match write_redirect_page(&state_dir, &url) {
        Ok(page) => page,
        Err(e) => {
            let _ = writeln!(
                err,
                "fw hub: cannot write the sign-in page in {}: {e}; open the URL above",
                state_dir.display()
            );
            return 1;
        }
    };
    let host = Host::current();
    let windows_page = if host.wsl { windows_path(&page) } else { None };
    let candidates = openers(&page, windows_page.as_deref(), &host);
    if candidates.is_empty() {
        let _ = writeln!(
            err,
            "fw hub: no way to open a browser here (set BROWSER, or install xdg-open{}); open the URL above",
            if host.wsl { " or wslview" } else { "" }
        );
        return 1;
    }
    let mut unstartable = Vec::new();
    for argv in &candidates {
        match run_opener(argv) {
            Ok(()) => return 0,
            Err(OpenerError::NotStarted(e)) => unstartable.push(format!("{}: {e}", argv[0])),
            Err(OpenerError::Failed(e)) => {
                let _ = writeln!(
                    err,
                    "fw hub: {} {e} (it may still have opened a tab); otherwise open the URL above",
                    argv[0]
                );
                return 1;
            }
        }
    }
    let _ = writeln!(
        err,
        "fw hub: could not start a browser ({}); open the URL above",
        unstartable.join("; ")
    );
    1
}

/// Write the page that forwards a browser to `url`: 0600 in `state_dir`
/// (a private directory), under a fresh random name. Pages from earlier
/// runs whose codes have expired are removed first.
pub(crate) fn write_redirect_page(state_dir: &Path, url: &str) -> std::io::Result<PathBuf> {
    remove_expired_pages(state_dir);
    let page = state_dir.join(format!(
        "{PAGE_PREFIX}{}{PAGE_SUFFIX}",
        crate::state::new_id()
    ));
    let href = html_escape(url);
    let script_url = serde_json::to_string(url).unwrap_or_default();
    let html = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={href}\">\n\
         <title>Opening fw hub</title></head>\n\
         <body><p>Opening <a href=\"{href}\">fw hub</a>...</p>\n\
         <script>location.replace({script_url});</script></body></html>\n"
    );
    write_private(&page, html.as_bytes())?;
    Ok(page)
}

fn remove_expired_pages(state_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(PAGE_PREFIX) && name.ends_with(PAGE_SUFFIX)) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > LOGIN_CODE_TTL);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `path` as a `file://` URL (bytes outside the unreserved set and `/`
/// percent-escaped).
pub(crate) fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// The Windows (`\\wsl.localhost\<distro>\...`) path of a WSL file, from
/// `wslpath -w`.
fn windows_path(path: &Path) -> Option<String> {
    let output = Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

/// A one-time login URL for the hub on this machine (loopback when it
/// listens everywhere), onto `--session` when given.
fn login_url(state_dir: &Path, args: &OpenArgs) -> Result<String, String> {
    let conn = HubConn::connect(state_dir)?;
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

/// The commands to try, in order, to open the redirect `page` (and, on
/// WSL, its Windows path for `cmd.exe`).
pub(crate) fn openers(page: &Path, windows_page: Option<&str>, host: &Host) -> Vec<Vec<String>> {
    let url = file_url(page);
    let mut candidates = Vec::new();
    // `$BROWSER`: a `:`-separated list of commands, each taking the URL as
    // its last argument or at `%s`.
    if let Some(browser) = &host.browser {
        for command in browser.split(':').filter(|c| !c.trim().is_empty()) {
            let mut argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
            if argv.iter().any(|a| a.contains("%s")) {
                for arg in &mut argv {
                    *arg = arg.replace("%s", &url);
                }
            } else {
                argv.push(url.clone());
            }
            candidates.push(argv);
        }
    }
    if host.wsl {
        // wslview translates a Linux path for Windows itself.
        if let Some(wslview) = host.has("wslview") {
            candidates.push(vec![wslview, page.to_string_lossy().into_owned()]);
        }
        if let (Some(cmd), Some(windows_page)) = (
            host.has("cmd.exe").or_else(|| {
                host.wsl_cmd
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
            }),
            windows_page,
        ) {
            // `start`'s first quoted argument is a window title, so the
            // path goes unquoted -- with cmd's metacharacters escaped.
            candidates.push(vec![
                cmd,
                "/c".into(),
                "start".into(),
                cmd_escape(windows_page),
            ]);
        }
    }
    if host.macos
        && let Some(open) = host.has("open")
    {
        candidates.push(vec![open, url.clone()]);
    }
    if let Some(xdg) = host.has("xdg-open") {
        candidates.push(vec![xdg, url]);
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

/// Why an opener did not work.
#[derive(Debug)]
enum OpenerError {
    /// It could not be started (not installed, not executable): the next
    /// may be tried.
    NotStarted(String),
    /// It ran and reported failure.
    Failed(String),
}

/// Run one opener: success when it exits 0, or is still running after
/// [`OPENER_WAIT`] (it is left to finish).
fn run_opener(argv: &[String]) -> Result<(), OpenerError> {
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
    let mut child = command
        .spawn()
        .map_err(|e| OpenerError::NotStarted(e.to_string()))?;
    let deadline = Instant::now() + OPENER_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(OpenerError::Failed(format!("exited with {status}"))),
            Ok(None) if Instant::now() >= deadline => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(OpenerError::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "/state/open-ab.html";
    const WIN: &str = r"\\wsl.localhost\Ubuntu\state\open-ab.html";
    const URL: &str = "file:///state/open-ab.html";

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

    fn page() -> &'static Path {
        Path::new(PAGE)
    }

    #[test]
    fn wsl_prefers_wslview_then_cmd_then_xdg_open() {
        let all = openers(
            page(),
            Some(WIN),
            &host(true, &["wslview", "cmd.exe", "xdg-open"]),
        );
        assert_eq!(all[0], ["/bin/wslview", PAGE]);
        assert_eq!(all[1], ["/bin/cmd.exe", "/c", "start", WIN]);
        assert_eq!(all[2], ["/bin/xdg-open", URL]);
        // cmd.exe off PATH: its usual mount.
        let mut mounted = host(true, &[]);
        mounted.wsl_cmd = Some(PathBuf::from(WSL_CMD));
        assert_eq!(openers(page(), Some(WIN), &mounted)[0][0], WSL_CMD);
        // No Windows path for the page: cmd.exe cannot be given it.
        assert!(openers(page(), None, &mounted).is_empty());
        // Not WSL: Windows openers are never tried.
        assert_eq!(
            openers(
                page(),
                Some(WIN),
                &host(false, &["wslview", "cmd.exe", "xdg-open"])
            ),
            [vec!["/bin/xdg-open".to_owned(), URL.to_owned()]]
        );
        assert!(openers(page(), None, &host(false, &[])).is_empty());
    }

    #[test]
    fn browser_variable_comes_first_with_its_placeholder() {
        let mut h = host(false, &["xdg-open"]);
        h.browser = Some("firefox --new-tab:chromium --app=%s".into());
        let all = openers(page(), None, &h);
        assert_eq!(all[0], ["firefox", "--new-tab", URL]);
        assert_eq!(all[1], ["chromium".to_owned(), format!("--app={URL}")]);
        assert_eq!(all[2][0], "/bin/xdg-open");
    }

    #[test]
    fn the_login_code_never_reaches_an_openers_argv() {
        let dir = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:7420/login?code=SECRETCODE&x=1#session=ff";
        let page = write_redirect_page(dir.path(), url).unwrap();
        let mut h = host(true, &["wslview", "cmd.exe", "xdg-open"]);
        h.browser = Some("firefox".into());
        for argv in openers(&page, Some(r"\\wsl.localhost\U\p.html"), &h) {
            assert!(argv.iter().all(|a| !a.contains("SECRETCODE")), "{argv:?}");
        }
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(
            html.contains("url=http://127.0.0.1:7420/login?code=SECRETCODE&amp;x=1#session=ff")
        );
        assert!(html.contains(
            r#"location.replace("http://127.0.0.1:7420/login?code=SECRETCODE&x=1#session=ff")"#
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&page).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn expired_redirect_pages_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("open-old.html");
        std::fs::write(&old, "x").unwrap();
        let stale = SystemTime::now() - LOGIN_CODE_TTL - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        let other = dir.path().join("state.json");
        std::fs::write(&other, "{}").unwrap();
        let fresh = write_redirect_page(dir.path(), "http://x/").unwrap();
        assert!(!old.exists());
        assert!(fresh.exists() && other.exists());
    }

    #[test]
    fn file_urls_escape_the_path() {
        assert_eq!(
            file_url(Path::new("/home/a b/open-1.html")),
            "file:///home/a%20b/open-1.html"
        );
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
    fn only_an_opener_that_cannot_start_falls_through() {
        assert!(run_opener(&["true".into()]).is_ok());
        assert!(matches!(
            run_opener(&["false".into()]),
            Err(OpenerError::Failed(e)) if e.contains("exited")
        ));
        assert!(matches!(
            run_opener(&["/definitely/not/a/program".into()]),
            Err(OpenerError::NotStarted(_))
        ));
    }
}
