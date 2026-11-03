//! Middleware: file each request's connection in the main or the
//! unauthenticated slot pool ([`super::slots`]) by whether the request
//! carries a valid credential, before any route sees it.
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
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::PoisonError;
use std::time::Instant;

/// Whether `headers` carry a live login cookie, the token, or a peer
/// credential.
fn presents_credential(app: &AppState, headers: &HeaderMap, client: Client) -> bool {
    let token = app.hub.token();
    let cookies = login_cookies(headers, &cookie_name(token.expose()));
    if cookies.iter().any(|c| app.hub.login_valid(c)) {
        return true;
    }
    let Some(bearer) = header_str(headers, header::AUTHORIZATION).and_then(bearer_value) else {
        return false;
    };
    if !client.same_machine {
        let mut failures = app.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if failures.locked(client.ip, Instant::now()).is_some() {
            return false;
        }
    }
    token.matches(bearer) || app.peers.verify(bearer).is_some()
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
    if presents_credential(&app, request.headers(), client) {
        if !admission.authenticated().await {
            return closing(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub dropped this connection; retry\n",
            );
        }
        return next.run(request).await;
    }
    // This machine's own requests (no proxy in between) are not counted
    // per client: the pool's size still bounds them.
    let counted = (!client.same_machine).then_some(client.ip);
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
