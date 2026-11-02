//! The installable-app pieces served at the origin root, where browsers
//! look for them: `/manifest.webmanifest` and the service worker `/sw.js`
//! (its scope is the directory it is served from, so it cannot live under
//! `/assets/`). Both are public: they hold no secrets, and a manifest is
//! fetched without cookies.

use crate::web;
use axum::http::header;
use axum::response::{IntoResponse, Response};

pub(super) async fn manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        web::MANIFEST,
    )
        .into_response()
}

/// The service worker, stamped with this build's asset digest so a new
/// binary replaces it (and its cached app shell).
pub(super) async fn service_worker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        web::service_worker(),
    )
        .into_response()
}
