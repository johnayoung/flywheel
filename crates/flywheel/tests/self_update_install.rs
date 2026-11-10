//! `fw self-update` source, build, and install, end to end through the
//! real binary: a scratch flywheel checkout cloned from a bare `origin`, a
//! fake `cargo` that "builds" unique marker-stamped copies of the `fw`
//! under test, and a temp install dir. Every run is confined to a
//! disposable `HOME` / `FLYWHEEL_HOME` (see `support`).

#![cfg(target_os = "linux")]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};
use support::{Fixture, STEPS, git, sha256};

const BUILD_ARGV: &str = "build --release --locked -p flywheel";

fn statuses(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

fn refused_at_preflight() -> Vec<(String, String)> {
    let mut expected = vec![("preflight".to_owned(), "failed".to_owned())];
    expected.extend(
        STEPS[1..]
            .iter()
            .map(|s| ((*s).to_owned(), "skipped".to_owned())),
    );
    expected
}

#[test]
fn installs_both_binaries_from_a_landed_checkout() {
    let fx = Fixture::new();
    let original = sha256(&fx.install.join("fw"));
    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(
        run.json_statuses(),
        statuses(&[
            ("preflight", "ok"),
            ("build", "ok"),
            ("drain", "skipped"),
            ("backup", "skipped"),
            ("install", "ok"),
            ("db-upgrade", "skipped"),
            ("hub", "skipped"),
            ("restart", "skipped"),
        ])
    );

    // The fake cargo ran the pinned build once, in the checkout.
    let log = fx.cargo_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0, fx.checkout.display().to_string());
    assert_eq!(log[0].1, BUILD_ARGV);

    // Both installed files are this run's artifacts.
    let marker = fx.last_marker().expect("the fake cargo built");
    let release = fx.checkout.join("target/release");
    fx.assert_installed_from(&release, &marker);

    let head = fx.head();
    assert_eq!(summary["outcome"], "updated");
    assert_eq!(summary["to"]["commit"], head.as_str());
    assert_eq!(
        summary["to"]["sha256"],
        sha256(&fx.install.join("fw")).as_str()
    );
    assert_eq!(summary["from"]["sha256"], original.as_str());
    assert_eq!(summary["from"]["commit"], serde_json::Value::Null);
    assert_eq!(summary["source"]["kind"], "checkout");
    assert_eq!(
        summary["source"]["path"],
        fx.checkout.display().to_string().as_str()
    );
    assert_eq!(summary["source"]["commit"], head.as_str());
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(summary["to"]["version"], version);
    for key in [
        "drained",
        "not_stopped",
        "unidentified_claims",
        "backups",
        "upgraded",
        "restarted",
        "manual",
    ] {
        assert_eq!(summary[key], serde_json::json!([]), "{key}");
    }

    // The previous pair is kept, and the record names commit, checkout,
    // and the installed bytes.
    let previous = fx.flywheel_home.join("self-update/previous");
    assert_eq!(sha256(&previous.join("fw")), original);
    assert_eq!(sha256(&previous.join("flywheel")), original);
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.flywheel_home.join("self-update/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["commit"], head.as_str());
    assert_eq!(
        record["checkout"],
        fx.checkout.display().to_string().as_str()
    );
    assert_eq!(record["fw_sha256"], sha256(&fx.install.join("fw")).as_str());
    assert_eq!(
        record["flywheel_sha256"],
        sha256(&fx.install.join("flywheel")).as_str()
    );

    // The installed binaries run and still print `fw <version>`.
    for name in ["fw", "flywheel"] {
        let out = std::process::Command::new(fx.install.join(name))
            .arg("--version")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("fw {version}\n")
        );
    }
    // No temp files are left in the install dir.
    assert_eq!(fx.install_manifest().len(), 2);
    // Nothing was written into HOME.
    assert!(fx.user_home_manifest().is_empty());
}

#[test]
fn human_output_is_one_line_per_step_then_the_outcome() {
    let fx = Fixture::new();
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 0, "{run:?}");
    let (steps, outcome) = run.human_statuses();
    assert_eq!(outcome, "updated");
    assert_eq!(steps[1], ("build".to_owned(), "ok".to_owned()));
    assert_eq!(steps[4], ("install".to_owned(), "ok".to_owned()));
    assert_eq!(run.stdout.lines().count(), STEPS.len() + 1, "{run:?}");
}

fn assert_refused_unchanged(fx: &Fixture, run: &support::Run, before: &Snapshot, needle: &str) {
    assert_eq!(run.code, 3, "{run:?}");
    let (steps, outcome) = run.human_statuses();
    assert_eq!(steps, refused_at_preflight(), "{run:?}");
    assert_eq!(outcome, "refused");
    assert!(run.stdout.contains(needle), "{needle:?} not in {run:?}");
    assert!(fx.cargo_log().is_empty(), "cargo ran: {:?}", fx.cargo_log());
    before.assert_unchanged(fx);
}

struct Snapshot {
    install: std::collections::BTreeMap<String, String>,
    home: std::collections::BTreeMap<String, String>,
}

impl Snapshot {
    fn take(fx: &Fixture) -> Self {
        Self {
            install: fx.install_manifest(),
            home: fx.home_manifest(),
        }
    }

    fn assert_unchanged(&self, fx: &Fixture) {
        assert_eq!(
            fx.install_manifest(),
            self.install,
            "the install dir changed"
        );
        assert_eq!(fx.home_manifest(), self.home, "FLYWHEEL_HOME changed");
    }
}

#[test]
fn a_modified_tracked_file_refuses_before_cargo() {
    let fx = Fixture::new();
    std::fs::write(fx.checkout.join("README.md"), "edited\n").unwrap();
    let before = Snapshot::take(&fx);
    let run = fx.run(&fx.checkout, &[]);
    assert_refused_unchanged(&fx, &run, &before, "README.md");
}

#[test]
fn an_untracked_file_alone_refuses_before_cargo() {
    let fx = Fixture::new();
    std::fs::write(fx.checkout.join("notes.txt"), "untracked\n").unwrap();
    // `git diff` sees nothing: only the untracked check can refuse this.
    assert_eq!(git(&fx.checkout, &["diff", "HEAD", "--name-only"]), "");
    let before = Snapshot::take(&fx);
    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 3, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "refused");
    assert_eq!(run.json_statuses(), refused_at_preflight());
    assert!(run.stdout.contains("notes.txt"), "{run:?}");
    assert!(fx.cargo_log().is_empty());
    before.assert_unchanged(&fx);
}

#[test]
fn an_unlanded_head_refuses_unless_allowed() {
    let fx = Fixture::new();
    git(&fx.checkout, &["checkout", "-qb", "feature"]);
    std::fs::write(fx.checkout.join("feature.txt"), "x\n").unwrap();
    git(&fx.checkout, &["add", "-A"]);
    git(&fx.checkout, &["commit", "-qm", "feature", "--no-gpg-sign"]);
    // Also absent locally: only origin/main could make it landed.
    git(&fx.checkout, &["branch", "-D", "main"]);
    let before = Snapshot::take(&fx);
    let run = fx.run(&fx.checkout, &[]);
    assert_refused_unchanged(&fx, &run, &before, "--allow-unlanded");

    let run = fx.run(&fx.checkout, &["--allow-unlanded", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.summary()["to"]["commit"], fx.head().as_str());
    let marker = fx.last_marker().unwrap();
    fx.assert_installed_from(&fx.checkout.join("target/release"), &marker);
}

#[test]
fn a_head_landed_only_on_origin_is_accepted() {
    let fx = Fixture::new();
    // Advance origin/main past the local main, then detach at it.
    let other = fx.root.join("other");
    git(
        &fx.root,
        &[
            "clone",
            "-q",
            fx.origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("more.txt"), "more\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "more", "--no-gpg-sign"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(&fx.checkout, &["fetch", "-q", "origin"]);
    git(&fx.checkout, &["checkout", "-q", "--detach", "origin/main"]);
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 0, "{run:?}");
}

#[test]
fn a_non_flywheel_repo_refuses_then_the_recorded_checkout_is_used() {
    let fx = Fixture::new();
    let unrelated = fx.root.join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    git(&unrelated, &["init", "-q", "-b", "main"]);
    std::fs::write(unrelated.join("README.md"), "other project\n").unwrap();
    git(&unrelated, &["add", "-A"]);
    git(&unrelated, &["commit", "-qm", "init", "--no-gpg-sign"]);

    // Fresh home: nothing recorded, so the unrelated repo is refused.
    let before = Snapshot::take(&fx);
    let run = fx.run(&unrelated, &[]);
    assert_refused_unchanged(&fx, &run, &before, "not a flywheel checkout");

    // A repo whose crates/flywheel is some other package is refused too.
    std::fs::create_dir_all(unrelated.join("crates/flywheel")).unwrap();
    std::fs::write(
        unrelated.join("crates/flywheel/Cargo.toml"),
        "[package]\nname = \"impostor\"\n\n[[bin]]\nname = \"fw\"\n\n[[bin]]\nname = \"flywheel\"\n",
    )
    .unwrap();
    git(&unrelated, &["add", "-A"]);
    git(&unrelated, &["commit", "-qm", "impostor", "--no-gpg-sign"]);
    let run = fx.run(&unrelated, &[]);
    assert_refused_unchanged(&fx, &run, &before, "not a flywheel checkout");

    // After one successful update, the same repo builds the recorded
    // checkout.
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 0, "{run:?}");
    let run = fx.run(&unrelated, &["--force", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let log = fx.cargo_log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].0, fx.checkout.display().to_string());
    assert_eq!(
        run.summary()["source"]["path"],
        fx.checkout.display().to_string().as_str()
    );

    // --from names the checkout explicitly from anywhere.
    let from = fx.checkout.display().to_string();
    let run = fx.run(&unrelated, &["--from", &from, "--force"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(fx.cargo_log()[2].0, from);
    // --from at a non-flywheel repo is refused.
    let before = Snapshot::take(&fx);
    let unrelated_str = unrelated.display().to_string();
    let run = fx.run(&fx.checkout, &["--from", &unrelated_str, "--force"]);
    assert_eq!(run.code, 3, "{run:?}");
    assert_eq!(fx.cargo_log().len(), 3);
    before.assert_unchanged(&fx);
}

#[test]
fn a_failed_build_refuses_with_the_install_dir_untouched() {
    let fx = Fixture::new();
    let before = Snapshot::take(&fx);
    let run = fx.run_with(&fx.checkout, &["--json"], |c| {
        c.env("FAKE_CARGO_EXIT", "101");
    });
    assert_eq!(run.code, 3, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "refused");
    let st = run.json_statuses();
    assert_eq!(st[0].1, "ok");
    assert_eq!(st[1], ("build".to_owned(), "failed".to_owned()));
    assert!(st[2..].iter().all(|(_, s)| s == "skipped"), "{st:?}");
    assert_eq!(fx.cargo_log().len(), 1);
    before.assert_unchanged(&fx);
}

#[test]
fn a_non_executable_artifact_refuses_with_the_install_dir_untouched() {
    let fx = Fixture::new();
    let before = Snapshot::take(&fx);
    let run = fx.run_with(&fx.checkout, &[], |c| {
        c.env("FAKE_CARGO_NOEXEC", "1");
    });
    assert_eq!(run.code, 3, "{run:?}");
    let (steps, outcome) = run.human_statuses();
    assert_eq!(outcome, "refused");
    assert_eq!(steps[1], ("build".to_owned(), "failed".to_owned()));
    assert!(run.stdout.contains("does not run"), "{run:?}");
    before.assert_unchanged(&fx);
}

#[test]
fn a_held_lock_refuses_a_second_run_at_once_naming_the_holder() {
    let fx = Fixture::new();
    let hold = fx.control.join("hold");
    let started = fx.control.join("hold.started");
    let mut first = fx.spawn_with(&fx.checkout, &["--json"], |c| {
        c.env("FAKE_CARGO_HOLD", &hold);
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "the first run never reached cargo"
        );
        if let Some(status) = first.try_wait().unwrap() {
            panic!("the first run exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = Snapshot::take(&fx);
    let t0 = Instant::now();
    let second = fx.run(&fx.checkout, &[]);
    let elapsed = t0.elapsed();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(second.code, 3, "{second:?}");
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        assert!(
            second.stdout.contains(&format!("pid {}", first.id())),
            "{second:?}"
        );
        assert_eq!(fx.cargo_log().len(), 1, "the second run ran cargo");
        before.assert_unchanged(&fx);
    }));
    // Release the first run (our own child) and let it finish.
    std::fs::write(&hold, "").unwrap();
    let output = first.wait_with_output().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn a_second_run_at_the_installed_head_is_current_and_force_updates() {
    let fx = Fixture::new();
    assert_eq!(fx.run(&fx.checkout, &[]).code, 0);
    let installed = fx.install_manifest();
    let before = Snapshot::take(&fx);

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "current");
    assert_eq!(summary["from"]["commit"], fx.head().as_str());
    assert_eq!(run.json_statuses()[0].1, "ok");
    assert!(run.json_statuses()[1..].iter().all(|(_, s)| s == "skipped"));
    assert_eq!(fx.cargo_log().len(), 1, "cargo ran for a current install");
    before.assert_unchanged(&fx);

    // --force rebuilds and installs new bytes.
    let run = fx.run(&fx.checkout, &["--force"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.human_statuses().1, "updated");
    assert_eq!(fx.cargo_log().len(), 2);
    assert_ne!(fx.install_manifest(), installed);
    let marker = fx.last_marker().unwrap();
    fx.assert_installed_from(&fx.checkout.join("target/release"), &marker);
}

#[test]
fn current_is_decided_by_installed_bytes_not_the_version_string() {
    let fx = Fixture::new();
    assert_eq!(fx.run(&fx.checkout, &[]).code, 0);
    // Same HEAD, same version string, different bytes: not current.
    support::copy_exe(&support::fw_under_test(), &fx.install.join("fw"));
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.human_statuses().1, "updated");
    assert_eq!(fx.cargo_log().len(), 2);
    let marker = fx.last_marker().unwrap();
    fx.assert_installed_from(&fx.checkout.join("target/release"), &marker);

    // A new landed commit is not current either.
    std::fs::write(fx.checkout.join("README.md"), "next\n").unwrap();
    git(&fx.checkout, &["commit", "-qam", "next", "--no-gpg-sign"]);
    let run = fx.run(&fx.checkout, &[]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(fx.cargo_log().len(), 3);
}

#[test]
fn dry_run_builds_but_changes_nothing() {
    let fx = Fixture::new();
    let before = Snapshot::take(&fx);
    let run = fx.run(&fx.checkout, &["--dry-run", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "planned");
    assert_eq!(summary["to"]["commit"], fx.head().as_str());
    let st = run.json_statuses();
    assert_eq!(st[0].1, "ok");
    assert_eq!(st[1].1, "ok");
    assert!(st[2..].iter().all(|(_, s)| s == "planned"), "{st:?}");
    assert_eq!(fx.cargo_log().len(), 1);
    before.assert_unchanged(&fx);
    // Only the lock file was written under FLYWHEEL_HOME.
    let all = support::manifest(&fx.flywheel_home, &[]);
    assert_eq!(all.keys().collect::<Vec<_>>(), ["self-update/lock"]);
}

#[test]
fn refused_install_dirs() {
    let fx = Fixture::new();
    // Lacking flywheel.
    let half = fx.root.join("half");
    std::fs::create_dir_all(&half).unwrap();
    support::copy_exe(&support::fw_under_test(), &half.join("fw"));
    let half_str = half.display().to_string();
    let before = Snapshot::take(&fx);
    let run = fx.run(&fx.checkout, &["--install-dir", &half_str]);
    assert_eq!(run.code, 3, "{run:?}");
    assert!(run.stdout.contains("lacks flywheel"), "{run:?}");
    assert_eq!(support::manifest(&half, &[]).len(), 1);

    // Inside a cargo target dir.
    let ws = fx.root.join("ws");
    let target = ws.join("target/release");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[workspace]\n").unwrap();
    support::copy_exe(&support::fw_under_test(), &target.join("fw"));
    support::copy_exe(&support::fw_under_test(), &target.join("flywheel"));
    let target_manifest = support::manifest(&target, &[]);
    let target_str = target.display().to_string();
    let run = fx.run(&fx.checkout, &["--install-dir", &target_str]);
    assert_eq!(run.code, 3, "{run:?}");
    assert!(run.stdout.contains("cargo target dir"), "{run:?}");
    assert_eq!(support::manifest(&target, &[]), target_manifest);

    // Missing.
    let missing = fx.root.join("missing").display().to_string();
    let run = fx.run(&fx.checkout, &["--install-dir", &missing]);
    assert_eq!(run.code, 3, "{run:?}");

    assert!(fx.cargo_log().is_empty());
    before.assert_unchanged(&fx);
}

#[test]
fn cargo_target_dir_locates_the_artifacts() {
    let fx = Fixture::new();
    let target = fx.root.join("shared-target");
    let run = fx.run_with(&fx.checkout, &[], |c| {
        c.env("CARGO_TARGET_DIR", &target);
    });
    assert_eq!(run.code, 0, "{run:?}");
    assert!(!fx.checkout.join("target").exists());
    let marker = fx.last_marker().unwrap();
    fx.assert_installed_from(&target.join("release"), &marker);
}

#[test]
fn usage_errors_exit_two_and_help_names_every_flag() {
    let fx = Fixture::new();
    let run = fx.run(&fx.checkout, &["--no-such-flag"]);
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(run.stdout, "");
    let run = fx.run(Path::new("/"), &["--help"]);
    assert_eq!(run.code, 0);
    for flag in [
        "--from",
        "--install-dir",
        "--allow-unlanded",
        "--force",
        "--drain-timeout",
        "--upgrade-postgres",
        "--dry-run",
        "--json",
    ] {
        assert!(run.stdout.contains(flag), "{flag}");
    }
    assert!(fx.cargo_log().is_empty());
}
