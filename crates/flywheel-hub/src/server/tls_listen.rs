//! https and http on one port: each accepted connection's first byte says
//! which it is (a TLS ClientHello starts with a handshake record, 0x16; an
//! HTTP request with a method's letter), so built-in TLS needs no second
//! listener, and a live handoff passes the one socket it always did.
//!
//! With TLS on, plain http from the network reaches only the CA onboarding
//! page (`/ca`, `server/ca_page.rs`); everything else is redirected to the
//! same URL over https. Loopback (a local browser, the CLI, `tailscale
//! serve`) and trusted proxies keep plain http: loopback is already a
//! secure context, and a proxy terminates TLS itself.

use super::{AppState, Peer};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// The first byte of a TLS handshake record.
const TLS_HANDSHAKE: u8 = 0x16;

/// How a request reached the hub (a request extension the accept loop
/// sets on every request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Transport {
    /// Over the hub's own TLS.
    pub(crate) tls: bool,
}

/// Whether `request` came over the hub's own TLS.
pub(crate) fn over_tls(extensions: &axum::http::Extensions) -> bool {
    extensions.get::<Transport>().is_some_and(|t| t.tls)
}

enum Stage {
    /// Waiting for the first byte.
    Sniffing(TcpStream, tokio_rustls::TlsAcceptor),
    Handshake(Box<tokio_rustls::Accept<TcpStream>>),
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    /// The handshake failed (reported once; every later call fails too).
    Failed,
}

/// An accepted connection that becomes plain TCP or TLS on its first
/// read. `secure` is set once that is known, before any request on it is
/// read.
pub(crate) struct Sniffed {
    state: Stage,
    secure: Arc<OnceLock<bool>>,
}

impl Sniffed {
    /// `stream`, served over TLS when `acceptor` is given and the client
    /// starts a handshake.
    pub(crate) fn new(
        stream: TcpStream,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    ) -> (Self, Arc<OnceLock<bool>>) {
        let secure = Arc::new(OnceLock::new());
        let state = match acceptor {
            Some(acceptor) => Stage::Sniffing(stream, acceptor),
            None => {
                let _ = secure.set(false);
                Stage::Plain(stream)
            }
        };
        (
            Self {
                state,
                secure: Arc::clone(&secure),
            },
            secure,
        )
    }

    /// Drive the sniff and the handshake until the stream is usable.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match &mut self.state {
                Stage::Plain(_) | Stage::Tls(_) => return Poll::Ready(Ok(())),
                Stage::Failed => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "TLS handshake failed",
                    )));
                }
                Stage::Sniffing(stream, _) => {
                    let mut byte = [0u8; 1];
                    let mut buf = ReadBuf::new(&mut byte);
                    ready!(stream.poll_peek(cx, &mut buf))?;
                    let tls = buf.filled() == [TLS_HANDSHAKE];
                    let Stage::Sniffing(stream, acceptor) =
                        std::mem::replace(&mut self.state, Stage::Failed)
                    else {
                        unreachable!("matched above");
                    };
                    self.state = if tls {
                        Stage::Handshake(Box::new(acceptor.accept(stream)))
                    } else {
                        let _ = self.secure.set(false);
                        Stage::Plain(stream)
                    };
                }
                Stage::Handshake(accept) => match ready!(Pin::new(accept.as_mut()).poll(cx)) {
                    Ok(stream) => {
                        let _ = self.secure.set(true);
                        self.state = Stage::Tls(Box::new(stream));
                    }
                    Err(e) => {
                        self.state = Stage::Failed;
                        return Poll::Ready(Err(e));
                    }
                },
            }
        }
    }
}

impl AsyncRead for Sniffed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_ready(cx))?;
        match &mut this.state {
            Stage::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stage::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            _ => unreachable!("ready"),
        }
    }
}

impl AsyncWrite for Sniffed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_ready(cx))?;
        match &mut this.state {
            Stage::Plain(s) => Pin::new(s).poll_write(cx, data),
            Stage::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, data),
            _ => unreachable!("ready"),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.state {
            Stage::Plain(s) => Pin::new(s).poll_flush(cx),
            Stage::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            _ => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.state {
            Stage::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stage::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            Stage::Sniffing(s, _) => Pin::new(s).poll_shutdown(cx),
            Stage::Handshake(_) | Stage::Failed => Poll::Ready(Ok(())),
        }
    }
}

/// What plain http from the network may reach when TLS is on: the CA
/// onboarding page and the static files it uses.
pub(crate) fn plain_http_allowed(path: &str) -> bool {
    path == "/ca"
        || path.starts_with("/ca/")
        || path == "/assets/icon.svg"
        || path.starts_with("/assets/vendor/fonts/")
}

/// Middleware: with TLS on, plain http from a client on the network gets
/// the CA page or a redirect to https (308: the method and body are kept);
/// loopback and trusted proxies pass.
pub(super) async fn plain_http_policy(
    State(app): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if app.options.tls.is_none() || over_tls(request.extensions()) {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .map(|ConnectInfo(p)| p.remote.ip().to_canonical());
    // No connect info: an in-process caller, this machine.
    let local = peer.is_none_or(|ip| ip.is_loopback() || app.options.trusted_proxies.contains(&ip));
    if local || plain_http_allowed(request.uri().path()) {
        return next.run(request).await;
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
        .filter(|a| !a.as_str().contains('@'));
    let Some(host) = host else {
        return super::error_response(
            StatusCode::BAD_REQUEST,
            "this hub is served over https: open it with https://",
        );
    };
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let location = format!("https://{host}{path}");
    let mut response = (
        StatusCode::PERMANENT_REDIRECT,
        format!("this hub is served over https: {location}\n"),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    // Not cached: turning TLS off again must not leave browsers bouncing.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ca_page_is_open_over_plain_http() {
        for open in [
            "/ca",
            "/ca/",
            "/ca/hub-ca.crt",
            "/ca/hub-ca.mobileconfig",
            "/assets/icon.svg",
            "/assets/vendor/fonts/martian-mono.woff2",
        ] {
            assert!(plain_http_allowed(open), "{open}");
        }
        for closed in [
            "/",
            "/login",
            "/cab",
            "/api/sessions",
            "/assets/app.js",
            "/share/x",
            "/healthz",
        ] {
            assert!(!plain_http_allowed(closed), "{closed}");
        }
    }
}
