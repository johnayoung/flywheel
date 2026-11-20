use super::*;
use crate::testutil::TempRepo;
use chrono::{Duration, TimeZone};
use flywheel_core::events::{DomainEvent, Landed, LandingParked};
use flywheel_core::lifecycle::Attempt;
use flywheel_core::store::{AttemptStore, DomainEventStore, LifecycleStore};
use flywheel_core::store_memory::InMemoryStore;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0).unwrap()
}

struct Fixture {
    repo: TempRepo,
    home: tempfile::TempDir,
    store: InMemoryStore,
}

impl Fixture {
    fn new() -> Self {
        let f = Self {
            repo: TempRepo::new(),
            home: tempfile::tempdir().unwrap(),
            store: InMemoryStore::new(),
        };
        f.repo.commit("root", &[("README", "r")]);
        f
    }

    fn paths(&self) -> ProjectPaths {
        ProjectPaths::at(self.home.path())
    }

    fn prefix(&self) -> String {
        format!("{}/", self.home.path().display())
    }

    /// The task file, its held-out registration, and its oracle on disk.
    fn define(&self, id: &str, with_oracle: bool) {
        let home = self.prefix();
        let task = serde_json::json!({
            "id": id,
            "goal": format!("do {id}"),
            "prerequisites": ["other"],
            "context": {"relevant": [format!("{home}specs/0001-spec.md -- the spec")],
                        "non_goals": [format!("do not read {home}held-out/, {home}verification/ or {home}verification/0001/oracle-{id}.sh")]},
            "graders": [{"type": "command", "name": "guard", "run": format!("bash {home}verification/guard.sh")}],
        });
        let file = self
            .home
            .path()
            .join("tasks/archive/01-phase")
            .join(format!("{id}.json"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, serde_json::to_string(&task).unwrap()).unwrap();
        std::fs::create_dir_all(self.home.path().join("specs")).unwrap();
        std::fs::write(self.home.path().join("specs/0001-spec.md"), "# spec\n").unwrap();
        std::fs::create_dir_all(self.home.path().join("verification")).unwrap();
        std::fs::write(self.home.path().join("verification/guard.sh"), "true\n").unwrap();
        if with_oracle {
            let dir = self.home.path().join("verification/0001");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("oracle-{id}.sh")),
                format!(
                    "source {home}verification/0001/helper.sh\nsh {home}verification/caps.sh\n"
                ),
            )
            .unwrap();
            std::fs::write(dir.join("helper.sh"), "true\n").unwrap();
            // Its package: a symlinked node_modules beside it.
            let modules = self.home.path().join("verification/shared_modules/lib");
            std::fs::create_dir_all(&modules).unwrap();
            std::fs::write(modules.join("index.js"), "module.exports = 1;\n").unwrap();
            let link = dir.join("node_modules");
            if !link.exists() {
                std::os::unix::fs::symlink(
                    self.home.path().join("verification/shared_modules"),
                    &link,
                )
                .unwrap();
            }
            std::fs::write(self.home.path().join("verification/caps.sh"), "true\n").unwrap();
            let reg = serde_json::json!({"graders": [{"type": "command",
                "run": format!("sh {home}verification/0001/oracle-{id}.sh"), "name": "oracle"}]});
            let held = self.home.path().join("held-out");
            std::fs::create_dir_all(&held).unwrap();
            std::fs::write(held.join(format!("{id}.json")), reg.to_string()).unwrap();
        }
    }

    /// A run with `attempts` (each `failed` or not), then `events` at
    /// offsets after the given attempt started.
    fn run(
        &self,
        id: &str,
        run_id: &str,
        status: Status,
        attempts: &[bool],
        events: &[(u32, DomainEventPayload)],
    ) {
        let mut lc = Lifecycle::new(id).with_run_id(run_id);
        lc.transition_to(Status::Ready, "", Some(t0())).unwrap();
        lc.transition_to(Status::Running, "", Some(t0())).unwrap();
        if status == Status::Done {
            lc.transition_to(Status::Validating, "", Some(t0()))
                .unwrap();
        }
        let error = if status == Status::Done { "" } else { "boom" };
        lc.transition_to(status, error, Some(t0() + Duration::hours(5)))
            .unwrap();
        self.store.create_lifecycle(&lc).unwrap();
        let start = |n: u32| t0() + Duration::hours(i64::from(n));
        for (i, failed) in attempts.iter().enumerate() {
            let n = i as u32 + 1;
            let mut a = Attempt::new(n, start(n), run_id);
            a.outcome = Some(if *failed {
                Outcome::ValidationFailed
            } else {
                Outcome::Succeeded
            });
            a.total_cost_usd = 1.5;
            a.agent_context
                .insert("base_commit_sha".into(), "a".repeat(40));
            self.store.save_attempt(run_id, &a, None).unwrap();
        }
        let mut version = self.store.load_lifecycle(run_id).unwrap().unwrap().version;
        for (attempt, payload) in events {
            let at = start(*attempt) + Duration::minutes(30);
            let ev = DomainEvent::new(run_id, at, payload.clone());
            version = self
                .store
                .append_domain_event(&ev, version)
                .unwrap()
                .version;
        }
    }

    fn landed_commit(&self, id: &str, run_id: &str) -> (String, String) {
        let base = self.repo.git(&["rev-parse", "HEAD"]);
        self.repo
            .commit(&format!("wip {id}\n\nFlywheel-Run: {run_id}"), &[("x", id)]);
        let tip = self.repo.commit(
            &format!("feat {id}\n\nFlywheel-Run: {run_id}"),
            &[("y", id)],
        );
        (base, tip)
    }
}

fn landed(sha: &str) -> DomainEventPayload {
    DomainEventPayload::Landed(Landed {
        strategy: "merge".into(),
        landed_ref: sha.into(),
        ..Landed::default()
    })
}

fn parked(kind: &str) -> DomainEventPayload {
    DomainEventPayload::LandingParked(LandingParked {
        park_kind: kind.into(),
        ..LandingParked::default()
    })
}

#[test]
fn sources_are_decided_from_the_store_and_bases_from_the_trailers() {
    let f = Fixture::new();
    for id in [
        "plain",
        "parked",
        "flaky",
        "retried",
        "failed",
        "verify-flake",
    ] {
        f.define(id, true);
    }
    f.define("no-oracle", false);

    let (plain_base, plain_tip) = f.landed_commit("plain", "run-plain");
    f.run(
        "plain",
        "run-plain",
        Status::Done,
        &[false],
        &[(1, landed(&plain_tip))],
    );
    let (_, parked_tip) = f.landed_commit("parked", "run-parked");
    f.run(
        "parked",
        "run-parked",
        Status::Done,
        &[false, false],
        &[
            (1, parked(PARK_KIND_HELD_OUT_GATE)),
            (2, landed(&parked_tip)),
        ],
    );
    let (_, flaky_tip) = f.landed_commit("flaky", "run-flaky");
    f.run(
        "flaky",
        "run-flaky",
        Status::Done,
        &[false],
        &[
            (1, parked(PARK_KIND_HELD_OUT_GATE)),
            (1, landed(&flaky_tip)),
        ],
    );
    let (_, retried_tip) = f.landed_commit("retried", "run-retried");
    f.run(
        "retried",
        "run-retried",
        Status::Done,
        &[true, false],
        &[(2, landed(&retried_tip))],
    );
    let (_, vf_tip) = f.landed_commit("verify-flake", "run-vf");
    f.run(
        "verify-flake",
        "run-vf",
        Status::Done,
        &[false],
        &[(1, parked("standing-verify")), (1, landed(&vf_tip))],
    );
    f.run("failed", "run-failed", Status::Failed, &[true], &[]);
    f.run("no-oracle", "run-no", Status::Done, &[false], &[]);

    let (candidates, skipped) =
        mine(&f.store, &f.paths(), f.repo.path(), &MineOptions::default()).unwrap();
    let by_id: BTreeMap<&str, &Candidate> =
        candidates.iter().map(|c| (c.task.id.as_str(), c)).collect();
    assert_eq!(by_id["plain"].task.source, Source::Landed);
    assert_eq!(
        by_id["plain"].task.base, plain_base,
        "base = parent of the run's own commits"
    );
    assert_eq!(by_id["plain"].task.reference, plain_tip);
    assert_eq!(by_id["plain"].task.prior_cost_usd, Some(1.5 * 1.0));
    assert_eq!(by_id["parked"].task.source, Source::HardParked);
    assert_eq!(by_id["flaky"].task.source, Source::Landed);
    assert!(
        by_id["flaky"].reason.contains("flake"),
        "{}",
        by_id["flaky"].reason
    );
    assert_eq!(by_id["retried"].task.source, Source::HardRetried);
    assert_eq!(
        by_id["verify-flake"].task.source,
        Source::Landed,
        "standing-verify parks are not hardness"
    );
    assert_eq!(by_id["failed"].task.source, Source::HardFailed);
    assert!(by_id["failed"].task.reference.is_empty());
    assert_eq!(
        by_id["plain"].task.provenance.phase.as_deref(),
        Some("01-phase")
    );
    assert!(
        skipped
            .iter()
            .any(|s| s.task_id == "no-oracle" && s.reason.contains("held-out"))
    );
    // Hard ones first.
    assert!(candidates[0].task.source.is_hard());

    let only_landed = MineOptions {
        hard: false,
        ..MineOptions::default()
    };
    let (c, _) = mine(&f.store, &f.paths(), f.repo.path(), &only_landed).unwrap();
    assert!(c.iter().all(|c| !c.task.source.is_hard()));
}

#[test]
fn the_draft_keeps_the_task_tokenized_its_specs_and_its_oracle_closure() {
    let f = Fixture::new();
    f.define("plain", true);
    let (_, tip) = f.landed_commit("plain", "run-plain");
    f.run(
        "plain",
        "run-plain",
        Status::Done,
        &[false],
        &[(1, landed(&tip))],
    );
    let (candidates, _) =
        mine(&f.store, &f.paths(), f.repo.path(), &MineOptions::default()).unwrap();
    let evals = tempfile::tempdir().unwrap();
    let manifest = build_draft(
        evals.path(),
        "core",
        "p1",
        f.repo.path(),
        &f.paths(),
        &candidates,
        t0(),
    )
    .unwrap();
    assert_eq!(manifest.tasks.len(), 1);
    let dir = SetDir::draft(evals.path(), "core");
    let task = std::fs::read_to_string(dir.task_file("plain")).unwrap();
    assert!(!task.contains("prerequisites"), "{task}");
    assert!(!task.contains(&f.prefix()), "{task}");
    assert!(
        task.contains("@EVAL_PROJECT_HOME@/specs/0001-spec.md"),
        "{task}"
    );
    assert!(dir.spec_dir("plain").join("specs/0001-spec.md").is_file());
    assert!(!dir.spec_dir("plain").join("held-out").exists());
    // The script its own grader runs comes along; the oracle it names does not.
    assert!(
        dir.spec_dir("plain")
            .join("verification/guard.sh")
            .is_file()
    );
    assert!(
        !dir.spec_dir("plain")
            .join("verification/0001/oracle-plain.sh")
            .exists()
    );
    // A top-level verification/ file an oracle names is not a package: the
    // rest of verification/ (other tasks' oracles, guards) stays out.
    let oracle_files = crate::set::files_under(&dir.oracle_dir("plain")).unwrap();
    assert!(
        !oracle_files
            .iter()
            .any(|f| f.ends_with("verification/guard.sh")),
        "{oracle_files:?}"
    );
    let reg = std::fs::read_to_string(dir.registration("plain")).unwrap();
    assert!(
        reg.contains("sh @EVAL_PROJECT_HOME@/verification/0001/oracle-plain.sh"),
        "{reg}"
    );
    let home = dir.oracle_dir("plain").join("home");
    for rel in [
        "verification/0001/oracle-plain.sh",
        "verification/0001/helper.sh",
        "verification/caps.sh",
        "verification/0001/node_modules/lib/index.js",
    ] {
        assert!(home.join(rel).is_file(), "{rel}");
    }
    let oracle = std::fs::read_to_string(home.join("verification/0001/oracle-plain.sh")).unwrap();
    assert!(
        oracle.contains("@EVAL_PROJECT_HOME@/verification/caps.sh"),
        "{oracle}"
    );
    // Adding again replaces, never duplicates.
    let again = build_draft(
        evals.path(),
        "core",
        "p1",
        f.repo.path(),
        &f.paths(),
        &candidates,
        t0(),
    )
    .unwrap();
    assert_eq!(again.tasks.len(), 1);
}

#[test]
fn work_base_falls_back_when_the_tip_has_no_trailer() {
    let repo = TempRepo::new();
    let a = repo.commit("a", &[("f", "1")]);
    let b = repo.commit("b", &[("f", "2")]);
    assert_eq!(work_base(repo.path(), &b, "run-x", "t"), None);
    let c = repo.commit("c\n\nFlywheel-Task: t", &[("f", "3")]);
    assert_eq!(work_base(repo.path(), &c, "run-x", "t"), Some(b.clone()));
    assert_ne!(a, b);
}
