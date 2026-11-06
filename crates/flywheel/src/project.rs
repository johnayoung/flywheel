//! `fw project`: the projects the flywheel home knows.
//!
//! * `fw project ls [--json]` -- every project, `{id, path}`.
//! * `fw project prune` -- remove project dirs whose recorded repo is gone
//!   or no longer carries that id; prints what it removed.
//! * `fw project relink [PATH]` -- re-point the cwd repo's id at an existing
//!   project dir (recovery after a re-clone or a lost `.git/config` key).
//!
//! Identity and the home layout live in `flywheel_orchestrator::home`.

use std::io::Write;

use flywheel_orchestrator::home::{
    Home, ProjectError, list_projects, prune_projects, relink_project,
};

/// The verb name.
pub const PROJECT_VERB: &str = "project";

/// `fw project --help`.
pub const PROJECT_USAGE: &str = "usage: fw project <ls [--json] | prune | relink [PATH]>

Projects are git repos, identified by the id in their .git/config
(flywheel.projectid); each keeps its state under
$FLYWHEEL_HOME/projects/<id>/ (default ~/.flywheel).

  ls [--json]    list every project (JSON: [{\"id\", \"path\"}])
  prune          remove project dirs whose recorded repo is gone or no
                 longer carries that id
  relink [PATH]  point the current repo at an existing project dir (PATH:
                 the dir or its id; bare: the project recording this repo's
                 path)
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
    }
}
