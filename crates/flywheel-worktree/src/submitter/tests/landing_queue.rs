//! The machine-local landing queue (spec 00089): a queued landing is
//! drained by whichever worker holds the merge lock, in `queued_at` order,
//! including entries other workers queued; a dead-lease entry is dropped
//! unlanded; every `Landed` witness carries a fresh size-1 batch.

use super::*;
use flywheel_orchestrator::claims::{ClaimStore, InMemoryClaimStore, conflict_keys};
use flywheel_orchestrator::landing_queue::{LandingQueue, LandingTicket};

const LEASE_SECONDS: f64 = 600.0;

fn capture_log(s: &mut GitWorktreeSubmitter) -> Arc<Mutex<Vec<String>>> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    s.log = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
    lines
}

/// A ledger with one seeded lifecycle per run so witnesses append.
fn ledger_for(runs: &[(&str, &str)]) -> Arc<InMemoryStore> {
    let store = InMemoryStore::new();
    for (run, task) in runs {
        let seed = DomainEvent::new(
            *run,
            Utc::now(),
            DomainEventPayload::LifecycleInitialized(flywheel_core::events::LifecycleInitialized {
                task_id: (*task).into(),
                ..Default::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
    }
    Arc::new(store)
}

fn landed_for(store: &InMemoryStore, run: &str) -> Vec<Landed> {
    store
        .list_domain_events(run)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::Landed(l) => Some(l),
            _ => None,
        })
        .collect()
}

fn parked_for(store: &InMemoryStore, run: &str) -> Vec<LandingParked> {
    store
        .list_domain_events(run)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::LandingParked(p) => Some(p),
            _ => None,
        })
        .collect()
}

fn claims() -> Arc<InMemoryClaimStore> {
    Arc::new(InMemoryClaimStore::new())
}

fn hold(claims: &InMemoryClaimStore, task: &str, worker: &str) {
    claims
        .acquire_claim(
            task,
            worker,
            Utc::now(),
            LEASE_SECONDS,
            &conflict_keys::<_, String>([]),
        )
        .unwrap()
        .expect("claim acquired");
}

fn ticket(owner: &str, claims: &Arc<InMemoryClaimStore>) -> LandingTicket {
    let store: Arc<dyn ClaimStore> = Arc::clone(claims) as Arc<dyn ClaimStore>;
    LandingTicket::new(owner, store, Arc::new(Utc::now))
}

fn request(tf: &Path, task: &str, run: &str, wt: &Path) -> SubmitRequest {
    let mut req = submit_req(tf, task, wt, Status::Done, &["true"]);
    req.run_id = run.to_owned();
    req
}

fn queue_of(s: &GitWorktreeSubmitter) -> LandingQueue {
    LandingQueue::beside_lock(&s.lock_path)
}

fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn first_parent_tasks(repo: &Path, range: &str) -> Vec<String> {
    let shas = git_ok(repo, &["rev-list", "--first-parent", "--reverse", range]);
    shas.lines()
        .map(|sha| trailer_value(repo, sha, "Flywheel-Task"))
        .collect()
}

#[test]
fn landing_queue_lone_entry_lands_as_today_with_one_verify_and_a_size_1_batch() {
    let (dir, repo) = repo();
    let store = ledger_for(&[("run-a", "a"), ("run-b", "b")]);
    let verify_log = dir.path().join("verify.log");
    let snapshot = dir.path().join("queue-during-verify.json");
    let probe = submitter(&repo, Arc::clone(&store));
    let queue_dir = queue_of(&probe).dir().to_path_buf();
    // The standing verify records each invocation and snapshots the queue
    // entry it is landing, so the batch shown while verifying can be
    // compared with the one later recorded on the witness.
    let verify = format!(
        "echo run >> {log} && cat {q}/*.entry.json > {snap}",
        log = verify_log.display(),
        q = queue_dir.display(),
        snap = snapshot.display()
    );
    let s = submitter(&repo, Arc::clone(&store)).with_verify_command(verify);
    let claims = claims();
    hold(&claims, "a", "w1");
    let (tf, wt) = prepared(&s, &repo, "01-phase", "a", "true");
    commit(&wt, "a.txt", "1", "feat: a");
    let before = rev(&repo, "main");

    let outcome = s.submit(&request(&tf, "a", "run-a", &wt).queued_by(ticket("w1", &claims)));

    let after = rev(&repo, "main");
    assert_eq!(
        outcome,
        SubmitOutcome::landed(LANDING_STRATEGY_MERGE, after.clone())
    );
    assert_ne!(after, before);
    let lands = landed_for(&store, "run-a");
    assert_eq!(lands.len(), 1);
    assert_eq!(lands[0].rung, RUNG_FAST_FORWARD);
    assert_eq!(lands[0].landed_ref, after);
    let batch = lands[0]
        .batch
        .clone()
        .expect("every Landed carries a batch");
    assert_eq!((batch.index, batch.size), (1, 1));
    assert!(batch.id.starts_with("batch-"));
    // Exactly one standing verify for the lone landing.
    assert_eq!(fs::read_to_string(&verify_log).unwrap().lines().count(), 1);
    // While verifying, the entry carried exactly the witness's batch.
    let seen: QueueEntry = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(seen.task_id, "a");
    assert_eq!(seen.owner, "w1");
    assert_eq!(seen.batch, Some(batch.clone()));
    // Settled: the queue holds neither the entry nor its settlement.
    let queue = queue_of(&s);
    assert!(queue.entries().unwrap().is_empty());
    assert!(queue.take_settlement("run-a").unwrap().is_none());
    // The claim is untouched (the caller releases it after settlement).
    assert_eq!(claims.load_claim("a").unwrap().unwrap().worker_id, "w1");

    // A second, separate lone landing gets its own fresh batch id.
    hold(&claims, "b", "w1");
    let (tf_b, wt_b) = prepared(&s, &repo, "01-phase", "b", "true");
    commit(&wt_b, "b.txt", "2", "feat: b");
    assert!(
        s.submit(&request(&tf_b, "b", "run-b", &wt_b).queued_by(ticket("w1", &claims)))
            .is_landed()
    );
    let second = landed_for(&store, "run-b")[0].batch.clone().unwrap();
    assert_eq!((second.index, second.size), (1, 1));
    assert_ne!(second.id, batch.id, "batch ids are never shared");
}

#[test]
fn landing_queue_direct_landing_takes_no_queue_entry_and_a_fresh_batch() {
    let (_dir, repo) = repo();
    let store = ledger_for(&[("run-a", "a"), ("run-b", "b")]);
    let s = submitter(&repo, Arc::clone(&store));
    let (tf, wt) = prepared(&s, &repo, "01-phase", "a", "true");
    commit(&wt, "a.txt", "1", "feat: a");
    let (tf_b, wt_b) = prepared(&s, &repo, "01-phase", "b", "true");
    commit(&wt_b, "b.txt", "2", "feat: b");
    // fw reland / the re-driver submit with no ticket: a direct landing.
    assert!(s.submit(&request(&tf, "a", "run-a", &wt)).is_landed());
    assert!(s.submit(&request(&tf_b, "b", "run-b", &wt_b)).is_landed());
    assert!(!queue_of(&s).dir().exists(), "no queue entry was written");
    let a = landed_for(&store, "run-a")[0].clone();
    let b = landed_for(&store, "run-b")[0].clone();
    assert_eq!(a.rung, RUNG_FAST_FORWARD);
    assert_eq!(b.rung, RUNG_REBASE);
    let (a, b) = (a.batch.unwrap(), b.batch.unwrap());
    assert_eq!((a.index, a.size, b.index, b.size), (1, 1, 1, 1));
    assert_ne!(a.id, b.id);
}

#[test]
fn landing_queue_lock_holder_lands_the_other_workers_entry_in_queued_at_order() {
    let (_dir, repo) = repo();
    let store = ledger_for(&[("run-a", "a"), ("run-b", "b")]);
    let claims = claims();
    hold(&claims, "a", "w1");
    hold(&claims, "b", "w2");
    // Two workers' submitters on one repo and one merge lock.
    let mut s1 = submitter(&repo, Arc::clone(&store));
    let mut s2 = submitter(&repo, Arc::clone(&store));
    let log1 = capture_log(&mut s1);
    let log2 = capture_log(&mut s2);
    let (tf_a, wt_a) = prepared(&s1, &repo, "01-phase", "a", "true");
    commit(&wt_a, "a.txt", "1", "feat: a");
    let (tf_b, wt_b) = prepared(&s2, &repo, "01-phase", "b", "true");
    commit(&wt_b, "b.txt", "2", "feat: b");
    let before = rev(&repo, "main");
    let queue = queue_of(&s1);

    // A landing in progress holds the merge lock while both enqueue.
    let held = merge_lock(&s1.lock_path).unwrap();
    let req_a = request(&tf_a, "a", "run-a", &wt_a).queued_by(ticket("w1", &claims));
    let first = std::thread::spawn(move || s1.submit(&req_a));
    wait_for(
        || queue.entries().unwrap().iter().any(|e| e.task_id == "a"),
        "a to enqueue",
    );
    let req_b = request(&tf_b, "b", "run-b", &wt_b).queued_by(ticket("w2", &claims));
    let second = std::thread::spawn(move || s2.submit(&req_b));
    wait_for(
        || queue.entries().unwrap().len() == 2,
        "b to enqueue behind a",
    );
    let fifo: Vec<(String, String)> = queue
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| (e.task_id, e.owner))
        .collect();
    assert_eq!(
        fifo,
        vec![
            ("a".to_owned(), "w1".to_owned()),
            ("b".to_owned(), "w2".to_owned())
        ]
    );
    drop(held);

    let outcome_a = first.join().unwrap();
    let outcome_b = second.join().unwrap();
    let after = rev(&repo, "main");
    // Each owner returns the outcome recorded for its own entry.
    let land_a = landed_for(&store, "run-a");
    let land_b = landed_for(&store, "run-b");
    assert_eq!((land_a.len(), land_b.len()), (1, 1));
    assert_eq!(
        outcome_a,
        SubmitOutcome::landed(LANDING_STRATEGY_MERGE, land_a[0].landed_ref.clone())
    );
    assert_eq!(
        outcome_b,
        SubmitOutcome::landed(LANDING_STRATEGY_MERGE, after.clone())
    );
    // FIFO: a landed first (fast-forward), b after it (rebased onto a).
    assert_eq!(land_a[0].rung, RUNG_FAST_FORWARD);
    assert_eq!(land_b[0].rung, RUNG_REBASE);
    assert_eq!(
        first_parent_tasks(&repo, &format!("{before}..{after}")),
        vec!["a", "b"]
    );
    assert!(is_ancestor(&repo, &land_a[0].landed_ref, &after));
    // Exactly one worker (whichever took the lock first) drained both
    // entries, the other's included, in queued_at order.
    let settled = |log: &Arc<Mutex<Vec<String>>>| -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("landing queue: settled"))
            .cloned()
            .collect()
    };
    let (by1, by2) = (settled(&log1), settled(&log2));
    let drained = if by1.is_empty() { by2 } else { by1.clone() };
    assert!(
        settled(&log1).is_empty() ^ settled(&log2).is_empty(),
        "one lock holder drained the queue: {:?} / {:?}",
        settled(&log1),
        settled(&log2)
    );
    assert_eq!(drained.len(), 2, "{drained:?}");
    assert!(
        drained[0].contains(" a (run run-a, owner w1)"),
        "{drained:?}"
    );
    assert!(
        drained[1].contains(" b (run run-b, owner w2)"),
        "{drained:?}"
    );
    let (ba, bb) = (
        land_a[0].batch.clone().unwrap(),
        land_b[0].batch.clone().unwrap(),
    );
    assert_eq!((ba.size, bb.size), (1, 1));
    assert_ne!(ba.id, bb.id);
    assert!(queue.entries().unwrap().is_empty());
    assert!(queue.take_settlement("run-a").unwrap().is_none());
    assert!(queue.take_settlement("run-b").unwrap().is_none());
}

#[test]
fn landing_queue_dead_lease_entry_is_dropped_and_the_next_lands() {
    let (_dir, repo) = repo();
    let store = ledger_for(&[("run-x", "x"), ("run-b", "b")]);
    let claims = claims();
    let mut s = submitter(&repo, Arc::clone(&store));
    let log = capture_log(&mut s);
    // x's owner was SIGKILLed: its entry stays queued ahead of b, but no
    // worker holds x's claim any more.
    let (tf_x, wt_x) = prepared(&s, &repo, "01-phase", "x", "true");
    let x_commit = commit(&wt_x, "x.txt", "dead", "feat: x");
    let queue = queue_of(&s);
    queue
        .enqueue(
            &request(&tf_x, "x", "run-x", &wt_x),
            "w-dead",
            Utc::now() - chrono::Duration::seconds(5),
        )
        .unwrap();
    hold(&claims, "b", "w2");
    let (tf_b, wt_b) = prepared(&s, &repo, "01-phase", "b", "true");
    commit(&wt_b, "b.txt", "2", "feat: b");

    let outcome = s.submit(&request(&tf_b, "b", "run-b", &wt_b).queued_by(ticket("w2", &claims)));

    assert!(outcome.is_landed(), "{outcome:?}");
    let main = rev(&repo, "main");
    assert!(!is_ancestor(&repo, &x_commit, &main), "x never landed");
    assert!(landed_for(&store, "run-x").is_empty());
    assert!(parked_for(&store, "run-x").is_empty());
    assert!(queue.entries().unwrap().is_empty(), "x was dropped");
    assert!(queue.take_settlement("run-x").unwrap().is_none());
    // x's worktree and branch are left for the existing recovery paths.
    assert!(wt_x.exists());
    assert!(branch_exists(&repo, "flywheel/01-phase/x"));
    assert_eq!(
        landed_for(&store, "run-b")[0].batch.as_ref().unwrap().size,
        1
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("dropping x (run run-x)")),
        "{:?}",
        log.lock().unwrap()
    );
}

#[test]
fn landing_queue_stolen_lease_entry_is_never_landed() {
    let (_dir, repo) = repo();
    let store = ledger_for(&[("run-a", "a")]);
    let claims = claims();
    // w1 queued a, but a peer stole the lease mid-wait.
    hold(&claims, "a", "w3");
    let s = submitter(&repo, Arc::clone(&store));
    let (tf, wt) = prepared(&s, &repo, "01-phase", "a", "true");
    let work = commit(&wt, "a.txt", "1", "feat: a");
    let before = rev(&repo, "main");

    let outcome = s.submit(&request(&tf, "a", "run-a", &wt).queued_by(ticket("w1", &claims)));

    assert!(
        matches!(&outcome, SubmitOutcome::NoOp { reason } if reason.contains("left the landing queue unlanded")),
        "{outcome:?}"
    );
    assert_eq!(rev(&repo, "main"), before);
    assert!(!is_ancestor(&repo, &work, &before));
    assert!(landed_for(&store, "run-a").is_empty());
    assert!(parked_for(&store, "run-a").is_empty());
    assert!(queue_of(&s).entries().unwrap().is_empty());
    assert!(wt.exists());
}

#[test]
fn landing_queue_parks_through_the_existing_ladder_and_settles_for_the_owner() {
    let (_dir, repo) = repo();
    let store = ledger_for(&[("run-a", "a")]);
    let claims = claims();
    hold(&claims, "a", "w1");
    let s = submitter(&repo, Arc::clone(&store)).with_protected_paths(["secrets/**"]);
    let (tf, wt) = prepared(&s, &repo, "01-phase", "a", "true");
    fs::create_dir_all(wt.join("secrets")).unwrap();
    commit(&wt, "secrets/key", "k", "feat: touch protected");
    let before = rev(&repo, "main");

    let outcome = s.submit(&request(&tf, "a", "run-a", &wt).queued_by(ticket("w1", &claims)));

    assert!(
        matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_PROTECTED_PATHS),
        "{outcome:?}"
    );
    assert_eq!(rev(&repo, "main"), before);
    assert_eq!(parked_for(&store, "run-a").len(), 1);
    assert!(landed_for(&store, "run-a").is_empty());
    assert!(queue_of(&s).entries().unwrap().is_empty());
}
