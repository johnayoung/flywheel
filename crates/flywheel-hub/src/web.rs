//! The embedded web app: every asset is compiled into the binary, so a hub
//! is a single static executable with no build step and no files on disk.
//!
//! Vendored third-party code lives under `web/vendor/<package>/` with its
//! license file (xterm.js 6.0.0, @xterm/addon-fit 0.11.0,
//! @xterm/addon-web-links 0.12.0; sourcemap comments stripped) and the
//! UI fonts under `web/vendor/fonts/` (Big Shoulders Display, Martian Mono,
//! JetBrains Mono: latin variable subsets from @fontsource-variable 5.3.0,
//! SIL OFL 1.1).

/// The app shell (served at `/` once logged in).
pub const INDEX_HTML: &str = include_str!("../web/index.html");

/// Answer to a successful login: `login.js` replaces it with `/` (keeping
/// the URL fragment) from this origin; without scripts, a meta refresh.
pub const LOGGED_IN_HTML: &str = "<!doctype html><html lang=\"en\" data-signed-in><head><meta charset=\"utf-8\">\
<title>flywheel hub</title><script src=\"/assets/login.js\"></script>\
<noscript><meta http-equiv=\"refresh\" content=\"0;url=/\"></noscript></head>\
<body><p>Signed in. <a href=\"/\">Continue to the hub</a>.</p></body></html>";

/// Every embedded asset under `/assets/`: `(path, bytes, content type)`.
const ASSETS: &[(&str, &[u8], &str)] = &[
    (
        "app.js",
        include_bytes!("../web/app.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "commands.js",
        include_bytes!("../web/commands.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "login.js",
        include_bytes!("../web/login.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "app.css",
        include_bytes!("../web/app.css"),
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
        "text/javascript; charset=utf-8",
    ),
    (
        "vendor/xterm/xterm.css",
        include_bytes!("../web/vendor/xterm/xterm.css"),
        "text/css; charset=utf-8",
    ),
    (
        "vendor/addon-fit/addon-fit.js",
        include_bytes!("../web/vendor/addon-fit/addon-fit.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "vendor/addon-web-links/addon-web-links.js",
        include_bytes!("../web/vendor/addon-web-links/addon-web-links.js"),
        "text/javascript; charset=utf-8",
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
/// the icon, and `login.js` (no app code).
pub fn is_public_asset(path: &str) -> bool {
    path == "icon.svg"
        || path == "login.js"
        || path
            .strip_prefix("vendor/fonts/")
            .is_some_and(|f| f.ends_with(".woff2") && !f.contains('/'))
}

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
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>flywheel hub - sign in</title>
<link rel="icon" type="image/svg+xml" href="/assets/icon.svg">
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
          color: inherit; font: 12px ui-monospace, monospace; }}
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
"#
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
    fn login_page_escapes_the_error() {
        let page = login_page(Some("<script>x</script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x"));
        assert!(!login_page(None).contains("class=\"err\""));
    }
}
