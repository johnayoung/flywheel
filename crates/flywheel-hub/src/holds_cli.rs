//! `fw hub holds` (what waits on you, grouped; answer one or a group) and
//! `fw hub decisions` (every answer kept): see [`crate::holds`].

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{fail, parse_duration, print_json};
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct HoldsArgs {
    /// Only holds of this kind: needs_input, awaiting_approval,
    /// permission, gate.
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// Answer the HOLDs named (an id, or a group's number as listed):
    /// approve, decline, yes (a permission prompt, once), deny[: FEEDBACK]
    /// (a gate), or any reply (an ask).
    #[arg(long, value_name = "ANSWER", requires = "holds")]
    answer: Option<String>,
    /// With --answer: a hold left out of the group, or answered its own
    /// way (`ID=ANSWER`); repeatable.
    #[arg(long, value_name = "ID[=ANSWER]", requires = "answer")]
    except: Vec<String>,
    /// With --answer: the holds or group numbers to answer.
    holds: Vec<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
pub(crate) struct DecisionsArgs {
    /// Only decisions of this kind: needs_input, awaiting_approval,
    /// permission, gate.
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// Only one session's (id or name).
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
    /// Only those this recent (`2h`, `7d`) or since a time (RFC 3339).
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,
    /// The most listed (newest first; 1-1000).
    #[arg(long, value_name = "N", default_value_t = 100)]
    limit: usize,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// A non-empty string field.
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `--since` as Unix ms.
fn since_ms(raw: &str, now: i64) -> Result<i64, String> {
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
        return Ok(at.timestamp_millis());
    }
    let seconds = parse_duration(raw)
        .map_err(|e| format!("--since {raw:?}: a duration or RFC 3339 time ({e})"))?;
    Ok(now - i64::try_from(seconds).unwrap_or(i64::MAX / 1000) * 1000)
}

fn local_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || "?".to_owned(),
        |t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::holds::cap(&line, max)
}

pub(crate) fn decisions(args: &DecisionsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut run = || -> Result<(), String> {
        let mut params = vec![format!("limit={}", args.limit.clamp(1, 1000))];
        if let Some(kind) = &args.kind {
            params.push(format!("kind={}", query_escape(kind)));
        }
        if let Some(session) = &args.session {
            params.push(format!("session={}", query_escape(session)));
        }
        if let Some(since) = &args.since {
            let now = chrono::Utc::now().timestamp_millis();
            params.push(format!("since={}", since_ms(since, now)?));
        }
        let conn = Endpoint::resolve(&args.endpoint)?;
        let found =
            conn.blocking_call("GET", &format!("/api/decisions?{}", params.join("&")), None)?;
        if args.json {
            print_json(out, &found);
        } else {
            print_decisions(out, &found);
        }
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn print_decisions(out: &mut dyn Write, found: &Value) {
    let list = found["decisions"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        let _ = writeln!(out, "No decisions recorded.");
        return;
    }
    for d in &list {
        let who = field(d, "session_name")
            .or_else(|| field(d, "session"))
            .unwrap_or("-");
        let batch = field(d, "batch").map_or_else(String::new, |b| format!("  batch {b}"));
        let _ = writeln!(
            out,
            "{}  {}  {}  by {}{batch}",
            local_time(d["at"].as_i64().unwrap_or(0)),
            field(d, "kind").unwrap_or("?"),
            who,
            field(d, "by").unwrap_or("?"),
        );
        let _ = writeln!(
            out,
            "  asked:  {}",
            one_line(field(d, "question").unwrap_or(""), 160)
        );
        if let Some(rec) = field(d, "recommendation") {
            let _ = writeln!(out, "  rec:    {}", one_line(rec, 160));
        }
        let _ = writeln!(
            out,
            "  answer: {}",
            one_line(field(d, "answer").unwrap_or(""), 160)
        );
    }
}

/// The hold ids `names` stands for: an id as is, a number as the group
/// listed with it (1-based).
fn resolve_holds(names: &[String], groups: &[Value]) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for name in names {
        if let Ok(n) = name.parse::<usize>() {
            let group = n
                .checked_sub(1)
                .and_then(|i| groups.get(i))
                .ok_or_else(|| format!("no group {n} (fw hub holds lists them)"))?;
            for hold in group["holds"].as_array().into_iter().flatten() {
                if let Some(id) = field(hold, "id") {
                    ids.push(id.to_owned());
                }
            }
        } else {
            ids.push(name.clone());
        }
    }
    Ok(ids)
}

pub(crate) fn holds(args: &HoldsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut run = || -> Result<(), String> {
        let conn = Endpoint::resolve(&args.endpoint)?;
        let path = match &args.kind {
            Some(kind) => format!("/api/holds?kind={}", query_escape(kind)),
            None => "/api/holds".to_owned(),
        };
        let listed = conn.blocking_call("GET", &path, None)?;
        let Some(answer) = &args.answer else {
            if args.json {
                print_json(out, &listed);
            } else {
                print_holds(out, &listed);
            }
            return Ok(());
        };
        let groups = listed["groups"].as_array().cloned().unwrap_or_default();
        let ids = resolve_holds(&args.holds, &groups)?;
        let mut overrides = serde_json::Map::new();
        for except in &args.except {
            let (id, own) = except
                .split_once('=')
                .map_or((except.as_str(), crate::holds::SKIP), |(i, a)| (i, a));
            overrides.insert(id.to_owned(), Value::String(own.to_owned()));
        }
        let done = conn.blocking_call(
            "POST",
            "/api/holds/answer",
            Some(&json!({ "holds": ids, "answer": answer, "overrides": overrides })),
        )?;
        if args.json {
            print_json(out, &done);
        } else {
            for r in done["results"].as_array().into_iter().flatten() {
                let hold = field(r, "hold").unwrap_or("?");
                match field(r, "error") {
                    Some(e) => {
                        let _ = writeln!(out, "{hold}: not answered: {e}");
                    }
                    None => {
                        let _ = writeln!(out, "{hold}: {}", field(r, "said").unwrap_or("ok"));
                    }
                }
            }
        }
        let failed = done["results"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["ok"] == false));
        if failed {
            return Err("some holds were not answered".into());
        }
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn print_holds(out: &mut dyn Write, listed: &Value) {
    let groups = listed["groups"].as_array().cloned().unwrap_or_default();
    if groups.is_empty() {
        let _ = writeln!(out, "Nothing waits on you.");
        return;
    }
    for (i, group) in groups.iter().enumerate() {
        let holds = group["holds"].as_array().cloned().unwrap_or_default();
        let _ = writeln!(
            out,
            "{}. {} ({}): {}",
            i + 1,
            field(group, "kind").unwrap_or("?"),
            holds.len(),
            field(group, "cause").unwrap_or(""),
        );
        for hold in &holds {
            let answers = hold["answers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .filter(|a| !a.is_empty())
                .map_or_else(String::new, |a| format!("  [{a}]"));
            let _ = writeln!(
                out,
                "   {}  {}{answers}",
                field(hold, "id").unwrap_or("?"),
                one_line(field(hold, "question").unwrap_or(""), 140),
            );
            if let Some(rec) = field(hold, "recommendation") {
                let _ = writeln!(out, "     recommends: {}", one_line(rec, 120));
            }
        }
    }
    let _ = writeln!(
        out,
        "\nAnswer a group: fw hub holds --answer approve 1 [--except ID[=ANSWER]]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_duration_or_a_time() {
        assert_eq!(since_ms("2h", 10_000_000).unwrap(), 10_000_000 - 7_200_000);
        assert_eq!(
            since_ms("2026-11-19T00:00:00Z", 0).unwrap(),
            chrono::DateTime::parse_from_rfc3339("2026-11-19T00:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
        assert!(since_ms("soon", 0).is_err());
    }

    #[test]
    fn a_group_number_stands_for_its_holds() {
        let groups = vec![
            json!({"holds": [{"id": "start:a:1"}, {"id": "start:a:2"}]}),
            json!({"holds": [{"id": "ask:b:3"}]}),
        ];
        assert_eq!(
            resolve_holds(&["1".into(), "perm:c:k".into()], &groups).unwrap(),
            vec!["start:a:1", "start:a:2", "perm:c:k"]
        );
        assert!(resolve_holds(&["3".into()], &groups).is_err());
    }

    #[test]
    fn decisions_print_question_recommendation_answer_and_who() {
        let mut out = Vec::new();
        print_decisions(
            &mut out,
            &json!({"decisions": [{"id": 1, "hold": "ask:s:1", "kind": "needs_input",
                "cause": "db", "session": "s", "session_name": "worker", "question": "Which db?",
                "recommendation": "sqlite", "answer": "postgres", "by": "operator", "at": 0,
                "batch": "b1"}]}),
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("needs_input  worker  by operator  batch b1"),
            "{text}"
        );
        assert!(text.contains("asked:  Which db?"), "{text}");
        assert!(text.contains("rec:    sqlite"), "{text}");
        assert!(text.contains("answer: postgres"), "{text}");
    }
}
