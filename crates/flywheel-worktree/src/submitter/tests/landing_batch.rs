//! The batched landing (spec 00089, criteria #2-#9, #15): the merge-lock
//! holder drains up to 4 queued entries as one batch, stacks them in queue
//! order with each entry's command graders re-run at its stacked position,
//! runs ONE standing verify on the stacked tip, and fast-forwards the base
//! through all of them; a failed tip lands every entry one at a time; an
//! entry that cannot stack settles after the batch.
//!
//! The standing verify is a stand-in script that logs `tree PASS|FAIL
//! markers...` per invocation (markers = the `m-*` files in its cwd) and
//! fails when every marker named on its command line is present.

use super::*;
use flywheel_orchestrator::claims::{ClaimStore, InMemoryClaimStore, conflict_keys};
use flywheel_orchestrator::landing_queue::{LandingQueue, LandingTicket};
use std::collections::BTreeSet;

const LEASE_SECONDS: f64 = 600.0;
const CULPRIT_TOKEN: &str = "TOKEN-culprit-7c1e9a";

struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
    store: Arc<InMemoryStore>,
    claims: Arc<InMemoryClaimStore>,
    log: Arc<Mutex<Vec<String>>>,
    s: GitWorktreeSubmitter,
}

/// One task of a scenario: its id, its command graders, and its worktree.
struct Entry {
    id: String,
    tf: PathBuf,
    wt: PathBuf,
    graders: Vec<String>,
}

impl Entry {
    fn run(&self) -> String {
        format!("run-{}", self.id)
    }

    fn owner(&self) -> String {
        format!("w-{}", self.id)
    }

    fn request(&self) -> SubmitRequest {
        let runs: Vec<&str> = self.graders.iter().map(String::as_str).collect();
        let mut req = submit_req(&self.tf, &self.id, &self.wt, Status::Done, &runs);
        req.run_id = self.run();
        req
    }
}

/// A repo, a ledger seeded for every task, and a submitter whose standing
/// verify is the logging stand-in failing when all of `fail_rule` exist.
fn fixture(tasks: &[&str], fail_rule: &[&str]) -> Fixture {
    let (dir, repo) = repo();
    let store = InMemoryStore::new();
    for task in tasks {
        let seed = DomainEvent::new(
            format!("run-{task}"),
            Utc::now(),
            DomainEventPayload::LifecycleInitialized(flywheel_core::events::LifecycleInitialized {
                task_id: (*task).into(),
                ..Default::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
    }
    let store = Arc::new(store);
    let script = dir.path().join("verify.sh");
    fs::write(
        &script,
        format!(
            r#"log="$1"; shift
t=$(git rev-parse 'HEAD^{{tree}}')
m=$(ls -1 | grep '^m-' | tr '\n' ' ')
fail=0
if [ $# -gt 0 ]; then fail=1; for x in "$@"; do [ -f "$x" ] || fail=0; done; fi
if [ $fail = 1 ]; then echo "$t FAIL $m" >> "$log"; echo "{CULPRIT_TOKEN}"; exit 1; fi
echo "$t PASS $m" >> "$log"
"#
        ),
    )
    .unwrap();
    let mut verify = format!(
        "sh {} {}",
        script.display(),
        dir.path().join("verify.log").display()
    );
    for marker in fail_rule {
        verify.push(' ');
        verify.push_str(marker);
    }
    let mut s = submitter(&repo, Arc::clone(&store)).with_verify_command(verify);
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    s.log = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
    Fixture {
        dir,
        repo,
        store,
        claims: Arc::new(InMemoryClaimStore::new()),
        log: lines,
        s,
    }
}

impl Fixture {
    /// Prepare `id`'s worktree and commit its marker `m-<id>`.
    fn task(&self, id: &str, graders: &[&str]) -> Entry {
        let tf = task_file(&self.repo, "01-phase", id, "true");
        let wt = self.s.prepare_worktree(&sandbox_req(&tf, id)).unwrap();
        commit(&wt, &format!("m-{id}"), id, &format!("feat: {id}"));
        self.claims
            .acquire_claim(
                id,
                &format!("w-{id}"),
                Utc::now(),
                LEASE_SECONDS,
                &conflict_keys::<_, String>([]),
            )
            .unwrap()
            .expect("claim acquired");
        Entry {
            id: id.to_owned(),
            tf,
            wt,
            graders: graders.iter().map(|g| (*g).to_owned()).collect(),
        }
    }

    fn queue(&self) -> LandingQueue {
        LandingQueue::beside_lock(&self.s.lock_path)
    }

    fn ticket(&self, owner: &str) -> LandingTicket {
        let claims: Arc<dyn ClaimStore> = Arc::clone(&self.claims) as Arc<dyn ClaimStore>;
        LandingTicket::new(owner, claims, Arc::new(Utc::now))
    }

    /// Queue every entry but the last in order (as other workers did while
    /// a landing held the lock), then submit the last as its owner: the
    /// lock holder drains them all in `queued_at` order.
    fn land_all(&self, entries: &[Entry]) -> SubmitOutcome {
        let (last, earlier) = entries.split_last().unwrap();
        let start = Utc::now() - chrono::Duration::seconds(60);
        for (i, entry) in earlier.iter().enumerate() {
            let at = start + chrono::Duration::milliseconds(i64::try_from(i).unwrap() * 10);
            self.queue()
                .enqueue(&entry.request(), &entry.owner(), at)
                .unwrap();
        }
        let outcome = self
            .s
            .submit(&last.request().queued_by(self.ticket(&last.owner())));
        assert!(
            self.queue().entries().unwrap().is_empty(),
            "every entry settled"
        );
        outcome
    }

    fn landed(&self, entry: &Entry) -> Vec<Landed> {
        self.store
            .list_domain_events(&entry.run())
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::Landed(l) => Some(l),
                _ => None,
            })
            .collect()
    }

    fn one_landed(&self, entry: &Entry) -> (Landed, LandedBatch) {
        let lands = self.landed(entry);
        assert_eq!(lands.len(), 1, "{} landed once: {lands:?}", entry.id);
        assert!(self.parked(entry).is_empty(), "{} never parked", entry.id);
        let batch = lands[0].batch.clone().expect("every Landed has a batch");
        (lands[0].clone(), batch)
    }

    fn parked(&self, entry: &Entry) -> Vec<LandingParked> {
        self.store
            .list_domain_events(&entry.run())
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::LandingParked(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// Every standing-verify invocation: (tree, passed, markers).
    fn verifies(&self) -> Vec<(String, bool, BTreeSet<String>)> {
        let raw = fs::read_to_string(self.dir.path().join("verify.log")).unwrap_or_default();
        raw.lines()
            .map(|line| {
                let mut parts = line.split_whitespace();
                let tree = parts.next().unwrap().to_owned();
                let passed = parts.next().unwrap() == "PASS";
                (tree, passed, parts.map(str::to_owned).collect())
            })
            .collect()
    }

    /// The trees the base took from landings (its reflog minus the init).
    fn base_trees(&self) -> Vec<String> {
        let shas = git_ok(&self.repo, &["log", "-g", "--format=%H", "main"]);
        let mut shas: Vec<&str> = shas.lines().collect();
        shas.pop();
        shas.iter()
            .map(|sha| rev(&self.repo, &format!("{sha}^{{tree}}")))
            .collect()
    }

    /// Criterion #2: every tree the base took is a passing verify's tree.
    fn assert_base_trees_verified(&self) {
        let passing: BTreeSet<String> = self
            .verifies()
            .into_iter()
            .filter(|(_, passed, _)| *passed)
            .map(|(tree, _, _)| tree)
            .collect();
        let trees = self.base_trees();
        assert!(!trees.is_empty(), "the base advanced");
        for tree in trees {
            assert!(
                passing.contains(&tree),
                "base tree {tree} was never seen by a passing standing verify ({passing:?})"
            );
        }
    }

    fn base_tree_has(&self, tree: &str, marker: &str) -> bool {
        !git_ok(&self.repo, &["ls-tree", "--name-only", tree, marker]).is_empty()
    }

    /// The order entries were settled in by the lock holder.
    fn settled_order(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|l| l.strip_prefix("landing queue: settled "))
            .map(|l| l.split_whitespace().next().unwrap().to_owned())
            .collect()
    }
}

fn markers(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|id| format!("m-{id}")).collect()
}

#[test]
fn landing_batch_six_queued_entries_land_through_two_verifies_in_queued_at_order() {
    let ids = ["a", "b", "c", "d", "e", "f"];
    let fx = fixture(&ids, &[]);
    let entries: Vec<Entry> = ids.iter().map(|id| fx.task(id, &["true"])).collect();
    let before = rev(&fx.repo, "main");

    let outcome = fx.land_all(&entries);

    let after = rev(&fx.repo, "main");
    assert_eq!(
        outcome,
        SubmitOutcome::landed(LANDING_STRATEGY_MERGE, after.clone())
    );
    // Exactly two standing verifies: the 4-stack, then the 2-stack, each
    // seeing every member's marker (not only its first commit's).
    let verifies = fx.verifies();
    assert_eq!(verifies.len(), 2, "{verifies:?}");
    assert!(verifies.iter().all(|(_, passed, _)| *passed));
    assert_eq!(verifies[0].2, markers(&["a", "b", "c", "d"]));
    assert_eq!(verifies[1].2, markers(&ids));
    // The 4 earliest by queued_at share one batch; the other 2 another.
    let batches: Vec<LandedBatch> = entries.iter().map(|e| fx.one_landed(e).1).collect();
    for (i, batch) in batches[..4].iter().enumerate() {
        assert_eq!(batch.id, batches[0].id);
        assert_eq!(
            (batch.index, batch.size),
            (u32::try_from(i).unwrap() + 1, 4)
        );
    }
    for (i, batch) in batches[4..].iter().enumerate() {
        assert_eq!(batch.id, batches[4].id);
        assert_eq!(
            (batch.index, batch.size),
            (u32::try_from(i).unwrap() + 1, 2)
        );
    }
    assert_ne!(batches[0].id, batches[4].id);
    // The first member was already on the base tip; the rest were stacked.
    assert_eq!(fx.one_landed(&entries[0]).0.rung, RUNG_FAST_FORWARD);
    assert!(
        entries[1..]
            .iter()
            .all(|e| fx.one_landed(e).0.rung == RUNG_REBASE)
    );
    // Two base advances, each to a verified tree, in queue order.
    assert_eq!(fx.base_trees().len(), 2);
    fx.assert_base_trees_verified();
    assert_eq!(
        range_shas(&fx.repo, &format!("{before}..{after}"))
            .iter()
            .map(|sha| trailer_value(&fx.repo, sha, "Flywheel-Task"))
            .collect::<Vec<_>>(),
        ids
    );
    // Every member is torn down, its witness ref is on the base.
    for (entry, (landed, _)) in entries.iter().map(|e| (e, fx.one_landed(e))) {
        assert!(is_ancestor(&fx.repo, &landed.landed_ref, &after));
        assert!(!entry.wt.exists());
        assert!(!branch_exists(
            &fx.repo,
            &format!("flywheel/01-phase/{}", entry.id)
        ));
    }
    assert_eq!(fx.settled_order(), ids);
}

#[test]
fn landing_batch_keeps_each_tasks_commits_unsquashed_in_queue_order_with_their_own_trailers() {
    let ids = ["p", "q", "r"];
    let fx = fixture(&ids, &[]);
    let entries: Vec<Entry> = ids
        .iter()
        .map(|id| {
            let entry = fx.task(id, &["true"]);
            commit(
                &entry.wt,
                &format!("{id}-2.txt"),
                "2",
                &format!("feat: {id} two"),
            );
            entry
        })
        .collect();
    let before = rev(&fx.repo, "main");

    fx.land_all(&entries);

    let after = rev(&fx.repo, "main");
    let first_parent = git_ok(
        &fx.repo,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{before}..{after}"),
        ],
    );
    let stamped: Vec<(String, String, String)> = first_parent
        .lines()
        .map(|sha| {
            (
                trailer_value(&fx.repo, sha, "Flywheel-Task"),
                trailer_value(&fx.repo, sha, "Flywheel-Run"),
                git_ok(&fx.repo, &["show", "-s", "--format=%s", sha]),
            )
        })
        .collect();
    let mut expected = Vec::new();
    for id in ids {
        expected.push((id.to_owned(), format!("run-{id}"), format!("feat: {id}")));
        expected.push((
            id.to_owned(),
            format!("run-{id}"),
            format!("feat: {id} two"),
        ));
    }
    assert_eq!(
        stamped, expected,
        "unsquashed, in queue order, own trailers"
    );
    for sha in first_parent.lines() {
        assert_eq!(
            trailer_value(&fx.repo, sha, "Flywheel-Phase"),
            "01-phase",
            "{sha}"
        );
    }
    let batch_ids: BTreeSet<String> = entries.iter().map(|e| fx.one_landed(e).1.id).collect();
    assert_eq!(batch_ids.len(), 1);
    assert_eq!(fx.verifies().len(), 1);
    fx.assert_base_trees_verified();
}

#[test]
fn landing_batch_grader_failure_at_a_stacked_position_defers_the_entry_and_the_rest_land_as_size_2()
{
    let ids = ["a", "c", "d"];
    let fx = fixture(&ids, &[]);
    let c_grader = "test ! -f m-a";
    let a = fx.task("a", &["true"]);
    let c = fx.task("c", &[c_grader]);
    let d = fx.task("d", &["true"]);
    let c_tip = rev(&c.wt, "HEAD");

    fx.land_all(&[a, c, d]);
    let (a, c, d) = (fx.task_ref("a"), fx.task_ref("c"), fx.task_ref("d"));

    let (_, ba) = fx.one_landed(&a);
    let (_, bd) = fx.one_landed(&d);
    assert_eq!(ba.id, bd.id);
    assert_eq!((ba.index, ba.size, bd.index, bd.size), (1, 2, 2, 2));
    // C settled after the batch, parked divergent-base with its receipt.
    assert!(fx.landed(&c).is_empty());
    let parks = fx.parked(&c);
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, PARK_KIND_DIVERGENT_BASE);
    assert_eq!(parks[0].receipts.len(), 1);
    assert_eq!(parks[0].receipts[0].grader_name.as_deref(), Some(c_grader));
    assert!(!parks[0].receipts[0].passed);
    assert_eq!(fx.settled_order(), ["a", "d", "c"]);
    // The batch's verify tree has no C marker; C's commit is not on base.
    let verifies = fx.verifies();
    assert_eq!(verifies.len(), 1, "{verifies:?}");
    assert_eq!(verifies[0].2, markers(&["a", "d"]));
    assert!(!is_ancestor(&fx.repo, &c_tip, "main"));
    fx.assert_base_trees_verified();
}

#[test]
fn landing_batch_failed_tip_lands_the_others_parks_only_the_culprit_with_n_plus_1_verifies() {
    let ids = ["a", "b", "c"];
    let fx = fixture(&ids, &["m-b"]);
    let entries: Vec<Entry> = ids.iter().map(|id| fx.task(id, &["true"])).collect();

    fx.land_all(&entries);

    // n+1 verifies: the failed tip, then one per entry against the real base.
    let verifies = fx.verifies();
    assert_eq!(verifies.len(), 4, "{verifies:?}");
    assert!(!verifies[0].1);
    assert_eq!(verifies[0].2, markers(&ids));
    assert_eq!(
        verifies[1..]
            .iter()
            .map(|(_, p, m)| (*p, m.clone()))
            .collect::<Vec<_>>(),
        vec![
            (true, markers(&["a"])),
            (false, markers(&["a", "b"])),
            (true, markers(&["a", "c"])),
        ]
    );
    let (_, ba) = fx.one_landed(&entries[0]);
    let (_, bc) = fx.one_landed(&entries[2]);
    assert_eq!((ba.size, bc.size), (1, 1));
    assert_ne!(ba.id, bc.id);
    let b = &entries[1];
    assert!(fx.landed(b).is_empty());
    let parks = fx.parked(b);
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
    assert!(
        parks[0]
            .receipts
            .iter()
            .any(|r| !r.passed && r.output_excerpt.contains(CULPRIT_TOKEN)),
        "{:?}",
        parks[0].receipts
    );
    for tree in fx.base_trees() {
        assert!(
            !fx.base_tree_has(&tree, "m-b"),
            "{tree} carries the culprit"
        );
    }
    fx.assert_base_trees_verified();
    assert_eq!(fx.settled_order(), ids);
    // The culprit's worktree is kept for forensics on its own commits.
    assert!(b.wt.join("m-b").exists());
}

#[test]
fn landing_batch_pair_interaction_lands_the_first_and_parks_the_second() {
    let ids = ["a", "b"];
    let fx = fixture(&ids, &["m-a", "m-b"]);
    let entries: Vec<Entry> = ids.iter().map(|id| fx.task(id, &["true"])).collect();

    fx.land_all(&entries);

    let (_, ba) = fx.one_landed(&entries[0]);
    assert_eq!(ba.size, 1);
    assert!(fx.landed(&entries[1]).is_empty());
    let parks = fx.parked(&entries[1]);
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
    // The tip, then A against the real base, then B stacked on landed A.
    assert_eq!(fx.verifies().len(), 3);
    for tree in fx.base_trees() {
        assert!(
            !(fx.base_tree_has(&tree, "m-a") && fx.base_tree_has(&tree, "m-b")),
            "{tree} carries both markers"
        );
    }
    fx.assert_base_trees_verified();
}

#[test]
fn landing_batch_non_rebasable_entry_settles_after_the_batch_through_the_ladder() {
    let ids = ["x", "y", "z"];
    let fx = fixture(&ids, &[]);
    let x = fx.task("x", &["true"]);
    commit(&x.wt, "file.txt", "line1-x\n", "feat: x edits file.txt");
    // Y edits then reverts file.txt: a rebase onto X conflicts, a merge
    // (the one-at-a-time ladder's merge-fallback rung) is clean.
    let y = fx.task("y", &["true"]);
    merge_bearing_branch(&y.wt);
    let z = fx.task("z", &["true"]);

    fx.land_all(&[x, y, z]);
    let (x, y, z) = (fx.task_ref("x"), fx.task_ref("y"), fx.task_ref("z"));

    let (_, bx) = fx.one_landed(&x);
    let (_, bz) = fx.one_landed(&z);
    assert_eq!(bx.id, bz.id);
    assert_eq!((bx.index, bx.size, bz.index, bz.size), (1, 2, 2, 2));
    // Y settled after their Landed witnesses, through the serial ladder's
    // merge-fallback rung, as a size-1 batch of its own.
    assert_eq!(fx.settled_order(), ["x", "z", "y"]);
    let (ly, by) = fx.one_landed(&y);
    assert_eq!(ly.rung, RUNG_MERGE_FALLBACK);
    assert_eq!(by.size, 1);
    assert_ne!(by.id, bx.id);
    let main = rev(&fx.repo, "main");
    assert!(fx.base_tree_has(&main, "m-y"));
    fx.assert_base_trees_verified();
}

#[test]
fn landing_batch_every_base_tree_equals_a_passing_verify_tree_across_batches_and_fallbacks() {
    let ids = ["a", "b", "c", "d", "e", "f", "g"];
    // e poisons any tree it is in: the second batch fails and falls back.
    let fx = fixture(&ids, &["m-e"]);
    let entries: Vec<Entry> = ids.iter().map(|id| fx.task(id, &["true"])).collect();

    fx.land_all(&entries);

    // Batch 1 (a-d) lands; batch 2 (e-g) fails its tip, then e, f, g alone.
    assert_eq!(fx.verifies().len(), 1 + 1 + 3);
    for id in ["a", "b", "c", "d", "f", "g"] {
        let entry = entries.iter().find(|e| e.id == id).unwrap();
        fx.one_landed(entry);
    }
    assert_eq!(
        fx.parked(&entries[4])[0].park_kind,
        PARK_KIND_STANDING_VERIFY
    );
    assert_eq!(fx.base_trees().len(), 3);
    fx.assert_base_trees_verified();
    for tree in fx.base_trees() {
        assert!(!fx.base_tree_has(&tree, "m-e"));
    }
}

#[test]
fn landing_batch_lone_entry_lands_at_todays_rung_with_one_verify_and_size_1() {
    let fx = fixture(&["a"], &[]);
    let a = fx.task("a", &["test -f peer.txt"]);
    commit(&fx.repo, "peer.txt", "advanced", "base moves on");

    let outcome = fx.land_all(std::slice::from_ref(&a));

    assert!(outcome.is_landed(), "{outcome:?}");
    let (landed, batch) = fx.one_landed(&a);
    // The base advanced, so today's ladder lands it at the rebase rung.
    assert_eq!(landed.rung, RUNG_REBASE);
    assert_eq!((batch.index, batch.size), (1, 1));
    assert_eq!(fx.verifies().len(), 1);
    assert!(
        !fx.log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("landing batch")),
        "a lone entry never stacks"
    );
}

impl Fixture {
    /// A handle on an already-landed-or-parked task (for its run/owner).
    fn task_ref(&self, id: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            tf: PathBuf::new(),
            wt: self.s.worktree(id),
            graders: Vec::new(),
        }
    }
}
