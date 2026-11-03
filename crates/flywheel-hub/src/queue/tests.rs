use super::*;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
}

fn item(id: &str, when: When) -> QueueItem {
    QueueItem {
        id: id.to_owned(),
        text: format!("prompt {id}"),
        when,
        added_at: at(0),
        blocked_at: None,
        state: ItemState::Pending,
    }
}

fn queue(items: Vec<QueueItem>) -> SessionQueue {
    SessionQueue {
        items,
        ..SessionQueue::default()
    }
}

fn status(status: AgentStatus, since: i64) -> Option<StatusView> {
    Some(StatusView {
        status,
        detail: None,
        since: at(since),
    })
}

/// An agent at rest since t=10, looked at t=20: nobody typing, nothing
/// holding it.
fn facts(status_now: Option<StatusView>) -> Facts {
    Facts {
        now: at(20),
        presence: Presence::Ready,
        status: status_now,
        typed_ago: None,
        draft: 0,
        editing: false,
        quiet: Duration::from_secs(8),
        owner: None,
    }
}

fn waits(decision: &Decision, needle: &str) {
    match decision {
        Decision::Wait(why) => assert!(why.contains(needle), "{why:?} lacks {needle:?}"),
        other => panic!("expected a wait ({needle}), got {other:?}"),
    }
}

#[test]
fn a_done_item_goes_once_the_agent_rests_and_waits_while_it_works() {
    let q = queue(vec![item("a1", When::Done), item("b2", When::Done)]);
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Done, 10))),
        Decision::Deliver("a1".into())
    );
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Idle, 10))),
        Decision::Deliver("a1".into()),
        "a seen turn (or a fresh agent) is at rest too"
    );
    waits(
        &decide(&q, &facts(status(AgentStatus::Working, 10))),
        "working",
    );
    waits(
        &decide(&q, &facts(status(AgentStatus::Unknown, 10))),
        "report",
    );
    waits(&decide(&q, &facts(None)), "report");
}

#[test]
fn never_while_blocked_whatever_the_trigger() {
    for when in [When::Done, When::Idle] {
        let q = queue(vec![item("a1", when)]);
        waits(
            &decide(&q, &facts(status(AgentStatus::Blocked, 10))),
            "blocked",
        );
    }
    let q = queue(vec![item("a1", When::BlockedCleared)]);
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Blocked, 12))),
        Decision::NoteBlocked {
            item: "a1".into(),
            at: at(12)
        }
    );
    let mut noted = q.clone();
    noted.items[0].blocked_at = Some(at(12));
    waits(
        &decide(&noted, &facts(status(AgentStatus::Blocked, 12))),
        "blocked",
    );
}

#[test]
fn the_next_item_waits_for_a_rest_after_the_last_delivery() {
    let mut q = queue(vec![item("b2", When::Done)]);
    q.last_delivered_at = Some(at(15));
    // Still the rest from before the delivery: the prompt was not taken up.
    waits(
        &decide(&q, &facts(status(AgentStatus::Done, 10))),
        "take up",
    );
    // It worked and finished again.
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Done, 17))),
        Decision::Deliver("b2".into())
    );
    // A prompt that never started a turn stalls the queue.
    let mut late = facts(status(AgentStatus::Done, 10));
    late.now = at(15) + chrono::Duration::from_std(ACK_TIMEOUT).unwrap();
    assert!(matches!(decide(&q, &late), Decision::Stall(why) if why.contains("did not start")));
}

#[test]
fn idle_waits_until_someone_has_seen_the_answer() {
    let q = queue(vec![item("a1", When::Idle)]);
    waits(&decide(&q, &facts(status(AgentStatus::Done, 10))), "looked");
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Idle, 10))),
        Decision::Deliver("a1".into())
    );
}

#[test]
fn blocked_cleared_waits_for_a_block_then_the_rest_after_it() {
    let mut q = queue(vec![item("a1", When::BlockedCleared)]);
    waits(
        &decide(&q, &facts(status(AgentStatus::Done, 10))),
        "block on",
    );
    q.items[0].blocked_at = Some(at(12));
    waits(
        &decide(&q, &facts(status(AgentStatus::Done, 10))),
        "after the block",
    );
    assert_eq!(
        decide(&q, &facts(status(AgentStatus::Done, 14))),
        Decision::Deliver("a1".into())
    );
}

#[test]
fn typing_owners_and_a_fresh_rest_hold_it_back() {
    let q = queue(vec![item("a1", When::Done)]);
    let mut typing = facts(status(AgentStatus::Done, 10));
    typing.typed_ago = Some(Duration::from_secs(3));
    waits(&decide(&q, &typing), "typed 3s ago");
    typing.typed_ago = Some(Duration::from_secs(9));
    assert_eq!(decide(&q, &typing), Decision::Deliver("a1".into()));

    let mut owned = facts(status(AgentStatus::Done, 10));
    owned.owner = Some("a fan-out grading".into());
    waits(&decide(&q, &owned), "a fan-out grading is using");

    let mut fresh = facts(status(AgentStatus::Done, 10));
    fresh.now = at(10) + chrono::Duration::milliseconds(500);
    waits(&decide(&q, &fresh), "settling");
}

#[test]
fn paused_stalled_gone_and_starting_sessions() {
    let mut q = queue(vec![item("a1", When::Done)]);
    q.paused = true;
    waits(&decide(&q, &facts(status(AgentStatus::Done, 10))), "paused");
    q.paused = false;
    q.stalled = Some(Stall {
        at: at(1),
        reason: "boom".into(),
        item: None,
    });
    waits(
        &decide(&q, &facts(status(AgentStatus::Done, 10))),
        "stalled: boom",
    );
    q.stalled = None;
    let mut gone = facts(status(AgentStatus::Done, 10));
    gone.presence = Presence::Gone("its agent exited (exit code 1)".into());
    assert_eq!(
        decide(&q, &gone),
        Decision::Stall("its agent exited (exit code 1)".into())
    );
    let mut starting = facts(None);
    starting.presence = Presence::Starting("waiting for the hub to restore it");
    waits(&decide(&q, &starting), "restore");
    q.items[0].state = ItemState::Sending { sending_at: at(5) };
    waits(
        &decide(&q, &facts(status(AgentStatus::Done, 10))),
        "being delivered",
    );
}

#[test]
fn stored_queues_round_trip_and_keep_their_order() {
    let mut q = queue(vec![
        item("p1", When::Idle),
        item("p2", When::BlockedCleared),
    ]);
    q.items.insert(
        0,
        QueueItem {
            state: ItemState::Delivered {
                delivered_at: at(3),
                reconciled: true,
            },
            ..item("d1", When::Done)
        },
    );
    q.last_delivered_at = Some(at(3));
    let text = serde_json::to_string(&q).unwrap();
    assert!(text.contains(r#""state":"delivered""#), "{text}");
    assert!(text.contains(r#""when":"blocked-cleared""#), "{text}");
    let back: SessionQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(back, q);
    assert_eq!(back.pending_count(), 2);
    assert_eq!(back.head().unwrap().id, "p1");
    // A queue written without the optional fields loads.
    let minimal: SessionQueue = serde_json::from_str(
        r#"{"items":[{"id":"x1","text":"t","added_at":"2027-01-15T08:00:00Z","state":"pending"}]}"#,
    )
    .unwrap();
    assert_eq!(minimal.items[0].when, When::Done);
    assert!(!minimal.paused);
}

#[test]
fn history_is_bounded_and_delivered_items_stay_first() {
    let mut q = SessionQueue::default();
    for n in 0..(MAX_DELIVERED_KEPT + 5) {
        q.items.push(QueueItem {
            state: ItemState::Delivered {
                delivered_at: at(i64::try_from(n).unwrap()),
                reconciled: false,
            },
            ..item(&format!("d{n}"), When::Done)
        });
    }
    q.items.insert(3, item("p1", When::Done));
    q.settle_order();
    q.trim_history();
    assert_eq!(q.items.len(), MAX_DELIVERED_KEPT + 1);
    assert_eq!(q.items.last().unwrap().id, "p1");
    assert_eq!(q.items[0].id, "d5", "the oldest went first");
}

#[test]
fn triggers_and_quiet_windows_parse() {
    assert_eq!(When::parse("done").unwrap(), When::Done);
    assert_eq!(
        When::parse("blocked_cleared").unwrap(),
        When::BlockedCleared
    );
    assert_eq!(When::parse(" IDLE ").unwrap(), When::Idle);
    assert!(When::parse("soon").is_err());
    assert_eq!(parse_quiet("8").unwrap(), Duration::from_secs(8));
    assert_eq!(parse_quiet("1m30s").unwrap(), Duration::from_secs(90));
    assert!(parse_quiet("0s").is_err());
    assert!(parse_quiet("11m").is_err());
    assert!(parse_quiet("5h").is_err());
    assert!(parse_quiet("5").is_ok());
}

#[test]
fn prompts_are_trimmed_and_bounded() {
    assert_eq!(validate_text("do it\n\n").unwrap(), "do it");
    assert!(validate_text(" \n ").is_err());
    assert!(validate_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    assert_eq!(normalized("a\n  b\tc "), "a b c");
}

#[test]
fn a_draft_left_in_the_input_or_an_open_editor_holds_it_back() {
    let q = queue(vec![item("a1", When::Done)]);
    let mut f = facts(status(AgentStatus::Done, 10));
    // Typed long ago, but never sent: pasting now would merge into it.
    f.typed_ago = Some(Duration::from_secs(600));
    f.draft = 1;
    waits(&decide(&q, &f), "never sent");
    f.draft = 0;
    assert_eq!(decide(&q, &f), Decision::Deliver("a1".into()));
    f.editing = true;
    waits(&decide(&q, &f), "editor");
}

#[test]
fn the_draft_model_follows_typing_editing_and_sending() {
    let mut d = Draft::default();
    d.feed(b"x");
    assert_eq!(d.chars(), 1);
    // Backspace (DEL and ^H) takes a character, multi-byte ones whole.
    d.feed("\u{e9}".as_bytes());
    assert_eq!(d.chars(), 2);
    d.feed(b"\x7f\x08");
    assert_eq!(d.chars(), 0);
    // Arrows, focus, and SGR mouse reports are not text.
    d.feed(b"\x1b[A\x1b[I\x1b[<0;3;4M\x1bOB");
    assert_eq!(d.chars(), 0);
    // Enter sends it; ^U and ^C clear it; ^W takes a word.
    d.feed(b"fix the bug\x17");
    assert_eq!(d.text(), "fix the ");
    d.feed(b"\x15");
    assert_eq!(d.chars(), 0);
    d.feed(b"abc\x03");
    assert_eq!(d.chars(), 0);
    d.feed(b"run it\r");
    assert_eq!(d.chars(), 0);
    // Escape twice running (one key per feed, as a browser sends them).
    d.feed(b"draft");
    d.feed(b"\x1b");
    assert_eq!(d.chars(), 5);
    d.feed(b"\x1b");
    assert_eq!(d.chars(), 0);
    d.feed(b"a\x1bb\x1b");
    assert_eq!(d.text(), "a");
    d.feed(b"\x15");
    // A bracketed paste is text, its line breaks included; Enter after
    // it sends it.
    d.feed(b"\x1b[200~one\rtwo\x1b[201~");
    assert_eq!(d.text(), "one\rtwo");
    d.feed(b"\r");
    assert_eq!(d.chars(), 0);
}

#[test]
fn typing_and_drafts_carry_across_a_handoff() {
    let old = QueueRuntime::default();
    old.note("s1", b"half a prom", true);
    old.note("s2", b"sent\r", true);
    let exported = old.typing_export();
    let json = serde_json::to_string(&exported).unwrap();
    let new = QueueRuntime::default();
    new.typing_import(serde_json::from_str(&json).unwrap());
    assert_eq!(new.draft_chars("s1"), 11);
    assert_eq!(new.draft_chars("s2"), 0);
    assert!(new.typed_ago("s1").unwrap() < Duration::from_secs(5));
    assert!(new.typed_ago("s2").is_some());
    new.clear_draft("s1");
    assert_eq!(new.draft_chars("s1"), 0);
    new.forget("s1");
    assert!(new.typed_ago("s1").is_none());
}

#[test]
fn an_editor_lease_lapses_or_is_let_go() {
    let rt = QueueRuntime::default();
    assert!(!rt.is_editing("i1"));
    rt.set_editing("i1", true);
    assert!(rt.is_editing("i1"));
    rt.set_editing("i1", false);
    assert!(!rt.is_editing("i1"));
}
