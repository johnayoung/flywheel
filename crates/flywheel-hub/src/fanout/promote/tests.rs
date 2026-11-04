use super::*;
use crate::agent::{AgentLaunch, SessionKind};
use crate::fanout::{Candidate, Grade, Trigger, Winner};
use crate::git::WorktreeRef;

fn spec(run: &str, name: Option<&str>, held_out: bool) -> GraderSpec {
    GraderSpec {
        run: run.into(),
        name: name.map(str::to_owned),
        held_out,
    }
}

fn candidate(label: &str, passed: bool, head: &str) -> Candidate {
    let now = Utc::now();
    Candidate {
        label: label.into(),
        session_id: format!("s-{label}"),
        kind: SessionKind::Claude,
        launch: AgentLaunch {
            model: Some("opus".into()),
            ..AgentLaunch::default()
        },
        worktree: WorktreeRef {
            repo: "/r".into(),
            path: format!("/r.worktrees/{label}").into(),
            branch: format!("fanout-{label}"),
            base: "main".into(),
        },
        pending: None,
        grade: Some(Grade {
            trigger: Trigger::Done,
            started_at: now,
            finished_at: now,
            duration_ms: 1,
            head: Some(head.into()),
            dirty: 0,
            untracked: 0,
            tree: "t".into(),
            moved: false,
            passed,
            graders: Vec::new(),
            tampered: Vec::new(),
            diff: crate::fanout::DiffStat {
                files: 1,
                insertions: 1,
                deletions: 0,
                untracked: 0,
            },
            cost_usd: None,
            tokens: 0,
            error: None,
            operator: false,
        }),
        gradings: 1,
        fixes: 0,
        cleaned: None,
    }
}

fn fanout() -> Fanout {
    Fanout {
        id: "abcd1234ef".into(),
        name: "Answer 42".into(),
        workspace_id: "w".into(),
        repo: "/r".into(),
        base: "main".into(),
        base_sha: "0123456789abcdef0123".into(),
        prompt: "# Make answer.txt say 42\n\nWrite 42 into answer.txt.\n".into(),
        commit_instruction: true,
        graders: vec![
            spec("test \"$(cat answer.txt)\" = 42", Some("answer"), false),
            spec("/abs/oracle.sh", Some("oracle"), true),
        ],
        timeout_secs: 60,
        protect: vec!["check.sh".into()],
        pins: Vec::new(),
        created_at: Utc::now(),
        candidates: vec![
            candidate("c1", true, "1111111111111111"),
            candidate("c2", false, "2222222222222222"),
        ],
        winner: None,
        origin: None,
        promoted: Vec::new(),
        seen_graded_at: None,
    }
}

#[test]
fn a_draft_maps_prompt_graders_protect_and_the_winner() {
    let mut f = fanout();
    let none = draft(&f, &PromoteRequest::default(), None).unwrap();
    assert_eq!(none.draft.goal, "Make answer.txt say 42");
    assert_eq!(none.draft.task_id, "answer-42-abcd");
    assert!(none.draft.notes.ends_with("Write 42 into answer.txt."));
    assert!(none.draft.notes.contains("# Make answer.txt say 42"));
    assert_eq!(
        none.draft.graders,
        vec![spec(
            "test \"$(cat answer.txt)\" = 42",
            Some("answer"),
            false
        )],
        "held-out graders stay out of the task file"
    );
    assert!(none.warnings[0].contains("1 held-out grader stays out"));
    assert!(
        none.warnings[1].contains("no winner picked") && none.warnings[1].contains("c1"),
        "a passing candidate is offered as a reference: {:?}",
        none.warnings
    );
    assert!(none.draft.constraints[0].contains("`check.sh`"));
    assert_eq!(none.reference, None, "no winner, none named: no reference");
    assert!(none.draft.references.is_empty());
    assert_eq!(none.draft.tags, vec![PROMOTED_TAG.to_owned()]);

    // No winner: the board's leader is the default reference.
    let led = draft(&f, &PromoteRequest::default(), Some("c1")).unwrap();
    assert_eq!(led.reference.as_deref(), Some("1111111111111111"));
    assert!(
        led.warnings
            .iter()
            .any(|w| w.contains("the reference is the leader, c1")),
        "{:?}",
        led.warnings
    );
    assert!(
        !led.warnings
            .iter()
            .any(|w| w.contains("no reference commit"))
    );

    f.winner = Some(Winner {
        label: "c1".into(),
        head: "1111111111111111".into(),
        base: "main".into(),
        at: Utc::now(),
    });
    let won = draft(&f, &PromoteRequest::default(), None).unwrap();
    assert_eq!(won.reference.as_deref(), Some("1111111111111111"));
    let line = &won.draft.references[0];
    assert!(
        line.contains("Commit 1111111111111111 (branch fanout-c1"),
        "{line}"
    );
    assert!(line.contains("not the solution to copy"), "{line}");

    let skip = PromoteRequest {
        reference: Some("none".into()),
        goal: Some("  Answer is 42.  ".into()),
        task_id: Some("answer".into()),
        phase: Some("07-x".into()),
        ..PromoteRequest::default()
    };
    let plain = draft(&f, &skip, None).unwrap();
    assert_eq!(plain.reference, None);
    assert_eq!(plain.draft.goal, "Answer is 42.");
    assert_eq!(plain.draft.task_id, "answer");
    assert_eq!(plain.draft.phase.as_deref(), Some("07-x"));

    let failing = PromoteRequest {
        reference: Some("c2".into()),
        ..PromoteRequest::default()
    };
    assert!(
        draft(&f, &failing, None)
            .unwrap_err()
            .contains("no passing grading")
    );
    let missing = PromoteRequest {
        reference: Some("c9".into()),
        ..PromoteRequest::default()
    };
    assert!(
        draft(&f, &missing, None)
            .unwrap_err()
            .contains("no candidate")
    );

    f.graders.retain(|g| g.held_out);
    assert!(
        draft(&f, &PromoteRequest::default(), None)
            .unwrap_err()
            .contains("held out")
    );
}

#[test]
fn default_goals_are_one_cut_line() {
    assert_eq!(
        default_goal("\n\n  ## Fix   the   parser \nmore"),
        "Fix the parser"
    );
    let long = "word ".repeat(400);
    let goal = default_goal(&long);
    assert!(goal.chars().count() <= MAX_GOAL_CHARS);
    assert!(goal.ends_with("word..."));
    assert_eq!(slug("  Hello, World! 42 ", 40), "hello-world-42");
}

#[test]
fn asking_to_fix_a_held_out_failure_shows_neither_its_command_nor_its_output() {
    let mut grade = candidate("c1", false, "1").grade.unwrap();
    let mut run = crate::fanout::GraderRun::skipped(&spec("/abs/oracle.sh secret", None, true));
    run.status = crate::fanout::GraderStatus::Failed;
    run.exit_code = Some(3);
    run.stdout = "expected 42 at line 9".into();
    grade.graders = vec![run];
    let prompt = crate::fanout::fix_prompt(&grade).unwrap();
    assert!(prompt.contains("held-out check"), "{prompt}");
    assert!(prompt.contains("exit 3"), "{prompt}");
    assert!(
        !prompt.contains("oracle") && !prompt.contains("line 9"),
        "{prompt}"
    );
    grade.graders[0].held_out = false;
    assert!(
        crate::fanout::fix_prompt(&grade)
            .unwrap()
            .contains("/abs/oracle.sh")
    );
}

fn seed() -> TaskSeed {
    TaskSeed {
        folder: "/r".into(),
        task_id: "answer".into(),
        phase: Some("01-a".into()),
        goal: "Answer 42".into(),
        prompt: "# Goal\n\nAnswer 42\n".into(),
        digest: "d".into(),
        graders: vec![spec("cargo test", Some("tests"), false)],
        held_out: vec![spec(
            "python3 /abs/.flywheel/verification/held-out/oracle.py",
            Some("held-out-oracle:oracle.py"),
            false,
        )],
        not_run: vec!["rubric".into()],
        context: SeedContext {
            relevant: vec!["answer.txt".into()],
            constraints: vec![constraint_line("README.md"), "No new files.".into()],
            notes: "Promoted from the hub's verified fan-out \"first\" (x).".into(),
            ..SeedContext::default()
        },
        tags: vec!["core".into(), PROMOTED_TAG.into()],
        protect: vec!["flywheel.toml".into(), ".github".into(), "README.md".into()],
        repo_protect: vec!["flywheel.toml".into(), ".github".into()],
        protect_skipped: vec!["*.lock".into()],
    }
}

fn request(prompt: &str, graders: Vec<GraderInput>) -> CreateFanout {
    serde_json::from_value::<CreateFanout>(serde_json::json!({
        "workspace_id": "w",
        "prompt": prompt,
        "candidates": [{"kind": "claude"}, {"kind": "claude"}],
        "graders": [],
        "from_task": "answer",
    }))
    .map(|mut r| {
        r.graders = graders;
        r
    })
    .unwrap()
}

#[test]
fn a_seed_brings_the_task_graders_and_hides_its_held_out_ones() {
    let s = seed();
    let r = seeded(request("", Vec::new()), &s).unwrap();
    assert_eq!(r.prompt, s.prompt);
    assert_eq!(r.name.as_deref(), Some("answer"));
    let valid = r.validated().unwrap();
    assert_eq!(
        valid.graders,
        vec![
            spec("cargo test", Some("tests"), false),
            spec(
                "python3 /abs/.flywheel/verification/held-out/oracle.py",
                Some("held-out-oracle:oracle.py"),
                true
            ),
        ]
    );
    assert!(!valid.prompt.contains("oracle"));
    let origin = r.origin.unwrap();
    assert_eq!((origin.task_id.as_str(), origin.held_out), ("answer", 1));

    // An edited grader list keeps the held-out ones (and replaces a
    // copy of one), never drops them.
    let edited = seeded(
        request(
            "my words",
            vec![
                GraderInput::Run("make check".into()),
                GraderInput::Run("python3 /abs/.flywheel/verification/held-out/oracle.py".into()),
            ],
        ),
        &s,
    )
    .unwrap();
    let graders = edited.validated().unwrap().graders;
    assert_eq!(graders.len(), 2);
    assert_eq!(graders[0].run, "make check");
    assert!(graders[1].held_out);

    let leak = seeded(
        request(
            "run /abs/.flywheel/verification/held-out/oracle.py first",
            Vec::new(),
        ),
        &s,
    )
    .unwrap_err();
    assert!(leak.contains("never shown to the agents"), "{leak}");

    // Not seeded by the server: refused.
    assert!(
        request("x", vec![GraderInput::Run("true".into())])
            .validated()
            .is_err()
    );
}

#[test]
fn a_client_cannot_mark_a_grader_held_out() {
    let r = request("x", Vec::new());
    let mut plain = r.clone();
    plain.from_task = None;
    plain.graders = vec![
        GraderInput::Run("cargo test".into()),
        GraderInput::Spec(spec("sh check.sh", None, true)),
    ];
    let graders = plain.validated().unwrap().graders;
    assert!(
        graders.iter().all(|g| !g.held_out),
        "only the registry makes a grader held out: {graders:?}"
    );
    // Through a seed too: a request's own held-out mark is dropped.
    let s = seed();
    let marked = seeded(
        request(
            "x",
            vec![GraderInput::Spec(spec("sh check.sh", None, true))],
        ),
        &s,
    )
    .unwrap();
    let graders = marked.validated().unwrap().graders;
    assert_eq!(
        graders
            .iter()
            .map(|g| (g.run.as_str(), g.held_out))
            .collect::<Vec<_>>(),
        [
            ("sh check.sh", false),
            (
                "python3 /abs/.flywheel/verification/held-out/oracle.py",
                true
            )
        ]
    );
}

#[test]
fn a_seed_brings_the_task_protected_paths_and_says_when_nothing_runs() {
    let s = seed();
    let r = seeded(request("", Vec::new()), &s).unwrap();
    assert_eq!(r.protect, ["flywheel.toml", ".github", "README.md"]);
    let valid = r.validated().unwrap();
    assert_eq!(valid.protect, ["flywheel.toml", ".github", "README.md"]);
    let origin = r.origin.unwrap();
    assert!(origin.prompt_seeded);
    assert_eq!(origin.repo_protect, ["flywheel.toml", ".github"]);
    assert!(
        !seeded(request("my own words", Vec::new()), &s)
            .unwrap()
            .origin
            .unwrap()
            .prompt_seeded
    );

    // The oracle's file name alone is a leak too.
    let named = seeded(request("make oracle.py happy", Vec::new()), &s).unwrap_err();
    assert!(named.contains("oracle.py"), "{named}");

    let mut rubric = seed();
    rubric.graders.clear();
    rubric.held_out.clear();
    rubric.not_run = vec!["rubric friendly".into()];
    let err = seeded(request("", Vec::new()), &rubric).unwrap_err();
    assert!(
        err.contains("rubric friendly") && err.contains("not ones the hub runs"),
        "{err}"
    );
}

#[test]
fn promoting_a_seeded_fan_out_keeps_the_task_structure() {
    let s = seed();
    let r = seeded(request("", Vec::new()), &s).unwrap();
    let mut f = fanout();
    f.prompt = r.prompt.clone();
    f.protect = r.validated().unwrap().protect;
    f.graders = vec![
        spec("cargo test", Some("tests"), false),
        spec(&s.held_out[0].run, None, true),
    ];
    f.origin = r.origin.clone();

    // The task it came from is the durable one: no duplicate by default.
    let refused = draft(&f, &PromoteRequest::default(), None).unwrap_err();
    assert!(
        refused.contains("came from flywheel task answer") && refused.contains("fw worker"),
        "{refused}"
    );

    let named = PromoteRequest {
        task_id: Some("answer-two".into()),
        ..PromoteRequest::default()
    };
    let d = draft(&f, &named, None).unwrap().draft;
    assert_eq!(d.goal, "Answer 42");
    assert_eq!(d.relevant, ["answer.txt"]);
    assert_eq!(
        d.constraints,
        [constraint_line("README.md"), "No new files.".to_owned()],
        "the task's constraints as they were, the repo's protected paths not repeated"
    );
    assert_eq!(
        d.notes.matches("Promoted from").count(),
        1,
        "never nested: {}",
        d.notes
    );
    assert!(!d.notes.contains("# Goal"), "{}", d.notes);
    assert_eq!(d.tags, ["core", PROMOTED_TAG]);

    // An edited prompt is what the agents got: it becomes the notes.
    f.origin.as_mut().unwrap().prompt_seeded = false;
    f.prompt = "Do it my way.".into();
    let edited = draft(&f, &named, None).unwrap().draft;
    assert!(edited.notes.ends_with("Do it my way."), "{}", edited.notes);
    assert_eq!(edited.relevant, ["answer.txt"]);
}

#[test]
fn a_promotion_view_follows_the_file_into_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let active = dir.path().join("tasks/active/03-x");
    std::fs::create_dir_all(&active).unwrap();
    let path = active.join("x.json");
    std::fs::write(&path, "{}").unwrap();
    let promotion = Promotion {
        task_id: "x".into(),
        phase: "03-x".into(),
        path: path.clone(),
        reference: None,
        at: Utc::now(),
    };
    assert_eq!(promotion.view().state, PromotedState::Active);
    let archived = dir.path().join("tasks/archive/03-x-2");
    std::fs::create_dir_all(dir.path().join("tasks/archive")).unwrap();
    std::fs::rename(&active, &archived).unwrap();
    let view = promotion.view();
    assert_eq!(view.state, PromotedState::Archived);
    assert_eq!(view.current, Some(archived.join("x.json")));
    std::fs::remove_dir_all(&archived).unwrap();
    assert_eq!(promotion.view().state, PromotedState::Gone);
}

#[test]
fn a_broken_held_out_pin_never_names_the_oracle_to_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let oracle = dir.path().join("secret-oracle.sh");
    std::fs::write(&oracle, "grep -qx 42 answer.txt\n").unwrap();
    let held = spec(&format!("sh '{}'", oracle.display()), None, true);
    let (pins, _) = crate::fanout::pin_graders(
        &[spec("true", None, false), held.clone()],
        dir.path(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(pins.len(), 1);
    assert!(pins[0].held_out);
    std::fs::write(&oracle, "true\n").unwrap();
    let why = crate::fanout::pins_broken(&pins).unwrap();
    assert!(why.contains("1 held-out grader script"), "{why}");
    assert!(!why.contains("secret-oracle"), "{why}");

    // Whatever an error says, ask-to-fix strips a held-out grader from it.
    let mut grade = candidate("c1", false, "1").grade.unwrap();
    grade.error = Some(format!("grader script {} changed", oracle.display()));
    grade.graders = vec![crate::fanout::GraderRun::skipped(&held)];
    let prompt = crate::fanout::fix_prompt(&grade).unwrap();
    assert!(!prompt.contains("secret-oracle"), "{prompt}");
    assert!(prompt.contains("[held-out]"), "{prompt}");
    // A broken pin is the operator's: the agent is asked nothing (and
    // so never hears that a held-out grader exists).
    grade.error = Some(why);
    grade.operator = true;
    assert_eq!(crate::fanout::fix_prompt(&grade), None);
}
