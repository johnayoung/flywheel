//! The hub's Flywheel panel against a real flywheel project: a temp git
//! repo whose project lives under a temp `$FLYWHEEL_HOME` (task files under
//! `<project>/tasks/active/`), and a SQLite store populated through the
//! real store APIs, bound to the project's partition (lifecycles,
//! attempts, grader receipts, a landing park, claims, a stop event), plus
//! the autopilot's activity record and a run transcript written by the
//! real telemetry sink. First the provider directly, then the real `fw hub
//! serve` binary over HTTP and the events WebSocket.

#![cfg(unix)]

// Stand-in scripts are written the flake-free way (see the helper).
#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use flywheel::hub_flywheel::{AUTOPILOT_ACTIVITY, HubFlywheel};
use flywheel_core::events::{DomainEvent, DomainEventPayload, LandingParked};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::store::{
    AttemptStore, ControlCommandStore, DomainEventStore, GraderResultRecord, GraderResultStore,
    LifecycleStore, TelemetryRecord, TelemetrySink,
};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::GraderType;
use flywheel_core::telemetry_file::FileTelemetrySink;
use flywheel_hub::flywheel::{AlertKind, FlywheelProvider, TaskLight};
use flywheel_orchestrator::autopilot_activity::{AutopilotActivity, epoch_now, write_activity};
use flywheel_orchestrator::claims::{ClaimStore, STOP_DANGLING_PREREQUISITE, SqliteClaimStore};
use flywheel_orchestrator::home::{Home, ProjectPaths, resolve_project};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use script::write_script;

/// The project `root` (a git repo) resolves to under the home at `home`,
/// minted on first sight exactly as any `fw` verb run there would: the
/// hub's panel shows only repos whose project the home knows.
fn adopt(home: &Path, root: &Path) -> Project {
    let project = resolve_project(&Home::at(home), root).unwrap();
    Project {
        id: project.id,
        paths: project.paths,
    }
}

struct Project {
    id: String,
    paths: ProjectPaths,
}

impl Project {
    fn tasks(&self) -> PathBuf {
        self.paths.tasks()
    }
    fn db(&self) -> PathBuf {
        self.paths.db()
    }
    fn dir(&self) -> PathBuf {
        self.paths.root().to_path_buf()
    }
    /// The project's core store, bound to its partition.
    fn store(&self) -> SqliteStore {
        std::fs::create_dir_all(self.dir()).unwrap();
        SqliteStore::open_partition(self.db(), &self.id).unwrap()
    }
}

fn write_task(tasks: &Path, phase: &str, id: &str, prerequisites: &[&str]) {
    let path = tasks.join("active").join(phase).join(format!("{id}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let task = json!({
        "id": id,
        "goal": format!("Goal for {id}."),
        "graders": [{"type": "command", "name": "unit", "run": "true"}],
        "prerequisites": prerequisites,
    });
    std::fs::write(path, task.to_string()).unwrap();
}

fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("expected an object"),
    }
}

/// A lifecycle through `path`, with one attempt (ended with `outcome`
/// when given) and that attempt's grader receipts.
fn seed_run(
    store: &SqliteStore,
    task_id: &str,
    run_id: &str,
    path: &[Status],
    at: DateTime<Utc>,
    outcome: Option<Outcome>,
    graders: &[(&str, bool)],
) {
    let mut lc = Lifecycle::new(task_id);
    lc.run_id = run_id.to_owned();
    for (i, status) in path.iter().enumerate() {
        let error = if *status == Status::FailedValidation {
            "grader unit failed"
        } else {
            ""
        };
        lc.transition_to(*status, error, Some(at + ChronoDuration::seconds(i as i64)))
            .unwrap();
    }
    store.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, at, run_id);
    attempt.outcome = outcome;
    attempt.input_tokens = 1200;
    attempt.output_tokens = 300;
    attempt.total_cost_usd = 0.25;
    attempt.iterations_completed = 1;
    attempt.turns = 4;
    attempt.last_activity_at = Some(at);
    if outcome.is_some() {
        attempt.ended_at = Some(at + ChronoDuration::seconds(30));
    }
    store.save_attempt(run_id, &attempt, None).unwrap();
    for (ordinal, (name, passed)) in graders.iter().enumerate() {
        store
            .append_grader_result(&GraderResultRecord {
                run_id: run_id.to_owned(),
                attempt_number: 1,
                ordinal: ordinal as u32,
                grader_type: GraderType::Command,
                grader_spec: obj(json!({"type": "command", "name": name, "run": "true"})),
                passed: *passed,
                duration_ms: 40 + ordinal as u64,
                payload: Map::new(),
                ts: at + ChronoDuration::seconds(20),
                grader_name: Some((*name).to_owned()),
                id: None,
            })
            .unwrap();
    }
}

const DONE: [Status; 4] = [
    Status::Ready,
    Status::Running,
    Status::Validating,
    Status::Done,
];

/// The project every test reads (see the module doc): `root` becomes a git
/// repo (no commit yet) whose project under `home` holds the tasks, store,
/// transcript, and autopilot record.
fn adopted_repo(home: &Path, root: &Path) -> Project {
    git(root, &["init", "-q"]);
    let project = adopt(home, root);
    let tasks = project.tasks();
    write_task(&tasks, "01-core", "alpha", &[]);
    write_task(&tasks, "01-core", "beta", &[]);
    write_task(&tasks, "01-core", "gamma", &[]);
    write_task(&tasks, "01-core", "delta", &["alpha"]);
    write_task(&tasks, "02-land", "eps", &[]);

    let db = project.db();
    let store = project.store();
    let t0 = Utc::now() - ChronoDuration::minutes(30);
    seed_run(
        &store,
        "alpha",
        "run-alpha",
        &DONE,
        t0,
        Some(Outcome::Succeeded),
        &[("unit", true), ("lint", true)],
    );
    seed_run(
        &store,
        "beta",
        "run-beta",
        &[
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::FailedValidation,
        ],
        t0 + ChronoDuration::minutes(5),
        Some(Outcome::ValidationFailed),
        &[("unit", false), ("lint", true)],
    );
    seed_run(
        &store,
        "eps",
        "run-eps",
        &DONE,
        t0 + ChronoDuration::minutes(10),
        Some(Outcome::Succeeded),
        &[("unit", true)],
    );
    let parked = store.load_lifecycle("run-eps").unwrap().unwrap();
    store
        .append_domain_event(
            &DomainEvent::new(
                "run-eps",
                t0 + ChronoDuration::minutes(11),
                DomainEventPayload::LandingParked(LandingParked {
                    park_kind: "divergent-base".into(),
                    detail: "base moved under the branch".into(),
                    ..LandingParked::default()
                }),
            ),
            parked.version,
        )
        .unwrap();
    seed_run(
        &store,
        "gamma",
        "run-gamma",
        &[Status::Ready, Status::Running],
        Utc::now() - ChronoDuration::seconds(20),
        None,
        &[],
    );

    let claims = SqliteClaimStore::open_partition(&db, &project.id).unwrap();
    claims
        .acquire_claim("gamma", "worker-7", Utc::now(), 600.0, &BTreeSet::new())
        .unwrap()
        .expect("claimed");
    claims
        .record_stop_event(
            STOP_DANGLING_PREREQUISITE,
            "zeta",
            "prerequisite omega is not in the listing",
            Utc::now(),
            "",
        )
        .unwrap();

    let sink = FileTelemetrySink::new(project.paths.logs());
    for text in ["Reading the task.", "All graders should pass now."] {
        let mut record = TelemetryRecord::new("run-alpha", t0, "AssistantMessage");
        record.payload = obj(json!({"content": [{"type": "text", "text": text}]}));
        record.attempt_number = Some(1);
        record.iteration_number = Some(1);
        sink.append_telemetry(&record).unwrap();
    }

    // This test process stands in for the daemon: its pid runs.
    let mut activity =
        AutopilotActivity::new(i64::from(std::process::id()), "idle", 3, epoch_now(), 60.0);
    activity.expires_at = Some(epoch_now() + 120.0);
    let path = project.paths.autopilot_logs().join(AUTOPILOT_ACTIVITY);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_activity(&path, &activity).unwrap();
    project
}

/// A temp dir holding `repo/` (the adopted repo) and `fw-home/` (its home).
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    project: Project,
}

impl Fixture {
    fn provider(&self) -> HubFlywheel {
        HubFlywheel::new("/usr/bin/false").with_home(&self.home)
    }
}

fn temp_repo() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("repo");
    let home = base.join("fw-home");
    std::fs::create_dir_all(&root).unwrap();
    let project = adopted_repo(&home, &root);
    Fixture {
        _tmp: tmp,
        root,
        home,
        project,
    }
}

#[test]
fn panel_reads_phases_runs_graders_claims_daemons_and_alerts() {
    let fx = temp_repo();
    let root = fx.root.clone();
    let provider = fx.provider();
    assert!(provider.adopted(&root));
    let panel = provider.panel(&root);
    assert_eq!(panel.problem, None, "{panel:#?}");
    assert_eq!(
        panel.source,
        format!("directory {}", fx.project.tasks().display())
    );
    assert_eq!(panel.store, format!("sqlite {}", fx.project.db().display()));

    let names: Vec<&str> = panel.phases.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["01-core", "02-land"]);
    let light = |id: &str| {
        panel
            .phases
            .iter()
            .flat_map(|p| &p.tasks)
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("task {id}"))
            .clone()
    };
    assert_eq!(light("alpha").state, TaskLight::Verified);
    assert_eq!(light("beta").state, TaskLight::Failed);
    assert_eq!(light("beta").error, "grader unit failed");
    assert_eq!(light("gamma").state, TaskLight::Running);
    assert_eq!(light("gamma").worker_id, None);
    assert_eq!(light("delta").state, TaskLight::Pending);
    assert_eq!(light("delta").prerequisites, ["alpha"]);
    let eps = light("eps");
    assert_eq!(eps.state, TaskLight::Parked);
    assert_eq!(eps.stranded.as_ref().unwrap().kind, "divergent-base");

    // Needs you: the failure, the strand, and the rowless stop.
    let alerts: Vec<(AlertKind, &str)> = panel
        .alerts
        .iter()
        .map(|a| (a.kind, a.subject.as_str()))
        .collect();
    assert!(alerts.contains(&(AlertKind::Failed, "beta")), "{alerts:?}");
    assert!(alerts.contains(&(AlertKind::Stranded, "eps")), "{alerts:?}");
    assert!(alerts.contains(&(AlertKind::Stopped, "zeta")), "{alerts:?}");
    assert_eq!(alerts.len(), 3, "{alerts:?}");
    let failed = panel.alerts.iter().find(|a| a.subject == "beta").unwrap();
    assert_eq!(failed.run_id.as_deref(), Some("run-beta"));
    let summary = panel.summary();
    assert_eq!(summary.counts["verified"], 1);
    assert_eq!(summary.counts["failed"], 1);
    assert_eq!(summary.counts["parked"], 1);
    assert_eq!(summary.alerts.len(), 3);

    // In flight: gamma, from the console's live rows.
    assert_eq!(panel.live.len(), 1);
    assert_eq!(panel.live[0].run_id, "run-gamma");
    assert_eq!(panel.live[0].tokens, 1500);

    // Recent runs, newest first, with the latest attempt's receipts.
    let runs: Vec<&str> = panel.runs.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(runs.len(), 4);
    let beta = panel.runs.iter().find(|r| r.run_id == "run-beta").unwrap();
    assert_eq!(beta.status, "failed_validation");
    assert_eq!(beta.outcome.as_deref(), Some("validation_failed"));
    let receipts: Vec<(&str, bool)> = beta
        .graders
        .iter()
        .map(|g| (g.name.as_str(), g.passed))
        .collect();
    assert_eq!(receipts, [("unit", false), ("lint", true)]);
    assert_eq!(beta.graders[0].kind, "command");
    assert!(beta.updated_at >= beta.started_at);

    // A live lease is a live worker; the activity record is a live
    // autopilot.
    assert_eq!(panel.claims.len(), 1);
    assert!(panel.claims[0].live && panel.claims[0].task_id == "gamma");
    assert!(panel.worker.live, "{:?}", panel.worker);
    assert!(panel.worker.detail.contains("worker-7"));
    assert!(panel.autopilot.live, "{:?}", panel.autopilot);
    assert!(
        panel
            .autopilot
            .detail
            .contains(&format!("pid {}", std::process::id()))
    );
}

#[test]
fn portfolio_reports_every_plan_with_progress_spend_and_verified_per_day() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("fw-home");
    let (good, broken) = (base.join("good"), base.join("broken"));
    std::fs::create_dir_all(&good).unwrap();
    std::fs::create_dir_all(&broken).unwrap();
    let project = adopted_repo(&home, &good);
    let broken_project = adopted_repo(&home, &broken);

    // An archived phase whose runs outnumber the recent-run window, its
    // verifying runs straddling the 24h boundary.
    let archived = project.tasks().join("archive").join("00-seed");
    std::fs::create_dir_all(&archived).unwrap();
    let store = project.store();
    for i in 0..14 {
        let id = format!("seed-{i}");
        let task =
            json!({"id": id, "goal": "Seed.", "graders": [{"type": "command", "run": "true"}]});
        std::fs::write(archived.join(format!("{id}.json")), task.to_string()).unwrap();
        let hours = if i < 10 { 30 } else { 2 };
        seed_run(
            &store,
            &id,
            &format!("run-seed-{i}"),
            &DONE,
            Utc::now() - ChronoDuration::hours(hours),
            Some(Outcome::Succeeded),
            &[],
        );
    }
    // The broken folder's store cannot be read.
    std::fs::write(broken_project.db(), b"not a sqlite database at all").unwrap();

    let provider = HubFlywheel::new("/usr/bin/false").with_home(&home);
    let panel = provider.panel(&good);
    assert_eq!(panel.problem, None, "{panel:#?}");
    assert_eq!(
        panel.runs.len(),
        12,
        "the recent window is bounded; plans are not"
    );
    let plan = |name: &str| {
        panel
            .plans
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("plan {name}: {:#?}", panel.plans))
            .clone()
    };
    let names: Vec<&str> = panel.plans.iter().map(|p| p.name.as_str()).collect();
    // Needing someone first: 01-core (beta failed), 02-land (eps parked).
    assert_eq!(names, ["01-core", "02-land", "00-seed"]);

    let seed = plan("00-seed");
    assert!(seed.archived);
    assert_eq!((seed.verified, seed.total), (14, 14));
    assert_eq!(seed.verified_24h, 4);
    assert!((seed.spend_usd - 3.5).abs() < 1e-9, "{}", seed.spend_usd);
    assert_eq!((seed.needs, seed.running), (0, 0));

    let core = plan("01-core");
    assert!(!core.archived);
    // alpha DONE; beta failed; gamma running; delta never ran.
    assert_eq!((core.verified, core.total), (1, 4));
    assert_eq!(core.verified_24h, 1);
    assert!((core.spend_usd - 0.75).abs() < 1e-9, "{}", core.spend_usd);
    assert_eq!((core.needs, core.running), (1, 1));

    let land = plan("02-land");
    assert_eq!((land.verified, land.total, land.verified_24h), (1, 1, 1));
    assert_eq!(land.needs, 1, "its DONE run's landing parked");
    assert_eq!(panel.summary().plans, panel.plans);

    // The broken folder names its problem and still lists its plans from
    // the task files; the good folder reads on.
    let failed = provider.panel(&broken);
    assert!(failed.problem.is_some(), "{failed:#?}");
    let listed: Vec<(&str, usize, usize)> = failed
        .plans
        .iter()
        .map(|p| (p.name.as_str(), p.verified, p.total))
        .collect();
    assert_eq!(listed, [("01-core", 0, 4), ("02-land", 0, 1)]);
    assert_eq!(provider.panel(&good).plans.len(), 3);
}

#[test]
fn run_output_is_fw_show_plus_the_transcript_tailed_incrementally() {
    let fx = temp_repo();
    let root = fx.root.clone();
    let provider = fx.provider();
    let out = provider.run_output(&root, "run-alpha", 0).unwrap();
    assert_eq!(out.task_id.as_deref(), Some("alpha"));
    assert_eq!(out.status.as_deref(), Some("done"));
    assert!(!out.active);
    assert!(
        out.summary.contains("run      : run-alpha"),
        "{}",
        out.summary
    );
    assert!(
        out.summary.contains("pass  command    unit"),
        "{}",
        out.summary
    );
    let bodies: Vec<&str> = out.entries.iter().map(|e| e.body.as_str()).collect();
    assert_eq!(
        bodies,
        ["Reading the task.", "All graders should pass now."]
    );
    assert_eq!(out.entries[0].kind, "agent_text");
    assert_eq!((out.from, out.next), (0, 2));

    // Nothing new: an empty page at the same cursor.
    let again = provider.run_output(&root, "run-alpha", out.next).unwrap();
    assert!(again.entries.is_empty());
    assert_eq!(again.next, 2);

    // The agent writes more: only the new entry comes back.
    let sink = FileTelemetrySink::new(fx.project.paths.logs());
    let mut record = TelemetryRecord::new("run-alpha", Utc::now(), "AssistantMessage");
    record.payload = obj(json!({"content": [{"type": "text", "text": "Done."}]}));
    record.attempt_number = Some(1);
    record.iteration_number = Some(1);
    sink.append_telemetry(&record).unwrap();
    let more = provider.run_output(&root, "run-alpha", again.next).unwrap();
    assert_eq!(more.from, 2);
    assert_eq!(more.entries.len(), 1);
    assert_eq!(more.entries[0].body, "Done.");
    assert_eq!(more.next, 3);

    // In flight runs say so; unknown runs are an error.
    assert!(provider.run_output(&root, "run-gamma", 0).unwrap().active);
    let missing = provider.run_output(&root, "run-nope", 0).unwrap_err();
    assert!(missing.contains("no run run-nope"), "{missing}");
}

#[test]
fn fingerprint_follows_the_store_and_the_task_tree() {
    let fx = temp_repo();
    let root = fx.root.clone();
    let provider = fx.provider();
    let first = provider.fingerprint(&root);
    assert_eq!(provider.fingerprint(&root), first, "stable when idle");
    std::thread::sleep(Duration::from_millis(20));
    write_task(&fx.project.tasks(), "03-next", "theta", &[]);
    let after_task = provider.fingerprint(&root);
    assert_ne!(after_task, first);
    std::thread::sleep(Duration::from_millis(20));
    let store = fx.project.store();
    seed_run(
        &store,
        "theta",
        "run-theta",
        &[Status::Ready],
        Utc::now(),
        None,
        &[],
    );
    assert_ne!(provider.fingerprint(&root), after_task);
    // And the panel follows: theta is now in flight-or-ready.
    let panel = provider.panel(&root);
    assert!(panel.phases.iter().any(|p| p.name == "03-next"));
}

#[test]
fn a_repo_without_a_store_lists_pending_tasks_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("repo");
    let home = base.join("fw-home");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    let project = adopt(&home, &root);
    write_task(&project.tasks(), "01-core", "alpha", &[]);
    let provider = HubFlywheel::new("/usr/bin/false").with_home(&home);
    let panel = provider.panel(&root);
    let problem = panel.problem.clone().unwrap();
    assert!(problem.contains("no store yet"), "{problem}");
    assert_eq!(panel.phases.len(), 1);
    assert_eq!(panel.phases[0].tasks[0].state, TaskLight::Pending);
    assert!(!project.db().exists());
    assert!(!panel.worker.live && !panel.autopilot.live);
    assert!(provider.run_output(&root, "run-x", 0).is_err());
    assert!(!project.db().exists());
    // Nothing of the panel lands in the repo tree.
    let status = Command::new("git")
        .args(["status", "--porcelain", "--ignored"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[test]
fn only_repos_whose_project_the_home_knows_are_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("fw-home");
    let provider = HubFlywheel::new("/usr/bin/false").with_home(&home);

    // Not a git repo at all.
    let plain = base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(!provider.adopted(&plain));

    // A git repo still carrying the legacy layout is not a project: the old
    // markers mean nothing now, and reading never mints an id.
    let legacy = base.join("legacy");
    std::fs::create_dir_all(legacy.join(".flywheel/tasks/active")).unwrap();
    git(&legacy, &["init", "-q"]);
    std::fs::write(
        legacy.join("flywheel.toml"),
        "[source]\nkind = \"directory\"\n",
    )
    .unwrap();
    assert!(!provider.adopted(&legacy));
    assert!(!home.exists(), "detection writes no home state");

    // Once a verb resolved it, the repo is a known project.
    adopt(&home, &legacy);
    assert!(provider.adopted(&legacy));
    // A project another home knows is not this home's.
    let other = HubFlywheel::new("/usr/bin/false").with_home(base.join("elsewhere"));
    assert!(!other.adopted(&legacy));

    // Launches run in the folder against its project: no policy file named.
    let worker = provider.worker_command(&legacy, &Default::default());
    assert_eq!(worker, ["/usr/bin/false", "worker"]);
    let autopilot = provider.autopilot_command(&legacy);
    assert_eq!(autopilot, ["/usr/bin/false", "autopilot"]);
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// The files of this process's open descriptors under `dir`.
#[cfg(target_os = "linux")]
fn open_files_under(pid: u32, dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|fds| {
            fds.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
                .filter(|target| target.starts_with(dir))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_linked_worktree_maps_to_its_repos_project_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("fw-home");
    let main = base.join("repo");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    std::fs::write(main.join("README.md"), "x\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let project = adopt(&home, &main);
    let sibling = base.join("repo.worktrees/task");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            sibling.to_str().unwrap(),
        ],
    );
    let provider = HubFlywheel::new("/usr/bin/false").with_home(&home);
    assert!(provider.adopted(&main));
    // The worktree shares the repo's git dir, so it is the same project:
    // its panel reads the project's one store, labelled as a worktree of
    // the main checkout.
    assert!(provider.adopted(&sibling));
    let panel = provider.panel(&sibling);
    assert_eq!(panel.worktree_of.as_deref(), Some(main.as_path()));
    assert_eq!(panel.summary().worktree_of.as_deref(), Some(main.as_path()));
    let problem = panel.problem.clone().unwrap_or_default();
    assert!(problem.contains("no store yet"), "{problem}");
    assert!(!project.db().exists(), "reading creates no store");

    project.store();
    let main_panel = provider.panel(&main);
    assert_eq!(main_panel.worktree_of, None);
    assert_eq!(main_panel.problem, None, "{main_panel:?}");
    assert_eq!(
        main_panel.store,
        format!("sqlite {}", project.db().display())
    );
    let panel = provider.panel(&sibling);
    assert_eq!(panel.worktree_of.as_deref(), Some(main.as_path()));
    assert_eq!(panel.problem, None, "{panel:?}");
    assert_eq!(panel.store, main_panel.store, "one project, one store");
}

#[cfg(target_os = "linux")]
#[test]
fn forgetting_a_folder_closes_its_store() {
    let fx = temp_repo();
    let root = fx.root.clone();
    let provider = fx.provider();
    let panel = provider.panel(&root);
    assert!(panel.problem.is_none(), "{:?}", panel.problem);
    let dir = fx.project.dir();
    assert!(
        !open_files_under(std::process::id(), &dir).is_empty(),
        "the store stays open between reads"
    );
    assert_eq!(provider.open_folders(), 1);
    provider.forget(&root);
    assert_eq!(provider.open_folders(), 0);
    assert_eq!(
        open_files_under(std::process::id(), &dir),
        Vec::<PathBuf>::new()
    );
    // A later read reopens it.
    assert!(provider.panel(&root).problem.is_none());
}

// --- the real hub ------------------------------------------------------------

struct Server {
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(server: &Server, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    http_as(server, &server.token, method, path, body)
}

/// [`http`] with `bearer` (a peer credential, say) instead of the token.
fn http_as(
    server: &Server,
    bearer: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        server.port,
        bearer,
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let value = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        serde_json::from_str(&dechunk(payload)).unwrap_or(Value::Null)
    } else {
        serde_json::from_str(payload).unwrap_or(Value::Null)
    };
    (status, value)
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}

/// A fresh empty HOME under the test dir, so the hub never resolves the
/// developer's `~/.claude`, `~/.codex`, or `~/code` import root.
fn hermetic_home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// The flywheel home the hub (and every `fw` it launches) resolves
/// projects under.
fn fw_home(dir: &Path) -> PathBuf {
    dir.join("fw-home")
}

fn start(dir: &Path) -> Server {
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "serve", "--port", "0", "--state-dir"])
        .arg(dir.join("state"))
        .env("PATH", path)
        .env("SHELL", "/bin/sh")
        .env("HOME", hermetic_home(dir))
        .env("FLYWHEEL_HOME", fw_home(dir))
        .env_remove("FLYWHEEL_PG_DSN")
        .env("CLAUDE_CONFIG_DIR", dir.join("claude-config"))
        .env("CODEX_HOME", dir.join("codex-home"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("unexpected banner {first:?}"));
    std::thread::spawn(move || for _ in lines {});
    let token = std::fs::read_to_string(dir.join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    Server { child, port, token }
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An events WebSocket (bearer auth) and its next `flywheel` message.
fn events(server: &Server) -> tungstenite::WebSocket<TcpStream> {
    use tungstenite::client::IntoClientRequest;
    let stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request = format!("ws://127.0.0.1:{}/api/events?client=fwtest", server.port)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", server.token).parse().unwrap(),
    );
    let (socket, _) = tungstenite::client(request, stream).unwrap();
    socket
}

fn next_flywheel(socket: &mut tungstenite::WebSocket<TcpStream>) -> Value {
    loop {
        let message = socket.read().expect("an events message");
        if let tungstenite::Message::Text(text) = message {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "flywheel" {
                return value;
            }
        }
    }
}

#[test]
fn the_hub_serves_the_panel_pushes_changes_and_starts_a_worker_session() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    // Never the real agent CLI.
    let fake = dir.join("bin/claude");
    write_script(&fake, "#!/bin/sh\necho fake claude >&2\nexit 1\n");
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let project = adopted_repo(&fw_home(&dir), &root);
    let plain = dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let server = start(&dir);
    let mut socket = events(&server);

    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "loop", "folders": [{"path": root}, {"path": plain}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();

    // The monitor finds the adopted folder and pushes its summary.
    let pushed = next_flywheel(&mut socket);
    assert_eq!(pushed["folder"], json!(root));
    assert_eq!(pushed["summary"]["counts"]["verified"], 1);
    assert_eq!(pushed["summary"]["alerts"].as_array().unwrap().len(), 3);
    assert_eq!(pushed["summary"]["worker_live"], true);

    let (code, list) = http(&server, "GET", "/api/flywheel", None);
    assert_eq!(code, 200);
    assert_eq!(list["enabled"], true);
    let folders = list["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1, "the plain folder is not adopted: {list}");

    let query = |path: &Path| {
        path.to_string_lossy()
            .replace('%', "%25")
            .replace('/', "%2F")
    };
    let (code, panel) = http(
        &server,
        "GET",
        &format!("/api/flywheel/panel?folder={}", query(&root)),
        None,
    );
    assert_eq!(code, 200, "{panel}");
    assert_eq!(panel["phases"][0]["name"], "01-core");
    assert_eq!(panel["runs"].as_array().unwrap().len(), 4);
    let (code, _) = http(
        &server,
        "GET",
        &format!("/api/flywheel/panel?folder={}", query(&plain)),
        None,
    );
    assert_eq!(code, 404);
    let (code, _) = http(
        &server,
        "GET",
        &format!("/api/flywheel/panel?folder={}", query(&dir)),
        None,
    );
    assert_eq!(code, 404, "not a workspace folder");

    // Run output, and run ids that could name other files are refused.
    let (code, out) = http(
        &server,
        "GET",
        &format!("/api/flywheel/run?folder={}&run=run-beta", query(&root)),
        None,
    );
    assert_eq!(code, 200, "{out}");
    assert!(
        out["summary"]
            .as_str()
            .unwrap()
            .contains("FAIL  command    unit")
    );
    let (code, _) = http(
        &server,
        "GET",
        &format!("/api/flywheel/run?folder={}&run=..%2Fx", query(&root)),
        None,
    );
    assert_eq!(code, 400);

    // A change on disk is pushed: the failed task's retry is verified.
    let store = project.store();
    seed_run(
        &store,
        "beta",
        "run-beta-2",
        &DONE,
        Utc::now(),
        Some(Outcome::Succeeded),
        &[("unit", true)],
    );
    let changed = wait_for("the retry to be pushed", || {
        let message = next_flywheel(&mut socket);
        (message["summary"]["counts"]["verified"] == 2).then_some(message)
    });
    assert_eq!(changed["summary"]["alerts"].as_array().unwrap().len(), 2);

    // Start worker here: a real hub shell session running `fw worker` in
    // the folder (a repo with no commit yet, so the worker has no branch
    // to land on, says so, and exits), then a shell.
    let (code, session) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({ "folder": root, "workspace_id": workspace_id, "cols": 100, "rows": 30 })),
    );
    assert_eq!(code, 201, "{session}");
    assert_eq!(session["name"], "fw worker");
    assert_eq!(session["kind"], "shell");
    assert_eq!(session["cwd"], json!(root));
    let argv: Vec<String> = session["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(argv[3], "fw-worker");
    assert_eq!(argv[5], "worker");
    let sid = session["id"].as_str().unwrap().to_owned();
    let screen = wait_for("the worker's last words", || {
        let (_, screen) = http(&server, "GET", &format!("/api/sessions/{sid}/screen"), None);
        let text = screen["lines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        text.contains("fw worker exited with status")
            .then_some(text)
    });
    assert!(screen.contains("policy error"), "{screen}");
    let (_, view) = http(&server, "GET", &format!("/api/sessions/{sid}"), None);
    assert_eq!(view["state"], "running", "the shell outlives the worker");

    // Only adopted workspace folders can start one.
    let (code, _) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({ "folder": plain })),
    );
    assert_eq!(code, 404);

    // Dropping the folder from the workspace (its sessions closed first)
    // removes it.
    let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{sid}"), None);
    assert_eq!(code, 204);
    let (code, _) = http(
        &server,
        "PATCH",
        &format!("/api/workspaces/{workspace_id}"),
        Some(json!({ "folders": [{"path": plain}] })),
    );
    assert_eq!(code, 200);
    let removed = wait_for("the removal push", || {
        let message = next_flywheel(&mut socket);
        message["summary"].is_null().then_some(message)
    });
    assert_eq!(removed["folder"], json!(root));
    // ... and closes its store in the hub.
    #[cfg(target_os = "linux")]
    wait_for("the hub to close the removed folder's store", || {
        open_files_under(server.child.id(), &project.dir())
            .is_empty()
            .then_some(())
    });
}

#[test]
fn start_worker_here_refuses_a_duplicate_while_the_first_still_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let fake = dir.join("bin/claude");
    write_script(&fake, "#!/bin/sh\necho fake claude >&2\nexit 1\n");
    // A project's git repo with nothing to do: its `fw worker` idles.
    let root = dir.join("idle");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README.md"), "idle\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    adopt(&fw_home(&dir), &root);
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "idle", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();
    let start_worker = |another: bool| {
        http(
            &server,
            "POST",
            "/api/flywheel/worker",
            Some(json!({ "folder": root, "workspace_id": workspace_id, "another": another })),
        )
    };
    let (code, first) = start_worker(false);
    assert_eq!(code, 201, "{first}");
    assert_eq!(first["name"], "fw worker");
    let first_id = first["id"].as_str().unwrap().to_owned();

    let (code, refused) = start_worker(false);
    assert_eq!(code, 409, "{refused}");
    assert_eq!(refused["running"]["id"], json!(first_id));
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("already running")
    );

    // Asked for explicitly: a second one, under a name of its own.
    let (code, second) = start_worker(true);
    assert_eq!(code, 201, "{second}");
    assert_eq!(second["name"], "fw worker 2");
    let (_, sessions) = http(&server, "GET", "/api/sessions", None);
    let names: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names.iter().filter(|n| **n == "fw worker").count(), 1);

    for session in [&first, &second] {
        let id = session["id"].as_str().unwrap();
        let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{id}"), None);
        assert_eq!(code, 204);
    }
    // Nothing runs there now: a plain start is accepted again.
    let (code, third) = start_worker(false);
    assert_eq!(code, 201, "{third}");
    assert_eq!(third["name"], "fw worker");
    let id = third["id"].as_str().unwrap();
    let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{id}"), None);
    assert_eq!(code, 204);
}

#[test]
fn no_flywheel_turns_the_panel_off() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args([
            "hub",
            "serve",
            "--port",
            "0",
            "--no-flywheel",
            "--state-dir",
        ])
        .arg(dir.join("state"))
        .env("PATH", path)
        .env("SHELL", "/bin/sh")
        .env("HOME", hermetic_home(&dir))
        .env("FLYWHEEL_HOME", fw_home(&dir))
        .env("CLAUDE_CONFIG_DIR", dir.join("claude-config"))
        .env("CODEX_HOME", dir.join("codex-home"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first.rsplit(':').next().unwrap().trim().parse().unwrap();
    std::thread::spawn(move || for _ in lines {});
    let token = std::fs::read_to_string(dir.join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    let server = Server { child, port, token };
    let (code, list) = http(&server, "GET", "/api/flywheel", None);
    assert_eq!(code, 200);
    assert_eq!(list, json!({ "enabled": false, "folders": [] }));
    let (code, _) = http(
        &server,
        "GET",
        &format!("/api/flywheel/panel?folder={}", dir.display()),
        None,
    );
    assert_eq!(code, 404);
}

/// A `claude` stand-in a real `fw worker` drives to DONE: it records the
/// task it ran (its worktree's name) in `ran`, commits a change there,
/// and ends the iteration with `verify`.
fn verifying_claude(path: &Path, ran: &Path) {
    write_script(
        path,
        &format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
echo "$id" >> '{ran}'
date +%s%N > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{{\"intent\":\"verify\",\"reason\":\"done\"}}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}}'
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{{"input_tokens":7,"output_tokens":2}}}}'
"#,
            ran = ran.display()
        ),
    );
}

fn screen_text(server: &Server, sid: &str) -> String {
    let (_, screen) = http(server, "GET", &format!("/api/sessions/{sid}/screen"), None);
    screen["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[test]
fn run_a_task_with_worker_here_starts_a_worker_scoped_to_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let ran = dir.join("ran.log");
    verifying_claude(&dir.join("bin/claude"), &ran);
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README.md"), "scoped\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(
        &root,
        &["config", "user.email", "hub-scope@example.invalid"],
    );
    git(&root, &["config", "user.name", "hub scope"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let tasks = adopt(&fw_home(&dir), &root).tasks();
    write_task(&tasks, "01-core", "alpha", &[]);
    write_task(&tasks, "01-core", "beta", &[]);
    write_task(&tasks, "01-core", "gamma", &["alpha"]);
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "scoped", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();
    let start_worker = |body: Value| http(&server, "POST", "/api/flywheel/worker", Some(body));
    let argv_of = |session: &Value| -> Vec<String> {
        session["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect()
    };

    // Ids that are not task ids, and a prerequisite switch with no task.
    for bad in [
        json!({ "folder": root, "tasks": ["a b"] }),
        json!({ "folder": root, "tasks": [""] }),
        json!({ "folder": root, "tasks": ["--once"], "another": true }),
        json!({ "folder": root, "with_prereqs": true }),
    ] {
        let (code, refused) = start_worker(bad);
        assert_eq!(code, 400, "{refused}");
    }

    // Run beta with worker here: `fw worker --task=beta`, which runs beta
    // alone, says so, and exits 0.
    let (code, session) = start_worker(json!({
        "folder": root, "workspace_id": workspace_id, "tasks": ["beta"], "cols": 120, "rows": 30,
    }));
    assert_eq!(code, 201, "{session}");
    assert_eq!(session["name"], "fw worker beta");
    let argv = argv_of(&session);
    assert_eq!(argv[3], "fw-worker");
    // The worker runs in the folder against its project: no policy file.
    assert_eq!(&argv[5..], ["worker", "--task=beta"]);
    let sid = session["id"].as_str().unwrap().to_owned();
    let screen = wait_for("the scoped worker to finish", || {
        let text = screen_text(&server, &sid);
        text.contains("fw worker exited with status")
            .then_some(text)
    });
    assert!(
        screen.contains("scope: 1/1 verified -- beta verified"),
        "{screen}"
    );
    assert!(
        screen.contains("fw worker exited with status 0"),
        "{screen}"
    );
    // The session is its shell now: the view says the worker ended, and
    // how, so the sidebar and panel stop counting it as a live worker.
    let worker = wait_for("the session's worker to read as ended", || {
        let (_, view) = http(&server, "GET", &format!("/api/sessions/{sid}"), None);
        (view["worker"]["running"] == false).then(|| view["worker"].clone())
    });
    assert_eq!(worker, json!({ "running": false, "exit_status": 0 }));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "beta\n");
    let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{sid}"), None);
    assert_eq!(code, 204);

    // A task with an unmet prerequisite, asked to bring it along.
    let (code, session) = start_worker(json!({
        "folder": root, "workspace_id": workspace_id, "tasks": ["gamma"], "with_prereqs": true,
    }));
    assert_eq!(code, 201, "{session}");
    assert_eq!(
        &argv_of(&session)[5..],
        ["worker", "--task=gamma", "--with-prereqs"]
    );
    let sid = session["id"].as_str().unwrap().to_owned();
    let screen = wait_for("the prerequisite run to finish", || {
        let text = screen_text(&server, &sid);
        text.contains("fw worker exited with status")
            .then_some(text)
    });
    assert!(
        screen.contains("scope: 2/2 verified -- gamma verified; prerequisite alpha verified"),
        "{screen}"
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap(),
        "beta\nalpha\ngamma\n"
    );
    let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{sid}"), None);
    assert_eq!(code, 204);

    // The generic Start worker here stays unscoped.
    let (code, session) = start_worker(json!({ "folder": root, "workspace_id": workspace_id }));
    assert_eq!(code, 201, "{session}");
    assert_eq!(session["name"], "fw worker");
    assert_eq!(&argv_of(&session)[5..], ["worker"]);
    let sid = session["id"].as_str().unwrap();
    let (code, _) = http(&server, "DELETE", &format!("/api/sessions/{sid}"), None);
    assert_eq!(code, 204);
}

/// The panel's pending task's `queued_in` (`null` when none).
fn queued_in(server: &Server, root: &Path, id: &str) -> (String, Value) {
    let (code, panel) = http(
        server,
        "GET",
        &format!(
            "/api/flywheel/panel?folder={}",
            root.display().to_string().replace('/', "%2F")
        ),
        None,
    );
    assert_eq!(code, 200, "{panel}");
    let task = panel["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap().clone())
        .find(|t| t["id"] == id)
        .unwrap_or_else(|| panic!("no task {id} in {panel}"));
    (
        task["state"].as_str().unwrap().to_owned(),
        task.get("queued_in").cloned().unwrap_or(Value::Null),
    )
}

#[test]
fn pending_tasks_a_running_scoped_worker_will_run_say_which() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let ran = dir.join("ran.log");
    let gate = dir.join("gate");
    std::fs::write(&gate, "").unwrap();
    // The verifying stand-in, held at its start while `gate` exists.
    verifying_claude(&dir.join("bin/claude-verify"), &ran);
    write_script(
        &dir.join("bin/claude"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" != \"--version\" ]; then while [ -e '{gate}' ]; do sleep 0.1; done; fi\n\
             exec '{verify}' \"$@\"\n",
            gate = gate.display(),
            verify = dir.join("bin/claude-verify").display()
        ),
    );
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README.md"), "scoped\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(
        &root,
        &["config", "user.email", "hub-queue@example.invalid"],
    );
    git(&root, &["config", "user.name", "hub queue"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let tasks = adopt(&fw_home(&dir), &root).tasks();
    write_task(&tasks, "01-core", "alpha", &[]);
    write_task(&tasks, "01-core", "delta", &["alpha"]);
    write_task(&tasks, "01-core", "other", &[]);
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "queued", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    // Nothing runs yet: nothing is queued.
    assert_eq!(queued_in(&server, &root, "delta").1, Value::Null);
    let (code, session) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({
            "folder": root, "workspace_id": workspace["id"], "tasks": ["delta"],
            "with_prereqs": true,
        })),
    );
    assert_eq!(code, 201, "{session}");
    let sid = session["id"].as_str().unwrap().to_owned();
    let worker = json!({ "session_id": sid, "name": "fw worker delta" });
    // While it holds alpha (the prerequisite it runs first), delta waits
    // in it; a task outside its scope waits in none.
    let (state, queued) = wait_for("delta to read as queued", || {
        let found = queued_in(&server, &root, "delta");
        (found.1 != Value::Null).then_some(found)
    });
    assert_eq!(state, "pending");
    assert_eq!(queued, worker);
    assert_eq!(
        queued_in(&server, &root, "other"),
        ("pending".into(), Value::Null)
    );
    std::fs::remove_file(&gate).unwrap();
    let screen = wait_for("the scoped worker to finish", || {
        let text = screen_text(&server, &sid);
        text.contains("fw worker exited with status")
            .then_some(text)
    });
    assert!(
        screen.contains("scope: 2/2 verified -- delta verified; prerequisite alpha verified"),
        "{screen}"
    );
    // Its worker gone, nothing is queued in it.
    let (state, queued) = wait_for("delta to read as verified", || {
        let found = queued_in(&server, &root, "delta");
        (found.0 == "verified").then_some(found)
    });
    assert_eq!((state.as_str(), queued), ("verified", Value::Null));
    assert_eq!(
        queued_in(&server, &root, "other"),
        ("pending".into(), Value::Null)
    );
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "alpha\ndelta\n");
}

#[test]
fn steer_say_and_interrupt_enqueue_one_command_for_a_live_run_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let root = dir.join("steered");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README.md"), "steered\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let project = adopt(&fw_home(&dir), &root);
    let at = Utc::now();
    {
        let store = project.store();
        seed_run(
            &store,
            "task-live",
            "run-live",
            &[Status::Ready, Status::Running],
            at,
            None,
            &[],
        );
        seed_run(
            &store,
            "task-checking",
            "run-checking",
            &[Status::Ready, Status::Running, Status::Validating],
            at,
            None,
            &[],
        );
        seed_run(
            &store,
            "task-done",
            "run-done",
            &DONE,
            at,
            Some(Outcome::Succeeded),
            &[],
        );
        seed_run(
            &store,
            "task-failed",
            "run-failed",
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::FailedValidation,
            ],
            at,
            Some(Outcome::ValidationFailed),
            &[],
        );
    }
    let commands = |run: &str| -> Vec<(String, Value)> {
        project
            .store()
            .pending_commands(run)
            .unwrap()
            .into_iter()
            .map(|c| (c.kind, Value::Object(c.payload)))
            .collect()
    };
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "steered", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let say = |bearer: &str, run: &str, message: &str| {
        http_as(
            &server,
            bearer,
            "POST",
            "/api/flywheel/steer/say",
            Some(json!({ "folder": root, "run": run, "message": message })),
        )
    };
    let interrupt = |bearer: &str, run: &str| {
        http_as(
            &server,
            bearer,
            "POST",
            "/api/flywheel/steer/interrupt",
            Some(json!({ "folder": root, "run": run })),
        )
    };
    let token = server.token.clone();

    // A running run: exactly one say carrying exactly the text.
    let (code, steered) = say(&token, "run-live", "please add docstrings");
    assert_eq!(code, 201, "{steered}");
    assert_eq!(steered["kind"], "say");
    assert_eq!(steered["task_id"], "task-live");
    assert_eq!(
        commands("run-live"),
        vec![("say".to_owned(), json!({"text": "please add docstrings"}))]
    );
    // A validating run: exactly one interrupt.
    let (code, steered) = interrupt(&token, "run-checking");
    assert_eq!(code, 201, "{steered}");
    assert_eq!(steered["kind"], "interrupt");
    assert_eq!(
        commands("run-checking"),
        vec![("interrupt".to_owned(), json!({}))]
    );

    // Done, failed, and unknown runs are refused; nothing is enqueued.
    for run in ["run-done", "run-failed"] {
        let (code, body) = say(&token, run, "hello");
        assert_eq!(code, 409, "{run}: {body}");
        let (code, body) = interrupt(&token, run);
        assert_eq!(code, 409, "{run}: {body}");
        assert_eq!(commands(run), vec![], "{run}");
    }
    let (code, _) = interrupt(&token, "run-nope");
    assert_eq!(code, 404);
    assert_eq!(commands("run-nope"), vec![]);

    // An empty message is refused.
    for empty in ["", "   "] {
        let (code, _) = say(&token, "run-live", empty);
        assert_eq!(code, 400);
    }
    assert_eq!(commands("run-live").len(), 1);

    // A peer credential is refused on both routes, enqueueing nothing.
    let (code, issued) = http(
        &server,
        "POST",
        "/api/peers",
        Some(json!({ "name": "hub-a" })),
    );
    assert_eq!(code, 201, "{issued}");
    let peer = issued["credential"].as_str().unwrap().to_owned();
    let (code, _) = say(&peer, "run-live", "from a peer");
    assert_eq!(code, 403);
    let (code, _) = interrupt(&peer, "run-live");
    assert_eq!(code, 403);
    assert_eq!(commands("run-live").len(), 1);
    assert_eq!(commands("run-checking").len(), 1);
}

/// The names under `dir` (sorted; none when it does not exist).
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn create_task_plans_then_writes_one_loader_valid_file_under_the_home_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let root = dir.join("planned");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README.md"), "planned\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let project = adopt(&fw_home(&dir), &root);
    let tasks = project.tasks();
    write_task(&tasks, "01-core", "alpha", &[]);
    let archived = tasks.join("archive/00-done/old.json");
    std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
    std::fs::write(
        &archived,
        r#"{"id":"old","goal":"Done long ago.","graders":[{"type":"command","run":"true"}]}"#,
    )
    .unwrap();
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "planned", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let body = |id: &str| {
        json!({
            "folder": root,
            "task_id": id,
            "goal": format!("Ship {id} with a passing unit check."),
            "phase": "01-core",
            "prerequisites": ["alpha", "old"],
            "checks": [
                {"run": "cargo test -p beta", "name": "unit"},
                {"run": "test -f README.md"},
            ],
        })
    };
    let with = |mut value: Value, extra: Value| {
        for (key, v) in extra.as_object().unwrap() {
            value[key] = v.clone();
        }
        value
    };
    let post = |value: Value| http(&server, "POST", "/api/flywheel/tasks", Some(value));
    let core = tasks.join("active/01-core");
    let before = listing(&core);
    let repo_clean = || {
        let out = Command::new("git")
            .args(["status", "--porcelain", "--ignored"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "repo tree changed"
        );
    };

    // The dry run names the file and its bytes, writes nothing.
    let (code, planned) = post(with(body("beta"), json!({"dry_run": true})));
    assert_eq!(code, 200, "{planned}");
    let path = core.join("beta.json");
    assert_eq!(planned["plan"]["path"], json!(path));
    assert_eq!(planned["plan"]["new_phase"], false);
    let fingerprint = planned["fingerprint"].as_str().unwrap().to_owned();
    assert_eq!(listing(&core), before);

    // No confirmation, a stale one, or one whose plan no longer matches:
    // refused, nothing written.
    let (code, _) = post(body("beta"));
    assert_eq!(code, 400);
    let (code, _) = post(with(body("beta"), json!({"confirm": "0000000000000000"})));
    assert_eq!(code, 409);
    let changed = with(
        body("beta"),
        json!({"goal": "Something else entirely.", "confirm": fingerprint}),
    );
    let (code, refused) = post(changed);
    assert_eq!(code, 409, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("review it again"),
        "{refused}"
    );
    assert_eq!(listing(&core), before);

    // Confirmed: exactly one file, the loader's task with those values.
    let (code, written) = post(with(body("beta"), json!({"confirm": fingerprint})));
    assert_eq!(code, 201, "{written}");
    assert_eq!(written["written"]["path"], json!(path));
    let mut after = before.clone();
    after.push("beta.json".into());
    after.sort();
    assert_eq!(listing(&core), after);
    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, planned["plan"]["contents"].as_str().unwrap());
    let task = flywheel_core::loaders::load_task_file(&path).unwrap();
    assert_eq!(task.id, "beta");
    assert_eq!(task.goal, "Ship beta with a passing unit check.");
    let checks: Vec<(String, Option<String>)> = task
        .graders
        .iter()
        .map(|g| match g {
            flywheel_core::task::Grader::Command(c) => (c.run.clone(), c.name.clone()),
            other => panic!("not a command check: {other:?}"),
        })
        .collect();
    assert_eq!(
        checks,
        vec![
            ("cargo test -p beta".to_owned(), Some("unit".to_owned())),
            ("test -f README.md".to_owned(), None),
        ]
    );
    assert_eq!(
        flywheel_orchestrator::sources::read_prerequisites(&path),
        ["alpha", "old"]
    );
    repo_clean();

    // Invalid input: refused with the problem named, nothing written.
    let after = listing(&core);
    let phases = listing(&tasks.join("active"));
    let refusals = [
        (with(body("gamma"), json!({"goal": "  "})), "goal"),
        (body("Has Space"), "task id"),
        (with(body("alpha"), json!({"prerequisites": []})), "already"),
        (with(body("old"), json!({"prerequisites": []})), "already"),
        (body("beta"), "already"),
        (
            with(body("gamma"), json!({"prerequisites": ["alpha", "nope"]})),
            "unknown prerequisite nope",
        ),
        (
            with(
                body("gamma"),
                json!({"checks": [{"run": "sh oracle.sh", "held_out": true}]}),
            ),
            "held-out",
        ),
        (with(body("gamma"), json!({"checks": []})), "check"),
        (
            with(body("gamma"), json!({"phase": "02_New"})),
            "kebab-case",
        ),
    ];
    for (request, named) in refusals {
        for step in [json!({"dry_run": true}), json!({"confirm": fingerprint})] {
            let (code, refused) = post(with(request.clone(), step));
            assert_eq!(code, 400, "{request}: {refused}");
            let error = refused["error"].as_str().unwrap();
            assert!(error.contains(named), "{named:?} not in {error:?}");
        }
    }
    assert_eq!(listing(&core), after);
    assert_eq!(listing(&tasks.join("active")), phases);

    // A brand-new kebab-case phase is accepted.
    let fresh = with(body("gamma"), json!({"phase": "02-extra"}));
    let (code, planned) = post(with(fresh.clone(), json!({"dry_run": true})));
    assert_eq!(code, 200, "{planned}");
    assert_eq!(planned["plan"]["new_phase"], true);
    let fingerprint = planned["fingerprint"].as_str().unwrap().to_owned();
    let (code, written) = post(with(fresh, json!({"confirm": fingerprint})));
    assert_eq!(code, 201, "{written}");
    assert!(tasks.join("active/02-extra/gamma.json").is_file());
    repo_clean();

    // A peer credential is refused, writing nothing.
    let (code, issued) = http(
        &server,
        "POST",
        "/api/peers",
        Some(json!({ "name": "hub-a" })),
    );
    assert_eq!(code, 201, "{issued}");
    let peer = issued["credential"].as_str().unwrap().to_owned();
    let (code, _) = http_as(
        &server,
        &peer,
        "POST",
        "/api/flywheel/tasks",
        Some(with(body("delta"), json!({"dry_run": true}))),
    );
    assert_eq!(code, 403);
    assert!(!core.join("delta.json").exists());
}
