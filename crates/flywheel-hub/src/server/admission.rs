//! Middleware: file each request's connection in the main, the share-link,
//! or the unauthenticated slot pool ([`super::slots`]) by the credential
//! the request carries, before any route sees it.
//!
//! This only sorts connections; `require_auth` still decides what a
//! request may do. The sort checks credentials without counting failures
//! (a wrong one is counted where it is refused) but not for a client the
//! failure limit has locked out. What it reveals -- whether the connection
//! stays open -- says only whether a 256-bit secret matched, which no rate
//! of guessing can exploit.

use super::slots::Admission;
use super::{AppState, Client, header_str, login_cookies};
use crate::auth::{bearer_value, cookie_name};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::PoisonError;
use std::time::Instant;

/// What a request presents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presented {
    /// A live login cookie, the token, or a peer credential.
    Credential,
    /// A live share link's credential (in its `/share/` path): an
    /// outsider's, which never takes a main slot.
    ShareLink,
    Nothing,
}

fn presented(app: &AppState, request: &Request, client: Client) -> Presented {
    let headers = request.headers();
    let locked = || {
        !client.same_machine
            && app
                .failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .locked(client.ip, Instant::now())
                .is_some()
    };
    let token = app.hub.token();
    let cookies = login_cookies(headers, &cookie_name(token.expose()));
    if cookies.iter().any(|c| app.hub.login_valid(c)) {
        return Presented::Credential;
    }
    if let Some(bearer) = header_str(headers, header::AUTHORIZATION).and_then(bearer_value)
        && !locked()
        && (token.matches(bearer) || app.peers.verify(bearer).is_some())
    {
        return Presented::Credential;
    }
    if let Some((credential, _)) = crate::share::credential_in_path(request.uri().path())
        && !locked()
        && matches!(
            app.hub.shares().verify(credential),
            crate::share::Verdict::Live(_)
        )
    {
        return Presented::ShareLink;
    }
    Presented::Nothing
}

fn closing(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// Sort the request's connection; an unauthenticated one answers with
/// `Connection: close`. Requests the accept loop did not admit (an
/// in-process router) pass through.
pub(super) async fn admit(State(app): State<AppState>, request: Request, next: Next) -> Response {
    let Some(admission) = request.extensions().get::<Admission>().cloned() else {
        return next.run(request).await;
    };
    let client = Client::of(request.extensions(), request.headers(), &app.options);
    // This machine's own requests (no proxy in between) are not counted
    // per client: the pool's size still bounds them.
    let counted = (!client.same_machine).then_some(client.ip);
    match presented(&app, &request, client) {
        Presented::Credential => {
            if !admission.authenticated().await {
                return closing(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the hub dropped this connection; retry\n",
                );
            }
            return next.run(request).await;
        }
        Presented::ShareLink => {
            if !admission.shared(counted) {
                return closing(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many share-link connections; retry\n",
                );
            }
            return next.run(request).await;
        }
        Presented::Nothing => {}
    }
    if !admission.unauthenticated(counted) {
        return closing(
            StatusCode::TOO_MANY_REQUESTS,
            "too many unauthenticated connections from this address\n",
        );
    }
    let mut response = next.run(request).await;
    if admission.pending() && response.status() != StatusCode::SWITCHING_PROTOCOLS {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}
