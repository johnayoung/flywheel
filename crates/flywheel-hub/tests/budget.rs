//! The build budget reaching sessions (see `flywheel_hub::budget`): each
//! gets the pool and its test threads, a variable already in the hub's
//! environment passes through unchanged, and a session started with the
//! budget off (or on a hub with it off) gets exactly what it would
//! without the feature.
//!
//! One test, alone in its binary: it sets this process's environment
//! (the hub's), which nothing else may read meanwhile.

mod support;

use flywheel_hub::budget::{BudgetConfig, FIFO_FILE, VARS};
use flywheel_hub::state::WorkspaceInput;
use flywheel_hub::{CreateSession, Hub, HubOptions};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn open(dir: &Path, budget: BudgetConfig) -> Arc<Hub> {
    let mut options = HubOptions::new(dir.join("state"));
    options.default_shell = vec!["/bin/sh".into()];
    options.build_budget = budget;
    support::open_hub(options)
}

/// A workspace of `folder`: its id.
fn workspace(hub: &Hub, folder: &Path) -> String {
    hub.create_workspace(WorkspaceInput {
        name: Some("budget".into()),
        folders: None,
        repos: Some(vec![folder.to_owned()]),
    })
    .unwrap()
    .id
}

/// A shell session in `workspace` that writes its environment to `out`
/// and waits; the environment, once written.
fn session_env(
    hub: &Arc<Hub>,
    workspace: &str,
    out: &Path,
    budget: Option<bool>,
) -> BTreeMap<String, String> {
    let tmp = format!("{}.tmp", out.display());
    let view = hub
        .create_session(CreateSession {
            workspace_id: workspace.to_owned(),
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "env > '{tmp}' && mv '{tmp}' '{}'; exec sleep 600",
                    out.display()
                ),
            ]),
            build_budget: budget,
            ..CreateSession::default()
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !out.exists() {
        assert!(
            Instant::now() < deadline,
            "the session never wrote its environment"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    hub.kill_session(&view.record.id).unwrap();
    std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// `env` without what names the session itself.
fn sans_ids(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    for key in [
        "FLYWHEEL_HUB_SESSION",
        "FLYWHEEL_HUB_WORKSPACE",
        "OLDPWD",
        "SHLVL",
        "_",
    ] {
        env.remove(key);
    }
    env
}

#[test]
fn sessions_take_the_budget_unless_the_hub_env_or_the_session_says_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("repo");
    std::fs::create_dir_all(&folder).unwrap();
    // This process's own setting of a variable wins (an agent session
    // running these tests may cap its test threads already).
    let theirs = |key: &str| std::env::var(key).ok();
    // A hub session running these tests exports its own hub's pool; the
    // hubs built here must start without one.
    // SAFETY: the only test in this binary; no other thread reads the
    // environment while it changes.
    unsafe {
        std::env::remove_var("MAKEFLAGS");
        std::env::remove_var("CARGO_MAKEFLAGS");
    }

    let on = BudgetConfig {
        enabled: true,
        jobs: 6,
        test_threads: 3,
    };
    let hub = open(&dir.path().join("on"), on);
    let fifo = std::fs::canonicalize(dir.path().join("on/state").join(FIFO_FILE)).unwrap();
    let flags = format!("-j6 --jobserver-auth=fifo:{}", fifo.display());
    let ws = workspace(&hub, &folder);
    let got = session_env(&hub, &ws, &dir.path().join("env-on"), None);
    let want = [flags.as_str(), flags.as_str(), "3", "3"];
    for (key, value) in VARS.iter().zip(want) {
        let expected = theirs(key).unwrap_or_else(|| value.to_owned());
        assert_eq!(got.get(*key), Some(&expected), "{key}");
    }
    assert_eq!(got.get("TERM").map(String::as_str), Some("xterm-256color"));
    // Off for one session: its environment is a budget-free one.
    let opted_out = sans_ids(session_env(
        &hub,
        &ws,
        &dir.path().join("env-out"),
        Some(false),
    ));
    hub.shutdown(Duration::from_secs(5));
    drop(hub);

    let off = open(
        &dir.path().join("off"),
        BudgetConfig {
            enabled: false,
            ..on
        },
    );
    assert!(
        !dir.path().join("off/state").join(FIFO_FILE).exists(),
        "no pool with the budget off"
    );
    let ws = workspace(&off, &folder);
    let plain = sans_ids(session_env(&off, &ws, &dir.path().join("env-off"), None));
    off.shutdown(Duration::from_secs(5));
    drop(off);
    assert_eq!(
        opted_out, plain,
        "opting out leaves exactly the environment of a hub without it"
    );
    for key in VARS {
        assert_eq!(plain.get(key), theirs(key).as_ref(), "{key}");
    }

    // Set in the hub's environment: passed through unchanged, and the
    // jobserver pair is left alone together.
    // SAFETY: the only test in this binary; no other thread reads the
    // environment while it changes.
    unsafe {
        std::env::set_var("MAKEFLAGS", "-j2");
        std::env::set_var("CARGO_BUILD_JOBS", "5");
    }
    let hub = open(&dir.path().join("preset"), on);
    let ws = workspace(&hub, &folder);
    let preset = session_env(&hub, &ws, &dir.path().join("env-preset"), None);
    hub.shutdown(Duration::from_secs(5));
    assert_eq!(preset.get("MAKEFLAGS").map(String::as_str), Some("-j2"));
    assert_eq!(
        preset.get("CARGO_BUILD_JOBS").map(String::as_str),
        Some("5")
    );
    assert_eq!(
        preset.get("CARGO_MAKEFLAGS"),
        None,
        "no second pool beside the hub env's"
    );
    assert_eq!(
        preset.get("NEXTEST_TEST_THREADS"),
        Some(&theirs("NEXTEST_TEST_THREADS").unwrap_or_else(|| "3".to_owned()))
    );
}
