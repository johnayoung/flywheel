//! The `init` verb: get a repo ready for flywheel without writing into it.
//!
//! `fw init` resolves (creating on first sight) the repo's home project,
//! creates the user config (`$FLYWHEEL_HOME/config.toml`) when it is
//! missing, installs the managed fw-* Claude Code skills at USER level
//! (`$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`), runs the postgres
//! preflight when the effective store is postgres, detects a likely verify
//! command from the repo's toolchain files, and PRINTS a suggested
//! `.flywheel.toml` for the operator to commit. The only byte it writes
//! inside the repo is the project id in `.git/config`.
//!
//! Every input goes through an injectable [`InitHost`] (cwd, environment,
//! home, postgres) so the verb is testable without a terminal.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{
    Access, ConfigFlags, Interaction, REPO_CONFIG_FILENAME, StoreSetting, load_config,
    read_repo_file,
};
use crate::home::{self, Home, ProjectError};
use crate::pg_preflight::{
    PreflightOptions, PreflightOutcome, format_report, run_postgres_preflight,
};
use crate::skills::{SKILL_NAMES, install_skills, settings_from_policy};
use crate::store_factory::PG_DSN_ENV;
use crate::store_registry::{Environ, environ_from_process};
use crate::trust;

use super::WorkflowError;
use super::args::InitArgs;

/// The copy-pasteable pointer `init` prints (never writes) for an
/// operator's CLAUDE.md (spec 00072 D-3).
pub const INIT_CLAUDEMD_POINTER: &str = "This repository runs on flywheel: load the `flywheel-ops` skill to \
operate the loop, and run `fw docs <topic>` for authoritative flywheel documentation.";

/// The user config `init` creates when none exists: every key commented
/// out at its built-in default.
pub const USER_CONFIG_TEMPLATE: &str = "\
# flywheel user config. Every key is optional; `fw config` shows the
# effective value of each and where it came from. Per-project overrides go
# in $FLYWHEEL_HOME/projects/<id>/config.toml; a repo may commit verify,
# protected_paths, land, and project_id in .flywheel.toml.
#
# model = \"claude-opus-4-8\"        # agent model (default: the agent CLI's own)
# concurrency = 1                   # worker pool size
# store = \"sqlite\"                  # or a postgres DSN shared by every project
# transcript_retention_days = 90
# verify = \"\"                       # the command every landing must pass
# protected_paths = []              # globs a task may never change
# land = \"merge\"                    # or \"pr\"
";

// --- Host seam ---------------------------------------------------------------------

/// The environment `init` runs against. The production host reads the
/// process cwd, environment, `$FLYWHEEL_HOME`, and postgres; tests script
/// each.
pub trait InitHost {
    /// The working directory (any directory inside the repo).
    fn root(&self) -> PathBuf;
    fn environ(&self) -> Environ;
    /// The flywheel home.
    fn home(&self) -> Result<Home, ProjectError>;
    fn postgres_preflight(
        &self,
        dsn: &str,
        schema: &str,
        options: PreflightOptions,
    ) -> PreflightOutcome;
}

/// The real host: the process cwd, environment, home, and postgres.
#[derive(Debug, Default)]
pub struct ProductionInitHost;

impl ProductionInitHost {
    pub fn new() -> Self {
        Self
    }
}

impl InitHost for ProductionInitHost {
    fn root(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_default()
    }

    fn environ(&self) -> Environ {
        environ_from_process()
    }

    fn home(&self) -> Result<Home, ProjectError> {
        Home::from_env()
    }

    fn postgres_preflight(
        &self,
        dsn: &str,
        schema: &str,
        options: PreflightOptions,
    ) -> PreflightOutcome {
        run_postgres_preflight(dsn, schema, options)
    }
}

// --- Verify-command detection ----------------------------------------------------------

/// A verify command detected from the repo's toolchain files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedVerify {
    pub command: &'static str,
    /// The file that gave it away.
    pub evidence: &'static str,
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// npm's placeholder `test` script (`npm init` writes it): not a test.
const NPM_PLACEHOLDER_TEST: &str = "no test specified";

fn package_json_has_test(root: &Path) -> bool {
    let Some(text) = read_text(&root.join("package.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| !script.trim().is_empty() && !script.contains(NPM_PLACEHOLDER_TEST))
}

fn uses_pytest(root: &Path) -> Option<&'static str> {
    if root.join("pytest.ini").is_file() {
        return Some("pytest.ini");
    }
    if root.join("conftest.py").is_file() {
        return Some("conftest.py");
    }
    if read_text(&root.join("pyproject.toml")).is_some_and(|t| t.contains("pytest")) {
        return Some("pyproject.toml");
    }
    if read_text(&root.join("setup.cfg")).is_some_and(|t| t.contains("[tool:pytest]")) {
        return Some("setup.cfg");
    }
    if read_text(&root.join("tox.ini")).is_some_and(|t| t.contains("[pytest]")) {
        return Some("tox.ini");
    }
    None
}

fn makefile_has_test_target(root: &Path) -> bool {
    ["Makefile", "makefile", "GNUmakefile"].iter().any(|name| {
        read_text(&root.join(name)).is_some_and(|text| {
            text.lines().any(|line| {
                line.strip_prefix("test")
                    .is_some_and(|rest| rest.starts_with(':') && !rest.starts_with(":="))
            })
        })
    })
}

/// The likely verify command for the repo at `root`, first match wins:
/// `Cargo.toml` -> `cargo test`, a `package.json` with a real `test`
/// script -> `npm test`, pytest configuration -> `pytest`, `go.mod` ->
/// `go test ./...`, a Makefile `test` target -> `make test`.
pub fn detect_verify(root: &Path) -> Option<DetectedVerify> {
    if root.join("Cargo.toml").is_file() {
        return Some(DetectedVerify {
            command: "cargo test",
            evidence: "Cargo.toml",
        });
    }
    if package_json_has_test(root) {
        return Some(DetectedVerify {
            command: "npm test",
            evidence: "package.json",
        });
    }
    if let Some(evidence) = uses_pytest(root) {
        return Some(DetectedVerify {
            command: "pytest",
            evidence,
        });
    }
    if root.join("go.mod").is_file() {
        return Some(DetectedVerify {
            command: "go test ./...",
            evidence: "go.mod",
        });
    }
    if makefile_has_test_target(root) {
        return Some(DetectedVerify {
            command: "make test",
            evidence: "Makefile",
        });
    }
    None
}

/// The `.flywheel.toml` `init` suggests (prints, never writes).
pub fn render_suggested_repo_file(verify: Option<&DetectedVerify>) -> String {
    let verify_line = match verify {
        Some(found) => format!("verify = \"{}\"\n", found.command),
        None => "# verify = \"<the command every landing must pass>\"\n".to_owned(),
    };
    format!(
        "{verify_line}# protected_paths = [\".github/**\", \"{REPO_CONFIG_FILENAME}\"]\n\
         # land = \"merge\"    # or \"pr\"\n"
    )
}

// --- Skills ------------------------------------------------------------------------------

/// The user-level skills directory: `$CLAUDE_CONFIG_DIR/skills`, else
/// `$HOME/.claude/skills`; `None` when neither is set.
pub fn user_skills_dir(environ: &Environ) -> Option<PathBuf> {
    let non_empty = |key: &str| environ.get(key).filter(|v| !v.is_empty());
    if let Some(dir) = non_empty("CLAUDE_CONFIG_DIR") {
        return Some(Path::new(dir).join("skills"));
    }
    non_empty("HOME").map(|home| Path::new(home).join(".claude").join("skills"))
}

fn install_user_skills(dest: &Path, out: &mut dyn Write) -> Result<(), WorkflowError> {
    let report = install_skills(dest, &settings_from_policy(None), &SKILL_NAMES)
        .map_err(|err| WorkflowError::Message(err.to_string()))?;
    writeln!(
        out,
        "skills:  {} (user level, shared by every repo)",
        dest.display()
    )?;
    for path in &report.created {
        writeln!(out, "created: {}", path.display())?;
    }
    for path in &report.updated {
        writeln!(out, "updated: {}", path.display())?;
    }
    for path in &report.skipped {
        writeln!(
            out,
            "exists:  {} (user-owned, left untouched)",
            path.display()
        )?;
    }
    Ok(())
}

// --- Agent auth ----------------------------------------------------------------------------

/// Print whether agent credentials are in place: `ANTHROPIC_API_KEY`, else a
/// `claude login` credential file, else a non-blocking warning.
pub fn report_agent_auth(environ: &Environ, out: &mut dyn Write) -> std::io::Result<()> {
    if environ
        .get("ANTHROPIC_API_KEY")
        .is_some_and(|v| !v.is_empty())
    {
        return writeln!(out, "agent auth: ANTHROPIC_API_KEY is set");
    }
    let creds = environ
        .get("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| Path::new(home).join(".claude").join(".credentials.json"));
    if creds.is_some_and(|p| p.is_file()) {
        return writeln!(out, "agent auth: detected a `claude login` credential file");
    }
    writeln!(
        out,
        "warning: no agent credentials detected; set ANTHROPIC_API_KEY (or run `claude login`) \
         before the first worker run"
    )
}

// --- The verb ------------------------------------------------------------------------------

/// The next-steps epilogue every successful init prints.
pub fn print_init_next_steps(out: &mut dyn Write, skills_installed: bool) -> std::io::Result<()> {
    writeln!(out, "Next steps:")?;
    writeln!(
        out,
        "  1. Drop one JSON file per task into $(fw project path --tasks)/active/<phase>/ \
         ('goal' and 'graders' are the only required fields)."
    )?;
    if skills_installed {
        writeln!(
            out,
            "     Or, in Claude Code: /fw-spec to spec a feature, /fw-plan to queue tasks."
        )?;
    }
    writeln!(
        out,
        "  2. Authenticate the agent: set ANTHROPIC_API_KEY (or run `claude login`) before the \
         first worker run."
    )?;
    writeln!(out, "  3. Run: fw worker")?;
    writeln!(out, "  4. Watch: fw status / fw live")?;
    writeln!(out)?;
    writeln!(
        out,
        "To help future Claude Code sessions discover flywheel, paste this line into your \
         CLAUDE.md (init never edits it for you):"
    )?;
    writeln!(out, "  {INIT_CLAUDEMD_POINTER}")
}

/// `fw init`. Exit 0 on success, 2 for a directory outside a git repo or
/// a blocking postgres preflight.
pub fn cmd_init(
    args: &InitArgs,
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let cwd = host.root();
    let home = match host.home() {
        Ok(home) => home,
        Err(failure) => {
            writeln!(err, "error: {failure}")?;
            return Ok(2);
        }
    };
    let resolved = home::resolve_project(&home, &cwd)
        .and_then(|project| home::worktree_toplevel(&cwd).map(|root| (project, root)));
    let (project, repo_root) = match resolved {
        Ok(resolved) => resolved,
        Err(failure) => {
            writeln!(err, "error: {failure}")?;
            return Ok(2);
        }
    };
    if let Some(line) = super::legacy_layout_notice(&repo_root, &home) {
        writeln!(err, "{line}")?;
    }
    writeln!(
        out,
        "project: {} ({})",
        project.id,
        project.paths.root().display()
    )?;

    let config_file = home.config_file();
    if config_file.exists() {
        writeln!(out, "exists:  {}", config_file.display())?;
    } else {
        if let Some(parent) = config_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config_file, USER_CONFIG_TEMPLATE)?;
        writeln!(out, "created: {}", config_file.display())?;
    }

    let environ = host.environ();
    let env_dsn = environ
        .get(PG_DSN_ENV)
        .filter(|v| !v.trim().is_empty())
        .cloned();
    let loaded = load_config(
        &home,
        &project,
        &repo_root,
        Access::ReadOnly,
        &ConfigFlags::default(),
        env_dsn,
        Interaction::Unattended,
    );
    let config = match loaded {
        Ok(loaded) => {
            for warning in &loaded.warnings {
                writeln!(err, "{warning}")?;
            }
            Some(loaded.config)
        }
        Err(failure) => {
            writeln!(err, "warning: {failure}")?;
            None
        }
    };
    if let Some(StoreSetting::Postgres(dsn)) = config.as_ref().map(|c| &c.store.value) {
        let outcome = host.postgres_preflight(
            dsn.expose(),
            "public",
            PreflightOptions {
                provision: args.provision,
                allow_unverified: args.allow_unverified,
            },
        );
        writeln!(out, "{}", format_report(&outcome.checks))?;
        if outcome.blocked() && !args.allow_unverified {
            writeln!(
                err,
                "error: postgres preflight found blocking issues (see the report above); fix \
                 them, or re-run with --allow-unverified"
            )?;
            return Ok(2);
        }
    }

    let skills_installed = if args.no_skills {
        false
    } else {
        match user_skills_dir(&environ) {
            Some(dest) => {
                install_user_skills(&dest, out)?;
                true
            }
            None => {
                writeln!(
                    out,
                    "skills:  skipped (neither CLAUDE_CONFIG_DIR nor HOME is set)"
                )?;
                false
            }
        }
    };
    report_agent_auth(&environ, out)?;

    writeln!(out)?;
    match read_repo_file(&repo_root) {
        Ok(Some(repo_file)) => {
            let trusted = trust::is_trusted(&home, &project.id, &repo_file.sha256).unwrap_or(false);
            let state = if trusted {
                "trusted"
            } else {
                "not trusted yet: run `fw trust`"
            };
            writeln!(out, "{REPO_CONFIG_FILENAME}: present ({state})")?;
        }
        Ok(None) => {
            let detected = detect_verify(&repo_root);
            match &detected {
                Some(found) => writeln!(
                    out,
                    "verify:  detected `{}` (from {})",
                    found.command, found.evidence
                )?,
                None => writeln!(
                    out,
                    "verify:  no test command detected; fill in your own below"
                )?,
            }
            writeln!(
                out,
                "Suggested {REPO_CONFIG_FILENAME} (init writes nothing into the repo: commit it \
                 yourself, then run `fw trust`):"
            )?;
            writeln!(out)?;
            for line in render_suggested_repo_file(detected.as_ref()).lines() {
                writeln!(out, "  {line}")?;
            }
        }
        Err(failure) => writeln!(err, "warning: {failure}")?,
    }
    writeln!(out)?;
    print_init_next_steps(out, skills_installed)?;
    Ok(0)
}

#[cfg(test)]
pub(crate) mod test_host {
    //! A scripted [`InitHost`] for the verb tests.

    use super::*;

    pub struct ScriptedHost {
        pub root: PathBuf,
        pub home: PathBuf,
        pub environ: Environ,
        pub preflight: Option<PreflightOutcome>,
        pub preflight_calls: std::cell::RefCell<Vec<String>>,
    }

    impl Default for ScriptedHost {
        /// A host whose home is a throwaway path under the temp dir (only
        /// an actual `init` run would create it).
        fn default() -> Self {
            Self {
                root: std::env::temp_dir(),
                home: std::env::temp_dir().join("flywheel-init-test-home-unused"),
                environ: Environ::new(),
                preflight: None,
                preflight_calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl InitHost for ScriptedHost {
        fn root(&self) -> PathBuf {
            self.root.clone()
        }
        fn environ(&self) -> Environ {
            self.environ.clone()
        }
        fn home(&self) -> Result<Home, ProjectError> {
            Ok(Home::at(&self.home))
        }
        fn postgres_preflight(&self, dsn: &str, _: &str, _: PreflightOptions) -> PreflightOutcome {
            self.preflight_calls.borrow_mut().push(dsn.to_owned());
            self.preflight.clone().unwrap_or(PreflightOutcome {
                checks: Vec::new(),
                provisioned: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_host::ScriptedHost;
    use super::*;
    use crate::pg_preflight::{CheckStatus, PreflightCheck};
    use std::process::Command;
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    }

    /// A temp dir holding a fresh git repo (`repo/`), a home (`home/`), and
    /// a user home (`user/`).
    fn setup() -> (TempDir, ScriptedHost) {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@flywheel.invalid"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "root"]);
        let user = tmp.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        let mut environ = Environ::new();
        environ.insert("HOME".into(), user.display().to_string());
        let host = ScriptedHost {
            root: repo,
            home: tmp.path().join("home"),
            environ,
            preflight: None,
            preflight_calls: std::cell::RefCell::new(Vec::new()),
        };
        (tmp, host)
    }

    fn run(args: InitArgs, host: &mut ScriptedHost) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cmd_init(&args, host, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn porcelain(repo: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["status", "--porcelain", "--ignored"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn init_writes_nothing_into_the_repo_and_suggests_a_repo_file() {
        let (tmp, mut host) = setup();
        std::fs::write(host.root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        git(&host.root, &["add", "Cargo.toml"]);
        git(&host.root, &["commit", "-q", "-m", "cargo"]);
        let (code, out, _) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 0, "{out}");
        assert_eq!(porcelain(&host.root), "");
        let config = tmp.path().join("home").join("config.toml");
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            USER_CONFIG_TEMPLATE
        );
        assert!(out.contains(&format!("created: {}", config.display())));
        assert!(out.contains("verify:  detected `cargo test` (from Cargo.toml)"));
        assert!(out.contains("  verify = \"cargo test\"\n"));
        assert!(out.contains("then run `fw trust`"));
        // Skills land at user level, never in the repo.
        let skill = tmp.path().join("user/.claude/skills/fw-spec/SKILL.md");
        assert!(skill.is_file());
        assert!(!host.root.join(".claude").exists());
        // The project dir exists and the id is in .git/config.
        assert!(out.starts_with("project: "));
        assert!(home::stored_project_id(&host.root).unwrap().is_some());

        // A second run keeps the user config and is still clean.
        std::fs::write(&config, "concurrency = 2\n").unwrap();
        let (code, out, _) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 0);
        assert!(out.contains(&format!("exists:  {}", config.display())));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "concurrency = 2\n"
        );
        assert_eq!(porcelain(&host.root), "");
    }

    #[test]
    fn the_user_config_template_is_a_valid_user_layer() {
        crate::config::parse_layer(
            USER_CONFIG_TEMPLATE,
            Path::new("config.toml"),
            crate::config::LayerKind::User,
        )
        .unwrap();
    }

    #[test]
    fn skills_honor_claude_config_dir_and_no_skills() {
        let (tmp, mut host) = setup();
        let claude = tmp.path().join("claude-config");
        host.environ
            .insert("CLAUDE_CONFIG_DIR".into(), claude.display().to_string());
        let (code, out, _) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 0);
        assert!(claude.join("skills/flywheel-ops/SKILL.md").is_file());
        assert!(out.contains("(user level, shared by every repo)"));

        let (tmp, mut host) = setup();
        let (code, out, _) = run(
            InitArgs {
                no_skills: true,
                ..InitArgs::default()
            },
            &mut host,
        );
        assert_eq!(code, 0);
        assert!(!out.contains("skills:"));
        assert!(!tmp.path().join("user/.claude").exists());
    }

    #[test]
    fn a_present_repo_file_is_reported_not_suggested() {
        let (_tmp, mut host) = setup();
        std::fs::write(host.root.join(".flywheel.toml"), "verify = \"true\"\n").unwrap();
        let (code, out, _) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 0);
        assert!(out.contains(".flywheel.toml: present (not trusted yet: run `fw trust`)"));
        assert!(!out.contains("Suggested"));
    }

    #[test]
    fn outside_a_git_repo_init_refuses_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut host = ScriptedHost {
            root: tmp.path().to_path_buf(),
            home: tmp.path().join("home"),
            environ: Environ::new(),
            preflight: None,
            preflight_calls: std::cell::RefCell::new(Vec::new()),
        };
        let (code, _, err) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 2);
        assert!(err.to_lowercase().contains("git"), "{err}");
        assert!(!tmp.path().join("home").join("config.toml").exists());
    }

    #[test]
    fn the_legacy_layout_is_noticed_and_left_alone() {
        let (_tmp, mut host) = setup();
        let legacy = "[source]\nkind = \"directory\"\n";
        std::fs::write(host.root.join("flywheel.toml"), legacy).unwrap();
        let (code, _, err) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 0);
        assert!(
            err.contains("flywheel.toml") && err.contains("ignored"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(host.root.join("flywheel.toml")).unwrap(),
            legacy
        );
    }

    #[test]
    fn a_postgres_store_runs_the_preflight_and_a_block_exits_two() {
        let (tmp, mut host) = setup();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        std::fs::write(
            tmp.path().join("home").join("config.toml"),
            "store = \"postgres://u:pw@db.invalid/flywheel\"\n",
        )
        .unwrap();
        host.preflight = Some(PreflightOutcome {
            checks: vec![PreflightCheck::new(
                "connect",
                CheckStatus::Block,
                "unreachable",
            )],
            provisioned: false,
        });
        let (code, _, err) = run(InitArgs::default(), &mut host);
        assert_eq!(code, 2);
        assert!(err.contains("--allow-unverified"));
        assert_eq!(
            host.preflight_calls.borrow().as_slice(),
            ["postgres://u:pw@db.invalid/flywheel"]
        );
        let (code, _, _) = run(
            InitArgs {
                allow_unverified: true,
                ..InitArgs::default()
            },
            &mut host,
        );
        assert_eq!(code, 0);
    }

    fn detect_in(files: &[(&str, &str)]) -> Option<&'static str> {
        let tmp = TempDir::new().unwrap();
        for (name, body) in files {
            std::fs::write(tmp.path().join(name), body).unwrap();
        }
        detect_verify(tmp.path()).map(|d| d.command)
    }

    #[test]
    fn verify_detection_covers_each_toolchain() {
        assert_eq!(detect_in(&[]), None);
        assert_eq!(detect_in(&[("Cargo.toml", "")]), Some("cargo test"));
        assert_eq!(
            detect_in(&[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)]),
            Some("npm test")
        );
        assert_eq!(
            detect_in(&[(
                "package.json",
                r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#
            )]),
            None
        );
        assert_eq!(
            detect_in(&[("pyproject.toml", "[tool.pytest.ini_options]\n")]),
            Some("pytest")
        );
        assert_eq!(detect_in(&[("pytest.ini", "")]), Some("pytest"));
        assert_eq!(
            detect_in(&[("go.mod", "module x\n")]),
            Some("go test ./...")
        );
        assert_eq!(
            detect_in(&[("Makefile", "build:\n\tcc x.c\ntest: build\n\t./x\n")]),
            Some("make test")
        );
        assert_eq!(detect_in(&[("Makefile", "tests:\n\t./x\n")]), None);
        assert_eq!(
            detect_in(&[("Cargo.toml", ""), ("Makefile", "test:\n")]),
            Some("cargo test")
        );
    }

    #[test]
    fn the_suggested_file_is_a_valid_repo_layer() {
        let parse = |text: &str| {
            crate::config::parse_layer(
                text,
                Path::new(".flywheel.toml"),
                crate::config::LayerKind::Repo,
            )
            .unwrap()
        };
        assert_eq!(parse(&render_suggested_repo_file(None)).verify, None);
        let layer = parse(&render_suggested_repo_file(Some(&DetectedVerify {
            command: "go test ./...",
            evidence: "go.mod",
        })));
        assert_eq!(layer.verify, Some(Some("go test ./...".to_owned())));
    }

    #[test]
    fn user_skills_dir_prefers_claude_config_dir() {
        let mut environ = Environ::new();
        assert_eq!(user_skills_dir(&environ), None);
        environ.insert("HOME".into(), "/h".into());
        assert_eq!(
            user_skills_dir(&environ),
            Some(PathBuf::from("/h/.claude/skills"))
        );
        environ.insert("CLAUDE_CONFIG_DIR".into(), "/c".into());
        assert_eq!(user_skills_dir(&environ), Some(PathBuf::from("/c/skills")));
    }
}
