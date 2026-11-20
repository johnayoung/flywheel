//! A set task's held-out oracle, made runnable: its files copied out of the
//! set into a location of the caller's choosing (the project-home token
//! replaced by that location), run against a clean tree of a given commit,
//! out of band -- the same verdict rule as the held-out gate: every command
//! exits zero, else fail.
//!
//! Oracles run hermetic: `FLYWHEEL_HOME` points at an empty directory (so
//! no oracle reads the live project home), `CARGO_TARGET_DIR` at the
//! caller's build cache outside the tree (an oracle that ignores it and
//! looks for `target/debug/...` fails V2, the noise floor's 00083 case),
//! and the hub-session variables are removed. An oracle may write
//! per-check scores to `$FLYWHEEL_EVAL_REWARD`
//! (`{"checks": [{"id", "passed", "note"}]}`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::git;
use crate::mine::PROJECT_HOME_TOKEN;
use crate::proc::{self, Locking, Ran, Spec};
use crate::set::{SetDir, SetError, files_under, io};

/// The variable an oracle may write per-check scores to.
pub const REWARD_ENV: &str = "FLYWHEEL_EVAL_REWARD";

/// Environment an oracle must not inherit.
pub const SCRUBBED_ENV: &[&str] = &[
    "FLYWHEEL_HOME",
    "FLYWHEEL_PG_DSN",
    "DATABASE_URL",
    "FLYWHEEL_HUB_SESSION",
    "FLYWHEEL_HUB_REPORT_SOCKET",
    "FLYWHEEL_HUB_WORKSPACE",
    "FLYWHEEL_HUB_REPORT_SECRET",
    "FLYWHEEL_BATCH",
    "FLYWHEEL_BATCH_BUDGET",
    "FW",
    "CARGO_TARGET_DIR",
];

/// A materialized oracle: where its files are and what to run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Oracle {
    pub root: PathBuf,
    /// `(name, command)` in registration order.
    pub commands: Vec<(Option<String>, String)>,
}

#[derive(Deserialize)]
struct Registration {
    graders: Vec<serde_json::Value>,
}

/// Copy `id`'s oracle out of `set` into `dest` (replaced), substituting the
/// project-home token with `dest`, and read its command graders. A
/// registration with a non-command grader is refused (the held-out gate's
/// rule: command graders only).
pub fn materialize(set: &SetDir, id: &str, dest: &Path) -> Result<Oracle, SetError> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(io(dest))?;
    }
    let src = set.oracle_dir(id).join("home");
    let token_value = dest.display().to_string();
    for rel in files_under(&src)? {
        let from = src.join(&rel);
        let to = dest.join(&rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let bytes = std::fs::read(&from).map_err(io(&from))?;
        match String::from_utf8(bytes) {
            Ok(text) => std::fs::write(&to, text.replace(PROJECT_HOME_TOKEN, &token_value))
                .map_err(io(&to))?,
            Err(raw) => std::fs::write(&to, raw.into_bytes()).map_err(io(&to))?,
        }
        let mode = std::fs::metadata(&from).map_err(io(&from))?.permissions();
        std::fs::set_permissions(&to, mode).map_err(io(&to))?;
        // A frozen set is read-only; the copy must be writable to replace.
        let mut writable = std::fs::metadata(&to).map_err(io(&to))?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        std::fs::set_permissions(&to, writable).map_err(io(&to))?;
    }
    std::fs::create_dir_all(dest).map_err(io(dest))?;
    let reg_path = set.registration(id);
    let text = std::fs::read_to_string(&reg_path).map_err(io(&reg_path))?;
    let text = text.replace(PROJECT_HOME_TOKEN, &token_value);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| SetError::Invalid(format!("{}: {e}", reg_path.display())))?;
    let reg: Registration = match value {
        serde_json::Value::Array(graders) => Registration { graders },
        other => serde_json::from_value(other)
            .map_err(|e| SetError::Invalid(format!("{}: {e}", reg_path.display())))?,
    };
    let mut commands = Vec::new();
    for g in reg.graders {
        if g.get("type").and_then(|t| t.as_str()) != Some("command") {
            return Err(SetError::Invalid(format!(
                "{}: held-out graders must be command graders",
                reg_path.display()
            )));
        }
        let run = g.get("run").and_then(|r| r.as_str()).ok_or_else(|| {
            SetError::Invalid(format!("{}: a grader without run", reg_path.display()))
        })?;
        commands.push((
            g.get("name").and_then(|n| n.as_str()).map(str::to_owned),
            run.to_owned(),
        ));
    }
    if commands.is_empty() {
        return Err(SetError::Invalid(format!(
            "{}: no graders",
            reg_path.display()
        )));
    }
    Ok(Oracle {
        root: dest.to_owned(),
        commands,
    })
}

/// One per-check score an oracle reported.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckScore {
    pub id: String,
    pub passed: bool,
    #[serde(default)]
    pub note: String,
}

/// An oracle's verdict on one tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub passed: bool,
    /// The first failing command's outcome (or the last command's).
    pub ran: Ran,
    pub secs: f64,
    /// Scores the oracle wrote to `$FLYWHEEL_EVAL_REWARD`.
    pub checks: Vec<CheckScore>,
    /// `exit 1 (held-out-oracle:x)`.
    pub detail: String,
}

/// How an oracle runs.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The build cache (`CARGO_TARGET_DIR`), outside the tree.
    pub target_dir: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub locking: Locking,
    /// The combined output of every command.
    pub log: Option<PathBuf>,
    /// Extra environment (tests; V5's noise).
    pub env: Vec<(String, String)>,
}

/// Run `oracle` with `tree` as the working directory. The first failing
/// command stops the rest.
pub fn run(oracle: &Oracle, tree: &Path, options: &RunOptions) -> std::io::Result<Verdict> {
    let empty_home = tempfile::tempdir()?;
    let reward = tempfile::NamedTempFile::new()?;
    let mut env = vec![
        (
            "FLYWHEEL_HOME".to_owned(),
            empty_home.path().display().to_string(),
        ),
        (REWARD_ENV.to_owned(), reward.path().display().to_string()),
    ];
    if let Some(target) = &options.target_dir {
        env.push(("CARGO_TARGET_DIR".into(), target.display().to_string()));
    }
    env.extend(options.env.iter().cloned());
    let mut total = 0.0;
    let mut last = None;
    let mut detail = String::new();
    let mut passed = true;
    for (name, cmd) in &oracle.commands {
        let ran = proc::run(&Spec {
            cwd: Some(tree),
            env: env.clone(),
            unset: SCRUBBED_ENV.iter().map(|s| (*s).to_owned()).collect(),
            timeout: options.timeout,
            log: options.log.as_deref(),
            locking: options.locking.clone(),
            ..Spec::shell(cmd)
        })?;
        total += ran.secs;
        let ok = ran.passed();
        if !ok {
            detail = format!(
                "{} ({})",
                ran.describe(),
                name.as_deref().unwrap_or(cmd.as_str())
            );
        }
        last = Some(ran);
        if !ok {
            passed = false;
            break;
        }
    }
    let checks = std::fs::read_to_string(reward.path())
        .ok()
        .filter(|t| !t.trim().is_empty())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("checks").cloned())
        .and_then(|c| serde_json::from_value::<Vec<CheckScore>>(c).ok())
        .unwrap_or_default();
    Ok(Verdict {
        passed,
        ran: last.expect("an oracle has at least one command"),
        secs: total,
        checks,
        detail: if passed { "pass".into() } else { detail },
    })
}

/// A reusable clean checkout for grading: a `--no-checkout` local clone of
/// `repo` (objects hardlinked), moved to any commit, every untracked file
/// removed but the build cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradeTree {
    pub path: PathBuf,
    repo: PathBuf,
    /// The base branch name and the task's base commit it is pinned to.
    base: Option<(String, String)>,
}

impl GradeTree {
    /// The tree at `path`, cloned from `repo` on first use.
    pub fn open(repo: &Path, path: &Path) -> Result<Self, SetError> {
        if !path.join(".git").exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io(parent))?;
            }
            let dir = path.parent().unwrap_or(Path::new("."));
            git::run(
                dir,
                &[
                    "clone",
                    "--quiet",
                    "--local",
                    "--no-checkout",
                    &repo.display().to_string(),
                    &path.display().to_string(),
                ],
            )?;
        }
        Ok(Self {
            path: path.to_owned(),
            repo: repo.to_owned(),
            base: None,
        })
    }

    /// After every checkout, `branch` (and `origin/<branch>`) names `base`,
    /// so an oracle's "fork point with the base branch" is the task's base
    /// whether the tree came from the operator's repo or a trial's clone.
    pub fn with_base(mut self, branch: &str, base: &str) -> Self {
        self.base = Some((branch.to_owned(), base.to_owned()));
        self
    }

    /// Move to `sha` with nothing left over from the last grading (fetching
    /// it from the source repo when the clone predates it).
    pub fn checkout(&self, sha: &str) -> Result<(), SetError> {
        self.checkout_from(&self.repo.clone(), sha)
    }

    /// [`Self::checkout`], fetching `sha` from `source` when it is missing
    /// (a trial's commit lives in the trial's own repository).
    pub fn checkout_from(&self, source: &Path, sha: &str) -> Result<(), SetError> {
        self.fetch_missing(source, sha)?;
        if let Some((_, base)) = &self.base {
            self.fetch_missing(source, base)?;
        }
        git::run(
            &self.path,
            &["checkout", "--quiet", "--detach", "--force", sha],
        )?;
        git::run(&self.path, &["clean", "-qfdx", "-e", "target"])?;
        if let Some((branch, base)) = &self.base {
            git::update_ref(&self.path, &format!("refs/heads/{branch}"), base)?;
            git::update_ref(&self.path, &format!("refs/remotes/origin/{branch}"), base)?;
        }
        Ok(())
    }

    /// Fetch `sha` from `source` unless the tree already has it.
    fn fetch_missing(&self, source: &Path, sha: &str) -> Result<(), SetError> {
        if git::rev_parse(&self.path, sha).is_ok() {
            return Ok(());
        }
        let tmp = format!("refs/flywheel/eval-grade/{}", &sha[..sha.len().min(12)]);
        // A bare sha is fetched through a temporary ref in the source.
        git::update_ref(source, &tmp, sha)?;
        let fetched = git::run(
            &self.path,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &source.display().to_string(),
                &tmp,
            ],
        );
        let _ = git::run(source, &["update-ref", "-d", &tmp]);
        fetched?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn set_with_oracle(script: &str) -> (tempfile::TempDir, SetDir) {
        let evals = tempfile::tempdir().unwrap();
        let set = SetDir::draft(evals.path(), "core");
        let home = set.oracle_dir("t1").join("home/verification");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("oracle.sh"), script).unwrap();
        std::fs::write(
            set.registration("t1"),
            format!(
                r#"{{"graders": [{{"type": "command", "name": "oracle", "run": "sh {PROJECT_HOME_TOKEN}/verification/oracle.sh"}}]}}"#
            ),
        )
        .unwrap();
        (evals, set)
    }

    #[test]
    fn a_tree_from_a_branchless_trial_clone_still_names_the_base_branch() {
        let repo = TempRepo::new();
        let base = repo.commit("base", &[("a.txt", "1")]);
        // A trial's clone: only its own branch, no `main` anywhere.
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
        git::run(
            dir.path(),
            &["clone", "-q", &repo.path().display().to_string(), "clone"],
        )
        .unwrap();
        git::run(&clone, &["checkout", "-q", "-b", "eval/trial"]).unwrap();
        git::run(&clone, &["branch", "-q", "-D", "main"]).unwrap();
        git::run(&clone, &["remote", "remove", "origin"]).unwrap();
        std::fs::write(clone.join("a.txt"), "2").unwrap();
        git::run(&clone, &["commit", "-qam", "agent"]).unwrap();
        let fin = git::rev_parse(&clone, "HEAD").unwrap();

        let bare = GradeTree::open(&clone, &dir.path().join("bare")).unwrap();
        bare.checkout_from(&clone, &fin).unwrap();
        assert!(!git::ok(&bare.path, &["merge-base", "HEAD", "main"]));

        let tree = GradeTree::open(&clone, &dir.path().join("tree"))
            .unwrap()
            .with_base("main", &base);
        tree.checkout_from(&clone, &fin).unwrap();
        for name in ["main", "origin/main"] {
            assert_eq!(
                git::run(&tree.path, &["merge-base", "HEAD", name]).unwrap(),
                base
            );
        }
        assert_eq!(git::default_branch(repo.path()), "main");
    }

    #[test]
    fn a_materialized_oracle_grades_a_tree_and_reports_its_checks() {
        let (_evals, set) = set_with_oracle(
            "test \"$(cat a.txt)\" = 2 || exit 1\n\
             test -z \"$FLYWHEEL_HUB_SESSION\" || exit 9\n\
             echo '{\"checks\":[{\"id\":\"c1\",\"passed\":true}]}' > \"$FLYWHEEL_EVAL_REWARD\"\n",
        );
        let dest = tempfile::tempdir().unwrap();
        let oracle = materialize(&set, "t1", &dest.path().join("o")).unwrap();
        assert!(
            oracle.commands[0]
                .1
                .contains(&dest.path().display().to_string())
        );
        let repo = TempRepo::new();
        let base = repo.commit("base", &[("a.txt", "1")]);
        let reference = repo.commit("ref", &[("a.txt", "2")]);
        let tree = GradeTree::open(repo.path(), &dest.path().join("tree")).unwrap();
        tree.checkout(&base).unwrap();
        let options = RunOptions {
            env: vec![("FLYWHEEL_HUB_SESSION".into(), String::new())],
            ..RunOptions::default()
        };
        let at_base = run(&oracle, &tree.path, &options).unwrap();
        assert!(!at_base.passed);
        assert!(
            at_base.detail.starts_with("exit 1 (oracle)"),
            "{}",
            at_base.detail
        );
        std::fs::write(tree.path.join("junk"), "x").unwrap();
        tree.checkout(&reference).unwrap();
        assert!(!tree.path.join("junk").exists());
        let at_ref = run(&oracle, &tree.path, &options).unwrap();
        assert!(at_ref.passed, "{:?}", at_ref.ran);
        assert_eq!(
            at_ref.checks,
            vec![CheckScore {
                id: "c1".into(),
                passed: true,
                note: String::new()
            }]
        );
    }

    #[test]
    fn non_command_registrations_are_refused() {
        let (_evals, set) = set_with_oracle("true\n");
        std::fs::write(
            set.registration("t1"),
            r#"{"graders": [{"type": "rubric", "assertions": ["x"]}]}"#,
        )
        .unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(
            materialize(&set, "t1", dest.path())
                .unwrap_err()
                .to_string()
                .contains("command graders")
        );
    }
}
