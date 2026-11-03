use super::*;

/// A fixture screen: Claude Code screens captured from 2.1.306 in a hub
/// session (`GET /screen`), Codex screens laid out from the codex TUI's
/// own snapshot tests.
fn fixture(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("detect/fixtures")
        .join(format!("{name}.txt"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(str::to_owned)
        .collect()
}

fn read(kind: SessionKind, name: &str) -> Verdict {
    Manifest::bundled(kind).evaluate(&fixture(name))
}

#[track_caller]
fn assert_reads(kind: SessionKind, name: &str, state: Option<ScreenState>, rule: Option<&str>) {
    let verdict = read(kind, name);
    assert_eq!(
        (verdict.state, verdict.rule.as_deref()),
        (state, rule),
        "{name}: {verdict:#?}"
    );
}

use ScreenState::{Blocked, Hold, Idle, Working};

#[test]
fn bundled_manifests_parse_and_name_their_files() {
    for kind in SessionKind::ALL {
        let manifest = Manifest::bundled(kind);
        assert_eq!(manifest.kind, kind);
        assert_eq!(manifest.origin, Origin::Bundled);
        assert!(manifest.rule_ids().count() > 1);
        assert_eq!(manifest.file(), format!("{}.toml", kind.as_str()));
    }
}

#[test]
fn claude_rules_read_real_claude_code_screens() {
    let claude = SessionKind::Claude;
    assert_reads(claude, "claude-trust", Some(Blocked), Some("folder_trust"));
    assert_reads(
        claude,
        "claude-permission-bash",
        Some(Blocked),
        Some("permission_prompt"),
    );
    assert_reads(
        claude,
        "claude-permission-write",
        Some(Blocked),
        Some("permission_prompt"),
    );
    assert_reads(
        claude,
        "claude-ask-question",
        Some(Blocked),
        Some("question"),
    );
    assert_reads(claude, "claude-working", Some(Working), Some("turn_footer"));
    assert_reads(
        claude,
        "claude-approved",
        Some(Working),
        Some("turn_footer"),
    );
    assert_reads(claude, "claude-idle", Some(Idle), Some("prompt"));
    assert_reads(claude, "claude-idle-manual", Some(Idle), Some("prompt"));
    assert_reads(claude, "claude-interrupted", Some(Idle), Some("prompt"));
    assert_reads(claude, "claude-done", Some(Idle), Some("prompt"));
    assert_reads(
        claude,
        "claude-question-cancelled",
        Some(Idle),
        Some("prompt"),
    );
    assert_reads(
        claude,
        "claude-transcript-view",
        Some(Hold),
        Some("transcript_viewer"),
    );
}

#[test]
fn claude_details_and_evidence_come_from_the_screen() {
    let verdict = read(SessionKind::Claude, "claude-permission-write");
    assert_eq!(
        verdict.detail.as_deref(),
        Some("Do you want to create notes.txt?")
    );
    let rows: Vec<usize> = verdict.evidence.iter().map(|e| e.row).collect();
    assert!(rows.windows(2).all(|w| w[0] <= w[1]), "{rows:?}");
    assert!(
        verdict
            .evidence
            .iter()
            .any(|e| e.text.trim() == "Esc to cancel · Tab to amend"),
        "{verdict:#?}"
    );
    let trust = read(SessionKind::Claude, "claude-trust");
    assert_eq!(
        trust.detail.as_deref(),
        Some("asks whether to trust this folder")
    );
    assert_eq!(
        read(SessionKind::Claude, "claude-ask-question")
            .detail
            .as_deref(),
        Some("asks a question")
    );
}

#[test]
fn claude_turn_spinner_reads_working_without_the_footer() {
    let mut rows = fixture("claude-working");
    // A narrow terminal cut the footer hint.
    for row in &mut rows {
        if row.contains("esc to interrupt") {
            *row = "  ⏸ manual mode on".into();
        }
    }
    assert_eq!(
        Manifest::bundled(SessionKind::Claude)
            .evaluate(&rows)
            .rule
            .as_deref(),
        Some("turn_spinner")
    );
}

#[test]
fn codex_rules_read_codex_tui_screens() {
    let codex = SessionKind::Codex;
    assert_reads(codex, "codex-trust", Some(Blocked), Some("folder_trust"));
    assert_reads(
        codex,
        "codex-approval-exec",
        Some(Blocked),
        Some("approval"),
    );
    assert_reads(
        codex,
        "codex-approval-patch",
        Some(Blocked),
        Some("approval"),
    );
    assert_reads(
        codex,
        "codex-approval-network",
        Some(Blocked),
        Some("approval"),
    );
    assert_reads(
        codex,
        "codex-legacy-allow",
        Some(Blocked),
        Some("legacy_approval"),
    );
    assert_reads(codex, "codex-question", Some(Blocked), Some("question"));
    assert_reads(codex, "codex-working", Some(Working), Some("turn_status"));
    assert_reads(codex, "codex-idle", Some(Idle), Some("composer"));
    assert_reads(codex, "codex-interrupted", Some(Idle), Some("composer"));
    // A subagent's approval (footer adds "or o to open thread"), an MCP
    // server's (approval overlay and elicitation form), and approvals
    // whose diff or command pushes the title far above the footer.
    for name in [
        "codex-approval-cross-thread",
        "codex-approval-mcp",
        "codex-approval-long-patch",
        "codex-approval-long-command",
    ] {
        assert_reads(codex, name, Some(Blocked), Some("approval"));
    }
    assert_reads(codex, "codex-mcp-form", Some(Blocked), Some("mcp_form"));
    // The detail names what is approved, not just the question.
    let detail = |name| read(codex, name).detail;
    assert_eq!(
        detail("codex-approval-exec").as_deref(),
        Some("Would you like to run the following command? $ cargo test -p parser")
    );
    assert_eq!(
        detail("codex-approval-cross-thread").as_deref(),
        Some("Would you like to run the following command? $ echo hi")
    );
    assert_eq!(
        detail("codex-approval-patch").as_deref(),
        Some(
            "Would you like to make the following edits? Destination: /home/me/code/app/src/parser.rs"
        )
    );
    assert_eq!(
        detail("codex-approval-long-patch").as_deref(),
        Some("Would you like to make the following edits? src/lib.rs (+30 -0)")
    );
    assert_eq!(
        detail("codex-approval-mcp").as_deref(),
        Some("github needs your approval.")
    );
    assert_eq!(
        detail("codex-mcp-form").as_deref(),
        Some("Allow this request?")
    );
    assert_eq!(
        read(codex, "codex-approval-network").detail.as_deref(),
        Some("Do you want to approve network access to \"example.com\"?")
    );
}

#[test]
fn blocked_is_strict_quoted_prompts_and_unknown_screens_never_block() {
    // An answer quoting the approval dialog's text is not the dialog.
    assert_reads(
        SessionKind::Codex,
        "codex-quoted-approval",
        Some(Idle),
        Some("composer"),
    );
    // A dialog footer alone, or the question alone, is not enough.
    let codex = Manifest::bundled(SessionKind::Codex);
    let footer_only = vec!["  Press enter to confirm or esc to cancel".to_owned()];
    assert_eq!(codex.evaluate(&footer_only).state, None);
    let mut exec = fixture("codex-approval-exec");
    exec.retain(|row| !row.contains("Press enter"));
    assert_ne!(codex.evaluate(&exec).state, Some(Blocked));
    // The dialog scrolled up into history with the composer back below.
    let mut scrolled = fixture("codex-approval-exec");
    scrolled.extend(fixture("codex-idle"));
    assert_eq!(codex.evaluate(&scrolled).state, Some(Idle));
    // Unknown screens: no verdict.
    for kind in SessionKind::ALL {
        let manifest = Manifest::bundled(kind);
        assert_eq!(manifest.evaluate(&[]).state, None);
        let noise: Vec<String> = (0..40).map(|i| format!("compiling crate {i}")).collect();
        assert_eq!(manifest.evaluate(&noise), Verdict::default());
    }
    // A Claude prompt that merely mentions a dialog.
    let mut claude = fixture("claude-idle-manual");
    claude[6] = "❯ why does it ask Do you want to proceed? and 1. Yes".into();
    assert_eq!(
        Manifest::bundled(SessionKind::Claude)
            .evaluate(&claude)
            .state,
        Some(Idle)
    );
}

#[test]
fn shell_rules_see_password_and_confirm_prompts_on_the_last_line() {
    let shell = Manifest::bundled(SessionKind::Shell);
    let read = |lines: &[&str]| {
        let rows: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
        let v = shell.evaluate(&rows);
        (v.state, v.rule)
    };
    assert_eq!(
        read(&["$ sudo true", "[sudo] password for me:"]),
        (Some(Blocked), Some("password".into()))
    );
    assert_eq!(
        read(&["Enter passphrase for key '/home/me/.ssh/id_ed25519':"]),
        (Some(Blocked), Some("password".into()))
    );
    assert_eq!(
        read(&["Do you want to continue? [Y/n]"]),
        (Some(Blocked), Some("confirm".into()))
    );
    assert_eq!(
        read(&["Are you sure you want to continue connecting (yes/no/[fingerprint])?"]),
        (Some(Blocked), Some("confirm".into()))
    );
    assert_eq!(
        read(&["me@box:~/code$"]),
        (Some(Idle), Some("prompt".into()))
    );
    // A password prompt scrolled up is history, not a wait.
    assert_eq!(
        read(&["[sudo] password for me:", "me@box:~$"]),
        (Some(Idle), Some("prompt".into()))
    );
    assert_eq!(read(&["building..."]), (None, None));
}

#[test]
fn traces_say_why_each_rule_did_not_match() {
    let manifest = Manifest::bundled(SessionKind::Codex);
    let (verdict, rules) = manifest.trace(&fixture("codex-working"));
    assert_eq!(verdict.rule.as_deref(), Some("turn_status"));
    let ids: Vec<&str> = manifest.rule_ids().collect();
    assert_eq!(rules.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ids);
    let at = ids.iter().position(|id| *id == "turn_status").unwrap();
    assert!(rules[at].matched && rules[at].reason.is_none());
    for rule in &rules[..at] {
        assert!(!rule.matched);
        assert!(rule.reason.as_deref().is_some_and(|r| !r.is_empty()));
    }
    for rule in &rules[at + 1..] {
        assert_eq!(
            rule.reason.as_deref(),
            Some("not tried: an earlier rule matched")
        );
    }
    let approval = rules.iter().find(|r| r.id == "approval").unwrap();
    assert!(
        approval
            .reason
            .as_deref()
            .unwrap()
            .starts_with("condition 1: no line matches"),
        "{approval:?}"
    );
}

fn parse(text: &str) -> Result<Manifest, ManifestError> {
    Manifest::parse(text, SessionKind::Codex, Origin::Bundled)
}

#[test]
fn manifests_are_validated() {
    let ok =
        "kind = \"codex\"\nversion = 2\n[[rule]]\nid = \"a\"\nstate = \"idle\"\nany = [\"x\"]\n";
    assert_eq!(parse(ok).unwrap().version, 2);
    let bad = |text: &str, needle: &str| {
        let e = parse(text).unwrap_err().0;
        assert!(e.contains(needle), "{e:?} lacks {needle:?}");
    };
    bad(
        "kind = \"claude\"\nversion = 1\n",
        "this file is for \"codex\"",
    );
    bad("kind = \"codex\"\nversion = 1\n", "no [[rule]]s");
    bad(&ok.replace("idle", "busy"), "state must be");
    bad(&ok.replace("id = \"a\"", "id = \"A b\""), "id must be");
    bad(
        &format!("{ok}[[rule]]\nid = \"a\"\nstate = \"idle\"\nany = [\"y\"]\n"),
        "duplicate id",
    );
    bad(&ok.replace("any = [\"x\"]", "lines = 3"), "no conditions");
    bad(
        &ok.replace("any = [\"x\"]", "any = [\"x\"]\nlines = 0"),
        "lines must be",
    );
    bad(
        &ok.replace("any = [\"x\"]", "any = [\"x\"]\nlines = 101"),
        "lines must be",
    );
    bad(&ok.replace("any = [\"x\"]", "line = ['(']"), "bad regex");
    bad(
        &ok.replace("any = [\"x\"]", "any = [\"  \"]"),
        "empty string",
    );
    bad(
        &ok.replace("any = [\"x\"]", "any = [\"x\"]\ndetail = \"d\""),
        "for blocked rules",
    );
    bad(
        &ok.replace("any = [\"x\"]", "anyy = [\"x\"]"),
        "unknown field",
    );
    bad(
        &ok.replace("any = [\"x\"]", "any = [\"x\"]\n[[rule.when]]\nlines = 2"),
        "when 1: no conditions",
    );
    let many: String = (0..=MAX_PATTERNS).map(|i| format!("\"p{i}\",")).collect();
    bad(
        &ok.replace("[\"x\"]", &format!("[{many}]")),
        "more than 16 patterns",
    );
    let long = "x".repeat(MAX_PATTERN_CHARS + 1);
    bad(
        &ok.replace("any = [\"x\"]", &format!("line = ['{long}']")),
        "longer than",
    );
    // A regex whose compiled program blows the size cap is refused.
    bad(
        &ok.replace("any = [\"x\"]", "line = ['\\w{200}\\w{200}']"),
        "bad regex",
    );
    let rules: String = (0..=MAX_RULES)
        .map(|i| format!("[[rule]]\nid = \"r{i}\"\nstate = \"idle\"\nany = [\"x\"]\n"))
        .collect();
    bad(
        &format!("kind = \"codex\"\nversion = 1\n{rules}"),
        "more than 64 rules",
    );
}

#[test]
fn rows_are_cut_and_only_the_live_bottom_is_read() {
    let manifest = parse(
        "kind = \"codex\"\nversion = 1\n[[rule]]\nid = \"far\"\nstate = \"blocked\"\nlines = 2\nany = [\"needle\"]\n[[rule]]\nid = \"tail\"\nstate = \"idle\"\nlines = 1\nline = ['x{3}$']\n",
    )
    .unwrap();
    // Blank rows do not count toward `lines`.
    let rows: Vec<String> = ["needle", "", "", "a", "", "b"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(manifest.evaluate(&rows).state, None);
    let rows: Vec<String> = ["a", "needle", "", "", "b", ""]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let verdict = manifest.evaluate(&rows);
    assert_eq!(verdict.rule.as_deref(), Some("far"));
    assert_eq!(verdict.evidence[0].row, 2);
    // A row is read to MAX_LINE_CHARS only.
    let wide = format!("{}xxx", "y".repeat(MAX_LINE_CHARS));
    assert_eq!(manifest.evaluate(&[wide]).state, None);
    let fits = format!("{}xxx", "y".repeat(MAX_LINE_CHARS - 3));
    assert_eq!(manifest.evaluate(&[fits]).state, Some(Idle));
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    // A different length every write: the stamp sees the change even
    // within one mtime tick.
    std::fs::write(dir.join(name), text).unwrap();
}

/// Let the once-a-second recheck run again.
fn expire(detection: &Detection) {
    detection.loaded.lock().unwrap().checked.take();
}

#[test]
fn overrides_win_bad_ones_are_reported_and_changes_reload() {
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join(OVERRIDE_DIR);
    let detection = Detection::with_overrides_quiet(state.path());
    assert_eq!(
        detection.manifest(SessionKind::Codex).origin,
        Origin::Bundled
    );
    assert!(detection.problems().is_empty());

    let custom = "kind = \"codex\"\nversion = 7\n[[rule]]\nid = \"mine\"\nstate = \"blocked\"\ndetail = \"custom\"\nany = [\"approve me\"]\n";
    write(&dir, "codex.toml", custom);
    write(&dir, "gemini.toml", "kind = \"gemini\"\n");
    // Within the recheck interval nothing is re-read...
    assert_eq!(detection.manifest(SessionKind::Codex).version, 1);
    expire(&detection);
    let codex = detection.manifest(SessionKind::Codex);
    assert_eq!(codex.version, 7);
    assert_eq!(
        codex.origin,
        Origin::Override {
            path: dir.join("codex.toml")
        }
    );
    let rows = vec!["please approve me".to_owned()];
    assert_eq!(codex.evaluate(&rows).rule.as_deref(), Some("mine"));
    // ...other kinds keep their bundled manifest...
    assert_eq!(
        detection.manifest(SessionKind::Claude).origin,
        Origin::Bundled
    );
    let problems = detection.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, dir.join("gemini.toml"));
    assert!(problems[0].error.contains("no such kind"));

    // An invalid edit: reported, and the bundled manifest is back in force
    // (never a half-loaded override).
    write(&dir, "codex.toml", &custom.replace("blocked", "stuck!"));
    expire(&detection);
    assert_eq!(
        detection.manifest(SessionKind::Codex).origin,
        Origin::Bundled
    );
    let problems = detection.problems();
    let codex_problem = problems
        .iter()
        .find(|p| p.path == dir.join("codex.toml"))
        .unwrap();
    assert!(
        codex_problem.error.contains("state must be")
            && codex_problem
                .error
                .contains("the bundled codex.toml is used"),
        "{codex_problem:?}"
    );
    // Fixed again, then removed.
    write(
        &dir,
        "codex.toml",
        &custom.replace("version = 7", "version = 8"),
    );
    expire(&detection);
    assert_eq!(detection.manifest(SessionKind::Codex).version, 8);
    std::fs::remove_file(dir.join("codex.toml")).unwrap();
    expire(&detection);
    assert_eq!(
        detection.manifest(SessionKind::Codex).origin,
        Origin::Bundled
    );

    // Oversized files are not read.
    let huge = format!("{custom}# {}\n", "x".repeat(MAX_MANIFEST_BYTES as usize));
    write(&dir, "shell.toml", &huge.replace("codex", "shell"));
    expire(&detection);
    assert!(
        detection
            .problems()
            .iter()
            .any(|p| p.path == dir.join("shell.toml") && p.error.contains("larger than"))
    );
    assert_eq!(
        detection.manifest(SessionKind::Shell).origin,
        Origin::Bundled
    );
}

#[test]
fn explanations_carry_the_rule_evidence_trace_and_note() {
    let detection = Detection::bundled();
    let rows = fixture("codex-approval-exec");
    let e = explain::explain_screen(&detection, SessionKind::Codex, Some(&rows));
    let screen = e.screen.as_ref().unwrap();
    assert_eq!(screen.verdict.rule.as_deref(), Some("approval"));
    assert!(screen.fallback.is_none());
    assert!(!screen.verdict.evidence.is_empty());
    assert_eq!(e.manifest.file, "codex.toml");
    assert!(e.note.starts_with("idle is not authorization"));
    let json = serde_json::to_value(&e).unwrap();
    assert_eq!(json["manifest"]["origin"], "bundled");
    assert_eq!(json["screen"]["state"], "blocked");
    assert_eq!(json["screen"]["evidence"][0]["row"], 6);
    let mut out = Vec::new();
    explain::render(&json, &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("screen   blocked by codex.toml#approval (bundled, version 1)"),
        "{text}"
    );
    assert!(text.contains("+ approval"), "{text}");
    assert!(
        text.contains("note     idle is not authorization"),
        "{text}"
    );
    // Nothing matched: the fallback says what that means.
    let e = explain::explain_screen(&detection, SessionKind::Codex, Some(&["make".to_owned()]));
    assert!(
        e.screen
            .unwrap()
            .fallback
            .unwrap()
            .contains("never blocked")
    );
}

#[test]
fn fw_hub_explain_reads_a_saved_screen_against_local_overrides() {
    let state = tempfile::tempdir().unwrap();
    let screen = state.path().join("screen.txt");
    std::fs::write(&screen, fixture("codex-approval-patch").join("\n")).unwrap();
    let run = |extra: &[&str]| {
        let mut args: Vec<String> = ["explain", "--file"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        args.push(screen.display().to_string());
        args.extend(["--state-dir".to_owned(), state.path().display().to_string()]);
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::cli::run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    };
    let (code, out, err) = run(&["--kind", "codex"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("screen   blocked by codex.toml#approval (bundled, version 1)  -- Would you like to make the following edits?"), "{out}");
    assert!(
        out.contains("Press enter to confirm or esc to cancel"),
        "{out}"
    );
    // Each rule's reason is printed whole (its regex is what to fix).
    assert!(
        out.contains(
            "- folder_trust     blocked  condition 1: no line matches '(?i)^\\s*(?:enter continue\\b|press enter to continue\\b)' in the last 1 lines"
        ),
        "{out}"
    );
    // An override for codex is used (and a broken one for claude is
    // reported, not used).
    write(
        &state.path().join(OVERRIDE_DIR),
        "codex.toml",
        "kind = \"codex\"\nversion = 3\n[[rule]]\nid = \"edits\"\nstate = \"working\"\nany = [\"make the following edits\"]\n",
    );
    write(
        &state.path().join(OVERRIDE_DIR),
        "claude.toml",
        "kind = \"claude\"\n",
    );
    let (code, out, _) = run(&["--agent", "codex", "--json"]);
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["screen"]["rule"], "edits");
    assert_eq!(json["manifest"]["origin"], "override");
    assert_eq!(json["manifest"]["version"], 3);
    // A status it sets names the override, not the bundled file.
    let detection = Detection::with_overrides_quiet(state.path());
    assert_eq!(
        detection.manifest(SessionKind::Codex).source_file(),
        "agent-detection/codex.toml"
    );
    assert_eq!(
        Manifest::bundled(SessionKind::Codex).source_file(),
        "codex.toml"
    );
    assert!(
        json["problems"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("claude.toml")
    );
    // --file needs --kind; neither a session nor --file is an error.
    let (code, _, err) = run(&[]);
    assert_eq!(code, 2, "{err}");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        crate::cli::run(&["explain".to_owned()], &mut out, &mut err),
        2
    );
}
