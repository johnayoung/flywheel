//! `fw self-update`: build a clean, landed flywheel checkout and install
//! the new `fw` / `flywheel` pair in place.
//!
//! Steps, always reported in this order: `preflight build drain backup
//! install db-upgrade hub restart` ([`output::STEPS`]). Preflight takes the
//! lock, validates the install dir and the source, and decides "already
//! current"; build runs `cargo build --release --locked -p flywheel` and
//! smoke-checks both artifacts; install keeps the previous pair under
//! `$FLYWHEEL_HOME/self-update/previous/` and replaces both binaries by
//! atomic rename, then records commit, checkout, and installed sha256.
//! Preflight also refuses while an operator console of this home runs.
//! Drain (after the build, before the install) SIGTERMs this home's
//! `worker` / `autopilot` / `triage` daemons found from `/proc` and waits
//! up to `--drain-timeout`; a survivor aborts the run (exit 1) with the
//! install dir untouched ([`drain`]). Stores ([`stores`]): after the
//! build and before the drain, every project in `fw project ls` is asked,
//! through the newly built `fw db status`, whether its store is behind; a
//! behind Postgres store without `--upgrade-postgres`, an unreachable one,
//! or one newer than the build refuses the run. Backup (after the drain)
//! snapshots each behind SQLite store into `projects/<id>/backups/`;
//! db-upgrade (after the install) runs the installed `fw db upgrade
//! --project ID` per store. A db-upgrade failure restores every upgraded
//! store from its backup, puts the previous pair and record back, and
//! exits 1 with install and db-upgrade `reverted`. Every refusal (exit 3)
//! happens before anything outside the lock file changes. Hub ([`hub`]):
//! a serving hub of this home is handed off to the installed `fw` as `fw
//! hub upgrade --binary` does (never `--allow-restart`; no hub is started
//! when none runs); a refused or failed handoff is not reverted and exits
//! 4 (`updated-hub-pending`). Restart ([`restart`]): drained hub workers
//! and autopilots get new sessions through the hub API; terminal
//! processes get a shell-quoted `manual` command. Every revert (drain
//! timeout, backup, install, or db-upgrade failure) restarts the
//! hub-owned processes it stopped on the previous binary, leaving the hub
//! itself alone.
//!
//! Spec: `docs/cli.md` (`self-update`).

pub mod args;
pub mod drain;
pub mod hub;
pub mod install;
pub mod output;
pub mod restart;
pub mod source;
pub mod stores;

use flywheel_orchestrator::home::Home;
use install::{InstallError, Record, StoreRecord};
use output::{BinaryInfo, Outcome, Reporter, SourceInfo, Status, Step, Summary};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use stores::{Backend, Planned};

pub const SELF_UPDATE_VERB: &str = "self-update";

/// `fw self-update --help`.
pub const SELF_UPDATE_USAGE: &str =
    "usage: fw self-update [--from PATH] [--install-dir DIR] [--allow-unlanded] [--force]
                      [--drain-timeout SECS] [--upgrade-postgres] [--dry-run] [--json]

Build a clean flywheel checkout at a commit landed on the default branch
(cargo build --release --locked -p flywheel) and install the new fw and
flywheel in place, keeping the previous pair under
$FLYWHEEL_HOME/self-update/previous/. Steps, in order: preflight build
drain backup install db-upgrade hub restart.

options:
  --from PATH          the checkout to build (default: the cwd repo when it is a
                       flywheel checkout, else the checkout the last update recorded)
  --install-dir DIR    where fw and flywheel are replaced (default: the running
                       executable's directory)
  --allow-unlanded     build a HEAD not reachable from the default branch
  --force              update even when HEAD and the installed bytes are current
  --drain-timeout SECS seconds to wait for each drained process (default 120)
  --upgrade-postgres   allow upgrading a Postgres store that is behind
  --dry-run            build, then print the plan; install nothing
  --json               JSON Lines: one {step,status,detail} per step, then one summary

exit: 0 updated, current, or planned; 1 failed after changes began (reverted);
      2 usage error; 3 refused before any change; 4 updated, hub not handed off
";

/// Run `fw self-update ...` (`args` excludes the verb).
pub fn run_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match args::parse(args) {
        Ok(args::Parsed::Help) => {
            let _ = out.write_all(SELF_UPDATE_USAGE.as_bytes());
            return 0;
        }
        Ok(args::Parsed::Run(parsed)) => parsed,
        Err(message) => {
            let _ = writeln!(err, "{SELF_UPDATE_USAGE}fw self-update: error: {message}");
            return 2;
        }
    };
    let env = Environment {
        home: Home::from_env().ok(),
        cwd: std::env::current_dir().unwrap_or_default(),
        current_exe: std::env::current_exe().unwrap_or_default(),
        cargo_target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
    };
    run(&parsed, &env, out)
}

/// The process facts a run depends on.
struct Environment {
    home: Option<Home>,
    cwd: PathBuf,
    current_exe: PathBuf,
    cargo_target_dir: Option<String>,
}

/// Refuse in the current step: report it failed, every later step skipped.
fn refuse(mut reporter: Reporter<'_>, step: Step, why: &str, summary: &Summary) -> i32 {
    reporter.step(step, Status::Failed, Some(why));
    reporter.finish(Status::Skipped, Outcome::Refused, summary)
}

fn run(args: &args::Args, env: &Environment, out: &mut dyn Write) -> i32 {
    let mut reporter = Reporter::new(out, args.json);
    let mut summary = Summary::default();

    // --- preflight -------------------------------------------------------------
    let Some(home) = env.home.clone() else {
        let why = "no flywheel home: set FLYWHEEL_HOME or HOME";
        return refuse(reporter, Step::Preflight, why, &summary);
    };
    let _lock = match install::take_lock(&home) {
        Ok(lock) => lock,
        Err(why) => return refuse(reporter, Step::Preflight, &why, &summary),
    };
    let record = install::read_record(&home);
    let install_dir =
        match install::resolve_install_dir(args.install_dir.as_deref(), &env.current_exe) {
            Ok(dir) => dir,
            Err(why) => return refuse(reporter, Step::Preflight, &why, &summary),
        };
    summary.from = installed_info(&install_dir, record.as_ref());
    let root = match source::resolve(
        args.from.as_deref(),
        &env.cwd,
        record.as_ref().map(|r| r.checkout.as_path()),
    ) {
        Ok(root) => root,
        Err(why) => return refuse(reporter, Step::Preflight, &why, &summary),
    };
    summary.source = Some(SourceInfo {
        path: root.display().to_string(),
        commit: None,
    });
    let checkout = match source::validate(&root, args.allow_unlanded) {
        Ok(checkout) => checkout,
        Err(why) => return refuse(reporter, Step::Preflight, &why, &summary),
    };
    summary.source = Some(SourceInfo {
        path: checkout.root.display().to_string(),
        commit: Some(checkout.head.clone()),
    });
    let short = source::short(&checkout.head).to_owned();
    if !args.force
        && record
            .as_ref()
            .is_some_and(|r| install::is_current(r, &checkout.head, &install_dir))
    {
        summary.to = summary.from.clone();
        let detail = format!(
            "already current: {} at {short} is installed in {}",
            checkout.root.display(),
            install_dir.display()
        );
        reporter.step(Step::Preflight, Status::Ok, Some(&detail));
        return reporter.finish(Status::Skipped, Outcome::Current, &summary);
    }
    if let drain::Discovery::Console(pid) = drain::discover(&home) {
        return refuse(reporter, Step::Preflight, &console_refusal(pid), &summary);
    }
    let detail = format!(
        "{} at {short}, install dir {}",
        checkout.root.display(),
        install_dir.display()
    );
    reporter.step(Step::Preflight, Status::Ok, Some(&detail));

    // --- build -----------------------------------------------------------------
    if let Err(why) = source::build(&checkout.root) {
        return refuse(reporter, Step::Build, &why, &summary);
    }
    let artifacts = source::artifacts(&checkout.root, env.cargo_target_dir.as_deref());
    let version = match source::smoke(&artifacts) {
        Ok(version) => version,
        Err(why) => return refuse(reporter, Step::Build, &why, &summary),
    };
    let hashes = install::sha256_file(&artifacts.fw)
        .and_then(|fw| install::sha256_file(&artifacts.flywheel).map(|fly| (fw, fly)));
    let (fw_sha, flywheel_sha) = match hashes {
        Ok(hashes) => hashes,
        Err(why) => return refuse(reporter, Step::Build, &why, &summary),
    };
    summary.to = BinaryInfo {
        version: Some(version.clone()),
        commit: Some(checkout.head.clone()),
        sha256: Some(fw_sha.clone()),
    };
    let detail = format!("fw {version} at {short}");
    reporter.step(Step::Build, Status::Ok, Some(&detail));

    // --- store plan -------------------------------------------------------------
    // Before any signal: the new build says which stores are behind, and a
    // store it cannot upgrade safely refuses the run here.
    let planned = match stores::plan(&home, &artifacts.fw, args.upgrade_postgres) {
        Ok(planned) => planned,
        Err(why) => return refuse(reporter, Step::Backup, &why, &summary),
    };

    // --- drain (discovery) -------------------------------------------------------
    // Re-read now: the build took a while and processes come and go.
    let set = match drain::discover(&home) {
        drain::Discovery::Drain(set) => set,
        drain::Discovery::Console(pid) => {
            return refuse(reporter, Step::Drain, &console_refusal(pid), &summary);
        }
    };

    // --- dry run ---------------------------------------------------------------
    if args.dry_run {
        summary.drained = set.iter().map(drain::Drainable::to_json).collect();
        let hub_pid = drain::hub_pid(&home.hub_dir());
        summary.hub = Some(serde_json::json!({
            "action": if hub_pid.is_some() { "handoff" } else { "not-running" },
            "old_pid": hub_pid,
            "new_pid": null,
        }));
        let drain_detail = if set.is_empty() {
            "no worker, autopilot, or triage process of this home is running".to_owned()
        } else {
            format!("would stop {}", describe(&set))
        };
        let detail = format!(
            "would replace fw and flywheel in {} (previous pair to {})",
            install_dir.display(),
            home.self_update_previous_dir().display()
        );
        let hub_detail = match hub_pid {
            Some(pid) => format!("hub pid {pid} is serving this home"),
            None => "no hub is serving this home".to_owned(),
        };
        summary.upgraded = planned.iter().map(|p| p.to_json(None)).collect();
        let to_back_up: Vec<&str> = planned
            .iter()
            .filter(|p| matches!(p.backend, Backend::Sqlite(_)))
            .map(|p| p.project_id.as_str())
            .collect();
        let backup_detail = if to_back_up.is_empty() {
            "no sqlite store is behind the build".to_owned()
        } else {
            format!(
                "would back up the store of {} under projects/<id>/backups/",
                to_back_up.join(", ")
            )
        };
        let upgrade_detail = if planned.is_empty() {
            "no store is behind the build".to_owned()
        } else {
            format!("would upgrade {}", describe_planned(&planned, None))
        };
        reporter.step(Step::Drain, Status::Planned, Some(&drain_detail));
        reporter.step(Step::Backup, Status::Planned, Some(&backup_detail));
        reporter.step(Step::Install, Status::Planned, Some(&detail));
        reporter.step(Step::DbUpgrade, Status::Planned, Some(&upgrade_detail));
        reporter.step(Step::Hub, Status::Planned, Some(&hub_detail));
        return reporter.finish(Status::Planned, Outcome::Planned, &summary);
    }

    // --- drain -----------------------------------------------------------------
    // What the drain stopped: restarted after the hub step, or on the
    // previous binary by any revert.
    let mut stopped: Vec<drain::Drainable> = Vec::new();
    if set.is_empty() {
        let detail = "no worker, autopilot, or triage process of this home is running";
        reporter.step(Step::Drain, Status::Skipped, Some(detail));
    } else {
        let timeout = std::time::Duration::from_secs(args.drain_timeout_secs);
        let drained = drain::drain(&set, timeout);
        summary.drained = drained
            .stopped
            .iter()
            .map(drain::Drainable::to_json)
            .collect();
        if !drained.not_stopped.is_empty() {
            summary.not_stopped = drained
                .not_stopped
                .iter()
                .map(drain::Drainable::to_json)
                .collect();
            let detail = format!(
                "still running {}s after SIGTERM: {}; nothing was installed",
                args.drain_timeout_secs,
                describe(&drained.not_stopped)
            );
            reporter.step(Step::Drain, Status::Failed, Some(&detail));
            return finish_reverted(
                reporter,
                &mut summary,
                &home,
                &drained.stopped,
                &install_dir,
            );
        }
        let detail = format!("stopped {}", describe(&drained.stopped));
        reporter.step(Step::Drain, Status::Ok, Some(&detail));
        stopped = drained.stopped;
    }
    summary.unidentified_claims = drain::live_claims(&home);

    // --- backup ----------------------------------------------------------------
    // One snapshot per behind SQLite store, after the drain and before any
    // store changes; `backups[i]` belongs to `planned[i]`.
    let mut backups: Vec<Option<PathBuf>> = vec![None; planned.len()];
    for (slot, plan) in backups.iter_mut().zip(&planned) {
        if !matches!(plan.backend, Backend::Sqlite(_)) {
            continue;
        }
        match stores::backup(&home, plan) {
            Ok(path) => {
                summary.backups.push(json!({
                    "project_id": plan.project_id,
                    "path": path.display().to_string(),
                    "run_version": plan.from.run,
                    "orchestrator_version": plan.from.orchestrator,
                }));
                *slot = Some(path);
            }
            Err(why) => {
                let detail = format!("{why}; no store was upgraded and nothing was installed");
                reporter.step(Step::Backup, Status::Failed, Some(&detail));
                return finish_reverted(reporter, &mut summary, &home, &stopped, &install_dir);
            }
        }
    }
    let backed_up: Vec<String> = backups
        .iter()
        .flatten()
        .map(|p| p.display().to_string())
        .collect();
    if backed_up.is_empty() {
        let detail = if planned.is_empty() {
            "no store is behind the build"
        } else {
            "no sqlite store is behind the build"
        };
        reporter.step(Step::Backup, Status::Skipped, Some(detail));
    } else {
        let detail = format!("wrote {}", backed_up.join(", "));
        reporter.step(Step::Backup, Status::Ok, Some(&detail));
    }

    // --- install ---------------------------------------------------------------
    let installed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut new_record = Record {
        commit: checkout.head.clone(),
        checkout: checkout.root.clone(),
        install_dir: install_dir.clone(),
        version: version.clone(),
        fw_sha256: fw_sha.clone(),
        flywheel_sha256: flywheel_sha.clone(),
        installed_at,
        stores: Vec::new(),
    };
    let installed = install::install(
        &home,
        &install_dir,
        [
            ("fw", artifacts.fw.as_path(), fw_sha.as_str()),
            (
                "flywheel",
                artifacts.flywheel.as_path(),
                flywheel_sha.as_str(),
            ),
        ],
    )
    .and_then(|()| {
        install::write_record(&home, &new_record)
            .map_err(|why| restore_previous(&home, &install_dir, why))
    });
    match installed {
        Ok(()) => {}
        Err(InstallError::Reverted(why)) => {
            summary.to = summary.from.clone();
            let detail = format!("{why}; the previous pair is back in place");
            reporter.step(Step::Install, Status::Reverted, Some(&detail));
            return finish_reverted(reporter, &mut summary, &home, &stopped, &install_dir);
        }
        Err(InstallError::NothingChanged(why)) => {
            summary.to = summary.from.clone();
            reporter.step(Step::Install, Status::Failed, Some(&why));
            return finish_reverted(reporter, &mut summary, &home, &stopped, &install_dir);
        }
        Err(InstallError::RevertFailed(why)) => {
            let detail = format!(
                "{why}; restore by hand from {}",
                home.self_update_previous_dir().display()
            );
            reporter.step(Step::Install, Status::Failed, Some(&detail));
            return finish_reverted(reporter, &mut summary, &home, &stopped, &install_dir);
        }
    }
    let install_detail = format!(
        "fw and flywheel {version} ({short}) in {}",
        install_dir.display()
    );

    // --- db-upgrade ------------------------------------------------------------
    // SQLite stores first (each restorable from its backup), Postgres last.
    let installed_fw = install_dir.join("fw");
    let mut order: Vec<usize> = (0..planned.len()).collect();
    order.sort_by_key(|&i| matches!(planned[i].backend, Backend::Postgres(_)));
    let mut upgraded: Vec<usize> = Vec::new();
    let mut failure = None;
    for i in order {
        match stores::upgrade(&installed_fw, &home, &planned[i]) {
            Ok(()) => upgraded.push(i),
            Err(why) => {
                failure = Some((i, why));
                break;
            }
        }
    }
    let Some((failed, why)) = failure else {
        reporter.step(Step::Install, Status::Ok, Some(&install_detail));
        summary.upgraded = planned
            .iter()
            .zip(&backups)
            .map(|(plan, backup)| plan.to_json(backup.as_deref()))
            .collect();
        if planned.is_empty() {
            let detail = "no store is behind the build";
            reporter.step(Step::DbUpgrade, Status::Skipped, Some(detail));
        } else {
            new_record.stores = planned
                .iter()
                .zip(&backups)
                .map(|(plan, backup)| StoreRecord {
                    project_id: plan.project_id.clone(),
                    store: plan.store_name().to_owned(),
                    backup: backup.clone(),
                })
                .collect();
            let mut detail = format!("upgraded {}", describe_planned(&planned, Some(&backups)));
            if let Err(e) = install::write_record(&home, &new_record) {
                detail.push_str(&format!("; could not record the upgraded stores: {e}"));
            }
            reporter.step(Step::DbUpgrade, Status::Ok, Some(&detail));
        }
        return hub_and_restart(reporter, &mut summary, &home, &stopped, &installed_fw);
    };

    // --- revert ----------------------------------------------------------------
    // Restore every store this run changed, then the previous pair and
    // record; the hub is not touched.
    let mut to_restore = upgraded.clone();
    if stores::changed(&planned[failed]) {
        to_restore.push(failed);
    }
    let mut restored = Vec::new();
    let mut still_upgraded = Vec::new();
    let mut store_errors = Vec::new();
    for i in to_restore {
        let plan = &planned[i];
        let outcome = match &backups[i] {
            Some(backup) => stores::restore(plan, backup),
            None => stores::restore(plan, Path::new("")),
        };
        match outcome {
            Ok(()) => restored.push(plan.project_id.clone()),
            Err(e) => {
                store_errors.push(e);
                still_upgraded.push(plan.to_json(backups[i].as_deref()));
            }
        }
    }
    summary.upgraded = still_upgraded;
    let binaries = install::restore_previous(&home, &install_dir);
    let record_back = match record.as_ref() {
        Some(previous) => install::write_record(&home, previous),
        None => match std::fs::remove_file(home.self_update_state()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!(
                "cannot remove {}: {e}",
                home.self_update_state().display()
            )),
            _ => Ok(()),
        },
    };
    match binaries.and(record_back) {
        Ok(()) => {
            summary.to = summary.from.clone();
            let detail = format!("{install_detail}; reverted: the previous pair is back in place");
            reporter.step(Step::Install, Status::Reverted, Some(&detail));
        }
        Err(e) => {
            let detail = format!(
                "{install_detail}; revert failed: {e}; restore by hand from {}",
                home.self_update_previous_dir().display()
            );
            reporter.step(Step::Install, Status::Failed, Some(&detail));
        }
    }
    let restored_detail = if restored.is_empty() {
        "no store had changed".to_owned()
    } else {
        format!("restored {} from their backups", restored.join(", "))
    };
    if store_errors.is_empty() {
        let detail = format!("{why}; {restored_detail}");
        reporter.step(Step::DbUpgrade, Status::Reverted, Some(&detail));
    } else {
        let detail = format!("{why}; {restored_detail}; {}", store_errors.join("; "));
        reporter.step(Step::DbUpgrade, Status::Failed, Some(&detail));
    }
    finish_reverted(reporter, &mut summary, &home, &stopped, &install_dir)
}

/// The hub and restart steps after a successful install and db-upgrade.
fn hub_and_restart(
    mut reporter: Reporter<'_>,
    summary: &mut Summary,
    home: &Home,
    stopped: &[drain::Drainable],
    installed_fw: &Path,
) -> i32 {
    // --- hub -------------------------------------------------------------------
    let hub_dir = home.hub_dir();
    let handoff = hub::hand_off(installed_fw, &hub_dir);
    summary.hub = Some(handoff.to_json());
    let client = match &handoff {
        hub::Handoff::NotRunning => {
            let detail = "no hub is serving this home; none was started";
            reporter.step(Step::Hub, Status::Skipped, Some(detail));
            Err("no hub serves this home to restart it in".to_owned())
        }
        hub::Handoff::Done { old_pid, new_pid } => {
            let detail = format!(
                "handed off from pid {old_pid} to pid {new_pid} ({}); every session kept running",
                installed_fw.display()
            );
            reporter.step(Step::Hub, Status::Ok, Some(&detail));
            hub::Client::connect(&hub_dir)
                .and_then(|client| client.wait_ready().map(|()| client))
                .map_err(|why| format!("the handed-off hub cannot be reached: {why}"))
        }
        hub::Handoff::Refused { old_pid, why } | hub::Handoff::Failed { old_pid, why } => {
            // Not reverted: binaries and stores are consistent, and the
            // hub can be upgraded alone.
            let command = hub::upgrade_command(installed_fw, &hub_dir, true);
            let detail = format!(
                "not handed off: {why}; the hub (pid {old_pid}) carries on on the previous                  binary; `{command}` stops and starts it instead"
            );
            reporter.step(Step::Hub, Status::Failed, Some(&detail));
            summary.manual.push(json!({
                "pid": old_pid,
                "cwd": hub_dir.display().to_string(),
                "command": command,
                "notes": [format!(
                    "the hub was not handed off ({why}); this stops and starts it: sessions are                      restored, their processes restarted"
                )],
            }));
            let note = "not restarted: the hub was not handed off (it runs the previous binary)";
            let left = restart::restart(stopped, None, installed_fw, note);
            summary.manual.extend(left.manual);
            let detail = format!(
                "hub workers are not restarted while the hub runs the previous binary; {}",
                if stopped.is_empty() {
                    "nothing was stopped".to_owned()
                } else {
                    format!(
                        "{} process(es) left to the operator (see manual)",
                        stopped.len()
                    )
                }
            );
            reporter.step(Step::Restart, Status::Skipped, Some(&detail));
            return reporter.finish(Status::Skipped, Outcome::UpdatedHubPending, summary);
        }
    };

    // --- restart ---------------------------------------------------------------
    let wants_hub = stopped.iter().any(|d| d.kind != drain::Kind::Terminal);
    let (client, no_hub) = match client {
        Ok(client) => (Some(client), String::new()),
        Err(why) => (None, why),
    };
    let out = restart::restart(
        stopped,
        client.as_ref().filter(|_| wants_hub),
        installed_fw,
        &no_hub,
    );
    let status = restart_status(stopped, &out);
    reporter.step(Step::Restart, status, Some(&out.detail()));
    summary.restarted = out.restarted;
    summary.manual.extend(out.manual);
    reporter.finish(Status::Skipped, Outcome::Updated, summary)
}

/// End a reverted run (exit 1): the hub is not touched; every hub-owned
/// process in `stopped` is restarted through the still-running hub on the
/// previous binary, every terminal one listed under `manual`.
fn finish_reverted(
    mut reporter: Reporter<'_>,
    summary: &mut Summary,
    home: &Home,
    stopped: &[drain::Drainable],
    install_dir: &Path,
) -> i32 {
    let hub_dir = home.hub_dir();
    let wants_hub = stopped.iter().any(|d| d.kind != drain::Kind::Terminal);
    let client = if !wants_hub {
        Err(String::new())
    } else if drain::hub_pid(&hub_dir).is_none() {
        Err("no hub serves this home to restart it in".to_owned())
    } else {
        hub::Client::connect(&hub_dir).map_err(|why| format!("the hub cannot be reached: {why}"))
    };
    let (client, no_hub) = match client {
        Ok(client) => (Some(client), String::new()),
        Err(why) => (None, why),
    };
    let out = restart::restart(stopped, client.as_ref(), &install_dir.join("fw"), &no_hub);
    reporter.step(
        Step::Hub,
        Status::Skipped,
        Some("not touched: the run was reverted"),
    );
    let status = restart_status(stopped, &out);
    let detail = if stopped.is_empty() {
        out.detail()
    } else {
        format!("on the previous binary: {}", out.detail())
    };
    reporter.step(Step::Restart, status, Some(&detail));
    summary.restarted = out.restarted;
    summary.manual.extend(out.manual);
    reporter.finish(Status::Skipped, Outcome::Failed, summary)
}

/// `failed` when a hub restart did not happen, `ok` when one did,
/// `skipped` when none was attempted (nothing stopped, or only `manual`
/// entries).
fn restart_status(stopped: &[drain::Drainable], out: &restart::Restarted) -> Status {
    if !out.failures.is_empty() {
        Status::Failed
    } else if stopped.is_empty() || !out.attempted() {
        Status::Skipped
    } else {
        Status::Ok
    }
}

/// `<id> (<store> run N -> T, orchestrator M -> U[, backup PATH])`, comma-separated.
fn describe_planned(planned: &[Planned], backups: Option<&[Option<PathBuf>]>) -> String {
    planned
        .iter()
        .enumerate()
        .map(
            |(i, plan)| match backups.and_then(|b| b.get(i)).and_then(Option::as_ref) {
                Some(backup) => format!("{}, backup {}", plan.describe(), backup.display()),
                None => plan.describe(),
            },
        )
        .collect::<Vec<_>>()
        .join("; ")
}

/// The preflight / drain refusal while an operator console of this home
/// runs: its supervisor would respawn a stopped worker mid-swap.
fn console_refusal(pid: u32) -> String {
    format!(
        "an operator console of this home is running (pid {pid}); quit it, then run fw self-update again"
    )
}

/// `<pid> <kind> <verb> in <cwd>`, comma-separated.
fn describe(set: &[drain::Drainable]) -> String {
    set.iter()
        .map(|d| {
            format!(
                "{} {} {} in {}",
                d.pid,
                d.kind.name(),
                d.verb,
                d.cwd_display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Put the `previous/` pair back after a failure that follows a complete
/// install (the record could not be written).
fn restore_previous(home: &Home, install_dir: &Path, why: String) -> InstallError {
    match install::restore_previous(home, install_dir) {
        Ok(()) => InstallError::Reverted(why),
        Err(e) => InstallError::RevertFailed(format!("{why}; revert: {e}")),
    }
}

/// `from`: the installed `fw`'s version, sha256, and -- when its bytes are
/// what the last update installed there -- that update's commit.
fn installed_info(install_dir: &Path, record: Option<&Record>) -> BinaryInfo {
    let fw = install_dir.join("fw");
    let sha256 = install::sha256_file(&fw).ok();
    let commit = record
        .filter(|r| r.install_dir == install_dir && sha256.as_deref() == Some(r.fw_sha256.as_str()))
        .map(|r| r.commit.clone());
    BinaryInfo {
        version: source::version_of(&fw).ok(),
        commit,
        sha256,
    }
}
