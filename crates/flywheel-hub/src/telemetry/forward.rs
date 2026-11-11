//! Sending this hub's events upstream: many hubs report into one
//! collector (`[telemetry] upstream` in `hub.toml` names a registered
//! machine; its peer credential and certificate pin are the link).
//!
//! Events leave in the order they reached the store, [`BATCH_EVENTS`] at a
//! time, as deflated NDJSON to `POST /api/telemetry/batch`. The cursor
//! moves only when the collector accepted a batch, so a collector that is
//! down costs nothing but delay: events wait in the local store (for
//! [`super::RETENTION_DAYS`]) and are sent when it is back. Delivery is
//! at-least-once; the collector ignores what it already has.

use super::{Event, Telemetry};
use crate::machines::Machines;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Events in one batch.
pub const BATCH_EVENTS: usize = 2000;
/// How long to wait for more when a batch came up empty.
const IDLE: Duration = Duration::from_secs(1);
/// One batch's request.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest pause after failures.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A batch's content type.
pub const NDJSON: &str = "application/x-ndjson";
/// The header saying a batch's body is deflated (zlib).
pub const DEFLATED: &str = "deflate";

/// One batch's body: an event a line, deflated.
pub fn encode(events: &[Event]) -> std::io::Result<Vec<u8>> {
    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    for event in events {
        serde_json::to_writer(&mut encoder, event)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()
}

/// A batch's events back from its body (`deflated`: as [`encode`] wrote
/// it; else plain NDJSON). At most `max_bytes` are inflated.
pub fn decode(body: &[u8], deflated: bool, max_bytes: usize) -> Result<Vec<Event>, String> {
    let text = if deflated {
        use std::io::Read;
        let mut text = Vec::new();
        flate2::read::ZlibDecoder::new(body)
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut text)
            .map_err(|e| format!("the batch does not inflate: {e}"))?;
        if text.len() > max_bytes {
            return Err(format!("a batch inflates to at most {max_bytes} bytes"));
        }
        text
    } else {
        body.to_vec()
    };
    let mut events = Vec::new();
    for (at, line) in text.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // A kind this build does not know is a newer hub's: skipped, so
        // one new event kind never stalls a fleet's forwarding.
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|e| format!("line {} of the batch is not JSON: {e}", at + 1))?;
        if let Ok(event) = serde_json::from_value::<Event>(value) {
            events.push(event);
        }
    }
    Ok(events)
}

/// Forward until `shutdown` flips: the task `fw hub serve` runs when an
/// upstream is configured.
pub async fn run(
    telemetry: Arc<Telemetry>,
    machines: Arc<Machines>,
    upstream: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let cursor = format!("upstream:{upstream}");
    let mut backoff = IDLE;
    let mut complained = false;
    loop {
        let pause = match send_one(&telemetry, &machines, &upstream, &cursor).await {
            Ok(0) => {
                backoff = IDLE;
                IDLE
            }
            Ok(_) => {
                backoff = IDLE;
                complained = false;
                Duration::ZERO
            }
            Err(message) => {
                if !complained {
                    eprintln!("fw hub: telemetry upstream {upstream:?}: {message} (retrying)");
                    complained = true;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                backoff
            }
        };
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// Send the next batch; how many events the collector took.
async fn send_one(
    telemetry: &Arc<Telemetry>,
    machines: &Machines,
    upstream: &str,
    cursor: &str,
) -> Result<usize, String> {
    let batch = {
        let telemetry = telemetry.clone();
        let cursor = cursor.to_owned();
        tokio::task::spawn_blocking(move || {
            telemetry.flush();
            telemetry.outbox(&cursor, BATCH_EVENTS)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?
    };
    let Some(last) = batch.last().map(|(id, _)| *id) else {
        return Ok(0);
    };
    let machine = machines.get(upstream).ok_or_else(|| {
        format!("no machine named {upstream:?} is registered (`fw hub machine add`)")
    })?;
    let events: Vec<Event> = batch.into_iter().map(|(_, event)| event).collect();
    let sent = events.len();
    let body = encode(&events).map_err(|e| e.to_string())?;
    let response = machine
        .send_encoded(
            "/api/telemetry/batch",
            body,
            NDJSON,
            DEFLATED,
            SEND_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let detail = crate::machines::read_capped(response, 2048)
            .await
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        return Err(format!("the collector answered {status}: {}", detail.trim()));
    }
    let telemetry = telemetry.clone();
    let cursor = cursor.to_owned();
    tokio::task::spawn_blocking(move || telemetry.advance(&cursor, last))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::What;

    #[test]
    fn a_batch_round_trips_and_is_bounded() {
        let event = |seq| Event {
            host: "a".repeat(32),
            session: "s1".to_owned(),
            seq,
            ts: 1000 + seq as i64,
            agent: None,
            work: Some("t1".to_owned()),
            item: None,
            what: What::TurnStarted {
                title: "fix the build".to_owned(),
            },
        };
        let events = vec![event(1), event(2)];
        let body = encode(&events).unwrap();
        assert_eq!(decode(&body, true, 1 << 20).unwrap(), events);
        assert!(decode(&body, true, 16).is_err(), "inflation is capped");
        let plain = format!(
            "{}\n\n{{\"host\":\"{}\",\"session\":\"s\",\"seq\":3,\"ts\":1,\"kind\":\"from.the.future\",\"body\":{{}}}}\n",
            serde_json::to_string(&events[0]).unwrap(),
            "a".repeat(32)
        );
        assert_eq!(
            decode(plain.as_bytes(), false, 1 << 20).unwrap(),
            vec![events[0].clone()],
            "an unknown kind is skipped, not fatal"
        );
        assert!(decode(b"not json\n", false, 1 << 20).is_err());
    }
}
