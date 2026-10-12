//! Claude Code skill installation -- `flywheel init`'s optional payload.
//!
//! `flywheel init` can drop a set of agent skills into the repo's
//! `.claude/skills/` so the authoring pipeline (idea -> spec -> tasks ->
//! held-out oracle -> retro -> proposals) is one slash command away in
//! Claude Code:
//!
//! * `/fw-spec` -- interview an idea into ungameable, end-state success
//!   criteria
//! * `/fw-plan` -- compile a spec or request into right-sized tasks, each
//!   spined on the strongest reward-hack-resistant grader the worker runs
//!   out-of-band
//! * `/fw-verify` -- blind-author the discriminating held-out test oracle
//!   for those tasks before execute, so the agent never writes the test that
//!   grades it
//! * `/fw-retro` -- forensic audit of how the loop executed a phase; every
//!   finding carries a re-runnable CLI pointer and stops at diagnosis
//! * `/fw-improve` -- turn cited retro findings into ranked, scoped
//!   proposals, each ending in a handoff (`/fw-spec`, `/fw-plan`, or accept)
//!
//! A sixth skill sits beside the authoring pipeline rather than in it:
//!
//! * `/flywheel-ops` -- operate the *running* loop: start and drain the
//!   worker, read run state off grader receipts, and recover parked or
//!   stranded work. It references only real `fw` verbs and `fw docs` topics
//!   and points to `fw docs <topic>` for depth instead of duplicating it.
//!
//! The markdown lives as greenfield templates with `__FW_*__` placeholder
//! tokens, embedded into this crate at compile time (see [`template`]) so
//! the binary is self-contained; rendering binds them to the repo's actual
//! settings (task directory, state directories, and -- for `fw-plan` -- the
//! work-source kind, so a GitHub-source repo gets issue-creation
//! instructions instead of task-file instructions).
//!
//! Every generated `SKILL.md` carries [`MANAGED_MARKER`] right after its
//! frontmatter. Re-running `flywheel init` (e.g. after changing the tasks
//! directory or the work source) regenerates marker-bearing files in place;
//! a file whose marker was removed is treated as user-owned and left
//! untouched. Names are `fw-`-prefixed precisely to avoid colliding with
//! generic command names (`/task`, `/define`) a repo may already have.

use crate::policy::WorkPolicy;
use std::path::{Component, Path, PathBuf};

/// Installed skill names. The first five are the authoring pipeline, in
/// pipeline order; `flywheel-ops` is the operability companion (driving the
/// already-built loop). Each becomes `.claude/skills/<name>/SKILL.md` and is
/// invoked as `/<name>`.
pub const SKILL_NAMES: [&str; 6] = [
    "fw-spec",
    "fw-plan",
    "fw-verify",
    "fw-retro",
    "fw-improve",
    "flywheel-ops",
];

/// Sentinel line every generated SKILL.md carries (right after the
/// frontmatter). Its presence is the regeneration contract: re-running
/// `flywheel init` overwrites marker-bearing files with a fresh render;
/// deleting the line hands the file to the user permanently.
pub const MANAGED_MARKER: &str = "<!-- managed-by: flywheel init -->";

/// Default install root, relative to the repo root `init` runs in.
pub const DEFAULT_SKILLS_ROOT: &str = ".claude/skills";

const DEFAULT_TASKS_DIR: &str = ".flywheel/tasks";
const DEFAULT_DB_PATH: &str = ".flywheel/flywheel.sqlite";

/// The skill templates, embedded from the single shipped copy under the
/// Python package's `_skill_templates/` directory so the two
/// implementations can never drift.
macro_rules! skill_template {
    ($file:literal) => {
        include_str!(concat!(
            "../../../packages/flywheel-orchestrator/src/flywheel_orchestrator/_skill_templates/",
            $file
        ))
    };
}

/// The raw (unrendered) template for a skill or delivery block, `None` for
/// an unknown name. Skill templates are keyed by their [`SKILL_NAMES`]
/// entry; the two `fw-plan` delivery blocks are keyed
/// `fw-plan-delivery-directory` / `fw-plan-delivery-github`.
pub fn template(name: &str) -> Option<&'static str> {
    Some(match name {
        "fw-spec" => skill_template!("fw-spec.md"),
        "fw-plan" => skill_template!("fw-plan.md"),
        "fw-verify" => skill_template!("fw-verify.md"),
        "fw-retro" => skill_template!("fw-retro.md"),
        "fw-improve" => skill_template!("fw-improve.md"),
        "flywheel-ops" => skill_template!("flywheel-ops.md"),
        "fw-plan-delivery-directory" => skill_template!("fw-plan-delivery-directory.md"),
        "fw-plan-delivery-github" => skill_template!("fw-plan-delivery-github.md"),
        _ => return None,
    })
}

/// Failures of the skill render / install sweep.
///
/// A leftover `__FW_*__` token after substitution means the template and
/// the renderer disagree about the placeholder set -- surfaced loudly so a
/// drifted template can never ship a broken skill.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("unknown skill template {0:?}")]
    UnknownTemplate(String),
    #[error("skill template {name:?} rendered with an unbound placeholder near {near:?}__")]
    UnboundPlaceholder { name: String, near: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SkillError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        SkillError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The repo-specific values a skill render binds to.
///
/// Derived from the effective work policy (see [`settings_from_policy`]);
/// defaults mirror `flywheel init`'s own defaults so a policy-less repo
/// renders identically to a default-answers init. [`state_root`]
/// anchors the conventional sibling directories (`specs/`, `audits/`,
/// `proposals/`): the tasks directory's parent when the directory layout is
/// in use, else the store directory.
///
/// [`state_root`]: SkillRenderSettings::state_root
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRenderSettings {
    pub tasks_dir: PathBuf,
    pub db_path: PathBuf,
    pub source_kind: String,
    pub github_repo: Option<String>,
    pub github_label: Option<String>,
}

impl Default for SkillRenderSettings {
    fn default() -> Self {
        Self {
            tasks_dir: PathBuf::from(DEFAULT_TASKS_DIR),
            db_path: PathBuf::from(DEFAULT_DB_PATH),
            source_kind: "directory".to_owned(),
            github_repo: None,
            github_label: None,
        }
    }
}

/// `Path.parent` the way Python reports it: the containing directory, or
/// the empty path (Python's `.`) for a bare name.
fn parent_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

impl SkillRenderSettings {
    pub fn state_root(&self) -> PathBuf {
        if self.source_kind == "directory" {
            parent_of(&self.tasks_dir)
        } else {
            parent_of(&self.db_path)
        }
    }

    pub fn specs_dir(&self) -> PathBuf {
        self.state_root().join("specs")
    }

    pub fn audits_dir(&self) -> PathBuf {
        self.state_root().join("audits")
    }

    pub fn proposals_dir(&self) -> PathBuf {
        self.state_root().join("proposals")
    }

    /// Mirrors the harness's telemetry root: `<db dir>/logs`.
    pub fn logs_dir(&self) -> PathBuf {
        parent_of(&self.db_path).join("logs")
    }
}

/// Bind render settings to the effective policy (or the defaults).
pub fn settings_from_policy(policy: Option<&WorkPolicy>) -> SkillRenderSettings {
    let Some(policy) = policy else {
        return SkillRenderSettings::default();
    };
    SkillRenderSettings {
        tasks_dir: policy
            .tasks_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TASKS_DIR)),
        db_path: policy
            .db_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DB_PATH)),
        source_kind: policy.source_kind.clone(),
        github_repo: policy.github_repo.clone(),
        github_label: policy.github_label.clone(),
    }
}

/// Outcome of one [`install_skills`] sweep, for operator output.
///
/// Paths are the written/skipped `SKILL.md` files. `created` did not exist
/// before; `updated` carried the managed marker and was regenerated;
/// `skipped` exists without the marker (user-owned) and was left
/// byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillInstallReport {
    pub created: Vec<PathBuf>,
    pub updated: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
}

/// A path rendered the way Python's `str(Path(...))` prints it: `.`
/// segments and empty components collapsed, no trailing separator, and the
/// bare current directory as `.`.
fn py_path_str(path: &Path) -> String {
    let mut out = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::CurDir) {
            out.push(component.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        out.to_string_lossy().into_owned()
    }
}

fn read_template(name: &str) -> Result<&'static str, SkillError> {
    template(name).ok_or_else(|| SkillError::UnknownTemplate(name.to_owned()))
}

/// The `fw-plan` delivery section for the configured work source.
fn delivery_block(settings: &SkillRenderSettings) -> Result<String, SkillError> {
    if settings.source_kind == "github"
        && let (Some(repo), Some(label)) = (&settings.github_repo, &settings.github_label)
        && !repo.is_empty()
        && !label.is_empty()
    {
        let block = read_template("fw-plan-delivery-github")?
            .replace("__FW_GH_REPO__", repo)
            .replace("__FW_GH_LABEL__", label);
        return Ok(block.trim_end_matches('\n').to_owned());
    }
    Ok(read_template("fw-plan-delivery-directory")?
        .trim_end_matches('\n')
        .to_owned())
}

/// Render one skill's `SKILL.md` content for `settings`.
///
/// Fails with [`SkillError::UnknownTemplate`] for a name outside
/// [`SKILL_NAMES`] and [`SkillError::UnboundPlaceholder`] for a leftover
/// placeholder token after substitution.
pub fn render_skill(name: &str, settings: &SkillRenderSettings) -> Result<String, SkillError> {
    if !SKILL_NAMES.contains(&name) {
        return Err(SkillError::UnknownTemplate(name.to_owned()));
    }
    let mut text = read_template(name)?.to_owned();
    if text.contains("__FW_DELIVERY__") {
        text = text.replace("__FW_DELIVERY__", &delivery_block(settings)?);
    }
    let replacements = [
        ("__FW_TASKS_DIR__", py_path_str(&settings.tasks_dir)),
        ("__FW_SPECS_DIR__", py_path_str(&settings.specs_dir())),
        ("__FW_AUDITS_DIR__", py_path_str(&settings.audits_dir())),
        (
            "__FW_PROPOSALS_DIR__",
            py_path_str(&settings.proposals_dir()),
        ),
        ("__FW_LOGS_DIR__", py_path_str(&settings.logs_dir())),
    ];
    for (token, value) in &replacements {
        text = text.replace(token, value);
    }
    if let Some(idx) = text.find("__FW_") {
        // Python: text[idx:].split("__", 2) -> ["", "FW_NAME", rest]; the
        // first two joined by "__" name the offending token.
        let leftover: Vec<&str> = text[idx..].splitn(3, "__").collect();
        let near = leftover[..leftover.len().min(2)].join("__");
        return Err(SkillError::UnboundPlaceholder {
            name: name.to_owned(),
            near,
        });
    }
    Ok(text)
}

/// Write (or regenerate) the managed skills under `dest_root`.
///
/// One sweep serves both the fresh-install and the reconfigure path: a
/// missing `SKILL.md` is created, a marker-bearing one is regenerated for
/// the (possibly changed) settings, and a file without the marker is
/// user-owned and skipped untouched. `names` is normally [`SKILL_NAMES`].
pub fn install_skills(
    dest_root: impl AsRef<Path>,
    settings: &SkillRenderSettings,
    names: &[&str],
) -> Result<SkillInstallReport, SkillError> {
    let dest_root = dest_root.as_ref();
    let mut report = SkillInstallReport::default();
    for name in names {
        let target = dest_root.join(name).join("SKILL.md");
        let rendered = render_skill(name, settings)?;
        if !target.exists() {
            let parent = target.parent().unwrap_or(dest_root);
            std::fs::create_dir_all(parent).map_err(|e| SkillError::io(parent, e))?;
            std::fs::write(&target, &rendered).map_err(|e| SkillError::io(&target, e))?;
            report.created.push(target);
            continue;
        }
        let existing = std::fs::read_to_string(&target).map_err(|e| SkillError::io(&target, e))?;
        if !existing.contains(MANAGED_MARKER) {
            report.skipped.push(target);
            continue;
        }
        if existing != rendered {
            std::fs::write(&target, &rendered).map_err(|e| SkillError::io(&target, e))?;
        }
        report.updated.push(target);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn settings_with_tasks_dir(tasks_dir: &str) -> SkillRenderSettings {
        SkillRenderSettings {
            tasks_dir: PathBuf::from(tasks_dir),
            ..SkillRenderSettings::default()
        }
    }

    // --- rendering ----------------------------------------------------------

    #[test]
    fn every_skill_renders_with_default_settings() {
        let settings = SkillRenderSettings::default();
        for name in SKILL_NAMES {
            let text = render_skill(name, &settings).unwrap();
            assert!(text.starts_with("---\n"), "{name}");
            assert!(text.contains(&format!("name: {name}")), "{name}");
            assert!(text.contains(MANAGED_MARKER), "{name}");
            assert!(!text.contains("__FW_"), "{name} leaked a placeholder");
        }
    }

    #[test]
    fn render_binds_default_paths() {
        let settings = SkillRenderSettings::default();
        let plan = render_skill("fw-plan", &settings).unwrap();
        assert!(plan.contains(".flywheel/tasks/active/"));
        assert!(plan.contains(".flywheel/specs/"));
        let spec = render_skill("fw-spec", &settings).unwrap();
        assert!(spec.contains(".flywheel/specs/NNNNN-FEATURE-<name>.md"));
        let retro = render_skill("fw-retro", &settings).unwrap();
        assert!(retro.contains(".flywheel/audits/"));
        assert!(retro.contains(".flywheel/logs/runs/"));
        let improve = render_skill("fw-improve", &settings).unwrap();
        assert!(improve.contains(".flywheel/proposals/"));
        let verify = render_skill("fw-verify", &settings).unwrap();
        assert!(verify.contains(".flywheel/tasks/active/"));
        assert!(verify.contains(".flywheel/specs/"));
    }

    #[test]
    fn fw_verify_instructs_held_out_registration_and_keeps_fence() {
        let text = render_skill("fw-verify", &SkillRenderSettings::default()).unwrap();
        assert!(text.contains("[held_out] root"));
        assert!(text.contains("<held-out-root>/<task_id>.json"));
        assert!(text.contains("KEYED BY the owning task id"));
        assert!(text.contains("ABSOLUTE OPERATOR PATH"));
        assert!(text.contains("wire it as an in-repo `command` grader"));
        assert!(text.contains("never a commit into the tracked tree"));
        assert!(text.contains("DIFFERENT, sanctioned"));
        assert!(!text.contains("__FW_"));
    }

    #[test]
    fn flywheel_ops_is_shipped_and_renders_operability_runbook() {
        assert!(SKILL_NAMES.contains(&"flywheel-ops"));
        let text = render_skill("flywheel-ops", &SkillRenderSettings::default()).unwrap();
        assert!(text.starts_with("---\n"));
        assert!(text.contains("name: flywheel-ops"));
        assert!(text.contains(MANAGED_MARKER));
        assert!(!text.contains("__FW_"));
        assert!(text.contains("fw worker"));
        assert!(text.contains("status --rollup"));
        assert!(text.contains("parked"));
        assert!(text.contains("stranded"));
        assert!(text.contains("fw docs"));
    }

    #[test]
    fn render_binds_custom_tasks_dir() {
        let text = render_skill("fw-plan", &settings_with_tasks_dir("work/tasks")).unwrap();
        assert!(text.contains("work/tasks/active/"));
        assert!(!text.contains(".flywheel/tasks"));
        assert!(text.contains("work/specs/"));
    }

    #[test]
    fn render_github_source_swaps_delivery_block() {
        let settings = SkillRenderSettings {
            source_kind: "github".to_owned(),
            github_repo: Some("octo/widgets".to_owned()),
            github_label: Some("flywheel".to_owned()),
            ..SkillRenderSettings::default()
        };
        let text = render_skill("fw-plan", &settings).unwrap();
        assert!(text.contains("gh issue create"));
        assert!(text.contains("octo/widgets"));
        assert!(text.contains("WORK ITEMS ARE GITHUB ISSUES"));
        assert!(!text.contains("CHOOSE PHASE DIRECTORY"));

        let directory = render_skill("fw-plan", &SkillRenderSettings::default()).unwrap();
        assert!(!directory.contains("gh issue create"));
        assert!(directory.contains("CHOOSE PHASE DIRECTORY"));
    }

    #[test]
    fn github_source_without_repo_or_label_falls_back_to_directory_block() {
        let settings = SkillRenderSettings {
            source_kind: "github".to_owned(),
            github_repo: Some("octo/widgets".to_owned()),
            github_label: None,
            ..SkillRenderSettings::default()
        };
        let text = render_skill("fw-plan", &settings).unwrap();
        assert!(!text.contains("gh issue create"));
        assert!(text.contains("CHOOSE PHASE DIRECTORY"));
    }

    #[test]
    fn render_unknown_name_fails() {
        let err = render_skill("fw-nope", &SkillRenderSettings::default()).unwrap_err();
        assert!(matches!(err, SkillError::UnknownTemplate(ref n) if n == "fw-nope"));
        assert_eq!(err.to_string(), "unknown skill template \"fw-nope\"");
        assert!(template("fw-nope").is_none());
    }

    #[test]
    fn unbound_placeholder_error_names_the_token() {
        let err = SkillError::UnboundPlaceholder {
            name: "fw-x".to_owned(),
            near: "__FW_NOPE".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "skill template \"fw-x\" rendered with an unbound placeholder near \"__FW_NOPE\"__"
        );
    }

    #[test]
    fn settings_from_policy_binds_policy_paths() {
        let policy = WorkPolicy {
            source_kind: "directory".to_owned(),
            tasks_dir: Some(PathBuf::from("custom/tasks")),
            db_path: Some(PathBuf::from("custom/state.sqlite")),
            ..WorkPolicy::default()
        };
        let settings = settings_from_policy(Some(&policy));
        assert_eq!(settings.tasks_dir, PathBuf::from("custom/tasks"));
        assert_eq!(settings.specs_dir(), PathBuf::from("custom/specs"));
        assert_eq!(settings.logs_dir(), PathBuf::from("custom/logs"));
        assert_eq!(
            settings_from_policy(None).tasks_dir,
            PathBuf::from(".flywheel/tasks")
        );
    }

    #[test]
    fn settings_for_tracker_source_anchor_on_db_dir() {
        let policy = WorkPolicy {
            source_kind: "github".to_owned(),
            github_repo: Some("octo/widgets".to_owned()),
            github_label: Some("flywheel".to_owned()),
            db_path: Some(PathBuf::from("state/flywheel.sqlite")),
            ..WorkPolicy::default()
        };
        let settings = settings_from_policy(Some(&policy));
        assert_eq!(settings.specs_dir(), PathBuf::from("state/specs"));
        assert_eq!(settings.audits_dir(), PathBuf::from("state/audits"));
    }

    #[test]
    fn bare_tasks_dir_anchors_siblings_at_the_current_directory() {
        let settings = settings_with_tasks_dir("tasks");
        assert_eq!(py_path_str(&settings.specs_dir()), "specs");
        assert_eq!(py_path_str(Path::new("./work/tasks/")), "work/tasks");
        assert_eq!(py_path_str(Path::new(".")), ".");
    }

    // --- install / regenerate semantics -------------------------------------

    #[test]
    fn install_creates_then_updates_managed_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(".claude").join("skills");
        let first = install_skills(&root, &SkillRenderSettings::default(), &SKILL_NAMES).unwrap();
        assert_eq!(first.created.len(), SKILL_NAMES.len());
        assert!(first.updated.is_empty());
        assert!(first.skipped.is_empty());
        for name in SKILL_NAMES {
            assert!(root.join(name).join("SKILL.md").is_file());
        }

        let second =
            install_skills(&root, &settings_with_tasks_dir("work/tasks"), &SKILL_NAMES).unwrap();
        assert!(second.created.is_empty());
        assert_eq!(second.updated.len(), SKILL_NAMES.len());
        let plan = std::fs::read_to_string(root.join("fw-plan").join("SKILL.md")).unwrap();
        assert!(plan.contains("work/tasks/active/"));

        // A same-settings re-run still reports every managed file as updated.
        let third =
            install_skills(&root, &settings_with_tasks_dir("work/tasks"), &SKILL_NAMES).unwrap();
        assert_eq!(third.updated.len(), SKILL_NAMES.len());
    }

    #[test]
    fn install_skips_user_owned_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(".claude").join("skills");
        install_skills(&root, &SkillRenderSettings::default(), &SKILL_NAMES).unwrap();
        let target = root.join("fw-plan").join("SKILL.md");
        let owned = std::fs::read_to_string(&target)
            .unwrap()
            .replace(&format!("{MANAGED_MARKER}\n"), "");
        std::fs::write(&target, &owned).unwrap();

        let report =
            install_skills(&root, &settings_with_tasks_dir("work/tasks"), &SKILL_NAMES).unwrap();
        assert!(report.skipped.contains(&target));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), owned);
        assert_eq!(report.updated.len(), SKILL_NAMES.len() - 1);
    }

    #[test]
    fn install_honors_an_explicit_name_subset() {
        let dir = TempDir::new().unwrap();
        let report =
            install_skills(dir.path(), &SkillRenderSettings::default(), &["fw-spec"]).unwrap();
        assert_eq!(report.created.len(), 1);
        assert!(dir.path().join("fw-spec").join("SKILL.md").is_file());
        assert!(!dir.path().join("fw-plan").exists());
        let err =
            install_skills(dir.path(), &SkillRenderSettings::default(), &["fw-nope"]).unwrap_err();
        assert!(matches!(err, SkillError::UnknownTemplate(_)));
    }
}
