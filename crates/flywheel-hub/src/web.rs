//! The embedded web app: every asset is compiled into the binary, so a hub
//! is a single static executable with no build step and no files on disk.
//!
//! Vendored third-party code lives under `web/vendor/<package>/` with its
//! license file (xterm.js 6.0.0, @xterm/addon-fit 0.11.0,
//! @xterm/addon-web-links 0.12.0, @xterm/addon-search 0.16.0; sourcemap
//! comments stripped) and the
//! UI fonts under `web/vendor/fonts/` (Big Shoulders Display, Martian Mono,
//! JetBrains Mono: latin variable subsets from @fontsource-variable 5.3.0,
//! SIL OFL 1.1).
//!
//! The PWA icons (`icon-192.png`, `icon-512.png`, `icon-maskable-512.png`,
//! `apple-touch-icon.png`) are `icon.svg`'s wheel rasterized by headless
//! Chromium: rounded for `any`, full bleed (wheel inside the 80% safe
//! circle) for `maskable` and iOS.

/// The app shell (served at `/` once logged in).
pub const INDEX_HTML: &str = include_str!("../web/index.html");

/// Answer to a successful login: `login.js` replaces it with `/` (keeping
/// the URL fragment) from this origin; without scripts, a meta refresh.
pub const LOGGED_IN_HTML: &str = "<!doctype html><html lang=\"en\" data-signed-in><head><meta charset=\"utf-8\">\
<title>flywheel hub</title><script src=\"/assets/login.js\"></script>\
<noscript><meta http-equiv=\"refresh\" content=\"0;url=/\"></noscript></head>\
<body><p>Signed in. <a href=\"/\">Continue to the hub</a>.</p></body></html>";

/// The web app manifest (served at `/manifest.webmanifest`).
pub const MANIFEST: &str = include_str!("../web/manifest.webmanifest");

/// The service worker's source; [`service_worker`] stamps it.
const SERVICE_WORKER: &str = include_str!("../web/sw.js");

/// Header the hub sets on the signed-in app page: the only page the
/// service worker keeps as the offline app shell.
pub const SHELL_HEADER: &str = "x-fw-hub-shell";

/// The service worker, stamped with [`build_id`] (a new binary's worker
/// replaces the old one and drops its cache).
pub fn service_worker() -> String {
    SERVICE_WORKER.replace("__FW_HUB_BUILD__", build_id())
}

/// A digest of every embedded page and asset.
pub fn build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(INDEX_HTML.as_bytes());
        digest.update(MANIFEST.as_bytes());
        for (path, bytes, _) in ASSETS {
            digest.update(path.as_bytes());
            digest.update(bytes);
        }
        digest.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

const JS: &str = "text/javascript; charset=utf-8";

/// Every embedded asset under `/assets/`: `(path, bytes, content type)`.
const ASSETS: &[(&str, &[u8], &str)] = &[
    ("app.js", include_bytes!("../web/app.js"), JS),
    ("app-base.js", include_bytes!("../web/app-base.js"), JS),
    ("app-layout.js", include_bytes!("../web/app-layout.js"), JS),
    ("app-stage.js", include_bytes!("../web/app-stage.js"), JS),
    ("app-tabs.js", include_bytes!("../web/app-tabs.js"), JS),
    ("app-events.js", include_bytes!("../web/app-events.js"), JS),
    (
        "app-sidebar.js",
        include_bytes!("../web/app-sidebar.js"),
        JS,
    ),
    (
        "app-dialogs.js",
        include_bytes!("../web/app-dialogs.js"),
        JS,
    ),
    (
        "app-keyboard.js",
        include_bytes!("../web/app-keyboard.js"),
        JS,
    ),
    ("attention.js", include_bytes!("../web/attention.js"), JS),
    ("flywheel.js", include_bytes!("../web/flywheel.js"), JS),
    ("fanout.js", include_bytes!("../web/fanout.js"), JS),
    ("mobile.js", include_bytes!("../web/mobile.js"), JS),
    ("notify.js", include_bytes!("../web/notify.js"), JS),
    ("pwa.js", include_bytes!("../web/pwa.js"), JS),
    (
        "icon-192.png",
        include_bytes!("../web/icon-192.png"),
        "image/png",
    ),
    (
        "icon-512.png",
        include_bytes!("../web/icon-512.png"),
        "image/png",
    ),
    (
        "icon-maskable-512.png",
        include_bytes!("../web/icon-maskable-512.png"),
        "image/png",
    ),
    (
        "apple-touch-icon.png",
        include_bytes!("../web/apple-touch-icon.png"),
        "image/png",
    ),
    ("commands.js", include_bytes!("../web/commands.js"), JS),
    ("import.js", include_bytes!("../web/import.js"), JS),
    ("worktree.js", include_bytes!("../web/worktree.js"), JS),
    ("fmt.js", include_bytes!("../web/fmt.js"), JS),
    ("launcher.js", include_bytes!("../web/launcher.js"), JS),
    ("review.js", include_bytes!("../web/review.js"), JS),
    ("explain.js", include_bytes!("../web/explain.js"), JS),
    ("layout.js", include_bytes!("../web/layout.js"), JS),
    ("machines.js", include_bytes!("../web/machines.js"), JS),
    ("panes.js", include_bytes!("../web/panes.js"), JS),
    ("search.js", include_bytes!("../web/search.js"), JS),
    ("settings.js", include_bytes!("../web/settings.js"), JS),
    ("sharing.js", include_bytes!("../web/sharing.js"), JS),
    ("term-view.js", include_bytes!("../web/term-view.js"), JS),
    ("viewer.js", include_bytes!("../web/viewer.js"), JS),
    ("player.js", include_bytes!("../web/player.js"), JS),
    (
        "viewer.css",
        include_bytes!("../web/viewer.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-sharing.css",
        include_bytes!("../web/app-sharing.css"),
        "text/css; charset=utf-8",
    ),
    ("login.js", include_bytes!("../web/login.js"), JS),
    (
        "app.css",
        include_bytes!("../web/app.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-sidebar.css",
        include_bytes!("../web/app-sidebar.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-stage.css",
        include_bytes!("../web/app-stage.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-dialogs.css",
        include_bytes!("../web/app-dialogs.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-phone.css",
        include_bytes!("../web/app-phone.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-worktrees.css",
        include_bytes!("../web/app-worktrees.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-drawers.css",
        include_bytes!("../web/app-drawers.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-search.css",
        include_bytes!("../web/app-search.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-machines.css",
        include_bytes!("../web/app-machines.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-settings.css",
        include_bytes!("../web/app-settings.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-priority.css",
        include_bytes!("../web/app-priority.css"),
        "text/css; charset=utf-8",
    ),
    (
        "app-fanout.css",
        include_bytes!("../web/app-fanout.css"),
        "text/css; charset=utf-8",
    ),
    (
        "icon.svg",
        include_bytes!("../web/icon.svg"),
        "image/svg+xml",
    ),
    (
        "vendor/fonts/big-shoulders-display.woff2",
        include_bytes!("../web/vendor/fonts/big-shoulders-display.woff2"),
        "font/woff2",
    ),
    (
        "vendor/fonts/martian-mono.woff2",
        include_bytes!("../web/vendor/fonts/martian-mono.woff2"),
        "font/woff2",
    ),
    (
        "vendor/fonts/jetbrains-mono.woff2",
        include_bytes!("../web/vendor/fonts/jetbrains-mono.woff2"),
        "font/woff2",
    ),
    (
        "vendor/xterm/xterm.js",
        include_bytes!("../web/vendor/xterm/xterm.js"),
        JS,
    ),
    (
        "vendor/xterm/xterm.css",
        include_bytes!("../web/vendor/xterm/xterm.css"),
        "text/css; charset=utf-8",
    ),
    (
        "vendor/addon-fit/addon-fit.js",
        include_bytes!("../web/vendor/addon-fit/addon-fit.js"),
        JS,
    ),
    (
        "vendor/addon-web-links/addon-web-links.js",
        include_bytes!("../web/vendor/addon-web-links/addon-web-links.js"),
        JS,
    ),
    (
        "vendor/addon-search/addon-search.js",
        include_bytes!("../web/vendor/addon-search/addon-search.js"),
        JS,
    ),
];

/// Look up an embedded asset by its path under `/assets/`.
pub fn asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    ASSETS
        .iter()
        .find(|(name, _, _)| *name == path)
        .map(|(_, bytes, content_type)| (*bytes, *content_type))
}

/// Assets served without the token: what the login page uses -- the fonts,
/// the icons (the manifest, fetched without cookies, names them), and
/// `login.js` (no app code).
pub fn is_public_asset(path: &str) -> bool {
    matches!(
        path,
        "icon.svg"
            | "login.js"
            | "icon-192.png"
            | "icon-512.png"
            | "icon-maskable-512.png"
            | "apple-touch-icon.png"
    ) || path
        .strip_prefix("vendor/fonts/")
        .is_some_and(|f| f.ends_with(".woff2") && !f.contains('/'))
}

/// What a share link's viewer page loads under
/// `/share/<credential>/assets/`: its own script and styles and the
/// terminal emulator -- none of the app's code.
pub fn is_viewer_asset(path: &str) -> bool {
    matches!(
        path,
        "viewer.js"
            | "term-view.js"
            | "viewer.css"
            | "vendor/xterm/xterm.js"
            | "vendor/xterm/xterm.css"
    )
}

/// The page a share link opens: a terminal, its title and status, nothing
/// else. `credential` is a validated share credential (hex and `_`).
pub fn viewer_page(credential: &str) -> String {
    let base = format!("/share/{}/assets", escape_html(credential));
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="dark light">
<meta name="robots" content="noindex, nofollow">
<title>Shared terminal - flywheel hub</title>
<link rel="icon" type="image/svg+xml" href="/assets/icon.svg">
<link rel="stylesheet" href="{base}/vendor/xterm/xterm.css">
<link rel="stylesheet" href="{base}/viewer.css">
<script src="{base}/vendor/xterm/xterm.js" defer></script>
<script src="{base}/term-view.js" defer></script>
<script src="{base}/viewer.js" defer></script>
</head>
<body class="vw">
<header class="vw-head">
  <div class="vw-titles">
    <span class="vw-kicker">flywheel hub &middot; <b>read-only view</b></span>
    <h1 id="vw-title">Shared terminal</h1>
  </div>
  <div class="vw-pills">
    <span id="vw-status" class="vw-pill" hidden></span>
    <span id="vw-link" class="vw-pill" data-link="connecting">connecting</span>
  </div>
</header>
<main id="vw-stage" class="vw-stage">
  <div id="vw-term" class="vw-term"></div>
  <div id="vw-note" class="vw-note" role="status" hidden></div>
</main>
<footer class="vw-foot">
  <span>Watching a live terminal: you cannot type into it.</span>
  <span id="vw-meta" class="vw-meta"></span>
</footer>
</body>
</html>
"##
    )
}

/// A share link that opens nothing (invalid, revoked, expired).
pub fn share_refused_page(message: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<meta name="robots" content="noindex, nofollow">
<title>Share link - flywheel hub</title>
<style>
  :root {{ color-scheme: dark; --bg: #0c0d0c; --ink: #ece6d6; --ink-2: #a8a291; --signal: #ff5a1f; }}
  @media (prefers-color-scheme: light) {{ :root {{ color-scheme: light; --bg: #f2eee4; --ink: #1b1c18; --ink-2: #57544b; --signal: #e2470c; }} }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--ink);
         font: 14px/1.6 ui-monospace, monospace; }}
  main {{ max-width: 32rem; padding: 2rem; border-top: 4px solid var(--signal); }}
  p {{ color: var(--ink-2); }}
</style>
</head>
<body>
<main>
  <h1>{}</h1>
  <p>Ask whoever shared it for a new link.</p>
</main>
</body>
</html>
"##,
        escape_html(message)
    )
}

/// The replay player (served at `/replay#<recording>` to a signed-in
/// browser).
pub const PLAYER_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="dark light">
<title>Replay - flywheel hub</title>
<link rel="icon" type="image/svg+xml" href="/assets/icon.svg">
<link rel="stylesheet" href="/assets/vendor/xterm/xterm.css">
<link rel="stylesheet" href="/assets/viewer.css">
<script src="/assets/vendor/xterm/xterm.js" defer></script>
<script src="/assets/term-view.js" defer></script>
<script src="/assets/player.js" defer></script>
</head>
<body class="vw pl">
<header class="vw-head">
  <div class="vw-titles">
    <span class="vw-kicker">flywheel hub &middot; <b>replay</b></span>
    <h1 id="vw-title">Recording</h1>
  </div>
  <div class="vw-pills">
    <a class="vw-pill" id="pl-download" href="#" download>download .cast</a>
    <a class="vw-pill" href="/">hub</a>
  </div>
</header>
<main id="vw-stage" class="vw-stage">
  <div id="vw-term" class="vw-term"></div>
  <div id="vw-note" class="vw-note" role="status" hidden></div>
</main>
<footer class="pl-bar">
  <button type="button" id="pl-play" class="pl-btn" disabled>Play</button>
  <input type="range" id="pl-seek" class="pl-seek" min="0" max="1000" step="1" value="0" disabled aria-label="Position">
  <span id="pl-time" class="pl-time">0:00 / 0:00</span>
  <label class="pl-opt">speed
    <select id="pl-speed">
      <option value="0.5">0.5x</option>
      <option value="1" selected>1x</option>
      <option value="2">2x</option>
      <option value="4">4x</option>
      <option value="8">8x</option>
    </select>
  </label>
  <label class="pl-opt"><input type="checkbox" id="pl-idle" checked> skip idle</label>
</footer>
</body>
</html>
"##;

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The token login page, optionally with an error line.
pub fn login_page(error: Option<&str>) -> String {
    let error = error
        .map(|e| format!("<p class=\"err\">{}</p>", escape_html(e)))
        .unwrap_or_default();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="dark light">
<title>flywheel hub - sign in</title>
<meta name="theme-color" content="#0c0d0c">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="apple-mobile-web-app-title" content="Flywheel">
<link rel="icon" type="image/svg+xml" href="/assets/icon.svg">
<link rel="apple-touch-icon" href="/assets/apple-touch-icon.png">
<link rel="manifest" href="/manifest.webmanifest">
<script src="/assets/login.js" defer></script>
<style>
  @font-face {{ font-family: "FW Display"; src: url(/assets/vendor/fonts/big-shoulders-display.woff2) format("woff2"); font-weight: 100 900; }}
  @font-face {{ font-family: "FW Mono"; src: url(/assets/vendor/fonts/martian-mono.woff2) format("woff2"); font-weight: 100 800; font-stretch: 75% 112.5%; }}
  :root {{ color-scheme: dark; --bg: #0c0d0c; --panel: #181a17; --ink: #ece6d6; --ink-2: #a8a291; --rule: #34362f; --signal: #ff5a1f; --alert: #ff3d5e; }}
  @media (prefers-color-scheme: light) {{
    :root {{ color-scheme: light; --bg: #f2eee4; --panel: #fbf9f3; --ink: #1b1c18; --ink-2: #57544b; --rule: #c3bca9; --signal: #e2470c; --alert: #c8143a; }}
  }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--ink);
         font: 12px/1.6 "FW Mono", ui-monospace, monospace; font-stretch: 87.5%; overflow: hidden; }}
  .wheel {{ position: fixed; right: -12rem; bottom: -12rem; width: 38rem; height: 38rem; color: var(--rule); opacity: .6;
           animation: spin 90s linear infinite; }}
  @keyframes spin {{ to {{ transform: rotate(360deg); }} }}
  form {{ position: relative; width: min(27rem, 90vw); padding: 0 1.8rem 1.8rem; background: var(--panel);
         border-top: 4px solid var(--signal); box-shadow: 0 30px 80px -24px rgba(0,0,0,.6), 0 0 0 1px var(--rule); }}
  .kicker {{ display: block; padding-top: 1.4rem; font-size: 10px; letter-spacing: .3em; text-transform: uppercase; color: var(--signal); }}
  h1 {{ margin: .1rem 0 .8rem; font: 800 3.2rem/.9 "FW Display", "Arial Narrow", sans-serif; text-transform: uppercase; }}
  p {{ margin: 0 0 1.3rem; color: var(--ink-2); }}
  code {{ color: var(--signal); }}
  input {{ width: 100%; padding: .7rem .8rem; border: 1px solid var(--rule); border-radius: 2px; background: var(--bg);
          color: inherit; font: 16px ui-monospace, monospace; }}
  input:focus {{ outline: none; border-color: var(--signal); }}
  button {{ margin-top: 1rem; width: 100%; padding: .75rem; border: 0; border-radius: 2px; background: var(--signal);
           color: #150904; font: 600 11px "FW Mono", ui-monospace, monospace; letter-spacing: .14em; text-transform: uppercase; cursor: pointer; }}
  .err {{ color: var(--alert); margin: .9rem 0 0; }}
  @media (prefers-reduced-motion: reduce) {{ .wheel {{ animation: none; }} }}
</style>
</head>
<body>
<svg class="wheel" viewBox="0 0 48 48" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-linecap="round"><circle cx="24" cy="24" r="21" stroke-width="2.4"/><circle cx="24" cy="24" r="15.5" stroke-width="1" stroke-dasharray="2.2 2.6"/><path d="M24 19V5M24 29v14M19.7 21.5 7.6 14.5M28.3 26.5l12.1 7M19.7 26.5 7.6 33.5M28.3 21.5l12.1-7" stroke-width="1.8"/></g><circle cx="24" cy="24" r="5" fill="currentColor"/></svg>
<form method="post" action="/login">
  <span class="kicker">flywheel hub</span>
  <h1>Sign in</h1>
  <p>Open a link from <code>fw hub url</code> (on a phone, scan <code>fw hub url --qr</code>), or paste its one-time code or the token from <code>fw hub token</code>.</p>
  <input name="token" type="password" autocomplete="current-password" autocapitalize="off" spellcheck="false" placeholder="code or token" aria-label="Code or token" autofocus required>
  <button type="submit">Sign in</button>
  {error}
</form>
</body>
</html>
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_resolves_and_unknown_paths_do_not() {
        for (path, bytes, _) in ASSETS {
            assert!(!bytes.is_empty(), "{path} is empty");
            assert!(asset(path).is_some());
        }
        assert!(asset("../Cargo.toml").is_none());
        assert!(asset("").is_none());
    }

    #[test]
    fn index_references_only_embedded_assets() {
        for reference in INDEX_HTML.split("/assets/").skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(
                asset(path).is_some(),
                "index.html references missing {path}"
            );
        }
    }

    /// The app's split scripts and stylesheets share one scope and one
    /// cascade: index.html loads every embedded part, the base first and
    /// the booting `app.js` last.
    #[test]
    fn index_loads_every_app_part_in_order() {
        let loaded = |prefix: &str, suffix: &str| -> Vec<String> {
            INDEX_HTML
                .split(prefix)
                .skip(1)
                .map(|r| r.split('"').next().unwrap().to_owned())
                .filter(|p| p.ends_with(suffix))
                .collect()
        };
        let scripts = loaded("<script src=\"/assets/", ".js");
        let sheets = loaded("<link rel=\"stylesheet\" href=\"/assets/", ".css");
        for (path, _, _) in ASSETS {
            if path.starts_with("app") && (path.ends_with(".js") || path.ends_with(".css")) {
                assert!(
                    scripts.iter().chain(&sheets).any(|p| p == path),
                    "index.html does not load {path}"
                );
            }
        }
        let parts: Vec<_> = scripts.iter().filter(|p| p.starts_with("app")).collect();
        assert_eq!(parts.first().unwrap().as_str(), "app-base.js");
        assert_eq!(scripts.last().unwrap(), "app.js");
        let sheets: Vec<_> = sheets.iter().filter(|p| p.starts_with("app")).collect();
        assert_eq!(sheets.first().unwrap().as_str(), "app.css");
        // The service worker keeps at most 64 of the shell's URLs.
        let shell = INDEX_HTML.matches("=\"/assets/").count();
        assert!(shell <= 64, "{shell} shell assets");
    }

    /// The names a classic script declares at its top level (column 0):
    /// what it adds to the page's one global scope.
    fn top_level_names(js: &str) -> Vec<&str> {
        js.lines()
            .filter_map(|line| {
                let line = line.strip_prefix("async ").unwrap_or(line);
                [
                    "function* ",
                    "function ",
                    "const ",
                    "let ",
                    "var ",
                    "class ",
                ]
                .iter()
                .find_map(|kw| line.strip_prefix(kw))
            })
            .map(|rest| {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect()
    }

    /// The app's parts share the page's global scope, so a top-level name
    /// in one is a page global: no two scripts may declare the same name
    /// (a redeclared const/let is a SyntaxError that stops the boot), the
    /// feature modules and vendor scripts declare none, and no app name
    /// takes a browser global's (window.status, window.name, ...).
    #[test]
    fn page_scripts_keep_the_global_scope_clean() {
        const WINDOW: &[&str] = &[
            "alert",
            "atob",
            "blur",
            "btoa",
            "chrome",
            "clientInformation",
            "close",
            "closed",
            "confirm",
            "console",
            "crypto",
            "customElements",
            "document",
            "escape",
            "eval",
            "event",
            "external",
            "fetch",
            "find",
            "focus",
            "frameElement",
            "frames",
            "getComputedStyle",
            "getSelection",
            "globalThis",
            "history",
            "indexedDB",
            "innerHeight",
            "innerWidth",
            "length",
            "localStorage",
            "location",
            "locationbar",
            "matchMedia",
            "menubar",
            "moveBy",
            "moveTo",
            "name",
            "navigation",
            "navigator",
            "open",
            "opener",
            "origin",
            "outerHeight",
            "outerWidth",
            "parent",
            "performance",
            "personalbar",
            "postMessage",
            "print",
            "prompt",
            "queueMicrotask",
            "reportError",
            "resizeBy",
            "resizeTo",
            "scheduler",
            "screen",
            "scroll",
            "scrollBy",
            "scrollTo",
            "scrollX",
            "scrollY",
            "scrollbars",
            "self",
            "sessionStorage",
            "speechSynthesis",
            "status",
            "statusbar",
            "stop",
            "structuredClone",
            "toolbar",
            "top",
            "unescape",
            "viewport",
            "visualViewport",
            "when",
            "window",
        ];
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for (path, bytes, kind) in ASSETS {
            if *kind != JS {
                continue;
            }
            let js = std::str::from_utf8(bytes).unwrap();
            let names = top_level_names(js);
            if !path.starts_with("app") {
                assert!(names.is_empty(), "{path} declares top-level {names:?}");
                continue;
            }
            for name in names {
                assert!(
                    !WINDOW.contains(&name),
                    "{path} declares {name}, a browser global"
                );
                if let Some((other, _)) = seen.iter().find(|(_, n)| *n == name) {
                    panic!("{name} is declared by both {other} and {path}");
                }
                seen.push((path, name));
            }
        }
        assert!(seen.len() > 100, "found only {} app names", seen.len());
    }

    #[test]
    fn stylesheet_fonts_are_embedded() {
        let (css, _) = asset("app.css").unwrap();
        let css = std::str::from_utf8(css).unwrap();
        let mut fonts = 0;
        for reference in css.split("url(/assets/").skip(1) {
            let path = reference.split(')').next().unwrap();
            assert!(asset(path).is_some(), "app.css references missing {path}");
            fonts += 1;
        }
        assert_eq!(fonts, 3, "one @font-face per vendored font");
    }

    #[test]
    fn vendored_code_keeps_its_licenses() {
        let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/vendor");
        for (path, _, _) in ASSETS {
            let Some(rest) = path.strip_prefix("vendor/") else {
                continue;
            };
            let package = rest.split('/').next().unwrap();
            let dir = vendor.join(package);
            let licensed = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with("LICENSE"));
            assert!(licensed, "{} has no LICENSE file", dir.display());
        }
    }

    #[test]
    fn only_fonts_and_the_icon_are_public() {
        assert!(is_public_asset("icon.svg"));
        assert!(is_public_asset("vendor/fonts/martian-mono.woff2"));
        assert!(is_public_asset("login.js"));
        for private in [
            "app.js",
            "app.css",
            "vendor/xterm/xterm.js",
            "vendor/fonts/LICENSE-martian-mono",
            "vendor/fonts/../../app.js.woff2",
            "vendor/fonts/x/y.woff2",
        ] {
            assert!(!is_public_asset(private), "{private}");
        }
        // Every public asset referenced by the login page exists.
        let page = login_page(None);
        for reference in page.split("/assets/").skip(1) {
            let path = reference.split([')', '"']).next().unwrap();
            assert!(is_public_asset(path) && asset(path).is_some(), "{path}");
        }
        for reference in LOGGED_IN_HTML.split("/assets/").skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(is_public_asset(path) && asset(path).is_some(), "{path}");
        }
    }

    #[test]
    fn the_viewer_page_loads_only_viewer_assets() {
        let credential = format!("fwview_{}_{}", "a".repeat(12), "b".repeat(64));
        let page = viewer_page(&credential);
        let prefix = format!("/share/{credential}/assets/");
        let mut seen = 0;
        for reference in page.split(prefix.as_str()).skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(is_viewer_asset(path) && asset(path).is_some(), "{path}");
            seen += 1;
        }
        assert_eq!(seen, 5);
        // Other assets it names are public (the icon).
        for reference in page.split("\"/assets/").skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(is_public_asset(path), "{path}");
        }
        for app_code in [
            "app.js",
            "app-base.js",
            "sharing.js",
            "player.js",
            "app.css",
        ] {
            assert!(!is_viewer_asset(app_code), "{app_code}");
        }
        assert!(share_refused_page("<b>").contains("&lt;b&gt;"));
        for reference in PLAYER_HTML.split("/assets/").skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(
                asset(path).is_some(),
                "player.html references missing {path}"
            );
        }
    }

    #[test]
    fn login_page_escapes_the_error() {
        let page = login_page(Some("<script>x</script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x"));
        assert!(!login_page(None).contains("class=\"err\""));
    }

    #[test]
    fn manifest_icons_are_public_and_embedded() {
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["start_url"], "/");
        let icons = manifest["icons"].as_array().unwrap();
        assert!(icons.iter().any(|i| i["purpose"] == "maskable"));
        assert!(icons.iter().any(|i| i["sizes"] == "512x512"));
        for icon in icons {
            let path = icon["src"]
                .as_str()
                .unwrap()
                .strip_prefix("/assets/")
                .unwrap();
            assert!(is_public_asset(path) && asset(path).is_some(), "{path}");
        }
        for page in [INDEX_HTML.to_owned(), login_page(None)] {
            assert!(page.contains(r#"<link rel="manifest" href="/manifest.webmanifest">"#));
            assert!(page.contains("apple-touch-icon"));
        }
    }

    #[test]
    fn png_icons_have_the_declared_sizes() {
        for (path, size) in [
            ("icon-192.png", 192),
            ("icon-512.png", 512),
            ("icon-maskable-512.png", 512),
            ("apple-touch-icon.png", 180),
        ] {
            let (bytes, kind) = asset(path).unwrap();
            assert_eq!(kind, "image/png");
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{path}");
            // IHDR: width and height, big-endian, at bytes 16..24.
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!((width, height), (size, size), "{path}");
        }
    }

    #[test]
    fn the_service_worker_is_stamped_and_leaves_the_api_alone() {
        let sw = service_worker();
        assert!(!sw.contains("__FW_HUB_BUILD__"));
        assert!(sw.contains(build_id()));
        assert_eq!(build_id().len(), 16);
        assert!(sw.contains(SHELL_HEADER));
        // Only same-origin GETs of `/` and `/assets/` are intercepted.
        assert!(sw.contains(r#"request.method !== "GET""#));
        assert!(!sw.contains("/api/"));
    }
}
