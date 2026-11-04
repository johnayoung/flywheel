use super::feed::draft_for;
use super::*;
use chrono::{Duration as Span, TimeZone};
use serde_json::json;

fn at(minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 3, 8, 0, 0).unwrap() + Span::minutes(minute)
}

fn draft(kind: &str, ws: &str, session: &str) -> Draft {
    Draft::new(kind, format!("{kind} in {session}")).about(&json!({
        "id": format!("id-{session}"),
        "name": session,
        "kind": "claude",
        "workspace_id": format!("w-{ws}"),
        "workspace": ws,
    }))
}

fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn rows_append_survive_a_reopen_and_page_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path(), Limits::default(), at(0));
    for i in 0..5 {
        journal.append(draft("turn.finished", "a", &format!("s{i}")), at(i));
    }
    let path = dir.path().join(JOURNAL_FILE);
    assert_eq!(mode(&path), 0o600);
    drop(journal);
    let mut journal = Journal::open(dir.path(), Limits::default(), at(10));
    assert_eq!(journal.len(), 5);
    assert_eq!(journal.next_id(), 6);
    let row = journal.append(draft("usage", "a", "s9"), at(11));
    assert_eq!(row.id, 6);
    let (page, more) = journal.page(&Filter::default(), None, 4);
    assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), [6, 5, 4, 3]);
    assert!(more);
    let (page, more) = journal.page(&Filter::default(), Some(3), 4);
    assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), [2, 1]);
    assert!(!more);
    // A torn last line (a crash mid-write) loses only that row.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"id\":7,\"at\""))
        .unwrap();
    let journal = Journal::open(dir.path(), Limits::default(), at(12));
    assert_eq!(journal.len(), 6);
}

#[test]
fn the_journal_rotates_by_size_keeps_its_files_and_forgets_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        file_cap: 2_000,
        keep_rotated: 2,
        rotate_age: Span::days(1),
        max_age: Span::days(30),
    };
    let mut journal = Journal::open(dir.path(), limits, at(0));
    for i in 0..200 {
        journal.append(draft("turn.finished", "a", &format!("s{i}")), at(i));
    }
    let files: Vec<u64> = [JOURNAL_FILE, "activity.1.jsonl", "activity.2.jsonl"]
        .iter()
        .map(|f| std::fs::metadata(dir.path().join(f)).unwrap().len())
        .collect();
    assert!(files.iter().all(|len| *len <= 2_000), "{files:?}");
    assert!(!dir.path().join("activity.3.jsonl").exists());
    assert_eq!(mode(&dir.path().join("activity.2.jsonl")), 0o600);
    // Memory holds what the files hold, no more.
    let on_disk: usize = [JOURNAL_FILE, "activity.1.jsonl", "activity.2.jsonl"]
        .iter()
        .map(|f| {
            std::fs::read_to_string(dir.path().join(f))
                .unwrap()
                .lines()
                .count()
        })
        .sum();
    assert_eq!(journal.len(), on_disk);
    assert!(on_disk < 200);
    let (newest, _) = journal.page(&Filter::default(), None, 1);
    assert_eq!(newest[0].id, 200);
    let reopened = Journal::open(dir.path(), limits, at(300));
    assert_eq!(reopened.len(), on_disk);
    assert_eq!(reopened.next_id(), 201);
}

#[test]
fn the_journal_rotates_by_age_and_drops_rows_past_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        file_cap: 1 << 20,
        keep_rotated: 3,
        rotate_age: Span::days(1),
        max_age: Span::days(3),
    };
    let day = |d: i64| at(0) + Span::days(d);
    let mut journal = Journal::open(dir.path(), limits, day(0));
    journal.append(draft("turn.finished", "a", "old"), day(0));
    journal.append(draft("turn.finished", "a", "old2"), day(0) + Span::hours(2));
    // A day on: the first file rotates.
    journal.append(draft("turn.finished", "a", "mid"), day(1) + Span::hours(1));
    assert!(dir.path().join("activity.1.jsonl").exists());
    // Four days on: the first rows are past the age limit, their file goes.
    journal.append(draft("turn.finished", "a", "new"), day(4));
    assert_eq!(journal.len(), 2, "only rows within 3 days stay");
    let (rows, _) = journal.page(&Filter::default(), None, 10);
    assert_eq!(
        rows.iter()
            .map(|r| r.session.clone().unwrap())
            .collect::<Vec<_>>(),
        ["new", "mid"]
    );
    let reopened = Journal::open(dir.path(), limits, day(4) + Span::minutes(30));
    assert_eq!(reopened.len(), 2);
    // Reopened later still, everything but the newest is too old.
    let later = Journal::open(dir.path(), limits, day(6));
    assert_eq!(later.len(), 1);
}

#[test]
fn the_default_limits_keep_thirty_days_of_a_row_every_few_hours() {
    let dir = tempfile::tempdir().unwrap();
    let start = at(0);
    let mut journal = Journal::open(dir.path(), Limits::default(), start);
    // Six weeks of a row every four hours (well under the size cap).
    let mut now = start;
    while now < start + Span::days(42) {
        journal.append(draft("turn.finished", "a", "s"), now);
        now += Span::hours(4);
    }
    let last = now - Span::hours(4);
    let oldest = |j: &Journal| {
        let rows = j.matching(&Filter::default());
        rows.first().unwrap().at
    };
    for j in [
        &journal,
        &Journal::open(dir.path(), Limits::default(), last),
    ] {
        let kept = last - oldest(j);
        assert!(
            kept >= Span::days(30) - Span::hours(4) && kept <= Span::days(30),
            "kept {} hours",
            kept.num_hours()
        );
    }
    let since = Filter {
        since: Some(last - Span::days(14)),
        ..Filter::default()
    };
    assert_eq!(journal.matching(&since).len(), 14 * 6 + 1);
    assert!(
        !dir.path()
            .join(format!("activity.{}.jsonl", KEEP_ROTATED + 1))
            .exists()
    );
}

#[test]
fn a_gap_in_the_rotated_files_is_closed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        file_cap: 300,
        keep_rotated: 3,
        rotate_age: Span::days(1),
        max_age: Span::days(30),
    };
    let mut journal = Journal::open(dir.path(), limits, at(0));
    for i in 0..8 {
        journal.append(draft("turn.finished", "a", &format!("s{i}")), at(i));
    }
    drop(journal);
    assert!(dir.path().join("activity.3.jsonl").exists());
    std::fs::remove_file(dir.path().join("activity.1.jsonl")).unwrap();
    let journal = Journal::open(dir.path(), limits, at(20));
    assert!(dir.path().join("activity.1.jsonl").exists());
    assert!(dir.path().join("activity.2.jsonl").exists());
    assert!(!dir.path().join("activity.3.jsonl").exists());
    assert!(journal.len() < 8);
}

#[test]
fn filters_take_workspace_session_kind_prefixes_and_a_span() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path(), Limits::default(), at(0));
    journal.append(draft("review.merged", "a", "x"), at(1));
    journal.append(draft("review.requested", "a", "y"), at(2));
    journal.append(draft("turn.finished", "b", "x"), at(3));
    journal.append(draft("reviewer.odd", "a", "x"), at(4));
    let ids = |f: Filter| {
        journal
            .page(&f, None, 50)
            .0
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    };
    let kinds = |k: &[&str]| Filter {
        kinds: k.iter().map(|s| (*s).to_owned()).collect(),
        ..Filter::default()
    };
    assert_eq!(
        ids(kinds(&["review"])),
        [2, 1],
        "review covers review.* only"
    );
    assert_eq!(ids(kinds(&["review.merged", "turn.finished"])), [3, 1]);
    let ws = Filter {
        workspace: Some("w-a".into()),
        ..Filter::default()
    };
    assert_eq!(ids(ws), [4, 2, 1]);
    let session = Filter {
        session: Some("id-x".into()),
        ..Filter::default()
    };
    assert_eq!(ids(session), [4, 3, 1]);
    // A name or a 4+ character id prefix picks the same rows (a removed
    // session's, a machine's workspace); a short prefix picks nothing.
    let by = |ws: Option<&str>, session: Option<&str>| Filter {
        workspace: ws.map(str::to_owned),
        session: session.map(str::to_owned),
        ..Filter::default()
    };
    assert_eq!(ids(by(Some("a"), None)), [4, 2, 1]);
    assert_eq!(ids(by(None, Some("x"))), [4, 3, 1]);
    assert_eq!(ids(by(None, Some("id-"))), Vec::<u64>::new());
    assert_eq!(ids(by(Some("w-a"), Some("x"))), [4, 1]);
    assert!(known_kind("review") && known_kind("review.merged") && known_kind("usage"));
    assert!(!known_kind("nonsense") && !known_kind("rev") && !known_kind("review."));
    let span = Filter {
        since: Some(at(2)),
        until: Some(at(4)),
        ..Filter::default()
    };
    assert_eq!(ids(span), [3, 2], "since inclusive, until exclusive");
}

#[test]
fn times_parse_as_rfc3339_unix_ms_and_ages() {
    let now = at(600);
    assert_eq!(parse_time("2h", now).unwrap(), at(480));
    assert_eq!(parse_time("90s", now).unwrap(), now - Span::seconds(90));
    assert_eq!(parse_time("7d", now).unwrap(), now - Span::days(7));
    assert_eq!(parse_time("1w", now).unwrap(), now - Span::days(7));
    // The grammar share expiries take: compound, any case.
    assert_eq!(parse_time("1h30m", now).unwrap(), at(510));
    assert_eq!(parse_time("2H", now).unwrap(), at(480));
    assert_eq!(parse_time("2026-11-03T08:00:00Z", now).unwrap(), at(0));
    assert_eq!(
        parse_time(&at(5).timestamp_millis().to_string(), now).unwrap(),
        at(5)
    );
    for bad in ["", "h", "2y", "-1h", "400d", "soon"] {
        assert!(parse_time(bad, now).is_err(), "{bad}");
    }
}

#[test]
fn cursors_round_trip_and_track_each_source() {
    let mut cursor = Cursor::parse("local:12,laptop:0").unwrap();
    assert_eq!(cursor.before("local"), Some(12));
    assert_eq!(cursor.before("laptop"), Some(0));
    assert_eq!(cursor.before("desk"), None);
    assert!(!cursor.exhausted());
    cursor.set("local", 0);
    assert!(cursor.exhausted());
    cursor.set("desk", 4);
    assert_eq!(cursor.encode(), "local:0,laptop:0,desk:4");
    assert_eq!(Cursor::parse(&cursor.encode()).unwrap(), cursor);
    for bad in ["local", "local:x", ":3", "local:-1"] {
        assert!(Cursor::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn scrub_redacts_token_shaped_words_and_caps_length() {
    let fake = "ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3x";
    assert_eq!(scrub(&format!("pushed {fake} ok")), "pushed [redacted] ok");
    assert_eq!(scrub("sk-ant-api03-ZZZZZZZZZZZZZZZZ"), "[redacted]");
    assert_eq!(
        scrub("Authorization: Bearer abc.def"),
        "Authorization: Bearer [redacted]"
    );
    assert_eq!(scrub("token hunter2"), "token [redacted]");
    assert_eq!(scrub("Q2xhdWRlQ29kZTEyMzQ1Njc4OTBhYmNkZWY"), "[redacted]");
    // A flywheel run id is a name, not a secret.
    let run = "run-b51a35dbed1a4fbcac5197bd689af9c7";
    assert_eq!(scrub(&format!("parked {run}")), format!("parked {run}"));
    assert_eq!(
        scrub("run-B51A35DBED1A4FBCAC5197BD689AF9C7x9"),
        "[redacted]"
    );
    // The hub's own token: 64 lowercase hex.
    let token = "7f7df99d6fe8ccd92e959f45f54dd2996c108fd931141f408e499a364c30ab27";
    assert_eq!(scrub(&format!("deploy {token}")), "deploy [redacted]");
    // Names, branches, commits, and the hub's ids are not secrets.
    for keep in [
        "merged hub/41 into main",
        "feature/fix-the-flaky-login-test-2",
        "0123456789abcdef0123456789abcdef01234567",
        "3f2a9c1b8e7d4f6a9b0c1d2e3f4a5b6c",
        "agent-20261103T101112Z.cast",
    ] {
        assert_eq!(scrub(keep), keep);
    }
    assert_eq!(scrub("a\x1b[31mred\nline"), "a [31mred line");
    let long = "word ".repeat(100);
    let cut = scrub(&long);
    assert_eq!(cut.chars().count(), TEXT_CAP);
    assert!(cut.ends_with('…'));
}

#[test]
fn rows_from_another_hub_are_scrubbed_again() {
    let token = "7f7df99d6fe8ccd92e959f45f54dd2996c108fd931141f408e499a364c30ab27";
    let mut data = serde_json::Map::new();
    data.insert("name".into(), json!(format!("x {token}")));
    data.insert("nested".into(), json!({"prompt": "everything"}));
    data.insert("count".into(), json!(3));
    let row = Row {
        id: 1,
        at: at(0),
        kind: "session.exited".into(),
        session_id: Some("s1".into()),
        session: Some(format!("deploy {token}")),
        session_kind: None,
        workspace_id: None,
        workspace: Some("w".repeat(300)),
        text: format!("exited {token}"),
        data,
    }
    .scrubbed();
    assert_eq!(row.session.as_deref(), Some("deploy [redacted]"));
    assert_eq!(row.text, "exited [redacted]");
    assert_eq!(row.workspace.unwrap().chars().count(), TEXT_CAP);
    assert_eq!(row.data["name"], json!("x [redacted]"));
    assert_eq!(row.data["nested"], json!(null));
    assert_eq!(row.data["count"], json!(3));
}

#[test]
fn lifecycle_events_become_rows_with_whitelisted_facts_only() {
    let session = json!({"id": "s1", "name": "agent", "kind": "claude", "workspace": "ws", "workspace_id": "w1",
        "cwd": "/home/me/secret-project", "branch": "hub/41"});
    let merged = draft_for(
        "review.merged",
        &json!({"session": session, "dir": "/home/me/secret-project", "head": "abc", "branch": "hub/41",
            "base": "main", "summary": "ff main to abc", "url": null}),
    )
    .unwrap();
    assert_eq!(merged.text, "merged hub/41 into main");
    assert_eq!(merged.session_id.as_deref(), Some("s1"));
    assert_eq!(merged.workspace.as_deref(), Some("ws"));
    let all = serde_json::to_string(&merged.clone().into_row(1, at(0))).unwrap();
    assert!(!all.contains("/home/me"), "{all}");
    // A queued prompt's text never reaches the journal.
    let delivered = draft_for(
        "queue.delivered",
        &json!({"session": session, "item": {"id": "q1", "text": "deploy with key hunter2", "bytes": 23}, "pending": 1}),
    )
    .unwrap();
    let all = serde_json::to_string(&delivered.into_row(2, at(0))).unwrap();
    assert!(!all.contains("hunter2"), "{all}");
    // Only a change to blocked is a row (turns and the sidebar cover the rest).
    for status in ["working", "done", "idle", "unknown"] {
        assert!(
            draft_for(
                "status.changed",
                &json!({"session": session, "status": status, "previous": "blocked"})
            )
            .is_none()
        );
    }
    let blocked = draft_for(
        "status.changed",
        &json!({"session": session, "status": "blocked", "detail": "Bash: curl -H 'Authorization: Bearer xyz'"}),
    )
    .unwrap();
    let all = serde_json::to_string(&blocked.into_row(3, at(0))).unwrap();
    assert!(!all.contains("Bearer") && !all.contains("curl"), "{all}");
    // A fan-out's grader output stays out; its id, winner, counts stay in.
    let graded = draft_for(
        "fanout.finished",
        &json!({"fanout": {"id": "f1", "name": "fix-login", "workspace": "ws", "workspace_id": "w1",
            "leader": "b", "winner": null, "counts": {"candidates": 3, "passing": 2},
            "candidates": [{"label": "a", "tail": "assertion failed: SECRET_TAIL"}]}}),
    )
    .unwrap();
    assert_eq!(
        graded.text,
        "fan-out fix-login graded: b leads (2/3 passing)"
    );
    assert_eq!(graded.data["fanout_id"], "f1");
    assert_eq!(graded.workspace_id.as_deref(), Some("w1"));
    let all = serde_json::to_string(&graded.into_row(4, at(0))).unwrap();
    assert!(!all.contains("SECRET_TAIL"), "{all}");
    assert!(draft_for("not.an.event", &json!({})).is_none());
}

#[test]
fn every_plugin_event_has_a_row_shape() {
    for event in crate::plugins::EVENTS {
        let payload = json!({"session": {"id": "s", "name": "n"}, "status": "blocked",
            "fanout": {"id": "f", "name": "n", "counts": {}}});
        assert!(draft_for(event, &payload).is_some(), "{event}");
    }
}

#[test]
fn a_digest_counts_turns_commands_cost_fanouts_and_reviews() {
    let rows: Vec<Row> = [
        draft("turn.finished", "a", "x"),
        draft("turn.finished", "a", "y"),
        draft("commands.suggested", "a", "x").with("count", 3),
        draft("usage", "a", "x").with("cost_usd", 0.125),
        draft("usage", "b", "z").with("cost_usd", 1.5),
        draft("turn.finished", "b", "z"),
        draft("review.merged", "a", "x"),
        draft("review.requested", "a", "x"),
        draft("session.exited", "b", "z"),
        Draft::new("plugin.ran", "plugin p ran on hub.started: ok"),
        Draft::new("fanout.finished", "graded")
            .in_workspace(Some("w-a"), Some("a"))
            .with_id("fanout_id", Some("f1"))
            .with("name", "fix")
            .with("leader", "b")
            .with("passing", 1)
            .with("candidates", 3),
        Draft::new("fanout.finished", "graded again")
            .in_workspace(Some("w-a"), Some("a"))
            .with_id("fanout_id", Some("f1"))
            .with("name", "fix")
            .with("winner", "b")
            .with("passing", 2)
            .with("candidates", 3),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, d)| d.into_row(i as u64 + 1, at(i as i64)))
    .collect();
    let digest = Digest::of(&rows, at(0), at(60));
    assert_eq!(digest.turns_finished, 3);
    assert_eq!(digest.commands_suggested, 3);
    assert!(
        (digest.cost_usd - 1.625).abs() < 1e-9,
        "{}",
        digest.cost_usd
    );
    assert_eq!(digest.reviews, 1, "a review request is not a review action");
    assert_eq!(digest.exited, 1);
    assert_eq!(digest.events, 12);
    assert!(
        digest.workspaces.iter().all(|w| !w.workspace.is_empty()),
        "a hub-level row is no workspace's: {:?}",
        digest.workspaces
    );
    assert_eq!(digest.workspaces.len(), 2);
    assert_eq!(
        digest.fanouts_graded.len(),
        1,
        "the latest grading per fan-out"
    );
    assert_eq!(digest.fanouts_graded[0].winner.as_deref(), Some("b"));
    assert_eq!(digest.fanouts_graded[0].passing, 2);
    let a = digest
        .workspaces
        .iter()
        .find(|w| w.workspace == "a")
        .unwrap();
    assert_eq!((a.turns_finished, a.commands_suggested), (2, 3));
    let b = digest
        .workspaces
        .iter()
        .find(|w| w.workspace == "b")
        .unwrap();
    assert!((b.cost_usd - 1.5).abs() < 1e-9);
    assert!(!digest.is_empty());
    assert!(Digest::of(&[], at(0), at(1)).is_empty());

    let mut merged = digest.clone();
    merged.merge(digest.clone(), "laptop");
    assert_eq!(merged.turns_finished, 6);
    assert!((merged.cost_usd - 3.25).abs() < 1e-9);
    assert_eq!(merged.fanouts_graded.len(), 2);
    assert_eq!(merged.fanouts_graded[1].machine.as_deref(), Some("laptop"));
    assert!(
        merged
            .workspaces
            .iter()
            .any(|w| w.machine.as_deref() == Some("laptop"))
    );
}

#[test]
fn presence_is_per_client_and_a_return_after_a_while_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let away = Span::minutes(10);
    let mut marks = Marks::load(dir.path());
    // Never seen: no digest on a first visit.
    assert_eq!(marks.set_present("laptop", 1, true, at(0), away), None);
    assert_eq!(
        marks.last_active("laptop", at(5)),
        Some(at(5)),
        "now while present"
    );
    assert_eq!(marks.set_present("laptop", 1, false, at(30), away), None);
    assert_eq!(marks.last_active("laptop", at(90)), Some(at(30)));
    // Back 5 minutes later: not away long enough.
    assert_eq!(marks.set_present("laptop", 1, true, at(35), away), None);
    marks.disconnect("laptop", 1, at(40));
    // A second tab of the same client: present while either is.
    marks.set_present("phone", 7, true, at(40), away);
    marks.set_present("phone", 8, true, at(41), away);
    marks.disconnect("phone", 7, at(42));
    assert!(marks.is_present("phone"));
    marks.disconnect("phone", 8, at(43));
    assert!(!marks.is_present("phone"));
    marks.save(at(44), true);
    let path = dir.path().join(MARKS_FILE);
    assert_eq!(mode(&path), 0o600);

    // A restart keeps the marks: each client gets its own span back.
    let mut marks = Marks::load(dir.path());
    let laptop = marks.set_present("laptop", 1, true, at(100), away).unwrap();
    assert_eq!((laptop.since, laptop.until), (at(40), at(100)));
    let phone = marks.set_present("phone", 2, true, at(101), away).unwrap();
    assert_eq!(phone.since, at(43), "active until its last tab closed");
    // Only once: still present, no second return.
    assert_eq!(marks.set_present("laptop", 3, true, at(200), away), None);
    // The CLI's mark moves only when it says so.
    marks.touch("cli", at(150));
    assert_eq!(marks.last_active("cli", at(300)), Some(at(150)));
}

#[test]
fn marks_keep_at_most_the_newest_clients() {
    let dir = tempfile::tempdir().unwrap();
    let mut marks = Marks::load(dir.path());
    for i in 0..(marks::MAX_CLIENTS as i64 + 10) {
        marks.touch(&format!("c{i}"), at(i));
    }
    assert_eq!(marks.last_active("c0", at(999)), None);
    assert_eq!(marks.last_active("c300", at(999)), None);
    let newest = format!("c{}", marks::MAX_CLIENTS + 9);
    assert!(marks.last_active(&newest, at(999)).is_some());
}

#[test]
fn rows_written_while_frozen_for_a_handoff_are_held_and_written_if_it_is_called_off() {
    let dir = tempfile::tempdir().unwrap();
    let activity = Activity::open(dir.path());
    let first = activity.record(Draft::new("usage", "before")).unwrap();
    activity.pause();
    // Frozen: nothing reaches the file the next hub reads.
    assert!(
        activity
            .record(Draft::new("session.exited", "during"))
            .is_none()
    );
    let next = Journal::open(dir.path(), Limits::default(), Utc::now());
    assert_eq!(
        next.next_id(),
        first.id + 1,
        "the next hub numbers on from disk"
    );
    assert_eq!(next.len(), 1);
    // Called off: the held row is written, in order, and writing carries on.
    let replayed = activity.resume();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].text, "during");
    assert_eq!(replayed[0].id, first.id + 1);
    assert!(activity.record(Draft::new("usage", "after")).is_some());
    assert!(activity.resume().is_empty());
    assert_eq!(activity.len(), 3);
}

#[test]
fn a_stopping_hub_writes_one_row_and_an_exit_says_how() {
    let dir = tempfile::tempdir().unwrap();
    let activity = Activity::open(dir.path());
    assert!(!activity.is_stopping());
    activity.shutdown(3);
    assert!(activity.is_stopping());
    let (rows, _) = activity.page(&Filter::default(), None, 10);
    assert_eq!(rows[0].kind, "hub.stopped");
    assert_eq!(rows[0].text, "hub stopped, hanging up 3 sessions");
    assert!(known_kind("hub.stopped"));
    let session = json!({"id": "s", "name": "n"});
    let killed = draft_for(
        "session.exited",
        &json!({"session": session, "exit": {"code": null, "signal": "Hangup"}}),
    )
    .unwrap();
    assert_eq!(killed.text, "was killed (Hangup)");
    assert_eq!(killed.data["signal"], "Hangup");
    let exited = draft_for(
        "session.exited",
        &json!({"session": session, "exit": {"code": 2, "signal": null}}),
    )
    .unwrap();
    assert_eq!(exited.text, "exited with code 2");
    assert_eq!(super::feed::short_count(950), "950");
    assert_eq!(super::feed::short_count(110_000), "110.0k");
    assert_eq!(super::feed::short_count(2_340_000), "2.3M");
}
