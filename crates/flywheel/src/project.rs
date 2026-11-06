//! `fw project`: the projects the flywheel home knows.
//!
//! * `fw project ls [--json]` -- every project, `{id, path}`.
//! * `fw project prune` -- remove project dirs whose recorded repo is gone
//!   or no longer carries that id; prints what it removed.
//! * `fw project relink [PATH]` -- re-point the cwd repo's id at an existing
//!   project dir (recovery after a re-clone or a lost `.git/config` key).
//! * `fw project path [--root|--specs|--tasks|--logs|--worktrees|--held-out]`
//!   -- the absolute dir of the cwd project (created if missing), so skills
//!   and scripts never hard-code a path.
//!
//! Identity and the home layout live in `flywheel_orchestrator::home`.

use std::io::Write;
use std::path::PathBuf;

use flywheel_orchestrator::config::{Access, ConfigError, ConfigFlags};
use flywheel_orchestrator::home::{
    self, Home, Project, ProjectError, list_projects, prune_projects, relink_project,
};
use flywheel_orchestrator::workflow::{load_project_config, notice_legacy_layout};

/// The verb name.
pub const PROJECT_VERB: &str = "project";

/// `fw project --help`.
pub const PROJECT_USAGE: &str =
    "usage: fw project <ls [--json] | prune | relink [PATH] | path [FLAG]>

Projects are git repos, identified by the id in their .git/config
(flywheel.projectid); each keeps its state under
$FLYWHEEL_HOME/projects/<id>/ (default ~/.flywheel).

  ls [--json]    list every project (JSON: [{\"id\", \"path\"}])
  prune          remove project dirs whose recorded repo is gone or no
                 longer carries that id
  relink [PATH]  point the current repo at an existing project dir (PATH:
                 the dir or its id; bare: the project recording this repo's
                 path)
  path [FLAG]    print the absolute dir of the current repo's project,
                 creating it if missing; one of --root (default), --specs,
                 --tasks, --logs, --worktrees, --held-out
";

/// Run `fw project ...` (`args` excludes the verb); returns the exit code.
pub fn run_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(sub) = args.first() else {
        let _ = err.write_all(PROJECT_USAGE.as_bytes());
        return 2;
    };
    let rest = &args[1..];
    if sub == "-h" || sub == "--help" || rest.iter().any(|a| a == "-h" || a == "--help") {
        let _ = out.write_all(PROJECT_USAGE.as_bytes());
        return 0;
    }
    let result = match sub.as_str() {
        "ls" => match rest {
            [] => ls(false, out),
            [flag] if flag == "--json" => ls(true, out),
            _ => return usage_error(err, &format!("unexpected arguments: {}", rest.join(" "))),
        },
        "prune" => match rest {
            [] => prune(out),
            _ => return usage_error(err, &format!("unexpected arguments: {}", rest.join(" "))),
        },
        "relink" => match rest {
            [] => relink(None, out),
            [target] if !target.starts_with('-') => relink(Some(target), out),
            _ => return usage_error(err, &format!("unexpected arguments: {}", rest.join(" "))),
        },
        "path" => {
            let kind = match rest {
                [] => PathKind::Root,
                [flag] => match PathKind::from_flag(flag) {
                    Some(kind) => kind,
                    None => return usage_error(err, &format!("unknown path flag: {flag}")),
                },
                _ => return usage_error(err, "path takes at most one flag"),
            };
            return match project_path(kind, out) {
                Ok(()) => 0,
                Err(failure) => {
                    let _ = writeln!(err, "fw project: error: {failure}");
                    1
                }
            };
        }
        other => return usage_error(err, &format!("unknown subcommand: {other}")),
    };
    match result {
        Ok(()) => 0,
        Err(failure) => {
            let _ = writeln!(err, "fw project: error: {failure}");
            1
        }
    }
}

/// One `fw project path` directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Root,
    Specs,
    Tasks,
    Logs,
    Worktrees,
    HeldOut,
}

impl PathKind {
    /// Every kind with its flag, in `--help` order.
    pub const ALL: [(PathKind, &'static str); 6] = [
        (PathKind::Root, "--root"),
        (PathKind::Specs, "--specs"),
        (PathKind::Tasks, "--tasks"),
        (PathKind::Logs, "--logs"),
        (PathKind::Worktrees, "--worktrees"),
        (PathKind::HeldOut, "--held-out"),
    ];

    pub fn from_flag(flag: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, name)| *name == flag)
            .map(|(kind, _)| *kind)
    }

    /// This kind's directory under `project`.
    pub fn dir(self, project: &Project) -> PathBuf {
        let paths = &project.paths;
        match self {
            PathKind::Root => paths.root().to_path_buf(),
            PathKind::Specs => paths.specs(),
            PathKind::Tasks => paths.tasks(),
            PathKind::Logs => paths.logs(),
            PathKind::Worktrees => paths.worktrees(),
            PathKind::HeldOut => paths.held_out(),
        }
    }
}

/// The project the working directory's invocations use: the cwd repo's
/// project, re-bound to the `project_id` its effective config names (a
/// shared project dir), loaded read-only -- an untrusted `.flywheel.toml`
/// is warned about and ignored. Writes nothing into the repo tree beyond
/// the `.git/config` id.
pub fn cwd_bound_project() -> Result<Project, ConfigError> {
    let home = Home::from_env()?;
    let cwd = std::env::current_dir().map_err(|err| {
        ConfigError::Project(ProjectError::Invalid(format!(
            "cannot resolve the working directory: {err}"
        )))
    })?;
    let project = home::resolve_project(&home, &cwd)?;
    let repo_root = home::worktree_toplevel(&cwd)?;
    notice_legacy_layout(&repo_root, &home);
    let config = load_project_config(
        &home,
        &project,
        &repo_root,
        Access::ReadOnly,
        &ConfigFlags::default(),
    )?;
    Ok(home::bind_project(
        &home,
        &project,
        &config.project_id.value,
    )?)
}

fn project_path(kind: PathKind, out: &mut dyn Write) -> Result<(), ConfigError> {
    let project = cwd_bound_project()?;
    let dir = kind.dir(&project);
    std::fs::create_dir_all(&dir).map_err(|failure| {
        ConfigError::Project(ProjectError::Io {
            path: dir.clone(),
            message: failure.to_string(),
        })
    })?;
    writeln!(out, "{}", dir.display()).map_err(|e| ConfigError::Project(io(e)))?;
    Ok(())
}

fn usage_error(err: &mut dyn Write, message: &str) -> i32 {
    let _ = err.write_all(PROJECT_USAGE.as_bytes());
    let _ = writeln!(err, "fw project: error: {message}");
    2
}

fn io(failure: std::io::Error) -> ProjectError {
    ProjectError::Invalid(failure.to_string())
}

fn ls(json: bool, out: &mut dyn Write) -> Result<(), ProjectError> {
    let home = Home::from_env()?;
    let rows = list_projects(&home)?;
    if json {
        let text =
            serde_json::to_string(&rows).map_err(|e| ProjectError::Invalid(e.to_string()))?;
        writeln!(out, "{text}").map_err(io)?;
        return Ok(());
    }
    if rows.is_empty() {
        writeln!(out, "(no projects under {})", home.projects_dir().display()).map_err(io)?;
    }
    for row in rows {
        writeln!(out, "{}  {}", row.id, row.path.display()).map_err(io)?;
    }
    Ok(())
}

fn prune(out: &mut dyn Write) -> Result<(), ProjectError> {
    let home = Home::from_env()?;
    let pruned = prune_projects(&home)?;
    if pruned.is_empty() {
        writeln!(out, "nothing to prune").map_err(io)?;
    }
    for entry in pruned {
        let path = entry
            .path
            .map(|p| format!(" ({})", p.display()))
            .unwrap_or_default();
        writeln!(out, "removed {}{path}: {}", entry.id, entry.reason).map_err(io)?;
    }
    Ok(())
}

fn relink(target: Option<&str>, out: &mut dyn Write) -> Result<(), ProjectError> {
    let home = Home::from_env()?;
    let cwd = std::env::current_dir().map_err(io)?;
    let project = relink_project(&home, &cwd, target)?;
    writeln!(
        out,
        "relinked {} -> {} ({})",
        project.meta.path.display(),
        project.id,
        project.paths.root().display()
    )
    .map_err(io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// `flywheel-hub` cannot depend on the orchestrator, so it mirrors the
    /// home rule; this pins its default state dir to `Home::hub_dir`.
    #[test]
    fn the_hub_default_state_dir_is_the_homes_hub_dir() {
        use flywheel_orchestrator::home::home_from;
        use std::path::Path;
        let cwd = Path::new("/work");
        let cases: [(Option<&str>, Option<&str>); 6] = [
            (Some("/x/fw"), Some("/home/u")),
            (Some("rel/fw"), Some("/home/u")),
            (Some(""), Some("/home/u")),
            (None, Some("/home/u")),
            (None, Some("")),
            (None, None),
        ];
        for (flywheel_home, home) in cases {
            assert_eq!(
                flywheel_hub::paths::default_state_dir_from(flywheel_home, home, cwd),
                home_from(flywheel_home, home, cwd).map(|root| Home::at(root).hub_dir()),
                "FLYWHEEL_HOME={flywheel_home:?} HOME={home:?}"
            );
        }
    }

    #[test]
    fn help_and_usage_errors() {
        let (code, out, _) = run(&["--help"]);
        assert_eq!((code, out.as_str()), (0, PROJECT_USAGE));
        assert_eq!(run(&["ls", "--help"]).0, 0);
        assert_eq!(run(&[]).0, 2);
        let (code, _, err) = run(&["frob"]);
        assert_eq!(code, 2);
        assert!(err.contains("unknown subcommand: frob"), "{err}");
        assert_eq!(run(&["ls", "--yaml"]).0, 2);
        assert_eq!(run(&["prune", "x"]).0, 2);
        assert_eq!(run(&["relink", "a", "b"]).0, 2);
        let (code, _, err) = run(&["path", "--nope"]);
        assert_eq!(code, 2);
        assert!(err.contains("unknown path flag: --nope"), "{err}");
        assert_eq!(run(&["path", "--specs", "--tasks"]).0, 2);
    }

    #[test]
    fn path_flags_round_trip() {
        for (kind, flag) in PathKind::ALL {
            assert_eq!(PathKind::from_flag(flag), Some(kind));
            assert!(PROJECT_USAGE.contains(flag), "{flag} missing from usage");
        }
        assert_eq!(PathKind::from_flag("--db"), None);
    }
}
