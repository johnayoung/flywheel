use super::store::{Marker, Store};
use super::*;
use crate::activity::{Draft, Row};
use chrono::{NaiveDate, TimeZone};
use serde_json::json;

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

fn config(times: &[&str], tz: &str) -> DigestConfig {
    DigestConfig::resolve(&DigestSection {
        times: Some(times.iter().map(|t| (*t).to_owned()).collect()),
        timezone: Some(tz.to_owned()),
        ..DigestSection::default()
    })
    .unwrap()
}

// --- schedule math ------------------------------------------------------------

#[test]
fn local_times_hold_across_new_york_dst_changes() {
    let tz: Tz = "America/New_York".parse().unwrap();
    let eight = [hm(8, 0)];
    // EST (UTC-5) the day before spring forward, EDT (UTC-4) on it.
    let day = |d: u32| NaiveDate::from_ymd_opt(2026, 3, d).unwrap();
    assert_eq!(
        schedule::slots_on(&tz, &eight, day(7)),
        [utc(2026, 3, 7, 13, 0)]
    );
    assert_eq!(
        schedule::slots_on(&tz, &eight, day(8)),
        [utc(2026, 3, 8, 12, 0)]
    );
    // 02:30 does not exist on 2026-03-08: due at 03:00 EDT; 02:15 and
    // 02:45 land on that one instant: one slot.
    assert_eq!(
        schedule::slots_on(&tz, &[hm(2, 15), hm(2, 30), hm(2, 45)], day(8)),
        [utc(2026, 3, 8, 7, 0)]
    );
    // 01:30 happens twice on 2026-11-01: its first pass (EDT) only.
    let nov1 = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
    assert_eq!(
        schedule::slots_on(&tz, &[hm(1, 30)], nov1),
        [utc(2026, 11, 1, 5, 30)]
    );
    // During the repeated hour (01:45 EST = 06:45Z) the latest slot is
    // still that first 01:30, not a second one.
    assert_eq!(
        schedule::latest_at_or_before(&tz, &[hm(1, 30)], utc(2026, 11, 1, 6, 45)),
        Some(utc(2026, 11, 1, 5, 30))
    );
    assert_eq!(
        schedule::next_after(&tz, &[hm(1, 30)], utc(2026, 11, 1, 5, 30)),
        Some(utc(2026, 11, 2, 6, 30))
    );
}

#[test]
fn local_times_hold_across_berlin_and_sydney_dst_changes() {
    let berlin: Tz = "Europe/Berlin".parse().unwrap();
    let times = [hm(8, 0), hm(18, 0)];
    // CET (UTC+1) -> CEST (UTC+2) on 2026-03-29.
    assert_eq!(
        schedule::upcoming(&berlin, &times, utc(2026, 3, 28, 12, 0), 4),
        [
            utc(2026, 3, 28, 17, 0),
            utc(2026, 3, 29, 6, 0),
            utc(2026, 3, 29, 16, 0),
            utc(2026, 3, 30, 6, 0),
        ]
    );
    // Back to CET on 2026-10-25.
    assert_eq!(
        schedule::upcoming(&berlin, &times, utc(2026, 10, 24, 12, 0), 3),
        [
            utc(2026, 10, 24, 16, 0),
            utc(2026, 10, 25, 7, 0),
            utc(2026, 10, 25, 17, 0),
        ]
    );
    // Southern hemisphere: Sydney leaves AEST (UTC+10) for AEDT (UTC+11)
    // on 2026-10-04 at 02:00.
    let sydney: Tz = "Australia/Sydney".parse().unwrap();
    assert_eq!(
        schedule::upcoming(&sydney, &[hm(7, 0)], utc(2026, 10, 2, 0, 0), 3),
        [
            utc(2026, 10, 2, 21, 0),
            utc(2026, 10, 3, 20, 0),
            utc(2026, 10, 4, 20, 0),
        ]
    );
    // Lord Howe moves by half an hour.
    let howe: Tz = "Australia/Lord_Howe".parse().unwrap();
    let gap = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
    assert_eq!(
        schedule::slots_on(&howe, &[hm(2, 10)], gap),
        [howe
            .with_ymd_and_hms(2026, 10, 4, 2, 30, 0)
            .unwrap()
            .with_timezone(&Utc)]
    );
}

#[test]
fn days_and_weeks_start_at_local_midnight() {
    let tz: Tz = "America/New_York".parse().unwrap();
    // Wed 2026-03-11 15:00 EDT.
    let now = utc(2026, 3, 11, 19, 0);
    assert_eq!(schedule::day_start(&tz, now), utc(2026, 3, 11, 4, 0));
    // Monday 2026-03-09 00:00 EDT (after the change).
    assert_eq!(schedule::week_start(&tz, now), utc(2026, 3, 9, 4, 0));
    assert_eq!(schedule::label_of(&tz, utc(2026, 3, 11, 12, 0)), "morning");
    assert_eq!(
        schedule::label_of(&tz, utc(2026, 3, 11, 17, 0)),
        "afternoon"
    );
    assert_eq!(schedule::label_of(&tz, utc(2026, 3, 11, 23, 0)), "evening");
}

/// A hub ticking every minute across both 2026 DST changes in New York
/// sends exactly once per configured local time per day, at that time.
#[test]
fn a_month_of_minute_ticks_sends_each_slot_exactly_once() {
    for (tz, start) in [
        ("America/New_York", utc(2026, 3, 1, 0, 0)),
        ("America/New_York", utc(2026, 10, 20, 0, 0)),
        ("Europe/Berlin", utc(2026, 3, 20, 0, 0)),
        ("Australia/Sydney", utc(2026, 9, 25, 0, 0)),
    ] {
        let config = config(&["01:30", "02:30", "08:00", "18:00"], tz);
        let zone = config.timezone;
        let me = Claimant {
            pid: 1,
            owner: "a".into(),
        };
        let mut marker = Marker::default();
        let mut sent: Vec<DateTime<Utc>> = Vec::new();
        let end = start + Duration::days(20);
        let mut now = start;
        while now < end {
            let (due, _) = decide(&mut marker, &config, now, &me, &|_| true);
            match due {
                Due::Send(pending) => {
                    let slot = pending.slot.unwrap();
                    assert!(
                        now - slot < Duration::minutes(1),
                        "{tz}: late {slot} at {now}"
                    );
                    sent.push(slot);
                    marker.pending = None;
                }
                Due::Skipped(slot) => panic!("{tz}: skipped {slot}"),
                Due::Off | Due::Idle => {}
            }
            now += Duration::minutes(1);
        }
        let mut per_day = std::collections::BTreeMap::<NaiveDate, Vec<String>>::new();
        for slot in &sent {
            let local = slot.with_timezone(&zone);
            per_day
                .entry(local.date_naive())
                .or_default()
                .push(local.format("%H:%M").to_string());
        }
        let days: Vec<_> = per_day.iter().collect();
        // The first day is armed mid-way (at its first minute): every day
        // after holds every time once (a skipped 02:30 sends at 03:00).
        for (date, times) in &days[1..days.len() - 1] {
            assert_eq!(times.len(), 4, "{tz} {date}: {times:?}");
            let mut unique = (*times).clone();
            unique.dedup();
            assert_eq!(unique.len(), 4, "{tz} {date}: twice: {times:?}");
            assert!(
                times.contains(&"08:00".to_owned()),
                "{tz} {date}: {times:?}"
            );
            assert!(
                times.contains(&"18:00".to_owned()),
                "{tz} {date}: {times:?}"
            );
            assert!(
                times.contains(&"02:30".to_owned()) || times.contains(&"03:00".to_owned()),
                "{tz} {date}: {times:?}"
            );
        }
        assert!(sent.windows(2).all(|w| w[0] < w[1]), "{tz}: out of order");
    }
}

// --- restarts and handoffs -----------------------------------------------------

fn claimant(pid: u32, owner: &str) -> Claimant {
    Claimant {
        pid,
        owner: owner.into(),
    }
}

#[test]
fn a_new_schedule_owes_nothing_before_it() {
    let config = config(&["08:00"], "UTC");
    let me = claimant(1, "a");
    let mut marker = Marker::default();
    // Configured at 08:30: today's 08:00 is not owed.
    let (due, changed) = decide(&mut marker, &config, utc(2026, 5, 1, 8, 30), &me, &|_| true);
    assert_eq!((due, changed), (Due::Idle, true));
    assert_eq!(
        decide(&mut marker, &config, utc(2026, 5, 1, 8, 31), &me, &|_| true).0,
        Due::Idle
    );
    // Tomorrow's is.
    let (due, _) = decide(&mut marker, &config, utc(2026, 5, 2, 8, 0), &me, &|_| true);
    let Due::Send(pending) = due else {
        panic!("{due:?}")
    };
    assert_eq!(pending.slot, Some(utc(2026, 5, 2, 8, 0)));
    // The first digest covers the day before its slot.
    assert_eq!(pending.since, utc(2026, 5, 1, 8, 0));
    assert_eq!(
        pending.id,
        format!("dg-{}", utc(2026, 5, 2, 8, 0).timestamp())
    );
    // A changed schedule re-arms: 07:00 today (passed) is not owed.
    let changed = config_with(&["07:00", "08:00"]);
    marker.pending = None;
    assert_eq!(
        decide(&mut marker, &changed, utc(2026, 5, 2, 8, 5), &me, &|_| true).0,
        Due::Idle
    );
    assert_eq!(
        decide(&mut marker, &changed, utc(2026, 5, 2, 8, 6), &me, &|_| true).0,
        Due::Idle
    );
}

fn config_with(times: &[&str]) -> DigestConfig {
    config(times, "UTC")
}

#[test]
fn a_hub_down_at_the_slot_sends_once_within_grace_else_skips() {
    let config = config(&["08:00", "18:00"], "UTC");
    let store_dir = tempfile::tempdir().unwrap();
    let store = Store::new(store_dir.path());
    let alive = |_: u32| true;
    let first = claimant(10, "first");
    // Armed the day before; the hub then stops before 08:00.
    store
        .update(|m| decide(m, &config, utc(2026, 5, 1, 6, 0), &first, &alive))
        .unwrap();
    // Restarted at 09:10 (within 2h of 08:00): sent once, on start.
    let second = claimant(11, "second");
    let due = store
        .update(|m| decide(m, &config, utc(2026, 5, 1, 9, 10), &second, &alive))
        .unwrap();
    let Due::Send(pending) = due else {
        panic!("{due:?}")
    };
    assert_eq!(pending.slot, Some(utc(2026, 5, 1, 8, 0)));
    store
        .update(|m| {
            m.pending = None;
            ((), true)
        })
        .unwrap();
    // Restarted again at once: nothing more.
    let third = claimant(12, "third");
    assert_eq!(
        store
            .update(|m| decide(m, &config, utc(2026, 5, 1, 9, 11), &third, &alive))
            .unwrap(),
        Due::Idle
    );
    // Down from 17:00 to 21:00: 18:00 is past grace -- skipped, not sent,
    // and not considered again.
    assert_eq!(
        store
            .update(|m| decide(m, &config, utc(2026, 5, 1, 21, 0), &third, &alive))
            .unwrap(),
        Due::Skipped(utc(2026, 5, 1, 18, 0))
    );
    assert_eq!(
        store
            .update(|m| decide(m, &config, utc(2026, 5, 1, 21, 1), &third, &alive))
            .unwrap(),
        Due::Idle
    );
    // The next morning's digest covers since the last one ended (09:10).
    let Due::Send(next) = store
        .update(|m| decide(m, &config, utc(2026, 5, 2, 8, 0), &third, &alive))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(next.since, utc(2026, 5, 1, 9, 10));
}

#[test]
fn two_hub_processes_at_one_slot_send_it_once() {
    // A live handoff: the old and the new hub both look at 08:00:00.
    let config = config(&["08:00"], "UTC");
    let dir = tempfile::tempdir().unwrap();
    let alive = |_: u32| true;
    Store::new(dir.path())
        .update(|m| {
            decide(
                m,
                &config,
                utc(2026, 5, 1, 7, 0),
                &claimant(1, "old"),
                &alive,
            )
        })
        .unwrap();
    let slot = utc(2026, 5, 1, 8, 0);
    let sends = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for (pid, owner) in [(1, "old"), (2, "new"), (1, "old"), (2, "new")] {
            let (config, sends, dir) = (&config, &sends, dir.path());
            scope.spawn(move || {
                let store = Store::new(dir);
                let due = store
                    .update(|m| decide(m, config, slot, &claimant(pid, owner), &alive))
                    .unwrap();
                if matches!(due, Due::Send(_)) {
                    sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(sends.into_inner(), 1);
}

#[test]
fn a_send_whose_hub_died_is_resent_once_by_the_next() {
    let config = config(&["08:00"], "UTC");
    let mut marker = Marker::default();
    let old = claimant(100, "old");
    let new = claimant(200, "new");
    decide(&mut marker, &config, utc(2026, 5, 1, 7, 0), &old, &|_| true);
    let Due::Send(claimed) = decide(&mut marker, &config, utc(2026, 5, 1, 8, 0), &old, &|_| true).0
    else {
        panic!()
    };
    // The old hub still runs: its send is not repeated.
    assert_eq!(
        decide(&mut marker, &config, utc(2026, 5, 1, 8, 1), &new, &|_| true).0,
        Due::Idle
    );
    // It died mid-send: the new hub delivers that same digest, once.
    let dead = |pid: u32| pid != 100;
    let Due::Send(again) = decide(&mut marker, &config, utc(2026, 5, 1, 8, 2), &new, &dead).0
    else {
        panic!()
    };
    assert_eq!(again.id, claimed.id);
    assert_eq!((again.since, again.until), (claimed.since, claimed.until));
    assert_eq!(
        decide(&mut marker, &config, utc(2026, 5, 1, 8, 3), &new, &dead).0,
        Due::Idle,
        "in flight with the new hub"
    );
    // A claim older than grace whose claimer is gone is dropped.
    let mut stale = Marker::default();
    decide(&mut stale, &config, utc(2026, 5, 1, 7, 0), &old, &|_| true);
    decide(&mut stale, &config, utc(2026, 5, 1, 8, 0), &old, &|_| true);
    assert_eq!(
        decide(&mut stale, &config, utc(2026, 5, 1, 11, 0), &new, &dead).0,
        Due::Idle
    );
    assert!(stale.pending.is_none());
}

#[test]
fn finished_digests_release_the_claim_and_keep_the_last_fourteen() {
    let mut marker = Marker {
        pending: Some(store::Pending {
            id: "dg-5".into(),
            slot: None,
            since: utc(2026, 1, 1, 0, 0),
            until: utc(2026, 1, 1, 0, 0),
            pid: 1,
            owner: "a".into(),
            claimed_at: utc(2026, 1, 1, 0, 0),
        }),
        ..Marker::default()
    };
    for n in 0..20 {
        finish(&mut marker, &report_with_id(&format!("dg-{n}")));
    }
    assert!(marker.pending.is_none());
    assert_eq!(marker.recent.len(), KEEP_RECENT);
    assert_eq!(marker.recent.last().unwrap().id, "dg-19");
    assert!(valid_id("dg-19") && valid_id("dg-m1700000000000"));
    assert!(!valid_id("dg-") && !valid_id("dg-x1") && !valid_id("../x"));
}

fn report_with_id(id: &str) -> Report {
    let live = Live::default();
    build(&Inputs {
        id: id.to_owned(),
        trigger: "scheduled",
        label: "morning".into(),
        slot: None,
        since: utc(2026, 1, 1, 0, 0),
        until: utc(2026, 1, 1, 1, 0),
        tz: Tz::UTC,
        rows: &[],
        live: &live,
        hub_url: None,
    })
}

// --- config ----------------------------------------------------------------------

#[test]
fn bad_digest_settings_are_each_named() {
    let errors = DigestConfig::resolve(&DigestSection {
        times: Some(vec!["8am".into(), "25:00".into(), "08:00".into()]),
        timezone: Some("Mars/Olympus".into()),
        channels: Some(vec!["email".into()]),
        quiet_if_empty: None,
    })
    .unwrap_err();
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors[0].starts_with("digest.times: \"8am\""), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("Mars/Olympus")));
    assert!(errors.iter().any(|e| e.contains("\"email\"")));
    let empty = DigestConfig::resolve(&DigestSection {
        channels: Some(Vec::new()),
        ..DigestSection::default()
    })
    .unwrap_err();
    assert!(empty[0].contains("empty"), "{empty:?}");
    let ok = config(&["18:00", "8:05", "18:00"], "Europe/Berlin");
    assert_eq!(ok.times_text(), ["08:05", "18:00"]);
    assert_eq!(ok.fingerprint(), "08:05,18:00@Europe/Berlin");
    assert!(!DigestConfig::default().enabled(), "off by default");
}

#[test]
fn the_digest_table_is_rewritten_in_place() {
    let section = DigestSection {
        times: Some(vec!["07:30".into()]),
        timezone: Some("UTC".into()),
        channels: Some(vec!["push".into()]),
        quiet_if_empty: Some(false),
    };
    let text = "# my hub\n[server]\nport = 7421 # mine\n\n[digest]\ntimes = [\"09:00\"]\n# old note\n\n[queue]\ntyping_quiet = \"8s\"\n";
    let out = splice_section(text, &section).unwrap();
    assert!(
        out.starts_with("# my hub\n[server]\nport = 7421 # mine\n\n[digest]\n"),
        "{out}"
    );
    assert!(
        out.ends_with("\n\n[queue]\ntyping_quiet = \"8s\"\n"),
        "{out}"
    );
    assert!(!out.contains("09:00") && !out.contains("old note"), "{out}");
    let parsed = crate::config::parse(&out).unwrap();
    assert_eq!(parsed.digest, section);
    assert_eq!(parsed.server.port, Some(7421));
    // No table yet: appended; an empty file: just the table.
    let appended = splice_section("[server]\nport = 1", &section).unwrap();
    assert!(
        appended.starts_with("[server]\nport = 1\n\n[digest]\n"),
        "{appended}"
    );
    assert_eq!(crate::config::parse(&appended).unwrap().digest, section);
    let fresh = splice_section("", &section).unwrap();
    assert!(fresh.starts_with("[digest]\n"), "{fresh}");
}

#[test]
fn settings_write_the_digest_table_and_refuse_what_they_cannot_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::config::CONFIG_FILE);
    std::fs::write(&path, "[server]\nport = 7499\n").unwrap();
    let watch = crate::config::ConfigWatch::open(dir.path(), crate::config::Flags::default(), None)
        .unwrap();
    let section = DigestSection {
        times: Some(vec!["08:00".into()]),
        timezone: Some("Asia/Tokyo".into()),
        ..DigestSection::default()
    };
    watch.write_digest(&section).unwrap();
    let current = watch.current();
    assert_eq!(current.digest.times_text(), ["08:00"]);
    assert_eq!(current.digest.timezone.name(), "Asia/Tokyo");
    assert_eq!(current.port, 7499);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("[server]\nport = 7499\n")
    );
    // Invalid values: refused, the file untouched.
    let before = std::fs::read_to_string(&path).unwrap();
    let bad = DigestSection {
        times: Some(vec!["noon".into()]),
        ..DigestSection::default()
    };
    assert!(matches!(
        watch.write_digest(&bad),
        Err(crate::config::WriteError::Invalid(_))
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    // Digest keys set as dotted keys elsewhere: not rewritten.
    std::fs::write(&path, "digest.timezone = \"UTC\"\n[server]\nport = 7499\n").unwrap();
    assert!(watch.reload() != crate::config::Reload::Unchanged);
    assert!(matches!(
        watch.write_digest(&section),
        Err(crate::config::WriteError::Conflict(_))
    ));
}

// --- what a digest says ----------------------------------------------------------

const FAKE_TOKEN: &str = "ghp_Zq8rT1x9Kc2mB7nW4yH6vL3pJ0sD5fG8aQ";
const PROMPT: &str = "PLEASE-REFACTOR-THE-BILLING-MODULE-NOW";

fn row(id: u64, at: DateTime<Utc>, draft: Draft) -> Row {
    let mut row: Row = serde_json::from_value(json!({
        "id": id, "at": at, "kind": draft.kind, "text": draft.text,
    }))
    .unwrap();
    row.session_id = draft.session_id;
    row.session = draft.session;
    row.workspace_id = draft.workspace_id;
    row.workspace = draft.workspace;
    row.data = draft.data;
    row
}

fn about(session: &str, name: &str, ws: &str, ws_name: &str) -> serde_json::Value {
    json!({"id": session, "name": name, "kind": "claude", "workspace_id": ws, "workspace": ws_name})
}

#[test]
fn a_digest_counts_per_workspace_and_ranks_what_needs_you() {
    let tz: Tz = "Europe/Berlin".parse().unwrap();
    // Thu 2026-06-04 18:00 CEST.
    let until = utc(2026, 6, 4, 16, 0);
    let since = utc(2026, 6, 4, 6, 0);
    let api = about("s1", &format!("api {FAKE_TOKEN}"), "w1", "personal");
    let web = about("s2", "web", "w2", "infrared");
    let rows = vec![
        // Monday: this week's spend, not today's.
        row(
            1,
            utc(2026, 6, 1, 9, 0),
            Draft::new("usage", "x").about(&api).with("cost_usd", 2.0),
        ),
        // Before the span, today: today's spend.
        row(
            2,
            utc(2026, 6, 4, 5, 0),
            Draft::new("usage", "x").about(&api).with("cost_usd", 0.5),
        ),
        row(
            3,
            utc(2026, 6, 4, 7, 0),
            Draft::new("turn.finished", PROMPT).about(&api),
        ),
        row(
            4,
            utc(2026, 6, 4, 8, 0),
            Draft::new("turn.finished", "x").about(&api),
        ),
        row(
            5,
            utc(2026, 6, 4, 8, 30),
            Draft::new("turn.finished", "x").about(&web),
        ),
        row(
            6,
            utc(2026, 6, 4, 9, 0),
            Draft::new("usage", "x").about(&web).with("cost_usd", 1.25),
        ),
        row(
            7,
            utc(2026, 6, 4, 9, 5),
            Draft::new("pr.checks_failed", "x")
                .about(&api)
                .with("number", 12),
        ),
        row(
            8,
            utc(2026, 6, 4, 9, 6),
            Draft::new("pr.checks_passed", "x")
                .about(&api)
                .with("number", 12),
        ),
        row(
            9,
            utc(2026, 6, 4, 9, 7),
            Draft::new("pr.checks_failed", "x")
                .about(&web)
                .with("number", 3),
        ),
        row(
            10,
            utc(2026, 6, 4, 9, 8),
            Draft::new("pr.merged", "x").about(&web).with("number", 4),
        ),
        row(
            11,
            utc(2026, 6, 4, 9, 9),
            Draft::new("pr.checks_passed", "x")
                .about(&web)
                .with("number", 4),
        ),
        row(
            12,
            utc(2026, 6, 4, 10, 0),
            Draft::new("fanout.finished", "x")
                .in_workspace(Some("w1"), Some("personal"))
                .with_id("fanout_id", Some("f1"))
                .with("name", "fix-login")
                .with("leader", "b")
                .with("passing", 2)
                .with("candidates", 3),
        ),
        row(
            13,
            utc(2026, 6, 4, 10, 30),
            Draft::new("autopilot.decision", PROMPT).with("folder", "code/infrared"),
        ),
        row(
            14,
            utc(2026, 6, 4, 11, 0),
            Draft::new("queue.stalled", PROMPT).about(&web),
        ),
        // After `until`: not counted.
        row(
            15,
            utc(2026, 6, 4, 16, 1),
            Draft::new("turn.finished", "x").about(&web),
        ),
    ];
    let live = Live {
        workspaces: vec![
            ("w1".into(), "personal".into()),
            ("w2".into(), "infrared".into()),
            ("w3".into(), "quiet".into()),
        ],
        folders: vec![("code/infrared".into(), "w2".into())],
        blocked: vec![
            LiveBlocked {
                session_id: "s2".into(),
                session: "web".into(),
                workspace_id: "w2".into(),
                since: utc(2026, 6, 4, 15, 0),
            },
            LiveBlocked {
                session_id: "s1".into(),
                session: format!("api {FAKE_TOKEN}"),
                workspace_id: "w1".into(),
                since: utc(2026, 6, 4, 12, 0),
            },
        ],
        gates: vec![LiveGate {
            workspace_id: Some("w2".into()),
            folder: "code/infrared".into(),
            task_id: "T-7".into(),
            run_id: "run-0123456789abcdef0123456789abcdef".into(),
            since: Some(utc(2026, 6, 4, 14, 0)),
        }],
        red_prs: vec![LivePr {
            session_id: "s2".into(),
            session: "web".into(),
            workspace_id: "w2".into(),
            number: 3,
        }],
        stalled: vec![LiveStall {
            session_id: "s2".into(),
            session: "web".into(),
            workspace_id: "w2".into(),
            at: utc(2026, 6, 4, 11, 0),
        }],
    };
    let report = build(&Inputs {
        id: "dg-1".into(),
        trigger: "scheduled",
        label: "evening".into(),
        slot: Some(until),
        since,
        until,
        tz,
        rows: &rows,
        live: &live,
        hub_url: Some("https://box.tail.ts.net/"),
    });
    let t = &report.totals;
    assert_eq!(
        (t.turns_finished, t.blocked_now, t.fanouts_graded),
        (3, 2, 1)
    );
    // PR 12 red then green; 3 red; 4 merged (a later green keeps it merged).
    assert_eq!((t.prs_red, t.prs_green, t.prs_merged), (1, 1, 1));
    assert_eq!(
        (t.autopilot_decisions, t.gates_waiting, t.queue_stalls),
        (1, 1, 1)
    );
    assert_eq!((t.spend_today_usd, t.spend_week_usd), (1.75, 3.75));
    assert!(!report.empty);
    assert_eq!(report.timezone, "Europe/Berlin");
    // Waiting-on-you first; a quiet workspace is left out.
    let names: Vec<&str> = report
        .workspaces
        .iter()
        .map(|w| w.workspace.as_str())
        .collect();
    assert_eq!(names, ["infrared", "personal"]);
    let infrared = &report.workspaces[0];
    assert_eq!(infrared.counts.gates_waiting, 1);
    assert_eq!(infrared.counts.autopilot_decisions, 1);
    assert_eq!(infrared.blocked[0].session, "web");
    let personal = &report.workspaces[1];
    assert_eq!(personal.counts.turns_finished, 2);
    assert_eq!(personal.counts.spend_week_usd, 2.5);
    assert_eq!(personal.fanouts[0].leader.as_deref(), Some("b"));
    assert_eq!(personal.blocked[0].session, "api [redacted]");
    // The gate, then the longest-blocked agent, then the next one.
    let kinds: Vec<&str> = report.needs_you.iter().map(|n| n.kind.as_str()).collect();
    assert_eq!(kinds, ["gate", "blocked", "blocked"]);
    assert_eq!(
        report.needs_you[0].link.hash,
        "#gate=run-0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        report.needs_you[1].link.url.as_deref(),
        Some("https://box.tail.ts.net/#session=s1")
    );
    assert_eq!(
        report.url.as_deref(),
        Some("https://box.tail.ts.net/#digest=dg-1")
    );
    // Push: names and counts; the top items as buttons.
    let push = report.push();
    assert_eq!(push.title, "flywheel evening digest");
    assert_eq!(push.priority, "high");
    assert!(
        push.message
            .starts_with("3 turns finished, 2 blocked, 1 fan-out graded, 1 PR red"),
        "{}",
        push.message
    );
    assert!(
        push.message
            .contains("\n1. approve autopilot task T-7 in code/infrared (infrared)"),
        "{}",
        push.message
    );
    assert!(push.message.contains("infrared: "), "{}", push.message);
    assert_eq!(push.actions.len(), 3);
    assert_eq!(
        push.click.as_deref(),
        Some("https://box.tail.ts.net/#digest=dg-1")
    );
    // Never the token, the prompt, or anything rows hold as text.
    let all = format!(
        "{}{}{}{}",
        serde_json::to_string(&report).unwrap(),
        push.title,
        push.message,
        report.text(&tz)
    );
    for forbidden in [FAKE_TOKEN, PROMPT, "x\"", "cost_usd\":2"] {
        assert!(!all.contains(forbidden), "{forbidden} in {all}");
    }
}

#[test]
fn nothing_happened_is_empty_and_says_so() {
    let report = report_with_id("dg-2");
    assert!(report.empty);
    assert_eq!(
        report.headline(),
        "Nothing happened, and nothing waits on you."
    );
    assert!(report.workspaces.is_empty() && report.needs_you.is_empty());
    let push = report.push();
    assert_eq!(push.priority, "default");
    assert!(push.click.is_none() && push.actions.is_empty());
}
