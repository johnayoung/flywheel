//! The CA onboarding page (`/ca`): public and read-only, over http and
//! https alike, so a phone that does not trust the hub yet can fetch its
//! CA certificate (DER, PEM, or an iOS `.mobileconfig` profile), compare
//! the fingerprint out of band, and follow its OS's install steps. No
//! credential ever comes here: the page is plain http to a device that
//! does not trust the CA yet, so `fw hub url --qr` gives it a QR of its
//! own and puts the login code only in the https login QR.
//!
//! Also `GET /api/tls` (the TLS status, signed in) and `POST
//! /api/tls/reload` (re-read the CA and re-issue the leaf now; `fw hub tls
//! rotate` calls it).

use super::{AppState, error_response};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use std::time::SystemTime;

const PAGE: &str = include_str!("../../web/ca.html");
const SCRIPT: &str = include_str!("../../web/ca.js");
const STYLE: &str = include_str!("../../web/ca.css");

/// The page's own policy: like the app's, but its script may probe this
/// host over https from an http page.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   font-src 'self'; connect-src 'self' https:; frame-ancestors 'none'; \
                   base-uri 'none'; form-action 'none'";

fn date(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%d")
        .to_string()
}

fn no_ca() -> Response {
    let mut page = Html(crate::web::notice_page(
        "No local CA",
        "This hub does not sign its own certificates: TLS is off, or it serves a certificate you \
         supplied (--tls CERT,KEY). Start it with `fw hub serve --tls auto` for a local CA.",
        "Open the hub",
    ))
    .into_response();
    *page.status_mut() = StatusCode::NOT_FOUND;
    page
}

fn fixed(mut response: Response, content_type: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /ca`.
pub(super) async fn page(State(app): State<AppState>) -> Response {
    let Some(tls) = app.options.tls.as_ref() else {
        return no_ca();
    };
    let Some(ca) = tls.ca() else {
        return no_ca();
    };
    let esc = crate::web::escape_html;
    let host = crate::machines::host_name();
    // This hub's own names (its leaf's, in or out of the constraints, but
    // not the extra ones) are not what a device is warned about; any other
    // public one is.
    let names = tls
        .names()
        .map_or_else(crate::tls::Names::default, |(mut inside, outside)| {
            inside.dns.extend(outside.dns);
            inside.ips.extend(outside.ips);
            inside
        });
    let public = ca.constraints().public(&names, tls.extra_names());
    let public_warning = if public.is_empty() {
        String::new()
    } else {
        let list: String = public
            .iter()
            .map(|spec| format!("<li>{}</li>", esc(&spec.to_string())))
            .collect();
        let lines: String = crate::tls::public_warnings(&public, tls.extra_names())
            .iter()
            .map(|line| format!("<p>{}</p>", esc(line)))
            .collect();
        format!(
            "<section class=\"public\" role=\"alert\"><h2>Warning: public names</h2><p>This CA \
             can also vouch for these <b>public</b> names (and every name under them). Install it \
             only if you run them and trust this machine's key with them: whoever holds it can \
             impersonate them to this device.</p><ul class=\"names\">{list}</ul>{lines}</section>"
        )
    };
    let constraints: String = ca
        .constraints()
        .list()
        .iter()
        .map(|c| format!("<li>{}</li>", esc(c)))
        .collect();
    let html = PAGE
        .replace(
            "{{HOST}}",
            &esc(if host.is_empty() {
                "this machine"
            } else {
                &host
            }),
        )
        .replace(
            "{{NAME}}",
            &esc(&crate::tls::ca_common_name(&ca).unwrap_or_default()),
        )
        .replace(
            "{{FINGERPRINT}}",
            // Lines may break after a colon only, never inside a byte.
            &esc(&crate::machines::display_fingerprint(ca.fingerprint())).replace(':', ":<wbr>"),
        )
        .replace("{{CONSTRAINTS}}", &constraints)
        .replace("{{PUBLIC}}", &public_warning)
        .replace("{{CREATED}}", &date(ca.created()))
        .replace("{{EXPIRES}}", &date(ca.expires()));
    let mut response = fixed(Html(html).into_response(), "text/html; charset=utf-8");
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    response
}

/// `GET /ca/hub-ca.crt`: DER (Android, Windows, macOS open it as a CA).
pub(super) async fn der(State(app): State<AppState>) -> Response {
    let Some(ca) = app.options.tls.as_ref().and_then(|t| t.ca()) else {
        return no_ca();
    };
    attachment(
        ca.cert_der().as_ref().to_vec(),
        "application/x-x509-ca-cert",
        "flywheel-hub-ca.crt",
    )
}

/// `GET /ca/hub-ca.pem`.
pub(super) async fn pem(State(app): State<AppState>) -> Response {
    let Some(ca) = app.options.tls.as_ref().and_then(|t| t.ca()) else {
        return no_ca();
    };
    attachment(
        ca.cert_pem().as_bytes().to_vec(),
        "application/x-pem-file",
        "flywheel-hub-ca.pem",
    )
}

/// `GET /ca/hub-ca.mobileconfig`: the iOS profile (Safari offers to
/// install it by its type).
pub(super) async fn mobileconfig(State(app): State<AppState>) -> Response {
    let Some(ca) = app.options.tls.as_ref().and_then(|t| t.ca()) else {
        return no_ca();
    };
    attachment(
        ca.mobileconfig().into_bytes(),
        "application/x-apple-aspen-config",
        "flywheel-hub-ca.mobileconfig",
    )
}

fn attachment(bytes: Vec<u8>, content_type: &'static str, name: &'static str) -> Response {
    let mut response = fixed(bytes.into_response(), content_type);
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// `GET /ca/probe`: answers at all only when the TLS handshake succeeded
/// (the page's trust check).
pub(super) async fn probe() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn script() -> Response {
    fixed(SCRIPT.into_response(), "text/javascript; charset=utf-8")
}

pub(super) async fn style() -> Response {
    fixed(STYLE.into_response(), "text/css; charset=utf-8")
}

/// `GET /api/tls`.
pub(super) async fn status(State(app): State<AppState>) -> Response {
    Json(crate::tls::summary(app.options.tls.as_deref())).into_response()
}

/// `POST /api/tls/reload`: re-read the CA (a rotation) and re-issue now.
pub(super) async fn reload(State(app): State<AppState>) -> Response {
    let Some(tls) = app.options.tls.clone() else {
        return error_response(StatusCode::CONFLICT, "TLS is off on this hub");
    };
    let done = tokio::task::spawn_blocking(move || {
        let names = tls.ca().map(|_| {
            // The names now (a rotation may have been made for new ones).
            crate::tls::discover_names(tls.listen(), tls.extra_names())
        });
        let done = tls.refresh(names.as_ref(), SystemTime::now(), true);
        (tls, done)
    })
    .await;
    match done {
        Ok((tls, Ok(refreshed))) => {
            crate::tls::log_refresh(&refreshed);
            Json(crate::tls::summary(Some(&tls))).into_response()
        }
        Ok((_, Err(e))) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
