//! The activity journal over HTTP (see [`crate::activity`]): `GET
//! /api/activity` (the timeline, cursor-paged), `GET
//! /api/activity/digest` ("while you were away"), and `POST
//! /api/activity/seen`. A full-scope request also asks every registered
//! machine (its own `/api/activity...` with `machines=none`, each within
//! [`MACHINE_TIMEOUT`]) and merges the answers, machine-labelled; a
//! machine that does not answer is named in `machines` and makes the
//! answer `partial`. A peer's request is answered from this hub alone.

use super::{ApiJson, ApiQuery, ApiResult, AppState, error_response};
use crate::activity::{
    Cursor, DEFAULT_LIMIT, Digest, Filter, LOCAL, MAX_LIMIT, Row, known_kind, parse_time,
};
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

#[derive(Deserialize, Default, Clone)]
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

/// The filter `query` asks for, as given (the selectors a machine
/// resolves itself), and whether `since=last-seen` found its client's mark
/// (`None`: not asked; never seen: the last 24 hours stand in).
fn filter_of(
    app: &AppState,
    query: &ActivityQuery,
    now: DateTime<Utc>,
) -> Result<(Filter, Option<bool>), String> {
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
    if let Some(unknown) = kinds.iter().find(|k| !known_kind(k)) {
        return Err(format!(
            "unknown kind {unknown:?} (a row kind such as turn.finished, or a family such as review)"
        ));
    }
    let id = |raw: &Option<String>| {
        raw.as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
    };
    let (since, seen) = if query.since.as_deref() == Some("last-seen") {
        let Some(client) = query.client.as_deref().filter(|c| valid_client(c)) else {
            return Err("since=last-seen needs client (1-64 of A-Z a-z 0-9 - _)".into());
        };
        let mark = app.hub.activity().last_active(client);
        (
            Some(mark.unwrap_or(now - chrono::Duration::days(1))),
            Some(mark.is_some()),
        )
    } else {
        (time(&query.since)?, None)
    };
    Ok((
        Filter {
            since,
            until: time(&query.until)?,
            workspace: id(&query.ws),
            session: id(&query.session),
            kinds,
        },
        seen,
    ))
}

/// What a request's selectors found on this hub.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Matched {
    workspace: bool,
    session: bool,
}

impl Matched {
    /// The first selector of `filter` nothing found, as an error.
    fn error(self, filter: &Filter) -> Option<String> {
        let none = |what: &str, query: &str| {
            format!("no {what} matches {query:?} (id, id prefix, or name)")
        };
        match (&filter.workspace, &filter.session) {
            (Some(ws), _) if !self.workspace => Some(none("workspace", ws)),
            (_, Some(session)) if !self.session => Some(none("session", session)),
            _ => None,
        }
    }

    fn json(self) -> Value {
        json!({ "workspace": self.workspace, "session": self.session })
    }

    /// What machine `answer` says its selectors found (a hub from before
    /// `matched` is taken to have found them).
    fn of_answer(answer: &Value) -> Self {
        let found = |key: &str| {
            answer
                .get("matched")
                .and_then(|m| m.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(true)
        };
        Self {
            workspace: found("workspace"),
            session: found("session"),
        }
    }

    fn or(self, other: Self) -> Self {
        Self {
            workspace: self.workspace || other.workspace,
            session: self.session || other.session,
        }
    }
}

/// `filter` with its workspace and session resolved against this hub by
/// the one resolver ([`crate::script::find_by`]): a live one becomes its
/// id. One naming nothing live stays as given (rows outlive what they
/// name; a machine resolves it for itself) and is found only if a row
/// here names it. One naming several is an error.
fn localize(app: &AppState, filter: &Filter) -> Result<(Filter, Matched), String> {
    let mut local = filter.clone();
    let mut matched = Matched::default();
    let in_journal = |workspace: Option<&String>, session: Option<&String>| {
        let probe = Filter {
            workspace: workspace.cloned(),
            session: session.cloned(),
            ..Filter::default()
        };
        !app.hub.activity().page(&probe, None, 1).0.is_empty()
    };
    if let Some(ws) = &filter.workspace {
        let items: Vec<(String, String)> = app
            .hub
            .workspaces()
            .into_iter()
            .map(|w| (w.id, w.name))
            .collect();
        match crate::script::find_by(&items, |(id, name)| (id, name), ws, "workspace")? {
            Some((id, _)) => {
                local.workspace = Some(id.clone());
                matched.workspace = true;
            }
            None => matched.workspace = in_journal(Some(ws), None),
        }
    }
    if let Some(session) = &filter.session {
        let items: Vec<(String, String)> = app
            .hub
            .sessions(None)
            .into_iter()
            .map(|v| (v.record.id, v.record.name))
            .collect();
        match crate::script::find_by(&items, |(id, name)| (id, name), session, "session")? {
            Some((id, _)) => {
                local.session = Some(id.clone());
                matched.session = true;
            }
            None => matched.session = in_journal(None, Some(session)),
        }
    }
    Ok((local, matched))
}

/// For a request that asks its machines (`asked`), the error when a
/// selector found nothing here nor on any machine that answered: none
/// when one did, or when a machine did not answer (it might have).
fn unmatched_anywhere(
    filter: &Filter,
    here: Matched,
    answers: &[(String, Result<Value, String>)],
) -> Option<String> {
    if answers.iter().any(|(_, a)| a.is_err()) {
        return None;
    }
    let found = answers
        .iter()
        .filter_map(|(_, a)| a.as_ref().ok())
        .fold(here, |m, a| m.or(Matched::of_answer(a)));
    found.error(filter)
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

/// Whether this hub answers for another (a peer, or a merging hub's
/// `machines=none`): its selectors finding nothing here is no error, only
/// said in `matched`.
fn for_another(query: &ActivityQuery, peer: bool) -> bool {
    peer || query.machines.as_deref() == Some("none")
}

/// For a request answered here alone (no machines to ask, and not for
/// another), the error when a selector found nothing.
fn unmatched_alone(
    query: &ActivityQuery,
    peer: bool,
    machines: &[Arc<Machine>],
    filter: &Filter,
    matched: Matched,
) -> Option<String> {
    if for_another(query, peer) || !machines.is_empty() {
        return None;
    }
    matched.error(filter)
}

fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, message)
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

/// The first `limit` rows of machine `name`'s answer, labelled with it:
/// only what parses as a row, written out again from the parse (serde
/// takes an array in field order as a row too).
fn machine_rows(answer: &Value, name: &str, limit: usize) -> Vec<Value> {
    answer
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| serde_json::from_value::<Row>(r.clone()).ok())
        .take(limit)
        .map(|r| {
            let mut r = json!(r.scrubbed());
            r["machine"] = Value::String(name.to_owned());
            r
        })
        .collect()
}

/// `GET /api/activity`: rows newest first across this hub and its
/// machines; `next` pages on (`cursor=`), `null` at the end.
pub(super) async fn list(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
    ApiQuery(query): ApiQuery<ActivityQuery>,
) -> ApiResult<Response> {
    let now = Utc::now();
    // As asked (what each machine resolves for itself), and resolved here.
    let (filter, local, matched) = match filter_of(&app, &query, now)
        .and_then(|(filter, _)| Ok((localize(&app, &filter)?, filter)))
    {
        Ok(((local, matched), filter)) => (filter, local, matched),
        Err(message) => return Ok(bad(&message)),
    };
    let machines = targets(&app, &query, peer.is_some());
    if let Some(message) = unmatched_alone(&query, peer.is_some(), &machines, &filter, matched) {
        return Ok(not_found(&message));
    }
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
        let (rows, more) = app.hub.activity().page(&local, local_before, limit);
        let rows = rows.iter().map(|r| json!(r)).collect();
        sources.push((LOCAL.to_owned(), rows, more));
    }
    let answers = ask_all(machines, |name| match cursor.before(name) {
        Some(0) => None,
        before => Some(format!(
            "/api/activity?{}",
            machine_query(&filter, limit, before)
        )),
    })
    .await;
    if !for_another(&query, peer.is_some())
        && let Some(message) = unmatched_anywhere(&filter, matched, &answers)
    {
        return Ok(not_found(&message));
    }
    let mut statuses = Vec::new();
    let mut failed = Vec::new();
    for (name, answer) in answers {
        statuses.push(machine_status(&name, &answer));
        match answer {
            Ok(answer) => {
                let rows = machine_rows(&answer, &name, limit);
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
        "matched": matched.json(),
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
    let (mut filter, seen_before) = match filter_of(
        &app,
        &ActivityQuery {
            kind: None,
            ..query.clone()
        },
        now,
    ) {
        Ok(found) => found,
        Err(message) => return Ok(bad(&message)),
    };
    filter.since = filter
        .since
        .or_else(|| Some(now - chrono::Duration::days(1)));
    filter.until = filter.until.or(Some(now));
    let (local_filter, matched) = match localize(&app, &filter) {
        Ok(local) => local,
        Err(message) => return Ok(bad(&message)),
    };
    let machines = targets(&app, &query, peer.is_some());
    if let Some(message) = unmatched_alone(&query, peer.is_some(), &machines, &filter, matched) {
        return Ok(not_found(&message));
    }
    let hub = Arc::clone(&app.hub);
    let mut digest =
        super::blocking(move || Ok(hub.activity().digest(&hub, &local_filter))).await?;
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
    if !for_another(&query, peer.is_some())
        && let Some(message) = unmatched_anywhere(&filter, matched, &answers)
    {
        return Ok(not_found(&message));
    }
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
    body["matched"] = matched.json();
    if let Some(seen_before) = seen_before {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_rows_are_relabelled_from_their_parse_and_odd_shapes_are_dropped() {
        let answer = json!({ "rows": [
            [1, "2026-11-03T00:00:00Z", "usage", null, null, null, null, null, "x"],
            { "id": 2, "at": "2026-11-03T00:00:01Z", "kind": "usage", "text": "ok", "extra": 1 },
            "junk",
            { "id": 3 },
        ], "next": null });
        let rows = machine_rows(&answer, "boxb", 10);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.is_object() && r["machine"] == "boxb"));
        assert_eq!(rows[1]["text"], "ok");
        assert!(rows[1].get("extra").is_none(), "only a row's own fields");
        assert_eq!(machine_rows(&answer, "boxb", 1).len(), 1);
        assert!(machine_rows(&json!({ "rows": 5 }), "boxb", 10).is_empty());
    }

    #[test]
    fn a_selector_unmatched_here_and_on_every_machine_is_an_error() {
        let filter = Filter {
            workspace: Some("nowhere".into()),
            ..Filter::default()
        };
        let here = Matched::default();
        let missed = |m: &str| {
            (
                m.to_owned(),
                Ok(json!({ "matched": { "workspace": false } })),
            )
        };
        let found = (
            "c".to_owned(),
            Ok(json!({ "matched": { "workspace": true } })),
        );
        let old = ("d".to_owned(), Ok(json!({ "rows": [] })));
        let down = ("e".to_owned(), Err("timed out".to_owned()));
        assert_eq!(
            unmatched_anywhere(&filter, here, &[missed("a"), missed("b")]).as_deref(),
            Some(r#"no workspace matches "nowhere" (id, id prefix, or name)"#)
        );
        assert!(unmatched_anywhere(&filter, here, &[]).is_some());
        assert!(unmatched_anywhere(&filter, here, &[missed("a"), found]).is_none());
        // A hub from before `matched`, or one that did not answer, might
        // have it.
        assert!(unmatched_anywhere(&filter, here, &[missed("a"), old]).is_none());
        assert!(unmatched_anywhere(&filter, here, &[missed("a"), down]).is_none());
        let here_found = Matched {
            workspace: true,
            session: false,
        };
        assert!(unmatched_anywhere(&filter, here_found, &[missed("a")]).is_none());
        let both = Filter {
            session: Some("s9".into()),
            ..filter
        };
        assert_eq!(
            unmatched_anywhere(&both, here_found, &[]).as_deref(),
            Some(r#"no session matches "s9" (id, id prefix, or name)"#)
        );
    }
}
