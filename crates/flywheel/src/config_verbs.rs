//! `fw config` and `fw trust`: the effective config and repo-file trust.
//!
//! * `fw config [--json] [--model M] [--concurrency N]` -- every effective
//!   key with its source (`default|user|repo|personal|env|flag`); the flags
//!   show what a verb given them would run with. Read-only: an untrusted
//!   `.flywheel.toml` is warned about and does not apply.
//! * `fw trust` -- approve the cwd repo's `.flywheel.toml` as it is now
//!   (shown first when stdin is a terminal). `fw trust --revoke` drops every
//!   approval of this project.
//!
//! The loader and trust store live in `flywheel_orchestrator::{config,
//! trust}`.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use flywheel_orchestrator::config::{
    Access, ConfigError, ConfigFlags, REPO_CONFIG_FILENAME, describe_repo_layer, read_repo_file,
};
use flywheel_orchestrator::home::{self, Home, Project};
use flywheel_orchestrator::trust;
use flywheel_orchestrator::workflow::{load_project_config, notice_legacy_layout};

pub const CONFIG_VERB: &str = "config";
pub const TRUST_VERB: &str = "trust";

/// `fw config --help`.
pub const CONFIG_USAGE: &str = "usage: fw config [--json] [--model MODEL] [--concurrency N]

Print the effective config of the project at the working directory: each
key, its value, and where it came from. Precedence: flag > env >
personal > repo > user > default.

  user      $FLYWHEEL_HOME/config.toml (any key)
  repo      <repo>/.flywheel.toml (verify, protected_paths, land,
            project_id; applies only once approved with `fw trust`)
  personal  $FLYWHEEL_HOME/projects/<id>/config.toml (any key)
  env       FLYWHEEL_PG_DSN (store)

options:
  --json             one JSON object: {key: {\"value\", \"source\"}}
  --model MODEL      show the config as a verb given --model would see it
  --concurrency N    likewise for --concurrency
";

/// `fw trust --help`.
pub const TRUST_USAGE: &str = "usage: fw trust [--revoke]

Approve the current repo's .flywheel.toml as it is now. Verbs that run
its commands (worker, autopilot, archive) refuse an unapproved file
unattended; editing the file revokes the approval.

options:
  --revoke   remove every approval for this project
";

/// The cwd's home, project, and repo top level (re-anchored like every
/// store verb).
fn cwd_project() -> Result<(Home, Project, PathBuf), ConfigError> {
    let home = Home::from_env()?;
    let cwd = std::env::current_dir().map_err(|err| {
        ConfigError::Project(home::ProjectError::Invalid(format!(
            "cannot resolve the working directory: {err}"
        )))
    })?;
    let project = home::resolve_project(&home, &cwd)?;
    let repo_root = home::worktree_toplevel(&cwd)?;
    Ok((home, project, repo_root))
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help")
}

/// Run `fw config ...` (`args` excludes the verb).
pub fn run_config(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if wants_help(args) {
        let _ = out.write_all(CONFIG_USAGE.as_bytes());
        return 0;
    }
    let mut json = false;
    let mut flags = ConfigFlags::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        match name {
            "--json" if inline.is_none() => json = true,
            "--model" | "--concurrency" => {
                let Some(value) = inline.or_else(|| rest.next().cloned()) else {
                    return usage_error(err, CONFIG_USAGE, &format!("{name} needs a value"));
                };
                if name == "--model" {
                    flags.model = Some(value);
                } else {
                    match value.trim().parse::<i64>() {
                        Ok(n) if n >= 1 => flags.concurrency = Some(n),
                        _ => {
                            return usage_error(
                                err,
                                CONFIG_USAGE,
                                "--concurrency must be an integer >= 1",
                            );
                        }
                    }
                }
            }
            other => {
                return usage_error(err, CONFIG_USAGE, &format!("unexpected argument: {other}"));
            }
        }
    }
    let (home, project, repo_root) = match cwd_project() {
        Ok(found) => found,
        Err(failure) => return fail(err, CONFIG_VERB, &failure, 2),
    };
    notice_legacy_layout(&repo_root, &home);
    let config = match load_project_config(&home, &project, &repo_root, Access::ReadOnly, &flags) {
        Ok(config) => config,
        Err(failure) => return fail(err, CONFIG_VERB, &failure, 2),
    };
    let written = if json {
        writeln!(out, "{}", config.to_json())
    } else {
        out.write_all(config.render_text().as_bytes())
    };
    i32::from(written.is_err())
}

/// Run `fw trust [--revoke]` (`args` excludes the verb).
pub fn run_trust(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if wants_help(args) {
        let _ = out.write_all(TRUST_USAGE.as_bytes());
        return 0;
    }
    let revoke = match args {
        [] => false,
        [flag] if flag == "--revoke" => true,
        _ => {
            return usage_error(
                err,
                TRUST_USAGE,
                &format!("unexpected arguments: {}", args.join(" ")),
            );
        }
    };
    let (home, project, repo_root) = match cwd_project() {
        Ok(found) => found,
        Err(failure) => return fail(err, TRUST_VERB, &failure, 2),
    };
    if revoke {
        return match trust::revoke(&home, &project.id) {
            Ok(removed) => {
                let _ = writeln!(
                    out,
                    "revoked {removed} approval{} for project {} ({})",
                    if removed == 1 { "" } else { "s" },
                    project.id,
                    project.meta.path.display()
                );
                0
            }
            Err(failure) => fail(err, TRUST_VERB, &failure.into(), 1),
        };
    }
    let repo = match read_repo_file(&repo_root) {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            let _ = writeln!(
                err,
                "fw trust: no {REPO_CONFIG_FILENAME} in {}: nothing to trust",
                repo_root.display()
            );
            return 1;
        }
        Err(failure) => return fail(err, TRUST_VERB, &failure, 2),
    };
    if std::io::stdin().is_terminal() {
        let _ = writeln!(out, "{}:", repo.path.display());
        let _ = writeln!(out, "----");
        let _ = out.write_all(repo.text.as_bytes());
        if !repo.text.ends_with('\n') {
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "----");
        for line in describe_repo_layer(&repo.layer) {
            let _ = writeln!(out, "  it {line}");
        }
    }
    match trust::approve(&home, &project.id, &repo.sha256, &repo.path) {
        Ok(_) => {
            let _ = writeln!(
                out,
                "trusted {} (sha256 {}) for project {}",
                repo.path.display(),
                &repo.sha256[..12],
                project.id
            );
            0
        }
        Err(failure) => fail(err, TRUST_VERB, &failure.into(), 1),
    }
}

fn fail(err: &mut dyn Write, verb: &str, failure: &ConfigError, code: i32) -> i32 {
    let _ = writeln!(err, "fw {verb}: error: {failure}");
    code
}

fn usage_error(err: &mut dyn Write, usage: &str, message: &str) -> i32 {
    let _ = err.write_all(usage.as_bytes());
    let _ = writeln!(err, "error: {message}");
    2
}
