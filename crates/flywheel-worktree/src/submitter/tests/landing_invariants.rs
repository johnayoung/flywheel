//! Landing invariants pinned by mutation testing: each test here kills a
//! planted submitter bug that the rest of the suite let through (wrong grader
//! verdict aggregation, dropped grader env, merge-base scoping of the
//! protected-path check, fail-closed diff errors, non-FF refusal, park
//! receipts, agent-session crash/dirty-tree handling, and logged witness
//! failures).

use super::*;

fn capture_log(s: &mut GitWorktreeSubmitter) -> Arc<Mutex<Vec<String>>> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    s.log = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
    lines
}

#[test]
fn post_rebase_reverify_parks_when_any_one_command_grader_fails() {
    let (_dir, repo) = repo();
    let store = ledger();
    let s = submitter(&repo, Arc::clone(&store));
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    commit(&wt, "feature.txt", "x", "work");
    commit(&repo, "peer.txt", "advanced", "advance base with peer.txt");
    let base_advanced = rev(&repo, "main");
    // The first grader still passes on the rebased tree; the second does not.
    let failing = "test ! -f peer.txt";
    let outcome = s.submit(&submit_req(
        &tf,
        "t1",
        &wt,
        Status::Done,
        &["true", failing],
    ));
    assert!(
        matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_DIVERGENT_BASE),
        "{outcome:?}"
    );
    assert_eq!(rev(&repo, "main"), base_advanced);
    assert!(landed(&store).is_empty());
    let parks = parked(&store);
    assert_eq!(parks.len(), 1);
    let receipts: Vec<(Option<&str>, bool)> = parks[0]
        .receipts
        .iter()
        .map(|r| (r.grader_name.as_deref(), r.passed))
        .collect();
    assert_eq!(receipts, vec![(Some("true"), true), (Some(failing), false)]);
}

#[test]
fn post_rebase_checks_run_under_the_configured_grader_env() {
    let (_dir, repo) = repo();
    let store = ledger();
    let probe = r#"test "$FW_LANDING_ENV_PROBE" = nonce-7f3"#;
    let mut env = BTreeMap::new();
    env.insert("PATH".to_owned(), std::env::var("PATH").unwrap());
    env.insert("FW_LANDING_ENV_PROBE".to_owned(), "nonce-7f3".to_owned());
    let s = submitter(&repo, Arc::clone(&store))
        .with_grader_env(env)
        .with_verify_command(probe);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    commit(&wt, "feature.txt", "x", "work");
    commit(&repo, "other.txt", "y", "base moves on");
    let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &[probe]));
    assert!(outcome.is_landed(), "{outcome:?}: {:?}", parked(&store));
    let lands = landed(&store);
    assert_eq!(lands.len(), 1);
    assert_eq!(lands[0].rung, RUNG_REBASE);
    assert!(repo.join("feature.txt").exists());
}

#[test]
fn protected_paths_changed_only_by_the_advanced_base_do_not_park_the_branch() {
    let (_dir, repo) = repo();
    let store = ledger();
    let s = submitter(&repo, Arc::clone(&store)).with_protected_paths(["conftest.py"]);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    commit(&wt, "feature.txt", "x", "work");
    // The base (not the task) edits the protected file after the branch forked.
    commit(
        &repo,
        "conftest.py",
        "operator fixture\n",
        "base edits conftest",
    );
    assert_eq!(
        s.protected_violations("flywheel/01-phase/t1", "main")
            .unwrap(),
        Vec::<String>::new()
    );
    let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
    assert!(outcome.is_landed(), "{outcome:?}");
    assert!(parked(&store).is_empty());
    assert_eq!(landed(&store)[0].rung, RUNG_REBASE);
    assert_eq!(
        fs::read_to_string(repo.join("conftest.py")).unwrap(),
        "operator fixture\n"
    );
}

#[test]
fn an_undiffable_branch_fails_the_protected_check_closed() {
    let (_dir, repo) = repo();
    let s = submitter(&repo, ledger()).with_protected_paths(["conftest.py"]);
    let violations = s
        .protected_violations("flywheel/01-phase/no-such-task", "main")
        .unwrap();
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].starts_with("<diff failed: "),
        "{violations:?}"
    );
    // No protected patterns configured: nothing to check, nothing to fail.
    let open = submitter(&repo, ledger());
    assert!(
        open.protected_violations("flywheel/01-phase/no-such-task", "main")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn out_of_tree_fast_forward_refuses_a_diverged_branch() {
    let (_dir, repo) = repo();
    let s = submitter(&repo, ledger());
    git_ok(&repo, &["branch", "side"]);
    git_ok(&repo, &["checkout", "-q", "side"]);
    commit(&repo, "side.txt", "s", "side work");
    git_ok(&repo, &["checkout", "-q", "main"]);
    commit(&repo, "main.txt", "m", "main moves on");
    git_ok(&repo, &["checkout", "-q", "-b", "operator"]);
    let main_before = rev(&repo, "main");
    let operator_before = rev(&repo, "HEAD");
    assert!(!s.base_is_checked_out("main").unwrap());
    assert!(!s.ff_merge("side", "main").unwrap());
    assert_eq!(rev(&repo, "main"), main_before);
    assert_eq!(rev(&repo, "HEAD"), operator_before);
    // A true fast-forward on the same out-of-tree path still advances.
    git_ok(&repo, &["branch", "ahead", "main"]);
    git_ok(&repo, &["checkout", "-q", "ahead"]);
    let ahead = commit(&repo, "ahead.txt", "a", "ahead work");
    git_ok(&repo, &["checkout", "-q", "operator"]);
    assert!(s.ff_merge("ahead", "main").unwrap());
    assert_eq!(rev(&repo, "main"), ahead);
    assert_eq!(rev(&repo, "HEAD"), operator_before);
}

#[test]
fn merge_fallback_standing_verify_park_carries_the_check_receipt() {
    let (_dir, repo) = repo();
    let store = ledger();
    let command = "echo STANDING_MERGED_NONCE_B58; test ! -f poison.txt";
    let s = submitter(&repo, Arc::clone(&store)).with_verify_command(command);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    merge_bearing_branch(&wt);
    fs::write(repo.join("file.txt"), "line1-main\n").unwrap();
    commit(&repo, "poison.txt", "boom\n", "base advances and poisons");
    s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
    let parks = parked(&store);
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
    assert_eq!(parks[0].receipts.len(), 1);
    assert_eq!(parks[0].receipts[0].grader_name.as_deref(), Some(command));
    assert!(!parks[0].receipts[0].passed);
    assert!(
        parks[0].receipts[0]
            .output_excerpt
            .contains("STANDING_MERGED_NONCE_B58")
    );
}

#[test]
fn session_that_commits_then_crashes_is_reset_to_the_pre_merge_tip() {
    let (_dir, repo) = repo();
    let store = ledger();
    let commit_then_crash: ConflictResolver = Arc::new(|request| {
        fs::write(request.worktree.join("file.txt"), "line1-resolved\n").unwrap();
        git_ok(&request.worktree, &["add", "-A"]);
        git_ok(&request.worktree, &["commit", "--no-edit"]);
        Err(ConflictResolutionError("crashed after committing".into()))
    });
    let s = submitter(&repo, Arc::clone(&store))
        .with_recovery_agent(30, 900.0, None)
        .with_resolve_conflict(commit_then_crash);
    let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
    let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
    assert!(
        matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
        if park_kind == PARK_KIND_MERGE_CONFLICT && detail.contains("crashed after committing"))
    );
    assert!(landed(&store).is_empty());
    assert_eq!(rev(&repo, "main"), base_before);
    assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
    assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
}

#[test]
fn crashed_session_wall_time_is_capped_at_the_configured_bound() {
    let (_dir, repo) = repo();
    let store = ledger();
    let slow_crash: ConflictResolver = Arc::new(|_| {
        std::thread::sleep(Duration::from_millis(300));
        Err(ConflictResolutionError("slow crash".into()))
    });
    let s = submitter(&repo, Arc::clone(&store))
        .with_recovery_agent(30, 0.05, None)
        .with_resolve_conflict(slow_crash);
    let (tf, wt, _head, _base) = drive_to_conflict(&s, &repo, "true");
    s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
    let parks = parked(&store);
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].agent_turns, Some(0));
    assert_eq!(parks[0].agent_wall_seconds, Some(0.05));
}

#[test]
fn resolution_leaving_stray_files_is_not_trusted_as_resolved() {
    let (_dir, repo) = repo();
    let store = ledger();
    let messy: ConflictResolver = Arc::new(|request| {
        fs::write(request.worktree.join("file.txt"), "line1-resolved\n").unwrap();
        git_ok(&request.worktree, &["add", "file.txt"]);
        fs::write(request.worktree.join("scratch.txt"), "agent notes\n").unwrap();
        Ok(ConflictResolutionReport {
            turns: 2,
            wall_seconds: 1.0,
        })
    });
    let s = submitter(&repo, Arc::clone(&store))
        .with_recovery_agent(30, 900.0, None)
        .with_resolve_conflict(messy);
    let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
    let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
    assert!(
        matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_MERGE_CONFLICT),
        "{outcome:?}"
    );
    assert!(landed(&store).is_empty());
    assert_eq!(rev(&repo, "main"), base_before);
    assert!(!repo.join("scratch.txt").exists());
    assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
    assert_eq!(parked(&store)[0].agent_turns, Some(2));
}

#[test]
fn a_park_witness_without_a_lifecycle_is_logged_not_dropped_silently() {
    let (_dir, repo) = repo();
    // No lifecycle for run-1: the park outcome stands and the gap is logged.
    let store = Arc::new(InMemoryStore::new());
    let mut s = submitter(&repo, Arc::clone(&store));
    let lines = capture_log(&mut s);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    commit(&wt, "feature.txt", "x", "work");
    fs::write(wt.join("stray"), "x").unwrap();
    assert!(
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
            .is_parked()
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == "cannot record landing-parked event: no lifecycle for run-1"),
        "{:?}",
        lines.lock().unwrap()
    );
}

#[test]
fn a_declined_landed_witness_is_logged_not_dropped_silently() {
    // A declined ledger write: the land stands and the refusal is logged.
    let (_dir, repo) = repo();
    let store = ledger();
    let mut s = submitter(&repo, Arc::clone(&store))
        .with_ledger_guard(Arc::new(DecliningGuard(AtomicUsize::new(0))));
    let lines = capture_log(&mut s);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
    commit(&wt, "feature.txt", "x", "work");
    assert!(
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
            .is_landed()
    );
    assert!(
        lines.lock().unwrap().iter().any(|l| l
            == "failed to record landed event for run-1 (ledger write declined: disk below threshold)"),
        "{:?}",
        lines.lock().unwrap()
    );
}
