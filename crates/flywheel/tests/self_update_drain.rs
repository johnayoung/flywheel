//! `fw self-update` discovery and drain, end to end through the real
//! binary. Every process here -- hubs, hub worker sessions, terminal
//! workers, a decoy worker of a second home, a console under a pty -- is
//! spawned by the test from the fixture's install dir, inside a disposable
//! `HOME` / `FLYWHEEL_HOME` (see `support`), and only those processes are
//! ever signalled.

#![cfg(target_os = "linux")]

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use support::{
    Fixture, Hub, Spawned, argv_of, confine, descendants_of, gone, new_session, project_repo,
    sha256, signal, stat_of, wait_until, worker_under, write_script,
};

const SETTLE: Duration = Duration::from_secs(60);

/// The fake agent: answers `--version`, then records its pid and blocks
/// in its process group until signalled. On SIGTERM it hashes the
/// installed `fw` into `term.log` before exiting.
fn fake_claude(fx: &Fixture) -> PathBuf {
    let bin = fx.root.join("agent-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\n\
             trap 'sha256sum \"{fw}\" | cut -c1-64 >> \"{term}\"; exit 143' TERM\n\
             echo $$ >> \"{agents}\"\n\
             sleep 300 &\n\
             wait\n",
            fw = fx.install.join("fw").display(),
            term = fx.control.join("term.log").display(),
            agents = fx.control.join("agents.pids").display(),
        ),
    );
    bin
}

/// `install/fw worker` in `cwd` for the home at `flywheel_home`, as the
/// leader of its own session (a terminal's foreground job).
fn spawn_terminal_worker(
    fx: &Fixture,
    spawned: &mut Spawned,
    cwd: &Path,
    home: &Path,
    flywheel_home: &Path,
    log: &str,
) -> u32 {
    let log = std::fs::File::create(fx.control.join(log)).unwrap();
    let mut command = Command::new(fx.install.join("fw"));
    command
        .args(["worker", "--poll-interval", "1"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    confine(&mut command, home, flywheel_home, &[]);
    new_session(&mut command);
    let pid = spawned.push(command.spawn().unwrap());
    // Wait for it to be the worker (exec done, still running).
    wait_until("the terminal worker to start", SETTLE, || {
        let argv = argv_of(pid);
        (argv.get(1).map(String::as_str) == Some("worker")).then_some(())
    });
    pid
}

/// A second disposable home with its own project repo and a worker of it
/// started from the same install dir: `(pid, starttime)`.
fn spawn_decoy(fx: &Fixture, spawned: &mut Spawned) -> (u32, u64) {
    let home = fx.root.join("decoy-home");
    let flywheel_home = fx.root.join("decoy-flywheel-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&flywheel_home).unwrap();
    let repo = fx.root.join("decoy-repo");
    project_repo(&repo, &home, &flywheel_home);
    let pid = spawn_terminal_worker(fx, spawned, &repo, &home, &flywheel_home, "decoy.log");
    assert_eq!(argv_of(pid)[0], fx.install.join("fw").display().to_string());
    let (_, start, _) = stat_of(pid).unwrap();
    (pid, start)
}

fn alive_as(pid: u32, start: u64) -> bool {
    stat_of(pid).is_some_and(|(_, s, state)| s == start && state != 'Z')
}

/// Every number in `value` equal to `pid`, and every string naming it as
/// a whole token.
fn mentions(value: &Value, pid: u32) -> bool {
    match value {
        Value::Number(n) => n.as_u64() == Some(u64::from(pid)),
        Value::String(s) => s
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == pid.to_string()),
        Value::Array(items) => items.iter().any(|v| mentions(v, pid)),
        Value::Object(map) => map.values().any(|v| mentions(v, pid)),
        _ => false,
    }
}

fn pids(entries: &Value) -> Vec<u64> {
    let mut out: Vec<u64> = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["pid"].as_u64().unwrap())
        .collect();
    out.sort_unstable();
    out
}

fn entry(entries: &Value, pid: u32) -> &Value {
    entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["pid"] == u64::from(pid))
        .unwrap_or_else(|| panic!("pid {pid} not in {entries}"))
}

fn lifecycle_statuses(project: &Path) -> Vec<(String, String)> {
    let db = rusqlite::Connection::open(project.join("flywheel.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    let mut stmt = db
        .prepare("SELECT task_id, status FROM lifecycles ORDER BY task_id")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn claims(project: &Path) -> Vec<(String, String)> {
    let db = rusqlite::Connection::open(project.join("flywheel.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    let mut stmt = db
        .prepare("SELECT task_id, worker_id FROM task_claims ORDER BY task_id")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn write_task(project: &Path, id: &str) {
    let phase = project.join("tasks/active/01-drain");
    std::fs::create_dir_all(&phase).unwrap();
    std::fs::write(
        phase.join(format!("{id}.json")),
        format!(
            r#"{{"id": "{id}", "goal": "Goal {id}.", "graders": [{{"type": "command", "run": "true"}}]}}"#
        ),
    )
    .unwrap();
}

/// Criterion #8 (drain half) and #14: the dry run lists exactly this
/// home's hub worker and terminal worker, plus the hub pid, signals
/// nothing, and never names the decoy of another home.
#[test]
fn dry_run_lists_exactly_the_drain_set_and_the_hub_pid() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let agent_bin = fake_claude(&fx);

    let hub_repo = fx.root.join("hub-repo");
    project_repo(&hub_repo, &fx.home, &fx.flywheel_home);
    let terminal_repo = fx.root.join("terminal-repo");
    let terminal_project = project_repo(&terminal_repo, &fx.home, &fx.flywheel_home);
    let terminal_project_id = terminal_project
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let hub = Hub::start(
        &mut spawned,
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agent_bin],
    );
    let (_sid, wrapper) = hub.start_worker(&hub_repo);
    let hub_worker = worker_under(wrapper);
    let terminal = spawn_terminal_worker(
        &fx,
        &mut spawned,
        &terminal_repo,
        &fx.home,
        &fx.flywheel_home,
        "terminal.log",
    );
    let (decoy, decoy_start) = spawn_decoy(&fx, &mut spawned);
    let watched: Vec<(u32, u64)> = [hub.pid, wrapper, hub_worker, terminal]
        .iter()
        .map(|pid| (*pid, stat_of(*pid).unwrap().1))
        .collect();
    let install_before = fx.install_manifest();

    let run = fx.run(&fx.checkout, &["--dry-run", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "planned");
    let statuses = run.json_statuses();
    assert_eq!(statuses[2], ("drain".to_owned(), "planned".to_owned()));

    let drained = &summary["drained"];
    let mut expected = vec![u64::from(hub_worker), u64::from(terminal)];
    expected.sort_unstable();
    assert_eq!(pids(drained), expected, "{run:?}");
    let hub_entry = entry(drained, hub_worker);
    assert_eq!(hub_entry["kind"], "hub-worker");
    assert_eq!(hub_entry["verb"], "worker");
    assert_eq!(hub_entry["cwd"], hub_repo.display().to_string().as_str());
    let terminal_entry = entry(drained, terminal);
    assert_eq!(terminal_entry["kind"], "terminal");
    assert_eq!(terminal_entry["verb"], "worker");
    assert_eq!(
        terminal_entry["cwd"],
        terminal_repo.display().to_string().as_str()
    );
    assert_eq!(terminal_entry["project_id"], terminal_project_id.as_str());
    for key in ["pid", "kind", "verb", "cwd", "project_id"] {
        assert!(hub_entry.get(key).is_some(), "{key} missing: {hub_entry}");
    }
    assert_eq!(summary["hub"]["old_pid"], u64::from(hub.pid), "{run:?}");

    // Nothing was signalled or installed, and the decoy is never named.
    for (pid, start) in &watched {
        assert!(alive_as(*pid, *start), "pid {pid} changed after a dry run");
    }
    assert!(alive_as(decoy, decoy_start), "the decoy was touched");
    for line in run.json_lines() {
        assert!(!mentions(&line, decoy), "the decoy appears in {line}");
    }
    assert_eq!(fx.install_manifest(), install_before);
    assert!(!fx.flywheel_home.join("self-update/previous").exists());
    assert!(!fx.flywheel_home.join("self-update/state.json").exists());
}

/// Criterion #14 on a real update: the decoy worker of a second home,
/// started from the same install dir, keeps running with the same pid and
/// appears nowhere; this home's terminal worker is drained.
#[test]
fn a_worker_of_another_home_is_never_drained() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let repo = fx.root.join("repo");
    let project = project_repo(&repo, &fx.home, &fx.flywheel_home);
    let terminal = spawn_terminal_worker(
        &fx,
        &mut spawned,
        &repo,
        &fx.home,
        &fx.flywheel_home,
        "terminal.log",
    );
    let (decoy, decoy_start) = spawn_decoy(&fx, &mut spawned);

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated");
    let statuses = run.json_statuses();
    assert_eq!(statuses[2], ("drain".to_owned(), "ok".to_owned()));
    assert_eq!(statuses[4], ("install".to_owned(), "ok".to_owned()));
    assert_eq!(pids(&summary["drained"]), vec![u64::from(terminal)]);
    let drained = entry(&summary["drained"], terminal);
    assert_eq!(drained["kind"], "terminal");
    assert_eq!(drained["verb"], "worker");
    assert_eq!(drained["cwd"], repo.display().to_string().as_str());
    assert_eq!(
        drained["project_id"],
        project.file_name().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(summary["not_stopped"], serde_json::json!([]));
    assert!(gone(terminal), "the terminal worker still runs");

    assert!(alive_as(decoy, decoy_start), "the decoy was stopped");
    for line in run.json_lines() {
        assert!(!mentions(&line, decoy), "the decoy appears in {line}");
    }

    // Human output for the same world (a fresh terminal worker) names the
    // drained pid and never the decoy.
    let again = spawn_terminal_worker(
        &fx,
        &mut spawned,
        &repo,
        &fx.home,
        &fx.flywheel_home,
        "terminal-2.log",
    );
    let run = fx.run(&fx.checkout, &["--force"]);
    assert_eq!(run.code, 0, "{run:?}");
    let (steps, outcome) = run.human_statuses();
    assert_eq!(outcome, "updated");
    assert_eq!(steps[2], ("drain".to_owned(), "ok".to_owned()));
    assert!(run.stdout.contains(&again.to_string()), "{run:?}");
    assert!(
        !run.stdout
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == decoy.to_string()),
        "{run:?}"
    );
    assert!(alive_as(decoy, decoy_start));
}

/// Criterion #9: a hub worker mid-turn is drained before the install --
/// its agent sees SIGTERM while the installed `fw` is still the
/// pre-update bytes, the run ends `interrupted`, and the claim is gone.
#[test]
fn a_mid_turn_hub_worker_is_interrupted_before_the_install() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let agent_bin = fake_claude(&fx);
    let repo = fx.root.join("repo");
    let project = project_repo(&repo, &fx.home, &fx.flywheel_home);
    write_task(&project, "mid-turn");
    // The hub is handed off and the worker restarted by the run: stop
    // whatever serves this home afterwards, and the agents it started.
    let agents = fx.control.join("agents.pids");
    let mut cleanup = support::HubCleanup::new(
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agents],
    );

    let hub = Hub::start(
        &mut spawned,
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agent_bin],
    );
    let (_sid, wrapper) = hub.start_worker(&repo);
    let worker = worker_under(wrapper);
    wait_until("the agent mid-turn with a claim", SETTLE, || {
        let agents = std::fs::read_to_string(fx.control.join("agents.pids")).unwrap_or_default();
        (!agents.trim().is_empty() && !claims(&project).is_empty()).then_some(())
    });
    for pid in descendants_of(wrapper) {
        spawned.track(pid);
    }
    assert_eq!(
        lifecycle_statuses(&project),
        vec![("mid-turn".to_owned(), "running".to_owned())]
    );
    let run_before = run_of(&project, "mid-turn");
    let held = claims(&project);
    assert_eq!(held.len(), 1, "{held:?}");
    let drained_worker_id = held[0].1.clone();
    assert_eq!(transitions(&project, &run_before), ["ready", "running"]);
    let pre_update = sha256(&fx.install.join("fw"));

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated");
    assert_eq!(pids(&summary["drained"]), vec![u64::from(worker)]);
    assert_eq!(entry(&summary["drained"], worker)["kind"], "hub-worker");
    assert!(gone(worker), "the hub worker still runs");

    // The agent got SIGTERM while the old bytes were installed.
    let term = std::fs::read_to_string(fx.control.join("term.log")).unwrap_or_default();
    let hashes: Vec<&str> = term.lines().collect();
    // The worker may signal the agent's group more than once; every
    // delivery must see the pre-update bytes.
    assert!(!hashes.is_empty(), "the agent never saw SIGTERM: {run:?}");
    assert!(
        hashes.iter().all(|h| *h == pre_update),
        "{hashes:?} vs {pre_update}"
    );
    assert_ne!(sha256(&fx.install.join("fw")), pre_update);
    // The drained run ended `interrupted` and the drained worker's claim
    // is gone. The hub worker restarted after the handoff resumes the
    // same run under a new worker id, so the evidence is the run's own
    // ledger and the drained worker's id, not the latest row.
    let ledger = transitions(&project, &run_before);
    assert!(
        ledger.starts_with(&["ready".into(), "running".into(), "interrupted".into()])
            && ledger[3..]
                .iter()
                .zip(["ready", "running"])
                .all(|(got, want)| got == want),
        "the drained run did not end interrupted (then, at most, resume): {ledger:?}"
    );
    let claims_after = claims(&project);
    assert!(
        claims_after.iter().all(|(_, w)| *w != drained_worker_id),
        "the drained worker's claim survives: {claims_after:?}"
    );
    assert_eq!(summary["unidentified_claims"], serde_json::json!([]));
    // Only the worker was signalled: its session (the wrapper) is the
    // same process, now served by the hub it was handed off to.
    assert_eq!(summary["hub"]["action"], "handoff", "{summary}");
    assert_eq!(summary["hub"]["old_pid"], u64::from(hub.pid));
    let new_hub = support::server_pid(&fx.flywheel_home).expect("no hub serves");
    cleanup.track(new_hub);
    assert!(!gone(new_hub));
    assert!(!gone(wrapper), "the worker's session was ended");
}

/// The `transitioned_to` targets run `run_id` recorded, in order.
fn transitions(project: &Path, run_id: &str) -> Vec<String> {
    let db = rusqlite::Connection::open(project.join("flywheel.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT payload_json FROM events WHERE run_id = ?1 AND kind = 'transitioned_to' \
             ORDER BY sequence",
        )
        .unwrap();
    stmt.query_map([run_id], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| {
            let payload: Value = serde_json::from_str(&payload.unwrap()).unwrap();
            payload["target"].as_str().unwrap().to_owned()
        })
        .collect()
}

/// The run id of task `task_id`'s lifecycle.
fn run_of(project: &Path, task_id: &str) -> String {
    let db = rusqlite::Connection::open(project.join("flywheel.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    db.query_row(
        "SELECT run_id FROM lifecycles WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )
    .unwrap()
}

/// Criterion #15: an operator console of this home, under a pty, refuses
/// the run with exit 3 naming its pid; cargo never runs.
#[test]
fn a_console_of_this_home_refuses_the_run() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let repo = fx.root.join("repo");
    project_repo(&repo, &fx.home, &fx.flywheel_home);
    let program = fx.install.join("fw");
    let mut command = Command::new("script");
    command
        .args(["-q", "-c"])
        .arg(program.display().to_string())
        .arg("/dev/null")
        .current_dir(&repo)
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    confine(&mut command, &fx.home, &fx.flywheel_home, &[]);
    new_session(&mut command);
    let script = command.spawn().unwrap();
    let script_pid = spawned.push(script);
    let console = wait_until("the console under the pty", SETTLE, || {
        descendants_of(script_pid).into_iter().find(|pid| {
            let argv = argv_of(*pid);
            argv.len() == 1 && argv[0] == program.display().to_string()
        })
    });
    spawned.track(console);
    let (_, console_start, _) = stat_of(console).unwrap();
    let install_before = fx.install_manifest();

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 3, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "refused");
    let statuses = run.json_statuses();
    assert_eq!(statuses[0], ("preflight".to_owned(), "failed".to_owned()));
    assert!(
        statuses[1..].iter().all(|(_, s)| s == "skipped"),
        "{statuses:?}"
    );
    let detail = run.json_lines()[0]["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains(&console.to_string()), "{detail}");
    assert!(fx.cargo_log().is_empty(), "cargo ran");
    assert_eq!(fx.install_manifest(), install_before);
    assert!(!fx.flywheel_home.join("self-update/previous").exists());
    assert!(
        alive_as(console, console_start),
        "the console was signalled"
    );

    // Human output names the pid too.
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 3, "{run:?}");
    assert!(run.stdout.contains(&console.to_string()), "{run:?}");
}

/// Criterion #16 (drain half): a SIGSTOPped worker survives SIGTERM; with
/// `--drain-timeout 5` the run fails (exit 1) in bounded time, names it
/// under `not_stopped`, and leaves the install dir byte-identical.
#[test]
fn a_stopped_worker_at_the_drain_timeout_fails_without_installing() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let repo_a = fx.root.join("repo-a");
    project_repo(&repo_a, &fx.home, &fx.flywheel_home);
    let repo_b = fx.root.join("repo-b");
    project_repo(&repo_b, &fx.home, &fx.flywheel_home);
    let frozen = spawn_terminal_worker(
        &fx,
        &mut spawned,
        &repo_a,
        &fx.home,
        &fx.flywheel_home,
        "frozen.log",
    );
    let other = spawn_terminal_worker(
        &fx,
        &mut spawned,
        &repo_b,
        &fx.home,
        &fx.flywheel_home,
        "other.log",
    );
    signal(frozen, libc::SIGSTOP);
    wait_until("the worker to stop", SETTLE, || {
        stat_of(frozen)
            .is_some_and(|(_, _, state)| state == 'T')
            .then_some(())
    });
    let install_before = fx.install_manifest();

    let started = Instant::now();
    let run = fx.run(&fx.checkout, &["--json", "--drain-timeout", "5"]);
    let elapsed = started.elapsed();
    assert_eq!(run.code, 1, "{run:?}");
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(60),
        "took {elapsed:?}"
    );
    let summary = run.summary();
    assert_eq!(summary["outcome"], "failed");
    let statuses = run.json_statuses();
    assert_eq!(statuses[2], ("drain".to_owned(), "failed".to_owned()));
    assert!(
        statuses[3..].iter().all(|(_, s)| s == "skipped"),
        "{statuses:?}"
    );
    assert_eq!(pids(&summary["not_stopped"]), vec![u64::from(frozen)]);
    let survivor = entry(&summary["not_stopped"], frozen);
    assert_eq!(survivor["kind"], "terminal");
    assert_eq!(survivor["cwd"], repo_a.display().to_string().as_str());
    assert_eq!(pids(&summary["drained"]), vec![u64::from(other)]);
    assert!(gone(other));
    assert!(!gone(frozen), "the stopped worker was killed");
    assert_eq!(
        fx.install_manifest(),
        install_before,
        "the install dir changed"
    );
    assert!(!fx.flywheel_home.join("self-update/previous").exists());
    assert!(!fx.flywheel_home.join("self-update/state.json").exists());

    // Human output carries the not-stopped line.
    let run = fx.run(&fx.checkout, &["--drain-timeout", "1"]);
    assert_eq!(run.code, 1, "{run:?}");
    let line = format!("not-stopped: {frozen} terminal {}", repo_a.display());
    assert!(run.stdout.lines().any(|l| l == line), "{run:?}");
    assert_eq!(fx.install_manifest(), install_before);

    // Resumed, it honours the pending SIGTERM and exits on its own.
    signal(frozen, libc::SIGCONT);
    wait_until("the resumed worker to exit", SETTLE, || {
        gone(frozen).then_some(())
    });
}

/// A live lease nobody drained (a SIGKILLed worker's lingering claim) is
/// reported as an unidentified claim and does not block the update.
#[test]
fn a_lingering_lease_is_reported_not_blocking() {
    let fx = Fixture::new();
    let repo = fx.root.join("repo");
    let project = project_repo(&repo, &fx.home, &fx.flywheel_home);
    let project_id = project.file_name().unwrap().to_string_lossy().into_owned();
    let store = flywheel_orchestrator::claims::SqliteClaimStore::open_partition(
        project.join("flywheel.sqlite"),
        &project_id,
    )
    .unwrap();
    use flywheel_orchestrator::claims::ClaimStore;
    let claim = store
        .acquire_claim(
            "orphaned-task",
            "worker-deadbeef",
            chrono::Utc::now(),
            3600.0,
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
    assert!(claim.is_some());
    drop(store);

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated");
    let claims = summary["unidentified_claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1, "{summary}");
    assert_eq!(claims[0]["project_id"], project_id.as_str());
    assert_eq!(claims[0]["task_id"], "orphaned-task");
    assert_eq!(claims[0]["worker_id"], "worker-deadbeef");

    let run = fx.run(&fx.checkout, &["--force"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert!(
        run.stdout.lines().any(|l| l.starts_with(&format!(
            "unidentified-claim: {project_id} orphaned-task worker-deadbeef "
        ))),
        "{run:?}"
    );
}
