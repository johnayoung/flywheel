//! A minimal blocking HTTP/1.1 client for the CLI verbs that talk to the
//! running hub on this machine (`fw hub url`, `fw hub token --rotate`).
//!
//! The peer is always our own server, so this handles exactly what it
//! answers: one request per connection (`Connection: close`), bodies
//! delimited by `Content-Length`, chunked encoding, or end of stream.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Largest response body accepted.
const MAX_BODY: usize = 1024 * 1024;

/// A response: status code and body.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// The API's `{"error": ...}` message, or the raw body.
    pub fn error_message(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_owned())
    }
}

/// Send `method path` to `addr` with a bearer credential and an optional
/// JSON body.
pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: &str,
    json_body: Option<&str>,
    timeout: Duration,
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = json_body.unwrap_or("");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {bearer}\r\n\
         Connection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if json_body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream
        .take(u64::try_from(MAX_BODY + 64 * 1024).unwrap_or(u64::MAX))
        .read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

/// Parse one complete HTTP/1.1 response.
pub fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| invalid("truncated response head"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| invalid("non-UTF-8 head"))?;
    let rest = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| invalid("bad content-length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(length) = length {
        if rest.len() < length {
            return Err(invalid("truncated body"));
        }
        rest[..length].to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > MAX_BODY {
        return Err(invalid("response too large"));
    }
    Ok(Response { status, body })
}

fn dechunk(mut rest: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| invalid("truncated chunk size"))?;
        let size_text = std::str::from_utf8(&rest[..end]).map_err(|_| invalid("bad chunk"))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("bad chunk size"))?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size + 2 || body.len() + size > MAX_BODY {
            return Err(invalid("truncated or oversized chunk"));
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_chunked_and_eof_bodies() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhiXX").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hi"[..]));
        let r = parse_response(
            b"HTTP/1.1 401 Unauthorized\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (401, &b"abcde"[..]));
        let r = parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert!(r.body.is_empty());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nhi").is_err());
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn error_message_prefers_the_api_error() {
        let r = Response {
            status: 400,
            body: br#"{"error":"nope"}"#.to_vec(),
        };
        assert_eq!(r.error_message(), "nope");
        let r = Response {
            status: 502,
            body: b" bad gateway \n".to_vec(),
        };
        assert_eq!(r.error_message(), "bad gateway");
    }
}
