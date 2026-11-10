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
//! Every refusal (exit 3) happens before anything outside the lock file
//! changes. The drain, backup, db-upgrade, hub, and restart steps report
//! `skipped` (`planned` under `--dry-run`): this build does not perform
//! them.
//!
//! Spec: `docs/cli.md` (`self-update`).

pub mod args;
pub mod install;
pub mod output;
pub mod source;

use flywheel_orchestrator::home::Home;
use install::{InstallError, Record};
use output::{BinaryInfo, Outcome, Reporter, SourceInfo, Status, Step, Summary};
use std::io::Write;
use std::path::{Path, PathBuf};

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

    // --- dry run ---------------------------------------------------------------
    if args.dry_run {
        let detail = format!(
            "would replace fw and flywheel in {} (previous pair to {})",
            install_dir.display(),
            home.self_update_previous_dir().display()
        );
        reporter.step(Step::Drain, Status::Planned, None);
        reporter.step(Step::Backup, Status::Planned, None);
        reporter.step(Step::Install, Status::Planned, Some(&detail));
        return reporter.finish(Status::Planned, Outcome::Planned, &summary);
    }

    // --- install ---------------------------------------------------------------
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
        let record = Record {
            commit: checkout.head.clone(),
            checkout: checkout.root.clone(),
            install_dir: install_dir.clone(),
            version: version.clone(),
            fw_sha256: fw_sha.clone(),
            flywheel_sha256: flywheel_sha.clone(),
            installed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        install::write_record(&home, &record)
            .map_err(|why| restore_previous(&home, &install_dir, why))
    });
    match installed {
        Ok(()) => {
            let detail = format!(
                "fw and flywheel {version} ({short}) in {}",
                install_dir.display()
            );
            reporter.step(Step::Install, Status::Ok, Some(&detail));
            reporter.finish(Status::Skipped, Outcome::Updated, &summary)
        }
        Err(InstallError::Reverted(why)) => {
            summary.to = summary.from.clone();
            let detail = format!("{why}; the previous pair is back in place");
            reporter.step(Step::Install, Status::Reverted, Some(&detail));
            reporter.finish(Status::Skipped, Outcome::Failed, &summary)
        }
        Err(InstallError::NothingChanged(why)) => {
            summary.to = summary.from.clone();
            reporter.step(Step::Install, Status::Failed, Some(&why));
            reporter.finish(Status::Skipped, Outcome::Failed, &summary)
        }
        Err(InstallError::RevertFailed(why)) => {
            let detail = format!(
                "{why}; restore by hand from {}",
                home.self_update_previous_dir().display()
            );
            reporter.step(Step::Install, Status::Failed, Some(&detail));
            reporter.finish(Status::Skipped, Outcome::Failed, &summary)
        }
    }
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
