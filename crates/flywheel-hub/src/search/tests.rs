use super::transcripts::meta_cost;
use super::*;
use serde_json::json;
use std::io::Write;

fn far() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

/// What an index holds: each message's metadata and text.
fn indexed(index: &TranscriptIndex) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (unit, meta) in index.messages.iter().enumerate() {
        writeln!(out, "{meta:?} {:?}", index.corpus.unit(unit)).unwrap();
    }
    out
}

#[test]
fn captured_transcripts_index_the_golden_messages() {
    for &(name, codex) in crate::test_golden::TRANSCRIPTS {
        let format = if codex { Format::Codex } else { Format::Claude };
        let path = crate::test_golden::fixture(name);
        let mut index = TranscriptIndex::new(path.clone(), format);
        assert!(index.update(far()));
        assert_eq!(index.error, None);
        let whole = indexed(&index);
        // Appended to in pieces, the same messages.
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join(name);
        let bytes = std::fs::read(&path).unwrap();
        for size in [1, 7, 64, 4096] {
            let _ = std::fs::remove_file(&copy);
            std::fs::write(&copy, b"").unwrap();
            let mut index = TranscriptIndex::new(copy.clone(), format);
            for piece in bytes.chunks(size) {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&copy)
                    .unwrap();
                file.write_all(piece).unwrap();
                assert!(index.update(far()));
            }
            assert_eq!(indexed(&index), whole, "{name} by {size}");
        }
        crate::test_golden::assert_golden(&format!("{name}.search.txt"), &whole);
    }
}

#[test]
fn literal_queries_match_case_insensitively_and_escape_metacharacters() {
    let m = Matcher::new("a.b(", false, false).unwrap();
    assert!(m.is_match("x A.B( y"));
    assert!(!m.is_match("axb("));
    let m = Matcher::new("Error", false, true).unwrap();
    assert!(m.is_match("an Error"));
    assert!(!m.is_match("an error"));
}

#[test]
fn regex_queries_compile_with_limits_and_skip_empty_matches() {
    let m = Matcher::new(r"^cargo (test|build)$", true, false).unwrap();
    let corpus = Corpus::from_units(["cargo test", "  cargo test", "CARGO BUILD", "cargo testing"]);
    let found = scan(&corpus, &m, 10, far());
    let units: Vec<usize> = found.hits.iter().map(|h| h.unit).collect();
    assert_eq!(units, [0, 2], "^ and $ anchor at line boundaries");
    assert!(
        Matcher::new("(", true, false)
            .unwrap_err()
            .contains("invalid regex")
    );
    // A pattern whose compiled form is huge is refused, not built.
    let huge = Matcher::new(r"\w{1000}\w{1000}\w{1000}", true, false).unwrap_err();
    assert!(huge.contains("too large"), "{huge}");
    assert!(Matcher::new("", false, false).is_err());
    assert!(Matcher::new(&"x".repeat(MAX_QUERY_BYTES + 1), false, false).is_err());
    // `a*` matches the empty string everywhere: only real matches count.
    let m = Matcher::new("z*", true, false).unwrap();
    let found = scan(&Corpus::from_units(["abc", "zz", "q"]), &m, 10, far());
    assert_eq!(found.units, 1);
    assert_eq!(found.hits[0].unit, 1);
}

#[test]
fn scans_keep_the_newest_hits_count_all_and_one_per_unit() {
    let lines: Vec<String> = (0..100)
        .map(|i| format!("line {i} needle needle"))
        .collect();
    let corpus = Corpus::from_units(&lines);
    let found = scan(
        &corpus,
        &Matcher::new("needle", false, false).unwrap(),
        3,
        far(),
    );
    assert!(found.complete);
    assert_eq!(found.units, 100);
    let units: Vec<usize> = found.hits.iter().map(|h| h.unit).collect();
    assert_eq!(units, [97, 98, 99]);
    assert!(found.hits.iter().all(|h| h.matches == 2));
    let last = found.hits[2];
    assert_eq!(&corpus.unit(99)[last.start..last.end], "needle");
    assert_eq!(last.start, "line 99 ".len());
}

#[test]
fn a_spent_budget_stops_the_scan() {
    let lines: Vec<String> = (0..20_000).map(|i| format!("x{i}")).collect();
    let corpus = Corpus::from_units(&lines);
    let found = scan(
        &corpus,
        &Matcher::new("x", false, false).unwrap(),
        5,
        Instant::now(),
    );
    assert!(!found.complete);
    assert!(found.units < 20_000);
}

#[test]
fn matches_never_run_across_units() {
    let corpus = Corpus::from_units(["build done", "next step"]);
    let spanning = Matcher::new(r"done\s+next", true, false).unwrap();
    let found = scan(&corpus, &spanning, 5, far());
    assert_eq!(found.units, 0, "neither unit holds the pattern");
    // A separator-only match is no hit (no empty match text).
    for pattern in [r"\s+$", r"\n", r"\s"] {
        let m = Matcher::new(pattern, true, false).unwrap();
        let found = scan(&Corpus::from_units(["ab", "cd", "ef"]), &m, 5, far());
        assert_eq!(found.units, 0, "{pattern}");
    }
    // A crossing match is retried inside the unit: `o\s\w+` crosses
    // from "go" into "on" but "do it" holds its own match later on.
    let corpus = Corpus::from_units(["do it go", "on", "multi\nline x"]);
    let m = Matcher::new(r"o\s\w+", true, false).unwrap();
    let found = scan(&corpus, &m, 5, far());
    let hits: Vec<(usize, &str)> = found
        .hits
        .iter()
        .map(|h| (h.unit, &corpus.unit(h.unit)[h.start..h.end]))
        .collect();
    assert_eq!(hits, [(0, "o it")]);
    assert_eq!(
        found.hits[0].matches, 1,
        "the crossing \"o\\non\" is not counted"
    );
    // Newlines inside one unit (a message, a block) still match.
    let m = Matcher::new(r"multi\nline", true, false).unwrap();
    let found = scan(&corpus, &m, 5, far());
    assert_eq!(found.hits[0].unit, 2);
    // `^`/`$` still see unit edges.
    let m = Matcher::new(r"^on$", true, false).unwrap();
    assert_eq!(scan(&corpus, &m, 5, far()).hits[0].unit, 1);
}

#[test]
fn corpus_units_and_front_drops() {
    let mut corpus = Corpus::from_units(["a", "", "multi\nline", "z"]);
    assert_eq!(corpus.len(), 4);
    assert_eq!(corpus.unit(1), "");
    assert_eq!(corpus.unit(2), "multi\nline");
    assert_eq!(corpus.unit_of(corpus.starts[2] + 7), 2);
    assert_eq!(
        corpus.unit_of(1),
        0,
        "a separator belongs to the unit before"
    );
    corpus.drop_front(2);
    assert_eq!(corpus.len(), 2);
    assert_eq!(corpus.unit(0), "multi\nline");
    assert_eq!(corpus.unit(1), "z");
    corpus.drop_front(5);
    assert_eq!(corpus.len(), 0);
}

#[test]
fn snippets_show_the_match_line_in_bounded_context() {
    let s = snippet("first\n  run cargo test now\nlast", 12, 17);
    assert_eq!(
        (s.before.as_str(), s.matched.as_str(), s.after.as_str()),
        ("run ", "cargo", " test now")
    );
    assert_eq!(s.line, 1);
    let long = format!("{}needle{}", "a".repeat(500), "b\tc".repeat(200));
    let s = snippet(&long, 500, 506);
    assert!(s.before.starts_with('…'));
    assert_eq!(s.before.chars().count(), BEFORE_CHARS + 1);
    assert_eq!(s.matched, "needle");
    assert!(s.after.ends_with('…'));
    assert!(!s.after.contains('\t'), "controls become spaces");
    // Multi-byte text is cut on character boundaries.
    let text = format!("{}é needle ü{}", "ü".repeat(100), "é".repeat(300));
    let at = text.find("needle").unwrap();
    let s = snippet(&text, at, at + 6);
    assert_eq!(s.matched, "needle");
    assert!(s.before.ends_with("é "));
}

#[test]
fn claude_lines_give_text_and_tool_calls_but_not_thinking_or_results() {
    let line = json!({
        "type": "assistant", "uuid": "a1", "timestamp": "2026-11-02T10:00:00Z",
        "message": {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "secret plan"},
            {"type": "text", "text": "I will run the tests."},
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test -p hub", "description": "Run tests"}}
        ]}
    });
    let messages = claude_messages(&line);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text, "I will run the tests.");
    assert_eq!(messages[0].role, Role::Assistant);
    assert_eq!(messages[1].kind, MessageKind::Tool);
    assert_eq!(messages[1].tool.as_deref(), Some("Bash"));
    assert_eq!(messages[1].id, "a1#tool0");
    assert!(messages[1].text.contains("command: cargo test -p hub"));
    let user = json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "fix the login bug"}});
    assert_eq!(claude_messages(&user)[0].role, Role::User);
    let result = json!({"type": "user", "uuid": "u2", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}});
    assert!(claude_messages(&result).is_empty());
    let side = json!({"type": "assistant", "uuid": "s", "isSidechain": true,
        "message": {"content": "subagent"}});
    assert!(claude_messages(&side).is_empty());
    assert!(claude_messages(&json!({"type": "summary", "summary": "x"})).is_empty());
}

#[test]
fn codex_lines_give_messages_and_calls_without_injected_context() {
    let reply = json!({"timestamp": "2026-11-02T10:00:00Z", "type": "response_item", "payload": {
        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}});
    let m = codex_messages(&reply, 42);
    assert_eq!(m[0].text, "Done.");
    assert_eq!(m[0].id, "@42");
    let user = json!({"type": "response_item", "payload": {"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]}});
    assert!(codex_messages(&user, 0).is_empty());
    let call = json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
        "arguments": "{\"command\":[\"bash\",\"-lc\",\"make test\"]}", "call_id": "c1"}});
    let m = codex_messages(&call, 0);
    assert_eq!(m[0].kind, MessageKind::Tool);
    assert_eq!(m[0].id, "c1");
    assert!(m[0].text.contains("make test"), "{}", m[0].text);
    let local =
        json!({"type": "local_shell_call", "action": {"type": "exec", "command": ["ls", "-la"]}});
    assert_eq!(codex_messages(&local, 7)[0].text, "shell\nls -la");
    let dev = json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
        "content": [{"type": "input_text", "text": "rules"}]}});
    assert!(codex_messages(&dev, 0).is_empty());
}

#[test]
fn transcript_indexes_read_incrementally_and_reread_replaced_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let line = |uuid: &str, text: &str| {
        json!({"type": "assistant", "uuid": uuid, "message": {"content": text}}).to_string()
    };
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "{}", line("a", "first answer")).unwrap();
    // A half-written line is held until its newline arrives.
    write!(
        file,
        "{{\"type\":\"user\",\"uuid\":\"b\",\"message\":{{\"content\":\"sec"
    )
    .unwrap();
    file.flush().unwrap();
    let mut index = TranscriptIndex::new(path.clone(), Format::Claude);
    assert!(index.update(far()));
    assert_eq!(index.messages.len(), 1);
    writeln!(file, "ond prompt\"}}}}").unwrap();
    writeln!(file, "not json").unwrap();
    assert!(index.update(far()));
    assert_eq!(index.messages.len(), 2);
    assert_eq!(index.corpus.unit(1), "second prompt");
    assert_eq!(index.messages[1].role, Role::User);
    // Replaced (new inode): read from the start, renumbered.
    let other = dir.path().join("new.jsonl");
    std::fs::write(&other, format!("{}\n", line("z", "fresh"))).unwrap();
    std::fs::rename(&other, &path).unwrap();
    assert!(index.update(far()));
    assert_eq!(index.messages.len(), 1);
    assert_eq!(index.corpus.unit(0), "fresh");
    // Gone: an error, nothing kept from the failed read.
    std::fs::remove_file(&path).unwrap();
    index.update(far());
    assert!(index.error.as_deref().unwrap().contains("cannot read"));
}

#[test]
fn the_transcript_cap_counts_message_metadata_not_only_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    // Tool calls with a one-character name and no input: next to no
    // text, but a uuid, timestamp, and tool name each.
    let mut text = String::new();
    for i in 0..2000 {
        text.push_str(
            &json!({"type": "assistant", "uuid": format!("{i:036}"),
                "timestamp": "2026-11-02T10:11:12.000Z",
                "message": {"content": [{"type": "tool_use", "name": "x", "input": {}}]}})
            .to_string(),
        );
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    let mut index = TranscriptIndex::new(path, Format::Claude);
    index.cap = 64 * 1024;
    assert!(index.update(far()));
    assert!(
        index.corpus.text.len() < 16 * 1024,
        "the text alone is far under the cap"
    );
    assert!(
        index.footprint() <= index.cap,
        "{} > {}",
        index.footprint(),
        index.cap
    );
    assert!(index.first_index() > 0, "the oldest were dropped");
    assert_eq!(index.next_index, 2000);
    assert_eq!(index.messages.len(), index.corpus.len());
    let counted: usize = index.messages.iter().map(meta_cost).sum();
    assert_eq!(index.meta_bytes, counted);
}

#[test]
fn idle_and_least_recently_used_transcript_indexes_are_evicted() {
    let search = SearchIndex::default();
    let slot = |name: &str, bytes: usize, ago: u64| {
        let slot = search.transcript(Path::new(name), Format::Claude);
        slot.bytes.store(bytes, Ordering::Relaxed);
        *slot.used.lock().unwrap() = Instant::now() - Duration::from_secs(ago);
        slot
    };
    drop(slot("/a", 100, 50));
    drop(slot("/b", 100, 40));
    drop(slot("/c", 100, 30));
    drop(slot("/idle", 100, 1000));
    // One a search holds right now is never dropped, however old.
    let busy = slot("/busy", 100, 2000);
    assert_eq!(search.transcript_count(), 5);
    search.sweep_with(1000, Duration::from_secs(600));
    assert_eq!(search.transcript_count(), 4, "the idle one went");
    // Over the cap: the least recently used go first.
    search.sweep_with(250, Duration::from_secs(600));
    let left: HashSet<PathBuf> = search.transcripts.lock().unwrap().keys().cloned().collect();
    assert_eq!(left, HashSet::from(["/c".into(), "/busy".into()]));
    assert_eq!(search.transcript_bytes(), 200);
    drop(busy);
    search.sweep_with(250, Duration::from_secs(600));
    assert_eq!(search.transcript_count(), 1, "the idle busy one, now free");
}

#[test]
fn over_long_lines_and_messages_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let big = "x".repeat(MAX_MESSAGE_BYTES + 100);
    let mut text =
        json!({"type": "assistant", "uuid": "a", "message": {"content": big}}).to_string();
    text.push('\n');
    text.push_str(&"y".repeat(MAX_LINE_BYTES + 10));
    text.push('\n');
    text.push_str(
        &json!({"type": "assistant", "uuid": "b", "message": {"content": "after"}}).to_string(),
    );
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    let mut index = TranscriptIndex::new(path, Format::Claude);
    assert!(index.update(far()));
    assert_eq!(index.messages.len(), 2, "the over-long line is skipped");
    assert!(index.messages[0].truncated);
    assert_eq!(index.corpus.unit(0).len(), MAX_MESSAGE_BYTES);
    assert_eq!(index.corpus.unit(1), "after");
}

#[test]
fn transcript_reading_stops_at_the_deadline_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut text = String::new();
    for i in 0..20_000 {
        text.push_str(&json!({"type": "user", "uuid": format!("u{i}"), "message": {"content": format!("prompt {i}")}}).to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    let mut index = TranscriptIndex::new(path, Format::Claude);
    assert!(!index.update(Instant::now()), "no time: nothing read yet");
    assert!(index.update(far()));
    assert_eq!(index.messages.len(), 20_000);
    assert_eq!(index.messages[19_999].index, 19_999);
}
