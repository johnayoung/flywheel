//! `fw self-update`'s hub and restart steps, end to end through the real
//! binary: a serving hub is handed off to the installed `fw` with its
//! sessions intact; drained hub workers and autopilots come back through
//! the hub API on the new bytes; terminal workers get an exact `manual`
//! command and their credentials are named, never printed; a refused
//! handoff exits 4; a drain timeout restarts the stopped hub workers on
//! the previous binary.
//!
//! Every process here is spawned by the test (or by the self-update run
//! it starts) inside a disposable `HOME` / `FLYWHEEL_HOME` with every
//! inherited `FLYWHEEL_*` dropped and an explicit `--install-dir` (see
//! `support`); only those processes are ever signalled.

#![cfg(target_os = "linux")]

mod support;

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use support::{
    Fixture, Hub, HubCleanup, Spawned, argv_of, confine, descendants_of, gone, new_session,
    project_repo, server_pid, sha256, signal, stat_of, wait_until, worker_under, write_script,
};

const SETTLE: Duration = Duration::from_secs(60);

/// The fake agent: answers `--version`, records its pid, then blocks
/// (bounded) until signalled.
fn fake_claude(fx: &Fixture) -> PathBuf {
    let bin = fx.root.join("agent-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\n\
             trap 'exit 143' TERM\n\
             echo $$ >> \"{agents}\"\n\
             sleep 240 &\n\
             wait\n",
            agents = fx.control.join("agents.pids").display(),
        ),
    );
    bin
}

fn write_task(project: &Path, id: &str) {
    let phase = project.join("tasks/active/01-hub");
    std::fs::create_dir_all(&phase).unwrap();
    std::fs::write(
        phase.join(format!("{id}.json")),
        format!(
            r#"{{"id": "{id}", "goal": "Goal {id}.", "graders": [{{"type": "command", "run": "true"}}]}}"#
        ),
    )
    .unwrap();
}

/// The sha256 of the image `pid` runs (read through `/proc/<pid>/exe`,
/// so a binary replaced since it started still hashes as what it runs).
fn exe_sha(pid: u32) -> String {
    sha256(Path::new(&format!("/proc/{pid}/exe")))
}

fn cwd_of(pid: u32) -> PathBuf {
    std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap()
}

/// `/proc/<pid>/cmdline` with empty arguments kept.
fn cmdline(pid: u32) -> Vec<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    bytes
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
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

/// Every session object (`{"id", "command", ...}`) in an API answer.
fn sessions_in(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("id").is_some_and(Value::is_string) && map.contains_key("command") {
                out.push(value.clone());
            }
            map.values().for_each(|v| sessions_in(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| sessions_in(v, out)),
        _ => {}
    }
}

/// session id -> the session view, as `GET /api/sessions` lists them.
fn sessions(hub: &Hub) -> BTreeMap<String, Value> {
    let (code, listed) = hub.http("GET", "/api/sessions", None);
    assert_eq!(code, 200, "{listed}");
    let mut found = Vec::new();
    sessions_in(&listed, &mut found);
    found
        .into_iter()
        .map(|s| (s["id"].as_str().unwrap().to_owned(), s))
        .collect()
}

fn workspace(hub: &Hub, name: &str, folders: &[&Path]) -> String {
    let folders: Vec<Value> = folders.iter().map(|f| json!({ "path": f })).collect();
    let (code, created) = hub.http(
        "POST",
        "/api/workspaces",
        Some(json!({ "name": name, "folders": folders })),
    );
    assert_eq!(code, 201, "{created}");
    created["id"].as_str().unwrap().to_owned()
}

/// The wrapper pid of session `sid` once it runs.
fn session_pid(hub: &Hub, sid: &str) -> u32 {
    wait_until("the session's process", SETTLE, || {
        let (_, view) = hub.http("GET", &format!("/api/sessions/{sid}"), None);
        view["live"]["pid"].as_u64().map(|p| p as u32)
    })
}

/// The `fw <verb>` child of a session wrapper.
fn daemon_under(wrapper: u32, verb: &str) -> u32 {
    wait_until(&format!("fw {verb} under {wrapper}"), SETTLE, || {
        descendants_of(wrapper)
            .into_iter()
            .find(|pid| argv_of(*pid).get(1).map(String::as_str) == Some(verb))
    })
}

fn screen(hub: &Hub, sid: &str) -> String {
    let (code, screen) = hub.http(
        "GET",
        &format!("/api/sessions/{sid}/screen?source=recent"),
        None,
    );
    assert_eq!(code, 200, "{screen}");
    screen.to_string()
}

fn random_hex(len: usize) -> String {
    let mut out = String::new();
    while out.len() < len {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(len);
    out
}

/// Criteria #10 and #22: the serving hub is handed off to the installed
/// `fw` (new pid, new bytes) with every session's process and screen
/// kept; the drained hub worker comes back in the same folder with the
/// same `--task` / `--with-prereqs` scope and the drained hub autopilot
/// comes back too, both running the newly installed bytes.
#[test]
fn a_handoff_keeps_sessions_and_restarts_hub_daemons_on_the_new_fw() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let agent_bin = fake_claude(&fx);
    let agents = fx.control.join("agents.pids");
    let mut cleanup = HubCleanup::new(
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agents],
    );
    let repo = fx.root.join("repo");
    let project = project_repo(&repo, &fx.home, &fx.flywheel_home);
    write_task(&project, "alpha");
    write_task(&project, "beta");
    let pilot_repo = fx.root.join("pilot-repo");
    project_repo(&pilot_repo, &fx.home, &fx.flywheel_home);

    let hub = Hub::start(
        &mut spawned,
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agent_bin],
    );
    let ws = workspace(&hub, "hub", &[&repo, &pilot_repo]);

    // A shell session with a marker on its screen.
    let marker = format!("marker-{}", random_hex(16));
    let (code, shell) = hub.http(
        "POST",
        "/api/sessions",
        Some(json!({
            "workspace_id": ws,
            "folder": repo,
            "command": ["/bin/sh", "-c", format!("echo {marker}; exec sleep 600")],
        })),
    );
    assert_eq!(code, 201, "{shell}");
    let shell_id = shell["id"].as_str().unwrap().to_owned();
    let shell_pid = session_pid(&hub, &shell_id);
    cleanup.track(shell_pid);
    wait_until("the marker on the shell's screen", SETTLE, || {
        screen(&hub, &shell_id).contains(&marker).then_some(())
    });

    // A scoped hub worker, mid-turn on alpha.
    let (code, worker_session) = hub.http(
        "POST",
        "/api/flywheel/worker",
        Some(json!({
            "folder": repo, "workspace_id": ws, "tasks": ["alpha"], "with_prereqs": true,
        })),
    );
    assert_eq!(code, 201, "{worker_session}");
    let worker_sid = worker_session["id"].as_str().unwrap().to_owned();
    let worker_wrapper = session_pid(&hub, &worker_sid);
    let worker = worker_under(worker_wrapper);
    let scope: Vec<String> = cmdline(worker)[2..].to_vec();
    assert_eq!(scope, ["--task=alpha", "--with-prereqs"]);
    wait_until("the agent mid-turn", SETTLE, || {
        let listed = std::fs::read_to_string(&agents).unwrap_or_default();
        (!listed.trim().is_empty()).then_some(())
    });

    // A hub autopilot.
    let (code, pilot_session) = hub.http(
        "POST",
        "/api/flywheel/autopilot/start",
        Some(json!({ "folder": pilot_repo, "workspace_id": ws, "confirm": true })),
    );
    assert_eq!(code, 201, "{pilot_session}");
    let pilot_sid = pilot_session["id"].as_str().unwrap().to_owned();
    let pilot_wrapper = session_pid(&hub, &pilot_sid);
    let pilot = daemon_under(pilot_wrapper, "autopilot");
    for pid in [worker_wrapper, pilot_wrapper] {
        cleanup.track(pid);
    }

    let before = sessions(&hub);
    let live_before: BTreeMap<String, Option<u64>> = before
        .iter()
        .map(|(id, s)| (id.clone(), s["live"]["pid"].as_u64()))
        .collect();
    assert_eq!(server_pid(&fx.flywheel_home), Some(hub.pid));

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated", "{run:?}");
    let statuses = run.json_statuses();
    assert_eq!(statuses[6], ("hub".to_owned(), "ok".to_owned()), "{run:?}");
    assert_eq!(
        statuses[7],
        ("restart".to_owned(), "ok".to_owned()),
        "{run:?}"
    );
    let marker_bytes = fx.last_marker().unwrap();
    let installed = sha256(&fx.install.join("fw"));
    assert!(
        std::fs::read(fx.install.join("fw"))
            .unwrap()
            .ends_with(marker_bytes.as_bytes())
    );
    assert_eq!(
        pids(&summary["drained"]),
        {
            let mut v = vec![u64::from(worker), u64::from(pilot)];
            v.sort_unstable();
            v
        },
        "{summary}"
    );
    assert_eq!(summary["manual"], json!([]), "{summary}");

    // #22: a real handoff -- a new hub process on the installed bytes,
    // every session the same process with the same screen.
    assert!(gone(hub.pid), "the old hub still runs");
    let new_hub = server_pid(&fx.flywheel_home).expect("no hub serves after the handoff");
    cleanup.track(new_hub);
    assert_ne!(new_hub, hub.pid);
    assert_eq!(summary["hub"]["action"], "handoff");
    assert_eq!(summary["hub"]["old_pid"], u64::from(hub.pid));
    assert_eq!(summary["hub"]["new_pid"], u64::from(new_hub));
    assert_eq!(exe_sha(new_hub), installed, "the new hub runs other bytes");
    let after = sessions(&hub);
    for (id, pid) in &live_before {
        let now = after
            .get(id)
            .unwrap_or_else(|| panic!("session {id} is gone"));
        assert_eq!(
            &now["live"]["pid"].as_u64(),
            pid,
            "session {id} changed process"
        );
    }
    assert!(stat_of(shell_pid).is_some() && !gone(shell_pid));
    assert!(
        screen(&hub, &shell_id).contains(&marker),
        "the marker was lost"
    );

    // #10: the worker and autopilot restarted as new sessions on the new
    // bytes, the worker with its scope and folder.
    let restarted = summary["restarted"].as_array().unwrap();
    assert_eq!(restarted.len(), 2, "{summary}");
    let by_kind = |kind: &str| -> &Value {
        restarted
            .iter()
            .find(|r| r["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {summary}"))
    };
    let new_worker_entry = by_kind("hub-worker");
    let new_worker = new_worker_entry["pid"].as_u64().unwrap() as u32;
    cleanup.track(new_worker);
    assert_eq!(
        new_worker_entry["folder"],
        repo.display().to_string().as_str()
    );
    let new_worker_sid = new_worker_entry["session_id"].as_str().unwrap();
    assert!(!before.contains_key(new_worker_sid), "not a new session");
    let new_session = after
        .get(new_worker_sid)
        .unwrap_or_else(|| panic!("{new_worker_sid} is not listed"));
    assert_eq!(new_session["workspace_id"], ws.as_str());
    assert_eq!(new_session["command"][3], "fw-worker");
    assert!(!gone(new_worker), "the restarted worker is not running");
    assert_eq!(
        cmdline(new_worker)[1..],
        ["worker", "--task=alpha", "--with-prereqs"]
    );
    assert_eq!(cwd_of(new_worker), repo);
    assert_eq!(
        exe_sha(new_worker),
        installed,
        "the worker runs other bytes"
    );
    let ancestry = descendants_of(session_pid(&hub, new_worker_sid));
    assert!(ancestry.contains(&new_worker), "not under its session");

    let new_pilot_entry = by_kind("hub-autopilot");
    let new_pilot = new_pilot_entry["pid"].as_u64().unwrap() as u32;
    cleanup.track(new_pilot);
    assert_eq!(
        new_pilot_entry["folder"],
        pilot_repo.display().to_string().as_str()
    );
    let new_pilot_sid = new_pilot_entry["session_id"].as_str().unwrap();
    assert!(!before.contains_key(new_pilot_sid));
    assert!(!gone(new_pilot));
    assert_eq!(cmdline(new_pilot)[1..], ["autopilot"]);
    assert_eq!(cwd_of(new_pilot), pilot_repo);
    assert_eq!(
        exe_sha(new_pilot),
        installed,
        "the autopilot runs other bytes"
    );
    assert!(gone(worker) && gone(pilot), "a drained daemon still runs");
}

/// Criteria #12, #13, #23: with no hub running the hub step is skipped
/// and no hub or `server.json` appears; a terminal worker is stopped, not
/// restarted, and its `manual` command, run verbatim, starts a worker
/// with the same argv in the same directory on the new bytes; the
/// credentials it ran with are named and their bytes appear nowhere.
#[test]
fn without_a_hub_a_terminal_worker_gets_an_exact_manual_command_and_no_secret_leaks() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let repo = fx.root.join("repo with space");
    project_repo(&repo, &fx.home, &fx.flywheel_home);
    let token = format!("sk-ant-oat01-{}", random_hex(48));
    let api_key = format!("sk-ant-api03-{}", random_hex(40));
    let poll = (2 + uuid::Uuid::new_v4().as_u128() % 50).to_string();
    let heartbeat = (3 + uuid::Uuid::new_v4().as_u128() % 70).to_string();

    let log = std::fs::File::create(fx.control.join("terminal.log")).unwrap();
    let mut command = Command::new(fx.install.join("fw"));
    command
        .args([
            "worker",
            "--poll-interval",
            &poll,
            "--heartbeat",
            &heartbeat,
        ])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    confine(&mut command, &fx.home, &fx.flywheel_home, &[]);
    command
        .env("CLAUDE_CODE_OAUTH_TOKEN", &token)
        .env("ANTHROPIC_API_KEY", &api_key);
    new_session(&mut command);
    let terminal = spawned.push(command.spawn().unwrap());
    wait_until("the terminal worker", SETTLE, || {
        (argv_of(terminal).get(1).map(String::as_str) == Some("worker")).then_some(())
    });
    let original = cmdline(terminal);
    let home_before = support::manifest(&fx.flywheel_home, &[]);
    let install_before = fx.install_manifest();

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated", "{run:?}");
    let statuses = run.json_statuses();
    assert_eq!(statuses[6], ("hub".to_owned(), "skipped".to_owned()));
    assert_eq!(statuses[7], ("restart".to_owned(), "skipped".to_owned()));
    assert_eq!(summary["hub"]["action"], "not-running");
    assert_eq!(summary["restarted"], json!([]));
    assert!(gone(terminal), "the terminal worker still runs");

    // #23: no hub was started.
    let hub_dir = fx.flywheel_home.join("hub");
    assert!(!hub_dir.join("server.json").exists());
    let hubs: Vec<u32> = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| {
            let argv = argv_of(*pid);
            argv.iter().any(|a| a == "hub")
                && argv.iter().any(|a| a == "serve")
                && support::of_home(*pid, &fx.flywheel_home)
        })
        .collect();
    assert!(hubs.is_empty(), "a hub of this home runs: {hubs:?}");
    let listing: Vec<String> = std::fs::read_dir(&hub_dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !listing
            .iter()
            .any(|n| n == "server.json" || n == "serve.lock"),
        "{listing:?}"
    );

    // #12: the manual entry names the credentials and reruns the exact
    // argv in the same directory on the new bytes.
    let manual = summary["manual"].as_array().unwrap();
    assert_eq!(manual.len(), 1, "{summary}");
    let entry = &manual[0];
    assert_eq!(entry["pid"], u64::from(terminal));
    assert_eq!(entry["cwd"], repo.display().to_string().as_str());
    let notes = entry["notes"].to_string();
    assert!(
        notes.contains(
            "ran with CLAUDE_CODE_OAUTH_TOKEN from an unknown source; set it before restarting"
        ),
        "{notes}"
    );
    assert!(notes.contains("ANTHROPIC_API_KEY"), "{notes}");
    let manual_command = entry["command"].as_str().unwrap().to_owned();
    assert!(
        manual_command.contains(&fx.install.join("fw").display().to_string()),
        "{manual_command}"
    );

    // #13: the secrets appear in no output and in no file the runs
    // created or changed.
    for secret in [&token, &api_key] {
        for (name, text) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
            assert!(!text.contains(secret.as_str()), "a secret is in {name}");
        }
    }
    let home_after = support::manifest(&fx.flywheel_home, &[]);
    let install_after = fx.install_manifest();
    let changed = |root: &Path,
                   before: &BTreeMap<String, String>,
                   after: &BTreeMap<String, String>|
     -> Vec<PathBuf> {
        after
            .iter()
            .filter(|(path, sha)| before.get(*path) != Some(sha))
            .map(|(path, _)| root.join(path))
            .collect()
    };
    let mut touched = changed(&fx.flywheel_home, &home_before, &home_after);
    touched.extend(changed(&fx.install, &install_before, &install_after));
    assert!(!touched.is_empty(), "the run changed nothing");
    for file in &touched {
        let bytes = std::fs::read(file).unwrap_or_default();
        for secret in [&token, &api_key] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "a secret is in {}",
                file.display()
            );
        }
    }

    // Run the manual command verbatim, from elsewhere.
    let mut rerun = Command::new("/bin/sh");
    rerun
        .args(["-c", &manual_command])
        .current_dir(&fx.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    confine(&mut rerun, &fx.home, &fx.flywheel_home, &[]);
    new_session(&mut rerun);
    let shell = spawned.push(rerun.spawn().unwrap());
    let restarted = wait_until("the manual command's worker", SETTLE, || {
        std::iter::once(shell)
            .chain(descendants_of(shell))
            .find(|pid| argv_of(*pid).get(1).map(String::as_str) == Some("worker"))
    });
    spawned.track(restarted);
    assert_eq!(cmdline(restarted)[1..], original[1..]);
    assert_eq!(
        cmdline(restarted)[0],
        fx.install.join("fw").display().to_string()
    );
    assert_eq!(cwd_of(restarted), repo);
    assert_eq!(exe_sha(restarted), sha256(&fx.install.join("fw")));

    // Human output prints the same kind of command on a `manual:` line:
    // a second run drains the worker the manual command started.
    let human = fx.run(&fx.checkout, &["--force"]);
    assert_eq!(human.code, 0, "{human:?}");
    let line = format!("manual: {restarted} {manual_command}");
    assert!(human.stdout.lines().any(|l| l == line), "{human:?}");
    assert!(gone(restarted));
}

/// The exit 4 path: a hub that cannot hand off (its handoff socket gone,
/// as a hub from before live handoff) is left serving on the previous
/// binary, not reverted; the hub worker is not restarted, and `fw hub
/// upgrade --allow-restart` is the named manual step.
#[test]
fn a_refused_handoff_exits_4_and_leaves_the_hub_to_allow_restart() {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let agent_bin = fake_claude(&fx);
    let agents = fx.control.join("agents.pids");
    let mut cleanup = HubCleanup::new(
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agents],
    );
    let repo = fx.root.join("repo");
    project_repo(&repo, &fx.home, &fx.flywheel_home);
    let hub = Hub::start(
        &mut spawned,
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agent_bin],
    );
    let (sid, wrapper) = hub.start_worker(&repo);
    cleanup.track(wrapper);
    let worker = worker_under(wrapper);
    std::fs::remove_file(fx.flywheel_home.join("hub/handoff.sock")).unwrap();
    let before: BTreeSet<String> = sessions(&hub).into_keys().collect();
    let pre_update = sha256(&fx.install.join("fw"));

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 4, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "updated-hub-pending", "{run:?}");
    let statuses = run.json_statuses();
    assert_eq!(statuses[4], ("install".to_owned(), "ok".to_owned()));
    assert_eq!(statuses[6], ("hub".to_owned(), "failed".to_owned()));
    assert_eq!(statuses[7], ("restart".to_owned(), "skipped".to_owned()));
    let hub_detail = run.json_lines()[6]["detail"].as_str().unwrap().to_owned();
    assert!(hub_detail.contains("--allow-restart"), "{hub_detail}");
    assert_eq!(summary["hub"]["action"], "refused");
    assert_eq!(summary["hub"]["old_pid"], u64::from(hub.pid));

    // Not reverted: the new pair stays installed; the hub carries on.
    assert_ne!(sha256(&fx.install.join("fw")), pre_update);
    assert!(!gone(hub.pid), "the hub was stopped");
    assert_eq!(server_pid(&fx.flywheel_home), Some(hub.pid));
    assert_eq!(exe_sha(hub.pid), pre_update);
    assert!(gone(worker), "the hub worker was not drained");
    assert_eq!(summary["restarted"], json!([]));
    let after: BTreeSet<String> = sessions(&hub).into_keys().collect();
    assert_eq!(after, before, "a session was started");
    assert!(after.contains(&sid));

    let manual = summary["manual"].as_array().unwrap();
    let hub_step = manual
        .iter()
        .find(|m| m["pid"] == u64::from(hub.pid))
        .unwrap_or_else(|| panic!("no hub manual step in {summary}"));
    let command = hub_step["command"].as_str().unwrap();
    assert!(command.contains("hub upgrade"), "{command}");
    assert!(command.contains("--allow-restart"), "{command}");
    let worker_step = manual
        .iter()
        .find(|m| m["pid"] == u64::from(worker))
        .unwrap_or_else(|| panic!("the hub worker is not listed in {summary}"));
    assert_eq!(worker_step["cwd"], repo.display().to_string().as_str());
}

/// Criterion #16 (restart half): one of two hub workers is SIGSTOPped;
/// with `--drain-timeout 5` the run fails (exit 1) without installing,
/// and the other hub worker runs again on the pre-update `fw` through
/// the untouched hub.
#[test]
fn a_drain_timeout_restarts_the_other_hub_worker_on_the_previous_fw() {
    drain_timeout_restarts_the_other_hub_worker(false);
}

/// Criterion #16 with both hub workers in one folder (the second started
/// with `another`): the frozen worker still runs there, so the restart of
/// the stopped one must not be refused as a duplicate worker.
#[test]
fn a_drain_timeout_restarts_the_other_hub_worker_of_the_same_folder() {
    drain_timeout_restarts_the_other_hub_worker(true);
}

fn drain_timeout_restarts_the_other_hub_worker(same_folder: bool) {
    let fx = Fixture::new();
    let mut spawned = Spawned::default();
    let agent_bin = fake_claude(&fx);
    let agents = fx.control.join("agents.pids");
    let mut cleanup = HubCleanup::new(
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agents],
    );
    let repo_a = fx.root.join("repo-a");
    let project_a = project_repo(&repo_a, &fx.home, &fx.flywheel_home);
    let repo_b = fx.root.join("repo-b");
    project_repo(&repo_b, &fx.home, &fx.flywheel_home);
    let hub = Hub::start(
        &mut spawned,
        &fx.install.join("fw"),
        &fx.home,
        &fx.flywheel_home,
        &[&agent_bin],
    );
    let ws = workspace(&hub, "two", &[&repo_a, &repo_b]);
    let start = |folder: &Path, another: bool| {
        let (code, session) = hub.http(
            "POST",
            "/api/flywheel/worker",
            Some(json!({ "folder": folder, "workspace_id": ws, "another": another })),
        );
        assert_eq!(code, 201, "{session}");
        session_pid(&hub, session["id"].as_str().unwrap())
    };
    // The worker that stops runs in `repo_b`, or beside the frozen one.
    let repo_b = if same_folder { repo_a.clone() } else { repo_b };
    let wrapper_a = start(&repo_a, false);
    let wrapper_b = start(&repo_b, same_folder);
    let frozen = worker_under(wrapper_a);
    let other = worker_under(wrapper_b);
    for pid in [wrapper_a, wrapper_b, frozen] {
        cleanup.track(pid);
    }
    // Freeze it between store transactions: a worker stopped holding the
    // store's write lock would make the run refuse up front instead.
    wait_until("the worker frozen outside a transaction", SETTLE, || {
        signal(frozen, libc::SIGSTOP);
        wait_until("the worker to stop", SETTLE, || {
            stat_of(frozen)
                .is_some_and(|(_, _, state)| state == 'T')
                .then_some(())
        });
        let db = project_a.join("flywheel.sqlite");
        let free = !db.exists()
            || rusqlite::Connection::open(&db)
                .and_then(|c| c.execute_batch("BEGIN IMMEDIATE; ROLLBACK;"))
                .is_ok();
        if !free {
            signal(frozen, libc::SIGCONT);
            std::thread::sleep(Duration::from_millis(300));
        }
        free.then_some(())
    });
    let pre_update = sha256(&fx.install.join("fw"));
    let install_before = fx.install_manifest();
    let before: BTreeSet<String> = sessions(&hub).into_keys().collect();

    let run = fx.run(&fx.checkout, &["--json", "--drain-timeout", "5"]);
    assert_eq!(run.code, 1, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "failed");
    let statuses = run.json_statuses();
    assert_eq!(statuses[2], ("drain".to_owned(), "failed".to_owned()));
    assert_eq!(statuses[6], ("hub".to_owned(), "skipped".to_owned()));
    assert_eq!(
        statuses[7],
        ("restart".to_owned(), "ok".to_owned()),
        "{run:?}"
    );
    assert_eq!(pids(&summary["not_stopped"]), vec![u64::from(frozen)]);
    assert_eq!(pids(&summary["drained"]), vec![u64::from(other)]);
    assert_eq!(
        fx.install_manifest(),
        install_before,
        "the install dir changed"
    );
    assert_eq!(server_pid(&fx.flywheel_home), Some(hub.pid));
    assert!(!gone(hub.pid));
    assert!(gone(other));
    assert!(!gone(frozen), "the stopped worker was killed");

    let restarted = summary["restarted"].as_array().unwrap();
    assert_eq!(restarted.len(), 1, "{summary}");
    assert_eq!(restarted[0]["kind"], "hub-worker");
    assert_eq!(
        restarted[0]["folder"],
        repo_b.display().to_string().as_str()
    );
    let again = restarted[0]["pid"].as_u64().unwrap() as u32;
    cleanup.track(again);
    assert!(!gone(again), "the restarted worker is not running");
    assert_eq!(cwd_of(again), repo_b);
    assert_eq!(exe_sha(again), pre_update, "not the previous binary");
    let sid = restarted[0]["session_id"].as_str().unwrap();
    assert!(!before.contains(sid), "not a new session");
    assert!(sessions(&hub).contains_key(sid));
    assert_eq!(summary["manual"], json!([]));

    signal(frozen, libc::SIGCONT);
    wait_until("the resumed worker to exit", SETTLE, || {
        gone(frozen).then_some(())
    });
}
