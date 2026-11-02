//! The embedded web app: every asset is compiled into the binary, so a hub
//! is a single static executable with no build step and no files on disk.
//!
//! Vendored third-party code lives under `web/vendor/<package>/` with its
//! license file (xterm.js 6.0.0, @xterm/addon-fit 0.11.0,
//! @xterm/addon-web-links 0.12.0; sourcemap comments stripped).

/// The app shell (served at `/` once logged in).
pub const INDEX_HTML: &str = include_str!("../web/index.html");

/// Answer to a successful login: navigate to `/` from this origin.
pub const LOGGED_IN_HTML: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta http-equiv=\"refresh\" content=\"0;url=/\"><title>flywheel hub</title></head>\
<body><p>Signed in. <a href=\"/\">Continue to the hub</a>.</p></body></html>";

/// Every embedded asset under `/assets/`: `(path, bytes, content type)`.
const ASSETS: &[(&str, &[u8], &str)] = &[
    (
        "app.js",
        include_bytes!("../web/app.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "app.css",
        include_bytes!("../web/app.css"),
        "text/css; charset=utf-8",
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
<title>flywheel hub - sign in</title>
<link rel="icon" href="data:,">
<style>
  :root {{ color-scheme: dark; }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center;
         background: #0e1013; color: #d7dae0; font: 15px/1.5 ui-sans-serif, system-ui, sans-serif; }}
  form {{ width: min(26rem, 90vw); padding: 2rem; border: 1px solid #262a31; border-radius: 10px; background: #15181d; }}
  h1 {{ margin: 0 0 .25rem; font-size: 1.15rem; letter-spacing: .02em; }}
  p {{ margin: 0 0 1.25rem; color: #8b919c; }}
  code {{ color: #e8b45a; }}
  input {{ box-sizing: border-box; width: 100%; padding: .65rem .75rem; border-radius: 6px; border: 1px solid #2f343d;
          background: #0e1013; color: inherit; font: 13px ui-monospace, monospace; }}
  button {{ margin-top: 1rem; width: 100%; padding: .65rem; border: 0; border-radius: 6px; background: #e8b45a;
           color: #15181d; font-weight: 600; cursor: pointer; }}
  .err {{ color: #ef6b6b; margin: .75rem 0 0; }}
</style>
</head>
<body>
<form method="post" action="/login">
  <h1>flywheel hub</h1>
  <p>Paste the token from <code>fw hub token</code>, or open the login URL <code>fw hub serve</code> printed.</p>
  <input name="token" type="password" autocomplete="current-password" placeholder="token" autofocus required>
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
    fn login_page_escapes_the_error() {
        let page = login_page(Some("<script>x</script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x"));
        assert!(!login_page(None).contains("class=\"err\""));
    }
}
