//! The activity journal over HTTP (see [`crate::activity`]): `GET
//! /api/activity` (the timeline, cursor-paged), `GET
//! /api/activity/digest` ("while you were away"), and `POST
//! /api/activity/seen`. A full-scope request also asks every registered
//! machine (its own `/api/activity...` with `machines=none`, each within
//! [`MACHINE_TIMEOUT`]) and merges the answers, machine-labelled; a
//! machine that does not answer is named in `machines` and makes the
//! answer `partial`. A peer's request is answered from this hub alone.

use super::{ApiJson, ApiQuery, ApiResult, AppState, error_response};
use crate::activity::{Cursor, DEFAULT_LIMIT, Digest, Filter, LOCAL, MAX_LIMIT, Row, parse_time};
use crate::machines::{Machine, read_capped};
use crate::peers::PeerInfo;
use crate::script::query_escape;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How long one machine's answer is waited for.
pub const MACHINE_TIMEOUT: Duration = Duration::from_secs(4);
/// The most a machine's answer may hold.
const MAX_MACHINE_BODY: usize = 8 * 1024 * 1024;

#[derive(Deserialize, Default)]
pub(super) struct ActivityQuery {
    since: Option<String>,
    until: Option<String>,
    ws: Option<String>,
    session: Option<String>,
    kind: Option<String>,
    limit: Option<String>,
    cursor: Option<String>,
    /// `none`: this hub only (what a merging hub asks a machine).
    machines: Option<String>,
    /// The digest's client (`since=last-seen` reads its mark).
    client: Option<String>,
}

fn bad(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, message)
}

/// A client id: 1..=64 of `[A-Za-z0-9_-]` (as the events socket's).
fn valid_client(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The filter `query` asks for (`since=last-seen` is the digest's).
fn filter_of(query: &ActivityQuery, now: DateTime<Utc>) -> Result<Filter, String> {
    let time = |raw: &Option<String>| -> Result<Option<DateTime<Utc>>, String> {
        raw.as_deref()
            .filter(|r| !r.is_empty())
            .map(|r| parse_time(r, now))
            .transpose()
    };
    let kinds = query
        .kind
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if kinds.len() > 32 || kinds.iter().any(|k| k.len() > 64) {
        return Err("kind takes at most 32 kinds".into());
    }
    let id = |raw: &Option<String>| raw.clone().filter(|r| !r.is_empty());
    Ok(Filter {
        since: time(&query.since)?,
        until: time(&query.until)?,
        workspace: id(&query.ws),
        session: id(&query.session),
        kinds,
    })
}

/// The query string asking a machine for the same rows.
fn machine_query(filter: &Filter, limit: usize, before: Option<u64>) -> String {
    let mut q = format!("machines=none&limit={limit}");
    if let Some(since) = filter.since {
        q.push_str(&format!("&since={}", query_escape(&since.to_rfc3339())));
    }
    if let Some(until) = filter.until {
        q.push_str(&format!("&until={}", query_escape(&until.to_rfc3339())));
    }
    if let Some(ws) = &filter.workspace {
        q.push_str(&format!("&ws={}", query_escape(ws)));
    }
    if let Some(session) = &filter.session {
        q.push_str(&format!("&session={}", query_escape(session)));
    }
    if !filter.kinds.is_empty() {
        q.push_str(&format!("&kind={}", query_escape(&filter.kinds.join(","))));
    }
    if let Some(before) = before {
        q.push_str(&format!("&cursor={LOCAL}:{before}"));
    }
    q
}

/// `GET path` on `machine` as JSON, or why not.
async fn machine_get(machine: &Machine, path: &str) -> Result<Value, String> {
    let response = machine
        .send(reqwest::Method::GET, path, None, MACHINE_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = read_capped(response, MAX_MACHINE_BODY).await?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("no activity journal there (an older flywheel hub?)".into());
    }
    if !status.is_success() {
        return Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            crate::net::error_text(&body)
        ));
    }
    serde_json::from_slice(&body).map_err(|e| format!("unexpected answer: {e}"))
}

/// The machines a request fans out to (none for a peer or `machines=none`).
fn targets(app: &AppState, query: &ActivityQuery, peer: bool) -> Vec<Arc<Machine>> {
    if peer || query.machines.as_deref() == Some("none") {
        return Vec::new();
    }
    app.machines.list()
}

/// Ask each of `machines` for `path_of(machine)` at once: (name, answer).
async fn ask_all(
    machines: Vec<Arc<Machine>>,
    path_of: impl Fn(&str) -> Option<String>,
) -> Vec<(String, Result<Value, String>)> {
    let mut set = tokio::task::JoinSet::new();
    let mut order = Vec::new();
    for machine in machines {
        let name = machine.name().to_owned();
        let Some(path) = path_of(&name) else { continue };
        order.push(name.clone());
        set.spawn(async move {
            let answer = tokio::time::timeout(MACHINE_TIMEOUT, machine_get(&machine, &path))
                .await
                .unwrap_or_else(|_| Err("timed out".into()));
            (name, answer)
        });
    }
    let mut answers = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(answer) = joined {
            answers.push(answer);
        }
    }
    answers.sort_by_key(|(name, _)| order.iter().position(|n| n == name));
    answers
}

fn machine_status(name: &str, answer: &Result<Value, String>) -> Value {
    match answer {
        Ok(_) => json!({ "name": name, "ok": true }),
        Err(error) => json!({ "name": name, "ok": false, "error": error }),
    }
}

/// `GET /api/activity`: rows newest first across this hub and its
/// machines; `next` pages on (`cursor=`), `null` at the end.
pub(super) async fn list(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
    ApiQuery(query): ApiQuery<ActivityQuery>,
) -> ApiResult<Response> {
    let now = Utc::now();
    let filter = match filter_of(&query, now) {
        Ok(filter) => filter,
        Err(message) => return Ok(bad(&message)),
    };
    let limit = match query.limit.as_deref() {
        None | Some("") => DEFAULT_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_LIMIT).contains(&n) => n,
            _ => return Ok(bad(&format!("limit must be 1-{MAX_LIMIT}"))),
        },
    };
    let mut cursor = match query.cursor.as_deref() {
        None | Some("") => Cursor::default(),
        Some(raw) => match Cursor::parse(raw) {
            Ok(cursor) => cursor,
            Err(message) => return Ok(bad(&message)),
        },
    };
    // (source, rows newest first, more past them)
    let mut sources: Vec<(String, Vec<Value>, bool)> = Vec::new();
    let local_before = cursor.before(LOCAL);
    if local_before != Some(0) {
        let (rows, more) = app.hub.activity().page(&filter, local_before, limit);
        let rows = rows.iter().map(|r| json!(r)).collect();
        sources.push((LOCAL.to_owned(), rows, more));
    }
    let machines = targets(&app, &query, peer.is_some());
    let answers = ask_all(machines, |name| match cursor.before(name) {
        Some(0) => None,
        before => Some(format!(
            "/api/activity?{}",
            machine_query(&filter, limit, before)
        )),
    })
    .await;
    let mut statuses = Vec::new();
    let mut failed = Vec::new();
    for (name, answer) in answers {
        statuses.push(machine_status(&name, &answer));
        match answer {
            Ok(answer) => {
                let rows: Vec<Value> = answer
                    .get("rows")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| serde_json::from_value::<Row>(r.clone()).is_ok())
                    .map(|mut r| {
                        r["machine"] = Value::String(name.clone());
                        r
                    })
                    .take(limit)
                    .collect();
                let more = answer.get("next").is_some_and(|n| !n.is_null());
                sources.push((name, rows, more));
            }
            Err(_) => failed.push(name),
        }
    }
    // Merge: newest first; the same instant in source order.
    let mut merged: Vec<(usize, &Value)> = sources
        .iter()
        .enumerate()
        .flat_map(|(i, (_, rows, _))| rows.iter().map(move |r| (i, r)))
        .collect();
    let at = |r: &Value| {
        r.get("at")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_micros())
            .unwrap_or(0)
    };
    merged.sort_by(|(ia, a), (ib, b)| at(b).cmp(&at(a)).then(ia.cmp(ib)));
    merged.truncate(limit);
    let mut taken = vec![0usize; sources.len()];
    for (i, _) in &merged {
        taken[*i] += 1;
    }
    for (i, (name, rows, more)) in sources.iter().enumerate() {
        let id_of = |r: &Value| r.get("id").and_then(Value::as_u64).unwrap_or(0);
        if taken[i] == rows.len() && !more {
            cursor.set(name, 0);
        } else if taken[i] > 0 {
            cursor.set(name, id_of(&rows[taken[i] - 1]));
        }
    }
    let rows: Vec<Value> = merged.into_iter().map(|(_, r)| r.clone()).collect();
    // The end: every source that answered is spent (one that did not is
    // only named; paging on would ask it again).
    let spent = sources
        .iter()
        .all(|(name, _, _)| cursor.before(name) == Some(0));
    let next = (!spent && !cursor.exhausted()).then(|| cursor.encode());
    Ok(Json(json!({
        "rows": rows,
        "next": next,
        "partial": !failed.is_empty(),
        "machines": statuses,
    }))
    .into_response())
}

/// `GET /api/activity/digest`: what happened over `since..until`
/// (`since=last-seen` with `client`: since that client was last active;
/// default the last 24 hours), merged across machines.
pub(super) async fn digest(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
    ApiQuery(query): ApiQuery<ActivityQuery>,
) -> ApiResult<Response> {
    let now = Utc::now();
    let last_seen = query.since.as_deref() == Some("last-seen");
    let mut filter = match filter_of(
        &ActivityQuery {
            since: None,
            until: query.until.clone(),
            ws: query.ws.clone(),
            session: query.session.clone(),
            ..ActivityQuery::default()
        },
        now,
    ) {
        Ok(filter) => filter,
        Err(message) => return Ok(bad(&message)),
    };
    let mut seen_before = false;
    filter.since = if last_seen {
        let Some(client) = query.client.as_deref().filter(|c| valid_client(c)) else {
            return Ok(bad(
                "since=last-seen needs client (1-64 of A-Z a-z 0-9 - _)",
            ));
        };
        let mark = app.hub.activity().last_active(client);
        seen_before = mark.is_some();
        mark
    } else {
        match query.since.as_deref().filter(|r| !r.is_empty()) {
            Some(raw) => match parse_time(raw, now) {
                Ok(at) => Some(at),
                Err(message) => return Ok(bad(&message)),
            },
            None => None,
        }
    };
    filter.since = filter
        .since
        .or_else(|| Some(now - chrono::Duration::days(1)));
    filter.until = filter.until.or(Some(now));
    let hub = Arc::clone(&app.hub);
    let local_filter = filter.clone();
    let mut digest =
        super::blocking(move || Ok(hub.activity().digest(&hub, &local_filter))).await?;
    let machines = targets(&app, &query, peer.is_some());
    let path = {
        let mut q = format!(
            "machines=none&since={}&until={}",
            query_escape(&digest.since.to_rfc3339()),
            query_escape(&digest.until.to_rfc3339())
        );
        if let Some(ws) = &filter.workspace {
            q.push_str(&format!("&ws={}", query_escape(ws)));
        }
        if let Some(session) = &filter.session {
            q.push_str(&format!("&session={}", query_escape(session)));
        }
        format!("/api/activity/digest?{q}")
    };
    let answers = ask_all(machines, |_| Some(path.clone())).await;
    let mut statuses = Vec::new();
    let mut partial = false;
    for (name, answer) in answers {
        let parsed = answer.and_then(|v| {
            serde_json::from_value::<Digest>(v).map_err(|e| format!("unexpected answer: {e}"))
        });
        match parsed {
            Ok(other) => {
                statuses.push(machine_status(&name, &Ok(Value::Null)));
                digest.merge(other, &name);
            }
            Err(error) => {
                statuses.push(machine_status(&name, &Err(error)));
                partial = true;
            }
        }
    }
    let mut body = json!(digest);
    body["empty"] = json!(digest.is_empty());
    body["partial"] = json!(partial);
    body["machines"] = json!(statuses);
    if last_seen {
        // Never seen: the last 24 hours stand in.
        body["last_seen"] = json!(seen_before);
    }
    Ok(Json(body).into_response())
}

#[derive(Deserialize)]
pub(super) struct SeenRequest {
    client: String,
}

/// `POST /api/activity/seen` `{"client"}`: that client saw everything up
/// to now (its next `since=last-seen` digest starts here).
pub(super) async fn seen(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<SeenRequest>,
) -> ApiResult<Response> {
    if !valid_client(&request.client) {
        return Ok(bad("client must be 1-64 of A-Z a-z 0-9 - _"));
    }
    app.hub.activity().mark_seen(&request.client);
    Ok(StatusCode::NO_CONTENT.into_response())
}
