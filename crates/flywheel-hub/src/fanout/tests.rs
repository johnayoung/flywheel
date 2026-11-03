use super::*;
use crate::state::HubStore;
use serde_json::json;

fn request(value: serde_json::Value) -> CreateFanout {
    serde_json::from_value(value).unwrap()
}

fn base_request() -> serde_json::Value {
    json!({
        "workspace_id": "w",
        "prompt": "Make the answer 42 in answer.txt",
        "candidates": [{"kind": "claude", "model": "opus"}, {"kind": "codex"}],
        "graders": ["test \"$(cat answer.txt)\" = 42", {"run": "true", "name": "smoke"}],
    })
}

#[test]
fn requests_are_validated_and_normalized() {
    let valid = request(base_request()).validated().unwrap();
    assert_eq!(valid.name, "Make the answer 42 in answer.txt");
    assert_eq!(valid.candidates.len(), 2);
    assert_eq!(valid.candidates[0].1.model.as_deref(), Some("opus"));
    assert_eq!(valid.graders[0].name, None);
    assert_eq!(valid.graders[1].label(), "smoke");
    assert_eq!(valid.timeout_secs, DEFAULT_TIMEOUT_SECS);

    let mut one = base_request();
    one["candidates"] = json!([{"kind": "claude"}]);
    assert!(request(one).validated().unwrap_err().contains("2 to 6"));

    let mut seven = base_request();
    seven["candidates"] = json!(vec![json!({"kind": "claude"}); 7]);
    assert!(request(seven).validated().is_err());

    let mut shell = base_request();
    shell["candidates"] = json!([{"kind": "claude"}, {"kind": "shell"}]);
    assert!(
        request(shell)
            .validated()
            .unwrap_err()
            .contains("candidate 2")
    );

    let mut codex_on_claude = base_request();
    codex_on_claude["candidates"] =
        json!([{"kind": "claude", "sandbox": "read-only"}, {"kind": "codex"}]);
    assert!(request(codex_on_claude).validated().is_err());

    let mut no_graders = base_request();
    no_graders["graders"] = json!([]);
    assert!(
        request(no_graders)
            .validated()
            .unwrap_err()
            .contains("grader")
    );

    let mut empty_grader = base_request();
    empty_grader["graders"] = json!(["   "]);
    assert!(
        request(empty_grader)
            .validated()
            .unwrap_err()
            .contains("grader 1")
    );

    let mut no_prompt = base_request();
    no_prompt["prompt"] = json!("  \n");
    assert!(request(no_prompt).validated().is_err());

    let mut timeout = base_request();
    timeout["timeout_secs"] = json!(0);
    assert!(request(timeout).validated().is_err());

    let mut protect = base_request();
    protect["protect"] = json!(["tests/", "./tests", "scripts/check.sh"]);
    assert_eq!(
        request(protect).validated().unwrap().protect,
        vec!["tests".to_owned(), "scripts/check.sh".to_owned()]
    );
    for bad in ["../x", "/etc/passwd", ".", ""] {
        let mut protect = base_request();
        protect["protect"] = json!([bad]);
        assert!(request(protect).validated().is_err(), "{bad:?} accepted");
    }
}

#[test]
fn protected_paths_cover_files_and_directories() {
    let protect = vec!["tests".to_owned(), "scripts/check.sh".to_owned()];
    let changed = vec![
        "tests/unit.rs".to_owned(),
        "testsuite.rs".to_owned(),
        "scripts/check.sh".to_owned(),
        "scripts/check.sh.bak".to_owned(),
        "src/lib.rs".to_owned(),
    ];
    assert_eq!(
        tampered(&protect, &changed),
        vec!["scripts/check.sh".to_owned(), "tests/unit.rs".to_owned()]
    );
    assert!(tampered(&[], &changed).is_empty());
}

#[test]
fn task_names_are_valid_worktree_tasks() {
    let name = task_name("Fix the login bug!! (urgent)", "3f2a", "c1");
    assert_eq!(name, "fix-the-login-bug-urgent-3f2a-c1");
    crate::git::validate_task(&name).unwrap();
    assert_eq!(task_name("!!!", "abcd", "c2"), "fanout-abcd-c2");
    let long = task_name(&"word ".repeat(40), "abcd", "c6");
    crate::git::validate_task(&long).unwrap();
}

#[test]
fn the_commit_instruction_is_appended_when_asked() {
    assert_eq!(sent_prompt("do it\n", false), "do it\n");
    let sent = sent_prompt("do it\n", true);
    assert!(sent.starts_with("do it\n\n"));
    assert!(sent.ends_with(COMMIT_INSTRUCTION));
}

fn run(name: &str, status: GraderStatus, code: Option<i32>) -> GraderRun {
    GraderRun {
        name: Some(name.to_owned()),
        run: format!("./{name}.sh"),
        status,
        exit_code: code,
        signal: None,
        duration_ms: 10,
        stdout: format!("{name} out"),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        detail: None,
    }
}

fn grade(passed: bool, lines: u64, graders: Vec<GraderRun>) -> Grade {
    Grade {
        trigger: Trigger::Done,
        started_at: Utc::now(),
        finished_at: Utc::now(),
        duration_ms: 5,
        head: Some("a".repeat(40)),
        dirty: 0,
        untracked: 0,
        tree: "t".into(),
        moved: false,
        passed,
        graders,
        tampered: Vec::new(),
        diff: DiffStat {
            files: 1,
            insertions: lines,
            deletions: 0,
            untracked: 0,
        },
        cost_usd: None,
        tokens: 0,
        error: None,
    }
}

fn view(
    label: &str,
    verdict: Verdict,
    phase: Phase,
    grade: Option<Grade>,
    cost: Option<f64>,
) -> CandidateView {
    CandidateView {
        label: label.to_owned(),
        session_id: format!("s-{label}"),
        session: Some(label.to_owned()),
        kind: SessionKind::Claude,
        model: None,
        branch: format!("b-{label}"),
        path: PathBuf::from(format!("/wt/{label}")),
        phase,
        verdict,
        rank: 0,
        why: Vec::new(),
        agent: None,
        session_state: Some("running".into()),
        grade,
        stale: None,
        pending: None,
        head: None,
        cost_usd: cost,
        tokens: 0,
        gradings: 1,
        fixes: 0,
        winner: false,
        cleaned: None,
        pickable: false,
    }
}

#[test]
fn ranking_puts_passing_first_then_smaller_diffs_then_lower_cost() {
    let pass = |lines| {
        grade(
            true,
            lines,
            vec![run("unit", GraderStatus::Passed, Some(0))],
        )
    };
    let fail = |passed: usize| {
        let mut graders = vec![run("a", GraderStatus::Passed, Some(0)); passed];
        graders.push(run("unit", GraderStatus::Failed, Some(1)));
        grade(false, 3, graders)
    };
    let mut views = vec![
        view("c1", Verdict::None, Phase::Working, None, None),
        view("c2", Verdict::Fail, Phase::Graded, Some(fail(0)), Some(0.1)),
        view(
            "c3",
            Verdict::Pass,
            Phase::Graded,
            Some(pass(40)),
            Some(0.1),
        ),
        view(
            "c4",
            Verdict::Pass,
            Phase::Graded,
            Some(pass(12)),
            Some(0.9),
        ),
        view(
            "c5",
            Verdict::Pass,
            Phase::Graded,
            Some(pass(12)),
            Some(0.2),
        ),
        view("c6", Verdict::Fail, Phase::Graded, Some(fail(1)), Some(0.1)),
    ];
    rank(&mut views);
    let order: Vec<&str> = views.iter().map(|v| v.label.as_str()).collect();
    assert_eq!(order, ["c5", "c4", "c3", "c6", "c2", "c1"]);
    assert_eq!(
        views.iter().map(|v| v.rank).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    let why = |i: usize| views[i].why.join(" | ");
    assert!(why(0).contains("passes all 1 grader"), "{}", why(0));
    assert!(
        why(0).contains("ahead of c4: same diff, cheaper ($0.20 vs $0.90)"),
        "{}",
        why(0)
    );
    assert!(why(0).contains("best of 3 passing"), "{}", why(0));
    assert!(
        why(1).contains("below c5: same diff, costs more"),
        "{}",
        why(1)
    );
    assert!(
        why(2).contains("below c4: larger diff (40 vs 12 lines)"),
        "{}",
        why(2)
    );
    assert!(why(3).contains("fails `unit` (exit 1)"), "{}", why(3));
    assert!(
        why(3).contains("ahead of c2: more graders pass (1 vs 0)"),
        "{}",
        why(3)
    );
    assert!(why(5).contains("still working"), "{}", why(5));
}

#[test]
fn verdicts_and_phases_follow_the_record() {
    let wt = WorktreeRef {
        repo: PathBuf::from("/r"),
        path: PathBuf::from("/r.worktrees/t"),
        branch: "t".into(),
        base: "main".into(),
    };
    let mut c = Candidate {
        label: "c1".into(),
        session_id: "s".into(),
        kind: SessionKind::Claude,
        launch: AgentLaunch::default(),
        worktree: wt,
        pending: None,
        grade: None,
        gradings: 0,
        fixes: 0,
        cleaned: None,
    };
    assert_eq!(
        phase_of(&c, Some(("running", true, Some(AgentStatus::Working)))),
        Phase::Working
    );
    assert_eq!(
        phase_of(&c, Some(("running", true, Some(AgentStatus::Blocked)))),
        Phase::Blocked
    );
    assert_eq!(
        phase_of(&c, Some(("running", true, Some(AgentStatus::Idle)))),
        Phase::Waiting
    );
    // Restored as a plain shell: no agent will report.
    assert_eq!(phase_of(&c, Some(("running", false, None))), Phase::Ended);
    assert_eq!(phase_of(&c, Some(("exited", true, None))), Phase::Ended);
    assert_eq!(phase_of(&c, None), Phase::Ended);
    assert_eq!(verdict_of(&c, None), Verdict::None);
    c.pending = Some(Pending {
        trigger: Trigger::Done,
        queued_at: Utc::now(),
        started_at: None,
    });
    assert_eq!(phase_of(&c, None), Phase::Queued);
    c.pending.as_mut().unwrap().started_at = Some(Utc::now());
    assert_eq!(phase_of(&c, None), Phase::Grading);
    assert_eq!(verdict_of(&c, None), Verdict::Pending);
    c.pending = None;
    c.grade = Some(grade(true, 1, vec![]));
    assert_eq!(verdict_of(&c, None), Verdict::Pass);
    assert_eq!(verdict_of(&c, Some("moved")), Verdict::Stale);
    c.grade = Some(grade(false, 1, vec![]));
    assert_eq!(verdict_of(&c, None), Verdict::Fail);
    c.cleaned = Some(Cleaned {
        at: Utc::now(),
        branch_deleted: false,
    });
    assert_eq!(phase_of(&c, None), Phase::Removed);
}

#[test]
fn the_fix_prompt_carries_the_failing_command_and_its_output() {
    let mut failed = run("unit", GraderStatus::Failed, Some(101));
    failed.stdout = "test answer ... FAILED\nexpected 42, got 41".into();
    failed.stderr = "error: 1 test failed".into();
    let g = grade(
        false,
        3,
        vec![
            run("fmt", GraderStatus::Passed, Some(0)),
            failed,
            run("lint", GraderStatus::Skipped, None),
        ],
    );
    let prompt = fix_prompt(&g).unwrap();
    assert!(prompt.contains("$ ./unit.sh\nexit 101"), "{prompt}");
    assert!(prompt.contains("expected 42, got 41"));
    assert!(prompt.contains("error: 1 test failed"));
    assert!(prompt.contains("Do not change the check itself"));
    assert!(
        !prompt.contains("fmt out"),
        "a passing grader's output is left out"
    );
    assert_eq!(g.failure().unwrap(), "fails `unit` (exit 101)");
    assert!(fix_prompt(&grade(true, 1, vec![])).is_none());

    let mut tampered = grade(false, 1, vec![run("unit", GraderStatus::Passed, Some(0))]);
    tampered.tampered = vec!["tests/unit.rs".into()];
    assert!(fix_prompt(&tampered).unwrap().contains("tests/unit.rs"));
    assert_eq!(
        tampered.failure().unwrap(),
        "changed protected path tests/unit.rs"
    );

    let mut long = run("unit", GraderStatus::Failed, Some(1));
    long.stdout = (0..5000).map(|i| format!("line {i}\n")).collect();
    let prompt = fix_prompt(&grade(false, 1, vec![long])).unwrap();
    assert!(prompt.contains("line 4999"));
    assert!(!prompt.contains("line 10\n"));
    assert!(prompt.len() < 5000);
}

#[test]
fn grading_control_pauses_cancels_and_stops() {
    let control = GradingControl::default();
    let first = control.begin(Duration::ZERO).unwrap();
    control.pause();
    assert!(first.is_cancelled(), "a pause cancels what runs");
    control.end(&first);
    assert!(
        control.begin(Duration::from_millis(20)).is_none(),
        "nothing starts while paused"
    );
    control.resume();
    let second = control.begin(Duration::ZERO).unwrap();
    assert!(!second.is_cancelled());
    control.stop();
    assert!(second.is_cancelled());
    assert!(control.is_stopped());
    assert!(control.begin(Duration::ZERO).is_none());
}

#[test]
fn state_files_from_before_fanouts_load_and_fanouts_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let v3 = json!({
        "version": 3,
        "workspaces": [{"id": "w1", "name": "demo", "folders": [{"path": repo}], "created_at": "2026-11-01T00:00:00Z"}],
        "sessions": [],
        "layouts": {},
    });
    std::fs::write(dir.path().join(crate::state::STATE_FILE), v3.to_string()).unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    assert!(store.fanouts().is_empty());
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join(crate::state::STATE_FILE)).unwrap())
            .unwrap();
    assert_eq!(written["version"], json!(crate::state::STATE_VERSION));
    assert_eq!(crate::state::STATE_VERSION, 4);

    let fanout = Fanout {
        id: "f1".into(),
        name: "demo".into(),
        workspace_id: "w1".into(),
        repo: repo.clone(),
        base: "main".into(),
        base_sha: "b".repeat(40),
        prompt: "p".into(),
        commit_instruction: true,
        graders: vec![GraderSpec {
            run: "true".into(),
            name: None,
        }],
        timeout_secs: 60,
        protect: vec![],
        created_at: Utc::now(),
        candidates: vec![],
        winner: None,
    };
    store.insert_fanout(fanout.clone()).unwrap();
    assert!(
        store.insert_fanout(fanout.clone()).is_err(),
        "ids are unique"
    );
    store
        .update_fanout("f1", |f| f.name = "renamed".into())
        .unwrap();
    drop(store);
    let mut store = HubStore::open(dir.path()).unwrap();
    assert_eq!(store.fanout("f1").unwrap().name, "renamed");
    assert!(store.update_fanout("nope", |_| ()).is_err());

    // A fan-out of another workspace is refused; deleting the workspace
    // drops its fan-outs.
    let mut orphan = fanout.clone();
    orphan.id = "f2".into();
    orphan.workspace_id = "gone".into();
    assert!(store.insert_fanout(orphan).is_err());
    store.delete_workspace("w1").unwrap();
    assert!(store.fanouts().is_empty());

    // A newer hub's file is refused, not rewritten.
    std::fs::write(
        dir.path().join(crate::state::STATE_FILE),
        json!({"version": 5}).to_string(),
    )
    .unwrap();
    assert!(HubStore::open(dir.path()).is_err());
}
